// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/gpu_cache_worker.rs
//! Process-isolated GPU cache worker.
//!
//! Provides out-of-process VRAM allocation and cache chunk management
//! communicating over an anonymous Unix domain socket pair.

mod allocation_budget;
mod compressed_codec;
pub mod host_budget;
mod slab_ipc;
mod slab_pages;
mod telemetry;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ramshared_vram::{
    GpuBudgetSnapshot, GpuBudgetTelemetry, GpuUtilizationTelemetry, VramError, VramMemory,
    VramProvider, WorkerTelemetryEnvelope,
};

use crate::compressed_cache::MAX_READ_LEN;
use crate::gpu_worker_slab_bridge::GpuWorkerSlabBridge;

use compressed_codec::CompressedStore;
pub use compressed_codec::{CACHE_READ_BUDGET, CODEC_SUBDEADLINE};
pub use slab_ipc::{
    MSG_SLAB_COMPRESS_BATCH_REQ, MSG_SLAB_COMPRESS_BATCH_RESP, MSG_SLAB_RECOVER_BATCH_REQ,
    MSG_SLAB_RECOVER_BATCH_RESP, MSG_SLAB_TELEMETRY_REQ, MSG_SLAB_TELEMETRY_RESP,
    decode_compress_batch_resp, decode_recover_batch_resp, encode_compress_batch_req,
    encode_recover_batch_req, run_gpu_worker_loop_with_device_codec,
};

pub const FRAME_HEADER_LEN: usize = 32;
pub const MAX_IPC_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;

pub const MSG_READ_REQ: u8 = 1;
pub const MSG_READ_RESP: u8 = 2;
pub const MSG_UPDATE: u8 = 3;
pub const MSG_PROMOTE: u8 = 4;
pub const MSG_DISABLE_REQ: u8 = 5;
pub const MSG_DISABLE_RESP: u8 = 6;
pub const MSG_HEARTBEAT_REQ: u8 = 7;
pub const MSG_HEARTBEAT_RESP: u8 = 8;
pub const MSG_HANDSHAKE_REQ: u8 = 9;
pub const MSG_HANDSHAKE_RESP: u8 = 10;
/// DT-11: parent → worker, no payload. `offset` is the range start and `aux`
/// its length. The worker drops every cached entry overlapping that range.
pub const MSG_INVALIDATE_REQ: u8 = 11;

pub const STATUS_OK: u8 = 0;
pub const STATUS_MISS: u8 = 1;
pub const STATUS_ERROR: u8 = 2;
/// Runtime headroom admitted above the configured reserve.
///
/// Re-exported from the single reserve authority rather than defined here: the
/// literal must match what `ramshared-cli` reports and what every other
/// admission surface enforces, and a second copy is a drift hazard the seal
/// cannot detect.
pub use ramshared_vram::RUNTIME_FREE_BUFFER_BYTES;
/// Absolute budget for one complete worker frame read (header plus payload).
/// Production clients heartbeat at least every few seconds (the daemon serve
/// loop ticks every five seconds), so a peer silent or mid-frame longer than
/// this budget is stalled and the worker must fail closed instead of blocking
/// until process teardown.
pub const WORKER_FRAME_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Free-bytes floor this worker demands before it will *recover* a cache
/// entry, on top of the configured reserve.
///
/// Deliberately not in `ramshared_vram::reserve_policy`: that authority owns the
/// **admission** headroom every surface passes to `required_free_bytes`, while
/// this is a recovery-path cushion used only here. It is still one literal — a
/// mirror elsewhere would drift the same way a second `2048` would.
const RUNTIME_RECOVERY_BUFFER_BYTES: u64 = 384 * 1024 * 1024;

/// Encodes cached bytes as KiB in the legacy `u32` IPC field without wrapping.
///
/// Cached bytes are a **hold** — occupancy of the cache — so this ceils. A
/// cache holding 512 bytes is a cache holding something, and publishing it as
/// `0 KiB` prints a non-zero measurement as zero. The same
/// direction is `hold_kib` in the daemon status publisher and
/// `hold_mib_label_from_bytes` in `readiness::render`; this is the IPC face of
/// that rule at the KiB unit.
///
/// The largest wire value denotes unknown; known values saturate one below it.
/// Reserved wire value for an unknown physical sample. A new worker must not
/// be paired with an older client that interprets this value as 4 TiB.
pub(crate) const CACHED_KIB_UNKNOWN: u32 = u32::MAX;
/// Handshake capability: the peer understands the unknown occupancy marker.
pub(crate) const CACHED_KIB_UNKNOWN_CAPABILITY: u32 = 0x5253_0001;

fn cached_kib_aux(cached_bytes: u64) -> u32 {
    u32::try_from(cached_bytes.div_ceil(1024))
        .unwrap_or(u32::MAX - 1)
        .min(u32::MAX - 1)
}

fn cached_kib_observation(cached_bytes: Option<u64>) -> u32 {
    cached_bytes.map_or(CACHED_KIB_UNKNOWN, cached_kib_aux)
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or_default()
}

fn effective_target_from_budget(budget: &GpuBudgetSnapshot, config: GpuWorkerConfig) -> u64 {
    if !budget.can_admit(0) {
        return 0;
    }
    budget.safe_target_bytes(
        config.target_bytes,
        config.reserve_floor_bytes,
        RUNTIME_FREE_BUFFER_BYTES,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameHeader {
    pub msg_type: u8,
    pub status: u8,
    pub correlation_id: u64,
    pub offset: u64,
    pub payload_len: u32,
    pub aux: u32,
}

impl FrameHeader {
    pub fn encode(&self) -> [u8; FRAME_HEADER_LEN] {
        let mut buf = [0u8; FRAME_HEADER_LEN];
        buf[0] = self.msg_type;
        buf[1] = self.status;
        buf[8..16].copy_from_slice(&self.correlation_id.to_le_bytes());
        buf[16..24].copy_from_slice(&self.offset.to_le_bytes());
        buf[24..28].copy_from_slice(&self.payload_len.to_le_bytes());
        buf[28..32].copy_from_slice(&self.aux.to_le_bytes());
        buf
    }

    pub fn decode(buf: &[u8; FRAME_HEADER_LEN]) -> Option<Self> {
        if buf[2..8].iter().any(|byte| *byte != 0) {
            return None;
        }

        let msg_type = buf[0];
        let status = buf[1];
        let correlation_id = u64::from_le_bytes([
            buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
        ]);
        let offset = u64::from_le_bytes([
            buf[16], buf[17], buf[18], buf[19], buf[20], buf[21], buf[22], buf[23],
        ]);
        let payload_len = u32::from_le_bytes([buf[24], buf[25], buf[26], buf[27]]);
        let aux = u32::from_le_bytes([buf[28], buf[29], buf[30], buf[31]]);
        Some(Self {
            msg_type,
            status,
            correlation_id,
            offset,
            payload_len,
            aux,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GpuWorkerConfig {
    pub target_bytes: u64,
    pub chunk_bytes: usize,
    /// Configured reserve floor, supplied by the sealed policy via
    /// `--reserve-floor` (`ReserveFloorPolicy::min_floor_bytes`).
    ///
    /// `0` means "no configured floor" (DT-9 case 3) and is safe: the shared
    /// helpers still apply the non-negotiable `capacity.div_ceil(5)` share
    /// before any allocation. Production always passes the sealed
    /// `gpu_reserve_min_mib` and the CLI parser requires the flag, so this
    /// field can never silently undercut the seal (RF-1, RF-5).
    pub reserve_floor_bytes: u64,
    /// DT-9: compression is **off by default**. The raw cache is the product;
    /// the codec is an opt-in accelerator on top of a working raw cache.
    ///
    /// Even when enabled, nothing compresses unless the selected provider
    /// reports a codec capability and the encoded form is strictly smaller
    /// than the raw payload (DT-10 usefulness gate).
    pub compression_enabled: bool,
}

impl Default for GpuWorkerConfig {
    fn default() -> Self {
        Self {
            target_bytes: 4 * 1024 * 1024 * 1024,
            chunk_bytes: 2 * 1024 * 1024,
            // DT-9 case 3, not a silent floor: `0` keeps only the shared
            // `capacity.div_ceil(5)` share. A `1536 MiB` default here was the
            // superseded PRD mitigation figure and a competing constant the
            // seal cannot detect (RF-1). Production supplies the sealed
            // `min_floor_bytes` through the required `--reserve-floor` flag.
            reserve_floor_bytes: 0,
            compression_enabled: false,
        }
    }
}

struct CacheChunk<'p, P: VramProvider + 'p> {
    mem: P::Mem<'p>,
    last_accessed: Instant,
    valid_ranges: Vec<(u64, u64)>,
}

impl<P: VramProvider> CacheChunk<'_, P> {
    fn contains(&self, start: u64, end: u64) -> bool {
        self.valid_ranges
            .iter()
            .any(|&(valid_start, valid_end)| valid_start <= start && end <= valid_end)
    }

    fn mark_valid(&mut self, start: u64, end: u64) {
        self.valid_ranges.push((start, end));
        self.valid_ranges.sort_unstable_by_key(|range| range.0);
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(self.valid_ranges.len());
        for (start, end) in self.valid_ranges.drain(..) {
            if let Some(last) = merged.last_mut()
                && start <= last.1
            {
                last.1 = last.1.max(end);
                continue;
            }
            merged.push((start, end));
        }
        self.valid_ranges = merged;
    }

    /// Drops every valid sub-range overlapping `[start, end)` (DT-11).
    ///
    /// Remnants outside the hole stay valid, so a partial invalidate keeps the
    /// bytes it did not cover readable and a later read never sees the punched
    /// span.
    fn invalidate_range(&mut self, start: u64, end: u64) {
        if start >= end {
            return;
        }
        let mut kept: Vec<(u64, u64)> = Vec::with_capacity(self.valid_ranges.len() + 1);
        for &(valid_start, valid_end) in &self.valid_ranges {
            if valid_end <= start || end <= valid_start {
                kept.push((valid_start, valid_end));
                continue;
            }
            if valid_start < start {
                kept.push((valid_start, start));
            }
            if end < valid_end {
                kept.push((end, valid_end));
            }
        }
        self.valid_ranges = kept;
    }
}

pub struct GpuCacheWorker<'p, P: VramProvider + 'p> {
    provider: &'p P,
    config: GpuWorkerConfig,
    effective_target_bytes: u64,
    chunks: HashMap<u64, CacheChunk<'p, P>>,
    disabled: bool,
    pressure_constrained: bool,
    compressed: CompressedStore,
    /// Provider-owned slab backing for the compressed store, one per allocator
    /// slab, in creation order. `slab_index` addresses into this list.
    compressed_slabs: Vec<P::Mem<'p>>,
    /// Provider-owned scratch for one codec call (DT-4 workspace).
    codec_workspace: Option<P::Mem<'p>>,
    /// Compressed page-slab admission. `None` unless compression is enabled
    /// and the budget can hold the staging arena plus one frame.
    slab_bridge: Option<GpuWorkerSlabBridge<'p>>,
    slab_pages: std::collections::BTreeMap<u64, ramshared_vram::slab_pool::VramSlabTicket>,
    slab_frame_pages: std::collections::BTreeSet<(u32, u64)>,
    host_metadata_guard: Option<host_budget::WorkerHostMetadataGuard>,
    device_codec_reservation: u64,
}

impl<'p, P: VramProvider + 'p> GpuCacheWorker<'p, P> {
    pub fn new(provider: &'p P, config: GpuWorkerConfig) -> Self {
        Self::new_with_device_codec(
            provider,
            config,
            None::<
                fn(
                    &'p P,
                    u64,
                    u32,
                )
                    -> Result<Box<dyn ramshared_vram::device_codec::GpuDeviceCodec + 'p>, String>,
            >,
        )
    }

    pub fn active_chunks_count(&self) -> usize {
        self.chunks.len()
    }

    pub fn is_disabled(&self) -> bool {
        self.disabled
    }

    pub fn handle_read(&mut self, offset: u64, len: usize) -> Option<Vec<u8>> {
        if self.disabled || self.config.chunk_bytes == 0 || len == 0 {
            return None;
        }
        // Read ceiling: refuse **before** any response allocation (DT-4).
        // `MAX_READ_LEN` is the private-response bound; a larger request would
        // allocate first and only then discover it cannot be served.
        if len > MAX_READ_LEN {
            return None;
        }
        if let Some(hit) = self.read_device_pages(offset, len) {
            return Some(hit);
        }
        if self.disabled {
            return None;
        }
        // Compressed extents are tried first: the codec is an accelerator on
        // top of the raw cache, so a miss here falls through to the raw path.
        if self.config.compression_enabled
            && !self.compressed.entries.is_empty()
            && let Some(hit) = self.read_compressed(offset, len)
        {
            return Some(hit);
        }
        // Spans chunk boundaries by the worker's own geometry (AUD-33). Every
        // overlapped chunk must contain its sub-range, otherwise the whole
        // read is a miss: a partial answer would expose a hole as bytes.
        let chunk_bytes = self.config.chunk_bytes as u64;
        let end = offset.saturating_add(len as u64);
        let mut buf = vec![0u8; len];
        let mut cursor = offset;
        while cursor < end {
            let chunk_base = (cursor / chunk_bytes).saturating_mul(chunk_bytes);
            let chunk_off = cursor.saturating_sub(chunk_base);
            let take = chunk_bytes
                .saturating_sub(chunk_off)
                .min(end.saturating_sub(cursor));
            let rel = cursor.saturating_sub(offset) as usize;
            let chunk = self.chunks.get_mut(&chunk_base)?;
            if !chunk.contains(chunk_off, chunk_off + take) {
                return None;
            }
            chunk.last_accessed = Instant::now();
            if chunk
                .mem
                .read_at(chunk_off, &mut buf[rel..rel + take as usize])
                .is_err()
            {
                return None;
            }
            cursor = cursor.saturating_add(take);
        }
        Some(buf)
    }

    pub fn handle_update(&mut self, offset: u64, data: &[u8]) {
        if self.disabled || self.config.chunk_bytes == 0 || data.is_empty() {
            return;
        }
        // DT-6: invalidate overlapping extents **before** the new bytes are
        // published, so a reader can never observe a stale extent alongside
        // the replacement.
        if self.config.compression_enabled {
            self.handle_invalidate(offset, data.len() as u64);
            if self.write_device_pages(offset, data)
                || self.disabled
                || (self.device_codec_reservation == 0 && self.try_compressed_update(offset, data))
            {
                return;
            }
        }
        self.publish_raw_span(offset, data);
    }

    /// Publishes `data` at `offset` into the raw chunk map, splitting on the
    /// worker's actual chunk boundaries (AUD-33).
    ///
    /// Chunk geometry is authoritative here; a caller-side payload cap cannot
    /// substitute for it. Every overlapped chunk receives exactly the
    /// sub-slice that falls inside it, so a write spanning a boundary updates
    /// both sides instead of being dropped. A sub-range that cannot be
    /// published is punched out of raw coverage — a skipped update never
    /// preserves stale validity.
    fn publish_raw_span(&mut self, offset: u64, data: &[u8]) {
        let chunk_bytes = self.config.chunk_bytes as u64;
        if chunk_bytes == 0 {
            return;
        }
        let end = offset.saturating_add(data.len() as u64);
        let mut cursor = offset;
        while cursor < end {
            let chunk_base = (cursor / chunk_bytes).saturating_mul(chunk_bytes);
            let chunk_off = cursor.saturating_sub(chunk_base);
            let take = chunk_bytes
                .saturating_sub(chunk_off)
                .min(end.saturating_sub(cursor));
            let rel = cursor.saturating_sub(offset) as usize;
            self.publish_chunk_slice(chunk_base, chunk_off, &data[rel..rel + take as usize]);
            cursor = cursor.saturating_add(take);
        }
    }

    /// Writes one in-chunk sub-slice, dropping the chunk when the bytes cannot
    /// be replaced (AUD-33).
    ///
    /// The failure path removes the whole chunk rather than punching only the
    /// sub-range: `write_at` failing means the backing store is not trustworthy
    /// for the extent it was asked to change, and a partial drop would leave
    /// pre-write bytes readable next to a range the caller believes is new.
    fn publish_chunk_slice(&mut self, chunk_base: u64, chunk_off: u64, data: &[u8]) {
        let sub_end = chunk_off.saturating_add(data.len() as u64);
        if let Some(chunk) = self.chunks.get_mut(&chunk_base) {
            if chunk.mem.write_at(chunk_off, data).is_err() {
                self.chunks.remove(&chunk_base);
                return;
            }
            chunk.mark_valid(chunk_off, sub_end);
            chunk.last_accessed = Instant::now();
            return;
        }
        self.allocate_and_write(chunk_base, chunk_off, data);
    }

    /// Drops every cached entry overlapping `[offset, offset+len)` (DT-11).
    ///
    /// The parent emits `MSG_INVALIDATE` when a write-mirror could not be
    /// delivered: the bytes still held for that range are pre-write, so a
    /// later Hit would serve them. One-way — no response is produced. A `len`
    /// of zero touches nothing.
    pub fn handle_invalidate(&mut self, offset: u64, len: u64) {
        if self.disabled || len == 0 {
            return;
        }
        let end = offset.saturating_add(len);
        self.invalidate_device_pages(offset, end);
        // DT-6/DT-11: drop the compressed index first so no extent can cover
        // the hole, then punch the raw chunks' valid ranges.
        self.invalidate_compressed_range(offset, end);
        let chunk_bytes = self.config.chunk_bytes as u64;
        if chunk_bytes == 0 {
            return;
        }
        let first = (offset / chunk_bytes).saturating_mul(chunk_bytes);
        let last = (end.saturating_sub(1) / chunk_bytes).saturating_mul(chunk_bytes);
        let mut chunk_base = first;
        loop {
            if let Some(chunk) = self.chunks.get_mut(&chunk_base) {
                chunk.invalidate_range(
                    offset.saturating_sub(chunk_base),
                    end.saturating_sub(chunk_base).min(chunk_bytes),
                );
                if chunk.valid_ranges.is_empty() {
                    // Nothing readable remains: give the VRAM back.
                    self.chunks.remove(&chunk_base);
                }
            }
            if chunk_base >= last {
                break;
            }
            chunk_base = chunk_base.saturating_add(chunk_bytes);
        }
    }

    pub fn handle_promote(&mut self, offset: u64, data: &[u8]) {
        self.handle_update(offset, data);
    }

    pub fn handle_disable(&mut self) {
        self.disabled = true;
        self.chunks.clear();
        self.invalidate_device_pages(0, u64::MAX);
        // Drop the codec before acknowledging revocation. A poisoned driver
        // implementation may retain its allocations until worker exit; the
        // caller must still observe process settlement before claiming release.
        drop(self.slab_bridge.take());
        drop(self.host_metadata_guard.take());
        self.disable_compressed();
    }

    /// Give clean cache chunks back when other GPU users consume the free buffer.
    /// The durable origin remains authoritative for every evicted range.
    pub fn reclaim_under_host_pressure(&mut self) -> Result<u64, VramError> {
        let mut released = 0u64;
        loop {
            let budget = self.provider.budget_snapshot()?;
            let free = if budget.can_admit(0) {
                budget.available_bytes()
            } else {
                0
            };
            let required_free = budget
                .required_free_bytes(self.config.reserve_floor_bytes, RUNTIME_FREE_BUFFER_BYTES);
            let recovery_free = required_free.max(RUNTIME_RECOVERY_BUFFER_BYTES);
            if free >= recovery_free {
                self.pressure_constrained = false;
            }
            if free >= required_free {
                break;
            }
            self.pressure_constrained = true;
            if !self.chunks.is_empty() && self.evict_coldest_chunk() {
                released = released.saturating_add(self.config.chunk_bytes as u64);
                continue;
            }
            // Device codec buffers are process-scoped. Separate admission
            // (`required_free`) from hard abort (`hard_abort_floor`, invariant check):
            // only breaching the hard sealed reserve returns `Busy`.
            if self.device_codec_reservation != 0 && self.slab_bridge.is_some() {
                let hard_abort_floor =
                    budget.required_free_bytes(self.config.reserve_floor_bytes, 0);
                if free < hard_abort_floor {
                    return Err(VramError::Busy);
                }
                break;
            }
            if !self.evict_coldest_chunk() {
                break;
            }
            released = released.saturating_add(self.config.chunk_bytes as u64);
        }
        Ok(released)
    }
}

/// Reads until `buffer` is full against one absolute `deadline`.
///
/// Mirrors the client-side `read_exact_until` discipline: the deadline is
/// absolute across every partial read of the frame, so a peer that trickles
/// bytes cannot extend it. Returns `Ok(false)` only when the peer closed
/// cleanly before delivering any byte (the loop's only clean-exit path);
/// a truncated frame, a stall, or deadline expiry fails closed.
fn read_exact_until(
    socket: &mut UnixStream,
    mut buffer: &mut [u8],
    deadline: Instant,
) -> Result<bool, String> {
    let mut read_any = false;
    while !buffer.is_empty() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("worker frame read deadline expired".to_string());
        }
        socket
            .set_read_timeout(Some(remaining))
            .map_err(|error| format!("worker read timeout setup error: {error}"))?;
        match socket.read(buffer) {
            Ok(0) if !read_any => return Ok(false),
            Ok(0) => return Err("worker peer closed mid-frame".to_string()),
            Ok(read) => {
                read_any = true;
                buffer = &mut buffer[read..];
            }
            Err(ref error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(ref error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                return Err("worker frame read deadline expired".to_string());
            }
            Err(error) => return Err(format!("worker read error: {error}")),
        }
    }
    Ok(true)
}

pub fn run_gpu_worker_loop<P: VramProvider>(
    socket: UnixStream,
    provider: P,
    config: GpuWorkerConfig,
) -> Result<(), String> {
    run_gpu_worker_loop_with_frame_read_timeout(socket, provider, config, WORKER_FRAME_READ_TIMEOUT)
}

/// Runs the worker loop with an injectable per-frame read budget.
///
/// `frame_read_timeout` bounds one complete frame read (header plus payload)
/// so a stalled peer cannot block the worker until process teardown.
pub fn run_gpu_worker_loop_with_frame_read_timeout<P: VramProvider>(
    socket: UnixStream,
    provider: P,
    config: GpuWorkerConfig,
    frame_read_timeout: Duration,
) -> Result<(), String> {
    let worker = GpuCacheWorker::new(&provider, config);
    run_gpu_worker_loop_with_worker(socket, worker, frame_read_timeout)
}

pub(super) fn run_gpu_worker_loop_with_worker<'p, P: VramProvider + 'p>(
    mut socket: UnixStream,
    mut worker: GpuCacheWorker<'p, P>,
    frame_read_timeout: Duration,
) -> Result<(), String> {
    let mut hdr_buf = [0u8; FRAME_HEADER_LEN];
    let mut peer_understands_unknown = false;

    loop {
        let frame_deadline = Instant::now()
            .checked_add(frame_read_timeout)
            .ok_or_else(|| "worker frame read deadline overflow".to_string())?;
        if !read_exact_until(&mut socket, &mut hdr_buf, frame_deadline)? {
            break;
        }

        let hdr = FrameHeader::decode(&hdr_buf)
            .ok_or_else(|| "worker received nonzero reserved header bytes".to_string())?;
        if hdr.msg_type != MSG_HANDSHAKE_REQ
            && !peer_understands_unknown
            && worker.observed_cached_bytes().is_none()
        {
            return Err("worker requires unknown-occupancy capability handshake".into());
        }

        let payload = if hdr.payload_len > 0 {
            if hdr.payload_len as usize > MAX_IPC_PAYLOAD_BYTES {
                return Err("worker payload len exceeds limit".to_string());
            }
            let mut buf = vec![0u8; hdr.payload_len as usize];
            if !read_exact_until(&mut socket, &mut buf, frame_deadline)? {
                return Err("worker peer closed mid-frame".to_string());
            }
            buf
        } else {
            Vec::new()
        };

        match hdr.msg_type {
            MSG_HANDSHAKE_REQ => {
                peer_understands_unknown = hdr.aux == CACHED_KIB_UNKNOWN_CAPABILITY;
                let occupancy = worker.observed_cached_bytes();
                let incompatible = occupancy.is_none() && !peer_understands_unknown;
                let resp = FrameHeader {
                    msg_type: MSG_HANDSHAKE_RESP,
                    status: if incompatible {
                        STATUS_ERROR
                    } else {
                        STATUS_OK
                    },
                    correlation_id: hdr.correlation_id,
                    offset: worker.target_bytes(),
                    payload_len: 0,
                    aux: if incompatible {
                        0
                    } else {
                        cached_kib_observation(occupancy)
                    },
                };
                if let Err(e) = socket.write_all(&resp.encode()) {
                    return Err(format!("worker write handshake resp error: {e}"));
                }
                if incompatible {
                    break;
                }
            }
            MSG_READ_REQ if hdr.aux as usize > MAX_IPC_PAYLOAD_BYTES => {
                return Err("worker read length exceeds limit".to_string());
            }
            MSG_READ_REQ => match worker.handle_read(hdr.offset, hdr.aux as usize) {
                Some(data) => {
                    let resp = FrameHeader {
                        msg_type: MSG_READ_RESP,
                        status: STATUS_OK,
                        correlation_id: hdr.correlation_id,
                        offset: hdr.offset,
                        payload_len: data.len() as u32,
                        aux: cached_kib_observation(worker.observed_cached_bytes()),
                    };
                    if let Err(e) = socket.write_all(&resp.encode()) {
                        return Err(format!("worker write read resp error: {e}"));
                    }
                    if let Err(e) = socket.write_all(&data) {
                        return Err(format!("worker write read data error: {e}"));
                    }
                }
                None => {
                    let resp = FrameHeader {
                        msg_type: MSG_READ_RESP,
                        status: STATUS_MISS,
                        correlation_id: hdr.correlation_id,
                        offset: hdr.offset,
                        payload_len: 0,
                        aux: cached_kib_observation(worker.observed_cached_bytes()),
                    };
                    if let Err(e) = socket.write_all(&resp.encode()) {
                        return Err(format!("worker write read resp error: {e}"));
                    }
                }
            },
            MSG_UPDATE => {
                worker.handle_update(hdr.offset, &payload);
            }
            MSG_PROMOTE => {
                worker.handle_promote(hdr.offset, &payload);
            }
            // DT-11: one-way, no response. Drop every cached entry
            // overlapping `[offset, offset+aux)`.
            MSG_INVALIDATE_REQ => {
                worker.handle_invalidate(hdr.offset, u64::from(hdr.aux));
            }
            MSG_DISABLE_REQ => {
                worker.handle_disable();
                let resp = FrameHeader {
                    msg_type: MSG_DISABLE_RESP,
                    status: STATUS_OK,
                    correlation_id: hdr.correlation_id,
                    offset: 0,
                    payload_len: 0,
                    aux: 0,
                };
                let _ = socket.write_all(&resp.encode());
                break;
            }
            MSG_HEARTBEAT_REQ => {
                if let Err(error) = worker.reclaim_under_host_pressure() {
                    eprintln!("[ramsharedd] GPU cache pressure reclamation refused: {error:?}");
                    worker.handle_disable();
                }
                // DT-8: one versioned envelope carries the existing GPU budget
                // telemetry nested beside the cache occupancy sample. Physical
                // `target_bytes`/`cached_bytes` stay in the frame header and are
                // never re-derived from the envelope. A sample that does not fit
                // the ceiling is omitted, never truncated.
                let telemetry_payload = if worker.is_disabled() {
                    Vec::new()
                } else {
                    let now = unix_time_ms();
                    let budget = worker
                        .provider
                        .budget_snapshot()
                        .ok()
                        .map(|snapshot| GpuBudgetTelemetry::from_snapshot(&snapshot, now));
                    // Busy time is sampled on the same provider at the same
                    // heartbeat as the budget, so the two share one adapter
                    // identity and one timestamp. `None` is "not measured"
                    // and travels as absence, never as a zero percent.
                    let utilization = worker
                        .provider
                        .utilization_snapshot()
                        .map(|snapshot| GpuUtilizationTelemetry::from_snapshot(&snapshot, now));
                    let cache = worker.observed_cache_telemetry(now);
                    WorkerTelemetryEnvelope::new(now, budget, utilization, cache)
                        .to_bounded_payload()
                        .unwrap_or_default()
                };
                let resp = FrameHeader {
                    msg_type: MSG_HEARTBEAT_RESP,
                    status: if worker.is_disabled() {
                        STATUS_ERROR
                    } else {
                        STATUS_OK
                    },
                    correlation_id: hdr.correlation_id,
                    offset: worker.target_bytes(),
                    payload_len: telemetry_payload.len() as u32,
                    aux: cached_kib_observation(worker.observed_cached_bytes()),
                };
                if let Err(e) = socket.write_all(&resp.encode()) {
                    return Err(format!("worker write heartbeat error: {e}"));
                }
                if let Err(e) = socket.write_all(&telemetry_payload) {
                    return Err(format!("worker write heartbeat telemetry error: {e}"));
                }
                if worker.is_disabled() {
                    break;
                }
            }
            _ => {
                if !slab_ipc::handle_slab_frame(&mut worker, &hdr, &payload, &mut socket)? {
                    return Err(format!(
                        "worker received unsupported message type {}",
                        hdr.msg_type
                    ));
                }
            }
        }
        if worker.is_disabled() {
            break;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests;
