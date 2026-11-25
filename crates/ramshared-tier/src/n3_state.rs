// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-tier/src/n3_state.rs
//! Pure host-authoritative N3 observation and lease state model.
//!
//! This module is deliberately independent of Windows, WDDM, CUDA, kernel
//! memory management, and the RamShared transport.  It models the contract
//! boundary described by the architecture design; it does
//! not establish physical residency or guest ownership.

mod lease_drain;
mod lease_types;
mod preflight;
mod restart;

pub use lease_types::{
    DrainAck, FailAck, Grant, GrantAck, GuestClaim, GuestDecision, HostEvent, LeaseState,
    LifecycleEvent, N3State, N3StateMachine, ProductTransport, ProtocolDecision, ProtocolEvent,
    Revoke, RevokeCompletion, ScrubResult, StateTag,
};
pub use preflight::{
    HostObservation, ObservationEvent, ObservationEventKind, PreflightAction, PreflightDecision,
    PreflightModel, PreflightState,
};
pub use restart::{
    AdapterId, Authority, CAPACITY_ALIGNMENT_BYTES, EventId, FailureReason, GenerationCheckpoint,
    LeaseId, MAX_CAPACITY_BYTES, MAX_GENERATION_HISTORY, MAX_IN_FLIGHT, MAX_OBSERVATION_AGE,
    MAX_OBSERVATION_EVENTS, MAX_OPAQUE_ID_BYTES, MAX_PROTOCOL_EVENT_HISTORY,
    MAX_RESTART_RECORD_BYTES, N3_SCHEMA_VERSION, OpaqueId, RESTART_RECORD_HEADER_BYTES,
    RefusalCode, RestartRecord, StateTransitionError,
};

use lease_types::{ActiveLease, EventFingerprint, EventRegistration, ScrubState, SeenEvent};
use preflight::valid_capacity;
#[cfg(test)]
use restart::GUEST_AUTHORITY_MARKER;

/// Pure N3 model: bounded observation preflight plus host-led lease protocol.
#[derive(Clone, Debug)]
pub struct LeaseMachine {
    preflight: PreflightModel,
    lease_state: LeaseState,
    pending_grant: Option<Grant>,
    active_lease: Option<ActiveLease>,
    seen_events: Vec<SeenEvent>,
    accepted_grant_acks: Vec<GrantAck>,
    completed_revokes: Vec<RevokeCompletion>,
    generation_history: Vec<(LeaseId, u64)>,
    restored_restart_epoch: Option<u64>,
    in_flight: u32,
    callbacks_pending: u32,
    unknown_in_flight: bool,
    scrub_state: ScrubState,
    sent_drain_ack: bool,
}

impl Default for LeaseMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl LeaseMachine {
    /// Starts with no host observation and no lease authorization.
    pub fn new() -> Self {
        Self {
            preflight: PreflightModel::new(),
            lease_state: LeaseState::Absent,
            pending_grant: None,
            active_lease: None,
            seen_events: Vec::new(),
            accepted_grant_acks: Vec::new(),
            completed_revokes: Vec::new(),
            generation_history: Vec::new(),
            restored_restart_epoch: None,
            in_flight: 0,
            callbacks_pending: 0,
            unknown_in_flight: false,
            scrub_state: ScrubState::Pending,
            sent_drain_ack: false,
        }
    }

    /// Applies a preflight observation; this cannot change the lease state.
    pub fn observe(&mut self, observation: HostObservation, now: u64) -> PreflightDecision {
        self.preflight.observe(observation, now)
    }

    pub fn preflight_state(&self) -> PreflightState {
        self.preflight.state()
    }

    pub fn lease_state(&self) -> LeaseState {
        self.lease_state
    }

    pub fn state(&self) -> LeaseState {
        self.lease_state
    }

    pub fn has_host_grant(&self) -> bool {
        self.active_lease.is_some() || self.pending_grant.is_some()
    }

    pub fn observation_count(&self) -> u64 {
        self.preflight.observation_count()
    }

    pub fn refusal_count(&self) -> u64 {
        self.preflight.refusal_count()
    }

    pub fn sent_drain_ack(&self) -> bool {
        self.sent_drain_ack
    }

    pub fn active_lease_id(&self) -> Option<&LeaseId> {
        self.active_lease.as_ref().map(|lease| &lease.lease_id)
    }

    pub fn lease_capacity_bytes(&self) -> Option<u64> {
        self.active_lease.as_ref().map(|lease| lease.capacity_bytes)
    }

    pub fn in_flight(&self) -> u32 {
        self.in_flight
    }

    pub fn callbacks_pending(&self) -> u32 {
        self.callbacks_pending
    }

    pub fn scrubbed(&self) -> bool {
        self.scrub_state == ScrubState::Succeeded
    }

    /// Returns the host epoch supplied by a successfully restored restart record.
    pub fn restored_restart_epoch(&self) -> Option<u64> {
        self.restored_restart_epoch
    }

    /// Decodes a caller-supplied bounded restart record, then atomically seeds
    /// a fresh model's generation history. This pure method performs no I/O.
    pub fn restore_restart_bytes(&mut self, bytes: &[u8]) -> Result<(), FailureReason> {
        let record = match RestartRecord::from_bytes(bytes) {
            Ok(record) => record,
            Err(reason) => return Err(self.fail_restart_restore(reason)),
        };
        self.restore_restart_record(record)
    }

    /// Atomically seeds a fresh model from an already-decoded host record.
    ///
    /// A caller must restore before observations or protocol events. Any invalid
    /// input or non-fresh state fails closed without replacing existing history.
    pub fn restore_restart_record(&mut self, record: RestartRecord) -> Result<(), FailureReason> {
        if let Err(reason) = record.validate() {
            return Err(self.fail_restart_restore(reason));
        }
        if self.lease_state != LeaseState::Absent
            || self.pending_grant.is_some()
            || self.active_lease.is_some()
            || !self.seen_events.is_empty()
            || !self.accepted_grant_acks.is_empty()
            || !self.completed_revokes.is_empty()
            || !self.generation_history.is_empty()
            || self.restored_restart_epoch.is_some()
            || self.preflight.observation_count() != 0
        {
            return Err(self.fail_restart_restore(FailureReason::StateTransition(
                StateTransitionError::IllegalTransition {
                    expected: Some(StateTag::Absent),
                    actual: self.lease_state.tag(),
                },
            )));
        }

        let host_epoch = record.host_epoch();
        let generation_history = record
            .into_checkpoints()
            .into_iter()
            .map(|checkpoint| (checkpoint.lease_id, checkpoint.generation))
            .collect();
        self.generation_history = generation_history;
        self.restored_restart_epoch = Some(host_epoch);
        Ok(())
    }

    pub fn request_demotion(&mut self) -> PreflightDecision {
        self.preflight.request_demotion()
    }

    pub fn refuse_guest_claim(&mut self, _claim: GuestClaim) -> PreflightDecision {
        self.preflight.refuse_guest_claim()
    }

    pub fn refuse_product_transport(&mut self, _transport: ProductTransport) -> PreflightDecision {
        self.preflight.refuse_product_scope()
    }

    /// Dispatches one host event through the same bounded transition methods.
    pub fn apply_host_event(&mut self, event: HostEvent, now: u64) -> ProtocolDecision {
        match event {
            HostEvent::Grant(grant) => self.receive_grant(grant, now),
            HostEvent::GrantAckAccepted(ack) => self.accept_grant_ack(ack),
            HostEvent::Revoke(revoke) => self.receive_revoke(revoke),
            HostEvent::RevokeCompleted(completion) => self.confirm_revoke(completion),
            HostEvent::Lifecycle(event) => self.fail_closed(event),
        }
    }

    /// Receives a host `GRANT`.  Validation enters `NEGOTIATING`; only a later
    /// host acceptance of the returned `GRANT_ACK` enters `GRANTED`.
    pub fn receive_grant(&mut self, grant: Grant, now: u64) -> ProtocolDecision {
        match self.register_event(EventFingerprint::Grant(grant.clone())) {
            EventRegistration::Duplicate => return ProtocolDecision::Noop,
            EventRegistration::Conflict => {
                return self.fail_for_event(
                    Some(grant.lease_id),
                    Some(grant.generation),
                    Some(grant.event_id),
                    FailureReason::ConflictingDuplicate,
                );
            }
            EventRegistration::Overflow => {
                return self.fail_for_event(
                    Some(grant.lease_id),
                    Some(grant.generation),
                    Some(grant.event_id),
                    FailureReason::MalformedRecord,
                );
            }
            EventRegistration::New => {}
        }

        if self.lease_state != LeaseState::Absent {
            return self.fail_for_event(
                Some(grant.lease_id),
                Some(grant.generation),
                Some(grant.event_id),
                FailureReason::StateTransition(StateTransitionError::IllegalTransition {
                    expected: Some(StateTag::Absent),
                    actual: self.lease_state.tag(),
                }),
            );
        }
        if grant
            .expected_state
            .is_some_and(|state| state != StateTag::Absent)
        {
            return self.fail_for_event(
                Some(grant.lease_id),
                Some(grant.generation),
                Some(grant.event_id),
                FailureReason::StateTransition(StateTransitionError::IllegalTransition {
                    expected: Some(StateTag::Absent),
                    actual: self.lease_state.tag(),
                }),
            );
        }
        if grant.contract_version != N3_SCHEMA_VERSION
            || grant.lease_id.as_bytes().is_empty()
            || grant.event_id.as_bytes().is_empty()
            || grant.generation == 0
            || grant.host_epoch == 0
            || grant.deadline <= grant.issued_at
            || now < grant.issued_at
            || now >= grant.deadline
        {
            return self.fail_for_event(
                Some(grant.lease_id),
                Some(grant.generation),
                Some(grant.event_id),
                FailureReason::MalformedRecord,
            );
        }
        if self
            .restored_restart_epoch
            .is_some_and(|epoch| grant.host_epoch < epoch)
        {
            return self.fail_for_event(
                Some(grant.lease_id),
                Some(grant.generation),
                Some(grant.event_id),
                FailureReason::StateTransition(StateTransitionError::StaleGeneration {
                    provided: grant.host_epoch,
                    expected: self.restored_restart_epoch.unwrap_or(0),
                }),
            );
        }
        let Some(observation) = self.preflight.latest_observation() else {
            return self.fail_for_event(
                Some(grant.lease_id),
                Some(grant.generation),
                Some(grant.event_id),
                FailureReason::NoFreshHostObservation,
            );
        };
        if observation.validate(now).is_err()
            || observation.host_epoch != grant.host_epoch
            || observation.budget_bytes < grant.capacity_bytes
            || !matches!(
                self.preflight.state(),
                PreflightState::Observing
                    | PreflightState::Constrained
                    | PreflightState::DemotionRequested
            )
        {
            return self.fail_for_event(
                Some(grant.lease_id),
                Some(grant.generation),
                Some(grant.event_id),
                FailureReason::NoFreshHostObservation,
            );
        }
        if !valid_capacity(grant.capacity_bytes) {
            return self.fail_for_event(
                Some(grant.lease_id),
                Some(grant.generation),
                Some(grant.event_id),
                FailureReason::InvalidCapacity,
            );
        }
        match self.validate_generation(&grant.lease_id, grant.generation) {
            Ok(()) => {}
            Err(reason) => {
                return self.fail_for_event(
                    Some(grant.lease_id),
                    Some(grant.generation),
                    Some(grant.event_id),
                    reason,
                );
            }
        }
        self.remember_generation(grant.lease_id.clone(), grant.generation);
        let ack = GrantAck {
            contract_version: grant.contract_version,
            lease_id: grant.lease_id.clone(),
            generation: grant.generation,
            event_id: grant.event_id.clone(),
        };
        self.lease_state = LeaseState::Negotiating(grant.generation);
        self.pending_grant = Some(grant);
        ProtocolDecision::GrantAck(ack)
    }

    /// Host acceptance of a previously emitted `GRANT_ACK`.
    pub fn accept_grant_ack(&mut self, ack: GrantAck) -> ProtocolDecision {
        if self.accepted_grant_acks.iter().any(|seen| seen == &ack) {
            return ProtocolDecision::Noop;
        }
        let Some(grant) = self.pending_grant.clone() else {
            return self.fail_for_event(
                Some(ack.lease_id),
                Some(ack.generation),
                Some(ack.event_id),
                FailureReason::StateTransition(StateTransitionError::IllegalTransition {
                    expected: Some(StateTag::Negotiating),
                    actual: self.lease_state.tag(),
                }),
            );
        };
        if self.lease_state != LeaseState::Negotiating(grant.generation)
            || ack.contract_version != grant.contract_version
            || ack.lease_id != grant.lease_id
            || ack.generation != grant.generation
            || ack.event_id != grant.event_id
        {
            return self.fail_for_event(
                Some(ack.lease_id),
                Some(ack.generation),
                Some(ack.event_id),
                FailureReason::LeaseIdentityMismatch,
            );
        }
        if self.accepted_grant_acks.len() >= MAX_PROTOCOL_EVENT_HISTORY {
            return self.fail_for_active(FailureReason::MalformedRecord);
        }
        self.accepted_grant_acks.push(ack);
        self.active_lease = Some(ActiveLease {
            lease_id: grant.lease_id,
            generation: grant.generation,
            grant_event_id: grant.event_id,
            capacity_bytes: grant.capacity_bytes,
            revoke: None,
        });
        self.pending_grant = None;
        self.lease_state = LeaseState::Granted(grant.generation);
        ProtocolDecision::Accepted(self.lease_state)
    }

    /// Alias for host-facing terminology.
    pub fn host_accepts_grant_ack(&mut self, ack: GrantAck) -> ProtocolDecision {
        self.accept_grant_ack(ack)
    }

    /// Receives a matching host `REVOKE`, blocks new I/O, and begins drain.
    pub fn receive_revoke(&mut self, revoke: Revoke) -> ProtocolDecision {
        match self.register_event(EventFingerprint::Revoke(revoke.clone())) {
            EventRegistration::Duplicate => return ProtocolDecision::Noop,
            EventRegistration::Conflict => {
                return self.fail_for_event(
                    Some(revoke.lease_id),
                    Some(revoke.generation),
                    Some(revoke.event_id),
                    FailureReason::ConflictingDuplicate,
                );
            }
            EventRegistration::Overflow => {
                return self.fail_for_event(
                    Some(revoke.lease_id),
                    Some(revoke.generation),
                    Some(revoke.event_id),
                    FailureReason::MalformedRecord,
                );
            }
            EventRegistration::New => {}
        }
        let Some(active) = self.active_lease.as_mut() else {
            return self.fail_for_event(
                Some(revoke.lease_id),
                Some(revoke.generation),
                Some(revoke.event_id),
                FailureReason::StateTransition(StateTransitionError::IllegalTransition {
                    expected: Some(StateTag::Granted),
                    actual: self.lease_state.tag(),
                }),
            );
        };
        if self.lease_state != LeaseState::Granted(active.generation)
            || revoke.lease_id != active.lease_id
            || revoke.generation != active.generation
            || revoke.deadline == 0
        {
            return self.fail_for_event(
                Some(revoke.lease_id),
                Some(revoke.generation),
                Some(revoke.event_id),
                FailureReason::LeaseIdentityMismatch,
            );
        }
        if revoke
            .expected_state
            .is_some_and(|state| state != StateTag::Granted)
        {
            return self.fail_for_event(
                Some(revoke.lease_id),
                Some(revoke.generation),
                Some(revoke.event_id),
                FailureReason::StateTransition(StateTransitionError::IllegalTransition {
                    expected: Some(StateTag::Granted),
                    actual: self.lease_state.tag(),
                }),
            );
        }
        active.revoke = Some(revoke);
        self.lease_state = LeaseState::Quiescing(active.generation);
        self.scrub_state = ScrubState::Pending;
        self.sent_drain_ack = false;
        ProtocolDecision::BeginDrain
    }
}

#[cfg(test)]
mod additional_tests;
#[cfg(test)]
mod tests;
