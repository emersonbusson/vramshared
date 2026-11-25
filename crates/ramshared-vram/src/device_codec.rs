// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-vram/src/device_codec.rs
// Device codec contract and deterministic LZ4 reference implementation
// (SPEC gpu-vram-hardware-compression ITEM-1/ITEM-2/ITEM-3).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::codec::crc32;
use crate::slab_backing::HostSlabBacking;
use crate::slab_pool::{SlabClass, SlabError, SlotEncoding, VramSlabPool, VramSlabTicket};

mod lz4;
pub use lz4::{Lz4DecodeError, Lz4DecodeResult, lz4_compress_block, lz4_decompress_block};

pub const MAX_BATCH_PAGES: usize = 64;
pub const PAGE_BYTES: usize = 4096;
pub const CODEC_SUBDEADLINE: Duration = Duration::from_millis(20);
pub const MAX_COMPRESSED_LEN: usize = 3071;

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageDecodeStatus {
    Ok = 0,
    PreDecodeCrcMismatch = 1,
    CorruptLz4Stream = 2,
    SkippedOrIncompressible = 3,
    InvalidTicket = 4,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompressedPageMeta {
    pub encoding: SlotEncoding,
    pub class: SlabClass,
    pub payload_len: u16,
    pub payload_crc32: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CodecError {
    DeadlineExceeded,
    BackendFailure(String),
    CorruptPayload { page: usize },
    InvalidBatchLength,
    Slab(SlabError),
}

impl From<SlabError> for CodecError {
    fn from(error: SlabError) -> Self {
        Self::Slab(error)
    }
}

/// Single-worker-thread bound hardware codec trait for batch VRAM compression and decode.
pub trait GpuDeviceCodec {
    fn backend_name(&self) -> &'static str;
    /// Bytes allocated on the device for the slab and workspace, when the
    /// implementation can account for both. Unknown is not a measured zero.
    fn device_allocation_bytes(&self) -> Option<(u64, u64)> {
        None
    }
    fn compress_and_pack_batch(
        &mut self,
        pages: &[[u8; PAGE_BYTES]],
        slab_alloc: &mut VramSlabPool,
        out_tickets: &mut [Option<VramSlabTicket>],
    ) -> Result<(), CodecError>;
    fn verify_and_unpack_batch(
        &mut self,
        tickets: &[VramSlabTicket],
        out_pages: &mut [[u8; PAGE_BYTES]],
        out_status: &mut [PageDecodeStatus],
    ) -> Result<(), CodecError>;

    /// Remove a codec-private payload after the caller invalidated its slot.
    fn invalidate(&mut self, _ticket: &VramSlabTicket) {}
}

#[derive(Clone, Copy)]
struct StoredPayload {
    epoch: u64,
    payload_len: u16,
}

/// Host reference codec. It owns the payload bytes so the CUDA/Vulkan codecs
/// can share the exact LZ4+CRC32 semantics without a device round trip.
pub struct ReferenceLz4Codec {
    backing: HostSlabBacking,
    epoch_index: HashMap<(u32, u8), StoredPayload>,
    deadline: Duration,
    force_deadline: bool,
    max_frames: Option<u32>,
}

impl Default for ReferenceLz4Codec {
    fn default() -> Self {
        Self::new()
    }
}

impl ReferenceLz4Codec {
    pub fn new() -> Self {
        Self::with_max_frames(None)
    }

    /// Construct a host reference codec with an optional hard frame cap.
    /// Production callers pass `None` and rely on the enclosing `VramSlabPool`
    /// plus host-memory budget; tests may pass `Some(limit)` to trigger rollback.
    pub fn with_max_frames(max_frames: Option<u32>) -> Self {
        Self {
            backing: HostSlabBacking::new(0).unwrap_or_default(),
            epoch_index: HashMap::new(),
            deadline: CODEC_SUBDEADLINE,
            force_deadline: false,
            max_frames,
        }
    }

    /// Deterministic test hook for the 20 ms deadline path.
    pub fn set_force_deadline(&mut self, force: bool) {
        self.force_deadline = force;
    }

    /// Test/lab hook for the deterministic reference codec. Production GPU
    /// codecs keep the sealed 20 ms deadline.
    pub fn set_deadline(&mut self, deadline: Duration) {
        self.deadline = deadline;
    }

    pub fn payload_len(&self, ticket: &VramSlabTicket) -> Option<usize> {
        let stored = self.epoch_index.get(&(ticket.frame_idx, ticket.sub_slot))?;
        (stored.epoch == ticket.epoch).then_some(usize::from(stored.payload_len))
    }

    /// Test/debug hook: the deterministic reference payload for one slot.
    pub fn payload_mut(&mut self, ticket: &VramSlabTicket) -> Option<&mut [u8]> {
        let stored = self.epoch_index.get(&(ticket.frame_idx, ticket.sub_slot))?;
        if stored.epoch != ticket.epoch || stored.payload_len != ticket.payload_len {
            return None;
        }
        self.backing.bytes_mut(ticket).ok()
    }

    fn check_deadline(&self, started: Instant) -> Result<(), CodecError> {
        if self.force_deadline || started.elapsed() > self.deadline {
            return Err(CodecError::DeadlineExceeded);
        }
        Ok(())
    }

    fn allocate_and_store(
        &mut self,
        page: &[u8; PAGE_BYTES],
        slab: &mut VramSlabPool,
        out: &mut Option<VramSlabTicket>,
    ) -> Result<(), CodecError> {
        let compressed = lz4_compress_block(page);
        let (encoding, class, payload) = match compressed {
            Some(bytes) if bytes.len() <= MAX_COMPRESSED_LEN => (
                SlotEncoding::Lz4,
                SlabClass::select_for_len(bytes.len() as u16),
                bytes,
            ),
            _ => (SlotEncoding::Raw, SlabClass::Raw4096, page.to_vec()),
        };
        let mut ticket = slab.alloc_sub_slot(class)?;
        let crc = crc32(&payload, 0);
        slab.commit(&mut ticket, encoding, payload.len() as u16, crc)?;
        let frame_count = ticket
            .frame_idx
            .checked_add(1)
            .ok_or(CodecError::InvalidBatchLength)?;
        if let Some(max_frames) = self.max_frames
            && frame_count > max_frames
        {
            let _ = slab.invalidate(&ticket);
            return Err(CodecError::BackendFailure(
                "reference codec frame budget exceeded".to_string(),
            ));
        }
        self.backing
            .ensure_frames(frame_count)
            .map_err(CodecError::Slab)?;
        self.backing
            .write(&ticket, &payload)
            .map_err(CodecError::Slab)?;
        self.epoch_index.insert(
            (ticket.frame_idx, ticket.sub_slot),
            StoredPayload {
                epoch: ticket.epoch,
                payload_len: ticket.payload_len,
            },
        );
        *out = Some(ticket);
        Ok(())
    }
}

impl GpuDeviceCodec for ReferenceLz4Codec {
    fn backend_name(&self) -> &'static str {
        "reference-lz4"
    }

    fn invalidate(&mut self, ticket: &VramSlabTicket) {
        self.epoch_index
            .remove(&(ticket.frame_idx, ticket.sub_slot));
    }

    fn compress_and_pack_batch(
        &mut self,
        pages: &[[u8; PAGE_BYTES]],
        slab_alloc: &mut VramSlabPool,
        out_tickets: &mut [Option<VramSlabTicket>],
    ) -> Result<(), CodecError> {
        if pages.len() > MAX_BATCH_PAGES || out_tickets.len() < pages.len() {
            return Err(CodecError::InvalidBatchLength);
        }
        let started = Instant::now();
        let mut committed = Vec::new();
        for (index, page) in pages.iter().enumerate() {
            if let Err(error) = self.check_deadline(started) {
                self.rollback(slab_alloc, &committed);
                out_tickets.fill(None);
                return Err(error);
            }
            let mut ticket = None;
            if let Err(error) = self.allocate_and_store(page, slab_alloc, &mut ticket) {
                self.rollback(slab_alloc, &committed);
                out_tickets.fill(None);
                return Err(error);
            }
            if let Some(ticket) = ticket {
                committed.push(ticket);
                out_tickets[index] = Some(ticket);
            }
        }
        Ok(())
    }

    fn verify_and_unpack_batch(
        &mut self,
        tickets: &[VramSlabTicket],
        out_pages: &mut [[u8; PAGE_BYTES]],
        out_status: &mut [PageDecodeStatus],
    ) -> Result<(), CodecError> {
        if tickets.len() > MAX_BATCH_PAGES
            || out_pages.len() < tickets.len()
            || out_status.len() < tickets.len()
        {
            return Err(CodecError::InvalidBatchLength);
        }
        let started = Instant::now();
        for (index, ticket) in tickets.iter().enumerate() {
            if let Err(error) = self.check_deadline(started) {
                out_status[index] = PageDecodeStatus::SkippedOrIncompressible;
                return Err(error);
            }
            let key = (ticket.frame_idx, ticket.sub_slot);
            let Some(stored) = self.epoch_index.get(&key).copied() else {
                out_status[index] = PageDecodeStatus::InvalidTicket;
                continue;
            };
            if stored.epoch != ticket.epoch || stored.payload_len != ticket.payload_len {
                out_status[index] = PageDecodeStatus::InvalidTicket;
                continue;
            }
            let Ok(payload) = self.backing.read(ticket) else {
                out_status[index] = PageDecodeStatus::InvalidTicket;
                continue;
            };
            if crc32(&payload, 0) != ticket.payload_crc32 {
                out_status[index] = PageDecodeStatus::PreDecodeCrcMismatch;
                continue;
            }
            if ticket.encoding == SlotEncoding::Raw {
                if payload.len() != PAGE_BYTES {
                    out_status[index] = PageDecodeStatus::CorruptLz4Stream;
                    continue;
                }
                out_pages[index].copy_from_slice(&payload);
                out_status[index] = PageDecodeStatus::Ok;
                continue;
            }
            match lz4_decompress_block(&payload, &mut out_pages[index]) {
                Ok(()) => out_status[index] = PageDecodeStatus::Ok,
                Err(Lz4DecodeError) => out_status[index] = PageDecodeStatus::CorruptLz4Stream,
            }
        }
        Ok(())
    }
}

impl ReferenceLz4Codec {
    fn rollback(&mut self, slab: &mut VramSlabPool, tickets: &[VramSlabTicket]) {
        for ticket in tickets {
            self.invalidate(ticket);
            let _ = slab.invalidate(ticket);
        }
    }
}

#[cfg(test)]
mod tests;
