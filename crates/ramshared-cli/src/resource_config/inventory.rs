// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/inventory.rs
//! Resource discovery for host memory, swap tables, Linux block devices, and Windows volumes.

use super::{
    DISCOVERY_TIMEOUT, HostMemorySnapshot, InventoryError, LINUX_INVENTORY_OUTPUT_LIMIT,
    LSBLK_ARGS, MemorySnapshot, ResourceSnapshot, RuntimePlatform,
    STORAGE_SAMPLE_FUTURE_TOLERANCE_MS, STORAGE_SAMPLE_MAX_AGE_MS, SwapDevice,
    WINDOWS_INVENTORY_OUTPUT_LIMIT, WindowsSnapshot, WindowsVolume, WslInventory,
    default_configuration_root, format_gib, gpu_inventory, prepared_origin,
};
use ramshared_block::storage_inventory::{BlockDevice, HostBacking, MountInfo};
use serde_json::Value;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) fn inventory_platform(
    platform: crate::readiness::probes::Observation<crate::readiness::probes::PlatformKind>,
) -> Result<RuntimePlatform, InventoryError> {
    use crate::readiness::probes::{Observation, PlatformKind};
    match platform {
        Observation::Measured(PlatformKind::NativeLinux) => Ok(RuntimePlatform::NativeLinux),
        Observation::Measured(PlatformKind::Wsl2) => Ok(RuntimePlatform::Wsl2),
        _ => Err(InventoryError(
            "storage inventory requires measured native Linux or WSL2 context".into(),
        )),
    }
}

pub(super) fn meminfo_kib(text: &str, key: &str) -> Option<u64> {
    let prefix = format!("{key}:");
    let line = text.lines().find(|line| line.starts_with(&prefix))?;
    let mut fields = line[prefix.len()..].split_whitespace();
    let value = fields.next()?.parse::<u64>().ok()?;
    if fields.next()? != "kB" || fields.next().is_some() {
        return None;
    }
    value.checked_mul(1024)
}

pub(super) fn parse_meminfo(text: &str) -> MemorySnapshot {
    let total = meminfo_kib(text, "MemTotal");
    let available = meminfo_kib(text, "MemAvailable");
    let (total_bytes, available_bytes) = match (total, available) {
        (Some(total), Some(available)) if total > 0 && available <= total => {
            (Some(total), Some(available))
        }
        _ => (None, None),
    };
    let swap_total = meminfo_kib(text, "SwapTotal");
    let swap_free = meminfo_kib(text, "SwapFree");
    let (swap_total_bytes, swap_free_bytes) = match (swap_total, swap_free) {
        (Some(total), Some(free)) if free <= total => (Some(total), Some(free)),
        _ => (None, None),
    };
    let required_counters_available = total_bytes.is_some()
        && available_bytes.is_some()
        && swap_total_bytes.is_some()
        && swap_free_bytes.is_some();

    MemorySnapshot {
        total_bytes,
        available_bytes,
        swap_total_bytes,
        swap_free_bytes,
        required_counters_available,
    }
}

pub(super) fn parse_swap_table(text: &str) -> Result<Vec<SwapDevice>, InventoryError> {
    let mut lines = text.lines();
    let header = lines
        .next()
        .ok_or_else(|| InventoryError("swap table is empty".into()))?;
    if header.split_whitespace().collect::<Vec<_>>()
        != ["Filename", "Type", "Size", "Used", "Priority"]
    {
        return Err(InventoryError("swap table header is malformed".into()));
    }

    let mut devices = Vec::new();
    for (index, line) in lines.enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 5 {
            return Err(InventoryError(format!(
                "swap table row {} is malformed",
                index + 2
            )));
        }
        let size_kib = fields[2].parse::<u64>().map_err(|_| {
            InventoryError(format!("swap table row {} has invalid size", index + 2))
        })?;
        let used_kib = fields[3]
            .parse::<u64>()
            .map_err(|_| InventoryError(format!("swap table row {} has invalid use", index + 2)))?;
        let priority = fields[4].parse::<i32>().map_err(|_| {
            InventoryError(format!("swap table row {} has invalid priority", index + 2))
        })?;
        if used_kib > size_kib {
            return Err(InventoryError(format!(
                "swap table row {} reports use above capacity",
                index + 2
            )));
        }
        devices.push(SwapDevice {
            filename: fields[0].to_string(),
            kind: fields[1].to_string(),
            size_kib,
            used_kib,
            priority,
        });
    }
    Ok(devices)
}

pub(super) fn json_string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

pub(super) fn json_u64(value: &Value, key: &str) -> Option<u64> {
    let value = value.get(key)?;
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}

pub(super) fn storage_eligibility(
    device: &BlockDevice,
    mount: Option<&MountInfo>,
) -> (bool, String) {
    ramshared_block::storage_inventory::storage_eligibility(device, mount)
}

pub(super) fn parse_lsblk_json(text: &str) -> Result<Vec<BlockDevice>, InventoryError> {
    ramshared_block::storage_inventory::parse_lsblk_json(text).map_err(InventoryError)
}

#[cfg(test)]
pub(super) fn parse_mountinfo(text: &str) -> Result<Vec<MountInfo>, InventoryError> {
    ramshared_block::storage_inventory::parse_mountinfo(text).map_err(InventoryError)
}

pub(super) fn attach_mount_inventory(devices: &mut [BlockDevice], mounts: Vec<MountInfo>) {
    ramshared_block::storage_inventory::attach_mount_inventory(devices, mounts, unix_millis());
}

pub(super) fn apply_platform_storage_policy(
    devices: &mut [BlockDevice],
    platform: RuntimePlatform,
) {
    if platform != RuntimePlatform::Wsl2 {
        return;
    }

    for device in devices {
        let has_mounted_supported_filesystem = device.mounts.iter().any(|mount| {
            matches!(
                mount.filesystem.to_ascii_lowercase().as_str(),
                "ext4" | "xfs"
            ) && mount.total_bytes.is_some()
                && mount.available_bytes.is_some()
        });
        if has_mounted_supported_filesystem {
            // The statvfs figures describe the guest filesystem, not the
            // Windows volume that backs the WSL2 virtual disk. Presenting them
            // as free host capacity is misleading, so withdraw the sample
            // instead of publishing a number the host cannot honor.
            for mount in &mut device.mounts {
                if matches!(
                    mount.filesystem.to_ascii_lowercase().as_str(),
                    "ext4" | "xfs"
                ) {
                    mount.total_bytes = None;
                    mount.available_bytes = None;
                    mount.capacity_observed_unix_ms = None;
                }
            }
            let guest_reason = device.eligibility_reason.trim().to_string();
            device.eligible_for_file_storage = false;
            device.eligibility_reason = if guest_reason.is_empty() {
                "WSL2 host-volume identity and free capacity are not bound to this guest filesystem"
                    .into()
            } else {
                format!(
                    "WSL2 host-volume identity and free capacity are not bound to this guest filesystem; guest check: {guest_reason}"
                )
            };
        }
    }
}

/// Bind the WSL virtual disk to its host volume so the guest's ext4 free
/// space is never presented as host capacity. The mount keeps its guest
/// numbers withdrawn; `host_backing` carries the authoritative volume values.
pub(super) fn bind_wsl_host_capacity(devices: &mut [BlockDevice], windows: &WindowsSnapshot) {
    let Some(wsl) = windows.wsl.as_ref() else {
        return;
    };
    let Some(drive) = wsl
        .drive_letter
        .as_deref()
        .filter(|drive| !drive.is_empty())
    else {
        return;
    };
    let Some(path) = wsl.vhdx_path.as_deref() else {
        return;
    };
    let drive_bytes = drive.as_bytes();
    let path_bytes = path.as_bytes();
    if drive_bytes.len() != 1
        || !drive_bytes[0].is_ascii_alphabetic()
        || path_bytes.len() < 3
        || !path_bytes[0].eq_ignore_ascii_case(&drive_bytes[0])
        || path_bytes[1] != b':'
        || path_bytes[2] != b'\\'
    {
        return;
    }
    let mut matching = windows.volumes.iter().filter(|volume| {
        volume
            .drive_letter
            .as_deref()
            .is_some_and(|letter| letter.eq_ignore_ascii_case(drive))
    });
    let Some(volume) = matching.next() else {
        return;
    };
    if matching.next().is_some() {
        return;
    }
    let (Some(total), Some(free)) = (volume.size_bytes, volume.free_bytes) else {
        return;
    };
    if total == 0 || free > total {
        return;
    }
    for device in devices {
        let mut bound = false;
        for mount in &mut device.mounts {
            if mount.mountpoint == "/"
                && !mount.read_only
                && matches!(
                    mount.filesystem.to_ascii_lowercase().as_str(),
                    "ext4" | "xfs"
                )
            {
                mount.host_backing = Some(HostBacking {
                    drive_letter: drive.to_string(),
                    total_bytes: total,
                    free_bytes: free,
                });
                bound = true;
            }
        }
        if bound {
            device.eligibility_reason = format!(
                "WSL2 guest filesystem; host-volume backing {drive}: free {} of {}; managed-file eligibility requires verified guest-local transport",
                format_gib(Some(free)),
                format_gib(Some(total))
            );
        }
    }
}

pub(super) fn parse_windows_volume(value: &Value) -> Result<WindowsVolume, InventoryError> {
    if !value.is_object() {
        return Err(InventoryError("Windows volume row is not an object".into()));
    }
    Ok(WindowsVolume {
        drive_letter: json_string(value, "drive_letter"),
        label: json_string(value, "label"),
        file_system: json_string(value, "file_system"),
        drive_type: json_string(value, "drive_type").unwrap_or_else(|| "unknown".into()),
        size_bytes: json_u64(value, "size_bytes"),
        free_bytes: json_u64(value, "free_bytes"),
        volume_id: json_string(value, "volume_id"),
    })
}

pub(super) fn parse_windows_snapshot(text: &str) -> Result<WindowsSnapshot, InventoryError> {
    let root: Value = serde_json::from_str(text)
        .map_err(|error| InventoryError(format!("Windows inventory JSON is invalid: {error}")))?;
    let host = root
        .get("host_memory")
        .and_then(Value::as_object)
        .ok_or_else(|| InventoryError("Windows inventory has no host_memory object".into()))?;
    let volume_rows = root
        .get("volumes")
        .and_then(Value::as_array)
        .ok_or_else(|| InventoryError("Windows inventory has no volumes array".into()))?;
    let host_memory_value = Value::Object(host.clone());
    let volumes = volume_rows
        .iter()
        .map(parse_windows_volume)
        .collect::<Result<Vec<_>, _>>()?;
    let wsl = root
        .get("wsl")
        .and_then(Value::as_object)
        .map(|wsl| WslInventory {
            distro: wsl.get("distro").and_then(Value::as_str).map(str::to_owned),
            vhdx_path: wsl
                .get("vhdx_path")
                .and_then(Value::as_str)
                .map(str::to_owned),
            drive_letter: wsl
                .get("drive_letter")
                .and_then(Value::as_str)
                .map(str::to_owned),
        });
    let observed_utc = json_string(&root, "observed_utc")
        .ok_or_else(|| InventoryError("Windows inventory has no observation time".into()))?;

    Ok(WindowsSnapshot {
        observed_utc,
        observed_unix_ms: json_u64(&root, "observed_unix_ms"),
        host_memory: HostMemorySnapshot {
            total_bytes: json_u64(&host_memory_value, "total_bytes"),
            free_bytes: json_u64(&host_memory_value, "free_bytes"),
            committed_bytes: json_u64(&host_memory_value, "committed_bytes"),
            commit_limit_bytes: json_u64(&host_memory_value, "commit_limit_bytes"),
        },
        volumes,
        wsl,
    })
}

pub(super) fn run_bounded_command(
    command: &mut Command,
    label: &str,
    output_limit: usize,
) -> Result<Vec<u8>, InventoryError> {
    let output = crate::bounded_process::run_capture_command(
        command,
        label,
        DISCOVERY_TIMEOUT,
        output_limit,
        |_| {},
    )
    .map_err(|error| InventoryError(error.to_string()))?;
    if !output.status.success() {
        return Err(InventoryError(format!("{label} exited unsuccessfully")));
    }
    Ok(output.stdout)
}

pub(super) fn collect_linux_block_devices() -> Result<Vec<BlockDevice>, InventoryError> {
    let mut command = Command::new("lsblk");
    command.args(LSBLK_ARGS);
    let output = run_bounded_command(
        &mut command,
        "lsblk inventory",
        LINUX_INVENTORY_OUTPUT_LIMIT,
    )?;
    let text = String::from_utf8(output)
        .map_err(|error| InventoryError(format!("lsblk output is not UTF-8: {error}")))?;
    let mut devices = parse_lsblk_json(&text)?;
    let mounts = ramshared_block::storage_inventory::read_mountinfo().map_err(InventoryError)?;
    attach_mount_inventory(&mut devices, mounts);
    Ok(devices)
}

pub(super) fn windows_inventory_script() -> &'static str {
    r#"
$ErrorActionPreference = 'Stop'
$os = Get-CimInstance Win32_OperatingSystem
$memory = Get-CimInstance Win32_PerfRawData_PerfOS_Memory
$distro = $env:RAMSHARED_WSL_DISTRO
$wsl = $null
$entry = $null
if ($distro) {
    foreach ($key in Get-ChildItem 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Lxss' -ErrorAction SilentlyContinue) {
        $props = Get-ItemProperty $key.PSPath -ErrorAction SilentlyContinue
        if ($null -ne $props -and $props.DistributionName -eq $distro) {
            $entry = $props
            break
        }
    }
}
if ($null -ne $entry -and $entry.BasePath) {
        $vhdx = Join-Path ([string]$entry.BasePath) 'ext4.vhdx'
        $drive = $null
        if (Test-Path -LiteralPath $vhdx) {
            $drive = [System.IO.Path]::GetPathRoot($vhdx).TrimEnd('\').TrimEnd(':')
        }
        $wsl = [pscustomobject]@{
            distro = [string]$distro
            vhdx_path = [string]$vhdx
            drive_letter = $drive
        }
}
$volumes = @(Get-Volume -ErrorAction Stop | ForEach-Object {
    [pscustomobject]@{
        drive_letter = if ($null -ne $_.DriveLetter) { [string]$_.DriveLetter } else { $null }
        label = [string]$_.FileSystemLabel
        file_system = [string]$_.FileSystem
        drive_type = [string]$_.DriveType
        size_bytes = if ($null -ne $_.Size) { [uint64]$_.Size } else { $null }
        free_bytes = if ($null -ne $_.SizeRemaining) { [uint64]$_.SizeRemaining } else { $null }
        volume_id = [string]$_.UniqueId
    }
})
$result = [pscustomobject]@{
    observed_utc = [DateTime]::UtcNow.ToString('o')
    observed_unix_ms = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    host_memory = [pscustomobject]@{
        total_bytes = [uint64]$os.TotalVisibleMemorySize * [uint64]1024
        free_bytes = [uint64]$os.FreePhysicalMemory * [uint64]1024
        committed_bytes = [uint64]$memory.CommittedBytes
        commit_limit_bytes = [uint64]$memory.CommitLimit
    }
    volumes = @($volumes)
    wsl = $wsl
}
ConvertTo-Json -InputObject $result -Depth 5 -Compress
"#
}

pub(super) fn collect_windows_snapshot() -> Result<WindowsSnapshot, InventoryError> {
    let script = windows_inventory_script();
    let ps_program = crate::resource_config::benchmark::windows_host::windows_tool_program(
        "powershell.exe",
        "/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe",
    );
    let mut command = Command::new(ps_program);
    if let Ok(distro) = std::env::var("WSL_DISTRO_NAME") {
        let wsl_env = std::env::var("WSLENV").unwrap_or_default();
        let wsl_env = if wsl_env.is_empty() {
            "RAMSHARED_WSL_DISTRO".to_string()
        } else {
            format!("{wsl_env}:RAMSHARED_WSL_DISTRO")
        };
        command.env("RAMSHARED_WSL_DISTRO", distro);
        command.env("WSLENV", wsl_env);
    }
    command.args(["-NoProfile", "-NonInteractive", "-Command", script]);
    let output = run_bounded_command(
        &mut command,
        "Windows host inventory",
        WINDOWS_INVENTORY_OUTPUT_LIMIT,
    )?;
    let text = String::from_utf8(output)
        .map_err(|error| InventoryError(format!("Windows inventory is not UTF-8: {error}")))?;
    parse_windows_snapshot(&text)
}

pub(crate) fn collect_snapshot() -> Result<ResourceSnapshot, InventoryError> {
    let platform = inventory_platform(crate::readiness::probes::probe_platform().platform)?;
    let meminfo = std::fs::read_to_string("/proc/meminfo")
        .map_err(|error| InventoryError(format!("cannot read guest memory counters: {error}")))?;
    let guest_memory = parse_meminfo(&meminfo);
    let guest_observed_unix_ms = unix_millis();
    let swaps_text = std::fs::read_to_string("/proc/swaps")
        .map_err(|error| InventoryError(format!("cannot read guest swap inventory: {error}")))?;
    let swaps = parse_swap_table(&swaps_text)?;
    let mut warnings = Vec::new();
    let mut block_devices = match collect_linux_block_devices() {
        Ok(devices) => devices,
        Err(error) => {
            warnings.push(format!("Linux block inventory unavailable: {error}"));
            Vec::new()
        }
    };
    let windows = if platform == RuntimePlatform::Wsl2 {
        match collect_windows_snapshot() {
            Ok(snapshot) => Some(snapshot),
            Err(error) => {
                warnings.push(format!("Windows host inventory unavailable: {error}"));
                None
            }
        }
    } else {
        None
    };
    apply_platform_storage_policy(&mut block_devices, platform);
    if let Some(windows) = &windows {
        bind_wsl_host_capacity(&mut block_devices, windows);
    }
    let gpu_observed_unix_ms = unix_millis();
    let gpu_inventory = match gpu_inventory::discover() {
        Ok(devices) => devices,
        Err(error) => {
            warnings.push(format!("GPU inventory unavailable: {error}"));
            Vec::new()
        }
    };
    let prepared_native_origin = prepared_origin::observe(platform, &default_configuration_root());
    let observed_unix_ms = unix_millis();

    Ok(ResourceSnapshot {
        platform,
        observed_unix_ms,
        guest_memory,
        guest_observed_unix_ms,
        swaps,
        block_devices,
        windows,
        gpu_inventory,
        gpu_observed_unix_ms,
        gpu_budget_status: "not sampled by storage inventory; no GPU context opened".into(),
        warnings,
        prepared_native_origin,
    })
}

pub(crate) fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

pub(super) fn sample_is_fresh(sample_unix_ms: Option<u64>, now_unix_ms: u64) -> bool {
    sample_unix_ms.is_some_and(|sample| {
        sample != 0
            && now_unix_ms != 0
            && sample <= now_unix_ms.saturating_add(STORAGE_SAMPLE_FUTURE_TOLERANCE_MS)
            && now_unix_ms.saturating_sub(sample) <= STORAGE_SAMPLE_MAX_AGE_MS
    })
}
