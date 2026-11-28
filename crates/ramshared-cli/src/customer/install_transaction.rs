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
use std::path::{Path, PathBuf};

use rustix::process::geteuid;

use super::install_apply::{
    execute_verified_apply, verify_declared_payload_identity, verify_install,
};
use super::install_args::{
    AutoUpdateMode, InstallMode, InstallRequest, parse_install_args, write_auto_update_status,
    write_plan_output,
};
use super::install_drain::{DrainInputs, InstallerLock};
use super::install_fs::{WslInstallHostMemoryCache, fs_error};
#[cfg(test)]
use super::install_lifecycle::NoopLifecycleHost;
use super::install_lifecycle::{
    LifecycleHost, PrepareUpgradeOutcome, SystemLifecycleHost, append_auto_update_log,
    complete_drain_custody, emit_committed_install, finalize_auto_update_outcome,
    finish_upgrade_lifecycle, prepare_upgrade_with_custody, run_attended_apply_with,
    write_deferred_output, write_up_to_date_output,
};
use super::install_units::{SystemUnitHost, UnitHost, validate_unit_plan};
use super::package_selector::{SelectorRecoveryAction, SinkWitness};
use super::package_source::DeclaredBundle;
use super::package_source::stream::MemoryObservation;
use super::package_trust::{VerifiedSignature, verify_bundle_from_policy};
use super::release_ordering::{
    AutoUpdatePolicy, ReleaseOrder, find_newest_bundle, read_auto_update_policy,
    read_enrolled_posture, release_is_newer,
};
use super::transition::TransitionPaths;
use super::{CustomerError, CustomerReason, CustomerState};
use crate::cascade::boot_policy_effect::BootPolicyHost;
use crate::cascade::boot_policy_host::SystemBootPolicyHost;
use crate::cascade::posture::Posture;
use crate::release_identity::read_release_version;

mod published_source;
mod reconciliation;
mod runtime_recovery;
mod scheduled;
use reconciliation::{
    finish_pending_drain, prepare_timer_runtime_request, recovered_request, run_reconcile,
};
use runtime_recovery::resume_committed_install;
pub(crate) use scheduled::SOCKET_PATH;

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
    published: bool,
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
    let scheduled = scheduled::is_invocation(args);
    if scheduled {
        match scheduled::prepare(&env) {
            Ok(false) => return 0,
            Ok(true) => {}
            Err(err) => {
                let _ = writeln!(stderr, "{err}");
                return err.exit_code();
            }
        }
    }
    let mut lifecycle = SystemLifecycleHost::production(scheduled);
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
            let lock = installer_lock(env, lifecycle)?;
            run_reconcile(
                req,
                env,
                action,
                lock.as_ref(),
                unit_host,
                boot_host,
                stdout,
            )?;
            if action == SelectorRecoveryAction::Complete
                && let Some(lock) = lock.as_ref()
                && let Some((inputs, _, _)) = lock.committed_runtime()?
            {
                let recovered = recovered_request(req, env, &inputs)?;
                if let Some((committed, activation)) = resume_committed_install(
                    &recovered,
                    env,
                    Some(lock),
                    None,
                    unit_host,
                    boot_host,
                    lifecycle,
                    stdout,
                )? {
                    emit_committed_install(&committed, &activation, req.json, stdout);
                }
            }
            Ok(())
        }
        InstallMode::Apply => {
            require_root(env)?;
            let lock = installer_lock(env, lifecycle)?;
            if let Some(lock) = &lock {
                lock.start_progress(req.approved_version.as_deref());
            }
            finish_pending_drain(req, env, lock.as_ref(), unit_host, boot_host)?;
            if let Some((committed, activation)) = resume_committed_install(
                req,
                env,
                lock.as_ref(),
                None,
                unit_host,
                boot_host,
                lifecycle,
                stdout,
            )? {
                emit_committed_install(&committed, &activation, req.json, stdout);
                return Ok(());
            }
            let (verified, drain) = verify_install(req, env, unit_host, None)?;
            let selector = env.selector_path();
            run_attended_apply_with(
                req,
                &selector,
                lock.as_ref().map(|lock| (lock, drain.as_ref())),
                unit_host,
                boot_host,
                lifecycle,
                stdout,
                |apply_req, u_host, out| {
                    execute_verified_apply(
                        apply_req,
                        env,
                        verified,
                        lock.as_ref(),
                        u_host,
                        boot_host,
                        memory_obs,
                        out,
                    )
                },
            )
        }
    }
}

fn installer_lock(
    env: &InstallEnvironment,
    lifecycle: &dyn LifecycleHost,
) -> Result<Option<InstallerLock>, CustomerError> {
    lifecycle
        .upgrade_stop()
        .map(|_| InstallerLock::acquire(&env.transition))
        .transpose()
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
    active: Option<&DrainInputs>,
    fetched: Option<published_source::FetchedPatch>,
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
    let published = published_source::enabled(req, policy_opt.as_ref());
    let newest = match active {
        Some(active) => Some(
            published_source::find_drain_bundle(
                env,
                bundle_root,
                policy_path,
                &active.candidate_manifest,
            )?
            .ok_or_else(|| {
                super::transition::recovery_refusal("original signed upgrade bundle is unavailable")
            })?,
        ),
        None if published => {
            fetched.and_then(|patch| patch.for_running(running.as_deref(), &env.selector_path()))
        }
        None => find_newest_bundle(bundle_root).ok().flatten(),
    };
    Ok(AutoUpdateContext {
        policy_path,
        policy_opt,
        published,
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
    explicit_bundle: bool,
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
    if order == ReleaseOrder::Equal || (order == ReleaseOrder::Older && !explicit_bundle) {
        append_auto_update_log(
            &env.auto_update_log,
            &format!(
                "UP-TO-DATE running={current} candidate={candidate} order={}",
                order.as_str(),
            ),
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
    lock: Option<&InstallerLock>,
    runtime: &mut AutoUpdateRuntime<'_, B>,
    stdout: &mut dyn Write,
) -> Result<(), CustomerError> {
    let mut apply_req = InstallRequest {
        mode: InstallMode::Apply,
        approved_version: Some(validated.candidate.clone()),
        approved_manifest_sha256: Some(validated.manifest_sha256),
        lower_sink: Some(validated.lower_sink),
        posture: runtime.enrolled,
        bundle_root: Some(validated.bundle_dir),
        running: runtime.running.clone(),
        policy: Some(validated.policy_path.to_path_buf()),
        ..req.clone()
    };
    prepare_timer_runtime_request(
        &mut apply_req,
        env,
        lock,
        runtime.unit_host,
        runtime.boot_host,
    )?;
    if let Some(resumed) = resume_committed_install(
        &apply_req,
        env,
        lock,
        Some(&validated.signature),
        runtime.unit_host,
        runtime.boot_host,
        runtime.lifecycle,
        stdout,
    )? {
        return finalize_auto_update_outcome(
            &env.auto_update_log,
            &validated.candidate,
            req.json,
            Ok(resumed),
            stdout,
        );
    }
    let (current, posture) = match authorize_auto_update_upgrade(
        env,
        runtime.running.as_deref(),
        runtime.enrolled,
        &validated.candidate,
        req.bundle_root.is_some(),
    )? {
        AutoUpdateDecision::UpToDate(current) => {
            write_up_to_date_output(&current, &validated.candidate, req.json, stdout);
            return Ok(());
        }
        AutoUpdateDecision::Upgrade(current, posture) => (current, posture),
    };
    if let Some(lock) = lock {
        lock.start_progress(Some(&validated.candidate));
    }
    apply_req.posture = Some(posture);
    let (verified, drain) = verify_install(
        &apply_req,
        env,
        runtime.unit_host,
        Some(validated.signature),
    )?;
    let custody = lock.map(|lock| (lock, drain.as_ref()));
    let live = runtime
        .lifecycle
        .observe_live_state(runtime.unit_host, runtime.boot_host)?;
    let quiescence = match prepare_upgrade_with_custody(live, runtime.lifecycle, custody)? {
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
    apply_req.enroll_default = quiescence.was_boot_enrolled;
    apply_req.no_enroll = !quiescence.was_boot_enrolled;
    let apply_res = execute_verified_apply(
        &apply_req,
        env,
        verified,
        lock,
        runtime.unit_host,
        runtime.boot_host,
        runtime.memory_obs,
        stdout,
    );
    let outcome = finish_upgrade_lifecycle(
        &quiescence,
        runtime.lifecycle,
        apply_res,
        |_| false,
        req.no_activate,
        stdout,
    );
    complete_drain_custody(custody, &outcome, runtime.unit_host)?;
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
    let mut lock = if mode == AutoUpdateMode::Apply {
        require_root(env)?;
        installer_lock(env, lifecycle)?
    } else {
        None
    };
    let active = lock
        .as_ref()
        .map(InstallerLock::active_inputs)
        .transpose()?
        .flatten();
    let fetched = if mode == AutoUpdateMode::Apply && active.is_none() {
        published_source::fetch_outside_install_lock(req, env, lifecycle, &mut lock)?
    } else {
        None
    };
    let active = lock
        .as_ref()
        .map(InstallerLock::active_inputs)
        .transpose()?
        .flatten();
    let ctx = load_auto_update_context(req, env, active.as_ref(), fetched)?;
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
    if ctx.published && ctx.newest.is_none() {
        published_source::write_no_prepared_patch(req.json, stdout);
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
    run_auto_update_apply(req, env, validated, lock.as_ref(), &mut runtime, stdout)
}

#[cfg(test)]
mod tests;
