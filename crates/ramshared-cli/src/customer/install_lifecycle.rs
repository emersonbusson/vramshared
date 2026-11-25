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
use std::process::Command;
use std::time::Duration;

use super::install_apply::CommittedInstall;
use super::install_args::{InstallRequest, utc_timestamp_now, write_commit_output};
use super::install_fs::fs_error;
use super::install_tokens::EnrollmentOutcome;
use super::install_units::{UnitHost, resolve_systemctl_bin};
use super::{CustomerError, CustomerReason, CustomerState};
use crate::bounded_process::run_capture_command;
use crate::cascade::boot_policy_effect::{
    BootPolicyHost, CASCADE_UNIT, HEALTH_UNIT, HOST_GATE_UNIT, SUPERVISOR_UNIT, observe_boot_policy,
};

const START_CASCADE_TIMEOUT: Duration = Duration::from_secs(150);
const STOP_CASCADE_TIMEOUT: Duration = Duration::from_secs(300);
const UNIT_OP_TIMEOUT: Duration = Duration::from_secs(30);
const ENABLE_ORDER: [&str; 3] = [HOST_GATE_UNIT, SUPERVISOR_UNIT, CASCADE_UNIT];
const DISABLE_ORDER: [&str; 3] = [CASCADE_UNIT, SUPERVISOR_UNIT, HOST_GATE_UNIT];
const FULL_ACTIVATE_ORDER: [&str; 4] = [HEALTH_UNIT, HOST_GATE_UNIT, SUPERVISOR_UNIT, CASCADE_UNIT];

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DrainSafetyVerdict {
    pub(super) safe: bool,
    pub(super) reason: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct LiveRuntimeState {
    pub(super) cascade_active: bool,
    pub(super) supervisor_active: bool,
    pub(super) health_active: bool,
    pub(super) health_enabled: bool,
    pub(super) boot_enrolled: bool,
    pub(super) active_managed_swap: bool,
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
    let boot_obs = observe_boot_policy(boot_host)?;
    let bits = boot_obs.enablement();
    Ok(LiveRuntimeState {
        cascade_active: is_active_state(&cascade.active_state),
        supervisor_active: is_active_state(&supervisor.active_state),
        health_active: is_active_state(&health.active_state),
        health_enabled: is_enabled_state(&health.file_state),
        boot_enrolled: bits.host_gate_enabled && bits.supervisor_enabled && bits.cascade_enabled,
        active_managed_swap,
    })
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
pub(super) fn prepare_seamless_auto_update(
    unit_host: &dyn UnitHost,
    boot_host: &dyn BootPolicyHost,
    lifecycle: &mut dyn LifecycleHost,
) -> Result<PrepareUpgradeOutcome, CustomerError> {
    let state = lifecycle.observe_live_state(unit_host, boot_host)?;
    let is_active = state.cascade_active || state.active_managed_swap;
    if is_active {
        let verdict = lifecycle.evaluate_drain_safety()?;
        if !verdict.safe {
            return Ok(PrepareUpgradeOutcome::Deferred(verdict.reason));
        }
        if state.cascade_active {
            lifecycle.stop_unit(CASCADE_UNIT)?;
        }
        if state.active_managed_swap {
            lifecycle.run_ordered_down()?;
        }
        if state.supervisor_active {
            lifecycle.stop_unit(SUPERVISOR_UNIT)?;
        }
    }
    if state.health_active {
        lifecycle.stop_unit(HEALTH_UNIT)?;
    }

    let mut disabled = Vec::new();
    let disable_result = (|| -> Result<(), CustomerError> {
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
    if let Err(err) = disable_result {
        for unit in disabled.into_iter().rev() {
            let _ = lifecycle.enable_unit(unit);
        }
        return Err(err);
    }

    Ok(PrepareUpgradeOutcome::Ready(UpgradeQuiescenceState {
        was_active: is_active,
        was_boot_enrolled: state.boot_enrolled,
        was_health_active: state.health_active,
        was_health_enabled: state.health_enabled,
    }))
}

/// Complete or restore runtime state after `execute_verified_apply` finishes
/// and releases `CustomerTransitionGuard`.
pub(super) fn finish_seamless_auto_update<T>(
    quiescence: &UpgradeQuiescenceState,
    lifecycle: &mut dyn LifecycleHost,
    apply_result: Result<T, CustomerError>,
) -> Result<(T, ActivationOutcome), CustomerError> {
    match apply_result {
        Ok(committed) => {
            if !quiescence.was_boot_enrolled && quiescence.was_health_enabled {
                lifecycle.enable_unit(HEALTH_UNIT)?;
            }
            let activation = if quiescence.was_active {
                activate_installed_units(lifecycle, false)
            } else if quiescence.was_health_active {
                activate_installed_units(lifecycle, true)
            } else {
                ActivationOutcome::Off
            };
            Ok((committed, activation))
        }
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
            if quiescence.was_active {
                if let ActivationOutcome::Refused { unit, detail } =
                    activate_installed_units(lifecycle, false)
                {
                    restore_errors.push(format!("restart {unit}: {detail}"));
                }
            } else if quiescence.was_health_active
                && let ActivationOutcome::Refused { unit, detail } =
                    activate_installed_units(lifecycle, true)
            {
                restore_errors.push(format!("restart {unit}: {detail}"));
            }
            if restore_errors.is_empty() {
                Err(apply_err)
            } else {
                Err(fs_error(
                    CustomerReason::RecoveryRequired,
                    format!(
                        "{}; runtime restore failed ({})",
                        apply_err.detail,
                        restore_errors.join("; ")
                    ),
                    CustomerState::RecoveryRequired,
                ))
            }
        }
    }
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

/// Production host adapter backed by systemd and canonical `cascade::down`.
pub(super) struct SystemLifecycleHost {
    systemctl_bin: std::path::PathBuf,
    systemd_marker: std::path::PathBuf,
}

impl SystemLifecycleHost {
    pub(super) fn production() -> Self {
        Self {
            systemctl_bin: std::path::PathBuf::from(resolve_systemctl_bin()),
            systemd_marker: std::path::PathBuf::from("/run/systemd/system"),
        }
    }

    #[cfg(test)]
    fn with_paths(systemctl_bin: std::path::PathBuf, systemd_marker: std::path::PathBuf) -> Self {
        Self {
            systemctl_bin,
            systemd_marker,
        }
    }

    fn run_unit(&self, verb: &str, unit: &str, timeout: Duration) -> Result<(), CustomerError> {
        run_systemctl_unit_with(
            &self.systemctl_bin,
            &self.systemd_marker,
            verb,
            unit,
            timeout,
        )
    }
}

fn map_drain_plan_verdict(observed: Result<Option<(bool, String)>, String>) -> DrainSafetyVerdict {
    match observed {
        Ok(None) => DrainSafetyVerdict {
            safe: true,
            reason: "no_lifecycle".to_owned(),
        },
        Ok(Some((safe, reason))) => DrainSafetyVerdict { safe, reason },
        Err(reason) => DrainSafetyVerdict {
            safe: false,
            reason,
        },
    }
}

fn map_ordered_down_error(detail: String) -> CustomerError {
    fs_error(
        CustomerReason::ActiveSession,
        format!("ordered swapoff-first drain failed: {detail}"),
        CustomerState::Active,
    )
}

fn run_systemctl_unit_with(
    systemctl_bin: &Path,
    systemd_marker: &Path,
    verb: &str,
    unit: &str,
    timeout: Duration,
) -> Result<(), CustomerError> {
    if !systemd_marker.exists() {
        return Err(fs_error(
            CustomerReason::ObservationUnavailable,
            format!("systemd unavailable for `{verb} {unit}`"),
            CustomerState::InstalledOff,
        ));
    }
    let mut cmd = Command::new(systemctl_bin);
    cmd.args([verb, "--", unit]);
    run_capture_command(
        &mut cmd,
        "installer lifecycle systemctl",
        timeout,
        4096,
        |_| {},
    )
    .map(|_| ())
    .map_err(|err| match err {
        crate::bounded_process::ProcessSpawnError::NonZeroExit { stderr, .. } => {
            let detail = stderr.trim();
            fs_error(
                CustomerReason::UnsupportedSurface,
                if detail.is_empty() {
                    format!("systemctl {verb} {unit} exited nonzero")
                } else {
                    format!("systemctl {verb} {unit}: {detail}")
                },
                CustomerState::InstalledOff,
            )
        }
        other => fs_error(
            CustomerReason::ObservationUnavailable,
            format!("systemctl {verb} {unit}: {other}"),
            CustomerState::InstalledOff,
        ),
    })
}

impl LifecycleHost for SystemLifecycleHost {
    fn observe_live_state(
        &self,
        unit_host: &dyn UnitHost,
        boot_host: &dyn BootPolicyHost,
    ) -> Result<LiveRuntimeState, CustomerError> {
        let active_managed_swap = !matches!(crate::cascade::evaluate_live_down_plan(), Ok(None));
        observe_systemd_runtime(unit_host, boot_host, active_managed_swap)
    }

    fn evaluate_drain_safety(&self) -> Result<DrainSafetyVerdict, CustomerError> {
        Ok(map_drain_plan_verdict(
            crate::cascade::evaluate_live_down_plan().map_err(|err| err.to_string()),
        ))
    }

    fn stop_unit(&mut self, unit: &str) -> Result<(), CustomerError> {
        let timeout = if unit == CASCADE_UNIT {
            STOP_CASCADE_TIMEOUT
        } else {
            UNIT_OP_TIMEOUT
        };
        self.run_unit("stop", unit, timeout)
    }

    fn start_unit(&mut self, unit: &str) -> Result<(), CustomerError> {
        let timeout = if unit == CASCADE_UNIT {
            START_CASCADE_TIMEOUT
        } else {
            UNIT_OP_TIMEOUT
        };
        self.run_unit("start", unit, timeout)
    }

    fn disable_unit(&mut self, unit: &str) -> Result<(), CustomerError> {
        self.run_unit("disable", unit, UNIT_OP_TIMEOUT)
    }

    fn enable_unit(&mut self, unit: &str) -> Result<(), CustomerError> {
        self.run_unit("enable", unit, UNIT_OP_TIMEOUT)
    }

    fn run_ordered_down(&mut self) -> Result<(), CustomerError> {
        crate::cascade::down_quiet().map_err(|err| map_ordered_down_error(err.to_string()))
    }

    fn ensure_recommended_config(&mut self, stdout: &mut dyn Write) {
        crate::resource_config::recommended::ensure_recommended_cascade_conf_live(stdout);
        crate::resource_config::assistant::emit_post_install_assistant_summary_live(stdout);
    }
}

#[cfg(test)]
mod tests;
