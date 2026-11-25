// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_source/tests/package_prepare_scenarios.rs

fn exercise_package_preparation(
    root: &std::path::Path,
    stage_pending: &crate::customer::transition::PackagePending,
    bundle: &DeclaredBundle,
    lines: &[String],
    checksums: &str,
    gated_streamed: &StreamVerifiedBundle<'_>,
) -> (
    crate::customer::package_trust::VerifiedSignature,
    crate::customer::package_trust::VerifiedSignature,
) {
    use crate::customer::transition::{
        OperationOwner, PackageAction, TransitionPaths, acquire_transition, enroll_pending,
    };
    // Coordinate the full guarded prepare sequence and reopen its result.
    let coordinated_stage = root.join("coordinated-stage");
    std::fs::create_dir(&coordinated_stage).unwrap();
    let coordinated_parent = root.join("coordinated-parent");
    std::fs::create_dir(&coordinated_parent).unwrap();
    let coordinated_metadata = std::fs::metadata(&coordinated_parent).unwrap();
    let coordinated_paths = TransitionPaths::under(&root.join("coordinated-transaction"));
    let signature = crate::customer::package_trust::VerifiedSignature::fixture(
        bundle.manifest_sha256().to_owned(),
    );
    let wrong_signature =
        crate::customer::package_trust::VerifiedSignature::fixture("f".repeat(64));
    let observed = crate::customer::package_transition::PackageObservation {
        snapshot: crate::customer::package_admission::LifecycleSnapshot {
            state: crate::customer::CustomerState::InstalledOff,
            owner: crate::customer::package_admission::LifecycleOwnership::Owned,
            guarded_hooks: true,
        },
        release: crate::customer::plan::ReleaseIdentity::new("0.15.0"),
        old_hash: stage_pending.old_hash.clone(),
        new_hash: stage_pending.new_hash.clone(),
    };
    assert!(
        crate::customer::package_transition::begin_verified_package_held(
            &coordinated_paths,
            stage_pending,
            Some(gated_streamed),
            Some(&signature),
            &mut || Ok(observed.clone()),
            &mut || None,
        )
        .is_err()
    );
    assert!(
        crate::customer::transition::read_pending(&coordinated_paths)
            .unwrap()
            .is_none()
    );
    let fresh_memory = || {
        let now = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_millis(),
        )
        .ok()?;
        Some(crate::customer::package_source::stream::MemoryObservation {
            platform: Some(ramshared_block::platform::PlatformKind::NativeLinux),
            guest: ramshared_tier::memory_headroom::MemorySample {
                total_bytes: 1_000,
                available_bytes: 700,
                sampled_at_unix_ms: now,
            },
            host: ramshared_tier::memory_headroom::HostScope::Native,
            cgroups: Some(
                crate::customer::package_source::stream::MemoryObservation::fixture_cgroups(now),
            ),
        })
    };
    let after_recheck = std::cell::Cell::new(false);
    let mut observations = 0;
    assert!(
        crate::customer::package_transition::begin_verified_package_held(
            &coordinated_paths,
            stage_pending,
            Some(gated_streamed),
            Some(&signature),
            &mut || {
                observations += 1;
                if observations == 3 {
                    after_recheck.set(true);
                }
                Ok(observed.clone())
            },
            &mut || if after_recheck.get() {
                None
            } else {
                fresh_memory()
            },
        )
        .is_err()
    );
    assert!(
        crate::customer::transition::read_pending(&coordinated_paths)
            .unwrap()
            .is_none()
    );
    let admitted = crate::customer::package_transition::begin_verified_package_held(
        &coordinated_paths,
        stage_pending,
        Some(gated_streamed),
        Some(&signature),
        &mut || Ok(observed.clone()),
        &mut || fresh_memory(),
    )
    .unwrap();
    let coordinated_guard = admitted.guard();
    assert!(acquire_transition(OperationOwner::Package, &coordinated_paths).is_err());
    let request = crate::customer::package_prepare::PreparePaths {
        stage: &coordinated_stage,
        parent: &coordinated_parent,
        parent_identity: (coordinated_metadata.dev(), coordinated_metadata.ino()),
        name: "release",
        reserve_bytes: 0,
    };
    let alternate_root = root.join("alternate-bundle");
    std::fs::create_dir(&alternate_root).unwrap();
    std::fs::write(alternate_root.join("SHA256SUMS"), checksums).unwrap();
    for line in lines {
        let (_, path) = line.split_once("  ./").unwrap();
        let destination = alternate_root.join(path);
        std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
        std::fs::copy(root.join(path), destination).unwrap();
    }
    let alternate = DeclaredBundle::open(&alternate_root, bundle.manifest_sha256()).unwrap();
    assert!(
        crate::customer::package_prepare::prepare_admitted(
            &admitted,
            stage_pending,
            &alternate,
            &signature,
            &request,
        )
        .is_err()
    );
    assert!(
        std::fs::read_dir(&coordinated_stage)
            .unwrap()
            .next()
            .is_none()
    );
    assert!(
        crate::customer::package_prepare::resume(
            coordinated_guard,
            stage_pending,
            bundle,
            &signature,
            &request,
        )
        .is_err()
    );
    let mut wrong_action = stage_pending.clone();
    wrong_action.action = PackageAction::Remove;
    assert!(
        crate::customer::package_prepare::prepare_admitted(
            &admitted,
            &wrong_action,
            bundle,
            &signature,
            &request,
        )
        .is_err()
    );
    assert!(
        crate::customer::package_prepare::prepare_admitted(
            &admitted,
            stage_pending,
            bundle,
            &wrong_signature,
            &request,
        )
        .is_err()
    );
    assert!(
        !crate::customer::package_stage_journal::stage_journal_present(&coordinated_paths).unwrap()
    );
    let selector_journal = coordinated_paths
        .transactions_root()
        .join("package-selector.json");
    std::fs::write(&selector_journal, b"interrupted\n").unwrap();
    std::fs::set_permissions(
        &selector_journal,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();
    assert!(
        crate::customer::package_prepare::prepare_admitted(
            &admitted,
            stage_pending,
            bundle,
            &signature,
            &request,
        )
        .is_err()
    );
    assert!(
        std::fs::read_dir(&coordinated_stage)
            .unwrap()
            .next()
            .is_none()
    );
    std::fs::remove_file(selector_journal).unwrap();
    let release = crate::customer::package_prepare::prepare_admitted(
        &admitted,
        stage_pending,
        bundle,
        &signature,
        &request,
    )
    .unwrap();
    assert_eq!(release.members.len(), bundle.stage_roles().unwrap().len());
    assert_eq!(
        std::fs::read(release.tree.path().join("bin/ramshared")).unwrap(),
        fixture_elf(62)
    );
    assert!(
        crate::customer::package_prepare::prepare_admitted(
            &admitted,
            stage_pending,
            bundle,
            &signature,
            &request,
        )
        .is_err()
    );
    let prior = coordinated_parent.join("prior-release");
    let prior_signature = crate::customer::package_trust::VerifiedSignature::fixture_with_manifest(
        prior_fixture_input(),
    );
    let prior_evidence = crate::customer::package_selector::PreviousEvidence {
        signature: &prior_signature,
        transformation: None,
    };
    let prior_input_digest = prepare_prior_release_fixture(&prior, b"different prior manifest\n");
    let selector = coordinated_parent.join("current");
    std::os::unix::fs::symlink("prior-release", &selector).unwrap();
    assert_ne!(prior_input_digest, stage_pending.old_hash);
    assert!(
        crate::customer::package_selector::record_prepared_intent(
            &admitted,
            stage_pending,
            bundle,
            &release,
            &signature,
            Some(&prior_evidence),
            &selector,
        )
        .is_err()
    );
    assert!(!crate::customer::package_selector::journal_present(&coordinated_paths).unwrap());
    std::fs::remove_file(prior.join("INSTALL_PROVENANCE.json")).unwrap();
    assert!(
        crate::customer::package_selector::record_prepared_intent(
            &admitted,
            stage_pending,
            bundle,
            &release,
            &signature,
            Some(&prior_evidence),
            &selector,
        )
        .is_err()
    );
    assert!(!crate::customer::package_selector::journal_present(&coordinated_paths).unwrap());
    let prior_input_digest = prepare_prior_release_fixture(&prior, &prior_fixture_input());
    assert_eq!(prior_input_digest, stage_pending.old_hash);
    let wrong_prior_signature =
        crate::customer::package_trust::VerifiedSignature::fixture("b".repeat(64));
    let wrong_evidence = crate::customer::package_selector::PreviousEvidence {
        signature: &wrong_prior_signature,
        transformation: None,
    };
    for proof in [None, Some(&wrong_evidence)] {
        assert!(
            crate::customer::package_selector::record_prepared_intent(
                &admitted,
                stage_pending,
                bundle,
                &release,
                &signature,
                proof,
                &selector,
            )
            .is_err()
        );
        assert!(!crate::customer::package_selector::journal_present(&coordinated_paths).unwrap());
    }
    assert!(
        crate::customer::package_selector::record_prepared_intent(
            &admitted,
            stage_pending,
            bundle,
            &release,
            &wrong_signature,
            Some(&prior_evidence),
            &selector,
        )
        .is_err()
    );
    assert!(!crate::customer::package_selector::journal_present(&coordinated_paths).unwrap());
    std::fs::remove_file(&selector).unwrap();
    std::os::unix::fs::symlink("../prior-release", &selector).unwrap();
    assert!(
        crate::customer::package_selector::record_prepared_intent(
            &admitted,
            stage_pending,
            bundle,
            &release,
            &signature,
            Some(&prior_evidence),
            &selector,
        )
        .is_err()
    );
    assert!(!crate::customer::package_selector::journal_present(&coordinated_paths).unwrap());
    std::fs::remove_file(&selector).unwrap();
    std::os::unix::fs::symlink("prior-release", &selector).unwrap();
    assert!(
        crate::customer::package_selector::record_prepared_intent_with_hook(
            &admitted,
            stage_pending,
            bundle,
            &release,
            (&signature, Some(&prior_evidence)),
            &selector,
            || {
                std::fs::remove_file(&selector).unwrap();
                std::os::unix::fs::symlink("prior-release", &selector).unwrap();
            },
        )
        .is_err()
    );
    let interrupted_journal = coordinated_paths
        .transactions_root()
        .join("package-selector.json");
    assert!(crate::customer::package_selector::journal_present(&coordinated_paths).unwrap());
    assert_eq!(
        std::fs::read_link(&selector).unwrap(),
        std::path::Path::new("prior-release")
    );
    assert!(crate::customer::transition::clear_pending(admitted.guard(), stage_pending).is_err());
    std::fs::remove_file(interrupted_journal).unwrap();
    let selector_intent = crate::customer::package_selector::record_prepared_intent(
        &admitted,
        stage_pending,
        bundle,
        &release,
        &signature,
        Some(&prior_evidence),
        &selector,
    )
    .unwrap();
    assert!(crate::customer::package_selector::journal_present(&coordinated_paths).unwrap());
    let journal_path = coordinated_paths
        .transactions_root()
        .join("package-selector.json");
    let intent: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&journal_path).unwrap()).unwrap();
    assert_eq!(intent["target"], "release");
    assert_eq!(intent["previous"]["target"], "prior-release");
    assert_eq!(
        intent["previous"]["input_bundle_manifest_sha256"],
        prior_input_digest
    );
    assert_eq!(intent["pending"]["new_hash"], stage_pending.new_hash);
    let original_intent = std::fs::read(&journal_path).unwrap();
    assert!(
        crate::customer::package_selector::record_prepared_intent(
            &admitted,
            stage_pending,
            bundle,
            &release,
            &signature,
            Some(&prior_evidence),
            &selector,
        )
        .is_err()
    );
    assert_eq!(std::fs::read(&journal_path).unwrap(), original_intent);
    assert_eq!(
        std::fs::read_link(&selector).unwrap(),
        std::path::Path::new("prior-release")
    );
    assert!(crate::customer::transition::clear_pending(admitted.guard(), stage_pending).is_err());
    drop(selector_intent);
    std::fs::remove_file(
        coordinated_paths
            .transactions_root()
            .join("package-selector.json"),
    )
    .unwrap();
    drop(release);
    drop(admitted);
    let coordinated_guard =
        acquire_transition(OperationOwner::Package, &coordinated_paths).unwrap();
    assert!(
        crate::customer::package_prepare::resume(
            &coordinated_guard,
            stage_pending,
            &alternate,
            &signature,
            &request,
        )
        .is_err()
    );
    std::fs::write(root.join("SHA256SUMS"), checksums).unwrap();
    let reopened_source = DeclaredBundle::open(root, bundle.manifest_sha256()).unwrap();
    let reopened = crate::customer::package_prepare::resume(
        &coordinated_guard,
        stage_pending,
        &reopened_source,
        &signature,
        &request,
    )
    .unwrap();
    assert_eq!(reopened.members.len(), bundle.stage_roles().unwrap().len());
    drop(reopened);
    drop(coordinated_guard);

    // Stray later-phase records are retained and never adopted.
    for (suffix, filename) in [
        ("tree", "package-tree.json"),
        ("publication", "package-publish.json"),
    ] {
        let paths = TransitionPaths::under(&root.join(format!("coordinator-foreign-{suffix}")));
        let guard = acquire_transition(OperationOwner::Package, &paths).unwrap();
        enroll_pending(&guard, stage_pending).unwrap();
        let journal = paths.transactions_root().join(filename);
        std::fs::write(&journal, b"foreign evidence\n").unwrap();
        std::fs::set_permissions(
            &journal,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )
        .unwrap();
        assert!(
            crate::customer::package_prepare::resume(
                &guard,
                stage_pending,
                bundle,
                &signature,
                &request
            )
            .is_err()
        );
        assert_eq!(std::fs::read(journal).unwrap(), b"foreign evidence\n");
    }

    exercise_fresh_selector_intent(
        root,
        bundle,
        gated_streamed,
        &signature,
        stage_pending,
        &mut || fresh_memory(),
    );
    (signature, wrong_signature)
}

fn prior_fixture_input() -> Vec<u8> {
    let hash = digest(b"fixture").unwrap();
    [
        "bin/ramshared",
        "bin/ramsharedd",
        "RELEASE_VERSION",
        "SOURCE_COMMIT",
        "SOURCE_TREE_STATE",
    ]
    .iter()
    .map(|name| format!("{hash}  ./{name}\n"))
    .collect::<String>()
    .into_bytes()
}

fn prepare_prior_release_fixture(root: &std::path::Path, input: &[u8]) -> String {
    std::fs::create_dir_all(root.join("bin")).unwrap();
    let input_digest = digest(input).unwrap();
    for name in [
        "bin/ramshared",
        "bin/ramsharedd",
        "RELEASE_VERSION",
        "SOURCE_COMMIT",
        "SOURCE_TREE_STATE",
    ] {
        std::fs::write(root.join(name), b"fixture").unwrap();
    }
    std::fs::write(root.join("INPUT_BUNDLE_SHA256SUMS"), input).unwrap();
    let provenance = serde_json::json!({
        "schema_version": crate::installed_image::INSTALLED_PROVENANCE_V2,
        "input_bundle_manifest_sha256": input_digest,
    });
    std::fs::write(root.join("INSTALL_PROVENANCE.json"), provenance.to_string()).unwrap();
    let mut lines = Vec::new();
    for name in [
        "bin/ramshared",
        "bin/ramsharedd",
        "RELEASE_VERSION",
        "SOURCE_COMMIT",
        "SOURCE_TREE_STATE",
        "INPUT_BUNDLE_SHA256SUMS",
        "INSTALL_PROVENANCE.json",
    ] {
        lines.push(format!(
            "{}  ./{name}",
            digest(&std::fs::read(root.join(name)).unwrap()).unwrap()
        ));
    }
    let installed_manifest = format!("{}\n", lines.join("\n"));
    std::fs::write(root.join("SHA256SUMS"), &installed_manifest).unwrap();
    std::fs::write(
        root.join("INSTALLED_MANIFEST_SHA256"),
        format!("{}\n", digest(installed_manifest.as_bytes()).unwrap()),
    )
    .unwrap();
    input_digest
}

fn exercise_fresh_selector_intent(
    root: &std::path::Path,
    bundle: &DeclaredBundle,
    streamed: &StreamVerifiedBundle<'_>,
    signature: &crate::customer::package_trust::VerifiedSignature,
    template: &crate::customer::transition::PackagePending,
    memory: &mut dyn FnMut() -> Option<crate::customer::package_source::stream::MemoryObservation>,
) {
    use crate::customer::transition::{PackageAction, TransitionPaths};
    let stage = root.join("selector-fresh-stage");
    let parent = root.join("selector-fresh-parent");
    std::fs::create_dir(&stage).unwrap();
    std::fs::create_dir(&parent).unwrap();
    let metadata = std::fs::metadata(&parent).unwrap();
    let paths = TransitionPaths::under(&root.join("selector-fresh-transaction"));
    let mut pending = template.clone();
    pending.action = PackageAction::Install;
    let observation = crate::customer::package_transition::PackageObservation {
        snapshot: crate::customer::package_admission::LifecycleSnapshot {
            state: crate::customer::CustomerState::Absent,
            owner: crate::customer::package_admission::LifecycleOwnership::Owned,
            guarded_hooks: false,
        },
        release: crate::customer::plan::ReleaseIdentity::new("v1"),
        old_hash: pending.old_hash.clone(),
        new_hash: pending.new_hash.clone(),
    };
    let admitted = crate::customer::package_transition::begin_verified_package_held(
        &paths,
        &pending,
        Some(streamed),
        Some(signature),
        &mut || Ok(observation.clone()),
        memory,
    )
    .unwrap();
    let request = crate::customer::package_prepare::PreparePaths {
        stage: &stage,
        parent: &parent,
        parent_identity: (metadata.dev(), metadata.ino()),
        name: "release",
        reserve_bytes: 0,
    };
    let release = crate::customer::package_prepare::prepare_admitted(
        &admitted, &pending, bundle, signature, &request,
    )
    .unwrap();
    let selector = parent.join("current");
    std::fs::write(&selector, b"foreign").unwrap();
    assert!(
        crate::customer::package_selector::record_prepared_intent(
            &admitted, &pending, bundle, &release, signature, None, &selector,
        )
        .is_err()
    );
    assert_eq!(std::fs::read(&selector).unwrap(), b"foreign");
    std::fs::remove_file(&selector).unwrap();
    let intent = crate::customer::package_selector::record_prepared_intent(
        &admitted, &pending, bundle, &release, signature, None, &selector,
    )
    .unwrap();
    let body = std::fs::read(paths.transactions_root().join("package-selector.json")).unwrap();
    let record: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(record["previous"].is_null());
    assert!(
        matches!(std::fs::symlink_metadata(&selector), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
    );
    assert!(crate::customer::transition::clear_pending(admitted.guard(), &pending).is_err());
    assert!(intent.held_fd_count() > 0);
    assert!(intent.previous_custody().is_none());
    assert!(intent.revalidate().is_ok());
    std::fs::write(parent.join("release/rogue"), b"x").unwrap();
    assert!(intent.revalidate().is_err());
    drop(intent);
}
