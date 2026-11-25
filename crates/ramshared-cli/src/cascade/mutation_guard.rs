// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/mutation_guard.rs
//! Customer-mutation custody admission for cascade effects (ITEM-6).
//!
//! Every cascade mutation entrypoint acquires one
//! [`crate::customer::transition::CustomerTransitionGuard`] before its first
//! effect and holds it across the whole effect, including postconditions.
//! Nested lifecycle calls — `boot`'s activation step, legacy migration's final
//! `up` — **borrow the already held typed guard**; they never acquire this
//! non-reentrant lock again. Observation commands take neither this lock nor
//! the workload ledger lock.
//!
//! This module is the only place a [`CustomerError`] admission becomes a
//! [`CascadeError`] or [`BootError`]. The stable customer reason travels in the
//! refusal so an operator or a script can match it without parsing prose.

use crate::cascade::{CascadeError, boot::BootError};
use crate::customer::transition::{
    CustomerTransitionGuard, OperationOwner, TransitionPaths, acquire_transition,
};
use crate::customer::{CustomerError, CustomerReason};

/// Enter mutation custody under the product layout.
///
/// [`TransitionPaths::production`] is the single place those absolute paths are
/// named. A test that must not write to `/run` or `/var/lib` uses
/// [`enter_at`] instead.
pub(crate) fn enter(owner: OperationOwner) -> Result<CustomerTransitionGuard, CascadeError> {
    enter_at(owner, &TransitionPaths::production())
}

/// Enter mutation custody under an injected layout. Tests and nested owners
/// that already isolated a filesystem use this.
pub(crate) fn enter_at(
    owner: OperationOwner,
    paths: &TransitionPaths,
) -> Result<CustomerTransitionGuard, CascadeError> {
    acquire_transition(owner, paths).map_err(admission_refusal)
}

/// A refused admission is a cascade refusal whose message still names the
/// customer reason.
///
/// Custody-boundary failures (the lock name vanished or was replaced, a
/// confinement check failed) are [`CascadeError::UnsafeContainment`]: the
/// boundary that was supposed to make mutation safe is no longer trustworthy,
/// so no containment effect may proceed. Everything else is a
/// [`CascadeError::Precondition`] — a deterministic "not now", never retried
/// (NFR-5, invariant check).
fn admission_refusal(error: CustomerError) -> CascadeError {
    let text = error.to_string();
    match error.reason {
        CustomerReason::UnsupportedSurface | CustomerReason::IdentityChanged => {
            CascadeError::UnsafeContainment(format!("customer mutation custody refused: {text}"))
        }
        _ => CascadeError::Precondition(format!("customer mutation custody refused: {text}")),
    }
}

/// The same refusal for the bootstrap surface, which reports typed [`BootError`]
/// so `status` can surface a stable reason (NFR-6).
pub(crate) fn admission_refusal_boot(error: CustomerError) -> BootError {
    BootError::Custody {
        reason: error.reason.as_str(),
        detail: error.detail,
    }
}

/// Enter [`OperationOwner::Boot`] custody under the product layout, refusing as
/// [`BootError`] rather than [`CascadeError`] so the bootstrap surface keeps one
/// typed refusal vocabulary (NFR-6).
pub(crate) fn enter_boot() -> Result<CustomerTransitionGuard, BootError> {
    acquire_transition(OperationOwner::Boot, &TransitionPaths::production())
        .map_err(admission_refusal_boot)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::customer::transition::{
        PackageAction, PackagePending, enroll_pending, read_pending,
    };
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    fn temp_paths() -> (PathBuf, TransitionPaths) {
        let n = NEXT_DIR.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "ramshared-mutation-guard-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create temp root");
        (root.clone(), TransitionPaths::under(&root))
    }

    fn pending_fixture() -> PackagePending {
        PackagePending {
            target_release: None,
            action: PackageAction::Upgrade,
            old_hash: "a".repeat(64),
            new_hash: "b".repeat(64),
            issued_unix: 1,
        }
    }

    #[test]
    fn enter_admits_one_owner_and_refuses_a_second_until_the_first_drops() {
        let (root, paths) = temp_paths();
        let first = enter_at(OperationOwner::Up, &paths).expect("first custody admission");
        let error = enter_at(OperationOwner::Selection, &paths)
            .expect_err("a second owner must not share the non-reentrant guard");
        assert!(
            error.to_string().contains("active_session"),
            "contending custody must name the stable busy reason: {error}"
        );
        drop(first);
        enter_at(OperationOwner::Selection, &paths).expect("custody is free once the holder drops");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn enter_refuses_every_mutation_owner_while_a_package_record_is_pending() {
        let (root, paths) = temp_paths();
        let holder = enter_at(OperationOwner::Package, &paths).expect("package owner admission");
        enroll_pending(&holder, &pending_fixture()).expect("enroll the durable record");
        drop(holder);

        for owner in [
            OperationOwner::Setup,
            OperationOwner::Up,
            OperationOwner::Boot,
            OperationOwner::Selection,
        ] {
            let error = enter_at(owner, &paths)
                .expect_err("a durable package record must refuse every other mutation owner");
            assert!(
                error.to_string().contains("recovery_required"),
                "owner {} must refuse with the recovery reason: {error}",
                owner.as_str()
            );
            assert!(
                error.to_string().contains("is pending"),
                "owner {} must name the pending record: {error}",
                owner.as_str()
            );
        }

        // The package owner is the one allowed past a record it already owns.
        let again = enter_at(OperationOwner::Package, &paths)
            .expect("the enrolling owner may continue its own transaction");
        assert!(read_pending(&paths).expect("read").is_some());
        drop(again);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn admission_refusal_shape_is_bound_to_the_customer_reason() {
        let custody = admission_refusal(CustomerError {
            reason: CustomerReason::UnsupportedSurface,
            detail: "transition lock vanished under the held guard".into(),
            state: crate::customer::CustomerState::InstalledOff,
        });
        assert!(
            matches!(custody, CascadeError::UnsafeContainment(_)),
            "a broken custody boundary must be an unsafe-containment refusal: {custody}"
        );
        assert!(
            custody.to_string().contains("unsupported_surface"),
            "the stable reason must survive the mapping: {custody}"
        );

        let busy = admission_refusal(CustomerError {
            reason: CustomerReason::ActiveSession,
            detail: "another customer mutation holds the transition guard".into(),
            state: crate::customer::CustomerState::InstalledOff,
        });
        assert!(
            matches!(busy, CascadeError::Precondition(_)),
            "a busy guard is a precondition refusal, not a containment failure: {busy}"
        );
        assert!(busy.to_string().contains("active_session"));

        let pending = admission_refusal(CustomerError {
            reason: CustomerReason::RecoveryRequired,
            detail: "package upgrade is pending".into(),
            state: crate::customer::CustomerState::RecoveryRequired,
        });
        assert!(matches!(pending, CascadeError::Precondition(_)));
        assert!(pending.to_string().contains("recovery_required"));

        let boot = admission_refusal_boot(CustomerError {
            reason: CustomerReason::ActiveSession,
            detail: "another customer mutation holds the transition guard".into(),
            state: crate::customer::CustomerState::InstalledOff,
        });
        assert!(
            matches!(
                boot,
                BootError::Custody {
                    reason: "active_session",
                    ..
                }
            ),
            "the bootstrap refusal must keep the stable reason: {boot}"
        );
        assert!(
            boot.to_string().contains("active_session"),
            "the bootstrap display must name the reason: {boot}"
        );
    }
}
