// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/prepared_origin.rs
//! Read-only projection of prepared native storage into desired resource plans.

use super::native_origin::{OriginCustody, open as open_native_origin};
use super::{InventoryError, ResourceSnapshot, RuntimePlatform};
use crate::artifact_file;
use ramshared_config::native_origin::{NATIVE_ORIGIN_MANIFEST_NAME, NativeOriginManifest};
use ramshared_config::resource_profile::{ResourceProfile, ResourceTarget};
use std::path::Path;

const ROOT_UID: u32 = 0;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Observation {
    NotApplicable,
    Absent,
    Receipt {
        manifest: Box<NativeOriginManifest>,
        observed_unix_ms: u64,
    },
    Unavailable(String),
}

pub(super) fn observe(platform: RuntimePlatform, root: &Path) -> Observation {
    observe_owned(platform, root, ROOT_UID)
}

fn observe_owned(platform: RuntimePlatform, root: &Path, owner_uid: u32) -> Observation {
    if platform != RuntimePlatform::NativeLinux {
        return Observation::NotApplicable;
    }
    let path = root.join(NATIVE_ORIGIN_MANIFEST_NAME);
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Observation::Absent,
        Err(error) => return Observation::Unavailable(error.to_string()),
        Ok(_) => {}
    }
    if rustix::process::geteuid().as_raw() != owner_uid {
        return Observation::Unavailable(
            "prepared origin inspection requires the protected manifest's owner".into(),
        );
    }
    match artifact_file::read_private(&path)
        .map_err(|error| error.to_string())
        .and_then(|text| NativeOriginManifest::parse(&text))
    {
        Ok(manifest) => Observation::Receipt {
            manifest: Box::new(manifest),
            observed_unix_ms: super::unix_millis(),
        },
        Err(detail) => Observation::Unavailable(detail),
    }
}

pub(super) fn project(
    snapshot: &ResourceSnapshot,
    profile: ResourceProfile,
    now_unix_ms: u64,
) -> Result<ResourceProfile, InventoryError> {
    project_owned(snapshot, profile, now_unix_ms, ROOT_UID)
}

fn refused(detail: impl Into<String>) -> InventoryError {
    InventoryError(format!("prepared native origin: {}", detail.into()))
}

fn project_owned(
    snapshot: &ResourceSnapshot,
    mut profile: ResourceProfile,
    now_unix_ms: u64,
    owner_uid: u32,
) -> Result<ResourceProfile, InventoryError> {
    if !profile
        .targets
        .iter()
        .any(|target| matches!(target, ResourceTarget::LinuxFileOriginRequest { .. }))
    {
        return Ok(profile);
    }
    if snapshot.platform != RuntimePlatform::NativeLinux {
        return Err(refused(
            "native receipt observation is unavailable on this platform",
        ));
    }
    let Some(proof) = verified_owned(snapshot, now_unix_ms, owner_uid)? else {
        return Ok(profile);
    };
    for target in &mut profile.targets {
        if !matches!(target, ResourceTarget::LinuxFileOriginRequest { .. }) {
            continue;
        }
        if !proof.accepts(target) {
            return Err(refused(
                "existing origin geometry is immutable; a different volume, path or size needs its migration owner",
            ));
        }
        *target = proof.target().clone();
    }
    Ok(profile)
}

/// Current read-only identity proof. Holding this object pins the opened file
/// and volume while the consumer binds desired policy to their metadata.
pub(crate) struct VerifiedOrigin<'a> {
    manifest: &'a NativeOriginManifest,
    _volume: OriginCustody,
    _file: std::fs::File,
}

impl VerifiedOrigin<'_> {
    pub(crate) fn target(&self) -> &ResourceTarget {
        &self.manifest.target
    }
    pub(super) fn accepts(&self, target: &ResourceTarget) -> bool {
        target == self.target() || self.manifest.matches_request(target)
    }
}

pub(crate) fn verified(
    snapshot: &ResourceSnapshot,
    now_unix_ms: u64,
) -> Result<Option<VerifiedOrigin<'_>>, InventoryError> {
    if snapshot.platform == RuntimePlatform::Wsl2 {
        return Ok(None);
    }
    verified_owned(snapshot, now_unix_ms, ROOT_UID)
}

fn verified_owned(
    snapshot: &ResourceSnapshot,
    now_unix_ms: u64,
    owner_uid: u32,
) -> Result<Option<VerifiedOrigin<'_>>, InventoryError> {
    let (manifest, observed_unix_ms) = match &snapshot.prepared_native_origin {
        Observation::Absent if snapshot.platform == RuntimePlatform::NativeLinux => {
            return Ok(None);
        }
        Observation::Receipt {
            manifest,
            observed_unix_ms,
        } if snapshot.platform == RuntimePlatform::NativeLinux => (manifest, *observed_unix_ms),
        Observation::Unavailable(detail) => return Err(refused(detail)),
        _ => {
            return Err(refused(
                "native receipt observation is unavailable on this platform",
            ));
        }
    };
    if observed_unix_ms == 0
        || now_unix_ms == 0
        || !super::sample_is_fresh(Some(observed_unix_ms), now_unix_ms)
    {
        return Err(refused(
            "receipt observation is stale or has no valid clock",
        ));
    }
    manifest.validate().map_err(refused)?;
    let ResourceTarget::LinuxFileOrigin {
        filesystem_uuid,
        device_identity,
        ..
    } = &manifest.target
    else {
        return Err(refused("receipt has no sealed native identity"));
    };
    let (mount, _) = super::linux_storage::observe_volume(
        snapshot,
        filesystem_uuid,
        device_identity,
        now_unix_ms,
    )
    .map_err(|(code, detail)| refused(format!("{code}: {detail}")))?;
    let volume = open_native_origin(mount, owner_uid).map_err(refused)?;
    let file = volume.verify(&manifest.target).map_err(refused)?;
    Ok(Some(VerifiedOrigin {
        manifest,
        _volume: volume,
        _file: file,
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use ramshared_config::resource_profile::{
        DISK_RESERVE_FLOOR_BYTES, MAX_RESOURCE_PROFILE_BYTES,
    };
    use std::fs::{self, File};
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixture {
        root: PathBuf,
        config: PathBuf,
        snapshot: ResourceSnapshot,
        profile: ResourceProfile,
        manifest: NativeOriginManifest,
        _held: File,
    }

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "ramshared-prepared-projection-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
            ));
            fs::create_dir(&root).unwrap();
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
            let config = root.join("configuration");
            fs::create_dir(&config).unwrap();
            let root_file = File::open(&root).unwrap();
            let stat = rustix::fs::statx(
                &root_file,
                "",
                rustix::fs::AtFlags::EMPTY_PATH,
                rustix::fs::StatxFlags::BASIC_STATS | rustix::fs::StatxFlags::MNT_ID,
            )
            .unwrap();
            let mut device = super::super::tests::fixture_block_device();
            device.major_minor = Some(format!("{}:{}", stat.stx_dev_major, stat.stx_dev_minor));
            device.mountpoints = vec![root.to_str().unwrap().into()];
            let mount = &mut device.mounts[0];
            mount.mountpoint = root.to_str().unwrap().into();
            mount.mount_id = stat.stx_mnt_id;
            mount.major_minor = device.major_minor.clone().unwrap();
            // Explicit capacity fixture: this does not qualify native hardware.
            mount.available_bytes = Some(DISK_RESERVE_FLOOR_BYTES + 8191);
            let profile = ResourceProfile::parse(
                "schema_version = 1\n[[targets]]\nkind = 'linux_file_origin_request'\nfilesystem_uuid = 'fs-uuid'\ndevice_identity = 'wwn:wwn-123'\nmanaged_relative_path = 'origin/data.img'\nallocated_bytes = 8192\n",
            ).unwrap();
            let volume = open_native_origin(mount, rustix::process::geteuid().as_raw()).unwrap();
            let (held, manifest) = volume
                .create_sealed(&profile.targets[0], |target| {
                    NativeOriginManifest::new(
                        "a".repeat(64),
                        "creation-approval".into(),
                        "b".repeat(64),
                        "creator-release".into(),
                        target,
                    )
                })
                .unwrap();
            let mut snapshot = super::super::tests::fixture_snapshot();
            snapshot.platform = RuntimePlatform::NativeLinux;
            snapshot.windows = None;
            snapshot.block_devices = vec![device];
            snapshot.prepared_native_origin = Observation::Receipt {
                manifest: Box::new(manifest.clone()),
                observed_unix_ms: super::super::unix_millis(),
            };
            Self {
                root,
                config,
                snapshot,
                profile,
                manifest,
                _held: held,
            }
        }
        fn project(&self, profile: ResourceProfile) -> Result<ResourceProfile, InventoryError> {
            project_owned(
                &self.snapshot,
                profile,
                super::super::unix_millis(),
                rustix::process::geteuid().as_raw(),
            )
        }
        fn origin(&self) -> PathBuf {
            self.root.join("var/lib/ramshared/origin/data.img")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).unwrap();
        }
    }

    #[test]
    fn prepared_origin_projection_credits_verified_allocation_and_preserves_desired_policy() {
        let fixture = Fixture::new();
        let mut desired = fixture.profile.clone();
        desired.planned_caps.zram_bytes = Some(1024 * 1024);
        let fallback = ResourceTarget::LinuxSwapfile {
            filesystem_uuid: "fs-uuid".into(),
            device_identity: "wwn:wwn-123".into(),
            managed_relative_path: "swap/fallback.swap".into(),
            bytes: 4096,
            priority: -1,
        };
        desired.targets.push(fallback.clone());
        desired
            .validate_for(ramshared_config::resource_profile::ResourcePlatform::NativeLinux)
            .unwrap();
        let original = desired.to_toml().unwrap();
        let creator = fixture.manifest.clone();
        let metadata = fs::metadata(fixture.origin()).unwrap();
        let bytes = fs::read(fixture.origin()).unwrap();
        let projected = fixture.project(desired.clone()).unwrap();
        assert_eq!(projected.planned_caps, desired.planned_caps);
        assert_eq!(
            projected.targets,
            vec![fixture.manifest.target.clone(), fallback]
        );
        let free = fixture.snapshot.block_devices[0].mounts[0]
            .available_bytes
            .unwrap();
        assert!(
            desired
                .required_free_bytes_by_volume()
                .unwrap()
                .values()
                .all(|v| *v > free)
        );
        assert_eq!(
            projected
                .required_free_bytes_by_volume()
                .unwrap()
                .values()
                .copied()
                .collect::<Vec<_>>(),
            vec![DISK_RESERVE_FLOOR_BYTES + 4096]
        );
        assert_eq!(desired.to_toml().unwrap(), original);
        assert_ne!(
            super::super::profile_digest(&original),
            creator.profile_fingerprint
        );
        assert_eq!(fixture.manifest, creator);
        assert_eq!(
            fs::metadata(fixture.origin()).unwrap().ino(),
            metadata.ino()
        );
        assert_eq!(fs::read(fixture.origin()).unwrap(), bytes);
        assert!(
            serde_json::to_value(&fixture.snapshot)
                .unwrap()
                .get("prepared_native_origin")
                .is_none()
        );
        if rustix::process::geteuid().as_raw() != ROOT_UID {
            assert!(project(&fixture.snapshot, desired, super::super::unix_millis()).is_err());
        }
    }

    #[test]
    fn prepared_origin_projection_refuses_geometry_identity_and_observation_drift() {
        let mut fixture = Fixture::new();
        let mut changed = fixture.profile.clone();
        if let ResourceTarget::LinuxFileOriginRequest {
            allocated_bytes, ..
        } = &mut changed.targets[0]
        {
            *allocated_bytes += 4096;
        }
        assert!(
            fixture
                .project(changed)
                .unwrap_err()
                .to_string()
                .contains("immutable")
        );
        let good = fixture.snapshot.prepared_native_origin.clone();
        for state in [
            Observation::NotApplicable,
            Observation::Unavailable("unreadable".into()),
            Observation::Receipt {
                manifest: Box::new(fixture.manifest.clone()),
                observed_unix_ms: 1,
            },
        ] {
            fixture.snapshot.prepared_native_origin = state;
            assert!(fixture.project(fixture.profile.clone()).is_err());
        }
        fixture.snapshot.prepared_native_origin = good.clone();
        assert!(
            project_owned(
                &fixture.snapshot,
                fixture.profile.clone(),
                0,
                rustix::process::geteuid().as_raw()
            )
            .is_err()
        );
        fixture.snapshot.prepared_native_origin = Observation::Receipt {
            manifest: Box::new(fixture.manifest.clone()),
            observed_unix_ms: 0,
        };
        assert!(fixture.project(fixture.profile.clone()).is_err());
        fixture.snapshot.prepared_native_origin = good;
        fixture.snapshot.block_devices[0].mounts[0].capacity_observed_unix_ms = Some(1);
        assert!(fixture.project(fixture.profile.clone()).is_err());
        fixture.snapshot.block_devices[0].mounts[0].capacity_observed_unix_ms =
            Some(super::super::unix_millis());
        fs::rename(fixture.origin(), fixture.root.join("preserved.img")).unwrap();
        fs::write(fixture.origin(), vec![0; 8192]).unwrap();
        fs::set_permissions(fixture.origin(), fs::Permissions::from_mode(0o600)).unwrap();
        assert!(fixture.project(fixture.profile.clone()).is_err());
        assert_eq!(
            fs::metadata(fixture.root.join("preserved.img"))
                .unwrap()
                .ino(),
            fixture._held.metadata().unwrap().ino()
        );
        fixture.snapshot.prepared_native_origin = Observation::Absent;
        assert_eq!(
            fixture.project(fixture.profile.clone()).unwrap(),
            fixture.profile
        );
        fixture.snapshot.platform = RuntimePlatform::Wsl2;
        assert!(fixture.project(fixture.profile.clone()).is_err());
        let empty = ResourceProfile::parse("schema_version = 1").unwrap();
        assert_eq!(fixture.project(empty.clone()).unwrap(), empty);
    }

    #[test]
    fn prepared_origin_observation_is_private_bounded_and_platform_scoped() {
        let fixture = Fixture::new();
        let uid = rustix::process::geteuid().as_raw();
        assert_eq!(
            observe_owned(RuntimePlatform::NativeLinux, &fixture.config, uid),
            Observation::Absent
        );
        let path = fixture.config.join(NATIVE_ORIGIN_MANIFEST_NAME);
        artifact_file::write_new(&path, fixture.manifest.encode().unwrap().as_bytes()).unwrap();
        assert!(matches!(
            observe_owned(RuntimePlatform::NativeLinux, &fixture.config, uid),
            Observation::Receipt { .. }
        ));
        assert_eq!(
            observe(RuntimePlatform::Wsl2, &fixture.config),
            Observation::NotApplicable
        );
        assert!(matches!(
            observe_owned(
                RuntimePlatform::NativeLinux,
                &fixture.config,
                uid.saturating_add(1)
            ),
            Observation::Unavailable(_)
        ));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            observe_owned(RuntimePlatform::NativeLinux, &fixture.config, uid),
            Observation::Unavailable(_)
        ));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&path, "invalid manifest").unwrap();
        assert!(matches!(
            observe_owned(RuntimePlatform::NativeLinux, &fixture.config, uid),
            Observation::Unavailable(_)
        ));
        fs::write(&path, vec![b'x'; MAX_RESOURCE_PROFILE_BYTES + 1]).unwrap();
        assert!(matches!(
            observe_owned(RuntimePlatform::NativeLinux, &fixture.config, uid),
            Observation::Unavailable(_)
        ));
        fs::remove_file(&path).unwrap();
        symlink(fixture.origin(), &path).unwrap();
        assert!(matches!(
            observe_owned(RuntimePlatform::NativeLinux, &fixture.config, uid),
            Observation::Unavailable(_)
        ));
        assert!(matches!(
            observe_owned(RuntimePlatform::NativeLinux, &path, uid),
            Observation::Unavailable(_)
        ));
    }

    #[test]
    fn prepared_origin_projection_is_consumed_by_config_planning() {
        let mut fixture = Fixture::new();
        fixture.snapshot.prepared_native_origin =
            Observation::Unavailable("receipt is not readable".into());
        let text = fixture.profile.to_toml().unwrap();
        let error = super::super::build_resource_plan(&fixture.snapshot, Some(&text)).unwrap_err();
        assert!(error.to_string().contains("receipt is not readable"));
        fixture.snapshot.prepared_native_origin = Observation::Absent;
        let plan = super::super::build_resource_plan(&fixture.snapshot, Some(&text)).unwrap();
        assert_eq!(plan.status, "blocked");
        assert_eq!(
            plan.profile_sha256,
            Some(super::super::profile_digest(&text))
        );
        assert_eq!(plan.targets[0].kind, "linux_file_origin_request");
        assert!(!plan.writes_performed && !plan.apply_enabled);
        if rustix::process::geteuid().as_raw() == ROOT_UID {
            fixture.snapshot.prepared_native_origin = Observation::Receipt {
                manifest: Box::new(fixture.manifest.clone()),
                observed_unix_ms: super::super::unix_millis(),
            };
            let plan = super::super::build_resource_plan(&fixture.snapshot, Some(&text)).unwrap();
            assert_eq!(plan.status, "ready_for_review");
            assert_eq!(plan.targets[0].kind, "linux_file_origin");
            assert_eq!(
                plan.targets[0].required_free_bytes,
                DISK_RESERVE_FLOOR_BYTES
            );
            assert_eq!(
                plan.profile_sha256,
                Some(super::super::profile_digest(&text))
            );
            assert!(!plan.writes_performed && !plan.apply_enabled);
        }
    }

    fn submit_dialog(
        dialog: &mut super::super::editor_save::SaveDialog,
        editor: &mut super::super::tier_editor::Editor,
        snapshot: &ResourceSnapshot,
        value: &str,
    ) {
        use ratatui::crossterm::event::KeyCode;
        let mut observe = || Ok(snapshot.clone());
        for ch in value.chars() {
            dialog.input(KeyCode::Char(ch), editor, &mut observe);
        }
        dialog.input(KeyCode::Enter, editor, &mut observe);
    }

    #[test]
    fn prepared_origin_editor_root_component_preserves_request_and_sealed_profiles() {
        use super::super::tier_editor::Editor;
        let fixture = Fixture::new();
        let now = super::super::unix_millis();
        if rustix::process::geteuid().as_raw() != ROOT_UID {
            assert!(Editor::from_profile(&fixture.snapshot, &fixture.profile, now).is_err());
            return;
        }
        let original_data = fs::read(fixture.origin()).unwrap();
        let inode = fs::metadata(fixture.origin()).unwrap().ino();
        for (index, target) in [
            fixture.profile.targets[0].clone(),
            fixture.manifest.target.clone(),
        ]
        .into_iter()
        .enumerate()
        {
            let mut desired = fixture.profile.clone();
            desired.targets = vec![target];
            let mut editor = Editor::from_profile(&fixture.snapshot, &desired, now).unwrap();
            assert_eq!(editor.profile(&fixture.snapshot, now).unwrap(), desired);
            editor.adjust_selected(true, now);
            desired.planned_caps.zram_bytes = Some(256 * 1024 * 1024);
            editor.next(); // The controlled fixture has no GPU rows.
            editor.clear_selected();
            editor.fill_selected(now);
            editor.adjust_selected(true, now);
            editor.adjust_selected(false, now);
            editor.cycle_selected(now);
            editor.reconcile(Editor::from_snapshot(&fixture.snapshot, now));
            assert_eq!(editor.profile(&fixture.snapshot, now).unwrap(), desired);
            let rendered = editor.render(now);
            assert!(rendered.contains("8.00 KiB"));
            assert!(rendered.contains("size and path locked"));
            eprintln!("{rendered}");
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 48)).unwrap();
            terminal
                .draw(|frame| super::super::draw_config_frame(frame, &fixture.snapshot, 0, &editor))
                .unwrap();
            let screen = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            for label in [
                "SSD origin",
                "8.00 KiB",
                "size and path locked",
                "10.00 GiB",
                "save draft",
            ] {
                assert!(screen.contains(label), "missing rendered label: {label}");
            }
            let output = fixture.config.join(format!("prepared-{index}.toml"));
            let mut dialog = super::super::editor_save::SaveDialog::default();
            submit_dialog(
                &mut dialog,
                &mut editor,
                &fixture.snapshot,
                output.to_str().unwrap(),
            );
            submit_dialog(&mut dialog, &mut editor, &fixture.snapshot, "SAVE");
            let saved = ResourceProfile::parse(&fs::read_to_string(output).unwrap()).unwrap();
            assert_eq!(saved, desired);
            assert_eq!(fs::metadata(fixture.origin()).unwrap().ino(), inode);
            assert_eq!(fs::read(fixture.origin()).unwrap(), original_data);
        }
        let omitted =
            ResourceProfile::parse("schema_version = 1\n[planned_caps]\nzram_bytes = 1048576\n")
                .unwrap();
        let editor = Editor::from_profile(&fixture.snapshot, &omitted, now).unwrap();
        assert_eq!(editor.profile(&fixture.snapshot, now).unwrap(), omitted);
    }

    #[test]
    fn prepared_origin_editor_refuses_replacement_during_save_and_recovers_matching_identity() {
        use super::super::tier_editor::Editor;
        let fixture = Fixture::new();
        let now = super::super::unix_millis();
        if rustix::process::geteuid().as_raw() != ROOT_UID {
            assert!(Editor::from_profile(&fixture.snapshot, &fixture.profile, now).is_err());
            return;
        }
        let mut editor = Editor::from_profile(&fixture.snapshot, &fixture.profile, now).unwrap();
        let output = fixture.config.join("must-not-save.toml");
        let mut dialog = super::super::editor_save::SaveDialog::default();
        submit_dialog(
            &mut dialog,
            &mut editor,
            &fixture.snapshot,
            output.to_str().unwrap(),
        );
        let preserved = fixture.root.join("preserved.img");
        fs::rename(fixture.origin(), &preserved).unwrap();
        fs::write(fixture.origin(), vec![0; 8192]).unwrap();
        fs::set_permissions(fixture.origin(), fs::Permissions::from_mode(0o600)).unwrap();
        submit_dialog(&mut dialog, &mut editor, &fixture.snapshot, "SAVE");
        assert!(!output.exists());
        assert!(editor.profile(&fixture.snapshot, now).is_err());
        assert!(editor.render(now).contains("binding preserved"));
        assert_eq!(
            fs::metadata(&preserved).unwrap().ino(),
            fixture._held.metadata().unwrap().ino()
        );
        // Restore only the two files this isolated fixture deliberately moved.
        fs::remove_file(fixture.origin()).unwrap();
        fs::rename(preserved, fixture.origin()).unwrap();
        editor.reconcile(Editor::from_snapshot(&fixture.snapshot, now));
        assert_eq!(
            editor.profile(&fixture.snapshot, now).unwrap(),
            fixture.profile
        );
    }

    #[test]
    fn prepared_origin_non_root_guards_and_save_dialog_edges() {
        let mut fixture = Fixture::new();
        let now = super::super::unix_millis();
        // WSL2 snapshot short-circuits `verified` to Ok(None).
        let mut wsl_snap = fixture.snapshot.clone();
        wsl_snap.platform = RuntimePlatform::Wsl2;
        assert!(verified(&wsl_snap, now).unwrap().is_none());

        // Non-LinuxFileOrigin target in receipt refuses in `verified_owned`.
        let mut non_origin = fixture.manifest.clone();
        non_origin.target = ResourceTarget::LinuxSwapfile {
            filesystem_uuid: "11111111-2222-4333-8444-555555555555".into(),
            device_identity: "partuuid:abcd".into(),
            managed_relative_path: "var/lib/ramshared/swapfile".into(),
            bytes: 64 * 1024 * 1024,
            priority: -2,
        };
        fixture.snapshot.prepared_native_origin = Observation::Receipt {
            manifest: Box::new(non_origin),
            observed_unix_ms: now,
        };
        assert!(
            verified_owned(&fixture.snapshot, now, rustix::process::geteuid().as_raw()).is_err()
        );

        // Exercise `submit_dialog` cleanly on an unprivileged snapshot editor.
        fixture.snapshot.prepared_native_origin = Observation::Absent;
        let mut snap_editor =
            super::super::tier_editor::Editor::from_snapshot(&fixture.snapshot, now);
        snap_editor.adjust_selected(true, now);
        let mut dialog = super::super::editor_save::SaveDialog::default();
        let output = fixture.config.join("snapshot-dialog.toml");
        submit_dialog(
            &mut dialog,
            &mut snap_editor,
            &fixture.snapshot,
            output.to_str().unwrap(),
        );
        submit_dialog(&mut dialog, &mut snap_editor, &fixture.snapshot, "SAVE");
        assert!(output.exists());
    }
}
