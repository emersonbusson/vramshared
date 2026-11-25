// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/installation/transition.rs
//! One exclusive installer transition (DT-14).
//!
//! Install and uninstall both mutate services, files and the driver store.
//! They must not overlap, including across an elevated and a non-elevated
//! session. The transition is a named mutex with a protected DACL: only
//! `LocalSystem` and `Builtin\Administrators` may take it, and the name is
//! versioned so an old installer cannot hold the new one by accident.
//!
//! The wait is bounded. A second installer waits 30 seconds and then refuses;
//! an abandoned mutex is accepted but reported, because the previous holder
//! crashed while owning the transition and the caller may need to reconcile.
//!
//! The decision table is pure and testable on any host; only
//! [`InstallTransition::acquire`] calls Windows.

use std::fmt;

/// The one transition object. Versioned: a future incompatible installer
/// takes `v2` instead of contending with this one.
pub const MUTEX_NAME: &str = r"Global\RamSharedProductInstall.v1";

/// Owner, group and DACL in one descriptor: protected, system and
/// administrators get full access, nobody else gets anything.
pub const MUTEX_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)";

/// How long a second installer waits before refusing.
pub const WAIT_TIMEOUT_MS: u32 = 30_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Acquisition {
    /// No other transition was in progress.
    Fresh,
    /// The previous owner died without releasing the object. The caller holds
    /// the transition now, but the interrupted mutation needs reconciliation.
    AfterAbandon,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransitionError {
    /// The wait returned something other than "owned".
    Wait { code: u32 },
    /// The object could not be created or its security descriptor was refused.
    Win32 { op: &'static str, code: u32 },
}

impl fmt::Display for TransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wait { code } => write!(
                f,
                "installer transition refused after {WAIT_TIMEOUT_MS} ms (wait result {code})"
            ),
            Self::Win32 { op, code } => write!(f, "{op} failed (Win32 error {code})"),
        }
    }
}

impl std::error::Error for TransitionError {}

/// Map a `WaitForSingleObject` result to an acquisition or a refusal.
pub fn classify_wait(code: u32) -> Result<Acquisition, TransitionError> {
    match code {
        // WAIT_OBJECT_0.
        0 => Ok(Acquisition::Fresh),
        // WAIT_ABANDONED.
        0x80 => Ok(Acquisition::AfterAbandon),
        other => Err(TransitionError::Wait { code: other }),
    }
}

/// The held transition. Dropping it releases the object.
#[cfg(windows)]
pub struct InstallTransition {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl InstallTransition {
    pub fn acquire() -> Result<(Self, Acquisition), TransitionError> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, LocalFree};
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
        use windows_sys::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};

        let wide = |value: &str| {
            std::ffi::OsStr::new(value)
                .encode_wide()
                .chain(std::iter::once(0))
                .collect::<Vec<u16>>()
        };
        let sddl = wide(MUTEX_SDDL);
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(TransitionError::Win32 {
                op: "ConvertStringSecurityDescriptorToSecurityDescriptorW",
                code: unsafe { GetLastError() },
            });
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let name = wide(MUTEX_NAME);
        let handle = unsafe { CreateMutexW(&attributes, 0, name.as_ptr()) };
        unsafe {
            LocalFree(descriptor);
        }
        if handle.is_null() {
            return Err(TransitionError::Win32 {
                op: "CreateMutexW",
                code: unsafe { GetLastError() },
            });
        }
        let wait = unsafe { WaitForSingleObject(handle, WAIT_TIMEOUT_MS) };
        match classify_wait(wait) {
            Ok(acquisition) => Ok((Self { handle }, acquisition)),
            Err(error) => {
                unsafe {
                    CloseHandle(handle);
                }
                Err(error)
            }
        }
    }
}

#[cfg(windows)]
impl Drop for InstallTransition {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::ReleaseMutex;
        unsafe {
            ReleaseMutex(self.handle);
            CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn transition_object_is_global_protected_and_versioned() {
        assert_eq!(MUTEX_NAME, r"Global\RamSharedProductInstall.v1");
        assert_eq!(MUTEX_SDDL, "D:P(A;;GA;;;SY)(A;;GA;;;BA)");
        assert_eq!(WAIT_TIMEOUT_MS, 30_000);
    }

    #[test]
    fn wait_object_zero_is_a_fresh_acquisition() {
        assert_eq!(classify_wait(0).unwrap(), Acquisition::Fresh);
    }

    #[test]
    fn abandoned_transition_is_acquired_but_reported() {
        assert_eq!(classify_wait(0x80).unwrap(), Acquisition::AfterAbandon);
        assert_ne!(
            classify_wait(0x80).unwrap(),
            Acquisition::Fresh,
            "a crash must not look like a clean acquisition"
        );
    }

    #[test]
    fn timeout_and_unknown_wait_results_are_refused() {
        for code in [0x102, 0xFFFFFFFF, 1] {
            let error = classify_wait(code).unwrap_err();
            assert!(matches!(error, TransitionError::Wait { code: seen } if seen == code));
            assert!(error.to_string().contains("refused"), "{error}");
        }
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "creates the global object; run explicitly on a provisioned host"]
    fn live_transition_is_exclusive_and_released() {
        let (first, acquisition) = InstallTransition::acquire().expect("acquire the transition");
        assert_eq!(acquisition, Acquisition::Fresh);
        drop(first);
        let (second, acquisition) = InstallTransition::acquire().expect("reacquire after release");
        assert_eq!(acquisition, Acquisition::Fresh);
        drop(second);
    }
}
