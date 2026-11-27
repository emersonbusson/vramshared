// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/install_transaction/tests/committed_runtime.rs

use super::*;
use crate::cascade::boot_policy_effect::{HOST_GATE_UNIT, SUPERVISOR_UNIT};

#[test]
fn committed_drain_replays_runtime_without_reinstalling() {
    use crate::customer::install_lifecycle::LiveRuntimeState;

    let base =
        std::env::temp_dir().join(format!("ramshared-runtime-recovery-{}", std::process::id()));
    std::fs::create_dir(&base).unwrap();
    let _fixture = Fixture(base.clone());
    for name in ["etc", "sink", "systemd"] {
        std::fs::create_dir(base.join(name)).unwrap();
    }
    let env = test_env(&base);
    let home = base.join("gnupg");
    let keyring = base.join("etc/release-keyring.gpg");
    let _agent = create_signing_key(&home, &keyring);
    let sink = base.join("sink").to_string_lossy().into_owned();
    std::fs::write(
        &env.default_policy,
        format!(
            "enabled=1\nlower_sink={sink}\nbundle_root={}\nrelease_keyring={}\n",
            env.default_bundle_root.display(),
            keyring.display(),
        ),
    )
    .unwrap();

    let initial = env.default_bundle_root.join("first/release");
    let candidate = env.default_bundle_root.join("second/release");
    let newer = env.default_bundle_root.join("third/release");
    let initial_version = "v0.15.0-30-gaaaa1111";
    let version = "v0.15.0-31-gbbbb2222";
    let sha1 = build_signed_bundle(&home, &initial, initial_version, b"prior");
    let sha2 = build_signed_bundle(&home, &candidate, version, b"selected");
    let install_args = |bundle: &Path, version: &str, manifest: String| {
        vec![
            "--approve-nbd-product-install".into(),
            version.into(),
            "--approve-manifest-sha256".into(),
            manifest,
            "--lower-sink".into(),
            sink.clone(),
            "--posture".into(),
            "writeback".into(),
            "--bundle-root".into(),
            bundle.display().to_string(),
        ]
    };

    let shared = std::rc::Rc::new(std::cell::RefCell::new(SharedSystemdState::default()));
    let mut units = StatefulUnitHost {
        state: shared.clone(),
    };
    let boot = StatefulBootHost {
        state: shared.clone(),
    };
    let mut lifecycle = StatefulLifecycleHost {
        state: shared.clone(),
    };
    let mut mem = || live_test_memory();
    let mut err = Vec::new();
    assert_eq!(
        run_with_lifecycle(
            &install_args(&initial, initial_version, sha1),
            &env,
            &mut units,
            &boot,
            &mut lifecycle,
            &mut mem,
            &mut Vec::new(),
            &mut err,
        ),
        0,
        "{}",
        String::from_utf8_lossy(&err)
    );

    let mut args = install_args(&candidate, version, sha2);
    args.push("--enroll-default".into());
    let req = parse_install_args(&args).unwrap();
    let mut live_env = test_env(&base);
    live_env.require_root = true;
    let mut refused_units = FakeUnitHost::inactive_all();
    let blocked = fs_error(
        CustomerReason::ObservationUnavailable,
        "fixture workloads slice observation failed",
        CustomerState::UnknownBlocked,
    );
    refused_units.statuses.insert(
        crate::customer::install_units::WORKLOADS_SLICE_UNIT.into(),
        Err(blocked.clone()),
    );
    let refusal = verify_install(&req, &live_env, &refused_units, None)
        .err()
        .unwrap();
    assert_eq!(refusal.detail, blocked.detail);
    assert!(read_pending(&env.transition).unwrap().is_none());
    assert!(!env.releases_dir().join(version).exists());

    let (verified, inputs) = verify_install(&req, &env, &units, None).unwrap();
    let lock = InstallerLock::acquire(&env.transition).unwrap();
    lock.fixture_record_drained(
        inputs.unwrap(),
        LiveRuntimeState {
            cascade_active: true,
            active_managed_swap: true,
            ..Default::default()
        },
    )
    .unwrap();
    execute_verified_apply(
        &req,
        &env,
        verified,
        Some(&lock),
        &mut units,
        &boot,
        &mut mem,
        &mut Vec::new(),
    )
    .unwrap();
    let (inputs, witness, _) = lock.committed_runtime().unwrap().unwrap();
    assert!(witness.enrolled);
    let prior_signature = crate::customer::package_trust::verify_bundle_from_policy(
        &initial,
        &env.default_policy,
        env.owner_uid,
    )
    .unwrap();
    let refusal = crate::customer::install_apply::verify_committed_request(
        &req,
        &env,
        &inputs,
        Some(&prior_signature),
    )
    .unwrap_err();
    assert!(
        refusal
            .detail
            .contains("committed upgrade signature no longer matches")
    );
    assert!(read_pending(&env.transition).unwrap().is_none());
    drop(lock);
    let selected = std::fs::metadata(env.selector_path()).unwrap();
    shared.borrow_mut().retain_drain_custody = true;

    let mut changed_activation = args.clone();
    changed_activation.push("--no-activate".into());
    let with_sink = |path: &Path| {
        let mut changed = args.clone();
        let value = changed
            .iter()
            .position(|arg| arg == "--lower-sink")
            .unwrap()
            + 1;
        changed[value] = path.display().to_string();
        changed
    };
    let different_sink = base.join("different-sink");
    std::fs::create_dir(&different_sink).unwrap();
    let reconcile = |restriction: &str| {
        vec![
            "--reconcile-complete".into(),
            "--bundle-root".into(),
            candidate.display().to_string(),
            restriction.into(),
        ]
    };
    for (changed, expected) in [
        (
            reconcile("--no-activate"),
            "recovery arguments differ from the drained package",
        ),
        (
            reconcile("--no-enroll"),
            "recovery arguments differ from the drained package",
        ),
        (
            changed_activation,
            "committed upgrade belongs to a different request",
        ),
        (
            with_sink(&base.join("absent-sink")),
            "cannot verify committed upgrade sink",
        ),
        (
            with_sink(&different_sink),
            "committed upgrade sink identity changed",
        ),
    ] {
        shared.borrow_mut().events.clear();
        err.clear();
        assert_ne!(
            run_with_lifecycle(
                &changed,
                &env,
                &mut units,
                &boot,
                &mut lifecycle,
                &mut mem,
                &mut Vec::new(),
                &mut err,
            ),
            0,
        );
        assert!(String::from_utf8_lossy(&err).contains(expected));
        assert!(shared.borrow().events.is_empty());
        assert_eq!(
            std::fs::metadata(env.selector_path()).unwrap().ino(),
            selected.ino()
        );
        assert!(
            InstallerLock::acquire(&env.transition)
                .unwrap()
                .committed_runtime()
                .unwrap()
                .is_some()
        );
    }

    let unit_path = env.systemd_dir.join(CASCADE_UNIT);
    let gate_unit_path = env.systemd_dir.join(HOST_GATE_UNIT);
    let supervisor_unit_path = env.systemd_dir.join(SUPERVISOR_UNIT);
    let approval_path = env.approval_dir.join(format!("activate-{version}.token"));
    let sealed_hash = env
        .releases_dir()
        .join(version)
        .join("INSTALLED_MANIFEST_SHA256");
    // Each absent or substituted input must leave runtime and receipts untouched.
    for path in [
        &unit_path,
        &gate_unit_path,
        &supervisor_unit_path,
        &approval_path,
        &sealed_hash,
    ] {
        let held = path.with_extension("fixture-held");
        let was_sealed = path == &sealed_hash;
        if was_sealed {
            std::fs::set_permissions(
                path.parent().unwrap(),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        std::fs::rename(path, &held).unwrap();
        shared.borrow_mut().events.clear();
        err.clear();
        assert_ne!(
            run_with_lifecycle(
                &args,
                &env,
                &mut units,
                &boot,
                &mut lifecycle,
                &mut mem,
                &mut Vec::new(),
                &mut err,
            ),
            0
        );
        assert!(shared.borrow().events.is_empty());
        std::fs::rename(&held, path).unwrap();
        if was_sealed {
            std::fs::set_permissions(
                path.parent().unwrap(),
                std::fs::Permissions::from_mode(0o555),
            )
            .unwrap();
        }
    }

    let acknowledgment = env
        .approval_dir
        .join(format!("wsl2-nbd-ack-{version}.token"));
    let held_acknowledgment = acknowledgment.with_extension("fixture-held");
    std::fs::rename(&acknowledgment, &held_acknowledgment).unwrap();

    let mut replay_error = || {
        shared.borrow_mut().events.clear();
        let mut error = Vec::new();
        assert_eq!(
            run_with_lifecycle(
                &args,
                &env,
                &mut units,
                &boot,
                &mut lifecycle,
                &mut mem,
                &mut Vec::new(),
                &mut error,
            ),
            5
        );
        assert!(shared.borrow().events.is_empty());
        assert_eq!(
            std::fs::metadata(env.selector_path()).unwrap().ino(),
            selected.ino()
        );
        let lock = InstallerLock::acquire(&env.transition).unwrap();
        assert_eq!(lock.committed_runtime().unwrap().unwrap().1, witness);
        String::from_utf8(error).unwrap()
    };

    assert!(replay_error().contains("committed enrollment acknowledgment is missing"));
    std::fs::write(&acknowledgment, b"accept-wsl2-nbd:v0.15.0-99-gffff0000\n").unwrap();
    assert!(replay_error().contains("committed enrollment acknowledgment changed"));
    std::fs::rename(&held_acknowledgment, &acknowledgment).unwrap();

    shared
        .borrow_mut()
        .enabled
        .insert(CASCADE_UNIT.into(), false);
    assert!(replay_error().contains("boot enrollment changed after package commit"));
    shared
        .borrow_mut()
        .enabled
        .insert(CASCADE_UNIT.into(), true);

    let held_bundle = base.join("temporarily-unavailable-bundle");
    std::fs::rename(&candidate, &held_bundle).unwrap();
    err.clear();
    shared.borrow_mut().events.clear();
    let absent = run_with_lifecycle(
        &reconcile("--enroll-default"),
        &env,
        &mut units,
        &boot,
        &mut lifecycle,
        &mut mem,
        &mut Vec::new(),
        &mut err,
    );
    std::fs::rename(&held_bundle, &candidate).unwrap();
    assert_eq!(absent, 5);
    assert!(String::from_utf8_lossy(&err).contains("original signed package is unavailable"));
    assert!(shared.borrow().events.is_empty());
    assert_eq!(
        std::fs::metadata(env.selector_path()).unwrap().ino(),
        selected.ino()
    );
    let lock = InstallerLock::acquire(&env.transition).unwrap();
    assert_eq!(lock.committed_runtime().unwrap().unwrap().1, witness);
    drop(lock);

    shared.borrow_mut().fail_start_on = Some(CASCADE_UNIT);
    let mut out = Vec::new();
    assert_eq!(
        run_with_lifecycle(
            &reconcile("--enroll-default"),
            &env,
            &mut units,
            &boot,
            &mut lifecycle,
            &mut mem,
            &mut out,
            &mut err,
        ),
        0
    );
    assert!(String::from_utf8_lossy(&out).contains("ACTIVATION=REFUSED"));
    assert!(
        InstallerLock::acquire(&env.transition)
            .unwrap()
            .committed_runtime()
            .unwrap()
            .is_some()
    );
    build_signed_bundle(&home, &newer, "v0.15.0-32-gcccc3333", b"next");
    assert_eq!(
        find_newest_bundle(&env.default_bundle_root).unwrap(),
        Some(newer)
    );
    // The selector effect journal can already be gone while the final pending
    // record still needs cleanup. The timer must follow the commit witness.
    let (inputs, _, _) = InstallerLock::acquire(&env.transition)
        .unwrap()
        .committed_runtime()
        .unwrap()
        .unwrap();
    let guard = acquire_transition(OperationOwner::Package, &env.transition).unwrap();
    enroll_pending(
        &guard,
        &PackagePending {
            target_release: Some(inputs.candidate_release),
            action: PackageAction::Upgrade,
            old_hash: inputs.previous_manifest,
            new_hash: inputs.candidate_manifest,
            issued_unix: 1,
        },
    )
    .unwrap();
    drop(guard);

    shared.borrow_mut().fail_start_on = None;
    shared.borrow_mut().events.clear();
    out.clear();
    err.clear();
    assert_eq!(
        run_with_lifecycle(
            &["--auto-update".into(), "--json".into()],
            &env,
            &mut units,
            &boot,
            &mut lifecycle,
            &mut mem,
            &mut out,
            &mut err,
        ),
        0,
        "{}",
        String::from_utf8_lossy(&err)
    );
    let document: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(document["activation"]["state"], "active");
    assert!(read_pending(&env.transition).unwrap().is_none());
    assert!(shared.borrow().active_managed_swap);
    assert!(
        shared
            .borrow()
            .events
            .iter()
            .all(|event| event.starts_with("start:"))
    );
    assert_eq!(
        read_release_version(&env.selector_path()).as_deref(),
        Some(version)
    );
    assert_eq!(
        std::fs::metadata(env.selector_path()).unwrap().ino(),
        selected.ino()
    );
    assert!(
        InstallerLock::acquire(&env.transition)
            .unwrap()
            .active_inputs()
            .unwrap()
            .is_none()
    );
}
