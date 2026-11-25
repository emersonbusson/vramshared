// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_source/tests/tree_scenarios.rs

fn exercise_tree_publication(
    root: &std::path::Path,
    stage_pending: &crate::customer::transition::PackagePending,
    bundle: &DeclaredBundle,
    signature: &crate::customer::package_trust::VerifiedSignature,
    wrong_signature: &crate::customer::package_trust::VerifiedSignature,
) -> (std::path::PathBuf, (u64, u64)) {
    use crate::customer::package_stage_journal::inventory::{
        resume_staged_inventory, stage_all_members,
    };
    use crate::customer::transition::{
        OperationOwner, TransitionPaths, acquire_transition, enroll_pending,
    };
    // A sealed tree must reject unowned entries before it can be published.
    let tree_stage = root.join("tree-test-stage");
    std::fs::create_dir(&tree_stage).unwrap();
    let tree_paths = TransitionPaths::under(&root.join("tree-test-transaction"));
    let tree_guard = acquire_transition(OperationOwner::Package, &tree_paths).unwrap();
    enroll_pending(&tree_guard, stage_pending).unwrap();
    drop(stage_all_members(&tree_guard, stage_pending, bundle, &tree_stage, 0).unwrap());
    let tree_parent = root.join("tree-test-parent");
    std::fs::create_dir(&tree_parent).unwrap();
    let meta = std::fs::metadata(&tree_parent).unwrap();
    let tree_parent_id = (meta.dev(), meta.ino());
    let selector_journal = tree_paths.transactions_root().join("package-selector.json");
    std::fs::write(&selector_journal, b"interrupted\n").unwrap();
    std::fs::set_permissions(
        &selector_journal,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();
    assert!(
        crate::customer::package_tree::create_tree(
            &tree_guard,
            stage_pending,
            bundle,
            &tree_parent,
            tree_parent_id,
            "release",
            0
        )
        .is_err()
    );
    assert!(
        crate::customer::package_tree::resume_tree(
            &tree_guard,
            stage_pending,
            bundle,
            &tree_parent,
            tree_parent_id,
            "release",
            0
        )
        .is_err()
    );
    assert!(!tree_parent.join("release").exists());
    std::fs::remove_file(selector_journal).unwrap();
    // Reject an unattainable reserve before either the journal or root exists.
    assert!(
        crate::customer::package_tree::create_tree(
            &tree_guard,
            stage_pending,
            bundle,
            &tree_parent,
            tree_parent_id,
            "release",
            u64::MAX,
        )
        .is_err()
    );
    assert!(!tree_parent.join("release").exists());
    assert!(!crate::customer::package_tree::tree_journal_present(&tree_paths).unwrap());
    let owned = crate::customer::package_tree::create_tree(
        &tree_guard,
        stage_pending,
        bundle,
        &tree_parent,
        tree_parent_id,
        "release",
        0,
    )
    .unwrap();
    let extra = owned.path().join("foreign");
    std::fs::write(&extra, b"keep").unwrap();
    assert!(
        crate::customer::package_tree::verify_tree(
            &tree_guard,
            stage_pending,
            bundle,
            &tree_parent,
            tree_parent_id,
            "release",
        )
        .is_err()
    );
    assert_eq!(std::fs::read(&extra).unwrap(), b"keep");
    std::fs::remove_file(extra).unwrap();
    assert!(resume_staged_inventory(&tree_guard, stage_pending, bundle, &tree_stage, 0).is_err());
    // Publication must first revalidate the durable tree, including every
    // recorded directory rather than accepting a caller-supplied inode.
    assert!(
        crate::customer::package_publish::inventory::publish_owned_all(
            &tree_guard,
            stage_pending,
            bundle,
            &owned,
            wrong_signature,
            0,
        )
        .is_err()
    );
    assert!(!crate::customer::package_publish::publication_journal_present(&tree_paths).unwrap());
    let bin = owned.path().join("bin");
    let saved_bin = owned.path().join("saved-bin");
    std::fs::rename(&bin, &saved_bin).unwrap();
    std::fs::create_dir(&bin).unwrap();
    std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    assert!(
        crate::customer::package_publish::inventory::publish_owned_all(
            &tree_guard,
            stage_pending,
            bundle,
            &owned,
            signature,
            0,
        )
        .is_err()
    );
    assert!(!bin.join("ramshared").exists());
    std::fs::remove_dir(&bin).unwrap();
    std::fs::rename(&saved_bin, &bin).unwrap();
    let published = crate::customer::package_publish::inventory::publish_owned_all(
        &tree_guard,
        stage_pending,
        bundle,
        &owned,
        signature,
        0,
    )
    .unwrap();
    assert_eq!(published.len(), bundle.stage_roles().unwrap().len());
    assert_eq!(
        std::fs::read(bin.join("ramshared")).unwrap(),
        fixture_elf(62)
    );
    drop(tree_guard);

    (tree_parent, tree_parent_id)
}
