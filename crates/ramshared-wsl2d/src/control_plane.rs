// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/control_plane.rs
//! Product control-plane composition (native-vsock ITEM-3, RF-3, RF-6).
//!
//! Scope note — read this before changing the composition:
//!
//! This module implements the **transport and lease-maintenance** half of
//! ITEM-3: the [`ControlPlane`] trait, [`VsockControlPlane`], and
//! [`HeartbeatLoop`]. The production composition lives in `cache_admission.rs`
//! (`compose_cache_admission`): it builds the shared authority, adopts the
//! already-verified sealed origin, supervises bounded attempts to reach the
//! Windows peer, and hands the authority to
//! `ramshared_block::AuthoritativeOriginBackend`, whose cache admission
//! consults it (DT-9). A host with no vsock peer is `origin_only` and its
//! cache is refused — verified test case Finding A was settled by SPEC DT-9,
//! and this module no longer leaves that question open.
//!
//! This trait has exactly **one** production implementation,
//! [`VsockControlPlane`]. A file-backed control plane is not defined: DT-9
//! keeps the locally sealed origin manifest as a local proof (not a
//! transport) and removes the file lease/heartbeat path; a future fallback
//! needs its own design and tests.
//!
//! Fail-closed rules encoded here (invariant checksecurity and benchmark guidelines):
//!
//! - Every transport failure during handshake or heartbeat drives
//!   [`ControlPlaneAuthority::on_vsock_disconnect`], which revokes cache
//!   authority and preserves only the verified origin identity (RF-3).
//! - A zero or non-positive heartbeat/lease interval from the peer is refused
//!   before any state is accepted (arg smuggling).
//! - Frame payloads are length-checked against the caller's cap before
//!   allocation (`ramshared_ipc::read_frame`).
//! - The HMAC shared secret is supplied by the caller from an environment or
//!   secret-store source. Nothing in this module hardcodes a key.

mod heartbeat;
use ramshared_ipc::gpu_catalog::{AuthenticatedCatalog, CatalogSession};
use ramshared_ipc::{
    self as ipc, FrameError, Handshake, HandshakeAck, HandshakeError, HandshakeFinish,
    HandshakeTranscript, Heartbeat, HostChallenge, LeaseDenied, LeaseGranted, LeaseRequest,
    MSG_HANDSHAKE, MSG_HANDSHAKE_ACK, MSG_HANDSHAKE_FINISH, MSG_LEASE_DENIED, MSG_LEASE_GRANTED,
    MSG_LEASE_REQUEST, read_frame, write_frame,
};

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::host_catalog::SharedHostCatalog;
use crate::host_gate::{
    ControlPlaneAuthority, ControlPlaneState, GateError, LeaseToken, SealedOrigin, SessionAuthority,
};
#[cfg(target_os = "linux")]
use crate::host_storage_state::{AuthenticatedHostStorage, SharedHostStorage};
use ipc::gpu_catalog::AuthenticatedHostMemory;
#[cfg(target_os = "linux")]
use ramshared_block::vhd_identity::RootVhdObservation;
use ramshared_ipc::storage_proof::StorageSession;

/// Errors from control-plane transport and lease maintenance.
#[derive(Debug, PartialEq, Eq)]
pub enum ControlPlaneError {
    Frame(FrameError),
    Handshake(HandshakeError),
    Gate(GateError),
    /// The peer refused to mint a lease, or the lease has already lapsed.
    LeaseDenied(String),
    /// Heartbeat observed a lease that has no remaining lifetime.
    LeaseExpired,
    /// No transport is currently authorised to serve (disconnected / safe mode).
    Disconnected,
    /// The peer sent a protocol value this side refuses to honour.
    Protocol(String),
}

impl std::fmt::Display for ControlPlaneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Frame(e) => write!(f, "control-plane frame error: {e}"),
            Self::Handshake(e) => write!(f, "control-plane handshake error: {e}"),
            Self::Gate(e) => write!(f, "control-plane gate error: {e}"),
            Self::LeaseDenied(e) => write!(f, "lease denied: {e}"),
            Self::LeaseExpired => write!(f, "lease expired"),
            Self::Disconnected => write!(f, "control plane disconnected"),
            Self::Protocol(e) => write!(f, "control-plane protocol error: {e}"),
        }
    }
}

impl std::error::Error for ControlPlaneError {}

impl From<FrameError> for ControlPlaneError {
    fn from(value: FrameError) -> Self {
        Self::Frame(value)
    }
}

impl From<HandshakeError> for ControlPlaneError {
    fn from(value: HandshakeError) -> Self {
        Self::Handshake(value)
    }
}

impl From<GateError> for ControlPlaneError {
    fn from(value: GateError) -> Self {
        Self::Gate(value)
    }
}

/// The authority the I/O path consults before using cache or origin.
///
/// The daemon publishes session-bound authority into the mirror consulted by
/// `AuthoritativeOriginBackend`. The transport remains separate from origin
/// I/O, and losing a lease cannot revoke the independently verified origin.
pub trait ControlPlane: Send {
    /// Current gate state, for telemetry and for callers that only read.
    fn state(&self) -> ControlPlaneState;

    /// Authorise a cache admission at `now_ms`.
    fn admit_cache(&self, now_ms: u64) -> Result<(), GateError>;

    /// Authorise authoritative-origin I/O. Lease state must not take this down
    /// (RF-3); only loss of the verified origin identity may.
    fn admit_origin_io(&self) -> Result<(), GateError>;

    /// One lease-maintenance beat. On failure the authority is already moved
    /// to its fail-closed state before this returns.
    fn heartbeat(&mut self, now_ms: u64) -> Result<(), ControlPlaneError>;
}

/// Live parameters the host negotiated in [`HandshakeAck`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NegotiatedCadence {
    pub heartbeat_secs: u64,
    pub lease_timeout_secs: u64,
}

/// Refuse a peer `accepted_version` outside the versions this build speaks.
///
/// The frame header's version and the payload's `accepted_version` are
/// different fields: `VsockFrameHeader::decode` bounds the former, nothing
/// else bounds the latter. A peer that claims version 1 (below
/// `IPC_MIN_VERSION`) or 6 (above `IPC_MAX_VERSION`) is refused here.
fn validate_accepted_version(accepted: u32) -> Result<u32, ControlPlaneError> {
    if !(ipc::IPC_MIN_VERSION..=ipc::IPC_MAX_VERSION).contains(&accepted) {
        return Err(ControlPlaneError::Protocol(format!(
            "peer accepted version {accepted}, outside the supported range {}..={}",
            ipc::IPC_MIN_VERSION,
            ipc::IPC_MAX_VERSION
        )));
    }
    Ok(accepted)
}

/// Reject a peer-supplied cadence that would spin the loop or outrun the lease.
fn validate_cadence(c: NegotiatedCadence) -> Result<NegotiatedCadence, ControlPlaneError> {
    if c.heartbeat_secs == 0 {
        return Err(ControlPlaneError::Protocol(
            "peer negotiated a zero heartbeat interval".into(),
        ));
    }
    if c.lease_timeout_secs == 0 {
        return Err(ControlPlaneError::Protocol(
            "peer negotiated a zero lease timeout".into(),
        ));
    }
    if c.lease_timeout_secs <= c.heartbeat_secs {
        return Err(ControlPlaneError::Protocol(format!(
            "lease timeout {} does not outlive heartbeat {}",
            c.lease_timeout_secs, c.heartbeat_secs
        )));
    }
    Ok(c)
}

/// Native-vsock control plane: drives the DT-3 handshake and keeps the lease
/// alive over one connected stream.
pub struct VsockControlPlane<S: ipc::vsock::VsockStreamTrait> {
    stream: S,
    authority: ControlPlaneAuthority,
    /// HMAC shared secret from an env or secret-store source. Never a constant.
    secret: Vec<u8>,
    boot_id: String,
    distro_id: String,
    origin: SealedOrigin,
    cadence: Option<NegotiatedCadence>,
    lease_deadline_ms: u64,
    /// Last successful handshake/ack time, shared with the lease watchdog.
    /// The heartbeat loop may block in a stream read after a peer disappears;
    /// this counter lets an independent watchdog revoke the authority.
    last_ack_ms: Arc<AtomicU64>,
    /// Last host-memory sample received on an authenticated heartbeat. The
    /// cache-status publisher holds this same handle.
    host_memory: Arc<Mutex<Option<AuthenticatedHostMemory>>>,
    /// An optional second view of this plane's authority.
    ///
    /// The daemon consults the authority from its own backend, and the plane
    /// owns the live one. A copy taken once at handshake time kept the
    /// handshake's deadline, so the cache was refused again one lease window
    /// later even though the heartbeats were alive and sliding the plane's
    /// own deadline. The mirror carries the plane's current authority after
    /// every change, so the view the backend consults is the live one.
    authority_mirror: Option<SessionAuthority>,
    io_timeout: std::time::Duration,
    transport_poisoned: bool,
    catalog_session: Option<CatalogSession>,
    host_catalog: Option<AuthenticatedCatalog>,
    shared_host_catalog: Option<SharedHostCatalog>,
    storage_session: Option<StorageSession>,
    #[cfg(target_os = "linux")]
    shared_host_storage: Option<SharedHostStorage>,
    #[cfg(target_os = "linux")]
    root_vhd_observer: Option<fn() -> Option<RootVhdObservation>>,
}

impl<S: ipc::vsock::VsockStreamTrait> VsockControlPlane<S> {
    /// Build a disconnected control plane over an already-connected stream.
    ///
    /// `secret` is supplied by the caller; this constructor never invents one.
    /// `origin` is an already-verified sealed origin — this module does not
    /// establish origin identity, it only carries it. It is installed on the
    /// authority immediately so RF-3 holds from construction: origin I/O is
    /// available before any lease and survives lease loss and disconnect.
    pub fn new(
        stream: S,
        secret: Vec<u8>,
        boot_id: String,
        distro_id: String,
        origin: SealedOrigin,
        io_timeout: std::time::Duration,
    ) -> Self {
        let mut authority = ControlPlaneAuthority::disconnected();
        authority.adopt_verified_origin(origin.clone());
        Self {
            stream,
            authority,
            secret,
            boot_id,
            distro_id,
            origin,
            cadence: None,
            lease_deadline_ms: 0,
            last_ack_ms: Arc::new(AtomicU64::new(0)),
            host_memory: Arc::new(Mutex::new(None)),
            authority_mirror: None,
            io_timeout,
            transport_poisoned: false,
            catalog_session: None,
            host_catalog: None,
            shared_host_catalog: None,
            storage_session: None,
            #[cfg(target_os = "linux")]
            shared_host_storage: None,
            #[cfg(target_os = "linux")]
            root_vhd_observer: None,
        }
    }

    /// Publish this plane's authority into a shared slot the backend consults.
    ///
    /// The mirror is a [`SessionAuthority`] (DT-4): retirement or supersession
    /// refuses late publication, including before a replacement lease exists.
    pub fn with_authority_mirror(mut self, mirror: SessionAuthority) -> Self {
        self.authority_mirror = Some(mirror);
        self.publish_authority();
        self
    }

    /// Copy the plane's current authority into the mirror, if one is set.
    fn publish_authority(&self) {
        let Some(mirror) = self.authority_mirror.as_ref() else {
            return;
        };
        let _ = mirror.publish(&self.authority);
    }

    pub fn authority(&self) -> &ControlPlaneAuthority {
        &self.authority
    }

    pub fn cadence(&self) -> Option<NegotiatedCadence> {
        self.cadence
    }

    /// Shared last-successful-ack clock for the independent lease watchdog.
    pub fn liveness_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.last_ack_ms)
    }

    /// Shared host-memory slot updated by every authenticated heartbeat.
    pub fn host_memory_handle(&self) -> Arc<Mutex<Option<AuthenticatedHostMemory>>> {
        Arc::clone(&self.host_memory)
    }

    /// Authenticated host metadata only; native VM/guest/worker correspondence
    /// must still be proved before any allocator can use these identities.
    pub fn gpu_catalog(&self) -> Option<&ramshared_vram::host_identity::HostGpuIdentityReport> {
        if self.authority.state() != ControlPlaneState::VsockLeased {
            return None;
        }
        let lease = self.authority.lease()?;
        if self
            .authority_mirror
            .as_ref()
            .is_some_and(|mirror| !mirror.lease_is_current(lease.lease_id))
        {
            return None;
        }
        self.host_catalog
            .as_ref()?
            .report(std::time::Instant::now())
    }

    pub fn with_shared_host_catalog(mut self, catalog: SharedHostCatalog) -> Self {
        self.attach_host_catalog(catalog);
        self
    }

    pub fn attach_host_catalog(&mut self, catalog: SharedHostCatalog) {
        self.shared_host_catalog = Some(catalog);
        self.publish_host_catalog();
    }
    #[cfg(target_os = "linux")]
    pub fn attach_host_storage(&mut self, storage: SharedHostStorage) {
        self.shared_host_storage = Some(storage);
    }
    #[cfg(target_os = "linux")]
    pub fn set_root_vhd_observer(&mut self, observer: fn() -> Option<RootVhdObservation>) {
        self.root_vhd_observer = Some(observer);
    }
    #[cfg(target_os = "linux")]
    pub(super) fn publish_host_storage(&self, observation: AuthenticatedHostStorage) {
        if let Some(shared) = &self.shared_host_storage {
            shared.publish(self.authority_mirror.clone(), observation);
        }
    }
    #[cfg(target_os = "linux")]
    pub(super) fn revoke_host_storage(&self) {
        if let Some(shared) = &self.shared_host_storage {
            shared.revoke_for(self.authority_mirror.as_ref());
        }
    }

    pub fn shared_host_catalog(&self) -> Option<&SharedHostCatalog> {
        self.shared_host_catalog.as_ref()
    }

    pub(crate) fn publish_host_catalog(&self) {
        let Some(shared) = &self.shared_host_catalog else {
            return;
        };
        let live = if self.authority.state() == ControlPlaneState::VsockLeased
            && !self.transport_poisoned
        {
            self.host_catalog.as_ref()
        } else {
            None
        };
        if let Some(mirror) = &self.authority_mirror {
            mirror.mutate(|_| shared.publish(live));
        } else {
            shared.publish(live);
        }
    }

    pub fn lease_deadline_ms(&self) -> u64 {
        self.lease_deadline_ms
    }

    /// Drive the DT-3 handshake and accept the resulting lease.
    ///
    /// Order (SPEC ITEM-3 / `HandshakeTranscript`):
    /// `Handshake` → `HandshakeAck` (verified) → `HandshakeFinish` →
    /// `LeaseRequest` → `LeaseGranted`.
    ///
    /// Any failure before `accept_lease` succeeds leaves the authority in safe
    /// mode with no lease. A connection failure never grants a lease
    /// (`host_gate::lease_after_connect`).
    pub fn perform_handshake(&mut self, now_ms: u64) -> Result<(), ControlPlaneError> {
        if self.transport_poisoned {
            return Err(ControlPlaneError::Disconnected);
        }
        self.catalog_session = None;
        self.host_catalog = None;
        self.publish_host_catalog();
        self.storage_session = None;
        #[cfg(target_os = "linux")]
        self.revoke_host_storage();
        self.authority.begin_handshake();
        self.publish_authority();

        let result = self.handshake_exchange(now_ms);
        if result.is_err() {
            self.poison_transport();
            // Fail closed: never leave the authority advertising Handshaking
            // after a failed attempt, and never leave a half-accepted lease.
            self.authority.on_vsock_disconnect();
            self.publish_authority();
            self.cadence = None;
            self.lease_deadline_ms = 0;
        }
        result
    }

    fn handshake_exchange(&mut self, now_ms: u64) -> Result<(), ControlPlaneError> {
        let mut exchange = ipc::deadline::DeadlineIo::new(&mut self.stream, self.io_timeout)
            .map_err(|error| FrameError::StreamIo(error.to_string()))?;

        let guest_nonce = ipc::random_nonce()?;
        let transcript = HandshakeTranscript {
            min_version: ipc::IPC_MIN_VERSION,
            max_version: ipc::IPC_MAX_VERSION,
            boot_id: self.boot_id.clone(),
            distro_id: self.distro_id.clone(),
            guest_nonce,
        };
        let opening_proof = ipc::guest_proof(&self.secret, &transcript)?;
        let opening = Handshake {
            min_version: ipc::IPC_MIN_VERSION,
            max_version: ipc::IPC_MAX_VERSION,
            boot_id: self.boot_id.clone(),
            distro_id: self.distro_id.clone(),
            guest_nonce,
            guest_proof: opening_proof,
        };
        let payload = ipc::encode_control(&opening)?;
        write_frame(&mut exchange, MSG_HANDSHAKE, &payload, 1)?;

        let (msg_type, ack_payload, correlation) =
            read_frame(&mut exchange, ipc::MAX_CONTROL_PAYLOAD)?;
        if msg_type != MSG_HANDSHAKE_ACK {
            return Err(ControlPlaneError::Protocol(format!(
                "expected handshake ack ({}), received {msg_type}",
                MSG_HANDSHAKE_ACK
            )));
        }
        let ack: HandshakeAck = ipc::decode_control(&ack_payload)?;
        validate_accepted_version(ack.accepted_version)?;
        if ack.accepted_version >= ipc::IPC_VERSION_4 && correlation != 1 {
            return Err(ControlPlaneError::Protocol(
                "host challenge round mismatch".into(),
            ));
        }

        let cadence = validate_cadence(NegotiatedCadence {
            heartbeat_secs: ack.heartbeat_secs,
            lease_timeout_secs: ack.lease_timeout_secs,
        })?;

        let challenge = HostChallenge {
            accepted_version: ack.accepted_version,
            heartbeat_secs: ack.heartbeat_secs,
            lease_timeout_secs: ack.lease_timeout_secs,
            host_nonce: ack.host_nonce,
        };
        if !ipc::verify_host_proof(&self.secret, &transcript, &challenge, &ack.host_proof) {
            return Err(ControlPlaneError::Handshake(HandshakeError::ProofMismatch));
        }

        let finish_proof = ipc::guest_finish_proof(&self.secret, &transcript, &challenge)?;
        let finish = HandshakeFinish {
            guest_finish_proof: finish_proof,
        };
        let payload = ipc::encode_control(&finish)?;
        write_frame(&mut exchange, MSG_HANDSHAKE_FINISH, &payload, 2)?;

        let request = LeaseRequest {
            nonce: guest_nonce.to_vec(),
        };
        let payload = ipc::encode_control(&request)?;
        write_frame(&mut exchange, MSG_LEASE_REQUEST, &payload, 3)?;

        let (msg_type, lease_payload, correlation) =
            read_frame(&mut exchange, ipc::MAX_CONTROL_PAYLOAD)?;
        if ack.accepted_version >= ipc::IPC_VERSION_4 && correlation != 3 {
            return Err(ControlPlaneError::Protocol(
                "lease grant round mismatch".into(),
            ));
        }
        let lease = match msg_type {
            MSG_LEASE_GRANTED => {
                let granted: LeaseGranted = ipc::decode_control(&lease_payload)?;
                LeaseToken {
                    lease_id: granted.lease_id,
                    deadline_ms: granted.deadline_ms,
                }
            }
            MSG_LEASE_DENIED => {
                let denied: LeaseDenied = ipc::decode_control(&lease_payload)?;
                return Err(ControlPlaneError::LeaseDenied(denied.reason));
            }
            other => {
                return Err(ControlPlaneError::Protocol(format!(
                    "expected lease granted ({}) or lease denied ({}), received {other}",
                    MSG_LEASE_GRANTED, MSG_LEASE_DENIED
                )));
            }
        };

        if lease.deadline_ms < now_ms {
            return Err(ControlPlaneError::LeaseDenied(
                "peer granted an already-expired lease".into(),
            ));
        }
        let catalog = if ack.accepted_version >= ipc::IPC_VERSION_4 {
            Some(CatalogSession::new(
                &self.secret,
                &transcript,
                &challenge,
                lease.lease_id,
            )?)
        } else {
            None
        };
        let storage_session = if ack.accepted_version == ipc::IPC_VERSION_5 {
            Some(StorageSession::new(
                &self.secret,
                &transcript,
                &challenge,
                lease.lease_id,
            )?)
        } else {
            None
        };
        exchange
            .finish()
            .map_err(|error| FrameError::StreamIo(error.to_string()))?;

        self.authority
            .accept_lease(lease.clone(), self.origin.clone(), now_ms)?;
        self.publish_authority();
        self.catalog_session = catalog;
        self.storage_session = storage_session;
        self.cadence = Some(cadence);
        self.lease_deadline_ms = lease.deadline_ms;
        self.last_ack_ms.store(now_ms, Ordering::Relaxed);
        self.publish_host_catalog();
        #[cfg(target_os = "linux")]
        self.revoke_host_storage();
        Ok(())
    }

    fn poison_transport(&mut self) {
        self.transport_poisoned = true;
        self.catalog_session = None;
        self.host_catalog = None;
        self.publish_host_catalog();
        self.storage_session = None;
        #[cfg(target_os = "linux")]
        self.revoke_host_storage();
        let _ = self.stream.shutdown_both();
        self.last_ack_ms.store(0, Ordering::Relaxed);
        if let Ok(mut host_memory) = self.host_memory.lock() {
            *host_memory = None;
        }
    }
}

impl<S: ipc::vsock::VsockStreamTrait> ControlPlane for VsockControlPlane<S> {
    fn state(&self) -> ControlPlaneState {
        self.authority.state()
    }

    fn admit_cache(&self, now_ms: u64) -> Result<(), GateError> {
        self.authority.admit_cache(now_ms)
    }

    fn admit_origin_io(&self) -> Result<(), GateError> {
        self.authority.admit_origin_io()
    }

    fn heartbeat(&mut self, now_ms: u64) -> Result<(), ControlPlaneError> {
        if self.authority.state() != ControlPlaneState::VsockLeased {
            return Err(ControlPlaneError::Disconnected);
        }

        let beat = Heartbeat {
            timestamp_ms: now_ms,
        };
        let payload = ipc::encode_control(&beat)?;
        match self.exchange_heartbeat(&payload, now_ms) {
            Ok(()) => Ok(()),
            // A clean lease expiry is not a transport failure. RF-3: lease
            // loss revokes cache authority and leaves the verified origin
            // path up, so this must stay at `OriginOnly` and must not be
            // downgraded to safe mode by the disconnect handler below.
            Err(ControlPlaneError::LeaseExpired) => {
                self.poison_transport();
                self.cadence = None;
                self.lease_deadline_ms = 0;
                Err(ControlPlaneError::LeaseExpired)
            }
            Err(error) => {
                self.poison_transport();
                // Fail closed on every transport failure. A beat that cannot be
                // completed is not evidence the lease is alive.
                self.authority.on_vsock_disconnect();
                self.publish_authority();
                self.cadence = None;
                self.lease_deadline_ms = 0;
                Err(error)
            }
        }
    }
}

mod heartbeat_loop;
pub use heartbeat_loop::{HeartbeatLoop, HeartbeatLoopStop};

#[cfg(test)]
mod tests;
