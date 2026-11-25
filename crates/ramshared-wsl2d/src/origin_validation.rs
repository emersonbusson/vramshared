// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/origin_validation.rs
//! FD-bound origin block-device validation and sysfs/blkid identity checks.

use std::fs::File;
use std::io::Seek;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use ramshared_block::FileOrigin;
use ramshared_wsl2d::host_gate::SealedOrigin;

use crate::command_runtime::command_stdout_with_timeout;
use crate::critical_devices::{
    collect_critical_device_numbers, linux_device_number, validate_stable_critical_device_snapshot,
};
use crate::origin_identity::{
    OriginIdentityObservation, SealedOriginManifest, canonical_device_number, canonical_guid,
    read_sealed_origin_manifest, validate_origin_manifest_identity,
};

pub(crate) fn process_fd_path(file: &File) -> PathBuf {
    Path::new("/proc")
        .join(std::process::id().to_string())
        .join("fd")
        .join(file.as_raw_fd().to_string())
}

pub(crate) fn block_metadata_rdev(
    metadata: &std::fs::Metadata,
    message: &str,
) -> Result<u64, String> {
    if !metadata.file_type().is_block_device() {
        return Err(message.to_string());
    }
    Ok(metadata.rdev())
}

fn open_parent_block_device_from_dev_t(
    partition_dev_t: &str,
) -> Result<(File, PathBuf, String), String> {
    open_parent_block_device_from_dev_t_at(
        partition_dev_t,
        Path::new("/sys/dev/block"),
        Path::new("/dev"),
    )
}

pub(crate) fn open_parent_block_device_from_dev_t_at(
    partition_dev_t: &str,
    sys_block_root: &Path,
    dev_root: &Path,
) -> Result<(File, PathBuf, String), String> {
    let sysfs = std::fs::canonicalize(sys_block_root.join(partition_dev_t))
        .map_err(|error| format!("resolve origin partition through dev_t: {error}"))?;
    if !sysfs.join("partition").is_file() {
        return Err("origin must be a partition with a distinct parent disk".into());
    }
    let parent_sysfs = sysfs
        .parent()
        .ok_or_else(|| "origin partition has no parent sysfs identity".to_string())?;
    let parent_name = parent_sysfs
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| {
            !value.is_empty()
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
        .ok_or_else(|| "origin parent block-device name is invalid".to_string())?;
    let parent_sysfs_dev_t = std::fs::read_to_string(parent_sysfs.join("dev"))
        .map_err(|error| format!("read origin parent sysfs dev_t: {error}"))?;
    let expected_dev_t = canonical_device_number(parent_sysfs_dev_t.trim())
        .ok_or_else(|| "origin parent sysfs dev_t is invalid".to_string())?;
    let path = dev_root.join(parent_name);
    let named = std::fs::metadata(&path)
        .map_err(|error| format!("stat origin parent {}: {error}", path.display()))?;
    if !named.file_type().is_block_device() || linux_device_number(named.rdev()) != expected_dev_t {
        return Err("origin parent path does not match its sysfs dev_t".into());
    }
    let file = File::options()
        .read(true)
        .open(&path)
        .map_err(|error| format!("open exact origin parent {}: {error}", path.display()))?;
    let opened = file
        .metadata()
        .map_err(|error| format!("stat opened origin parent: {error}"))?;
    if !opened.file_type().is_block_device()
        || opened.rdev() != named.rdev()
        || linux_device_number(opened.rdev()) != expected_dev_t
    {
        return Err("origin parent identity changed while opening".into());
    }
    Ok((file, path, expected_dev_t))
}

pub(crate) fn block_identity_value(device: &Path, field: &str) -> Result<String, String> {
    block_identity_value_with(device, field, |args| {
        command_stdout_with_timeout("/usr/sbin/blkid", args, Duration::from_secs(5))
    })
}

pub(crate) fn block_identity_value_with(
    device: &Path,
    field: &str,
    query_stdout: impl FnOnce(&[&str]) -> Option<String>,
) -> Result<String, String> {
    let device = device
        .to_str()
        .ok_or_else(|| "block-device path is not UTF-8".to_string())?;
    // Bypass libblkid's global /run/blkid/blkid.tab cache so probing a single
    // sealed origin fd never scans or stalls on an unresponsive NBD block device.
    query_stdout(&["-c", "/dev/null", "-s", field, "-o", "value", device])
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("cannot read {field} from {device}"))
}

/// Host/device adapters injected into [`open_validated_origin_with`] so
/// production enforces real block-device nodes and sysfs/blkid state while unit
/// tests verify identity and race refusal logic deterministically.
pub(crate) trait OriginValidationHost {
    fn read_manifest(
        &mut self,
        manifest_path: &str,
    ) -> Result<SealedOriginManifest, Box<dyn std::error::Error>>;
    fn block_rdev(&mut self, metadata: &std::fs::Metadata, message: &str) -> Result<u64, String>;
    fn open_parent(&mut self, partition_dev_t: &str) -> Result<(File, PathBuf, String), String>;
    fn collect_critical(&mut self) -> Result<Vec<String>, String>;
    fn read_identity(&mut self, device: &Path, field: &str) -> Result<String, String>;
}

pub(crate) struct ProductionOriginValidationHost;

impl OriginValidationHost for ProductionOriginValidationHost {
    fn read_manifest(
        &mut self,
        manifest_path: &str,
    ) -> Result<SealedOriginManifest, Box<dyn std::error::Error>> {
        Ok(read_sealed_origin_manifest(manifest_path)?)
    }

    fn block_rdev(&mut self, metadata: &std::fs::Metadata, message: &str) -> Result<u64, String> {
        block_metadata_rdev(metadata, message)
    }

    fn open_parent(&mut self, partition_dev_t: &str) -> Result<(File, PathBuf, String), String> {
        open_parent_block_device_from_dev_t(partition_dev_t)
    }

    fn collect_critical(&mut self) -> Result<Vec<String>, String> {
        collect_critical_device_numbers()
    }

    fn read_identity(&mut self, device: &Path, field: &str) -> Result<String, String> {
        block_identity_value(device, field)
    }
}

pub(crate) fn open_validated_origin(
    manifest_path: &str,
    logical_size: u64,
) -> Result<(FileOrigin, SealedOrigin), Box<dyn std::error::Error>> {
    let mut host = ProductionOriginValidationHost;
    open_validated_origin_with(manifest_path, logical_size, &mut host)
}

pub(crate) fn open_validated_origin_with(
    manifest_path: &str,
    logical_size: u64,
    host: &mut dyn OriginValidationHost,
) -> Result<(FileOrigin, SealedOrigin), Box<dyn std::error::Error>> {
    let manifest = host.read_manifest(manifest_path)?;
    let resolved = std::fs::canonicalize(&manifest.origin_path)?;
    let metadata = std::fs::metadata(&resolved)?;
    let initial_rdev =
        host.block_rdev(&metadata, "origin PARTUUID must resolve to a block device")?;
    let mut file = File::options().read(true).write(true).open(&resolved)?;
    let opened = file.metadata()?;
    let opened_rdev = host
        .block_rdev(&opened, "origin device identity changed while opening")
        .ok();
    if opened_rdev != Some(initial_rdev) {
        return Err("origin device identity changed while opening".into());
    }
    let origin_size = file.seek(std::io::SeekFrom::End(0))?;
    let partition_dev_t = linux_device_number(opened_rdev.unwrap_or(initial_rdev));
    let (parent_file, parent_path, parent_dev_t) = host.open_parent(&partition_dev_t)?;
    let partition_fd_path = process_fd_path(&file);
    let parent_fd_path = process_fd_path(&parent_file);
    let critical_before = host.collect_critical()?;
    let observation = OriginIdentityObservation {
        partuuid: canonical_guid(&host.read_identity(&partition_fd_path, "PARTUUID")?)
            .ok_or("opened origin PARTUUID is invalid")?,
        ptuuid: canonical_guid(&host.read_identity(&parent_fd_path, "PTUUID")?)
            .ok_or("opened origin PTUUID is invalid")?,
        path_dev_t: linux_device_number(initial_rdev),
        fd_dev_t: partition_dev_t,
        parent_dev_t,
        swap_uuid: canonical_guid(&host.read_identity(&partition_fd_path, "UUID")?)
            .ok_or("opened origin swap UUID is invalid")?,
        swap_type: host.read_identity(&partition_fd_path, "TYPE")?,
        origin_size,
        critical_dev_ts: critical_before.clone(),
    };
    let resolved_after = std::fs::canonicalize(&manifest.origin_path)?;
    let metadata_after = std::fs::metadata(&resolved_after)?;
    let opened_after = file.metadata()?;
    let parent_named_after = std::fs::metadata(&parent_path)?;
    let parent_opened_after = parent_file.metadata()?;
    let meta_after_rdev = host.block_rdev(&metadata_after, "").ok();
    let open_after_rdev = host.block_rdev(&opened_after, "").ok();
    let parent_named_rdev = host.block_rdev(&parent_named_after, "").ok();
    let parent_open_rdev = host.block_rdev(&parent_opened_after, "").ok();
    if resolved_after != resolved
        || meta_after_rdev != open_after_rdev
        || open_after_rdev != Some(initial_rdev)
        || parent_named_rdev.is_none()
        || parent_named_rdev != parent_open_rdev
        || parent_open_rdev.map(linux_device_number).as_deref()
            != Some(observation.parent_dev_t.as_str())
    {
        return Err(
            "origin or parent path changed across fd-bound external identity probes".into(),
        );
    }
    let critical_after = host.collect_critical()?;
    validate_stable_critical_device_snapshot(&critical_before, &critical_after)?;
    validate_origin_manifest_identity(&manifest, &observation, logical_size)?;
    file.seek(std::io::SeekFrom::Start(0))?;
    Ok((
        FileOrigin::from_file(file),
        SealedOrigin {
            logical_capacity_mib: manifest.logical_capacity_mib,
            partuuid: manifest.partuuid.clone(),
            origin_vhdx: manifest.origin_path.clone(),
        },
    ))
}
