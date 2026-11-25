// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/windows_host.rs
//! Windows host helpers: config open, elevation, pagefile, LUN identity (SPEC DT-1/DT-8/DT-11).
//!
//! Cover target: N/A — E2E-only for COM/WMI/VPD. Pure identity helpers tested below.

#![cfg(windows)]
#![allow(unsafe_code)]

mod native_ops;

pub use native_ops::observe_host_residue;

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};

use crate::config::{ConfigError, WinDriveConfig};
use crate::host_safety::merge_pagefile_sources;
use crate::service::{ObservedVolumeIdentity, parse_product_friendly_name};

use native_ops::{
    FSCTL_UNLOCK_VOLUME, active_pagefiles_native, configured_pagefiles_registry, emit_event_native,
    flush_and_dismount_handle, fsctl, is_elevated_native, lock_volume_device, pagefile_identity,
    read_owned_config_native, sha256_hex,
};

/// Host-side errors (no kernel addresses).
#[derive(Debug)]
pub enum HostError {
    NotElevated,
    Config(ConfigError),
    Io(String),
    Pagefile(String),
    Volume(String),
    Identity(String),
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostError::NotElevated => write!(f, "elevated token required"),
            HostError::Config(e) => write!(f, "config: {e}"),
            HostError::Io(s) => write!(f, "io: {s}"),
            HostError::Pagefile(s) => write!(f, "pagefile: {s}"),
            HostError::Volume(s) => write!(f, "volume: {s}"),
            HostError::Identity(s) => write!(f, "identity: {s}"),
        }
    }
}

impl std::error::Error for HostError {}

/// Pagefile identity row from WMI / canonical volume query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PagefileIdentity {
    pub name: String,
    pub volume: String,
}

/// LUN identity conjunction (DT-11).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LunIdentity {
    pub vendor: String,
    pub product: String,
    pub serial: String,
    pub size_bytes: u64,
    pub disk_number: u32,
}

#[derive(serde::Deserialize)]
struct DiskInfo {
    #[serde(rename = "Number")]
    number: u32,
    #[serde(rename = "FriendlyName")]
    friendly_name: String,
    #[serde(rename = "Size")]
    size: u64,
    #[serde(rename = "SerialNumber")]
    serial_number: String,
}

impl LunIdentity {
    pub const VENDOR: &'static str = "RAMSHARE";
    pub const PRODUCT: &'static str = "VRAMDISK";

    /// Exact identity match (vendor, product, serial, size).
    pub fn matches_expected(&self, serial: &str, size_bytes: u64) -> bool {
        self.vendor == Self::VENDOR
            && self.product == Self::PRODUCT
            && self.serial.eq_ignore_ascii_case(serial)
            && self.size_bytes == size_bytes
    }
}

fn parse_find_lun_output(
    text: &str,
    expected_serial: &str,
    expected_size_bytes: u64,
) -> Result<Option<LunIdentity>, HostError> {
    let disks = serde_json::from_str::<Vec<DiskInfo>>(text)
        .map_err(|error| HostError::Identity(format!("malformed Get-Disk output: {error}")))?;
    if disks.is_empty() {
        return Ok(None);
    }
    if disks.len() != 1 {
        return Err(HostError::Identity(format!(
            "ambiguous exact LUN identity count={}",
            disks.len()
        )));
    }

    let info = disks.into_iter().next().ok_or_else(|| {
        HostError::Identity("exact LUN identity disappeared during parsing".into())
    })?;
    let (vendor, product) =
        parse_product_friendly_name(info.friendly_name.trim()).map_err(HostError::Identity)?;
    let lun = LunIdentity {
        vendor,
        product,
        serial: info.serial_number.trim().to_string(),
        size_bytes: info.size,
        disk_number: info.number,
    };
    if !lun.matches_expected(expected_serial, expected_size_bytes) {
        return Err(HostError::Identity(
            "Get-Disk returned a contradictory exact LUN identity".into(),
        ));
    }
    Ok(Some(lun))
}

/// Exclusive volume lock handle.
#[derive(Debug)]
pub struct LockedVolume {
    pub letter: char,
    pub disk_number: u32,
    handle: HANDLE,
}

// HANDLE is an OS resource token; exclusive ownership may move across threads
// (e.g. bounded lock helper) as long as only one owner uses the handle.
unsafe impl Send for LockedVolume {}

impl Drop for LockedVolume {
    fn drop(&mut self) {
        if self.handle != INVALID_HANDLE_VALUE && !self.handle.is_null() {
            // Best-effort unlock + close.
            let _ = fsctl(self.handle, FSCTL_UNLOCK_VOLUME);
            unsafe {
                CloseHandle(self.handle);
            }
            self.handle = INVALID_HANDLE_VALUE;
        }
    }
}

/// Aggregate host queries used by product runtime.
pub struct WindowsHostState;

impl WindowsHostState {
    pub fn is_elevated() -> bool {
        is_elevated_native()
    }

    /// Read config once from absolute path; reject relative and reparse attributes (DT-1).
    pub fn read_owned_config(path: &Path) -> Result<WinDriveConfig, HostError> {
        read_owned_config_native(path)
    }

    /// Gate A/B pagefile query. Fail-closed on any error (DT-8).
    ///
    /// Require both configured and actually allocated pagefile sources (DT-8).
    /// Either source failing is unsafe. The active set comes from
    /// `EnumPageFilesW` in-process: no WMI provider, child process or timeout
    /// budget is involved, so a provider hang cannot consume teardown.
    pub fn active_pagefiles() -> Result<Vec<PagefileIdentity>, HostError> {
        let configured = configured_pagefiles_registry()
            .map(|rows| rows.into_iter().map(|row| row.name).collect())
            .map_err(|e| e.to_string());
        let active = active_pagefiles_native().map_err(|e| e.to_string());
        merge_pagefile_sources(configured, active)
            .map(|paths| paths.into_iter().map(pagefile_identity).collect())
            .map_err(HostError::Pagefile)
    }

    pub fn lock_volume(letter: char) -> Result<LockedVolume, HostError> {
        Self::lock_product_volume(letter, None)
    }

    pub fn lock_product_volume(
        letter: char,
        expected_disk_number: Option<u32>,
    ) -> Result<LockedVolume, HostError> {
        let letter = letter.to_ascii_uppercase();
        if !('D'..='Z').contains(&letter) {
            return Err(HostError::Volume("letter must be D..=Z".into()));
        }
        // \\.\D: volume path
        let path = format!("\\\\.\\{letter}:");
        Self::lock_product_volume_path(&path, letter, expected_disk_number)
    }

    pub fn lock_product_volume_path(
        path: &str,
        letter: char,
        expected_disk_number: Option<u32>,
    ) -> Result<LockedVolume, HostError> {
        let letter = letter.to_ascii_uppercase();
        let drive_path = format!("\\\\.\\{letter}:");
        let volume_guid = path.starts_with(r"\\?\Volume{") && path.ends_with('}');
        if path != drive_path && !volume_guid {
            return Err(HostError::Volume("invalid volume device path".into()));
        }
        let (disk_number, handle) = lock_volume_device(path, expected_disk_number)?;
        Ok(LockedVolume {
            letter,
            disk_number,
            handle,
        })
    }

    pub fn flush_and_dismount(vol: &LockedVolume) -> Result<(), HostError> {
        flush_and_dismount_handle(vol.handle)
    }

    /// Observe product volume by exact serial+size (unique VPD identity).
    ///
    /// Letter is taken from config (Day-0 controlled), not from `Get-Partition`,
    /// which hangs under GPU-PV volume churn and burned the graceful-stop budget.
    pub fn observe_product_volume(
        letter: char,
        mount_path: Option<&Path>,
        serial: &str,
        size_bytes: u64,
    ) -> Result<(ObservedVolumeIdentity, u32, Option<String>), HostError> {
        let letter = letter.to_ascii_uppercase();
        if !('D'..='Z').contains(&letter) {
            return Err(HostError::Volume("letter must be D..=Z".into()));
        }
        if serial.len() != 16 || !serial.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(HostError::Identity("serial must be 16 hex chars".into()));
        }
        let mut env_values = vec![
            ("RAMSHARED_SERIAL", serial.to_string()),
            ("RAMSHARED_LETTER", letter.to_string()),
            ("RAMSHARED_SIZE", size_bytes.to_string()),
        ];
        let partition_binding = if let Some(path) = mount_path {
            env_values.push(("RAMSHARED_MOUNT", path.to_string_lossy().replace('/', "\\")));
            concat!(
                "$wantMount=$env:RAMSHARED_MOUNT; ",
                "$p=@(Get-Partition -DiskNumber $d[0].Number -ErrorAction SilentlyContinue | ",
                "Where-Object { $ap=@($_.AccessPaths | Where-Object { ",
                "([string]$_).TrimEnd('\\') -ieq $wantMount.TrimEnd('\\') }); $ap.Count -eq 1 }); ",
                "if(@($p).Length -ne 1){ Write-Error ('mount_binding_count='+@($p).Length); exit 43 }; "
            )
            .to_string()
        } else {
            concat!(
                "$p=@(Get-Partition -DiskNumber $d[0].Number -ErrorAction SilentlyContinue | ",
                "Where-Object { ([string]$_.DriveLetter) -ieq $wantLetter }); ",
                "if(@($p).Length -ne 1){ Write-Error ('letter_binding_count='+@($p).Length); exit 43 }; "
            )
            .to_string()
        };
        // Bind the configured access surface to the exact disk. Serial+size
        // without the partition relation could select the product disk while a
        // foreign volume occupied the configured path.
        let script = format!(
            concat!(
                "$ErrorActionPreference='Stop'; ",
                "$wantSerial=$env:RAMSHARED_SERIAL; $wantLetter=$env:RAMSHARED_LETTER; $wantSize=[uint64]$env:RAMSHARED_SIZE; ",
                "$d=@(Get-Disk -ErrorAction SilentlyContinue | Where-Object {{ ",
                "  ((([string]$_.SerialNumber).Trim()) -ieq $wantSerial) -and ",
                "  ([uint64]$_.Size -eq $wantSize) -and ",
                "  ($_.FriendlyName -match 'RAMSHARE') ",
                "}}); ",
                "if(@($d).Length -ne 1){{ Write-Error ('disk_count='+@($d).Length); exit 42 }}; ",
                "$raw=([string]$d[0].PartitionStyle -eq 'RAW' -and [uint32]$d[0].NumberOfPartitions -eq 0); ",
                "if(-not $raw){{ {partition_binding} }} else {{ ",
                "$foreign=@(Get-Partition -ErrorAction SilentlyContinue | Where-Object {{ ",
                "([string]$_.DriveLetter) -ieq $wantLetter }}); ",
                "if(@($foreign).Length -ne 0){{ Write-Error ('raw_letter_occupied='+$wantLetter); exit 43 }}; ",
                "$p=@() }}; ",
                "$v=if($raw){{ @() }}else{{ @($p[0] | Get-Volume -ErrorAction Stop) }}; ",
                "if($raw){{ $vp='RAW' }}else{{ ",
                "if(@($v).Length -ne 1){{ Write-Error ('volume_count='+@($v).Length); exit 44 }}; ",
                "$vp=([string]$v[0].Path).TrimEnd('\\'); ",
                "if($vp.Length -lt 13 -or -not $vp.StartsWith('\\\\?\\Volume{{') -or -not $vp.EndsWith('}}')){{ Write-Error 'volume_path_invalid'; exit 45 }}; ",
                "$parsedGuid=[guid]::Empty; ",
                "if(-not [guid]::TryParse($vp.Substring(11,$vp.Length-12),[ref]$parsedGuid)){{ Write-Error 'volume_guid_invalid'; exit 45 }}; ",
                "}}; ",
                "$n=($d[0].FriendlyName -replace '\\s+',' ').Trim(); ",
                "Write-Output ([string]$d[0].Number+'|'+$n+'|'+([string]$d[0].SerialNumber).Trim()+'|'+[string]$d[0].Size+'|'+$vp)"
            ),
            partition_binding = partition_binding,
        );
        let env_refs = env_values
            .iter()
            .map(|(key, value)| (*key, value.as_str()))
            .collect::<Vec<_>>();
        let output = run_powershell_bounded(&script, Duration::from_secs(12), &env_refs)
            .map_err(HostError::Identity)?;
        if !output.status.success() {
            return Err(HostError::Identity(format!(
                "product volume query failed status={:?} stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
                    .chars()
                    .take(200)
                    .collect::<String>()
            )));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut parts = stdout.trim().split('|');
        let disk_number = parts
            .next()
            .and_then(|value| value.parse::<u32>().ok())
            .ok_or_else(|| HostError::Identity("missing disk number".into()))?;
        let name = parts.next().unwrap_or_default();
        let observed_serial = parts.next().unwrap_or_default().trim().to_string();
        let observed_size = parts
            .next()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| HostError::Identity("missing disk size".into()))?;
        let volume_path_token = parts.next().unwrap_or_default();
        let volume_path = if volume_path_token == "RAW" {
            None
        } else {
            Some(volume_path_token.to_string())
        };
        if parts.next().is_some() {
            return Err(HostError::Identity(
                "ambiguous product identity output".into(),
            ));
        }
        let (vendor, product) = parse_product_friendly_name(name).map_err(HostError::Identity)?;
        if observed_size != size_bytes {
            return Err(HostError::Identity(format!(
                "capacity mismatch expected={size_bytes} observed={observed_size}"
            )));
        }
        Ok((
            ObservedVolumeIdentity {
                letter,
                vendor,
                product,
                serial: observed_serial,
                size_bytes: observed_size,
            },
            disk_number,
            volume_path,
        ))
    }

    pub fn find_lun(
        serial: &str,
        size_bytes: u64,
        provider_timeout: Duration,
    ) -> Result<Option<LunIdentity>, HostError> {
        // Storage module via PowerShell (VPD serial when exposed). The query
        // always serializes an array so zero is distinguishable from malformed
        // output and ambiguity is never silently truncated.
        if serial.len() != 16 || !serial.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(HostError::Identity("serial must be 16 hex chars".into()));
        }
        let script = concat!(
            "$wantSerial=$env:RAMSHARED_SERIAL; $wantSize=[uint64]$env:RAMSHARED_SIZE; ",
            "$items = @(Get-Disk | Where-Object { $_.Size -eq $wantSize -and ",
            "(([string]$_.FriendlyName -replace '\\s+', ' ').Trim()) -eq 'RAMSHARE VRAMDISK' -and ",
            "([string]$_.SerialNumber).Trim() -ieq $wantSerial } | ",
            "Select-Object Number,FriendlyName,Size,SerialNumber); ",
            "ConvertTo-Json -InputObject $items -Compress"
        );
        let size_value = size_bytes.to_string();
        let output = run_powershell_bounded(
            script,
            provider_timeout,
            &[
                ("RAMSHARED_SERIAL", serial),
                ("RAMSHARED_SIZE", size_value.as_str()),
            ],
        )
        .map_err(HostError::Identity)?;
        if !output.status.success() {
            return Err(HostError::Identity("Get-Disk failed".into()));
        }
        let text = String::from_utf8_lossy(&output.stdout);
        parse_find_lun_output(text.trim(), serial, size_bytes)
    }

    pub fn binary_sha256(path: &Path) -> Result<String, HostError> {
        let mut f = File::open(path).map_err(|e| HostError::Io(e.to_string()))?;
        let mut data = Vec::new();
        f.read_to_end(&mut data)
            .map_err(|e| HostError::Io(e.to_string()))?;
        sha256_hex(&data).map_err(HostError::Io)
    }

    pub fn emit_event(summary: &str) -> Result<(), HostError> {
        emit_event_native(summary)
    }
}

fn run_powershell_bounded(
    script: &str,
    timeout: Duration,
    environment: &[(&str, &str)],
) -> Result<Output, String> {
    // Channel + helper thread so a hung Get-Partition/WMI cannot block teardown
    // forever even if kill/wait races on Windows.
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
    let wrapped_script = format!("$ProgressPreference='SilentlyContinue'; {script}");
    let encoded_script = encode_powershell_command(&wrapped_script);
    let environment = validate_powershell_environment(environment)?;
    let (tx, rx) = std::sync::mpsc::channel();
    let done = std::sync::Arc::new(AtomicBool::new(false));
    let done_w = std::sync::Arc::clone(&done);
    let worker = std::thread::spawn(move || {
        let mut command = Command::new("powershell.exe");
        command.args([
            "-NoProfile",
            "-NonInteractive",
            "-OutputFormat",
            "Text",
            "-EncodedCommand",
            &encoded_script,
        ]);
        for (key, value) in &environment {
            command.env(key, value);
        }
        let child = match command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                done_w.store(true, AtomicOrdering::Release);
                let _ = tx.send(Err(format!("spawn: {e}")));
                return;
            }
        };
        let pid = child.id();
        let done_k = std::sync::Arc::clone(&done_w);
        // Watchdog: force-kill tree only if still running after timeout.
        std::thread::spawn(move || {
            let slice = Duration::from_millis(50);
            let mut waited = Duration::ZERO;
            while waited < timeout {
                if done_k.load(AtomicOrdering::Acquire) {
                    return;
                }
                std::thread::sleep(slice);
                waited += slice;
            }
            if !done_k.load(AtomicOrdering::Acquire) {
                let _ = Command::new("taskkill")
                    .args(["/PID", &pid.to_string(), "/T", "/F"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        });
        let out = child.wait_with_output().map_err(|e| format!("output: {e}"));
        done_w.store(true, AtomicOrdering::Release);
        let _ = tx.send(out);
    });
    let result = match rx.recv_timeout(timeout + Duration::from_millis(750)) {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(e)) => Err(e),
        Err(_) => {
            done.store(true, AtomicOrdering::Release);
            Err(format!(
                "PowerShell timeout after {} ms",
                timeout.as_millis()
            ))
        }
    };
    // Join only on success path; on timeout the worker may still be reaping.
    if result.is_ok() {
        let _ = worker.join();
    }
    result
}

fn validate_powershell_environment(
    environment: &[(&str, &str)],
) -> Result<Vec<(String, String)>, String> {
    const ALLOWED: [&str; 4] = [
        "RAMSHARED_LETTER",
        "RAMSHARED_MOUNT",
        "RAMSHARED_SERIAL",
        "RAMSHARED_SIZE",
    ];
    let mut validated = Vec::with_capacity(environment.len());
    for (key, value) in environment {
        if !ALLOWED.contains(key) {
            return Err(format!("PowerShell environment key is not allowed: {key}"));
        }
        if value.contains('\0') {
            return Err(format!("PowerShell environment value contains NUL: {key}"));
        }
        if validated.iter().any(|(seen, _)| seen == key) {
            return Err(format!("duplicate PowerShell environment key: {key}"));
        }
        validated.push(((*key).to_string(), (*value).to_string()));
    }
    Ok(validated)
}

pub fn encode_powershell_command(script: &str) -> String {
    use base64::Engine as _;

    let utf16le = script
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    base64::engine::general_purpose::STANDARD.encode(utf16le)
}

/// Reject relative and empty config paths.
pub fn validate_absolute_config_path(path: &Path) -> Result<(), ConfigError> {
    if path.as_os_str().is_empty() {
        return Err(ConfigError::Invalid {
            field: "config",
            detail: "empty path".into(),
        });
    }
    if !path.is_absolute() {
        return Err(ConfigError::Invalid {
            field: "config",
            detail: "relative config path rejected".into(),
        });
    }
    Ok(())
}

/// SCM fixed product config path (DT-1).
pub fn scm_config_path() -> PathBuf {
    PathBuf::from(r"C:\ProgramData\RamShared\winsvc.toml")
}

#[cfg(test)]
#[path = "windows_host/tests.rs"]
mod suite;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn find_lun_zero_is_retryable() {
        assert_eq!(
            parse_find_lun_output("[]", "ABCDEF0123456789", 64 * 1024 * 1024).unwrap(),
            None
        );
    }

    #[test]
    fn find_lun_exact_singleton_succeeds() {
        let json = r#"[{"Number":7,"FriendlyName":"RAMSHARE VRAMDISK","Size":67108864,"SerialNumber":"ABCDEF0123456789"}]"#;
        let observed = parse_find_lun_output(json, "ABCDEF0123456789", 64 * 1024 * 1024)
            .unwrap()
            .unwrap();
        assert_eq!(observed.disk_number, 7);
        assert!(observed.matches_expected("ABCDEF0123456789", 64 * 1024 * 1024));
    }

    #[test]
    fn find_lun_ambiguity_is_refused() {
        let row = r#"{"Number":7,"FriendlyName":"RAMSHARE VRAMDISK","Size":67108864,"SerialNumber":"ABCDEF0123456789"}"#;
        let error = parse_find_lun_output(
            &format!("[{row},{row}]"),
            "ABCDEF0123456789",
            64 * 1024 * 1024,
        )
        .unwrap_err();
        assert!(error.to_string().contains("ambiguous exact LUN identity"));
    }

    #[test]
    fn find_lun_malformed_output_is_refused() {
        let error = parse_find_lun_output(r#"{"Number":7}"#, "ABCDEF0123456789", 64 * 1024 * 1024)
            .unwrap_err();
        assert!(error.to_string().contains("malformed Get-Disk output"));
    }
}
