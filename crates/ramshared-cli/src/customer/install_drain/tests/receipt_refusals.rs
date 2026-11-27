// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared

use super::super::super::transition::{
    PackageAction, PackagePending, clear_pending, enroll_pending,
};
use super::*;

#[test]
fn terminal_drain_keeps_its_receipt_until_exact_package_completion() {
    let fixture = Fixture::new();
    let lock = InstallerLock::acquire(&fixture.paths).unwrap();
    let mut receipt = intent();
    let inputs = receipt.inputs.clone();
    let pending = PackagePending {
        target_release: Some(inputs.candidate_release.clone()),
        action: PackageAction::Upgrade,
        old_hash: inputs.previous_manifest.clone(),
        new_hash: inputs.candidate_manifest.clone(),
        issued_unix: 1,
    };

    assert!(lock.package_recovery_plan(&pending).unwrap().is_none());
    lock.publish_intent(&receipt).unwrap();
    let error = lock.package_recovery_plan(&pending).err().unwrap();
    assert!(error.detail.contains("requires a completed drain"));
    receipt.phase = Phase::Drained;
    lock.publish(&receipt).unwrap();
    let error = lock.package_recovery_plan(&pending).err().unwrap();
    assert!(error.detail.contains("enrollment choice was not recorded"));
    lock.record_package_plan(false).unwrap();

    let mut foreign = pending.clone();
    foreign.new_hash = "d".repeat(64);
    let error = lock.package_recovery_plan(&foreign).err().unwrap();
    assert!(error.detail.contains("differs from the drained upgrade"));
    let plan = lock.package_recovery_plan(&pending).unwrap().unwrap();
    assert_eq!(plan.inputs, inputs);
    assert!(!plan.enroll);
    assert!(plan.committed.is_none());

    let guard = acquire_transition(OperationOwner::Package, &fixture.paths).unwrap();
    enroll_pending(&guard, &pending).unwrap();
    drop(guard);
    let error = lock.clear_finished(None, false).unwrap_err();
    assert!(error.detail.contains("receipt changed before completion"));
    let error = lock.clear_finished(Some(&inputs), false).unwrap_err();
    assert!(error.detail.contains("package effects remain pending"));
    receipt.effective_enrollment = Some(false);
    assert_eq!(lock.read().unwrap(), Some(receipt));

    let guard = acquire_transition(OperationOwner::Package, &fixture.paths).unwrap();
    clear_pending(&guard, &pending).unwrap();
    drop(guard);
    lock.clear_finished(Some(&inputs), false).unwrap();
    lock.clear_finished(Some(&inputs), false).unwrap();
    assert!(lock.read().unwrap().is_none());
}

#[test]
fn missing_or_replaced_lock_cannot_publish_a_drain_receipt() {
    let fixture = Fixture::new();
    let lock = InstallerLock::acquire(&fixture.paths).unwrap();
    let original = fixture.paths.lock_root.join(LOCK_NAME);
    let held = fixture.paths.lock_root.join("retained-lock");
    std::fs::rename(&original, &held).unwrap();
    let error = lock.publish(&intent()).unwrap_err();
    assert!(error.detail.contains("installer lock disappeared"));

    let replacement = InstallerLock::acquire(&fixture.paths).unwrap();
    let error = lock.publish(&intent()).unwrap_err();
    assert!(error.detail.contains("installer lock identity changed"));
    assert!(replacement.read().unwrap().is_none());

    drop(replacement);
    std::fs::remove_file(&original).unwrap();
    std::fs::rename(&held, &original).unwrap();
    lock.publish_intent(&intent()).unwrap();
    assert_eq!(lock.read().unwrap().unwrap().phase, Phase::Intent);
}

#[test]
fn corrupt_or_wrong_version_receipts_never_restart_the_stop() {
    let fixture = Fixture::new();
    let lock = InstallerLock::acquire(&fixture.paths).unwrap();
    let inputs = signed_pair();
    let systemd = FakeStop::new([], 11);
    let mut receipt = intent();
    receipt.schema_version = 2;
    lock.publish(&receipt).unwrap();
    let fail = || {
        lock.prepare(Some(&inputs), &occupied(), &NoopLifecycleHost, &systemd)
            .err()
            .unwrap()
    };

    assert!(
        fail()
            .detail
            .contains("invalid upgrade drain receipt identity or posture")
    );
    let path = fixture.paths.transactions_root().join(RECEIPT_NAME);
    std::fs::write(&path, b"{").unwrap();
    assert!(fail().detail.contains("invalid upgrade drain receipt"));
    assert_eq!(std::fs::read(&path).unwrap(), b"{");
    assert_eq!(systemd.submit_count.get(), 0);
}
