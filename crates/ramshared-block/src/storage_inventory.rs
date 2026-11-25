// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/storage_inventory.rs
//! Linux storage inventory and shared eligibility, separate from caller authority.
//! Records also carry optional WSL host-backing observations; they are not grants.
use serde::Serialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;

mod parse;
pub use parse::{parse_lsblk_json, parse_mountinfo};

pub const LINUX_INVENTORY_OUTPUT_LIMIT: usize = 1024 * 1024;
pub const LSBLK_ARGS: &[&str] = &[
    "--json",
    "--bytes",
    "--output",
    "name,path,type,size,fstype,uuid,partuuid,mountpoints,pkname,maj:min,wwn,serial,ro,rm,rota,tran,model",
];
pub const SUPPORTED_SWAP_FILESYSTEMS: &[&str] = &["ext4", "xfs"];
pub const QUALIFIED_SWAP_TRANSPORTS: &[&str] = &[
    "ata", "ide", "mmc", "nvme", "pci", "sas", "sata", "scsi", "virtio",
];
pub const REFUSED_SWAP_TRANSPORTS: &[&str] = &[
    "aoe", "drbd", "fc", "fcoe", "iscsi", "nbd", "network", "nvme-of", "nvmeof", "nvmf", "rbd",
];

/// Read this process's kernel mount table within the shared parser budget.
pub fn read_mountinfo() -> Result<Vec<MountInfo>, String> {
    let file = File::open(format!("/proc/{}/mountinfo", std::process::id()))
        .map_err(|e| format!("cannot read Linux mount inventory: {e}"))?;
    let mut bytes = Vec::new();
    file.take(LINUX_INVENTORY_OUTPUT_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read bounded Linux mount inventory: {e}"))?;
    let text =
        String::from_utf8(bytes).map_err(|e| format!("mount inventory is not UTF-8: {e}"))?;
    parse_mountinfo(&text)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BlockDevice {
    pub name: String,
    pub path: String,
    pub kind: String,
    pub size_bytes: Option<u64>,
    pub filesystem: Option<String>,
    pub uuid: Option<String>,
    pub mountpoints: Vec<String>,
    pub parent: Option<String>,
    pub major_minor: Option<String>,
    pub partition_uuid: Option<String>,
    pub hardware_identity: Option<String>,
    pub parent_hardware_identity: Option<String>,
    pub mounts: Vec<MountInfo>,
    pub read_only: Option<bool>,
    pub removable: Option<bool>,
    pub rotational: Option<bool>,
    pub transport: Option<String>,
    pub model: Option<String>,
    pub eligible_for_file_storage: bool,
    pub eligibility_reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct HostBacking {
    pub drive_letter: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MountInfo {
    pub mount_id: u64,
    pub parent_mount_id: u64,
    pub major_minor: String,
    pub root: String,
    pub mountpoint: String,
    pub mount_options: Vec<String>,
    pub filesystem: String,
    pub source: String,
    pub super_options: Vec<String>,
    pub read_only: bool,
    pub total_bytes: Option<u64>,
    pub available_bytes: Option<u64>,
    pub capacity_observed_unix_ms: Option<u64>,
    pub host_backing: Option<HostBacking>,
}

/// Static backing policy is checked before any capacity syscall.
fn backing_eligibility(device: &BlockDevice) -> Result<(), String> {
    match device.read_only {
        Some(false) => {}
        Some(true) => return Err("block device is read-only".into()),
        None => return Err("block-device read-only state is unavailable".into()),
    }
    match device.removable {
        Some(false) => {}
        Some(true) => return Err("removable storage is not eligible".into()),
        None => return Err("removable status is unavailable".into()),
    }
    let transport = device
        .transport
        .as_deref()
        .ok_or("storage transport is unavailable; local backing cannot be verified")?;
    if transport.eq_ignore_ascii_case("usb") {
        return Err("USB storage is not eligible for managed files".into());
    }
    if REFUSED_SWAP_TRANSPORTS
        .iter()
        .any(|known| transport.eq_ignore_ascii_case(known))
    {
        return Err("network-backed storage is not eligible for managed files".into());
    }
    if !QUALIFIED_SWAP_TRANSPORTS
        .iter()
        .any(|known| transport.eq_ignore_ascii_case(known))
    {
        return Err(format!(
            "storage transport {transport} is not qualified as local"
        ));
    }
    if device.kind != "part" && device.kind != "disk" {
        return Err("select a filesystem on a partition or whole disk".into());
    }
    if device.uuid.as_deref().is_none_or(str::is_empty) {
        return Err("filesystem UUID is unavailable".into());
    }
    let identity = match device.kind.as_str() {
        "part" => device.parent_hardware_identity.as_deref(),
        "disk" => device.hardware_identity.as_deref(),
        _ => None,
    };
    if identity.is_none_or(str::is_empty) {
        return Err("stable storage-device identity is unavailable".into());
    }
    Ok(())
}

fn static_mount<'a>(
    device: &BlockDevice,
    mount: Option<&'a MountInfo>,
) -> Result<&'a MountInfo, String> {
    backing_eligibility(device)?;
    let mount = mount.ok_or("no current mount record matches this device")?;
    if device.major_minor.as_deref() != Some(mount.major_minor.as_str()) {
        return Err("mounted device identity does not match".into());
    }
    if mount.root != "/" {
        return Err("mount does not expose the filesystem root".into());
    }
    if mount.read_only {
        return Err("filesystem is mounted read-only".into());
    }
    if mount.mount_id == 0 {
        return Err("mount identity or free-space measurement is unavailable".into());
    }
    if !SUPPORTED_SWAP_FILESYSTEMS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(&mount.filesystem))
    {
        return Err(format!(
            "filesystem {} is not qualified",
            mount.filesystem.to_ascii_lowercase()
        ));
    }
    Ok(mount)
}

/// Eligibility is an observation, not a lifecycle or serving grant.
pub fn storage_eligibility(device: &BlockDevice, mount: Option<&MountInfo>) -> (bool, String) {
    let mount = match static_mount(device, mount) {
        Ok(mount) => mount,
        Err(reason) => return (false, reason),
    };
    match (mount.total_bytes, mount.available_bytes) {
        (Some(_), Some(_)) => (
            true,
            "verified ext4/XFS mount; recheck identity and free space before use".into(),
        ),
        _ => (
            false,
            "mount identity or free-space measurement is unavailable".into(),
        ),
    }
}

pub fn backing_device_identity(device: &BlockDevice) -> Option<&str> {
    device
        .parent_hardware_identity
        .as_deref()
        .or(device.hardware_identity.as_deref())
}

/// Match the durable volume tuple without using transient names as identity.
pub fn matches_volume(device: &BlockDevice, filesystem_uuid: &str, device_identity: &str) -> bool {
    device.uuid.as_deref() == Some(filesystem_uuid)
        && backing_device_identity(device) == Some(device_identity)
}

pub fn resolve_mount<'a>(
    devices: &'a [BlockDevice],
    filesystem_uuid: &str,
    device_identity: &str,
) -> Result<(&'a BlockDevice, &'a MountInfo), (&'static str, String)> {
    let mut matched_filesystem = false;
    let mut current_mounts = Vec::new();
    for device in devices {
        if !matches_volume(device, filesystem_uuid, device_identity) {
            continue;
        }
        matched_filesystem = true;
        for mount in &device.mounts {
            if device.major_minor.as_deref() == Some(mount.major_minor.as_str()) {
                current_mounts.push((device, mount));
            }
        }
    }

    if current_mounts.len() > 1 {
        return Err((
            "identity_ambiguous",
            "stable filesystem identity resolves to multiple current mounts".into(),
        ));
    }
    let Some((device, mount)) = current_mounts.first().copied() else {
        let reason = if matched_filesystem {
            "stable filesystem is not currently mounted on its reported device"
        } else {
            "stable filesystem and backing-device identity are not present in the inventory"
        };
        return Err(("identity_unavailable", reason.into()));
    };

    Ok((device, mount))
}

/// Static backing validation for an already allocated origin.
/// Existing data access does not depend on free-space telemetry or new allocation.
pub fn origin_mount_eligibility(device: &BlockDevice, mount: &MountInfo) -> Result<(), String> {
    static_mount(device, Some(mount)).map(|_| ())
}

fn mount_capacity_bytes(mountpoint: &str) -> Option<(u64, u64)> {
    let stats = rustix::fs::statvfs(mountpoint).ok()?;
    let available_blocks = stats.f_bavail;
    let total_blocks = stats.f_blocks;
    let fragment_size = if stats.f_frsize == 0 {
        stats.f_bsize
    } else {
        stats.f_frsize
    };
    let total_bytes = total_blocks.checked_mul(fragment_size)?;
    let available_bytes = available_blocks.checked_mul(fragment_size)?;
    (available_bytes <= total_bytes).then_some((total_bytes, available_bytes))
}

pub fn attach_mount_inventory(
    devices: &mut [BlockDevice],
    mounts: Vec<MountInfo>,
    observed_unix_ms: u64,
) {
    attach_with(devices, mounts, observed_unix_ms, mount_capacity_bytes);
}

fn device_index(devices: &[BlockDevice]) -> HashMap<String, Option<usize>> {
    let mut index = HashMap::new();
    for (position, device) in devices.iter().enumerate() {
        if let Some(number) = &device.major_minor {
            index
                .entry(number.clone())
                .and_modify(|entry| *entry = None)
                .or_insert(Some(position));
        }
    }
    index
}

fn sampled_mounts(
    devices: &[BlockDevice],
    index: &HashMap<String, Option<usize>>,
    mounts: Vec<MountInfo>,
    observed_unix_ms: u64,
    mut probe: impl FnMut(&str) -> Option<(u64, u64)>,
) -> HashMap<String, Vec<MountInfo>> {
    let mut grouped = HashMap::<String, Vec<MountInfo>>::new();
    for mut mount in mounts {
        mount.total_bytes = None;
        mount.available_bytes = None;
        mount.capacity_observed_unix_ms = None;
        let candidate = index
            .get(&mount.major_minor)
            .copied()
            .flatten()
            .map(|position| &devices[position]);
        if observed_unix_ms != 0
            && candidate.is_some_and(|device| static_mount(device, Some(&mount)).is_ok())
            && let Some((total, available)) = probe(&mount.mountpoint)
                .filter(|(total, available)| *total > 0 && available <= total)
        {
            mount.total_bytes = Some(total);
            mount.available_bytes = Some(available);
            mount.capacity_observed_unix_ms = Some(observed_unix_ms);
        }
        grouped
            .entry(mount.major_minor.clone())
            .or_default()
            .push(mount);
    }
    grouped
}

fn attach_with(
    devices: &mut [BlockDevice],
    mounts: Vec<MountInfo>,
    observed_unix_ms: u64,
    probe: impl FnMut(&str) -> Option<(u64, u64)>,
) {
    let index = device_index(devices);
    let mounts_by_device = sampled_mounts(devices, &index, mounts, observed_unix_ms, probe);
    for device in devices {
        device.eligible_for_file_storage = false;
        device.mounts.clear();
        if device
            .major_minor
            .as_ref()
            .is_some_and(|number| index.get(number).is_some_and(Option::is_none))
        {
            device.eligibility_reason = "current device identity is ambiguous".into();
            continue;
        }
        if let Some(mounts) = device
            .major_minor
            .as_ref()
            .and_then(|number| mounts_by_device.get(number))
        {
            device.mounts.clone_from(mounts);
        }
        if !device.mounts.is_empty() {
            device.mountpoints = device
                .mounts
                .iter()
                .map(|mount| mount.mountpoint.clone())
                .collect();
        }
        let eligibility = device
            .mounts
            .iter()
            .map(|mount| storage_eligibility(device, Some(mount)))
            .find(|result| result.0)
            .or_else(|| {
                device
                    .mounts
                    .first()
                    .map(|mount| storage_eligibility(device, Some(mount)))
            });
        if let Some((eligible, reason)) = eligibility {
            device.eligible_for_file_storage = eligible;
            device.eligibility_reason = reason;
        } else if device.mountpoints.is_empty() {
            device.eligibility_reason = "filesystem is not mounted".into();
        } else {
            device.eligibility_reason = "mount identity does not match a block device".into();
        }
    }
}

#[cfg(test)]
mod tests;
