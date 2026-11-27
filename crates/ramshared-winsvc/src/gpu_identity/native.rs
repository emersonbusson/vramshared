// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/gpu_identity/native.rs
//! Native context-free CUDA identity and owned GDI interface queries.
use super::{HostGpuIdentityReport, IdentitySource, ProducerProcess, collect_identities};
use core::ffi::c_void;
use ramshared_vram::GpuAdapterIdentity;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetProcessTimes,
};

#[repr(C)]
#[derive(Clone, Copy, Eq, PartialEq)]
struct Luid {
    low: u32,
    high: u32,
}
#[repr(C)]
struct OpenAdapter {
    luid: Luid,
    handle: u32,
}
#[repr(C)]
struct QueryAdapter {
    handle: u32,
    kind: u32,
    data: *mut c_void,
    bytes: u32,
}
#[repr(C)]
struct CloseAdapter {
    handle: u32,
}
const _: () = assert!(std::mem::size_of::<Luid>() == 8);
const _: () = assert!(std::mem::size_of::<OpenAdapter>() == 12);
const _: () = assert!(std::mem::size_of::<QueryAdapter>() == 24);
const _: () = assert!(std::mem::size_of::<CloseAdapter>() == 4);
const INTERFACE_QUERY: u32 = 79;
type InterfaceBuffer = [u16; ramshared_vram::host_identity::INTERFACE_UTF16_CAPACITY];
const INTERFACE_BYTES: u32 = std::mem::size_of::<InterfaceBuffer>() as u32;
const _: () = assert!(INTERFACE_BYTES == 520);

#[link(name = "gdi32")]
unsafe extern "system" {
    fn D3DKMTOpenAdapterFromLuid(request: *mut OpenAdapter) -> i32;
    fn D3DKMTQueryAdapterInfo(request: *mut QueryAdapter) -> i32;
    fn D3DKMTCloseAdapter(request: *const CloseAdapter) -> i32;
}
trait AdapterApi {
    fn open(&mut self, request: &mut OpenAdapter) -> i32;
    fn query(&mut self, request: &mut QueryAdapter, output: &mut InterfaceBuffer) -> i32;
    fn close(&mut self, handle: u32) -> i32;
}
struct NativeGdi;
impl AdapterApi for NativeGdi {
    fn open(&mut self, request: &mut OpenAdapter) -> i32 {
        // SAFETY: writable exact SDK layout, kept alive through the native call.
        unsafe { D3DKMTOpenAdapterFromLuid(request) }
    }
    fn query(&mut self, request: &mut QueryAdapter, output: &mut InterfaceBuffer) -> i32 {
        if request.data != output.as_mut_ptr().cast()
            || request.bytes != INTERFACE_BYTES
            || request.kind != INTERFACE_QUERY
            || request.handle == 0
        {
            return -1;
        }
        // SAFETY: exact SDK layout and validated pointer to the live output buffer.
        unsafe { D3DKMTQueryAdapterInfo(request) }
    }
    fn close(&mut self, handle: u32) -> i32 {
        // SAFETY: one nonzero handle owned by this query; exact close layout.
        unsafe { D3DKMTCloseAdapter(&CloseAdapter { handle }) }
    }
}
struct OwnedAdapter<'a, A: AdapterApi> {
    api: &'a mut A,
    handle: u32,
    closed: bool,
}
impl<A: AdapterApi> OwnedAdapter<'_, A> {
    fn close(&mut self) -> i32 {
        self.closed = true;
        self.api.close(self.handle)
    }
}
impl<A: AdapterApi> Drop for OwnedAdapter<'_, A> {
    fn drop(&mut self) {
        if !self.closed {
            let _ = self.close();
        }
    }
}

fn query_interface(luid: &str, api: &mut impl AdapterApi) -> Result<String, String> {
    let (high, low) = luid.split_once(':').ok_or("missing native LUID")?;
    let high = u32::from_str_radix(high, 16).map_err(|_| "invalid native LUID")?;
    let low = u32::from_str_radix(low, 16).map_err(|_| "invalid native LUID")?;
    let expected = Luid { low, high };
    let mut opened = OpenAdapter {
        luid: expected,
        handle: 0,
    };
    let status = api.open(&mut opened);
    if opened.handle == 0 {
        return Err(format!("native adapter open returned no handle: {status}"));
    }
    let mut owned = OwnedAdapter {
        api,
        handle: opened.handle,
        closed: false,
    };
    let result = if status != 0 || opened.luid != expected {
        Err(format!(
            "native adapter open failed or changed LUID: {status}"
        ))
    } else {
        query_owned_interface(&mut owned)
    };
    let closed = owned.close();
    if closed != 0 {
        return Err(format!("native adapter close failed: {closed}"));
    }
    result
}
fn query_owned_interface(owned: &mut OwnedAdapter<'_, impl AdapterApi>) -> Result<String, String> {
    // A nonzero sentinel prevents an unwritten terminator from looking valid.
    let mut output = [u16::MAX; ramshared_vram::host_identity::INTERFACE_UTF16_CAPACITY];
    let data = output.as_mut_ptr().cast();
    let mut query = QueryAdapter {
        handle: owned.handle,
        kind: INTERFACE_QUERY,
        data,
        bytes: INTERFACE_BYTES,
    };
    let status = owned.api.query(&mut query, &mut output);
    if status != 0
        || query.handle != owned.handle
        || query.kind != INTERFACE_QUERY
        || query.data != data
        || query.bytes != INTERFACE_BYTES
    {
        return Err(format!(
            "native GPU interface query failed or changed scope: {status}"
        ));
    }
    let length = output
        .iter()
        .position(|unit| *unit == 0)
        .ok_or("unterminated native GPU interface")?;
    if length == 0 {
        return Err("empty native GPU interface".into());
    }
    String::from_utf16(&output[..length]).map_err(|_| "invalid native GPU interface UTF-16".into())
}
struct CudaIdentitySource {
    cuda: ramshared_cuda::Cuda,
}
impl IdentitySource for CudaIdentitySource {
    fn device_count(&mut self) -> Result<i32, String> {
        self.cuda.device_count().map_err(|e| e.to_string())
    }
    fn adapter_identity(&mut self, ordinal: u32) -> Result<GpuAdapterIdentity, String> {
        let ordinal = i32::try_from(ordinal).map_err(|_| "native CUDA ordinal overflow")?;
        self.cuda
            .device(ordinal)
            .map_err(|e| e.to_string())?
            .adapter_identity()
            .ok_or_else(|| "native CUDA identity missing".into())
    }
    fn adapter_interface(&mut self, identity: &GpuAdapterIdentity) -> Result<String, String> {
        query_interface(
            identity
                .luid
                .as_deref()
                .ok_or("native CUDA host LUID missing")?,
            &mut NativeGdi,
        )
    }
}
pub(crate) fn process_identity() -> Result<ProducerProcess, String> {
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: current-process pseudo handle and four distinct live output values.
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(ProducerProcess {
        // SAFETY: current-process identity query has no pointer arguments.
        pid: unsafe { GetCurrentProcessId() },
        creation_time_100ns: (u64::from(created.dwHighDateTime) << 32)
            | u64::from(created.dwLowDateTime),
    })
}
/// Native observation only. Call from a separately supervised producer invocation.
pub fn inspect_native() -> Result<HostGpuIdentityReport, String> {
    let start = Instant::now();
    let started_at_unix_ms = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_millis(),
    )
    .map_err(|_| "native observation clock overflow")?;
    let producer = process_identity()?;
    let mut source = CudaIdentitySource {
        cuda: ramshared_cuda::Cuda::load().map_err(|e| e.to_string())?,
    };
    let adapters = collect_identities(&mut source)?;
    // Native library shutdown belongs to this observation's duration as well.
    drop(source);
    let elapsed_ms =
        u64::try_from(start.elapsed().as_millis()).map_err(|_| "native query duration overflow")?;
    let report = HostGpuIdentityReport {
        schema_version: 1,
        producer,
        started_at_unix_ms,
        elapsed_ms,
        adapters,
    };
    report.validate()?;
    Ok(report)
}
#[cfg(test)]
#[path = "native_tests.rs"]
mod tests;
