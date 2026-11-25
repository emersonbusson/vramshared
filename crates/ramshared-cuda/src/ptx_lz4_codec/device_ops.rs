// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cuda/src/ptx_lz4_codec/device_ops.rs
// Resident CUDA PTX codec implementation bound to an active `Context`, keeping
// compressed slab payloads in physical GPU VRAM across batches with a 20 ms
// `cuEventQuery` completion deadline and DT-3 non-free retention on poison.

use core::ffi::c_void;
use core::mem::ManuallyDrop;
use std::collections::HashMap;
use std::time::Duration;

use ramshared_vram::device_codec::{
    CODEC_SUBDEADLINE, CodecError, GpuDeviceCodec, MAX_BATCH_PAGES, PAGE_BYTES, PageDecodeStatus,
};
use ramshared_vram::slab_pool::{FRAME_BYTES, SlotEncoding, VramSlabPool, VramSlabTicket};

use crate::driver::{Context, CudaError, DeviceMem};
use crate::ffi::CUDA_SUCCESS;

use super::syms::{
    CuEvent, CuFunction, CuModule, ENCODING_LZ4, ENCODING_RAW, KernelSyms, ScopedCuEvent,
    get_kernel_fn, launch_and_poll_cuda_event, load_ptx_module, pack_dtod_slab_slot,
    read_u32_array, write_u32_array, write_u64_array,
};
use super::{DeviceStagingArena, RESERVE_FLOOR_EXTRA_BYTES, STAGING_META_BYTES};

const META_OFFSETS_BYTE_OFFSET: usize = 0;
const META_LENS_BYTE_OFFSET: usize = MAX_BATCH_PAGES * 8;
const META_CRCS_BYTE_OFFSET: usize = META_LENS_BYTE_OFFSET + MAX_BATCH_PAGES * 4;
const META_ENCS_BYTE_OFFSET: usize = META_CRCS_BYTE_OFFSET + MAX_BATCH_PAGES * 4;
const META_STATUS_BYTE_OFFSET: usize = META_ENCS_BYTE_OFFSET + MAX_BATCH_PAGES * 4;

#[derive(Clone, Copy)]
struct StoredSlotMeta {
    epoch: u64,
    payload_len: u16,
}

/// Hardware GPU LZ4 + CRC32 codec backed by a resident CUDA `Context`.
///
/// Payloads live exclusively in device VRAM inside `d_slab_vram` across batches;
/// staging buffers and the compiled PTX module are allocated once upfront. On
/// timeout past the 20 ms `cuEventQuery` deadline, the codec poisons itself and
/// intentionally retains VRAM buffers in `Drop` (DT-3) to prevent GPU UAF.
pub struct CudaContextPtxCodec<'a, 'c> {
    staging: DeviceStagingArena,
    pool_bytes: usize,
    slots: HashMap<(u32, u8), StoredSlotMeta>,
    poisoned: bool,
    force_deadline: bool,
    deadline: Duration,
    kernels_launched: u64,
    events_polled: u64,
    ctx: &'a Context<'c>,
    syms: KernelSyms,
    module: CuModule,
    fn_batch_compress: CuFunction,
    fn_batch_unpack: CuFunction,
    done_event: CuEvent,
    d_slab_vram: ManuallyDrop<DeviceMem<'a, 'c>>,
    d_raw: ManuallyDrop<DeviceMem<'a, 'c>>,
    d_comp: ManuallyDrop<DeviceMem<'a, 'c>>,
    d_hash: ManuallyDrop<DeviceMem<'a, 'c>>,
    d_meta: ManuallyDrop<DeviceMem<'a, 'c>>,
}

impl<'a, 'c> CudaContextPtxCodec<'a, 'c> {
    /// Initializes the resident CUDA PTX codec, pre-allocating staging and
    /// device slab VRAM (`pool_frames * FRAME_BYTES`).
    pub fn new(
        ctx: &'a Context<'c>,
        available_bytes: u64,
        pool_frames: u32,
    ) -> Result<Self, CodecError> {
        let syms = KernelSyms::load(ctx)?;
        Self::init_with_syms(ctx, available_bytes, pool_frames, syms)
    }

    #[cfg(test)]
    pub(super) fn new_with_syms(
        ctx: &'a Context<'c>,
        available_bytes: u64,
        pool_frames: u32,
        syms: KernelSyms,
    ) -> Result<Self, CodecError> {
        Self::init_with_syms(ctx, available_bytes, pool_frames, syms)
    }

    fn init_with_syms(
        ctx: &'a Context<'c>,
        available_bytes: u64,
        pool_frames: u32,
        syms: KernelSyms,
    ) -> Result<Self, CodecError> {
        if pool_frames == 0 {
            return Err(CodecError::BackendFailure("pool_frames must be > 0".into()));
        }
        let staging = DeviceStagingArena::preaccount(available_bytes)?;
        if pool_frames > staging.usable_frames() {
            return Err(CodecError::BackendFailure(format!(
                "requested pool_frames={pool_frames} exceeds usable_frames={}",
                staging.usable_frames()
            )));
        }
        let pool_bytes = usize::try_from(u64::from(pool_frames) * FRAME_BYTES as u64)
            .map_err(|_| CodecError::BackendFailure("slab pool size overflow".into()))?;
        if available_bytes < RESERVE_FLOOR_EXTRA_BYTES.saturating_add(pool_bytes as u64) {
            return Err(CodecError::BackendFailure(
                "insufficient VRAM budget for staging + slab pool".into(),
            ));
        }
        let mut scoped_mod = load_ptx_module(&syms)?;
        let fn_batch_compress = get_kernel_fn(
            &syms,
            scoped_mod.module,
            c"ramshared_batch_lz4_compress_crc32_kernel",
        )?;
        let fn_batch_unpack = get_kernel_fn(
            &syms,
            scoped_mod.module,
            c"ramshared_batch_verify_and_unpack_kernel",
        )?;

        let mut raw_event: CuEvent = core::ptr::null_mut();
        // SAFETY: `raw_event` is a valid out-pointer and CUDA context is current.
        let rc = unsafe { (syms.event_create)(&mut raw_event, 0) };
        if rc != CUDA_SUCCESS {
            return Err(CodecError::BackendFailure(format!(
                "cuEventCreate failed: {rc}"
            )));
        }
        let mut scoped_event = ScopedCuEvent {
            event: raw_event,
            destroy: syms.event_destroy,
        };

        let map_err = |e: CudaError| CodecError::BackendFailure(e.to_string());
        let batch_bytes = MAX_BATCH_PAGES * PAGE_BYTES;
        let d_slab_vram = ctx.alloc(pool_bytes).map_err(map_err)?;
        let d_raw = ctx.alloc(batch_bytes).map_err(map_err)?;
        let d_comp = ctx.alloc(batch_bytes).map_err(map_err)?;
        let d_hash = ctx.alloc(batch_bytes).map_err(map_err)?;
        let mut d_meta = ctx.alloc(STAGING_META_BYTES as usize).map_err(map_err)?;
        d_meta.zero().map_err(map_err)?;

        let module = scoped_mod.module;
        scoped_mod.module = core::ptr::null_mut();
        let done_event = scoped_event.event;
        scoped_event.event = core::ptr::null_mut();

        Ok(Self {
            staging,
            pool_bytes,
            slots: HashMap::new(),
            poisoned: false,
            force_deadline: false,
            deadline: CODEC_SUBDEADLINE,
            kernels_launched: 0,
            events_polled: 0,
            ctx,
            syms,
            module,
            fn_batch_compress,
            fn_batch_unpack,
            done_event,
            d_slab_vram: ManuallyDrop::new(d_slab_vram),
            d_raw: ManuallyDrop::new(d_raw),
            d_comp: ManuallyDrop::new(d_comp),
            d_hash: ManuallyDrop::new(d_hash),
            d_meta: ManuallyDrop::new(d_meta),
        })
    }

    pub fn staging(&self) -> &DeviceStagingArena {
        &self.staging
    }

    pub fn poisoned(&self) -> bool {
        self.poisoned
    }

    pub fn kernels_launched(&self) -> u64 {
        self.kernels_launched
    }

    pub fn events_polled(&self) -> u64 {
        self.events_polled
    }

    pub fn set_force_deadline(&mut self, force: bool) {
        self.force_deadline = force;
    }

    pub fn set_deadline(&mut self, deadline: Duration) {
        self.deadline = deadline;
    }

    pub fn poison(&mut self) {
        self.poisoned = true;
        self.staging.poison();
    }

    /// XOR-flips one byte inside the physical VRAM slab buffer to verify
    /// in-GPU pre-decode CRC32 tamper detection in hardware tests.
    pub fn inject_vram_bitflip(&mut self, ticket: &VramSlabTicket, byte_offset: usize, mask: u8) {
        let Some(off) = ticket
            .vram_byte_offset()
            .and_then(|base| usize::try_from(base).ok())
            .and_then(|base| base.checked_add(byte_offset))
            .filter(|off| *off < self.pool_bytes)
        else {
            return;
        };
        let mut byte = [0u8; 1];
        if self.d_slab_vram.read_at(off, &mut byte).is_ok() {
            byte[0] ^= mask;
            let _ = self.d_slab_vram.write_at(off, &byte);
        }
    }

    fn admit(&mut self) -> Result<(), CodecError> {
        if self.poisoned {
            return Err(CodecError::DeadlineExceeded);
        }
        if self.force_deadline {
            self.poison();
            return Err(CodecError::DeadlineExceeded);
        }
        Ok(())
    }

    fn launch_and_poll_event(
        &mut self,
        func: CuFunction,
        grid_x: u32,
        params: &mut [*mut c_void],
    ) -> Result<(), CodecError> {
        self.kernels_launched = self.kernels_launched.saturating_add(1);
        match launch_and_poll_cuda_event(
            &self.syms,
            func,
            self.done_event,
            grid_x,
            params,
            self.deadline,
        ) {
            Ok(polls) => {
                self.events_polled = self.events_polled.saturating_add(polls);
                Ok(())
            }
            Err(err) => {
                self.poison();
                Err(err)
            }
        }
    }

    fn rollback(&mut self, slab_alloc: &mut VramSlabPool, tickets: &[VramSlabTicket]) {
        for ticket in tickets {
            self.slots.remove(&(ticket.frame_idx, ticket.sub_slot));
            let _ = slab_alloc.invalidate(ticket);
        }
    }
}

impl Drop for CudaContextPtxCodec<'_, '_> {
    fn drop(&mut self) {
        if self.poisoned {
            // DT-3: keep device buffers, module, and event intact after timeout.
            return;
        }
        // SAFETY: not poisoned -> no kernel is in flight using these VRAM resources.
        unsafe {
            ManuallyDrop::drop(&mut self.d_meta);
            ManuallyDrop::drop(&mut self.d_hash);
            ManuallyDrop::drop(&mut self.d_comp);
            ManuallyDrop::drop(&mut self.d_raw);
            ManuallyDrop::drop(&mut self.d_slab_vram);
            if !self.done_event.is_null() {
                let _ = (self.syms.event_destroy)(self.done_event);
            }
            if !self.module.is_null() {
                let _ = (self.syms.module_unload)(self.module);
            }
        }
        let _ = self.ctx;
    }
}

impl GpuDeviceCodec for CudaContextPtxCodec<'_, '_> {
    fn device_allocation_bytes(&self) -> Option<(u64, u64)> {
        Some((self.pool_bytes as u64, super::STAGING_TOTAL_BYTES))
    }
    fn backend_name(&self) -> &'static str {
        "cuda-ptx-gpu"
    }

    fn invalidate(&mut self, ticket: &VramSlabTicket) {
        self.slots.remove(&(ticket.frame_idx, ticket.sub_slot));
    }

    fn compress_and_pack_batch(
        &mut self,
        pages: &[[u8; PAGE_BYTES]],
        slab_alloc: &mut VramSlabPool,
        out_tickets: &mut [Option<VramSlabTicket>],
    ) -> Result<(), CodecError> {
        self.admit()?;
        if pages.len() > MAX_BATCH_PAGES || out_tickets.len() < pages.len() {
            return Err(CodecError::InvalidBatchLength);
        }
        out_tickets.fill(None);
        if pages.is_empty() {
            return Ok(());
        }
        let n = pages.len();
        let mut flat = Vec::with_capacity(n * PAGE_BYTES);
        for page in pages {
            flat.extend_from_slice(page);
        }
        self.d_raw
            .write_at(0, &flat)
            .map_err(|e| CodecError::BackendFailure(e.to_string()))?;

        let meta_base = self.d_meta.device_ptr();
        let (mut src_ptr, mut dst_ptr, mut hash_ptr) = (
            self.d_raw.device_ptr(),
            self.d_comp.device_ptr(),
            self.d_hash.device_ptr(),
        );
        let (mut out_lens_ptr, mut out_crcs_ptr, mut out_encs_ptr) = (
            meta_base + META_LENS_BYTE_OFFSET as u64,
            meta_base + META_CRCS_BYTE_OFFSET as u64,
            meta_base + META_ENCS_BYTE_OFFSET as u64,
        );
        let mut params: [*mut c_void; 6] = [
            (&mut src_ptr as *mut u64).cast(),
            (&mut dst_ptr as *mut u64).cast(),
            (&mut hash_ptr as *mut u64).cast(),
            (&mut out_lens_ptr as *mut u64).cast(),
            (&mut out_crcs_ptr as *mut u64).cast(),
            (&mut out_encs_ptr as *mut u64).cast(),
        ];
        self.launch_and_poll_event(self.fn_batch_compress, n as u32, &mut params)?;

        let lens = read_u32_array(&self.d_meta, META_LENS_BYTE_OFFSET, n)?;
        let crcs = read_u32_array(&self.d_meta, META_CRCS_BYTE_OFFSET, n)?;
        let encs = read_u32_array(&self.d_meta, META_ENCS_BYTE_OFFSET, n)?;

        let mut committed = Vec::with_capacity(n);
        for i in 0..n {
            match pack_dtod_slab_slot(
                self.syms.memcpy_dtod,
                self.d_comp.device_ptr(),
                self.d_slab_vram.device_ptr(),
                self.pool_bytes,
                i,
                encs[i],
                lens[i],
                crcs[i],
                slab_alloc,
            ) {
                Ok(ticket) => {
                    self.slots.insert(
                        (ticket.frame_idx, ticket.sub_slot),
                        StoredSlotMeta {
                            epoch: ticket.epoch,
                            payload_len: ticket.payload_len,
                        },
                    );
                    committed.push(ticket);
                    out_tickets[i] = Some(ticket);
                }
                Err(err) => {
                    self.rollback(slab_alloc, &committed);
                    out_tickets.fill(None);
                    return Err(err);
                }
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
        self.admit()?;
        if tickets.len() > MAX_BATCH_PAGES
            || out_pages.len() < tickets.len()
            || out_status.len() < tickets.len()
        {
            return Err(CodecError::InvalidBatchLength);
        }
        let n = tickets.len();
        if n == 0 {
            return Ok(());
        }

        let mut valid = vec![false; n];
        let mut offsets = vec![0u64; n];
        let mut lens = vec![0u32; n];
        let mut crcs = vec![0u32; n];
        let mut encs = vec![0u32; n];
        let mut any_valid = false;

        for (i, ticket) in tickets.iter().enumerate() {
            let stored = self
                .slots
                .get(&(ticket.frame_idx, ticket.sub_slot))
                .copied();
            let offset_ok = ticket.vram_byte_offset().filter(|&off| {
                off.checked_add(u64::from(ticket.payload_len))
                    .is_some_and(|end| end <= self.pool_bytes as u64)
            });
            match (stored, offset_ok) {
                (Some(meta), Some(off))
                    if meta.epoch == ticket.epoch
                        && meta.payload_len == ticket.payload_len
                        && ticket.payload_len > 0 =>
                {
                    valid[i] = true;
                    any_valid = true;
                    offsets[i] = off;
                    lens[i] = u32::from(ticket.payload_len);
                    crcs[i] = ticket.payload_crc32;
                    encs[i] = match ticket.encoding {
                        SlotEncoding::Raw => ENCODING_RAW,
                        SlotEncoding::Lz4 => ENCODING_LZ4,
                    };
                }
                _ => out_status[i] = PageDecodeStatus::InvalidTicket,
            }
        }
        if !any_valid {
            return Ok(());
        }

        write_u64_array(&mut self.d_meta, META_OFFSETS_BYTE_OFFSET, &offsets)?;
        write_u32_array(&mut self.d_meta, META_LENS_BYTE_OFFSET, &lens)?;
        write_u32_array(&mut self.d_meta, META_CRCS_BYTE_OFFSET, &crcs)?;
        write_u32_array(&mut self.d_meta, META_ENCS_BYTE_OFFSET, &encs)?;
        let zero_status = vec![0u8; n];
        self.d_meta
            .write_at(META_STATUS_BYTE_OFFSET, &zero_status)
            .map_err(|e| CodecError::BackendFailure(e.to_string()))?;

        let meta_base = self.d_meta.device_ptr();
        let (mut slab_ptr, mut pool_bytes_u64) =
            (self.d_slab_vram.device_ptr(), self.pool_bytes as u64);
        let (mut offs_ptr, mut lens_ptr, mut crcs_ptr, mut encs_ptr) = (
            meta_base + META_OFFSETS_BYTE_OFFSET as u64,
            meta_base + META_LENS_BYTE_OFFSET as u64,
            meta_base + META_CRCS_BYTE_OFFSET as u64,
            meta_base + META_ENCS_BYTE_OFFSET as u64,
        );
        let (mut dst_ptr, mut dst_cap, mut status_ptr) = (
            self.d_raw.device_ptr(),
            PAGE_BYTES as u32,
            meta_base + META_STATUS_BYTE_OFFSET as u64,
        );
        let mut params: [*mut c_void; 9] = [
            (&mut slab_ptr as *mut u64).cast(),
            (&mut pool_bytes_u64 as *mut u64).cast(),
            (&mut offs_ptr as *mut u64).cast(),
            (&mut lens_ptr as *mut u64).cast(),
            (&mut crcs_ptr as *mut u64).cast(),
            (&mut encs_ptr as *mut u64).cast(),
            (&mut dst_ptr as *mut u64).cast(),
            (&mut dst_cap as *mut u32).cast(),
            (&mut status_ptr as *mut u64).cast(),
        ];
        self.launch_and_poll_event(self.fn_batch_unpack, n as u32, &mut params)?;

        let mut statuses = vec![0u8; n];
        self.d_meta
            .read_at(META_STATUS_BYTE_OFFSET, &mut statuses)
            .map_err(|e| CodecError::BackendFailure(e.to_string()))?;
        let mut flat = vec![0u8; n * PAGE_BYTES];
        self.d_raw
            .read_at(0, &mut flat)
            .map_err(|e| CodecError::BackendFailure(e.to_string()))?;

        for i in 0..n {
            if !valid[i] {
                out_status[i] = PageDecodeStatus::InvalidTicket;
                continue;
            }
            out_status[i] = match statuses[i] {
                0 => {
                    out_pages[i].copy_from_slice(&flat[i * PAGE_BYTES..(i + 1) * PAGE_BYTES]);
                    PageDecodeStatus::Ok
                }
                1 => PageDecodeStatus::PreDecodeCrcMismatch,
                2 => PageDecodeStatus::CorruptLz4Stream,
                _ => PageDecodeStatus::InvalidTicket,
            };
        }
        Ok(())
    }
}
