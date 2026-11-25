// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/effect_binding.rs
//! Effect binding and the lifecycle record around it.
//!
//! Extracted from `cascade_io.rs` as one band of the boundary plan in
//! `workspace module layout`. Binding is the step between
//! "we know which device this is" and "we may act on it": the device is
//! pinned to an opened descriptor and an exact `dev_t`, and the lifecycle
//! record that names what is about to happen is written, validated and
//! re-read before the effect lands.
//!
//! The record is not bookkeeping. A half-written record is refused on the
//! next attempt rather than trusted, and a record whose origin does not
//! match the current boot is refused before any device is touched.

#[allow(unused_imports)]
use super::*;

pub(super) struct EffectBoundDevice {
    identity: BoundDeviceIdentity,
    effect_path: String,
    #[cfg(not(test))]
    file: fs::File,
}

impl EffectBoundDevice {
    pub(super) fn path(&self) -> &str {
        &self.effect_path
    }

    pub(super) fn revalidate_named_identity(&self) -> Result<(), CascadeError> {
        let observed = observe_bound_device(&self.identity.path, self.identity.kind)?;
        if observed != self.identity {
            return Err(CascadeError::Precondition(format!(
                "managed device identity changed across an external effect: {}",
                self.identity.path
            )));
        }
        #[cfg(not(test))]
        {
            let metadata = self.file.metadata().map_err(|error| {
                CascadeError::Precondition(format!(
                    "stat pinned managed device after external effect: {error}"
                ))
            })?;
            validate_pinned_effect_metadata(&metadata, &self.identity.dev_t)?;
        }
        Ok(())
    }
}

pub(super) fn validate_pinned_effect_metadata(
    metadata: &fs::Metadata,
    expected_dev_t: &str,
) -> Result<(), CascadeError> {
    if !metadata.file_type().is_block_device()
        || linux_device_number(metadata.rdev()) != expected_dev_t
    {
        return Err(CascadeError::Precondition(
            "pinned managed-device dev_t changed across an external effect".into(),
        ));
    }
    Ok(())
}

pub(super) fn bind_device_for_effect(
    device: &BoundDeviceIdentity,
) -> Result<EffectBoundDevice, CascadeError> {
    bind_device_for_effect_with(device, revalidate_bound_device)
}

pub(super) fn bind_legacy_device_for_effect(
    device: &BoundDeviceIdentity,
) -> Result<EffectBoundDevice, CascadeError> {
    bind_device_for_effect_with(device, revalidate_legacy_bound_device)
}

fn bind_device_for_effect_with(
    device: &BoundDeviceIdentity,
    revalidate: fn(&BoundDeviceIdentity) -> Result<(), CascadeError>,
) -> Result<EffectBoundDevice, CascadeError> {
    #[cfg(test)]
    {
        revalidate(device)?;
        Ok(EffectBoundDevice {
            identity: device.clone(),
            effect_path: device.path.clone(),
        })
    }
    #[cfg(not(test))]
    {
        let (file, effect_path) =
            open_device_for_effect_with(device, revalidate, |path| fs::File::open(path))?;
        Ok(EffectBoundDevice {
            identity: device.clone(),
            effect_path,
            file,
        })
    }
}

pub(super) fn open_device_for_effect_with(
    device: &BoundDeviceIdentity,
    mut revalidate: impl FnMut(&BoundDeviceIdentity) -> Result<(), CascadeError>,
    open: impl FnOnce(&Path) -> std::io::Result<fs::File>,
) -> Result<(fs::File, String), CascadeError> {
    revalidate(device)?;
    let file = open(Path::new(&device.path)).map_err(|error| {
        CascadeError::Precondition(format!(
            "open exact managed device for effect binding: {error}"
        ))
    })?;
    let metadata = file.metadata().map_err(|error| {
        CascadeError::Precondition(format!("stat exact managed device effect binding: {error}"))
    })?;
    if !metadata.file_type().is_block_device()
        || linux_device_number(metadata.rdev()) != device.dev_t
    {
        return Err(CascadeError::Precondition(
            "opened managed-device fd does not match the sealed dev_t".into(),
        ));
    }
    revalidate(device)?;
    let effect_path = format!("/proc/{}/fd/{}", std::process::id(), file.as_raw_fd());
    let opened = fs::metadata(&effect_path).map_err(|error| {
        CascadeError::Precondition(format!("stat managed-device proc-fd effect path: {error}"))
    })?;
    if linux_device_number(opened.rdev()) != device.dev_t {
        return Err(CascadeError::Precondition(
            "managed-device proc-fd path does not preserve the sealed dev_t".into(),
        ));
    }
    Ok((file, effect_path))
}

pub(super) fn validate_lifecycle_binding(binding: &LifecycleBinding) -> Result<(), CascadeError> {
    if binding.schema_version != LIFECYCLE_BINDING_SCHEMA
        || !canonical_boot_id(&binding.boot_id)
        || !canonical_invocation_id(&binding.daemon_invocation_id)
        || binding.daemon_pid == 0
        || binding.daemon_start_ticks == 0
        || binding.daemon_instance_id
            != format!("{}-{}", binding.daemon_pid, binding.daemon_start_ticks)
        || binding.export_socket.path.is_empty()
        || binding.export_socket.inode == 0
        || !canonical_origin_uuid(&binding.origin.partuuid)
        || !canonical_origin_uuid(&binding.origin.ptuuid)
        || !canonical_origin_uuid(&binding.origin.expected_swap_uuid)
        || !canonical_device_number(&binding.origin.partition_dev_t)
        || !canonical_device_number(&binding.origin.parent_dev_t)
        || !canonical_sha256(&binding.origin.host_manifest_sha256)
        || !canonical_sha256(&binding.origin.configuration_sha256)
    {
        return Err(CascadeError::Precondition(
            "lifecycle binding has an invalid schema or identity".into(),
        ));
    }
    let nbd_count = binding
        .devices
        .iter()
        .filter(|device| device.kind == ManagedDeviceKind::Nbd)
        .count();
    let ublk_count = binding
        .devices
        .iter()
        .filter(|device| device.kind == ManagedDeviceKind::Ublk)
        .count();
    let zram_count = binding
        .devices
        .iter()
        .filter(|device| device.kind == ManagedDeviceKind::Zram)
        .count();
    let unique = binding
        .devices
        .iter()
        .map(|device| device.path.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    if nbd_count != 1
        || ublk_count != 0
        || zram_count > 1
        || unique.len() != binding.devices.len()
        || binding.devices.iter().any(|device| {
            device_kind_for_path(&device.path) != Some(device.kind)
                || device.dev_t != device.sysfs_dev_t
                || !canonical_device_number(&device.dev_t)
                || device.sysfs_path.is_empty()
        })
    {
        return Err(CascadeError::Precondition(
            "lifecycle binding device cardinality or identity is invalid".into(),
        ));
    }
    Ok(())
}

/// Staging path for one atomic replace of a runtime record.
///
/// The name is unique per **write**, not per process. A `.{stem}.{pid}.tmp`
/// name is the same name for every publisher in the process, so a second
/// publisher truncates — or, with the pre-clean these writers used to do,
/// deletes — the first one's staging file while the first one still holds it.
/// The first one's `rename` then hands the second one's bytes to the first
/// one's destination: two records exchanged, each published under the other's
/// name. The monotonic sequence keeps concurrent writes apart and
/// `create_new` on the staging file refuses any residue rather than silently
/// reusing it.
///
/// The daemon's cache-status publisher names its staging file the same way
/// (`cache_status_staging_path`); the two helpers are kept local to their
/// crate for the same boundary reason as the KiB publish faces.
pub(super) fn runtime_staging_path(parent: &Path, stem: &str) -> PathBuf {
    static NEXT_STAGING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let sequence = NEXT_STAGING
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .to_string();
    parent.join(format!(".{stem}.{}.{sequence}.tmp", std::process::id()))
}

pub(super) fn write_lifecycle_binding(
    paths: &RuntimePaths,
    binding: &LifecycleBinding,
) -> Result<(), CascadeError> {
    validate_lifecycle_binding(binding)?;
    let payload = serde_json::to_vec(binding)
        .map_err(|error| CascadeError::Io(format!("serialize lifecycle binding: {error}")))?;
    if payload.is_empty() || payload.len() as u64 > LIFECYCLE_BINDING_MAX_BYTES {
        return Err(CascadeError::Precondition(
            "lifecycle binding exceeds its bounded schema".into(),
        ));
    }
    let temporary = runtime_staging_path(&paths.runtime_dir, "lifecycle-binding");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|error| CascadeError::Io(format!("create lifecycle binding: {error}")))?;
    file.write_all(&payload)
        .and_then(|()| file.sync_all())
        .map_err(|error| CascadeError::Io(format!("persist lifecycle binding: {error}")))?;
    drop(file);
    fs::rename(&temporary, &paths.lifecycle_binding_file)
        .map_err(|error| CascadeError::Io(format!("publish lifecycle binding: {error}")))?;
    fs::File::open(&paths.runtime_dir)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| CascadeError::Io(format!("sync lifecycle binding directory: {error}")))?;
    Ok(())
}

pub(super) fn read_lifecycle_binding(
    paths: &RuntimePaths,
) -> Result<LifecycleBinding, CascadeError> {
    let payload = read_lifecycle_binding_payload(&paths.lifecycle_binding_file)?;
    let binding = serde_json::from_slice::<LifecycleBinding>(&payload).map_err(|error| {
        CascadeError::Precondition(format!("invalid lifecycle binding: {error}"))
    })?;
    validate_lifecycle_binding(&binding)?;
    Ok(binding)
}

fn read_lifecycle_binding_payload(path: &Path) -> Result<Vec<u8>, CascadeError> {
    let named = fs::symlink_metadata(path)
        .map_err(|error| CascadeError::Io(format!("stat lifecycle binding: {error}")))?;
    if !named.file_type().is_file()
        || named.nlink() != 1
        || named.len() == 0
        || named.len() > LIFECYCLE_BINDING_MAX_BYTES
        || named.mode() & 0o077 != 0
    {
        return Err(CascadeError::Precondition(
            "lifecycle binding is not a sealed regular file".into(),
        ));
    }
    #[cfg(not(test))]
    if named.uid() != 0 {
        return Err(CascadeError::Precondition(
            "lifecycle binding is not root-owned".into(),
        ));
    }
    #[cfg(test)]
    binding_read_tests::invoke_before_open(path);
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| CascadeError::Io(format!("open lifecycle binding: {error}")))?;
    let opened = file
        .metadata()
        .map_err(|error| CascadeError::Io(format!("stat opened lifecycle binding: {error}")))?;
    if !same_binding_snapshot(&opened, &named) {
        return Err(CascadeError::Precondition(
            "lifecycle binding changed while opening".into(),
        ));
    }
    #[cfg(test)]
    binding_read_tests::invoke_before_read(path);
    let mut payload = Vec::new();
    (&mut file)
        .take(LIFECYCLE_BINDING_MAX_BYTES + 1)
        .read_to_end(&mut payload)
        .map_err(|error| CascadeError::Io(format!("read lifecycle binding: {error}")))?;
    if payload.len() as u64 > LIFECYCLE_BINDING_MAX_BYTES {
        return Err(CascadeError::Precondition(
            "bounded lifecycle binding read exceeded its limit".into(),
        ));
    }
    let after = file.metadata().map_err(|error| {
        CascadeError::Io(format!("reinspect opened lifecycle binding: {error}"))
    })?;
    let named_after = fs::symlink_metadata(path)
        .map_err(|error| CascadeError::Io(format!("reinspect named lifecycle binding: {error}")))?;
    if !same_binding_snapshot(&opened, &after)
        || !same_binding_snapshot(&opened, &named_after)
        || payload.len() as u64 != opened.len()
    {
        return Err(CascadeError::Precondition(
            "lifecycle binding metadata changed during bounded read".into(),
        ));
    }
    Ok(payload)
}

fn same_binding_snapshot(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.len() == right.len()
        && left.nlink() == right.nlink()
        && left.uid() == right.uid()
        && left.mode() == right.mode()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

pub(super) fn binding_origin_matches_current(
    binding: &LifecycleBinding,
) -> Result<(), CascadeError> {
    let current = read_sealed_origin_config()?;
    let expected = BoundOriginIdentity {
        partuuid: current.partuuid,
        ptuuid: current.ptuuid,
        partition_dev_t: current.partition_dev_t,
        parent_dev_t: current.parent_dev_t,
        expected_swap_uuid: current.expected_swap_uuid,
        host_manifest_sha256: current.host_manifest_sha256,
        configuration_sha256: current.configuration_sha256,
    };
    if binding.origin != expected {
        return Err(CascadeError::Precondition(
            "lifecycle binding no longer matches the sealed origin manifest".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "effect_binding/tests.rs"]
mod binding_read_tests;
