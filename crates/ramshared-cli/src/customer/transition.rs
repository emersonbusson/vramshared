// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/transition.rs
//! Shared mutation guard and durable package-exclusion admission (ITEM-5).
//!
//! One owner of customer mutation. Unit, package and direct CLI paths all
//! acquire the guard declared in [`super::transition_custody`]; none of them
//! may bypass it. Observation commands take neither this lock nor the workload
//! ledger lock and report an in-progress transition without claiming a clean
//! [`super::CustomerState`].
//!
//! Two durable things live under [`TransitionPaths`]:
//!
//! - the **transition lock**, an exclusive non-blocking OS lock on a singly
//!   linked regular file beneath the lock root. It is released by process
//!   death, which is correct: a dead mutator holds nothing.
//! - a **[`PackagePending`] record**, fsynced before package effects and
//!   deliberately **not** held by the lock. A package-manager transaction
//!   cannot hold a lock merely by calling a short guard binary that exits, so
//!   exclusion is carried by this record instead. It outlives the hook process
//!   and blocks activation until the same identity commits or an attended
//!   reconciliation clears it.
//!
//! Lock order is fixed: the customer transition guard is taken **before** the
//! workload ledger lock, and no path acquires them in reverse.
//!
use rustix::fs::{AtFlags, FlockOperation, flock, fsync, unlinkat};
use serde::{Deserialize, Serialize};

pub(crate) use super::transition_custody::{
    CustomerTransitionGuard, OperationOwner, TransitionPaths, confinement_refusal,
    open_directory_no_follow, read_journal_body, validate_directory, write_journal_record,
};
use super::transition_custody::{open_named_file, validate_regular};
use super::{CustomerError, CustomerReason, CustomerState};

/// The package action a [`PackagePending`] record binds to.
///
/// Declared next to the record that needs it rather than in
/// `customer/package.rs`, which is the module the SPEC originally assigned it
/// to. `package.rs` imports this vocabulary instead of inventing a second
/// one; see the SPEC decision note.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PackageAction {
    Install,
    Upgrade,
    Remove,
}

impl PackageAction {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Upgrade => "upgrade",
            Self::Remove => "remove",
        }
    }
}

/// A durable package transaction that outlives the process that enrolled it.
///
/// Bound to the action and to the two content hashes the package is moving
/// between. A record naming other hashes is a different transaction and is
/// never silently adopted.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PackagePending {
    /// Exact attended target for an install or upgrade; absent in legacy records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) target_release: Option<String>,
    pub(crate) action: PackageAction,
    pub(crate) old_hash: String,
    pub(crate) new_hash: String,
    pub(crate) issued_unix: u64,
}

fn validate_pending_identity(pending: &PackagePending) -> Result<(), CustomerError> {
    if !super::package::is_lowercase_sha256(&pending.old_hash)
        || !super::package::is_lowercase_sha256(&pending.new_hash)
        || pending.target_release.as_ref().is_some_and(|target| {
            crate::release_identity::parse_identity_line(target.as_bytes()).as_deref()
                != Some(target.as_str())
        })
    {
        return Err(confinement_refusal(
            "package pending record requires canonical lowercase SHA-256 identities",
        ));
    }
    Ok(())
}

fn require_package_owner(
    guard: &CustomerTransitionGuard,
    operation: &str,
) -> Result<(), CustomerError> {
    if guard.owner() != OperationOwner::Package {
        return Err(confinement_refusal(format!(
            "{operation} requires the package transition owner"
        )));
    }
    Ok(())
}

fn busy_refusal(detail: impl Into<String>) -> CustomerError {
    CustomerError {
        reason: CustomerReason::ActiveSession,
        detail: detail.into(),
        state: CustomerState::InstalledOff,
    }
}

/// Owners that must not run while a package transaction is pending.
///
/// A package action itself enrolls the record, so it is the one owner allowed
/// past a pending record it already owns.
fn refuses_pending_package(owner: OperationOwner) -> bool {
    !matches!(owner, OperationOwner::Package)
}

/// The PRD row for an effect that failed with uncertain device state: exit 5,
/// and the state that needs attended reconciliation.
pub(crate) fn recovery_refusal(detail: impl Into<String>) -> CustomerError {
    CustomerError {
        reason: CustomerReason::RecoveryRequired,
        detail: detail.into(),
        state: CustomerState::RecoveryRequired,
    }
}

/// Acquire the one customer mutation guard.
///
/// `paths` is injected so a test never writes to the product layout; callers
/// that mean the product pass [`TransitionPaths::production`]. Contention is
/// `active_session` (exit 4) and never a wait: no unattended operation blocks
/// on the mutation lock.
///
/// A durable [`PackagePending`] record left behind by a hook that has already
/// exited is an interrupted package transaction. Owners that must not run
/// under one are refused as `recovery_required` (exit 5) with the state that
/// needs attended reconciliation — the record is never silently cleared.
pub(crate) fn acquire_transition(
    owner: OperationOwner,
    paths: &TransitionPaths,
) -> Result<CustomerTransitionGuard, CustomerError> {
    let lock_file = open_named_file(
        &paths.lock_root,
        TransitionPaths::lock_file_name(),
        "transition lock",
        true,
    )?
    .ok_or_else(|| confinement_refusal("transition lock was not created under the lock root"))?;
    let lock_identity = validate_regular(&lock_file, "transition lock")?;
    match flock(&lock_file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(error) if error == rustix::io::Errno::WOULDBLOCK => {
            return Err(busy_refusal(
                "another customer mutation holds the transition guard; no new effect is started",
            ));
        }
        Err(error) => {
            return Err(confinement_refusal(format!(
                "lock customer transition guard: {error}"
            )));
        }
    }

    let guard = CustomerTransitionGuard::adopted(lock_file, lock_identity, owner, paths.clone());
    if let Err(error) = guard.revalidate() {
        drop(guard);
        return Err(error);
    }

    if owner != OperationOwner::Setup {
        super::transaction::ensure_setup_reconciled(paths)?;
    }

    if refuses_pending_package(owner) {
        if super::package_stage_journal::stage_journal_present(paths)?
            || super::package_publish::publication_journal_present(paths)?
            || super::package_tree::tree_journal_present(paths)?
            || super::package_selector::journal_present(paths)?
        {
            return Err(recovery_refusal(
                "package stage requires reconciliation; no mutation is admitted",
            ));
        }
        match read_pending(paths) {
            Ok(Some(pending)) => {
                drop(guard);
                return Err(recovery_refusal(format!(
                    "package {} from {} to {} is pending; activation is refused until the same identity commits or an attended reconciliation clears it",
                    pending.action.as_str(),
                    pending.old_hash,
                    pending.new_hash
                )));
            }
            Ok(None) => {}
            Err(error) => {
                drop(guard);
                return Err(error);
            }
        }
    }

    Ok(guard)
}

/// Observe a durable package transaction. Takes no lock: an observation
/// command must be able to report a pending record without becoming a mutator.
pub(crate) fn read_pending(
    paths: &TransitionPaths,
) -> Result<Option<PackagePending>, CustomerError> {
    let Some(body) = read_journal_body(
        paths,
        TransitionPaths::pending_file_name(),
        "package pending record",
    )?
    else {
        return Ok(None);
    };
    let record: PackagePending = serde_json::from_str(&body).map_err(|error| {
        confinement_refusal(format!(
            "package pending record is not a strict record: {error}"
        ))
    })?;
    validate_pending_identity(&record)?;
    Ok(Some(record))
}

/// Enroll a package transaction under an already held guard.
///
/// The record is fsynced before the caller performs any package effect. That
/// ordering is the whole contract: if the effect runs and the record does not
/// exist, a crash cannot be told apart from a clean finish.
pub(crate) fn enroll_pending(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
) -> Result<(), CustomerError> {
    require_package_owner(guard, "enroll package pending record")?;
    validate_pending_identity(pending)?;
    guard.revalidate()?;
    if let Some(existing) = read_pending(guard.paths())? {
        if existing == *pending {
            // Idempotent re-enroll of the same transaction.
            return Ok(());
        }
        return Err(busy_refusal(
            "a different package transaction is already pending; it is never silently replaced",
        ));
    }

    let body = serde_json::to_string(pending)
        .map_err(|error| confinement_refusal(format!("encode package pending record: {error}")))?;
    write_journal_record(
        guard,
        TransitionPaths::pending_file_name(),
        "package pending record",
        &body,
    )
}

/// Clear a package transaction under an already held guard.
///
/// Callers must have already proved quiescence and preserved the receipt.
/// Clearing without that proof is how a half-applied package becomes a clean
/// `Ready` claim.
pub(crate) fn clear_pending(
    guard: &CustomerTransitionGuard,
    expected: &PackagePending,
) -> Result<(), CustomerError> {
    require_package_owner(guard, "clear package pending record")?;
    validate_pending_identity(expected)?;
    guard.revalidate()?;
    if super::package_stage_journal::stage_journal_present(guard.paths())?
        || super::package_publish::publication_journal_present(guard.paths())?
        || super::package_tree::tree_journal_present(guard.paths())?
        || super::package_selector::journal_present(guard.paths())?
    {
        return Err(recovery_refusal(
            "package stage requires reconciliation before clearing pending",
        ));
    }
    match read_pending(guard.paths())? {
        Some(current) if current == *expected => {}
        Some(_) => {
            return Err(recovery_refusal(
                "package transaction identity changed before clear",
            ));
        }
        None => {
            return Err(recovery_refusal(
                "package transaction record is missing; completion is not proved",
            ));
        }
    }
    let directory = open_directory_no_follow(guard.paths().transactions_root(), true)?
        .ok_or_else(|| confinement_refusal("package transaction journal vanished during clear"))?;
    validate_directory(&directory, "package transaction journal")?;
    match unlinkat(
        &directory,
        TransitionPaths::pending_file_name(),
        AtFlags::empty(),
    ) {
        Ok(()) => {}
        Err(error) if error == rustix::io::Errno::NOENT => {
            return Err(recovery_refusal(
                "package transaction record disappeared during clear",
            ));
        }
        Err(error) => {
            return Err(confinement_refusal(format!(
                "clear package pending record: {error}"
            )));
        }
    }
    fsync(&directory).map_err(|error| {
        confinement_refusal(format!("sync package transaction journal: {error}"))
    })?;
    guard.revalidate()
}

#[cfg(test)]
mod tests;
