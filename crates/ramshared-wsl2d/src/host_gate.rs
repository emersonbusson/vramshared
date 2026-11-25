// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/host_gate.rs
//! Absorbed gate logic from `ramshared-host-gate.sh`.
//!
//! Origin manifest validation, guardian health check, safe-mode gate,
//! and lease minting — all in Rust, no scripts.
//!

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Errors from gate evaluation.
#[derive(Debug, PartialEq, Eq)]
pub enum GateError {
    ManifestInvalid(String),
    ManifestHashMismatch,
    GuardianStale,
    GuardianUnhealthy,
    SafeModeForeignBootId,
    SafeModeBlocked,
    LeaseDenied(String),
    OriginAuthorityRevoked,
}

impl std::fmt::Display for GateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ManifestInvalid(e) => write!(f, "origin manifest invalid: {e}"),
            Self::ManifestHashMismatch => write!(f, "origin manifest SHA-256 mismatch"),
            Self::GuardianStale => write!(f, "guardian health proof is stale"),
            Self::GuardianUnhealthy => write!(f, "guardian reports unhealthy"),
            Self::SafeModeForeignBootId => write!(f, "safe-mode gate: foreign boot_id"),
            Self::SafeModeBlocked => write!(f, "safe-mode gate: blocked"),
            Self::LeaseDenied(e) => write!(f, "lease denied: {e}"),
            Self::OriginAuthorityRevoked => write!(f, "origin authority revoked"),
        }
    }
}

impl std::error::Error for GateError {}

/// Validated and sealed origin configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedOrigin {
    pub logical_capacity_mib: u64,
    pub partuuid: String,
    pub origin_vhdx: String,
}

/// Lease token minted after all gates pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseToken {
    pub lease_id: u32,
    pub deadline_ms: u64,
}

/// Safe-mode decision after gate evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafeModeDecision {
    Allow,
    Deny,
}

/// Guardian health status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardianHealthStatus {
    pub timestamp_ms: u64,
    pub healthy: bool,
}

/// Validate origin manifest bytes and expected SHA-256 hash.
///
/// Absorbs the Python manifest validation from `ramshared-host-gate.sh`.
/// Validates size bounds, SHA-256 integrity, and required fields.
pub fn validate_origin_manifest(
    data: &[u8],
    expected_sha256: &str,
) -> Result<SealedOrigin, GateError> {
    // Size bound: 1..=64KB (matches ORIGIN_MANIFEST_MAX_BYTES)
    if data.is_empty() || data.len() > 64 * 1024 {
        return Err(GateError::ManifestInvalid(format!(
            "size {} out of bounds",
            data.len()
        )));
    }

    // SHA-256 integrity check
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    let computed: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if !computed.eq_ignore_ascii_case(expected_sha256) {
        return Err(GateError::ManifestHashMismatch);
    }

    // Parse as JSON and extract required fields (strip UTF-8 BOM if present)
    let json_bytes = data.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(data);
    let value: serde_json::Value = serde_json::from_slice(json_bytes)
        .map_err(|e| GateError::ManifestInvalid(e.to_string()))?;

    let logical_capacity_mib = value
        .get("logical_capacity_mib")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| GateError::ManifestInvalid("missing logical_capacity_mib".into()))?;

    let partuuid = value
        .get("partuuid")
        .and_then(|v| v.as_str())
        .ok_or_else(|| GateError::ManifestInvalid("missing partuuid".into()))?
        .to_string();

    let origin_vhdx = value
        .get("origin_vhdx")
        .and_then(|v| v.as_str())
        .ok_or_else(|| GateError::ManifestInvalid("missing origin_vhdx".into()))?
        .to_string();

    if logical_capacity_mib == 0 {
        return Err(GateError::ManifestInvalid(
            "logical_capacity_mib must be > 0".into(),
        ));
    }

    Ok(SealedOrigin {
        logical_capacity_mib,
        partuuid,
        origin_vhdx,
    })
}

/// Check guardian health proof freshness and status.
///
/// Absorbs the guardian health check from `ramshared-host-gate.sh`.
/// `max_age_sec` defaults to 60 (matching `guardian_proof_max_age_sec`).
pub fn check_guardian_health(
    health: &GuardianHealthStatus,
    max_age_sec: u64,
) -> Result<(), GateError> {
    if !health.healthy {
        return Err(GateError::GuardianUnhealthy);
    }

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64;

    let age_ms = now_ms.saturating_sub(health.timestamp_ms);
    if age_ms > max_age_sec * 1000 {
        return Err(GateError::GuardianStale);
    }

    Ok(())
}

/// Evaluate safe-mode gate.
///
/// Absorbs the safe-mode gate check from `ramshared-host-gate.sh`.
/// Refuses if boot_id is foreign (different from expected).
pub fn evaluate_safe_mode(
    safe_mode: bool,
    received_boot_id: &str,
    expected_boot_id: &str,
) -> Result<SafeModeDecision, GateError> {
    if received_boot_id != expected_boot_id {
        return Err(GateError::SafeModeForeignBootId);
    }
    if safe_mode {
        return Ok(SafeModeDecision::Deny);
    }
    Ok(SafeModeDecision::Allow)
}

/// Mint a lease token after all gates pass.
///
/// Absorbs the lease minting from `ramshared-host-gate.sh`.
/// Requires: valid origin manifest + guardian healthy + safe-mode allows.
pub fn mint_lease(
    origin: &SealedOrigin,
    guardian_ok: bool,
    safe_mode_decision: SafeModeDecision,
    lease_id: u32,
    ttl_ms: u64,
) -> Result<LeaseToken, GateError> {
    if !guardian_ok {
        return Err(GateError::LeaseDenied("guardian not healthy".into()));
    }
    if safe_mode_decision == SafeModeDecision::Deny {
        return Err(GateError::LeaseDenied("safe mode active".into()));
    }
    if origin.logical_capacity_mib == 0 {
        return Err(GateError::LeaseDenied("invalid origin capacity".into()));
    }

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64;

    Ok(LeaseToken {
        lease_id,
        deadline_ms: now_ms.saturating_add(ttl_ms),
    })
}

/// Check if a lease is expired at the current wall clock.
pub fn lease_expired(lease: &LeaseToken) -> bool {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64;
    lease_expired_at(lease, now_ms)
}

/// Check if a lease is expired at an explicit instant (testable, no sleep).
pub fn lease_expired_at(lease: &LeaseToken, now_ms: u64) -> bool {
    now_ms >= lease.deadline_ms
}

/// Control-plane state, matching the SPEC telemetry enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlPlaneState {
    Disconnected,
    Handshaking,
    VsockLeased,
    OriginOnly,
    SafeMode,
}

impl ControlPlaneState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Disconnected => "disconnected",
            Self::Handshaking => "handshaking",
            Self::VsockLeased => "vsock_leased",
            Self::OriginOnly => "origin_only",
            Self::SafeMode => "safe_mode",
        }
    }
}

/// Fail-closed authority for cache admission and origin I/O (ITEM-3, RF-3, RF-6).
///
/// Cache admission requires a live lease **and** a verified origin identity.
/// Origin I/O requires only the verified origin identity, so lease loss
/// revokes the cache without taking the authoritative path down. Losing the
/// origin identity blocks both paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlPlaneAuthority {
    state: ControlPlaneState,
    lease: Option<LeaseToken>,
    origin: Option<SealedOrigin>,
}

impl ControlPlaneAuthority {
    /// Start with no lease and no verified origin.
    pub fn disconnected() -> Self {
        Self {
            state: ControlPlaneState::Disconnected,
            lease: None,
            origin: None,
        }
    }

    pub fn state(&self) -> ControlPlaneState {
        self.state
    }

    pub fn lease(&self) -> Option<&LeaseToken> {
        self.lease.as_ref()
    }

    pub fn origin(&self) -> Option<&SealedOrigin> {
        self.origin.as_ref()
    }

    /// Install an already-verified sealed origin identity.
    ///
    /// Origin identity is established independently of the lease (for example
    /// by `read_sealed_origin_manifest` on the host, or by a manifest the
    /// caller has already validated). Installing it here is what makes RF-3
    /// expressible: `admit_origin_io` must be able to succeed before any lease
    /// exists and must survive lease loss and transport disconnect. Only
    /// [`Self::lose_origin_identity`] may take it away.
    ///
    /// A later [`Self::accept_lease`] still installs the origin it is given;
    /// that is the lease-coupled path and remains valid.
    pub fn adopt_verified_origin(&mut self, origin: SealedOrigin) {
        self.origin = Some(origin);
    }

    /// Begin a handshake attempt. Does not grant any authority.
    pub fn begin_handshake(&mut self) {
        self.state = ControlPlaneState::Handshaking;
    }

    /// Accept a freshly minted lease together with its verified origin.
    ///
    /// An already-expired token is refused: accepting it would advertise a
    /// lease that cannot admit anything.
    pub fn accept_lease(
        &mut self,
        lease: LeaseToken,
        origin: SealedOrigin,
        now_ms: u64,
    ) -> Result<(), GateError> {
        if lease_expired_at(&lease, now_ms) {
            return Err(GateError::LeaseDenied("lease already expired".into()));
        }
        self.lease = Some(lease);
        self.origin = Some(origin);
        self.state = ControlPlaneState::VsockLeased;
        Ok(())
    }

    /// Transport disconnect fails closed: enter safe mode and revoke cache
    /// authority. The verified origin identity is preserved so origin I/O
    /// can continue (RF-3).
    pub fn on_vsock_disconnect(&mut self) {
        self.state = ControlPlaneState::SafeMode;
        self.lease = None;
    }

    /// Lease loss revokes cache authority and leaves only the verified origin
    /// path (ITEM-3).
    pub fn on_lease_expired(&mut self) {
        self.lease = None;
        if self.state == ControlPlaneState::VsockLeased {
            self.state = ControlPlaneState::OriginOnly;
        }
    }

    /// No peer was reachable at startup: the authority keeps the verified
    /// origin and serves it without the cache (DT-9's file-only host face).
    ///
    /// Only moves from a pre-lease state. A plane that already reached
    /// `VsockLeased` (and lost it) or `SafeMode` must not be downgraded to
    /// `OriginOnly`: those transitions carry their own meaning, and this is
    /// the startup composition's "the bounded attempt found no peer" answer.
    pub fn enter_origin_only(&mut self) {
        if matches!(
            self.state,
            ControlPlaneState::Disconnected | ControlPlaneState::Handshaking
        ) {
            self.lease = None;
            self.state = ControlPlaneState::OriginOnly;
        }
    }

    /// Origin identity loss blocks every I/O path (ITEM-3 companion).
    pub fn lose_origin_identity(&mut self) {
        self.origin = None;
    }

    /// Cache admission: live lease + verified origin. Everything else is
    /// refused — there is no file fallback and no cached-only path.
    pub fn admit_cache(&self, now_ms: u64) -> Result<(), GateError> {
        if self.state == ControlPlaneState::SafeMode {
            return Err(GateError::LeaseDenied("safe mode after disconnect".into()));
        }
        if self.state != ControlPlaneState::VsockLeased {
            return Err(GateError::LeaseDenied(
                "cache admission requires a live vsock lease".into(),
            ));
        }
        let lease = self
            .lease
            .as_ref()
            .ok_or_else(|| GateError::LeaseDenied("no live lease".into()))?;
        if lease_expired_at(lease, now_ms) {
            return Err(GateError::LeaseDenied("lease expired".into()));
        }
        if self.origin.is_none() {
            return Err(GateError::OriginAuthorityRevoked);
        }
        Ok(())
    }

    /// Origin I/O admission: only the verified origin identity is required.
    /// Lease state (including expiry and safe mode) does not take the
    /// authoritative path down.
    pub fn admit_origin_io(&self) -> Result<(), GateError> {
        if self.origin.is_none() {
            return Err(GateError::OriginAuthorityRevoked);
        }
        Ok(())
    }
}

/// The shared control-plane authority plus the generation of its current
/// lease session (SPEC `control-plane-and-release-resilience` DT-4, RF-4).
///
/// A lease is minted, used and lost by one *session*. When a session ends its
/// watchdog and heartbeat threads can still be draining, and without a guard
/// their late writes (`on_lease_expired`, or the heartbeat's authority
/// overwrite) would clobber a freshly minted lease and flip the cache back to
/// `safe_mode`. [`Self::begin_session`] supersedes every earlier session: a
/// [`SessionAuthority`] write is a no-op once a newer session has begun.
///
/// Reads stay on the raw `Arc<Mutex<ControlPlaneAuthority>>` so the cache
/// admission keeps one view; only the session-bound writers are guarded.
#[derive(Clone)]
pub struct SharedAuthority {
    slot: Arc<std::sync::Mutex<ControlPlaneAuthority>>,
    current_generation: Arc<std::sync::atomic::AtomicU64>,
}

impl SharedAuthority {
    /// Start with no lease, no verified origin, and no session.
    pub fn new() -> Self {
        Self {
            slot: Arc::new(std::sync::Mutex::new(ControlPlaneAuthority::disconnected())),
            current_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// The unguarded read/write slot the cache admission consults.
    pub fn slot(&self) -> Arc<std::sync::Mutex<ControlPlaneAuthority>> {
        Arc::clone(&self.slot)
    }

    /// Begin a new lease session, superseding any earlier one.
    ///
    /// A prior [`SessionAuthority`] becomes stale the moment this returns, so
    /// its writes become no-ops (DT-4) — the stale-thread race cannot reach a
    /// newer lease.
    pub fn begin_session(&self) -> SessionAuthority {
        // The bump holds the slot lock so it cannot interleave between a
        // writer's generation check and its write: a session that is about to
        // publish either finishes first (a write before supersession) or sees
        // the new generation and refuses (DT-4 under the lock, not a
        // check-then-act window).
        let generation = {
            let _slot = self.slot.lock().unwrap_or_else(|error| error.into_inner());
            self.current_generation
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1
        };
        SessionAuthority {
            slot: Arc::clone(&self.slot),
            current_generation: Arc::clone(&self.current_generation),
            retired: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            generation,
        }
    }
}

impl Default for SharedAuthority {
    fn default() -> Self {
        Self::new()
    }
}

/// A generation-bound writer for [`SharedAuthority`] (SPEC DT-4, RF-4).
///
/// `publish` and `mutate` are no-ops after retirement or supersession. Stopping
/// a session must fence late acknowledgments even while its threads remain
/// owned and no replacement generation exists.
#[derive(Clone)]
pub struct SessionAuthority {
    slot: Arc<std::sync::Mutex<ControlPlaneAuthority>>,
    current_generation: Arc<std::sync::atomic::AtomicU64>,
    retired: Arc<std::sync::atomic::AtomicBool>,
    generation: u64,
}

impl SessionAuthority {
    /// Observe only this session and the exact authority slot it owns.
    /// Busy, poisoned, retired and superseded authorities refuse observation.
    pub(crate) fn observe_current<R>(
        &self,
        expected: &Arc<std::sync::Mutex<ControlPlaneAuthority>>,
        observe: impl FnOnce(&ControlPlaneAuthority) -> R,
    ) -> Option<R> {
        if !Arc::ptr_eq(&self.slot, expected) {
            return None;
        }
        let shared = self.slot.try_lock().ok()?;
        self.is_current().then(|| observe(&shared))
    }

    /// True while this session is neither retired nor superseded.
    ///
    /// Advisory for observers (the host-memory refresher). Writers must not
    /// branch on this and then write: [`Self::publish`], [`Self::mutate`] and
    /// [`Self::set_lease_lost_if_current`] re-check under the slot lock that
    /// [`SharedAuthority::begin_session`] holds while it bumps.
    pub fn is_current(&self) -> bool {
        !self.retired.load(std::sync::atomic::Ordering::Acquire)
            && self
                .current_generation
                .load(std::sync::atomic::Ordering::SeqCst)
                == self.generation
    }

    /// Permanently fence session writers and perform final cleanup atomically.
    /// Repeated cleanup is allowed within this generation, including after a
    /// child retired it, but cannot alter a replacement session's state.
    pub fn retire(&self, cleanup: impl FnOnce(&mut ControlPlaneAuthority)) -> bool {
        let mut shared = self.slot.lock().unwrap_or_else(|error| error.into_inner());
        if self
            .current_generation
            .load(std::sync::atomic::Ordering::SeqCst)
            != self.generation
        {
            return false;
        }
        self.retired
            .store(true, std::sync::atomic::Ordering::Release);
        cleanup(&mut shared);
        true
    }

    /// Advisory metadata read, fenced under the authority lock. Allocation
    /// effects still require their own current authority and scope checks.
    pub fn lease_is_current(&self, lease_id: u32) -> bool {
        let Ok(shared) = self.slot.lock() else {
            return false;
        };
        self.is_current()
            && shared.state() == ControlPlaneState::VsockLeased
            && shared
                .lease()
                .is_some_and(|lease| lease.lease_id == lease_id)
    }

    pub(crate) fn same_session(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.slot, &other.slot) && self.generation == other.generation
    }

    /// Publish this session's authority if it is still the live one.
    ///
    /// The generation is checked **under the slot lock**, the same lock
    /// [`SharedAuthority::begin_session`] holds while it bumps. A write can
    /// therefore never pass its check and land after a newer session began.
    pub fn publish(&self, authority: &ControlPlaneAuthority) -> bool {
        let Ok(mut shared) = self.slot.lock() else {
            return false;
        };
        if !self.is_current() {
            return false;
        }
        *shared = authority.clone();
        true
    }

    /// Mutate the shared authority if this session is still the live one.
    pub fn mutate(&self, f: impl FnOnce(&mut ControlPlaneAuthority)) -> bool {
        let Ok(mut shared) = self.slot.lock() else {
            return false;
        };
        if !self.is_current() {
            return false;
        }
        f(&mut shared);
        true
    }

    /// Publish the lease-loss signal if this session is still the live one.
    ///
    /// A superseded session's late loss must not mark the *newer* lease as
    /// lost, which would make the re-lease loop churn and the cache gate
    /// revoke a cache that still has a live lease (DT-4).
    pub fn set_lease_lost_if_current(&self, flag: &std::sync::atomic::AtomicBool) -> bool {
        // Same lock as `begin_session`: a superseded session cannot set the
        // shared loss flag after the newer session reset it.
        let _slot = self.slot.lock().unwrap_or_else(|error| error.into_inner());
        if !self.is_current() {
            return false;
        }
        flag.store(true, std::sync::atomic::Ordering::Release);
        true
    }
}

/// Gate lease minting on a successful control-plane connection.
///
/// There is no file fallback (RF-6 / SPEC §MODIFY): a connection failure
/// must never grant a lease. The mint closure runs only after `connect`
/// succeeds, so a failed connection cannot produce a token by construction.
pub fn lease_after_connect<T, E, F>(connect: Result<T, E>, mint: F) -> Result<LeaseToken, GateError>
where
    F: FnOnce(T) -> Result<LeaseToken, GateError>,
{
    let established =
        connect.map_err(|_| GateError::LeaseDenied("control-plane connection failed".into()))?;
    mint(established)
}

#[cfg(test)]
mod tests;
