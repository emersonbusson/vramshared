// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_selector.rs
//! Selector transaction evidence and exclusion.

pub(in crate::customer) mod derived_config;
mod descriptor_budget;
mod effect;
mod intent;
mod previous_inventory;
mod previous_members;
mod reconciliation;
mod sealing;
mod sink_binding;
mod transformation;
mod tree_custody;
#[cfg(test)]
pub(super) use effect::{SelectorEffectHooks, publish_sealed_selector_with_hooks};
pub(super) use effect::{SelectorEffectPhase, effect_journal_present, publish_sealed_selector};
pub(super) use intent::record_prepared_intent;
#[cfg(test)]
pub(super) use intent::record_prepared_intent_with_hook;
#[cfg(test)]
pub(super) use reconciliation::SelectorRecoveryOutcome;
pub(super) use reconciliation::{
    SelectorRecoveryAction, inspect_effect_journal, reconcile_selector,
    remove_owned_unselected_release,
};
pub(super) use sealing::finalize_and_seal_release;
#[cfg(test)]
pub(super) use sealing::finalize_and_seal_release_with_hook;

use std::ffi::OsStr;

use super::CustomerError;
use super::transition::{
    CustomerTransitionGuard, OperationOwner, PackagePending, TransitionPaths, read_journal_body,
    read_pending, recovery_refusal,
};

pub(in crate::customer) use sink_binding::SinkWitness;

pub(in crate::customer) struct PreviousEvidence<'a> {
    pub signature: &'a super::package_trust::VerifiedSignature,
    pub transformation: Option<PreviousTransformation<'a>>,
}

pub(in crate::customer) struct PreviousTransformation<'a> {
    pub role: &'a str,
    pub template: &'a [u8],
    pub sink: &'a SinkWitness,
}

const JOURNAL: &str = "package-selector.json";

/// Existing, malformed, or unreadable evidence requires reconciliation.
pub(super) fn journal_present(paths: &TransitionPaths) -> Result<bool, CustomerError> {
    let intent_present = read_journal_body(paths, OsStr::new(JOURNAL), "package selector journal")
        .map(|body| body.is_some())
        .map_err(|_| recovery_refusal("package selector journal unavailable; retain evidence"))?;
    Ok(intent_present || effect_journal_present(paths)?)
}

pub(super) fn require_absent(paths: &TransitionPaths) -> Result<(), CustomerError> {
    if journal_present(paths)? {
        return Err(recovery_refusal("package selector requires reconciliation"));
    }
    Ok(())
}

/// Remove selector intent and effect journals only when any present effect record is terminal
/// (`Verified | RolledBack`) and both records belong to `pending`.
pub(super) fn clear_selector_journals(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
) -> Result<(), CustomerError> {
    if guard.owner() != OperationOwner::Package {
        return Err(recovery_refusal(
            "clearing selector journals requires package transition owner",
        ));
    }
    guard.revalidate()?;
    if read_pending(guard.paths())?.as_ref() != Some(pending) {
        return Err(recovery_refusal(
            "pending record does not match selector journal clear target",
        ));
    }
    if let Some(effect) = inspect_effect_journal(guard.paths())?
        && (&effect.pending != pending
            || !matches!(
                effect.phase,
                SelectorEffectPhase::Verified | SelectorEffectPhase::RolledBack
            ))
    {
        return Err(recovery_refusal(
            "cannot clear non-terminal selector effect journal; reconcile first",
        ));
    }
    intent::verify_intent_pending_for_clear(guard.paths(), pending)?;
    super::transition_custody::clear_journal_file(
        guard,
        OsStr::new(effect::EFFECT_JOURNAL),
        "selector effect journal",
    )?;
    super::transition_custody::clear_journal_file(
        guard,
        OsStr::new(JOURNAL),
        "package selector journal",
    )
}
