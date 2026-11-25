// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-dxg/src/lib.rs
//! Minimal `/dev/dxg` WDDM video-memory budget provider.
//!
//! The layouts mirror Microsoft's WSL 6.18 `d3dkmthk.h`. This crate is the
//! only `unsafe` boundary for dxg ioctls; policy remains in safe Rust.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::time::Instant;

use ramshared_vram::{GpuAdapterIdentity, GpuBudgetSnapshot, GpuBudgetSource};

unsafe extern "C" {
    fn ioctl(fd: i32, request: u64, ...) -> i32;
}

mod process_identity;
mod query_scope;
pub use process_identity::LinuxProcessIdentity;
#[cfg(test)]
mod process_scope_tests;
pub use query_scope::{BudgetProcess, BudgetQueryScope, BudgetRequest, MemorySegmentGroup};

use ramshared_vram::host_identity::{
    INTERFACE_UTF16_CAPACITY, decode_interface_id_utf16, validate_interface_id,
};

pub mod uapi {
    pub const ENUM_ADAPTERS2_IOCTL: u64 = 0xc010_4714;
    pub const QUERY_ADAPTER_INFO_IOCTL: u64 = 0xc018_4709;
    pub const QUERY_VIDEO_MEMORY_INFO_IOCTL: u64 = 0xc038_470a;
    pub const CLOSE_ADAPTER_IOCTL: u64 = 0xc004_4715;
    pub const KMTQAITYPE_VGPUINTERFACEID: u32 = 79;
    pub const MAX_ADAPTERS: usize = 64;

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct QueryAdapterInfo {
        pub adapter: u32,
        pub query_type: u32,
        pub private_driver_data: u64,
        pub private_driver_data_size: u32,
        pub _pad: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct AdapterInfo {
        pub adapter_handle: u32,
        pub luid_low: u32,
        pub luid_high: u32,
        pub num_sources: u32,
        pub present_move_regions_preferred: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct EnumAdapters2 {
        pub num_adapters: u32,
        pub reserved: u32,
        pub adapters: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct QueryVideoMemoryInfo {
        pub process: u64,
        pub adapter: u32,
        pub memory_segment_group: i32,
        pub budget: u64,
        pub current_usage: u64,
        pub current_reservation: u64,
        pub available_for_reservation: u64,
        pub physical_adapter_index: u32,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdapterLuid {
    pub low: u32,
    pub high: u32,
}

impl fmt::Display for AdapterLuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:08x}:{:08x}", self.high, self.low)
    }
}

impl AdapterLuid {
    /// Parses the canonical `high:low` hexadecimal representation used by the shared GPU identity.
    pub fn parse_normalized(value: &str) -> Result<Self, DxgError> {
        let Some((high, low)) = value.split_once(':') else {
            return Err(DxgError::Malformed("adapter_luid"));
        };
        if high.len() != 8 || low.len() != 8 || value.bytes().any(|byte| byte.is_ascii_uppercase())
        {
            return Err(DxgError::Malformed("adapter_luid"));
        }
        let high =
            u32::from_str_radix(high, 16).map_err(|_| DxgError::Malformed("adapter_luid"))?;
        let low = u32::from_str_radix(low, 16).map_err(|_| DxgError::Malformed("adapter_luid"))?;
        if high == 0 && low == 0 {
            return Err(DxgError::Malformed("adapter_luid"));
        }
        Ok(Self { low, high })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdapterLuidNamespace {
    /// Physical Windows adapter namespace used by CUDA, Vulkan and DXGI.
    WindowsHost,
    /// Guest VM-bus channel namespace returned by WSL DXG enumeration.
    WslGuest,
}

#[derive(Clone, Debug)]
pub struct BudgetSnapshot {
    pub adapter: AdapterLuid,
    pub adapter_namespace: AdapterLuidNamespace,
    pub adapter_interface_id: Option<String>,
    pub query_scope: BudgetQueryScope,
    pub budget: u64,
    pub current_usage: u64,
    pub current_reservation: u64,
    pub available_for_reservation: u64,
    pub sampled_at: Instant,
}

impl BudgetSnapshot {
    /// Converts WDDM's adapter-bound segment budget into the shared provider contract.
    /// WDDM reports a budget rather than physical capacity, so total capacity stays unknown.
    pub fn to_vram_budget(&self) -> GpuBudgetSnapshot {
        GpuBudgetSnapshot {
            adapter: Some(GpuAdapterIdentity {
                node_mask: (self.adapter_namespace == AdapterLuidNamespace::WindowsHost)
                    .then(|| 1_u32.checked_shl(self.query_scope.physical_adapter_index))
                    .flatten(),
                backend: match self.adapter_namespace {
                    AdapterLuidNamespace::WindowsHost => "dxg",
                    AdapterLuidNamespace::WslGuest => "dxg-guest",
                }
                .into(),
                key: self.adapter.to_string(),
                luid: match self.adapter_namespace {
                    AdapterLuidNamespace::WindowsHost => Some(self.adapter.to_string()),
                    AdapterLuidNamespace::WslGuest => None,
                },
            }),
            total_bytes: None,
            budget_bytes: self.budget,
            used_bytes: self.current_usage,
            source: GpuBudgetSource::DriverReported,
            sampled_at: self.sampled_at,
        }
    }
}

pub trait GpuBudgetProvider {
    fn snapshot(&self) -> Result<BudgetSnapshot, DxgError>;
}

#[derive(Debug, Eq, PartialEq)]
pub enum DxgError {
    Unavailable(String),
    Io(String),
    DeviceNotFound,
    UnsupportedHardware,
    BufferOverflow,
    PermissionDenied,
    NoAdapters,
    AmbiguousAdapters(usize),
    AdapterNotFound(AdapterLuid),
    InterfaceNotFound(String),
    DuplicateInterfaceId,
    TooManyAdapters(u32),
    Malformed(&'static str),
    BadAddress,
}

impl fmt::Display for DxgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(message) => write!(f, "dxg unavailable: {message}"),
            Self::Io(message) => write!(f, "dxg ioctl failed: {message}"),
            Self::DeviceNotFound => write!(f, "dxg device not found"),
            Self::UnsupportedHardware => write!(f, "dxg unsupported hardware"),
            Self::BadAddress => write!(f, "dxg bad memory address"),
            Self::BufferOverflow => write!(f, "dxg buffer overflow"),
            Self::PermissionDenied => write!(f, "dxg permission denied"),
            Self::NoAdapters => write!(f, "dxg returned no adapters"),
            Self::AmbiguousAdapters(count) => {
                write!(f, "dxg returned {count} adapters; explicit LUID required")
            }
            Self::AdapterNotFound(luid) => write!(f, "dxg adapter LUID {luid} not found"),
            Self::InterfaceNotFound(interface_id) => {
                write!(f, "dxg adapter interface {interface_id} not found")
            }
            Self::DuplicateInterfaceId => write!(f, "dxg returned duplicate adapter interface"),
            Self::TooManyAdapters(count) => write!(f, "dxg adapter count {count} exceeds 64"),
            Self::Malformed(field) => write!(f, "dxg returned malformed field: {field}"),
        }
    }
}

impl std::error::Error for DxgError {}

impl DxgError {
    pub fn from_sys_error(error: std::io::Error) -> Self {
        match error.raw_os_error() {
            Some(libc::ENODEV) => Self::DeviceNotFound,
            Some(libc::EFAULT) => Self::BadAddress,
            Some(libc::ENOTTY) => Self::UnsupportedHardware,
            Some(libc::EOVERFLOW) => Self::BufferOverflow,
            Some(libc::EPERM) | Some(libc::EACCES) => Self::PermissionDenied,
            _ => Self::Io(error.to_string()),
        }
    }

    pub fn permits_startup_fallback(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }
}

pub fn select_adapter(
    adapters: &[AdapterLuid],
    requested: Option<AdapterLuid>,
) -> Result<AdapterLuid, DxgError> {
    if let Some(luid) = requested {
        return adapters
            .iter()
            .copied()
            .find(|candidate| *candidate == luid)
            .ok_or(DxgError::AdapterNotFound(luid));
    }
    match adapters {
        [] => Err(DxgError::NoAdapters),
        [only] => Ok(*only),
        many => Err(DxgError::AmbiguousAdapters(many.len())),
    }
}

pub struct DxgBudgetProvider {
    file: File,
    adapter_handle: u32,
    adapter_luid: AdapterLuid,
    close: fn(&File, u32),
    query_scope: BudgetQueryScope,
}

impl DxgBudgetProvider {
    pub fn open(requested: Option<AdapterLuid>) -> Result<Self, DxgError> {
        Self::open_path("/dev/dxg", requested)
    }

    /// Standalone diagnostic of physical index zero; product composition uses
    /// `open_request` with the allocator's observed node instead.
    pub fn open_path(
        path: impl AsRef<Path>,
        requested: Option<AdapterLuid>,
    ) -> Result<Self, DxgError> {
        Self::open_path_request(
            path,
            BudgetRequest {
                adapter: requested,
                physical_adapter_index: Some(0),
            },
        )
    }

    pub fn open_request(request: BudgetRequest) -> Result<Self, DxgError> {
        Self::open_path_request("/dev/dxg", request)
    }

    pub fn open_interface(
        interface_id: &str,
        physical_adapter_index: u32,
    ) -> Result<Self, DxgError> {
        Self::open_path_interface("/dev/dxg", interface_id, physical_adapter_index)
    }

    fn open_path_request(path: impl AsRef<Path>, request: BudgetRequest) -> Result<Self, DxgError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(open_error)?;
        Self::from_file_request(file, request, enumerate, close_adapter)
    }

    fn open_path_interface(
        path: impl AsRef<Path>,
        interface_id: &str,
        physical_adapter_index: u32,
    ) -> Result<Self, DxgError> {
        if validate_interface_id(interface_id).is_err() {
            return Err(DxgError::Malformed("interface_id"));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(open_error)?;
        Self::from_file_interface(
            file,
            interface_id,
            physical_adapter_index,
            enumerate,
            |file, handle, scope| {
                query_adapter_interface(handle, scope, |query| {
                    ioctl_mut_allow_unsupported(file, uapi::QUERY_ADAPTER_INFO_IOCTL, query)
                })
            },
            close_adapter,
        )
    }

    fn from_file_request(
        file: File,
        request: BudgetRequest,
        enumerate: impl FnOnce(&File) -> Result<Vec<uapi::AdapterInfo>, DxgError>,
        close: fn(&File, u32),
    ) -> Result<Self, DxgError> {
        let query_scope = validated_local_scope(request.physical_adapter_index)?;
        let infos = enumerate(&file)?;
        Self::from_infos_with_close(file, infos, request.adapter, query_scope, close)
    }

    fn from_file_interface(
        file: File,
        interface_id: &str,
        physical_adapter_index: u32,
        enumerate: impl FnOnce(&File) -> Result<Vec<uapi::AdapterInfo>, DxgError>,
        query_interface: impl FnMut(&File, u32, &BudgetQueryScope) -> Result<Option<String>, DxgError>,
        close: fn(&File, u32),
    ) -> Result<Self, DxgError> {
        if validate_interface_id(interface_id).is_err() {
            return Err(DxgError::Malformed("interface_id"));
        }
        let query_scope = validated_local_scope(Some(physical_adapter_index))?;
        let infos = enumerate(&file)?;
        Self::from_infos_interface_with_close(
            file,
            infos,
            interface_id,
            query_scope,
            query_interface,
            close,
        )
    }

    fn from_infos_with_close(
        file: File,
        infos: Vec<uapi::AdapterInfo>,
        requested: Option<AdapterLuid>,
        query_scope: BudgetQueryScope,
        close: fn(&File, u32),
    ) -> Result<Self, DxgError> {
        let selected = (|| {
            validate_adapter_infos(&infos, &query_scope)?;
            let luids: Vec<_> = infos
                .iter()
                .map(|info| AdapterLuid {
                    low: info.luid_low,
                    high: info.luid_high,
                })
                .collect();
            let selected = select_adapter(&luids, requested)?;
            let info = infos
                .iter()
                .find(|info| info.luid_low == selected.low && info.luid_high == selected.high)
                .copied()
                .ok_or(DxgError::AdapterNotFound(selected))?;
            Ok((info, selected))
        })();
        let retained = selected.as_ref().ok().map(|(info, _)| info.adapter_handle);
        close_owned_infos(&infos, retained, |handle| close(&file, handle));
        let (selected_info, selected) = selected?;
        Ok(Self {
            file,
            adapter_handle: selected_info.adapter_handle,
            adapter_luid: selected,
            query_scope,
            close,
        })
    }

    fn from_infos_interface_with_close(
        file: File,
        infos: Vec<uapi::AdapterInfo>,
        requested_interface: &str,
        query_scope: BudgetQueryScope,
        mut query_interface: impl FnMut(
            &File,
            u32,
            &BudgetQueryScope,
        ) -> Result<Option<String>, DxgError>,
        close: fn(&File, u32),
    ) -> Result<Self, DxgError> {
        let selected = (|| {
            if validate_interface_id(requested_interface).is_err() {
                return Err(DxgError::Malformed("interface_id"));
            }
            validate_adapter_infos(&infos, &query_scope)?;
            let mut seen: Vec<String> = Vec::with_capacity(infos.len());
            let mut matched: Option<(uapi::AdapterInfo, AdapterLuid)> = None;
            for &info in &infos {
                let iface = query_interface(&file, info.adapter_handle, &query_scope)?
                    .ok_or(DxgError::UnsupportedHardware)?;
                if seen.iter().any(|prev| prev.eq_ignore_ascii_case(&iface)) {
                    return Err(DxgError::DuplicateInterfaceId);
                }
                if iface.eq_ignore_ascii_case(requested_interface) {
                    matched = Some((
                        info,
                        AdapterLuid {
                            low: info.luid_low,
                            high: info.luid_high,
                        },
                    ));
                }
                seen.push(iface);
            }
            matched.ok_or_else(|| DxgError::InterfaceNotFound(requested_interface.to_owned()))
        })();
        let retained = selected.as_ref().ok().map(|(info, _)| info.adapter_handle);
        close_owned_infos(&infos, retained, |handle| close(&file, handle));
        let (selected_info, selected) = selected?;
        Ok(Self {
            file,
            adapter_handle: selected_info.adapter_handle,
            adapter_luid: selected,
            query_scope,
            close,
        })
    }

    pub fn adapter_luid(&self) -> AdapterLuid {
        self.adapter_luid
    }

    pub fn adapter_interface(&self) -> Result<Option<String>, DxgError> {
        if !self.query_scope.is_current_process_local()
            || !self
                .query_scope
                .process_identity
                .as_ref()
                .is_some_and(LinuxProcessIdentity::is_alive)
        {
            return Err(DxgError::Malformed("process_scope"));
        }
        query_adapter_interface(self.adapter_handle, &self.query_scope, |query| {
            ioctl_mut_allow_unsupported(&self.file, uapi::QUERY_ADAPTER_INFO_IOCTL, query)
        })
    }
}

impl GpuBudgetProvider for DxgBudgetProvider {
    fn snapshot(&self) -> Result<BudgetSnapshot, DxgError> {
        if !self.query_scope.is_current_process_local() {
            return Err(DxgError::Malformed("process_scope"));
        }
        let adapter_interface_id = self.adapter_interface()?;
        query_memory_snapshot_with_interface(
            self.adapter_handle,
            self.adapter_luid,
            &self.query_scope,
            adapter_interface_id,
            |query| ioctl_mut(&self.file, uapi::QUERY_VIDEO_MEMORY_INFO_IOCTL, query),
        )
    }
}

fn open_error(error: std::io::Error) -> DxgError {
    if error.kind() == std::io::ErrorKind::NotFound {
        DxgError::Unavailable(error.to_string())
    } else {
        DxgError::from_sys_error(error)
    }
}

fn query_adapter_interface(
    adapter_handle: u32,
    scope: &BudgetQueryScope,
    query_info: impl FnOnce(&mut uapi::QueryAdapterInfo) -> Result<bool, DxgError>,
) -> Result<Option<String>, DxgError> {
    if !scope.is_current_process_local() {
        return Err(DxgError::Malformed("query_scope"));
    }
    let mut buffer = [u16::MAX; INTERFACE_UTF16_CAPACITY];
    let buffer_ptr = buffer.as_mut_ptr() as u64;
    let buffer_bytes = u32::try_from(std::mem::size_of_val(&buffer))
        .map_err(|_| DxgError::Malformed("vgpu_interface_id"))?;
    let mut query = uapi::QueryAdapterInfo {
        adapter: adapter_handle,
        query_type: uapi::KMTQAITYPE_VGPUINTERFACEID,
        private_driver_data: buffer_ptr,
        private_driver_data_size: buffer_bytes,
        _pad: 0,
    };
    if !query_info(&mut query)? {
        return Ok(None);
    }
    if query.adapter != adapter_handle
        || query.query_type != uapi::KMTQAITYPE_VGPUINTERFACEID
        || query.private_driver_data != buffer_ptr
        || query.private_driver_data_size != buffer_bytes
        || query._pad != 0
    {
        return Err(DxgError::Malformed("query_adapter_info"));
    }
    decode_interface_id_utf16(&buffer)
        .map(Some)
        .map_err(|_| DxgError::Malformed("vgpu_interface_id"))
}

#[cfg(test)]
fn query_memory_snapshot(
    adapter_handle: u32,
    adapter_luid: AdapterLuid,
    scope: &BudgetQueryScope,
    query_memory: impl FnOnce(&mut uapi::QueryVideoMemoryInfo) -> Result<(), DxgError>,
) -> Result<BudgetSnapshot, DxgError> {
    query_memory_snapshot_with_interface(adapter_handle, adapter_luid, scope, None, query_memory)
}

fn query_memory_snapshot_with_interface(
    adapter_handle: u32,
    adapter_luid: AdapterLuid,
    scope: &BudgetQueryScope,
    adapter_interface_id: Option<String>,
    query_memory: impl FnOnce(&mut uapi::QueryVideoMemoryInfo) -> Result<(), DxgError>,
) -> Result<BudgetSnapshot, DxgError> {
    if !scope.is_current_process_local() {
        return Err(DxgError::Malformed("query_scope"));
    }
    let sampled_at = Instant::now();
    let mut query = uapi::QueryVideoMemoryInfo {
        adapter: adapter_handle,
        memory_segment_group: 0,
        physical_adapter_index: scope.physical_adapter_index,
        ..Default::default()
    };
    query_memory(&mut query)?;
    validate_query(&query)?;
    if query.adapter != adapter_handle
        || query.memory_segment_group != 0
        || query.physical_adapter_index != scope.physical_adapter_index
    {
        return Err(DxgError::Malformed("query_scope"));
    }
    Ok(BudgetSnapshot {
        query_scope: scope.clone(),
        adapter: adapter_luid,
        adapter_namespace: AdapterLuidNamespace::WslGuest,
        adapter_interface_id,
        budget: query.budget,
        current_usage: query.current_usage,
        current_reservation: query.current_reservation,
        available_for_reservation: query.available_for_reservation,
        sampled_at,
    })
}

impl Drop for DxgBudgetProvider {
    fn drop(&mut self) {
        if self.query_scope.belongs_to_current_process() {
            (self.close)(&self.file, self.adapter_handle);
        }
    }
}

fn enumerate(file: &File) -> Result<Vec<uapi::AdapterInfo>, DxgError> {
    let mut request = uapi::EnumAdapters2::default();
    ioctl_mut(file, uapi::ENUM_ADAPTERS2_IOCTL, &mut request)?;
    validate_enum(&request, None)?;
    let mut infos = vec![uapi::AdapterInfo::default(); request.num_adapters as usize];
    request.adapters = infos.as_mut_ptr() as u64;
    ioctl_mut(file, uapi::ENUM_ADAPTERS2_IOCTL, &mut request)?;
    finish_enumeration(&request, infos, |handle| close_adapter(file, handle))
}

fn finish_enumeration(
    request: &uapi::EnumAdapters2,
    mut infos: Vec<uapi::AdapterInfo>,
    mut close: impl FnMut(u32),
) -> Result<Vec<uapi::AdapterInfo>, DxgError> {
    if let Err(error) = validate_enum(request, Some(infos.len())) {
        close_owned_infos(&infos, None, &mut close);
        return Err(error);
    }
    let count = request.num_adapters as usize;
    if infos[count..].iter().any(|info| info.adapter_handle != 0) {
        close_owned_infos(&infos, None, &mut close);
        return Err(DxgError::Malformed("enum.unreported_handles"));
    }
    infos.truncate(count);
    Ok(infos)
}

/// Each nonzero returned handle is owned once, even if malformed metadata repeats it.
fn close_owned_infos(
    infos: &[uapi::AdapterInfo],
    retained: Option<u32>,
    mut close: impl FnMut(u32),
) {
    for (index, info) in infos.iter().enumerate() {
        if info.adapter_handle != 0
            && Some(info.adapter_handle) != retained
            && !infos[..index]
                .iter()
                .any(|previous| previous.adapter_handle == info.adapter_handle)
        {
            close(info.adapter_handle);
        }
    }
}

fn validate_enum(request: &uapi::EnumAdapters2, capacity: Option<usize>) -> Result<(), DxgError> {
    if request.reserved != 0 {
        return Err(DxgError::Malformed("enum.reserved"));
    }
    if request.num_adapters == 0 {
        return Err(DxgError::NoAdapters);
    }
    let limit = capacity.unwrap_or(uapi::MAX_ADAPTERS);
    if request.num_adapters as usize > limit {
        return Err(DxgError::TooManyAdapters(request.num_adapters));
    }
    Ok(())
}

fn validate_query(query: &uapi::QueryVideoMemoryInfo) -> Result<(), DxgError> {
    if query.process != 0 {
        return Err(DxgError::Malformed("process"));
    }
    if query.adapter == 0 {
        return Err(DxgError::Malformed("adapter"));
    }
    Ok(())
}

fn close_adapter(file: &File, handle: u32) {
    let mut handle = handle;
    let _ = ioctl_mut(file, uapi::CLOSE_ADAPTER_IOCTL, &mut handle);
}

fn validated_local_scope(
    physical_adapter_index: Option<u32>,
) -> Result<BudgetQueryScope, DxgError> {
    let index = physical_adapter_index
        .filter(|index| *index < u32::BITS)
        .ok_or(DxgError::Malformed("physical_adapter_index"))?;
    let query_scope = BudgetQueryScope::current_process_local(index);
    if !query_scope.is_current_process_local() {
        return Err(DxgError::Malformed("process_scope"));
    }
    Ok(query_scope)
}

fn validate_adapter_infos(
    infos: &[uapi::AdapterInfo],
    query_scope: &BudgetQueryScope,
) -> Result<(), DxgError> {
    if query_scope.physical_adapter_index >= u32::BITS {
        return Err(DxgError::Malformed("physical_adapter_index"));
    }
    if !query_scope.is_current_process_local() {
        return Err(DxgError::Malformed("process_scope"));
    }
    for (index, info) in infos.iter().enumerate() {
        if info.adapter_handle == 0 {
            return Err(DxgError::Malformed("adapter_handle"));
        }
        if (info.luid_high == 0 && info.luid_low == 0)
            || infos[..index].iter().any(|previous| {
                previous.adapter_handle == info.adapter_handle
                    || (previous.luid_high == info.luid_high && previous.luid_low == info.luid_low)
            })
        {
            return Err(DxgError::Malformed("adapter_identity"));
        }
    }
    Ok(())
}

fn ioctl_mut<T>(file: &File, request: u64, value: &mut T) -> Result<(), DxgError> {
    // SAFETY: `value` points to the exact repr(C) layout for `request` and stays
    // alive for the synchronous ioctl. The kernel validates nested pointers.
    let result = unsafe { ioctl(file.as_raw_fd(), request, value as *mut T) };
    if result < 0 {
        Err(DxgError::from_sys_error(std::io::Error::last_os_error()))
    } else {
        Ok(())
    }
}

fn ioctl_mut_allow_unsupported<T>(
    file: &File,
    request: u64,
    value: &mut T,
) -> Result<bool, DxgError> {
    // SAFETY: `value` points to the exact repr(C) layout for `request` and stays
    // alive for the synchronous ioctl. The kernel validates nested pointers.
    let result = unsafe { ioctl(file.as_raw_fd(), request, value as *mut T) };
    if result < 0 {
        let error = std::io::Error::last_os_error();
        if matches!(
            error.raw_os_error(),
            Some(libc::ENOTTY | libc::ENOSYS | libc::EOPNOTSUPP)
        ) {
            return Ok(false);
        }
        Err(DxgError::from_sys_error(error))
    } else {
        Ok(true)
    }
}

#[cfg(test)]
mod tests;
