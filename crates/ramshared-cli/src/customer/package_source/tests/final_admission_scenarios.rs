// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_source/tests/final_admission_scenarios.rs

fn exercise_final_admission(
    root: &std::path::Path,
    stage_pending: &crate::customer::transition::PackagePending,
    bundle: &DeclaredBundle,
    lines: &[String],
    verified: &VerifiedBundle,
    checksums: &str,
) {
    use crate::customer::package::ManifestArchitecture::Amd64;
    use crate::customer::package_stage_journal::inventory::verify_staged_inventory;
    use crate::customer::transition::{
        OperationOwner, PackageAction, PackagePending, TransitionPaths, acquire_transition,
        enroll_pending,
    };
    let missing = format!("{}\n", lines[..lines.len() - 1].join("\n"));
    std::fs::write(root.join("SHA256SUMS"), &missing).unwrap();
    let incomplete = DeclaredBundle::open(root, &digest(missing.as_bytes()).unwrap()).unwrap();
    assert!(incomplete.verify_all_for(1024 * 1024, Amd64).is_none());
    let disagreement_paths = TransitionPaths::under(&root.join("disagreement-transaction"));
    let disagreement_guard =
        acquire_transition(OperationOwner::Package, &disagreement_paths).unwrap();
    let mut disagreement_pending = stage_pending.clone();
    disagreement_pending.new_hash = digest(missing.as_bytes()).unwrap();
    enroll_pending(&disagreement_guard, &disagreement_pending).unwrap();
    assert!(
        verify_staged_inventory(&disagreement_guard, &disagreement_pending, &incomplete).is_err()
    );
    drop(disagreement_guard);
    let streamed = bundle
        .verify_streamed_for(u64::MAX, u64::MAX, Amd64)
        .unwrap();
    std::fs::write(root.join("bin/ramshared"), b"tampered").unwrap();
    assert_eq!(
        verified.members.get("ramshared").unwrap().bytes,
        fixture_elf(62)
    );
    assert!(bundle.verify_all_for(1024 * 1024, Amd64).is_none());
    use crate::customer::CustomerState;
    use crate::customer::package_admission::{LifecycleOwnership, LifecycleSnapshot};
    use crate::customer::package_transition::{PackageObservation, begin_verified_package};
    use crate::customer::plan::ReleaseIdentity;
    use crate::customer::transition::read_pending;
    let paths = TransitionPaths::under(&root.join("transaction-fixture"));
    let pending = PackagePending {
        target_release: Some("v1".into()),
        action: PackageAction::Upgrade,
        old_hash: "a".repeat(64),
        new_hash: verified.manifest_sha256().into(),
        issued_unix: 1,
    };
    let observed = PackageObservation {
        snapshot: LifecycleSnapshot {
            state: CustomerState::InstalledOff,
            owner: LifecycleOwnership::Owned,
            guarded_hooks: true,
        },
        release: ReleaseIdentity::new("0.15.0"),
        old_hash: pending.old_hash.clone(),
        new_hash: pending.new_hash.clone(),
    };
    assert!(
        bundle
            .verify_streamed_for(u64::MAX, u64::MAX, Amd64)
            .is_none()
    );
    let signature =
        crate::customer::package_trust::VerifiedSignature::fixture(pending.new_hash.clone());
    assert!(
        begin_verified_package(
            &paths,
            &pending,
            Some(&streamed),
            Some(&signature),
            &mut || Ok(observed.clone()),
            &mut || None,
        )
        .is_err()
    );
    assert!(read_pending(&paths).unwrap().is_none());
    std::fs::write(root.join("bin/ramshared"), fixture_elf(62)).unwrap();
    begin_verified_package(
        &paths,
        &pending,
        Some(&streamed),
        Some(&signature),
        &mut || Ok(observed.clone()),
        &mut || None,
    )
    .unwrap();
    assert_eq!(read_pending(&paths).unwrap(), Some(pending));
    let extra = format!("{checksums}{}  ./unexpected\n", digest(b"x").unwrap());
    std::fs::write(root.join("SHA256SUMS"), &extra).unwrap();
    let with_extra = DeclaredBundle::open(root, &digest(extra.as_bytes()).unwrap()).unwrap();
    assert!(with_extra.verify_all_for(1024 * 1024, Amd64).is_none());
}
