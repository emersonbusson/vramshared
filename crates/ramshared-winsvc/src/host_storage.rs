// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/host_storage.rs
//! Host-volume observations for later authenticated storage admission.
//!
//! These samples are informational only. They do not bind a guest mount to a
//! VHDX, reserve physical capacity, or authorize allocation or swapoff.

use serde::{Deserialize, Serialize};

pub mod candidate;
pub mod discovery;
pub mod exchange;
pub mod vhd;

#[cfg(windows)]
pub mod sampler;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostFileIdentity {
    pub volume_serial: u32,
    pub file_index: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostVolumeSample {
    pub sampled_at_unix_ms: u64,
    pub volume_id: String,
    pub file_identity: HostFileIdentity,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub available_bytes: u64,
}

fn validate_sample(
    sampled_at_unix_ms: u64,
    volume_id: &str,
    file_identity: HostFileIdentity,
    total_bytes: u64,
    free_bytes: u64,
    available_bytes: u64,
) -> Option<HostVolumeSample> {
    if sampled_at_unix_ms == 0
        || volume_id.is_empty()
        || total_bytes == 0
        || free_bytes > total_bytes
        || available_bytes > free_bytes
    {
        return None;
    }
    Some(HostVolumeSample {
        sampled_at_unix_ms,
        volume_id: volume_id.into(),
        file_identity,
        total_bytes,
        free_bytes,
        available_bytes,
    })
}

/// Extract a local volume GUID directly from a handle-resolved Windows path.
fn volume_guid_root(path: &[u16]) -> Option<String> {
    let path = String::from_utf16(path).ok()?;
    let prefix = r"\\?\Volume{";
    let rest = path.strip_prefix(prefix)?;
    let guid = rest.get(..36)?;
    if !guid.bytes().enumerate().all(|(index, byte)| {
        if matches!(index, 8 | 13 | 18 | 23) {
            byte == b'-'
        } else {
            byte.is_ascii_hexdigit()
        }
    }) || !rest.get(36..)?.starts_with("}\\")
    {
        return None;
    }
    Some(path.get(..prefix.len() + 38)?.to_string())
}

/// A bounded producer report; it is not a guest binding or allocation authority.
pub const MAX_STORAGE_REPORT_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostStorageReport {
    pub schema_version: u8,
    pub producer: crate::gpu_identity::ProducerProcess,
    pub started_at_unix_ms: u64,
    pub elapsed_ms: u64,
    pub sample: HostVolumeSample,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtual_disk_id: Option<[u8; 16]>,
}

impl HostStorageReport {
    pub fn validate(&self) -> Result<(), String> {
        let end = self
            .started_at_unix_ms
            .checked_add(self.elapsed_ms)
            .ok_or("storage observation window overflow")?;
        let sample = &self.sample;
        if self.schema_version != 1
            || self.producer.pid == 0
            || self.producer.creation_time_100ns == 0
            || self.started_at_unix_ms == 0
            || self.virtual_disk_id == Some([0; 16])
            || self.elapsed_ms >= ramshared_vram::GPU_BUDGET_MAX_AGE_MS
            || !(self.started_at_unix_ms..=end).contains(&sample.sampled_at_unix_ms)
            || volume_guid_root(&sample.volume_id.encode_utf16().collect::<Vec<_>>()).as_deref()
                != Some(sample.volume_id.as_str())
            || validate_sample(
                sample.sampled_at_unix_ms,
                &sample.volume_id,
                sample.file_identity.clone(),
                sample.total_bytes,
                sample.free_bytes,
                sample.available_bytes,
            )
            .is_none()
        {
            return Err("storage report identity, capacity or timing is invalid".into());
        }
        Ok(())
    }
}

pub fn decode_report(bytes: &[u8]) -> Result<HostStorageReport, String> {
    if bytes.len() > MAX_STORAGE_REPORT_BYTES {
        return Err("storage report exceeds its byte bound".into());
    }
    let report: HostStorageReport =
        serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    report.validate()?;
    Ok(report)
}

impl crate::gpu_identity::supervisor::ProbeReport for HostStorageReport {
    fn decode(bytes: &[u8]) -> Result<Self, String> {
        decode_report(bytes)
    }
    fn producer(&self) -> &crate::gpu_identity::ProducerProcess {
        &self.producer
    }
    fn window(&self) -> (u64, u64) {
        (self.started_at_unix_ms, self.elapsed_ms)
    }
}

/// Read-only child entrypoint; a serving process must provide bounded custody.
pub fn inspect_command(path: &std::path::Path) -> i32 {
    #[cfg(windows)]
    let result = inspect_file(path).and_then(|report| {
        use std::io::Write;
        let bytes = serde_json::to_vec(&report).map_err(|error| error.to_string())?;
        if bytes.len() > MAX_STORAGE_REPORT_BYTES {
            return Err("storage report exceeds its byte bound".into());
        }
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(&bytes)
            .and_then(|()| stdout.write_all(b"\n"))
            .map_err(|error| error.to_string())
    });
    #[cfg(not(windows))]
    let result: Result<(), String> = {
        let _ = path;
        Err("native host storage inspection requires Windows".into())
    };
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("storage observation failed: {error}");
            2
        }
    }
}

/// Observe a file in the current producer process. A serving parent must use
/// bounded child custody; this function does not authorize an allocation.
#[cfg(windows)]
pub fn inspect_file(path: &std::path::Path) -> Result<HostStorageReport, String> {
    use std::time::{Instant, SystemTime, UNIX_EPOCH};
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentProcessId, GetProcessTimes,
    };

    let started = Instant::now();
    let started_at_unix_ms = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_millis(),
    )
    .map_err(|error| error.to_string())?;
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: the process pseudo-handle is valid and all outputs are distinct.
    if unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let pinned = open_pinned(path).ok_or("file custody unavailable")?;
    let mut sample = sample_open_file(&pinned).ok_or("file volume observation unavailable")?;
    let virtual_disk_id = vhd::observe_vhd_identifier(path);
    let current = std::fs::File::open(path)
        .ok()
        .and_then(|file| sample_open_file(&file))
        .ok_or("file identity reobservation unavailable")?;
    if sample.file_identity != current.file_identity || sample.volume_id != current.volume_id {
        return Err("file identity changed during VHD observation".into());
    }
    // Age the observation from the beginning of the potentially blocking
    // query, never from completion. This also avoids wall/monotonic rounding
    // differences placing the sample outside its process observation window.
    sample.sampled_at_unix_ms = started_at_unix_ms;
    let report = HostStorageReport {
        schema_version: 1,
        producer: crate::gpu_identity::ProducerProcess {
            // SAFETY: Windows returns the ID of this process without reopening it.
            pid: unsafe { GetCurrentProcessId() },
            creation_time_100ns: (u64::from(created.dwHighDateTime) << 32)
                | u64::from(created.dwLowDateTime),
        },
        started_at_unix_ms,
        elapsed_ms: u64::try_from(started.elapsed().as_millis())
            .map_err(|error| error.to_string())?,
        sample,
        virtual_disk_id,
    };
    report.validate()?;
    Ok(report)
}

/// Retain the file without sharing delete access through a complete sample.
#[cfg(windows)]
fn open_pinned(path: &std::path::Path) -> Option<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
    if !path.is_absolute() {
        return None;
    }
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(path)
        .ok()
}

/// Observe the volume holding a named local file. Run in an isolated bounded
/// observer: filesystem calls may block. This does not prove a guest binding.
#[cfg(windows)]
pub fn sample_file(path: &std::path::Path) -> Option<HostVolumeSample> {
    if !path.is_absolute() {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    sample_open_file(&file)
}

/// Measure the volume of the exact file held open through the query.
#[cfg(windows)]
pub fn sample_open_file(file: &std::fs::File) -> Option<HostVolumeSample> {
    use std::os::windows::io::AsRawHandle;
    use std::time::{SystemTime, UNIX_EPOCH};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetDiskFreeSpaceExW, GetDriveTypeW, GetFileInformationByHandle,
        GetFinalPathNameByHandleW,
    };

    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let handle = file.as_raw_handle();
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    let mut name = [0u16; 32768];
    let length = unsafe {
        if GetFileInformationByHandle(handle, &mut info) == 0 {
            return None;
        }
        // VOLUME_NAME_GUID binds the result to the file's volume, without
        // resolving a potentially replaced drive-letter mount point.
        GetFinalPathNameByHandleW(handle, name.as_mut_ptr(), name.len() as u32, 1)
    } as usize;
    if length == 0 || length >= name.len() {
        return None;
    }
    name[length] = 0;
    let identity = HostFileIdentity {
        volume_serial: info.dwVolumeSerialNumber,
        file_index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    };
    let id = volume_guid_root(&name[..length])?;
    let root: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
    // DRIVE_FIXED, excluding removable, remote and unknown storage.
    const DRIVE_FIXED: u32 = 3;
    let mut available = 0u64;
    let mut total = 0u64;
    let mut free = 0u64;
    unsafe {
        if GetDriveTypeW(root.as_ptr()) != DRIVE_FIXED
            || GetDiskFreeSpaceExW(root.as_ptr(), &mut available, &mut total, &mut free) == 0
        {
            return None;
        }
    }
    let sampled_at = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_millis(),
    )
    .ok()?;
    validate_sample(sampled_at, &id, identity, total, free, available)
}

pub use ramshared_ipc::storage_proof::{format_disk_id_hex, parse_disk_id_hex};

#[cfg(any(windows, test))]
fn select_inspected_storage(
    disk_id: [u8; 16],
    reports: impl IntoIterator<Item = Result<HostStorageReport, String>>,
) -> Result<HostStorageReport, String> {
    let mut matched = None;
    if disk_id == [0; 16] {
        return Err("invalid zero disk identity".into());
    }
    for result in reports {
        match result {
            Ok(report) if report.virtual_disk_id == Some(disk_id) => {
                if matched.is_some() {
                    return Err(format!(
                        "ambiguous host storage match: {}",
                        format_disk_id_hex(&disk_id)
                    ));
                }
                matched = Some(report);
            }
            Ok(report) if report.virtual_disk_id.is_some_and(|id| id != [0; 16]) => {}
            Ok(_) => return Err("host storage candidate has no valid disk identity".into()),
            Err(error) => {
                return Err(format!(
                    "host storage candidate could not be inspected: {error}"
                ));
            }
        }
    }
    matched.ok_or_else(|| {
        format!(
            "no registered storage match: {}",
            format_disk_id_hex(&disk_id)
        )
    })
}

#[cfg(any(windows, test))]
fn inspect_stable_registration(
    disk_id: [u8; 16],
    mut observe: impl FnMut() -> Result<Vec<discovery::RegistrationObservation>, String>,
    mut inspect: impl FnMut(&std::path::Path) -> Result<HostStorageReport, String>,
) -> Result<HostStorageReport, String> {
    let mut before = observe()?;
    before.sort_unstable();
    let report =
        select_inspected_storage(disk_id, before.iter().map(|(_, _, path, _)| inspect(path)))?;
    let mut after = observe()?;
    after.sort_unstable();
    if before != after {
        return Err("WSL registrations changed during disk inspection".into());
    }
    Ok(report)
}

#[cfg(windows)]
pub fn inspect_by_disk_id(disk_id: [u8; 16]) -> Result<HostStorageReport, String> {
    inspect_stable_registration(disk_id, discovery::stable_loaded_registrations, |path| {
        inspect_file(path).map_err(|error| format!("{}: {error}", path.display()))
    })
}

#[cfg(not(windows))]
pub fn inspect_by_disk_id(_disk_id: [u8; 16]) -> Result<HostStorageReport, String> {
    Err("host storage inspection requires Windows".into())
}

pub fn inspect_disk_command(disk_id: [u8; 16]) -> i32 {
    match inspect_by_disk_id(disk_id).and_then(|report| {
        serde_json::to_string_pretty(&report)
            .map_err(|error| format!("serialize host storage report: {error}"))
    }) {
        Ok(json) => {
            println!("{json}");
            0
        }
        Err(error) => {
            eprintln!("ramshared-winsvc inspect-storage: {error}");
            1
        }
    }
}

#[cfg(test)]
#[path = "host_storage/tests.rs"]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
