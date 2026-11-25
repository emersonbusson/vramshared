// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/transaction.rs
//! Supervised setup effects and a durable transaction receipt (ITEM-5).
//!
//! A setup is not "the command returned zero". It is a sequence of effects that
//! leave a persistent origin behind, and a crash between two of them must be
//! distinguishable from a clean finish on the next boot. This module is the
//! coordinator: it writes a durable [`TransactionRecord`] **before** the first
//! effect, asks the effect owner for the owned-origin count, and only then
//! commits the record and returns an [`InstallationReceipt`].
//!
//! Three rules make the receipt mean something:
//!
//! - **Identity is pinned.** The approval must bind the exact plan fingerprint,
//!   and the committed record remembers the `(plan, approval)` pair that owns
//!   the origin. A different pair is a re-plan, never a second origin.
//! - **Effects are typed.** [`SetupEffects`] names the approved operations.
//!   There is no shell, no command string and no path an operator could widen.
//! - **Interruption is recovery.** An effect error or a count that is not
//!   exactly one leaves the record at [`TransactionPhase::Applying`] and
//!   refuses every later apply as `recovery_required` (exit 5). The record is
//!   retained. The explicit reconciliation owner can commit exact existing
//!   read-back; it never clears the journal or reissues an allocation.
//!
//! The record lives under the transactions root
//! [`super::transition::TransitionPaths`] names, because that is the single
//! name of the durable customer-mutation layout. Its filename is owned here:
//! a setup transaction is not a package pending record.
//!
//! An installation rollback cannot silently undo a successful origin
//! initialization or free active swap. Persistent origins live outside
//! package-owned release directories, which is why the only effect this
//! coordinator issues is the typed create of one owned origin.
//!
use std::ffi::OsStr;

use serde::{Deserialize, Serialize};

use super::plan::{ApprovalBinding, SetupPlan, binding_matches};
use super::transition::{
    CustomerTransitionGuard, OperationOwner, TransitionPaths, confinement_refusal,
    read_journal_body, recovery_refusal, write_journal_record,
};
use super::{CustomerError, CustomerReason, CustomerState};

mod reconciliation;
pub(crate) use reconciliation::reconcile_setup;

/// Durable state of one setup transaction.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TransactionPhase {
    /// Written before the first effect. Survives a crash and is never a clean
    /// state: the next apply is refused until attended reconciliation.
    Applying,
    /// Written only after exactly one owned origin has been observed.
    Committed,
}

/// The durable record of one setup attempt.
///
/// `deny_unknown_fields` because an ignored field is how a second meaning is
/// smuggled into a grant. While the phase is [`TransactionPhase::Applying`]
/// there is no `applied_unix`: a commit timestamp on a half-applied setup is
/// exactly the lie the record exists to prevent.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TransactionRecord {
    pub(crate) plan_fingerprint: String,
    pub(crate) approval_id: String,
    pub(crate) release_version: String,
    pub(crate) phase: TransactionPhase,
    pub(crate) issued_unix: u64,
    pub(crate) applied_unix: Option<u64>,
}

impl TransactionRecord {
    /// True when this record is the completed setup of exactly that plan under
    /// exactly that grant. Any other committed record is a different owner.
    fn owns(&self, plan_fingerprint: &str, approval_id: &str) -> bool {
        self.phase == TransactionPhase::Committed
            && self.plan_fingerprint == plan_fingerprint
            && self.approval_id == approval_id
    }
}

/// What a completed setup proved. Returned to the caller; the durable copy of
/// the same facts is the committed [`TransactionRecord`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InstallationReceipt {
    pub(crate) plan_fingerprint: String,
    pub(crate) approval_id: String,
    pub(crate) release_version: String,
    pub(crate) applied_unix: u64,
}

/// Typed, approved setup effects. Never arbitrary shell execution.
///
/// An error from either method means the resulting device state is
/// **uncertain**: the coordinator cannot prove an origin is absent after a
/// failed create, so it keeps the `Applying` record and refuses a recovery-free
/// retry. An owner that can prove it changed nothing still returns `Ok`, and
/// the count check is what rejects the outcome.
pub(crate) trait SetupEffects {
    /// Create the one persistent origin this plan owns.
    fn create_owned_origin(&mut self, plan: &SetupPlan) -> Result<(), CustomerError>;

    /// Complete metadata for an already proven inode, never allocate payload.
    /// The reconciler calls this only after exact original-attempt validation.
    fn publish_existing_origin(&self, _plan: &SetupPlan) -> Result<(), CustomerError> {
        Ok(())
    }

    /// How many origins this plan currently owns. Must be exactly `1` after a
    /// successful create, and must not grow across an idempotent re-apply.
    fn owned_origin_count(&self, plan: &SetupPlan) -> Result<u32, CustomerError>;
}

/// Filename of the setup transaction record, beneath the transactions root.
fn record_file_name() -> &'static OsStr {
    OsStr::new("setup-transaction.json")
}

/// Identity fields written to the journal are bounded so a hostile record
/// cannot bloat it. The approval parser bounds the same values at parse time.
const MAX_IDENTITY_FIELD_BYTES: usize = 128;

fn identity_changed(detail: impl Into<String>) -> CustomerError {
    CustomerError {
        reason: CustomerReason::IdentityChanged,
        detail: detail.into(),
        state: CustomerState::InstalledOff,
    }
}

fn authorization_required(detail: impl Into<String>) -> CustomerError {
    CustomerError {
        reason: CustomerReason::AuthorizationRequired,
        detail: detail.into(),
        state: CustomerState::InstalledOff,
    }
}

/// Refuse a request that is malformed on a new command surface (exit 2).
fn malformed(detail: impl Into<String>) -> CustomerError {
    confinement_refusal(detail)
}

fn validate_identity_fields(record: &TransactionRecord) -> Result<(), CustomerError> {
    for (label, value) in [
        ("plan_fingerprint", record.plan_fingerprint.as_str()),
        ("approval_id", record.approval_id.as_str()),
        ("release_version", record.release_version.as_str()),
    ] {
        if value.is_empty() || value.len() > MAX_IDENTITY_FIELD_BYTES {
            return Err(malformed(format!(
                "{label} length {} is outside 1..={MAX_IDENTITY_FIELD_BYTES}",
                value.len()
            )));
        }
    }
    match (record.phase, record.applied_unix) {
        (TransactionPhase::Applying, None) | (TransactionPhase::Committed, Some(_)) => Ok(()),
        (TransactionPhase::Applying, Some(_)) => Err(malformed(
            "an applying setup transaction has no applied_unix",
        )),
        (TransactionPhase::Committed, None) => Err(malformed(
            "a committed setup transaction must record applied_unix",
        )),
    }
}

fn read_record(paths: &TransitionPaths) -> Result<Option<TransactionRecord>, CustomerError> {
    // Absent journal directory or absent record name is the normal empty
    // state: no setup has ever been attempted under this layout.
    let Some(body) = read_journal_body(paths, record_file_name(), "setup transaction record")?
    else {
        return Ok(None);
    };
    let record: TransactionRecord = serde_json::from_str(&body).map_err(|error| {
        malformed(format!(
            "setup transaction record is not a strict record: {error}"
        ))
    })?;
    validate_identity_fields(&record)?;
    Ok(Some(record))
}

/// An uncertain setup outlives its process and blocks other mutation owners.
pub(super) fn ensure_setup_reconciled(paths: &TransitionPaths) -> Result<(), CustomerError> {
    match read_record(paths).map_err(|error| {
        recovery_refusal(format!(
            "cannot establish setup transaction state; record retained: {error}"
        ))
    })? {
        Some(record) if record.phase == TransactionPhase::Applying => Err(recovery_refusal(
            "setup transaction is still applying; attended reconciliation is required before another mutation",
        )),
        _ => Ok(()),
    }
}

fn write_record(
    guard: &CustomerTransitionGuard,
    record: &TransactionRecord,
) -> Result<(), CustomerError> {
    validate_identity_fields(record)?;
    let body = serde_json::to_string(record)
        .map_err(|error| malformed(format!("encode setup transaction record: {error}")))?;
    write_journal_record(guard, record_file_name(), "setup transaction record", &body)
}

/// Apply one approved setup under an already held transition guard.
///
/// `now_unix` is injected so the approval window is decided by the caller's
/// clock, not by a hidden wall-clock read: a test that would have to wait for a
/// grant to expire cannot be a unit test, and a coordinator that silently
/// consults `SystemTime` cannot be replayed.
///
/// The guard must already be owned by [`OperationOwner::Setup`]. The caller
/// acquired it; this function never takes the lock and never waits.
pub(crate) fn apply_setup(
    plan: SetupPlan,
    approval: ApprovalBinding,
    guard: &mut CustomerTransitionGuard,
    effects: &mut impl SetupEffects,
    now_unix: u64,
) -> Result<InstallationReceipt, CustomerError> {
    if guard.owner() != OperationOwner::Setup {
        return Err(malformed(format!(
            "setup effects require the setup owner, not {}",
            guard.owner().as_str()
        )));
    }
    guard.revalidate()?;
    plan.validate_operation()?;
    let plan_fingerprint = plan.fingerprint();
    if approval.plan_fingerprint != plan_fingerprint {
        return Err(identity_changed(format!(
            "approval binds plan {} and this plan is {}; re-plan required, no new effects",
            approval.plan_fingerprint, plan_fingerprint
        )));
    }

    if let Some(existing) = read_record(guard.paths())? {
        if existing.phase == TransactionPhase::Applying {
            return Err(recovery_refusal(format!(
                "setup of plan {} under approval {} was interrupted in {} and is still applying; the record is retained and attended reconciliation is required",
                existing.plan_fingerprint, existing.approval_id, existing.release_version
            )));
        }
        if existing.owns(&plan_fingerprint, &approval.approval_id)
            && existing.release_version == plan.release_version
        {
            // the same approved setup applied twice is one
            // origin and one receipt. No new effect is issued.
            verify_owned_origin(effects, &plan)?;
            guard.revalidate()?;
            return Ok(receipt_from(&existing, &plan_fingerprint, &approval));
        }
        return Err(identity_changed(format!(
            "a committed setup of plan {} under approval {} already owns the origin; this request is a different identity and produces no new effects",
            existing.plan_fingerprint, existing.approval_id
        )));
    }

    if !binding_matches(&approval, &plan, now_unix) {
        return Err(authorization_required(format!(
            "approval {} is valid [{}, {}) and now is {}; no new effects",
            approval.approval_id, approval.issued_unix, approval.expires_unix, now_unix
        )));
    }

    let applying = TransactionRecord {
        plan_fingerprint: plan_fingerprint.clone(),
        approval_id: approval.approval_id.clone(),
        release_version: plan.release_version.clone(),
        phase: TransactionPhase::Applying,
        issued_unix: now_unix,
        applied_unix: None,
    };
    write_record(guard, &applying)?;

    if let Err(error) = effects.create_owned_origin(&plan) {
        // The record stays at Applying on purpose: a failed create leaves the
        // device state uncertain, and the next apply must not look like a
        // first attempt.
        return Err(recovery_refusal(format!(
            "creating the owned origin failed with uncertain device state: {error}"
        )));
    }

    verify_owned_origin(effects, &plan)?;

    let committed = TransactionRecord {
        plan_fingerprint: plan_fingerprint.clone(),
        approval_id: approval.approval_id.clone(),
        release_version: plan.release_version.clone(),
        phase: TransactionPhase::Committed,
        issued_unix: applying.issued_unix,
        applied_unix: Some(now_unix),
    };
    write_record(guard, &committed)?;
    Ok(receipt_from(&committed, &plan_fingerprint, &approval))
}

/// A committed journal is not evidence that its origin still exists.
fn verify_owned_origin(effects: &impl SetupEffects, plan: &SetupPlan) -> Result<(), CustomerError> {
    let owned = effects.owned_origin_count(plan).map_err(|error| {
        recovery_refusal(format!("cannot observe the owned origin count: {error}"))
    })?;
    if owned != 1 {
        return Err(recovery_refusal(format!(
            "expected exactly one owned origin and observed {owned}; the transaction record is retained"
        )));
    }

    Ok(())
}

fn receipt_from(
    record: &TransactionRecord,
    plan_fingerprint: &str,
    approval: &ApprovalBinding,
) -> InstallationReceipt {
    InstallationReceipt {
        plan_fingerprint: plan_fingerprint.to_string(),
        approval_id: approval.approval_id.clone(),
        release_version: record.release_version.clone(),
        applied_unix: record.applied_unix.unwrap_or(record.issued_unix),
    }
}

#[cfg(test)]
mod tests;
