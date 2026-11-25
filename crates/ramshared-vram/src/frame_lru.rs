// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-vram/src/frame_lru.rs
//
// Intrusive O(1) frame LRU used by the VRAM slab pool.

/// Doubly linked list over a fixed frame index space. Every operation is O(1)
/// and allocates nothing after construction.
pub(super) struct FrameLru {
    prev: Vec<Option<u32>>,
    next: Vec<Option<u32>>,
    linked: Vec<bool>,
    head: Option<u32>,
    tail: Option<u32>,
}

impl FrameLru {
    /// Allocated element storage, excluding allocator bookkeeping.
    pub(super) fn known_storage_bytes(&self) -> u64 {
        let links = self
            .prev
            .capacity()
            .saturating_add(self.next.capacity())
            .saturating_mul(std::mem::size_of::<Option<u32>>());
        links.saturating_add(self.linked.capacity().div_ceil(8)) as u64
    }
    pub(super) fn new(len: usize) -> Result<Self, std::collections::TryReserveError> {
        let mut prev = Vec::new();
        let mut next = Vec::new();
        let mut linked = Vec::new();
        prev.try_reserve_exact(len)?;
        next.try_reserve_exact(len)?;
        linked.try_reserve_exact(len)?;
        prev.resize(len, None);
        next.resize(len, None);
        linked.resize(len, false);
        Ok(Self {
            prev,
            next,
            linked,
            head: None,
            tail: None,
        })
    }

    pub(super) fn push_back(&mut self, idx: u32) {
        let Some(slot) = self.linked.get_mut(idx as usize) else {
            return;
        };
        if *slot {
            return;
        }
        *slot = true;
        self.prev[idx as usize] = self.tail;
        self.next[idx as usize] = None;
        if let Some(tail) = self.tail {
            self.next[tail as usize] = Some(idx);
        } else {
            self.head = Some(idx);
        }
        self.tail = Some(idx);
    }

    pub(super) fn remove(&mut self, idx: u32) {
        if !self.linked.get(idx as usize).copied().unwrap_or(false) {
            return;
        }
        let prev = self.prev[idx as usize];
        let next = self.next[idx as usize];
        if let Some(prev) = prev {
            self.next[prev as usize] = next;
        } else {
            self.head = next;
        }
        if let Some(next) = next {
            self.prev[next as usize] = prev;
        } else {
            self.tail = prev;
        }
        self.linked[idx as usize] = false;
        self.prev[idx as usize] = None;
        self.next[idx as usize] = None;
    }

    pub(super) fn pop_front(&mut self) -> Option<u32> {
        let head = self.head?;
        self.remove(head);
        Some(head)
    }

    /// Move a frame to the most-recently-used end. `push_back` already does
    /// this for a new frame, so a repeated call on the tail is a no-op.
    pub(super) fn touch(&mut self, idx: u32) {
        if self.tail == Some(idx) {
            return;
        }
        self.remove(idx);
        self.push_back(idx);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn impossible_frame_count_is_a_recoverable_allocation_error() {
        assert!(FrameLru::new(usize::MAX).is_err());
    }

    #[test]
    fn lru_moves_touched_frames_and_pops_the_oldest() {
        let mut lru = FrameLru::new(3).unwrap();
        lru.push_back(0);
        lru.push_back(1);
        lru.push_back(2);
        lru.touch(0);
        assert_eq!(lru.pop_front(), Some(1));
        assert_eq!(lru.pop_front(), Some(2));
        assert_eq!(lru.pop_front(), Some(0));
        assert_eq!(lru.pop_front(), None);
    }

    #[test]
    fn lru_remove_is_idempotent_and_ignores_out_of_range() {
        let mut lru = FrameLru::new(2).unwrap();
        lru.push_back(0);
        lru.remove(0);
        lru.remove(0);
        lru.remove(9);
        assert_eq!(lru.pop_front(), None);
    }
}
