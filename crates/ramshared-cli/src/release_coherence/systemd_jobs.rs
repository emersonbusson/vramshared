// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/release_coherence/systemd_jobs.rs
//! Recheck the target's manager job before submitting a release restart.
use super::RestartFailure;
use crate::bounded_process::{self, ProcessSpawnError};
use std::time::Duration;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct RunningUnit {
    pub(super) pid: u32,
    pub(super) invocation: String,
}

pub(super) fn parse_quiescent_unit(text: &str, expected: &str) -> Option<bool> {
    let mut values = std::collections::BTreeMap::new();
    for line in text.lines() {
        let (key, value) = line.split_once('=')?;
        if !matches!(
            key,
            "Id" | "LoadState" | "ActiveState" | "MainPID" | "ControlPID" | "Job"
        ) || values.insert(key, value).is_some()
        {
            return None;
        }
    }
    if values.get("Id")? != &expected
        || !matches!(*values.get("LoadState")?, "loaded" | "not-found" | "masked")
    {
        return None;
    }
    let active = *values.get("ActiveState")?;
    if !matches!(
        active,
        "active"
            | "inactive"
            | "failed"
            | "activating"
            | "deactivating"
            | "reloading"
            | "refreshing"
            | "maintenance"
    ) {
        return None;
    }
    let job = *values.get("Job")?;
    let processes_idle = if expected.ends_with(".service") {
        values.get("MainPID")?.parse::<u32>().ok()? == 0
            && values.get("ControlPID")?.parse::<u32>().ok()? == 0
    } else if expected.ends_with(".timer") {
        if values.contains_key("MainPID") || values.contains_key("ControlPID") {
            return None;
        }
        true
    } else {
        return None;
    };
    Some(matches!(active, "inactive" | "failed") && processes_idle && job.is_empty())
}

/// Uncooperative activators must also be prevented from starting by systemd.
pub(super) fn parse_masked_or_absent_unit(text: &str, expected: &str) -> Option<bool> {
    let quiescent = parse_quiescent_unit(text, expected)?;
    let load = text
        .lines()
        .find_map(|line| line.strip_prefix("LoadState="))?;
    Some(quiescent && matches!(load, "masked" | "not-found"))
}

/// Observe one exact unit; a failed or malformed query is never quiescent.
pub(crate) fn unit_is_quiescent(unit: &str) -> Option<bool> {
    observe_unit_with("systemctl", unit, Duration::from_secs(5), false)
}

pub(crate) fn unit_is_masked_or_absent(unit: &str) -> Option<bool> {
    observe_unit_with("systemctl", unit, Duration::from_secs(5), true)
}

#[cfg(test)]
pub(super) fn unit_is_quiescent_with(bin: &str, unit: &str, timeout: Duration) -> Option<bool> {
    observe_unit_with(bin, unit, timeout, false)
}

fn observe_unit_with(bin: &str, unit: &str, timeout: Duration, require_mask: bool) -> Option<bool> {
    let mut command = std::process::Command::new(bin);
    command.args([
        "show",
        "--all",
        "--property=Id",
        "--property=LoadState",
        "--property=ActiveState",
        "--property=MainPID",
        "--property=ControlPID",
        "--property=Job",
        "--no-pager",
        "--",
        unit,
    ]);
    let output = bounded_process::run_capture_command(
        &mut command,
        "preparation unit observation",
        timeout,
        4096,
        |_| {},
    )
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = std::str::from_utf8(&output.stdout).ok()?;
    if require_mask {
        parse_masked_or_absent_unit(text, unit)
    } else {
        parse_quiescent_unit(text, unit)
    }
}

/// A missing, malformed or pending job observation cannot authorize a new
/// effect. This is a point-in-time check, not durable or exclusive custody.
pub(super) fn require_idle(bin: &str, unit: &str, timeout: Duration) -> Result<(), RestartFailure> {
    let mut command = std::process::Command::new(bin);
    command.args(["show", "--all", "--property=Job", "--no-pager", "--", unit]);
    let output = bounded_process::run_capture_command(
        &mut command,
        "release-coherence job observation",
        timeout,
        1024,
        |_| {},
    )
    .map_err(|error| {
        let reason = format!("systemd job observation failed: {error}");
        match error {
            ProcessSpawnError::BinaryNotFound { .. } | ProcessSpawnError::SpawnFailed { .. } => {
                RestartFailure::Refused(reason)
            }
            _ => RestartFailure::Uncertain(reason),
        }
    })?;
    if !output.status.success() {
        return Err(RestartFailure::Uncertain(
            "systemd job observation exited unsuccessfully".into(),
        ));
    }
    let text = std::str::from_utf8(&output.stdout)
        .map_err(|_| RestartFailure::Uncertain("systemd job observation is not UTF-8".into()))?;
    let mut lines = text.lines();
    if lines.next() != Some("Job=") || lines.next().is_some() {
        return Err(RestartFailure::Uncertain(
            "systemd job is pending or observation is malformed".into(),
        ));
    }
    Ok(())
}

/// A successful try-restart can be a no-op when the unit stopped meanwhile.
/// This only checks its postcondition; it does not prove executable identity.
pub(super) fn require_running_after(
    bin: &str,
    unit: &str,
    timeout: Duration,
) -> Result<RunningUnit, RestartFailure> {
    let mut command = std::process::Command::new(bin);
    command.args([
        "show",
        "--all",
        "--property=ActiveState",
        "--property=MainPID",
        "--property=Job",
        "--property=InvocationID",
        "--no-pager",
        "--",
        unit,
    ]);
    let uncertain = || RestartFailure::Uncertain("post-restart unit state is unverified".into());
    let output = bounded_process::run_capture_command(
        &mut command,
        "release-coherence post-restart observation",
        timeout,
        1024,
        |_| {},
    )
    .map_err(|_| uncertain())?;
    if !output.status.success() {
        return Err(uncertain());
    }
    let text = std::str::from_utf8(&output.stdout).map_err(|_| uncertain())?;
    let (mut active, mut pid, mut job, mut invocation) = (None, None, None, None);
    for line in text.lines() {
        let (key, value) = line.split_once('=').ok_or_else(uncertain)?;
        let slot = match key {
            "ActiveState" => &mut active,
            "MainPID" => &mut pid,
            "Job" => &mut job,
            "InvocationID" => &mut invocation,
            _ => return Err(uncertain()),
        };
        if slot.replace(value).is_some() {
            return Err(uncertain());
        }
    }
    let pid = pid
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0)
        .ok_or_else(uncertain)?;
    if active != Some("active") || job != Some("") {
        return Err(uncertain());
    }
    let id = invocation.ok_or_else(uncertain)?;
    if id.len() != 32
        || !id.bytes().all(|byte| byte.is_ascii_hexdigit())
        || id.bytes().all(|byte| byte == b'0')
    {
        return Err(uncertain());
    }
    Ok(RunningUnit {
        pid,
        invocation: id.to_ascii_lowercase(),
    })
}
