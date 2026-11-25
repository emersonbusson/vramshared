// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/windows_host/native_ops.rs
//! Native Win32/BCrypt/Registry/Pagefile operations for `windows_host`.

#![allow(unsafe_code)]

use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, FALSE, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_ALG_HANDLE, BCRYPT_HASH_HANDLE, BCRYPT_SHA256_ALGORITHM, BCryptCloseAlgorithmProvider,
    BCryptCreateHash, BCryptDestroyHash, BCryptFinishHash, BCryptHashData,
    BCryptOpenAlgorithmProvider,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FlushFileBuffers, GetFileAttributesW, OPEN_EXISTING,
    ReadFile,
};
use windows_sys::Win32::System::Ioctl::{
    IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, STORAGE_DEVICE_DESCRIPTOR,
    STORAGE_PROPERTY_QUERY, StorageDeviceProperty,
};
use windows_sys::Win32::System::ProcessStatus::{ENUM_PAGE_FILE_INFORMATION, EnumPageFilesW};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::config::{ConfigError, MAX_CONFIG_BYTES, WinDriveConfig};

use super::{HostError, PagefileIdentity, validate_absolute_config_path};

// FSCTL codes (ntifs/winioctl).
pub(super) const FSCTL_LOCK_VOLUME: u32 = 0x0009_0018;
pub(super) const FSCTL_UNLOCK_VOLUME: u32 = 0x0009_001c;
const FSCTL_DISMOUNT_VOLUME: u32 = 0x0009_0020;
const IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: u32 = 0x0056_0000;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DiskExtent {
    disk_number: u32,
    starting_offset: i64,
    extent_length: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VolumeDiskExtentsOne {
    number_of_disk_extents: u32,
    extents: [DiskExtent; 1],
}

pub(super) fn is_elevated_native() -> bool {
    unsafe {
        let mut token: HANDLE = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == FALSE {
            return false;
        }
        let mut elev = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut ret = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elev as *mut _ as *mut _,
            size_of_val(&elev) as u32,
            &mut ret,
        );
        CloseHandle(token);
        ok != FALSE && elev.TokenIsElevated != 0
    }
}

/// Read config once from absolute path; reject relative and reparse attributes (DT-1).
pub(super) fn read_owned_config_native(path: &Path) -> Result<WinDriveConfig, HostError> {
    validate_absolute_config_path(path).map_err(HostError::Config)?;
    if path_is_reparse(path) {
        return Err(HostError::Config(ConfigError::Invalid {
            field: "config",
            detail: "reparse-point config path rejected".into(),
        }));
    }
    // Open once without following reparse (OPEN_REPARSE_POINT on the file itself).
    let wide = path_to_wide(path)?;
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0x8000_0000, // GENERIC_READ
            FILE_SHARE_READ,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(HostError::Io(last_err("CreateFile config")));
    }
    let mut buf = vec![0u8; MAX_CONFIG_BYTES + 1];
    let mut total = 0usize;
    while total < buf.len() {
        let mut read = 0u32;
        let ok = unsafe {
            ReadFile(
                handle,
                buf[total..].as_mut_ptr(),
                (buf.len() - total) as u32,
                &mut read,
                ptr::null_mut(),
            )
        };
        if ok == FALSE {
            unsafe {
                CloseHandle(handle);
            }
            return Err(HostError::Io(last_err("ReadFile config")));
        }
        if read == 0 {
            break;
        }
        total += read as usize;
    }
    unsafe {
        CloseHandle(handle);
    }
    if total > MAX_CONFIG_BYTES {
        return Err(HostError::Config(ConfigError::Invalid {
            field: "config",
            detail: format!("exceeds {MAX_CONFIG_BYTES}"),
        }));
    }
    buf.truncate(total);
    WinDriveConfig::from_reader(&buf).map_err(HostError::Config)
}

/// Read `HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management\PagingFiles`.
pub(super) fn configured_pagefiles_registry() -> Result<Vec<PagefileIdentity>, HostError> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Registry::{
        HKEY_LOCAL_MACHINE, KEY_READ, REG_MULTI_SZ, RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
    };

    let subkey = to_wide(r"SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management");
    let mut hkey = ptr::null_mut();
    let open =
        unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, subkey.as_ptr(), 0, KEY_READ, &mut hkey) };
    if open != 0 {
        return Err(HostError::Pagefile(format!(
            "RegOpenKeyExW Memory Management failed status={open}"
        )));
    }
    let name = to_wide("PagingFiles");
    let mut ty = 0u32;
    let mut size = 0u32;
    let q1 = unsafe {
        RegQueryValueExW(
            hkey,
            name.as_ptr(),
            ptr::null_mut(),
            &mut ty,
            ptr::null_mut(),
            &mut size,
        )
    };
    if q1 != 0 {
        unsafe {
            RegCloseKey(hkey);
        }
        return Err(HostError::Pagefile(format!(
            "RegQueryValueExW PagingFiles size failed status={q1}"
        )));
    }
    if size == 0 || !size.is_multiple_of(2) {
        unsafe {
            RegCloseKey(hkey);
        }
        return Err(HostError::Pagefile(format!(
            "PagingFiles invalid byte length {size}"
        )));
    }
    if ty != REG_MULTI_SZ {
        unsafe {
            RegCloseKey(hkey);
        }
        return Err(HostError::Pagefile(format!(
            "PagingFiles unexpected type {ty}"
        )));
    }
    let mut buf = vec![0u8; size as usize];
    let q2 = unsafe {
        RegQueryValueExW(
            hkey,
            name.as_ptr(),
            ptr::null_mut(),
            &mut ty,
            buf.as_mut_ptr(),
            &mut size,
        )
    };
    unsafe {
        RegCloseKey(hkey);
    }
    if q2 != 0 {
        return Err(HostError::Pagefile(format!(
            "RegQueryValueExW PagingFiles failed status={q2}"
        )));
    }
    // MULTI_SZ is UTF-16LE double-null terminated.
    let wide: Vec<u16> = buf
        .as_slice()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|chunk| u16::from_le_bytes(*chunk))
        .collect();
    if wide.last().copied() != Some(0) {
        return Err(HostError::Pagefile(
            "PagingFiles is not null terminated".into(),
        ));
    }
    let mut out = Vec::new();
    let mut start = 0usize;
    for i in 0..wide.len() {
        if wide[i] == 0 {
            if i == start {
                break; // double-null terminator
            }
            let s = std::ffi::OsString::from_wide(&wide[start..i]);
            let s_lossy = s.to_string_lossy().to_string();
            let path = parse_configured_pagefile_entry(&s_lossy).map_err(HostError::Pagefile)?;
            out.push(pagefile_identity(path));
            start = i + 1;
        }
    }
    Ok(out)
}

pub(super) fn lock_volume_device(
    path: &str,
    expected_disk_number: Option<u32>,
) -> Result<(u32, HANDLE), HostError> {
    let wide = to_wide(path);
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0xC000_0000, // GENERIC_READ|GENERIC_WRITE
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(HostError::Volume(last_err("CreateFile volume")));
    }
    let disk_number = match volume_disk_number(handle) {
        Ok(number) => number,
        Err(e) => {
            unsafe { CloseHandle(handle) };
            return Err(HostError::Volume(e));
        }
    };
    if expected_disk_number.is_some_and(|expected| expected != disk_number) {
        unsafe { CloseHandle(handle) };
        return Err(HostError::Volume(format!(
            "volume remapped: expected PhysicalDrive{} got PhysicalDrive{disk_number}",
            expected_disk_number.unwrap_or_default()
        )));
    }
    if !fsctl(handle, FSCTL_LOCK_VOLUME) {
        unsafe {
            CloseHandle(handle);
        }
        return Err(HostError::Volume(last_err("FSCTL_LOCK_VOLUME")));
    }
    Ok((disk_number, handle))
}

pub(super) fn flush_and_dismount_handle(handle: HANDLE) -> Result<(), HostError> {
    let ok = unsafe { FlushFileBuffers(handle) };
    if ok == FALSE {
        return Err(HostError::Volume(last_err("FlushFileBuffers")));
    }
    if !fsctl(handle, FSCTL_DISMOUNT_VOLUME) {
        return Err(HostError::Volume(last_err("FSCTL_DISMOUNT_VOLUME")));
    }
    Ok(())
}

pub(super) fn emit_event_native(summary: &str) -> Result<(), HostError> {
    use windows_sys::Win32::System::EventLog::{
        DeregisterEventSource, EVENTLOG_INFORMATION_TYPE, RegisterEventSourceW, ReportEventW,
    };

    // Lifecycle summary only — no payloads. Best-effort Event Log via Windows API.
    let safe: String = summary
        .chars()
        .filter(|c| c.is_ascii() && !c.is_control())
        .take(200)
        .collect();
    let msg = format!("RamShared: {}", safe);
    let source_wide = to_wide("Application");
    let msg_wide = to_wide(&msg);
    unsafe {
        let handle = RegisterEventSourceW(ptr::null(), source_wide.as_ptr());
        if !handle.is_null() {
            let strings = [msg_wide.as_ptr()];
            ReportEventW(
                handle,
                EVENTLOG_INFORMATION_TYPE,
                0,
                1000,
                ptr::null_mut(),
                1,
                0,
                strings.as_ptr(),
                ptr::null(),
            );
            DeregisterEventSource(handle);
        }
    }
    Ok(())
}

fn path_is_reparse(path: &Path) -> bool {
    let wide = match path_to_wide(path) {
        Ok(w) => w,
        Err(_) => return false,
    };
    let attr = unsafe { GetFileAttributesW(wide.as_ptr()) };
    if attr == u32::MAX {
        return false;
    }
    (attr & FILE_ATTRIBUTE_REPARSE_POINT) != 0
}

fn path_to_wide(path: &Path) -> Result<Vec<u16>, HostError> {
    let s = path
        .to_str()
        .ok_or_else(|| HostError::Io("non-utf8 path".into()))?;
    Ok(to_wide(s))
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Count RamShared/VRAMDISK physical drives and detect a pagefile on the
/// product volume without spawning PowerShell.
///
/// The disk half queries `IOCTL_STORAGE_QUERY_PROPERTY` on each physical drive
/// and matches the vendor/product identity the same way the old CIM predicate
/// did. The pagefile half walks `EnumPageFilesW`. A probe that cannot answer is
/// an error, never a fabricated zero.
pub fn observe_host_residue(volume_letter: char) -> Result<(u32, bool), HostError> {
    let letter = volume_letter.to_ascii_uppercase();
    if !letter.is_ascii_uppercase() {
        return Err(HostError::Io("invalid product volume letter".into()));
    }
    let disks = (0..64u32)
        .filter_map(physical_disk_identity)
        .filter(|identity| {
            let upper = identity.to_ascii_uppercase();
            upper.contains("RAMSHARE") || upper.contains("VRAMDISK")
        })
        .count() as u32;
    Ok((disks, pagefile_on_volume(letter)?))
}

fn physical_disk_identity(disk: u32) -> Option<String> {
    let device = to_wide(&format!(r"\\.\PhysicalDrive{disk}"));
    let handle = unsafe {
        CreateFileW(
            device.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceProperty,
        QueryType: PropertyStandardQuery,
        ..Default::default()
    };
    let mut buffer = [0u8; 1024];
    let mut returned = 0u32;
    let ok = unsafe {
        windows_sys::Win32::System::IO::DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            &query as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            buffer.as_mut_ptr() as *mut std::ffi::c_void,
            buffer.len() as u32,
            &mut returned,
            ptr::null_mut(),
        )
    };
    unsafe { CloseHandle(handle) };
    if ok == FALSE || (returned as usize) < std::mem::size_of::<STORAGE_DEVICE_DESCRIPTOR>() {
        return None;
    }
    // SAFETY: the IOCTL returned at least one descriptor into `buffer`, and
    // every offset is bounds-checked against `returned` before reading.
    let descriptor = unsafe { &*(buffer.as_ptr() as *const STORAGE_DEVICE_DESCRIPTOR) };
    let mut identity = String::new();
    for offset in [descriptor.VendorIdOffset, descriptor.ProductIdOffset] {
        let start = offset as usize;
        if offset == 0 || start >= returned as usize {
            continue;
        }
        let end = buffer[start..returned as usize]
            .iter()
            .position(|byte| *byte == 0)
            .map(|length| start + length)
            .unwrap_or(returned as usize);
        identity.push_str(&String::from_utf8_lossy(&buffer[start..end]));
        identity.push(' ');
    }
    Some(identity)
}

pub(super) struct PagefileNames {
    pub(super) names: Vec<String>,
}

pub(super) unsafe extern "system" fn pagefile_name_callback(
    context: *mut std::ffi::c_void,
    _info: *mut ENUM_PAGE_FILE_INFORMATION,
    name: windows_sys::core::PCWSTR,
) -> windows_sys::core::BOOL {
    if context.is_null() || name.is_null() {
        return 1;
    }
    // SAFETY: `EnumPageFilesW` passes the context pointer back unchanged and
    // `name` is a NUL-terminated UTF-16 string valid for this callback.
    let collector = unsafe { &mut *(context as *mut PagefileNames) };
    let mut length = 0usize;
    unsafe {
        while *name.add(length) != 0 {
            length += 1;
        }
    }
    collector.names.push(String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(name, length)
    }));
    1
}

/// Every allocated pagefile, named exactly as the session manager reports it.
///
/// Replaces the `Get-CimInstance Win32_PageFileUsage` child process: the call
/// is in-process and a failed enumeration is an error, never an empty list.
pub(super) fn active_pagefiles_native() -> Result<Vec<String>, HostError> {
    let mut collector = PagefileNames { names: Vec::new() };
    let ok = unsafe {
        EnumPageFilesW(
            Some(pagefile_name_callback),
            &mut collector as *mut _ as *mut std::ffi::c_void,
        )
    };
    if ok == FALSE {
        return Err(HostError::Pagefile(last_err("EnumPageFilesW")));
    }
    Ok(collector.names)
}

fn pagefile_on_volume(letter: char) -> Result<bool, HostError> {
    Ok(active_pagefiles_native()?
        .iter()
        .any(|path| pagefile_path_on_volume(path, letter)))
}

pub(super) fn pagefile_path_on_volume(path: &str, letter: char) -> bool {
    let bytes = path.as_bytes();
    bytes.get(1) == Some(&b':')
        && bytes
            .first()
            .is_some_and(|drive| drive.eq_ignore_ascii_case(&(letter as u8)))
}

fn size_of_val<T>(v: &T) -> usize {
    std::mem::size_of_val(v)
}

fn last_err(op: &str) -> String {
    let e = unsafe { windows_sys::Win32::Foundation::GetLastError() };
    format!("{op} win32={e}")
}

pub(super) fn fsctl(handle: HANDLE, code: u32) -> bool {
    let mut ret = 0u32;
    unsafe {
        windows_sys::Win32::System::IO::DeviceIoControl(
            handle,
            code,
            ptr::null(),
            0,
            ptr::null_mut(),
            0,
            &mut ret,
            ptr::null_mut(),
        ) != FALSE
    }
}

fn volume_disk_number(handle: HANDLE) -> Result<u32, String> {
    let mut extents = VolumeDiskExtentsOne::default();
    let mut returned = 0u32;
    let ok = unsafe {
        windows_sys::Win32::System::IO::DeviceIoControl(
            handle,
            IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
            ptr::null(),
            0,
            &mut extents as *mut _ as *mut std::ffi::c_void,
            std::mem::size_of::<VolumeDiskExtentsOne>() as u32,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if ok == FALSE {
        return Err(last_err("IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS"));
    }
    if extents.number_of_disk_extents != 1
        || returned < std::mem::size_of::<VolumeDiskExtentsOne>() as u32
    {
        return Err(format!(
            "volume must have exactly one disk extent count={} bytes={returned}",
            extents.number_of_disk_extents
        ));
    }
    Ok(extents.extents[0].disk_number)
}

fn bcrypt_ok(status: i32) -> bool {
    status >= 0
}

pub(super) fn sha256_hex(data: &[u8]) -> Result<String, String> {
    unsafe {
        let mut alg: BCRYPT_ALG_HANDLE = ptr::null_mut();
        let status = BCryptOpenAlgorithmProvider(&mut alg, BCRYPT_SHA256_ALGORITHM, ptr::null(), 0);
        if !bcrypt_ok(status) {
            return Err(format!("BCryptOpenAlgorithmProvider={status}"));
        }
        let mut hash: BCRYPT_HASH_HANDLE = ptr::null_mut();
        let st = BCryptCreateHash(alg, &mut hash, ptr::null_mut(), 0, ptr::null_mut(), 0, 0);
        if !bcrypt_ok(st) {
            BCryptCloseAlgorithmProvider(alg, 0);
            return Err(format!("BCryptCreateHash={st}"));
        }
        let st = BCryptHashData(
            hash,
            data.as_ptr() as *const _ as *mut _,
            data.len() as u32,
            0,
        );
        if !bcrypt_ok(st) {
            BCryptDestroyHash(hash);
            BCryptCloseAlgorithmProvider(alg, 0);
            return Err(format!("BCryptHashData={st}"));
        }
        let mut out = [0u8; 32];
        let st = BCryptFinishHash(hash, out.as_mut_ptr(), 32, 0);
        BCryptDestroyHash(hash);
        BCryptCloseAlgorithmProvider(alg, 0);
        if !bcrypt_ok(st) {
            return Err(format!("BCryptFinishHash={st}"));
        }
        Ok(out.iter().map(|b| format!("{b:02x}")).collect())
    }
}

pub(super) fn parse_configured_pagefile_entry(entry: &str) -> Result<String, String> {
    let entry = entry.trim();
    let (path_and_min, max) = entry
        .rsplit_once(char::is_whitespace)
        .ok_or_else(|| "PagingFiles entry missing maximum size".to_string())?;
    let (path, min) = path_and_min
        .trim_end()
        .rsplit_once(char::is_whitespace)
        .ok_or_else(|| "PagingFiles entry missing minimum size".to_string())?;
    if min.parse::<u64>().is_err() || max.parse::<u64>().is_err() {
        return Err("PagingFiles entry has non-numeric size".into());
    }
    let path = path.trim();
    if path.len() < 3 || path.as_bytes().get(1) != Some(&b':') {
        return Err("PagingFiles entry has invalid drive path".into());
    }
    Ok(path.to_string())
}

pub(super) fn pagefile_identity(path: String) -> PagefileIdentity {
    let volume = path.get(..3).unwrap_or("").to_string();
    PagefileIdentity { name: path, volume }
}
