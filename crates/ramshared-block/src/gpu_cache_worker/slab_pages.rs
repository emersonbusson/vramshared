// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/gpu_cache_worker/slab_pages.rs
//! Page-indexed normal I/O over the device slab codec.
use super::*;
use ramshared_vram::device_codec::{CodecError, MAX_BATCH_PAGES, PAGE_BYTES, PageDecodeStatus};

impl<'p, P: VramProvider + 'p> GpuCacheWorker<'p, P> {
    pub(super) fn prune_device_frames(&mut self, frames: &[u32]) {
        let mut invalidated_tickets = Vec::new();
        {
            let Some(bridge) = self.slab_bridge.as_ref() else {
                return;
            };
            for &frame in frames {
                let keys: Vec<_> = self
                    .slab_frame_pages
                    .range((frame, 0)..=(frame, u64::MAX))
                    .copied()
                    .collect();
                for (_, page) in keys {
                    if self
                        .slab_pages
                        .get(&page)
                        .is_some_and(|ticket| bridge.pool().lookup(ticket).is_err())
                        && let Some(ticket) = self.slab_pages.remove(&page)
                    {
                        self.slab_frame_pages.remove(&(frame, page));
                        invalidated_tickets.push(ticket);
                    }
                }
            }
        }
        if let Some(bridge) = self.slab_bridge.as_mut() {
            for ticket in &invalidated_tickets {
                bridge.invalidate_codec_ticket(ticket);
            }
        }
        self.sync_device_host_metadata();
    }

    fn sync_device_host_metadata(&self) {
        if let (Some(guard), Some(bridge)) = (&self.host_metadata_guard, &self.slab_bridge) {
            let live = super::host_budget::live_worker_metadata_bytes(
                bridge.pool(),
                self.slab_pages.len(),
                self.slab_frame_pages.len(),
            );
            let _ = guard.update_usage(live);
        }
    }

    fn ensure_device_host_metadata_room(&mut self, incoming_pages: usize) -> bool {
        let extra =
            (incoming_pages as u64).saturating_mul(super::host_budget::LIVE_PAGE_METADATA_BYTES);
        loop {
            let (limit, projected) = {
                let Some(guard) = &self.host_metadata_guard else {
                    return true;
                };
                let Some(bridge) = &self.slab_bridge else {
                    return false;
                };
                let live = super::host_budget::live_worker_metadata_bytes(
                    bridge.pool(),
                    self.slab_pages.len(),
                    self.slab_frame_pages.len(),
                );
                (guard.limit_bytes(), live.saturating_add(extra))
            };
            if projected <= limit
                && self
                    .host_metadata_guard
                    .as_ref()
                    .is_some_and(|guard| guard.update_usage(projected))
            {
                return true;
            }
            let evicted = self
                .slab_bridge
                .as_mut()
                .and_then(|bridge| bridge.evict_coldest_frame());
            let Some(frame) = evicted else {
                return false;
            };
            self.prune_device_frames(&[frame]);
        }
    }
    pub(super) fn invalidate_device_pages(&mut self, start: u64, end: u64) {
        if start >= end {
            return;
        }
        let first = start / PAGE_BYTES as u64;
        let last = (end - 1) / PAGE_BYTES as u64;
        let keys: Vec<_> = self
            .slab_pages
            .range(first..=last)
            .map(|(&key, _)| key)
            .collect();
        for key in keys {
            if let Some(ticket) = self.slab_pages.remove(&key) {
                self.slab_frame_pages.remove(&(ticket.frame_idx, key));
                if let Some(bridge) = self.slab_bridge.as_mut() {
                    bridge.invalidate_ticket(&ticket);
                }
            }
        }
    }

    pub(super) fn write_device_pages(&mut self, offset: u64, data: &[u8]) -> bool {
        if self.device_codec_reservation == 0
            || !offset.is_multiple_of(PAGE_BYTES as u64)
            || !data.len().is_multiple_of(PAGE_BYTES)
            || data.is_empty()
        {
            return false;
        }
        let Some(end) = offset.checked_add(data.len() as u64) else {
            return false;
        };
        let batch_pages = data.len() / PAGE_BYTES;
        if !self.ensure_device_host_metadata_room(batch_pages.min(MAX_BATCH_PAGES)) {
            self.invalidate_device_pages(offset, end);
            return false;
        }
        for (batch, bytes) in data.chunks(MAX_BATCH_PAGES * PAGE_BYTES).enumerate() {
            let pages: Vec<[u8; PAGE_BYTES]> = bytes.as_chunks::<PAGE_BYTES>().0.to_vec();
            let mut tickets = vec![None; pages.len()];
            let (result, frames) = {
                let Some(bridge) = self.slab_bridge.as_mut() else {
                    return false;
                };
                bridge.begin_eviction_observation();
                let result = bridge.compress_batch(&pages, &mut tickets);
                (result, bridge.finish_eviction_observation())
            };
            // Examine only pages belonging to frames evicted in this batch.
            // This also runs on failure, since a failed batch can evict an
            // earlier clean frame before rolling back its own allocations.
            self.prune_device_frames(&frames);
            let Some(bridge) = self.slab_bridge.as_mut() else {
                return false;
            };
            if result.is_err()
                || tickets.iter().any(|ticket| {
                    ticket
                        .as_ref()
                        .is_none_or(|ticket| bridge.pool().lookup(ticket).is_err())
                })
            {
                for ticket in tickets.iter().flatten() {
                    bridge.invalidate_ticket(ticket);
                }
                if matches!(result, Err(CodecError::DeadlineExceeded)) {
                    self.compressed.note_codec_timeout();
                    self.disabled = true;
                }
                self.invalidate_device_pages(offset, end);
                return false;
            }
            let base = offset / PAGE_BYTES as u64 + (batch * MAX_BATCH_PAGES) as u64;
            for (index, ticket) in tickets.into_iter().enumerate() {
                if let Some(ticket) = ticket {
                    let page = base + index as u64;
                    if let Some(previous) = self.slab_pages.insert(page, ticket) {
                        self.slab_frame_pages.remove(&(previous.frame_idx, page));
                    }
                    self.slab_frame_pages.insert((ticket.frame_idx, page));
                }
            }
            self.sync_device_host_metadata();
        }
        true
    }

    pub(super) fn read_device_pages(&mut self, offset: u64, len: usize) -> Option<Vec<u8>> {
        if self.device_codec_reservation == 0
            || !offset.is_multiple_of(PAGE_BYTES as u64)
            || !len.is_multiple_of(PAGE_BYTES)
            || len == 0
        {
            return None;
        }
        let first = offset / PAGE_BYTES as u64;
        let tickets: Vec<_> = (0..len / PAGE_BYTES)
            .map(|i| self.slab_pages.get(&(first + i as u64)).cloned())
            .collect::<Option<_>>()?;
        let bridge = self.slab_bridge.as_mut()?;
        let mut result = Vec::with_capacity(len);
        for (batch_index, batch) in tickets.chunks(MAX_BATCH_PAGES).enumerate() {
            let mut pages = vec![[0; PAGE_BYTES]; batch.len()];
            let mut status = vec![PageDecodeStatus::InvalidTicket; batch.len()];
            if let Err(error) = bridge.recover_batch(batch, &mut pages, &mut status, |_| {
                Err("origin fallback required".into())
            }) {
                for (index, ticket) in batch.iter().enumerate() {
                    if bridge.pool().lookup(ticket).is_err() {
                        let page = first + (batch_index * MAX_BATCH_PAGES + index) as u64;
                        self.slab_pages.remove(&page);
                        self.slab_frame_pages.remove(&(ticket.frame_idx, page));
                    }
                }
                if matches!(error, CodecError::DeadlineExceeded) {
                    self.compressed.note_codec_timeout();
                    self.disabled = true;
                }
                return None;
            }
            if status.iter().any(|s| *s != PageDecodeStatus::Ok) {
                return None;
            }
            for page in pages {
                result.extend_from_slice(&page);
            }
        }
        Some(result)
    }
}
