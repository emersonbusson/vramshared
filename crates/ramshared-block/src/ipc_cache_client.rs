// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/ipc_cache_client.rs
//! IPC cache client communicating with the isolated GPU cache worker.

use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use crate::gpu_cache_worker::{
    CACHED_KIB_UNKNOWN, CACHED_KIB_UNKNOWN_CAPABILITY, FRAME_HEADER_LEN, FrameHeader,
    MAX_IPC_PAYLOAD_BYTES, MSG_DISABLE_REQ, MSG_DISABLE_RESP, MSG_HANDSHAKE_REQ,
    MSG_HANDSHAKE_RESP, MSG_HEARTBEAT_REQ, MSG_HEARTBEAT_RESP, MSG_INVALIDATE_REQ, MSG_PROMOTE,
    MSG_READ_REQ, MSG_READ_RESP, MSG_UPDATE, STATUS_ERROR, STATUS_MISS, STATUS_OK,
};
use crate::isolated_origin::{BestEffortCache, CacheMutation, CacheRead, MAX_CACHE_MUTATION_BYTES};
use crate::origin_cache::CacheState;
use ramshared_vram::{
    GpuAdapterIdentity, GpuBudgetTelemetry, GpuUtilizationTelemetry,
    MAX_WORKER_TELEMETRY_PAYLOAD_BYTES, WorkerCacheTelemetry, WorkerTelemetryEnvelope,
};

mod adapter_binding;
mod deferred_invalidations;
mod operations;
mod slab_operations;
use adapter_binding::AdapterBinding;
use deferred_invalidations::DeferredInvalidations;

/// Synchronous IPC read/heartbeat budget for production clients.
///
/// Covers one GPU PTX decompress/compress batch (`CODEC_SUBDEADLINE = 20 ms`),
/// bounded unacknowledged promotes, and DXCore/NVML telemetry snapshots under
/// sustained swap pressure while still fail-closing a hung worker in 0.25 s.
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_millis(250);
/// Handshake covers worker **process startup**, not the IPC round-trip: the
/// child must dlopen CUDA/NVML, create a context, and complete WDDM budget
/// revalidation before it can answer. Measured 2.86 s at load 12 and past the
/// old 5 s bound by load ~42, so the startup budget is separate from the 5 s
/// teardown bound (SPEC: DT-2). Reads/heartbeats keep NFR-1's 50 ms.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// Disable/teardown uses a longer timeout to accommodate GPU context cleanup.
/// SPEC: DT-5 (5s bounded supervisor teardown).
pub const DISABLE_TIMEOUT: Duration = Duration::from_secs(5);
/// Minimum spacing between heartbeat round-trips. The serve loop ticks once
/// per NBD request; without this, telemetry would issue one IPC round-trip per
/// request and compete with data frames on the shared ordered stream (DT-1).
pub const MIN_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
/// Quiet time after a write-mirror frame before a cache read is issued again.
///
/// SPEC DT-1 puts mutations and reads on one ordered socket with no control
/// lane. Write mirrors are split into 64 KiB nonblocking frames that the
/// worker must drain sequentially; a read queued behind a burst waits for the
/// whole backlog and can miss NFR-1's 50 ms bound, which fail-closes the
/// cache. Promotes from the read path itself are exempt so a cold read stream
/// can populate the cache without suppressing its own future hits.
pub const READ_DRAIN_GRACE: Duration = Duration::from_millis(50);
/// After consecutive write mirrors, leave a bounded interval in which writes
/// invalidate stale entries instead of occupying the worker with new data.
/// This gives cache reads a quiet window even under a continuous write load.
pub const WRITE_MIRROR_PAUSE: Duration = Duration::from_millis(250);
/// Quiet time after any mutation frame before a heartbeat is issued.
///
/// Heartbeats are telemetry and may wait out a longer drain than a data read:
/// a heartbeat that collides with a mutation burst is the exact failure that
/// permanently revoked the cache under write load (worker EPIPE after the
/// parent's fail-closed shutdown).
pub const HEARTBEAT_DRAIN_GRACE: Duration = Duration::from_millis(200);
const MAX_UNACKED_PROMOTE_BYTES: usize = 256 * 1024;
const MAX_UNACKED_PROMOTE_FRAMES: u32 = 4;

/// Parent-side scheduling for round-trip requests on the shared ordered
/// stream (DT-1). Production uses [`PacingPolicy::default`] so telemetry and
/// cache reads never queue behind a mutation burst. Protocol cycle tests use
/// [`PacingPolicy::immediate`] to drive the wire without waiting out the
/// drain graces.
#[derive(Clone, Copy, Debug)]
pub struct PacingPolicy {
    pub read_drain_grace: Duration,
    pub heartbeat_drain_grace: Duration,
    pub min_heartbeat_interval: Duration,
}

impl Default for PacingPolicy {
    fn default() -> Self {
        Self {
            read_drain_grace: READ_DRAIN_GRACE,
            heartbeat_drain_grace: HEARTBEAT_DRAIN_GRACE,
            min_heartbeat_interval: MIN_HEARTBEAT_INTERVAL,
        }
    }
}

impl PacingPolicy {
    /// Issue every round-trip immediately. Used by protocol cycle tests that
    /// drive a worker already drained of mutations.
    pub const fn immediate() -> Self {
        Self {
            read_drain_grace: Duration::ZERO,
            heartbeat_drain_grace: Duration::ZERO,
            min_heartbeat_interval: Duration::ZERO,
        }
    }
}
/// Heartbeat payload ceiling (DT-8). An envelope larger than this is rejected
/// whole — never truncated into a plausible-looking sample.
const MAX_GPU_BUDGET_PAYLOAD_BYTES: u32 = MAX_WORKER_TELEMETRY_PAYLOAD_BYTES as u32;

/// Largest payload a single cache mutation frame may carry. One frame is one
/// nonblocking send: the origin path must never block on the GPU worker, so a
/// logical mutation larger than this is split by the caller into one frame per
/// slice (see `AuthoritativeOriginBackend::update_cache`).
pub const MAX_MUTATION_FRAME_DATA_BYTES: usize = MAX_CACHE_MUTATION_BYTES;

/// Ceiling on parent-side unwritten mutation bytes (DT-10). The socket can be
/// backpressured while the worker drains `handle_update`; the queue absorbs the
/// burst without stalling origin I/O and without abandoning a mid-frame write.
/// An `Update` that cannot enter the queue revokes the cache; a `Promote` is
/// simply not cached.
pub const MAX_PENDING_MUTATION_BYTES: usize = 4 * 1024 * 1024;

fn deadline_after(timeout: Duration) -> io::Result<Instant> {
    Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "IPC deadline overflow"))
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    let duration = deadline.saturating_duration_since(Instant::now());
    if duration.is_zero() {
        Err(io::Error::new(ErrorKind::TimedOut, "IPC deadline expired"))
    } else {
        Ok(duration)
    }
}

fn read_exact_until(
    socket: &mut UnixStream,
    mut buffer: &mut [u8],
    deadline: Instant,
) -> io::Result<()> {
    while !buffer.is_empty() {
        socket.set_read_timeout(Some(remaining(deadline)?))?;
        match socket.read(buffer) {
            Ok(0) => {
                return Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "IPC stream closed",
                ));
            }
            Ok(read) => buffer = &mut buffer[read..],
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn write_all_until(
    socket: &mut UnixStream,
    mut buffer: &[u8],
    deadline: Instant,
) -> io::Result<()> {
    while !buffer.is_empty() {
        socket.set_write_timeout(Some(remaining(deadline)?))?;
        match socket.write(buffer) {
            Ok(0) => {
                return Err(io::Error::new(
                    ErrorKind::WriteZero,
                    "IPC stream made no progress",
                ));
            }
            Ok(written) => buffer = &buffer[written..],
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Outcome of offering a complete mutation frame to the pending queue.
enum MutationQueue {
    /// The frame is committed to delivery: fully on the wire, or whole in the
    /// pending queue behind earlier frames.
    Queued,
    /// The queue is full and the frame was not stored. The caller must not
    /// pretend the mutation will reach the worker.
    Dropped,
    /// The peer is gone. Only a deterministic write error produces this;
    /// backpressure is [`MutationQueue::Queued`] or [`MutationQueue::Dropped`].
    PeerGone(io::Error),
}

/// Drains as much of `pending` as the socket accepts without blocking.
/// Returns `Ok(())` only when `pending` is empty.
fn drain_pending(socket: &mut UnixStream, pending: &mut Vec<u8>) -> io::Result<()> {
    while !pending.is_empty() {
        match socket.write(pending) {
            Ok(0) => {
                return Err(io::Error::new(
                    ErrorKind::WriteZero,
                    "IPC stream made no progress",
                ));
            }
            Ok(written) => {
                pending.drain(..written);
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(()),
            Err(error) if error.kind() == ErrorKind::TimedOut => return Ok(()),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Offers `frame` to the stream, storing any unwritten tail in `pending`.
///
/// The socket is nonblocking for the duration so origin I/O can never stall
/// behind a full socket buffer (DT-2/DT-10). A short write is not an error: the
/// remainder waits in `pending` and is completed by a later drain, which keeps
/// the byte stream framed. Only a deterministic peer error is fatal.
fn queue_frame(socket: &mut UnixStream, pending: &mut Vec<u8>, frame: &[u8]) -> MutationQueue {
    if let Err(error) = socket.set_nonblocking(true) {
        return MutationQueue::PeerGone(error);
    }
    let result = queue_frame_nonblocking(socket, pending, frame);
    if let Err(error) = socket.set_nonblocking(false)
        && matches!(result, MutationQueue::Queued)
    {
        return MutationQueue::PeerGone(error);
    }
    result
}

fn queue_frame_nonblocking(
    socket: &mut UnixStream,
    pending: &mut Vec<u8>,
    frame: &[u8],
) -> MutationQueue {
    match drain_pending(socket, pending) {
        Ok(()) => {}
        Err(error) => return MutationQueue::PeerGone(error),
    }
    if !pending.is_empty() {
        // An earlier frame is still draining. The stream is mid-frame, so a new
        // frame can only join the queue; it must not be interleaved on the wire.
        return if pending.len().saturating_add(frame.len()) > MAX_PENDING_MUTATION_BYTES {
            MutationQueue::Dropped
        } else {
            pending.extend_from_slice(frame);
            MutationQueue::Queued
        };
    }
    let mut rest = frame;
    while !rest.is_empty() {
        match socket.write(rest) {
            Ok(0) => {
                return MutationQueue::PeerGone(io::Error::new(
                    ErrorKind::WriteZero,
                    "IPC stream made no progress",
                ));
            }
            Ok(written) => rest = &rest[written..],
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error)
                if error.kind() == ErrorKind::WouldBlock || error.kind() == ErrorKind::TimedOut =>
            {
                break;
            }
            Err(error) => return MutationQueue::PeerGone(error),
        }
    }
    if !rest.is_empty() {
        if rest.len() > MAX_PENDING_MUTATION_BYTES {
            return MutationQueue::Dropped;
        }
        pending.extend_from_slice(rest);
    }
    MutationQueue::Queued
}

/// Completes any unwritten mutation bytes without blocking.
///
/// `Ok(true)` means the stream is framed again (nothing pending) and a request
/// may be written. `Ok(false)` means the queue still holds a mid-frame tail: a
/// cache read must degrade to a miss rather than corrupt the stream (DT-10).
fn try_drain_pending(socket: &mut UnixStream, pending: &mut Vec<u8>) -> io::Result<bool> {
    if pending.is_empty() {
        return Ok(true);
    }
    socket.set_nonblocking(true)?;
    let drain_result = drain_pending(socket, pending);
    let restore_result = socket.set_nonblocking(false);
    drain_result.and(restore_result)?;
    Ok(pending.is_empty())
}

pub struct IpcCacheClient {
    socket: UnixStream,
    read_timeout: Duration,
    timeouts_configured: bool,
    state: CacheState,
    cached_bytes: u64,
    cached_bytes_known: bool,
    /// Capacity the worker **admitted**, not the figure this client asked for.
    ///
    /// Seeded with the request only so a pre-handshake consumer has something
    /// to show; `perform_handshake` overwrites it with the worker's `offset`,
    /// including zero, and `target_bytes()` reports 0 unless the cache is
    /// `Active`. Requested and admitted are therefore never confused at the
    /// reporting surface (AUD-35).
    target_bytes: u64,
    gpu_budget: Option<GpuBudgetTelemetry>,
    adapter_binding: AdapterBinding,
    /// Adapter busy time from the same heartbeat as the budget. `None` is
    /// "not measured", never an idle adapter.
    gpu_utilization: Option<GpuUtilizationTelemetry>,
    cache_telemetry: Option<WorkerCacheTelemetry>,
    seq: u64,
    /// Last write-mirror (`MSG_UPDATE`) queue stamp, used to defer cache reads
    /// while the worker drains a write burst.
    last_update_at: Option<Instant>,
    mirror_pause_until: Option<Instant>,
    /// Last mutation stamp of any kind (update or promote), used to defer
    /// heartbeats while the worker drains.
    last_mutation_at: Option<Instant>,
    /// Last successful heartbeat, used to space telemetry round-trips.
    last_heartbeat_at: Option<Instant>,
    /// Unwritten mutation-frame bytes (DT-10). Non-empty means the byte stream
    /// is mid-frame: a request would corrupt it, so reads degrade to a miss
    /// until the queue drains.
    pending_frame: Vec<u8>,
    deferred_invalidations: DeferredInvalidations,
    unacked_promote_bytes: usize,
    unacked_promote_frames: u32,
    pacing: PacingPolicy,
}

impl IpcCacheClient {
    pub fn new(socket: UnixStream, read_timeout: Duration, target_bytes: u64) -> Self {
        Self::with_pacing(socket, read_timeout, target_bytes, PacingPolicy::default())
    }

    pub fn with_pacing(
        socket: UnixStream,
        read_timeout: Duration,
        target_bytes: u64,
        pacing: PacingPolicy,
    ) -> Self {
        let timeouts_configured = !read_timeout.is_zero()
            && socket.set_read_timeout(Some(read_timeout)).is_ok()
            && socket.set_write_timeout(Some(read_timeout)).is_ok();
        if !timeouts_configured {
            eprintln!("[ramsharedd] isolated GPU cache unavailable: IPC timeout setup failed");
        }
        Self {
            socket,
            read_timeout,
            timeouts_configured,
            state: if timeouts_configured {
                CacheState::Active
            } else {
                CacheState::Unavailable
            },
            cached_bytes: 0,
            cached_bytes_known: false,
            target_bytes,
            gpu_budget: None,
            adapter_binding: AdapterBinding::default(),
            gpu_utilization: None,
            cache_telemetry: None,
            seq: 0,
            last_update_at: None,
            mirror_pause_until: None,
            last_mutation_at: None,
            last_heartbeat_at: None,
            pending_frame: Vec::new(),
            deferred_invalidations: DeferredInvalidations::default(),
            unacked_promote_bytes: 0,
            unacked_promote_frames: 0,
            pacing,
        }
    }

    /// Set the immutable adapter request before any wire operation.
    pub fn expect_adapter(&mut self, identity: GpuAdapterIdentity) -> Result<(), &'static str> {
        if self.seq != 0 || self.adapter_binding.identity().is_some() {
            return Err("worker adapter request cannot be replaced after binding or I/O");
        }
        self.adapter_binding = AdapterBinding::expected(identity)?;
        Ok(())
    }

    /// Static replacement identity; this is never a live budget or a lease.
    pub fn bound_adapter_identity(&self) -> Option<&GpuAdapterIdentity> {
        self.adapter_binding.identity()
    }

    pub fn perform_handshake(&mut self) -> Result<(), String> {
        if !self.timeouts_configured || self.state != CacheState::Active {
            return Err("IPC timeout configuration is unavailable".to_string());
        }
        self.seq = self.seq.saturating_add(1);
        let req = FrameHeader {
            msg_type: MSG_HANDSHAKE_REQ,
            status: STATUS_OK,
            correlation_id: self.seq,
            offset: 0,
            payload_len: 0,
            aux: CACHED_KIB_UNKNOWN_CAPABILITY,
        };
        let deadline = match deadline_after(HANDSHAKE_TIMEOUT) {
            Ok(deadline) => deadline,
            Err(error) => {
                self.fail("handshake deadline setup failed");
                return Err(format!("handshake deadline setup failed: {error}"));
            }
        };
        if let Err(error) = write_all_until(&mut self.socket, &req.encode(), deadline) {
            self.fail("handshake write failed");
            return Err(format!("handshake write error: {error}"));
        }

        let mut buf = [0u8; FRAME_HEADER_LEN];
        if let Err(error) = read_exact_until(&mut self.socket, &mut buf, deadline) {
            self.fail("handshake read failed");
            return Err(format!("handshake read error: {error}"));
        }
        if let Err(error) = self
            .socket
            .set_read_timeout(Some(self.read_timeout))
            .and_then(|()| self.socket.set_write_timeout(Some(self.read_timeout)))
        {
            self.fail("steady-state read timeout restore failed");
            return Err(format!("handshake timeout restore failed: {error}"));
        }

        let Some(resp) = FrameHeader::decode(&buf) else {
            self.fail("handshake response header malformed");
            return Err("invalid handshake response header".to_string());
        };
        // AUD-34: the handshake response is a payloadless frame carrying a
        // known status and the worker's capacity figure. A frame that does not
        // have that shape is not a handshake answer.
        if resp.msg_type != MSG_HANDSHAKE_RESP
            || resp.correlation_id != self.seq
            || resp.payload_len != 0
            || (resp.status != STATUS_OK && resp.status != STATUS_ERROR)
        {
            self.fail("handshake response mismatched");
            return Err("invalid handshake response".to_string());
        }
        if resp.status != STATUS_OK {
            // A refusal is not a fault: no capacity was admitted and no repair
            // is required (AUD-35).
            self.fail("handshake response status failed");
            return Err("handshake refused".to_string());
        }
        // AUD-35: `offset` is the capacity the worker **admitted**, including
        // zero. Taking it only when positive would keep the requested figure
        // and then report Active against capacity that was never granted —
        // the zero-target refusal becomes invisible to every consumer.
        self.target_bytes = resp.offset;
        self.observe_cached_kib(resp.aux);
        // target_bytes == 0 means the worker has no VRAM provider (GAP-6):
        // report Unavailable so telemetry and cascade gates see the truth.
        // A capacity refusal is not a fault and needs no destructive repair.
        if self.target_bytes == 0 {
            self.state = CacheState::Unavailable;
        } else {
            self.state = CacheState::Active;
        }
        Ok(())
    }

    /// Drops the heartbeat samples this client is holding.
    ///
    /// `gpu_budget` and `cache_telemetry` are a snapshot of a live worker at
    /// `sampled_at_unix_ms`, not a property of the client. They are only
    /// meaningful while the cache is still `Active` and a later heartbeat can
    /// replace them. Once that stops being true, keeping the last sample would
    /// let a publisher republish it under a fresh `written_at_unix_ms`, turning
    /// a stale sample into a bound object that only a freshness predicate can
    /// catch. Clearing is what makes the honest state visible as `None`.
    fn clear_held_telemetry(&mut self) {
        self.gpu_budget = None;
        self.gpu_utilization = None;
        self.cache_telemetry = None;
    }

    pub(super) fn clear_unacked_mutation_window(&mut self) {
        self.unacked_promote_bytes = 0;
        self.unacked_promote_frames = 0;
        self.last_mutation_at = None;
    }

    fn fail(&mut self, reason: impl std::fmt::Display) {
        eprintln!("[ramsharedd] isolated GPU cache unavailable: {reason}");
        self.state = CacheState::Unavailable;
        self.cached_bytes = 0;
        self.cached_bytes_known = false;
        self.clear_held_telemetry();
        self.clear_unacked_mutation_window();
        self.pending_frame.clear();
        self.deferred_invalidations.clear();
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
    }

    /// True while the worker is likely still draining a mutation burst, so a
    /// round-trip request would queue behind it and miss NFR-1's bound.
    fn write_mirrors_are_draining(&self) -> bool {
        self.last_update_at
            .is_some_and(|at| at.elapsed() < self.pacing.read_drain_grace)
    }

    fn pause_write_mirror(&mut self, now: Instant) -> bool {
        if self.pacing.read_drain_grace.is_zero() {
            return false;
        }
        if self.mirror_pause_until.is_some_and(|until| now < until) {
            return true;
        }
        self.mirror_pause_until = None;
        if self.last_update_at.is_some_and(|at| {
            now.checked_duration_since(at)
                .is_some_and(|age| age < self.pacing.read_drain_grace)
        }) {
            self.mirror_pause_until = now.checked_add(WRITE_MIRROR_PAUSE);
            return self.mirror_pause_until.is_some();
        }
        false
    }

    fn mutations_are_draining(&self) -> bool {
        self.last_mutation_at
            .is_some_and(|at| at.elapsed() < self.pacing.heartbeat_drain_grace)
    }

    fn heartbeat_is_due(&self) -> bool {
        self.last_heartbeat_at
            .is_none_or(|at| at.elapsed() >= self.pacing.min_heartbeat_interval)
    }

    fn observe_cached_kib(&mut self, value: u32) {
        self.cached_bytes_known = value != CACHED_KIB_UNKNOWN;
        self.cached_bytes = if self.cached_bytes_known {
            u64::from(value) << 10
        } else {
            0
        };
    }

    fn observed_cached_bytes(&self) -> Result<u64, &'static str> {
        self.cached_bytes_known
            .then_some(self.cached_bytes)
            .ok_or("GPU cache physical occupancy is unknown")
    }

    pub fn refresh_cached_bytes(&mut self) -> Result<u64, &'static str> {
        match deadline_after(self.read_timeout) {
            Ok(deadline) => crate::BoundedCache::refresh_until(self, deadline),
            Err(_) => {
                self.fail("heartbeat deadline setup failed");
                Err("GPU cache worker heartbeat deadline could not be configured")
            }
        }
    }

    /// Adapter budget from the last heartbeat (its own schema, unchanged).
    pub fn gpu_budget_telemetry(&self) -> Option<&GpuBudgetTelemetry> {
        self.gpu_budget.as_ref()
    }

    /// Adapter busy time from the last heartbeat.
    ///
    /// `None` is "not measured" — the worker has no utilization source, or the
    /// driver refused the call. It is never a zero-percent idle sample.
    pub fn gpu_utilization_telemetry(&self) -> Option<&GpuUtilizationTelemetry> {
        self.gpu_utilization.as_ref()
    }

    /// Cache occupancy and codec health from the last heartbeat.
    ///
    /// Logical cache bytes are cache occupancy in the worker's address space —
    /// never guest or host RAM (DT-8).
    pub fn cache_telemetry(&self) -> Option<&WorkerCacheTelemetry> {
        self.cache_telemetry.as_ref()
    }

    fn send_mutation_frame(&mut self, header: FrameHeader, payload: &[u8]) -> CacheMutation {
        if self.state != CacheState::Active {
            return CacheMutation::Skipped;
        }
        if payload.len() > MAX_MUTATION_FRAME_DATA_BYTES {
            self.fail("cache mutation exceeds nonblocking frame limit");
            return CacheMutation::Failed;
        }
        // Assemble the complete frame before offering it to the queue.
        let mut frame = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
        frame.extend_from_slice(&header.encode());
        frame.extend_from_slice(payload);

        match queue_frame(&mut self.socket, &mut self.pending_frame, &frame) {
            MutationQueue::Queued => {}
            MutationQueue::Dropped => {
                // DT-10/DT-11: a dropped `Promote` only means "not cached",
                // so a later read is a miss — the correct answer. A dropped
                // `Update` would leave pre-write bytes in the worker that a
                // later Hit could serve, so it is converted into
                // `MSG_INVALIDATE` for exactly that range instead of
                // revoking the whole cache.
                if header.msg_type == MSG_UPDATE {
                    return self.convert_dropped_update_to_invalidate(header.offset, payload.len());
                }
                return CacheMutation::Skipped;
            }
            MutationQueue::PeerGone(error) => {
                // Surface the errno: a deterministic peer error is the only
                // remaining reason a mutation send fails closed (DT-10), and
                // operators need to tell EPIPE from a bad descriptor.
                eprintln!("[ramsharedd] mutation frame write error: {error}");
                self.fail("nonblocking mutation frame write failed");
                return CacheMutation::Failed;
            }
        }

        // Accepted bytes are queued, not evidence of GPU allocation: keep the
        // last confirmed occupancy sample and let the next heartbeat replace
        // it. Zeroing here would make every mutation report an empty cache.
        let now = Instant::now();
        self.last_mutation_at = Some(now);
        if header.msg_type == MSG_UPDATE {
            self.last_update_at = Some(now);
        } else if header.msg_type == MSG_PROMOTE {
            self.unacked_promote_bytes = self.unacked_promote_bytes.saturating_add(frame.len());
            self.unacked_promote_frames = self.unacked_promote_frames.saturating_add(1);
        }
        CacheMutation::Accepted
    }
}

impl BestEffortCache for IpcCacheClient {
    fn read(&mut self, offset: u64, destination: &mut [u8]) -> CacheRead {
        match deadline_after(self.read_timeout) {
            Ok(deadline) => crate::BoundedCache::read_until(self, offset, destination, deadline),
            Err(_) => {
                self.fail("read deadline setup failed");
                CacheRead::Failed
            }
        }
    }

    fn update(&mut self, offset: u64, data: &[u8]) -> CacheMutation {
        if self.state != CacheState::Active {
            return CacheMutation::Skipped;
        }
        if data.len() > MAX_MUTATION_FRAME_DATA_BYTES {
            self.fail("cache mutation exceeds nonblocking frame limit");
            return CacheMutation::Failed;
        }
        if !self.deferred_invalidations.is_empty() {
            let result = self.defer_invalidation(offset, data.len());
            if result == CacheMutation::Skipped {
                let _ = self.flush_deferred_invalidations();
                if self.state != CacheState::Active {
                    return CacheMutation::Failed;
                }
            }
            return result;
        }
        if self.pause_write_mirror(Instant::now()) {
            return self.convert_dropped_update_to_invalidate(offset, data.len());
        }
        self.seq = self.seq.saturating_add(1);
        let req = FrameHeader {
            msg_type: MSG_UPDATE,
            status: STATUS_OK,
            correlation_id: self.seq,
            offset,
            payload_len: data.len() as u32,
            aux: 0,
        };
        self.send_mutation_frame(req, data)
    }

    fn promote(&mut self, offset: u64, data: &[u8]) -> CacheMutation {
        if !self.deferred_invalidations.is_empty() {
            return CacheMutation::Skipped;
        }
        if !self.pacing.read_drain_grace.is_zero()
            && (self.unacked_promote_bytes >= MAX_UNACKED_PROMOTE_BYTES
                || self.unacked_promote_frames >= MAX_UNACKED_PROMOTE_FRAMES)
        {
            return CacheMutation::Skipped;
        }
        self.seq = self.seq.saturating_add(1);
        let req = FrameHeader {
            msg_type: MSG_PROMOTE,
            status: STATUS_OK,
            correlation_id: self.seq,
            offset,
            payload_len: data.len() as u32,
            aux: 0,
        };
        self.send_mutation_frame(req, data)
    }

    fn disable(&mut self) -> CacheMutation {
        match deadline_after(DISABLE_TIMEOUT) {
            Ok(deadline) => crate::BoundedCache::disable_until(self, deadline),
            Err(_) => {
                self.clear_held_telemetry();
                self.state = CacheState::Stuck;
                CacheMutation::Failed
            }
        }
    }

    fn state(&self) -> CacheState {
        self.state
    }

    fn cached_bytes(&self) -> u64 {
        if self.state == CacheState::Active {
            self.cached_bytes
        } else {
            0
        }
    }

    fn refresh_cached_bytes(&mut self) -> Result<u64, &'static str> {
        IpcCacheClient::refresh_cached_bytes(self)
    }

    fn target_bytes(&self) -> u64 {
        // Admitted capacity, never a desired one (AUD-35). Same gate as
        // `cached_bytes`: a client that is Off, Stuck, Unavailable or that
        // never completed a handshake has admitted nothing, so it reports 0
        // rather than the figure it asked for.
        if self.state == CacheState::Active {
            self.target_bytes
        } else {
            0
        }
    }

    fn bound_adapter_identity(&self) -> Option<GpuAdapterIdentity> {
        IpcCacheClient::bound_adapter_identity(self).cloned()
    }

    fn gpu_budget_telemetry(&self) -> Option<GpuBudgetTelemetry> {
        IpcCacheClient::gpu_budget_telemetry(self).cloned()
    }

    fn gpu_utilization_telemetry(&self) -> Option<GpuUtilizationTelemetry> {
        IpcCacheClient::gpu_utilization_telemetry(self).cloned()
    }

    fn cache_telemetry(&self) -> Option<WorkerCacheTelemetry> {
        IpcCacheClient::cache_telemetry(self).cloned()
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod identity_tests;

#[cfg(test)]
mod operation_tests;
