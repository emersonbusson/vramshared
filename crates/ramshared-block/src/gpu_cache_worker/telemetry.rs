// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/gpu_cache_worker/telemetry.rs
//! Cache occupancy and codec telemetry.
use super::*;
use crate::compressed_cache::CacheRepresentation;
use ramshared_vram::{CodecCapability, CodecState, CodecTelemetry, WorkerCacheTelemetry};

impl<'p, P: VramProvider + 'p> GpuCacheWorker<'p, P> {
    pub fn target_bytes(&self) -> u64 {
        self.effective_target_bytes
    }

    pub fn cached_bytes(&self) -> u64 {
        let slabs = self
            .compressed_slabs
            .iter()
            .fold(0u64, |total, slab| total.saturating_add(slab.len() as u64));
        let workspace = self
            .codec_workspace
            .as_ref()
            .map_or(0, |memory| memory.len() as u64);
        let device = self
            .slab_bridge
            .as_ref()
            .and_then(|bridge| bridge.device_allocation_bytes())
            .map_or(0, |(slab, workspace)| slab.saturating_add(workspace));
        self.raw_cached_bytes()
            .saturating_add(slabs)
            .saturating_add(workspace)
            .saturating_add(device)
    }

    pub(super) fn raw_cached_bytes(&self) -> u64 {
        (self.chunks.len() as u64).saturating_mul(self.config.chunk_bytes as u64)
    }

    pub fn observed_cached_bytes(&self) -> Option<u64> {
        if self.device_codec_reservation != 0
            && self
                .slab_bridge
                .as_ref()?
                .device_allocation_bytes()
                .is_none()
        {
            return None;
        }
        Some(self.cached_bytes())
    }

    /// Omit the sample when a resident device codec cannot account for its
    /// allocations; zero would misrepresent unknown physical occupancy.
    pub fn observed_cache_telemetry(
        &self,
        sampled_at_unix_ms: u64,
    ) -> Option<WorkerCacheTelemetry> {
        if self.device_codec_reservation != 0
            && self
                .slab_bridge
                .as_ref()?
                .device_allocation_bytes()
                .is_none()
        {
            return None;
        }
        Some(self.cache_telemetry(sampled_at_unix_ms))
    }
    /// Cache occupancy and codec health for one heartbeat sample (DT-8).
    ///
    /// Physical `cached_bytes` / `target_bytes` stay in the frame header; this
    /// sample never re-labels cache occupancy as guest or host RAM.
    pub fn cache_telemetry(&self, sampled_at_unix_ms: u64) -> WorkerCacheTelemetry {
        let mut logical_cached_bytes = 0u64;
        let mut compressed_payload_bytes = 0u64;
        let mut raw_payload_bytes = 0u64;
        for entry in &self.compressed.entries {
            logical_cached_bytes = logical_cached_bytes.saturating_add(entry.logical_len);
            match &entry.representation {
                CacheRepresentation::Raw { address } => {
                    raw_payload_bytes =
                        raw_payload_bytes.saturating_add(address.span.stored_len as u64);
                }
                CacheRepresentation::Compressed { address, .. } => {
                    compressed_payload_bytes =
                        compressed_payload_bytes.saturating_add(address.span.stored_len as u64);
                }
            }
        }
        // Device tickets are counted only while the pool still accepts their
        // epoch; an evicted frame is not a cache hit or valid telemetry.
        if self.device_codec_reservation != 0
            && let Some(bridge) = &self.slab_bridge
        {
            for ticket in self
                .slab_pages
                .values()
                .filter(|ticket| bridge.pool().lookup(ticket).is_ok())
            {
                logical_cached_bytes = logical_cached_bytes
                    .saturating_add(ramshared_vram::device_codec::PAGE_BYTES as u64);
                match ticket.encoding {
                    ramshared_vram::slab_pool::SlotEncoding::Lz4 => {
                        compressed_payload_bytes =
                            compressed_payload_bytes.saturating_add(u64::from(ticket.payload_len))
                    }
                    ramshared_vram::slab_pool::SlotEncoding::Raw => {
                        raw_payload_bytes =
                            raw_payload_bytes.saturating_add(u64::from(ticket.payload_len))
                    }
                }
            }
        }
        // Bytes living in the raw chunk map bypassed the compressed store.
        let mut raw_bypass_bytes = 0u64;
        for chunk in self.chunks.values() {
            for (start, end) in &chunk.valid_ranges {
                let len = end.saturating_sub(*start);
                raw_bypass_bytes = raw_bypass_bytes.saturating_add(len);
            }
        }
        logical_cached_bytes = logical_cached_bytes.saturating_add(raw_bypass_bytes);

        let device_allocations = self
            .slab_bridge
            .as_ref()
            .and_then(|bridge| bridge.device_allocation_bytes());
        let physical_cache_slab_bytes = self
            .compressed_slabs
            .iter()
            .map(|slab| slab.len() as u64)
            .fold(0u64, u64::saturating_add)
            .saturating_add(device_allocations.map_or(0, |(slab, _)| slab));
        let codec_workspace_bytes = self
            .codec_workspace
            .as_ref()
            .map(|mem| mem.len() as u64)
            .unwrap_or(0)
            .saturating_add(device_allocations.map_or(0, |(_, workspace)| workspace));
        let metadata_bytes = (self.device_codec_reservation == 0)
            .then_some(self.compressed.allocator.metadata_bytes() as u64);
        let device_telemetry = self.slab_bridge.as_ref().map(|bridge| bridge.telemetry());
        let integrity_errors = self.codec_integrity_errors().saturating_add(
            device_telemetry
                .as_ref()
                .and_then(|telemetry| telemetry.predecode_crc_mismatches)
                .unwrap_or(0),
        );
        let decode_errors = self.codec_decode_errors().saturating_add(
            device_telemetry
                .as_ref()
                .and_then(|telemetry| telemetry.corrupt_lz4_fallbacks)
                .unwrap_or(0),
        );

        let capability = if self.provider.cache_codec().is_some()
            || (self.device_codec_reservation != 0 && self.slab_bridge.is_some())
        {
            CodecCapability::Available
        } else {
            CodecCapability::RawOnly
        };
        let state = if self.disabled {
            CodecState::Disabled
        } else if capability == CodecCapability::RawOnly || !self.config.compression_enabled {
            CodecState::RawOnly
        } else if self.codec_timeouts() > 0 {
            CodecState::TimedOut
        } else if self.codec_faults() > 0 || integrity_errors > 0 || decode_errors > 0 {
            CodecState::Faulted
        } else {
            CodecState::Ready
        };
        let refusal_reason = match (capability, state) {
            (CodecCapability::RawOnly, _) => Some("no-provider-codec"),
            (_, CodecState::RawOnly) => Some("compression-disabled"),
            (_, CodecState::Disabled) => Some("worker-disabled"),
            (_, CodecState::TimedOut) => Some("codec-subdeadline"),
            (_, CodecState::Faulted) => Some("codec-fault"),
            (_, CodecState::Ready) => None,
        };

        WorkerCacheTelemetry {
            schema_version: ramshared_vram::WORKER_TELEMETRY_SCHEMA_VERSION,
            sampled_at_unix_ms,
            codec: CodecTelemetry::new(capability, state, refusal_reason),
            gpu_codec_activity: self
                .slab_bridge
                .as_ref()
                .and_then(GpuWorkerSlabBridge::gpu_codec_activity),
            logical_cached_bytes,
            physical_cache_slab_bytes,
            codec_workspace_bytes,
            compressed_payload_bytes,
            raw_payload_bytes,
            metadata_bytes,
            device_metadata_lower_bound_bytes: (self.device_codec_reservation != 0)
                .then(|| {
                    self.slab_bridge.as_ref().map(|bridge| {
                        super::host_budget::live_worker_metadata_bytes(
                            bridge.pool(),
                            self.slab_pages.len(),
                            self.slab_frame_pages.len(),
                        )
                    })
                })
                .flatten(),
            raw_bypass_bytes,
            codec_integrity_errors: integrity_errors,
            codec_decode_errors: decode_errors,
            codec_timeouts: self.codec_timeouts(),
        }
    }
}
