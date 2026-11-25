// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_source/tests/reconciliation_scenarios.rs

fn exercise_missing_publication_reconciliation(
    root: &std::path::Path,
    tree_parent: &std::path::Path,
    tree_parent_id: (u64, u64),
    stage_pending: &crate::customer::transition::PackagePending,
    bundle: &DeclaredBundle,
    signature: &crate::customer::package_trust::VerifiedSignature,
    wrong_signature: &crate::customer::package_trust::VerifiedSignature,
) {
    use crate::customer::package_stage_journal::inventory::stage_all_members;
    use crate::customer::transition::{
        OperationOwner, TransitionPaths, acquire_transition, enroll_pending,
    };
    // Detect a change after intent is persisted, immediately before link.
    let boundary_stage = root.join("tree-boundary-stage");
    std::fs::create_dir(&boundary_stage).unwrap();
    let boundary_paths = TransitionPaths::under(&root.join("tree-boundary-transaction"));
    let boundary_guard = acquire_transition(OperationOwner::Package, &boundary_paths).unwrap();
    enroll_pending(&boundary_guard, stage_pending).unwrap();
    drop(stage_all_members(&boundary_guard, stage_pending, bundle, &boundary_stage, 0).unwrap());
    let boundary_tree = crate::customer::package_tree::create_tree(
        &boundary_guard,
        stage_pending,
        bundle,
        tree_parent,
        tree_parent_id,
        "boundary",
        0,
    )
    .unwrap();
    let first_role = bundle.stage_roles().unwrap().first().unwrap().clone();
    let (destination, _, _) = bundle.publication_member(&first_role).unwrap();
    assert!(
        crate::customer::package_publish::publish_owned_with_before_link(
            &boundary_guard,
            stage_pending,
            bundle,
            &boundary_tree,
            &first_role,
            || {
                std::fs::set_permissions(
                    boundary_tree.path(),
                    std::os::unix::fs::PermissionsExt::from_mode(0o755),
                )
                .unwrap();
                Ok(())
            },
        )
        .is_err()
    );
    assert!(!boundary_tree.path().join(&destination).exists());
    std::fs::set_permissions(
        boundary_tree.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let review = crate::customer::package_publish::reconciliation::observe_missing(
        &boundary_guard,
        stage_pending,
        bundle,
        &boundary_tree,
        signature,
    )
    .unwrap()
    .unwrap();
    assert_eq!(review.fingerprint().len(), 64);
    let again = crate::customer::package_publish::reconciliation::observe_missing(
        &boundary_guard,
        stage_pending,
        bundle,
        &boundary_tree,
        signature,
    )
    .unwrap()
    .unwrap();
    assert_eq!(review.fingerprint(), again.fingerprint());
    assert!(
        crate::customer::package_publish::reconciliation::observe_missing(
            &boundary_guard,
            stage_pending,
            bundle,
            &boundary_tree,
            wrong_signature,
        )
        .is_err()
    );
    let refusal = crate::customer::package_publish::inventory::resume_owned_all(
        &boundary_guard,
        stage_pending,
        bundle,
        &boundary_tree,
        signature,
        0,
    )
    .unwrap_err();
    assert!(refusal.detail.contains(review.fingerprint()));
    assert!(refusal.detail.contains("not an approval"));
    assert!(
        refusal
            .detail
            .contains(&format!("{:?}", boundary_tree.path().join(&destination)))
    );
    assert!(refusal.detail.contains(&first_role));
    assert!(refusal.detail.contains(&stage_pending.new_hash));
    assert!(!boundary_tree.path().join(&destination).exists());
    let boundary_journal = boundary_paths
        .transactions_root()
        .join("package-publish.json");
    let boundary_history = std::fs::read_to_string(&boundary_journal).unwrap();
    let mut altered: serde_json::Value = serde_json::from_str(boundary_history.trim()).unwrap();
    altered["creation_nonce"][0] = serde_json::json!(255);
    if serde_json::from_str::<serde_json::Value>(boundary_history.trim()).unwrap()["creation_nonce"]
        [0]
        == 255
    {
        altered["creation_nonce"][0] = serde_json::json!(254);
    }
    std::fs::write(&boundary_journal, format!("{altered}\n")).unwrap();
    let changed = crate::customer::package_publish::reconciliation::observe_missing(
        &boundary_guard,
        stage_pending,
        bundle,
        &boundary_tree,
        signature,
    )
    .unwrap()
    .unwrap();
    assert_ne!(review.fingerprint(), changed.fingerprint());
    std::fs::write(&boundary_journal, boundary_history).unwrap();
    let stage_journal = boundary_paths
        .transactions_root()
        .join("package-stage.json");
    let stage_history = std::fs::read_to_string(&stage_journal).unwrap();
    let mut changed_stage_history = String::new();
    for line in stage_history.lines() {
        let mut row: serde_json::Value = serde_json::from_str(line).unwrap();
        if row["role"] == first_role {
            row["limit"] = serde_json::json!(row["limit"].as_u64().unwrap() + 1);
        }
        changed_stage_history.push_str(&format!("{row}\n"));
    }
    std::fs::write(&stage_journal, changed_stage_history).unwrap();
    let changed_stage = crate::customer::package_publish::reconciliation::observe_missing(
        &boundary_guard,
        stage_pending,
        bundle,
        &boundary_tree,
        signature,
    );
    assert!(changed_stage.is_err());
    std::fs::write(&stage_journal, stage_history).unwrap();
    std::fs::write(boundary_tree.path().join(&destination), b"foreign").unwrap();
    assert!(
        crate::customer::package_publish::reconciliation::observe_missing(
            &boundary_guard,
            stage_pending,
            bundle,
            &boundary_tree,
            signature,
        )
        .unwrap()
        .is_none()
    );
    std::fs::remove_file(boundary_tree.path().join(&destination)).unwrap();
    drop(boundary_guard);

    // A prior published member must remain valid through the observation.
    let prior_stage = root.join("tree-prior-review-stage");
    std::fs::create_dir(&prior_stage).unwrap();
    let prior_paths = TransitionPaths::under(&root.join("tree-prior-review-transaction"));
    let prior_guard = acquire_transition(OperationOwner::Package, &prior_paths).unwrap();
    enroll_pending(&prior_guard, stage_pending).unwrap();
    drop(stage_all_members(&prior_guard, stage_pending, bundle, &prior_stage, 0).unwrap());
    let prior_tree = crate::customer::package_tree::create_tree(
        &prior_guard,
        stage_pending,
        bundle,
        tree_parent,
        tree_parent_id,
        "prior-review",
        0,
    )
    .unwrap();
    let ordered_roles = bundle.stage_roles().unwrap();
    let mut role_iter = ordered_roles.iter();
    let prior_role = role_iter.next().unwrap();
    let missing_role = role_iter.next().unwrap();
    crate::customer::package_publish::publish_owned_member(
        &prior_guard,
        stage_pending,
        bundle,
        &prior_tree,
        prior_role,
        signature,
    )
    .unwrap();
    assert!(
        crate::customer::package_publish::publish_signed_owned_with_before_link(
            &prior_guard,
            stage_pending,
            bundle,
            &prior_tree,
            missing_role,
            signature,
            || Err(crate::customer::transition::recovery_refusal(
                "test interruption"
            )),
        )
        .is_err()
    );
    let prior_path = prior_tree
        .path()
        .join(bundle.publication_member(prior_role).unwrap().0);
    let prior_bytes = std::fs::read(&prior_path).unwrap();
    assert!(
        crate::customer::package_publish::reconciliation::observe_missing_with_hook(
            &prior_guard,
            stage_pending,
            bundle,
            &prior_tree,
            signature,
            || std::fs::write(&prior_path, b"changed").unwrap(),
        )
        .is_err()
    );
    std::fs::write(&prior_path, prior_bytes).unwrap();
    assert!(
        crate::customer::package_publish::reconciliation::observe_missing(
            &prior_guard,
            stage_pending,
            bundle,
            &prior_tree,
            signature,
        )
        .unwrap()
        .is_some()
    );
    drop(prior_guard);
}
