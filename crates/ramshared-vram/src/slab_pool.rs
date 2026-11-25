// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-vram/src/slab_pool.rs
// Size-classed VRAM slab metadata (SPEC gpu-vram-hardware-compression ITEM-1).
use crate::frame_lru::FrameLru;
use std::collections::HashMap;

mod model;
pub use model::{
    FRAME_BYTES, MAX_SLOTS_PER_FRAME, SLOT_ALIGNMENT_BYTES, SlabClass, SlabError, SlotEncoding,
    VramSlabTicket,
};

struct Frame {
    class: SlabClass,
    allocated: u128,
    committed: u128,
    used: u8,
    epoch: u64,
    in_class_list: bool,
    class_pos: Option<usize>,
}
impl Frame {
    fn new() -> Self {
        Self {
            class: SlabClass::B512,
            allocated: 0,
            committed: 0,
            used: 0,
            epoch: 0,
            in_class_list: false,
            class_pos: None,
        }
    }
    fn slots(&self) -> u8 {
        self.class.slots_per_frame()
    }
    fn is_full(&self) -> bool {
        self.used >= self.slots()
    }
}
pub struct VramSlabPool {
    frames: Vec<Frame>,
    /// Frames with at least one free sub-slot, per class.
    class_frames: [Vec<u32>; 5],
    /// Frames with no allocated sub-slot.
    free_frames: Vec<u32>,
    lru: FrameLru,
    /// One pool-wide slot epoch table; a per-frame HashMap is unnecessary.
    slot_epochs: HashMap<(u32, u8), u64>,
    next_epoch: u64,
    active_frames: u64,
    logical_pages: u64,
    evictions: u64,
    observed_evictions: Option<Vec<u32>>,
}
impl VramSlabPool {
    /// Metadata-only pool for a real GPU backing region.
    pub fn new(frame_count: u32) -> Result<Self, SlabError> {
        if frame_count == 0 {
            return Err(SlabError::InvalidFrameCount);
        }
        let count = usize::try_from(frame_count).map_err(|_| SlabError::InvalidFrameCount)?;
        let mut frames = Vec::new();
        let mut free_frames = Vec::new();
        frames
            .try_reserve_exact(count)
            .map_err(|_| SlabError::AllocationFailed)?;
        free_frames
            .try_reserve_exact(count)
            .map_err(|_| SlabError::AllocationFailed)?;
        let lru = FrameLru::new(count).map_err(|_| SlabError::AllocationFailed)?;
        frames.resize_with(count, Frame::new);
        for idx in (0..frame_count).rev() {
            free_frames.push(idx);
        }
        Ok(Self {
            frames,
            class_frames: std::array::from_fn(|_| Vec::new()),
            free_frames,
            lru,
            slot_epochs: HashMap::new(),
            next_epoch: 1,
            active_frames: 0,
            logical_pages: 0,
            evictions: 0,
            observed_evictions: None,
        })
    }
    pub fn frame_count(&self) -> u32 {
        self.frames.len() as u32
    }
    /// Fixed per-frame host storage across `frames`, `free_frames`, and `FrameLru`.
    pub const fn per_frame_fixed_metadata_bytes() -> usize {
        std::mem::size_of::<Frame>()
            + std::mem::size_of::<u32>()
            + 2 * std::mem::size_of::<Option<u32>>()
            + 1
    }
    /// Live entries held in the slot-epoch override map.
    pub fn slot_epochs_len(&self) -> usize {
        self.slot_epochs.len()
    }
    /// Lower bound from owned vector storage plus the slot-epoch override table.
    pub fn known_total_metadata_bytes(&self) -> u64 {
        let epochs = self
            .slot_epochs
            .capacity()
            .saturating_mul(std::mem::size_of::<((u32, u8), u64)>().saturating_add(1))
            as u64;
        self.known_fixed_metadata_bytes().saturating_add(epochs)
    }
    /// Evict the oldest active frame back onto the free list so host metadata shrinks cleanly.
    pub fn evict_lru_frame(&mut self) -> Option<u32> {
        let frame_idx = self.pop_lru()?;
        if self.evict_frame(frame_idx).is_ok() {
            self.free_frames.push(frame_idx);
            Some(frame_idx)
        } else {
            None
        }
    }
    /// Lower bound from owned vector storage only. Hash maps, caller indices,
    /// allocator bookkeeping and codec memory are deliberately excluded.
    pub fn known_fixed_metadata_bytes(&self) -> u64 {
        let frames = self
            .frames
            .capacity()
            .saturating_mul(std::mem::size_of::<Frame>());
        let free = self
            .free_frames
            .capacity()
            .saturating_mul(std::mem::size_of::<u32>());
        let classes = self.class_frames.iter().fold(0usize, |sum, list| {
            sum.saturating_add(list.capacity().saturating_mul(std::mem::size_of::<u32>()))
        });
        let events = self.observed_evictions.as_ref().map_or(0, |list| {
            list.capacity().saturating_mul(std::mem::size_of::<u32>())
        });
        (frames
            .saturating_add(free)
            .saturating_add(classes)
            .saturating_add(events) as u64)
            .saturating_add(self.lru.known_storage_bytes())
    }
    pub fn active_frames(&self) -> u64 {
        self.active_frames
    }
    pub fn logical_pages_cached(&self) -> u64 {
        self.logical_pages
    }
    pub fn evictions(&self) -> u64 {
        self.evictions
    }
    /// Begin a bounded caller-owned observation of frame evictions.
    pub fn begin_eviction_observation(&mut self) {
        self.observed_evictions = Some(Vec::new());
    }
    /// Finish the observation; subsequent allocations do not retain events.
    pub fn finish_eviction_observation(&mut self) -> Vec<u32> {
        self.observed_evictions.take().unwrap_or_default()
    }
    pub fn effective_density_ratio(&self) -> Option<f64> {
        if self.active_frames == 0 {
            return None;
        }
        let logical = (self.logical_pages as f64) * 4096.0;
        let physical = (self.active_frames as f64) * FRAME_BYTES as f64;
        Some(logical / physical)
    }
    /// Allocate one sub-slot, evicting the oldest clean frame when necessary.
    pub fn alloc_sub_slot(&mut self, class: SlabClass) -> Result<VramSlabTicket, SlabError> {
        let frame_idx = self.frame_for_class(class)?;
        let frame = self
            .frames
            .get(frame_idx as usize)
            .ok_or(SlabError::FrameOutOfRange)?;
        if frame.class != class {
            return Err(SlabError::InvalidTicket);
        }
        let frame = self
            .frames
            .get_mut(frame_idx as usize)
            .ok_or(SlabError::FrameOutOfRange)?;
        let slot = (!frame.allocated).trailing_zeros();
        if slot >= u32::from(frame.slots()) {
            return Err(SlabError::InvalidTicket);
        }
        let sub_slot = slot as u8;
        let epoch = self
            .slot_epochs
            .get(&(frame_idx, sub_slot))
            .copied()
            .unwrap_or(frame.epoch);
        frame.allocated |= 1u128 << slot;
        frame.used = frame.used.saturating_add(1);
        let full = frame.is_full();
        if full {
            self.detach_class(frame_idx);
        } else {
            self.attach_class(frame_idx);
        }
        self.touch(frame_idx);
        Ok(VramSlabTicket {
            frame_idx,
            sub_slot,
            class,
            encoding: SlotEncoding::Lz4,
            payload_len: 0,
            payload_crc32: 0,
            epoch,
        })
    }
    /// Commit payload metadata; the ticket stays valid until invalidated.
    pub fn commit(
        &mut self,
        ticket: &mut VramSlabTicket,
        encoding: SlotEncoding,
        payload_len: u16,
        payload_crc32: u32,
    ) -> Result<(), SlabError> {
        ticket.encoding = encoding;
        ticket.payload_len = payload_len;
        ticket.payload_crc32 = payload_crc32;
        if let Err(error) = ticket.validate() {
            let _ = self.invalidate(ticket);
            return Err(error);
        }
        let is_set = {
            let frame = self
                .frames
                .get(ticket.frame_idx as usize)
                .ok_or(SlabError::FrameOutOfRange)?;
            self.ticket_bit_is_set(frame, ticket)
        };
        if !is_set {
            return Err(SlabError::InvalidTicket);
        }
        let frame = self
            .frames
            .get_mut(ticket.frame_idx as usize)
            .ok_or(SlabError::FrameOutOfRange)?;
        let was_committed = frame.committed & (1u128 << ticket.sub_slot) != 0;
        frame.committed |= 1u128 << ticket.sub_slot;
        if !was_committed {
            self.logical_pages = self.logical_pages.saturating_add(1);
        }
        Ok(())
    }
    /// Clear a slot and advance its epoch; repeated calls are safe no-ops.
    pub fn invalidate(&mut self, ticket: &VramSlabTicket) -> bool {
        let Some(frame) = self.frames.get(ticket.frame_idx as usize) else {
            return false;
        };
        if !self.ticket_bit_is_set(frame, ticket) {
            return false;
        }
        let Some(frame) = self.frames.get_mut(ticket.frame_idx as usize) else {
            return false;
        };
        let bit = 1u128 << ticket.sub_slot;
        frame.allocated &= !bit;
        let was_committed = frame.committed & bit != 0;
        frame.committed &= !bit;
        frame.used = frame.used.saturating_sub(1);
        let next = self.next_epoch;
        self.next_epoch = self.next_epoch.saturating_add(1);
        self.slot_epochs
            .insert((ticket.frame_idx, ticket.sub_slot), next);
        if was_committed {
            self.logical_pages = self.logical_pages.saturating_sub(1);
        }
        if frame.used == 0 {
            self.release_empty_frame(ticket.frame_idx);
        } else {
            self.attach_class(ticket.frame_idx);
        }
        true
    }
    /// Validate the allocation (slot, class, frame and epoch) of a ticket.
    pub fn lookup(&self, ticket: &VramSlabTicket) -> Result<(), SlabError> {
        self.checked_ticket_offset(ticket).map(|_| ())
    }

    /// Validate a ticket and promote its frame to the most-recently-used end.
    /// Read hits must call this; allocation alone is not a read.
    pub fn lookup_and_touch(&mut self, ticket: &VramSlabTicket) -> Result<(), SlabError> {
        self.lookup(ticket)?;
        self.touch(ticket.frame_idx);
        Ok(())
    }
    fn ticket_bit_is_set(&self, frame: &Frame, ticket: &VramSlabTicket) -> bool {
        if ticket.sub_slot >= frame.slots() || frame.class != ticket.class {
            return false;
        }
        let bit = 1u128 << ticket.sub_slot;
        if frame.allocated & bit == 0 {
            return false;
        }
        let epoch = self
            .slot_epochs
            .get(&(ticket.frame_idx, ticket.sub_slot))
            .copied()
            .unwrap_or(frame.epoch);
        epoch == ticket.epoch
    }
    fn checked_ticket_offset(&self, ticket: &VramSlabTicket) -> Result<u64, SlabError> {
        if ticket.sub_slot >= ticket.class.slots_per_frame() {
            return Err(SlabError::SubSlotOutOfRange);
        }
        let frame = self
            .frames
            .get(ticket.frame_idx as usize)
            .ok_or(SlabError::FrameOutOfRange)?;
        if frame.class != ticket.class || !self.ticket_bit_is_set(frame, ticket) {
            return Err(SlabError::InvalidTicket);
        }
        ticket.vram_byte_offset().ok_or(SlabError::Overflow)
    }
    fn frame_for_class(&mut self, class: SlabClass) -> Result<u32, SlabError> {
        if let Some(frame_idx) = self.class_frames[class as usize].pop() {
            let frame = self
                .frames
                .get_mut(frame_idx as usize)
                .ok_or(SlabError::FrameOutOfRange)?;
            frame.in_class_list = false;
            frame.class_pos = None;
            return Ok(frame_idx);
        }
        if let Some(frame_idx) = self.free_frames.pop() {
            self.assign_frame(frame_idx, class)?;
            return Ok(frame_idx);
        }
        let frame_idx = self.pop_lru().ok_or(SlabError::FrameOutOfRange)?;
        self.evict_frame(frame_idx)?;
        self.assign_frame(frame_idx, class)?;
        Ok(frame_idx)
    }
    fn assign_frame(&mut self, frame_idx: u32, class: SlabClass) -> Result<(), SlabError> {
        self.purge_frame_epochs(frame_idx);
        let frame = self
            .frames
            .get_mut(frame_idx as usize)
            .ok_or(SlabError::FrameOutOfRange)?;
        frame.class = class;
        frame.allocated = 0;
        frame.committed = 0;
        frame.used = 0;
        frame.epoch = self.next_epoch;
        self.next_epoch = self.next_epoch.saturating_add(1);
        frame.in_class_list = false;
        frame.class_pos = None;
        self.attach_class(frame_idx);
        self.lru.push_back(frame_idx);
        self.active_frames = self.active_frames.saturating_add(1);
        Ok(())
    }
    fn evict_frame(&mut self, frame_idx: u32) -> Result<(), SlabError> {
        self.detach_class(frame_idx);
        self.purge_frame_epochs(frame_idx);
        let frame = self
            .frames
            .get_mut(frame_idx as usize)
            .ok_or(SlabError::FrameOutOfRange)?;
        let committed = frame.committed.count_ones() as u64;
        self.logical_pages = self.logical_pages.saturating_sub(committed);
        frame.epoch = self.next_epoch;
        self.next_epoch = self.next_epoch.saturating_add(1);
        frame.allocated = 0;
        frame.committed = 0;
        frame.used = 0;
        frame.in_class_list = false;
        frame.class_pos = None;
        self.active_frames = self.active_frames.saturating_sub(1);
        self.evictions = self.evictions.saturating_add(1);
        if let Some(events) = self.observed_evictions.as_mut() {
            events.push(frame_idx);
        }
        Ok(())
    }
    fn release_empty_frame(&mut self, frame_idx: u32) {
        self.detach_class(frame_idx);
        self.lru.remove(frame_idx);
        self.purge_frame_epochs(frame_idx);
        let frame = &mut self.frames[frame_idx as usize];
        frame.allocated = 0;
        frame.committed = 0;
        frame.used = 0;
        self.active_frames = self.active_frames.saturating_sub(1);
        self.free_frames.push(frame_idx);
    }
    fn purge_frame_epochs(&mut self, frame_idx: u32) {
        for slot in 0..MAX_SLOTS_PER_FRAME {
            self.slot_epochs.remove(&(frame_idx, slot as u8));
        }
    }

    fn attach_class(&mut self, frame_idx: u32) {
        let frame = &self.frames[frame_idx as usize];
        if frame.in_class_list || frame.used >= frame.slots() {
            return;
        }
        let class = frame.class as usize;
        let pos = self.class_frames[class].len();
        self.class_frames[class].push(frame_idx);
        let frame = &mut self.frames[frame_idx as usize];
        frame.in_class_list = true;
        frame.class_pos = Some(pos);
    }
    fn detach_class(&mut self, frame_idx: u32) {
        let Some(pos) = self.frames[frame_idx as usize].class_pos else {
            return;
        };
        let class = self.frames[frame_idx as usize].class as usize;
        let list = &mut self.class_frames[class];
        if pos >= list.len() {
            return;
        }
        list.swap_remove(pos);
        if let Some(moved) = list.get(pos).copied() {
            self.frames[moved as usize].class_pos = Some(pos);
        }
        let frame = &mut self.frames[frame_idx as usize];
        frame.in_class_list = false;
        frame.class_pos = None;
    }
    fn touch(&mut self, frame_idx: u32) {
        self.lru.touch(frame_idx);
    }
    fn pop_lru(&mut self) -> Option<u32> {
        self.lru.pop_front()
    }
}

#[cfg(test)]
mod tests;
