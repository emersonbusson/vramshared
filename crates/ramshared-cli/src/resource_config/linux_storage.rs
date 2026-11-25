// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/linux_storage.rs
//! Native Linux volume resolution and existing-origin observation.

use super::*;

pub(super) use ramshared_block::storage_inventory::backing_device_identity;

fn resolve_mount<'a>(
    snapshot: &'a ResourceSnapshot,
    filesystem_uuid: &str,
    device_identity: &str,
) -> Result<(&'a BlockDevice, &'a MountInfo), (&'static str, String)> {
    if snapshot.platform != RuntimePlatform::NativeLinux {
        return Err((
            "identity_unavailable",
            "native Linux targets are unavailable on this platform".into(),
        ));
    }
    ramshared_block::storage_inventory::resolve_mount(
        &snapshot.block_devices,
        filesystem_uuid,
        device_identity,
    )
}

pub(super) fn observe_volume<'a>(
    snapshot: &'a ResourceSnapshot,
    filesystem_uuid: &str,
    device_identity: &str,
    now_unix_ms: u64,
) -> Result<(&'a MountInfo, StorageObservation), (&'static str, String)> {
    let (device, mount) = resolve_mount(snapshot, filesystem_uuid, device_identity)?;
    let (eligible, reason) = storage_eligibility(device, Some(mount));
    if !eligible {
        return Err(("target_ineligible", reason));
    }
    if !sample_is_fresh(mount.capacity_observed_unix_ms, now_unix_ms) {
        return Err((
            "stale_sample",
            "filesystem free-space sample is missing or stale".into(),
        ));
    }
    let (Some(total), Some(free)) = (mount.total_bytes, mount.available_bytes) else {
        return Err((
            "capacity_unavailable",
            "filesystem total/free capacity is unavailable".into(),
        ));
    };
    if free > total {
        return Err((
            "inconsistent_sample",
            "filesystem free capacity exceeds its total capacity".into(),
        ));
    }
    Ok((
        mount,
        StorageObservation {
            free_bytes: free,
            mount_id: Some(mount.mount_id),
        },
    ))
}

/// Existing fully allocated data needs current identity, not allocation capacity.
pub(super) fn observe_existing_mount<'a>(
    snapshot: &'a ResourceSnapshot,
    filesystem_uuid: &str,
    device_identity: &str,
    now_unix_ms: u64,
) -> Result<&'a MountInfo, (&'static str, String)> {
    if !sample_is_fresh(Some(snapshot.observed_unix_ms), now_unix_ms) {
        return Err((
            "stale_sample",
            "native volume inventory is missing or stale".into(),
        ));
    }
    let (device, mount) = resolve_mount(snapshot, filesystem_uuid, device_identity)?;
    ramshared_block::storage_inventory::origin_mount_eligibility(device, mount)
        .map_err(|reason| ("target_ineligible", reason))?;
    Ok(mount)
}

pub(super) fn linux_target_free_bytes(
    snapshot: &ResourceSnapshot,
    filesystem_uuid: &str,
    device_identity: &str,
    now_unix_ms: u64,
) -> Result<StorageObservation, (&'static str, String)> {
    observe_volume(snapshot, filesystem_uuid, device_identity, now_unix_ms)
        .map(|(_, observation)| observation)
}

pub(super) fn observe_target(
    snapshot: &ResourceSnapshot,
    target: &ResourceTarget,
    now_unix_ms: u64,
) -> Result<StorageObservation, (&'static str, String)> {
    let StorageVolumeIdentity::Linux {
        filesystem_uuid,
        device_identity,
    } = resource_volume_identity(target)
    else {
        return Err((
            "identity_unavailable",
            "a native Linux target is required".into(),
        ));
    };
    let (mount, observation) =
        observe_volume(snapshot, &filesystem_uuid, &device_identity, now_unix_ms)?;
    if matches!(target, ResourceTarget::LinuxFileOrigin { .. }) {
        native_origin::verify(target, mount)
            .map_err(|reason| ("origin_identity_unverified", reason))?;
    }
    Ok(observation)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::super::tests::{fixture_block_device, fixture_snapshot};
    use super::super::*;

    #[test]
    fn native_linux_plan_resolves_current_mount_from_stable_filesystem_identity() {
        let mut snapshot = fixture_snapshot();
        snapshot.platform = RuntimePlatform::NativeLinux;
        snapshot.windows = None;
        let mut device = fixture_block_device();
        device.eligible_for_file_storage = true;
        snapshot.block_devices = vec![device];
        let profile = r#"
schema_version = 1

[[targets]]
kind = "linux_swapfile"
filesystem_uuid = "fs-uuid"
device_identity = "wwn:wwn-123"
managed_relative_path = "swap/ramshared.swap"
bytes = 1073741824
priority = -1
"#;

        let plan = build_resource_plan(&snapshot, Some(profile)).expect("native plan builds");

        assert_eq!(plan.status, "ready_for_review");
        assert_eq!(plan.targets[0].status, "storage_ready");
        assert_eq!(plan.targets[0].observed_mount_id, Some(41));
        assert_eq!(plan.targets[0].required_free_bytes, 11 * 1024 * 1024 * 1024);
        assert_eq!(
            plan.targets[0].observed_free_bytes,
            Some(200 * 1024 * 1024 * 1024)
        );
        assert!(render_plan_text(&plan).contains("current mount ID (ephemeral): 41"));

        let mut stale_snapshot = snapshot.clone();
        stale_snapshot.block_devices[0].mounts[0].capacity_observed_unix_ms = Some(1);
        let stale_plan = build_resource_plan(&stale_snapshot, Some(profile))
            .expect("stale native telemetry is represented as a refusal");
        assert_eq!(stale_plan.targets[0].status, "stale_sample");
    }

    #[test]
    fn native_linux_origin_request_plan_binds_volume_without_claiming_creation() {
        let mut snapshot = fixture_snapshot();
        snapshot.platform = RuntimePlatform::NativeLinux;
        snapshot.windows = None;
        let mut device = fixture_block_device();
        device.eligible_for_file_storage = true;
        snapshot.block_devices = vec![device];
        let profile = r#"
schema_version = 1

[[targets]]
kind = "linux_file_origin_request"
filesystem_uuid = "fs-uuid"
device_identity = "wwn:wwn-123"
managed_relative_path = "origin/ramshared.img"
allocated_bytes = 4294967296
"#;

        let plan = build_resource_plan(&snapshot, Some(profile)).expect("origin request plans");

        assert_eq!(plan.status, "ready_for_review");
        assert_eq!(plan.targets[0].kind, "linux_file_origin_request");
        assert_eq!(plan.targets[0].status, "storage_ready");
        assert_eq!(plan.targets[0].required_free_bytes, 14 * 1024 * 1024 * 1024);
        assert_eq!(plan.targets[0].observed_mount_id, Some(41));
        assert!(!plan.writes_performed);
        assert!(!plan.apply_enabled);
        assert!(render_plan_text(&plan).contains("Read-only plan"));
    }

    #[test]
    fn native_linux_profile_survives_a_new_mount_namespace_id() {
        let mut snapshot = fixture_snapshot();
        snapshot.platform = RuntimePlatform::NativeLinux;
        snapshot.windows = None;
        let mut device = fixture_block_device();
        device.eligible_for_file_storage = true;
        device.mounts[0].mount_id = 990;
        snapshot.block_devices = vec![device];
        let profile = r#"
schema_version = 1

[[targets]]
kind = "linux_swapfile"
filesystem_uuid = "fs-uuid"
device_identity = "wwn:wwn-123"
managed_relative_path = "swap/ramshared.swap"
bytes = 1073741824
priority = -1
"#;

        let plan = build_resource_plan(&snapshot, Some(profile)).expect("profile plans");

        assert_eq!(plan.targets[0].status, "storage_ready");
        assert_eq!(plan.targets[0].observed_mount_id, Some(990));
    }

    #[test]
    fn native_linux_plan_refuses_multiple_current_mounts_for_one_profile_identity() {
        let mut snapshot = fixture_snapshot();
        snapshot.platform = RuntimePlatform::NativeLinux;
        snapshot.windows = None;
        let mut device = fixture_block_device();
        device.eligible_for_file_storage = true;
        let mut second_mount = device.mounts[0].clone();
        second_mount.mount_id = 42;
        second_mount.mountpoint = "/data-alias".into();
        device.mounts.push(second_mount);
        snapshot.block_devices = vec![device];
        let profile = r#"
schema_version = 1

[[targets]]
kind = "linux_swapfile"
filesystem_uuid = "fs-uuid"
device_identity = "wwn:wwn-123"
managed_relative_path = "swap/ramshared.swap"
bytes = 1073741824
priority = -1
"#;

        let plan = build_resource_plan(&snapshot, Some(profile)).expect("profile plans");

        assert_eq!(plan.targets[0].status, "identity_ambiguous");
        assert_eq!(plan.targets[0].observed_mount_id, None);
    }

    #[test]
    fn sealed_origin_plan_refuses_unverified_identity() {
        let mut snapshot = fixture_snapshot();
        snapshot.platform = RuntimePlatform::NativeLinux;
        snapshot.windows = None;
        snapshot.block_devices = vec![fixture_block_device()];
        let profile = ResourceProfile {
            schema_version: RESOURCE_PROFILE_SCHEMA_VERSION,
            planned_caps: PlannedTierCaps::default(),
            targets: vec![ResourceTarget::LinuxFileOrigin {
                filesystem_uuid: "fs-uuid".into(),
                device_identity: "wwn:wwn-123".into(),
                managed_relative_path: "origin/ramshared.img".into(),
                inode: 123,
                allocated_bytes: 4096,
                identity_field_hash: "a".repeat(64),
            }],
        };
        let plan = build_resource_plan(&snapshot, Some(&profile.to_toml().unwrap())).unwrap();
        assert_eq!(plan.status, "blocked");
        assert_ne!(plan.targets[0].status, "storage_ready");
        assert!(!plan.writes_performed);
        assert!(!plan.apply_enabled);
    }
    #[test]
    fn linux_volume_resolution_refuses_missing_identity_capacity_and_wrong_surface() {
        let mut snapshot = fixture_snapshot();
        let now = unix_millis();
        assert!(super::linux_target_free_bytes(&snapshot, "fs-uuid", "wwn:wwn-123", now).is_err());
        snapshot.platform = RuntimePlatform::NativeLinux;
        snapshot.block_devices = vec![fixture_block_device()];
        let original = snapshot.clone();
        assert!(super::linux_target_free_bytes(&snapshot, "absent", "absent", now).is_err());
        snapshot.block_devices[0].mounts.clear();
        assert!(super::linux_target_free_bytes(&snapshot, "fs-uuid", "wwn:wwn-123", now).is_err());
        snapshot = original.clone();
        snapshot.block_devices[0].read_only = Some(true);
        assert_eq!(
            super::linux_target_free_bytes(&snapshot, "fs-uuid", "wwn:wwn-123", now)
                .unwrap_err()
                .0,
            "target_ineligible"
        );
        snapshot = original.clone();
        snapshot.block_devices[0].mounts[0].available_bytes = None;
        assert_eq!(
            super::linux_target_free_bytes(&snapshot, "fs-uuid", "wwn:wwn-123", now)
                .unwrap_err()
                .0,
            "target_ineligible"
        );
        snapshot = original;
        snapshot.block_devices[0].mounts[0].available_bytes = Some(u64::MAX);
        assert_eq!(
            super::linux_target_free_bytes(&snapshot, "fs-uuid", "wwn:wwn-123", now)
                .unwrap_err()
                .0,
            "inconsistent_sample"
        );
        let foreign = ResourceTarget::WslFallback {
            windows_volume_id: "vol".into(),
            path: "C:\\swap.vhdx".into(),
            bytes: 4096,
        };
        assert!(super::observe_target(&snapshot, &foreign, now).is_err());
    }
}
