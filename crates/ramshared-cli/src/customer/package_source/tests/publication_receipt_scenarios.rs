// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_source/tests/publication_receipt_scenarios.rs

fn exercise_publication_receipt(
    root: &std::path::Path,
    stage_paths: &crate::customer::transition::TransitionPaths,
    stage_dir: &std::path::Path,
    stage_guard: crate::customer::transition::CustomerTransitionGuard,
    stage_pending: &crate::customer::transition::PackagePending,
    bundle: &mut DeclaredBundle,
    checksums: &str,
) {
    use crate::customer::package_stage_journal::inventory::verify_staged_inventory;
    use crate::customer::transition::{OperationOwner, acquire_transition};
    // Exercise the descriptor-held publication and its interrupted receipt.
    use crate::customer::package_publish::{
        publication_journal_present, publish_member, resume_publication,
    };
    use crate::customer::transition::clear_pending;
    use std::os::unix::fs::MetadataExt;
    let release_root = root.join("publication-root");
    std::fs::create_dir(&release_root).unwrap();
    std::fs::create_dir(release_root.join("bin")).unwrap();
    let meta = std::fs::metadata(&release_root).unwrap();
    let release_identity = (meta.dev(), meta.ino());
    let destination = release_root.join("bin/ramshared");
    std::fs::write(&destination, b"foreign").unwrap();
    assert!(
        publish_member(
            &stage_guard,
            stage_pending,
            &*bundle,
            &release_root,
            release_identity,
            "ramshared"
        )
        .is_err()
    );
    assert_eq!(std::fs::read(&destination).unwrap(), b"foreign");
    assert!(!publication_journal_present(stage_paths).unwrap());
    std::fs::remove_file(&destination).unwrap();
    let bin_path = release_root.join("bin");
    let saved_bin = release_root.join("saved-bin");
    std::fs::rename(&bin_path, &saved_bin).unwrap();
    std::os::unix::fs::symlink(&saved_bin, &bin_path).unwrap();
    assert!(
        publish_member(
            &stage_guard,
            stage_pending,
            &*bundle,
            &release_root,
            release_identity,
            "ramshared"
        )
        .is_err()
    );
    assert!(!saved_bin.join("ramshared").exists());
    std::fs::remove_file(&bin_path).unwrap();
    std::fs::rename(&saved_bin, &bin_path).unwrap();
    let member = bundle
        .payload
        .contents
        .iter_mut()
        .find(|member| member.role == "ramshared")
        .unwrap();
    let original_mode = std::mem::replace(&mut member.mode, "0777".into());
    assert!(
        publish_member(
            &stage_guard,
            stage_pending,
            &*bundle,
            &release_root,
            release_identity,
            "ramshared"
        )
        .is_err()
    );
    bundle
        .payload
        .contents
        .iter_mut()
        .find(|member| member.role == "ramshared")
        .unwrap()
        .mode = original_mode;
    assert!(!publication_journal_present(stage_paths).unwrap());
    assert!(
        publish_member(
            &stage_guard,
            stage_pending,
            &*bundle,
            &release_root,
            (release_identity.0, release_identity.1 + 1),
            "ramshared"
        )
        .is_err()
    );
    assert!(
        publish_member(
            &stage_guard,
            stage_pending,
            &*bundle,
            &release_root,
            release_identity,
            "unknown"
        )
        .is_err()
    );
    assert!(!publication_journal_present(stage_paths).unwrap());
    // Kill a child after the name exists but before completion is recorded.
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::time::Duration;
    std::fs::write(root.join("SHA256SUMS"), checksums).unwrap();
    drop(stage_guard);
    let mut child = Command::new("/proc/self/exe")
        .args([
            "--exact",
            "customer::package_source::tests::publication_kill_child",
            "--ignored",
            "--nocapture",
        ])
        .env("RAMSHARED_PUBLICATION_CHILD_DIR", root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (send, receive) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line.contains("RAMSHARED_PUBLICATION_READY") {
                let _ = send.send(());
                break;
            }
        }
    });
    let ready = receive.recv_timeout(Duration::from_secs(15));
    let killed = child.kill();
    let status = child.wait();
    reader.join().unwrap();
    assert!(
        ready.is_ok(),
        "publication child never reached the linked phase"
    );
    assert!(killed.is_ok());
    assert!(!status.unwrap().success());
    let stage_guard = acquire_transition(OperationOwner::Package, stage_paths).unwrap();
    assert_eq!(std::fs::read(&destination).unwrap(), fixture_elf(62));
    assert_eq!(
        std::fs::metadata(&destination).unwrap().mode() & 0o7777,
        0o755
    );
    assert!(publication_journal_present(stage_paths).unwrap());
    let publication_path = stage_paths.transactions_root().join("package-publish.json");
    let publication_journal = std::fs::read_to_string(&publication_path).unwrap();
    let mut intent: serde_json::Value =
        serde_json::from_str(publication_journal.lines().next().unwrap()).unwrap();
    assert!(intent["stage_identity"].is_array());
    assert!(intent["stage_parent_identity"].is_array());
    intent["stage_identity"][1] = serde_json::json!(u64::MAX);
    std::fs::write(&publication_path, format!("{intent}\n")).unwrap();
    assert!(resume_publication(&stage_guard, stage_pending, &*bundle).is_err());
    std::fs::write(&publication_path, &publication_journal).unwrap();
    std::fs::write(&destination, vec![0u8; 64]).unwrap();
    assert!(resume_publication(&stage_guard, stage_pending, &*bundle).is_err());
    std::fs::write(&destination, fixture_elf(62)).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(resume_publication(&stage_guard, stage_pending, &*bundle).is_err());
    std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o755)).unwrap();
    let created = std::fs::File::open(&destination).unwrap();
    let mut creation_nonce = [0u8; 32];
    assert_eq!(
        rustix::fs::fgetxattr(
            &created,
            "user.ramshared.publication",
            creation_nonce.as_mut_slice()
        )
        .unwrap(),
        32
    );
    rustix::fs::fsetxattr(
        &created,
        "user.ramshared.publication",
        &[0u8; 32],
        rustix::fs::XattrFlags::REPLACE,
    )
    .unwrap();
    assert!(resume_publication(&stage_guard, stage_pending, &*bundle).is_err());
    rustix::fs::fsetxattr(
        &created,
        "user.ramshared.publication",
        &creation_nonce,
        rustix::fs::XattrFlags::REPLACE,
    )
    .unwrap();
    drop(created);
    let extra_link = release_root.join("bin/extra-link");
    std::fs::hard_link(&destination, &extra_link).unwrap();
    assert!(resume_publication(&stage_guard, stage_pending, &*bundle).is_err());
    std::fs::remove_file(extra_link).unwrap();
    let saved = release_root.join("bin/original");
    std::fs::rename(&destination, &saved).unwrap();
    let missing = resume_publication(&stage_guard, stage_pending, &*bundle).unwrap_err();
    assert!(missing.detail.contains("effect may have occurred"));
    assert_eq!(std::fs::read(&saved).unwrap(), fixture_elf(62));
    assert!(publication_journal_present(stage_paths).unwrap());
    std::fs::write(&destination, b"replacement").unwrap();
    assert!(resume_publication(&stage_guard, stage_pending, &*bundle).is_err());
    assert_eq!(std::fs::read(&destination).unwrap(), b"replacement");
    std::fs::remove_file(&destination).unwrap();
    std::fs::rename(&saved, &destination).unwrap();
    resume_publication(&stage_guard, stage_pending, &*bundle).unwrap();
    assert!(resume_publication(&stage_guard, stage_pending, &*bundle).is_err());
    let stage_journal = stage_paths.transactions_root().join("package-stage.json");
    let saved_journal = stage_paths.transactions_root().join("saved-stage.json");
    std::fs::rename(&stage_journal, &saved_journal).unwrap();
    assert!(clear_pending(&stage_guard, stage_pending).is_err());
    let pending_path = stage_paths.transactions_root().join("package-pending.json");
    let saved_pending = stage_paths.transactions_root().join("saved-pending.json");
    std::fs::rename(&pending_path, &saved_pending).unwrap();
    drop(stage_guard);
    assert!(acquire_transition(OperationOwner::Up, stage_paths).is_err());
    let stage_guard = acquire_transition(OperationOwner::Package, stage_paths).unwrap();
    std::fs::rename(&saved_pending, &pending_path).unwrap();
    std::fs::rename(&saved_journal, &stage_journal).unwrap();
    std::fs::write(stage_dir.join("member-0"), b"tampered").unwrap();
    assert!(verify_staged_inventory(&stage_guard, stage_pending, &*bundle).is_err());
    drop(stage_guard);
}
