// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/service_identity.rs
//! Service identity — one native reader for a service SID (DT-17).
//!
//! `protect_version_directory` used to resolve the broker service SID by
//! spawning `sc.exe showsid` and scanning its human-readable output for a
//! token beginning with `S-1-5-80-`. Three things were wrong with that:
//!
//! 1. it parsed localized text — the label on this host is `SID DO SERVIÇO:`,
//!    so the parser depended on the console code page it never checked;
//! 2. it spawned a child process whose absence from a restricted `PATH` fails
//!    the install for a reason that has nothing to do with the install;
//! 3. it re-derived a contract the OS already exposes as a string.
//!
//! The SID is an OS fact: `LookupAccountNameW` on the virtual account
//! `NT SERVICE\<name>` returns it, `ConvertSidToStringSidW` renders it. No
//! text is scanned and no process is created. The descriptor policy (which
//! ACEs the version directory carries) stays pure and is tested off-Windows.

use std::fmt;

/// Every service SID lives under this authority.
pub const SERVICE_SID_PREFIX: &str = "S-1-5-80-";

/// A service SID carries five sub-authorities derived from the service name.
const SERVICE_SID_SUBAUTHORITIES: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceIdentityError {
    /// The account name cannot be built or the SID cannot be rendered.
    Config(String),
    /// `LookupAccountNameW` refused the virtual account. `code` is the Win32
    /// error; `ERROR_NONE_MAPPED` means the service has no SID of that type.
    Unresolved { service: String, code: u32 },
}

impl fmt::Display for ServiceIdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(reason) => write!(f, "service identity: {reason}"),
            Self::Unresolved { service, code } => write!(
                f,
                "service identity: NT SERVICE\\{service} is not resolvable (win32 error {code})"
            ),
        }
    }
}

impl std::error::Error for ServiceIdentityError {}

/// Build the virtual account the SCM exposes for a service.
///
/// The name is part of a security descriptor, so a separator or an empty name
/// is refused instead of being interpolated.
pub fn service_account_name(service: &str) -> Result<String, ServiceIdentityError> {
    if service.is_empty() || service.contains('\\') || service.contains('/') {
        return Err(ServiceIdentityError::Config(format!(
            "invalid service name: {service:?}"
        )));
    }
    Ok(format!("NT SERVICE\\{service}"))
}

/// Accept a string only if it is a service SID.
///
/// Fail-closed: a value that is not `S-1-5-80-` followed by five decimal
/// sub-authorities is refused, so a malformed render can never reach an SDDL
/// string and widen the directory ACL by accident.
pub fn accept_service_sid(sid: &str) -> Result<&str, ServiceIdentityError> {
    let Some(rest) = sid.strip_prefix(SERVICE_SID_PREFIX) else {
        return Err(ServiceIdentityError::Config(format!(
            "not a service SID: {sid}"
        )));
    };
    let parts: Vec<&str> = rest.split('-').collect();
    if parts.len() != SERVICE_SID_SUBAUTHORITIES
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err(ServiceIdentityError::Config(format!(
            "not a service SID: {sid}"
        )));
    }
    Ok(sid)
}

/// The version directory descriptor.
///
/// `broker_sid` is `None` while the directory is staged and the service does
/// not exist yet: the tree is then reachable only by `SYSTEM` and
/// `BUILTIN\Administrators`, which is stricter than the final policy. Once the
/// broker service is registered, the caller re-applies the descriptor with the
/// service SID so the broker can read its own version. Never the other way
/// round — a directory published with a guessed SID would hand a not-yet-owned
/// identity access to the installed tree.
pub fn version_directory_sddl(broker_sid: Option<&str>) -> Result<String, ServiceIdentityError> {
    let mut sddl = String::from("D:P(A;;GA;;;SY)(A;;GA;;;BA)");
    if let Some(sid) = broker_sid {
        let sid = accept_service_sid(sid)?;
        sddl.push_str("(A;;GRGX;;;");
        sddl.push_str(sid);
        sddl.push(')');
    }
    Ok(sddl)
}

/// Resolve the SID of an installed service through the OS.
#[cfg(windows)]
pub fn service_sid(service: &str) -> Result<String, ServiceIdentityError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{LookupAccountNameW, SID_NAME_USE};

    /// `SECURITY_MAX_SID_SIZE`.
    const MAX_SID_BYTES: usize = 68;
    const MAX_DOMAIN_CHARS: usize = 256;

    let account = service_account_name(service)?;
    let account_w: Vec<u16> = std::ffi::OsStr::new(&account)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let mut sid = [0u8; MAX_SID_BYTES];
    let mut sid_len = sid.len() as u32;
    let mut domain = [0u16; MAX_DOMAIN_CHARS];
    let mut domain_len = domain.len() as u32;
    let mut sid_type: SID_NAME_USE = 0;

    let resolved = unsafe {
        LookupAccountNameW(
            std::ptr::null(),
            account_w.as_ptr(),
            sid.as_mut_ptr().cast(),
            &mut sid_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut sid_type,
        )
    };
    if resolved == 0 {
        let code = std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or_default() as u32;
        return Err(ServiceIdentityError::Unresolved {
            service: service.to_string(),
            code,
        });
    }

    let mut rendered: *mut u16 = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid.as_mut_ptr().cast(), &mut rendered) } == 0 {
        let code = std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or_default() as u32;
        return Err(ServiceIdentityError::Unresolved {
            service: service.to_string(),
            code,
        });
    }

    let mut len = 0usize;
    while unsafe { *rendered.add(len) } != 0 {
        len += 1;
    }
    let text = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(rendered, len) });
    unsafe { LocalFree(rendered.cast()) };
    accept_service_sid(&text)?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_name_is_the_virtual_service_account() {
        let account = service_account_name("RamSharedBroker")
            .unwrap_or_else(|error| panic!("account name refused: {error}"));
        assert_eq!(account, "NT SERVICE\\RamSharedBroker");
    }

    #[test]
    fn account_name_refuses_separators_and_empty_names() {
        for bad in ["", "a\\b", "a/b"] {
            assert!(service_account_name(bad).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn service_sid_prefix_and_shape_are_enforced() {
        let good = "S-1-5-80-1014140700-3308905587-3330345912-272242898-93311788";
        let accepted = accept_service_sid(good).unwrap_or_else(|error| panic!("refused: {error}"));
        assert_eq!(accepted, good);
        for bad in [
            "",
            "S-1-5-18",
            "S-1-5-80-1-2-3-4",
            "S-1-5-80-1-2-3-4-5-6",
            "S-1-5-80-1-2-3-4-x",
            " S-1-5-80-1-2-3-4-5",
            "S-1-5-80-1-2-3-4-5\n",
        ] {
            assert!(accept_service_sid(bad).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn staged_descriptor_is_admins_only() {
        let sddl = version_directory_sddl(None)
            .unwrap_or_else(|error| panic!("staged descriptor refused: {error}"));
        assert_eq!(sddl, "D:P(A;;GA;;;SY)(A;;GA;;;BA)");
    }

    #[test]
    fn broker_descriptor_adds_read_execute_for_the_service_sid_only() {
        let sid = "S-1-5-80-1014140700-3308905587-3330345912-272242898-93311788";
        let sddl = version_directory_sddl(Some(sid))
            .unwrap_or_else(|error| panic!("broker descriptor refused: {error}"));
        assert_eq!(
            sddl,
            format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGX;;;{sid})")
        );
        assert!(!sddl.contains(";;;WD)"), "Everyone must never appear");
    }

    #[test]
    fn malformed_broker_sid_never_reaches_the_descriptor() {
        assert!(version_directory_sddl(Some("S-1-5-18")).is_err());
        assert!(version_directory_sddl(Some("S-1-5-80-1-2-3-4-5);(A;;GA;;;WD")).is_err());
    }

    /// Live parity with the SCM: when `sc.exe` can name a service SID, the
    /// native lookup must return the same string. Skipped when no probe
    /// service is resolvable, so a stripped SKU does not fail the suite.
    #[cfg(windows)]
    #[test]
    fn native_sid_matches_sc_exe_when_available() {
        for probe in ["wuauserv", "EventLog", "Schedule"] {
            let started = std::time::Instant::now();
            let Ok(native) = service_sid(probe) else {
                continue;
            };
            let native_elapsed = started.elapsed();
            let started = std::time::Instant::now();
            let output = std::process::Command::new("sc.exe")
                .args(["showsid", probe])
                .output();
            let Ok(output) = output else { continue };
            if !output.status.success() {
                continue;
            }
            let sc_elapsed = started.elapsed();
            let text = String::from_utf8_lossy(&output.stdout);
            let expected = text
                .split_whitespace()
                .find(|part| part.starts_with(SERVICE_SID_PREFIX))
                .unwrap_or_else(|| panic!("sc.exe printed no service SID"));
            assert_eq!(native, expected, "probe {probe}");
            eprintln!("probe={probe} sid={native} native={native_elapsed:?} sc_exe={sc_elapsed:?}");
            return;
        }
        panic!("no probe service was resolvable; the parity path did not run");
    }

    /// Fail-closed: an unregistered name is a refusal, never a derived guess.
    /// This is why `install` resolves the SID only after the service exists.
    #[cfg(windows)]
    #[test]
    fn unregistered_service_identity_is_refused() {
        match service_sid("RamSharedSidProbeNotInstalled") {
            Err(ServiceIdentityError::Unresolved { .. }) => {}
            other => panic!("unregistered service resolved: {other:?}"),
        }
    }
}
