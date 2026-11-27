// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/install_transaction/reconciliation.rs

//! Attended reconciliation of the package selector and its durable effects.

use std::io::Write;
use std::os::unix::fs::MetadataExt;

use super::super::install_apply::{clear_all_package_journals, verify_candidate_trust};
use super::super::install_args::InstallRequest;
use super::super::install_drain::InstallerLock;
use super::super::install_fs::fs_error;
use super::super::install_units::UnitHost;
use super::super::package_prepare::{PreparePaths, resume as resume_prepared};
use super::super::package_selector::{
    SelectorEffectPhase, SelectorRecoveryAction, inspect_effect_journal, reconcile_selector,
    remove_owned_unselected_release,
};
use super::super::package_source::DeclaredBundle;
use super::super::package_stage_journal::cleanup_staged_members;
use super::super::package_tree::rollback::observe_owned_root;
use super::super::transition::{
    CustomerTransitionGuard, OperationOwner, PackagePending, acquire_transition, read_pending,
};
use super::super::{CustomerError, CustomerReason, CustomerState};
use super::{INSTALL_RESERVE_BYTES, InstallEnvironment, resolve_bundle_dir};
use crate::cascade::boot_policy_effect::BootPolicyHost;
use crate::installed_image::observe_installed_input_bundle_digest;

mod drain;
pub(super) use drain::recovered_request;

pub(super) fn finish_pending_drain(
    req: &InstallRequest,
    env: &InstallEnvironment,
    lock: Option<&InstallerLock>,
    unit_host: &mut dyn UnitHost,
    boot_host: &impl BootPolicyHost,
) -> Result<(), CustomerError> {
    if let Some(lock) = lock
        && lock.active_inputs()?.is_some()
        && read_pending(&env.transition)?.is_some()
    {
        run_reconcile(
            req,
            env,
            SelectorRecoveryAction::Complete,
            Some(lock),
            unit_host,
            boot_host,
            &mut std::io::sink(),
        )?;
    }
    Ok(())
}

pub(super) fn prepare_timer_runtime_request(
    req: &mut InstallRequest,
    env: &InstallEnvironment,
    lock: Option<&InstallerLock>,
    unit_host: &mut dyn UnitHost,
    boot_host: &impl BootPolicyHost,
) -> Result<(), CustomerError> {
    finish_pending_drain(req, env, lock, unit_host, boot_host)?;
    if let Some(lock) = lock
        && let Some((inputs, _, _)) = lock.committed_runtime()?
    {
        *req = recovered_request(req, env, &inputs)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn reconcile_complete(
    req: &InstallRequest,
    env: &InstallEnvironment,
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    lock: Option<&InstallerLock>,
    unit_host: &mut dyn UnitHost,
    boot_host: &impl BootPolicyHost,
    stdout: &mut dyn Write,
) -> Result<(), CustomerError> {
    if let Some(lock) = lock
        && let Some(plan) = lock.package_recovery_plan(pending)?
    {
        drain::settle_package(req, env, guard, pending, lock, plan, unit_host, boot_host)?;
        cleanup_staged_members(guard, pending, &env.stage_dir())?;
        clear_all_package_journals(guard, pending)?;
        let _ = writeln!(stdout, "NBD_INSTALL_STATE=RECONCILED_COMPLETE");
        return Ok(());
    }
    let bundle_dir = resolve_bundle_dir(req.bundle_root.as_deref(), &env.default_bundle_root)?;
    let signature = verify_candidate_trust(&bundle_dir, req, env)?;
    let declared = DeclaredBundle::open_verified(&bundle_dir, &signature)
        .filter(|declared| declared.manifest_sha256() == pending.new_hash)
        .ok_or_else(|| {
            fs_error(
                CustomerReason::IdentityChanged,
                "NBD_INSTALL_REASON=RECONCILE_BUNDLE_INVALID",
                CustomerState::RecoveryRequired,
            )
        })?;
    if let Some(effect) = inspect_effect_journal(&env.transition)? {
        if effect.pending != *pending || effect.phase != SelectorEffectPhase::Verified {
            return Err(fs_error(
                CustomerReason::RecoveryRequired,
                "NBD_INSTALL_REASON=RECONCILE_COMPLETE_UNSUPPORTED_WITHOUT_SEALED_SELECTOR",
                CustomerState::RecoveryRequired,
            ));
        }
    } else {
        let target_ver = pending
            .target_release
            .as_deref()
            .filter(|version| super::super::transition::is_single_component_release_name(version))
            .ok_or_else(|| {
                fs_error(
                    CustomerReason::RecoveryRequired,
                    "NBD_INSTALL_REASON=RECONCILE_TARGET_MISSING",
                    CustomerState::RecoveryRequired,
                )
            })?;
        let releases_meta = std::fs::metadata(env.releases_dir()).map_err(|_| {
            fs_error(
                CustomerReason::RecoveryRequired,
                "NBD_INSTALL_REASON=PRODUCT_ROOT_INVALID",
                CustomerState::RecoveryRequired,
            )
        })?;
        let stage_dir = env.stage_dir();
        let _ = resume_prepared(
            guard,
            pending,
            &declared,
            &signature,
            &PreparePaths {
                stage: &stage_dir,
                parent: &env.releases_dir(),
                parent_identity: (releases_meta.dev(), releases_meta.ino()),
                name: target_ver,
                reserve_bytes: INSTALL_RESERVE_BYTES,
                progress: None,
            },
        )?;
    }
    // Recovering package preparation does not publish the selector.
    // Bind completion to the verified input of the release actually selected.
    if observe_installed_input_bundle_digest(&env.selector_path()).as_deref()
        != Some(pending.new_hash.as_str())
    {
        return Err(fs_error(
            CustomerReason::RecoveryRequired,
            "NBD_INSTALL_REASON=RECONCILE_COMPLETE_UNSUPPORTED_WITHOUT_SEALED_SELECTOR",
            CustomerState::RecoveryRequired,
        ));
    }
    unit_host.daemon_reload()?;
    unit_host.sync_selected_release()?;
    cleanup_staged_members(guard, pending, &env.stage_dir())?;
    clear_all_package_journals(guard, pending)?;
    let _ = writeln!(stdout, "NBD_INSTALL_STATE=RECONCILED_COMPLETE");
    Ok(())
}

fn reconcile_rollback(
    env: &InstallEnvironment,
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    stdout: &mut dyn Write,
) -> Result<(), CustomerError> {
    if pending
        .target_release
        .as_deref()
        .is_some_and(|version| !super::super::transition::is_single_component_release_name(version))
    {
        return Err(super::super::transition::recovery_refusal(
            "pending target release is not a single path component",
        ));
    }
    let tree = observe_owned_root(guard, pending, &env.releases_dir())?;
    if inspect_effect_journal(&env.transition)?.is_some() {
        let _ = reconcile_selector(guard, pending, SelectorRecoveryAction::Rollback, None, None)?;
    }
    if let Some(tree) = tree {
        remove_owned_unselected_release(tree.path(), tree.identity(), &env.selector_path())?;
    }
    cleanup_staged_members(guard, pending, &env.stage_dir())?;
    clear_all_package_journals(guard, pending)?;
    let _ = writeln!(stdout, "NBD_INSTALL_STATE=RECONCILED_ROLLBACK");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run_reconcile(
    req: &InstallRequest,
    env: &InstallEnvironment,
    action: SelectorRecoveryAction,
    lock: Option<&InstallerLock>,
    unit_host: &mut dyn UnitHost,
    boot_host: &impl BootPolicyHost,
    stdout: &mut dyn Write,
) -> Result<(), CustomerError> {
    let guard = acquire_transition(OperationOwner::Package, &env.transition)?;
    let Some(pending) = read_pending(&env.transition)? else {
        if action == SelectorRecoveryAction::Complete
            && let Some(lock) = lock
            && lock.committed_runtime()?.is_some()
        {
            return Ok(());
        }
        return Err(fs_error(
            CustomerReason::UnsupportedSurface,
            "NBD_INSTALL_REASON=NO_PENDING_TRANSACTION",
            CustomerState::InstalledOff,
        ));
    };
    match action {
        SelectorRecoveryAction::Complete => reconcile_complete(
            req, env, &guard, &pending, lock, unit_host, boot_host, stdout,
        ),
        SelectorRecoveryAction::Rollback => reconcile_rollback(env, &guard, &pending, stdout),
    }
}
