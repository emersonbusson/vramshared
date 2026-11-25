// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/release_coherence/systemd.rs
//! Systemd-backed fleet observation and joint-move effect for release coherence.

use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::bounded_process;
use crate::installed_image::PRODUCT_ROOT;

use super::sync_custody::ReleaseSyncGuard;
use super::{
    ProductProcessView, ReleaseCoherence, ReleaseCoherenceEffect, RestartFailure, observe_fleet,
};

/// How long one `systemctl` call may take before it is a failure.
///
/// A joint move is not on a hot path — it runs after an install or an `up` —
/// so the bound is generous enough for a local dbus round-trip and still
/// finite. A hung systemd is reported as a failed observation, never waited on
/// forever.
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(5);

/// The systemd-backed fleet observation.
///
/// Observation reads, never effects. Everything here is available to an
/// unprivileged reader on purpose: the status surface must be honest even when
/// it is not root.
pub(crate) struct SystemProductProcessView {
    systemctl_bin: String,
    units: std::cell::RefCell<std::collections::HashMap<String, UnitObservation>>,
}

impl Default for SystemProductProcessView {
    fn default() -> Self {
        Self {
            systemctl_bin: "systemctl".to_string(),
            units: std::cell::RefCell::new(std::collections::HashMap::new()),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct UnitObservation {
    pub(super) active: Option<bool>,
    pub(super) main_pid: Option<u32>,
    invocation: Option<[u8; 32]>,
}

impl SystemProductProcessView {
    pub(super) fn with_systemctl(bin: impl Into<String>) -> Self {
        Self {
            systemctl_bin: bin.into(),
            units: std::cell::RefCell::new(std::collections::HashMap::new()),
        }
    }

    /// One `systemctl show` per unit, cached, because the trait asks for the
    /// activity and the pid as separate facts.
    fn observe_unit(&self, unit: &str) -> Option<UnitObservation> {
        if let Some(cached) = self.units.borrow().get(unit) {
            return Some(*cached);
        }
        let observed = self.query_unit(unit)?;
        self.units.borrow_mut().insert(unit.to_string(), observed);
        Some(observed)
    }

    fn query_unit(&self, unit: &str) -> Option<UnitObservation> {
        let mut command = std::process::Command::new(&self.systemctl_bin);
        command.args([
            "show",
            "--property=ActiveState",
            "--property=MainPID",
            "--property=InvocationID",
            "--no-pager",
            "--",
            unit,
        ]);
        let output = bounded_process::run_capture_command(
            &mut command,
            "release-coherence unit query",
            SYSTEMCTL_TIMEOUT,
            bounded_process::DEFAULT_OUTPUT_LIMIT,
            |_| {},
        )
        .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8(output.stdout).ok()?;
        Some(parse_unit_observation(&text))
    }

    pub(super) fn unit_matches_running(
        &self,
        unit: &str,
        expected: &super::systemd_jobs::RunningUnit,
    ) -> bool {
        self.observe_unit(unit).is_some_and(|observation| {
            observation.main_pid == Some(expected.pid)
                && observation
                    .invocation
                    .is_some_and(|id| id.as_slice() == expected.invocation.as_bytes())
        }) && self.unit_observation_still_current(unit)
    }
}

pub(super) fn parse_unit_observation(text: &str) -> UnitObservation {
    let mut observed = UnitObservation {
        active: None,
        main_pid: None,
        invocation: None,
    };
    let mut seen_active = false;
    let mut seen_pid = false;
    let mut seen_invocation = false;
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "ActiveState" => {
                if seen_active {
                    return UnitObservation {
                        active: None,
                        main_pid: None,
                        invocation: None,
                    };
                }
                seen_active = true;
                observed.active = match value {
                    "active" | "reloading" => Some(true),
                    "inactive" | "failed" | "deactivating" | "activating" => Some(false),
                    _ => None,
                };
            }
            "MainPID" => {
                if seen_pid {
                    return UnitObservation {
                        active: None,
                        main_pid: None,
                        invocation: None,
                    };
                }
                seen_pid = true;
                // `0` is kept: it is the observed fact that the unit has no
                // main process, which is what a completed `Type=oneshot`
                // reports. Filtering it away would fold that fact into "the
                // query did not say" and let a running oneshot gate look like
                // an unclassifiable process.
                observed.main_pid = value.parse::<u32>().ok();
            }
            "InvocationID" => {
                if seen_invocation {
                    return UnitObservation {
                        active: None,
                        main_pid: None,
                        invocation: None,
                    };
                }
                seen_invocation = true;
                if value.len() == 32
                    && value.bytes().all(|byte| byte.is_ascii_hexdigit())
                    && value.bytes().any(|byte| byte != b'0')
                {
                    let mut id = [0u8; 32];
                    for (dest, byte) in id.iter_mut().zip(value.bytes()) {
                        *dest = byte.to_ascii_lowercase();
                    }
                    observed.invocation = Some(id);
                }
            }
            _ => {}
        }
    }
    observed
}

impl ProductProcessView for SystemProductProcessView {
    fn unit_is_active(&self, unit: &str) -> Option<bool> {
        self.observe_unit(unit)?.active
    }

    fn unit_main_pid(&self, unit: &str) -> Option<u32> {
        self.observe_unit(unit)?.main_pid
    }

    fn unit_observation_still_current(&self, unit: &str) -> bool {
        let Some(before) = self.observe_unit(unit) else {
            return false;
        };
        before.active == Some(true)
            && before.main_pid.is_some_and(|pid| pid != 0)
            && before.invocation.is_some()
            && self.query_unit(unit) == Some(before)
    }

    fn process_executable(&self, pid: u32) -> Option<super::ObservedExecutable> {
        // Keep the procfs reference: resolving it to a pathname and reopening
        // that pathname can inspect a replacement rather than the image held
        // by the process. A missing or inaccessible image remains unknown.
        super::ObservedExecutable::open_process(pid)
    }

    fn process_start_instant(&self, pid: u32) -> Option<SystemTime> {
        // `/proc/<pid>`'s directory mtime is the process start instant and is
        // world-stat-able even when `/proc/<pid>/exe` is not. Verified equal to
        // the unit's `ActiveEnterTimestamp` on this host, to the nanosecond.
        std::fs::metadata(format!("/proc/{pid}"))
            .ok()?
            .modified()
            .ok()
    }

    fn selected_release_instant(&self) -> Option<SystemTime> {
        // The selector's **symlink** mtime is the repoint instant. Following
        // the link would report the release directory instead, which moves
        // when the release is written and not when it is selected.
        std::fs::symlink_metadata(Path::new(PRODUCT_ROOT).join("current"))
            .ok()?
            .modified()
            .ok()
    }
}

/// The only effect surface a joint move has, executed with `systemctl`.
///
/// Offers `try-restart` only. Systemd's unit policy can still signal or kill
/// the unit, and its job can outlive the bounded `systemctl` client.
pub(crate) struct SystemdReleaseCoherenceEffect<'a> {
    pub(super) custody: &'a ReleaseSyncGuard,
    pub(super) product_root: &'a Path,
    pub(super) selected: &'a str,
    pub(super) systemctl_bin: &'a str,
}

impl ReleaseCoherenceEffect for SystemdReleaseCoherenceEffect<'_> {
    fn try_restart(&mut self, unit: &str) -> Result<(), RestartFailure> {
        self.custody
            .revalidate()
            .map_err(RestartFailure::Uncertain)?;
        let previous = super::systemd_jobs::require_running_after(
            self.systemctl_bin,
            unit,
            SYSTEMCTL_TIMEOUT,
        )?;
        run_systemctl_try_restart_checked(self.systemctl_bin, unit, || {
            self.custody.revalidate()?;
            super::selector::require_selection(self.product_root, self.selected)
        })?;
        self.custody
            .revalidate()
            .map_err(RestartFailure::Uncertain)?;
        super::selector::require_selection(self.product_root, self.selected)
            .map_err(RestartFailure::Uncertain)?;
        let current = super::systemd_jobs::require_running_after(
            self.systemctl_bin,
            unit,
            SYSTEMCTL_TIMEOUT,
        )?;
        if previous.invocation == current.invocation {
            return Err(RestartFailure::Uncertain(
                "systemd invocation did not change after restart request".into(),
            ));
        }
        self.custody
            .revalidate()
            .map_err(RestartFailure::Uncertain)?;
        super::selector::require_selection(self.product_root, self.selected)
            .map_err(RestartFailure::Uncertain)?;
        let view = SystemProductProcessView::with_systemctl(self.systemctl_bin);
        verify_selected_image(self.product_root, self.selected, unit, &view)?;
        if !view.unit_matches_running(unit, &current) {
            return Err(RestartFailure::Uncertain(
                "post-restart PID or invocation changed during image observation".into(),
            ));
        }
        self.custody
            .revalidate()
            .map_err(RestartFailure::Uncertain)?;
        super::selector::require_selection(self.product_root, self.selected)
            .map_err(RestartFailure::Uncertain)
    }

    fn observed_state(&mut self, unit: &str) -> String {
        run_systemctl_observed_state(self.systemctl_bin, unit)
    }
}

pub(super) fn verify_selected_image(
    product_root: &Path,
    selected: &str,
    unit: &str,
    view: &dyn ProductProcessView,
) -> Result<(), RestartFailure> {
    let unknown = || RestartFailure::Uncertain("post-restart selected image is unverified".into());
    let fleet = observe_fleet(
        &product_root.join("current"),
        Some(selected.to_string()),
        view,
    )
    .map_err(|_| unknown())?;
    let member = fleet.members.iter().find(|member| {
        matches!(&member.kind, super::ProductProcessKind::Systemd(name) if name == unit)
    }).ok_or_else(unknown)?;
    if member.active != Some(true)
        || !member.main_pid.is_some_and(|pid| pid != 0)
        || !matches!(
            member.status,
            crate::installed_image::RunningInstallStatus::InstalledActiveRelease
                | crate::installed_image::RunningInstallStatus::InstalledBoth
        )
    {
        return Err(unknown());
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn run_systemctl_try_restart(bin: &str, unit: &str) -> Result<(), RestartFailure> {
    run_systemctl_try_restart_checked(bin, unit, || Ok(()))
}

pub(super) fn run_systemctl_try_restart_checked(
    bin: &str,
    unit: &str,
    revalidate: impl FnOnce() -> Result<(), String>,
) -> Result<(), RestartFailure> {
    super::systemd_jobs::require_idle(bin, unit, SYSTEMCTL_TIMEOUT)?;
    revalidate().map_err(RestartFailure::Uncertain)?;
    let mut command = std::process::Command::new(bin);
    command.args(["--job-mode=fail", "try-restart", "--", unit]);
    let output = bounded_process::run_capture_command(
        &mut command,
        "release-coherence joint move",
        SYSTEMCTL_TIMEOUT,
        bounded_process::DEFAULT_OUTPUT_LIMIT,
        |_| {},
    )
    .map_err(|error| {
        let message = format!("bounded systemctl try-restart failed: {error}");
        match error {
            bounded_process::ProcessSpawnError::BinaryNotFound { .. }
            | bounded_process::ProcessSpawnError::SpawnFailed { .. } => {
                RestartFailure::Refused(message)
            }
            _ => RestartFailure::Uncertain(message),
        }
    })?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            Err(RestartFailure::Uncertain(format!(
                "systemctl exited with {}",
                output.status
            )))
        } else {
            Err(RestartFailure::Uncertain(format!(
                "systemctl exited with {}: {stderr}",
                output.status
            )))
        }
    }
}

pub(super) fn run_systemctl_observed_state(bin: &str, unit: &str) -> String {
    let mut command = std::process::Command::new(bin);
    command.args([
        "show",
        "--property=LoadState",
        "--property=ActiveState",
        "--property=SubState",
        "--property=Job",
        unit,
    ]);
    let output = bounded_process::run_capture_command(
        &mut command,
        "release-coherence unit state",
        SYSTEMCTL_TIMEOUT,
        bounded_process::DEFAULT_OUTPUT_LIMIT,
        |_| {},
    );
    match output {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            let mut parts = Vec::new();
            for line in text.lines() {
                if let Some((key, value)) = line.split_once('=') {
                    parts.push(format!("{key}={value}"));
                }
            }
            if parts.is_empty() {
                "unobserved".to_string()
            } else {
                parts.join(",")
            }
        }
        _ => "unobserved".to_string(),
    }
}

/// The fleet answer the status surface reports.
///
/// No selected release means there is no installed image to disagree with, so
/// the fleet is vacuously coherent: a source-tree run must not be permanently
/// blocked by DT-6. An unreadable observation is `Unknown`, never `Coherent`.
pub(crate) fn observe_release_coherence() -> ReleaseCoherence {
    match super::selector::selected_release_observation(Path::new(PRODUCT_ROOT)) {
        Ok(selected) => {
            let direct = observe_release_coherence_under(
                Path::new(PRODUCT_ROOT),
                selected,
                &SystemProductProcessView::default(),
            );
            if direct == ReleaseCoherence::Unknown
                && let Some(cached) =
                    crate::cascade::root_status_cache::verified_release_coherence()
            {
                return cached;
            }
            direct
        }
        Err(_) => ReleaseCoherence::Unknown,
    }
}

pub(super) fn observe_release_coherence_under(
    product_root: &Path,
    selected: Option<String>,
    view: &dyn ProductProcessView,
) -> ReleaseCoherence {
    let Some(selected) = selected else {
        return ReleaseCoherence::Coherent;
    };
    let release_root = product_root.join("current");
    match observe_fleet(&release_root, Some(selected), view) {
        Ok(fleet) => fleet.aggregate(),
        Err(_) => ReleaseCoherence::Unknown,
    }
}
