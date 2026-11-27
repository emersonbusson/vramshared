// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/install_transaction/tests/rollback_interruption.rs

use super::*;
use crate::customer::package_source::DeclaredBundle;
use crate::customer::package_stage_journal::inventory::stage_all_members;
use crate::customer::package_tree::{continue_tree, tree_journal_present};
use crate::customer::package_trust::verify_bundle_with_keyring;
use crate::customer::transition::recovery_refusal;

#[test]
fn rollback_recovers_a_root_sealed_before_its_confirmation() {
    let base =
        std::env::temp_dir().join(format!("ramshared-rollback-intent-{}", std::process::id()));
    std::fs::create_dir(&base).unwrap();
    let _fixture = Fixture(base.clone());
    std::fs::create_dir(base.join("etc")).unwrap();
    let env = test_env(&base);
    let home = base.join("gnupg");
    let keyring = base.join("etc/release-keyring.gpg");
    let _agent = create_signing_key(&home, &keyring);
    let release = "v1.0.2";
    let candidate = env.default_bundle_root.join("pending/release");
    let candidate_hash = build_signed_bundle(&home, &candidate, release, b"candidate");
    let signature = verify_bundle_with_keyring(&candidate, &keyring, env.owner_uid).unwrap();
    let bundle = DeclaredBundle::open_verified(&candidate, &signature).unwrap();
    let pending = PackagePending {
        target_release: Some(release.into()),
        action: PackageAction::Install,
        old_hash: "a".repeat(64),
        new_hash: candidate_hash,
        issued_unix: 1,
    };
    let guard = acquire_transition(OperationOwner::Package, &env.transition).unwrap();
    enroll_pending(&guard, &pending).unwrap();
    std::fs::create_dir_all(env.stage_dir()).unwrap();
    stage_all_members(
        &guard,
        &pending,
        &bundle,
        &env.stage_dir(),
        INSTALL_RESERVE_BYTES,
    )
    .unwrap();
    let parent = env.releases_dir();
    std::fs::create_dir_all(&parent).unwrap();
    let meta = std::fs::metadata(&parent).unwrap();
    let result = continue_tree(
        &guard,
        &pending,
        &bundle,
        (&parent, (meta.dev(), meta.ino()), INSTALL_RESERVE_BYTES),
        release,
        false,
        &mut |relative| {
            assert!(relative.as_os_str().is_empty());
            Err(recovery_refusal("interrupted before root confirmation"))
        },
    );
    assert!(result.is_err());
    let root = parent.join(release);
    assert!(root.is_dir());
    let receipt = env.transition.transactions_root().join("package-tree.json");
    let recorded = std::fs::read_to_string(&receipt).unwrap();
    assert_eq!(recorded.lines().count(), 1);
    let intent: serde_json::Value = serde_json::from_str(&recorded).unwrap();
    assert_eq!(intent["phase"], "intent");
    let stale = PackagePending {
        issued_unix: pending.issued_unix + 1,
        ..pending.clone()
    };
    let error =
        crate::customer::package_tree::rollback::observe_owned_root(&guard, &stale, &parent)
            .err()
            .unwrap();
    assert!(error.detail.contains("rollback tree transaction changed"));
    drop(guard);

    let mut units = FakeUnitHost::inactive_all();
    let boot = FakeBootHost::all_disabled();
    let mut rollback = |out: &mut Vec<u8>, err: &mut Vec<u8>| {
        out.clear();
        err.clear();
        run_with(
            &["--reconcile-rollback".into()],
            &env,
            &mut units,
            &boot,
            &mut live_test_memory,
            out,
            err,
        )
    };
    let mut out = Vec::new();
    let mut err = Vec::new();
    let marker = root.join("unrelated-output");
    std::fs::write(&marker, b"retain me").unwrap();
    assert_ne!(rollback(&mut out, &mut err), 0);
    assert!(String::from_utf8_lossy(&err).contains("tree directory is not empty"));
    assert_eq!(std::fs::read(&marker).unwrap(), b"retain me");
    std::fs::remove_file(&marker).unwrap();

    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_ne!(rollback(&mut out, &mut err), 0);
    assert!(String::from_utf8_lossy(&err).contains("unconfirmed release directory mode changed"));
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();

    let held = parent.join("retained-confirmation-fixture");
    std::fs::rename(&root, &held).unwrap();
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_ne!(rollback(&mut out, &mut err), 0);
    assert!(String::from_utf8_lossy(&err).contains("tree creation identity is unavailable"));
    assert!(root.is_dir());
    std::fs::remove_dir(&root).unwrap();
    std::fs::rename(&held, &root).unwrap();

    let held_parent = env.product_root.join("retained-releases-fixture");
    std::fs::rename(&parent, &held_parent).unwrap();
    std::fs::create_dir(&parent).unwrap();
    std::fs::rename(held_parent.join(release), &root).unwrap();
    assert_ne!(rollback(&mut out, &mut err), 0);
    assert!(String::from_utf8_lossy(&err).contains("rollback releases directory identity changed"));
    std::fs::rename(&root, held_parent.join(release)).unwrap();
    std::fs::remove_dir(&parent).unwrap();
    std::fs::rename(&held_parent, &parent).unwrap();
    assert_eq!(std::fs::read_to_string(&receipt).unwrap(), recorded);

    let mut foreign_intent = intent;
    foreign_intent["pending"]["issued_unix"] = serde_json::json!(stale.issued_unix);
    let foreign_receipt = format!("{foreign_intent}\n");
    std::fs::write(&receipt, &foreign_receipt).unwrap();
    assert_ne!(rollback(&mut out, &mut err), 0);
    assert!(
        String::from_utf8_lossy(&err)
            .contains("release directory has no matching rollback receipt")
    );
    assert!(root.is_dir());
    assert_eq!(std::fs::read_to_string(&receipt).unwrap(), foreign_receipt);
    assert_eq!(read_pending(&env.transition).unwrap(), Some(pending));
    std::fs::write(&receipt, &recorded).unwrap();

    assert_eq!(
        rollback(&mut out, &mut err),
        0,
        "{}",
        String::from_utf8_lossy(&err)
    );
    assert!(!root.exists());
    assert!(!tree_journal_present(&env.transition).unwrap());
    assert!(read_pending(&env.transition).unwrap().is_none());
}

#[test]
fn rollback_clears_receipts_when_no_release_tree_could_exist() {
    let base =
        std::env::temp_dir().join(format!("ramshared-rollback-absent-{}", std::process::id()));
    std::fs::create_dir(&base).unwrap();
    let _fixture = Fixture(base.clone());
    let env = test_env(&base);
    for target_release in [None, Some("v1.0.2".to_string())] {
        let pending = PackagePending {
            target_release,
            action: PackageAction::Install,
            old_hash: "a".repeat(64),
            new_hash: "b".repeat(64),
            issued_unix: 1,
        };
        let guard = acquire_transition(OperationOwner::Package, &env.transition).unwrap();
        enroll_pending(&guard, &pending).unwrap();
        drop(guard);
        let mut err = Vec::new();
        assert_eq!(
            run_with(
                &["--reconcile-rollback".into()],
                &env,
                &mut FakeUnitHost::inactive_all(),
                &FakeBootHost::all_disabled(),
                &mut live_test_memory,
                &mut Vec::new(),
                &mut err,
            ),
            0,
            "{}",
            String::from_utf8_lossy(&err),
        );
        assert!(read_pending(&env.transition).unwrap().is_none());
        assert!(!env.releases_dir().exists());
    }
}
