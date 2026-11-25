// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_source/tests/inventory_publication_scenarios.rs

fn exercise_inventory_publication(
    root: &std::path::Path,
    stage_pending: &crate::customer::transition::PackagePending,
    bundle: &DeclaredBundle,
    roles: &std::collections::BTreeSet<String>,
) {
    use crate::customer::package_publish::{publication_journal_present, publish_member};
    use crate::customer::package_stage_journal::inventory::stage_all_members;
    use crate::customer::transition::{
        OperationOwner, TransitionPaths, acquire_transition, enroll_pending,
    };
    // Publish a full tree after an interrupted second member, preserving a collision.
    use crate::customer::package_publish::inventory::{publish_all, resume_all, verify_all};
    let multi_dir = root.join("multi-publication-stage");
    std::fs::create_dir(&multi_dir).unwrap();
    let multi_paths = TransitionPaths::under(&root.join("multi-publication-transaction"));
    let multi_guard = acquire_transition(OperationOwner::Package, &multi_paths).unwrap();
    enroll_pending(&multi_guard, stage_pending).unwrap();
    drop(stage_all_members(&multi_guard, stage_pending, bundle, &multi_dir, 0).unwrap());
    let multi_root = root.join("multi-publication-root");
    std::fs::create_dir(&multi_root).unwrap();
    for role in roles {
        let (destination, _, _) = bundle.publication_member(role).unwrap();
        std::fs::create_dir_all(multi_root.join(destination).parent().unwrap()).unwrap();
    }
    let meta = std::fs::metadata(&multi_root).unwrap();
    let multi_identity = (meta.dev(), meta.ino());
    assert!(
        verify_all(
            &multi_guard,
            stage_pending,
            bundle,
            &multi_root,
            multi_identity
        )
        .is_err()
    );
    assert!(
        publish_all(
            &multi_guard,
            stage_pending,
            bundle,
            &multi_root,
            multi_identity,
            u64::MAX
        )
        .is_err()
    );
    assert!(!publication_journal_present(&multi_paths).unwrap());
    let unexpected = multi_root.join("unexpected");
    std::fs::write(&unexpected, b"foreign").unwrap();
    assert!(
        publish_all(
            &multi_guard,
            stage_pending,
            bundle,
            &multi_root,
            multi_identity,
            0
        )
        .is_err()
    );
    assert!(!publication_journal_present(&multi_paths).unwrap());
    std::fs::remove_file(&unexpected).unwrap();
    let unexpected_dir = multi_root.join("unexpected-dir");
    std::fs::create_dir(&unexpected_dir).unwrap();
    assert!(
        publish_all(
            &multi_guard,
            stage_pending,
            bundle,
            &multi_root,
            multi_identity,
            0
        )
        .is_err()
    );
    std::fs::remove_dir(&unexpected_dir).unwrap();
    let redirected = multi_root.join("redirected");
    std::os::unix::fs::symlink(multi_root.join("bin"), &redirected).unwrap();
    assert!(
        publish_all(
            &multi_guard,
            stage_pending,
            bundle,
            &multi_root,
            multi_identity,
            0
        )
        .is_err()
    );
    std::fs::remove_file(redirected).unwrap();
    let last_role = roles.last().unwrap();
    let collision = multi_root.join(bundle.publication_member(last_role).unwrap().0);
    std::fs::write(&collision, b"foreign").unwrap();
    assert!(
        publish_all(
            &multi_guard,
            stage_pending,
            bundle,
            &multi_root,
            multi_identity,
            0
        )
        .is_err()
    );
    assert_eq!(std::fs::read(&collision).unwrap(), b"foreign");
    assert!(!publication_journal_present(&multi_paths).unwrap());
    std::fs::remove_file(&collision).unwrap();
    let mut role_iter = roles.iter();
    let first = role_iter.next().unwrap();
    let second = role_iter.next().unwrap();
    publish_member(
        &multi_guard,
        stage_pending,
        bundle,
        &multi_root,
        multi_identity,
        first,
    )
    .unwrap();
    let first_path = multi_root.join(bundle.publication_member(first).unwrap().0);
    let first_journal = multi_paths.transactions_root().join("package-publish.json");
    let first_history = std::fs::read_to_string(&first_journal).unwrap();
    let mut altered_history = String::new();
    for line in first_history.lines() {
        let mut row: serde_json::Value = serde_json::from_str(line).unwrap();
        row["stage_identity"][1] = serde_json::json!(u64::MAX);
        altered_history.push_str(&format!("{row}\n"));
    }
    std::fs::write(&first_journal, altered_history).unwrap();
    assert!(
        publish_member(
            &multi_guard,
            stage_pending,
            bundle,
            &multi_root,
            multi_identity,
            second
        )
        .is_err()
    );
    std::fs::write(&first_journal, first_history).unwrap();
    let first_contents = std::fs::read(&first_path).unwrap();
    std::fs::write(&first_path, b"tampered").unwrap();
    assert!(
        publish_member(
            &multi_guard,
            stage_pending,
            bundle,
            &multi_root,
            multi_identity,
            second
        )
        .is_err()
    );
    let second_path = multi_root.join(bundle.publication_member(second).unwrap().0);
    assert!(!second_path.exists());
    std::fs::write(&first_path, &first_contents).unwrap();
    assert!(
        crate::customer::package_publish::publish_with_hook(
            &multi_guard,
            stage_pending,
            bundle,
            &multi_root,
            multi_identity,
            second,
            || Err(crate::customer::transition::recovery_refusal(
                "test interruption"
            )),
        )
        .is_err()
    );
    let journal = multi_paths.transactions_root().join("package-publish.json");
    let before = std::fs::read(&journal).unwrap();
    assert!(
        resume_all(
            &multi_guard,
            stage_pending,
            bundle,
            &multi_root,
            multi_identity,
            u64::MAX
        )
        .is_err()
    );
    assert_eq!(std::fs::read(&journal).unwrap(), before);
    std::fs::write(&first_path, b"tampered").unwrap();
    assert!(
        resume_all(
            &multi_guard,
            stage_pending,
            bundle,
            &multi_root,
            multi_identity,
            0
        )
        .is_err()
    );
    assert_eq!(std::fs::read(&journal).unwrap(), before);
    std::fs::write(&first_path, &first_contents).unwrap();
    let published = resume_all(
        &multi_guard,
        stage_pending,
        bundle,
        &multi_root,
        multi_identity,
        0,
    )
    .unwrap();
    assert_eq!(published.len(), roles.len());
    drop(published);
    assert_eq!(
        verify_all(
            &multi_guard,
            stage_pending,
            bundle,
            &multi_root,
            multi_identity
        )
        .unwrap()
        .len(),
        roles.len()
    );
    let complete_history = std::fs::read_to_string(&journal).unwrap();
    let mut changed_history = String::new();
    for line in complete_history.lines() {
        let mut row: serde_json::Value = serde_json::from_str(line).unwrap();
        if row["role"] == *first {
            row["stage_identity"][1] = serde_json::json!(u64::MAX);
        }
        changed_history.push_str(&format!("{row}\n"));
    }
    std::fs::write(&journal, changed_history).unwrap();
    assert!(
        verify_all(
            &multi_guard,
            stage_pending,
            bundle,
            &multi_root,
            multi_identity
        )
        .is_err()
    );
    std::fs::write(&journal, complete_history).unwrap();
    let outside_link = root.join("published-outside-link");
    std::fs::hard_link(&first_path, &outside_link).unwrap();
    assert!(
        verify_all(
            &multi_guard,
            stage_pending,
            bundle,
            &multi_root,
            multi_identity
        )
        .is_err()
    );
    std::fs::remove_file(&outside_link).unwrap();
    let extra = multi_root.join("extra-file");
    std::fs::write(&extra, b"foreign").unwrap();
    assert!(
        verify_all(
            &multi_guard,
            stage_pending,
            bundle,
            &multi_root,
            multi_identity
        )
        .is_err()
    );
    std::fs::remove_file(extra).unwrap();
    std::fs::write(&first_path, b"changed").unwrap();
    assert!(
        verify_all(
            &multi_guard,
            stage_pending,
            bundle,
            &multi_root,
            multi_identity
        )
        .is_err()
    );
    drop(multi_guard);

    let fresh_dir = root.join("fresh-publication-stage");
    std::fs::create_dir(&fresh_dir).unwrap();
    let fresh_paths = TransitionPaths::under(&root.join("fresh-publication-transaction"));
    let fresh_guard = acquire_transition(OperationOwner::Package, &fresh_paths).unwrap();
    enroll_pending(&fresh_guard, stage_pending).unwrap();
    drop(stage_all_members(&fresh_guard, stage_pending, bundle, &fresh_dir, 0).unwrap());
    let fresh_root = root.join("fresh-publication-root");
    std::fs::create_dir(&fresh_root).unwrap();
    for role in roles {
        let (destination, _, _) = bundle.publication_member(role).unwrap();
        std::fs::create_dir_all(fresh_root.join(destination).parent().unwrap()).unwrap();
    }
    let meta = std::fs::metadata(&fresh_root).unwrap();
    let fresh_identity = (meta.dev(), meta.ino());
    let fresh = publish_all(
        &fresh_guard,
        stage_pending,
        bundle,
        &fresh_root,
        fresh_identity,
        0,
    )
    .unwrap();
    assert_eq!(fresh.len(), roles.len());
    drop(fresh);
    drop(fresh_guard);
}
