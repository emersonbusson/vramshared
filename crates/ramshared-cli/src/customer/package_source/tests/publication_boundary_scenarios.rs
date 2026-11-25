// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_source/tests/publication_boundary_scenarios.rs

fn exercise_publication_boundaries(
    root: &std::path::Path,
    tree_parent: &std::path::Path,
    tree_parent_id: (u64, u64),
    stage_pending: &crate::customer::transition::PackagePending,
    bundle: &DeclaredBundle,
    signature: &crate::customer::package_trust::VerifiedSignature,
) {
    use crate::customer::package_stage_journal::inventory::stage_all_members;
    use crate::customer::transition::{
        OperationOwner, TransitionPaths, acquire_transition, enroll_pending,
    };
    // A changed staging file must be rejected before the destination is linked.
    let changed_stage = root.join("tree-stage-boundary-stage");
    std::fs::create_dir(&changed_stage).unwrap();
    let changed_paths = TransitionPaths::under(&root.join("tree-stage-boundary-transaction"));
    let changed_guard = acquire_transition(OperationOwner::Package, &changed_paths).unwrap();
    enroll_pending(&changed_guard, stage_pending).unwrap();
    drop(stage_all_members(&changed_guard, stage_pending, bundle, &changed_stage, 0).unwrap());
    let changed_tree = crate::customer::package_tree::create_tree(
        &changed_guard,
        stage_pending,
        bundle,
        tree_parent,
        tree_parent_id,
        "stage-boundary",
        0,
    )
    .unwrap();
    let changed_role = bundle.stage_roles().unwrap().first().unwrap().clone();
    let changed_destination = bundle.publication_member(&changed_role).unwrap().0;
    let selector_journal = changed_paths
        .transactions_root()
        .join("package-selector.json");
    std::fs::write(&selector_journal, b"interrupted\n").unwrap();
    std::fs::set_permissions(
        &selector_journal,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();
    assert!(
        crate::customer::package_publish::publish_owned_member(
            &changed_guard,
            stage_pending,
            bundle,
            &changed_tree,
            &changed_role,
            signature
        )
        .is_err()
    );
    assert!(
        crate::customer::package_publish::resume_owned_publication(
            &changed_guard,
            stage_pending,
            bundle,
            &changed_tree,
            signature
        )
        .is_err()
    );
    assert!(
        crate::customer::package_publish::inventory::publish_owned_all(
            &changed_guard,
            stage_pending,
            bundle,
            &changed_tree,
            signature,
            0
        )
        .is_err()
    );
    assert!(
        crate::customer::package_publish::inventory::resume_owned_all(
            &changed_guard,
            stage_pending,
            bundle,
            &changed_tree,
            signature,
            0
        )
        .is_err()
    );
    assert!(!changed_tree.path().join(&changed_destination).exists());
    std::fs::remove_file(selector_journal).unwrap();
    assert!(
        crate::customer::package_publish::publish_owned_with_before_link(
            &changed_guard,
            stage_pending,
            bundle,
            &changed_tree,
            &changed_role,
            || {
                std::fs::write(changed_stage.join("member-0"), b"changed").unwrap();
                Ok(())
            },
        )
        .is_err()
    );
    assert!(!changed_tree.path().join(changed_destination).exists());
    drop(changed_guard);

    // After link, reject a changed tree before confirming the record.
    let linked_stage = root.join("tree-linked-stage");
    std::fs::create_dir(&linked_stage).unwrap();
    let linked_paths = TransitionPaths::under(&root.join("tree-linked-transaction"));
    let linked_guard = acquire_transition(OperationOwner::Package, &linked_paths).unwrap();
    enroll_pending(&linked_guard, stage_pending).unwrap();
    drop(stage_all_members(&linked_guard, stage_pending, bundle, &linked_stage, 0).unwrap());
    let linked_tree = crate::customer::package_tree::create_tree(
        &linked_guard,
        stage_pending,
        bundle,
        tree_parent,
        tree_parent_id,
        "linked",
        0,
    )
    .unwrap();
    let first_role = bundle.stage_roles().unwrap().first().unwrap().clone();
    let (destination, _, _) = bundle.publication_member(&first_role).unwrap();
    assert!(
        crate::customer::package_publish::publish_owned_with_after_link(
            &linked_guard,
            stage_pending,
            bundle,
            &linked_tree,
            &first_role,
            || {
                std::fs::set_permissions(
                    linked_tree.path(),
                    std::os::unix::fs::PermissionsExt::from_mode(0o755),
                )
                .unwrap();
                Ok(())
            },
        )
        .is_err()
    );
    assert!(linked_tree.path().join(destination).is_file());
    std::fs::set_permissions(
        linked_tree.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let linked_request = crate::customer::package_prepare::PreparePaths {
        stage: &linked_stage,
        parent: tree_parent,
        parent_identity: tree_parent_id,
        name: "linked",
        reserve_bytes: 0,
    };
    let linked = crate::customer::package_prepare::resume(
        &linked_guard,
        stage_pending,
        bundle,
        signature,
        &linked_request,
    )
    .unwrap();
    assert_eq!(linked.members.len(), bundle.stage_roles().unwrap().len());
    drop(linked_guard);

    exercise_publication_space_change(
        root,
        tree_parent,
        tree_parent_id,
        stage_pending,
        bundle,
        signature,
    );
}

fn exercise_publication_space_change(
    root: &std::path::Path,
    parent: &std::path::Path,
    parent_id: (u64, u64),
    pending: &crate::customer::transition::PackagePending,
    bundle: &DeclaredBundle,
    signature: &crate::customer::package_trust::VerifiedSignature,
) {
    use crate::customer::package_stage_journal::inventory::stage_all_members;
    use crate::customer::transition::{
        OperationOwner, TransitionPaths, acquire_transition, enroll_pending,
    };
    let stage = root.join("space-change-stage");
    std::fs::create_dir(&stage).unwrap();
    let paths = TransitionPaths::under(&root.join("space-change-transaction"));
    let guard = acquire_transition(OperationOwner::Package, &paths).unwrap();
    enroll_pending(&guard, pending).unwrap();
    drop(stage_all_members(&guard, pending, bundle, &stage, 0).unwrap());
    let tree = crate::customer::package_tree::create_tree(
        &guard,
        pending,
        bundle,
        parent,
        parent_id,
        "space-change",
        0,
    )
    .unwrap();
    let roles = bundle.stage_roles().unwrap();
    assert!(roles.len() > 1);
    let second = bundle
        .publication_member(roles.iter().nth(1).unwrap())
        .unwrap()
        .0;
    let ceiling = crate::customer::test_space::SpaceCeiling::new();
    let mut published = 0;
    let result = crate::customer::package_publish::inventory::publish_owned_all_with_hook(
        &guard,
        pending,
        bundle,
        &tree,
        signature,
        0,
        &mut |count| {
            published = count;
            if count == 1 {
                ceiling.set(Some(0));
            }
        },
    );
    ceiling.set(None);
    assert!(
        result
            .unwrap_err()
            .detail
            .contains("insufficient observed space")
    );
    assert_eq!(published, 1);
    assert!(!tree.path().join(second).exists());
    assert!(crate::customer::package_publish::publication_journal_present(&paths).unwrap());
    let resumed = crate::customer::package_publish::inventory::resume_owned_all(
        &guard, pending, bundle, &tree, signature, 0,
    )
    .unwrap();
    assert_eq!(resumed.len(), roles.len());
}
