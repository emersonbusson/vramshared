// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_transition.rs
//! Package transaction admission under the shared mutation guard.

use super::package_admission::{LifecycleSnapshot, PackagePlan, admit_package};
use super::plan::ReleaseIdentity;
use super::transition::{
    CustomerTransitionGuard, OperationOwner, PackagePending, TransitionPaths, acquire_transition,
    enroll_pending, read_pending, recovery_refusal,
};
use super::{CustomerError, CustomerReason, CustomerState};

/// Bind admission to the verified candidate before creating a durable record.
#[cfg(test)]
pub(super) fn begin_verified_package(
    paths: &TransitionPaths,
    request: &PackagePending,
    candidate: Option<&super::package_source::StreamVerifiedBundle<'_>>,
    signature: Option<&super::package_trust::VerifiedSignature>,
    observe: &mut dyn FnMut() -> Result<PackageObservation, CustomerError>,
    memory: &mut dyn FnMut() -> Option<super::package_source::stream::MemoryObservation>,
) -> Result<PackagePlan, CustomerError> {
    let transaction =
        begin_verified_package_held(paths, request, candidate, signature, observe, memory)?;
    Ok(transaction.plan)
}

/// Admission result retains the lock needed by the subsequent effect owner.
pub(super) struct AdmittedPackage<'a> {
    guard: CustomerTransitionGuard,
    plan: PackagePlan,
    source: Option<&'a super::package_source::DeclaredBundle>,
    candidate_release: Option<String>,
    verification_limits: Option<(u64, u64)>,
}

impl AdmittedPackage<'_> {
    pub(super) fn guard(&self) -> &CustomerTransitionGuard {
        &self.guard
    }
    pub(super) fn plan(&self) -> &PackagePlan {
        &self.plan
    }
    pub(super) fn candidate_release(&self) -> Option<&str> {
        self.candidate_release.as_deref()
    }
    pub(super) fn verification_limits(&self) -> Option<(u64, u64)> {
        self.verification_limits
    }
    pub(super) fn admits_source(&self, source: &super::package_source::DeclaredBundle) -> bool {
        self.source
            .is_some_and(|admitted| std::ptr::eq(admitted, source))
    }
}

/// Admit and persist while keeping the shared guard until the caller releases it.
pub(super) fn begin_verified_package_held<'a>(
    paths: &TransitionPaths,
    request: &PackagePending,
    candidate: Option<&super::package_source::StreamVerifiedBundle<'a>>,
    signature: Option<&super::package_trust::VerifiedSignature>,
    observe: &mut dyn FnMut() -> Result<PackageObservation, CustomerError>,
    memory: &mut dyn FnMut() -> Option<super::package_source::stream::MemoryObservation>,
) -> Result<AdmittedPackage<'a>, CustomerError> {
    use super::transition::PackageAction;
    match (request.action, candidate, signature) {
        (PackageAction::Install | PackageAction::Upgrade, Some(bundle), Some(proof))
            if bundle.manifest_sha256() == request.new_hash
                && proof.manifest_sha256() == request.new_hash
                && request.target_release.as_ref().is_some_and(|target| {
                    crate::release_identity::parse_identity_line(target.as_bytes()).as_deref()
                        == Some(target.as_str())
                })
                && proof.revalidate().is_some() => {}
        (PackageAction::Remove, None, None) if request.target_release.is_none() => {}
        _ => {
            return Err(CustomerError {
                reason: CustomerReason::IdentityChanged,
                detail: "verified bundle or signature does not match the package transaction"
                    .into(),
                state: CustomerState::InstalledOff,
            });
        }
    }
    let guard = acquire_transition(OperationOwner::Package, paths)?;
    let (plan, candidate_release) =
        admit_and_enroll(&guard, request, candidate, signature, observe, memory)?;
    Ok(AdmittedPackage {
        guard,
        plan,
        source: candidate.and_then(|bundle| bundle.source()),
        candidate_release,
        verification_limits: candidate.map(|bundle| bundle.verification_limits()),
    })
}

/// Observations must be obtained by a trusted owner while the guard is held.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PackageObservation {
    pub(crate) snapshot: LifecycleSnapshot,
    pub(crate) release: ReleaseIdentity,
    pub(crate) old_hash: String,
    pub(crate) new_hash: String,
}

/// Enroll once before package effects; a pending transaction is never replayed.
/// This coordinator does not itself authenticate or observe the host.
#[cfg(test)]
fn begin_package(
    paths: &TransitionPaths,
    request: &PackagePending,
    observe: &mut dyn FnMut() -> Result<PackageObservation, CustomerError>,
) -> Result<PackagePlan, CustomerError> {
    let guard = acquire_transition(OperationOwner::Package, paths)?;
    admit_and_enroll(&guard, request, None, None, observe, &mut || None).map(|(plan, _)| plan)
}

fn admit_and_enroll(
    guard: &CustomerTransitionGuard,
    request: &PackagePending,
    candidate: Option<&super::package_source::StreamVerifiedBundle<'_>>,
    signature: Option<&super::package_trust::VerifiedSignature>,
    observe: &mut dyn FnMut() -> Result<PackageObservation, CustomerError>,
    memory: &mut dyn FnMut() -> Option<super::package_source::stream::MemoryObservation>,
) -> Result<(PackagePlan, Option<String>), CustomerError> {
    let paths = guard.paths();
    if super::package_stage_journal::stage_journal_present(paths)?
        || super::package_publish::publication_journal_present(paths)?
        || super::package_tree::tree_journal_present(paths)?
        || super::package_selector::journal_present(paths)?
    {
        return Err(recovery_refusal(
            "package stage requires reconciliation before a new transaction",
        ));
    }
    if read_pending(paths)?.is_some() {
        return Err(recovery_refusal(
            "package transaction is already pending; reconcile before any new effect",
        ));
    }
    let first = observe()?;
    if first.old_hash != request.old_hash || first.new_hash != request.new_hash {
        return Err(CustomerError {
            reason: CustomerReason::IdentityChanged,
            detail: "package payload identity differs from the approved transaction".into(),
            state: CustomerState::InstalledOff,
        });
    }
    if crate::release_identity::parse_identity_line(first.release.version.as_bytes()).as_deref()
        != Some(first.release.version.as_str())
    {
        return Err(CustomerError {
            reason: CustomerReason::IdentityChanged,
            detail: "observed package release identity is malformed".into(),
            state: CustomerState::InstalledOff,
        });
    }
    let plan = admit_package(request.action, &first.snapshot, &first.release)?;
    if observe()? != first {
        return Err(CustomerError {
            reason: CustomerReason::IdentityChanged,
            detail: "package observation changed before pending enrollment".into(),
            state: CustomerState::InstalledOff,
        });
    }
    guard.revalidate()?;
    if signature.is_some_and(|proof| proof.revalidate().is_none()) {
        return Err(CustomerError {
            reason: CustomerReason::IdentityChanged,
            detail: "release trust policy or keyring changed before enrollment".into(),
            state: CustomerState::InstalledOff,
        });
    }
    let candidate_release = match (candidate, signature) {
        (Some(bundle), Some(proof)) => Some(
            bundle
                .authenticated_release_identity(proof, memory)
                .ok_or_else(|| CustomerError {
                    reason: CustomerReason::IdentityChanged,
                    detail: "package source, trust or memory admission changed before enrollment"
                        .into(),
                    state: CustomerState::InstalledOff,
                })?
                .to_owned(),
        ),
        (None, None) => None,
        _ => {
            return Err(recovery_refusal(
                "package candidate authority is incomplete",
            ));
        }
    };
    if candidate_release.is_some() && candidate_release != request.target_release {
        return Err(CustomerError {
            reason: CustomerReason::IdentityChanged,
            detail: "authenticated release differs from the requested target".into(),
            state: CustomerState::InstalledOff,
        });
    }
    if request.action == super::transition::PackageAction::Install
        && candidate_release.as_deref() != Some(first.release.version.as_str())
    {
        return Err(CustomerError {
            reason: CustomerReason::IdentityChanged,
            detail: "fresh-install plan differs from the authenticated target".into(),
            state: CustomerState::InstalledOff,
        });
    }
    if candidate.is_some() && observe()? != first {
        return Err(CustomerError {
            reason: CustomerReason::IdentityChanged,
            detail: "package lifecycle changed during source revalidation".into(),
            state: CustomerState::InstalledOff,
        });
    }
    if candidate.is_some_and(|bundle| bundle.check_fresh_memory(memory).is_none()) {
        return Err(CustomerError {
            reason: CustomerReason::IdentityChanged,
            detail: "memory admission became unavailable before enrollment".into(),
            state: CustomerState::InstalledOff,
        });
    }
    guard.revalidate()?;
    if signature.is_some_and(|proof| proof.revalidate().is_none()) {
        return Err(CustomerError {
            reason: CustomerReason::IdentityChanged,
            detail: "release trust changed during source revalidation".into(),
            state: CustomerState::InstalledOff,
        });
    }
    enroll_pending(guard, request)?;
    Ok((plan, candidate_release))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;
    use crate::customer::CustomerState;
    use crate::customer::package_admission::{LifecycleOwnership, LifecycleSnapshot};
    use crate::customer::transition::{
        OperationOwner, PackageAction, TransitionPaths, acquire_transition, read_pending,
    };
    use std::collections::VecDeque;

    #[test]
    fn selector_journal_refuses_new_admission_before_observation() {
        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("package-selector-pending-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let paths = TransitionPaths::under(&root);
        std::fs::create_dir_all(paths.transactions_root()).unwrap();
        let journal = paths.transactions_root().join("package-selector.json");
        std::fs::write(&journal, b"interrupted\n").unwrap();
        std::fs::set_permissions(&journal, std::fs::Permissions::from_mode(0o600)).unwrap();
        let pending = PackagePending {
            target_release: Some("v1".into()),
            action: PackageAction::Install,
            old_hash: "a".repeat(64),
            new_hash: "b".repeat(64),
            issued_unix: 1,
        };
        let mut observed = false;
        let result = begin_package(&paths, &pending, &mut || {
            observed = true;
            Err(recovery_refusal("unexpected observation"))
        });
        assert_eq!(result.unwrap_err().reason, CustomerReason::RecoveryRequired);
        assert!(!observed);
        assert!(read_pending(&paths).unwrap().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fresh_install_plan_uses_the_authenticated_target() {
        let root =
            std::env::temp_dir().join(format!("package-fresh-target-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let paths = TransitionPaths::under(&root);
        let pending = PackagePending {
            target_release: Some("fixture-release".into()),
            action: PackageAction::Install,
            old_hash: "a".repeat(64),
            new_hash: "b".repeat(64),
            issued_unix: 1,
        };
        let candidate = crate::customer::package_source::StreamVerifiedBundle::fixture(
            pending.new_hash.clone(),
        );
        let signature =
            crate::customer::package_trust::VerifiedSignature::fixture(pending.new_hash.clone());
        let mut observation = PackageObservation {
            snapshot: LifecycleSnapshot {
                state: CustomerState::Absent,
                owner: LifecycleOwnership::Owned,
                guarded_hooks: false,
            },
            release: ReleaseIdentity::new("different-release"),
            old_hash: pending.old_hash.clone(),
            new_hash: pending.new_hash.clone(),
        };
        assert!(
            begin_verified_package_held(
                &paths,
                &pending,
                Some(&candidate),
                Some(&signature),
                &mut || Ok(observation.clone()),
                &mut || None,
            )
            .is_err()
        );
        assert!(read_pending(&paths).unwrap().is_none());
        observation.release = ReleaseIdentity::new("fixture-release");
        let admitted = begin_verified_package_held(
            &paths,
            &pending,
            Some(&candidate),
            Some(&signature),
            &mut || Ok(observation.clone()),
            &mut || None,
        )
        .unwrap();
        assert_eq!(admitted.plan().old_version, "fixture-release");
        assert_eq!(read_pending(&paths).unwrap(), Some(pending));
        drop(admitted);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn admitted_transaction_keeps_guard_until_effect_owner_releases_it() {
        let root =
            std::env::temp_dir().join(format!("package-held-admission-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let paths = TransitionPaths::under(&root);
        let pending = PackagePending {
            target_release: Some("fixture-release".into()),
            action: PackageAction::Upgrade,
            old_hash: "a".repeat(64),
            new_hash: "b".repeat(64),
            issued_unix: 1,
        };
        let candidate = crate::customer::package_source::StreamVerifiedBundle::fixture(
            pending.new_hash.clone(),
        );
        let signature =
            crate::customer::package_trust::VerifiedSignature::fixture(pending.new_hash.clone());
        let observation = PackageObservation {
            snapshot: LifecycleSnapshot {
                state: CustomerState::InstalledOff,
                owner: LifecycleOwnership::Owned,
                guarded_hooks: true,
            },
            release: ReleaseIdentity::new("0.15.0"),
            old_hash: pending.old_hash.clone(),
            new_hash: pending.new_hash.clone(),
        };
        let transaction = begin_verified_package_held(
            &paths,
            &pending,
            Some(&candidate),
            Some(&signature),
            &mut || Ok(observation.clone()),
            &mut || None,
        )
        .unwrap();
        assert_eq!(transaction.plan().action, PackageAction::Upgrade);
        assert_eq!(transaction.candidate_release(), Some("fixture-release"));
        transaction.guard().revalidate().unwrap();
        assert_eq!(read_pending(&paths).unwrap(), Some(pending.clone()));
        assert!(acquire_transition(OperationOwner::Package, &paths).is_err());
        drop(transaction);
        let guard = acquire_transition(OperationOwner::Package, &paths).unwrap();
        drop(guard);
        assert_eq!(read_pending(&paths).unwrap(), Some(pending));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn package_admission_binds_verified_candidate_before_pending() {
        let root =
            std::env::temp_dir().join(format!("package-begin-verified-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let paths = TransitionPaths::under(&root);
        let pending = PackagePending {
            target_release: Some("fixture-release".into()),
            action: PackageAction::Upgrade,
            old_hash: "a".repeat(64),
            new_hash: "b".repeat(64),
            issued_unix: 1,
        };
        let bundle = crate::customer::package_source::StreamVerifiedBundle::fixture("c".repeat(64));
        let signature =
            crate::customer::package_trust::VerifiedSignature::fixture(pending.new_hash.clone());
        let wrong_signature =
            crate::customer::package_trust::VerifiedSignature::fixture("c".repeat(64));
        assert!(
            begin_verified_package(
                &paths,
                &pending,
                Some(&bundle),
                Some(&signature),
                &mut || panic!("mismatch must precede observation"),
                &mut || None,
            )
            .is_err()
        );
        assert!(
            begin_verified_package(
                &paths,
                &pending,
                None,
                Some(&signature),
                &mut || panic!("missing source must precede observation"),
                &mut || None
            )
            .is_err()
        );
        assert!(read_pending(&paths).unwrap().is_none());
        let bundle = crate::customer::package_source::StreamVerifiedBundle::fixture(
            pending.new_hash.clone(),
        );
        assert!(
            begin_verified_package(
                &paths,
                &pending,
                Some(&bundle),
                None,
                &mut || panic!("missing signature must precede observation"),
                &mut || None
            )
            .is_err()
        );
        assert!(
            begin_verified_package(
                &paths,
                &pending,
                Some(&bundle),
                Some(&wrong_signature),
                &mut || panic!("wrong signature must precede observation"),
                &mut || None,
            )
            .is_err()
        );
        assert!(read_pending(&paths).unwrap().is_none());
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
        begin_verified_package(
            &paths,
            &pending,
            Some(&bundle),
            Some(&signature),
            &mut || Ok(observed.clone()),
            &mut || None,
        )
        .unwrap();
        assert_eq!(read_pending(&paths).unwrap(), Some(pending));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn removal_admission_does_not_require_new_package_signature() {
        let root =
            std::env::temp_dir().join(format!("package-removal-admission-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let paths = TransitionPaths::under(&root);
        let pending = PackagePending {
            target_release: None,
            action: PackageAction::Remove,
            old_hash: "a".repeat(64),
            new_hash: "b".repeat(64),
            issued_unix: 1,
        };
        let observation = PackageObservation {
            snapshot: LifecycleSnapshot {
                state: CustomerState::InstalledOff,
                owner: LifecycleOwnership::Owned,
                guarded_hooks: true,
            },
            release: ReleaseIdentity::new("0.15.0"),
            old_hash: pending.old_hash.clone(),
            new_hash: pending.new_hash.clone(),
        };
        begin_verified_package(
            &paths,
            &pending,
            None,
            None,
            &mut || Ok(observation.clone()),
            &mut || None,
        )
        .unwrap();
        assert_eq!(read_pending(&paths).unwrap(), Some(pending));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn package_admission_refuses_lifecycle_change_after_source_recheck() {
        let root = std::env::temp_dir().join(format!(
            "package-admission-late-change-{}",
            std::process::id()
        ));
        std::fs::create_dir(&root).unwrap();
        let paths = TransitionPaths::under(&root);
        let pending = PackagePending {
            target_release: Some("fixture-release".into()),
            action: PackageAction::Upgrade,
            old_hash: "a".repeat(64),
            new_hash: "b".repeat(64),
            issued_unix: 1,
        };
        let candidate = crate::customer::package_source::StreamVerifiedBundle::fixture(
            pending.new_hash.clone(),
        );
        let signature =
            crate::customer::package_trust::VerifiedSignature::fixture(pending.new_hash.clone());
        let initial = PackageObservation {
            snapshot: LifecycleSnapshot {
                state: CustomerState::InstalledOff,
                owner: LifecycleOwnership::Owned,
                guarded_hooks: true,
            },
            release: ReleaseIdentity::new("0.15.0"),
            old_hash: pending.old_hash.clone(),
            new_hash: pending.new_hash.clone(),
        };
        let mut changed = initial.clone();
        changed.snapshot.state = CustomerState::Active;
        let mut observations = VecDeque::from([initial.clone(), initial, changed]);
        assert!(
            begin_verified_package(
                &paths,
                &pending,
                Some(&candidate),
                Some(&signature),
                &mut || Ok(observations.pop_front().unwrap()),
                &mut || None
            )
            .is_err()
        );
        assert!(read_pending(&paths).unwrap().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn admission_is_persisted_before_releasing_guard_and_refuses_replay() {
        let root = std::env::temp_dir().join(format!("package-begin-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let paths = TransitionPaths::under(&root);
        let pending = PackagePending {
            target_release: Some("fixture-release".into()),
            action: PackageAction::Upgrade,
            old_hash: "a".repeat(64),
            new_hash: "b".repeat(64),
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
        let mut observations = VecDeque::from([observed.clone(), observed.clone()]);
        begin_package(&paths, &pending, &mut || {
            assert!(acquire_transition(OperationOwner::Up, &paths).is_err());
            Ok(observations.pop_front().unwrap())
        })
        .unwrap();
        assert_eq!(read_pending(&paths).unwrap(), Some(pending.clone()));
        assert!(acquire_transition(OperationOwner::Up, &paths).is_err());
        assert!(
            begin_package(&paths, &pending, &mut || panic!(
                "pending must refuse before observation"
            ))
            .is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn changed_observation_never_writes_pending() {
        let root =
            std::env::temp_dir().join(format!("package-begin-changed-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let paths = TransitionPaths::under(&root);
        let pending = PackagePending {
            target_release: Some("fixture-release".into()),
            action: PackageAction::Upgrade,
            old_hash: "a".repeat(64),
            new_hash: "b".repeat(64),
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
        let mut changed_hash = observed.clone();
        changed_hash.new_hash = "c".repeat(64);
        let mut changed_owner = observed.clone();
        changed_owner.snapshot.owner = LifecycleOwnership::Foreign;
        let mut changed_hooks = observed.clone();
        changed_hooks.snapshot.guarded_hooks = false;
        let mut changed_release = observed.clone();
        changed_release.release = ReleaseIdentity::new("0.15.1");
        for changed in [changed_hash, changed_owner, changed_hooks, changed_release] {
            let mut observations = VecDeque::from([observed.clone(), changed]);
            assert!(
                begin_package(&paths, &pending, &mut || Ok(observations
                    .pop_front()
                    .unwrap()))
                .is_err()
            );
            assert!(read_pending(&paths).unwrap().is_none());
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalid_or_active_observation_never_enrolls() {
        let root =
            std::env::temp_dir().join(format!("package-begin-refusal-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let paths = TransitionPaths::under(&root);
        let pending = PackagePending {
            target_release: Some("fixture-release".into()),
            action: PackageAction::Upgrade,
            old_hash: "a".repeat(64),
            new_hash: "b".repeat(64),
            issued_unix: 1,
        };
        let observed = PackageObservation {
            snapshot: LifecycleSnapshot {
                state: CustomerState::InstalledOff,
                owner: LifecycleOwnership::Owned,
                guarded_hooks: true,
            },
            release: ReleaseIdentity::new("0.15.0"),
            old_hash: "c".repeat(64),
            new_hash: pending.new_hash.clone(),
        };
        assert!(begin_package(&paths, &pending, &mut || Ok(observed.clone())).is_err());
        let active = PackageObservation {
            snapshot: LifecycleSnapshot {
                state: CustomerState::Active,
                ..observed.snapshot
            },
            old_hash: pending.old_hash.clone(),
            ..observed.clone()
        };
        assert!(begin_package(&paths, &pending, &mut || Ok(active.clone())).is_err());
        let malformed = PackageObservation {
            release: ReleaseIdentity::new(""),
            old_hash: pending.old_hash.clone(),
            ..observed.clone()
        };
        assert!(begin_package(&paths, &pending, &mut || Ok(malformed.clone())).is_err());
        assert!(
            begin_package(&paths, &pending, &mut || Err(recovery_refusal(
                "observation failed"
            )))
            .is_err()
        );
        assert!(read_pending(&paths).unwrap().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_second_observation_and_invalid_hash_never_enroll() {
        let root =
            std::env::temp_dir().join(format!("package-begin-failure-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let paths = TransitionPaths::under(&root);
        let mut pending = PackagePending {
            target_release: Some("fixture-release".into()),
            action: PackageAction::Upgrade,
            old_hash: "a".repeat(64),
            new_hash: "b".repeat(64),
            issued_unix: 1,
        };
        let mut observed = PackageObservation {
            snapshot: LifecycleSnapshot {
                state: CustomerState::InstalledOff,
                owner: LifecycleOwnership::Owned,
                guarded_hooks: true,
            },
            release: ReleaseIdentity::new("0.15.0"),
            old_hash: pending.old_hash.clone(),
            new_hash: pending.new_hash.clone(),
        };
        let mut count = 0;
        assert!(
            begin_package(&paths, &pending, &mut || {
                count += 1;
                if count == 1 {
                    Ok(observed.clone())
                } else {
                    Err(recovery_refusal("second observation failed"))
                }
            })
            .is_err()
        );
        assert_eq!(count, 2);
        assert!(read_pending(&paths).unwrap().is_none());
        pending.new_hash = "invalid".into();
        observed.new_hash = pending.new_hash.clone();
        assert!(begin_package(&paths, &pending, &mut || Ok(observed.clone())).is_err());
        assert!(read_pending(&paths).unwrap().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replaced_guard_prevents_pending_enrollment() {
        let root = std::env::temp_dir().join(format!("package-begin-lock-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let paths = TransitionPaths::under(&root);
        let pending = PackagePending {
            target_release: Some("fixture-release".into()),
            action: PackageAction::Upgrade,
            old_hash: "a".repeat(64),
            new_hash: "b".repeat(64),
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
        let mut calls = 0;
        assert!(
            begin_package(&paths, &pending, &mut || {
                calls += 1;
                if calls == 2 {
                    let lock = paths.lock_root.join("transition.lock");
                    std::fs::rename(&lock, paths.lock_root.join("previous.lock")).unwrap();
                    std::fs::write(&lock, b"foreign").unwrap();
                }
                Ok(observed.clone())
            })
            .is_err()
        );
        assert!(read_pending(&paths).unwrap().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}
