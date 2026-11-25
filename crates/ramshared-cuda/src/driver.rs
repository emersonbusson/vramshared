// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cuda/src/driver.rs
//! Safe wrappers (RAII) over the CUDA Driver API. SPEC §4, §8.
//!
//! Ownership Model:
//! - [`Cuda`] owns the dynamic library handle + the resolved symbol table (longest-lived).
//! - [`Context`] borrows `&Cuda` and calls `cuCtxDestroy` in its `Drop` implementation.
//! - [`DeviceMem`] borrows `&Context` and calls `cuMemFree` in its `Drop` implementation.
//!
//! The `Drop` order guarantees the reverse order of allocation required by CUDA
//! (freeing memory -> destroying context -> closing library), translating the kernel's
//! `goto out_err` pattern into Rust's borrow checker invariants.

use core::ffi::{CStr, c_char, c_void};
use core::fmt;

use ramshared_vram::{GpuAdapterIdentity, format_luid};

use crate::ffi::{CUDA_SUCCESS, CuContext, CuDevice, CuDevicePtr, CuResult, Syms};

/// CUDA layer error representation. No `panic`/`unwrap` in production paths (coding.md rules).
#[derive(Debug)]
pub enum CudaError {
    /// Dynamic library loading failed to find a candidate library.
    Load(String),
    /// Symbol resolution failed for a required symbol.
    Symbol(String),
    /// A CUDA Driver API call returned an error code.
    Driver {
        op: &'static str,
        code: i32,
        msg: String,
    },
    /// VRAM memory region access out of bounds (offset + len > size).
    OutOfRange { off: usize, len: usize, size: usize },
    /// Invalid argument supplied to driver wrapper.
    InvalidValue(String),
    /// The requested feature is unsupported by the loaded driver version.
    Unsupported(String),
}

impl fmt::Display for CudaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CudaError::Load(s) => write!(f, "failed to load CUDA library: {s}"),
            CudaError::Symbol(s) => write!(f, "required CUDA symbol missing: {s}"),
            CudaError::Driver { op, code, msg } => {
                write!(f, "{op} failed (CUresult={code}): {msg}")
            }
            CudaError::OutOfRange { off, len, size } => {
                write!(f, "out of bounds access: off={off} len={len} > size={size}")
            }
            CudaError::InvalidValue(s) => write!(f, "invalid argument: {s}"),
            CudaError::Unsupported(s) => write!(f, "unsupported driver feature: {s}"),
        }
    }
}

impl core::error::Error for CudaError {}

const HOST_PAGE_BYTES: usize = 4096;

pub(super) fn validate_host_registration(
    host_ptr: *mut c_void,
    len: usize,
) -> Result<(), CudaError> {
    if host_ptr.is_null() {
        return Err(CudaError::InvalidValue("host_ptr cannot be null".into()));
    }
    if len == 0 {
        return Err(CudaError::InvalidValue(
            "length must be greater than zero".into(),
        ));
    }
    if !len.is_multiple_of(HOST_PAGE_BYTES) {
        return Err(CudaError::InvalidValue(format!(
            "length {len} must be a multiple of page size {HOST_PAGE_BYTES}"
        )));
    }
    if !(host_ptr as usize).is_multiple_of(HOST_PAGE_BYTES) {
        return Err(CudaError::InvalidValue(format!(
            "host_ptr {host_ptr:p} must be aligned to {HOST_PAGE_BYTES}-byte page boundary"
        )));
    }
    Ok(())
}

/// RAII wrapper for the loaded dynamic library handle: calls close on `Drop`.
pub(crate) struct Lib(pub(crate) *mut c_void);

impl Drop for Lib {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: self.0 was returned by a successful open call and has not been closed.
            unsafe { crate::loader::close(self.0) };
        }
    }
}

/// CUDA library loaded and initialized successfully (`cuInit(0)`).
///
/// Holds NVML alongside it: on WSL2 GPU-PV `cuMemGetInfo` is scoped to the
/// calling process's channel and does not observe other processes, so the
/// VRAM budget must be read from the device-wide NVML authority.
pub struct Cuda {
    _lib: Lib,
    syms: Syms,
    nvml: crate::nvml::Nvml,
}

impl Cuda {
    /// Loads the CUDA driver library (using OS-specific candidates) and runs `cuInit(0)`.
    pub fn load() -> Result<Self, CudaError> {
        let lib = crate::driver_library::load(crate::driver_library::DriverLibrary::Cuda)
            .map_err(CudaError::Load)?;
        let handle = lib.0;

        // SAFETY: handle is an active library context; each resolved symbol is a valid
        // Driver API function pointer conforming to the signatures in ffi.rs.
        let syms = unsafe {
            Syms {
                init: load_sym(handle, c"cuInit")?,
                device_get_count: load_sym(handle, c"cuDeviceGetCount")?,
                device_get: load_sym(handle, c"cuDeviceGet")?,
                device_get_name: load_sym(handle, c"cuDeviceGetName")?,
                // Only v2 explicitly identifies a subscribed MIG compute instance.
                // Missing v2 leaves budget identity unavailable, never inferred.
                device_get_uuid: load_sym_opt(handle, c"cuDeviceGetUuid_v2"),
                device_get_luid: load_sym_opt(handle, c"cuDeviceGetLuid"),
                ctx_create: load_sym(handle, c"cuCtxCreate_v2")?,
                ctx_destroy: load_sym(handle, c"cuCtxDestroy_v2")?,
                ctx_synchronize: load_sym(handle, c"cuCtxSynchronize")?,
                mem_alloc: load_sym(handle, c"cuMemAlloc_v2")?,
                mem_free: load_sym(handle, c"cuMemFree_v2")?,
                memcpy_htod: load_sym(handle, c"cuMemcpyHtoD_v2")?,
                memcpy_dtoh: load_sym(handle, c"cuMemcpyDtoH_v2")?,
                memset_d8: load_sym(handle, c"cuMemsetD8_v2")?,
                mem_get_info: load_sym(handle, c"cuMemGetInfo_v2")?,
                get_error_string: load_sym_opt(handle, c"cuGetErrorString"),
                mem_host_register: load_sym_opt(handle, c"cuMemHostRegister_v2")
                    .or_else(|| load_sym_opt(handle, c"cuMemHostRegister")),
                mem_host_unregister: load_sym_opt(handle, c"cuMemHostUnregister"),
                mem_host_get_device_pointer: load_sym_opt(handle, c"cuMemHostGetDevicePointer_v2")
                    .or_else(|| load_sym_opt(handle, c"cuMemHostGetDevicePointer")),
            }
        };

        // SAFETY: init symbol resolved successfully.
        let r = unsafe { (syms.init)(0) };
        check(&syms, r, "cuInit")?;

        // The embedded kernel is part of the driver contract: a corrupt or
        // truncated image must fail the load, not fall through to a CPU
        // substitute under a CUDA name.
        if !crate::ptx_lz4_codec::validate_embedded_ptx_image(
            crate::ptx_lz4_codec::EMBEDDED_LZ4_CRC32_PTX,
        ) {
            return Err(CudaError::Load(
                "embedded PTX image failed structural validation".into(),
            ));
        }

        // Fail closed when NVML is missing. Without it the budget would come
        // from the per-process `cuMemGetInfo` view and the cache would refuse
        // to yield to another VRAM consumer, which is the wrong safe default.
        let nvml = crate::nvml::Nvml::load().map_err(|e| CudaError::Load(e.to_string()))?;

        Ok(Cuda {
            _lib: lib,
            syms,
            nvml,
        })
    }

    /// Returns the number of CUDA-capable devices visible to the system.
    pub(crate) fn lib_handle(&self) -> *mut c_void {
        self._lib.0
    }

    pub(crate) fn syms(&self) -> &Syms {
        &self.syms
    }

    pub fn device_count(&self) -> Result<i32, CudaError> {
        let mut count: i32 = 0;
        // SAFETY: count points to a valid local memory location.
        let r = unsafe { (self.syms.device_get_count)(&mut count) };
        check(&self.syms, r, "cuDeviceGetCount")?;
        Ok(count)
    }

    /// Gets the device handle for the specified `ordinal` index, resolving its name.
    pub fn device(&self, ordinal: i32) -> Result<Device, CudaError> {
        let mut raw: CuDevice = 0;
        // SAFETY: raw points to a valid local memory location.
        let r = unsafe { (self.syms.device_get)(&mut raw, ordinal) };
        check(&self.syms, r, "cuDeviceGet")?;

        let mut buf = [0_i8; 128];
        // SAFETY: buf has space for `len` bytes; the API writes a null-terminated string.
        let r = unsafe {
            (self.syms.device_get_name)(buf.as_mut_ptr() as *mut c_char, buf.len() as i32, raw)
        };
        check(&self.syms, r, "cuDeviceGetName")?;
        buf[buf.len() - 1] = 0; // guarantees null-termination even if the API filled the entire buffer
        // SAFETY: buf was initialized by the driver call and has a guaranteed terminating null byte.
        let name = unsafe { CStr::from_ptr(buf.as_ptr() as *const c_char) }
            .to_string_lossy()
            .into_owned();

        let uuid = self.syms.device_get_uuid.and_then(|get_uuid| {
            let mut uuid = [0_i8; 16];
            // SAFETY: uuid is a writable 16-byte CUDA UUID buffer and raw is a valid device.
            (unsafe { get_uuid(&mut uuid, raw) } == CUDA_SUCCESS).then_some(uuid)
        });
        let windows_scope = self.syms.device_get_luid.and_then(|get_luid| {
            let mut luid = [0_i8; 8];
            let mut node_mask = 0_u32;
            // SAFETY: luid and node_mask are writable outputs and raw is a valid device.
            (unsafe { get_luid(luid.as_mut_ptr(), &mut node_mask, raw) } == CUDA_SUCCESS)
                .then(|| format_luid(luid.map(|byte| byte as u8)).map(|luid| (luid, node_mask)))
                .flatten()
        });

        let (luid, node_mask) =
            windows_scope.map_or((None, None), |(luid, mask)| (Some(luid), Some(mask)));
        Ok(Device {
            node_mask,
            raw,
            name,
            ordinal,
            uuid,
            luid,
        })
    }

    /// Device-wide memory observation without creating a CUDA context.
    pub fn device_memory(&self, device: &Device) -> Result<crate::nvml::NvmlMemory, CudaError> {
        self.memory_for_identity(device.adapter_identity().as_ref())
    }

    fn memory_for_identity(
        &self,
        adapter: Option<&GpuAdapterIdentity>,
    ) -> Result<crate::nvml::NvmlMemory, CudaError> {
        let adapter = adapter.ok_or_else(|| CudaError::Driver {
            op: "nvmlDeviceGetHandleByUUID",
            code: -1,
            msg: "CUDA adapter identity is unavailable".into(),
        })?;
        self.nvml
            .device_memory_for_adapter(adapter)
            .map_err(|e| CudaError::Driver {
                op: "nvmlDeviceGetMemoryInfo",
                code: -1,
                msg: e.to_string(),
            })
    }

    /// Creates a CUDA context on the specified device (becomes current on the calling thread).
    pub fn create_context<'a>(&'a self, device: &Device) -> Result<Context<'a>, CudaError> {
        let mut raw: CuContext = core::ptr::null_mut();
        // SAFETY: raw points to a valid local; device.raw is a valid CUdevice handle.
        let r = unsafe { (self.syms.ctx_create)(&mut raw, 0, device.raw) };
        check(&self.syms, r, "cuCtxCreate")?;
        Ok(Context {
            cuda: self,
            raw,
            adapter: device.adapter_identity(),
            ordinal: device.ordinal,
        })
    }
}

/// A CUDA device (ordinal index + name).
#[derive(Clone, Debug)]
pub struct Device {
    raw: CuDevice,
    name: String,
    ordinal: i32,
    uuid: Option<[i8; 16]>,
    luid: Option<String>,
    node_mask: Option<u32>,
}

impl Device {
    /// Read the identity captured during device enumeration without opening a
    /// context or reserving device memory.
    pub fn adapter_identity(&self) -> Option<GpuAdapterIdentity> {
        cuda_adapter_identity(self.uuid, self.luid.clone()).map(|mut identity| {
            identity.node_mask = self.node_mask;
            identity
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Device ordinal passed to [`Cuda::device`].
    pub fn ordinal(&self) -> i32 {
        self.ordinal
    }
}

/// CUDA context representation. `Drop` implementation calls `cuCtxDestroy`.
///
/// **Thread Affinity:** The active CUDA context is *thread-local*. This `Context` (and
/// derived `DeviceMem` allocations) must be used on the **same thread** that created them.
/// This is why the daemon executes all VRAM I/O on a single thread. Accessing from another thread
/// would require calling `cuCtxSetCurrent` (not implemented here). The DEMOTE thread only calls `swapoff`.
pub struct Context<'a> {
    cuda: &'a Cuda,
    raw: CuContext,
    adapter: Option<GpuAdapterIdentity>,
    ordinal: i32,
}

fn cuda_adapter_identity(
    uuid: Option<[i8; 16]>,
    luid: Option<String>,
) -> Option<GpuAdapterIdentity> {
    let uuid_key = uuid
        .filter(|uuid| uuid.iter().any(|byte| *byte != 0))
        .map(|uuid| {
            uuid.iter()
                .map(|byte| format!("{:02x}", *byte as u8))
                .collect::<String>()
        });
    let key = uuid_key.or_else(|| luid.as_ref().map(|luid| format!("luid:{luid}")))?;
    Some(GpuAdapterIdentity {
        node_mask: None,
        backend: "cuda".into(),
        key,
        luid,
    })
}

impl<'a> Context<'a> {
    pub(crate) fn cuda(&self) -> &'a Cuda {
        self.cuda
    }

    pub fn adapter_identity(&self) -> Option<&GpuAdapterIdentity> {
        self.adapter.as_ref()
    }

    /// Device ordinal this context was created on.
    pub fn ordinal(&self) -> i32 {
        self.ordinal
    }

    /// Device-wide VRAM occupancy across every process on this adapter (NVML).
    pub fn device_memory(&self) -> Result<crate::nvml::NvmlMemory, CudaError> {
        self.cuda.memory_for_identity(self.adapter.as_ref())
    }

    /// Device-wide GPU busy percents for this adapter (NVML).
    ///
    /// A present value is a measurement (`0%` = observed idle); every
    /// unavailability is an error and must be published as "not measured",
    /// never as `0%`. See [`crate::nvml::Nvml::device_utilization`].
    pub fn device_utilization(&self) -> Result<crate::nvml::GpuUtilization, CudaError> {
        self.cuda
            .nvml
            .device_utilization_for_adapter(self.adapter.as_ref().ok_or_else(|| {
                CudaError::Driver {
                    op: "nvmlDeviceGetHandleByUUID",
                    code: -1,
                    msg: "CUDA adapter identity is unavailable".into(),
                }
            })?)
            .map_err(|e| CudaError::Driver {
                op: "nvmlDeviceGetUtilizationRates",
                code: -1,
                msg: e.to_string(),
            })
    }

    /// Returns the free and total VRAM capacities in bytes (`cuMemGetInfo`).
    ///
    /// Scope is the calling process's GPU-PV channel, not the whole adapter:
    /// other processes' allocations are invisible here. Use
    /// [`Context::device_memory`] for device-wide occupancy; the VRAM budget
    /// does so, and this method remains only as the raw allocator-local view.
    pub fn mem_info(&self) -> Result<(usize, usize), CudaError> {
        let (mut free, mut total) = (0_usize, 0_usize);
        // SAFETY: out-parameters are valid local pointers; CUDA context is current on the calling thread.
        let r = unsafe { (self.cuda.syms.mem_get_info)(&mut free, &mut total) };
        check(&self.cuda.syms, r, "cuMemGetInfo")?;
        Ok((free, total))
    }

    /// Allocates `bytes` of VRAM. The allocation is released when the returned `DeviceMem` is dropped.
    pub fn alloc(&self, bytes: usize) -> Result<DeviceMem<'_, 'a>, CudaError> {
        let mut ptr: CuDevicePtr = 0;
        // SAFETY: ptr points to a valid local; CUDA context is current.
        let r = unsafe { (self.cuda.syms.mem_alloc)(&mut ptr, bytes) };
        check(&self.cuda.syms, r, "cuMemAlloc")?;
        Ok(DeviceMem {
            ctx: self,
            ptr,
            len: bytes,
        })
    }

    /// Registers an existing host allocation for zero-copy device access (`cuMemHostRegister`).
    ///
    /// Validates that `host_ptr` is non-null, `len > 0`, `len` is a multiple of 4096, and
    /// `host_ptr` is 4096-byte page aligned (SPEC §RF-1, §DT-2, invariant check).
    ///
    /// # Safety
    ///
    /// The caller must ensure that `host_ptr` points to at least `len` valid, allocated host
    /// bytes that remain valid and are not deallocated while the returned [`PinnedHostMapping`] is alive.
    pub unsafe fn register_host<'c>(
        &'c self,
        host_ptr: *mut c_void,
        len: usize,
        flags: u32,
    ) -> Result<PinnedHostMapping<'c, 'a>, CudaError> {
        validate_host_registration(host_ptr, len)?;

        let fn_register = self.cuda.syms.mem_host_register.ok_or_else(|| {
            CudaError::Unsupported("cuMemHostRegister not supported by driver".into())
        })?;
        let fn_get_dptr = self.cuda.syms.mem_host_get_device_pointer.ok_or_else(|| {
            CudaError::Unsupported("cuMemHostGetDevicePointer not supported by driver".into())
        })?;
        let fn_unreg = self.cuda.syms.mem_host_unregister.ok_or_else(|| {
            CudaError::Unsupported("cuMemHostUnregister not supported by driver".into())
        })?;

        // SAFETY: host_ptr is non-null, 4096-aligned, and points to len valid bytes.
        let r = unsafe { fn_register(host_ptr, len, flags) };
        check(&self.cuda.syms, r, "cuMemHostRegister")?;

        let mut dev_ptr: CuDevicePtr = 0;
        // SAFETY: dev_ptr points to a valid local u64; host_ptr was successfully registered.
        let r = unsafe { fn_get_dptr(&mut dev_ptr, host_ptr, 0) };
        if r != CUDA_SUCCESS {
            // SAFETY: host_ptr was successfully registered and fn_unreg is required above.
            unsafe {
                let _ = fn_unreg(host_ptr);
            }
            check(&self.cuda.syms, r, "cuMemHostGetDevicePointer")?;
        }

        Ok(PinnedHostMapping {
            ctx: self,
            host_ptr,
            dev_ptr,
            len,
        })
    }
}

/// Zero-copy registered host memory mapping (`cuMemHostRegister`).
///
/// ```compile_fail
/// fn requires_send<T: Send>() {}
/// requires_send::<ramshared_cuda::PinnedHostMapping<'static, 'static>>();
/// ```
///
/// Borrows the active [`Context`]. The host memory remains mapped into the
/// GPU virtual address space for the lifetime of this struct and is cleanly
/// unregistered via `cuMemHostUnregister` on `Drop`.
pub struct PinnedHostMapping<'c, 'a> {
    ctx: &'c Context<'a>,
    host_ptr: *mut c_void,
    dev_ptr: CuDevicePtr,
    len: usize,
}

impl<'c, 'a> PinnedHostMapping<'c, 'a> {
    /// Returns the mapped CUDA device virtual pointer.
    pub fn dev_ptr(&self) -> CuDevicePtr {
        self.dev_ptr
    }

    /// Returns the length in bytes of the registered mapping.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns true if the mapping is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns a host slice view of the registered memory.
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: host_ptr is non-null and points to at least len valid bytes.
        unsafe { core::slice::from_raw_parts(self.host_ptr as *const u8, self.len) }
    }

    /// Returns a mutable host slice view of the registered memory.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: host_ptr is non-null and points to at least len valid bytes.
        unsafe { core::slice::from_raw_parts_mut(self.host_ptr as *mut u8, self.len) }
    }
}

impl Drop for PinnedHostMapping<'_, '_> {
    fn drop(&mut self) {
        if let Some(fn_unreg) = self.ctx.cuda.syms.mem_host_unregister {
            // SAFETY: host_ptr was successfully registered by cuMemHostRegister.
            unsafe {
                let _ = fn_unreg(self.host_ptr);
            }
        }
    }
}

impl Drop for Context<'_> {
    fn drop(&mut self) {
        // SAFETY: raw handle was returned by cuCtxCreate and has not been destroyed yet. Best-effort drop.
        unsafe {
            let _ = (self.cuda.syms.ctx_destroy)(self.raw);
        }
    }
}

/// Allocated VRAM memory region. `Drop` implementation calls `cuMemFree`. Borrows the [`Context`].
pub struct DeviceMem<'c, 'a> {
    ctx: &'c Context<'a>,
    ptr: CuDevicePtr,
    len: usize,
}

impl DeviceMem<'_, '_> {
    pub fn device_ptr(&self) -> CuDevicePtr {
        self.ptr
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Fills the entire region with zeroes (`cuMemsetD8` + synchronize). SPEC §6.2/§11.
    pub fn zero(&mut self) -> Result<(), CudaError> {
        let syms = &self.ctx.cuda.syms;
        // SAFETY: ptr and len accurately describe the region allocated for this memory object.
        let r = unsafe { (syms.memset_d8)(self.ptr, 0, self.len) };
        check(syms, r, "cuMemsetD8")?;
        // SAFETY: cuCtxSynchronize takes no arguments.
        let r = unsafe { (syms.ctx_synchronize)() };
        check(syms, r, "cuCtxSynchronize")
    }

    /// Copies `src` bytes into VRAM at the specified `off` offset (Host->Device, synchronous).
    pub fn write_at(&mut self, off: usize, src: &[u8]) -> Result<(), CudaError> {
        self.bounds(off, src.len())?;
        let syms = &self.ctx.cuda.syms;
        // SAFETY: offset and length validated by bounds(); src is a valid memory slice.
        let r = unsafe {
            (syms.memcpy_htod)(
                self.ptr + off as u64,
                src.as_ptr() as *const c_void,
                src.len(),
            )
        };
        check(syms, r, "cuMemcpyHtoD")
    }

    /// Copies bytes from VRAM at `off` into the `dst` buffer (Device->Host, synchronous).
    pub fn read_at(&self, off: usize, dst: &mut [u8]) -> Result<(), CudaError> {
        self.bounds(off, dst.len())?;
        let syms = &self.ctx.cuda.syms;
        // SAFETY: offset and length validated by bounds(); dst is a valid mutable slice.
        let r = unsafe {
            (syms.memcpy_dtoh)(
                dst.as_mut_ptr() as *mut c_void,
                self.ptr + off as u64,
                dst.len(),
            )
        };
        check(syms, r, "cuMemcpyDtoH")
    }

    fn bounds(&self, off: usize, len: usize) -> Result<(), CudaError> {
        match off.checked_add(len) {
            Some(end) if end <= self.len => Ok(()),
            _ => Err(CudaError::OutOfRange {
                off,
                len,
                size: self.len,
            }),
        }
    }
}

impl Drop for DeviceMem<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: ptr was returned by a successful cuMemAlloc call and has not been freed.
        unsafe {
            let _ = (self.ctx.cuda.syms.mem_free)(self.ptr);
        }
    }
}

// --- internal helpers ---

/// SAFETY: `handle` must refer to a valid open library; `name` must be a valid C-string;
/// type `T` must be a C function pointer of pointer size.
pub(crate) unsafe fn load_sym<T: Copy>(handle: *mut c_void, name: &CStr) -> Result<T, CudaError> {
    // SAFETY: caller contract (valid handle, null-terminated symbol name).
    let sym = unsafe { crate::loader::sym(handle, name.as_ptr()) };
    if sym.is_null() {
        return Err(CudaError::Symbol(name.to_string_lossy().into_owned()));
    }
    const {
        assert!(core::mem::size_of::<T>() == core::mem::size_of::<*mut c_void>());
    }
    // SAFETY: T is a C function pointer of the same size as a raw pointer.
    Ok(unsafe { core::mem::transmute_copy::<*mut c_void, T>(&sym) })
}

/// Optional symbol resolution (symbol may be missing in legacy stubs).
pub(crate) fn load_sym_opt<T: Copy>(handle: *mut c_void, name: &CStr) -> Option<T> {
    // SAFETY: same preconditions as load_sym.
    unsafe { load_sym(handle, name).ok() }
}

fn check(syms: &Syms, r: CuResult, op: &'static str) -> Result<(), CudaError> {
    if r == CUDA_SUCCESS {
        Ok(())
    } else {
        Err(CudaError::Driver {
            op,
            code: r,
            msg: err_string(syms, r),
        })
    }
}

fn err_string(syms: &Syms, r: CuResult) -> String {
    if let Some(f) = syms.get_error_string {
        let mut p: *const c_char = core::ptr::null();
        // SAFETY: f is a resolved cuGetErrorString pointer; p is a valid out-pointer.
        unsafe { f(r, &mut p) };
        if !p.is_null() {
            // SAFETY: p points to a static null-terminated CUDA error message string.
            return unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
        }
    }
    format!("CUresult={r}")
}

#[cfg(test)]
pub(crate) mod tests;
