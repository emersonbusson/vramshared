// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/ipc_cache_client/deferred_invalidations.rs
//! Bounded dirty ranges retained while the worker socket is backpressured.

pub(super) const MAX_RANGES: usize = 2048;
const FLUSH_BATCH_LIMIT: usize = 128;

#[derive(Default)]
pub(super) struct DeferredInvalidations {
    ranges: Vec<(u64, u64)>,
}

impl DeferredInvalidations {
    /// Merge overlapping or adjacent ranges only when one wire frame can
    /// represent the complete union. A failed insertion leaves custody with
    /// the caller, which must revoke the cache.
    pub(super) fn insert(&mut self, start: u64, len: usize) -> bool {
        let Ok(len) = u64::try_from(len) else {
            return false;
        };
        if len == 0 || len > u64::from(u32::MAX) {
            return false;
        }
        let Some(mut end) = start.checked_add(len) else {
            return false;
        };
        let mut start = start;
        let mut index = 0;
        while index < self.ranges.len() {
            let (old_start, old_end) = self.ranges[index];
            let merged_start = start.min(old_start);
            let merged_end = end.max(old_end);
            if start <= old_end
                && old_start <= end
                && merged_end - merged_start <= u64::from(u32::MAX)
            {
                start = merged_start;
                end = merged_end;
                self.ranges.swap_remove(index);
            } else {
                index += 1;
            }
        }
        if self.ranges.len() >= MAX_RANGES {
            return false;
        }
        self.ranges.push((start, end));
        true
    }

    pub(super) fn first(&self) -> Result<Option<(u64, u32)>, ()> {
        let Some(&(start, end)) = self.ranges.first() else {
            return Ok(None);
        };
        let length = end.checked_sub(start).ok_or(())?;
        let length = u32::try_from(length).map_err(|_| ())?;
        if length == 0 {
            return Err(());
        }
        Ok(Some((start, length)))
    }

    pub(super) fn pop_first(&mut self) {
        self.ranges.swap_remove(0);
    }

    pub(super) fn clear(&mut self) {
        self.ranges.clear();
    }

    pub(super) fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }
}

use super::{CacheMutation, CacheState, FrameHeader, IpcCacheClient, MutationQueue};
use super::{MSG_INVALIDATE_REQ, STATUS_OK, queue_frame};
use std::time::Instant;

impl IpcCacheClient {
    /// DT-11: replace a write-mirror that could not be queued with an
    /// `MSG_INVALIDATE` for exactly its range.
    pub(super) fn convert_dropped_update_to_invalidate(
        &mut self,
        offset: u64,
        range_len: usize,
    ) -> CacheMutation {
        self.seq = self.seq.saturating_add(1);
        let invalidate = FrameHeader {
            msg_type: MSG_INVALIDATE_REQ,
            status: STATUS_OK,
            correlation_id: self.seq,
            offset,
            payload_len: 0,
            aux: range_len as u32,
        };
        match queue_frame(
            &mut self.socket,
            &mut self.pending_frame,
            &invalidate.encode(),
        ) {
            MutationQueue::Queued => {
                self.last_mutation_at = Some(Instant::now());
                CacheMutation::Skipped
            }
            MutationQueue::Dropped => self.defer_invalidation(offset, range_len),
            MutationQueue::PeerGone(error) => {
                eprintln!("[ramsharedd] invalidate frame write error: {error}");
                self.fail("nonblocking invalidate frame write failed");
                CacheMutation::Failed
            }
        }
    }

    pub(super) fn defer_invalidation(&mut self, offset: u64, len: usize) -> CacheMutation {
        if len == 0 {
            return CacheMutation::Skipped;
        }
        if !self.deferred_invalidations.insert(offset, len) {
            self.fail("deferred invalidation capacity or range invalid");
            return CacheMutation::Failed;
        }
        self.last_mutation_at = Some(Instant::now());
        CacheMutation::Skipped
    }

    /// Queue up to `FLUSH_BATCH_LIMIT` ranges per call behind prior mutations.
    pub(super) fn flush_deferred_invalidations(&mut self) -> Result<bool, ()> {
        if self.state != CacheState::Active {
            return Err(());
        }
        for _ in 0..FLUSH_BATCH_LIMIT {
            let (offset, len) = match self.deferred_invalidations.first() {
                Ok(Some(range)) => range,
                Ok(None) => return Ok(true),
                Err(()) => {
                    self.fail("deferred invalidation invariant failed");
                    return Err(());
                }
            };
            self.seq = self.seq.saturating_add(1);
            let frame = FrameHeader {
                msg_type: MSG_INVALIDATE_REQ,
                status: STATUS_OK,
                correlation_id: self.seq,
                offset,
                payload_len: 0,
                aux: len,
            };
            match queue_frame(&mut self.socket, &mut self.pending_frame, &frame.encode()) {
                MutationQueue::Queued => {
                    self.deferred_invalidations.pop_first();
                    self.last_mutation_at = Some(Instant::now());
                }
                MutationQueue::Dropped => return Ok(false),
                MutationQueue::PeerGone(_) => {
                    self.fail("deferred invalidation write failed");
                    return Err(());
                }
            }
        }
        Ok(self.deferred_invalidations.is_empty())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;

    #[test]
    fn ranges_merge_and_keep_large_offsets_without_wrapping() {
        let mut ranges = DeferredInvalidations::default();
        let base = 8u64 << 30;
        assert!(ranges.insert(base + 4096, 4096));
        assert!(ranges.insert(base, 4096));
        assert_eq!(ranges.first(), Ok(Some((base, 8192))));
        assert!(ranges.insert(base + 16384, 4096));
        assert_eq!(ranges.ranges.len(), 2);
        ranges.pop_first();
        assert_eq!(ranges.first(), Ok(Some((base + 16384, 4096))));
        ranges.clear();
        assert!(ranges.is_empty());
    }

    #[test]
    fn invalid_or_unrepresentable_ranges_are_refused() {
        let mut ranges = DeferredInvalidations::default();
        assert!(!ranges.insert(0, 0));
        assert!(!ranges.insert(u64::MAX - 1, 4));
        if let Ok(too_large) = usize::try_from(u64::from(u32::MAX) + 1) {
            assert!(!ranges.insert(0, too_large));
        }
        assert!(ranges.is_empty());
        assert!(ranges.insert(0, u32::MAX as usize));
        assert!(ranges.insert(u64::from(u32::MAX), 1));
        assert_eq!(ranges.ranges.len(), 2);
    }

    #[test]
    fn malformed_internal_range_revokes_instead_of_allowing_a_read() {
        let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut client = IpcCacheClient::new(socket, std::time::Duration::from_secs(1), 1024);
        client.deferred_invalidations.ranges.push((5, 5));
        assert!(client.flush_deferred_invalidations().is_err());
        assert_eq!(client.state, CacheState::Unavailable);
    }
}
