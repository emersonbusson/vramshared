// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/vsock_lease/lifecycle.rs
//! Transport-specific configured admission over shared actual-thread custody.
use crate::thread_custody::OwnedThread;
pub use crate::thread_custody::{StopSignal, ThreadState as StopState};
use std::io;
use std::sync::Arc;
#[cfg(windows)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Kernel mutex name reserving single-process custody of the vsock control authority.
pub const CONTROL_AUTHORITY_MUTEX_NAME: &str = "Global\\RamShared.ControlAuthority.v1";

#[cfg(windows)]
static CONFIGURED_ACTIVE: AtomicBool = AtomicBool::new(false);
#[cfg(windows)]
#[derive(Debug)]
pub(super) struct ConfiguredPermit {
    // The name stays reserved until this handle closes after worker settlement.
    _process_lock: std::os::windows::io::OwnedHandle,
}
#[cfg(windows)]
impl ConfiguredPermit {
    pub fn acquire() -> io::Result<Self> {
        use std::os::windows::io::FromRawHandle;
        use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, SetLastError};
        use windows_sys::Win32::System::Threading::CreateMutexW;
        CONFIGURED_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                io::Error::other("configured control-plane custody is already retained")
            })?;
        let name: Vec<u16> = format!("{CONTROL_AUTHORITY_MUTEX_NAME}\0")
            .encode_utf16()
            .collect();
        // SAFETY: the name is NUL-terminated and the default descriptor is valid;
        // no thread owns the mutex, so handle lifetime itself reserves the name.
        let (raw, last) = unsafe {
            SetLastError(0);
            let raw = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
            (raw, GetLastError())
        };
        if raw.is_null() {
            CONFIGURED_ACTIVE.store(false, Ordering::Release);
            return Err(io::Error::from_raw_os_error(last as i32));
        }
        // SAFETY: CreateMutexW returned one uniquely owned kernel handle.
        let handle = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(raw.cast()) };
        if last == ERROR_ALREADY_EXISTS {
            CONFIGURED_ACTIVE.store(false, Ordering::Release);
            return Err(io::Error::other(
                "configured control-plane custody is held by another process",
            ));
        }
        Ok(Self {
            _process_lock: handle,
        })
    }
}
#[cfg(windows)]
impl Drop for ConfiguredPermit {
    fn drop(&mut self) {
        CONFIGURED_ACTIVE.store(false, Ordering::Release);
    }
}

#[derive(Debug)]
pub struct ControlPlaneTask {
    worker: OwnedThread,
    #[cfg(windows)]
    pub(super) permit: Option<ConfiguredPermit>,
}
impl ControlPlaneTask {
    pub fn spawn<F>(name: &str, run: F) -> io::Result<Self>
    where
        F: FnOnce(Arc<StopSignal>) -> io::Result<()> + Send + 'static,
    {
        Ok(Self {
            worker: OwnedThread::spawn(name, run)?,
            #[cfg(windows)]
            permit: None,
        })
    }
    /// Observe actual completion without requesting that the worker stop.
    pub fn poll(&mut self) -> io::Result<StopState> {
        let result = self.worker.observe();
        #[cfg(windows)]
        if self.worker.has_joined() {
            self.permit.take();
        }
        result
    }
    pub fn request_stop(&self) {
        self.worker.request_stop();
    }
    pub fn wait_for_stop(&mut self, budget: Duration) -> io::Result<StopState> {
        let result = self.worker.wait_for_stop(budget);
        #[cfg(windows)]
        if self.worker.has_joined() {
            self.permit.take();
        }
        result
    }
}
// Field drop order joins the actual worker before releasing its permit.

#[cfg(all(test, windows))]
mod cross_process_tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::ConfiguredPermit;
    use std::process::Command;

    #[test]
    #[ignore = "native Windows process-global authority permit test"]
    fn authority_permit_is_exclusive_across_processes() {
        const CHILD: &str = "RAMSHARED_TEST_AUTHORITY_PERMIT_CHILD";
        if let Ok(expected) = std::env::var(CHILD) {
            if expected == "exit" {
                let _permit = ConfiguredPermit::acquire().expect("child owns permit");
                std::process::exit(7);
            }
            let acquired = ConfiguredPermit::acquire().is_ok();
            assert_eq!(acquired, expected == "acquire");
            return;
        }
        let permit = ConfiguredPermit::acquire().expect("parent owns permit");
        let run_child = |expected| {
            let status = Command::new(std::env::current_exe().unwrap())
                .args(["vsock_lease::lifecycle::cross_process_tests::authority_permit_is_exclusive_across_processes", "--exact", "--ignored", "--nocapture"])
                .env(CHILD, expected)
                .status()
                .unwrap();
            assert!(status.success());
        };
        run_child("refuse");
        drop(permit);
        run_child("acquire");
        let abnormal = Command::new(std::env::current_exe().unwrap())
            .args(["vsock_lease::lifecycle::cross_process_tests::authority_permit_is_exclusive_across_processes", "--exact", "--ignored"])
            .env(CHILD, "exit")
            .status()
            .unwrap();
        assert_eq!(abnormal.code(), Some(7));
        assert!(ConfiguredPermit::acquire().is_ok());
    }
}
