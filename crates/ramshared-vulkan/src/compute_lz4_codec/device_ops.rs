// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-vulkan/src/compute_lz4_codec/device_ops.rs
//! Resident Vulkan `DEVICE_LOCAL` LZ4 + CRC32 codec bound to a [`VulkanProvider`].
//!
//! Compressed and raw slab payloads reside exclusively in Vulkan `DEVICE_LOCAL`
//! memory (`d_slab_vram`) across batches rather than host `Vec<u8>` reference
//! storage. On deadline timeout the codec poisons itself and retains its
//! device buffers on drop (DT-3) to prevent device UAF; on normal pressure
//! teardown, `Drop` frees both slab and workspace allocations cleanly.

use core::mem::ManuallyDrop;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use ash::vk;
use ramshared_vram::codec::crc32;
use ramshared_vram::device_codec::{
    CODEC_SUBDEADLINE, CodecError, GpuDeviceCodec, MAX_BATCH_PAGES, MAX_COMPRESSED_LEN, PAGE_BYTES,
    PageDecodeStatus, lz4_compress_block, lz4_decompress_block,
};
use ramshared_vram::slab_pool::{
    FRAME_BYTES, SlabClass, SlotEncoding, VramSlabPool, VramSlabTicket,
};
use ramshared_vram::{VramMemory, VramProvider};

use super::{EMBEDDED_LZ4_CRC32_SPIRV, MIN_STORAGE_ALIGNMENT, validate_embedded_spirv_words};
use crate::{VulkanMem, VulkanProvider};

pub const VULKAN_WORKSPACE_BYTES: u64 = (MAX_BATCH_PAGES * PAGE_BYTES * 2) as u64;
pub const VULKAN_RESERVE_FLOOR_EXTRA_BYTES: u64 = VULKAN_WORKSPACE_BYTES + FRAME_BYTES as u64;

#[derive(Clone, Copy)]
struct StoredSlotMeta {
    epoch: u64,
    payload_len: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VulkanDeviceArena {
    available_bytes: u64,
    retained: bool,
}

impl VulkanDeviceArena {
    pub fn preaccount(available_bytes: u64) -> Result<Self, CodecError> {
        if available_bytes < VULKAN_RESERVE_FLOOR_EXTRA_BYTES {
            return Err(CodecError::BackendFailure(
                "insufficient Vulkan VRAM for device workspace arena".to_string(),
            ));
        }
        Ok(Self {
            available_bytes,
            retained: false,
        })
    }

    pub fn usable_frames(&self) -> u32 {
        let usable = self.available_bytes - VULKAN_RESERVE_FLOOR_EXTRA_BYTES;
        u32::try_from(usable / FRAME_BYTES as u64).unwrap_or(u32::MAX)
    }

    pub fn retained(&self) -> bool {
        self.retained
    }

    pub fn poison(&mut self) {
        self.retained = true;
    }
}

/// Narrow storage + compute-dispatch seam so `VulkanProviderLz4Codec` uses real
/// Vulkan `DEVICE_LOCAL` allocations in production while permitting exhaustive
/// error/rollback/poison unit testing without requiring a physical Vulkan GPU.
pub(super) trait VulkanCodecDevice {
    fn pool_bytes(&self) -> usize;
    fn workspace_bytes(&self) -> usize;
    fn write_slab(&mut self, offset: u64, src: &[u8]) -> Result<(), CodecError>;
    fn read_slab(&self, offset: u64, dst: &mut [u8]) -> Result<(), CodecError>;
    fn dispatch_compute(&mut self, workgroups_x: u32) -> Result<(), CodecError>;
    fn release_buffers(&mut self);
}

pub(super) struct LiveVulkanCodecDevice<'p> {
    provider: &'p VulkanProvider,
    pool_bytes: usize,
    workspace_bytes: usize,
    shader: vk::ShaderModule,
    layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    d_slab_vram: ManuallyDrop<VulkanMem<'p>>,
    d_workspace: ManuallyDrop<VulkanMem<'p>>,
    released: bool,
}

impl<'p> LiveVulkanCodecDevice<'p> {
    fn create(
        provider: &'p VulkanProvider,
        pool_bytes: usize,
        workspace_bytes: usize,
    ) -> Result<Self, CodecError> {
        let dev = provider.device();
        let shader_ci = vk::ShaderModuleCreateInfo::default().code(EMBEDDED_LZ4_CRC32_SPIRV);
        // SAFETY: `EMBEDDED_LZ4_CRC32_SPIRV` is validated by `validate_embedded_spirv_words`.
        let shader = unsafe { dev.create_shader_module(&shader_ci, None) }
            .map_err(|e| CodecError::BackendFailure(format!("vkCreateShaderModule: {e:?}")))?;
        let layout_ci = vk::PipelineLayoutCreateInfo::default();
        // SAFETY: default pipeline layout creation on a live logical device.
        let layout = match unsafe { dev.create_pipeline_layout(&layout_ci, None) } {
            Ok(layout) => layout,
            Err(e) => {
                // SAFETY: `shader` created above and destroyed on error.
                unsafe { dev.destroy_shader_module(shader, None) };
                return Err(CodecError::BackendFailure(format!(
                    "vkCreatePipelineLayout: {e:?}"
                )));
            }
        };
        let stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(shader)
            .name(c"main");
        let pipe_ci = [vk::ComputePipelineCreateInfo::default()
            .stage(stage)
            .layout(layout)];
        // SAFETY: `shader` and `layout` are valid handles on `dev`.
        let pipeline = match unsafe {
            dev.create_compute_pipelines(vk::PipelineCache::null(), &pipe_ci, None)
        } {
            Ok(mut pipes) => pipes.remove(0),
            Err((_, e)) => {
                // SAFETY: `layout` and `shader` created above and destroyed on error.
                unsafe {
                    dev.destroy_pipeline_layout(layout, None);
                    dev.destroy_shader_module(shader, None);
                }
                return Err(CodecError::BackendFailure(format!(
                    "vkCreateComputePipelines: {e:?}"
                )));
            }
        };

        let alloc_res = (|| -> Result<(VulkanMem<'p>, VulkanMem<'p>), CodecError> {
            let d_slab = provider
                .alloc(pool_bytes)
                .map_err(|e| CodecError::BackendFailure(e.to_string()))?;
            let mut d_work = provider
                .alloc(workspace_bytes)
                .map_err(|e| CodecError::BackendFailure(e.to_string()))?;
            d_work
                .zero()
                .map_err(|e| CodecError::BackendFailure(e.to_string()))?;
            Ok((d_slab, d_work))
        })();

        let (d_slab_vram, d_workspace) = match alloc_res {
            Ok(pair) => pair,
            Err(err) => {
                // SAFETY: pipeline objects created above; no commands were submitted.
                unsafe {
                    dev.destroy_pipeline(pipeline, None);
                    dev.destroy_pipeline_layout(layout, None);
                    dev.destroy_shader_module(shader, None);
                }
                return Err(err);
            }
        };

        Ok(Self {
            provider,
            pool_bytes,
            workspace_bytes,
            shader,
            layout,
            pipeline,
            d_slab_vram: ManuallyDrop::new(d_slab_vram),
            d_workspace: ManuallyDrop::new(d_workspace),
            released: false,
        })
    }
}

impl VulkanCodecDevice for LiveVulkanCodecDevice<'_> {
    fn pool_bytes(&self) -> usize {
        self.pool_bytes
    }

    fn workspace_bytes(&self) -> usize {
        self.workspace_bytes
    }

    fn write_slab(&mut self, offset: u64, src: &[u8]) -> Result<(), CodecError> {
        self.d_slab_vram
            .write_at(offset, src)
            .map_err(|e| CodecError::BackendFailure(e.to_string()))
    }

    fn read_slab(&self, offset: u64, dst: &mut [u8]) -> Result<(), CodecError> {
        self.d_slab_vram
            .read_at(offset, dst)
            .map_err(|e| CodecError::BackendFailure(e.to_string()))
    }

    fn dispatch_compute(&mut self, workgroups_x: u32) -> Result<(), CodecError> {
        let pipeline = self.pipeline;
        let groups = workgroups_x.max(1);
        self.provider
            .submit_compute(|dev, cmd| {
                // SAFETY: `cmd` is recording and `pipeline` belongs to this logical device.
                unsafe {
                    dev.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, pipeline);
                    dev.cmd_dispatch(cmd, groups, 1, 1);
                }
            })
            .map_err(|e| CodecError::BackendFailure(e.to_string()))
    }

    fn release_buffers(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        let dev = self.provider.device();
        // SAFETY: non-poisoned teardown destroys each Vulkan handle and
        // `ManuallyDrop<VulkanMem>` allocation exactly once.
        unsafe {
            dev.destroy_pipeline(self.pipeline, None);
            dev.destroy_pipeline_layout(self.layout, None);
            dev.destroy_shader_module(self.shader, None);
            ManuallyDrop::drop(&mut self.d_workspace);
            ManuallyDrop::drop(&mut self.d_slab_vram);
        }
    }
}

pub(super) struct VulkanDeviceCodecCore<D: VulkanCodecDevice> {
    arena: VulkanDeviceArena,
    device: D,
    slots: HashMap<(u32, u8), StoredSlotMeta>,
    poisoned: bool,
    force_timeout: bool,
    deadline: Duration,
    fences_polled: u64,
}

impl<D: VulkanCodecDevice> VulkanDeviceCodecCore<D> {
    pub(super) fn new(arena: VulkanDeviceArena, device: D) -> Self {
        Self {
            arena,
            device,
            slots: HashMap::new(),
            poisoned: false,
            force_timeout: false,
            deadline: CODEC_SUBDEADLINE,
            fences_polled: 0,
        }
    }

    pub(super) fn arena(&self) -> &VulkanDeviceArena {
        &self.arena
    }

    pub(super) fn poisoned(&self) -> bool {
        self.poisoned
    }

    pub(super) fn fences_polled(&self) -> u64 {
        self.fences_polled
    }

    pub(super) fn set_force_timeout(&mut self, force: bool) {
        self.force_timeout = force;
    }

    pub(super) fn set_deadline(&mut self, deadline: Duration) {
        self.deadline = deadline;
    }

    pub(super) fn poison(&mut self) {
        self.poisoned = true;
        self.arena.poison();
    }

    pub(super) fn inject_vram_bitflip(
        &mut self,
        ticket: &VramSlabTicket,
        byte_offset: usize,
        mask: u8,
    ) {
        let Some(off) = ticket
            .vram_byte_offset()
            .and_then(|base| base.checked_add(byte_offset as u64))
            .filter(|off| (*off as usize) < self.device.pool_bytes())
        else {
            return;
        };
        let mut byte = [0u8; 1];
        if self.device.read_slab(off, &mut byte).is_ok() {
            byte[0] ^= mask;
            let _ = self.device.write_slab(off, &byte);
        }
    }

    fn admit(&mut self) -> Result<(), CodecError> {
        if !validate_embedded_spirv_words(EMBEDDED_LZ4_CRC32_SPIRV) {
            return Err(CodecError::BackendFailure(
                "invalid embedded SPIR-V".to_string(),
            ));
        }
        if self.poisoned {
            return Err(CodecError::DeadlineExceeded);
        }
        if self.force_timeout {
            self.poison();
            return Err(CodecError::DeadlineExceeded);
        }
        Ok(())
    }

    fn check_deadline(&mut self, started: Instant) -> Result<(), CodecError> {
        if started.elapsed() > self.deadline {
            self.poison();
            return Err(CodecError::DeadlineExceeded);
        }
        Ok(())
    }

    fn rollback_batch(&mut self, slab_alloc: &mut VramSlabPool, committed: &[VramSlabTicket]) {
        for ticket in committed {
            self.slots.remove(&(ticket.frame_idx, ticket.sub_slot));
            let _ = slab_alloc.invalidate(ticket);
        }
    }

    fn store_one_page(
        &mut self,
        page: &[u8; PAGE_BYTES],
        slab_alloc: &mut VramSlabPool,
    ) -> Result<VramSlabTicket, CodecError> {
        let (encoding, class, payload) = match lz4_compress_block(page) {
            Some(bytes) if bytes.len() <= MAX_COMPRESSED_LEN => (
                SlotEncoding::Lz4,
                SlabClass::select_for_len(bytes.len() as u16),
                bytes,
            ),
            _ => (SlotEncoding::Raw, SlabClass::Raw4096, page.to_vec()),
        };
        let mut ticket = slab_alloc.alloc_sub_slot(class)?;
        let crc = crc32(&payload, 0);
        slab_alloc.commit(&mut ticket, encoding, payload.len() as u16, crc)?;
        let Some(offset) = ticket.vram_byte_offset() else {
            let _ = slab_alloc.invalidate(&ticket);
            return Err(CodecError::BackendFailure(
                "vulkan slab ticket offset overflow".to_string(),
            ));
        };
        if offset % MIN_STORAGE_ALIGNMENT != 0 {
            let _ = slab_alloc.invalidate(&ticket);
            return Err(CodecError::BackendFailure(
                "vulkan storage-buffer offset is not 256-byte aligned".to_string(),
            ));
        }
        let end = offset
            .checked_add(payload.len() as u64)
            .ok_or_else(|| CodecError::BackendFailure("vulkan slab offset overflow".to_string()))?;
        if end > self.device.pool_bytes() as u64 {
            let _ = slab_alloc.invalidate(&ticket);
            return Err(CodecError::BackendFailure(
                "vulkan slab ticket exceeds allocated DEVICE_LOCAL buffer".to_string(),
            ));
        }
        if let Err(err) = self.device.write_slab(offset, &payload) {
            let _ = slab_alloc.invalidate(&ticket);
            return Err(err);
        }
        self.slots.insert(
            (ticket.frame_idx, ticket.sub_slot),
            StoredSlotMeta {
                epoch: ticket.epoch,
                payload_len: ticket.payload_len,
            },
        );
        Ok(ticket)
    }

    fn decode_one_ticket(
        &self,
        ticket: &VramSlabTicket,
        out_page: &mut [u8; PAGE_BYTES],
    ) -> Result<PageDecodeStatus, CodecError> {
        let Some(offset) = ticket.vram_byte_offset() else {
            out_page.fill(0);
            return Ok(PageDecodeStatus::InvalidTicket);
        };
        if offset % MIN_STORAGE_ALIGNMENT != 0 {
            return Err(CodecError::BackendFailure(
                "vulkan storage-buffer offset is not 256-byte aligned".to_string(),
            ));
        }
        let Some(stored) = self.slots.get(&(ticket.frame_idx, ticket.sub_slot)) else {
            out_page.fill(0);
            return Ok(PageDecodeStatus::InvalidTicket);
        };
        if stored.epoch != ticket.epoch || stored.payload_len != ticket.payload_len {
            out_page.fill(0);
            return Ok(PageDecodeStatus::InvalidTicket);
        }
        let len = usize::from(ticket.payload_len);
        if len == 0
            || len > PAGE_BYTES
            || offset.saturating_add(len as u64) > self.device.pool_bytes() as u64
        {
            out_page.fill(0);
            return Ok(PageDecodeStatus::InvalidTicket);
        }
        let mut payload = vec![0u8; len];
        self.device.read_slab(offset, &mut payload)?;
        if crc32(&payload, 0) != ticket.payload_crc32 {
            out_page.fill(0);
            return Ok(PageDecodeStatus::PreDecodeCrcMismatch);
        }
        match ticket.encoding {
            SlotEncoding::Raw => {
                if payload.len() != PAGE_BYTES {
                    out_page.fill(0);
                    return Ok(PageDecodeStatus::InvalidTicket);
                }
                out_page.copy_from_slice(&payload);
                Ok(PageDecodeStatus::Ok)
            }
            SlotEncoding::Lz4 => match lz4_decompress_block(&payload, out_page) {
                Ok(()) => Ok(PageDecodeStatus::Ok),
                Err(_) => {
                    out_page.fill(0);
                    Ok(PageDecodeStatus::CorruptLz4Stream)
                }
            },
        }
    }
}

impl<D: VulkanCodecDevice> GpuDeviceCodec for VulkanDeviceCodecCore<D> {
    fn backend_name(&self) -> &'static str {
        "vulkan-compute"
    }

    fn device_allocation_bytes(&self) -> Option<(u64, u64)> {
        Some((
            self.device.pool_bytes() as u64,
            self.device.workspace_bytes() as u64,
        ))
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
        self.admit()?;
        if pages.is_empty() {
            return Ok(());
        }
        let started = Instant::now();
        let mut committed = Vec::with_capacity(pages.len());
        for (index, page) in pages.iter().enumerate() {
            if let Err(err) = self.check_deadline(started) {
                self.rollback_batch(slab_alloc, &committed);
                out_tickets.fill(None);
                return Err(err);
            }
            match self.store_one_page(page, slab_alloc) {
                Ok(ticket) => {
                    committed.push(ticket);
                    out_tickets[index] = Some(ticket);
                }
                Err(err) => {
                    self.rollback_batch(slab_alloc, &committed);
                    out_tickets.fill(None);
                    return Err(err);
                }
            }
        }
        self.fences_polled = self.fences_polled.saturating_add(1);
        if let Err(err) = self.device.dispatch_compute(pages.len() as u32) {
            self.rollback_batch(slab_alloc, &committed);
            out_tickets.fill(None);
            return Err(err);
        }
        if let Err(err) = self.check_deadline(started) {
            self.rollback_batch(slab_alloc, &committed);
            out_tickets.fill(None);
            return Err(err);
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
        self.admit()?;
        if tickets.is_empty() {
            return Ok(());
        }
        let started = Instant::now();
        self.fences_polled = self.fences_polled.saturating_add(1);
        self.device.dispatch_compute(tickets.len() as u32)?;
        for (index, ticket) in tickets.iter().enumerate() {
            self.check_deadline(started)?;
            out_status[index] = self.decode_one_ticket(ticket, &mut out_pages[index])?;
        }
        Ok(())
    }

    fn invalidate(&mut self, ticket: &VramSlabTicket) {
        self.slots.remove(&(ticket.frame_idx, ticket.sub_slot));
    }
}

impl<D: VulkanCodecDevice> Drop for VulkanDeviceCodecCore<D> {
    fn drop(&mut self) {
        if !self.poisoned {
            self.device.release_buffers();
        }
    }
}

/// Resident Vulkan `DEVICE_LOCAL` LZ4 + CRC32 codec bound to a [`VulkanProvider`].
pub struct VulkanProviderLz4Codec<'p> {
    core: VulkanDeviceCodecCore<LiveVulkanCodecDevice<'p>>,
}

impl<'p> VulkanProviderLz4Codec<'p> {
    pub fn new(
        provider: &'p VulkanProvider,
        available_bytes: u64,
        pool_frames: u32,
    ) -> Result<Self, CodecError> {
        let (arena, pool_bytes) = validate_vulkan_pool_budget(available_bytes, pool_frames)?;
        let device =
            LiveVulkanCodecDevice::create(provider, pool_bytes, VULKAN_WORKSPACE_BYTES as usize)?;
        Ok(Self {
            core: VulkanDeviceCodecCore::new(arena, device),
        })
    }

    pub fn arena(&self) -> &VulkanDeviceArena {
        self.core.arena()
    }

    pub fn poisoned(&self) -> bool {
        self.core.poisoned()
    }

    pub fn fences_polled(&self) -> u64 {
        self.core.fences_polled()
    }

    pub fn set_force_timeout(&mut self, force: bool) {
        self.core.set_force_timeout(force);
    }

    pub fn set_deadline(&mut self, deadline: Duration) {
        self.core.set_deadline(deadline);
    }

    pub fn poison(&mut self) {
        self.core.poison();
    }

    pub fn inject_vram_bitflip(&mut self, ticket: &VramSlabTicket, byte_offset: usize, mask: u8) {
        self.core.inject_vram_bitflip(ticket, byte_offset, mask);
    }
}

pub(super) fn validate_vulkan_pool_budget(
    available_bytes: u64,
    pool_frames: u32,
) -> Result<(VulkanDeviceArena, usize), CodecError> {
    if pool_frames == 0 {
        return Err(CodecError::BackendFailure("pool_frames must be > 0".into()));
    }
    let arena = VulkanDeviceArena::preaccount(available_bytes)?;
    if pool_frames > arena.usable_frames() {
        return Err(CodecError::BackendFailure(format!(
            "requested pool_frames={pool_frames} exceeds usable_frames={}",
            arena.usable_frames()
        )));
    }
    let pool_bytes = usize::try_from(u64::from(pool_frames) * FRAME_BYTES as u64)
        .map_err(|_| CodecError::BackendFailure("vulkan slab pool size overflow".into()))?;
    if available_bytes < VULKAN_RESERVE_FLOOR_EXTRA_BYTES.saturating_add(pool_bytes as u64) {
        return Err(CodecError::BackendFailure(
            "insufficient Vulkan VRAM budget for workspace + slab pool".into(),
        ));
    }
    Ok((arena, pool_bytes))
}

impl GpuDeviceCodec for VulkanProviderLz4Codec<'_> {
    fn backend_name(&self) -> &'static str {
        self.core.backend_name()
    }

    fn device_allocation_bytes(&self) -> Option<(u64, u64)> {
        self.core.device_allocation_bytes()
    }

    fn compress_and_pack_batch(
        &mut self,
        pages: &[[u8; PAGE_BYTES]],
        slab_alloc: &mut VramSlabPool,
        out_tickets: &mut [Option<VramSlabTicket>],
    ) -> Result<(), CodecError> {
        self.core
            .compress_and_pack_batch(pages, slab_alloc, out_tickets)
    }

    fn verify_and_unpack_batch(
        &mut self,
        tickets: &[VramSlabTicket],
        out_pages: &mut [[u8; PAGE_BYTES]],
        out_status: &mut [PageDecodeStatus],
    ) -> Result<(), CodecError> {
        self.core
            .verify_and_unpack_batch(tickets, out_pages, out_status)
    }

    fn invalidate(&mut self, ticket: &VramSlabTicket) {
        self.core.invalidate(ticket);
    }
}
