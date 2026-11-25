// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/install_transaction.rs
//! Complete native sealed installation, auto-update, rollback, and recovery coordinator.

use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use rustix::process::geteuid;

use super::install_apply::{
    clear_all_package_journals, clear_stage_artifacts, execute_verified_apply,
    verify_candidate_trust, verify_declared_payload_identity,
};
use super::install_args::{
    AutoUpdateMode, InstallMode, InstallRequest, parse_install_args, write_auto_update_status,
    write_plan_output,
};
use super::install_fs::{WslInstallHostMemoryCache, fs_error};
#[cfg(test)]
use super::install_lifecycle::NoopLifecycleHost;
use super::install_lifecycle::{
    ActivationOutcome, LifecycleHost, PrepareUpgradeOutcome, SystemLifecycleHost,
    activate_installed_units, append_auto_update_log, emit_committed_install,
    finalize_auto_update_outcome, finish_seamless_auto_update, prepare_seamless_auto_update,
    should_activate_after_install, write_deferred_output, write_up_to_date_output,
};
use super::install_units::{MANAGED_UNITS, SystemUnitHost, UnitHost, validate_unit_plan};
use super::package_prepare::{PreparePaths, resume as resume_prepared};
use super::package_selector::{
    SelectorEffectPhase, SelectorRecoveryAction, SinkWitness, inspect_effect_journal,
    reconcile_selector, remove_owned_unselected_release,
};
use super::package_source::DeclaredBundle;
use super::package_source::stream::MemoryObservation;
use super::package_trust::{VerifiedSignature, verify_bundle_from_policy};
use super::release_ordering::{
    AutoUpdatePolicy, ReleaseOrder, find_newest_bundle, read_auto_update_policy,
    read_enrolled_posture, release_is_newer,
};
use super::transition::{
    CustomerTransitionGuard, OperationOwner, PackagePending, TransitionPaths, acquire_transition,
    read_pending,
};
use super::{CustomerError, CustomerReason, CustomerState};
use crate::cascade::boot_policy_effect::BootPolicyHost;
use crate::cascade::boot_policy_host::SystemBootPolicyHost;
use crate::cascade::posture::Posture;
use crate::installed_image::{observe_installed_input_bundle_digest, verify_release_manifest};
use crate::release_identity::read_release_version;

pub(super) const INSTALL_RESERVE_BYTES: u64 = 16 * 1024 * 1024;

pub(super) struct InstallEnvironment {
    pub(super) product_root: PathBuf,
    pub(super) systemd_dir: PathBuf,
    pub(super) approval_dir: PathBuf,
    pub(super) default_bundle_root: PathBuf,
    pub(super) default_policy: PathBuf,
    pub(super) auto_update_log: PathBuf,
    pub(super) transition: TransitionPaths,
    pub(super) owner_uid: u32,
    pub(super) require_root: bool,
    pub(super) utc_override: Option<String>,
}

impl InstallEnvironment {
    pub(super) fn production() -> Self {
        Self {
            product_root: PathBuf::from(crate::installed_image::PRODUCT_ROOT),
            systemd_dir: PathBuf::from("/etc/systemd/system"),
            approval_dir: PathBuf::from("/var/lib/ramshared/approvals"),
            default_bundle_root: PathBuf::from("artifacts/packages"),
            default_policy: PathBuf::from("/etc/ramshared/auto-update.conf"),
            auto_update_log: PathBuf::from("/var/log/ramshared/auto-update.log"),
            transition: TransitionPaths::production(),
            owner_uid: 0,
            require_root: true,
            utc_override: None,
        }
    }

    pub(super) fn selector_path(&self) -> PathBuf {
        self.product_root.join("current")
    }

    pub(super) fn releases_dir(&self) -> PathBuf {
        self.product_root.join("releases")
    }

    pub(super) fn stage_dir(&self) -> PathBuf {
        self.product_root.join("staging")
    }
}

struct AutoUpdateContext<'a> {
    policy_path: &'a Path,
    policy_opt: Option<AutoUpdatePolicy>,
    running: Option<String>,
    enrolled: Option<Posture>,
    newest: Option<PathBuf>,
}

struct ValidatedAutoUpdate<'a> {
    policy_path: &'a Path,
    bundle_dir: PathBuf,
    signature: VerifiedSignature,
    manifest_sha256: String,
    lower_sink: String,
    candidate: String,
}

/// Top-level `ramshared install` dispatch.
pub(crate) fn run(args: &[String], stdout: &mut dyn Write, stderr: &mut dyn Write) -> u8 {
    let env = InstallEnvironment::production();
    let mut unit_host = SystemUnitHost;
    let boot_host = SystemBootPolicyHost;
    let mut lifecycle = SystemLifecycleHost::production();
    let mut host_mem_cache = WslInstallHostMemoryCache::new();
    let mut live_mem = || host_mem_cache.observe_install_memory();
    run_with_lifecycle(
        args,
        &env,
        &mut unit_host,
        &boot_host,
        &mut lifecycle,
        &mut live_mem,
        stdout,
        stderr,
    )
}

#[cfg(test)]
pub(super) fn run_with(
    args: &[String],
    env: &InstallEnvironment,
    unit_host: &mut dyn UnitHost,
    boot_host: &impl BootPolicyHost,
    memory_obs: &mut dyn FnMut() -> Option<MemoryObservation>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> u8 {
    let mut lifecycle = NoopLifecycleHost;
    run_with_lifecycle(
        args,
        env,
        unit_host,
        boot_host,
        &mut lifecycle,
        memory_obs,
        stdout,
        stderr,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run_with_lifecycle(
    args: &[String],
    env: &InstallEnvironment,
    unit_host: &mut dyn UnitHost,
    boot_host: &impl BootPolicyHost,
    lifecycle: &mut dyn LifecycleHost,
    memory_obs: &mut dyn FnMut() -> Option<MemoryObservation>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> u8 {
    let req = match parse_install_args(args) {
        Ok(req) => req,
        Err(err) => {
            let _ = writeln!(stderr, "{err}");
            return err.exit_code();
        }
    };
    match execute_request(
        &req, env, unit_host, boot_host, lifecycle, memory_obs, stdout,
    ) {
        Ok(()) => 0,
        Err(err) => {
            if matches!(
                req.mode,
                InstallMode::Plan | InstallMode::Apply | InstallMode::Reconcile(_)
            ) && !err.detail.contains("NBD_INSTALL_STATE=")
            {
                let _ = writeln!(stderr, "NBD_INSTALL_STATE=REFUSED");
            }
            let _ = writeln!(stderr, "{err}");
            err.exit_code()
        }
    }
}

fn execute_request(
    req: &InstallRequest,
    env: &InstallEnvironment,
    unit_host: &mut dyn UnitHost,
    boot_host: &impl BootPolicyHost,
    lifecycle: &mut dyn LifecycleHost,
    memory_obs: &mut dyn FnMut() -> Option<MemoryObservation>,
    stdout: &mut dyn Write,
) -> Result<(), CustomerError> {
    match req.mode {
        InstallMode::Plan => run_plan(req, env, stdout),
        InstallMode::AutoUpdate(mode) => run_auto_update(
            mode, req, env, unit_host, boot_host, lifecycle, memory_obs, stdout,
        ),
        InstallMode::Reconcile(action) => {
            require_root(env)?;
            run_reconcile(req, env, action, unit_host, stdout)
        }
        InstallMode::Apply => {
            require_root(env)?;
            let committed =
                execute_verified_apply(req, env, None, unit_host, boot_host, memory_obs, stdout)?;
            let activation =
                if should_activate_after_install(req, committed.enroll_outcome.as_ref()) {
                    lifecycle.ensure_recommended_config(stdout);
                    activate_installed_units(lifecycle, false)
                } else {
                    ActivationOutcome::Off
                };
            emit_committed_install(&committed, &activation, req.json, stdout);
            Ok(())
        }
    }
}

fn require_root(env: &InstallEnvironment) -> Result<(), CustomerError> {
    if env.require_root && geteuid().as_raw() != 0 {
        return Err(fs_error(
            CustomerReason::AuthorizationRequired,
            "NBD_INSTALL_REASON=ROOT_REQUIRED: installation requires root privileges",
            CustomerState::InstalledOff,
        ));
    }
    Ok(())
}

pub(super) fn resolve_bundle_dir(
    requested: Option<&Path>,
    default_root: &Path,
) -> Result<PathBuf, CustomerError> {
    if let Some(path) = requested {
        if path.join("RELEASE_VERSION").exists() {
            return Ok(path.to_path_buf());
        }
        return find_newest_bundle(path)?.ok_or_else(|| {
            fs_error(
                CustomerReason::ObservationUnavailable,
                "NBD_INSTALL_REASON=BUNDLE_NOT_FOUND",
                CustomerState::InstalledOff,
            )
        });
    }
    find_newest_bundle(default_root)?.ok_or_else(|| {
        fs_error(
            CustomerReason::ObservationUnavailable,
            "NBD_INSTALL_REASON=BUNDLE_NOT_FOUND",
            CustomerState::InstalledOff,
        )
    })
}

fn run_plan(
    req: &InstallRequest,
    env: &InstallEnvironment,
    stdout: &mut dyn Write,
) -> Result<(), CustomerError> {
    let bundle_dir = resolve_bundle_dir(req.bundle_root.as_deref(), &env.default_bundle_root)?;
    let release = read_release_version(&bundle_dir).ok_or_else(|| {
        fs_error(
            CustomerReason::UnsupportedSurface,
            "NBD_INSTALL_REASON=RELEASE_VERSION_INVALID",
            CustomerState::InstalledOff,
        )
    })?;
    let selector = env.selector_path();
    let running = read_release_version(&selector);
    let prior_root = std::fs::canonicalize(&selector).ok();
    let release_manifest_bytes =
        std::fs::read(bundle_dir.join("release-manifest.json")).map_err(|_| {
            fs_error(
                CustomerReason::UnsupportedSurface,
                "NBD_INSTALL_REASON=RELEASE_LAYOUT_INVALID: missing release-manifest.json",
                CustomerState::InstalledOff,
            )
        })?;
    verify_declared_payload_identity(&bundle_dir, &release_manifest_bytes)?;
    let sink = match req.lower_sink.as_deref() {
        Some(raw) => Some(SinkWitness::observe(raw).ok_or_else(|| {
            fs_error(
                CustomerReason::UnsupportedSurface,
                format!("NBD_INSTALL_REASON=LOWER_SINK_INVALID ({raw})"),
                CustomerState::InstalledOff,
            )
        })?),
        None => None,
    };
    if !req.legacy_unit_approvals.is_empty() {
        validate_unit_plan(
            &bundle_dir,
            prior_root.as_deref(),
            &env.product_root,
            &env.systemd_dir,
            &req.legacy_unit_approvals,
        )?;
    }
    write_plan_output(
        &release,
        &env.releases_dir().join(&release),
        &selector,
        running.as_deref(),
        sink.as_ref(),
        req.json,
        stdout,
    );
    Ok(())
}

fn load_auto_update_context<'a>(
    req: &'a InstallRequest,
    env: &'a InstallEnvironment,
) -> Result<AutoUpdateContext<'a>, CustomerError> {
    let policy_path = req.policy.as_deref().unwrap_or(&env.default_policy);
    let owner = env.require_root.then_some(0).or(Some(env.owner_uid));
    let policy_opt = read_auto_update_policy(policy_path, owner)?;
    let running = read_release_version(&env.selector_path());
    let enrolled = running
        .as_deref()
        .and_then(|rel| read_enrolled_posture(&env.approval_dir, rel).ok().flatten());
    let bundle_root = req
        .bundle_root
        .as_deref()
        .or_else(|| policy_opt.as_ref().and_then(|p| p.bundle_root.as_deref()))
        .unwrap_or(&env.default_bundle_root);
    let newest = find_newest_bundle(bundle_root).ok().flatten();
    Ok(AutoUpdateContext {
        policy_path,
        policy_opt,
        running,
        enrolled,
        newest,
    })
}

fn require_enabled_policy_sink(
    policy_path: &Path,
    policy_opt: Option<AutoUpdatePolicy>,
    env: &InstallEnvironment,
) -> Result<String, CustomerError> {
    let Some(policy) = policy_opt else {
        append_auto_update_log(
            &env.auto_update_log,
            &format!("REFUSED no-policy-file {}", policy_path.display()),
        );
        return Err(fs_error(
            CustomerReason::AuthorizationRequired,
            format!("auto-update: no policy file at {}", policy_path.display()),
            CustomerState::InstalledOff,
        ));
    };
    if !policy.enabled {
        append_auto_update_log(&env.auto_update_log, "REFUSED disabled enabled=0");
        return Err(fs_error(
            CustomerReason::AuthorizationRequired,
            format!("auto-update: enabled=0 in {}", policy_path.display()),
            CustomerState::InstalledOff,
        ));
    }
    let Some(lower_sink_path) = policy.lower_sink.as_ref() else {
        append_auto_update_log(&env.auto_update_log, "REFUSED bad-lower-sink absent");
        return Err(fs_error(
            CustomerReason::UnsupportedSurface,
            "auto-update: lower_sink must be configured in policy",
            CustomerState::InstalledOff,
        ));
    };
    lower_sink_path
        .to_str()
        .ok_or_else(|| {
            fs_error(
                CustomerReason::UnsupportedSurface,
                "auto-update: lower_sink is not UTF-8",
                CustomerState::InstalledOff,
            )
        })
        .map(str::to_owned)
}

fn validate_auto_update_candidate<'a>(
    ctx: AutoUpdateContext<'a>,
    env: &InstallEnvironment,
) -> Result<ValidatedAutoUpdate<'a>, CustomerError> {
    let lower_sink = require_enabled_policy_sink(ctx.policy_path, ctx.policy_opt, env)?;
    let Some(bundle_dir) = ctx.newest else {
        append_auto_update_log(&env.auto_update_log, "NO-BUNDLE");
        return Err(fs_error(
            CustomerReason::ObservationUnavailable,
            "auto-update: no sealed bundle available",
            CustomerState::InstalledOff,
        ));
    };
    let Some(signature) = verify_bundle_from_policy(&bundle_dir, ctx.policy_path, env.owner_uid)
    else {
        append_auto_update_log(&env.auto_update_log, "REFUSED untrusted-release");
        return Err(fs_error(
            CustomerReason::AuthorizationRequired,
            "auto-update: signature verification failed against release_keyring",
            CustomerState::InstalledOff,
        ));
    };
    let manifest_sha256 = signature.manifest_sha256().to_owned();
    let stream_bundle = DeclaredBundle::open(&bundle_dir, &manifest_sha256).ok_or_else(|| {
        append_auto_update_log(&env.auto_update_log, "REFUSED manifest-invalid");
        fs_error(
            CustomerReason::IdentityChanged,
            "auto-update: declared bundle validation failed",
            CustomerState::InstalledOff,
        )
    })?;
    let candidate = stream_bundle
        .member("RELEASE_VERSION", 4096)
        .and_then(|bytes| crate::release_identity::parse_identity_line(&bytes))
        .ok_or_else(|| {
            append_auto_update_log(&env.auto_update_log, "REFUSED invalid-bundle-identity");
            fs_error(
                CustomerReason::UnsupportedSurface,
                "auto-update: bundle release identity is invalid",
                CustomerState::InstalledOff,
            )
        })?;
    Ok(ValidatedAutoUpdate {
        policy_path: ctx.policy_path,
        bundle_dir,
        signature,
        manifest_sha256,
        lower_sink,
        candidate,
    })
}

fn emit_auto_update_check(
    env: &InstallEnvironment,
    running: Option<&str>,
    candidate: &str,
    json: bool,
    stdout: &mut dyn Write,
) {
    let order = running.map_or(ReleaseOrder::Newer, |cur| release_is_newer(candidate, cur));
    append_auto_update_log(
        &env.auto_update_log,
        &format!(
            "CHECK trust=verified order={} running={} candidate={candidate}",
            order.as_str(),
            running.unwrap_or("none")
        ),
    );
    if json {
        let doc = serde_json::json!({
            "running": running,
            "candidate": candidate,
            "trust": "verified",
            "order": order.as_str(),
            "update_available": order == ReleaseOrder::Newer,
        });
        let _ = writeln!(stdout, "{doc}");
        return;
    }
    let _ = writeln!(stdout, "running: {}", running.unwrap_or("none"));
    let _ = writeln!(stdout, "candidate: {candidate}");
    let _ = writeln!(stdout, "trust: verified ({})", order.as_str());
}

enum AutoUpdateDecision {
    UpToDate(String),
    Upgrade(String, Posture),
}

fn authorize_auto_update_upgrade(
    env: &InstallEnvironment,
    running: Option<&str>,
    enrolled: Option<Posture>,
    candidate: &str,
) -> Result<AutoUpdateDecision, CustomerError> {
    let current = running.ok_or_else(|| {
        append_auto_update_log(&env.auto_update_log, "REFUSED no-installed-release");
        fs_error(
            CustomerReason::IdentityChanged,
            "auto-update: no installed release; run attended installer first",
            CustomerState::InstalledOff,
        )
    })?;
    let posture = enrolled.ok_or_else(|| {
        append_auto_update_log(
            &env.auto_update_log,
            &format!("REFUSED POSTURE_NOT_ENROLLED running={current}"),
        );
        fs_error(
            CustomerReason::AuthorizationRequired,
            format!("auto-update: running release {current} has no enrolled posture token"),
            CustomerState::InstalledOff,
        )
    })?;
    let order = release_is_newer(candidate, current);
    if order == ReleaseOrder::Equal {
        append_auto_update_log(
            &env.auto_update_log,
            &format!("UP-TO-DATE running={current} candidate={candidate}"),
        );
        return Ok(AutoUpdateDecision::UpToDate(current.to_owned()));
    }
    if order != ReleaseOrder::Newer {
        append_auto_update_log(
            &env.auto_update_log,
            &format!(
                "REFUSED DOWNGRADE order={} running={current} candidate={candidate}",
                order.as_str(),
            ),
        );
        return Err(fs_error(
            CustomerReason::IdentityChanged,
            format!(
                "auto-update: refusing {candidate} (ordering verdict {} vs {current})",
                order.as_str()
            ),
            CustomerState::InstalledOff,
        ));
    }
    Ok(AutoUpdateDecision::Upgrade(current.to_owned(), posture))
}

struct AutoUpdateRuntime<'a, B> {
    running: Option<String>,
    enrolled: Option<Posture>,
    unit_host: &'a mut dyn UnitHost,
    boot_host: &'a B,
    lifecycle: &'a mut dyn LifecycleHost,
    memory_obs: &'a mut dyn FnMut() -> Option<MemoryObservation>,
}

fn run_auto_update_apply<B: BootPolicyHost>(
    req: &InstallRequest,
    env: &InstallEnvironment,
    validated: ValidatedAutoUpdate<'_>,
    runtime: &mut AutoUpdateRuntime<'_, B>,
    stdout: &mut dyn Write,
) -> Result<(), CustomerError> {
    let (current, posture) = match authorize_auto_update_upgrade(
        env,
        runtime.running.as_deref(),
        runtime.enrolled,
        &validated.candidate,
    )? {
        AutoUpdateDecision::UpToDate(current) => {
            write_up_to_date_output(&current, &validated.candidate, req.json, stdout);
            return Ok(());
        }
        AutoUpdateDecision::Upgrade(current, posture) => (current, posture),
    };
    require_root(env)?;
    let quiescence = match prepare_seamless_auto_update(
        runtime.unit_host,
        runtime.boot_host,
        runtime.lifecycle,
    )? {
        PrepareUpgradeOutcome::Deferred(reason) => {
            append_auto_update_log(
                &env.auto_update_log,
                &format!(
                    "DEFERRED reason={reason} running={current} candidate={}",
                    validated.candidate
                ),
            );
            write_deferred_output(&validated.candidate, &reason, req.json, stdout);
            return Ok(());
        }
        PrepareUpgradeOutcome::Ready(q) => q,
    };
    append_auto_update_log(
        &env.auto_update_log,
        &format!("UPDATE from={current} to={}", validated.candidate),
    );
    let mut apply_req = req.clone();
    apply_req.mode = InstallMode::Apply;
    apply_req.approved_version = Some(validated.candidate.clone());
    apply_req.approved_manifest_sha256 = Some(validated.manifest_sha256);
    apply_req.lower_sink = Some(validated.lower_sink);
    apply_req.posture = Some(posture);
    apply_req.bundle_root = Some(validated.bundle_dir);
    apply_req.running = Some(current);
    apply_req.enroll_default = quiescence.was_boot_enrolled;
    apply_req.no_enroll = !quiescence.was_boot_enrolled;
    if apply_req.policy.is_none() {
        apply_req.policy = Some(validated.policy_path.to_path_buf());
    }
    let apply_res = execute_verified_apply(
        &apply_req,
        env,
        Some(validated.signature),
        runtime.unit_host,
        runtime.boot_host,
        runtime.memory_obs,
        stdout,
    );
    let outcome = finish_seamless_auto_update(&quiescence, runtime.lifecycle, apply_res);
    finalize_auto_update_outcome(
        &env.auto_update_log,
        &validated.candidate,
        req.json,
        outcome,
        stdout,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_auto_update(
    mode: AutoUpdateMode,
    req: &InstallRequest,
    env: &InstallEnvironment,
    unit_host: &mut dyn UnitHost,
    boot_host: &impl BootPolicyHost,
    lifecycle: &mut dyn LifecycleHost,
    memory_obs: &mut dyn FnMut() -> Option<MemoryObservation>,
    stdout: &mut dyn Write,
) -> Result<(), CustomerError> {
    let ctx = load_auto_update_context(req, env)?;
    if mode == AutoUpdateMode::Status {
        write_auto_update_status(
            ctx.policy_path,
            ctx.policy_opt.as_ref(),
            ctx.running.as_deref(),
            ctx.enrolled,
            ctx.newest.as_deref(),
            req.json,
            stdout,
        );
        return Ok(());
    }
    let running = ctx.running.clone();
    let enrolled = ctx.enrolled;
    let validated = validate_auto_update_candidate(ctx, env)?;
    if mode == AutoUpdateMode::Check {
        emit_auto_update_check(
            env,
            running.as_deref(),
            &validated.candidate,
            req.json,
            stdout,
        );
        return Ok(());
    }
    let mut runtime = AutoUpdateRuntime {
        running,
        enrolled,
        unit_host,
        boot_host,
        lifecycle,
        memory_obs,
    };
    run_auto_update_apply(req, env, validated, &mut runtime, stdout)
}

fn reconcile_complete(
    req: &InstallRequest,
    env: &InstallEnvironment,
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    unit_host: &mut dyn UnitHost,
    stdout: &mut dyn Write,
) -> Result<(), CustomerError> {
    let bundle_dir = resolve_bundle_dir(req.bundle_root.as_deref(), &env.default_bundle_root)?;
    let signature = verify_candidate_trust(&bundle_dir, req, env)?;
    let declared = DeclaredBundle::open(&bundle_dir, &pending.new_hash).ok_or_else(|| {
        fs_error(
            CustomerReason::IdentityChanged,
            "NBD_INSTALL_REASON=RECONCILE_BUNDLE_INVALID",
            CustomerState::RecoveryRequired,
        )
    })?;
    if let Some(effect) = inspect_effect_journal(&env.transition)? {
        if effect.pending != *pending
            || effect.phase != SelectorEffectPhase::Verified
            || observe_installed_input_bundle_digest(&env.selector_path()).as_deref()
                != Some(pending.new_hash.as_str())
        {
            return Err(fs_error(
                CustomerReason::RecoveryRequired,
                "NBD_INSTALL_REASON=RECONCILE_COMPLETE_UNSUPPORTED_WITHOUT_SEALED_SELECTOR",
                CustomerState::RecoveryRequired,
            ));
        }
    } else {
        let target_ver = pending.target_release.as_deref().ok_or_else(|| {
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
            },
        )?;
    }
    if verify_release_manifest(&env.selector_path()).is_none() {
        return Err(fs_error(
            CustomerReason::RecoveryRequired,
            "NBD_INSTALL_REASON=RECONCILE_COMPLETE_UNSUPPORTED_WITHOUT_SEALED_SELECTOR",
            CustomerState::RecoveryRequired,
        ));
    }
    unit_host.daemon_reload()?;
    unit_host.sync_selected_release()?;
    clear_stage_artifacts(&env.stage_dir());
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
    if inspect_effect_journal(&env.transition)?.is_some() {
        let _ = reconcile_selector(guard, pending, SelectorRecoveryAction::Rollback, None, None)?;
    }
    if let Some(target_ver) = pending.target_release.as_deref() {
        let candidate_dir = env.releases_dir().join(target_ver);
        if let Ok(md) = std::fs::symlink_metadata(&candidate_dir) {
            remove_owned_unselected_release(
                &candidate_dir,
                (md.dev(), md.ino()),
                &env.selector_path(),
            )?;
        }
    }
    clear_stage_artifacts(&env.stage_dir());
    clear_all_package_journals(guard, pending)?;
    let _ = writeln!(stdout, "NBD_INSTALL_STATE=RECONCILED_ROLLBACK");
    let _ = MANAGED_UNITS;
    Ok(())
}

fn run_reconcile(
    req: &InstallRequest,
    env: &InstallEnvironment,
    action: SelectorRecoveryAction,
    unit_host: &mut dyn UnitHost,
    stdout: &mut dyn Write,
) -> Result<(), CustomerError> {
    let guard = acquire_transition(OperationOwner::Package, &env.transition)?;
    let Some(pending) = read_pending(&env.transition)? else {
        return Err(fs_error(
            CustomerReason::UnsupportedSurface,
            "NBD_INSTALL_REASON=NO_PENDING_TRANSACTION",
            CustomerState::InstalledOff,
        ));
    };
    match action {
        SelectorRecoveryAction::Complete => {
            reconcile_complete(req, env, &guard, &pending, unit_host, stdout)
        }
        SelectorRecoveryAction::Rollback => reconcile_rollback(env, &guard, &pending, stdout),
    }
}

#[cfg(test)]
mod tests;
