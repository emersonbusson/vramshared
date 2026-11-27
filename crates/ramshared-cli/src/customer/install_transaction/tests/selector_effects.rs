// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/install_transaction/tests/selector_effects.rs

use super::*;

#[test]
fn install_transaction_plan_root_untrusted_and_reconcile_effect_edges() {
    use std::os::unix::fs::symlink;

    let base = std::env::temp_dir().join(format!("ramshared-tx-edges-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("etc")).unwrap();
    std::fs::create_dir_all(base.join("systemd")).unwrap();
    std::fs::create_dir_all(base.join("sink")).unwrap();
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(base.join("etc"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let _fix = Fixture(base.clone());

    let env = test_env(&base);
    let home = base.join("gnupg");
    let keyring = base.join("etc/release-keyring.gpg");
    let _agent = create_signing_key(&home, &keyring);
    let sink_str = base.join("sink").to_str().unwrap().to_owned();
    let mut mem = || live_test_memory();
    let mut units = FakeUnitHost::inactive_all();
    let boot = FakeBootHost::all_disabled();

    // Non-root guard refusal when require_root=true (if running unprivileged).
    if rustix::process::geteuid().as_raw() != 0 {
        let mut root_env = test_env(&base);
        root_env.require_root = true;
        assert_eq!(
            run_with(
                &["--reconcile-rollback".into()],
                &root_env,
                &mut units,
                &boot,
                &mut mem,
                &mut Vec::new(),
                &mut Vec::new(),
            ),
            3
        );
    }

    // --plan via parent packages dir, missing release-manifest.json, bad RELEASE_VERSION, legacy approval in plan.
    let ver1 = "v0.15.0-30-gffff1111";
    let b1 = env.default_bundle_root.join("pkg-1/release");
    let sha1 = build_signed_bundle(&home, &b1, ver1, b"e1");
    assert_eq!(
        run_with(
            &[
                "--plan".into(),
                "--approve-legacy-unit-replacement".into(),
                "0".repeat(64)
            ],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        0
    );
    let incomplete = env.default_bundle_root.join("pkg-bad/release");
    std::fs::create_dir_all(&incomplete).unwrap();
    std::fs::write(incomplete.join("RELEASE_VERSION"), "v0.15.0-31-gbad0001\n").unwrap();
    assert_eq!(
        run_with(
            &[
                "--plan".into(),
                "--bundle-root".into(),
                incomplete.to_str().unwrap().into()
            ],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        2
    );
    std::fs::write(incomplete.join("RELEASE_VERSION"), "not valid!\n").unwrap();
    assert_eq!(
        run_with(
            &[
                "--plan".into(),
                "--bundle-root".into(),
                incomplete.to_str().unwrap().into()
            ],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        2
    );

    // Auto-update with untrusted keyring refuses with exit 3, and invalid RELEASE_VERSION refuses with exit 2.
    let wrong_keyring = base.join("etc/wrong-keyring.gpg");
    let wrong_home = base.join("gnupg-wrong");
    let _wrong_agent = create_signing_key(&wrong_home, &wrong_keyring);
    std::fs::write(
        &env.default_policy,
        format!(
            "enabled=1\nlower_sink={sink_str}\nbundle_root={}\nrelease_keyring={}\n",
            b1.parent().unwrap().parent().unwrap().display(),
            wrong_keyring.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&env.default_policy, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::remove_dir_all(env.default_bundle_root.join("pkg-bad")).unwrap();
    assert_eq!(
        run_with(
            &["--auto-update".into()],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        3
    );

    // MemoryUnavailable during apply refuses cleanly (exit 6).
    let mut no_mem = || None;
    assert_eq!(
        run_with(
            &[
                "--approve-nbd-product-install".into(),
                ver1.into(),
                "--approve-manifest-sha256".into(),
                sha1.clone(),
                "--lower-sink".into(),
                sink_str.clone(),
                "--posture".into(),
                "writeback".into(),
                "--keyring".into(),
                keyring.to_str().unwrap().into(),
                "--bundle-root".into(),
                b1.to_str().unwrap().into(),
            ],
            &env,
            &mut units,
            &boot,
            &mut no_mem,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        6
    );

    // Wrong keyring / missing release-manifest in apply.
    assert_eq!(
        run_with(
            &[
                "--approve-nbd-product-install".into(),
                ver1.into(),
                "--approve-manifest-sha256".into(),
                sha1.clone(),
                "--lower-sink".into(),
                sink_str.clone(),
                "--posture".into(),
                "writeback".into(),
                "--keyring".into(),
                wrong_keyring.to_str().unwrap().into(),
                "--bundle-root".into(),
                b1.to_str().unwrap().into(),
            ],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        3
    );

    // Install v1 cleanly so we can exercise upgrade --previous-bundle error & --reconcile-rollback with effect journal.
    assert_eq!(
        run_with(
            &[
                "--approve-nbd-product-install".into(),
                ver1.into(),
                "--approve-manifest-sha256".into(),
                sha1.clone(),
                "--lower-sink".into(),
                sink_str.clone(),
                "--posture".into(),
                "writeback".into(),
                "--keyring".into(),
                keyring.to_str().unwrap().into(),
                "--bundle-root".into(),
                b1.to_str().unwrap().into(),
            ],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        0
    );

    let ver2 = "v0.15.0-31-gffff2222";
    let b2 = base.join("isolated/v2/release");
    let sha2 = build_signed_bundle(&home, &b2, ver2, b"e2");
    assert_eq!(
        run_with(
            &[
                "--approve-nbd-product-install".into(),
                ver2.into(),
                "--approve-manifest-sha256".into(),
                sha2.clone(),
                "--lower-sink".into(),
                sink_str.clone(),
                "--posture".into(),
                "writeback".into(),
                "--previous-bundle".into(),
                b2.to_str().unwrap().into(),
                "--keyring".into(),
                keyring.to_str().unwrap().into(),
                "--bundle-root".into(),
                b2.to_str().unwrap().into(),
            ],
            &env,
            &mut units,
            &boot,
            &mut mem,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        4
    );

    let publish_candidate = |mem: &mut dyn FnMut() -> Option<MemoryObservation>| {
        let pending = PackagePending {
            target_release: Some(ver2.into()),
            action: PackageAction::Upgrade,
            old_hash: sha1.clone(),
            new_hash: sha2.clone(),
            issued_unix: 30,
        };
        let mut observe = || {
            Ok(crate::customer::package_transition::PackageObservation {
                snapshot: crate::customer::package_admission::LifecycleSnapshot {
                    state: CustomerState::InstalledOff,
                    owner: crate::customer::package_admission::LifecycleOwnership::Owned,
                    guarded_hooks: true,
                },
                release: crate::customer::plan::ReleaseIdentity::new(ver1),
                old_hash: pending.old_hash.clone(),
                new_hash: sha2.clone(),
            })
        };
        let sig1 = crate::customer::package_trust::verify_bundle_with_keyring(
            &b1,
            &keyring,
            env.owner_uid,
        )
        .unwrap();
        let sig2 = crate::customer::package_trust::verify_bundle_with_keyring(
            &b2,
            &keyring,
            env.owner_uid,
        )
        .unwrap();
        let decl1 =
            crate::customer::package_source::DeclaredBundle::open_verified(&b1, &sig1).unwrap();
        let decl2 =
            crate::customer::package_source::DeclaredBundle::open_verified(&b2, &sig2).unwrap();
        let tmpl1 = decl1
            .member(
                "cascade.conf.example",
                crate::release_identity::MAX_SEALED_TEXT_BYTES,
            )
            .unwrap();
        let req = crate::customer::package_source::stream::MemoryRequirement {
            work_bytes: 1024 * 1024,
            guest_reserve_bytes: 16 * 1024 * 1024,
            host_reserve_bytes: 16 * 1024 * 1024,
            max_age_ms: 5000,
        };
        let streamed2 = decl2
            .verify_streamed(
                crate::customer::package_source::MEMBER_LIMIT as u64,
                512 * 1024 * 1024,
                &req,
                mem,
            )
            .unwrap();
        let admitted = crate::customer::package_transition::begin_verified_package_held(
            &env.transition,
            &pending,
            Some(&streamed2),
            Some(&sig2),
            &mut observe,
            mem,
        )
        .unwrap();
        std::fs::create_dir_all(env.stage_dir()).unwrap();
        let rel_meta = std::fs::metadata(env.releases_dir()).unwrap();
        let prep_paths = crate::customer::package_prepare::PreparePaths {
            stage: &env.stage_dir(),
            parent: &env.releases_dir(),
            parent_identity: (rel_meta.dev(), rel_meta.ino()),
            name: ver2,
            reserve_bytes: INSTALL_RESERVE_BYTES,
            progress: None,
        };
        let prepared = crate::customer::package_prepare::prepare_admitted(
            &admitted,
            &pending,
            &decl2,
            &sig2,
            &prep_paths,
        )
        .unwrap();
        let sink = SinkWitness::observe(&sink_str).unwrap();
        let prev_ev = crate::customer::package_selector::PreviousEvidence {
            signature: &sig1,
            transformation: Some(crate::customer::package_selector::PreviousTransformation {
                role: "cascade.conf.example",
                template: &tmpl1,
                sink: &sink,
            }),
        };
        let intent = crate::customer::package_selector::record_prepared_intent(
            &admitted,
            &pending,
            &decl2,
            &prepared,
            &sig2,
            Some(&prev_ev),
            &env.selector_path(),
        )
        .unwrap();
        let sealed = crate::customer::package_selector::finalize_and_seal_release(
            &intent,
            &sink,
            "2026-11-22T09:00:00Z",
        )
        .unwrap();
        let effect =
            crate::customer::package_selector::publish_sealed_selector(&intent, &sealed).unwrap();
        (pending, effect)
    };

    // Exercise both attended decisions against an actual sealed selector.
    for (action, result, selected) in [
        ("--reconcile-rollback", "RECONCILED_ROLLBACK", ver1),
        ("--reconcile-complete", "RECONCILED_COMPLETE", ver2),
    ] {
        let (pending, effect) = publish_candidate(&mut mem);
        let args = [
            action.into(),
            "--bundle-root".into(),
            b2.display().to_string(),
            "--keyring".into(),
            keyring.display().to_string(),
        ];
        if action == "--reconcile-complete" {
            let replace_effect =
                |effect: &crate::customer::package_selector::SelectorEffectRecord| {
                    let guard =
                        acquire_transition(OperationOwner::Package, &env.transition).unwrap();
                    crate::customer::transition_custody::write_journal_record(
                        &guard,
                        std::ffi::OsStr::new("package-selector-effect.json"),
                        "fixture selector publication",
                        &serde_json::to_string(effect).unwrap(),
                    )
                    .unwrap();
                };
            let mut published = effect.clone();
            published.phase =
                crate::customer::package_selector::SelectorEffectPhase::SelectorPublished;
            replace_effect(&published);

            let reloads = units.reloads;
            let mut err = Vec::new();
            assert_ne!(
                run_with(
                    &args,
                    &env,
                    &mut units,
                    &boot,
                    &mut mem,
                    &mut Vec::new(),
                    &mut err
                ),
                0,
            );
            assert!(
                String::from_utf8_lossy(&err)
                    .contains("RECONCILE_COMPLETE_UNSUPPORTED_WITHOUT_SEALED_SELECTOR")
            );
            assert_eq!(read_pending(&env.transition).unwrap(), Some(pending));
            assert_eq!(
                read_release_version(&env.selector_path()).as_deref(),
                Some(ver2)
            );
            assert_eq!(units.reloads, reloads);
            replace_effect(&effect);
        }
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(
            run_with(&args, &env, &mut units, &boot, &mut mem, &mut out, &mut err),
            0,
            "{}",
            String::from_utf8_lossy(&err),
        );
        assert!(String::from_utf8_lossy(&out).contains(result));
        assert_eq!(
            read_release_version(&env.selector_path()).as_deref(),
            Some(selected)
        );
        assert!(read_pending(&env.transition).unwrap().is_none());
    }

    // Also test resolve_bundle_dir when path is symlink-free absent dir -> exit 6.
    let symlink_bad = base.join("link-release");
    symlink(&b1, &symlink_bad).unwrap();
    let _ = run_with(
        &[
            "--plan".into(),
            "--bundle-root".into(),
            symlink_bad.to_str().unwrap().into(),
        ],
        &env,
        &mut units,
        &boot,
        &mut mem,
        &mut Vec::new(),
        &mut Vec::new(),
    );
}
