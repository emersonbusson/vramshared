// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cuda/src/nvml/adapter.rs
//! CUDA/NVML correspondence through full UUID and driver handle readback.
use super::{CStr, Nvml, NvmlDevice, NvmlError, c_char, c_uint};
use ramshared_vram::GpuAdapterIdentity;
use std::ffi::CString;

// NVML_DEVICE_UUID_V2_BUFFER_SIZE: SDK bound for nvmlDeviceGetUUID.
const UUID_BUFFER_BYTES: usize = 96;
fn invalid(op: &'static str, message: &'static str) -> NvmlError {
    NvmlError::Call {
        op,
        code: -1,
        msg: message.into(),
    }
}
impl Nvml {
    pub(super) fn adapter_handle(
        &self,
        adapter: &GpuAdapterIdentity,
    ) -> Result<NvmlDevice, NvmlError> {
        let expected = adapter.nvidia_uuid().ok_or_else(|| {
            invalid(
                "nvmlDeviceGetHandleByUUID",
                "CUDA budget requires a complete stable NVIDIA UUID",
            )
        })?;
        let expected = CString::new(expected)
            .map_err(|_| invalid("nvmlDeviceGetHandleByUUID", "invalid NVIDIA UUID encoding"))?;
        let mut device: NvmlDevice = core::ptr::null_mut();
        // SAFETY: the initialized session owns the function table; expected is
        // a live terminated C string and device is a writable stack output.
        let result =
            unsafe { (self.syms.device_get_handle_by_uuid)(expected.as_ptr(), &mut device) };
        self.check(result, "nvmlDeviceGetHandleByUUID")?;
        if device.is_null() {
            return Err(invalid("nvmlDeviceGetHandleByUUID", "null device handle"));
        }
        let mut observed = [0 as c_char; UUID_BUFFER_BYTES];
        // SAFETY: device is a non-null handle from this session. observed has
        // exactly UUID_BUFFER_BYTES writable bytes and outlives the call.
        let result = unsafe {
            (self.syms.device_get_uuid)(device, observed.as_mut_ptr(), UUID_BUFFER_BYTES as c_uint)
        };
        self.check(result, "nvmlDeviceGetUUID")?;
        let observed = observed.map(|byte| byte as u8);
        let observed = CStr::from_bytes_until_nul(&observed)
            .map_err(|_| invalid("nvmlDeviceGetUUID", "NVML UUID readback is not terminated"))?;
        if !observed
            .to_bytes()
            .eq_ignore_ascii_case(expected.to_bytes())
        {
            return Err(invalid(
                "nvmlDeviceGetUUID",
                "NVML handle UUID differs from the CUDA adapter",
            ));
        }
        Ok(device)
    }
}
#[cfg(test)]
mod tests;
