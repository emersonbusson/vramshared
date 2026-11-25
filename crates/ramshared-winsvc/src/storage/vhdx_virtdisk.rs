// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/storage/vhdx_virtdisk.rs
// Native VHDX attachment custodian (SPEC zero-script-native-runtime ITEM-4,
// RF-5, DT-5). Pure receipt/reconcile logic is testable on Linux; the
// VirtDisk.dll adapter is compiled only on Windows.
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
pub const VHDX_LEASE_SCHEMA: u8 = 1;
pub const VHDX_LEASE_FILE: &str = "vhdx-lease.json";
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct VhdxLease {
    pub schema_version: u8,
    pub vhdx_path: String,
    pub disk_guid: String,
    pub physical_drive_number: u32,
    pub boot_id: String,
    pub attached_at_unix_ms: u64,
}

impl VhdxLease {
    pub fn validate(&self) -> Result<(), VhdxError> {
        if self.schema_version != VHDX_LEASE_SCHEMA
            || self.vhdx_path.is_empty()
            || self.disk_guid.is_empty()
            || self.boot_id.is_empty()
        {
            return Err(VhdxError::InvalidLease);
        }
        if !Path::new(&self.vhdx_path).is_absolute() {
            return Err(VhdxError::InvalidLease);
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum VhdxError {
    InvalidLease,
    LeaseActive,
    DetachRefusedActiveLease,
    Io(std::io::Error),
    VirtDisk(String),
    NotWindows,
}

impl std::fmt::Display for VhdxError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidLease => formatter.write_str("invalid VHDX lease"),
            Self::LeaseActive => formatter.write_str("another VHDX lease is active"),
            Self::DetachRefusedActiveLease => {
                formatter.write_str("VHDX detach refused while a lease is active")
            }
            Self::Io(error) => write!(formatter, "VHDX lease io: {error}"),
            Self::VirtDisk(detail) => write!(formatter, "VirtDisk failure: {detail}"),
            Self::NotWindows => formatter.write_str("VirtDisk is only available on Windows"),
        }
    }
}

impl std::error::Error for VhdxError {}

impl From<std::io::Error> for VhdxError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Host-side custody state. `attach` is idempotent for the same lease and
/// refuses a second, different VHDX while one is active.
#[derive(Clone, Debug, Default)]
pub struct VhdxReceipt {
    lease: Option<VhdxLease>,
}

impl VhdxReceipt {
    pub fn new(lease: VhdxLease) -> Result<Self, VhdxError> {
        lease.validate()?;
        Ok(Self { lease: Some(lease) })
    }

    pub fn from_lease(lease: Option<VhdxLease>) -> Result<Self, VhdxError> {
        if let Some(lease) = &lease {
            lease.validate()?;
        }
        Ok(Self { lease })
    }

    pub fn is_active(&self) -> bool {
        self.lease.is_some()
    }

    pub fn lease(&self) -> Option<&VhdxLease> {
        self.lease.as_ref()
    }

    /// Persist this receipt through the same atomic temporary+rename path.
    /// An absent lease removes the receipt, never leaves a stale JSON file.
    pub fn save(&self, path: &Path) -> Result<(), VhdxError> {
        match &self.lease {
            Some(lease) => write_lease_atomic(path, lease),
            None => match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(VhdxError::Io(error)),
            },
        }
    }

    /// Reconcile a new attach request. Byte-identical identity is a safe
    /// no-op; a different active identity is refused before any VirtDisk call.
    pub fn attach_idempotent(&mut self, incoming: &VhdxLease) -> Result<(), VhdxError> {
        incoming.validate()?;
        match &self.lease {
            None => {
                self.lease = Some(incoming.clone());
                Ok(())
            }
            Some(current)
                if current.vhdx_path == incoming.vhdx_path
                    && current.disk_guid == incoming.disk_guid =>
            {
                Ok(())
            }
            Some(_) => Err(VhdxError::LeaseActive),
        }
    }

    /// Detach is allowed only after every WSL session lease and volume lock
    /// is gone. The caller supplies both observations; this function does not
    /// read host state.
    pub fn detach_allowed(
        &mut self,
        active_sessions: u32,
        volume_locked: bool,
    ) -> Result<(), VhdxError> {
        if active_sessions > 0 || volume_locked {
            return Err(VhdxError::DetachRefusedActiveLease);
        }
        self.lease = None;
        Ok(())
    }
}

/// Persist the intent receipt before AttachVirtualDisk. The rename is atomic
/// on the same filesystem; callers keep the temporary file in the destination
/// directory for that reason.
pub fn write_lease_atomic(path: &Path, lease: &VhdxLease) -> Result<(), VhdxError> {
    lease.validate()?;
    let parent = path.parent().ok_or(VhdxError::InvalidLease)?;
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(VHDX_LEASE_FILE),
        std::process::id()
    ));
    let payload = serde_json::to_vec_pretty(lease)
        .map_err(|error| VhdxError::Io(std::io::Error::other(error.to_string())))?;
    {
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(&payload)?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary, path)?;
    Ok(())
}

pub fn lease_path(program_data: &Path) -> PathBuf {
    program_data.join("transactions").join(VHDX_LEASE_FILE)
}

pub fn read_lease(path: &Path) -> Result<Option<VhdxLease>, VhdxError> {
    let payload = match std::fs::read(path) {
        Ok(payload) => payload,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(VhdxError::Io(error)),
    };
    let lease: VhdxLease = serde_json::from_slice(&payload)
        .map_err(|error| VhdxError::Io(std::io::Error::other(error.to_string())))?;
    lease.validate()?;
    Ok(Some(lease))
}

#[cfg(windows)]
mod virtdisk {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};

    use super::{VhdxError, VhdxReceipt};

    const VIRTUAL_STORAGE_TYPE_DEVICE_VHDX: u32 = 3;
    const VIRTUAL_STORAGE_TYPE_VENDOR_MICROSOFT: [u8; 16] = [
        0xEC, 0x98, 0x4A, 0xEC, 0xA0, 0xF9, 0x47, 0xE9, 0x90, 0x1F, 0x71, 0x41, 0x5A, 0x66, 0x34,
        0x5B,
    ];
    const OPEN_VIRTUAL_DISK_VERSION_1: u32 = 1;
    const CREATE_VIRTUAL_DISK_VERSION_1: u32 = 1;
    const VIRTUAL_DISK_ACCESS_NONE: u32 = 0;
    const ATTACH_VIRTUAL_DISK_FLAG_PERMANENT_LIFETIME: u32 = 0x0000_0004;
    const DETACH_VIRTUAL_DISK_FLAG_NONE: u32 = 0;

    #[repr(C)]
    struct VirtualStorageType {
        device_id: u32,
        vendor_id: [u8; 16],
    }

    /// Version 1 layout from `<virtdisk.h>`:
    /// `Version`, `UniqueId[16]`, `MaximumSize`, `BlockSizeInBytes`,
    /// `SectorSizeInBytes`, `ParentPath`, `SourcePath`. `UniqueId` must be
    /// present; omitting it shifts every following field by 16 bytes.
    #[repr(C)]
    struct CreateVirtualDiskParameters {
        version: u32,
        unique_id: [u8; 16],
        maximum_size: u64,
        block_size_in_bytes: u32,
        sector_size_in_bytes: u32,
        parent_path: *const u16,
        source_path: *const u16,
    }

    #[cfg(target_pointer_width = "64")]
    const _: () = assert!(std::mem::size_of::<CreateVirtualDiskParameters>() == 56);

    #[repr(C)]
    struct OpenVirtualDiskParameters {
        version: u32,
        rw_depth: u32,
        reserved: u32,
    }

    #[link(name = "virtdisk")]
    unsafe extern "system" {
        fn CreateVirtualDisk(
            storage_type: *const VirtualStorageType,
            path: *const u16,
            access_mask: u32,
            flags: u32,
            security_descriptor: *const c_void,
            parameters: *const CreateVirtualDiskParameters,
            overlapped: *const c_void,
            handle: *mut HANDLE,
        ) -> u32;
        fn OpenVirtualDisk(
            storage_type: *const c_void,
            path: *const u16,
            access_mask: u32,
            flags: u32,
            parameters: *const c_void,
            handle: *mut HANDLE,
        ) -> u32;
        fn AttachVirtualDisk(
            handle: HANDLE,
            security_descriptor: *const c_void,
            flags: u32,
            provider_specific_flags: u32,
            parameters: *const c_void,
            overlapped: *const c_void,
        ) -> u32;
        fn DetachVirtualDisk(handle: HANDLE, flags: u32, provider_specific_flags: u32) -> u32;
    }

    pub struct VhdxAttachment {
        handle: Option<HANDLE>,
        path: String,
    }

    impl VhdxAttachment {
        pub fn open_and_attach(path: &Path, disk_guid: &str) -> Result<Self, VhdxError> {
            let storage = VirtualStorageType {
                device_id: VIRTUAL_STORAGE_TYPE_DEVICE_VHDX,
                vendor_id: VIRTUAL_STORAGE_TYPE_VENDOR_MICROSOFT,
            };
            let wide: Vec<u16> = path
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            let parameters = OpenVirtualDiskParameters {
                version: OPEN_VIRTUAL_DISK_VERSION_1,
                rw_depth: 1,
                reserved: 0,
            };
            let mut handle: HANDLE = INVALID_HANDLE_VALUE;
            // SAFETY: `storage` and `wide` are NUL-terminated/valid for the
            // call; `handle` is a valid out pointer.
            let result = unsafe {
                OpenVirtualDisk(
                    (&raw const storage).cast(),
                    wide.as_ptr(),
                    VIRTUAL_DISK_ACCESS_NONE,
                    0,
                    (&raw const parameters).cast(),
                    &mut handle,
                )
            };
            if result != 0 || handle == INVALID_HANDLE_VALUE {
                return Err(VhdxError::VirtDisk(format!(
                    "OpenVirtualDisk error {result}"
                )));
            }
            // SAFETY: `handle` was produced by OpenVirtualDisk and is valid.
            let attached = unsafe {
                AttachVirtualDisk(
                    handle,
                    std::ptr::null(),
                    ATTACH_VIRTUAL_DISK_FLAG_PERMANENT_LIFETIME,
                    0,
                    std::ptr::null(),
                    std::ptr::null(),
                )
            };
            if attached != 0 {
                // SAFETY: `handle` is owned by this function.
                unsafe { CloseHandle(handle) };
                return Err(VhdxError::VirtDisk(format!(
                    "AttachVirtualDisk error {attached}"
                )));
            }
            Ok(Self {
                handle: Some(handle),
                path: disk_guid.to_string(),
            })
        }

        pub fn create(path: &Path, maximum_size: u64, disk_guid: &str) -> Result<Self, VhdxError> {
            let storage = VirtualStorageType {
                device_id: VIRTUAL_STORAGE_TYPE_DEVICE_VHDX,
                vendor_id: VIRTUAL_STORAGE_TYPE_VENDOR_MICROSOFT,
            };
            let wide: Vec<u16> = path
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            let parameters = CreateVirtualDiskParameters {
                version: CREATE_VIRTUAL_DISK_VERSION_1,
                unique_id: [0u8; 16],
                maximum_size,
                block_size_in_bytes: 0,
                sector_size_in_bytes: 0,
                parent_path: std::ptr::null(),
                source_path: std::ptr::null(),
            };
            let mut handle: HANDLE = INVALID_HANDLE_VALUE;
            // SAFETY: all pointers are valid for the call; `handle` is an out
            // pointer owned by this function.
            let result = unsafe {
                CreateVirtualDisk(
                    &storage,
                    wide.as_ptr(),
                    VIRTUAL_DISK_ACCESS_NONE,
                    0,
                    std::ptr::null(),
                    &parameters,
                    std::ptr::null(),
                    &mut handle,
                )
            };
            if result != 0 || handle == INVALID_HANDLE_VALUE {
                return Err(VhdxError::VirtDisk(format!(
                    "CreateVirtualDisk error {result}"
                )));
            }
            // SAFETY: `handle` was produced by CreateVirtualDisk.
            let attached = unsafe {
                AttachVirtualDisk(
                    handle,
                    std::ptr::null(),
                    ATTACH_VIRTUAL_DISK_FLAG_PERMANENT_LIFETIME,
                    0,
                    std::ptr::null(),
                    std::ptr::null(),
                )
            };
            if attached != 0 {
                // SAFETY: `handle` is owned by this function.
                unsafe { CloseHandle(handle) };
                return Err(VhdxError::VirtDisk(format!(
                    "AttachVirtualDisk error {attached}"
                )));
            }
            Ok(Self {
                handle: Some(handle),
                path: disk_guid.to_string(),
            })
        }

        pub fn detach(
            mut self,
            receipt: &mut VhdxReceipt,
            active_sessions: u32,
            volume_locked: bool,
        ) -> Result<(), VhdxError> {
            receipt.detach_allowed(active_sessions, volume_locked)?;
            let handle = self
                .handle
                .take()
                .ok_or_else(|| VhdxError::VirtDisk("attachment handle is closed".to_string()))?;
            // SAFETY: `handle` is owned by this attachment and valid.
            let result = unsafe { DetachVirtualDisk(handle, DETACH_VIRTUAL_DISK_FLAG_NONE, 0) };
            // SAFETY: `handle` is owned and closed exactly once.
            unsafe { CloseHandle(handle) };
            if result != 0 {
                return Err(VhdxError::VirtDisk(format!(
                    "DetachVirtualDisk error {result}"
                )));
            }
            Ok(())
        }

        pub fn path(&self) -> &str {
            &self.path
        }
    }

    impl Drop for VhdxAttachment {
        fn drop(&mut self) {
            // PERMANENT_LIFETIME means Drop must only close the handle; the
            // explicit detach path owns DetachVirtualDisk and takes the handle
            // first, so Drop never double-closes it.
            if let Some(handle) = self.handle.take() {
                // SAFETY: the handle is owned by this attachment.
                unsafe { CloseHandle(handle) };
            }
        }
    }
}

#[cfg(windows)]
pub use virtdisk::VhdxAttachment;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod edge_tests;
