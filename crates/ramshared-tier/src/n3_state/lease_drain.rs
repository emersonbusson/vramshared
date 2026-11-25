// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-tier/src/n3_state/lease_drain.rs
//! In-flight tracking, scrub/drain transitions, cleanup, and history validation for [`LeaseMachine`].

use super::lease_types::{
    DrainAck, EventFingerprint, EventRegistration, FailAck, LifecycleEvent, ProtocolDecision,
    RevokeCompletion, ScrubResult, ScrubState, SeenEvent, StateTag,
};
use super::preflight::PreflightModel;
use super::restart::{
    EventId, FailureReason, LeaseId, MAX_GENERATION_HISTORY, MAX_IN_FLIGHT,
    MAX_PROTOCOL_EVENT_HISTORY, StateTransitionError,
};
use super::{LeaseMachine, LeaseState};

impl LeaseMachine {
    /// Begins one bounded I/O operation while the exact host grant is active.
    pub fn begin_io(&mut self) -> Result<(), FailureReason> {
        // Generation 0 is not a grant: checkpoint load refuses
        // `generation == 0` (see the `checkpoint.generation == 0` reject), so
        // `Granted(0)` cannot exist and an absent active lease can never
        // satisfy this comparison. The `0` is the unreachable arm of a
        // refusal, not a licence to run with no lease.
        if self.lease_state != LeaseState::Granted(self.active_generation().unwrap_or(0)) {
            return Err(FailureReason::IoNotGranted);
        }
        if self.in_flight >= MAX_IN_FLIGHT {
            self.unknown_in_flight = true;
            return Err(FailureReason::UnknownInFlight);
        }
        self.in_flight += 1;
        Ok(())
    }

    /// Sets a deterministic in-flight count for a test or adapter boundary.
    pub fn set_in_flight(&mut self, count: u32) -> Result<(), FailureReason> {
        if count > MAX_IN_FLIGHT {
            self.unknown_in_flight = true;
            return Err(FailureReason::UnknownInFlight);
        }
        self.in_flight = count;
        Ok(())
    }

    pub fn complete_io(&mut self) -> Result<(), FailureReason> {
        if self.in_flight == 0 {
            self.unknown_in_flight = true;
            return Err(FailureReason::UnknownInFlight);
        }
        self.in_flight -= 1;
        Ok(())
    }

    pub fn set_callbacks_pending(&mut self, count: u32) -> Result<(), FailureReason> {
        if count > MAX_IN_FLIGHT {
            self.unknown_in_flight = true;
            return Err(FailureReason::UnknownInFlight);
        }
        self.callbacks_pending = count;
        Ok(())
    }

    pub fn mark_unknown_in_flight(&mut self) {
        self.unknown_in_flight = true;
    }

    /// Records the result of the guest-visible scrub.  No bytes are touched by
    /// this pure module; `Succeeded` is evidence supplied by its owner.
    pub fn scrub_guest_data(&mut self, result: ScrubResult) -> ProtocolDecision {
        self.scrub_state = result.into();
        if result == ScrubResult::Failed {
            return self.fail_for_active(FailureReason::ScrubFailed);
        }
        ProtocolDecision::Noop
    }

    /// Attempts to finish a revoke drain before its host deadline.
    pub fn drain(&mut self, now: u64) -> ProtocolDecision {
        let Some(active) = self.active_lease.as_ref() else {
            return self.fail_for_event(
                None,
                None,
                None,
                FailureReason::StateTransition(StateTransitionError::IllegalTransition {
                    expected: Some(StateTag::Granted),
                    actual: self.lease_state.tag(),
                }),
            );
        };
        let Some(revoke) = active.revoke.as_ref() else {
            return self.fail_for_active(FailureReason::StateTransition(
                StateTransitionError::IllegalTransition {
                    expected: Some(StateTag::Quiescing),
                    actual: self.lease_state.tag(),
                },
            ));
        };
        if self.lease_state != LeaseState::Quiescing(active.generation) {
            return self.fail_for_active(FailureReason::StateTransition(
                StateTransitionError::IllegalTransition {
                    expected: Some(StateTag::Quiescing),
                    actual: self.lease_state.tag(),
                },
            ));
        }
        if now >= revoke.deadline {
            return self.fail_for_active(FailureReason::DrainTimeout);
        }
        if self.unknown_in_flight {
            return self.fail_for_active(FailureReason::UnknownInFlight);
        }
        if self.in_flight != 0 {
            return self.fail_for_active(FailureReason::InFlightNotDrained);
        }
        if self.callbacks_pending != 0 {
            return self.fail_for_active(FailureReason::CallbackNotDrained);
        }
        match self.scrub_state {
            ScrubState::Pending => return ProtocolDecision::Blocked(FailureReason::ScrubPending),
            ScrubState::Failed => return self.fail_for_active(FailureReason::ScrubFailed),
            ScrubState::Succeeded => {}
        }
        let ack = DrainAck {
            lease_id: active.lease_id.clone(),
            generation: active.generation,
            event_id: revoke.event_id.clone(),
            in_flight: self.in_flight,
            callbacks_pending: self.callbacks_pending,
            scrubbed: true,
        };
        self.sent_drain_ack = true;
        self.lease_state = LeaseState::Drained(active.generation);
        ProtocolDecision::DrainAck(ack)
    }

    /// Host completion is required after `DRAIN_ACK`; absence or contradiction
    /// fails closed rather than assuming physical host zeroing.
    pub fn confirm_revoke(&mut self, completion: RevokeCompletion) -> ProtocolDecision {
        if self
            .completed_revokes
            .iter()
            .any(|seen| seen == &completion)
        {
            return ProtocolDecision::Noop;
        }
        let Some(active) = self.active_lease.as_ref() else {
            return self.fail_for_event(
                Some(completion.lease_id),
                Some(completion.generation),
                Some(completion.event_id),
                FailureReason::MissingRevokeCompletion,
            );
        };
        let Some(revoke) = active.revoke.as_ref() else {
            return self.fail_for_active(FailureReason::MissingRevokeCompletion);
        };
        if self.lease_state != LeaseState::Drained(active.generation)
            || completion.lease_id != active.lease_id
            || completion.generation != active.generation
            || completion.event_id != revoke.event_id
        {
            return self.fail_for_event(
                Some(completion.lease_id),
                Some(completion.generation),
                Some(completion.event_id),
                FailureReason::MissingRevokeCompletion,
            );
        }
        if self.completed_revokes.len() >= MAX_PROTOCOL_EVENT_HISTORY {
            return self.fail_for_active(FailureReason::MalformedRecord);
        }
        self.completed_revokes.push(completion);
        self.lease_state = LeaseState::Revoked;
        ProtocolDecision::Revoked
    }

    /// Completes local cleanup after `REVOKED` or `FAILED`.
    pub fn complete_cleanup(&mut self) {
        if self.active_lease.is_some() && self.scrub_state != ScrubState::Succeeded {
            return;
        }
        if matches!(
            self.lease_state,
            LeaseState::Revoked | LeaseState::Failed(_)
        ) {
            self.pending_grant = None;
            self.active_lease = None;
            self.in_flight = 0;
            self.callbacks_pending = 0;
            self.unknown_in_flight = false;
            self.scrub_state = ScrubState::Pending;
            self.sent_drain_ack = false;
            self.lease_state = LeaseState::Absent;
        }
    }

    /// A crash/restart always discards the active lease but retains generation
    /// history so the old host identity cannot be replayed.
    pub fn restart(&mut self) {
        self.pending_grant = None;
        self.active_lease = None;
        self.seen_events.clear();
        self.accepted_grant_acks.clear();
        self.completed_revokes.clear();
        self.in_flight = 0;
        self.callbacks_pending = 0;
        self.unknown_in_flight = false;
        self.scrub_state = ScrubState::Pending;
        self.sent_drain_ack = false;
        self.lease_state = LeaseState::Absent;
        self.preflight = PreflightModel::new();
    }

    /// Invalidates the current lease for reset, channel, liveness, or crash.
    pub fn fail_closed(&mut self, event: LifecycleEvent) -> ProtocolDecision {
        let reason = match event {
            LifecycleEvent::Reset => FailureReason::Reset,
            LifecycleEvent::ChannelLoss => FailureReason::ChannelLoss,
            LifecycleEvent::LeaseExpired => FailureReason::LeaseExpired,
            LifecycleEvent::WslRestart => FailureReason::WslRestart,
            LifecycleEvent::Suspend => FailureReason::Suspend,
            LifecycleEvent::DriverUpgrade => FailureReason::DriverUpgrade,
            LifecycleEvent::GuestCrash => FailureReason::GuestCrash,
        };
        if self.active_lease.is_none() && self.pending_grant.is_none() {
            self.lease_state = LeaseState::Failed(reason);
            return ProtocolDecision::FailAck(FailAck {
                lease_id: None,
                generation: None,
                event_id: None,
                reason,
            });
        }
        self.fail_for_active(reason)
    }

    pub(super) fn fail_for_active(&mut self, reason: FailureReason) -> ProtocolDecision {
        let (lease_id, generation, event_id) = if let Some(active) = &self.active_lease {
            (
                Some(active.lease_id.clone()),
                Some(active.generation),
                Some(
                    active
                        .revoke
                        .as_ref()
                        .map_or(&active.grant_event_id, |revoke| &revoke.event_id)
                        .clone(),
                ),
            )
        } else if let Some(grant) = &self.pending_grant {
            (
                Some(grant.lease_id.clone()),
                Some(grant.generation),
                Some(grant.event_id.clone()),
            )
        } else {
            (None, None, None)
        };
        self.lease_state = LeaseState::Failed(reason);
        ProtocolDecision::FailAck(FailAck {
            lease_id,
            generation,
            event_id,
            reason,
        })
    }

    pub(super) fn fail_for_event(
        &mut self,
        lease_id: Option<LeaseId>,
        generation: Option<u64>,
        event_id: Option<EventId>,
        reason: FailureReason,
    ) -> ProtocolDecision {
        self.pending_grant = None;
        self.lease_state = LeaseState::Failed(reason);
        ProtocolDecision::FailAck(FailAck {
            lease_id,
            generation,
            event_id,
            reason,
        })
    }

    pub(super) fn fail_restart_restore(&mut self, reason: FailureReason) -> FailureReason {
        self.pending_grant = None;
        self.active_lease = None;
        self.lease_state = LeaseState::Failed(reason);
        reason
    }

    pub(super) fn active_generation(&self) -> Option<u64> {
        self.active_lease.as_ref().map(|lease| lease.generation)
    }

    pub(super) fn validate_generation(
        &self,
        lease_id: &LeaseId,
        generation: u64,
    ) -> Result<(), FailureReason> {
        if let Some((_, previous)) = self
            .generation_history
            .iter()
            .find(|(known_lease, _)| known_lease == lease_id)
        {
            if generation <= *previous {
                return Err(FailureReason::StateTransition(
                    StateTransitionError::StaleGeneration {
                        provided: generation,
                        expected: *previous,
                    },
                ));
            }
            if generation != previous.saturating_add(1) {
                return Err(FailureReason::GenerationGap);
            }
            return Ok(());
        }
        if self.generation_history.len() >= MAX_GENERATION_HISTORY {
            return Err(FailureReason::MalformedRecord);
        }
        Ok(())
    }

    pub(super) fn remember_generation(&mut self, lease_id: LeaseId, generation: u64) {
        if let Some((_, previous)) = self
            .generation_history
            .iter_mut()
            .find(|(known_lease, _)| known_lease == &lease_id)
        {
            *previous = generation;
            return;
        }
        if self.generation_history.len() < MAX_GENERATION_HISTORY {
            self.generation_history.push((lease_id, generation));
        }
    }

    pub(super) fn register_event(&mut self, fingerprint: EventFingerprint) -> EventRegistration {
        let event_id = match &fingerprint {
            EventFingerprint::Grant(grant) => grant.event_id.clone(),
            EventFingerprint::Revoke(revoke) => revoke.event_id.clone(),
        };
        if let Some(seen) = self
            .seen_events
            .iter()
            .find(|seen| seen.event_id == event_id)
        {
            if seen.fingerprint == fingerprint {
                return EventRegistration::Duplicate;
            }
            return EventRegistration::Conflict;
        }
        if self.seen_events.len() >= MAX_PROTOCOL_EVENT_HISTORY {
            return EventRegistration::Overflow;
        }
        self.seen_events.push(SeenEvent {
            event_id,
            fingerprint,
        });
        EventRegistration::New
    }
}
