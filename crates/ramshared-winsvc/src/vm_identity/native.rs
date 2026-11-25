// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/vm_identity/native.rs
//! Read-only ComputeCore query. Call only inside the supervised VM probe child.
use super::api::Api;
use super::{MAX_VM_REPORT_BYTES, VmIdentityReport, project_systems};
use crate::gpu_identity::ProducerProcess;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use windows_sys::Win32::Foundation::{FILETIME, LocalFree};
use windows_sys::Win32::System::HostComputeSystem::HCS_OPERATION;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetProcessTimes,
};
struct Operation<'a> {
    api: &'a Api,
    handle: HCS_OPERATION,
}
impl Drop for Operation<'_> {
    fn drop(&mut self) {
        // SAFETY: unique nonnull tracking handle; no callbacks were installed.
        unsafe { (self.api.close)(self.handle) };
    }
}
struct Document(*mut u16);
impl Drop for Document {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: API transfers this result allocation; documented LocalFree owner.
            let _ = unsafe { LocalFree(self.0.cast()) };
        }
    }
}
fn check(result: i32) -> Result<(), String> {
    if result < 0 {
        Err(format!(
            "ComputeCore query failed: HRESULT 0x{:08x}",
            result as u32
        ))
    } else {
        Ok(())
    }
}
fn query() -> Result<Vec<u8>, String> {
    // SAFETY: null optional context/callback. This child owns the returned operation.
    let api = Api::load()?;
    let operation = Operation {
        api: &api,
        handle: unsafe { (api.create)(std::ptr::null(), None) },
    };
    if operation.handle.is_null() {
        return Err("ComputeCore operation unavailable".into());
    }
    let query: Vec<u16> = r#"{"Types":["VirtualMachine"]}"#.encode_utf16().chain([0]).collect();
    // SAFETY: live NUL-terminated query and actual tracking handle; read-only enumeration.
    check(unsafe { (api.enumerate)(query.as_ptr(), operation.handle) })?;
    let mut document = Document(std::ptr::null_mut());
    // SAFETY: actual operation and live PWSTR output. API timeout is not exit/cancel proof;
    // the serving parent independently supervises this entire child process/job.
    check(unsafe { (api.wait)(operation.handle, 2000, &mut document.0) })?;
    if document.0.is_null() {
        return Err("native VM query returned no document".into());
    }
    let mut units = Vec::new();
    for offset in 0..=MAX_VM_REPORT_BYTES {
        // SAFETY: successful API result is a valid NUL-terminated UTF-16 allocation;
        // stop at NUL and impose a finite copy bound before conversion/JSON parsing.
        let unit = unsafe { *document.0.add(offset) };
        if unit == 0 {
            let text = String::from_utf16(&units).map_err(|error| error.to_string())?;
            if text.len() > MAX_VM_REPORT_BYTES {
                return Err("native VM properties exceed their byte bound".into());
            }
            return Ok(text.into_bytes());
        }
        units.push(unit);
    }
    Err("native VM properties exceed their UTF-16 bound".into())
}
pub(super) fn inspect() -> Result<VmIdentityReport, String> {
    let started = Instant::now();
    let started_at_unix_ms = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_millis(),
    )
    .map_err(|error| error.to_string())?;
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: pseudo handle for this child and four distinct exact SDK outputs.
    if unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let systems = project_systems(&query()?)?;
    let report = VmIdentityReport {
        schema_version: 1,
        // SAFETY: returns this child's process ID; no PID-based reopening.
        producer: ProducerProcess {
            pid: unsafe { GetCurrentProcessId() },
            creation_time_100ns: (u64::from(created.dwHighDateTime) << 32)
                | u64::from(created.dwLowDateTime),
        },
        started_at_unix_ms,
        elapsed_ms: u64::try_from(started.elapsed().as_millis())
            .map_err(|error| error.to_string())?,
        systems,
    };
    report.validate()?;
    Ok(report)
}
