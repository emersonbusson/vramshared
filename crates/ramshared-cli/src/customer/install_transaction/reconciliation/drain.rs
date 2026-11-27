// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/install_transaction/reconciliation/drain.rs

//! Reconcile a published package with durable installer drain custody.

use super::super::runtime_recovery::{verify_committed_approvals, verify_committed_selection};
use crate::cascade::boot_policy_effect::BootPolicyHost;
use crate::cascade::posture::Posture;
use crate::customer::CustomerError;
use crate::customer::install_apply::{verify_candidate_trust, verify_committed_request};
use crate::customer::install_args::{InstallMode, InstallRequest};
use crate::customer::install_drain::{
    CommitWitness, DrainInputs, InstallerLock, PackageRecoveryPlan,
};
use crate::customer::install_tokens::{recover_activation_approval, recover_enrollment};
use crate::customer::install_transaction::InstallEnvironment;
use crate::customer::install_units::UnitHost;
use crate::customer::package_selector::{inspect_effect_journal, settle_observed_publication};
use crate::customer::release_ordering::find_previous_bundle;
use crate::customer::transition::{CustomerTransitionGuard, PackagePending, recovery_refusal};
use crate::release_identity::read_release_version;

pub(in crate::customer::install_transaction) fn recovered_request(
    req: &InstallRequest,
    env: &InstallEnvironment,
    inputs: &DrainInputs,
) -> Result<InstallRequest, CustomerError> {
    let posture = match inputs.posture.as_str() {
        "writeback" => Posture::Writeback,
        "lab_swap" => Posture::Swap,
        _ => return Err(recovery_refusal("recorded package posture is invalid")),
    };
    // An omitted flag inherits the receipt. An explicit flag cannot change it.
    if (req.no_activate && !inputs.no_activate)
        || (req.no_enroll && !inputs.no_enroll)
        || (req.enroll_default && !inputs.enroll_default)
        || req
            .lower_sink
            .as_ref()
            .is_some_and(|path| path != &inputs.sink_path)
        || req.posture.is_some_and(|specified| specified != posture)
        || req
            .approved_version
            .as_ref()
            .is_some_and(|version| version != &inputs.candidate_release)
        || req
            .approved_manifest_sha256
            .as_ref()
            .is_some_and(|hash| hash != &inputs.candidate_manifest)
    {
        return Err(recovery_refusal(
            "recovery arguments differ from the drained package",
        ));
    }
    let bundle_dir = if let Some(root) = req.bundle_root.as_ref()
        && read_release_version(root).is_some()
    {
        root.clone()
    } else {
        let root = req
            .bundle_root
            .as_deref()
            .unwrap_or(&env.default_bundle_root);
        let policy = req.policy.as_deref().unwrap_or(&env.default_policy);
        find_previous_bundle(root, policy, env.owner_uid, &inputs.candidate_manifest)?.ok_or_else(
            || recovery_refusal("original signed package is unavailable for recovery"),
        )?
    };
    Ok(InstallRequest {
        mode: InstallMode::Apply,
        approved_version: Some(inputs.candidate_release.clone()),
        approved_manifest_sha256: Some(inputs.candidate_manifest.clone()),
        lower_sink: Some(inputs.sink_path.clone()),
        posture: Some(posture),
        enroll_default: inputs.enroll_default,
        no_enroll: inputs.no_enroll,
        no_activate: inputs.no_activate,
        bundle_root: Some(bundle_dir),
        ..req.clone()
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn settle_package(
    req: &InstallRequest,
    env: &InstallEnvironment,
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    lock: &InstallerLock,
    plan: PackageRecoveryPlan,
    unit_host: &mut dyn UnitHost,
    boot_host: &impl BootPolicyHost,
) -> Result<(), CustomerError> {
    let req = recovered_request(req, env, &plan.inputs)?;
    let bundle = req
        .bundle_root
        .as_deref()
        .ok_or_else(|| recovery_refusal("recovered package has no signed input"))?;
    let signature = verify_candidate_trust(bundle, &req, env)?;
    let posture = verify_committed_request(&req, env, &plan.inputs, Some(&signature))?;
    if let Some(witness) = plan.committed {
        verify_committed_selection(env, &plan.inputs, &witness)?;
        return verify_committed_approvals(env, &plan.inputs, witness.enrolled, posture);
    }

    let observed = inspect_effect_journal(&env.transition)?
        .ok_or_else(|| recovery_refusal("interrupted package has no published selector effect"))?;
    let witness = CommitWitness {
        directory: observed.target_identity,
        sealed_manifest: observed.sealed_manifest_sha256.clone(),
        enrolled: plan.enroll,
    };
    verify_committed_selection(env, &plan.inputs, &witness)?;
    let effect =
        settle_observed_publication(guard, pending, &env.selector_path(), &signature, observed)?;
    unit_host.daemon_reload()?;
    unit_host.sync_selected_release()?;
    recover_activation_approval(&env.approval_dir, &plan.inputs.candidate_release, posture)?;
    if plan.enroll {
        recover_enrollment(boot_host, &env.approval_dir, &plan.inputs.candidate_release)?;
    }
    verify_committed_approvals(env, &plan.inputs, plan.enroll, posture)?;
    lock.record_committed(guard, &effect, plan.enroll)
}
