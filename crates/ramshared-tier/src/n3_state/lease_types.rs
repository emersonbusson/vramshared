// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-tier/src/n3_state/lease_types.rs
//! Host-led lease protocol messages, states, and internal lease-tracking records.

use super::LeaseMachine;
use super::restart::{EventId, FailureReason, LeaseId, N3_SCHEMA_VERSION};

/// Host grant event.  It is validated as a whole before a lease is installed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Grant {
    pub contract_version: u16,
    pub lease_id: LeaseId,
    pub generation: u64,
    pub event_id: EventId,
    pub capacity_bytes: u64,
    pub host_epoch: u64,
    pub issued_at: u64,
    pub deadline: u64,
    pub expected_state: Option<StateTag>,
}

impl Grant {
    /// Convenience constructor for the current public schema.
    pub fn host(
        lease_id: LeaseId,
        generation: u64,
        event_id: EventId,
        capacity_bytes: u64,
        host_epoch: u64,
        issued_at: u64,
        deadline: u64,
    ) -> Self {
        Self::new(
            N3_SCHEMA_VERSION,
            lease_id,
            generation,
            event_id,
            capacity_bytes,
            host_epoch,
            issued_at,
            deadline,
        )
    }

    /// Creates a host grant event without applying it to a machine.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        contract_version: u16,
        lease_id: LeaseId,
        generation: u64,
        event_id: EventId,
        capacity_bytes: u64,
        host_epoch: u64,
        issued_at: u64,
        deadline: u64,
    ) -> Self {
        Self {
            contract_version,
            lease_id,
            generation,
            event_id,
            capacity_bytes,
            host_epoch,
            issued_at,
            deadline,
            expected_state: None,
        }
    }

    pub fn with_capacity_bytes(mut self, capacity_bytes: u64) -> Self {
        self.capacity_bytes = capacity_bytes;
        self
    }

    pub fn with_expected_state(mut self, expected_state: StateTag) -> Self {
        self.expected_state = Some(expected_state);
        self
    }
}

/// Host revoke event.  Revoke blocks new I/O before any drain decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Revoke {
    pub lease_id: LeaseId,
    pub generation: u64,
    pub event_id: EventId,
    pub deadline: u64,
    pub expected_state: Option<StateTag>,
}

impl Revoke {
    /// Convenience constructor for an exact active lease identity.
    pub fn host(lease_id: LeaseId, generation: u64, event_id: EventId, deadline: u64) -> Self {
        Self::new(lease_id, generation, event_id, deadline)
    }

    pub fn new(lease_id: LeaseId, generation: u64, event_id: EventId, deadline: u64) -> Self {
        Self {
            lease_id,
            generation,
            event_id,
            deadline,
            expected_state: None,
        }
    }

    pub fn with_expected_state(mut self, expected_state: StateTag) -> Self {
        self.expected_state = Some(expected_state);
        self
    }
}

/// Host completion after it has accepted a guest `DRAIN_ACK`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevokeCompletion {
    pub lease_id: LeaseId,
    pub generation: u64,
    pub event_id: EventId,
}

impl RevokeCompletion {
    pub fn new(lease_id: LeaseId, generation: u64, event_id: EventId) -> Self {
        Self {
            lease_id,
            generation,
            event_id,
        }
    }
}

/// Guest acknowledgement for a host grant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrantAck {
    pub contract_version: u16,
    pub lease_id: LeaseId,
    pub generation: u64,
    pub event_id: EventId,
}

/// Guest acknowledgement after all drain and privacy conditions are complete.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DrainAck {
    pub lease_id: LeaseId,
    pub generation: u64,
    pub event_id: EventId,
    pub in_flight: u32,
    pub callbacks_pending: u32,
    pub scrubbed: bool,
}

/// Guest failure acknowledgement.  It never asserts that the lease drained.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FailAck {
    pub lease_id: Option<LeaseId>,
    pub generation: Option<u64>,
    pub event_id: Option<EventId>,
    pub reason: FailureReason,
}

/// State tag used by optional event prior-state expectations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StateTag {
    Absent,
    Negotiating,
    Granted,
    Quiescing,
    Drained,
    Revoked,
    Failed,
}

/// Exact host-led lease lifecycle.  No preflight variant is represented here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LeaseState {
    /// No active or pending lease exists.
    Absent,
    /// A complete host grant is awaiting host acceptance of `GRANT_ACK`.
    Negotiating(u64),
    /// Host has accepted the guest acknowledgement for this generation.
    Granted(u64),
    /// Revoke has blocked new I/O and awaits drain/scrub.
    Quiescing(u64),
    /// Guest drain proof was sent; host completion is still required.
    Drained(u64),
    /// Host completed revoke; cleanup returns to `Absent`.
    Revoked,
    /// Fail-closed terminal state until cleanup or restart.
    Failed(FailureReason),
}

/// Descriptive aliases for integrations that call the model N3 state.
pub type N3State = LeaseState;
pub type N3StateMachine = LeaseMachine;

impl LeaseState {
    pub fn tag(self) -> StateTag {
        match self {
            Self::Absent => StateTag::Absent,
            Self::Negotiating(_) => StateTag::Negotiating,
            Self::Granted(_) => StateTag::Granted,
            Self::Quiescing(_) => StateTag::Quiescing,
            Self::Drained(_) => StateTag::Drained,
            Self::Revoked => StateTag::Revoked,
            Self::Failed(_) => StateTag::Failed,
        }
    }

    pub fn generation(self) -> Option<u64> {
        match self {
            Self::Negotiating(generation)
            | Self::Granted(generation)
            | Self::Quiescing(generation)
            | Self::Drained(generation) => Some(generation),
            Self::Absent | Self::Revoked | Self::Failed(_) => None,
        }
    }

    pub fn is_granted(self) -> bool {
        matches!(self, Self::Granted(_))
    }
}

/// Host/guest lifecycle events that invalidate a lease without a safe revoke.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleEvent {
    Reset,
    ChannelLoss,
    LeaseExpired,
    WslRestart,
    Suspend,
    DriverUpgrade,
    GuestCrash,
}

/// Explicit guest claims rejected at the N3 boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestClaim {
    PfnRange { start: u64, count: u64 },
    NumaNode(u32),
    DevicePrivate,
    PhysicalResidency,
}

/// Product or upstream choices intentionally owned by other SPECs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductTransport {
    Nbd,
    Ublk,
    WindowsDriver,
    UpstreamContribution,
}

/// Pure scrub result.  The model records completion; it never touches memory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScrubResult {
    Pending,
    Succeeded,
    Failed,
}

/// Host-originated event dispatcher for callers that want one event surface.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostEvent {
    Grant(Grant),
    GrantAckAccepted(GrantAck),
    Revoke(Revoke),
    RevokeCompleted(RevokeCompletion),
    Lifecycle(LifecycleEvent),
}

pub type ProtocolEvent = HostEvent;

/// Decision emitted by the pure protocol model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProtocolDecision {
    Noop,
    GrantAck(GrantAck),
    Accepted(LeaseState),
    BeginDrain,
    DrainAck(DrainAck),
    Revoked,
    FailAck(FailAck),
    Blocked(FailureReason),
    Refused(FailureReason),
}

/// Alias used by callers that call all output a guest decision.
pub type GuestDecision = ProtocolDecision;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum EventFingerprint {
    Grant(Grant),
    Revoke(Revoke),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SeenEvent {
    pub(super) event_id: EventId,
    pub(super) fingerprint: EventFingerprint,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ActiveLease {
    pub(super) lease_id: LeaseId,
    pub(super) generation: u64,
    pub(super) grant_event_id: EventId,
    pub(super) capacity_bytes: u64,
    pub(super) revoke: Option<Revoke>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ScrubState {
    Pending,
    Succeeded,
    Failed,
}

impl From<ScrubResult> for ScrubState {
    fn from(result: ScrubResult) -> Self {
        match result {
            ScrubResult::Pending => Self::Pending,
            ScrubResult::Succeeded => Self::Succeeded,
            ScrubResult::Failed => Self::Failed,
        }
    }
}

pub(super) enum EventRegistration {
    New,
    Duplicate,
    Conflict,
    Overflow,
}
