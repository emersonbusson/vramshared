// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/root_status_cache/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;

use super::*;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ramshared-root-status-cache-{}-{name}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

fn sample_view(published_at_unix_ms: u64) -> RootStatusView {
    RootStatusView {
        schema_version: SCHEMA_VERSION,
        published_at_unix_ms,
        selected_release: Some("v0.14.1-1437-g1234abcd".to_string()),
        daemon_instance_id: Some("42:1000".to_string()),
        release_coherence: "coherent".to_string(),
        tier_device: Some("nbd0".to_string()),
        posture: Some("writeback".to_string()),
        posture_reason: "token_binding".to_string(),
    }
}

#[test]
fn publish_and_read_verified_view_roundtrip_and_mode() {
    let uid = current_uid().unwrap();
    let dir = temp_dir("roundtrip");
    let path = dir.join("root-status-view.json");
    let view = sample_view(1_000_000);

    publish_root_status_view_at(&path, uid, &view).unwrap();
    let mode = fs::symlink_metadata(&path).unwrap().mode() & 0o777;
    assert_eq!(mode, 0o644);

    let read = read_verified_root_status_view_at(
        &path,
        uid,
        1_002_500,
        &view.selected_release,
        &view.daemon_instance_id,
    )
    .expect("fresh matching view must be admitted");
    assert_eq!(read, view);
    assert_eq!(
        parse_release_coherence(&read.release_coherence),
        Some(ReleaseCoherence::Coherent)
    );
    assert_eq!(
        parse_status_posture(read.posture.as_deref().unwrap()),
        Some(Posture::Writeback)
    );
    assert_eq!(parse_status_posture("lab_swap"), Some(Posture::Swap));
    assert_eq!(
        parse_release_coherence("stale"),
        Some(ReleaseCoherence::Stale)
    );
    assert_eq!(
        parse_release_coherence("unknown"),
        Some(ReleaseCoherence::Unknown)
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn reader_fails_closed_on_stale_future_release_or_daemon_mismatch() {
    let uid = current_uid().unwrap();
    let dir = temp_dir("freshness");
    let path = dir.join("root-status-view.json");
    let view = sample_view(1_000_000);
    publish_root_status_view_at(&path, uid, &view).unwrap();

    // Exact boundary passes; 1 ms past max age refuses.
    assert!(
        read_verified_root_status_view_at(
            &path,
            uid,
            1_000_000 + ROOT_STATUS_VIEW_MAX_AGE_MS,
            &view.selected_release,
            &view.daemon_instance_id,
        )
        .is_some()
    );
    assert!(
        read_verified_root_status_view_at(
            &path,
            uid,
            1_000_000 + ROOT_STATUS_VIEW_MAX_AGE_MS + 1,
            &view.selected_release,
            &view.daemon_instance_id,
        )
        .is_none()
    );
    // Future-dated beyond tolerance refuses.
    assert!(
        read_verified_root_status_view_at(
            &path,
            uid,
            1_000_000 - FUTURE_SKEW_TOLERANCE_MS - 1,
            &view.selected_release,
            &view.daemon_instance_id,
        )
        .is_none()
    );
    // Release selector switch refuses immediately.
    assert!(
        read_verified_root_status_view_at(
            &path,
            uid,
            1_000_500,
            &Some("v0.14.1-9999-gdeadbeef".to_string()),
            &view.daemon_instance_id,
        )
        .is_none()
    );
    // Daemon restart or exit refuses immediately.
    assert!(
        read_verified_root_status_view_at(
            &path,
            uid,
            1_000_500,
            &view.selected_release,
            &Some("99:2000".to_string()),
        )
        .is_none()
    );
    assert!(
        read_verified_root_status_view_at(&path, uid, 1_000_500, &view.selected_release, &None)
            .is_none()
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn reader_and_publisher_refuse_symlink_foreign_uid_writable_mode_and_bad_fields() {
    let uid = current_uid().unwrap();
    let dir = temp_dir("trust");
    let real_path = dir.join("real.json");
    let link_path = dir.join("link.json");
    let view = sample_view(1_000_000);
    publish_root_status_view_at(&real_path, uid, &view).unwrap();
    symlink(&real_path, &link_path).unwrap();

    // Symlink target is refused via O_NOFOLLOW.
    assert!(
        read_verified_root_status_view_at(
            &link_path,
            uid,
            1_000_500,
            &view.selected_release,
            &view.daemon_instance_id,
        )
        .is_none()
    );
    // Wrong UID refuses.
    assert!(
        read_verified_root_status_view_at(
            &real_path,
            uid.wrapping_add(1),
            1_000_500,
            &view.selected_release,
            &view.daemon_instance_id,
        )
        .is_none()
    );
    // Group/world writable file refuses.
    fs::set_permissions(&real_path, fs::Permissions::from_mode(0o664)).unwrap();
    assert!(
        read_verified_root_status_view_at(
            &real_path,
            uid,
            1_000_500,
            &view.selected_release,
            &view.daemon_instance_id,
        )
        .is_none()
    );
    // Oversize file refuses.
    fs::set_permissions(&real_path, fs::Permissions::from_mode(0o644)).unwrap();
    fs::write(
        &real_path,
        vec![b' '; (ROOT_STATUS_VIEW_MAX_BYTES + 16) as usize],
    )
    .unwrap();
    assert!(
        read_verified_root_status_view_at(
            &real_path,
            uid,
            1_000_500,
            &view.selected_release,
            &view.daemon_instance_id,
        )
        .is_none()
    );

    // Publisher rejects invalid fields, symlinked parent, wrong parent uid, and writable parent.
    let mut bad_schema = view.clone();
    bad_schema.schema_version = 2;
    assert!(publish_root_status_view_at(&real_path, uid, &bad_schema).is_err());

    let mut bad_ts = view.clone();
    bad_ts.published_at_unix_ms = 0;
    assert!(publish_root_status_view_at(&real_path, uid, &bad_ts).is_err());

    let mut bad_coherence = view.clone();
    bad_coherence.release_coherence = "healthy".to_string();
    assert!(publish_root_status_view_at(&real_path, uid, &bad_coherence).is_err());

    let mut bad_dev = view.clone();
    bad_dev.tier_device = Some("../nbd0".to_string());
    assert!(publish_root_status_view_at(&real_path, uid, &bad_dev).is_err());

    let mut bad_posture = view.clone();
    bad_posture.posture = Some("invalid".to_string());
    assert!(publish_root_status_view_at(&real_path, uid, &bad_posture).is_err());

    let mut bad_reason = view.clone();
    bad_reason.posture_reason = String::new();
    assert!(publish_root_status_view_at(&real_path, uid, &bad_reason).is_err());

    assert!(publish_root_status_view_at(&real_path, uid.wrapping_add(1), &view).is_err());

    let symlink_dir = dir.join("dir-link");
    symlink(&dir, &symlink_dir).unwrap();
    assert!(publish_root_status_view_at(&symlink_dir.join("out.json"), uid, &view).is_err());

    let writable_dir = dir.join("writable");
    fs::create_dir_all(&writable_dir).unwrap();
    fs::set_permissions(&writable_dir, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(publish_root_status_view_at(&writable_dir.join("out.json"), uid, &view).is_err());

    let mut oversize = view.clone();
    oversize.selected_release = Some("v".repeat(ROOT_STATUS_VIEW_MAX_BYTES as usize));
    assert!(publish_root_status_view_at(&real_path, uid, &oversize).is_err());

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn observation_projection_and_posture_binding_filter_honestly() {
    let dir = temp_dir("obs-proj");
    let swap_dev = dir.join("swap-dev");
    fs::write(&swap_dev, "/dev/nbd0\n").unwrap();

    let mut status = BTreeMap::new();
    status.insert("release_coherence".into(), Value::String("coherent".into()));
    status.insert("posture".into(), Value::String("writeback".into()));
    status.insert(
        "posture_reason".into(),
        Value::String("token_binding".into()),
    );

    assert!(view_from_observation(&status, 0, None, None, &swap_dev).is_none());

    let view = view_from_observation(
        &status,
        1_795_470_000_000,
        Some("v0.14.1-1437-g1234abcd".into()),
        Some("100:5000".into()),
        &swap_dev,
    )
    .expect("valid observation must project");
    assert_eq!(view.tier_device.as_deref(), Some("nbd0"));
    assert_eq!(
        posture_from_view(&view),
        Some(StatusPosture {
            posture: Some(Posture::Writeback),
            reason: "root_monitor_view",
        })
    );

    let mut unverified_posture = view.clone();
    unverified_posture.posture_reason = "token_missing".into();
    assert!(posture_from_view(&unverified_posture).is_none());

    let mut missing_posture = view.clone();
    missing_posture.posture = None;
    assert!(posture_from_view(&missing_posture).is_none());

    // Invalid coherence refuses projection; non-root publish_from_observation is a safe no-op.
    status.insert("release_coherence".into(), Value::String("bogus".into()));
    assert!(view_from_observation(&status, 1_795_470_000_000, None, None, &swap_dev).is_none());
    publish_from_observation(&status, 1_795_470_000_000);
    let _ = (
        verified_release_coherence(),
        verified_tier_device(),
        verified_posture(),
    );

    let _ = fs::remove_dir_all(&dir);
}
