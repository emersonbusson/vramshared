// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/install_drain/tests.rs

#![allow(clippy::unwrap_used)]

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::PathBuf;

use super::super::install_lifecycle::NoopLifecycleHost;
use super::systemd::parse_unit;
use super::*;
use crate::operation_progress::OperationProgressV1;

struct Fixture {
    root: PathBuf,
    paths: TransitionPaths,
}

impl Fixture {
    fn new() -> Self {
        let seq = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("ramshared-drain-{}-{seq}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let paths = TransitionPaths::under(&root);
        Self { root, paths }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn signed_pair() -> DrainInputs {
    DrainInputs {
        previous_release: "v1.0.2".to_owned(),
        previous_manifest: "a".repeat(64),
        previous_directory: (2, 3),
        candidate_release: "v1.0.3".to_owned(),
        candidate_manifest: "b".repeat(64),
        sink_path: "/test/sink".into(),
        sink_identity: "c".repeat(64),
        posture: "writeback".to_owned(),
        no_activate: false,
        no_enroll: false,
        enroll_default: false,
    }
}

fn occupied() -> LiveRuntimeState {
    LiveRuntimeState {
        cascade_active: true,
        supervisor_active: true,
        host_gate_active: false,
        health_active: true,
        health_enabled: true,
        boot_enrolled: true,
        active_managed_swap: true,
    }
}

fn unit(active: &str, sub: &str, result: &str, job: &str, pid: u32, birth: u64) -> ObservedUnit {
    parse_unit(
        &format!("LoadState=loaded\nActiveState={active}\nSubState={sub}\nResult={result}\nJob={job}\nControlPID={pid}\n"),
        |_| Some(birth),
    ).unwrap()
}

fn intent() -> Receipt {
    Receipt {
        schema_version: 1,
        boot_id: current_boot_id().unwrap(),
        inputs: signed_pair(),
        prior_runtime: occupied(),
        effective_enrollment: None,
        phase: Phase::Intent,
    }
}

struct FakeStop {
    observations: RefCell<VecDeque<ObservedUnit>>,
    submit_count: Cell<u32>,
    reply: Option<NonZeroU32>,
}

impl FakeStop {
    fn new(observations: impl IntoIterator<Item = ObservedUnit>, job: u32) -> Self {
        Self {
            observations: RefCell::new(observations.into_iter().collect()),
            submit_count: Cell::new(0),
            reply: NonZeroU32::new(job),
        }
    }
}

impl StopHost for FakeStop {
    fn submit(&self) -> Result<NonZeroU32, CustomerError> {
        self.submit_count.set(self.submit_count.get() + 1);
        self.reply
            .ok_or_else(|| recovery_refusal("test: systemd reply lost"))
    }

    fn observe(&self) -> Result<ObservedUnit, CustomerError> {
        Ok(self.observations.borrow_mut().pop_front().unwrap())
    }

    fn no_managed_lifecycle(&self) -> bool {
        true
    }

    fn progress(&self) -> Option<OperationProgressV1> {
        None
    }
}

#[test]
fn lost_reply_never_submits_another_stop() {
    let fixture = Fixture::new();
    let lock = InstallerLock::acquire(&fixture.paths).unwrap();
    let inputs = signed_pair();
    let active = unit("active", "exited", "success", "", 0, 0);
    let initial = FakeStop::new([active], 0);
    assert!(
        lock.prepare(Some(&inputs), &occupied(), &NoopLifecycleHost, &initial)
            .is_err()
    );
    assert_eq!(initial.submit_count.get(), 1);
    assert_eq!(lock.read().unwrap().unwrap().phase, Phase::Intent);
    drop(lock);

    let running = unit("deactivating", "stop", "success", "927", 10, 70);
    let stopped = unit("inactive", "dead", "success", "", 0, 0);
    let resumed = FakeStop::new([running, stopped], 0);
    let lock = InstallerLock::acquire(&fixture.paths).unwrap();
    let off = LiveRuntimeState::default();
    assert!(matches!(
        lock.prepare(Some(&inputs), &off, &NoopLifecycleHost, &resumed).unwrap(),
        DrainOutcome::Deferred(reason) if reason == "STOP_REPLY_NOT_RECORDED"
    ));
    assert!(matches!(
        lock.prepare(Some(&inputs), &off, &NoopLifecycleHost, &resumed).unwrap(),
        DrainOutcome::Drained(prior) if prior == occupied()
    ));
    assert_eq!(resumed.submit_count.get(), 0);
    lock.clear_finished(Some(&inputs), false).unwrap();
    assert!(lock.read().unwrap().is_none());
}

#[test]
fn stale_identity_cannot_adopt_a_stop() {
    let mut receipt = intent();
    let mut wrong = signed_pair();
    wrong.candidate_manifest = "d".repeat(64);
    assert!(receipt.check_identity(&wrong, &receipt.boot_id).is_err());
    assert!(
        receipt
            .check_identity(&signed_pair(), "different-boot")
            .is_err()
    );

    let expected = unit("deactivating", "stop", "success", "10", 90, 80);
    receipt.phase = Phase::Submitted {
        job: NonZeroU32::new(10).unwrap(),
        process: expected.process,
    };
    assert!(matches!(
        receipt.decide_stop(&expected, false),
        Ok(StopDecision::Observe { .. })
    ));
    let wrong_job = unit("deactivating", "stop", "success", "11", 90, 80);
    let recycled_pid = unit("deactivating", "stop", "success", "10", 90, 81);
    assert!(receipt.decide_stop(&wrong_job, false).is_err());
    assert!(receipt.decide_stop(&recycled_pid, false).is_err());
}

#[test]
fn a_quiet_unit_alone_cannot_authorize_apply() {
    let receipt = intent();
    let inactive = unit("inactive", "dead", "success", "", 0, 0);
    let failed = unit("failed", "failed", "exit-code", "", 0, 0);
    let still_active = unit("active", "exited", "success", "", 0, 0);
    assert!(receipt.decide_stop(&inactive, false).is_err());
    assert!(receipt.decide_stop(&failed, true).is_err());
    assert!(receipt.decide_stop(&still_active, true).is_err());
    assert_eq!(
        receipt.decide_stop(&inactive, true).unwrap(),
        StopDecision::Complete
    );
}

#[test]
fn intent_and_job_survive_reopening_the_installer_lock() {
    let fixture = Fixture::new();
    let mut receipt = intent();
    let lock = InstallerLock::acquire(&fixture.paths).unwrap();
    lock.publish_intent(&receipt).unwrap();
    assert!(InstallerLock::acquire(&fixture.paths).is_err());
    drop(lock);
    let lock = InstallerLock::acquire(&fixture.paths).unwrap();
    assert_eq!(lock.read().unwrap(), Some(receipt.clone()));
    receipt.phase = Phase::Submitted {
        job: NonZeroU32::new(311).unwrap(),
        process: None,
    };
    let down_guard = acquire_transition(OperationOwner::Up, &fixture.paths).unwrap();
    lock.publish(&receipt).unwrap();
    drop(down_guard);
    drop(lock);
    let lock = InstallerLock::acquire(&fixture.paths).unwrap();
    assert_eq!(lock.read().unwrap(), Some(receipt));
}

#[test]
fn off_upgrade_creates_no_drain_intent() {
    let fixture = Fixture::new();
    let lock = InstallerLock::acquire(&fixture.paths).unwrap();
    let off = LiveRuntimeState::default();
    let systemd = FakeStop::new([], 0);
    assert!(matches!(
        lock.prepare(Some(&signed_pair()), &off, &NoopLifecycleHost, &systemd)
            .unwrap(),
        DrainOutcome::Unneeded
    ));
    assert!(lock.read().unwrap().is_none());
}

#[test]
fn owned_stop_records_the_control_process_before_accepting_quiescence() {
    let fixture = Fixture::new();
    let lock = InstallerLock::acquire(&fixture.paths).unwrap();
    require_no_drain(&fixture.paths).unwrap();
    let inputs = signed_pair();
    let started = unit("active", "exited", "success", "", 0, 0);
    let draining = unit("deactivating", "stop", "success", "83", 10, 70);
    let stopped = unit("inactive", "dead", "success", "", 0, 0);
    let systemd = FakeStop::new([started, draining, stopped], 83);
    assert!(matches!(
        lock.prepare(Some(&inputs), &occupied(), &NoopLifecycleHost, &systemd).unwrap(),
        DrainOutcome::Drained(prior) if prior == occupied()
    ));
    assert_eq!(systemd.submit_count.get(), 1);
    assert!(require_no_drain(&fixture.paths).is_err());
    assert!(lock.validate_no_prior_install().is_err());
    lock.record_package_plan(true).unwrap();
    lock.record_package_plan(true).unwrap();
    assert!(lock.record_package_plan(false).is_err());
    assert!(lock.clear_finished(Some(&inputs), true).is_err());
    assert_eq!(
        lock.read().unwrap().unwrap().effective_enrollment,
        Some(true)
    );
    lock.clear_finished(Some(&inputs), false).unwrap();
    require_no_drain(&fixture.paths).unwrap();
}

mod receipt_refusals;
