// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/gpu_cache_worker/compressed_codec.rs
//! Compressed extent store, VRAM slab backing, and hardware codec path
//! for [`super::GpuCacheWorker`].

use std::time::{Duration, Instant};

use ramshared_vram::device_codec::{CodecError, PAGE_BYTES, PageDecodeStatus};
use ramshared_vram::slab_pool::VramSlabTicket;
use ramshared_vram::{
    CodecId, CodecStatus, GpuBudgetSnapshot, GpuCacheCodec, VramMemory, VramOutputReservation,
    VramProvider, VramSpan, crc32,
};

use crate::compressed_cache::{
    CacheEntry, CacheRepresentation, MAX_EXTENT_LEN, ReadCoverage, SlabSpan, VramSpanAllocator,
    invalidate_overlaps, read_coverage, split_extent,
};
use crate::gpu_worker_slab_bridge::GpuWorkerSlabBridge;

use super::{GpuCacheWorker, GpuWorkerConfig, RUNTIME_FREE_BUFFER_BYTES};

/// Codec-operation sub-deadline inside the 50 ms cache-read budget (DT-3).
///
/// Bounds **admission and inter-step continuation only**. It cannot preempt a
/// blocked driver call: a provider call that never returns is already outside
/// what this worker can bound, and claiming otherwise would be a false
/// guarantee. A codec step that exceeds its slice reports `TimedOut` and the
/// raw path or a miss serves instead (DT-11).
pub const CODEC_SUBDEADLINE: Duration = Duration::from_millis(20);
/// Overall cache-read budget the codec sub-deadline sits inside (DT-3).
pub const CACHE_READ_BUDGET: Duration = Duration::from_millis(50);

fn codec_error_text(error: &CodecError) -> String {
    match error {
        CodecError::BackendFailure(detail) => detail.clone(),
        other => format!("{other:?}"),
    }
}

/// Extent index for the optional compressed representation (DT-4..DT-7).
///
/// Lives alongside the raw chunk map: when compression is disabled or
/// unsupported this store stays empty and the raw path is the whole product
/// (DT-9). Codec faults drop compressed entries only and never touch the raw
/// chunks (DT-11).
pub(super) struct CompressedStore {
    pub(super) entries: Vec<CacheEntry>,
    pub(super) allocator: VramSpanAllocator,
    generation: u64,
    /// Codec faults observed. Reported for triage; never a disable signal.
    codec_faults: u64,
    /// Checksum refusals before decode (DT-7).
    codec_integrity_errors: u64,
    /// Decoder failures: bad status, wrong length, or output checksum mismatch.
    codec_decode_errors: u64,
    /// Codec sub-deadline exceedances (DT-3).
    codec_timeouts: u64,
    /// Completed codec operations. A non-zero count with an in-flight
    /// operation is what `worker_teardown_waits_for_codec_completion` asserts
    /// against before the store is dropped.
    completed_ops: u64,
    /// Set while one codec operation is in flight.
    in_flight: bool,
}

impl CompressedStore {
    pub(super) fn new(physical_target_bytes: u64) -> Self {
        Self {
            entries: Vec::new(),
            allocator: VramSpanAllocator::new(physical_target_bytes),
            generation: 0,
            codec_faults: 0,
            codec_integrity_errors: 0,
            codec_decode_errors: 0,
            codec_timeouts: 0,
            completed_ops: 0,
            in_flight: false,
        }
    }

    fn next_generation(&mut self) -> u64 {
        self.generation = self.generation.saturating_add(1);
        self.generation
    }

    /// DT-11: record a codec-only fault. The raw cache keeps serving.
    ///
    /// The typed counters feed the telemetry envelope (DT-8); `codec_faults`
    /// stays the aggregate so existing triage callers keep working.
    fn note_codec_fault(&mut self) {
        self.codec_faults = self.codec_faults.saturating_add(1);
        self.in_flight = false;
    }

    /// DT-7: a checksum refused an entry before decode.
    fn note_integrity_error(&mut self) {
        self.codec_integrity_errors = self.codec_integrity_errors.saturating_add(1);
        self.note_codec_fault();
    }

    /// A decode/encode step failed without being a checksum or timeout.
    fn note_decode_error(&mut self) {
        self.codec_decode_errors = self.codec_decode_errors.saturating_add(1);
        self.note_codec_fault();
    }

    /// DT-3: the codec sub-deadline fired.
    pub(super) fn note_codec_timeout(&mut self) {
        self.codec_timeouts = self.codec_timeouts.saturating_add(1);
        self.note_codec_fault();
    }

    fn note_codec_completion(&mut self) {
        self.completed_ops = self.completed_ops.saturating_add(1);
        self.in_flight = false;
    }
}

impl<'p, P: VramProvider + 'p> GpuCacheWorker<'p, P> {
    pub(super) fn new_with_device_codec<F>(
        provider: &'p P,
        config: GpuWorkerConfig,
        make_codec: Option<F>,
    ) -> Self
    where
        F: FnOnce(
            &'p P,
            u64,
            u32,
        )
            -> Result<Box<dyn ramshared_vram::device_codec::GpuDeviceCodec + 'p>, String>,
    {
        Self::new_with_device_codec_and_host_budget(provider, config, 1, None, None, make_codec)
    }

    pub fn new_with_device_codec_and_host_budget<F>(
        provider: &'p P,
        config: GpuWorkerConfig,
        worker_count: u32,
        shared_budget: Option<super::host_budget::SharedHostMetadataBudget>,
        host_observation: Option<super::host_budget::HostMemoryObservation>,
        make_codec: Option<F>,
    ) -> Self
    where
        F: FnOnce(
            &'p P,
            u64,
            u32,
        )
            -> Result<Box<dyn ramshared_vram::device_codec::GpuDeviceCodec + 'p>, String>,
    {
        let effective_target = match provider.budget_snapshot() {
            Ok(budget) => super::effective_target_from_budget(&budget, config),
            Err(_) => 0,
        };
        let mut config = config;
        if effective_target >= 4096 && effective_target < config.chunk_bytes as u64 {
            let power = 1u64 << (63 - effective_target.leading_zeros());
            if let Ok(chunk) = usize::try_from(power) {
                config.chunk_bytes = chunk;
            }
        }
        let has_device_factory = make_codec.is_some();
        let (mut slab_bridge, mut host_metadata_guard) = if config.compression_enabled {
            match super::host_budget::admit_slab_bridge_with_codec(
                provider,
                effective_target,
                config.chunk_bytes as u64,
                worker_count,
                shared_budget,
                host_observation,
                make_codec,
            ) {
                Some((bridge, guard)) => (Some(bridge), Some(guard)),
                None => (None, None),
            }
        } else {
            (None, None)
        };
        if has_device_factory
            && slab_bridge
                .as_ref()
                .and_then(GpuWorkerSlabBridge::device_allocation_bytes)
                .is_some_and(|(pool, workspace)| {
                    pool.checked_add(workspace)
                        .is_none_or(|total| total > effective_target)
                })
        {
            drop(slab_bridge.take());
            drop(host_metadata_guard.take());
        }
        let device_codec_reservation = if has_device_factory && slab_bridge.is_some() {
            slab_bridge
                .as_ref()
                .and_then(GpuWorkerSlabBridge::device_allocation_bytes)
                .and_then(|(pool, workspace)| pool.checked_add(workspace))
                .filter(|measured| *measured > 0 && *measured <= effective_target)
                .unwrap_or(effective_target)
        } else {
            drop(host_metadata_guard.take());
            0
        };
        Self {
            provider,
            config,
            effective_target_bytes: effective_target,
            chunks: std::collections::HashMap::new(),
            disabled: false,
            pressure_constrained: false,
            compressed: CompressedStore::new(effective_target),
            compressed_slabs: Vec::new(),
            codec_workspace: None,
            slab_bridge,
            slab_pages: std::collections::BTreeMap::new(),
            slab_frame_pages: std::collections::BTreeSet::new(),
            host_metadata_guard,
            device_codec_reservation,
        }
    }

    /// The admitted page-slab bridge, if compression and the budget allow it.
    pub fn slab_bridge(&self) -> Option<&GpuWorkerSlabBridge<'p>> {
        self.slab_bridge.as_ref()
    }

    /// Compress pages into the admitted slab pool. The caller owns the tickets
    /// and must return them through [`Self::recover_pages_from_slabs`].
    pub fn compress_pages_into_slabs(
        &mut self,
        pages: &[[u8; PAGE_BYTES]],
        out_tickets: &mut [Option<VramSlabTicket>],
    ) -> Result<(), String> {
        let (result, frames) = {
            let bridge = self
                .slab_bridge
                .as_mut()
                .ok_or_else(|| "compressed slab bridge is not admitted".to_string())?;
            bridge.begin_eviction_observation();
            let result = bridge.compress_batch(pages, out_tickets);
            (result, bridge.finish_eviction_observation())
        };
        self.prune_device_frames(&frames);
        let bridge = self
            .slab_bridge
            .as_mut()
            .ok_or_else(|| "compressed slab bridge is not admitted".to_string())?;
        if let Err(error) = result {
            for ticket in out_tickets.iter_mut().take(pages.len()) {
                if let Some(ticket) = ticket.take() {
                    bridge.invalidate_ticket(&ticket);
                }
            }
            if matches!(
                error,
                ramshared_vram::device_codec::CodecError::DeadlineExceeded
            ) {
                self.compressed.note_codec_timeout();
                self.disabled = true;
            }
            return Err(codec_error_text(&error));
        }
        if out_tickets.len() < pages.len()
            || out_tickets[..pages.len()].iter().any(|ticket| {
                ticket
                    .as_ref()
                    .is_none_or(|ticket| bridge.pool().lookup(ticket).is_err())
            })
        {
            for ticket in out_tickets.iter_mut().take(pages.len()) {
                if let Some(ticket) = ticket.take() {
                    bridge.invalidate_ticket(&ticket);
                }
            }
            return Err("compressed batch contains unavailable tickets".to_string());
        }
        Ok(())
    }

    /// Recover pages from the admitted slab pool. A CRC/LZ4 failure is
    /// replaced from `read_origin` and the failed ticket is invalidated.
    pub fn recover_pages_from_slabs<F>(
        &mut self,
        tickets: &[VramSlabTicket],
        out_pages: &mut [[u8; PAGE_BYTES]],
        out_status: &mut [PageDecodeStatus],
        read_origin: F,
    ) -> Result<(), String>
    where
        F: FnMut(usize) -> Result<[u8; PAGE_BYTES], String>,
    {
        let result = self
            .slab_bridge
            .as_mut()
            .ok_or_else(|| "compressed slab bridge is not admitted".to_string())?
            .recover_batch(tickets, out_pages, out_status, read_origin);
        let frames: Vec<_> = tickets.iter().map(|ticket| ticket.frame_idx).collect();
        self.prune_device_frames(&frames);
        if matches!(
            result,
            Err(ramshared_vram::device_codec::CodecError::DeadlineExceeded)
        ) {
            self.compressed.note_codec_timeout();
            self.disabled = true;
        }
        result.map_err(|error| codec_error_text(&error))
    }

    /// Codec faults seen on this worker (DT-11 triage counter).
    pub fn codec_faults(&self) -> u64 {
        self.compressed.codec_faults
    }

    /// Checksum refusals before decode (DT-7).
    pub fn codec_integrity_errors(&self) -> u64 {
        self.compressed.codec_integrity_errors
    }

    /// Decoder failures: bad status, wrong length, or output checksum mismatch.
    pub fn codec_decode_errors(&self) -> u64 {
        self.compressed.codec_decode_errors
    }

    /// Codec sub-deadline exceedances (DT-3).
    pub fn codec_timeouts(&self) -> u64 {
        self.compressed.codec_timeouts
    }

    /// Codec operations that ran to completion (teardown proof).
    pub fn completed_codec_ops(&self) -> u64 {
        self.compressed.completed_ops
    }

    /// Whether a codec operation is currently in flight.
    pub fn codec_in_flight(&self) -> bool {
        self.compressed.in_flight
    }

    /// Live compressed extents (0 when compression is off or raw-only).
    pub fn compressed_entries_count(&self) -> usize {
        self.compressed.entries.len()
    }

    /// Provider slabs backing the compressed store.
    pub fn compressed_slab_count(&self) -> usize {
        self.compressed_slabs.len()
    }

    /// Codec/slab admission free floor (DT-4 / NFR-2).
    ///
    /// The **full** parent free floor: `required_free_bytes(configured_reserve,
    /// runtime_headroom)`. Using the configured `reserve_floor_bytes` alone is
    /// forbidden — it would let a slab allocation consume the runtime buffer
    /// and the `ceil(capacity/5)` safety share.
    pub fn codec_admission_required_free(&self, budget: &GpuBudgetSnapshot) -> u64 {
        budget.required_free_bytes(self.config.reserve_floor_bytes, RUNTIME_FREE_BUFFER_BYTES)
    }

    /// `true` when one more codec/slab allocation of `needed` bytes is admissible
    /// under the full free floor and a fresh, driver-reported budget (DT-4).
    pub(super) fn codec_admission_allows(&self, needed: u64) -> bool {
        self.provider.budget_snapshot().is_ok_and(|budget| {
            budget.can_admit(0)
                && budget.available_bytes()
                    >= needed.saturating_add(self.codec_admission_required_free(&budget))
        })
    }

    /// Drops any compressed extents overlapping `[offset, end)` and frees their slabs.
    pub(super) fn invalidate_compressed_range(&mut self, offset: u64, end: u64) {
        let removed = invalidate_overlaps(&mut self.compressed.entries, offset, end);
        for entry in removed {
            self.release_entry_storage(entry);
        }
    }

    /// Clears in-flight and resident compressed entries when disabling the worker.
    pub(super) fn disable_compressed(&mut self) {
        // DT-11 / teardown: every codec operation is either complete or has
        // already reported its fault; the store is dropped after that point.
        self.compressed.in_flight = false;
        self.compressed.entries.clear();
    }

    /// Serves one read from the compressed extent index, or `None` (DT-7).
    ///
    /// Every covering extent is checksum-verified **before** the decoder is
    /// invoked. A stored-checksum mismatch refuses the entry without decoding
    /// and returns `None` so the raw path or a miss serves (DT-11). The
    /// response is assembled into a private buffer only after all extents
    /// decode successfully (DT-6: never a partial assembly).
    pub(super) fn read_compressed(&mut self, offset: u64, len: usize) -> Option<Vec<u8>> {
        let end = offset.checked_add(len as u64)?;
        let coverage = read_coverage(&self.compressed.entries, offset, end);
        if !matches!(coverage, ReadCoverage::Exact | ReadCoverage::Contiguous) {
            return None;
        }
        let deadline = Instant::now().checked_add(CODEC_SUBDEADLINE)?;
        let mut assembled = vec![0u8; len];
        let mut filled = 0usize;
        let mut cursor = offset;
        while cursor < end {
            if Instant::now() >= deadline {
                self.compressed.note_codec_timeout();
                return None;
            }
            let index =
                self.compressed.entries.iter().position(|entry| {
                    entry.logical_start <= cursor && cursor < entry.logical_end()
                })?;
            let entry = self.compressed.entries[index].clone();
            self.compressed.entries[index].last_accessed = Instant::now();
            let within = (cursor - entry.logical_start) as usize;
            let take = ((entry.logical_end() - cursor) as usize).min(len - filled);
            // `decode_entry` records its own typed refusal; do not double-count.
            let chunk = self.decode_entry(&entry)?;
            // A provider call cannot be preempted here, but a result that
            // returns after the sub-deadline must not be reported as timely.
            if Instant::now() >= deadline {
                self.compressed.note_codec_timeout();
                return None;
            }
            if within + take > chunk.len() {
                self.compressed.note_decode_error();
                return None;
            }
            assembled[filled..filled + take].copy_from_slice(&chunk[within..within + take]);
            filled += take;
            cursor = cursor.saturating_add(take as u64);
        }
        if filled != len {
            return None;
        }
        if Instant::now() >= deadline {
            self.compressed.note_codec_timeout();
            return None;
        }
        self.compressed.note_codec_completion();
        Some(assembled)
    }

    /// Ensures the provider-owned codec scratch buffer can fit one call.
    pub(super) fn ensure_codec_workspace<'w>(
        provider: &'p P,
        codec_workspace: &'w mut Option<P::Mem<'p>>,
        codec: &dyn GpuCacheCodec<P::Mem<'p>>,
        reserve_floor: u64,
    ) -> Option<&'w mut P::Mem<'p>> {
        let workspace_needed = codec.workspace_bytes(1, MAX_EXTENT_LEN).unwrap_or(1).max(1);
        let workspace_ready = codec_workspace
            .as_ref()
            .is_some_and(|mem| mem.len() >= workspace_needed);
        if !workspace_ready {
            let budget = provider.budget_snapshot().ok()?;
            let floor = budget.required_free_bytes(reserve_floor, RUNTIME_FREE_BUFFER_BYTES);
            if !budget.can_admit(0)
                || budget.available_bytes() < floor.saturating_add(workspace_needed as u64)
            {
                return None;
            }
            *codec_workspace = provider.alloc(workspace_needed).ok();
        }
        codec_workspace.as_mut()
    }

    /// Decodes one entry into a private buffer after the stored checksum passes.
    ///
    /// Every refusal is typed here so the telemetry envelope (DT-8) can tell
    /// a checksum rejection from a decoder failure or a sub-deadline hit. The
    /// raw cache keeps serving either way (DT-11).
    fn decode_entry(&mut self, entry: &CacheEntry) -> Option<Vec<u8>> {
        match &entry.representation {
            CacheRepresentation::Raw { address } => {
                let Some(slab_len) = self.compressed.allocator.slab_len(address.slab_index) else {
                    self.compressed.note_decode_error();
                    return None;
                };
                if address.span.check_within(slab_len).is_err() {
                    self.compressed.note_decode_error();
                    return None;
                }
                let mut buf = vec![0u8; entry.logical_len as usize];
                let Some(slab) = self.compressed_slabs.get(address.slab_index) else {
                    self.compressed.note_decode_error();
                    return None;
                };
                let payload = address.span.stored_len.min(buf.len());
                if slab
                    .read_at(address.span.offset, &mut buf[..payload])
                    .is_err()
                {
                    self.compressed.note_decode_error();
                    return None;
                }
                if crc32(&buf, 0) != entry.original_crc32 {
                    self.compressed.note_integrity_error();
                    return None;
                }
                Some(buf)
            }
            CacheRepresentation::Compressed {
                address,
                original_crc32,
                stored_crc32,
                codec_id: _,
            } => {
                let Some(slab_len) = self.compressed.allocator.slab_len(address.slab_index) else {
                    self.compressed.note_decode_error();
                    return None;
                };
                if address.span.check_within(slab_len).is_err() {
                    self.compressed.note_decode_error();
                    return None;
                }
                let provider: &'p P = self.provider;
                let Some(codec) = provider.cache_codec() else {
                    self.compressed.note_decode_error();
                    return None;
                };
                // The provider-side checksum runs while the bytes are still in
                // VRAM. A mismatch refuses the entry **without** invoking the
                // decoder (DT-7).
                let computed = {
                    let workspace = Self::ensure_codec_workspace(
                        provider,
                        &mut self.codec_workspace,
                        codec,
                        self.config.reserve_floor_bytes,
                    );
                    let (Some(workspace), Some(slab)) =
                        (workspace, self.compressed_slabs.get(address.slab_index))
                    else {
                        self.compressed.note_decode_error();
                        return None;
                    };
                    match codec.checksum_batch(slab, &[address.span], workspace) {
                        Ok(sums) => sums.into_iter().next(),
                        Err(_) => {
                            self.compressed.note_integrity_error();
                            return None;
                        }
                    }
                };
                if computed.as_ref() != Some(stored_crc32) {
                    self.compressed.note_integrity_error();
                    return None;
                }
                let (statuses, bytes) = {
                    let (Some(workspace), Some(slab)) = (
                        self.codec_workspace.as_mut(),
                        self.compressed_slabs.get(address.slab_index),
                    ) else {
                        self.compressed.note_decode_error();
                        return None;
                    };
                    let mut out = vec![Vec::new()];
                    match codec.decompress_batch_from(
                        slab,
                        &[address.span],
                        &[entry.logical_len as usize],
                        &mut out,
                        workspace,
                    ) {
                        Ok(statuses) => (statuses, out.pop()),
                        Err(_) => {
                            self.compressed.note_decode_error();
                            return None;
                        }
                    }
                };
                if statuses.first() != Some(&CodecStatus::Ok) {
                    self.compressed.note_decode_error();
                    return None;
                }
                let Some(bytes) = bytes else {
                    self.compressed.note_decode_error();
                    return None;
                };
                if bytes.len() as u64 != entry.logical_len {
                    self.compressed.note_decode_error();
                    return None;
                }
                if crc32(&bytes, 0) != *original_crc32 {
                    self.compressed.note_integrity_error();
                    return None;
                }
                Some(bytes)
            }
        }
    }

    /// Attempts to publish `data` as compressed extents (DT-9, DT-10).
    ///
    /// Returns `true` when every extent was published compressed. Returns
    /// `false` when compression is unsupported, inadmissible, or not strictly
    /// smaller — the caller then takes the raw path. A codec fault here never
    /// disables the worker (DT-11).
    pub(super) fn try_compressed_update(&mut self, offset: u64, data: &[u8]) -> bool {
        let Some(codec) = self.provider.cache_codec() else {
            return false;
        };
        if data.len() > MAX_EXTENT_LEN {
            // Split oversized updates into per-extent publications so no
            // extent exceeds the DT-4 cap.
            let mut published_any = false;
            for (rel, len) in split_extent(0, data.len() as u64) {
                let slice = &data[rel as usize..(rel + len) as usize];
                if !self.publish_one_extent(offset.saturating_add(rel), slice, codec) {
                    return published_any;
                }
                published_any = true;
            }
            return published_any;
        }
        self.publish_one_extent(offset, data, codec)
    }

    /// Publishes one extent, compressed only when it is strictly smaller.
    fn publish_one_extent(
        &mut self,
        logical_start: u64,
        data: &[u8],
        codec: &dyn GpuCacheCodec<P::Mem<'p>>,
    ) -> bool {
        let logical_len = data.len() as u64;
        let original_crc = crc32(data, 0);
        let max_encoded = match codec.max_encoded_len(data.len()) {
            Ok(len) => len,
            Err(_) => {
                self.compressed.note_decode_error();
                return false;
            }
        };
        // Reserve at the worst case so a growing encode cannot overrun.
        let reservation_len = max_encoded.max(1);
        // Admission uses the **full** parent free floor, never the configured
        // reserve alone (DT-4 / NFR-2).
        if !self.codec_admission_allows(reservation_len as u64) {
            return false;
        }
        let address = match self.compressed.allocator.alloc(reservation_len) {
            Ok(address) => address,
            Err(_) => return false,
        };
        // Back a freshly created slab with provider memory.
        while self.compressed_slabs.len() <= address.slab_index {
            if !self.grow_slab_backing() {
                let _ = self.compressed.allocator.free(address);
                return false;
            }
        }
        let reservation =
            match VramOutputReservation::new(address.span.offset, reservation_len, reservation_len)
            {
                Ok(reservation) => reservation,
                Err(_) => {
                    let _ = self.compressed.allocator.free(address);
                    return false;
                }
            };
        self.compressed.in_flight = true;
        // Disjoint field borrows: the slab is shared, the workspace is unique.
        let results = {
            let workspace = Self::ensure_codec_workspace(
                self.provider,
                &mut self.codec_workspace,
                codec,
                self.config.reserve_floor_bytes,
            );
            let (Some(workspace), Some(slab)) =
                (workspace, self.compressed_slabs.get_mut(address.slab_index))
            else {
                self.compressed.note_decode_error();
                let _ = self.compressed.allocator.free(address);
                return false;
            };
            codec.compress_batch_into(&[data], slab, &[reservation], workspace)
        };
        let encoded = match results {
            Ok(results) if results.len() == 1 => results[0],
            _ => {
                self.compressed.note_decode_error();
                let _ = self.compressed.allocator.free(address);
                return false;
            }
        };
        if !encoded.status.is_ok() || encoded.encoded_len == 0 || encoded.encoded_len >= data.len()
        {
            // DT-10 usefulness gate: keep raw unless the encoded form is
            // strictly smaller. `Incompressible` is not a fault.
            self.compressed.note_codec_completion();
            let _ = self.compressed.allocator.free(address);
            return false;
        }
        let stored_span =
            match VramSpan::new(address.span.offset, encoded.encoded_len, reservation_len) {
                Ok(span) => span,
                Err(_) => {
                    self.compressed.note_decode_error();
                    let _ = self.compressed.allocator.free(address);
                    return false;
                }
            };
        // Provider-side checksum of the **stored** payload (DT-7).
        let stored_crc = {
            let workspace = Self::ensure_codec_workspace(
                self.provider,
                &mut self.codec_workspace,
                codec,
                self.config.reserve_floor_bytes,
            );
            let (Some(workspace), Some(slab)) =
                (workspace, self.compressed_slabs.get(address.slab_index))
            else {
                self.compressed.note_decode_error();
                let _ = self.compressed.allocator.free(address);
                return false;
            };
            match codec.checksum_batch(slab, &[stored_span], workspace) {
                Ok(sums) if sums.len() == 1 => sums[0],
                _ => {
                    self.compressed.note_integrity_error();
                    let _ = self.compressed.allocator.free(address);
                    return false;
                }
            }
        };
        let generation = self.compressed.next_generation();
        self.compressed.entries.push(CacheEntry {
            logical_start,
            logical_len,
            original_crc32: original_crc,
            generation,
            last_accessed: Instant::now(),
            representation: CacheRepresentation::Compressed {
                address: SlabSpan {
                    slab_index: address.slab_index,
                    span: stored_span,
                },
                original_crc32: original_crc,
                stored_crc32: stored_crc,
                codec_id: match codec.codec_id() {
                    CodecId::Raw => 0,
                    CodecId::Fake => 1,
                    CodecId::NvcompLz4 => 2,
                },
            },
        });
        self.compressed.note_codec_completion();
        true
    }

    /// Returns one entry's backing storage to the allocator.
    fn release_entry_storage(&mut self, entry: CacheEntry) {
        let address = match &entry.representation {
            CacheRepresentation::Raw { address } => *address,
            CacheRepresentation::Compressed { address, .. } => *address,
        };
        let _ = self.compressed.allocator.free(address);
    }

    /// Releases the coldest compressed extent (DT-5 / DT-6).
    ///
    /// Compressed extents hold VRAM too: under host pressure they must be
    /// reclaimable exactly like raw chunks. The released range becomes a Gap,
    /// so a later read misses and the durable origin serves — never a stale
    /// extent.
    pub(super) fn evict_coldest_compressed_extent(&mut self) -> bool {
        let Some(index) = self
            .compressed
            .entries
            .iter()
            .enumerate()
            .min_by_key(|(_, entry)| entry.last_accessed)
            .map(|(i, _)| i)
        else {
            return false;
        };
        let entry = self.compressed.entries.remove(index);
        self.release_entry_storage(entry);
        true
    }
}
