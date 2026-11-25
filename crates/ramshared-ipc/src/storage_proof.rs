// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-ipc/src/storage_proof.rs
//! Authenticated storage-observation envelope used by the storage transport.
//! A verified envelope is not a guest binding, a complete inventory proof, or
//! an allocation authorization; callers must independently establish each.
use crate::{
    FrameError, HandshakeTranscript, HostChallenge, IPC_VERSION_5, compute_hmac, host_proof,
    verify_hmac,
};
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

const LABEL: &[u8] = b"ramshared-storage-observation-v1";
const REQUEST_LABEL: &[u8] = b"ramshared-storage-request-v1";
const MAX_REQUEST_BYTES: usize = 1024;
const MAX_BYTES: usize = 64 * 1024;
const MAX_CANDIDATES: usize = 256;
pub const MAX_STORAGE_AGE_MS: u64 = 5_000;

/// Storage authority bounded by the boot-time clock at request start.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BootBoundStorageDeadline {
    boot_id: String,
    #[cfg(target_os = "linux")]
    time_namespace_device: u64,
    #[cfg(target_os = "linux")]
    time_namespace_inode: u64,
    started_ns: u64,
    expires_ns: u64,
}

#[cfg(target_os = "linux")]
fn time_namespace() -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata("/proc/self/ns/time").ok()?;
    (metadata.ino() != 0).then_some((metadata.dev(), metadata.ino()))
}

fn canonical_boot_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
            }
        })
}

/// Read a same-boot clock that includes system suspend time.
#[cfg(target_os = "linux")]
pub fn storage_boot_clock() -> Option<(String, u64)> {
    use std::io::Read;

    let mut boot = String::new();
    std::fs::File::open("/proc/sys/kernel/random/boot_id")
        .ok()?
        .take(64)
        .read_to_string(&mut boot)
        .ok()?;
    let boot = boot.strip_suffix('\n')?;
    if !canonical_boot_id(boot) {
        return None;
    }
    let now = rustix::time::clock_gettime(rustix::time::ClockId::Boottime);
    let nanos = u64::try_from(now.tv_sec)
        .ok()?
        .checked_mul(1_000_000_000)?
        .checked_add(u64::try_from(now.tv_nsec).ok()?)?;
    Some((boot.to_owned(), nanos))
}

impl BootBoundStorageDeadline {
    /// Start the bound before transport; neither sample age nor lease can
    /// extend it, even if the host suspends while a response is in flight.
    pub fn new(boot_id: &str, started_ns: u64, sample_age_ms: u64, lease_ms: u64) -> Option<Self> {
        if !canonical_boot_id(boot_id) {
            return None;
        }
        let remaining_ms = MAX_STORAGE_AGE_MS.checked_sub(sample_age_ms)?.min(lease_ms);
        if remaining_ms == 0 {
            return None;
        }
        #[cfg(target_os = "linux")]
        let (time_namespace_device, time_namespace_inode) = time_namespace()?;
        Some(Self {
            boot_id: boot_id.to_owned(),
            #[cfg(target_os = "linux")]
            time_namespace_device,
            #[cfg(target_os = "linux")]
            time_namespace_inode,
            started_ns,
            expires_ns: started_ns.checked_add(remaining_ms.checked_mul(1_000_000)?)?,
        })
    }

    /// Validate persisted data again; old, future or foreign-boot proof fails.
    pub fn is_current(&self, boot_id: &str, now_ns: u64) -> bool {
        #[cfg(target_os = "linux")]
        if time_namespace() != Some((self.time_namespace_device, self.time_namespace_inode)) {
            return false;
        }
        canonical_boot_id(&self.boot_id)
            && self.boot_id == boot_id
            && self
                .expires_ns
                .checked_sub(self.started_ns)
                .is_some_and(|ttl| ttl > 0 && ttl <= MAX_STORAGE_AGE_MS * 1_000_000)
            && now_ns >= self.started_ns
            && now_ns < self.expires_ns
    }

    /// Return elapsed time only while the boot-bound deadline remains valid.
    pub fn elapsed(&self, boot_id: &str, now_ns: u64) -> Option<Duration> {
        self.is_current(boot_id, now_ns)
            .then(|| Duration::from_nanos(now_ns - self.started_ns))
    }

    #[cfg(target_os = "linux")]
    fn with_sample_age(&self, sample_age_ms: u64) -> Option<Self> {
        if !self.is_current(&self.boot_id, self.started_ns) {
            return None;
        }
        let age_bound = Self::new(
            &self.boot_id,
            self.started_ns,
            sample_age_ms,
            MAX_STORAGE_AGE_MS,
        )?;
        Some(Self {
            expires_ns: self.expires_ns.min(age_bound.expires_ns),
            ..self.clone()
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StorageCandidate {
    pub disk_id: [u8; 16],
    pub volume_id: String,
    pub file_serial: u32,
    pub file_index: u64,
    pub total_bytes: u64,
    pub available_bytes: u64,
    /// Original producer age, never reset by forwarding.
    pub age_ms: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Body {
    lease_id: u32,
    round: u64,
    root_id: [u8; 16],
    complete: bool,
    candidates: Vec<StorageCandidate>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    body: Body,
    proof: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestBody {
    lease_id: u32,
    round: u64,
    disk_id: [u8; 16],
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestEnvelope {
    body: RequestBody,
    proof: Vec<u8>,
}

/// A request verified for one session and round, consumed when responding.
pub struct VerifiedStorageRequest {
    binding: [u8; 32],
    session_identity: Arc<()>,
    body: RequestBody,
}

impl VerifiedStorageRequest {
    /// Identifier authenticated for this request; it is not guest ownership.
    pub fn disk_id(&self) -> [u8; 16] {
        self.body.disk_id
    }
}

fn refuse(reason: &str) -> FrameError {
    FrameError::PayloadDeserialization(format!("storage proof refused: {reason}"))
}
fn valid_body(body: &Body) -> bool {
    if body.root_id == [0; 16]
        || !body.complete
        || body.candidates.is_empty()
        || body.candidates.len() > MAX_CANDIDATES
    {
        return false;
    }
    let mut root_count = 0usize;
    for (index, item) in body.candidates.iter().enumerate() {
        if item.disk_id == [0; 16]
            || item.volume_id.is_empty()
            || item.volume_id.len() > 128
            || item.volume_id.chars().any(char::is_control)
            || item.file_index == 0
            || item.total_bytes == 0
            || item.available_bytes > item.total_bytes
            || item.age_ms >= MAX_STORAGE_AGE_MS
            || body.candidates[..index]
                .iter()
                .any(|prior| prior.disk_id == item.disk_id)
        {
            return false;
        }
        if item.disk_id == body.root_id {
            root_count += 1;
        }
    }
    root_count == 1
}

/// A verified observation has an immutable monotonic expiration. The caller
/// checks current lease revocation on every read; completeness and guest/file
/// binding remain independent caller obligations.
pub struct AuthenticatedStorage {
    retired: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    round: u64,
    lease_id: u32,
    root_id: [u8; 16],
    candidates: Vec<StorageCandidate>,
    received: Instant,
    expires: Instant,
    #[cfg(target_os = "linux")]
    boot_deadline: Option<BootBoundStorageDeadline>,
}
impl AuthenticatedStorage {
    #[cfg(target_os = "linux")]
    pub(crate) fn bind_boot_deadline(
        &mut self,
        deadline: &BootBoundStorageDeadline,
    ) -> Result<(), FrameError> {
        let max_age = self
            .candidates
            .iter()
            .map(|item| item.age_ms)
            .max()
            .ok_or_else(|| refuse("empty catalog"))?;
        let deadline = deadline
            .with_sample_age(max_age)
            .ok_or_else(|| refuse("invalid boot deadline"))?;
        let (boot, now) = storage_boot_clock().ok_or_else(|| refuse("boot clock unavailable"))?;
        if !deadline.is_current(&boot, now) {
            return Err(refuse("boot deadline expired or changed"));
        }
        self.boot_deadline = Some(deadline);
        Ok(())
    }

    /// Return the original boot deadline only while this lease is current.
    #[cfg(target_os = "linux")]
    pub fn current_boot_deadline(
        &self,
        now: Instant,
        lease_is_current: impl FnOnce(u32) -> bool,
    ) -> Option<&BootBoundStorageDeadline> {
        self.candidates(now, lease_is_current)?;
        let deadline = self.boot_deadline.as_ref()?;
        let (boot, boot_ns) = storage_boot_clock()?;
        deadline.is_current(&boot, boot_ns).then_some(deadline)
    }

    /// Return the remaining authenticated lifetime, rounded down so a caller
    /// cannot extend a proof when transferring it to another clock domain.
    /// Less than one millisecond remaining is unusable.
    pub fn remaining_lifetime_ms(
        &self,
        now: Instant,
        lease_is_current: impl FnOnce(u32) -> bool,
    ) -> Option<u64> {
        self.candidates(now, lease_is_current)?;
        u64::try_from(self.expires.checked_duration_since(now)?.as_millis())
            .ok()
            .filter(|remaining| *remaining > 0)
    }

    /// Select only the disk bound to this request. This is an observation,
    /// not proof of inventory completeness, guest ownership or allocation.
    pub fn candidate_for(
        &self,
        disk_id: [u8; 16],
        now: Instant,
        lease_is_current: impl FnOnce(u32) -> bool,
    ) -> Option<&StorageCandidate> {
        if disk_id != self.root_id {
            return None;
        }
        self.candidates(now, lease_is_current)?
            .iter()
            .find(|candidate| candidate.disk_id == disk_id)
    }

    pub fn candidates(
        &self,
        now: Instant,
        lease_is_current: impl FnOnce(u32) -> bool,
    ) -> Option<&[StorageCandidate]> {
        (now >= self.received
            && now < self.expires
            && !self.retired.load(Ordering::SeqCst)
            && self.generation.load(Ordering::SeqCst) == self.round
            && lease_is_current(self.lease_id)
            && self.generation.load(Ordering::SeqCst) == self.round
            && !self.retired.load(Ordering::SeqCst))
        .then_some(self.candidates.as_slice())
    }
}

/// Separate domain and sequence from GPU metadata. Never send this envelope to
/// an older peer until a future capability negotiation explicitly permits it.
pub struct StorageSession {
    binding: [u8; 32],
    lease_id: u32,
    round: u64,
    retired: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    pending_request: Option<[u8; 16]>,
    pending_verified_request: Option<[u8; 16]>,
    session_identity: Arc<()>,
}
impl StorageSession {
    pub(crate) fn transport_round(&self) -> u64 {
        self.round
    }

    pub(crate) fn transport_identity(&self) -> Arc<()> {
        Arc::clone(&self.session_identity)
    }

    pub(crate) fn retire(&mut self) {
        self.retired.store(true, Ordering::SeqCst);
    }

    /// Call only after the connection's mutual handshake and lease verification.
    pub fn new(
        secret: &[u8],
        transcript: &HandshakeTranscript,
        challenge: &HostChallenge,
        lease_id: u32,
    ) -> Result<Self, FrameError> {
        if secret.is_empty()
            || lease_id == 0
            || challenge.accepted_version != IPC_VERSION_5
            || !(transcript.min_version..=transcript.max_version)
                .contains(&challenge.accepted_version)
        {
            return Err(refuse("missing key, lease or authenticated protocol"));
        }
        let binding =
            host_proof(secret, transcript, challenge).map_err(|e| refuse(&e.to_string()))?;
        Ok(Self {
            binding,
            lease_id,
            round: 1,
            retired: Arc::new(AtomicBool::new(false)),
            generation: Arc::new(AtomicU64::new(0)),
            pending_request: None,
            pending_verified_request: None,
            session_identity: Arc::new(()),
        })
    }
    fn proof_bytes(&self, body: &Body) -> Result<Vec<u8>, FrameError> {
        let data = serde_json::to_vec(body).map_err(|e| refuse(&e.to_string()))?;
        if data.len() > MAX_BYTES {
            return Err(refuse("body too large"));
        }
        let mut result = Vec::with_capacity(LABEL.len() + self.binding.len() + data.len());
        result.extend_from_slice(LABEL);
        result.extend_from_slice(&self.binding);
        result.extend_from_slice(&data);
        Ok(result)
    }

    fn request_proof_bytes(&self, body: &RequestBody) -> Result<Vec<u8>, FrameError> {
        let data = serde_json::to_vec(body).map_err(|e| refuse(&e.to_string()))?;
        let mut bytes = Vec::with_capacity(REQUEST_LABEL.len() + self.binding.len() + data.len());
        bytes.extend_from_slice(REQUEST_LABEL);
        bytes.extend_from_slice(&self.binding);
        bytes.extend_from_slice(&data);
        Ok(bytes)
    }

    /// Create one request after the negotiated capability permits it. If the
    /// transport fails or the outcome is uncertain, discard the session.
    pub fn request(&mut self, secret: &[u8], disk_id: [u8; 16]) -> Result<Vec<u8>, FrameError> {
        if secret.is_empty()
            || disk_id == [0; 16]
            || self.pending_request.is_some()
            || self.retired.load(Ordering::SeqCst)
        {
            return Err(refuse("invalid request identity or session"));
        }
        let body = RequestBody {
            lease_id: self.lease_id,
            round: self.round,
            disk_id,
        };
        let proof = compute_hmac(secret, &self.request_proof_bytes(&body)?);
        let bytes = serde_json::to_vec(&RequestEnvelope { body, proof })
            .map_err(|e| refuse(&e.to_string()))?;
        if bytes.len() > MAX_REQUEST_BYTES {
            return Err(refuse("request too large"));
        }
        self.pending_request = Some(disk_id);
        Ok(bytes)
    }

    /// Invalid requests retire this session and its existing observations.
    pub fn verify_request(
        &mut self,
        secret: &[u8],
        bytes: &[u8],
    ) -> Result<VerifiedStorageRequest, FrameError> {
        let result = (|| {
            if secret.is_empty()
                || bytes.len() > MAX_REQUEST_BYTES
                || self.retired.load(Ordering::SeqCst)
                || self.pending_verified_request.is_some()
            {
                return Err(refuse("invalid request or session"));
            }
            let request: RequestEnvelope =
                serde_json::from_slice(bytes).map_err(|e| refuse(&e.to_string()))?;
            if request.body.lease_id != self.lease_id
                || request.body.round != self.round
                || request.body.disk_id == [0; 16]
                || !verify_hmac(
                    secret,
                    &self.request_proof_bytes(&request.body)?,
                    &request.proof,
                )
            {
                return Err(refuse("request identity, round or proof mismatch"));
            }
            self.pending_verified_request = Some(request.body.disk_id);
            Ok(VerifiedStorageRequest {
                binding: self.binding,
                session_identity: Arc::clone(&self.session_identity),
                body: request.body,
            })
        })();
        if result.is_err() {
            self.retired.store(true, Ordering::SeqCst);
        }
        result
    }

    fn consume_verified_request(
        &mut self,
        request: VerifiedStorageRequest,
    ) -> Result<[u8; 16], FrameError> {
        if self.retired.load(Ordering::SeqCst)
            || self.pending_verified_request != Some(request.body.disk_id)
            || !Arc::ptr_eq(&self.session_identity, &request.session_identity)
            || request.binding != self.binding
            || request.body.lease_id != self.lease_id
            || request.body.round != self.round
        {
            self.retired.store(true, Ordering::SeqCst);
            return Err(refuse("request no longer belongs to this session"));
        }
        self.pending_verified_request = None;
        Ok(request.body.disk_id)
    }

    pub fn seal_requested(
        &mut self,
        secret: &[u8],
        request: VerifiedStorageRequest,
        complete: bool,
        candidates: Vec<StorageCandidate>,
    ) -> Result<Vec<u8>, FrameError> {
        let disk_id = self.consume_verified_request(request)?;
        let result = self.seal(secret, disk_id, complete, candidates);
        if result.is_err() {
            self.retired.store(true, Ordering::SeqCst);
        }
        result
    }

    /// Seal an authenticated transient-unavailable observation so a temporarily
    /// stalled host storage probe revokes guest storage capacity cleanly
    /// without dropping the multiplexed v5 vsock lease.
    pub fn seal_unavailable(
        &mut self,
        secret: &[u8],
        request: VerifiedStorageRequest,
    ) -> Result<Vec<u8>, FrameError> {
        let disk_id = self.consume_verified_request(request)?;
        let result = self.seal_body(
            secret,
            Body {
                lease_id: self.lease_id,
                round: self.round,
                root_id: disk_id,
                complete: false,
                candidates: Vec::new(),
            },
            false,
        );
        if result.is_err() {
            self.retired.store(true, Ordering::SeqCst);
        }
        result
    }

    fn seal(
        &mut self,
        secret: &[u8],
        root_id: [u8; 16],
        complete: bool,
        candidates: Vec<StorageCandidate>,
    ) -> Result<Vec<u8>, FrameError> {
        let body = Body {
            lease_id: self.lease_id,
            round: self.round,
            root_id,
            complete,
            candidates,
        };
        self.seal_body(secret, body, true)
    }

    fn seal_body(
        &mut self,
        secret: &[u8],
        body: Body,
        require_candidates: bool,
    ) -> Result<Vec<u8>, FrameError> {
        if self.retired.load(Ordering::SeqCst) {
            return Err(refuse("session retired"));
        }
        let next = self
            .round
            .checked_add(1)
            .ok_or_else(|| refuse("round exhausted"))?;
        let valid = if require_candidates {
            valid_body(&body)
        } else {
            body.root_id != [0; 16] && !body.complete && body.candidates.is_empty()
        };
        if secret.is_empty() || !valid {
            return Err(refuse("invalid candidate evidence"));
        }
        let proof = compute_hmac(secret, &self.proof_bytes(&body)?);
        let data =
            serde_json::to_vec(&Envelope { body, proof }).map_err(|e| refuse(&e.to_string()))?;
        if data.len() > MAX_BYTES {
            return Err(refuse("envelope too large"));
        }
        self.round = next;
        Ok(data)
    }

    /// `lease_remaining_ms` must be derived from the same authenticated live
    /// lease, measured at `started`; it cannot be taken from this envelope.
    pub fn open(
        &mut self,
        secret: &[u8],
        root_id: [u8; 16],
        data: &[u8],
        started: Instant,
        received: Instant,
        lease_remaining_ms: u64,
    ) -> Result<AuthenticatedStorage, FrameError> {
        match self.open_optional(secret, root_id, data, started, received, lease_remaining_ms)? {
            Some(storage) => Ok(storage),
            None => {
                self.retired.store(true, Ordering::SeqCst);
                Err(refuse("empty catalog"))
            }
        }
    }

    /// Verify a host response, distinguishing an authenticated transient
    /// unavailable observation (`Ok(None)`) from a full candidate catalog
    /// (`Ok(Some(..))`) without retiring the live v5 session.
    pub fn open_optional(
        &mut self,
        secret: &[u8],
        root_id: [u8; 16],
        data: &[u8],
        started: Instant,
        received: Instant,
        lease_remaining_ms: u64,
    ) -> Result<Option<AuthenticatedStorage>, FrameError> {
        if self.retired.load(Ordering::SeqCst) {
            return Err(refuse("session retired"));
        }
        let result = if self.pending_request.take() == Some(root_id) {
            self.open_inner(secret, root_id, data, started, received, lease_remaining_ms)
        } else {
            Err(refuse("response does not match an outstanding request"))
        };
        if result.is_err() {
            self.retired.store(true, Ordering::SeqCst);
        }
        result
    }

    fn open_inner(
        &mut self,
        secret: &[u8],
        root_id: [u8; 16],
        data: &[u8],
        started: Instant,
        received: Instant,
        lease_remaining_ms: u64,
    ) -> Result<Option<AuthenticatedStorage>, FrameError> {
        let next = self
            .round
            .checked_add(1)
            .ok_or_else(|| refuse("round exhausted"))?;
        if secret.is_empty() || data.len() > MAX_BYTES {
            return Err(refuse("missing key or oversized envelope"));
        }
        let envelope: Envelope =
            serde_json::from_slice(data).map_err(|e| refuse(&e.to_string()))?;
        let body = envelope.body;
        if body.lease_id != self.lease_id
            || body.round != self.round
            || body.root_id != root_id
            || !verify_hmac(secret, &self.proof_bytes(&body)?, &envelope.proof)
        {
            return Err(refuse("identity, sequence, proof or evidence mismatch"));
        }
        let transit = u64::try_from(
            received
                .checked_duration_since(started)
                .ok_or_else(|| refuse("clock moved backwards"))?
                .as_nanos()
                .div_ceil(1_000_000),
        )
        .map_err(|_| refuse("elapsed age overflow"))?;
        let lease = lease_remaining_ms
            .checked_sub(transit)
            .filter(|value| *value > 0)
            .ok_or_else(|| refuse("lease expired in transit"))?;
        if !body.complete && body.candidates.is_empty() && body.root_id != [0; 16] {
            let accepted_round = self.round;
            self.round = next;
            self.generation.store(accepted_round, Ordering::SeqCst);
            return Ok(None);
        }
        if !valid_body(&body) {
            return Err(refuse("identity, sequence, proof or evidence mismatch"));
        }
        let max_age = body
            .candidates
            .iter()
            .map(|item| item.age_ms)
            .max()
            .ok_or_else(|| refuse("empty catalog"))?;
        let age = max_age
            .checked_add(transit)
            .filter(|value| *value < MAX_STORAGE_AGE_MS)
            .ok_or_else(|| refuse("sample expired in transit"))?;
        let expires = received
            .checked_add(Duration::from_millis(lease.min(MAX_STORAGE_AGE_MS - age)))
            .ok_or_else(|| refuse("expiry overflow"))?;
        let accepted_round = self.round;
        self.round = next;
        self.generation.store(accepted_round, Ordering::SeqCst);
        Ok(Some(AuthenticatedStorage {
            retired: Arc::clone(&self.retired),
            generation: Arc::clone(&self.generation),
            round: accepted_round,
            lease_id: self.lease_id,
            root_id: body.root_id,
            candidates: body.candidates,
            #[cfg(target_os = "linux")]
            boot_deadline: None,
            received,
            expires,
        }))
    }
}

impl Drop for StorageSession {
    fn drop(&mut self) {
        self.retired.store(true, Ordering::SeqCst);
    }
}

/// Minimum host NTFS reserve preserved beneath WSL2 emergency swap allocation (1 GiB).
pub const HOST_STORAGE_RESERVE_BYTES: u64 = 1024 * 1024 * 1024;

/// Authenticated Windows volume telemetry published into `/run/ramshared/cache-status.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostStorageTelemetry {
    pub disk_id: String,
    pub volume_id: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub age_ms: u64,
    pub deadline: BootBoundStorageDeadline,
}

/// Bind guest-reported filesystem free space by authenticated Windows NTFS headroom.
pub fn bound_wsl_storage_free_bytes(guest_free_bytes: u64, host_available_bytes: u64) -> u64 {
    guest_free_bytes.min(host_available_bytes.saturating_sub(HOST_STORAGE_RESERVE_BYTES))
}

pub fn format_disk_id_hex(disk_id: &[u8; 16]) -> String {
    let mut hex = String::with_capacity(32);
    for byte in disk_id {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

pub fn parse_disk_id_hex(text: &str) -> Result<[u8; 16], String> {
    if text.len() != 32 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("disk-id must be 32 ASCII hexadecimal characters".into());
    }
    let mut out = [0u8; 16];
    let (chunks, _) = text.as_bytes().as_chunks::<2>();
    for (idx, chunk) in chunks.iter().enumerate() {
        let pair =
            std::str::from_utf8(chunk).map_err(|_| "invalid UTF-8 in disk-id".to_string())?;
        out[idx] = u8::from_str_radix(pair, 16)
            .map_err(|e| format!("invalid hex byte in disk-id: {e}"))?;
    }
    Ok(out)
}

#[cfg(test)]
#[path = "storage_proof/tests.rs"]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
