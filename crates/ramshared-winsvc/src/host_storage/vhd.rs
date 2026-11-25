// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/host_storage/vhd.rs
//! Read-only VHD identifier observations. No attachment or swap authority.

#[cfg(any(windows, test))]
fn extract_disk_identifier(version: u32, used: u32, identifier: &[u8; 16]) -> Option<[u8; 16]> {
    // Version 14 is VirtualDiskId (page-83 identity), not Version 2
    // Identifier, which can identify a different VHD metadata object.
    (version == 14 && (24..=40).contains(&used) && *identifier != [0; 16]).then_some(*identifier)
}

/// Query the file as an information-only, read-only VHD. The caller must still
/// bind this path observation to a held file, a guest device and a live lease.
/// Execute under bounded child custody because native disk APIs may block.
#[cfg(windows)]
pub fn observe_vhd_identifier(path: &std::path::Path) -> Option<[u8; 16]> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};

    #[repr(C)]
    struct StorageType {
        device_id: u32,
        vendor_id: [u8; 16],
    }
    #[repr(C)]
    struct OpenParameters {
        version: u32,
        get_info_only: i32,
        read_only: i32,
        resiliency_guid: [u8; 16],
    }
    #[repr(C, align(8))]
    struct DiskInfo {
        version: u32,
        padding: u32,
        data: [u8; 32],
    }
    const _: () = assert!(std::mem::size_of::<OpenParameters>() == 28);
    const _: () = assert!(std::mem::size_of::<DiskInfo>() == 40);
    #[link(name = "virtdisk")]
    unsafe extern "system" {
        fn OpenVirtualDisk(
            storage: *const std::ffi::c_void,
            path: *const u16,
            access: u32,
            flags: u32,
            params: *const std::ffi::c_void,
            handle: *mut HANDLE,
        ) -> u32;
        fn GetVirtualDiskInformation(
            handle: HANDLE,
            size: *mut u32,
            info: *mut DiskInfo,
            used: *mut u32,
        ) -> u32;
    }
    if !path.is_absolute() {
        return None;
    }
    let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    if name.len() > 32767 || name[..name.len() - 1].contains(&0) {
        return None;
    }
    let storage = StorageType {
        device_id: 3,
        vendor_id: [
            0xec, 0x98, 0x4a, 0xec, 0xa0, 0xf9, 0x47, 0xe9, 0x90, 0x1f, 0x71, 0x41, 0x5a, 0x66,
            0x34, 0x5b,
        ],
    };
    let params = OpenParameters {
        version: 2,
        get_info_only: 1,
        read_only: 1,
        resiliency_guid: [0; 16],
    };
    let mut handle: HANDLE = INVALID_HANDLE_VALUE;
    // SAFETY: exact SDK structures, read-only info mode, live output handle.
    // 0x400 permits sparse VHDX files; no attach, write or resize is requested.
    if unsafe {
        OpenVirtualDisk(
            (&raw const storage).cast(),
            name.as_ptr(),
            0,
            0x400,
            (&raw const params).cast(),
            &mut handle,
        )
    } != 0
        || handle.is_null()
        || handle == INVALID_HANDLE_VALUE
    {
        return None;
    }
    // SAFETY: successful OpenVirtualDisk transfers this unique handle.
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    let mut info = DiskInfo {
        version: 14,
        padding: 0,
        data: [0; 32],
    };
    let mut size = std::mem::size_of::<DiskInfo>() as u32;
    let mut used = 0u32;
    // SAFETY: owned handle and exact fixed-size buffer and output pointers.
    if unsafe { GetVirtualDiskInformation(handle.as_raw_handle(), &mut size, &mut info, &mut used) }
        != 0
    {
        return None;
    }
    let id: [u8; 16] = info.data[..16].try_into().ok()?;
    extract_disk_identifier(info.version, used, &id)
}

#[cfg(test)]
#[path = "vhd/tests.rs"]
mod tests;
