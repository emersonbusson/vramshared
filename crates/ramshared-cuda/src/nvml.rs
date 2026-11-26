// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cuda/src/nvml.rs
//! NVML (NVIDIA Management Library) device-wide VRAM occupancy.
//!
//! `cuMemGetInfo` is the wrong authority for a VRAM budget on WSL2 GPU-PV: the
//! paravirtual CUDA shim accounts the calling process's channel, so free/total
//! never move when another process allocates device memory. A containment chain
//! built on that number cannot yield to a GPU application.
//!
//! `nvmlDeviceGetMemoryInfo` reports the whole adapter and does observe every
//! process, which is what the budget must describe. NVML ships with the same
//! NVIDIA driver package as `libcuda.so.1`, so it is loaded the same way: at
//! runtime, through the platform loader, with no build-time toolkit dependency.

use core::ffi::{c_char, c_int, c_uint, c_void};
use core::fmt;
use std::ffi::CStr;

use crate::driver::{Lib, load_sym, load_sym_opt};
mod adapter;

/// `nvmlReturn_t`. `NVML_SUCCESS` shares the zero value with `CUDA_SUCCESS`.
pub type NvmlResult = c_int;
/// `NVML_SUCCESS`.
pub const NVML_SUCCESS: NvmlResult = 0;

/// `nvmlDevice_t` — opaque device handle.
type NvmlDevice = *mut c_void;

/// `nvmlMemory_t` — device-wide memory accounting in bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct NvmlMemory {
    /// Total physical device memory.
    pub total: u64,
    /// Unallocated device memory.
    pub free: u64,
    /// Device memory allocated by all contexts on the adapter.
    pub used: u64,
}

/// `nvmlUtilization_t` — percent-of-time busy counters from the driver.
///
/// Both fields are whole percents in the range `0..=100`. The driver reports
/// them as `unsigned int`; [`GpuUtilization`] is the validated public form.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct NvmlUtilization {
    /// Percent of the sample window with at least one kernel executing.
    pub gpu: c_uint,
    /// Percent of the sample window with device memory being read or written.
    pub memory: c_uint,
}

const _: () = assert!(core::mem::size_of::<NvmlMemory>() == 24);
const _: () = assert!(core::mem::size_of::<NvmlUtilization>() == 8);

/// Device-wide kernel, memory, and optional video busy time from NVML.
///
/// A present value is a measurement: `gpu_pct: 0` means the GPU was observed
/// had no kernels executing over the sample window. Unavailability
/// (NVML absent, symbol missing, refusing call, malformed reply) is reported
/// through [`NvmlError`] and never as a zero sample.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GpuUtilization {
    /// Percent of the sample window with at least one kernel executing (`0..=100`).
    pub gpu_pct: u8,
    /// Percent of the sample window with device memory traffic (`0..=100`).
    pub memory_pct: u8,
    /// NVML video encoder busy time, when the driver supplies it.
    pub encoder_pct: Option<u8>,
    /// NVML video decoder busy time, when the driver supplies it.
    pub decoder_pct: Option<u8>,
}

impl GpuUtilization {
    /// Validates a driver reply into typed percents.
    ///
    /// Values outside `0..=100` are malformed: they are refused rather than
    /// clamped or reported, so a bad sample stays absent instead of becoming a
    /// fabricated utilization number.
    fn from_driver(raw: &NvmlUtilization) -> Result<Self, NvmlError> {
        Ok(Self {
            gpu_pct: percent(raw.gpu, "gpu")?,
            memory_pct: percent(raw.memory, "memory")?,
            encoder_pct: None,
            decoder_pct: None,
        })
    }
}

/// Converts one driver percent into `u8`, or refuses it as malformed.
fn percent(value: c_uint, field: &'static str) -> Result<u8, NvmlError> {
    if value > 100 {
        return Err(NvmlError::Call {
            op: "nvmlDeviceGetUtilizationRates",
            code: -1,
            msg: format!("malformed {field} percent {value} (expected 0..=100)"),
        });
    }
    // Bound-checked above: a percent cannot exceed 100.
    Ok(value as u8)
}

type FnNvmlInit = unsafe extern "C" fn() -> NvmlResult;
type FnNvmlShutdown = unsafe extern "C" fn() -> NvmlResult;
type FnNvmlDeviceGetHandleByIndex = unsafe extern "C" fn(c_uint, *mut NvmlDevice) -> NvmlResult;
type FnNvmlDeviceGetHandleByUuid =
    unsafe extern "C" fn(*const c_char, *mut NvmlDevice) -> NvmlResult;
type FnNvmlDeviceGetUuid = unsafe extern "C" fn(NvmlDevice, *mut c_char, c_uint) -> NvmlResult;
type FnNvmlDeviceGetMemoryInfo = unsafe extern "C" fn(NvmlDevice, *mut NvmlMemory) -> NvmlResult;
type FnNvmlDeviceGetVideoUtilization =
    unsafe extern "C" fn(NvmlDevice, *mut c_uint, *mut c_uint) -> NvmlResult;
type FnNvmlDeviceGetUtilizationRates =
    unsafe extern "C" fn(NvmlDevice, *mut NvmlUtilization) -> NvmlResult;
type FnNvmlErrorString = unsafe extern "C" fn(NvmlResult) -> *const c_char;

struct NvmlSyms {
    init: FnNvmlInit,
    shutdown: FnNvmlShutdown,
    device_get_handle_by_index: FnNvmlDeviceGetHandleByIndex,
    device_get_handle_by_uuid: FnNvmlDeviceGetHandleByUuid,
    device_get_uuid: FnNvmlDeviceGetUuid,
    device_get_memory_info: FnNvmlDeviceGetMemoryInfo,
    device_get_utilization_rates: FnNvmlDeviceGetUtilizationRates,
    device_get_encoder_utilization: Option<FnNvmlDeviceGetVideoUtilization>,
    device_get_decoder_utilization: Option<FnNvmlDeviceGetVideoUtilization>,
    error_string: Option<FnNvmlErrorString>,
}

/// A loaded and initialized NVML library.
pub struct Nvml {
    _lib: Lib,
    syms: NvmlSyms,
}

/// Error raised while loading NVML or reading device memory.
#[derive(Debug)]
pub enum NvmlError {
    /// No candidate library could be opened.
    Load(String),
    /// A required exported symbol is missing.
    Symbol(String),
    /// An NVML call returned a non-success status.
    Call {
        /// Operation that failed.
        op: &'static str,
        /// `nvmlReturn_t` code.
        code: NvmlResult,
        /// Driver-provided description, when available.
        msg: String,
    },
}

impl fmt::Display for NvmlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NvmlError::Load(s) => write!(f, "failed to load NVML library: {s}"),
            NvmlError::Symbol(s) => write!(f, "required NVML symbol missing: {s}"),
            NvmlError::Call { op, code, msg } => {
                write!(f, "{op} failed (nvmlReturn_t={code}): {msg}")
            }
        }
    }
}

impl std::error::Error for NvmlError {}

impl Nvml {
    /// Loads NVML and runs `nvmlInit_v2`.
    pub fn load() -> Result<Self, NvmlError> {
        let lib = crate::driver_library::load(crate::driver_library::DriverLibrary::Nvml)
            .map_err(NvmlError::Load)?;
        let handle = lib.0;

        // SAFETY: handle is an active library context; each resolved symbol is a
        // valid NVML function pointer conforming to the signatures above.
        let syms = unsafe {
            NvmlSyms {
                init: load_sym(handle, c"nvmlInit_v2").map_err(map_sym)?,
                shutdown: load_sym(handle, c"nvmlShutdown").map_err(map_sym)?,
                device_get_handle_by_index: load_sym(handle, c"nvmlDeviceGetHandleByIndex_v2")
                    .or_else(|_| load_sym(handle, c"nvmlDeviceGetHandleByIndex"))
                    .map_err(map_sym)?,
                device_get_handle_by_uuid: load_sym(handle, c"nvmlDeviceGetHandleByUUID")
                    .map_err(map_sym)?,
                device_get_uuid: load_sym(handle, c"nvmlDeviceGetUUID").map_err(map_sym)?,
                device_get_memory_info: load_sym(handle, c"nvmlDeviceGetMemoryInfo")
                    .map_err(map_sym)?,
                device_get_utilization_rates: load_sym(handle, c"nvmlDeviceGetUtilizationRates")
                    .map_err(map_sym)?,
                device_get_encoder_utilization: load_sym_opt(
                    handle,
                    c"nvmlDeviceGetEncoderUtilization",
                ),
                device_get_decoder_utilization: load_sym_opt(
                    handle,
                    c"nvmlDeviceGetDecoderUtilization",
                ),
                error_string: load_sym_opt(handle, c"nvmlErrorString"),
            }
        };

        // SAFETY: init symbol resolved successfully.
        let r = unsafe { (syms.init)() };
        if r != NVML_SUCCESS {
            return Err(NvmlError::Call {
                op: "nvmlInit_v2",
                code: r,
                msg: err_string(&syms, r),
            });
        }

        Ok(Nvml { _lib: lib, syms })
    }

    /// Returns memory for an index in NVML's own namespace. CUDA indices
    /// are not interchangeable; use `device_memory_for_adapter` for CUDA.
    ///
    /// `used` is the sum over every process on the adapter, which is exactly
    /// the occupancy a VRAM budget must track to yield to other applications.
    pub fn device_memory(&self, ordinal: i32) -> Result<NvmlMemory, NvmlError> {
        self.read_memory(self.device_handle(ordinal)?)
    }

    /// CUDA callers must bind the full driver UUID, never a CUDA ordinal.
    pub fn device_memory_for_adapter(
        &self,
        adapter: &ramshared_vram::GpuAdapterIdentity,
    ) -> Result<NvmlMemory, NvmlError> {
        self.read_memory(self.adapter_handle(adapter)?)
    }

    fn read_memory(&self, device: NvmlDevice) -> Result<NvmlMemory, NvmlError> {
        let mut memory = NvmlMemory::default();
        // SAFETY: device is a non-null handle resolved below and still owned by
        // the initialized NVML session; memory is a stack out-pointer valid for
        // the duration of this synchronous call. No aliasing with device.
        let r = unsafe { (self.syms.device_get_memory_info)(device, &mut memory) };
        self.check(r, "nvmlDeviceGetMemoryInfo")?;
        Ok(memory)
    }

    /// Returns utilization for an NVML-native index. CUDA ordinals belong to
    /// a different namespace; CUDA uses `device_utilization_for_adapter`.
    ///
    /// `gpu_pct` is the percent of the sample window with at least one kernel
    /// executing; `memory_pct` is the percent with device memory traffic.
    /// Both are measured values in `0..=100`: a returned `0` means the adapter
    /// was observed idle over the window.
    ///
    /// Every unavailability is an [`NvmlError`] and never a zero sample: NVML
    /// absent or the symbol missing fails at [`Nvml::load`], and a refusing or
    /// malformed call fails here. Callers that publish telemetry must map the
    /// error to "not measured" rather than to `0%`.
    pub fn device_utilization(&self, ordinal: i32) -> Result<GpuUtilization, NvmlError> {
        self.read_utilization(self.device_handle(ordinal)?)
    }

    /// Query utilization only after UUID lookup and independent handle readback.
    pub fn device_utilization_for_adapter(
        &self,
        adapter: &ramshared_vram::GpuAdapterIdentity,
    ) -> Result<GpuUtilization, NvmlError> {
        self.read_utilization(self.adapter_handle(adapter)?)
    }

    fn read_utilization(&self, device: NvmlDevice) -> Result<GpuUtilization, NvmlError> {
        let mut raw = NvmlUtilization::default();
        // SAFETY: device is a non-null handle resolved below and still owned by
        // the initialized NVML session; raw is a stack out-pointer valid for the
        // duration of this synchronous call. No aliasing with device.
        let r = unsafe { (self.syms.device_get_utilization_rates)(device, &mut raw) };
        self.check(r, "nvmlDeviceGetUtilizationRates")?;
        let mut sample = GpuUtilization::from_driver(&raw)?;
        sample.encoder_pct = video_utilization(self.syms.device_get_encoder_utilization, device);
        sample.decoder_pct = video_utilization(self.syms.device_get_decoder_utilization, device);
        Ok(sample)
    }

    /// Resolves `ordinal` to a non-null device handle, or refuses.
    fn device_handle(&self, ordinal: i32) -> Result<NvmlDevice, NvmlError> {
        if ordinal < 0 {
            return Err(NvmlError::Call {
                op: "nvmlDeviceGetHandleByIndex_v2",
                code: -1,
                msg: format!("negative device ordinal {ordinal}"),
            });
        }
        let mut device: NvmlDevice = core::ptr::null_mut();
        // SAFETY: device is a stack out-pointer for an opaque handle, valid for
        // the duration of this synchronous call.
        let r = unsafe { (self.syms.device_get_handle_by_index)(ordinal as c_uint, &mut device) };
        self.check(r, "nvmlDeviceGetHandleByIndex_v2")?;
        if device.is_null() {
            return Err(NvmlError::Call {
                op: "nvmlDeviceGetHandleByIndex_v2",
                code: -1,
                msg: "null device handle".into(),
            });
        }
        Ok(device)
    }

    fn check(&self, r: NvmlResult, op: &'static str) -> Result<(), NvmlError> {
        if r == NVML_SUCCESS {
            Ok(())
        } else {
            Err(NvmlError::Call {
                op,
                code: r,
                msg: err_string(&self.syms, r),
            })
        }
    }
}

impl Drop for Nvml {
    fn drop(&mut self) {
        // SAFETY: the library was initialized successfully and is still mapped.
        unsafe {
            (self.syms.shutdown)();
        }
    }
}

fn video_utilization(
    read: Option<FnNvmlDeviceGetVideoUtilization>,
    device: NvmlDevice,
) -> Option<u8> {
    let read = read?;
    let mut utilization = c_uint::MAX;
    let mut sampling_period_us = 0;
    // SAFETY: the caller passes a live NVML device handle. Both outputs are
    // independent, writable stack values with the exact NVML ABI types.
    let result = unsafe { read(device, &mut utilization, &mut sampling_period_us) };
    (result == NVML_SUCCESS && utilization <= 100).then_some(utilization as u8)
}

fn map_sym(error: crate::driver::CudaError) -> NvmlError {
    match error {
        crate::driver::CudaError::Symbol(name) => NvmlError::Symbol(name),
        other => NvmlError::Symbol(other.to_string()),
    }
}

fn err_string(syms: &NvmlSyms, r: NvmlResult) -> String {
    if let Some(f) = syms.error_string {
        // SAFETY: f is a resolved nvmlErrorString; the returned pointer is a
        // driver-owned NUL-terminated string valid for the duration of the call.
        let p = unsafe { f(r) };
        if !p.is_null() {
            // SAFETY: p is a non-null NUL-terminated C string from the driver.
            let s = unsafe { CStr::from_ptr(p) };
            return s.to_string_lossy().into_owned();
        }
    }
    format!("nvmlReturn_t={r}")
}

/// Test-only NVML whose device-wide numbers are injected.
///
/// A budget test must be able to make NVML disagree with `cuMemGetInfo` on
/// purpose: that divergence is exactly the WSL2 GPU-PV failure this module
/// exists to correct, and a mock that cannot express it cannot guard the fix.
#[cfg(test)]
pub(crate) mod mock {
    use super::*;
    use core::cell::Cell;

    thread_local! {
        static MEM: Cell<(u64, u64, u64)> = const { Cell::new((0, 0, 0)) };
        static MEM_RESULT: Cell<NvmlResult> = const { Cell::new(NVML_SUCCESS) };
        static UTIL: Cell<(c_uint, c_uint)> = const { Cell::new((0, 0)) };
        static UTIL_RESULT: Cell<NvmlResult> = const { Cell::new(NVML_SUCCESS) };
        static HANDLE_NULL: Cell<bool> = const { Cell::new(false) };
        static ERROR_STRING_NULL: Cell<bool> = const { Cell::new(false) };
    }

    /// Sets `(used, free, total)` returned by the mock, in bytes.
    pub(crate) fn set_device_memory(used: u64, free: u64, total: u64) {
        MEM.with(|cell| cell.set((used, free, total)));
    }

    /// Makes the mock `nvmlDeviceGetMemoryInfo` return `code` instead of
    /// success. A budget must handle NVML refusing the call, not only a
    /// happy reading.
    pub(crate) fn set_memory_result(code: NvmlResult) {
        MEM_RESULT.with(|cell| cell.set(code));
    }

    /// Sets `(gpu, memory)` percents returned by the mock. Raw `c_uint` so a
    /// test can also inject a malformed out-of-range reply.
    pub(crate) fn set_device_utilization(gpu: c_uint, memory: c_uint) {
        UTIL.with(|cell| cell.set((gpu, memory)));
    }

    /// Makes the mock `nvmlDeviceGetUtilizationRates` return `code` instead of
    /// success. Utilization must handle NVML refusing the call, not only a
    /// happy reading.
    pub(crate) fn set_utilization_result(code: NvmlResult) {
        UTIL_RESULT.with(|cell| cell.set(code));
    }

    /// Makes the mock `nvmlDeviceGetHandleByIndex` succeed and leave the
    /// device handle null, which is how a driver signals an unusable index
    /// while still returning `NVML_SUCCESS`.
    pub(crate) fn set_handle_returns_null(yes: bool) {
        HANDLE_NULL.with(|cell| cell.set(yes));
    }

    /// Makes the mock `nvmlErrorString` return a null pointer, which is how a
    /// driver declines to describe a status code.
    pub(crate) fn set_error_string_returns_null(yes: bool) {
        ERROR_STRING_NULL.with(|cell| cell.set(yes));
    }

    unsafe extern "C" fn mock_init() -> NvmlResult {
        NVML_SUCCESS
    }

    unsafe extern "C" fn mock_shutdown() -> NvmlResult {
        NVML_SUCCESS
    }

    unsafe extern "C" fn mock_handle(ordinal: c_uint, device: *mut NvmlDevice) -> NvmlResult {
        // SAFETY: device is a valid out-pointer provided by the caller.
        if HANDLE_NULL.with(|cell| cell.get()) {
            unsafe { *device = core::ptr::null_mut() };
            return NVML_SUCCESS;
        }
        unsafe { *device = (ordinal as usize + 1) as NvmlDevice };
        NVML_SUCCESS
    }

    unsafe extern "C" fn mock_memory(device: NvmlDevice, memory: *mut NvmlMemory) -> NvmlResult {
        if device.is_null() {
            return -1;
        }
        let code = MEM_RESULT.with(|cell| cell.get());
        if code != NVML_SUCCESS {
            return code;
        }
        let (used, free, total) = MEM.with(|cell| cell.get());
        // SAFETY: memory is a valid out-pointer provided by the caller.
        unsafe { *memory = NvmlMemory { total, free, used } };
        NVML_SUCCESS
    }

    unsafe extern "C" fn mock_utilization(
        device: NvmlDevice,
        utilization: *mut NvmlUtilization,
    ) -> NvmlResult {
        if device.is_null() {
            return -1;
        }
        let code = UTIL_RESULT.with(|cell| cell.get());
        if code != NVML_SUCCESS {
            return code;
        }
        let (gpu, memory) = UTIL.with(|cell| cell.get());
        // SAFETY: utilization is a valid out-pointer provided by the caller.
        unsafe { *utilization = NvmlUtilization { gpu, memory } };
        NVML_SUCCESS
    }

    unsafe extern "C" fn mock_error_string(_r: NvmlResult) -> *const c_char {
        if ERROR_STRING_NULL.with(|cell| cell.get()) {
            return core::ptr::null();
        }
        c"mock nvml error".as_ptr()
    }

    unsafe extern "C" fn reordered_memory(
        device: NvmlDevice,
        memory: *mut NvmlMemory,
    ) -> NvmlResult {
        let (total, used) = match device as usize {
            1 => (32 * 1024_u64.pow(3), 20 * 1024_u64.pow(3)),
            2 => (96 * 1024_u64.pow(3), 90 * 1024_u64.pow(3)),
            _ => return -1,
        };
        // SAFETY: the caller supplies a writable NvmlMemory output.
        unsafe {
            *memory = NvmlMemory {
                total,
                used,
                free: total - used,
            }
        };
        NVML_SUCCESS
    }

    unsafe extern "C" fn reordered_utilization(
        device: NvmlDevice,
        output: *mut NvmlUtilization,
    ) -> NvmlResult {
        let (gpu, memory) = match device as usize {
            1 => (93, 81),
            2 => (11, 31),
            _ => return -1,
        };
        // SAFETY: the caller supplies a writable NvmlUtilization output.
        unsafe { *output = NvmlUtilization { gpu, memory } };
        NVML_SUCCESS
    }

    /// CUDA ordinal zero names UUID A; NVML index zero names physical B.
    pub(crate) fn build_reordered() -> Nvml {
        let mut nvml = build();
        nvml.syms.device_get_handle_by_uuid = reordered_uuid_handle;
        nvml.syms.device_get_uuid = reordered_uuid_readback;
        nvml.syms.device_get_memory_info = reordered_memory;
        nvml.syms.device_get_utilization_rates = reordered_utilization;
        nvml
    }

    const A: &CStr = c"GPU-12000000-0000-0000-0000-000000000000";
    const B: &CStr = c"GPU-34000000-0000-0000-0000-000000000000";
    unsafe extern "C" fn uuid_handle(uuid: *const c_char, device: *mut NvmlDevice) -> NvmlResult {
        // SAFETY: the caller supplies a live terminated input and writable output.
        if unsafe { CStr::from_ptr(uuid) } != A {
            return -6;
        }
        unsafe {
            *device = if HANDLE_NULL.with(|c| c.get()) {
                core::ptr::null_mut()
            } else {
                1usize as NvmlDevice
            }
        };
        NVML_SUCCESS
    }
    unsafe extern "C" fn reordered_uuid_handle(
        uuid: *const c_char,
        device: *mut NvmlDevice,
    ) -> NvmlResult {
        // SAFETY: the caller supplies a live terminated input and writable output.
        let uuid = unsafe { CStr::from_ptr(uuid) };
        let handle = if uuid == A {
            2
        } else if uuid == B {
            1
        } else {
            return -6;
        };
        unsafe { *device = handle as NvmlDevice };
        NVML_SUCCESS
    }
    unsafe extern "C" fn uuid_readback(
        _: NvmlDevice,
        output: *mut c_char,
        length: c_uint,
    ) -> NvmlResult {
        if (length as usize) < A.to_bytes_with_nul().len() {
            return -1;
        }
        // SAFETY: output has the caller-reported length; the source is a static C string.
        unsafe { core::ptr::copy_nonoverlapping(A.as_ptr(), output, A.to_bytes_with_nul().len()) };
        NVML_SUCCESS
    }
    unsafe extern "C" fn reordered_uuid_readback(
        device: NvmlDevice,
        output: *mut c_char,
        length: c_uint,
    ) -> NvmlResult {
        let uuid = match device as usize {
            1 => B,
            2 => A,
            _ => return -6,
        };
        if (length as usize) < uuid.to_bytes_with_nul().len() {
            return -1;
        }
        // SAFETY: output has the caller-reported length; the source is a static C string.
        unsafe {
            core::ptr::copy_nonoverlapping(uuid.as_ptr(), output, uuid.to_bytes_with_nul().len())
        };
        NVML_SUCCESS
    }

    unsafe extern "C" fn delayed_memory(device: NvmlDevice, output: *mut NvmlMemory) -> NvmlResult {
        std::thread::sleep(std::time::Duration::from_millis(20));
        // SAFETY: this forwards the caller's valid output and opaque fixture handle.
        unsafe { reordered_memory(device, output) }
    }
    pub(crate) fn build_delayed() -> Nvml {
        let mut nvml = build_reordered();
        nvml.syms.device_get_memory_info = delayed_memory;
        nvml
    }

    /// Builds a mock NVML handle for unit tests. `nvmlErrorString` is left
    /// unresolved, so callers exercise the numeric fallback.
    pub(crate) fn build() -> Nvml {
        Nvml {
            _lib: Lib(core::ptr::null_mut()),
            syms: NvmlSyms {
                init: mock_init,
                shutdown: mock_shutdown,
                device_get_handle_by_index: mock_handle,
                device_get_handle_by_uuid: uuid_handle,
                device_get_uuid: uuid_readback,
                device_get_memory_info: mock_memory,
                device_get_utilization_rates: mock_utilization,
                device_get_encoder_utilization: None,
                device_get_decoder_utilization: None,
                error_string: None,
            },
        }
    }

    /// Builds a mock NVML whose `nvmlErrorString` symbol is resolved, so the
    /// driver-provided description path is exercised as well as the fallback.
    pub(crate) fn build_with_error_string() -> Nvml {
        Nvml {
            _lib: Lib(core::ptr::null_mut()),
            syms: NvmlSyms {
                init: mock_init,
                shutdown: mock_shutdown,
                device_get_handle_by_index: mock_handle,
                device_get_handle_by_uuid: uuid_handle,
                device_get_uuid: uuid_readback,
                device_get_memory_info: mock_memory,
                device_get_utilization_rates: mock_utilization,
                device_get_encoder_utilization: None,
                device_get_decoder_utilization: None,
                error_string: Some(mock_error_string),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    // unwrap/expect allowed in tests only (coding.md rules).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// Restores mock defaults when the injecting test ends, including on
    /// panic, so a failure does not poison later tests on the
    /// single-threaded coverage runner.
    struct MockReset;

    impl Drop for MockReset {
        fn drop(&mut self) {
            mock::set_memory_result(NVML_SUCCESS);
            mock::set_utilization_result(NVML_SUCCESS);
            mock::set_device_utilization(0, 0);
            mock::set_handle_returns_null(false);
            mock::set_error_string_returns_null(false);
        }
    }

    #[test]
    fn video_utilization_reports_valid_counters_independently() {
        unsafe extern "C" fn encoder(
            _: NvmlDevice,
            output: *mut c_uint,
            period: *mut c_uint,
        ) -> NvmlResult {
            // SAFETY: the sampler provides writable out-parameters.
            unsafe {
                *output = 0;
                *period = 100_000;
            }
            NVML_SUCCESS
        }

        unsafe extern "C" fn malformed_decoder(
            _: NvmlDevice,
            output: *mut c_uint,
            _: *mut c_uint,
        ) -> NvmlResult {
            // SAFETY: the sampler provides a writable utilization output.
            unsafe { *output = 101 };
            NVML_SUCCESS
        }

        unsafe extern "C" fn unsupported(
            _: NvmlDevice,
            _: *mut c_uint,
            _: *mut c_uint,
        ) -> NvmlResult {
            -3
        }

        let mut nvml = mock::build();
        nvml.syms.device_get_encoder_utilization = Some(encoder);
        nvml.syms.device_get_decoder_utilization = Some(malformed_decoder);
        let sample = nvml.device_utilization(0).unwrap();
        assert_eq!((sample.encoder_pct, sample.decoder_pct), (Some(0), None));

        nvml.syms.device_get_encoder_utilization = Some(unsupported);
        nvml.syms.device_get_decoder_utilization = Some(encoder);
        let sample = nvml.device_utilization(0).unwrap();
        assert_eq!((sample.encoder_pct, sample.decoder_pct), (None, Some(0)));
    }

    #[test]
    fn nvml_memory_layout_matches_driver_struct() {
        // nvmlMemory_t is three unsigned long long fields with no padding.
        assert_eq!(core::mem::size_of::<NvmlMemory>(), 24);
        assert_eq!(core::mem::align_of::<NvmlMemory>(), 8);
        assert_eq!(core::mem::offset_of!(NvmlMemory, total), 0);
        assert_eq!(core::mem::offset_of!(NvmlMemory, free), 8);
        assert_eq!(core::mem::offset_of!(NvmlMemory, used), 16);
    }

    #[test]
    fn error_display_is_descriptive() {
        let e = NvmlError::Call {
            op: "nvmlDeviceGetMemoryInfo",
            code: 3,
            msg: "not supported".into(),
        };
        let s = e.to_string();
        assert!(s.contains("nvmlDeviceGetMemoryInfo"));
        assert!(s.contains("nvmlReturn_t=3"));
    }

    #[test]
    fn display_covers_load_and_symbol_variants() {
        let load = NvmlError::Load("no candidate opened".into());
        assert_eq!(
            load.to_string(),
            "failed to load NVML library: no candidate opened"
        );

        let symbol = NvmlError::Symbol("nvmlInit_v2".into());
        assert_eq!(
            symbol.to_string(),
            "required NVML symbol missing: nvmlInit_v2"
        );
    }

    #[test]
    fn map_sym_preserves_symbol_names_and_stringifies_other_errors() {
        let named = map_sym(crate::driver::CudaError::Symbol("nvmlInit_v2".into()));
        match named {
            NvmlError::Symbol(name) => assert_eq!(name, "nvmlInit_v2"),
            other => panic!("expected Symbol variant, got {other:?}"),
        }

        // A non-Symbol CudaError is still surfaced as a missing symbol,
        // keeping the NVML error vocabulary to the two load-time causes.
        let other = map_sym(crate::driver::CudaError::Load("candidate.so".into()));
        let s = other.to_string();
        assert!(s.contains("required NVML symbol missing"), "got: {s}");
        assert!(s.contains("candidate.so"), "got: {s}");
    }

    #[test]
    fn negative_ordinal_is_rejected_before_any_driver_call() {
        let _reset = MockReset;
        let nvml = mock::build();
        let e = nvml.device_memory(-1).unwrap_err();
        assert!(e.to_string().contains("negative device ordinal -1"));
    }

    #[test]
    fn null_device_handle_is_reported_not_followed() {
        let _reset = MockReset;
        mock::set_handle_returns_null(true);
        let nvml = mock::build();
        let e = nvml.device_memory(0).unwrap_err();
        let s = e.to_string();
        assert!(s.contains("null device handle"), "got: {s}");
        assert!(s.contains("nvmlDeviceGetHandleByIndex_v2"), "got: {s}");
    }

    #[test]
    fn check_rejects_non_success_and_falls_back_without_error_string() {
        let _reset = MockReset;
        mock::set_memory_result(-3);
        let nvml = mock::build();
        let e = nvml.device_memory(0).unwrap_err();
        // error_string is unresolved on this mock, so the fallback names the code.
        let s = e.to_string();
        assert!(s.contains("nvmlDeviceGetMemoryInfo"), "got: {s}");
        assert!(s.contains("nvmlReturn_t=-3"), "got: {s}");
    }

    #[test]
    fn err_string_prefers_driver_description_and_survives_null() {
        let _reset = MockReset;
        mock::set_memory_result(-5);
        let nvml = mock::build_with_error_string();
        let e = nvml.device_memory(0).unwrap_err();
        let s = e.to_string();
        assert!(s.contains("mock nvml error"), "got: {s}");

        // A driver that declines to describe the code falls back to the numeric.
        mock::set_error_string_returns_null(true);
        let e = nvml.device_memory(0).unwrap_err();
        let s = e.to_string();
        assert!(s.contains("nvmlReturn_t=-5"), "got: {s}");
    }

    #[test]
    fn nvml_utilization_layout_matches_driver_struct() {
        // nvmlUtilization_t is two unsigned int fields with no padding.
        assert_eq!(core::mem::size_of::<NvmlUtilization>(), 8);
        assert_eq!(core::mem::align_of::<NvmlUtilization>(), 4);
        assert_eq!(core::mem::offset_of!(NvmlUtilization, gpu), 0);
        assert_eq!(core::mem::offset_of!(NvmlUtilization, memory), 4);
    }

    /// Positive: a bind that returns valid rates parses into typed percents.
    #[test]
    fn device_utilization_parses_valid_rates() {
        let _reset = MockReset;
        mock::set_device_utilization(37, 82);
        let nvml = mock::build();
        let rates = nvml.device_utilization(0).expect("valid rates must parse");
        assert_eq!(
            rates,
            GpuUtilization {
                gpu_pct: 37,
                memory_pct: 82,
                encoder_pct: None,
                decoder_pct: None,
            }
        );

        // A measured idle is a real zero sample, not an absence.
        mock::set_device_utilization(0, 0);
        let idle = nvml.device_utilization(0).expect("idle must parse");
        assert_eq!(
            idle,
            GpuUtilization {
                gpu_pct: 0,
                memory_pct: 0,
                encoder_pct: None,
                decoder_pct: None,
            }
        );
    }

    /// Negative: an unusable NVML is honest absence, never a zero sample.
    ///
    /// Callers that map this error to `0%` would show an idle GPU while the
    /// adapter is busy; the error is the only "not measured" signal.
    #[test]
    fn device_utilization_unavailable_is_absence_not_zero() {
        let _reset = MockReset;
        mock::set_utilization_result(-3);
        let nvml = mock::build();
        let sample = nvml.device_utilization(0);
        assert!(
            sample.is_err(),
            "refusing call must be absence, got {sample:?}"
        );
        assert!(
            !matches!(
                sample,
                Ok(GpuUtilization {
                    gpu_pct: 0,
                    memory_pct: 0,
                    ..
                })
            ),
            "absence must not collapse into a measured idle"
        );
    }

    /// Negative: a malformed driver reply is absence, not a clamped percent.
    #[test]
    fn device_utilization_malformed_rates_are_absence_not_clamped() {
        let _reset = MockReset;
        let nvml = mock::build();

        mock::set_device_utilization(101, 50);
        let e = nvml.device_utilization(0).unwrap_err();
        let s = e.to_string();
        assert!(s.contains("malformed gpu percent 101"), "got: {s}");
        assert!(s.contains("nvmlDeviceGetUtilizationRates"), "got: {s}");

        mock::set_device_utilization(50, 255);
        let e = nvml.device_utilization(0).unwrap_err();
        let s = e.to_string();
        assert!(s.contains("malformed memory percent 255"), "got: {s}");
    }

    #[test]
    fn device_utilization_rejects_negative_ordinal_and_null_handle() {
        let _reset = MockReset;
        let nvml = mock::build();
        let e = nvml.device_utilization(-1).unwrap_err();
        assert!(e.to_string().contains("negative device ordinal -1"));

        mock::set_handle_returns_null(true);
        let e = nvml.device_utilization(0).unwrap_err();
        let s = e.to_string();
        assert!(s.contains("null device handle"), "got: {s}");
        assert!(s.contains("nvmlDeviceGetHandleByIndex_v2"), "got: {s}");
    }

    /// Real NVML load against whatever NVIDIA stack the host provides. With
    /// the driver present this covers candidate selection, symbol resolution,
    /// `nvmlInit_v2`, and a real device read; without it, the candidate loop
    /// failure path. Both outcomes are legitimate and the assertions pin
    /// whichever happened.
    #[test]
    fn load_reports_whatever_the_host_nvidia_stack_provides() {
        match Nvml::load() {
            Ok(nvml) => {
                let mem = nvml.device_memory(0).expect("device 0 must report memory");
                assert!(
                    mem.total > 0,
                    "device 0 must report non-zero total: {mem:?}"
                );
                assert!(mem.used <= mem.total, "occupancy must be coherent: {mem:?}");
            }
            Err(e) => {
                let s = e.to_string();
                assert!(
                    s.starts_with("failed to load NVML library")
                        || s.starts_with("required NVML symbol missing")
                        || s.contains("nvmlInit_v2"),
                    "load failure must be descriptive, got: {s}"
                );
            }
        }
    }
}
