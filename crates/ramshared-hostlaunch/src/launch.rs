// SPDX-License-Identifier: GPL-2.0
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-hostlaunch/src/launch.rs
//
// Copyright (c) 2026 RamShared contributors.
//
// One child process with no window.
//
// SPEC DT-11: `CreateProcessW` with `CREATE_NO_WINDOW`, the standard handles
// inherited explicitly, one bounded wait, `TerminateProcess` on the deadline,
// and the child's exit code forwarded unchanged. The handles this module owns
// are closed on every path, including the timeout path.

use crate::LaunchRequest;

#[cfg(not(windows))]
pub(crate) fn run(_request: &LaunchRequest) -> Result<u32, crate::LaunchError> {
    Err(crate::LaunchError::WindowsOnly)
}

#[cfg(windows)]
pub(crate) fn run(request: &LaunchRequest) -> Result<u32, crate::LaunchError> {
    imp::run(request)
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::ptr;

    use windows_sys::Win32::Foundation::{
        CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, INVALID_HANDLE_VALUE,
        WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_NO_WINDOW, CreateProcessW, GetCurrentProcess, GetExitCodeProcess, INFINITE,
        PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW, TerminateProcess,
        WaitForSingleObject,
    };

    use crate::{LaunchError, LaunchRequest, TIMEOUT_EXIT};

    struct OwnedHandle(HANDLE);

    impl OwnedHandle {
        fn new(handle: HANDLE) -> Self {
            Self(handle)
        }
    }

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: the handle was returned by CreateProcessW and has
                // not been closed anywhere else; Drop runs once.
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    /// The caller's three standard handles, duplicated as inheritable.
    ///
    /// `STARTF_USESTDHANDLES` only works when the handles are inheritable.
    /// The launcher is a GUI-subsystem process, so it has no console of its
    /// own; it must forward the pipes the caller gave it. Duplicating instead
    /// of forwarding the originals keeps the caller's handles untouched and
    /// makes the inheritance explicit on every path.
    struct StandardHandles {
        input: OwnedHandle,
        output: OwnedHandle,
        error: OwnedHandle,
    }

    impl StandardHandles {
        fn duplicate() -> Result<Self, LaunchError> {
            Ok(Self {
                input: Self::duplicate_one(STD_INPUT_HANDLE)?,
                output: Self::duplicate_one(STD_OUTPUT_HANDLE)?,
                error: Self::duplicate_one(STD_ERROR_HANDLE)?,
            })
        }

        fn duplicate_one(kind: u32) -> Result<OwnedHandle, LaunchError> {
            // SAFETY: GetStdHandle is a pure kernel32 query for this process.
            let source = unsafe { GetStdHandle(kind) };
            // A task started without inherited stdio has no handle to forward.
            // `STARTF_USESTDHANDLES` with a null handle is the honest shape.
            if source.is_null() || source == INVALID_HANDLE_VALUE {
                return Ok(OwnedHandle::new(std::ptr::null_mut()));
            }
            let mut duplicate: HANDLE = std::ptr::null_mut();
            // SAFETY: source is a live handle owned by this process, the
            // out-pointer is valid, and the child needs one inheritable copy.
            let created = unsafe {
                DuplicateHandle(
                    GetCurrentProcess(),
                    source,
                    GetCurrentProcess(),
                    &mut duplicate,
                    0,
                    1,
                    DUPLICATE_SAME_ACCESS,
                )
            };
            if created == 0 {
                return Err(os_error("DuplicateHandle"));
            }
            Ok(OwnedHandle::new(duplicate))
        }
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn os_error(operation: &str) -> LaunchError {
        LaunchError::Spawn(format!("{operation}: {}", std::io::Error::last_os_error()))
    }

    pub(crate) fn run(request: &LaunchRequest) -> Result<u32, LaunchError> {
        let (program, args) = request.resolve_child()?;
        let command_line = crate::argv::build_command_line(&program, &args)?;
        let mut command_line_wide = wide(&command_line);
        let program_wide = wide(&program);

        // Explicit standard handles: a GUI-subsystem process owns no console,
        // so without these the child would receive invalid handles instead of
        // the caller's pipes.
        let stdio = StandardHandles::duplicate()?;
        let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
        startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        startup.dwFlags = STARTF_USESTDHANDLES;
        startup.hStdInput = stdio.input.0;
        startup.hStdOutput = stdio.output.0;
        startup.hStdError = stdio.error.0;

        let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: both strings are NUL-terminated UTF-16 buffers that outlive
        // the call, the STARTUPINFOW is initialized with its own size, and the
        // process information block is a valid out-pointer.
        let created = unsafe {
            CreateProcessW(
                program_wide.as_ptr(),
                command_line_wide.as_mut_ptr(),
                ptr::null(),
                ptr::null(),
                1, // bInheritHandles: the standard handles above
                CREATE_NO_WINDOW,
                ptr::null::<c_void>(),
                ptr::null(),
                &startup,
                &mut process,
            )
        };
        if created == 0 {
            return Err(os_error("CreateProcessW"));
        }

        let process_handle = OwnedHandle::new(process.hProcess);
        let _thread_handle = OwnedHandle::new(process.hThread);

        // 0 is the explicit "no deadline" value: the guardian producer's task
        // action is PT0S, and bounding it would kill the watcher on a timer.
        let timeout = match request.timeout_ms {
            0 => INFINITE,
            value => u32::try_from(value).unwrap_or(u32::MAX),
        };
        // SAFETY: process_handle is a live process handle owned by this scope.
        let waited = unsafe { WaitForSingleObject(process_handle.0, timeout) };
        match waited {
            WAIT_OBJECT_0 => {
                let mut code = 0u32;
                // SAFETY: the process is signaled, so its exit code is stable.
                if unsafe { GetExitCodeProcess(process_handle.0, &mut code) } == 0 {
                    return Err(LaunchError::Wait(format!(
                        "GetExitCodeProcess: {}",
                        std::io::Error::last_os_error()
                    )));
                }
                Ok(code)
            }
            WAIT_TIMEOUT => {
                // The deadline is the product's bound, not a request to keep
                // waiting. Terminate, give it one short grace to die, then
                // return the timeout status. The handle closes on scope exit.
                // SAFETY: the process handle is live and owned by this scope.
                unsafe { TerminateProcess(process_handle.0, u32::from(TIMEOUT_EXIT)) };
                // SAFETY: same handle; the bounded grace prevents a zombie
                // from outliving this function.
                unsafe { WaitForSingleObject(process_handle.0, 5_000) };
                Ok(u32::from(TIMEOUT_EXIT))
            }
            other => Err(LaunchError::Wait(format!(
                "WaitForSingleObject returned {other}: {}",
                std::io::Error::last_os_error()
            ))),
        }
    }
}
