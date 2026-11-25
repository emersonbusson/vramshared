// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-vram/src/slab_pool/model.rs
//! Slab classes, stable ticket layout and checked ticket geometry.

pub const FRAME_BYTES: usize = 65_536;
pub const SLOT_ALIGNMENT_BYTES: u64 = 256;
pub const MAX_SLOTS_PER_FRAME: usize = 128;
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlabClass {
    B512 = 0,
    B1024 = 1,
    B2048 = 2,
    B3072 = 3,
    Raw4096 = 4,
}
impl SlabClass {
    pub const ALL: [Self; 5] = [
        Self::B512,
        Self::B1024,
        Self::B2048,
        Self::B3072,
        Self::Raw4096,
    ];
    /// A payload at or above 3072 bytes is stored raw (DT-4, SPEC §3.1).
    #[inline]
    pub const fn select_for_len(compressed_len: u16) -> Self {
        match compressed_len {
            0..=512 => Self::B512,
            513..=1024 => Self::B1024,
            1025..=2048 => Self::B2048,
            2049..=3071 => Self::B3072,
            _ => Self::Raw4096,
        }
    }
    #[inline]
    pub const fn stride_bytes(self) -> usize {
        match self {
            Self::B512 => 512,
            Self::B1024 => 1024,
            Self::B2048 => 2048,
            Self::B3072 => 3072,
            Self::Raw4096 => 4096,
        }
    }
    #[inline]
    pub const fn slots_per_frame(self) -> u8 {
        (FRAME_BYTES / self.stride_bytes()) as u8
    }
    #[inline]
    pub fn from_index(index: u8) -> Option<Self> {
        Self::ALL.get(index as usize).copied()
    }
}
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlotEncoding {
    Lz4 = 0,
    Raw = 1,
}
impl SlotEncoding {
    #[inline]
    pub const fn is_raw(self) -> bool {
        matches!(self, Self::Raw)
    }
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VramSlabTicket {
    pub frame_idx: u32,
    pub sub_slot: u8,
    pub class: SlabClass,
    pub encoding: SlotEncoding,
    pub payload_len: u16,
    pub payload_crc32: u32,
    pub epoch: u64,
}
impl VramSlabTicket {
    #[inline]
    pub fn vram_byte_offset(&self) -> Option<u64> {
        let frame_base = u64::from(self.frame_idx).checked_mul(FRAME_BYTES as u64)?;
        let sub_offset = u64::from(self.sub_slot).checked_mul(self.class.stride_bytes() as u64)?;
        frame_base.checked_add(sub_offset)
    }
    #[inline]
    pub fn end_offset(&self) -> Option<u64> {
        self.vram_byte_offset()?
            .checked_add(self.class.stride_bytes() as u64)
    }
    pub fn validate(&self) -> Result<(), SlabError> {
        if self.sub_slot >= self.class.slots_per_frame() {
            return Err(SlabError::SubSlotOutOfRange);
        }
        if self.vram_byte_offset().is_none() || self.end_offset().is_none() {
            return Err(SlabError::Overflow);
        }
        match self.encoding {
            SlotEncoding::Raw => {
                if self.class != SlabClass::Raw4096 || self.payload_len != 4096 {
                    return Err(SlabError::InvalidTicket);
                }
            }
            SlotEncoding::Lz4 => {
                if self.class == SlabClass::Raw4096
                    || self.payload_len == 0
                    || self.payload_len > 3071
                    || usize::from(self.payload_len) > self.class.stride_bytes()
                {
                    return Err(SlabError::InvalidTicket);
                }
            }
        }
        Ok(())
    }
}
/// Slab pool refusal. No variant implies a partial mutation; every mutating
/// method validates before it touches the frame table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SlabError {
    InvalidFrameCount,
    AllocationFailed,
    FrameOutOfRange,
    SubSlotOutOfRange,
    InvalidTicket,
    Overflow,
    PayloadTooLarge,
    PayloadTooSmall,
}
