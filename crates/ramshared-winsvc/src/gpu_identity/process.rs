// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/gpu_identity/process.rs
//! Actual Windows process/job custody and exclusive nonblocking pipe reads.
mod pipe;
mod spawn;
use super::ProducerProcess;
use super::supervisor::{ExitObservation, PipeRead, Producer};
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::Path;
use windows_sys::Win32::Foundation::{FILETIME, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::JobObjects::{
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation,
    QueryInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, GetProcessId, GetProcessTimes, WaitForSingleObject,
};

const _: () = assert!(std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() == 48);
/// Keep in the process-lifetime supervisor until native exit and an empty job.
/// Closing the job on parent death contains all members; it does not prove exit.
pub struct NativeProcess {
    process: OwnedHandle,
    job: OwnedHandle,
    stdout: pipe::AsyncPipe,
    stderr: pipe::AsyncPipe,
}
impl NativeProcess {
    pub fn spawn_identity(executable: &Path) -> Result<Self, String> {
        spawn::spawn(executable, "inspect-gpus --json")
    }
    pub(crate) fn spawn_gpu_activity(executable: &Path) -> Result<Self, String> {
        spawn::spawn(executable, "inspect-gpu-activity --supervised-json")
    }
    pub fn spawn_vm_identity(executable: &Path) -> Result<Self, String> {
        spawn::spawn(executable, "inspect-vms --json")
    }
    /// Start one read-only storage observer inside the same private job and
    /// bounded pipe custody as the existing native producers.
    pub fn spawn_storage(executable: &Path, file: &Path) -> Result<Self, String> {
        spawn::spawn(executable, &storage_arguments(file)?)
    }
    pub fn spawn_storage_disk(executable: &Path, disk_id: [u8; 16]) -> Result<Self, String> {
        spawn::spawn(executable, &storage_disk_arguments(disk_id))
    }
    #[cfg(test)]
    fn spawn_fixture(executable: &Path, arguments: &str) -> Result<Self, String> {
        spawn::spawn(executable, arguments)
    }
}
pub(crate) fn storage_arguments(file: &Path) -> Result<String, String> {
    let path = file
        .to_str()
        .ok_or("storage observer path is not Unicode")?;
    if !file.is_absolute()
        || path.is_empty()
        || path.len() > 16 * 1024
        || path.ends_with('\\')
        || path.ends_with('/')
        || path.contains('"')
        || path.chars().any(char::is_control)
    {
        return Err("storage observer requires one bounded absolute file path".into());
    }
    Ok(format!("inspect-storage --file \"{path}\""))
}
pub(crate) fn storage_disk_arguments(disk_id: [u8; 16]) -> String {
    format!(
        "inspect-storage --disk-id {}",
        crate::host_storage::format_disk_id_hex(&disk_id)
    )
}

fn last_error() -> String {
    std::io::Error::last_os_error().to_string()
}
fn check(ok: i32) -> Result<(), String> {
    if ok == 0 { Err(last_error()) } else { Ok(()) }
}
impl Producer for NativeProcess {
    fn identity(&self) -> Result<ProducerProcess, String> {
        let (mut created, mut exited, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        // SAFETY: owned live process handle and four distinct exact SDK outputs.
        check(unsafe {
            GetProcessTimes(
                self.process.as_raw_handle(),
                &mut created,
                &mut exited,
                &mut kernel,
                &mut user,
            )
        })?;
        // SAFETY: same owned process handle, not a PID lookup.
        let pid = unsafe { GetProcessId(self.process.as_raw_handle()) };
        let creation_time_100ns =
            (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
        if pid == 0 || creation_time_100ns == 0 {
            return Err("native process identity is unavailable".into());
        }
        Ok(ProducerProcess {
            pid,
            creation_time_100ns,
        })
    }
    fn observe_exit(&mut self) -> Result<ExitObservation, String> {
        // SAFETY: process handle stays owned; zero wait never waits for a child.
        let event = unsafe { WaitForSingleObject(self.process.as_raw_handle(), 0) };
        let code = match event {
            WAIT_TIMEOUT => None,
            WAIT_OBJECT_0 => {
                let mut code = 0;
                // SAFETY: signalled owned process handle and live DWORD output.
                check(unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &mut code) })?;
                Some(code)
            }
            _ => return Err(last_error()),
        };
        let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        let mut returned = 0;
        // SAFETY: owned private job, exact SDK class/layout and output length.
        check(unsafe {
            QueryInformationJobObject(
                self.job.as_raw_handle(),
                JobObjectBasicAccountingInformation,
                (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                std::mem::size_of_val(&info) as u32,
                &mut returned,
            )
        })?;
        if returned as usize != std::mem::size_of_val(&info) {
            return Err("native job accounting size changed".into());
        }
        Ok(ExitObservation {
            code,
            job_empty: info.ActiveProcesses == 0,
        })
    }
    fn read_stdout(&mut self, buffer: &mut [u8]) -> Result<PipeRead, String> {
        self.stdout.read(buffer)
    }
    fn read_stderr(&mut self, buffer: &mut [u8]) -> Result<PipeRead, String> {
        self.stderr.read(buffer)
    }
    fn terminate_job(&mut self) -> Result<(), String> {
        // SAFETY: this exact unnamed private job owns only this producer tree.
        check(unsafe { TerminateJobObject(self.job.as_raw_handle(), 2) })
    }
}
#[cfg(test)]
#[path = "process_tests.rs"]
mod tests;
