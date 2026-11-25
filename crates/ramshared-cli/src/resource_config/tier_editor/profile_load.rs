// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/tier_editor/profile_load.rs
//! Restore representable desired profiles without silently dropping requests.
use super::{Editor, Row, Tier, VolumeKey};
use crate::resource_config::{
    self as config, DraftStorageIdentity, DraftTargetRole, InventoryError, ResourceProfile,
    ResourceSnapshot, ResourceTarget,
};

impl Editor {
    pub(in crate::resource_config) fn from_profile(
        snapshot: &ResourceSnapshot,
        profile: &ResourceProfile,
        now: u64,
    ) -> Result<Self, InventoryError> {
        profile
            .validate_for(config::profile_platform(snapshot.platform))
            .map_err(|error| InventoryError(error.to_string()))?;
        if profile
            .planned_caps
            .zram_bytes
            .is_some_and(|value| value % super::MIB != 0)
            || profile
                .planned_caps
                .origin_bytes
                .is_some_and(|value| value % super::MIB != 0)
            || profile
                .planned_caps
                .vram_bytes
                .values()
                .any(|value| value % super::MIB != 0)
        {
            return Err(unsupported("sizes outside the editor's MiB resolution"));
        }
        let mut editor = Self::from_snapshot(snapshot, now);
        for row in &mut editor.rows {
            row.value = 0;
        }
        editor.prepared.omit();
        editor.origin_cap_override = Some(profile.planned_caps.origin_bytes);
        editor.rows[0].value = profile.planned_caps.zram_bytes.unwrap_or(0);
        if profile.planned_caps.zram_bytes == Some(0)
            || profile.planned_caps.origin_bytes == Some(0)
        {
            return Err(unsupported("explicit zero ceilings"));
        }
        for (key, value) in &profile.planned_caps.vram_bytes {
            let uuid = key
                .strip_prefix("nvidia-uuid:")
                .filter(|uuid| uuid.starts_with("gpu-") && *uuid == uuid.to_ascii_lowercase())
                .ok_or_else(|| unsupported("GPU key format"))?;
            if *value == 0 {
                return Err(unsupported("explicit zero GPU ceilings"));
            }
            if let Some(row) = editor
                .rows
                .iter_mut()
                .find(|row| matches!(&row.tier, Tier::Gpu(id) if id.eq_ignore_ascii_case(uuid)))
            {
                row.value = *value;
            } else {
                editor.rows.push(Row {
                    tier: Tier::Gpu(uuid.into()),
                    value: *value,
                    volume: None,
                    maximum: None,
                    observed_unix_ms: 0,
                    host_share: None,
                    runtime_headroom: None,
                });
            }
        }
        let mut roles = std::collections::HashSet::new();
        for target in &profile.targets {
            if editor.restore_prepared(snapshot, target, now)? {
                if !roles.insert("origin") {
                    return Err(unsupported("multiple targets for one storage role"));
                }
                continue;
            }
            if target.allocated_bytes() % super::MIB != 0 {
                return Err(unsupported("sizes outside the editor's MiB resolution"));
            }
            let (tier, role, storage) = target_request(target)?;
            if !roles.insert(tier_name(&tier)) {
                return Err(unsupported("multiple targets for one storage role"));
            }
            let key = match &storage {
                DraftStorageIdentity::NativeLinux {
                    filesystem_uuid,
                    device_identity,
                } => VolumeKey::Linux {
                    filesystem_uuid: filesystem_uuid.clone(),
                    device_identity: device_identity.clone(),
                },
                DraftStorageIdentity::Wsl2 { volume_id, .. } => {
                    VolumeKey::Windows(volume_id.to_ascii_lowercase())
                }
            };
            let expected = config::materialize_draft_target(
                &config::DraftVolumeCandidate {
                    display: String::new(),
                    short_display: String::new(),
                    free_bytes: 0,
                    observed_unix_ms: 0,
                    storage,
                },
                role,
                target.allocated_bytes(),
            )?;
            if &expected != target {
                return Err(unsupported(
                    "custom paths, priorities, or non-canonical target fields",
                ));
            }
            let row = editor
                .rows
                .iter_mut()
                .find(|row| row.tier == tier)
                .ok_or_else(|| unsupported("storage role"))?;
            row.value = target.allocated_bytes();
            row.volume = Some(key);
        }
        Ok(editor)
    }
}

fn unsupported(reason: &str) -> InventoryError {
    InventoryError(format!(
        "bar editor cannot preserve {reason}; no file was changed. Inspect this profile with config plan"
    ))
}
fn tier_name(tier: &Tier) -> &'static str {
    match tier {
        Tier::Origin => "origin",
        Tier::Fallback => "fallback",
        _ => "emergency",
    }
}
fn target_request(
    target: &ResourceTarget,
) -> Result<(Tier, DraftTargetRole, DraftStorageIdentity), InventoryError> {
    let (tier, role) = match target {
        ResourceTarget::LinuxSwapfile { .. } | ResourceTarget::WslFallback { .. } => {
            (Tier::Fallback, DraftTargetRole::FallbackSwap)
        }
        ResourceTarget::LinuxEmergencySwapfile { .. }
        | ResourceTarget::WslEmergencyStorage { .. } => {
            (Tier::Emergency, DraftTargetRole::Emergency)
        }
        ResourceTarget::LinuxFileOriginRequest { .. } | ResourceTarget::WslOrigin { .. } => {
            (Tier::Origin, DraftTargetRole::RamSharedOrigin)
        }
        ResourceTarget::LinuxFileOrigin { .. } => {
            return Err(unsupported("a sealed origin's immutable identity"));
        }
    };
    let storage = match target {
        ResourceTarget::LinuxSwapfile {
            filesystem_uuid,
            device_identity,
            ..
        }
        | ResourceTarget::LinuxEmergencySwapfile {
            filesystem_uuid,
            device_identity,
            ..
        }
        | ResourceTarget::LinuxFileOriginRequest {
            filesystem_uuid,
            device_identity,
            ..
        } => DraftStorageIdentity::NativeLinux {
            filesystem_uuid: filesystem_uuid.clone(),
            device_identity: device_identity.clone(),
        },
        ResourceTarget::WslFallback {
            windows_volume_id,
            path,
            ..
        }
        | ResourceTarget::WslEmergencyStorage {
            windows_volume_id,
            path,
            ..
        }
        | ResourceTarget::WslOrigin {
            windows_volume_id,
            path,
            ..
        } => DraftStorageIdentity::Wsl2 {
            volume_id: windows_volume_id.clone(),
            drive_letter: if path.as_bytes().get(1) == Some(&b':') {
                path.get(..1).map(str::to_owned)
            } else {
                None
            },
        },
        _ => return Err(unsupported("sealed origin identity")),
    };
    Ok((tier, role, storage))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    const GIB: u64 = 1024 * 1024 * 1024;

    fn fixture(native: bool) -> (ResourceSnapshot, ResourceProfile) {
        let mut snapshot = config::tests::fixture_snapshot();
        if native {
            snapshot.platform = config::RuntimePlatform::NativeLinux;
            snapshot.windows = None;
            snapshot.block_devices = vec![config::tests::fixture_block_device()];
        }
        let now = config::unix_millis();
        snapshot.gpu_inventory =
            config::gpu_inventory::parse_inventory("GPU-a, 6144, 6144").expect("GPU fixture");
        snapshot.gpu_observed_unix_ms = now;
        let mut editor = Editor::from_snapshot(&snapshot, now);
        for row in &mut editor.rows {
            if !matches!(row.tier, Tier::Gpu(_)) {
                row.value = GIB;
            }
        }
        let profile = editor.profile(&snapshot, now).expect("initial profile");
        (snapshot, profile)
    }

    #[test]
    fn saved_profiles_restore_every_tier_and_separate_origin_ceiling() {
        for native in [false, true] {
            let (snapshot, mut profile) = fixture(native);
            for cap in [None, Some(2 * GIB)] {
                profile.planned_caps.origin_bytes = cap;
                let editor = Editor::from_profile(&snapshot, &profile, config::unix_millis())
                    .expect("restore");
                assert_eq!(
                    editor
                        .profile(&snapshot, config::unix_millis())
                        .expect("roundtrip"),
                    profile
                );
            }
        }
    }

    #[test]
    fn missing_resources_and_lower_limits_keep_desired_values() {
        let (snapshot, profile) = fixture(false);
        let mut absent = snapshot.clone();
        absent.gpu_inventory.clear();
        absent.windows.as_mut().expect("host").volumes.clear();
        let mut editor = Editor::from_profile(&absent, &profile, config::unix_millis())
            .expect("missing hardware stays visible");
        assert!(editor.profile(&absent, config::unix_millis()).is_err());
        assert!(
            editor
                .rows
                .iter()
                .any(|row| matches!(row.tier, Tier::Gpu(_)) && row.value == 3840 * 1024 * 1024)
        );
        editor.reconcile(Editor::from_snapshot(&snapshot, config::unix_millis()));
        assert_eq!(
            editor
                .profile(&snapshot, config::unix_millis())
                .expect("restored hardware"),
            profile
        );
        let mut smaller = snapshot.clone();
        smaller.guest_memory.total_bytes = Some(GIB / 2);
        let editor = Editor::from_profile(&smaller, &profile, config::unix_millis())
            .expect("keep excessive request");
        assert!(
            editor
                .render(config::unix_millis())
                .contains("above current limit")
        );
        assert!(editor.profile(&smaller, config::unix_millis()).is_err());
    }

    #[test]
    fn existing_gpu_draft_above_new_free_floor_remains_visible_and_blocked() {
        let (snapshot, mut profile) = fixture(false);
        profile
            .planned_caps
            .vram_bytes
            .insert("nvidia-uuid:gpu-a".into(), 4 * GIB);
        let now = config::unix_millis();
        let editor =
            Editor::from_profile(&snapshot, &profile, now).expect("preserve old desired cap");
        assert!(
            editor
                .rows
                .iter()
                .any(|row| matches!(row.tier, Tier::Gpu(_)) && row.value == 4 * GIB)
        );
        assert!(editor.render(now).contains("above current limit"));
        assert!(editor.profile(&snapshot, now).is_err());
        assert_eq!(
            profile.planned_caps.vram_bytes["nvidia-uuid:gpu-a"],
            4 * GIB
        );
    }

    #[test]
    fn unsupported_shapes_are_refused_without_silent_loss() {
        let (snapshot, original) = fixture(false);
        let mut profile = original.clone();
        profile
            .planned_caps
            .vram_bytes
            .insert("manual-id".into(), GIB);
        assert!(Editor::from_profile(&snapshot, &profile, config::unix_millis()).is_err());
        for cap in [Some(0), Some(1)] {
            profile = original.clone();
            profile.planned_caps.zram_bytes = cap;
            assert!(Editor::from_profile(&snapshot, &profile, config::unix_millis()).is_err());
        }
        profile = original.clone();
        let mut extra = profile.targets[1].clone();
        if let ResourceTarget::WslFallback { path, .. } = &mut extra {
            *path = "C:\\wsl\\ramshared\\other.vhdx".into();
        }
        profile.targets.push(extra);
        assert!(Editor::from_profile(&snapshot, &profile, config::unix_millis()).is_err());
        profile = original;
        if let ResourceTarget::WslOrigin { path, .. } = &mut profile.targets[0] {
            *path = "C:\\wsl\\ramshared\\custom.vhdx".into();
        }
        assert!(Editor::from_profile(&snapshot, &profile, config::unix_millis()).is_err());
        let (native, mut profile) = fixture(true);
        if let ResourceTarget::LinuxSwapfile { priority, .. } = &mut profile.targets[1] {
            *priority = 10;
        }
        assert!(Editor::from_profile(&native, &profile, config::unix_millis()).is_err());
        profile.targets = vec![ResourceTarget::LinuxFileOrigin {
            filesystem_uuid: "fs-uuid".into(),
            device_identity: "wwn:wwn-123".into(),
            managed_relative_path: "origin/ramshared-origin.img".into(),
            inode: 1,
            allocated_bytes: GIB,
            identity_field_hash: "a".repeat(64),
        }];
        assert!(Editor::from_profile(&native, &profile, config::unix_millis()).is_err());
    }
}
