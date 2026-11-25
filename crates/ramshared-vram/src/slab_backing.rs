// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-vram/src/slab_backing.rs
//
// Host-side backing store for the deterministic reference codec. Production
// GPU codecs address VRAM directly and do not use this module.

use super::slab_pool::{FRAME_BYTES, SlabError, VramSlabTicket};

/// A contiguous host allocation mirroring one slab-pool frame space.
#[derive(Default)]
pub struct HostSlabBacking {
    bytes: Vec<u8>,
}

impl HostSlabBacking {
    pub fn new(frame_count: u32) -> Result<Self, SlabError> {
        let frames = usize::try_from(frame_count).map_err(|_| SlabError::InvalidFrameCount)?;
        let bytes = frames.checked_mul(FRAME_BYTES).ok_or(SlabError::Overflow)?;
        Ok(Self {
            bytes: vec![0u8; bytes],
        })
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Grow the backing to hold at least `frame_count` frames. The reference
    /// codec is a test/lab backend; the caller caps the frame count.
    pub fn ensure_frames(&mut self, frame_count: u32) -> Result<(), SlabError> {
        let frames = usize::try_from(frame_count).map_err(|_| SlabError::InvalidFrameCount)?;
        let needed = frames.checked_mul(FRAME_BYTES).ok_or(SlabError::Overflow)?;
        if self.bytes.len() < needed {
            self.bytes.resize(needed, 0);
        }
        Ok(())
    }

    /// Mutable view of the committed payload, used by the deterministic test
    /// hook that flips a byte to exercise the CRC gate.
    pub fn bytes_mut(&mut self, ticket: &VramSlabTicket) -> Result<&mut [u8], SlabError> {
        if ticket.sub_slot >= ticket.class.slots_per_frame() {
            return Err(SlabError::SubSlotOutOfRange);
        }
        let len = usize::from(ticket.payload_len);
        if len == 0 {
            return Err(SlabError::PayloadTooSmall);
        }
        let start = usize::try_from(ticket.vram_byte_offset().ok_or(SlabError::Overflow)?)
            .map_err(|_| SlabError::Overflow)?;
        let end = start.checked_add(len).ok_or(SlabError::Overflow)?;
        if end > self.bytes.len() {
            return Err(SlabError::FrameOutOfRange);
        }
        self.bytes.get_mut(start..end).ok_or(SlabError::Overflow)
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn write(&mut self, ticket: &VramSlabTicket, payload: &[u8]) -> Result<(), SlabError> {
        let capacity = ticket.class.stride_bytes();
        if payload.len() > capacity {
            return Err(SlabError::PayloadTooLarge);
        }
        let start = self.offset(ticket)?;
        let end = start
            .checked_add(payload.len())
            .ok_or(SlabError::Overflow)?;
        let dst = self.bytes.get_mut(start..end).ok_or(SlabError::Overflow)?;
        dst.copy_from_slice(payload);
        Ok(())
    }

    pub fn read(&self, ticket: &VramSlabTicket) -> Result<Vec<u8>, SlabError> {
        let len = usize::from(ticket.payload_len);
        if len == 0 {
            return Err(SlabError::PayloadTooSmall);
        }
        let start = self.offset(ticket)?;
        let end = start.checked_add(len).ok_or(SlabError::Overflow)?;
        let src = self.bytes.get(start..end).ok_or(SlabError::Overflow)?;
        Ok(src.to_vec())
    }

    fn offset(&self, ticket: &VramSlabTicket) -> Result<usize, SlabError> {
        if ticket.sub_slot >= ticket.class.slots_per_frame() {
            return Err(SlabError::SubSlotOutOfRange);
        }
        let offset = ticket.vram_byte_offset().ok_or(SlabError::Overflow)?;
        let start = usize::try_from(offset).map_err(|_| SlabError::Overflow)?;
        let stride = ticket.class.stride_bytes();
        let end = start.checked_add(stride).ok_or(SlabError::Overflow)?;
        if end > self.bytes.len() {
            return Err(SlabError::FrameOutOfRange);
        }
        Ok(start)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::slab_pool::{SlabClass, SlotEncoding};

    fn ticket(frame_idx: u32, sub_slot: u8, class: SlabClass) -> VramSlabTicket {
        VramSlabTicket {
            frame_idx,
            sub_slot,
            class,
            encoding: SlotEncoding::Lz4,
            payload_len: 0,
            payload_crc32: 0,
            epoch: 1,
        }
    }

    #[test]
    fn backing_roundtrips_a_payload_at_the_ticket_offset() {
        let mut backing = HostSlabBacking::new(1).unwrap();
        let mut ticket = ticket(0, 3, SlabClass::B1024);
        ticket.payload_len = 900;
        backing.write(&ticket, &[0x5A; 900]).unwrap();
        assert_eq!(backing.read(&ticket).unwrap(), vec![0x5A; 900]);
    }

    #[test]
    fn backing_rejects_overflow_and_missing_payload() {
        let mut backing = HostSlabBacking::new(1).unwrap();
        let mut ticket = ticket(0, 0, SlabClass::B512);
        assert!(matches!(
            backing.write(&ticket, &[0u8; 513]),
            Err(SlabError::PayloadTooLarge)
        ));
        assert!(matches!(
            backing.read(&ticket),
            Err(SlabError::PayloadTooSmall)
        ));
        ticket.frame_idx = 2;
        assert!(matches!(
            backing.write(&ticket, &[0u8; 1]),
            Err(SlabError::FrameOutOfRange)
        ));
    }
}
