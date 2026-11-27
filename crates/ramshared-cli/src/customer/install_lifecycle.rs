// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/install_lifecycle.rs
//! Fail-closed post-install activation and seamless active-tier auto-update lifecycle.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::install_apply::CommittedInstall;
use super::install_args::{InstallRequest, utc_timestamp_now, write_commit_output};
use super::install_drain::{DrainInputs, DrainOutcome, InstallerLock, systemd::SystemStop};
use super::install_fs::fs_error;
use super::install_tokens::EnrollmentOutcome;
use super::install_units::{UnitHost, UnitRuntimeStatus};
use super::{CustomerError, CustomerReason, CustomerState};
use crate::cascade::boot_policy_effect::{
    BootPolicyHost, CASCADE_UNIT, HEALTH_UNIT, HOST_GATE_UNIT, SUPERVISOR_UNIT, observe_boot_policy,
};

const ENABLE_ORDER: [&str; 3] = [HOST_GATE_UNIT, SUPERVISOR_UNIT, CASCADE_UNIT];
const DISABLE_ORDER: [&str; 3] = [CASCADE_UNIT, SUPERVISOR_UNIT, HOST_GATE_UNIT];
const FULL_ACTIVATE_ORDER: [&str; 4] = [HEALTH_UNIT, HOST_GATE_UNIT, SUPERVISOR_UNIT, CASCADE_UNIT];

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DrainSafetyVerdict {
    pub(super) safe: bool,
    pub(super) reason: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LiveRuntimeState {
    pub(super) cascade_active: bool,
    pub(super) supervisor_active: bool,
    #[serde(default)]
    pub(super) host_gate_active: bool,
    pub(super) health_active: bool,
    pub(super) health_enabled: bool,
    pub(super) boot_enrolled: bool,
    pub(super) active_managed_swap: bool,
}

impl LiveRuntimeState {
    pub(super) fn needs_upgrade_quiescence(&self) -> bool {
        self.cascade_active
            || self.supervisor_active
            || self.host_gate_active
            || self.health_active
            || self.health_enabled
            || self.boot_enrolled
            || self.active_managed_swap
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ActivationOutcome {
    Off,
    Active,
    Refused { unit: &'static str, detail: String },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct UpgradeQuiescenceState {
    pub(super) was_active: bool,
    pub(super) was_supervisor_active: bool,
    pub(super) was_host_gate_active: bool,
    pub(super) was_boot_enrolled: bool,
    pub(super) was_health_active: bool,
    pub(super) was_health_enabled: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum PrepareUpgradeOutcome {
    Ready(UpgradeQuiescenceState),
    Deferred(String),
}

pub(super) trait LifecycleHost {
    fn observe_live_state(
        &self,
        unit_host: &dyn UnitHost,
        boot_host: &dyn BootPolicyHost,
    ) -> Result<LiveRuntimeState, CustomerError>;
    fn evaluate_drain_safety(&self) -> Result<DrainSafetyVerdict, CustomerError>;
    fn stop_unit(&mut self, unit: &str) -> Result<(), CustomerError>;
    fn start_unit(&mut self, unit: &str) -> Result<(), CustomerError>;
    fn disable_unit(&mut self, unit: &str) -> Result<(), CustomerError>;
    fn enable_unit(&mut self, unit: &str) -> Result<(), CustomerError>;
    fn run_ordered_down(&mut self) -> Result<(), CustomerError>;
    fn ensure_recommended_config(&mut self, _stdout: &mut dyn Write) {}
    fn upgrade_stop(&self) -> Option<SystemStop<'_>> {
        None
    }
}

/// Pure observation of systemd units plus caller-supplied managed-lifecycle fact.
pub(super) fn observe_systemd_runtime(
    unit_host: &dyn UnitHost,
    boot_host: &dyn BootPolicyHost,
    active_managed_swap: bool,
) -> Result<LiveRuntimeState, CustomerError> {
    let cascade = unit_host.query_unit(CASCADE_UNIT)?;
    let supervisor = unit_host.query_unit(SUPERVISOR_UNIT)?;
    let health = unit_host.query_unit(HEALTH_UNIT)?;
    let host_gate = unit_host.query_unit(HOST_GATE_UNIT)?;
    let boot_enrolled = if !active_managed_swap
        && [&cascade, &health].into_iter().all(is_inert_disabled_unit)
        && [&supervisor, &host_gate].into_iter().all(is_absent_unit)
    {
        // Earlier native installs could leave both boot prerequisites absent.
        false
    } else {
        let bits = observe_boot_policy(boot_host)?.enablement();
        bits.host_gate_enabled && bits.supervisor_enabled && bits.cascade_enabled
    };
    Ok(LiveRuntimeState {
        cascade_active: is_active_state(&cascade.active_state),
        supervisor_active: is_active_state(&supervisor.active_state),
        host_gate_active: is_active_state(&host_gate.active_state),
        health_active: is_active_state(&health.active_state),
        health_enabled: is_enabled_state(&health.file_state),
        boot_enrolled,
        active_managed_swap,
    })
}

fn is_inert_disabled_unit(unit: &UnitRuntimeStatus) -> bool {
    unit.active_state == "inactive" && unit.file_state == "disabled"
}

fn is_absent_unit(unit: &UnitRuntimeStatus) -> bool {
    unit.active_state == "inactive" && matches!(unit.file_state.as_str(), "" | "not-found")
}

fn is_active_state(state: &str) -> bool {
    matches!(
        state,
        "active" | "activating" | "reloading" | "deactivating"
    )
}

fn is_enabled_state(state: &str) -> bool {
    matches!(
        state,
        "enabled" | "enabled-runtime" | "linked" | "linked-runtime" | "alias"
    )
}

/// Immediate post-install activation runs only when enrollment succeeded and
/// the operator did not opt out with `--no-activate`.
pub(super) fn should_activate_after_install(
    req: &InstallRequest,
    enroll: Option<&EnrollmentOutcome>,
) -> bool {
    req.enroll_default
        && !req.no_activate
        && matches!(enroll, Some(EnrollmentOutcome::Enrolled { .. }))
}

/// Start enrolled units in strict dependency order AFTER package custody is
/// released. Never forces or bypasses hardware/host safety gates.
pub(super) fn activate_installed_units(
    host: &mut dyn LifecycleHost,
    health_only: bool,
) -> ActivationOutcome {
    let units: &[&'static str] = if health_only {
        &[HEALTH_UNIT]
    } else {
        &FULL_ACTIVATE_ORDER
    };
    for &unit in units {
        if let Err(err) = host.start_unit(unit) {
            return ActivationOutcome::Refused {
                unit,
                detail: err.detail,
            };
        }
    }
    ActivationOutcome::Active
}

/// Gate-then-drain preparation before an unattended `--auto-update`.
///
/// If managed swap or cascade is active and canonical `down --plan` is not
/// safe, returns `PrepareUpgradeOutcome::Deferred` with zero mutations.
#[cfg(test)]
pub(super) fn prepare_seamless_auto_update(
    unit_host: &dyn UnitHost,
    boot_host: &dyn BootPolicyHost,
    lifecycle: &mut dyn LifecycleHost,
) -> Result<PrepareUpgradeOutcome, CustomerError> {
    let state = lifecycle.observe_live_state(unit_host, boot_host)?;
    prepare_upgrade_from_state(state, lifecycle, false)
}

pub(super) fn prepare_upgrade_with_custody(
    live: LiveRuntimeState,
    lifecycle: &mut dyn LifecycleHost,
    custody: Option<(&InstallerLock, Option<&DrainInputs>)>,
) -> Result<PrepareUpgradeOutcome, CustomerError> {
    if let (Some((lock, inputs)), Some(systemd)) = (custody, lifecycle.upgrade_stop()) {
        match lock.prepare(inputs, &live, lifecycle, &systemd)? {
            DrainOutcome::Unneeded => {}
            DrainOutcome::Deferred(reason) => {
                return Ok(PrepareUpgradeOutcome::Deferred(reason));
            }
            DrainOutcome::Drained(prior) => {
                return prepare_upgrade_from_state(prior, lifecycle, true);
            }
        }
    }
    prepare_upgrade_from_state(live, lifecycle, false)
}

fn prepare_upgrade_from_state(
    state: LiveRuntimeState,
    lifecycle: &mut dyn LifecycleHost,
    drained: bool,
) -> Result<PrepareUpgradeOutcome, CustomerError> {
    let is_active = state.cascade_active || state.active_managed_swap;
    if is_active && !drained {
        let verdict = lifecycle.evaluate_drain_safety()?;
        if !verdict.safe {
            return Ok(PrepareUpgradeOutcome::Deferred(verdict.reason));
        }
    }

    let mut stopped = UpgradeQuiescenceState::default();
    let mut disabled = Vec::new();
    let prepare_result = (|| -> Result<(), CustomerError> {
        if !drained {
            if state.cascade_active {
                lifecycle.stop_unit(CASCADE_UNIT)?;
            }
            if state.active_managed_swap {
                lifecycle.run_ordered_down()?;
            }
        }
        stopped.was_active = is_active;
        if state.supervisor_active {
            lifecycle.stop_unit(SUPERVISOR_UNIT)?;
            stopped.was_supervisor_active = true;
        }
        if state.host_gate_active {
            lifecycle.stop_unit(HOST_GATE_UNIT)?;
            stopped.was_host_gate_active = true;
        }
        if state.health_active {
            lifecycle.stop_unit(HEALTH_UNIT)?;
            stopped.was_health_active = true;
        }
        if state.boot_enrolled {
            for unit in DISABLE_ORDER {
                lifecycle.disable_unit(unit)?;
                disabled.push(unit);
            }
        }
        if state.health_enabled {
            lifecycle.disable_unit(HEALTH_UNIT)?;
            disabled.push(HEALTH_UNIT);
        }
        Ok(())
    })();
    if let Err(err) = prepare_result {
        let mut restore_errors = Vec::new();
        for unit in disabled.into_iter().rev() {
            if let Err(enable_err) = lifecycle.enable_unit(unit) {
                restore_errors.push(format!("{unit}: {}", enable_err.detail));
            }
        }
        record_activation_error(restore_runtime(&stopped, lifecycle), &mut restore_errors);
        return Err(with_restore_errors(err, restore_errors));
    }

    Ok(PrepareUpgradeOutcome::Ready(UpgradeQuiescenceState {
        was_active: is_active,
        was_supervisor_active: state.supervisor_active,
        was_host_gate_active: state.host_gate_active,
        was_boot_enrolled: state.boot_enrolled,
        was_health_active: state.health_active,
        was_health_enabled: state.health_enabled,
    }))
}

/// Attended `ramshared install` apply coordinator.
///
/// Automatically detects an already-installed host with active units, boot
/// enrollment, or active swap; safely quiesces via canonical drain check,
/// preserves boot enrollment and `/etc/ramshared/*` configuration across the
/// upgrade, and restores/activates all 3 tiers afterward.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_attended_apply_with(
    req: &InstallRequest,
    selector_path: &Path,
    custody: Option<(&InstallerLock, Option<&DrainInputs>)>,
    unit_host: &mut dyn UnitHost,
    boot_host: &dyn BootPolicyHost,
    lifecycle: &mut dyn LifecycleHost,
    stdout: &mut dyn Write,
    apply_fn: impl FnOnce(
        &InstallRequest,
        &mut dyn UnitHost,
        &mut dyn Write,
    ) -> Result<CommittedInstall, CustomerError>,
) -> Result<(), CustomerError> {
    let installed = match std::fs::symlink_metadata(selector_path) {
        Ok(_) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => {
            return Err(fs_error(
                CustomerReason::ObservationUnavailable,
                format!("could not observe installed release selector: {err}"),
                CustomerState::InstalledOff,
            ));
        }
    };
    if installed {
        let live = lifecycle.observe_live_state(unit_host, boot_host)?;
        if live.needs_upgrade_quiescence() || custody.is_some() {
            let quiescence = match prepare_upgrade_with_custody(live, lifecycle, custody)? {
                PrepareUpgradeOutcome::Ready(q) => q,
                PrepareUpgradeOutcome::Deferred(reason) => {
                    let code = if reason.starts_with("DRAIN_PENDING_JOB_")
                        || matches!(
                            reason.as_str(),
                            "CASCADE_NOT_SETTLED_FOR_STOP" | "STOP_REPLY_NOT_RECORDED"
                        ) {
                        "DRAIN_DEFERRED"
                    } else {
                        "DRAIN_UNSAFE"
                    };
                    return Err(fs_error(
                        CustomerReason::ActiveSession,
                        format!("NBD_INSTALL_REASON={code} ({reason})"),
                        CustomerState::Active,
                    ));
                }
            };
            let mut apply_req = req.clone();
            if !req.enroll_default && !req.no_enroll && quiescence.was_boot_enrolled {
                apply_req.enroll_default = true;
            }
            let apply_res = apply_fn(&apply_req, unit_host, stdout);
            let outcome = finish_upgrade_lifecycle(
                &quiescence,
                lifecycle,
                apply_res,
                |c| should_activate_after_install(req, c.enroll_outcome.as_ref()),
                req.no_activate,
                stdout,
            );
            complete_drain_custody(custody, &outcome, unit_host)?;
            let (committed, activation) = outcome?;
            emit_committed_install(&committed, &activation, req.json, stdout);
            return Ok(());
        }
    } else if let Some((lock, _)) = custody {
        lock.validate_no_prior_install()?;
    }
    let committed = apply_fn(req, unit_host, stdout)?;
    let activation = if should_activate_after_install(req, committed.enroll_outcome.as_ref()) {
        lifecycle.ensure_recommended_config(stdout);
        activate_installed_units(lifecycle, false)
    } else {
        ActivationOutcome::Off
    };
    emit_committed_install(&committed, &activation, req.json, stdout);
    Ok(())
}

pub(super) fn complete_drain_custody<T>(
    custody: Option<(&InstallerLock, Option<&DrainInputs>)>,
    outcome: &Result<(T, ActivationOutcome), CustomerError>,
    unit_host: &dyn UnitHost,
) -> Result<(), CustomerError> {
    let Some((lock, inputs)) = custody else {
        return Ok(());
    };
    let restored = match outcome {
        Ok((_, activation)) => !matches!(activation, ActivationOutcome::Refused { .. }),
        Err(error) => error.state != CustomerState::RecoveryRequired,
    };
    if restored {
        if outcome.is_ok()
            && let Some((input, _, _)) = lock.committed_runtime()?
        {
            observe_committed_activation(unit_host, input.no_activate)?;
        }
        lock.clear_finished(inputs, outcome.is_ok())?;
    }
    Ok(())
}

pub(super) fn observe_committed_activation(
    unit_host: &dyn UnitHost,
    no_activate: bool,
) -> Result<ActivationOutcome, CustomerError> {
    let runtime = unit_host.query_unit(CASCADE_UNIT)?;
    match runtime.active_state.as_str() {
        "active" => Ok(ActivationOutcome::Active),
        "inactive" if no_activate => Ok(ActivationOutcome::Off),
        state => Err(super::transition::recovery_refusal(format!(
            "committed cascade did not settle ({state})"
        ))),
    }
}

/// Complete or restore runtime state after `execute_verified_apply` finishes
/// and releases `CustomerTransitionGuard`.
#[cfg(test)]
pub(super) fn finish_seamless_auto_update<T>(
    quiescence: &UpgradeQuiescenceState,
    lifecycle: &mut dyn LifecycleHost,
    apply_result: Result<T, CustomerError>,
) -> Result<(T, ActivationOutcome), CustomerError> {
    let mut sink = std::io::sink();
    finish_upgrade_lifecycle(
        quiescence,
        lifecycle,
        apply_result,
        |_| false,
        false,
        &mut sink,
    )
}

/// Shared post-apply runtime restoration for both `--auto-update` and attended
/// in-place upgrades over an already-installed host. Preserves existing
/// `/etc/ramshared/*` configuration (or seeds safe 3-tier defaults if absent)
/// and restores/activates all 3 tiers when previously active or newly enrolled.
pub(super) fn finish_upgrade_lifecycle<T>(
    quiescence: &UpgradeQuiescenceState,
    lifecycle: &mut dyn LifecycleHost,
    apply_result: Result<T, CustomerError>,
    should_activate_committed: impl FnOnce(&T) -> bool,
    no_activate: bool,
    stdout: &mut dyn Write,
) -> Result<(T, ActivationOutcome), CustomerError> {
    match apply_result {
        Ok(committed) => {
            if !quiescence.was_boot_enrolled && quiescence.was_health_enabled {
                lifecycle.enable_unit(HEALTH_UNIT)?;
            }
            let activate_full =
                !no_activate && (quiescence.was_active || should_activate_committed(&committed));
            let activation = if activate_full {
                lifecycle.ensure_recommended_config(stdout);
                activate_installed_units(lifecycle, false)
            } else if no_activate {
                ActivationOutcome::Off
            } else {
                restore_runtime(quiescence, lifecycle)
            };
            Ok((committed, activation))
        }
        Err(apply_err) if apply_err.state == CustomerState::RecoveryRequired => Err(apply_err),
        Err(apply_err) => {
            let mut restore_errors = Vec::new();
            if quiescence.was_boot_enrolled {
                for unit in ENABLE_ORDER {
                    if let Err(err) = lifecycle.enable_unit(unit) {
                        restore_errors.push(format!("{unit}: {}", err.detail));
                    }
                }
            }
            if quiescence.was_health_enabled
                && let Err(err) = lifecycle.enable_unit(HEALTH_UNIT)
            {
                restore_errors.push(format!("{HEALTH_UNIT}: {}", err.detail));
            }
            record_activation_error(restore_runtime(quiescence, lifecycle), &mut restore_errors);
            Err(with_restore_errors(apply_err, restore_errors))
        }
    }
}

fn restore_runtime(
    state: &UpgradeQuiescenceState,
    lifecycle: &mut dyn LifecycleHost,
) -> ActivationOutcome {
    if state.was_active {
        return activate_installed_units(lifecycle, false);
    }
    let units = [
        (HEALTH_UNIT, state.was_health_active),
        (HOST_GATE_UNIT, state.was_host_gate_active),
        (SUPERVISOR_UNIT, state.was_supervisor_active),
    ];
    for (unit, was_active) in units {
        if was_active && let Err(err) = lifecycle.start_unit(unit) {
            return ActivationOutcome::Refused {
                unit,
                detail: err.detail,
            };
        }
    }
    ActivationOutcome::Off
}

fn record_activation_error(outcome: ActivationOutcome, errors: &mut Vec<String>) {
    if let ActivationOutcome::Refused { unit, detail } = outcome {
        errors.push(format!("restart {unit}: {detail}"));
    }
}

fn with_restore_errors(error: CustomerError, restore_errors: Vec<String>) -> CustomerError {
    if restore_errors.is_empty() {
        return error;
    }
    fs_error(
        CustomerReason::RecoveryRequired,
        format!(
            "{}; runtime restore failed ({})",
            error.detail,
            restore_errors.join("; ")
        ),
        CustomerState::RecoveryRequired,
    )
}

pub(super) fn write_up_to_date_output(
    running: &str,
    candidate: &str,
    json: bool,
    stdout: &mut dyn Write,
) {
    if json {
        let doc = serde_json::json!({
            "state": "up_to_date",
            "running": running,
            "candidate": candidate,
        });
        let _ = writeln!(stdout, "{doc}");
        return;
    }
    let _ = writeln!(stdout, "NBD_AUTO_UPDATE_STATE=UP_TO_DATE");
}

pub(super) fn write_deferred_output(
    candidate: &str,
    reason: &str,
    json: bool,
    stdout: &mut dyn Write,
) {
    if json {
        let doc = serde_json::json!({
            "state": "deferred",
            "candidate": candidate,
            "reason": reason,
        });
        let _ = writeln!(stdout, "{doc}");
        return;
    }
    let _ = writeln!(stdout, "NBD_AUTO_UPDATE_STATE=DEFERRED");
    let _ = writeln!(stdout, "NBD_AUTO_UPDATE_CANDIDATE={candidate}");
    let _ = writeln!(stdout, "NBD_AUTO_UPDATE_REASON={reason}");
}

pub(super) fn emit_committed_install(
    committed: &CommittedInstall,
    activation: &ActivationOutcome,
    json: bool,
    stdout: &mut dyn Write,
) {
    write_commit_output(
        &committed.release,
        &committed.selector,
        committed.posture,
        committed.enroll_outcome.as_ref(),
        activation,
        json,
        stdout,
    );
}

pub(super) fn append_auto_update_log(log_path: &Path, line: &str) {
    let formatted = format!("{} {line}\n", utc_timestamp_now());
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(log_path) {
        let _ = file.write_all(formatted.as_bytes());
    }
}

pub(super) fn finalize_auto_update_outcome(
    log_path: &Path,
    candidate: &str,
    json: bool,
    outcome: Result<(CommittedInstall, ActivationOutcome), CustomerError>,
    stdout: &mut dyn Write,
) -> Result<(), CustomerError> {
    match outcome {
        Ok((committed, activation)) => {
            emit_committed_install(&committed, &activation, json, stdout);
            append_auto_update_log(
                log_path,
                &format!(
                    "INSTALLED {candidate}{}",
                    activation_log_suffix(&activation)
                ),
            );
            Ok(())
        }
        Err(err) => {
            append_auto_update_log(
                log_path,
                &format!("FAILED rc={} candidate={candidate}", err.exit_code()),
            );
            Err(err)
        }
    }
}

pub(super) fn activation_log_suffix(activation: &ActivationOutcome) -> &'static str {
    match activation {
        ActivationOutcome::Active => " activation=ACTIVE",
        ActivationOutcome::Off => "",
        ActivationOutcome::Refused { .. } => " activation=REFUSED",
    }
}

/// No-op lifecycle adapter used by isolated package-transaction unit tests that
/// do not touch live systemd or swap.
#[cfg(test)]
pub(super) struct NoopLifecycleHost;

#[cfg(test)]
impl LifecycleHost for NoopLifecycleHost {
    fn observe_live_state(
        &self,
        _unit_host: &dyn UnitHost,
        _boot_host: &dyn BootPolicyHost,
    ) -> Result<LiveRuntimeState, CustomerError> {
        Ok(LiveRuntimeState::default())
    }
    fn evaluate_drain_safety(&self) -> Result<DrainSafetyVerdict, CustomerError> {
        Ok(DrainSafetyVerdict {
            safe: true,
            reason: "noop".to_owned(),
        })
    }
    fn stop_unit(&mut self, _unit: &str) -> Result<(), CustomerError> {
        Ok(())
    }
    fn start_unit(&mut self, _unit: &str) -> Result<(), CustomerError> {
        Ok(())
    }
    fn disable_unit(&mut self, _unit: &str) -> Result<(), CustomerError> {
        Ok(())
    }
    fn enable_unit(&mut self, _unit: &str) -> Result<(), CustomerError> {
        Ok(())
    }
    fn run_ordered_down(&mut self) -> Result<(), CustomerError> {
        Ok(())
    }
}

mod system;
pub(super) use system::SystemLifecycleHost;
#[cfg(test)]
use system::{map_drain_plan_verdict, map_ordered_down_error};

#[cfg(test)]
mod tests;
