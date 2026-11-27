// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/install_transaction/tests/pending_selection.rs

use super::*;
use crate::customer::package_source::DeclaredBundle;
use crate::customer::package_stage_journal::{inventory::stage_all_members, stage_journal_present};
use crate::customer::package_trust::verify_bundle_with_keyring;

#[test]
fn prepared_upgrade_does_not_complete_with_the_prior_release_selected() {
    let base = std::env::temp_dir().join(format!(
        "ramshared-pending-selection-{}",
        std::process::id()
    ));
    std::fs::create_dir(&base).unwrap();
    let _fixture = Fixture(base.clone());
    for name in ["etc", "sink", "systemd"] {
        std::fs::create_dir(base.join(name)).unwrap();
    }
    let env = test_env(&base);
    let home = base.join("gnupg");
    let keyring = base.join("etc/release-keyring.gpg");
    let _agent = create_signing_key(&home, &keyring);
    let prior_release = "v1.0.1";
    let candidate_release = "v1.0.2";
    let prior = env.default_bundle_root.join("prior/release");
    let candidate = env.default_bundle_root.join("candidate/release");
    let prior_hash = build_signed_bundle(&home, &prior, prior_release, b"prior");
    let candidate_hash = build_signed_bundle(&home, &candidate, candidate_release, b"candidate");
    let mut units = FakeUnitHost::inactive_all();
    let boot = FakeBootHost::all_disabled();
    let mut mem = live_test_memory;

    let mut err = Vec::new();
    assert_eq!(
        run_with(
            &[
                "--approve-nbd-product-install".into(),
                prior_release.into(),
                "--approve-manifest-sha256".into(),
                prior_hash.clone(),
                "--lower-sink".into(),
                base.join("sink").display().to_string(),
                "--posture".into(),
                "writeback".into(),
                "--bundle-root".into(),
                prior.display().to_string(),
                "--keyring".into(),
                keyring.display().to_string(),
            ],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut err,
        ),
        0,
        "{}",
        String::from_utf8_lossy(&err),
    );
    let reloads = units.reloads;
    let complete =
        |bundle: &Path, units: &mut FakeUnitHost, out: &mut Vec<u8>, err: &mut Vec<u8>| {
            out.clear();
            err.clear();
            run_with(
                &[
                    "--reconcile-complete".into(),
                    "--bundle-root".into(),
                    bundle.display().to_string(),
                    "--keyring".into(),
                    keyring.display().to_string(),
                ],
                &env,
                units,
                &boot,
                &mut live_test_memory,
                out,
                err,
            )
        };
    let pending = PackagePending {
        target_release: Some(candidate_release.into()),
        action: PackageAction::Upgrade,
        old_hash: prior_hash.clone(),
        new_hash: candidate_hash,
        issued_unix: 1,
    };
    let mut out = Vec::new();
    let legacy = PackagePending {
        target_release: None,
        ..pending.clone()
    };
    let guard = acquire_transition(OperationOwner::Package, &env.transition).unwrap();
    enroll_pending(&guard, &legacy).unwrap();
    drop(guard);
    assert_ne!(complete(&prior, &mut units, &mut out, &mut err), 0);
    assert!(String::from_utf8_lossy(&err).contains("RECONCILE_BUNDLE_INVALID"));
    assert_eq!(read_pending(&env.transition).unwrap(), Some(legacy.clone()));
    assert_ne!(complete(&candidate, &mut units, &mut out, &mut err), 0);
    assert!(String::from_utf8_lossy(&err).contains("RECONCILE_TARGET_MISSING"));
    assert_eq!(read_pending(&env.transition).unwrap(), Some(legacy.clone()));

    let guard = acquire_transition(OperationOwner::Package, &env.transition).unwrap();
    crate::customer::transition::clear_pending(&guard, &legacy).unwrap();
    enroll_pending(&guard, &pending).unwrap();
    drop(guard);
    let releases = env.releases_dir();
    let held_releases = env.product_root.join("fixture-held-releases");
    std::fs::rename(&releases, &held_releases).unwrap();
    assert_ne!(complete(&candidate, &mut units, &mut out, &mut err), 0);
    assert!(String::from_utf8_lossy(&err).contains("PRODUCT_ROOT_INVALID"));
    assert_eq!(
        read_pending(&env.transition).unwrap(),
        Some(pending.clone())
    );
    assert_eq!(units.reloads, reloads);
    assert!(!stage_journal_present(&env.transition).unwrap());
    std::fs::rename(&held_releases, &releases).unwrap();

    let signature = verify_bundle_with_keyring(&candidate, &keyring, env.owner_uid).unwrap();
    let bundle = DeclaredBundle::open_verified(&candidate, &signature).unwrap();
    let guard = acquire_transition(OperationOwner::Package, &env.transition).unwrap();
    std::fs::create_dir_all(env.stage_dir()).unwrap();
    stage_all_members(
        &guard,
        &pending,
        &bundle,
        &env.stage_dir(),
        INSTALL_RESERVE_BYTES,
    )
    .unwrap();
    drop(guard);

    assert_ne!(
        complete(&candidate, &mut units, &mut out, &mut err),
        0,
        "{}",
        String::from_utf8_lossy(&out),
    );
    assert!(
        String::from_utf8_lossy(&err)
            .contains("RECONCILE_COMPLETE_UNSUPPORTED_WITHOUT_SEALED_SELECTOR")
    );
    assert_eq!(units.reloads, reloads);
    assert_eq!(
        read_pending(&env.transition).unwrap(),
        Some(pending.clone())
    );
    assert!(stage_journal_present(&env.transition).unwrap());
    assert_eq!(
        observe_installed_input_bundle_digest(&env.selector_path()).as_deref(),
        Some(prior_hash.as_str()),
    );

    let mut rollback = |out: &mut Vec<u8>, err: &mut Vec<u8>| {
        out.clear();
        err.clear();
        run_with(
            &["--reconcile-rollback".into()],
            &env,
            &mut units,
            &boot,
            &mut mem,
            out,
            err,
        )
    };
    let owned = env.releases_dir().join(candidate_release);
    let held = env.releases_dir().join("retained-test-candidate");
    std::fs::rename(&owned, &held).unwrap();
    std::fs::create_dir(&owned).unwrap();
    let marker = owned.join("unrelated-output");
    std::fs::write(&marker, b"retain me").unwrap();
    assert_ne!(rollback(&mut out, &mut err), 0);
    assert!(String::from_utf8_lossy(&err).contains("tree directory identity or mount changed"));
    assert_eq!(std::fs::read(&marker).unwrap(), b"retain me");
    assert_eq!(read_pending(&env.transition).unwrap(), Some(pending));
    std::fs::remove_file(&marker).unwrap();
    std::fs::remove_dir(&owned).unwrap();
    std::fs::rename(&held, &owned).unwrap();

    assert_eq!(
        rollback(&mut out, &mut err),
        0,
        "{}",
        String::from_utf8_lossy(&err)
    );
    assert!(!owned.exists());
    assert!(String::from_utf8_lossy(&out).contains("RECONCILED_ROLLBACK"));
    assert!(read_pending(&env.transition).unwrap().is_none());
    assert_eq!(
        read_release_version(&env.selector_path()).as_deref(),
        Some(prior_release),
    );
}

#[test]
fn rollback_preserves_a_release_directory_without_transaction_custody() {
    let base =
        std::env::temp_dir().join(format!("ramshared-foreign-release-{}", std::process::id()));
    std::fs::create_dir(&base).unwrap();
    let _fixture = Fixture(base.clone());
    let env = test_env(&base);
    let release = "v1.0.2";
    let unrelated = env.releases_dir().join(release);
    std::fs::create_dir_all(&unrelated).unwrap();
    let retained = unrelated.join("retained-build-output");
    std::fs::write(&retained, b"existing output").unwrap();
    let pending = PackagePending {
        target_release: Some(release.into()),
        action: PackageAction::Upgrade,
        old_hash: "a".repeat(64),
        new_hash: "b".repeat(64),
        issued_unix: 1,
    };
    let guard = acquire_transition(OperationOwner::Package, &env.transition).unwrap();
    enroll_pending(&guard, &pending).unwrap();
    drop(guard);

    let mut out = Vec::new();
    let mut err = Vec::new();
    let result = run_with(
        &["--reconcile-rollback".into()],
        &env,
        &mut FakeUnitHost::inactive_all(),
        &FakeBootHost::all_disabled(),
        &mut live_test_memory,
        &mut out,
        &mut err,
    );
    assert_ne!(result, 0, "{}", String::from_utf8_lossy(&out));
    assert!(
        String::from_utf8_lossy(&err).contains("release directory has no durable rollback custody")
    );
    assert_eq!(std::fs::read(&retained).unwrap(), b"existing output");
    assert_eq!(read_pending(&env.transition).unwrap(), Some(pending));
}
