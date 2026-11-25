// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_selector/intent/tests.rs
#![allow(clippy::unwrap_used)]

use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::symlink;

use super::*;
use crate::customer::transition::{
    OperationOwner, TransitionPaths, acquire_transition, enroll_pending,
};

fn fixture(name: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("ramshared-selector-{name}-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    root
}

#[test]
fn previous_selector_rejects_unsafe_and_unavailable_targets() {
    let root = fixture("previous");
    let parent = File::open(&root).unwrap();
    let leaf = OsStr::new("current");
    assert!(previous(&parent, &root, leaf).unwrap().record.is_none());
    std::fs::write(root.join(leaf), b"foreign").unwrap();
    assert!(previous(&parent, &root, leaf).is_err());
    std::fs::remove_file(root.join(leaf)).unwrap();
    for target in ["../outside", "/absolute", "a//b", "./relative", "missing"] {
        symlink(target, root.join(leaf)).unwrap();
        assert!(previous(&parent, &root, leaf).is_err(), "{target}");
        std::fs::remove_file(root.join(leaf)).unwrap();
    }
    let invalid = std::ffi::OsString::from_vec(vec![0xff]);
    symlink(invalid, root.join(leaf)).unwrap();
    assert!(previous(&parent, &root, leaf).is_err());
    std::fs::remove_file(root.join(leaf)).unwrap();
    std::fs::create_dir(root.join("old")).unwrap();
    symlink("old", root.join(leaf)).unwrap();
    let snapshot = previous(&parent, &root, leaf).unwrap();
    assert_eq!(snapshot.record.unwrap().target, Path::new("old"));
    assert!(snapshot.entry.is_some() && snapshot.root.is_some());
    assert!(parent_matches(&root, &parent, identity(&parent).unwrap()).unwrap());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn journal_write_is_exclusive_and_detects_same_inode_changes() {
    let root = fixture("journal");
    let paths = TransitionPaths::under(&root);
    let guard = acquire_transition(OperationOwner::Package, &paths).unwrap();
    let pending = PackagePending {
        target_release: Some("v1".into()),
        action: PackageAction::Install,
        old_hash: "a".repeat(64),
        new_hash: "b".repeat(64),
        issued_unix: 1,
    };
    enroll_pending(&guard, &pending).unwrap();
    let record = Intent {
        schema: 1,
        pending,
        selector: root.join("current"),
        parent_identity: (1, 2),
        target: PathBuf::from("releases/v1"),
        target_identity: (1, 3),
        previous: None,
    };
    let journal = persist(&guard, &record).unwrap();
    assert!(verify_journal(&guard, &record, &journal).is_ok());
    assert!(persist(&guard, &record).is_err());
    std::fs::write(
        paths.transactions_root().join(super::super::JOURNAL),
        b"changed\n",
    )
    .unwrap();
    assert!(verify_journal(&guard, &record, &journal).is_err());
    assert!(super::super::journal_present(&paths).unwrap());
    drop(journal);
    drop(guard);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "subprocess fixture invoked by interrupted_intent_preserves_evidence"]
fn selector_intent_kill_child() {
    use std::io::{Read, Write};
    let Ok(root) = std::env::var("RAMSHARED_SELECTOR_TEST_ROOT") else {
        return;
    };
    let phase = std::env::var("RAMSHARED_SELECTOR_TEST_PHASE").unwrap();
    let root = PathBuf::from(root);
    let paths = TransitionPaths::under(&root);
    let guard = acquire_transition(OperationOwner::Package, &paths).unwrap();
    let pending = PackagePending {
        target_release: Some("v1".into()),
        action: PackageAction::Install,
        old_hash: "a".repeat(64),
        new_hash: "b".repeat(64),
        issued_unix: 1,
    };
    enroll_pending(&guard, &pending).unwrap();
    let record = Intent {
        schema: 1,
        pending,
        selector: root.join("current"),
        parent_identity: (1, 2),
        target: PathBuf::from("releases/v1"),
        target_identity: (1, 3),
        previous: None,
    };
    if phase == "write_failure" {
        rustix::process::setrlimit(
            rustix::process::Resource::Fsize,
            rustix::process::Rlimit {
                current: Some(0),
                maximum: Some(0),
            },
        )
        .unwrap();
    }
    let wait = |at: &str| {
        if phase == at {
            println!("RAMSHARED_SELECTOR_INTENT_READY");
            std::io::stdout().flush().unwrap();
            let mut byte = [0u8; 1];
            let _ = std::io::stdin().read(&mut byte);
        }
    };
    let result = persist_with_hooks(&guard, &record, || wait("created"), || wait("synced"));
    if phase == "write_failure" {
        assert!(result.is_err());
        wait("write_failure");
    }
}

#[test]
fn interrupted_intent_preserves_evidence_and_blocks_mutation() {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::time::Duration;
    for phase in ["created", "synced", "write_failure"] {
        let root = fixture(phase);
        let mut command = if phase == "write_failure" {
            let mut shell = Command::new("/bin/bash");
            shell
                .args(["-c", "trap '' XFSZ; exec \"$@\"", "selector-test"])
                .arg(std::env::current_exe().unwrap());
            shell
        } else {
            Command::new("/proc/self/exe")
        };
        let mut child = command
            .args([
                "--exact",
                "customer::package_selector::intent::tests::selector_intent_kill_child",
                "--ignored",
                "--nocapture",
            ])
            .env("RAMSHARED_SELECTOR_TEST_ROOT", &root)
            .env("RAMSHARED_SELECTOR_TEST_PHASE", phase)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, receiver) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if line.contains("RAMSHARED_SELECTOR_INTENT_READY") {
                    let _ = sender.send(());
                    break;
                }
            }
        });
        let ready = receiver.recv_timeout(Duration::from_secs(15));
        let killed = child.kill();
        let status = child.wait();
        reader.join().unwrap();
        assert!(ready.is_ok() && killed.is_ok() && !status.unwrap().success());
        let paths = TransitionPaths::under(&root);
        assert!(read_pending(&paths).unwrap().is_some());
        let bytes = std::fs::read(paths.transactions_root().join(super::super::JOURNAL)).unwrap();
        assert_eq!(bytes.is_empty(), phase != "synced");
        assert!(acquire_transition(OperationOwner::Up, &paths).is_err());
        assert!(
            matches!(std::fs::symlink_metadata(root.join("current")), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
