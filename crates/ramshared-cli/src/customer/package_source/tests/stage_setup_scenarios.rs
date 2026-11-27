// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_source/tests/stage_setup_scenarios.rs

struct StagedFixture {
    stage_dir: std::path::PathBuf,
    stage_paths: crate::customer::transition::TransitionPaths,
    stage_guard: crate::customer::transition::CustomerTransitionGuard,
    stage_pending: crate::customer::transition::PackagePending,
    inventory: std::collections::BTreeMap<String, crate::customer::package_stage::StageSlot>,
}

fn exercise_stage_setup(
    root: &std::path::Path,
    bundle: &DeclaredBundle,
    verified: &VerifiedBundle,
) -> StagedFixture {
    use crate::customer::package::ManifestArchitecture::{Amd64, Arm64};
    use crate::customer::package_stage_journal::inventory::{
        stage_all_members, verify_staged_inventory,
    };
    use crate::customer::package_stage_journal::{StageRequest, stage_declared_member};
    use crate::customer::transition::{
        OperationOwner, PackageAction, PackagePending, TransitionPaths, acquire_transition,
        enroll_pending,
    };
    let stage_dir = root.join("stream-stage");
    std::fs::create_dir(&stage_dir).unwrap();
    let stage_paths = TransitionPaths::under(&root.join("stream-transaction"));
    let stage_guard = acquire_transition(OperationOwner::Package, &stage_paths).unwrap();
    let stage_pending = PackagePending {
        target_release: Some("v1".into()),
        action: PackageAction::Upgrade,
        old_hash: digest(&prior_fixture_input()).unwrap(),
        new_hash: verified.manifest_sha256().into(),
        issued_unix: 1,
    };
    assert!(verify_staged_inventory(&stage_guard, &stage_pending, bundle).is_err());
    let mut wrong_pending = stage_pending.clone();
    wrong_pending.new_hash = "d".repeat(64);
    assert!(verify_staged_inventory(&stage_guard, &wrong_pending, bundle).is_err());
    enroll_pending(&stage_guard, &stage_pending).unwrap();
    assert!(verify_staged_inventory(&stage_guard, &stage_pending, bundle).is_err());
    let selector_journal = stage_paths
        .transactions_root()
        .join("package-selector.json");
    std::fs::write(&selector_journal, b"interrupted\n").unwrap();
    std::fs::set_permissions(
        &selector_journal,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();
    assert!(stage_all_members(&stage_guard, &stage_pending, bundle, &stage_dir, 0).is_err());
    let first_role = bundle.stage_roles().unwrap().first().unwrap().clone();
    let direct = StageRequest {
        pending: &stage_pending,
        bundle,
        role: &first_role,
        parent: &stage_dir,
        name: "member-0",
        limit: bundle.member_size(&first_role).unwrap(),
    };
    assert!(stage_declared_member(&stage_guard, direct).is_err());
    assert!(std::fs::read_dir(&stage_dir).unwrap().next().is_none());
    std::fs::remove_file(selector_journal).unwrap();
    let incomplete_paths = TransitionPaths::under(&root.join("incomplete-transaction"));
    let incomplete_guard = acquire_transition(OperationOwner::Package, &incomplete_paths).unwrap();
    enroll_pending(&incomplete_guard, &stage_pending).unwrap();
    let incomplete_request = StageRequest {
        pending: &stage_pending,
        bundle,
        role: "ramshared",
        parent: &stage_dir,
        name: "incomplete",
        limit: 1,
    };
    assert!(stage_declared_member(&incomplete_guard, incomplete_request).is_err());
    assert!(verify_staged_inventory(&incomplete_guard, &stage_pending, bundle).is_err());
    drop(incomplete_guard);
    // Insufficient observed disk capacity and collisions must have no stage effects.
    assert!(stage_all_members(&stage_guard, &stage_pending, bundle, &stage_dir, u64::MAX).is_err());
    assert!(!stage_dir.join("member-0").exists());
    std::fs::write(stage_dir.join("member-0"), b"existing").unwrap();
    assert!(stage_all_members(&stage_guard, &stage_pending, bundle, &stage_dir, 0).is_err());
    assert_eq!(
        std::fs::read(stage_dir.join("member-0")).unwrap(),
        b"existing"
    );
    std::fs::remove_file(stage_dir.join("member-0")).unwrap();
    exercise_stage_space_change(root, bundle, &stage_pending);
    let mut inventory =
        stage_all_members(&stage_guard, &stage_pending, bundle, &stage_dir, 0).unwrap();
    assert_eq!(inventory.len(), bundle.stage_roles().unwrap().len());
    assert!(
        bundle
            .verify_staged_semantics_for(&mut inventory, Amd64)
            .is_some()
    );
    assert!(
        bundle
            .verify_staged_semantics_for(&mut inventory, Arm64)
            .is_none()
    );
    StagedFixture {
        stage_dir,
        stage_paths,
        stage_guard,
        stage_pending,
        inventory,
    }
}

fn exercise_stage_space_change(
    root: &std::path::Path,
    bundle: &DeclaredBundle,
    pending: &crate::customer::transition::PackagePending,
) {
    use crate::customer::package_stage_journal::inventory::{
        resume_staged_inventory, stage_all_members_with_hook,
    };
    use crate::customer::transition::{
        OperationOwner, TransitionPaths, acquire_transition, enroll_pending,
    };
    let stage = root.join("stage-space-change");
    std::fs::create_dir(&stage).unwrap();
    let paths = TransitionPaths::under(&root.join("stage-space-transaction"));
    let guard = acquire_transition(OperationOwner::Package, &paths).unwrap();
    enroll_pending(&guard, pending).unwrap();
    let ceiling = crate::customer::test_space::SpaceCeiling::new();
    let mut staged = 0;
    let result = stage_all_members_with_hook(&guard, pending, bundle, &stage, 0, &mut |count, _total| {
        staged = count;
        if count == 1 {
            ceiling.set(Some(0));
        }
    });
    ceiling.set(None);
    assert!(
        result
            .err()
            .unwrap()
            .detail
            .contains("insufficient observed space")
    );
    assert_eq!(staged, 1);
    assert!(!stage.join("member-1").exists());
    let resumed = resume_staged_inventory(&guard, pending, bundle, &stage, 0).unwrap();
    assert_eq!(resumed.len(), bundle.stage_roles().unwrap().len());
}
