// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/install_drain/systemd.rs

//! Systemd owns the stop and its control process. Installer observation carries
//! no cancellation authority.

use std::num::NonZeroU32;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::super::install_fs::{fs_error, read_bounded_regular_at};
use super::super::transition_custody::open_directory_no_follow;
use super::super::{CustomerError, CustomerReason, CustomerState};
use crate::bounded_process::run_capture_command;
use crate::cascade::boot_policy_effect::CASCADE_UNIT;
use crate::operation_progress::{
    MAX_OPERATION_PROGRESS_BYTES, OperationKind, OperationProgressV1,
    SYSTEM_OPERATION_PROGRESS_PATH, parse_live_operation_progress, read_proc_start_ticks,
    unix_epoch_ms,
};

const JOB_PREFIX: &str = "/org/freedesktop/systemd1/job/";
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::customer) struct StopProcess {
    pub(super) pid: NonZeroU32,
    pub(super) start_ticks: u64,
}

#[derive(Debug)]
pub(in crate::customer) struct ObservedUnit {
    active: String,
    sub: String,
    result: String,
    pub(super) job: Option<NonZeroU32>,
    control_pid: u32,
    pub(super) process: Option<StopProcess>,
}

impl ObservedUnit {
    pub(super) fn admits_stop(&self) -> bool {
        self.active == "active"
            && self.sub == "exited"
            && self.job.is_none()
            && self.control_pid == 0
    }

    pub(super) fn successfully_inactive(&self) -> bool {
        self.active == "inactive"
            && self.sub == "dead"
            && self.result == "success"
            && self.job.is_none()
            && self.control_pid == 0
    }

    pub(super) fn is_stop_job(&self, expected: NonZeroU32) -> bool {
        self.job == Some(expected)
            && matches!(
                (self.active.as_str(), self.sub.as_str()),
                ("active", "exited") | ("deactivating", "stop")
            )
    }
}

pub(in crate::customer) trait StopHost {
    fn submit(&self) -> Result<NonZeroU32, CustomerError>;
    fn observe(&self) -> Result<ObservedUnit, CustomerError>;
    fn no_managed_lifecycle(&self) -> bool;
    fn progress(&self) -> Option<OperationProgressV1>;
    fn observation_window(&self) -> Duration {
        Duration::from_secs(180)
    }
}

pub(in crate::customer) struct SystemStop<'a> {
    systemctl: &'a Path,
    unit: &'a str,
    scheduled: bool,
}

impl<'a> SystemStop<'a> {
    pub(in crate::customer) fn new(systemctl: &'a Path) -> Self {
        Self {
            systemctl,
            unit: CASCADE_UNIT,
            scheduled: false,
        }
    }

    pub(in crate::customer) fn with_scheduled_window(mut self, scheduled: bool) -> Self {
        self.scheduled = scheduled;
        self
    }
}

impl StopHost for SystemStop<'_> {
    fn observation_window(&self) -> Duration {
        Duration::from_secs(if self.scheduled { 540 } else { 180 })
    }

    fn submit(&self) -> Result<NonZeroU32, CustomerError> {
        let mut cmd = Command::new("/usr/bin/busctl");
        cmd.args([
            "--system",
            "--timeout=4s",
            "--json=short",
            "call",
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
            "StopUnit",
            "ss",
            self.unit,
            "fail",
        ]);
        let stdout = capture(&mut cmd, "submit systemd StopUnit")?;
        job_from_reply(&stdout)
            .ok_or_else(|| unavailable("invalid StopUnit reply; submission is unresolved"))
    }

    fn observe(&self) -> Result<ObservedUnit, CustomerError> {
        let mut cmd = Command::new(self.systemctl);
        cmd.args([
            "show",
            "--property=LoadState,ActiveState,SubState,Result,Job,ControlPID",
            "--",
            self.unit,
        ]);
        let stdout = capture(&mut cmd, "observe systemd stop")?;
        parse_unit(&stdout, |pid| {
            read_proc_start_ticks(Path::new("/proc"), pid)
        })
        .ok_or_else(|| unavailable("invalid systemd stop observation"))
    }

    fn no_managed_lifecycle(&self) -> bool {
        matches!(crate::cascade::evaluate_live_down_plan(), Ok(None))
    }

    fn progress(&self) -> Option<OperationProgressV1> {
        let path = Path::new(SYSTEM_OPERATION_PROGRESS_PATH);
        let directory = open_directory_no_follow(path.parent()?, false).ok()??;
        let bytes =
            read_bounded_regular_at(&directory, path.file_name()?, MAX_OPERATION_PROGRESS_BYTES)?;
        parse_live_operation_progress(&bytes, Path::new("/proc"), unix_epoch_ms())
    }
}

fn unavailable(detail: impl Into<String>) -> CustomerError {
    fs_error(
        CustomerReason::ObservationUnavailable,
        detail,
        CustomerState::Active,
    )
}

fn capture(cmd: &mut Command, label: &'static str) -> Result<String, CustomerError> {
    // Force the local manager and exclude inherited D-Bus endpoint overrides.
    cmd.env_remove("DBUS_SYSTEM_BUS_ADDRESS")
        .env_remove("SYSTEMD_BUS_ADDRESS")
        .env("LC_ALL", "C")
        .env("SYSTEMD_COLORS", "0");
    let output = run_capture_command(cmd, label, QUERY_TIMEOUT, 4096, |_| {})
        .map_err(|error| unavailable(format!("{label}: {error}")))?;
    String::from_utf8(output.stdout)
        .map_err(|error| unavailable(format!("{label}: invalid UTF-8: {error}")))
}

fn job_from_reply(body: &str) -> Option<NonZeroU32> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct JobReply {
        #[serde(rename = "type")]
        signature: String,
        data: [String; 1],
    }
    let reply: JobReply = serde_json::from_str(body).ok()?;
    if reply.signature != "o" {
        return None;
    }
    reply.data[0].strip_prefix(JOB_PREFIX)?.parse().ok()
}

pub(super) fn parse_unit(
    body: &str,
    read_start: impl FnOnce(u32) -> Option<u64>,
) -> Option<ObservedUnit> {
    let mut fields = std::collections::BTreeMap::new();
    for line in body.lines() {
        let (key, value) = line.split_once('=')?;
        if fields.insert(key, value).is_some() {
            return None;
        }
    }
    if *fields.get("LoadState")? != "loaded" {
        return None;
    }
    let active = fields.get("ActiveState")?.to_string();
    let sub = fields.get("SubState")?.to_string();
    let result = fields.get("Result")?.to_string();
    let job = match *fields.get("Job")? {
        "" => None,
        id => Some(id.parse::<NonZeroU32>().ok()?),
    };
    let control_pid = fields.get("ControlPID")?.parse::<u32>().ok()?;
    let process = NonZeroU32::new(control_pid).and_then(|pid| {
        Some(StopProcess {
            pid,
            start_ticks: read_start(control_pid)?,
        })
    });
    Some(ObservedUnit {
        active,
        sub,
        result,
        job,
        control_pid,
        process,
    })
}

pub(super) struct WaitForProgress {
    started: Instant,
    last_advance: Instant,
    mark: Option<(String, (u32, u64))>,
}

impl WaitForProgress {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            started: now,
            last_advance: now,
            mark: None,
        }
    }

    pub(super) fn observe<'a>(
        &mut self,
        now: Instant,
        process: StopProcess,
        record: Option<&'a OperationProgressV1>,
    ) -> Option<&'a OperationProgressV1> {
        let record = record.filter(|record| {
            record.kind == OperationKind::Teardown
                && record.owner_pid == process.pid.get()
                && record.owner_start_ticks == process.start_ticks
        })?;
        let point = (record.step_index, record.completed_bytes);
        match self.mark.as_mut() {
            None => self.mark = Some((record.operation_id.clone(), point)),
            Some((id, prior)) if *id == record.operation_id && point > *prior => {
                *prior = point;
                self.last_advance = now;
            }
            Some((id, prior)) if *id == record.operation_id && point == *prior => {}
            Some(_) => return None,
        }
        Some(record)
    }

    pub(super) fn expired(&self, now: Instant, max: Duration, stall: Duration) -> bool {
        now.duration_since(self.started) >= max || now.duration_since(self.last_advance) >= stall
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn only_the_scheduled_updater_extends_the_hard_drain_observation_window() {
        let normal = SystemStop::new(Path::new("/usr/bin/true"));
        assert_eq!(normal.observation_window(), Duration::from_secs(180));
        let scheduled = normal.with_scheduled_window(true);
        assert_eq!(scheduled.observation_window(), Duration::from_secs(540));
    }

    #[test]
    fn parses_the_exact_systemd_job_reply_and_unit_state() {
        let reply = r#"{"type":"o","data":["/org/freedesktop/systemd1/job/14491"]}"#;
        assert_eq!(job_from_reply(reply).unwrap().get(), 14491);
        assert!(job_from_reply(&reply.replace("14491", "0")).is_none());
        assert!(job_from_reply(&reply.replace("systemd1/job", "systemd1/unit")).is_none());
        assert!(job_from_reply(&reply.replace("\"o\"", "\"s\"")).is_none());
        let props = "Result=success\nLoadState=loaded\nActiveState=inactive\nSubState=dead\nJob=\nControlPID=0\n";
        assert!(parse_unit(props, |_| None).unwrap().successfully_inactive());
        assert!(parse_unit("Result=success\n", |_| None).is_none());
        assert!(parse_unit(&format!("{props}Job=10\n"), |_| None).is_none());
        assert!(
            parse_unit(
                &props.replace("LoadState=loaded", "LoadState=not-found"),
                |_| None
            )
            .is_none()
        );
    }

    #[test]
    fn systemd_observation_refuses_an_unusable_successful_reply() {
        let adapter = SystemStop::new(Path::new("/usr/bin/true"));
        let error = adapter.observe().unwrap_err();
        assert_eq!(error.reason, CustomerReason::ObservationUnavailable);
        assert_eq!(error.state, CustomerState::Active);
        assert_eq!(error.detail, "invalid systemd stop observation");
    }

    #[test]
    fn command_capture_refuses_failed_or_undecodable_output() {
        let mut failed = Command::new("/usr/bin/false");
        let error = capture(&mut failed, "fixture command").unwrap_err();
        assert_eq!(error.reason, CustomerReason::ObservationUnavailable);
        assert_eq!(error.state, CustomerState::Active);
        assert!(error.detail.starts_with("fixture command: "));

        let mut invalid = Command::new("/usr/bin/printf");
        invalid.arg(r"\377");
        let error = capture(&mut invalid, "fixture command").unwrap_err();
        assert_eq!(error.reason, CustomerReason::ObservationUnavailable);
        assert_eq!(error.state, CustomerState::Active);
        assert!(error.detail.starts_with("fixture command: invalid UTF-8: "));
    }

    #[test]
    fn only_owned_byte_or_step_advance_refreshes_stall_deadline() {
        let start = Instant::now();
        let at = |seconds| start + Duration::from_secs(seconds);
        let control = StopProcess {
            pid: NonZeroU32::new(9).unwrap(),
            start_ticks: 23,
        };
        let max = Duration::from_secs(160);
        let stall = Duration::from_secs(40);
        let mut wait = WaitForProgress::new(start);
        let mut progress = OperationProgressV1 {
            schema_version: 1,
            operation_id: "ordered-down".to_owned(),
            kind: OperationKind::Teardown,
            phase: "swapoff".to_owned(),
            step_index: 1,
            total_steps: 2,
            target: None,
            completed_bytes: 8,
            total_bytes: 64,
            throughput_mib_per_s: None,
            started_at_unix_ms: 1,
            updated_at_unix_ms: 1,
            ttl_ms: 500,
            owner_pid: 9,
            owner_start_ticks: 23,
            upgrade: None,
            package_files: None,
        };
        wait.observe(at(1), control, Some(&progress));
        progress.updated_at_unix_ms = 10;
        wait.observe(at(20), control, Some(&progress));
        assert!(wait.expired(at(40), max, stall));

        progress.completed_bytes = 20;
        wait.observe(at(38), control, Some(&progress));
        assert!(!wait.expired(at(76), max, stall));
        progress.owner_start_ticks += 1;
        progress.completed_bytes = 64;
        wait.observe(at(76), control, Some(&progress));
        assert!(wait.expired(at(78), max, stall));

        progress.owner_start_ticks -= 1;
        progress.step_index += 1;
        progress.completed_bytes = 0;
        wait.observe(at(77), control, Some(&progress));
        assert!(!wait.expired(at(116), max, stall));
        progress.completed_bytes = 8;
        wait.observe(at(116), control, Some(&progress));
        progress.completed_bytes = 16;
        wait.observe(at(153), control, Some(&progress));
        assert!(wait.expired(at(160), max, stall));
    }

    #[test]
    #[ignore = "requires root and a live systemd manager; uses a disposable service"]
    fn native_stop_job_outlives_the_request_client() {
        use std::io::Write;
        use std::path::PathBuf;

        struct RuntimeUnit {
            name: String,
            path: PathBuf,
        }
        impl Drop for RuntimeUnit {
            fn drop(&mut self) {
                let mut stop = Command::new("/usr/bin/systemctl");
                stop.args(["stop", "--", &self.name]);
                let _ = capture(&mut stop, "remove transient installer test unit");
                let _ = std::fs::remove_file(&self.path);
            }
        }

        let name = format!(
            "ramshared-installer-stop-proof-{}.service",
            std::process::id()
        );
        let path = Path::new("/run/systemd/system").join(&name);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let unit = RuntimeUnit { name, path };
        // Keep the definition available after the unit is garbage-collected.
        file.write_all(b"[Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart=/usr/bin/true\nExecStop=/usr/bin/sleep 3\n").unwrap();
        drop(file);
        let mut start = Command::new("/usr/bin/systemctl");
        start.args(["start", "--", &unit.name]);
        capture(&mut start, "start disposable installer test unit").unwrap();
        let host = SystemStop {
            scheduled: false,
            systemctl: Path::new("/usr/bin/systemctl"),
            unit: &unit.name,
        };
        let deadline = Instant::now() + Duration::from_secs(8);
        let wait_until = |state: &dyn Fn(&ObservedUnit) -> bool| {
            loop {
                let observed = host.observe().unwrap();
                if state(&observed) {
                    break observed;
                }
                assert!(Instant::now() < deadline, "{observed:?}");
                std::thread::sleep(Duration::from_millis(50));
            }
        };
        assert!(wait_until(&ObservedUnit::admits_stop).job.is_none());
        let job = host.submit().unwrap();
        let draining =
            wait_until(&|observed| observed.is_stop_job(job) && observed.process.is_some());
        assert_eq!(draining.job, Some(job));
        assert!(
            wait_until(&ObservedUnit::successfully_inactive)
                .process
                .is_none()
        );
    }
}
