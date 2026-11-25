// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/vsock_lease.rs
//! Host side of the native-vsock control plane (SPEC ITEM-4, DT-3/DT-5).
//!
//! The guest drives: `Handshake` → `HandshakeAck` → `HandshakeFinish` →
//! `LeaseRequest` → `LeaseGranted`, then heartbeats. This module verifies the
//! role-separated HMAC proofs, mints the lease, and slides its deadline on
//! every beat. It never writes to the guest cache: the lease only tells the
//! guest that the host authority is alive.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::vm_id_resolve::parse_guid;

mod heartbeat;
mod lifecycle;
mod listener;
pub use heartbeat::{HostSamples, StorageSample, serve_heartbeats};
pub use lifecycle::{CONTROL_AUTHORITY_MUTEX_NAME, ControlPlaneTask, StopSignal, StopState};
pub use listener::spawn_configured_listener;
use ramshared_ipc::deadline::DeadlineIo;
use ramshared_ipc::gpu_catalog::CatalogSession;
use ramshared_ipc::storage_proof::StorageSession;
use ramshared_ipc::vsock::VsockError;
use ramshared_ipc::vsock::VsockStreamTrait;
use ramshared_ipc::{
    FrameError, Handshake, HandshakeAck, HandshakeError, HandshakeFinish, HandshakeTranscript,
    HostChallenge, IPC_MAX_VERSION, IPC_MIN_VERSION, LeaseGranted, LeaseRequest,
    MAX_CONTROL_PAYLOAD, MSG_HANDSHAKE, MSG_HANDSHAKE_ACK, MSG_HANDSHAKE_FINISH, MSG_LEASE_GRANTED,
    MSG_LEASE_REQUEST, decode_control, encode_control, host_proof, random_nonce, read_frame,
    verify_guest_finish_proof, verify_guest_proof, write_frame,
};
use serde::Deserialize;
use std::time::Duration;

/// Default liveness beat announced to the guest. Kept at 1s so host GPU
/// catalog telemetry (`GPU_BUDGET_MAX_AGE_MS = 5_000`) refreshes with margin.
pub const DEFAULT_HEARTBEAT_SECS: u64 = 1;
/// Default lease lifetime announced to the guest.
pub const DEFAULT_LEASE_TIMEOUT_SECS: u64 = 15;
/// Largest accepted shared-secret file.
pub const MAX_KEY_BYTES: u64 = 4096;
/// Largest accepted product config file.
pub const MAX_CONFIG_BYTES: u64 = 64 * 1024;

/// How the host key file is protected at rest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyProtection {
    /// Raw bytes. Kept for tests and for a lab key file; not the product default.
    #[default]
    Plain,
    /// Windows DPAPI machine scope, matching the provisioning script.
    DpapiMachine,
}

/// Domain separator shared with the provisioning script's DPAPI entropy.
pub const DPAPI_ENTROPY: &[u8] = b"ramshared-vsock-key-v1";

/// Which side initiates the AF_HYPERV/AF_VSOCK stream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VsockMode {
    /// Host listens; guest connects (native Hyper-V).
    #[default]
    Listen,
    /// Guest listens; host connects to the guest VM ID (WSL2 native direction).
    Connect,
}

/// Optional `[vsock]` table in the product TOML.
///
/// Absent means the control plane is not configured and no listener starts.
/// Present but invalid is an error: a misconfigured authority must not be
/// silently ignored.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VsockConfig {
    /// Guest-visible `AF_VSOCK` port, encoded in the host service GUID.
    pub port: u32,
    /// Absolute path to the shared HMAC secret (host-owned, no guest path).
    pub key_file: PathBuf,
    #[serde(default = "default_heartbeat_secs")]
    pub heartbeat_secs: u64,
    #[serde(default = "default_lease_timeout_secs")]
    pub lease_timeout_secs: u64,
    #[serde(default)]
    pub key_protection: KeyProtection,
    #[serde(default)]
    pub mode: VsockMode,
    /// Guest VM GUID for `mode = "connect"` (canonical hyphenated form).
    #[serde(default)]
    pub vm_id: Option<String>,
}

fn default_heartbeat_secs() -> u64 {
    DEFAULT_HEARTBEAT_SECS
}

fn default_lease_timeout_secs() -> u64 {
    DEFAULT_LEASE_TIMEOUT_SECS
}

impl VsockConfig {
    /// Parse the optional `[vsock]` table out of a product config.
    ///
    /// The other top-level tables are owned by `WinDriveConfig`; serde ignores
    /// them here, so both readers can share one file.
    pub fn from_toml(text: &str) -> Result<Option<Self>, LeaseServerError> {
        #[derive(Deserialize)]
        struct Root {
            vsock: Option<VsockConfig>,
        }
        let root: Root = toml::from_str(text)
            .map_err(|error| LeaseServerError::Config(format!("invalid [vsock] table: {error}")))?;
        match root.vsock {
            Some(config) => {
                config.validate()?;
                Ok(Some(config))
            }
            None => Ok(None),
        }
    }

    /// Read and validate the `[vsock]` table from `path`.
    pub fn load(path: &Path) -> Result<Option<Self>, LeaseServerError> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| LeaseServerError::Config(format!("{}: {error}", path.display())))?;
        if text.len() as u64 > MAX_CONFIG_BYTES {
            return Err(LeaseServerError::Config(format!(
                "{}: config exceeds {MAX_CONFIG_BYTES} bytes",
                path.display()
            )));
        }
        Self::from_toml(&text)
    }

    /// Fail closed on a control plane that could never authenticate or bind.
    pub fn validate(&self) -> Result<(), LeaseServerError> {
        if self.port == 0 || self.port > 0x7FFF_FFFF {
            return Err(LeaseServerError::Config(
                "vsock port must be in 1..=0x7FFFFFFF".into(),
            ));
        }
        if !self.key_file.is_absolute() {
            return Err(LeaseServerError::Config(
                "vsock key_file must be an absolute path".into(),
            ));
        }
        LeasePolicy::new(self.heartbeat_secs, self.lease_timeout_secs)?;
        if self.mode == VsockMode::Connect {
            // SPEC RF-1/RF-2/DT-1: `vm_id` is optional. `None` or `"auto"`
            // means "resolve one current native Linux VM route"; anything else must
            // be a well-formed GUID (an explicit operator override).
            match self.vm_id.as_deref() {
                None | Some("auto") => {}
                Some(guid) => {
                    parse_guid(guid)?;
                }
            }
        }
        Ok(())
    }

    /// Read and unprotect the shared secret.
    ///
    /// The file is host-owned (the service runs as LocalSystem). With
    /// `key_protection = "dpapi_machine"` it is a DPAPI blob produced by the
    /// provisioning script; the caller must treat the returned bytes as secret
    /// and never log them.
    pub fn read_key(&self) -> Result<Vec<u8>, LeaseServerError> {
        let blob = self.read_key_blob()?;
        match self.key_protection {
            KeyProtection::Plain => Ok(blob),
            KeyProtection::DpapiMachine => unprotect_dpapi_machine(&blob),
        }
    }

    fn read_key_blob(&self) -> Result<Vec<u8>, LeaseServerError> {
        let file = std::fs::File::open(&self.key_file).map_err(|error| {
            LeaseServerError::Key(format!("{}: {error}", self.key_file.display()))
        })?;
        let metadata = file.metadata().map_err(|error| {
            LeaseServerError::Key(format!("{}: {error}", self.key_file.display()))
        })?;
        if !metadata.is_file() {
            return Err(LeaseServerError::Key(format!(
                "{}: not a regular file",
                self.key_file.display()
            )));
        }
        if metadata.len() == 0 || metadata.len() > MAX_KEY_BYTES {
            return Err(LeaseServerError::Key(format!(
                "{}: key must be 1..={MAX_KEY_BYTES} bytes",
                self.key_file.display()
            )));
        }
        let mut key = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_KEY_BYTES)
            .read_to_end(&mut key)
            .map_err(|error| {
                LeaseServerError::Key(format!("{}: {error}", self.key_file.display()))
            })?;
        if key.len() as u64 != metadata.len() {
            return Err(LeaseServerError::Key(format!(
                "{}: key file changed while reading",
                self.key_file.display()
            )));
        }
        Ok(key)
    }
}

#[cfg(windows)]
fn unprotect_dpapi_machine(blob: &[u8]) -> Result<Vec<u8>, LeaseServerError> {
    use windows_sys::Win32::Foundation::{HLOCAL, LocalFree};
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_LOCAL_MACHINE, CRYPTPROTECT_UI_FORBIDDEN,
        CryptUnprotectData,
    };

    let blob_len = u32::try_from(blob.len())
        .map_err(|_| LeaseServerError::Key("DPAPI blob is too large".into()))?;
    let entropy_len = u32::try_from(DPAPI_ENTROPY.len())
        .map_err(|_| LeaseServerError::Key("DPAPI entropy is too large".into()))?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: blob_len,
        pbData: blob.as_ptr().cast_mut(),
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: entropy_len,
        pbData: DPAPI_ENTROPY.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: both input blobs point to live slices for the duration of the
    // call; optional pointers are null; the output blob is owned by the API
    // and freed below on every path after a successful call.
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            &entropy,
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_LOCAL_MACHINE | CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        return Err(LeaseServerError::Key(format!(
            "DPAPI unprotect failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    if output.pbData.is_null() {
        return Err(LeaseServerError::Key(
            "DPAPI returned an empty plaintext buffer".into(),
        ));
    }
    let len = output.cbData as usize;
    if len == 0 || len > MAX_KEY_BYTES as usize {
        // SAFETY: the API allocated this buffer; free it exactly once.
        unsafe {
            LocalFree(output.pbData as HLOCAL);
        }
        return Err(LeaseServerError::Key(format!(
            "DPAPI plaintext length {len} outside 1..={MAX_KEY_BYTES}"
        )));
    }
    let mut key = vec![0u8; len];
    // SAFETY: the API wrote `len` bytes to `output.pbData`; the destination has
    // the same length and does not overlap the source.
    unsafe {
        std::ptr::copy_nonoverlapping(output.pbData, key.as_mut_ptr(), len);
        // Zero the API buffer before returning it to the allocator.
        std::ptr::write_bytes(output.pbData, 0, len);
        LocalFree(output.pbData as HLOCAL);
    }
    Ok(key)
}

#[cfg(not(windows))]
fn unprotect_dpapi_machine(_blob: &[u8]) -> Result<Vec<u8>, LeaseServerError> {
    Err(LeaseServerError::Key(
        "dpapi_machine key protection requires Windows".into(),
    ))
}

/// Announced beat and lease cadence. The lease must outlive one beat so a
/// single missed beat does not revoke the cache by itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeasePolicy {
    pub heartbeat_secs: u64,
    pub lease_timeout_secs: u64,
}

impl LeasePolicy {
    pub fn new(heartbeat_secs: u64, lease_timeout_secs: u64) -> Result<Self, LeaseServerError> {
        if heartbeat_secs == 0 || lease_timeout_secs == 0 {
            return Err(LeaseServerError::Config(
                "vsock heartbeat and lease timeout must be nonzero".into(),
            ));
        }
        if lease_timeout_secs <= heartbeat_secs {
            return Err(LeaseServerError::Config(format!(
                "vsock lease timeout {lease_timeout_secs}s does not outlive heartbeat {heartbeat_secs}s"
            )));
        }
        Ok(Self {
            heartbeat_secs,
            lease_timeout_secs,
        })
    }

    fn timeout_ms(self) -> u64 {
        self.lease_timeout_secs.saturating_mul(1000)
    }
}

/// A lease minted for one authenticated guest connection.
pub struct LeaseSession {
    pub lease_id: u32,
    pub deadline_ms: u64,
    pub catalog: Option<CatalogSession>,
    pub storage: Option<StorageSession>,
}

/// Why the host ended a session.
#[derive(Debug)]
pub enum LeaseServerError {
    Config(String),
    Key(String),
    Frame(FrameError),
    Handshake(HandshakeError),
    Vsock(VsockError),
    Protocol(&'static str),
    Refused(&'static str),
    LeaseExpired,
    Io(std::io::Error),
}

impl std::fmt::Display for LeaseServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(reason) => write!(f, "vsock config refused: {reason}"),
            Self::Key(reason) => write!(f, "vsock key refused: {reason}"),
            Self::Frame(error) => write!(f, "vsock frame error: {error}"),
            Self::Handshake(error) => write!(f, "vsock handshake error: {error}"),
            Self::Vsock(error) => write!(f, "vsock transport error: {error}"),
            Self::Protocol(reason) => write!(f, "vsock protocol error: {reason}"),
            Self::Refused(reason) => write!(f, "vsock peer refused: {reason}"),
            Self::LeaseExpired => write!(f, "vsock lease expired"),
            Self::Io(error) => write!(f, "vsock io error: {error}"),
        }
    }
}

impl std::error::Error for LeaseServerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Frame(error) => Some(error),
            Self::Handshake(error) => Some(error),
            Self::Vsock(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<FrameError> for LeaseServerError {
    fn from(error: FrameError) -> Self {
        Self::Frame(error)
    }
}

impl From<HandshakeError> for LeaseServerError {
    fn from(error: HandshakeError) -> Self {
        Self::Handshake(error)
    }
}

/// Drive the DT-3 exchange on one accepted connection and mint a lease.
///
/// Version negotiation is `max(min_guest, IPC_MIN_VERSION) ..=
/// min(max_guest, IPC_MAX_VERSION)`; no overlap and any failed guest proof leave
/// the guest origin-only. The nonce in `LeaseRequest` must be the opening
/// nonce, so a captured grant cannot be replayed onto a different handshake.
pub fn serve_handshake<S: Read + Write>(
    stream: &mut S,
    secret: &[u8],
    policy: LeasePolicy,
    now_ms: u64,
) -> Result<LeaseSession, LeaseServerError> {
    let (msg_type, payload, correlation) = read_frame(stream, MAX_CONTROL_PAYLOAD)?;
    if msg_type != MSG_HANDSHAKE {
        return Err(LeaseServerError::Protocol("expected handshake opening"));
    }
    let opening: Handshake = decode_control(&payload)?;
    if opening.min_version > opening.max_version {
        return Err(LeaseServerError::Refused("inverted version window"));
    }
    if opening.boot_id.is_empty() || opening.distro_id.is_empty() {
        return Err(LeaseServerError::Refused("empty identity claim"));
    }
    let low = opening.min_version.max(IPC_MIN_VERSION);
    let high = opening.max_version.min(IPC_MAX_VERSION);
    if low > high {
        return Err(LeaseServerError::Refused(
            "no mutually supported protocol version",
        ));
    }
    if high >= ramshared_ipc::IPC_VERSION_4 && correlation != 1 {
        return Err(LeaseServerError::Protocol("opening round mismatch"));
    }
    let transcript = HandshakeTranscript {
        min_version: opening.min_version,
        max_version: opening.max_version,
        boot_id: opening.boot_id.clone(),
        distro_id: opening.distro_id.clone(),
        guest_nonce: opening.guest_nonce,
    };
    if !verify_guest_proof(secret, &transcript, &opening.guest_proof) {
        return Err(LeaseServerError::Refused("guest opening proof mismatch"));
    }

    let host_nonce = random_nonce()?;
    let challenge = HostChallenge {
        accepted_version: high,
        heartbeat_secs: policy.heartbeat_secs,
        lease_timeout_secs: policy.lease_timeout_secs,
        host_nonce,
    };
    let ack = HandshakeAck {
        accepted_version: high,
        heartbeat_secs: policy.heartbeat_secs,
        lease_timeout_secs: policy.lease_timeout_secs,
        host_nonce,
        host_proof: host_proof(secret, &transcript, &challenge)?,
    };
    write_frame(stream, MSG_HANDSHAKE_ACK, &encode_control(&ack)?, 1)?;

    let (msg_type, payload, correlation) = read_frame(stream, MAX_CONTROL_PAYLOAD)?;
    if msg_type != MSG_HANDSHAKE_FINISH
        || (high >= ramshared_ipc::IPC_VERSION_4 && correlation != 2)
    {
        return Err(LeaseServerError::Protocol("expected handshake finish"));
    }
    let finish: HandshakeFinish = decode_control(&payload)?;
    if !verify_guest_finish_proof(secret, &transcript, &challenge, &finish.guest_finish_proof) {
        return Err(LeaseServerError::Refused("guest finish proof mismatch"));
    }

    let (msg_type, payload, correlation) = read_frame(stream, MAX_CONTROL_PAYLOAD)?;
    if msg_type != MSG_LEASE_REQUEST || (high >= ramshared_ipc::IPC_VERSION_4 && correlation != 3) {
        return Err(LeaseServerError::Protocol("expected lease request"));
    }
    let request: LeaseRequest = decode_control(&payload)?;
    if request.nonce.as_slice() != opening.guest_nonce.as_slice() {
        return Err(LeaseServerError::Refused("lease request nonce mismatch"));
    }

    let nonce = random_nonce()?;
    let lease_id = u32::from_be_bytes([nonce[0], nonce[1], nonce[2], nonce[3]]) | 1;
    let deadline_ms = now_ms.saturating_add(policy.timeout_ms());
    let granted = LeaseGranted {
        lease_id,
        deadline_ms,
    };
    write_frame(stream, MSG_LEASE_GRANTED, &encode_control(&granted)?, 3)?;
    let catalog = if high >= ramshared_ipc::IPC_VERSION_4 {
        Some(CatalogSession::new(
            secret,
            &transcript,
            &challenge,
            lease_id,
        )?)
    } else {
        None
    };
    let storage = if high == ramshared_ipc::IPC_VERSION_5 {
        Some(StorageSession::new(
            secret,
            &transcript,
            &challenge,
            lease_id,
        )?)
    } else {
        None
    };
    Ok(LeaseSession {
        lease_id,
        deadline_ms,
        catalog,
        storage,
    })
}

/// Handshake plus heartbeat loop for one accepted connection.
pub fn serve_connection<S: VsockStreamTrait, F: FnMut() -> u64, H: FnMut() -> HostSamples>(
    stream: &mut S,
    secret: &[u8],
    policy: LeasePolicy,
    now: F,
    samples: H,
) -> Result<usize, LeaseServerError> {
    serve_connection_with_stop(stream, secret, policy, now, samples, None)
}

/// Production cancellation uses the owned task's monotonic signal.
pub fn serve_connection_with_stop<
    S: VsockStreamTrait,
    F: FnMut() -> u64,
    H: FnMut() -> HostSamples,
>(
    stream: &mut S,
    secret: &[u8],
    policy: LeasePolicy,
    mut now: F,
    samples: H,
    stop: Option<&StopSignal>,
) -> Result<usize, LeaseServerError> {
    let stop = stop.map(StopSignal::flag);
    let mut exchange = match stop {
        Some(stop) => DeadlineIo::new_cancellable(&mut *stream, Duration::from_secs(5), stop),
        None => DeadlineIo::new(&mut *stream, Duration::from_secs(5)),
    }
    .map_err(LeaseServerError::Io)?;
    let mut session = serve_handshake(&mut exchange, secret, policy, now())?;
    exchange.finish().map_err(LeaseServerError::Io)?;
    heartbeat::serve_heartbeats_with_stop(
        stream,
        &mut session,
        policy,
        now,
        None,
        samples,
        heartbeat::SessionControl { secret, stop },
    )
}

#[cfg(all(test, unix))]
mod tests;
