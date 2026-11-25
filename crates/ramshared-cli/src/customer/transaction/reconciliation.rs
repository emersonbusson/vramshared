// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/transaction/reconciliation.rs
//! Attended completion of an exact existing, independently proven setup inode.
use super::*;

pub(crate) fn reconcile_setup(
    plan: SetupPlan,
    approval: ApprovalBinding,
    guard: &mut CustomerTransitionGuard,
    effects: &impl SetupEffects,
    now_unix: u64,
) -> Result<InstallationReceipt, CustomerError> {
    if guard.owner() != OperationOwner::Setup {
        return Err(malformed(
            "setup reconciliation requires the setup transition owner",
        ));
    }
    guard.revalidate()?;
    plan.validate_operation()?;
    let existing = read_record(guard.paths())
        .map_err(|error| recovery_refusal(format!("cannot reconcile setup journal: {error}")))?
        .ok_or_else(|| recovery_refusal("no setup transaction is available for reconciliation"))?;
    let fingerprint = plan.fingerprint();
    validate_original_attempt(&existing, &plan, &approval)?;
    if existing.phase == TransactionPhase::Applying
        && (now_unix == 0 || now_unix < existing.issued_unix)
    {
        return Err(CustomerError {
            reason: CustomerReason::ObservationUnavailable,
            detail:
                "reconciliation completion clock is unavailable or precedes the original attempt"
                    .into(),
            state: CustomerState::RecoveryRequired,
        });
    }

    // This explicit effect may publish missing metadata for an independently
    // proven existing inode. It is ordered after all original-attempt checks;
    // immutable access excludes the allocation mutator.
    effects.publish_existing_origin(&plan)?;
    verify_owned_origin(effects, &plan)?;
    guard.revalidate()?;
    if existing.phase == TransactionPhase::Committed {
        return Ok(receipt_from(&existing, &fingerprint, &approval));
    }
    let committed = TransactionRecord {
        phase: TransactionPhase::Committed,
        applied_unix: Some(now_unix),
        ..existing
    };
    write_record(guard, &committed)?;
    Ok(receipt_from(&committed, &fingerprint, &approval))
}

fn validate_original_attempt(
    record: &TransactionRecord,
    plan: &SetupPlan,
    approval: &ApprovalBinding,
) -> Result<(), CustomerError> {
    let fingerprint = plan.fingerprint();
    if approval.plan_fingerprint != fingerprint
        || record.plan_fingerprint != fingerprint
        || record.approval_id != approval.approval_id
        || record.release_version != plan.release_version
    {
        return Err(identity_changed(
            "reconciliation requires the original plan, approval and release; existing evidence is retained",
        ));
    }
    if record.issued_unix == 0
        || record
            .applied_unix
            .is_some_and(|time| time < record.issued_unix)
    {
        return Err(recovery_refusal(
            "setup journal timestamps do not establish a valid original attempt",
        ));
    }
    // Expired authorization is not renewed. Its original issuance must still
    // be inside that grant, and reconciliation can issue no allocation effect.
    if !binding_matches(approval, plan, record.issued_unix) {
        return Err(authorization_required(
            "original approval did not cover this journal's issuance time",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
