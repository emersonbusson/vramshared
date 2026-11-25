// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/host_storage/discovery/user_hives.rs
//! Loaded SID hive discovery for WSL2 `ext4.vhdx` files under `HKEY_USERS`.

use std::collections::HashSet;
use std::path::PathBuf;

#[cfg(windows)]
const MAX_DISCOVERY_ENTRIES: usize = 4096;

#[cfg(any(windows, test))]
pub(in crate::host_storage) type RegistrationObservation = (String, String, PathBuf, u64);

#[cfg(any(windows, test))]
fn stable_registration_observations(
    mut observe: impl FnMut() -> Result<Vec<RegistrationObservation>, String>,
) -> Result<Vec<RegistrationObservation>, String> {
    let mut before = observe()?;
    let mut after = observe()?;
    before.sort_unstable();
    after.sort_unstable();
    if before != after {
        return Err("WSL registrations changed during discovery".into());
    }
    Ok(after)
}

#[cfg(any(windows, test))]
#[derive(Clone, Debug, Eq, PartialEq)]
enum RegisteredVhdx {
    Present(PathBuf),
    /// The `BasePath` entry points to a local drive letter whose fixed volume
    /// is freshly confirmed online, so `NotFound` proves an orphaned/deleted
    /// distro directory rather than an offline or unreadable storage volume.
    AbsentOnMountedFixedVolume,
}

#[cfg(any(windows, test))]
fn local_drive_root(path: &std::path::Path) -> Option<String> {
    let raw = path.to_str()?;
    let bytes = raw.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        return Some(format!("{}:\\", (bytes[0] as char).to_ascii_uppercase()));
    }
    None
}

#[cfg(any(windows, test))]
fn registered_vhdx(
    base_path: Option<&str>,
    probe: impl FnOnce(&std::path::Path) -> std::io::Result<bool>,
    drive_online: impl FnOnce(&str) -> bool,
) -> Result<RegisteredVhdx, String> {
    let base_path = base_path.ok_or("WSL registration has no readable base path")?;
    let path = normalize_lxss_vhdx_path(base_path).ok_or("invalid WSL registration path")?;
    match probe(&path) {
        Ok(true) => Ok(RegisteredVhdx::Present(path)),
        Ok(false) => Err("registered VHDX is not a regular file".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let Some(root) = local_drive_root(&path) else {
                return Err(format!("cannot inspect registered VHDX: {error}"));
            };
            if drive_online(&root) {
                Ok(RegisteredVhdx::AbsentOnMountedFixedVolume)
            } else {
                Err(format!(
                    "cannot inspect registered VHDX on unavailable volume {root}: {error}"
                ))
            }
        }
        Err(error) => Err(format!("cannot inspect registered VHDX: {error}")),
    }
}

#[cfg(windows)]
fn is_mounted_fixed_drive(drive_root: &str) -> bool {
    use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetDriveTypeW};

    const DRIVE_FIXED: u32 = 3;
    let wide: Vec<u16> = drive_root.encode_utf16().chain(Some(0)).collect();
    let mut available = 0u64;
    let mut total = 0u64;
    let mut free = 0u64;
    // SAFETY: `wide` is NUL-terminated and all three output pointers are distinct stack slots.
    unsafe {
        GetDriveTypeW(wide.as_ptr()) == DRIVE_FIXED
            && GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, &mut total, &mut free) != 0
            && total > 0
    }
}

/// Select numeric SID-shaped hive names, including service and cloud SIDs.
/// Per-user `_Classes` hives are not separate WSL registration owners.
#[cfg(any(windows, test))]
pub fn is_loaded_user_sid(name: &str) -> bool {
    if name.len() > 184 || name.to_ascii_lowercase().ends_with("_classes") {
        return false;
    }
    let mut parts = name.split('-');
    let numeric = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    if parts.next() != Some("S") || parts.next() != Some("1") || !parts.next().is_some_and(numeric)
    {
        return false;
    }
    let mut subauthorities = parts.peekable();
    subauthorities.peek().is_some() && subauthorities.all(numeric)
}

/// Resolve a `BasePath` registry value into its `ext4.vhdx` path without
/// admitting relative paths. This does not establish junction-free ancestry.
#[cfg(any(windows, test))]
pub fn normalize_lxss_vhdx_path(base_path: &str) -> Option<PathBuf> {
    let trimmed = base_path.trim().trim_end_matches('\0');
    if trimmed.is_empty() || trimmed.chars().any(char::is_control) {
        return None;
    }
    let normalized = strip_extended_drive_prefix(trimmed);
    if !is_windows_absolute_drive_or_unc(normalized) {
        return None;
    }
    Some(PathBuf::from(normalized).join("ext4.vhdx"))
}

#[cfg(any(windows, test))]
fn strip_extended_drive_prefix(path: &str) -> &str {
    if let Some(rest) = path.strip_prefix(r"\\?\") {
        let bytes = rest.as_bytes();
        if bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && (bytes[2] == b'\\' || bytes[2] == b'/')
        {
            return rest;
        }
    }
    path
}

#[cfg(any(windows, test))]
fn is_windows_absolute_drive_or_unc(path: &str) -> bool {
    let bytes = path.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        return true;
    }
    path.starts_with(r"\\")
}

/// Enumerate registered VHDX paths in loaded user hives, refusing incomplete
/// enumeration. This does not establish ownership or include unloaded hives.
#[cfg(windows)]
pub fn enumerate_all_vhdx_paths() -> Result<Vec<PathBuf>, String> {
    Ok(stable_loaded_registrations()?
        .into_iter()
        .map(|(_, _, path, _)| path)
        .collect())
}

#[cfg(windows)]
pub(in crate::host_storage) fn stable_loaded_registrations()
-> Result<Vec<RegistrationObservation>, String> {
    stable_registration_observations(enumerate_loaded_registrations)
}

#[cfg(windows)]
fn enumerate_loaded_registrations() -> Result<Vec<RegistrationObservation>, String> {
    use windows_sys::Win32::System::Registry::HKEY_USERS;

    let mut seen = HashSet::new();
    let mut paths = Vec::new();
    let sids = enum_subkeys(HKEY_USERS, None)?;
    for (sid, _) in sids {
        if !is_loaded_user_sid(&sid) {
            if sid.starts_with("S-") && !sid.to_ascii_lowercase().ends_with("_classes") {
                return Err("unrecognized SID-like hive name".into());
            }
            continue;
        }
        let lxss_root = format!(r"{sid}\Software\Microsoft\Windows\CurrentVersion\Lxss");
        for (guid, last_write) in enum_subkeys(HKEY_USERS, Some(&lxss_root))? {
            if last_write == 0 {
                return Err(format!("WSL registration has no change timestamp: {guid}"));
            }
            let entry_key = format!(r"{lxss_root}\{guid}");
            let base_path = read_reg_sz(HKEY_USERS, &entry_key, "BasePath");
            let vhdx = match registered_vhdx(
                base_path.as_deref(),
                |path| {
                    std::fs::symlink_metadata(path).map(|metadata| metadata.file_type().is_file())
                },
                is_mounted_fixed_drive,
            )
            .map_err(|error| format!("WSL registration {entry_key}: {error}"))?
            {
                RegisteredVhdx::Present(path) => path,
                RegisteredVhdx::AbsentOnMountedFixedVolume => continue,
            };
            let dedup_key = vhdx.to_string_lossy().to_ascii_lowercase();
            if !seen.insert(dedup_key) {
                return Err(format!(
                    "duplicate registered VHDX path: {}",
                    vhdx.display()
                ));
            }
            if paths.len() >= MAX_DISCOVERY_ENTRIES {
                return Err("WSL registration count exceeds its bound".into());
            }
            paths.push((sid.clone(), guid, vhdx, last_write));
        }
    }
    Ok(paths)
}

#[cfg(not(windows))]
pub fn enumerate_all_vhdx_paths() -> Result<Vec<PathBuf>, String> {
    let _ = HashSet::<String>::new;
    Err("WSL registration enumeration requires Windows".into())
}

#[cfg(windows)]
fn enum_subkeys(
    root: windows_sys::Win32::System::Registry::HKEY,
    subpath: Option<&str>,
) -> Result<Vec<(String, u64)>, String> {
    use windows_sys::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_ITEMS, ERROR_PATH_NOT_FOUND, FILETIME,
    };
    use windows_sys::Win32::System::Registry::{
        HKEY, KEY_READ, RegCloseKey, RegEnumKeyExW, RegOpenKeyExW,
    };

    struct KeyGuard(Option<HKEY>);
    impl Drop for KeyGuard {
        fn drop(&mut self) {
            if let Some(handle) = self.0.take() {
                // SAFETY: `handle` is an opened HKEY owned by this guard.
                unsafe {
                    RegCloseKey(handle);
                }
            }
        }
    }

    let opened_guard = if let Some(path) = subpath {
        let mut wide: Vec<u16> = path.encode_utf16().collect();
        wide.push(0);
        let mut handle = std::ptr::null_mut();
        // SAFETY: `root` and NUL-terminated `wide` point to valid read-only memory.
        let status = unsafe { RegOpenKeyExW(root, wide.as_ptr(), 0, KEY_READ, &mut handle) };
        if status == ERROR_FILE_NOT_FOUND || status == ERROR_PATH_NOT_FOUND {
            return Ok(Vec::new());
        }
        if status != 0 {
            return Err(format!("cannot open WSL registry key {path}: {status}"));
        }
        KeyGuard(Some(handle))
    } else {
        KeyGuard(None)
    };
    let target = opened_guard.0.unwrap_or(root);
    let mut result = Vec::new();
    let mut index = 0u32;
    loop {
        let mut name_buf = [0u16; 256];
        let mut name_len = name_buf.len() as u32;
        let mut last_write = FILETIME::default();
        // SAFETY: `target` is valid; `name_buf` capacity matches `name_len`.
        let status = unsafe {
            RegEnumKeyExW(
                target,
                index,
                name_buf.as_mut_ptr(),
                &mut name_len,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut last_write,
            )
        };
        index = index.saturating_add(1);
        if status == ERROR_NO_MORE_ITEMS {
            break;
        }
        if index as usize > MAX_DISCOVERY_ENTRIES {
            return Err("WSL registry enumeration exceeds its bound".into());
        }
        if status == 0 {
            let decoded = String::from_utf16(&name_buf[..name_len as usize])
                .map_err(|_| "WSL registry key is not valid UTF-16")?;
            let timestamp =
                (u64::from(last_write.dwHighDateTime) << 32) | u64::from(last_write.dwLowDateTime);
            result.push((decoded, timestamp));
            continue;
        }
        return Err(format!("WSL registry enumeration failed: {status}"));
    }
    Ok(result)
}

#[cfg(windows)]
fn read_reg_sz(
    root: windows_sys::Win32::System::Registry::HKEY,
    subpath: &str,
    value_name: &str,
) -> Option<String> {
    use windows_sys::Win32::System::Registry::{
        KEY_READ, REG_EXPAND_SZ, REG_SZ, RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
    };

    let mut key_wide: Vec<u16> = subpath.encode_utf16().collect();
    key_wide.push(0);
    let mut val_wide: Vec<u16> = value_name.encode_utf16().collect();
    val_wide.push(0);
    let mut handle = std::ptr::null_mut();
    // SAFETY: `root` and NUL-terminated `key_wide` are valid for `RegOpenKeyExW`.
    if unsafe { RegOpenKeyExW(root, key_wide.as_ptr(), 0, KEY_READ, &mut handle) } != 0 {
        return None;
    }
    let mut ty = 0u32;
    let mut byte_len = 0u32;
    // SAFETY: valid open `handle` querying byte length before buffer allocation.
    let size_status = unsafe {
        RegQueryValueExW(
            handle,
            val_wide.as_ptr(),
            std::ptr::null(),
            &mut ty,
            std::ptr::null_mut(),
            &mut byte_len,
        )
    };
    if size_status != 0
        || (ty != REG_SZ && ty != REG_EXPAND_SZ)
        || byte_len == 0
        || byte_len > 32 * 1024
        || !byte_len.is_multiple_of(2)
    {
        // SAFETY: valid handle closed on early exit.
        unsafe {
            RegCloseKey(handle);
        }
        return None;
    }
    let mut buf = vec![0u16; (byte_len / 2) as usize];
    let mut actual_bytes = byte_len;
    // SAFETY: `buf` holds `actual_bytes` bytes owned by this function.
    let read_status = unsafe {
        RegQueryValueExW(
            handle,
            val_wide.as_ptr(),
            std::ptr::null(),
            &mut ty,
            buf.as_mut_ptr().cast(),
            &mut actual_bytes,
        )
    };
    // SAFETY: `handle` is closed exactly once.
    unsafe {
        RegCloseKey(handle);
    }
    if read_status != 0 || (ty != REG_SZ && ty != REG_EXPAND_SZ) || !actual_bytes.is_multiple_of(2)
    {
        return None;
    }
    buf.truncate((actual_bytes / 2) as usize);
    while buf.last() == Some(&0) {
        buf.pop();
    }
    String::from_utf16(&buf).ok()
}

#[cfg(test)]
#[path = "user_hives/tests.rs"]
mod tests;
