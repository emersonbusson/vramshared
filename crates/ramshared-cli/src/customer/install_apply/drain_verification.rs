// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/install_apply/drain_verification.rs

//! Signed input and quiescence checks around a privileged drain.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use super::super::install_args::InstallRequest;
use super::super::install_drain::DrainInputs;
use super::super::install_fs::fs_error;
use super::super::install_transaction::{InstallEnvironment, resolve_bundle_dir};
use super::super::install_units::{UnitHost, check_slice_before_drain};
use super::super::package_selector::SinkWitness;
use super::super::package_trust::VerifiedSignature;
use super::super::transition::recovery_refusal;
use super::super::{CustomerError, CustomerReason, CustomerState};
use super::{
    PriorInstallState, VerifiedCandidate, inspect_prior_install_state, install_err,
    open_verified_candidate, recovery_err, validate_apply_inputs, verify_candidate_trust,
};
use crate::cascade::CascadeError;
use crate::installed_image::observe_installed_input_bundle_digest;

pub(in crate::customer) fn verify_install(
    req: &InstallRequest,
    env: &InstallEnvironment,
    unit_host: &dyn UnitHost,
    preverified_sig: Option<VerifiedSignature>,
) -> Result<(VerifiedCandidate, Option<DrainInputs>), CustomerError> {
    let inputs = validate_apply_inputs(req)?;
    let candidate = open_verified_candidate(req, env, &inputs, preverified_sig)?;
    let prior = inspect_prior_install_state(
        req,
        env,
        &candidate.bundle_dir,
        &env.selector_path(),
        inputs.approved_ver,
    )?;
    if env.require_root && prior.prior_identity.is_some() {
        check_slice_before_drain(
            unit_host,
            &candidate.bundle_dir,
            prior.prior_root.as_deref(),
            &env.product_root,
            &env.systemd_dir,
            &req.legacy_unit_approvals,
        )?;
    }
    let drain = prior.prior_identity.map(|previous_directory| DrainInputs {
        previous_release: prior.old_release,
        previous_manifest: prior.old_hash,
        previous_directory,
        candidate_release: inputs.approved_ver.to_owned(),
        candidate_manifest: candidate.signature.manifest_sha256().to_owned(),
        sink_path: candidate.sink.binding().path.to_owned(),
        sink_identity: candidate.sink.binding().identity.to_owned(),
        posture: inputs.posture.as_status().to_owned(),
        no_activate: req.no_activate,
        no_enroll: req.no_enroll,
        enroll_default: req.enroll_default,
    });
    Ok((candidate, drain))
}

pub(in crate::customer) fn verify_committed_request(
    req: &InstallRequest,
    env: &InstallEnvironment,
    expected: &DrainInputs,
    preverified_sig: Option<&VerifiedSignature>,
) -> Result<crate::cascade::posture::Posture, CustomerError> {
    let input = validate_apply_inputs(req)?;
    if input.approved_ver != expected.candidate_release
        || input.approved_sha != expected.candidate_manifest
        || input.posture.as_status() != expected.posture
        || req.no_activate != expected.no_activate
        || req.no_enroll != expected.no_enroll
        || req.enroll_default != expected.enroll_default
    {
        return Err(recovery_refusal(
            "committed upgrade belongs to a different request",
        ));
    }
    let own_signature;
    let signature = match preverified_sig {
        Some(signature) => signature,
        None => {
            let bundle = resolve_bundle_dir(req.bundle_root.as_deref(), &env.default_bundle_root)?;
            own_signature = verify_candidate_trust(&bundle, req, env)?;
            &own_signature
        }
    };
    if signature.manifest_sha256() != expected.candidate_manifest
        || signature.revalidate().is_none()
    {
        return Err(recovery_refusal(
            "committed upgrade signature no longer matches",
        ));
    }
    let sink = SinkWitness::observe(input.lower_sink_raw)
        .ok_or_else(|| recovery_refusal("cannot verify committed upgrade sink"))?;
    if sink.binding().identity != expected.sink_identity {
        return Err(recovery_refusal("committed upgrade sink identity changed"));
    }
    Ok(input.posture)
}

pub(super) fn verify_quiesced_install(
    env: &InstallEnvironment,
    prior: &PriorInstallState,
    check_units: impl FnOnce() -> Result<(), CustomerError>,
) -> Result<(), CustomerError> {
    verify_observed_quiescence(
        &env.selector_path(),
        prior,
        check_units,
        crate::cascade::evaluate_live_down_plan,
    )
}

fn verify_observed_quiescence(
    selector: &Path,
    prior: &PriorInstallState,
    check_units: impl FnOnce() -> Result<(), CustomerError>,
    observe_lifecycle: impl FnOnce() -> Result<Option<(bool, String)>, CascadeError>,
) -> Result<(), CustomerError> {
    check_units()?;
    let location = resolve_prior_identity(selector)?;
    if location.as_ref().map(|loc| loc.identity) != prior.prior_identity
        || (location.is_some()
            && observe_installed_input_bundle_digest(selector).as_deref()
                != Some(prior.old_hash.as_str()))
    {
        return Err(install_err(
            CustomerReason::IdentityChanged,
            "NBD_INSTALL_REASON=PRIOR_SELECTOR_CHANGED_DURING_DRAIN",
        ));
    }
    match observe_lifecycle() {
        Ok(None) => {}
        Ok(Some(_)) => {
            return Err(fs_error(
                CustomerReason::ActiveSession,
                "NBD_INSTALL_REASON=MANAGED_LIFECYCLE_STILL_PRESENT",
                CustomerState::Active,
            ));
        }
        Err(error) => {
            return Err(fs_error(
                CustomerReason::ObservationUnavailable,
                format!("NBD_INSTALL_REASON=MANAGED_LIFECYCLE_UNPROVED ({error})"),
                CustomerState::UnknownBlocked,
            ));
        }
    }
    Ok(())
}

pub(super) struct PriorLocation {
    pub(super) root: PathBuf,
    pub(super) identity: (u64, u64),
}

pub(super) fn resolve_prior_identity(
    selector: &Path,
) -> Result<Option<PriorLocation>, CustomerError> {
    match std::fs::symlink_metadata(selector) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(recovery_err(format!(
                "NBD_INSTALL_REASON=SELECTOR_UNAVAILABLE ({error})"
            )));
        }
    }
    let root = std::fs::canonicalize(selector)
        .map_err(|_| recovery_err("NBD_INSTALL_REASON=SELECTOR_TARGET_INVALID"))?;
    let meta = std::fs::symlink_metadata(&root)
        .ok()
        .filter(std::fs::Metadata::is_dir)
        .ok_or_else(|| recovery_err("NBD_INSTALL_REASON=PRIOR_RELEASE_INVALID"))?;
    Ok(Some(PriorLocation {
        root,
        identity: (meta.dev(), meta.ino()),
    }))
}

#[cfg(test)]
mod tests;
