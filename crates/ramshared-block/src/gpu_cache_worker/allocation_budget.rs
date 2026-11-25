// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/gpu_cache_worker/allocation_budget.rs
//! Physical allocation admission for legacy compressed slabs.
use super::*;

impl<'p, P: VramProvider + 'p> GpuCacheWorker<'p, P> {
    /// Grows the slab backing by one provider allocation (DT-5).
    ///
    /// Called only after the span allocator has accepted a new slab, and only
    /// when the full free floor admits the allocation.
    pub(super) fn grow_slab_backing(&mut self) -> bool {
        let needed = crate::compressed_cache::SLAB_BYTES as u64;
        let slabs = self
            .compressed_slabs
            .iter()
            .fold(0u64, |sum, slab| sum.saturating_add(slab.len() as u64));
        let workspace = self
            .codec_workspace
            .as_ref()
            .map_or(0, |mem| mem.len() as u64);
        let committed = self
            .raw_cached_bytes()
            .saturating_add(slabs)
            .saturating_add(workspace)
            .saturating_add(self.device_codec_reservation);
        if committed.saturating_add(needed) > self.effective_target_bytes {
            return false;
        }
        if !self.codec_admission_allows(needed) {
            return false;
        }
        match self.provider.alloc(crate::compressed_cache::SLAB_BYTES) {
            Ok(mem) => {
                self.compressed_slabs.push(mem);
                true
            }
            Err(_) => false,
        }
    }
    pub(super) fn allocate_and_write(&mut self, chunk_base: u64, chunk_off: u64, data: &[u8]) {
        if self.pressure_constrained {
            return;
        }
        let chunk_bytes = self.config.chunk_bytes;
        let needed = chunk_bytes as u64;

        // Preserve the display reserve and runtime buffer from the live headroom
        // on every allocation, including allocations after external GPU use changes.
        let admissible = self.provider.budget_snapshot().is_ok_and(|budget| {
            budget.can_admit(0)
                && budget.available_bytes()
                    >= needed.saturating_add(budget.required_free_bytes(
                        self.config.reserve_floor_bytes,
                        RUNTIME_FREE_BUFFER_BYTES,
                    ))
        });
        if !admissible {
            return;
        }

        let compressed_bytes = self
            .compressed_slabs
            .iter()
            .fold(0u64, |sum, slab| sum.saturating_add(slab.len() as u64));
        let workspace_bytes = self
            .codec_workspace
            .as_ref()
            .map_or(0, |mem| mem.len() as u64);
        let raw_ceiling = self.effective_target_bytes.saturating_sub(
            compressed_bytes
                .saturating_add(workspace_bytes)
                .saturating_add(self.device_codec_reservation),
        );
        if needed > raw_ceiling {
            return;
        }
        while self.raw_cached_bytes().saturating_add(needed) > raw_ceiling {
            if !self.evict_coldest_chunk() {
                return;
            }
        }

        if let Ok(mut mem) = self.provider.alloc(chunk_bytes)
            && mem.write_at(chunk_off, data).is_ok()
        {
            self.chunks.insert(
                chunk_base,
                CacheChunk {
                    mem,
                    last_accessed: Instant::now(),
                    valid_ranges: vec![(chunk_off, chunk_off + data.len() as u64)],
                },
            );
        }
    }

    pub(super) fn evict_coldest_chunk(&mut self) -> bool {
        let coldest = self
            .chunks
            .iter()
            .min_by_key(|(_, chunk)| chunk.last_accessed)
            .map(|(&base, _)| base);
        if let Some(base) = coldest {
            self.chunks.remove(&base);
            return true;
        }
        self.evict_coldest_compressed_extent()
    }
}
