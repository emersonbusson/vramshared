// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cuda/src/ptx_lz4_codec/syms.rs
// CUDA driver symbol loading, RAII module/event guards, metadata staging
// helpers, and single-page GPU verify+unpack dispatch for `ptx_lz4_codec`.

use core::ffi::{c_char, c_uint, c_void};
use std::time::{Duration, Instant};

use ramshared_vram::device_codec::{CodecError, MAX_COMPRESSED_LEN, PAGE_BYTES, PageDecodeStatus};
use ramshared_vram::slab_pool::{SlabClass, SlotEncoding, VramSlabPool, VramSlabTicket};

use crate::driver::{Context, CudaError, DeviceMem, load_sym};
use crate::ffi::{CUDA_SUCCESS, CuDevicePtr, CuResult};

use super::ptx::{EMBEDDED_LZ4_CRC32_PTX, validate_embedded_ptx_image};

const CUDA_ERROR_NOT_READY: CuResult = 600;

pub(super) type CuModule = *mut c_void;
pub(super) type CuFunction = *mut c_void;
pub(super) type CuEvent = *mut c_void;
pub(super) type CuStream = *mut c_void;

pub(super) type FnModuleLoadData = unsafe extern "C" fn(*mut CuModule, *const c_void) -> CuResult;
pub(super) type FnModuleUnload = unsafe extern "C" fn(CuModule) -> CuResult;
pub(super) type FnModuleGetFunction =
    unsafe extern "C" fn(*mut CuFunction, CuModule, *const c_char) -> CuResult;
pub(super) type FnLaunchKernel = unsafe extern "C" fn(
    CuFunction,
    c_uint,
    c_uint,
    c_uint,
    c_uint,
    c_uint,
    c_uint,
    c_uint,
    CuStream,
    *mut *mut c_void,
    *mut *mut c_void,
) -> CuResult;
pub(super) type FnEventCreate = unsafe extern "C" fn(*mut CuEvent, c_uint) -> CuResult;
pub(super) type FnEventDestroy = unsafe extern "C" fn(CuEvent) -> CuResult;
pub(super) type FnEventRecord = unsafe extern "C" fn(CuEvent, CuStream) -> CuResult;
pub(super) type FnEventQuery = unsafe extern "C" fn(CuEvent) -> CuResult;
pub(super) type FnMemcpyDtoD = unsafe extern "C" fn(CuDevicePtr, CuDevicePtr, usize) -> CuResult;

pub(super) const ENCODING_RAW: u32 = 1;
pub(super) const ENCODING_LZ4: u32 = 2;

#[derive(Clone, Copy)]
pub(super) struct KernelSyms {
    pub(super) module_load_data: FnModuleLoadData,
    pub(super) module_unload: FnModuleUnload,
    pub(super) module_get_function: FnModuleGetFunction,
    pub(super) launch_kernel: FnLaunchKernel,
    pub(super) event_create: FnEventCreate,
    pub(super) event_destroy: FnEventDestroy,
    pub(super) event_record: FnEventRecord,
    pub(super) event_query: FnEventQuery,
    pub(super) memcpy_dtod: FnMemcpyDtoD,
}

impl KernelSyms {
    pub(super) fn load(ctx: &Context<'_>) -> Result<Self, CodecError> {
        let handle = ctx.cuda().lib_handle();
        let map_err = |e: CudaError| CodecError::BackendFailure(e.to_string());
        // SAFETY: `handle` is an active libcuda shared object handle held by `ctx.cuda()`.
        unsafe {
            Ok(Self {
                module_load_data: load_sym(handle, c"cuModuleLoadData").map_err(map_err)?,
                module_unload: load_sym(handle, c"cuModuleUnload").map_err(map_err)?,
                module_get_function: load_sym(handle, c"cuModuleGetFunction").map_err(map_err)?,
                launch_kernel: load_sym(handle, c"cuLaunchKernel").map_err(map_err)?,
                event_create: load_sym(handle, c"cuEventCreate").map_err(map_err)?,
                event_destroy: load_sym(handle, c"cuEventDestroy_v2")
                    .or_else(|_| load_sym(handle, c"cuEventDestroy"))
                    .map_err(map_err)?,
                event_record: load_sym(handle, c"cuEventRecord").map_err(map_err)?,
                event_query: load_sym(handle, c"cuEventQuery").map_err(map_err)?,
                memcpy_dtod: load_sym(handle, c"cuMemcpyDtoD_v2")
                    .or_else(|_| load_sym(handle, c"cuMemcpyDtoD"))
                    .map_err(map_err)?,
            })
        }
    }
}

pub(super) struct ScopedCuModule {
    pub(super) module: CuModule,
    pub(super) unload: FnModuleUnload,
}

impl Drop for ScopedCuModule {
    fn drop(&mut self) {
        if !self.module.is_null() {
            // SAFETY: module was returned by cuModuleLoadData and not yet unloaded.
            unsafe {
                let _ = (self.unload)(self.module);
            }
        }
    }
}

pub(super) struct ScopedCuEvent {
    pub(super) event: CuEvent,
    pub(super) destroy: FnEventDestroy,
}

impl Drop for ScopedCuEvent {
    fn drop(&mut self) {
        if !self.event.is_null() {
            // SAFETY: event was returned by cuEventCreate and not yet destroyed.
            unsafe {
                let _ = (self.destroy)(self.event);
            }
        }
    }
}

pub(super) fn load_ptx_module(syms: &KernelSyms) -> Result<ScopedCuModule, CodecError> {
    if !validate_embedded_ptx_image(EMBEDDED_LZ4_CRC32_PTX) {
        return Err(CodecError::BackendFailure("invalid embedded PTX".into()));
    }
    let mut raw_mod: CuModule = core::ptr::null_mut();
    // SAFETY: EMBEDDED_LZ4_CRC32_PTX is a NUL-terminated PTX 7.0 sm_50 image.
    let rc =
        unsafe { (syms.module_load_data)(&mut raw_mod, EMBEDDED_LZ4_CRC32_PTX.as_ptr().cast()) };
    if rc != CUDA_SUCCESS {
        return Err(CodecError::BackendFailure(format!(
            "cuModuleLoadData failed: {rc}"
        )));
    }
    Ok(ScopedCuModule {
        module: raw_mod,
        unload: syms.module_unload,
    })
}

pub(super) fn get_kernel_fn(
    syms: &KernelSyms,
    module: CuModule,
    name: &core::ffi::CStr,
) -> Result<CuFunction, CodecError> {
    let mut func: CuFunction = core::ptr::null_mut();
    // SAFETY: `module` is an active CUmodule and `name` is NUL-terminated.
    let rc = unsafe { (syms.module_get_function)(&mut func, module, name.as_ptr()) };
    if rc != CUDA_SUCCESS || func.is_null() {
        return Err(CodecError::BackendFailure(format!(
            "cuModuleGetFunction({}) failed: {rc}",
            name.to_string_lossy()
        )));
    }
    Ok(func)
}

pub(super) fn read_u32_array(
    meta: &DeviceMem<'_, '_>,
    byte_offset: usize,
    count: usize,
) -> Result<Vec<u32>, CodecError> {
    let mut raw = vec![0u8; count * 4];
    meta.read_at(byte_offset, &mut raw)
        .map_err(|e| CodecError::BackendFailure(e.to_string()))?;
    Ok(raw
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect())
}

pub(super) fn write_u32_array(
    meta: &mut DeviceMem<'_, '_>,
    byte_offset: usize,
    vals: &[u32],
) -> Result<(), CodecError> {
    let mut raw = Vec::with_capacity(vals.len() * 4);
    for v in vals {
        raw.extend_from_slice(&v.to_le_bytes());
    }
    meta.write_at(byte_offset, &raw)
        .map_err(|e| CodecError::BackendFailure(e.to_string()))
}

pub(super) fn write_u64_array(
    meta: &mut DeviceMem<'_, '_>,
    byte_offset: usize,
    vals: &[u64],
) -> Result<(), CodecError> {
    let mut raw = Vec::with_capacity(vals.len() * 8);
    for v in vals {
        raw.extend_from_slice(&v.to_le_bytes());
    }
    meta.write_at(byte_offset, &raw)
        .map_err(|e| CodecError::BackendFailure(e.to_string()))
}

/// Combined single-kernel GPU verify+unpack helper used by [`super::CudaPtxLz4Codec`].
pub(super) fn verify_and_unpack_single_on_gpu(
    ctx: &Context<'_>,
    payload: &[u8],
    expected_crc: u32,
    encoding: SlotEncoding,
    out_page: &mut [u8; PAGE_BYTES],
) -> Result<PageDecodeStatus, CodecError> {
    let syms = KernelSyms::load(ctx)?;
    verify_and_unpack_single_with_syms(ctx, &syms, payload, expected_crc, encoding, out_page)
}

pub(super) fn verify_and_unpack_single_with_syms(
    ctx: &Context<'_>,
    syms: &KernelSyms,
    payload: &[u8],
    expected_crc: u32,
    encoding: SlotEncoding,
    out_page: &mut [u8; PAGE_BYTES],
) -> Result<PageDecodeStatus, CodecError> {
    let scoped = load_ptx_module(syms)?;
    let unpack_fn = get_kernel_fn(
        syms,
        scoped.module,
        c"ramshared_crc32_and_lz4_unpack_kernel",
    )?;
    let map_err = |e: CudaError| CodecError::BackendFailure(e.to_string());
    let mut d_src = ctx.alloc(payload.len()).map_err(map_err)?;
    let mut d_dst = ctx.alloc(PAGE_BYTES).map_err(map_err)?;
    let mut d_status = ctx.alloc(4).map_err(map_err)?;
    d_src.write_at(0, payload).map_err(map_err)?;
    d_dst.zero().map_err(map_err)?;
    d_status.zero().map_err(map_err)?;

    let (mut src_ptr, mut src_len) = (d_src.device_ptr(), payload.len() as u32);
    let mut crc_arg = expected_crc;
    let mut enc_arg = match encoding {
        SlotEncoding::Raw => ENCODING_RAW,
        SlotEncoding::Lz4 => ENCODING_LZ4,
    };
    let (mut dst_ptr, mut dst_cap, mut status_ptr) =
        (d_dst.device_ptr(), PAGE_BYTES as u32, d_status.device_ptr());
    let mut params: [*mut c_void; 7] = [
        (&mut src_ptr as *mut u64).cast(),
        (&mut src_len as *mut u32).cast(),
        (&mut crc_arg as *mut u32).cast(),
        (&mut enc_arg as *mut u32).cast(),
        (&mut dst_ptr as *mut u64).cast(),
        (&mut dst_cap as *mut u32).cast(),
        (&mut status_ptr as *mut u64).cast(),
    ];
    // SAFETY: `unpack_fn` and `params` match `ramshared_crc32_and_lz4_unpack_kernel`.
    let rc = unsafe {
        (syms.launch_kernel)(
            unpack_fn,
            1,
            1,
            1,
            1,
            1,
            1,
            0,
            core::ptr::null_mut(),
            params.as_mut_ptr(),
            core::ptr::null_mut(),
        )
    };
    if rc != CUDA_SUCCESS || unsafe { (ctx.cuda().syms().ctx_synchronize)() } != CUDA_SUCCESS {
        return Err(CodecError::BackendFailure(format!(
            "combined unpack kernel launch failed: {rc}"
        )));
    }
    let mut status_byte = [0u8; 1];
    d_status.read_at(0, &mut status_byte).map_err(map_err)?;
    match status_byte[0] {
        0 => {
            d_dst.read_at(0, out_page).map_err(map_err)?;
            Ok(PageDecodeStatus::Ok)
        }
        1 => Ok(PageDecodeStatus::PreDecodeCrcMismatch),
        _ => Ok(PageDecodeStatus::CorruptLz4Stream),
    }
}

/// Validates kernel output encoding/length, copies payload device-to-device
/// into `d_slab_base`, and commits the ticket in `slab_alloc`.
#[allow(clippy::too_many_arguments)]
pub(super) fn pack_dtod_slab_slot(
    memcpy_dtod: FnMemcpyDtoD,
    d_comp_base: CuDevicePtr,
    d_slab_base: CuDevicePtr,
    pool_bytes: usize,
    index: usize,
    enc: u32,
    len: u32,
    crc: u32,
    slab_alloc: &mut VramSlabPool,
) -> Result<VramSlabTicket, CodecError> {
    let (encoding, payload_len) = match enc {
        ENCODING_LZ4 => {
            let valid = usize::try_from(len)
                .ok()
                .filter(|l| *l > 0 && *l <= MAX_COMPRESSED_LEN)
                .ok_or_else(|| {
                    CodecError::BackendFailure("invalid GPU LZ4 payload length".into())
                })?;
            (SlotEncoding::Lz4, valid as u16)
        }
        ENCODING_RAW => {
            if len as usize != PAGE_BYTES {
                return Err(CodecError::BackendFailure(
                    "invalid GPU Raw payload length".into(),
                ));
            }
            (SlotEncoding::Raw, PAGE_BYTES as u16)
        }
        other => {
            return Err(CodecError::BackendFailure(format!(
                "unknown GPU encoding tag: {other}"
            )));
        }
    };
    let mut ticket = slab_alloc.alloc_sub_slot(SlabClass::select_for_len(payload_len))?;
    let Some(vram_off) = ticket.vram_byte_offset() else {
        let _ = slab_alloc.invalidate(&ticket);
        return Err(CodecError::BackendFailure(
            "ticket VRAM offset overflow".into(),
        ));
    };
    if vram_off.saturating_add(u64::from(payload_len)) > pool_bytes as u64 {
        let _ = slab_alloc.invalidate(&ticket);
        return Err(CodecError::BackendFailure(
            "ticket VRAM offset exceeds resident slab allocation".into(),
        ));
    }
    let src_dev = d_comp_base + (index * PAGE_BYTES) as u64;
    let dst_dev = d_slab_base + vram_off;
    // SAFETY: both device pointers stay within `d_comp` and `d_slab_vram` bounds.
    let rc = unsafe { memcpy_dtod(dst_dev, src_dev, payload_len as usize) };
    if rc != CUDA_SUCCESS {
        let _ = slab_alloc.invalidate(&ticket);
        return Err(CodecError::BackendFailure(format!(
            "cuMemcpyDtoD failed: {rc}"
        )));
    }
    if let Err(e) = slab_alloc.commit(&mut ticket, encoding, payload_len, crc) {
        let _ = slab_alloc.invalidate(&ticket);
        return Err(CodecError::from(e));
    }
    Ok(ticket)
}

/// Launches a 1D CUDA grid, records `done_event`, and spins until completion or `deadline`.
/// Returns `Ok(polls)` or `Err(error)` (caller must poison buffers on any failure).
pub(super) fn launch_and_poll_cuda_event(
    syms: &KernelSyms,
    func: CuFunction,
    done_event: CuEvent,
    grid_x: u32,
    params: &mut [*mut c_void],
    deadline: Duration,
) -> Result<u64, CodecError> {
    // SAFETY: caller provides `func` and `params` matching its PTX signature.
    let rc = unsafe {
        (syms.launch_kernel)(
            func,
            grid_x,
            1,
            1,
            1,
            1,
            1,
            0,
            core::ptr::null_mut(),
            params.as_mut_ptr(),
            core::ptr::null_mut(),
        )
    };
    if rc != CUDA_SUCCESS {
        return Err(CodecError::BackendFailure(format!(
            "cuLaunchKernel failed: {rc}"
        )));
    }
    // SAFETY: `done_event` was created on the active CUDA context.
    let rc = unsafe { (syms.event_record)(done_event, core::ptr::null_mut()) };
    if rc != CUDA_SUCCESS {
        return Err(CodecError::BackendFailure(format!(
            "cuEventRecord failed: {rc}"
        )));
    }
    let started = Instant::now();
    let mut polls = 0u64;
    loop {
        polls = polls.saturating_add(1);
        // SAFETY: `done_event` is recorded on the active default stream.
        let q = unsafe { (syms.event_query)(done_event) };
        if q == CUDA_SUCCESS {
            return Ok(polls);
        }
        if q != CUDA_ERROR_NOT_READY {
            return Err(CodecError::BackendFailure(format!(
                "cuEventQuery failed: {q}"
            )));
        }
        if started.elapsed() >= deadline {
            return Err(CodecError::DeadlineExceeded);
        }
        core::hint::spin_loop();
    }
}
