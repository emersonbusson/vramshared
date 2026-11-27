// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/setup/cache_mode/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;

use super::*;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ramshared-cli-cache-mode-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn binding(dir: &Path, uid: u32) -> StageBinding {
    StageBinding {
        effective_uid: uid,
        target_path: dir.join("cache-mode.json"),
        active_release: "v1.0.4".into(),
        expected_release: "v1.0.4".into(),
        active_origin: "5039dca1-61a0-41ff-aa08-221f49326a1b".into(),
        expected_origin: "5039dca1-61a0-41ff-aa08-221f49326a1b".into(),
        role: "wsl2".into(),
        key_ref: "/etc/ramshared/pooled.key".into(),
    }
}

#[test]
fn stage_refuses_nonroot_stale_binding_and_invalid_args() {
    for bad in [
        vec![],
        vec!["apply".into()],
        vec!["stage".into()],
        vec!["stage".into(), "--mode".into(), "hybrid".into()],
        vec!["stage".into(), "--unknown".into()],
        vec![
            "stage".into(),
            "--mode".into(),
            "pooled".into(),
            "--mode".into(),
            "independent".into(),
        ],
    ] {
        let e = parse_stage_args(&bad).unwrap_err();
        assert_eq!(e.reason, CustomerReason::UnsupportedSurface);
    }
    assert_eq!(
        parse_stage_args(&[
            "stage".into(),
            "--mode".into(),
            "pooled".into(),
            "--json".into()
        ])
        .unwrap(),
        (CacheMode::Pooled, true)
    );
    assert_eq!(
        parse_stage_args(&["stage".into(), "--mode".into(), "independent".into()]).unwrap(),
        (CacheMode::Independent, false)
    );

    let dir = temp_dir("refusals");
    let nonroot = binding(&dir, 1000);
    let err = stage_with_binding(CacheMode::Pooled, &nonroot).unwrap_err();
    assert_eq!(err.reason, CustomerReason::AuthorizationRequired);
    assert!(!nonroot.target_path.exists());

    let mut stale = binding(&dir, 0);
    stale.active_release = "v1.0.3".into();
    let err = stage_with_binding(CacheMode::Pooled, &stale).unwrap_err();
    assert_eq!(err.reason, CustomerReason::IdentityChanged);
    assert!(!stale.target_path.exists());

    let mut out = Vec::new();
    let mut err_buf = Vec::new();
    assert_eq!(run(&["bad".into()], &mut out, &mut err_buf), 2);
    assert!(out.is_empty());

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn stage_reconciles_torn_temp_and_is_idempotent_across_boots() {
    let dir = temp_dir("reconcile");
    let bind = binding(&dir, 0);

    // Seed a torn temp file left by a prior interrupted commit.
    let torn = dir.join(".cache-mode.json.tmp.99999");
    fs::write(&torn, b"{truncated").unwrap();

    let first = stage_with_binding(CacheMode::Pooled, &bind).unwrap();
    assert_eq!(first.next_boot_profile, CacheMode::Pooled);
    assert_eq!(first.generation, 1);
    assert_eq!(first.reconciled_torn_temps, 1);
    assert!(!first.storage_touched);
    assert!(!first.worker_started);
    assert!(!torn.exists());
    assert!(bind.target_path.is_file());

    // Duplicate staging returns identical generation idempotently (2x = 1x).
    let dup = stage_with_binding(CacheMode::Pooled, &bind).unwrap();
    assert_eq!(dup.generation, 1);
    assert_eq!(dup.reconciled_torn_temps, 0);

    // Changing to Independent increments generation monotonically.
    let back = stage_with_binding(CacheMode::Independent, &bind).unwrap();
    assert_eq!(back.next_boot_profile, CacheMode::Independent);
    assert_eq!(back.generation, 2);
    assert_eq!(back.next_boot_profile.as_str(), "independent");
    assert_eq!(CacheMode::Pooled.as_str(), "pooled");

    // Corrupt persisted file fails closed to RecoveryRequired without overwriting.
    fs::write(&bind.target_path, b"not-json").unwrap();
    let corrupt = stage_with_binding(CacheMode::Pooled, &bind).unwrap_err();
    assert_eq!(corrupt.reason, CustomerReason::RecoveryRequired);

    // Formatting and CLI helper paths: run_with_binding (JSON + text + error), read_origin_binding,
    // reconcile_torn_temps error branches, and commit/read IO refusals.
    let dir2 = temp_dir("format-and-io");
    let bind2 = binding(&dir2, 0);
    let mut out = Vec::new();
    let mut err_buf = Vec::new();
    let args_json = [
        "stage".to_string(),
        "--mode".to_string(),
        "pooled".to_string(),
        "--json".to_string(),
    ];
    assert_eq!(
        run_with_binding(&args_json, &bind2, &mut out, &mut err_buf),
        0
    );
    assert!(String::from_utf8(out).unwrap().contains("\"pooled\""));
    let mut text_out = Vec::new();
    let args_text = [
        "stage".to_string(),
        "--mode".to_string(),
        "independent".to_string(),
    ];
    assert_eq!(
        run_with_binding(&args_text, &bind2, &mut text_out, &mut err_buf),
        0
    );
    assert!(
        String::from_utf8(text_out)
            .unwrap()
            .contains("mode=independent generation=2")
    );
    let nonroot_bind = binding(&dir2, 1000);
    let mut err_out = Vec::new();
    assert_eq!(
        run_with_binding(&args_text, &nonroot_bind, &mut Vec::new(), &mut err_out),
        3
    );

    let origin_conf = dir2.join("origin.conf");
    let native_toml = dir2.join("native-origin.toml");
    fs::write(&origin_conf, "# comment\nORIGIN_PARTUUID=\"abc-123\"\n").unwrap();
    assert_eq!(
        read_origin_binding_and_role(&origin_conf, &native_toml),
        ("abc-123".to_string(), "wsl2".to_string())
    );
    let mut target = ramshared_config::resource_profile::ResourceTarget::LinuxFileOrigin {
        filesystem_uuid: "fs-native-456".into(),
        device_identity: "wwn:0x1234".into(),
        managed_relative_path: "origin/data.img".into(),
        inode: 77,
        allocated_bytes: 8192,
        identity_field_hash: String::new(),
    };
    let hash = ramshared_config::native_origin::identity_hash(&target).unwrap();
    if let ramshared_config::resource_profile::ResourceTarget::LinuxFileOrigin {
        identity_field_hash,
        ..
    } = &mut target
    {
        *identity_field_hash = hash;
    }
    let manifest = ramshared_config::native_origin::NativeOriginManifest::new(
        "a".repeat(64),
        "approval-1".into(),
        "b".repeat(64),
        "1.0.1".into(),
        target,
    )
    .unwrap();
    fs::write(&native_toml, manifest.encode().unwrap()).unwrap();
    assert_eq!(
        read_origin_binding_and_role(&dir2.join("absent.conf"), &native_toml),
        ("fs-native-456".to_string(), "secondary_guest".to_string())
    );

    assert_eq!(
        reconcile_torn_temps(Path::new("/")).unwrap_err().reason,
        CustomerReason::UnsupportedSurface
    );
    assert_eq!(
        reconcile_torn_temps(&dir2.join("..")).unwrap_err().reason,
        CustomerReason::UnsupportedSurface
    );
    assert_eq!(
        reconcile_torn_temps(&dir2.join("missing-sub/cache-mode.json"))
            .unwrap_err()
            .reason,
        CustomerReason::RecoveryRequired
    );

    // Target is a directory -> read fails cleanly to RecoveryRequired.
    let mut dir_target = binding(&dir2, 0);
    dir_target.target_path = dir2.join("target-as-dir");
    fs::create_dir_all(&dir_target.target_path).unwrap();
    assert_eq!(
        stage_with_binding(CacheMode::Pooled, &dir_target)
            .unwrap_err()
            .reason,
        CustomerReason::RecoveryRequired
    );
    // Direct commit_atomic refusal when parent directory is missing or path has no parent/filename.
    let doc = StagedCacheMode {
        schema_version: SCHEMA_VERSION,
        profile: CacheMode::Pooled,
        role: "wsl2".into(),
        endpoint_version: CACHE_WIRE_VERSION,
        data_port: DEFAULT_POOLED_CACHE_PORT,
        key_ref: DEFAULT_KEY_REF.into(),
        release_binding: "v1.0.4".into(),
        origin_binding: "orig".into(),
        generation: 1,
    };
    assert_eq!(
        commit_atomic(Path::new("/"), &doc).unwrap_err().reason,
        CustomerReason::UnsupportedSurface
    );
    assert_eq!(
        commit_atomic(&dir2.join(".."), &doc).unwrap_err().reason,
        CustomerReason::UnsupportedSurface
    );
    assert_eq!(
        commit_atomic(&dir2.join("no-such-dir/cache-mode.json"), &doc)
            .unwrap_err()
            .reason,
        CustomerReason::RecoveryRequired
    );
    // Rename into an existing directory triggers commit failure & cleans temp file.
    assert_eq!(
        commit_atomic(&dir_target.target_path, &doc)
            .unwrap_err()
            .reason,
        CustomerReason::RecoveryRequired
    );

    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&dir2);
}
