// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/native_origin.rs
//! Native existing-file admission; platform and fresh volume proof precede I/O.
use std::path::PathBuf;
use std::time::Duration;

use ramshared_block::FileOrigin;
use ramshared_block::native_origin::{NativeOriginVolume, OriginMount};
use ramshared_block::platform::{PlatformKind, probe_platform};
use ramshared_block::storage_inventory::{
    BlockDevice, LINUX_INVENTORY_OUTPUT_LIMIT, LSBLK_ARGS, matches_volume,
    origin_mount_eligibility, parse_lsblk_json, read_mountinfo, resolve_mount,
};
use ramshared_config::native_origin::{NativeOriginManifest, default_native_origin_path};
use ramshared_config::resource_profile::ResourceTarget;

use super::command_runtime::command_stdout_with_limit;
use super::manifest_file::read_bounded_manifest_file;

pub(super) struct NativeOriginRequest {
    pub(super) manifest: PathBuf,
    pub(super) expected_bytes: Option<u64>,
}

impl NativeOriginRequest {
    pub(super) fn new(path: &str, expected_bytes: Option<u64>) -> Result<Self, String> {
        let request = Self {
            manifest: PathBuf::from(path),
            expected_bytes,
        };
        request.validate()?;
        Ok(request)
    }

    fn validate(&self) -> Result<(), String> {
        if self.manifest != default_native_origin_path() || self.expected_bytes == Some(0) {
            return Err("native origin requires the canonical prepared manifest and a positive explicit size".into());
        }
        Ok(())
    }
}

pub(super) struct AdmittedOrigin {
    pub(super) file: FileOrigin,
    pub(super) logical_bytes: u64,
    pub(super) manifest: NativeOriginManifest,
    pub(super) cache_session: Option<std::sync::Arc<super::native_cache::NativeCacheSession>>,
}

fn native_surface(uid: u32, platform: Option<PlatformKind>) -> Result<(), String> {
    if uid != 0 || platform != Some(PlatformKind::NativeLinux) {
        return Err(
            "native origin serving requires root and a measured native Linux platform".into(),
        );
    }
    Ok(())
}

fn observe_volume(manifest: &NativeOriginManifest) -> Result<Vec<BlockDevice>, String> {
    // This uses the serving daemon's retained helper owner. A discovery failure
    // must never introduce another process owner with exit-on-uncertainty policy.
    let output = command_stdout_with_limit(
        "/usr/bin/lsblk",
        LSBLK_ARGS,
        Duration::from_secs(10),
        LINUX_INVENTORY_OUTPUT_LIMIT,
    )
    .ok_or("native block inventory is unavailable")?;
    volume_from_tables(manifest, &output, read_mountinfo()?)
}

fn volume_from_tables(
    manifest: &NativeOriginManifest,
    output: &str,
    mounts: Vec<ramshared_block::storage_inventory::MountInfo>,
) -> Result<Vec<BlockDevice>, String> {
    let mut devices = parse_lsblk_json(output)?;
    let (uuid, backing, _) = geometry(manifest)?;
    devices.retain(|device| matches_volume(device, uuid, backing));
    let [device] = devices.as_mut_slice() else {
        return Err("native volume identity is absent or ambiguous".into());
    };
    // Existing, fully allocated data access needs no new capacity allocation.
    // Do not query free space on unrelated mounts or make recovery depend on it.
    device.mounts = mounts
        .into_iter()
        .filter(|mount| device.major_minor.as_deref() == Some(mount.major_minor.as_str()))
        .collect();
    Ok(devices)
}

fn geometry(manifest: &NativeOriginManifest) -> Result<(&str, &str, u64), String> {
    let ResourceTarget::LinuxFileOrigin {
        filesystem_uuid,
        device_identity,
        allocated_bytes,
        ..
    } = &manifest.target
    else {
        return Err("a sealed existing native file origin is required".into());
    };
    Ok((filesystem_uuid, device_identity, *allocated_bytes))
}

fn admit(
    manifest: &NativeOriginManifest,
    devices: &[BlockDevice],
    expected_bytes: Option<u64>,
    uid: u32,
) -> Result<(NativeOriginVolume, std::fs::File, u64), String> {
    let (uuid, backing, logical_bytes) = geometry(manifest)?;
    if expected_bytes.is_some_and(|expected| expected != logical_bytes) {
        return Err("native logical size must equal the prepared origin; resizing existing data requires its migration owner".into());
    }
    let (device, mount) = resolve_mount(devices, uuid, backing).map_err(|(_, detail)| detail)?;
    origin_mount_eligibility(device, mount)?;
    let volume = NativeOriginVolume::open(
        &OriginMount {
            mountpoint: mount.mountpoint.clone(),
            mount_id: mount.mount_id,
            major_minor: mount.major_minor.clone(),
        },
        uid,
    )?;
    let file = volume.verify_for_io(&manifest.target)?;
    Ok((volume, file, logical_bytes))
}

pub(super) fn open(request: &NativeOriginRequest) -> Result<AdmittedOrigin, String> {
    native_surface(
        rustix::process::geteuid().as_raw(),
        probe_platform().platform,
    )?;
    request.validate()?;
    let mut admitted = open_with(
        request,
        0,
        || read_bounded_manifest_file(&request.manifest, true),
        observe_volume,
    )?;
    attach_cache_configuration(&mut admitted, super::native_cache::NativeCacheSession::open);
    Ok(admitted)
}
fn attach_cache_configuration(
    admitted: &mut AdmittedOrigin,
    open: impl FnOnce(
        &NativeOriginManifest,
    ) -> Result<std::sync::Arc<super::native_cache::NativeCacheSession>, String>,
) {
    match open(&admitted.manifest) {
        Ok(session) => admitted.cache_session = Some(session),
        Err(error) => {
            eprintln!(
                "[ramsharedd] native cache generation unavailable; verified origin retained: {error}"
            );
            admitted.cache_session =
                Some(super::native_cache::NativeCacheSession::unavailable(error));
        }
    }
}

fn open_with(
    request: &NativeOriginRequest,
    uid: u32,
    mut read_manifest: impl FnMut() -> Result<Vec<u8>, String>,
    mut observe: impl FnMut(&NativeOriginManifest) -> Result<Vec<BlockDevice>, String>,
) -> Result<AdmittedOrigin, String> {
    let bytes = read_manifest()?;
    let text = std::str::from_utf8(&bytes).map_err(|_| "native origin manifest is not UTF-8")?;
    let manifest = NativeOriginManifest::parse(text)?;
    let before = observe(&manifest)?;
    let (volume, file, logical_bytes) = admit(&manifest, &before, request.expected_bytes, uid)?;
    let after = observe(&manifest)?;
    if after != before || read_manifest()? != bytes {
        return Err("native manifest or volume changed across descriptor admission".into());
    }
    volume.verify(&manifest.target)?;
    Ok(AdmittedOrigin {
        file: FileOrigin::from_file(file),
        logical_bytes,
        manifest,
        cache_session: None,
    })
}

#[cfg(test)]
mod tests;
