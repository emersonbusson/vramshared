// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_selector/reconciliation/tests.rs
#![allow(clippy::unwrap_used)]

use std::fs::File;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use rustix::fs::{Mode, fchmod};

use super::super::effect::{
    PreviousSelectorRecord, SelectorEffectPhase, SelectorEffectRecord, persist_record,
};
use super::super::tree_custody::{DescriptorBudget, TreeCustody};
use super::*;
use crate::customer::transition::{
    OperationOwner, PackageAction, PackagePending, TransitionPaths, acquire_transition,
    enroll_pending,
};

struct TempFixture(PathBuf);
impl Drop for TempFixture {
    fn drop(&mut self) {
        remove_owned_unselected_release_or_unseal(&self.0);
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn remove_owned_unselected_release_or_unseal(root: &Path) {
    if let Ok(md) = std::fs::symlink_metadata(root)
        && md.is_dir()
    {
        if let Ok(fd) = File::open(root) {
            let _ = fchmod(&fd, Mode::from_raw_mode(0o755));
        }
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    remove_owned_unselected_release_or_unseal(&entry.path());
                } else if entry.file_type().is_ok_and(|t| t.is_file())
                    && let Ok(fd) = File::open(entry.path())
                {
                    let _ = fchmod(&fd, Mode::from_raw_mode(0o644));
                }
            }
        }
    }
}

fn digest(bytes: &[u8]) -> String {
    let mut r = bytes;
    crate::installed_image::sha256_reader(&mut r).unwrap()
}

fn create_sealed_release_fixture(
    root: &Path,
    version: &str,
    commit: &str,
    input_hash: &str,
) -> ((u64, u64), String, String) {
    std::fs::create_dir_all(root.join("bin")).unwrap();
    std::fs::create_dir_all(root.join("scripts/safety")).unwrap();
    let cli = b"\x7fELF\x02\x01\x01\x00cli".to_vec();
    let daemon = b"\x7fELF\x02\x01\x01\x00daemon".to_vec();
    let cfg = b"NBD_LOWER_SINK=/tmp\n".to_vec();
    std::fs::write(root.join("bin/ramshared"), &cli).unwrap();
    std::fs::write(root.join("bin/ramsharedd"), &daemon).unwrap();
    std::fs::write(root.join("scripts/safety/cascade.conf.example"), &cfg).unwrap();
    std::fs::write(root.join("RELEASE_VERSION"), format!("{version}\n")).unwrap();
    std::fs::write(root.join("SOURCE_COMMIT"), format!("{commit}\n")).unwrap();
    std::fs::write(root.join("SOURCE_TREE_STATE"), b"clean\n").unwrap();
    let input_body = format!(
        "{}  ./bin/ramshared\n{}  ./bin/ramsharedd\n",
        digest(&cli),
        digest(&daemon)
    );
    let actual_input_hash = if input_hash.is_empty() {
        digest(input_body.as_bytes())
    } else {
        input_hash.to_owned()
    };
    let input_bytes = format!("{}  ./bin/ramshared\n", digest(&cli));
    let _ = input_bytes;
    // Build authentic input bundle manifest whose hash equals `actual_input_hash` when unspecified.
    std::fs::write(root.join("INPUT_BUNDLE_SHA256SUMS"), input_body.as_bytes()).unwrap();
    let provenance = serde_json::json!({
        "schema_version": crate::installed_image::INSTALLED_PROVENANCE_V2,
        "input_bundle_manifest_sha256": digest(input_body.as_bytes()),
        "source_commit": commit,
        "source_branch": "main",
        "source_tree_state": "clean",
        "installed_at_utc": "2026-11-22T07:00:00Z",
        "lower_sink": {
            "canonical_path": "/tmp",
            "identity_sha256": "c".repeat(64),
            "filesystem_block_bytes": 4096,
            "available_kib_at_bind": 1024
        }
    })
    .to_string();
    std::fs::write(
        root.join("INSTALL_PROVENANCE.json"),
        format!("{provenance}\n"),
    )
    .unwrap();

    let mut entries = std::collections::BTreeMap::new();
    for rel in [
        "INPUT_BUNDLE_SHA256SUMS",
        "INSTALL_PROVENANCE.json",
        "RELEASE_VERSION",
        "SOURCE_COMMIT",
        "SOURCE_TREE_STATE",
        "bin/ramshared",
        "bin/ramsharedd",
        "scripts/safety/cascade.conf.example",
    ] {
        entries.insert(rel, digest(&std::fs::read(root.join(rel)).unwrap()));
    }
    let mut sha_text = String::new();
    for (rel, h) in entries {
        sha_text.push_str(&format!("{h}  ./{rel}\n"));
    }
    std::fs::write(root.join("SHA256SUMS"), sha_text.as_bytes()).unwrap();
    let installed_hash = digest(sha_text.as_bytes());
    std::fs::write(
        root.join("INSTALLED_MANIFEST_SHA256"),
        format!("{installed_hash}\n"),
    )
    .unwrap();

    for exec in ["bin/ramshared", "bin/ramsharedd"] {
        std::fs::set_permissions(root.join(exec), std::fs::Permissions::from_mode(0o555)).unwrap();
    }
    for non_exec in [
        "INPUT_BUNDLE_SHA256SUMS",
        "INSTALL_PROVENANCE.json",
        "INSTALLED_MANIFEST_SHA256",
        "RELEASE_VERSION",
        "SHA256SUMS",
        "SOURCE_COMMIT",
        "SOURCE_TREE_STATE",
        "scripts/safety/cascade.conf.example",
    ] {
        std::fs::set_permissions(root.join(non_exec), std::fs::Permissions::from_mode(0o444))
            .unwrap();
    }
    for dir in ["bin", "scripts/safety", "scripts", ""] {
        let p = if dir.is_empty() {
            root.to_path_buf()
        } else {
            root.join(dir)
        };
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o555)).unwrap();
    }
    let _ = actual_input_hash;
    let md = std::fs::symlink_metadata(root).unwrap();
    (
        (md.dev(), md.ino()),
        digest(input_body.as_bytes()),
        installed_hash,
    )
}

#[test]
fn reconcile_rolls_back_staged_and_published_upgrade_and_refuses_foreign() {
    let base = std::env::temp_dir().join(format!(
        "ramshared-selector-reconcile-{}",
        std::process::id()
    ));
    let _guard = TempFixture(base.clone());
    let _ = std::fs::remove_dir_all(&base);
    let product = base.join("opt/ramshared");
    let tx_root = base.join("tx");
    let old_root = product.join("releases/v1");
    let new_root = product.join("releases/v2");
    std::fs::create_dir_all(&old_root).unwrap();
    let old_meta = std::fs::metadata(&old_root).unwrap();
    let commit = "0".repeat(40);
    let (new_id, input_hash, installed_hash) =
        create_sealed_release_fixture(&new_root, "v2", &commit, "");

    let selector = product.join("current");
    symlink("releases/v1", &selector).unwrap();
    let prev_entry_meta = std::fs::symlink_metadata(&selector).unwrap();
    let parent_meta = std::fs::metadata(&product).unwrap();

    let paths = TransitionPaths::under(&tx_root);
    let guard = acquire_transition(OperationOwner::Package, &paths).unwrap();
    let pending = PackagePending {
        target_release: Some("v2".into()),
        action: PackageAction::Upgrade,
        old_hash: "a".repeat(64),
        new_hash: input_hash.clone(),
        issued_unix: 10,
    };
    enroll_pending(&guard, &pending).unwrap();

    let staging_name = ".current.test.staging".to_owned();
    symlink("releases/v2", product.join(&staging_name)).unwrap();
    let staging_meta = std::fs::symlink_metadata(product.join(&staging_name)).unwrap();
    let mut record = SelectorEffectRecord {
        schema: 1,
        phase: SelectorEffectPhase::SelectorStaged,
        pending: pending.clone(),
        selector: selector.clone(),
        parent_identity: (parent_meta.dev(), parent_meta.ino()),
        target: PathBuf::from("releases/v2"),
        target_identity: new_id,
        input_manifest_sha256: input_hash.clone(),
        sealed_manifest_sha256: installed_hash.clone(),
        staging_name: staging_name.clone(),
        staging_identity: Some((staging_meta.dev(), staging_meta.ino())),
        previous: Some(PreviousSelectorRecord {
            target: PathBuf::from("releases/v1"),
            entry_identity: (prev_entry_meta.dev(), prev_entry_meta.ino()),
            root_identity: (old_meta.dev(), old_meta.ino()),
            input_manifest_sha256: Some("a".repeat(64)),
        }),
    };
    persist_record(&guard, &record).unwrap();

    // Case 1: Rollback from SelectorStaged removes owned staging link and preserves v1.
    let outcome = reconcile_selector(
        &guard,
        &pending,
        SelectorRecoveryAction::Rollback,
        None,
        None,
    )
    .unwrap();
    assert_eq!(outcome, SelectorRecoveryOutcome::RolledBack);
    assert_eq!(
        std::fs::read_link(&selector).unwrap(),
        Path::new("releases/v1")
    );
    assert!(!product.join(&staging_name).exists());
    // Idempotent second rollback check.
    assert_eq!(
        reconcile_selector(
            &guard,
            &pending,
            SelectorRecoveryAction::Rollback,
            None,
            None
        )
        .unwrap(),
        SelectorRecoveryOutcome::RolledBack
    );
    assert!(
        reconcile_selector(
            &guard,
            &pending,
            SelectorRecoveryAction::Complete,
            None,
            None
        )
        .is_err()
    );

    // Case 2: Simulate SelectorPublished (atomic rename succeeded), then roll back to v1.
    std::fs::remove_file(&selector).unwrap();
    symlink("releases/v2", product.join(&staging_name)).unwrap();
    let pub_meta = std::fs::symlink_metadata(product.join(&staging_name)).unwrap();
    std::fs::rename(product.join(&staging_name), &selector).unwrap();
    record.phase = SelectorEffectPhase::SelectorPublished;
    record.staging_identity = Some((pub_meta.dev(), pub_meta.ino()));
    persist_record(&guard, &record).unwrap();

    let rb_pub = reconcile_selector(
        &guard,
        &pending,
        SelectorRecoveryAction::Rollback,
        None,
        None,
    )
    .unwrap();
    assert_eq!(rb_pub, SelectorRecoveryOutcome::RolledBack);
    assert_eq!(
        std::fs::read_link(&selector).unwrap(),
        Path::new("releases/v1")
    );

    // Case 3: Foreign replacement of `current` is refused and never overwritten.
    std::fs::remove_file(&selector).unwrap();
    std::fs::write(&selector, b"foreign-file").unwrap();
    record.phase = SelectorEffectPhase::SelectorPublished;
    persist_record(&guard, &record).unwrap();
    assert!(
        reconcile_selector(
            &guard,
            &pending,
            SelectorRecoveryAction::Rollback,
            None,
            None
        )
        .is_err()
    );
    assert_eq!(std::fs::read(&selector).unwrap(), b"foreign-file");
    assert_eq!(
        inspect_effect_journal(&paths).unwrap().unwrap().phase,
        SelectorEffectPhase::SelectorPublished
    );

    // Owned cleanup refuses active selector target and removes unselected release safely.
    std::fs::remove_file(&selector).unwrap();
    symlink("releases/v2", &selector).unwrap();
    assert!(remove_owned_unselected_release(&new_root, new_id, &selector).is_err());
    std::fs::remove_file(&selector).unwrap();
    symlink("releases/v1", &selector).unwrap();
    assert!(remove_owned_unselected_release(&new_root, new_id, &selector).is_ok());
    assert!(!new_root.exists());
}

#[test]
#[ignore = "subprocess fixture invoked by selector_effect_crash_reconciles"]
fn selector_effect_crash_child() {
    let Ok(root_str) = std::env::var("RAMSHARED_EFFECT_CHILD_ROOT") else {
        return;
    };
    let root = PathBuf::from(root_str);
    let product = root.join("opt/ramshared");
    let paths = TransitionPaths::under(&root.join("tx"));
    let guard = acquire_transition(OperationOwner::Package, &paths).unwrap();
    let v2 = product.join("releases/v2");
    let input_hash = "b".repeat(64);
    let (v2_id, actual_input, installed_hash) =
        create_sealed_release_fixture(&v2, "v2", &"1".repeat(40), "");
    let _ = input_hash;
    let pending = PackagePending {
        target_release: Some("v2".into()),
        action: PackageAction::Install,
        old_hash: "a".repeat(64),
        new_hash: actual_input.clone(),
        issued_unix: 2,
    };
    enroll_pending(&guard, &pending).unwrap();

    let staging_name = ".current.child.staging".to_owned();
    let selector = product.join("current");
    rustix::fs::symlinkat(
        "releases/v2",
        open_directory_no_follow(&product, false).unwrap().unwrap(),
        staging_name.as_str(),
    )
    .unwrap();
    std::fs::rename(product.join(&staging_name), &selector).unwrap();
    let cur_meta = std::fs::symlink_metadata(&selector).unwrap();
    let parent_meta = std::fs::metadata(&product).unwrap();
    let record = SelectorEffectRecord {
        schema: 1,
        phase: SelectorEffectPhase::SelectorPublished,
        pending,
        selector,
        parent_identity: (parent_meta.dev(), parent_meta.ino()),
        target: PathBuf::from("releases/v2"),
        target_identity: v2_id,
        input_manifest_sha256: actual_input,
        sealed_manifest_sha256: installed_hash,
        staging_name,
        staging_identity: Some((cur_meta.dev(), cur_meta.ino())),
        previous: None,
    };
    persist_record(&guard, &record).unwrap();
    // Simulate unclean exit while lock is dropped cleanly by OS.
    std::process::exit(0);
}

#[test]
fn selector_effect_crash_reconciles() {
    let base =
        std::env::temp_dir().join(format!("ramshared-selector-crash-{}", std::process::id()));
    let _guard = TempFixture(base.clone());
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("opt/ramshared/releases")).unwrap();

    let exe = std::env::current_exe().unwrap();
    let out = std::process::Command::new(exe)
        .args([
            "--ignored",
            "--exact",
            "customer::package_selector::reconciliation::tests::selector_effect_crash_child",
        ])
        .env("RAMSHARED_EFFECT_CHILD_ROOT", &base)
        .output()
        .unwrap();
    assert!(out.status.success());

    let paths = TransitionPaths::under(&base.join("tx"));
    assert!(super::super::journal_present(&paths).unwrap());
    let guard = acquire_transition(OperationOwner::Package, &paths).unwrap();
    let pending = read_pending(&paths).unwrap().unwrap();
    let rec = inspect_effect_journal(&paths).unwrap().unwrap();
    assert_eq!(rec.phase, SelectorEffectPhase::SelectorPublished);

    // Complete from SelectorPublished with real SealedReleaseCustody (`TreeCustody::capture_sealed`)!
    let mut budget = DescriptorBudget::acquire().unwrap();
    let tree = TreeCustody::capture_sealed(
        &base.join("opt/ramshared/releases/v2"),
        rec.target_identity,
        &rec.sealed_manifest_sha256,
        &rec.input_manifest_sha256,
        &mut budget,
    )
    .unwrap();
    assert!(tree.revalidate().is_ok());

    // Rollback on fresh install removes owned `current` symlink.
    let outcome = reconcile_selector(
        &guard,
        &pending,
        SelectorRecoveryAction::Rollback,
        None,
        None,
    )
    .unwrap();
    assert_eq!(outcome, SelectorRecoveryOutcome::RolledBack);
    assert!(std::fs::symlink_metadata(base.join("opt/ramshared/current")).is_err());
}

#[test]
fn reconciliation_and_effect_primitives_refuse_malformed_and_foreign_states() {
    let base = std::env::temp_dir().join(format!(
        "ramshared-selector-edge-cases-{}",
        std::process::id()
    ));
    let _guard = TempFixture(base.clone());
    let _ = std::fs::remove_dir_all(&base);
    let product = base.join("opt/ramshared");
    std::fs::create_dir_all(product.join("releases/v1")).unwrap();
    let v1_md = std::fs::metadata(product.join("releases/v1")).unwrap();
    let v1_id = (v1_md.dev(), v1_md.ino());
    let paths = TransitionPaths::under(&base.join("tx"));

    // Boot owner cannot invoke selector reconciliation; absent effect journal refuses.
    let boot_guard = acquire_transition(OperationOwner::Boot, &paths).unwrap();
    let pending = PackagePending {
        target_release: Some("v1".into()),
        action: PackageAction::Install,
        old_hash: "a".repeat(64),
        new_hash: "b".repeat(64),
        issued_unix: 10,
    };
    assert!(
        reconcile_selector(
            &boot_guard,
            &pending,
            SelectorRecoveryAction::Rollback,
            None,
            None,
        )
        .is_err()
    );
    drop(boot_guard);

    let guard = acquire_transition(OperationOwner::Package, &paths).unwrap();
    assert!(
        reconcile_selector(
            &guard,
            &pending,
            SelectorRecoveryAction::Rollback,
            None,
            None
        )
        .is_err()
    );
    enroll_pending(&guard, &pending).unwrap();
    assert!(
        reconcile_selector(
            &guard,
            &pending,
            SelectorRecoveryAction::Rollback,
            None,
            None
        )
        .is_err()
    );

    // Malformed / invalid-field effect journals are rejected cleanly by inspect_effect_journal.
    let ej_path = paths.transactions_root().join(EFFECT_JOURNAL);
    std::fs::write(&ej_path, b"not-json\n").unwrap();
    assert!(inspect_effect_journal(&paths).is_err());

    let parent_fd = open_directory_no_follow(&product, false).unwrap().unwrap();
    let parent_md = std::fs::metadata(&product).unwrap();
    let mut rec = SelectorEffectRecord {
        schema: 2,
        phase: SelectorEffectPhase::RolledBack,
        pending: pending.clone(),
        selector: product.join("current"),
        parent_identity: (parent_md.dev(), parent_md.ino()),
        target: PathBuf::from("releases/v1"),
        target_identity: v1_id,
        input_manifest_sha256: "b".repeat(64),
        sealed_manifest_sha256: "c".repeat(64),
        staging_name: ".staging".into(),
        staging_identity: Some((1, 1)),
        previous: None,
    };
    persist_record(&guard, &rec).unwrap();
    assert!(inspect_effect_journal(&paths).is_err());

    rec.schema = 1;
    rec.staging_name = "bad/name".into();
    persist_record(&guard, &rec).unwrap();
    assert!(inspect_effect_journal(&paths).is_err());

    rec.staging_name = ".current.test.staging".into();
    rec.pending.issued_unix = 999;
    persist_record(&guard, &rec).unwrap();
    assert!(
        reconcile_selector(
            &guard,
            &pending,
            SelectorRecoveryAction::Rollback,
            None,
            None
        )
        .is_err()
    );

    // RolledBack with lingering staging symlink refuses verification; Install IntentBound rolls back cleanly.
    rec.pending = pending.clone();
    persist_record(&guard, &rec).unwrap();
    std::fs::write(product.join(&rec.staging_name), b"not-a-link").unwrap();
    assert!(
        reconcile_selector(
            &guard,
            &pending,
            SelectorRecoveryAction::Rollback,
            None,
            None
        )
        .is_err()
    );
    assert!(
        super::super::effect::cleanup_owned_staging_link(
            &parent_fd,
            &rec.staging_name,
            rec.staging_identity
        )
        .is_err()
    );
    std::fs::remove_file(product.join(&rec.staging_name)).unwrap();

    symlink("releases/v1", product.join(&rec.staging_name)).unwrap();
    assert!(
        super::super::effect::cleanup_owned_staging_link(
            &parent_fd,
            &rec.staging_name,
            Some((999, 999))
        )
        .is_err()
    );
    std::fs::remove_file(product.join(&rec.staging_name)).unwrap();

    rec.phase = SelectorEffectPhase::IntentBound;
    rec.staging_identity = None;
    persist_record(&guard, &rec).unwrap();
    assert_eq!(
        reconcile_selector(
            &guard,
            &pending,
            SelectorRecoveryAction::Rollback,
            None,
            None
        )
        .unwrap(),
        SelectorRecoveryOutcome::RolledBack
    );
    assert!(
        reconcile_selector(
            &guard,
            &pending,
            SelectorRecoveryAction::Rollback,
            None,
            None
        )
        .is_ok()
    );

    // Parent identity mismatch guard + foreign stat/verify/restore refusals.
    rec.parent_identity = (0, 0);
    persist_record(&guard, &rec).unwrap();
    assert!(
        reconcile_selector(
            &guard,
            &pending,
            SelectorRecoveryAction::Rollback,
            None,
            None
        )
        .is_err()
    );

    std::fs::write(product.join("current"), b"regular").unwrap();
    assert!(super::super::effect::stat_no_follow(&parent_fd, OsStr::new("current")).is_err());
    assert!(
        super::super::effect::verify_symlink(&parent_fd, OsStr::new("current"), "releases/v1")
            .is_err()
    );
    std::fs::remove_file(product.join("current")).unwrap();
    symlink("wrong", product.join("current")).unwrap();
    assert!(
        super::super::effect::verify_symlink(&parent_fd, OsStr::new("current"), "releases/v1")
            .is_err()
    );
    assert!(
        super::super::effect::check_live_selector_matches_prior(
            &parent_fd,
            OsStr::new("current"),
            None
        )
        .is_err()
    );
    std::fs::remove_file(product.join("current")).unwrap();
    assert!(
        super::super::effect::check_live_selector_matches_prior(
            &parent_fd,
            OsStr::new("current"),
            None
        )
        .is_ok()
    );

    let bad_prev = PreviousSelectorRecord {
        target: PathBuf::from("releases/v1"),
        entry_identity: (1, 1),
        root_identity: (0, 0),
        input_manifest_sha256: None,
    };
    assert!(
        super::super::effect::restore_prior_symlink(
            &parent_fd,
            &product,
            OsStr::new("current"),
            &bad_prev
        )
        .is_err()
    );
    let missing_prev = PreviousSelectorRecord {
        target: PathBuf::from("releases/missing"),
        ..bad_prev
    };
    assert!(
        super::super::effect::restore_prior_symlink(
            &parent_fd,
            &product,
            OsStr::new("current"),
            &missing_prev
        )
        .is_err()
    );

    // Unselected release removal guards: wrong identity and already-absent path.
    assert!(
        remove_owned_unselected_release(
            &product.join("releases/v1"),
            (0, 0),
            &product.join("current")
        )
        .is_err()
    );
    assert!(
        remove_owned_unselected_release(
            &product.join("releases/absent"),
            v1_id,
            &product.join("current")
        )
        .is_ok()
    );
    assert!(
        remove_owned_unselected_release(Path::new("rel"), v1_id, &product.join("current")).is_err()
    );
    assert!(
        remove_owned_unselected_release(
            &product.join("missing-parent/v1"),
            v1_id,
            &product.join("current")
        )
        .is_err()
    );
    symlink("releases/v1", product.join("link-release")).unwrap();
    assert!(
        remove_owned_unselected_release(
            &product.join("link-release"),
            v1_id,
            &product.join("current")
        )
        .is_err()
    );

    // Foreign symlink replacement refuses rollback without touching the foreign link.
    symlink("foreign-target", product.join("current")).unwrap();
    rec.phase = SelectorEffectPhase::SelectorPublished;
    rec.parent_identity = (parent_md.dev(), parent_md.ino());
    rec.previous = Some(PreviousSelectorRecord {
        target: PathBuf::from("releases/v0"),
        entry_identity: (999, 999),
        root_identity: (999, 998),
        input_manifest_sha256: None,
    });
    persist_record(&guard, &rec).unwrap();
    assert!(
        reconcile_selector(
            &guard,
            &pending,
            SelectorRecoveryAction::Rollback,
            None,
            None
        )
        .is_err()
    );
    assert_eq!(
        std::fs::read_link(product.join("current")).unwrap(),
        Path::new("foreign-target")
    );
    assert!(
        reconcile_selector(
            &guard,
            &pending,
            SelectorRecoveryAction::Complete,
            None,
            None
        )
        .is_err()
    );

    rec.phase = SelectorEffectPhase::IntentBound;
    persist_record(&guard, &rec).unwrap();
    assert!(
        reconcile_selector(
            &guard,
            &pending,
            SelectorRecoveryAction::Complete,
            None,
            None
        )
        .is_err()
    );
    rec.phase = SelectorEffectPhase::Verified;
    persist_record(&guard, &rec).unwrap();
    assert!(
        reconcile_selector(
            &guard,
            &pending,
            SelectorRecoveryAction::Complete,
            None,
            None
        )
        .is_err()
    );

    // Rollback edge cases in effect.rs: parent mismatch, owned-id target change (Install & Upgrade),
    // non-UTF8 targets, missing verify_symlink target, and symlink effect journal.
    use std::os::unix::ffi::OsStringExt;
    let non_utf8 = PathBuf::from(std::ffi::OsString::from_vec(vec![0xff]));
    let cur_link_md = std::fs::symlink_metadata(product.join("current")).unwrap();
    let cur_link_id = (cur_link_md.dev(), cur_link_md.ino());
    let mut bad_rec = rec.clone();
    bad_rec.parent_identity = (0, 0);
    assert!(
        super::super::effect::rollback_selector_effect(
            &guard,
            &parent_fd,
            &product,
            &mut bad_rec,
            None
        )
        .is_err()
    );

    let mut bad_target_none = rec.clone();
    bad_target_none.staging_identity = Some(cur_link_id);
    bad_target_none.previous = None;
    assert!(
        super::super::effect::rollback_selector_effect(
            &guard,
            &parent_fd,
            &product,
            &mut bad_target_none,
            None
        )
        .is_err()
    );

    let mut bad_target_some = rec.clone();
    bad_target_some.staging_identity = Some(cur_link_id);
    assert!(
        super::super::effect::rollback_selector_effect(
            &guard,
            &parent_fd,
            &product,
            &mut bad_target_some,
            None
        )
        .is_err()
    );

    let mut non_utf8_rec = rec.clone();
    non_utf8_rec.target = non_utf8.clone();
    assert!(
        super::super::effect::rollback_selector_effect(
            &guard,
            &parent_fd,
            &product,
            &mut non_utf8_rec,
            None
        )
        .is_err()
    );

    let mut no_leaf_rec = rec.clone();
    no_leaf_rec.selector = PathBuf::from("/");
    assert!(
        super::super::effect::rollback_selector_effect(
            &guard,
            &parent_fd,
            &product,
            &mut no_leaf_rec,
            None
        )
        .is_err()
    );

    assert!(
        super::super::effect::verify_symlink(&parent_fd, OsStr::new("absent-link"), "releases/v1")
            .is_err()
    );
    let non_utf8_prev = PreviousSelectorRecord {
        target: non_utf8,
        entry_identity: cur_link_id,
        root_identity: v1_id,
        input_manifest_sha256: None,
    };
    assert!(
        super::super::effect::check_live_selector_matches_prior(
            &parent_fd,
            OsStr::new("current"),
            Some(&non_utf8_prev),
        )
        .is_err()
    );
    assert!(
        super::super::effect::restore_prior_symlink(
            &parent_fd,
            &product,
            OsStr::new("current"),
            &non_utf8_prev,
        )
        .is_err()
    );

    std::fs::remove_file(&ej_path).unwrap();
    symlink("foreign", &ej_path).unwrap();
    assert!(super::super::effect::effect_journal_present(&paths).is_err());
    std::fs::remove_file(&ej_path).unwrap();

    // Missing selector parent during reconcile_selector hits open_directory_no_follow None branch.
    let gone = base.join("gone-parent");
    std::fs::create_dir(&gone).unwrap();
    rec.selector = gone.join("current");
    persist_record(&guard, &rec).unwrap();
    std::fs::remove_dir(&gone).unwrap();
    assert!(
        reconcile_selector(
            &guard,
            &pending,
            SelectorRecoveryAction::Rollback,
            None,
            None
        )
        .is_err()
    );
}
