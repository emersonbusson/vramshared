// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/native_origin/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::native_fixture::Fixture;
use ramshared_block::OriginStorage;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

#[test]
fn native_cache_generation_refusal_retains_verified_origin_bytes() {
    let fixture = Fixture::new();
    let text = fixture.manifest.encode().unwrap();
    let request = NativeOriginRequest {
        manifest: fixture.root.join("receipt.toml"),
        expected_bytes: None,
    };
    let mut admitted = open_with(
        &request,
        fixture.uid,
        || Ok(text.as_bytes().to_vec()),
        |_| Ok(fixture.devices.clone()),
    )
    .unwrap();
    attach_cache_configuration(&mut admitted, |_| {
        Err("held or unsafe cache generation".into())
    });
    let session = admitted.cache_session.as_ref().unwrap();
    assert!(
        ramshared_block::CacheAdmission::admit_cache(&crate::native_cache::NativeCacheAdmission(
            session.clone()
        ))
        .is_err()
    );
    assert_eq!(session.generation(), None);
    let mut bytes = [0; 4096];
    admitted.file.read_exact_at(0, &mut bytes).unwrap();
    assert_eq!(bytes, [17; 4096]);
    admitted.file.write_all_at(4096, &[31; 4096]).unwrap();
    admitted.file.sync_data().unwrap();
    assert_eq!(
        &fs::read(fixture.origin_path()).unwrap()[4096..],
        &[31; 4096]
    );
}

#[test]
fn native_origin_manifest_binds_open_fd_identity() {
    let fixture = Fixture::new();
    let text = fixture.manifest.encode().unwrap();
    let request = NativeOriginRequest {
        manifest: fixture.root.join("receipt.toml"),
        expected_bytes: None,
    };
    let admitted = open_with(
        &request,
        fixture.uid,
        || Ok(text.as_bytes().to_vec()),
        |_| Ok(fixture.devices.clone()),
    )
    .unwrap();
    assert_eq!(admitted.logical_bytes, 8192);
    let mut bytes = [0; 4096];
    admitted.file.read_exact_at(0, &mut bytes).unwrap();
    assert_eq!(bytes, [17; 4096]);
    admitted.file.write_all_at(4096, &[29; 4096]).unwrap();
    admitted.file.sync_data().unwrap();
    assert_eq!(
        &fs::read(fixture.origin_path()).unwrap()[4096..],
        &[29; 4096]
    );
    assert_eq!(fs::metadata(fixture.origin_path()).unwrap().len(), 8192);
    assert!(
        admit(&fixture.manifest, &fixture.devices, None, fixture.uid)
            .err()
            .unwrap()
            .contains("I/O owner")
    );
    drop(admitted);
    assert!(admit(&fixture.manifest, &fixture.devices, Some(8192), fixture.uid).is_ok());
}

#[test]
fn native_origin_manifest_rejects_path_mount_and_inode_drift() {
    let fixture = Fixture::new();
    let original = fs::read(fixture.origin_path()).unwrap();
    for expected in [Some(0), Some(4096), Some(16384)] {
        assert!(admit(&fixture.manifest, &fixture.devices, expected, fixture.uid).is_err());
    }
    let mut changed = fixture.devices.clone();
    changed[0].mounts[0].mount_id += 1;
    assert!(admit(&fixture.manifest, &changed, None, fixture.uid).is_err());
    changed = fixture.devices.clone();
    changed[0].read_only = Some(true);
    assert!(admit(&fixture.manifest, &changed, None, fixture.uid).is_err());
    changed = fixture.devices.clone();
    changed.push(changed[0].clone());
    assert!(admit(&fixture.manifest, &changed, None, fixture.uid).is_err());
    assert!(admit(&fixture.manifest, &[], None, fixture.uid).is_err());
    let moved = fixture.root.join("retained.img");
    fs::rename(fixture.origin_path(), &moved).unwrap();
    fs::write(fixture.origin_path(), &original).unwrap();
    fs::set_permissions(fixture.origin_path(), fs::Permissions::from_mode(0o600)).unwrap();
    assert_ne!(
        fs::metadata(&moved).unwrap().ino(),
        fs::metadata(fixture.origin_path()).unwrap().ino()
    );
    assert!(admit(&fixture.manifest, &fixture.devices, None, fixture.uid).is_err());
    assert_eq!(fs::read(&moved).unwrap(), original);
}

#[test]
fn native_origin_open_refuses_observation_and_manifest_changes_and_retains_data() {
    let fixture = Fixture::new();
    let text = fixture.manifest.encode().unwrap();
    let request = NativeOriginRequest {
        manifest: fixture.root.join("receipt.toml"),
        expected_bytes: None,
    };
    let mut calls = 0;
    assert!(
        open_with(
            &request,
            fixture.uid,
            || Ok(text.as_bytes().to_vec()),
            |_| {
                calls += 1;
                let mut devices = fixture.devices.clone();
                if calls == 2 {
                    devices[0].mounts[0].mountpoint.push_str("-changed");
                }
                Ok(devices)
            }
        )
        .is_err()
    );
    let mut reads = 0;
    assert!(
        open_with(
            &request,
            fixture.uid,
            || {
                reads += 1;
                let mut bytes = text.as_bytes().to_vec();
                if reads == 2 {
                    bytes.push(b'\n');
                }
                Ok(bytes)
            },
            |_| Ok(fixture.devices.clone())
        )
        .is_err()
    );
    assert!(
        open_with(
            &request,
            fixture.uid,
            || Err("unreadable".into()),
            |_| panic!("reader failed first")
        )
        .is_err()
    );
    assert!(
        open_with(
            &request,
            fixture.uid,
            || Ok(vec![0xff]),
            |_| panic!("invalid text")
        )
        .is_err()
    );
    assert!(
        open_with(
            &request,
            fixture.uid,
            || Ok(b"bad=true".to_vec()),
            |_| panic!("invalid manifest")
        )
        .is_err()
    );
    assert!(
        open_with(
            &request,
            fixture.uid,
            || Ok(text.as_bytes().to_vec()),
            |_| Err("missing inventory".into())
        )
        .is_err()
    );
    assert_eq!(fs::read(fixture.origin_path()).unwrap(), [17; 8192]);
    assert!(
        admit(&fixture.manifest, &fixture.devices, None, fixture.uid).is_ok(),
        "failed admission released only its own lease"
    );
}

#[test]
fn native_origin_requests_and_surface_refuse_ambiguity_before_file_access() {
    let path = default_native_origin_path();
    let path = path.to_str().unwrap();
    assert!(NativeOriginRequest::new(path, None).is_ok());
    assert!(NativeOriginRequest::new(path, Some(0)).is_err());
    assert!(NativeOriginRequest::new("/tmp/caller.toml", None).is_err());
    assert!(native_surface(0, Some(PlatformKind::NativeLinux)).is_ok());
    for (uid, kind) in [
        (1, Some(PlatformKind::NativeLinux)),
        (0, None),
        (0, Some(PlatformKind::Wsl1)),
        (0, Some(PlatformKind::Wsl2)),
    ] {
        assert!(native_surface(uid, kind).is_err());
    }
}

#[test]
fn native_origin_volume_observation_joins_identity_without_capacity_queries() {
    let fixture = Fixture::new();
    let raw=serde_json::json!({"blockdevices":[{"name":"fixture","path":"/dev/fixture","type":"disk","wwn":"fixture-disk","uuid":"fixture-fs","maj:min":fixture.devices[0].major_minor,"ro":false,"rm":false,"tran":"virtio"}]}).to_string();
    let devices =
        volume_from_tables(&fixture.manifest, &raw, fixture.devices[0].mounts.clone()).unwrap();
    assert_eq!(devices[0].mounts[0].total_bytes, None);
    assert!(admit(&fixture.manifest, &devices, None, fixture.uid).is_ok());
    assert!(volume_from_tables(&fixture.manifest, "bad", vec![]).is_err());
    assert!(volume_from_tables(&fixture.manifest, r#"{"blockdevices":[]}"#, vec![]).is_err());
    let mut duplicate: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let copy = duplicate["blockdevices"][0].clone();
    duplicate["blockdevices"].as_array_mut().unwrap().push(copy);
    assert!(volume_from_tables(&fixture.manifest, &duplicate.to_string(), vec![]).is_err());
}
