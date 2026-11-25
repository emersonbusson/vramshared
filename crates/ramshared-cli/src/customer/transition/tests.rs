// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/transition/tests.rs
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "fixture failures and unexpected outcomes must fail the test immediately"
)]

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use rustix::fs::{AtFlags, FileType, statat};
use sha2::{Digest, Sha256};

use super::super::transition_custody::open_directory_no_follow;
use super::*;

static TEMP: AtomicU64 = AtomicU64::new(0);

fn fixture() -> PathBuf {
    let parent = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    for _ in 0..1024 {
        let number = TEMP.fetch_add(1, AtomicOrdering::SeqCst);
        let path = parent.join(format!(
            "ramshared-transition-{}-{number}",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => return path,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!("create isolated transition fixture: {error}"),
        }
    }
    panic!("exhausted isolated transition fixture names");
}

fn digest(label: &str) -> String {
    Sha256::digest(label.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn hashes(tag: &str) -> (String, String) {
    (digest(&format!("{tag}-old")), digest(&format!("{tag}-new")))
}

/// The filesystem type a named object currently has, without following a
/// symlink. Confinement tests use this to prove a hostile name is still the
/// hostile object after being refused.
fn named_type(directory: &Path, name: &OsStr) -> Option<FileType> {
    let directory = open_directory_no_follow(directory, false).ok().flatten()?;
    let stat = statat(&directory, name, AtFlags::SYMLINK_NOFOLLOW).ok()?;
    Some(FileType::from_raw_mode(stat.st_mode))
}

fn guard_transactions(guard: &CustomerTransitionGuard) -> PathBuf {
    guard.paths().transactions_root().to_path_buf()
}

/// Exactly one owner holds the customer mutation guard at a time. The
/// losers are refused with a stable busy reason and produce no effect; the
/// winner is the only one that may proceed.
#[test]
// TestName: concurrent_setup_up_and_package_have_one_owner
fn concurrent_setup_up_and_package_have_one_owner() {
    let root = fixture();
    let paths = TransitionPaths::under(&root);

    let setup = acquire_transition(OperationOwner::Setup, &paths)
        .expect("the first mutator takes the guard");
    assert_eq!(setup.owner(), OperationOwner::Setup);

    for owner in [
        OperationOwner::Up,
        OperationOwner::Package,
        OperationOwner::Boot,
        OperationOwner::Selection,
    ] {
        let refused = acquire_transition(owner, &paths)
            .expect_err("a second mutator must not share the guard");
        assert_eq!(
            refused.reason,
            CustomerReason::ActiveSession,
            "{} must report a stable busy reason",
            owner.as_str()
        );
        assert_eq!(refused.exit_code(), 4);
        assert_eq!(refused.state, CustomerState::InstalledOff);
    }

    // A nested acquire in the same process contends too: flock is per
    // open-file-description, so borrowing the guard is enforced by the
    // kernel, not by convention.
    let nested = acquire_transition(OperationOwner::Setup, &paths)
        .expect_err("a nested acquire must borrow the held guard, not take it again");
    assert_eq!(nested.reason, CustomerReason::ActiveSession);

    drop(setup);

    // Once released, the next owner is admitted exactly once.
    let package = acquire_transition(OperationOwner::Package, &paths)
        .expect("the next owner takes the released guard");
    assert_eq!(package.owner(), OperationOwner::Package);
    drop(package);
    let _ = fs::remove_dir_all(&root);
}

/// A hostile name at any component is refused, never followed and never
/// adopted. The target a symlink points at is never opened.
#[test]
// TestName: transition_rejects_symlink_and_foreign_owned_path
fn transition_rejects_symlink_and_foreign_owned_path() {
    let root = fixture();
    let paths = TransitionPaths::under(&root);

    // A pre-created symlink at the lock file name pointing at a
    // foreign-owned object must be refused; the target stays unopened.
    fs::create_dir_all(root.join("run")).unwrap();
    symlink("/etc/passwd", root.join("run/transition.lock")).unwrap();
    let refused = acquire_transition(OperationOwner::Setup, &paths)
        .expect_err("a symlink at the lock name must never be opened");
    assert_eq!(refused.reason, CustomerReason::UnsupportedSurface);
    assert_eq!(refused.state, CustomerState::InstalledOff);
    assert_eq!(
        named_type(&root.join("run"), TransitionPaths::lock_file_name()),
        Some(FileType::Symlink),
        "the hostile name must still be the symlink that was refused"
    );
    fs::remove_file(root.join("run/transition.lock")).unwrap();

    // A symlink at a directory component is refused the same way.
    fs::create_dir_all(root.join("elsewhere")).unwrap();
    symlink(root.join("elsewhere"), root.join("run/evil")).unwrap();
    let mut swapped = paths.clone();
    swapped.lock_root = root.join("run/evil").join("nested");
    let refused = acquire_transition(OperationOwner::Setup, &swapped)
        .expect_err("a symlinked directory component must never be traversed");
    assert_eq!(refused.reason, CustomerReason::UnsupportedSurface);

    // A directory that is not safe to own state under is refused. The
    // refusal names the parent directory that failed the check, not the
    // object that was being opened: naming the lock here sent the operator
    // to the wrong file.
    let mut unsafe_root = paths.clone();
    unsafe_root.lock_root = root.join("world");
    fs::create_dir(&unsafe_root.lock_root).unwrap();
    fs::set_permissions(&unsafe_root.lock_root, fs::Permissions::from_mode(0o777)).unwrap();
    let refused = acquire_transition(OperationOwner::Setup, &unsafe_root)
        .expect_err("a world-writable lock root is not ours to trust");
    assert_eq!(refused.reason, CustomerReason::UnsupportedSurface);
    assert!(
        refused
            .detail
            .contains("parent directory of transition lock"),
        "the refusal must name the parent that failed, not the lock: {}",
        refused.detail
    );
    assert!(
        !refused
            .detail
            .starts_with("transition lock must be a directory"),
        "the refusal must not blame the object for its parent's mode: {}",
        refused.detail
    );

    // A path that leaves the root is refused, not normalized.
    let mut escaping = paths.clone();
    escaping.lock_root = root.join("run").join("..").join("run");
    let refused = acquire_transition(OperationOwner::Setup, &escaping)
        .expect_err("an unsafe path component is refused");
    assert_eq!(refused.reason, CustomerReason::UnsupportedSurface);

    // A pre-created lock file that is not a singly linked regular file is
    // refused: a hard link is a second name for the same object and is
    // exactly the alias this guard exists to reject.
    let mut linked = paths.clone();
    linked.lock_root = root.join("linked");
    fs::create_dir(&linked.lock_root).unwrap();
    fs::write(linked.lock_root.join("transition.lock"), b"x").unwrap();
    fs::hard_link(
        linked.lock_root.join("transition.lock"),
        linked.lock_root.join("alias.lock"),
    )
    .unwrap();
    let refused = acquire_transition(OperationOwner::Setup, &linked)
        .expect_err("a hard-linked lock file is not singly linked");
    assert_eq!(refused.reason, CustomerReason::UnsupportedSurface);

    // An empty path is refused rather than interpreted as the anchor.
    let mut empty = paths.clone();
    empty.lock_root = PathBuf::new();
    let refused = acquire_transition(OperationOwner::Setup, &empty)
        .expect_err("an empty transition path is refused");
    assert_eq!(refused.reason, CustomerReason::UnsupportedSurface);

    // A journal that is not safe to read is an error, not an empty
    // journal: a pending record must never be masked by an unreadable
    // directory.
    let mut hostile = paths.clone();
    hostile.transactions_root = root.join("hostile-journal");
    fs::create_dir(&hostile.transactions_root).unwrap();
    fs::set_permissions(
        &hostile.transactions_root,
        fs::Permissions::from_mode(0o777),
    )
    .unwrap();
    let refused = acquire_transition(OperationOwner::Up, &hostile)
        .expect_err("an untrustworthy journal is not an empty journal");
    assert_eq!(refused.reason, CustomerReason::RecoveryRequired);

    let _ = fs::remove_dir_all(&root);
}

/// A package transaction is carried by a durable record, not by the lock.
/// When the hook process exits the lock is gone and the record remains, so
/// activation is still refused until the same identity commits or an
/// attended reconciliation clears it. The record is never silently
/// dropped and never reported as a clean Ready.
#[test]
// TestName: package_pending_blocks_activation_across_hook_exit
fn package_pending_blocks_activation_across_hook_exit() {
    let root = fixture();
    let paths = TransitionPaths::under(&root);
    let (old_hash, new_hash) = hashes("pkg");

    {
        let hook = acquire_transition(OperationOwner::Package, &paths)
            .expect("the package hook takes the guard to enroll");
        enroll_pending(
            &hook,
            &PackagePending {
                target_release: None,
                action: PackageAction::Upgrade,
                old_hash: old_hash.clone(),
                new_hash: new_hash.clone(),
                issued_unix: 1,
            },
        )
        .expect("the pending record is fsynced before any package effect");
    }
    // Hook exited: the lock is gone. The record is not.

    for owner in [
        OperationOwner::Up,
        OperationOwner::Setup,
        OperationOwner::Boot,
        OperationOwner::Selection,
    ] {
        let refused = acquire_transition(owner, &paths)
            .expect_err("a pending package transaction blocks activation across hook exit");
        assert_eq!(
            refused.reason,
            CustomerReason::RecoveryRequired,
            "{} must report the interrupted package transaction",
            owner.as_str()
        );
        assert_eq!(refused.exit_code(), 5);
        assert_eq!(refused.state, CustomerState::RecoveryRequired);
    }

    // Observation takes no lock and reports the pending record.
    let observed = read_pending(&paths)
        .expect("observation reads the journal without mutating")
        .expect("the pending record is durable");
    assert_eq!(observed.action, PackageAction::Upgrade);
    assert_eq!(observed.old_hash, old_hash);
    assert_eq!(observed.new_hash, new_hash);

    // The package owner itself may still reconcile: it is the one owner
    // allowed past a record it already owns.
    let reconcile = acquire_transition(OperationOwner::Package, &paths)
        .expect("the package owner reconciles its own pending record");
    clear_pending(&reconcile, &observed)
        .expect("quiescence and receipt preservation clear the record");
    drop(reconcile);

    // Once cleared, activation is admitted again and nothing is pending.
    let up = acquire_transition(OperationOwner::Up, &paths)
        .expect("activation resumes after the transaction is reconciled");
    assert!(read_pending(&paths).expect("journal is readable").is_none());
    drop(up);

    // A re-enroll of the same transaction is idempotent; a different one
    // never replaces a live record.
    let hook = acquire_transition(OperationOwner::Package, &paths).expect("package owner");
    let record = PackagePending {
        target_release: None,
        action: PackageAction::Install,
        old_hash: hashes("first").0,
        new_hash: hashes("first").1,
        issued_unix: 2,
    };
    enroll_pending(&hook, &record).expect("first enroll");
    enroll_pending(&hook, &record).expect("the same transaction re-enrolls idempotently");
    let other = PackagePending {
        target_release: None,
        action: PackageAction::Remove,
        old_hash: hashes("second").0,
        new_hash: hashes("second").1,
        issued_unix: 3,
    };
    let refused = enroll_pending(&hook, &other)
        .expect_err("a different transaction never replaces a live one");
    assert_eq!(refused.reason, CustomerReason::ActiveSession);
    drop(hook);

    let _ = fs::remove_dir_all(&root);
}

/// The product layout and the owner/action vocabulary are named in one
/// place each. A later command may not invent a string and reach around
/// the guard, and a reader of the code knows exactly where `/run` and
/// `/var/lib` appear.
#[test]
fn transition_vocabulary_and_paths_are_the_single_named_contract() {
    let production = TransitionPaths::production();
    assert_eq!(production.lock_root, PathBuf::from("/run/ramshared"));
    assert_eq!(
        production.transactions_root,
        PathBuf::from("/var/lib/ramshared/transactions")
    );

    for (owner, name) in [
        (OperationOwner::Setup, "setup"),
        (OperationOwner::Up, "up"),
        (OperationOwner::Boot, "boot"),
        (OperationOwner::Selection, "selection"),
        (OperationOwner::Package, "package"),
    ] {
        assert_eq!(owner.as_str(), name);
    }
    for (action, name) in [
        (PackageAction::Install, "install"),
        (PackageAction::Upgrade, "upgrade"),
        (PackageAction::Remove, "remove"),
    ] {
        assert_eq!(action.as_str(), name);
    }

    let root = fixture();
    let paths = TransitionPaths::under(&root);
    let guard = acquire_transition(OperationOwner::Setup, &paths).expect("the guard");
    assert_eq!(guard.paths(), &paths);
    drop(guard);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn package_journal_mutations_require_package_custody() {
    let root = fixture();
    let paths = TransitionPaths::under(&root);
    let guard = acquire_transition(OperationOwner::Setup, &paths).expect("setup guard");
    let pending = PackagePending {
        target_release: None,
        action: PackageAction::Install,
        old_hash: hashes("setup-owner-old").0,
        new_hash: hashes("setup-owner-new").1,
        issued_unix: 7,
    };

    let enrollment = enroll_pending(&guard, &pending);
    let observed = read_pending(&paths).expect("observe package journal");
    let clear = clear_pending(&guard, &pending);
    drop(guard);
    let _ = fs::remove_dir_all(&root);

    assert!(
        enrollment.is_err(),
        "a setup guard must not enroll a package transaction"
    );
    assert!(
        observed.is_none(),
        "a refused non-package enrollment must leave no journal"
    );
    assert!(
        clear.is_err(),
        "a setup guard must not clear package transaction evidence"
    );
}

#[test]
fn selector_journal_blocks_pending_clear_and_other_mutations() {
    use std::os::unix::fs::PermissionsExt;
    let root = fixture();
    let paths = TransitionPaths::under(&root);
    let guard = acquire_transition(OperationOwner::Package, &paths).unwrap();
    let pending = PackagePending {
        target_release: Some("v1".into()),
        action: PackageAction::Install,
        old_hash: hashes("selector-old").0,
        new_hash: hashes("selector-new").1,
        issued_unix: 1,
    };
    enroll_pending(&guard, &pending).unwrap();
    let journal = paths.transactions_root().join("package-selector.json");
    fs::write(&journal, b"interrupted selector\n").unwrap();
    fs::set_permissions(&journal, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(clear_pending(&guard, &pending).is_err());
    assert_eq!(read_pending(&paths).unwrap(), Some(pending));
    fs::remove_file(
        paths
            .transactions_root()
            .join(TransitionPaths::pending_file_name()),
    )
    .unwrap();
    drop(guard);
    assert!(acquire_transition(OperationOwner::Up, &paths).is_err());
    fs::remove_file(journal).unwrap();
    let journal = paths.transactions_root().join("package-selector.json");
    std::os::unix::fs::symlink("missing", &journal).unwrap();
    assert!(acquire_transition(OperationOwner::Up, &paths).is_err());
    fs::remove_file(journal).unwrap();
    assert!(acquire_transition(OperationOwner::Up, &paths).is_ok());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn package_pending_clear_requires_exact_transaction_identity() {
    let root = fixture();
    let paths = TransitionPaths::under(&root);
    let guard = acquire_transition(OperationOwner::Package, &paths).expect("package guard");
    let pending = PackagePending {
        target_release: None,
        action: PackageAction::Upgrade,
        old_hash: hashes("clear-old").0,
        new_hash: hashes("clear-new").1,
        issued_unix: 42,
    };
    enroll_pending(&guard, &pending).expect("enroll transaction");
    let mut different = pending.clone();
    different.issued_unix += 1;
    let mut different_action = pending.clone();
    different_action.action = PackageAction::Remove;
    let mut different_hash = pending.clone();
    different_hash.new_hash = hashes("other").1;
    for candidate in [&different, &different_action, &different_hash] {
        assert!(clear_pending(&guard, candidate).is_err());
        assert_eq!(read_pending(&paths).unwrap(), Some(pending.clone()));
    }
    clear_pending(&guard, &pending).expect("matching transaction clears");
    assert_eq!(read_pending(&paths).unwrap(), None);
    drop(guard);
    let _ = fs::remove_dir_all(&root);
}

/// A pending record is a strict, hash-bound object. A malformed record, a
/// foreign name at the journal, and a record that binds no content are all
/// refusals — never an empty journal and never a silently ignored entry.
#[test]
fn pending_journal_refuses_malformed_foreign_and_empty_records() {
    let root = fixture();
    let paths = TransitionPaths::under(&root);

    let hook = acquire_transition(OperationOwner::Package, &paths).expect("package owner");

    // A record that binds no content is refused before anything is
    // written.
    for record in [
        PackagePending {
            target_release: None,
            action: PackageAction::Install,
            old_hash: String::new(),
            new_hash: hashes("empty-old").1,
            issued_unix: 1,
        },
        PackagePending {
            target_release: None,
            action: PackageAction::Install,
            old_hash: hashes("empty-new").0,
            new_hash: String::new(),
            issued_unix: 1,
        },
    ] {
        let refused = enroll_pending(&hook, &record)
            .expect_err("a record that binds no content must be refused");
        assert_eq!(refused.reason, CustomerReason::UnsupportedSurface);
    }

    // A malformed journal body is refused, not treated as empty.
    fs::create_dir_all(guard_transactions(&hook)).unwrap();
    fs::write(
        guard_transactions(&hook).join("package-pending.json"),
        b"{not json",
    )
    .unwrap();
    let refused =
        read_pending(&paths).expect_err("a malformed journal body is not an empty journal");
    assert_eq!(refused.reason, CustomerReason::UnsupportedSurface);
    let refused = enroll_pending(
        &hook,
        &PackagePending {
            target_release: None,
            action: PackageAction::Upgrade,
            old_hash: hashes("under-malformed").0,
            new_hash: hashes("under-malformed").1,
            issued_unix: 2,
        },
    )
    .expect_err("a malformed journal body blocks enrollment");
    assert_eq!(refused.reason, CustomerReason::UnsupportedSurface);

    // A foreign name at the journal is refused before the body is read.
    fs::remove_file(guard_transactions(&hook).join("package-pending.json")).unwrap();
    symlink(
        "/etc/passwd",
        guard_transactions(&hook).join("package-pending.json"),
    )
    .unwrap();
    let refused =
        read_pending(&paths).expect_err("a symlink at the journal name must never be opened");
    assert_eq!(refused.reason, CustomerReason::UnsupportedSurface);
    assert_eq!(
        named_type(
            &guard_transactions(&hook),
            TransitionPaths::pending_file_name()
        ),
        Some(FileType::Symlink),
        "the hostile name must still be the symlink that was refused"
    );
    fs::remove_file(guard_transactions(&hook).join("package-pending.json")).unwrap();
    drop(hook);

    // An absent record cannot establish that this transaction completed.
    let hook = acquire_transition(OperationOwner::Package, &paths).expect("package owner");
    let expected = PackagePending {
        target_release: None,
        action: PackageAction::Install,
        old_hash: hashes("missing-old").0,
        new_hash: hashes("missing-new").1,
        issued_unix: 3,
    };
    assert!(clear_pending(&hook, &expected).is_err());
    drop(hook);
    let _ = fs::remove_dir_all(&root);
}

/// The guard re-pins what it opened. If the named lock object is replaced
/// or removed under a held guard, the next mutator step is refused rather
/// than operating on an object the guard never opened.
#[test]
fn guard_detects_lock_file_replaced_or_removed_under_held_custody() {
    let root = fixture();
    let paths = TransitionPaths::under(&root);

    // Removal: the named object is gone, so there is nothing to re-pin.
    let guard = acquire_transition(OperationOwner::Package, &paths).expect("the guard");
    fs::remove_file(root.join("run/transition.lock")).unwrap();
    let refused = enroll_pending(
        &guard,
        &PackagePending {
            target_release: None,
            action: PackageAction::Install,
            old_hash: hashes("gone").0,
            new_hash: hashes("gone").1,
            issued_unix: 1,
        },
    )
    .expect_err("a removed lock object is not the object the guard holds");
    assert_eq!(refused.reason, CustomerReason::UnsupportedSurface);
    drop(guard);

    // Replacement: a different inode at the same name is a different
    // object, and the guard refuses to act on it.
    let guard = acquire_transition(OperationOwner::Package, &paths).expect("the guard");
    fs::remove_file(root.join("run/transition.lock")).unwrap();
    fs::write(root.join("run/transition.lock"), b"replacement").unwrap();
    let refused = enroll_pending(
        &guard,
        &PackagePending {
            target_release: None,
            action: PackageAction::Install,
            old_hash: hashes("replaced").0,
            new_hash: hashes("replaced").1,
            issued_unix: 2,
        },
    )
    .expect_err("a replaced lock object is not the object the guard holds");
    assert_eq!(refused.reason, CustomerReason::UnsupportedSurface);
    drop(guard);
    let _ = fs::remove_dir_all(&root);
}

/// Package identity digests are canonical SHA-256 values. Refusal happens
/// before a journal record can authorize any package-manager effect.
#[test]
// TestName: package_pending_refuses_noncanonical_hashes_before_write
fn package_pending_refuses_noncanonical_hashes_before_write() {
    let root = fixture();
    let paths = TransitionPaths::under(&root);
    let guard = acquire_transition(OperationOwner::Package, &paths).expect("package guard");
    let (valid_old, valid_new) = hashes("candidate");
    let malformed = [
        (String::new(), valid_new.clone()),
        ("a".repeat(63), valid_new.clone()),
        ("g".repeat(64), valid_new.clone()),
        ("A".repeat(64), valid_new.clone()),
        (valid_old.clone(), String::new()),
        (valid_old.clone(), "f".repeat(63)),
        (valid_old, "G".repeat(64)),
    ];
    for (index, (old_hash, new_hash)) in malformed.into_iter().enumerate() {
        let refused = enroll_pending(
            &guard,
            &PackagePending {
                target_release: None,
                action: PackageAction::Upgrade,
                old_hash,
                new_hash,
                issued_unix: 4 + index as u64,
            },
        )
        .expect_err("noncanonical digests cannot identify a package transaction");
        assert_eq!(refused.reason, CustomerReason::UnsupportedSurface);
        assert!(
            read_pending(&paths)
                .expect("journal remains readable")
                .is_none()
        );
    }
    drop(guard);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn package_pending_binds_a_bounded_target_and_reads_legacy_records() {
    let root = fixture();
    let paths = TransitionPaths::under(&root);
    let guard = acquire_transition(OperationOwner::Package, &paths).unwrap();
    let (old_hash, new_hash) = hashes("candidate");
    let mut pending = PackagePending {
        target_release: None,
        action: PackageAction::Upgrade,
        old_hash,
        new_hash,
        issued_unix: 1,
    };
    let legacy = serde_json::to_string(&pending).unwrap();
    assert!(!legacy.contains("target_release"));
    assert_eq!(
        serde_json::from_str::<PackagePending>(&legacy).unwrap(),
        pending
    );
    for invalid in ["", " v1", "v1\nother", "v1\0"] {
        pending.target_release = Some(invalid.into());
        assert!(enroll_pending(&guard, &pending).is_err());
        assert!(read_pending(&paths).unwrap().is_none());
    }
    pending.target_release = Some("a".repeat(crate::release_identity::MAX_IDENTITY_LINE_BYTES + 1));
    assert!(enroll_pending(&guard, &pending).is_err());
    assert!(read_pending(&paths).unwrap().is_none());
    pending.target_release = Some("v1".into());
    enroll_pending(&guard, &pending).unwrap();
    assert_eq!(read_pending(&paths).unwrap(), Some(pending));
    drop(guard);
    let _ = fs::remove_dir_all(root);
}

/// Reading a persisted record applies the same strict identity contract as
/// enrollment; corrupt bytes cannot become an apparently owned transaction.
#[test]
// TestName: read_pending_refuses_noncanonical_hashes
fn read_pending_refuses_noncanonical_hashes() {
    let root = fixture();
    let paths = TransitionPaths::under(&root);
    fs::create_dir_all(paths.transactions_root()).unwrap();
    let pending = PackagePending {
        target_release: None,
        action: PackageAction::Upgrade,
        old_hash: "A".repeat(64),
        new_hash: hashes("candidate").1,
        issued_unix: 5,
    };
    fs::write(
        paths
            .transactions_root()
            .join(TransitionPaths::pending_file_name()),
        serde_json::to_vec(&pending).unwrap(),
    )
    .unwrap();
    let refused = read_pending(&paths).expect_err("an uppercase digest is a corrupt journal");
    assert_eq!(refused.reason, CustomerReason::UnsupportedSurface);
    let _ = fs::remove_dir_all(&root);
}
