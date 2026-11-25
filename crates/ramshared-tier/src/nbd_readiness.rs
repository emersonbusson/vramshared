// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-tier/src/nbd_readiness.rs
//! Pure WSL2 NBD product capability-readiness policy.
//!
//! This module does not inspect the host or perform lifecycle actions. Callers
//! provide a fresh observation and receive one stable state and reason. The
//! outer lifecycle remains responsible for swapoff-first teardown. A separate
//! transport-admission gate may still refuse activation on a platform whose
//! NBD swap liveness is not qualified.
//!

/// One mebibyte in bytes.
pub const MIB_BYTES: u64 = 1024 * 1024;
/// The minimum lower-tier margin required for NBD demotion.
pub const MIN_LOWER_TIER_MARGIN_BYTES: u64 = 512 * MIB_BYTES;

/// Product-level state, distinct from the daemon's internal lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductState {
    /// The managed NBD swap and daemon are both absent after a safe teardown.
    ProductOff,
    /// All gates pass for the NBD product, including active identity when present.
    Ready,
    /// A deterministic gate failed; no retry or mutation is authorized.
    Blocked,
}

impl ProductState {
    /// Stable public spelling for status output.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProductOff => "PRODUCT_OFF",
            Self::Ready => "READY",
            Self::Blocked => "BLOCKED",
        }
    }
}

/// Product transport values. There is deliberately no ublk product value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductTransport {
    /// The WSL2 product transport when separate admission policy permits it.
    Nbd,
    /// No managed NBD transport is currently active.
    None,
}

impl ProductTransport {
    /// Stable public spelling for status output.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Nbd => "nbd",
            Self::None => "none",
        }
    }
}

/// Result of one observed external gate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Gate {
    /// The observed gate passed.
    Pass,
    /// The observed gate failed.
    Fail,
    /// The gate could not be measured safely.
    Unknown,
    /// The gate has no subject in this observation, such as binary identity
    /// while no daemon is running.
    NotApplicable,
}

/// Explicit authorization status for a requested mutation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Approval {
    /// The operation is read-only and does not need approval.
    NotRequired,
    /// Current scoped approval was supplied by the caller.
    Present,
    /// A mutation was requested without current scoped approval.
    Missing,
}

/// Requested operation. This policy only plans or refuses; it never mutates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    /// Inspect current state without host mutation.
    ReadOnly,
    /// Install a sealed product release.
    Install,
    /// Start the NBD cascade.
    Activate,
    /// Stop the NBD cascade after swapoff succeeds.
    Deactivate,
    /// Retire the exact legacy ublk product service wiring.
    RetireLegacyUblk,
}

impl Operation {
    const fn is_mutating(self) -> bool {
        !matches!(self, Self::ReadOnly)
    }
}

/// Identity of the measured lower-tier sink.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LowerTierSink {
    /// One explicit lower-tier identity was observed.
    Known(String),
    /// More than one candidate could own the claimed capacity.
    Ambiguous,
}

/// Capacity observation supplied by the owning filesystem/swap layer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CapacitySample {
    /// No lower-tier sink or fresh capacity reading is available.
    Unknown,
    /// The lower-tier measurement is older than the caller's freshness bound.
    Stale,
    /// A current free absorbable-capacity measurement.
    Observed {
        /// Exactly one sink identity.
        sink: LowerTierSink,
        /// Capacity available to absorb NBD pages now, not nominal size.
        free_absorbable_bytes: u64,
        /// Filesystem or swap allocation alignment used by this measurement.
        alignment_bytes: u64,
    },
}

/// Stable refusal classifications. These are deterministic and never retried
/// by this policy layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefusalCode {
    /// The product transport is missing or is not NBD for an active NBD tier.
    TransportMustBeNbd,
    /// A legacy ublk product unit or device is still active.
    LegacyUblkProductActive,
    /// A legacy ublk product unit or device could not be observed.
    LegacyUblkProductUnknown,
    /// The NBD swap presence could not be observed.
    NbdSwapActiveUnknown,
    /// The NBD swap and daemon presence disagree.
    NbdLifecycleIncomplete,
    /// The sealed-release gate failed.
    ReleaseGateFailed,
    /// The sealed-release gate could not be measured.
    ReleaseGateUnknown,
    /// Relay `--check` failed.
    RelayGateFailed,
    /// Relay `--check` could not be measured.
    RelayGateUnknown,
    /// A live daemon does not match the selected sealed binary.
    BinaryMatchFailed,
    /// A live daemon needs a binary identity result but none is available.
    BinaryMatchRequired,
    /// Binary identity data is inconsistent or unreadable.
    BinaryMatchUnknown,
    /// The configured NBD logical capacity is zero.
    InvalidVramSize,
    /// Capacity arithmetic cannot represent the required lower-tier size.
    CapacityOverflow,
    /// No exact lower-tier identity was supplied.
    LowerTierSinkUnknown,
    /// The lower-tier owner is ambiguous.
    LowerTierSinkAmbiguous,
    /// The capacity sample is stale.
    LowerTierMeasurementStale,
    /// The lower-tier measurement does not respect its allocation alignment.
    LowerTierAlignmentInvalid,
    /// The free lower-tier capacity is below the required demotion sink.
    LowerTierShortfall,
    /// A requested mutation lacks a current scoped approval.
    ApprovalMissing,
    /// A reboot, WSL shutdown, or equivalent restart action was requested.
    RebootRequested,
}

impl RefusalCode {
    /// Stable public spelling for status output.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TransportMustBeNbd => "TRANSPORT_MUST_BE_NBD",
            Self::LegacyUblkProductActive => "LEGACY_UBLK_PRODUCT_ACTIVE",
            Self::LegacyUblkProductUnknown => "LEGACY_UBLK_PRODUCT_UNKNOWN",
            Self::NbdSwapActiveUnknown => "NBD_SWAP_ACTIVE_UNKNOWN",
            Self::NbdLifecycleIncomplete => "NBD_LIFECYCLE_INCOMPLETE",
            Self::ReleaseGateFailed => "RELEASE_GATE_FAILED",
            Self::ReleaseGateUnknown => "RELEASE_GATE_UNKNOWN",
            Self::RelayGateFailed => "RELAY_GATE_FAILED",
            Self::RelayGateUnknown => "RELAY_GATE_UNKNOWN",
            Self::BinaryMatchFailed => "BINARY_MATCH_FAILED",
            Self::BinaryMatchRequired => "BINARY_MATCH_REQUIRED",
            Self::BinaryMatchUnknown => "BINARY_MATCH_UNKNOWN",
            Self::InvalidVramSize => "INVALID_VRAM_SIZE",
            Self::CapacityOverflow => "CAPACITY_OVERFLOW",
            Self::LowerTierSinkUnknown => "LOWER_TIER_SINK_UNKNOWN",
            Self::LowerTierSinkAmbiguous => "LOWER_TIER_SINK_AMBIGUOUS",
            Self::LowerTierMeasurementStale => "LOWER_TIER_MEASUREMENT_STALE",
            Self::LowerTierAlignmentInvalid => "LOWER_TIER_ALIGNMENT_INVALID",
            Self::LowerTierShortfall => "LOWER_TIER_SHORTFALL",
            Self::ApprovalMissing => "APPROVAL_MISSING",
            Self::RebootRequested => "REBOOT_REQUESTED",
        }
    }
}

/// Why the product state was selected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadinessReason {
    /// No state evaluation was requested.
    NotEvaluated,
    /// The product is intentionally quiescent and all observed gates passed.
    ProductOff,
    /// Every required gate passed.
    AllGatesPass,
    /// A deterministic refusal selected `BLOCKED`.
    Refusal(RefusalCode),
}

impl ReadinessReason {
    /// Stable public spelling for status output.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotEvaluated => "not_evaluated",
            Self::ProductOff => "product_off",
            Self::AllGatesPass => "all_gates_pass",
            Self::Refusal(code) => code.as_str(),
        }
    }
}

/// Fresh observations consumed by [`evaluate_product`].
///
/// Every swap-table-derived field is a three-state witness: `Some(v)` is a
/// measured value, `None` is a probe that never took (the swap table was
/// unreadable). `None` is neither a backing nor a refutation and is refused
/// before any positive classification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductInput {
    /// Reported product transport. `None` when the swap table was unreadable.
    pub transport: Option<ProductTransport>,
    /// A managed NBD device is still listed in the active swap set.
    /// `Some(true)` measured one present, `Some(false)` measured its absence,
    /// `None` never took the probe.
    pub nbd_swap_active: Option<bool>,
    /// The exact managed daemon identity is alive.
    pub daemon_running: bool,
    /// An active legacy ublk service or product device was observed.
    /// `Some(true)` measured one present, `Some(false)` measured its absence,
    /// `None` never took the probe.
    pub legacy_ublk_product_active: Option<bool>,
    /// Sealed version ownership, mode, manifest, and selector result.
    pub release_gate: Gate,
    /// Read-only Relay `--check` result.
    pub relay_gate: Gate,
    /// Selected-release BINARY_MATCH result for an active daemon.
    pub binary_match: Gate,
    /// Fresh lower-tier free-capacity observation.
    pub capacity: CapacitySample,
    /// Configured logical NBD tier size.
    pub vram_bytes: u64,
    /// Requested action classification.
    pub operation: Operation,
    /// Current scoped approval status.
    pub approval: Approval,
    /// Any forbidden reboot, shutdown, or termination request.
    pub reboot_requested: bool,
}

/// A deterministic evaluation result. `mutation_permitted` only states that
/// a separately implemented outer lifecycle has the required approval and
/// gates; this module itself performs no mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductDecision {
    /// Product status.
    pub state: ProductState,
    /// Stable successful or refusal reason.
    pub reason: ReadinessReason,
    /// Deterministic gate failures are never retried by this policy.
    pub retry_allowed: bool,
    /// Every evaluation is exactly one bounded attempt.
    pub attempts: u8,
    /// An approved mutating operation may advance to its outer lifecycle.
    pub mutation_permitted: bool,
}

impl ProductDecision {
    fn blocked(reason: RefusalCode) -> Self {
        Self {
            state: ProductState::Blocked,
            reason: ReadinessReason::Refusal(reason),
            retry_allowed: false,
            attempts: 1,
            mutation_permitted: false,
        }
    }

    fn accepted(state: ProductState, reason: ReadinessReason, operation: Operation) -> Self {
        Self {
            state,
            reason,
            retry_allowed: false,
            attempts: 1,
            mutation_permitted: operation.is_mutating(),
        }
    }
}

/// Computes the exact required free lower-tier capacity:
/// `V + max(ceil(0.10 * V), 512 MiB)`.
pub fn minimum_lower_tier_bytes(vram_bytes: u64) -> Result<u64, RefusalCode> {
    if vram_bytes == 0 {
        return Err(RefusalCode::InvalidVramSize);
    }

    let ten_percent = vram_bytes
        .checked_div(10)
        .and_then(|whole| whole.checked_add(u64::from(!vram_bytes.is_multiple_of(10))))
        .ok_or(RefusalCode::CapacityOverflow)?;
    let margin = ten_percent.max(MIN_LOWER_TIER_MARGIN_BYTES);
    vram_bytes
        .checked_add(margin)
        .ok_or(RefusalCode::CapacityOverflow)
}

/// Validates a fresh, exact lower-tier sink against the NBD demotion formula.
/// The returned number is the required free lower-tier capacity in bytes.
pub fn validate_lower_tier_capacity(
    vram_bytes: u64,
    sample: &CapacitySample,
) -> Result<u64, RefusalCode> {
    let required = minimum_lower_tier_bytes(vram_bytes)?;
    match sample {
        CapacitySample::Unknown => Err(RefusalCode::LowerTierSinkUnknown),
        CapacitySample::Stale => Err(RefusalCode::LowerTierMeasurementStale),
        CapacitySample::Observed {
            sink,
            free_absorbable_bytes,
            alignment_bytes,
        } => {
            match sink {
                LowerTierSink::Known(identity) if !identity.trim().is_empty() => {}
                LowerTierSink::Known(_) => return Err(RefusalCode::LowerTierSinkUnknown),
                LowerTierSink::Ambiguous => return Err(RefusalCode::LowerTierSinkAmbiguous),
            }
            if *alignment_bytes == 0 || free_absorbable_bytes % alignment_bytes != 0 {
                return Err(RefusalCode::LowerTierAlignmentInvalid);
            }
            if *free_absorbable_bytes < required {
                return Err(RefusalCode::LowerTierShortfall);
            }
            Ok(required)
        }
    }
}

/// Evaluates the NBD product state from one fresh observation.
///
/// The order is fail-closed: forbidden host requests and missing approval
/// return before any positive classification; an unobserved swap-table fact
/// is refused before any positive classification that would need it;
/// lifecycle disagreement, legacy ublk ownership, and every required
/// read-only gate then block readiness.
pub fn evaluate_product(input: &ProductInput) -> ProductDecision {
    if input.reboot_requested {
        return ProductDecision::blocked(RefusalCode::RebootRequested);
    }
    if input.operation.is_mutating() && input.approval != Approval::Present {
        return ProductDecision::blocked(RefusalCode::ApprovalMissing);
    }
    // An unobserved swap table is never a measured absence. Refuse it before
    // the arms below would read `None` as `false` and publish a positive
    // classification the probe cannot back.
    if input.legacy_ublk_product_active.is_none() {
        return ProductDecision::blocked(RefusalCode::LegacyUblkProductUnknown);
    }
    if input.nbd_swap_active.is_none() {
        return ProductDecision::blocked(RefusalCode::NbdSwapActiveUnknown);
    }
    let nbd_swap_active = input.nbd_swap_active == Some(true);
    if input.legacy_ublk_product_active == Some(true) {
        return ProductDecision::blocked(RefusalCode::LegacyUblkProductActive);
    }
    if nbd_swap_active != input.daemon_running {
        return ProductDecision::blocked(RefusalCode::NbdLifecycleIncomplete);
    }
    if nbd_swap_active && input.transport != Some(ProductTransport::Nbd) {
        return ProductDecision::blocked(RefusalCode::TransportMustBeNbd);
    }
    if let Some(reason) = release_refusal(input.release_gate) {
        return ProductDecision::blocked(reason);
    }
    if let Err(reason) = validate_lower_tier_capacity(input.vram_bytes, &input.capacity) {
        return ProductDecision::blocked(reason);
    }
    if let Some(reason) = relay_refusal(input.relay_gate) {
        return ProductDecision::blocked(reason);
    }
    if let Some(reason) = binary_refusal(input.binary_match, input.daemon_running) {
        return ProductDecision::blocked(reason);
    }

    if !nbd_swap_active {
        return ProductDecision::accepted(
            ProductState::ProductOff,
            ReadinessReason::ProductOff,
            input.operation,
        );
    }

    ProductDecision::accepted(
        ProductState::Ready,
        ReadinessReason::AllGatesPass,
        input.operation,
    )
}

fn release_refusal(gate: Gate) -> Option<RefusalCode> {
    match gate {
        Gate::Pass => None,
        Gate::Fail => Some(RefusalCode::ReleaseGateFailed),
        Gate::Unknown | Gate::NotApplicable => Some(RefusalCode::ReleaseGateUnknown),
    }
}

fn relay_refusal(gate: Gate) -> Option<RefusalCode> {
    match gate {
        Gate::Pass => None,
        Gate::Fail => Some(RefusalCode::RelayGateFailed),
        Gate::Unknown | Gate::NotApplicable => Some(RefusalCode::RelayGateUnknown),
    }
}

fn binary_refusal(gate: Gate, daemon_running: bool) -> Option<RefusalCode> {
    match (gate, daemon_running) {
        (Gate::Pass, _) => None,
        (Gate::NotApplicable, false) => None,
        (Gate::Fail, _) => Some(RefusalCode::BinaryMatchFailed),
        (Gate::Unknown, _) => Some(RefusalCode::BinaryMatchUnknown),
        (Gate::NotApplicable, true) => Some(RefusalCode::BinaryMatchRequired),
    }
}

#[cfg(test)]
mod tests;

/// Semantic error for NBD probe connection failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NbdReadinessError {
    /// Connection was refused (ECONNREFUSED).
    ConnectionRefused,
    /// Connection timed out (ETIMEDOUT).
    Timeout,
    /// Any other IO error kind.
    Other(std::io::ErrorKind),
}

impl core::fmt::Display for NbdReadinessError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ConnectionRefused => f.write_str("NBD connection refused"),
            Self::Timeout => f.write_str("NBD connection timed out"),
            Self::Other(kind) => write!(f, "NBD IO error: {:?}", kind),
        }
    }
}

impl core::error::Error for NbdReadinessError {}

impl From<std::io::ErrorKind> for NbdReadinessError {
    fn from(kind: std::io::ErrorKind) -> Self {
        match kind {
            std::io::ErrorKind::ConnectionRefused => Self::ConnectionRefused,
            std::io::ErrorKind::TimedOut => Self::Timeout,
            _ => Self::Other(kind),
        }
    }
}

#[cfg(test)]
mod nbd_readiness_error_tests {
    use super::*;

    #[test]
    fn nbd_readiness_error_mappings() {
        assert_eq!(
            NbdReadinessError::from(std::io::ErrorKind::ConnectionRefused),
            NbdReadinessError::ConnectionRefused
        );
        assert_eq!(
            NbdReadinessError::from(std::io::ErrorKind::TimedOut),
            NbdReadinessError::Timeout
        );
        assert_eq!(
            NbdReadinessError::from(std::io::ErrorKind::InvalidData),
            NbdReadinessError::Other(std::io::ErrorKind::InvalidData)
        );
    }

    #[test]
    fn nbd_readiness_error_display_names_every_variant() {
        assert_eq!(
            NbdReadinessError::ConnectionRefused.to_string(),
            "NBD connection refused"
        );
        assert_eq!(
            NbdReadinessError::Timeout.to_string(),
            "NBD connection timed out"
        );
        assert_eq!(
            NbdReadinessError::Other(std::io::ErrorKind::UnexpectedEof).to_string(),
            "NBD IO error: UnexpectedEof"
        );
    }
}
