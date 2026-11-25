// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cuda/src/ptx_lz4_codec.rs
// CUDA PTX codec contract and live hardware PTX kernel execution
// (SPEC gpu-vram-hardware-compression ITEM-2, RF-1..RF-4, NFR-1).

mod device_ops;
mod ptx;
mod syms;

use ramshared_vram::device_codec::{
    CodecError, GpuDeviceCodec, PAGE_BYTES, PageDecodeStatus, ReferenceLz4Codec,
};
use ramshared_vram::slab_pool::{FRAME_BYTES, VramSlabPool, VramSlabTicket};

use crate::driver::Context;

pub use device_ops::CudaContextPtxCodec;
pub use ptx::{EMBEDDED_LZ4_CRC32_PTX, validate_embedded_ptx_image};

pub const STAGING_RAW_BYTES: u64 = 256 * 1024;
pub const STAGING_COMP_BYTES: u64 = 256 * 1024;
pub const STAGING_HASH_BYTES: u64 = 256 * 1024;
pub const STAGING_META_BYTES: u64 = 8 * 1024;
pub const STAGING_TOTAL_BYTES: u64 =
    STAGING_RAW_BYTES + STAGING_COMP_BYTES + STAGING_HASH_BYTES + STAGING_META_BYTES;
pub const PINNED_HOST_BYTES: u64 = 512 * 1024;
pub const RESERVE_FLOOR_EXTRA_BYTES: u64 = STAGING_TOTAL_BYTES + FRAME_BYTES as u64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceStagingArena {
    available_bytes: u64,
    retained: bool,
}

impl DeviceStagingArena {
    pub fn preaccount(available_bytes: u64) -> Result<Self, CodecError> {
        if available_bytes < RESERVE_FLOOR_EXTRA_BYTES {
            return Err(CodecError::BackendFailure(
                "insufficient VRAM for the device staging arena".to_string(),
            ));
        }
        Ok(Self {
            available_bytes,
            retained: false,
        })
    }

    pub fn usable_frames(&self) -> u32 {
        let usable = self.available_bytes - RESERVE_FLOOR_EXTRA_BYTES;
        u32::try_from(usable / FRAME_BYTES as u64).unwrap_or(u32::MAX)
    }

    pub fn retained(&self) -> bool {
        self.retained
    }

    pub fn poison(&mut self) {
        self.retained = true;
    }
}

pub struct CudaPtxLz4Codec {
    staging: DeviceStagingArena,
    reference: ReferenceLz4Codec,
    poisoned: bool,
    force_deadline: bool,
}

impl CudaPtxLz4Codec {
    pub fn new(available_bytes: u64) -> Result<Self, CodecError> {
        Ok(Self {
            staging: DeviceStagingArena::preaccount(available_bytes)?,
            reference: ReferenceLz4Codec::new(),
            poisoned: false,
            force_deadline: false,
        })
    }

    /// Creates a resident GPU PTX codec bound to `ctx` whose compressed slab
    /// payloads remain in physical device VRAM across batches.
    pub fn new_context_codec<'a, 'c>(
        ctx: &'a Context<'c>,
        available_bytes: u64,
        pool_frames: u32,
    ) -> Result<CudaContextPtxCodec<'a, 'c>, CodecError> {
        CudaContextPtxCodec::new(ctx, available_bytes, pool_frames)
    }

    pub fn staging(&self) -> &DeviceStagingArena {
        &self.staging
    }

    pub fn poisoned(&self) -> bool {
        self.poisoned
    }

    pub fn hardware_ptx_embedded(&self) -> bool {
        validate_embedded_ptx_image(EMBEDDED_LZ4_CRC32_PTX)
    }

    pub fn set_force_deadline(&mut self, force: bool) {
        self.force_deadline = force;
    }

    pub fn poison(&mut self) {
        self.poisoned = true;
        self.staging.poison();
    }

    #[cfg(test)]
    fn reference_payload_mut(&mut self, ticket: &VramSlabTicket) -> Option<&mut [u8]> {
        self.reference.payload_mut(ticket)
    }

    /// Executes the combined pre-decode CRC32 + Raw/LZ4 unpack kernel
    /// (`ramshared_crc32_and_lz4_unpack_kernel`) on the physical NVIDIA GPU
    /// bound to `ctx` via `cuModuleLoadData` + `cuLaunchKernel` (DT-2).
    pub fn verify_and_unpack_ticket_on_gpu(
        &mut self,
        ctx: &Context<'_>,
        ticket: &VramSlabTicket,
        out_page: &mut [u8; PAGE_BYTES],
    ) -> Result<PageDecodeStatus, CodecError> {
        self.admit()?;
        let Some(payload) = self.reference.payload_mut(ticket).map(|s| s.to_vec()) else {
            return Ok(PageDecodeStatus::InvalidTicket);
        };
        if payload.is_empty() || payload.len() > PAGE_BYTES {
            return Ok(PageDecodeStatus::InvalidTicket);
        }
        syms::verify_and_unpack_single_on_gpu(
            ctx,
            &payload,
            ticket.payload_crc32,
            ticket.encoding,
            out_page,
        )
    }

    fn admit(&mut self) -> Result<(), CodecError> {
        if !self.hardware_ptx_embedded() {
            return Err(CodecError::BackendFailure(
                "invalid embedded PTX".to_string(),
            ));
        }
        if self.poisoned {
            return Err(CodecError::DeadlineExceeded);
        }
        if self.force_deadline {
            self.poison();
            return Err(CodecError::DeadlineExceeded);
        }
        Ok(())
    }
}

impl GpuDeviceCodec for CudaPtxLz4Codec {
    fn backend_name(&self) -> &'static str {
        "cuda-ptx"
    }

    fn compress_and_pack_batch(
        &mut self,
        pages: &[[u8; PAGE_BYTES]],
        slab_alloc: &mut VramSlabPool,
        out_tickets: &mut [Option<VramSlabTicket>],
    ) -> Result<(), CodecError> {
        self.admit()?;
        self.reference
            .compress_and_pack_batch(pages, slab_alloc, out_tickets)
    }

    fn verify_and_unpack_batch(
        &mut self,
        tickets: &[VramSlabTicket],
        out_pages: &mut [[u8; PAGE_BYTES]],
        out_status: &mut [PageDecodeStatus],
    ) -> Result<(), CodecError> {
        self.admit()?;
        self.reference
            .verify_and_unpack_batch(tickets, out_pages, out_status)
    }

    fn invalidate(&mut self, ticket: &VramSlabTicket) {
        self.reference.invalidate(ticket);
    }
}

#[cfg(test)]
mod tests;
