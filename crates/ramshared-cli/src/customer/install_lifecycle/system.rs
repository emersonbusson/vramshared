// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/install_lifecycle/system.rs

//! Native service effects for installer activation and supporting units.

use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use super::super::install_drain::systemd::SystemStop;
use super::super::install_fs::fs_error;
use super::super::install_units::{UnitHost, resolve_systemctl_bin};
use super::super::{CustomerError, CustomerReason, CustomerState};
use super::{DrainSafetyVerdict, LifecycleHost, LiveRuntimeState, observe_systemd_runtime};
use crate::bounded_process::run_capture_command;
use crate::cascade::boot_policy_effect::{BootPolicyHost, CASCADE_UNIT};

const START_CASCADE_TIMEOUT: Duration = Duration::from_secs(150);
const STOP_CASCADE_TIMEOUT: Duration = Duration::from_secs(300);
const UNIT_OP_TIMEOUT: Duration = Duration::from_secs(30);

/// Production host adapter backed by systemd and canonical `cascade::down`.
pub(in crate::customer) struct SystemLifecycleHost {
    systemctl_bin: std::path::PathBuf,
    systemd_marker: std::path::PathBuf,
    scheduled: bool,
}

impl SystemLifecycleHost {
    pub(in crate::customer) fn production(scheduled: bool) -> Self {
        Self {
            systemctl_bin: std::path::PathBuf::from(resolve_systemctl_bin()),
            systemd_marker: std::path::PathBuf::from("/run/systemd/system"),
            scheduled,
        }
    }

    #[cfg(test)]
    pub(super) fn with_paths(
        systemctl_bin: std::path::PathBuf,
        systemd_marker: std::path::PathBuf,
    ) -> Self {
        Self {
            systemctl_bin,
            systemd_marker,
            scheduled: false,
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

pub(super) fn map_drain_plan_verdict(
    observed: Result<Option<(bool, String)>, String>,
) -> DrainSafetyVerdict {
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

pub(super) fn map_ordered_down_error(detail: String) -> CustomerError {
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
    fn upgrade_stop(&self) -> Option<SystemStop<'_>> {
        Some(SystemStop::new(&self.systemctl_bin).with_scheduled_window(self.scheduled))
    }

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
