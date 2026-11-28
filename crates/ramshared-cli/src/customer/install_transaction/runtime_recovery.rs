// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/install_transaction/runtime_recovery.rs

//! Replay the runtime effects of an already committed, authenticated upgrade.

use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use super::super::CustomerError;
use super::super::install_apply::{CommittedInstall, verify_committed_request};
use super::super::install_args::InstallRequest;
use super::super::install_drain::{CommitWitness, DrainInputs, InstallPhase, InstallerLock};
use super::super::install_fs::{digest, read_bounded_regular_at};
use super::super::install_lifecycle::{
    ActivationOutcome, LifecycleHost, UpgradeQuiescenceState, finish_upgrade_lifecycle,
    observe_committed_activation,
};
use super::super::install_tokens::EnrollmentOutcome;
use super::super::install_units::update::{UPDATE_SOCKET_UNIT, UPDATE_UNITS, arm_if_enrolled};
use super::super::install_units::{MANAGED_UNITS, MAX_UNIT_FILE_BYTES, UnitHost};
use super::super::package_trust::VerifiedSignature;
use super::super::release_ordering::read_enrolled_posture;
use super::super::transition::{OperationOwner, acquire_transition, recovery_refusal};
use super::super::transition_custody::open_directory_no_follow;
use super::InstallEnvironment;
use crate::cascade::boot::parse_wsl2_acceptance;
use crate::cascade::boot_policy_effect::BootPolicyHost;
use crate::cascade::posture::Posture;
use crate::installed_image::verify_committed_release;
use crate::release_identity::read_release_version;

#[allow(clippy::too_many_arguments)]
pub(super) fn resume_committed_install(
    req: &InstallRequest,
    env: &InstallEnvironment,
    lock: Option<&InstallerLock>,
    signature: Option<&VerifiedSignature>,
    unit_host: &mut dyn UnitHost,
    boot_host: &dyn BootPolicyHost,
    lifecycle: &mut dyn LifecycleHost,
    stdout: &mut dyn Write,
) -> Result<Option<(CommittedInstall, ActivationOutcome)>, CustomerError> {
    let Some(lock) = lock else { return Ok(None) };
    let Some((inputs, witness, prior)) = lock.committed_runtime()? else {
        return Ok(None);
    };
    lock.start_progress(Some(&inputs.candidate_release));
    let posture = verify_committed_request(req, env, &inputs, signature)?;
    let selected = verify_selected_runtime(env, &inputs, &witness, posture)?;
    if witness.enrolled
        && !lifecycle
            .observe_live_state(unit_host, boot_host)?
            .boot_enrolled
    {
        return Err(recovery_refusal(
            "boot enrollment changed after package commit",
        ));
    }
    let quiescence = UpgradeQuiescenceState {
        was_active: prior.cascade_active || prior.active_managed_swap,
        was_supervisor_active: prior.supervisor_active,
        was_host_gate_active: prior.host_gate_active,
        was_boot_enrolled: prior.boot_enrolled,
        was_health_active: prior.health_active,
        was_health_enabled: prior.health_enabled,
    };
    let committed = CommittedInstall {
        release: inputs.candidate_release.clone(),
        selector: selected,
        posture,
        enroll_outcome: witness
            .enrolled
            .then(|| EnrollmentOutcome::Enrolled { warnings: vec![] }),
    };
    lock.progress_phase(InstallPhase::Finalize);
    let (committed, activation) = finish_upgrade_lifecycle(
        &quiescence,
        lifecycle,
        Ok(committed),
        |_| witness.enrolled,
        inputs.no_activate,
        stdout,
    )?;
    if matches!(activation, ActivationOutcome::Refused { .. }) {
        return Ok(Some((committed, activation)));
    }
    let activation = observe_committed_activation(unit_host, inputs.no_activate)?;
    verify_selected_runtime(env, &inputs, &witness, posture)?;
    lock.clear_finished(Some(&inputs), true)?;
    if env.require_root
        && let Err(err) = arm_if_enrolled(
            &env.releases_dir().join(&inputs.candidate_release),
            &env.default_policy,
            env.owner_uid,
            unit_host,
        )
    {
        let _ = writeln!(
            stdout,
            "NBD_INSTALL_WARNING=update-schedule: {}",
            err.detail
        );
    }
    Ok(Some((committed, activation)))
}

fn verify_selected_runtime(
    env: &InstallEnvironment,
    inputs: &DrainInputs,
    witness: &CommitWitness,
    posture: Posture,
) -> Result<PathBuf, CustomerError> {
    // Fail before touching units when package publication still needs reconciliation.
    // Drop the customer lock before systemd starts the boot owner.
    let guard = acquire_transition(OperationOwner::Up, &env.transition)?;
    let selector = verify_committed_selection(env, inputs, witness)?;
    verify_committed_approvals(env, inputs, witness.enrolled, posture)?;
    guard.revalidate()?;
    Ok(selector)
}

pub(super) fn verify_committed_selection(
    env: &InstallEnvironment,
    inputs: &DrainInputs,
    witness: &CommitWitness,
) -> Result<PathBuf, CustomerError> {
    let selector = env.selector_path();
    let release = env.releases_dir().join(&inputs.candidate_release);
    if !matches_directory(&selector, &release, witness.directory)
        || read_release_version(&selector).as_deref() != Some(&inputs.candidate_release)
    {
        return Err(recovery_refusal("committed release selector changed"));
    }
    let hashes = verify_committed_release(
        &selector,
        &witness.sealed_manifest,
        &inputs.candidate_manifest,
    )
    .ok_or_else(|| recovery_refusal("committed release seal no longer verifies"))?;
    let unit_dir = open_directory_no_follow(&env.systemd_dir, false)?
        .ok_or_else(|| recovery_refusal("committed systemd directory is missing"))?;
    let update = hashes.contains_key(&format!("systemd/{UPDATE_SOCKET_UNIT}"));
    let update_group = UPDATE_UNITS.into_iter().filter(|_| update);
    for unit in MANAGED_UNITS.into_iter().chain(update_group) {
        let expected = hashes.get(&format!("systemd/{unit}"));
        if expected.is_none()
            || read_bounded_regular_at(&unit_dir, OsStr::new(unit), MAX_UNIT_FILE_BYTES)
                .map(|bytes| digest(&bytes))
                .as_ref()
                != expected
        {
            return Err(recovery_refusal(format!("committed unit {unit} changed")));
        }
    }
    if !matches_directory(&selector, &release, witness.directory) {
        return Err(recovery_refusal("committed release selector changed"));
    }
    Ok(selector)
}

pub(super) fn verify_committed_approvals(
    env: &InstallEnvironment,
    inputs: &DrainInputs,
    enrolled: bool,
    posture: Posture,
) -> Result<(), CustomerError> {
    if read_enrolled_posture(&env.approval_dir, &inputs.candidate_release)? != Some(posture) {
        return Err(recovery_refusal(
            "committed upgrade activation approval changed",
        ));
    }
    if enrolled {
        let dir = open_directory_no_follow(&env.approval_dir, false)?
            .ok_or_else(|| recovery_refusal("committed approval directory is missing"))?;
        let name = format!("wsl2-nbd-ack-{}.token", inputs.candidate_release);
        let bytes = read_bounded_regular_at(&dir, OsStr::new(&name), 512)
            .ok_or_else(|| recovery_refusal("committed enrollment acknowledgment is missing"))?;
        let valid = std::str::from_utf8(&bytes)
            .ok()
            .is_some_and(|body| parse_wsl2_acceptance(body, &inputs.candidate_release).is_ok());
        if !valid {
            return Err(recovery_refusal(
                "committed enrollment acknowledgment changed",
            ));
        }
    }
    Ok(())
}

fn matches_directory(selector: &Path, release: &Path, expected: (u64, u64)) -> bool {
    std::fs::canonicalize(selector).is_ok_and(|selected| selected == release)
        && std::fs::symlink_metadata(release)
            .is_ok_and(|metadata| metadata.is_dir() && (metadata.dev(), metadata.ino()) == expected)
}
