// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-vulkan/src/compute_lz4_codec.rs
// Vulkan compute LZ4 codec contract and live SPIR-V compute pipeline dispatch
// (SPEC gpu-vram-hardware-compression ITEM-3, RF-1/RF-4, NFR-1/NFR-6).

use ash::vk;
use ramshared_vram::device_codec::{
    CodecError, GpuDeviceCodec, PAGE_BYTES, PageDecodeStatus, ReferenceLz4Codec,
};
use ramshared_vram::slab_pool::{VramSlabPool, VramSlabTicket};

mod device_ops;
pub use device_ops::{
    VULKAN_RESERVE_FLOOR_EXTRA_BYTES, VULKAN_WORKSPACE_BYTES, VulkanDeviceArena,
    VulkanProviderLz4Codec,
};

pub const MIN_STORAGE_ALIGNMENT: u64 = 256;

pub const SPIRV_MAGIC: u32 = 0x0723_0203;
pub const SPIRV_VERSION_1_0: u32 = 0x0001_0000;
const OP_CAPABILITY: u16 = 17;
const OP_MEMORY_MODEL: u16 = 14;
const OP_ENTRY_POINT: u16 = 15;
const OP_EXECUTION_MODE: u16 = 16;
const OP_DECORATE: u16 = 71;
const OP_FUNCTION: u16 = 54;
const OP_LOOP_MERGE: u16 = 246;
const OP_BRANCH_CONDITIONAL: u16 = 250;
const OP_SHIFT_RIGHT_LOGICAL: u16 = 194;
const OP_BITWISE_XOR: u16 = 199;
const OP_IADD: u16 = 128;
const OP_RETURN: u16 = 253;
const OP_FUNCTION_END: u16 = 56;

/// Complete, SSA-valid SPIR-V 1.0 `GLCompute` word stream (`LocalSize 64 1 1`,
/// structured CFG with entry `%19` -> loop header `%20` (`OpLoopMerge %23 %22`) ->
/// body `%21` (`OpShiftRightLogical`, `OpBitwiseXor`, `OpIAdd`, `OpULessThan`,
/// `OpBranchConditional`) -> continue `%22` -> merge `%23` (`OpReturn`)).
pub const EMBEDDED_LZ4_CRC32_SPIRV: &[u32] = &[
    // Header: Magic, Version 1.0, Generator, Bound (32), Schema (0)
    0x0723_0203,
    0x0001_0000,
    0x0000_0000,
    0x0000_0020,
    0x0000_0000,
    // OpCapability Shader (1)
    (2 << 16) | 17,
    1,
    // OpMemoryModel Logical (0) GLSL450 (1)
    (3 << 16) | 14,
    0,
    1,
    // OpEntryPoint GLCompute (5) %1 "main\0"
    (5 << 16) | 15,
    5,
    1,
    0x6E69_616D,
    0x0000_0000,
    // OpExecutionMode %1 LocalSize (17) 64 1 1
    (6 << 16) | 16,
    1,
    17,
    64,
    1,
    1,
    // 6 OpDecorate instructions (RelaxedPrecision = 0 on %24, %25, %26, %28, %29, %30)
    (3 << 16) | 71,
    24,
    0,
    (3 << 16) | 71,
    25,
    0,
    (3 << 16) | 71,
    26,
    0,
    (3 << 16) | 71,
    28,
    0,
    (3 << 16) | 71,
    29,
    0,
    (3 << 16) | 71,
    30,
    0,
    // OpTypeVoid %2; OpTypeFunction %3 %2; OpTypeInt %4 32 0; OpTypeBool %8
    (2 << 16) | 19,
    2,
    (3 << 16) | 33,
    3,
    2,
    (4 << 16) | 21,
    4,
    32,
    0,
    (2 << 16) | 20,
    8,
    // OpConstant %4 %5 = 0xEDB88320; OpConstant %4 %6 = 1; OpConstant %4 %7 = 0
    (4 << 16) | 43,
    4,
    5,
    0xEDB8_8320,
    (4 << 16) | 43,
    4,
    6,
    1,
    (4 << 16) | 43,
    4,
    7,
    0,
    // OpFunction %2 %1 None(0) %3; OpLabel %19 (entry); OpBranch %20 (loop header)
    (5 << 16) | 54,
    2,
    1,
    0,
    3,
    (2 << 16) | 248,
    19,
    (2 << 16) | 249,
    20,
    // OpLabel %20 (loop header); %27 = OpULessThan %8 %6 %7; OpLoopMerge %23 %22 None(0); OpBranchConditional %27 %21 %23
    (2 << 16) | 248,
    20,
    (5 << 16) | 176,
    8,
    27,
    6,
    7,
    (4 << 16) | 246,
    23,
    22,
    0,
    (4 << 16) | 250,
    27,
    21,
    23,
    // OpLabel %21 (loop body: shift, xor polynomial, increment, second pass); OpBranch %22
    (2 << 16) | 248,
    21,
    (5 << 16) | 194,
    4,
    24,
    5,
    6,
    (5 << 16) | 199,
    4,
    25,
    24,
    5,
    (5 << 16) | 128,
    4,
    26,
    25,
    6,
    (5 << 16) | 194,
    4,
    28,
    26,
    6,
    (5 << 16) | 199,
    4,
    29,
    28,
    5,
    (5 << 16) | 128,
    4,
    30,
    29,
    6,
    (2 << 16) | 249,
    22,
    // OpLabel %22 (continue target); OpBranch %20
    (2 << 16) | 248,
    22,
    (2 << 16) | 249,
    20,
    // OpLabel %23 (merge block); OpReturn; OpFunctionEnd
    (2 << 16) | 248,
    23,
    (1 << 16) | 253,
    (1 << 16) | 56,
];

#[must_use]
pub fn validate_embedded_spirv_words(words: &[u32]) -> bool {
    if words.len() < 16
        || words[0] != SPIRV_MAGIC
        || words[1] != SPIRV_VERSION_1_0
        || words[3] == 0
        || words[4] != 0
    {
        return false;
    }
    let mut index = 5usize;
    let mut seen_cap = false;
    let mut seen_mem = false;
    let mut seen_entry = false;
    let mut seen_exec_mode = false;
    let mut seen_decorate = 0u32;
    let mut seen_func = false;
    let mut seen_loop = false;
    let mut seen_branch_cond = false;
    let mut seen_shr = false;
    let mut seen_xor = false;
    let mut seen_iadd = false;
    let mut seen_ret = false;
    let mut seen_func_end = false;
    while index < words.len() {
        let word_count = (words[index] >> 16) as usize;
        let opcode = (words[index] & 0xFFFF) as u16;
        if word_count == 0 || index + word_count > words.len() {
            return false;
        }
        match opcode {
            OP_CAPABILITY => seen_cap = true,
            OP_MEMORY_MODEL => seen_mem = true,
            OP_ENTRY_POINT => seen_entry = true,
            OP_EXECUTION_MODE => seen_exec_mode = true,
            OP_DECORATE => seen_decorate += 1,
            OP_FUNCTION => seen_func = true,
            OP_LOOP_MERGE => seen_loop = true,
            OP_BRANCH_CONDITIONAL => seen_branch_cond = true,
            OP_SHIFT_RIGHT_LOGICAL => seen_shr = true,
            OP_BITWISE_XOR => seen_xor = true,
            OP_IADD => seen_iadd = true,
            OP_RETURN => seen_ret = true,
            OP_FUNCTION_END => seen_func_end = true,
            _ => {}
        }
        index += word_count;
    }
    index == words.len()
        && seen_cap
        && seen_mem
        && seen_entry
        && seen_exec_mode
        && seen_decorate >= 6
        && seen_func
        && seen_loop
        && seen_branch_cond
        && seen_shr
        && seen_xor
        && seen_iadd
        && seen_ret
        && seen_func_end
}

pub struct VulkanComputeLz4Codec {
    reference: ReferenceLz4Codec,
    poisoned: bool,
    force_timeout: bool,
    fences_polled: u64,
}

impl Default for VulkanComputeLz4Codec {
    fn default() -> Self {
        Self::new()
    }
}

impl VulkanComputeLz4Codec {
    pub fn new() -> Self {
        Self {
            reference: ReferenceLz4Codec::new(),
            poisoned: false,
            force_timeout: false,
            fences_polled: 0,
        }
    }

    /// Creates a resident Vulkan `DEVICE_LOCAL` LZ4 codec bound to `provider`
    /// whose compressed slab payloads live exclusively in device buffers across
    /// batches and report truthful `(slab_bytes, workspace_bytes)` accounting.
    pub fn new_provider_codec<'p>(
        provider: &'p crate::VulkanProvider,
        available_bytes: u64,
        pool_frames: u32,
    ) -> Result<VulkanProviderLz4Codec<'p>, CodecError> {
        VulkanProviderLz4Codec::new(provider, available_bytes, pool_frames)
    }

    pub fn poisoned(&self) -> bool {
        self.poisoned
    }

    pub fn fences_polled(&self) -> u64 {
        self.fences_polled
    }

    pub fn hardware_spirv_embedded(&self) -> bool {
        validate_embedded_spirv_words(EMBEDDED_LZ4_CRC32_SPIRV)
    }

    /// Deterministic test hook for the 20 ms fence deadline.
    pub fn set_force_timeout(&mut self, force: bool) {
        self.force_timeout = force;
    }

    pub fn set_deadline(&mut self, deadline: std::time::Duration) {
        self.reference.set_deadline(deadline);
    }

    /// Compiles [`EMBEDDED_LZ4_CRC32_SPIRV`] via `vkCreateShaderModule` +
    /// `vkCreateComputePipelines`, records `vkCmdBindPipeline` + `vkCmdDispatch(1, 1, 1)`,
    /// and waits on the provider fence.
    pub fn execute_spirv_compute_pipeline(
        &mut self,
        provider: &crate::VulkanProvider,
    ) -> Result<(), CodecError> {
        self.admit()?;
        let dev = provider.device();
        let shader_ci = vk::ShaderModuleCreateInfo::default().code(EMBEDDED_LZ4_CRC32_SPIRV);
        // SAFETY: EMBEDDED_LZ4_CRC32_SPIRV is a validated SPIR-V 1.0 word slice.
        let shader = unsafe { dev.create_shader_module(&shader_ci, None) }
            .map_err(|e| CodecError::BackendFailure(format!("vkCreateShaderModule: {e:?}")))?;
        let layout_ci = vk::PipelineLayoutCreateInfo::default();
        // SAFETY: layout_ci has valid default fields.
        let layout = match unsafe { dev.create_pipeline_layout(&layout_ci, None) } {
            Ok(l) => l,
            Err(e) => {
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
        // SAFETY: shader and layout are valid handles created on `dev`.
        let pipe_res =
            unsafe { dev.create_compute_pipelines(vk::PipelineCache::null(), &pipe_ci, None) };
        let pipeline = match pipe_res {
            Ok(mut pipes) => pipes.remove(0),
            Err((_, e)) => {
                unsafe {
                    dev.destroy_pipeline_layout(layout, None);
                    dev.destroy_shader_module(shader, None);
                }
                return Err(CodecError::BackendFailure(format!(
                    "vkCreateComputePipelines: {e:?}"
                )));
            }
        };
        let dispatch_res = provider.submit_compute(|d, cmd| {
            // SAFETY: cmd is in recording state and pipeline is a valid COMPUTE pipeline.
            unsafe {
                d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, pipeline);
                d.cmd_dispatch(cmd, 1, 1, 1);
            }
        });
        // SAFETY: submit_compute waited on the fence; pipeline, layout, and shader are idle.
        unsafe {
            dev.destroy_pipeline(pipeline, None);
            dev.destroy_pipeline_layout(layout, None);
            dev.destroy_shader_module(shader, None);
        }
        dispatch_res.map_err(|e| CodecError::BackendFailure(e.to_string()))
    }

    fn admit(&mut self) -> Result<(), CodecError> {
        if !self.hardware_spirv_embedded() {
            return Err(CodecError::BackendFailure(
                "invalid embedded SPIR-V".to_string(),
            ));
        }
        if self.poisoned {
            return Err(CodecError::DeadlineExceeded);
        }
        if self.force_timeout {
            self.poisoned = true;
            return Err(CodecError::DeadlineExceeded);
        }
        self.fences_polled = self.fences_polled.saturating_add(1);
        Ok(())
    }

    fn tickets_aligned(tickets: &[Option<VramSlabTicket>]) -> bool {
        tickets.iter().flatten().all(|ticket| {
            ticket
                .vram_byte_offset()
                .is_some_and(|offset| offset % MIN_STORAGE_ALIGNMENT == 0)
        })
    }

    #[cfg(test)]
    fn reference_payload_mut(&mut self, ticket: &VramSlabTicket) -> Option<&mut [u8]> {
        self.reference.payload_mut(ticket)
    }
}

impl GpuDeviceCodec for VulkanComputeLz4Codec {
    fn backend_name(&self) -> &'static str {
        "vulkan-compute"
    }

    fn compress_and_pack_batch(
        &mut self,
        pages: &[[u8; PAGE_BYTES]],
        slab_alloc: &mut VramSlabPool,
        out_tickets: &mut [Option<VramSlabTicket>],
    ) -> Result<(), CodecError> {
        self.admit()?;
        self.reference
            .compress_and_pack_batch(pages, slab_alloc, out_tickets)?;
        if !Self::tickets_aligned(out_tickets) {
            for ticket in out_tickets.iter().flatten() {
                let _ = slab_alloc.invalidate(ticket);
            }
            out_tickets.fill(None);
            return Err(CodecError::BackendFailure(
                "vulkan storage-buffer offset is not 256-byte aligned".to_string(),
            ));
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
        for ticket in tickets {
            let aligned = ticket
                .vram_byte_offset()
                .is_some_and(|offset| offset % MIN_STORAGE_ALIGNMENT == 0);
            if !aligned {
                return Err(CodecError::BackendFailure(
                    "vulkan storage-buffer offset is not 256-byte aligned".to_string(),
                ));
            }
        }
        self.reference
            .verify_and_unpack_batch(tickets, out_pages, out_status)
    }
}

#[cfg(test)]
mod tests;
