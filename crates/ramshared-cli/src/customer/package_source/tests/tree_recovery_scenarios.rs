// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_source/tests/tree_recovery_scenarios.rs

fn exercise_tree_recovery(
    root: &std::path::Path,
    tree_parent: &std::path::Path,
    tree_parent_id: (u64, u64),
    stage_pending: &crate::customer::transition::PackagePending,
    bundle: &DeclaredBundle,
    signature: &crate::customer::package_trust::VerifiedSignature,
    checksums: &str,
) {
    use crate::customer::package_stage_journal::inventory::stage_all_members;
    use crate::customer::transition::{
        OperationOwner, TransitionPaths, acquire_transition, enroll_pending,
    };
    // A crash after sealing a directory must resume only the same tree.
    let resume_stage = root.join("tree-resume-stage");
    std::fs::create_dir(&resume_stage).unwrap();
    let resume_paths = TransitionPaths::under(&root.join("tree-resume-transaction"));
    let resume_guard = acquire_transition(OperationOwner::Package, &resume_paths).unwrap();
    enroll_pending(&resume_guard, stage_pending).unwrap();
    drop(stage_all_members(&resume_guard, stage_pending, bundle, &resume_stage, 0).unwrap());
    assert!(
        crate::customer::package_tree::continue_tree(
            &resume_guard,
            stage_pending,
            bundle,
            (tree_parent, tree_parent_id, 0),
            "resume",
            false,
            &mut |_| Err(crate::customer::transition::recovery_refusal(
                "injected interruption"
            )),
        )
        .is_err()
    );
    assert!(tree_parent.join("resume").is_dir());
    // Space failure on resume retains the already sealed root.
    assert!(
        crate::customer::package_tree::resume_tree(
            &resume_guard,
            stage_pending,
            bundle,
            tree_parent,
            tree_parent_id,
            "resume",
            u64::MAX,
        )
        .is_err()
    );
    let resume_root = tree_parent.join("resume");
    let saved_root = tree_parent.join("saved-resume");
    std::fs::rename(&resume_root, &saved_root).unwrap();
    std::fs::create_dir(&resume_root).unwrap();
    std::fs::set_permissions(
        &resume_root,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    assert!(
        crate::customer::package_tree::resume_tree(
            &resume_guard,
            stage_pending,
            bundle,
            tree_parent,
            tree_parent_id,
            "resume",
            0,
        )
        .is_err()
    );
    assert!(resume_root.is_dir());
    std::fs::remove_dir(&resume_root).unwrap();
    std::fs::rename(&saved_root, &resume_root).unwrap();
    let recovered = crate::customer::package_tree::resume_tree(
        &resume_guard,
        stage_pending,
        bundle,
        tree_parent,
        tree_parent_id,
        "resume",
        0,
    )
    .unwrap();
    assert_eq!(recovered.path(), tree_parent.join("resume"));
    assert!(
        crate::customer::package_tree::resume_tree(
            &resume_guard,
            stage_pending,
            bundle,
            tree_parent,
            tree_parent_id,
            "different",
            0,
        )
        .is_err()
    );
    let first_role = bundle.stage_roles().unwrap().first().unwrap().clone();
    assert!(
        crate::customer::package_publish::publish_with_hook(
            &resume_guard,
            stage_pending,
            bundle,
            recovered.path(),
            recovered.identity(),
            &first_role,
            || Err(crate::customer::transition::recovery_refusal(
                "injected publication interruption"
            )),
        )
        .is_err()
    );
    drop(resume_guard);
    let resume_guard = acquire_transition(OperationOwner::Package, &resume_paths).unwrap();
    let recovered = crate::customer::package_tree::reopen_owned_tree(
        &resume_guard,
        stage_pending,
        bundle,
        tree_parent,
        tree_parent_id,
        "resume",
    )
    .unwrap();
    let resumed = crate::customer::package_publish::inventory::resume_owned_all(
        &resume_guard,
        stage_pending,
        bundle,
        &recovered,
        signature,
        0,
    )
    .unwrap();
    assert_eq!(resumed.len(), bundle.stage_roles().unwrap().len());
    drop(resume_guard);

    // Terminate an independent process after sealing and before confirmation.
    let kill_stage = root.join("tree-kill-stage");
    std::fs::create_dir(&kill_stage).unwrap();
    let kill_paths = TransitionPaths::under(&root.join("tree-kill-transaction"));
    let kill_guard = acquire_transition(OperationOwner::Package, &kill_paths).unwrap();
    enroll_pending(&kill_guard, stage_pending).unwrap();
    drop(stage_all_members(&kill_guard, stage_pending, bundle, &kill_stage, 0).unwrap());
    drop(kill_guard);
    std::fs::write(root.join("SHA256SUMS"), checksums).unwrap();
    let mut child = std::process::Command::new("/proc/self/exe")
        .args([
            "--exact",
            "customer::package_source::tests::tree_kill_child",
            "--ignored",
            "--nocapture",
        ])
        .env("RAMSHARED_TREE_CHILD_DIR", root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            if line.contains("RAMSHARED_TREE_READY") {
                let _ = send.send(());
                break;
            }
        }
    });
    let ready = receive.recv_timeout(std::time::Duration::from_secs(15));
    let killed = child.kill();
    let status = child.wait();
    reader.join().unwrap();
    assert!(ready.is_ok(), "tree child never reached the sealed phase");
    assert!(killed.is_ok());
    assert!(!status.unwrap().success());
    let kill_guard = acquire_transition(OperationOwner::Package, &kill_paths).unwrap();
    let killed_metadata = std::fs::metadata(tree_parent.join("killed")).unwrap();
    let killed_identity = (killed_metadata.dev(), killed_metadata.ino());
    let recovered = crate::customer::package_tree::resume_tree(
        &kill_guard,
        stage_pending,
        bundle,
        tree_parent,
        tree_parent_id,
        "killed",
        0,
    )
    .unwrap();
    assert_eq!(recovered.path(), tree_parent.join("killed"));
    assert_eq!(recovered.identity(), killed_identity);
    let tree_journal = kill_paths.transactions_root().join("package-tree.json");
    let intact_journal = std::fs::read(&tree_journal).unwrap();
    let mut damaged = intact_journal.clone();
    damaged.extend_from_slice(b"{broken");
    std::fs::write(&tree_journal, &damaged).unwrap();
    assert!(
        crate::customer::package_tree::verify_tree(
            &kill_guard,
            stage_pending,
            bundle,
            tree_parent,
            tree_parent_id,
            "killed",
        )
        .is_err()
    );
    assert_eq!(std::fs::read(&tree_journal).unwrap(), damaged);
    std::fs::write(&tree_journal, &intact_journal).unwrap();
    assert!(
        crate::customer::package_tree::verify_tree(
            &kill_guard,
            stage_pending,
            bundle,
            tree_parent,
            tree_parent_id,
            "killed",
        )
        .is_ok()
    );
    drop(kill_guard);
}
