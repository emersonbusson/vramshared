// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/release_coherence/tests.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

pub(super) use super::*;

pub(super) use crate::installed_image::{
    ProcessInstallOrigin, running_install_status, running_install_status_from_metadata,
};

pub(super) use std::fs;

pub(super) fn installed() -> RunningInstallStatus {
    RunningInstallStatus::InstalledActiveRelease
}

pub(super) fn pending() -> RunningInstallStatus {
    RunningInstallStatus::UpdatePendingActiveRelease
}

pub(super) fn member(
    kind: ProductProcessKind,
    status: RunningInstallStatus,
    active: Option<bool>,
) -> ProductProcess {
    ProductProcess {
        kind,
        main_pid: Some(1),
        executable: PathBuf::from("/opt/ramshared/current/bin/ramshared"),
        status,
        active,
    }
}

/// A unit that is up but holds no main process: what a completed
/// `Type=oneshot` `RemainAfterExit=yes` unit reports (`MainPID=0`).
pub(super) fn member_without_process(
    kind: ProductProcessKind,
    status: RunningInstallStatus,
    active: Option<bool>,
) -> ProductProcess {
    ProductProcess {
        main_pid: Some(0),
        executable: PathBuf::from("/opt/ramshared/current/scripts/safety/ramshared-host-gate.sh"),
        ..member(kind, status, active)
    }
}

pub(super) fn write_unit(root: &Path, name: &str, body_template: &str) {
    let dir = root.join("systemd");
    std::fs::create_dir_all(&dir).expect("create systemd dir");
    let display = root.to_string_lossy();
    std::fs::write(dir.join(name), body_template.replace("@ROOT@", &display)).expect("write unit");
}

/// The set is whatever the selected release payload declares. Growing the
/// payload grows the set; a constant wearing a derivation's clothes would
/// not.
#[test]
fn product_process_set_follows_the_selected_release_payload() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-product-set-payload-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("bin")).expect("create bin");
    std::fs::write(root.join("bin/ramshared"), b"cli").expect("write cli");

    assert_eq!(product_process_set(&root).unwrap(), Vec::new());

    write_unit(
        &root,
        "ramshared-supervisor.service",
        "[Service]\nExecStart=@ROOT@/bin/ramshared supervise\n",
    );
    let set = product_process_set(&root).unwrap();
    assert_eq!(set.len(), 1);
    assert_eq!(set[0].unit, "ramshared-supervisor.service");
    assert_eq!(
        set[0].exec_paths,
        vec![root.join("bin/ramshared").canonicalize().unwrap()]
    );

    write_unit(
        &root,
        "ramshared-cascade-health.service",
        "[Service]\nExecStart=@ROOT@/bin/ramshared monitor\n",
    );
    let set = product_process_set(&root).unwrap();
    assert_eq!(set.len(), 2);
    let health = set
        .iter()
        .find(|entry| entry.unit == "ramshared-cascade-health.service")
        .expect("second unit declared");
    assert_eq!(
        health.exec_paths,
        vec![root.join("bin/ramshared").canonicalize().unwrap()]
    );

    std::fs::remove_dir_all(&root).expect("remove product set fixture");
}

/// A unit that executes something outside the selected release is not a
/// product process of that release. Inert staging units fail this way.
#[test]
fn product_process_set_refuses_an_execstart_outside_the_release() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-product-set-outside-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("bin")).expect("create bin");
    std::fs::write(root.join("bin/ramshared"), b"cli").expect("write cli");

    write_unit(
        &root,
        "ramsharedd.service",
        "[Service]\nExecStartPre=@ROOT@/bin/ramshared prepare\nExecStart=/usr/bin/false\n",
    );
    write_unit(
        &root,
        "ramshared-cascade.service",
        "[Service]\nExecStart=@ROOT@/bin/ramshared boot\nExecStop=@ROOT@/bin/ramshared down\n",
    );
    write_unit(
        &root,
        "ramshared-host-gate.service",
        "[Service]\nExecStartPre=-@ROOT@/bin/ramshared ensure-origin\nExecStart=@ROOT@/scripts/safety/ramshared-host-gate.sh\n",
    );

    let mut set = product_process_set(&root).unwrap();
    set.sort_by(|left, right| left.unit.cmp(&right.unit));
    let units: Vec<&str> = set.iter().map(|entry| entry.unit.as_str()).collect();
    assert_eq!(
        units,
        vec!["ramshared-cascade.service", "ramshared-host-gate.service"]
    );
    // ExecStartPre counts; the inert staging unit does not.
    assert_eq!(set[1].exec_paths.len(), 2);
    assert_eq!(
        set[1].exec_paths[0],
        root.join("scripts/safety/ramshared-host-gate.sh"),
        "identity classification must refer to ExecStart before preparatory commands"
    );
    let reset = format!(
        "ExecStart={}/bin/ramshared\nExecStart=\nExecStart=/usr/bin/false\n",
        root.display()
    );
    assert!(
        declared_exec_paths(&root, &reset).is_empty(),
        "a reset removes an earlier release-owned ExecStart"
    );

    std::fs::remove_dir_all(&root).expect("remove product set fixture");
}

#[test]
fn fleet_coherence_is_stale_until_every_product_process_matches() {
    let coherent = ReleaseFleet {
        selected_release: Some("v0.15.0".into()),
        members: vec![
            member(
                ProductProcessKind::Systemd("a.service".into()),
                installed(),
                Some(true),
            ),
            member(ProductProcessKind::Client, installed(), Some(true)),
        ],
    };
    assert_eq!(coherent.aggregate(), ReleaseCoherence::Coherent);

    let stale = ReleaseFleet {
        selected_release: Some("v0.15.0".into()),
        members: vec![
            member(
                ProductProcessKind::Systemd("a.service".into()),
                installed(),
                Some(true),
            ),
            member(
                ProductProcessKind::Systemd("b.service".into()),
                pending(),
                Some(true),
            ),
        ],
    };
    assert_eq!(stale.aggregate(), ReleaseCoherence::Stale);
}

/// One known-stale member is the news. Reporting the fleet as merely
/// "unknown" because a second member is unclassifiable is how nobody acts.
#[test]
fn fleet_coherence_prefers_stale_over_unknown() {
    let fleet = ReleaseFleet {
        selected_release: Some("v0.15.0".into()),
        members: vec![
            member(
                ProductProcessKind::Systemd("a.service".into()),
                RunningInstallStatus::Unknown,
                Some(true),
            ),
            member(
                ProductProcessKind::Systemd("b.service".into()),
                pending(),
                Some(true),
            ),
        ],
    };
    assert_eq!(fleet.aggregate(), ReleaseCoherence::Stale);
}

#[test]
fn fleet_coherence_is_unknown_when_nothing_can_be_proved() {
    let fleet = ReleaseFleet {
        selected_release: Some("v0.15.0".into()),
        members: vec![
            member(
                ProductProcessKind::Systemd("a.service".into()),
                RunningInstallStatus::Unknown,
                Some(true),
            ),
            member(
                ProductProcessKind::Systemd("b.service".into()),
                RunningInstallStatus::Unknown,
                Some(true),
            ),
        ],
    };
    assert_eq!(fleet.aggregate(), ReleaseCoherence::Unknown);

    let empty = ReleaseFleet {
        selected_release: Some("v0.15.0".into()),
        members: Vec::new(),
    };
    assert_eq!(empty.aggregate(), ReleaseCoherence::Coherent);
}

/// A unit the operator left off holds no running image that could
/// disagree with the selected release. Its unclassifiable status is not
/// news and must not stamp the fleet `Unknown` forever — which is exactly
/// what the boot policy ("nothing comes up on a WSL2 restart by itself")
/// would otherwise guarantee.
#[test]
fn an_inactive_product_unit_does_not_drive_the_fleet_answer() {
    let fleet = ReleaseFleet {
        selected_release: Some("v0.15.0".into()),
        members: vec![
            member(
                ProductProcessKind::Systemd("ramshared-cascade-health.service".into()),
                installed(),
                Some(true),
            ),
            member(
                ProductProcessKind::Systemd("ramshared-cascade.service".into()),
                RunningInstallStatus::Unknown,
                Some(false),
            ),
        ],
    };
    assert_eq!(fleet.aggregate(), ReleaseCoherence::Coherent);
}

/// A completed `Type=oneshot` `RemainAfterExit=yes` unit is `active` with
/// `MainPID=0`. It is up, but nothing is executing, so it cannot be holding
/// bytes from another release. Its unclassifiable status must not stamp the
/// fleet `Unknown` forever — which is the standing shape of the host gate.
#[test]
fn a_completed_oneshot_does_not_drive_the_fleet_answer() {
    let fleet = ReleaseFleet {
        selected_release: Some("v0.15.0".into()),
        members: vec![
            member(
                ProductProcessKind::Systemd("ramshared-cascade-health.service".into()),
                installed(),
                Some(true),
            ),
            member_without_process(
                ProductProcessKind::Systemd("ramshared-host-gate.service".into()),
                RunningInstallStatus::Unknown,
                Some(true),
            ),
        ],
    };
    assert_eq!(fleet.aggregate(), ReleaseCoherence::Coherent);
}

/// A oneshot that is up with no process is not proof that a *running* unit
/// is fine. Precedence is unchanged.
#[test]
fn a_completed_oneshot_cannot_hide_a_running_stale_one() {
    let fleet = ReleaseFleet {
        selected_release: Some("v0.15.0".into()),
        members: vec![
            member_without_process(
                ProductProcessKind::Systemd("ramshared-host-gate.service".into()),
                RunningInstallStatus::Unknown,
                Some(true),
            ),
            member(
                ProductProcessKind::Systemd("ramshared-supervisor.service".into()),
                pending(),
                Some(true),
            ),
        ],
    };
    assert_eq!(fleet.aggregate(), ReleaseCoherence::Stale);
}

/// `None` is unobserved, never "off". Reading it as off is how a stale or
/// unclassifiable running unit gets silently skipped.
#[test]
fn an_unobserved_unit_still_drives_the_fleet_answer() {
    let fleet = ReleaseFleet {
        selected_release: Some("v0.15.0".into()),
        members: vec![
            member(
                ProductProcessKind::Systemd("ramshared-cascade-health.service".into()),
                installed(),
                Some(true),
            ),
            member(
                ProductProcessKind::Systemd("ramshared-supervisor.service".into()),
                RunningInstallStatus::Unknown,
                None,
            ),
        ],
    };
    assert_eq!(fleet.aggregate(), ReleaseCoherence::Unknown);

    let stale_when_unobserved = ReleaseFleet {
        selected_release: Some("v0.15.0".into()),
        members: vec![
            member(
                ProductProcessKind::Systemd("ramshared-cascade-health.service".into()),
                installed(),
                Some(true),
            ),
            member(
                ProductProcessKind::Systemd("ramshared-supervisor.service".into()),
                pending(),
                None,
            ),
        ],
    };
    assert_eq!(stale_when_unobserved.aggregate(), ReleaseCoherence::Stale);
}

/// Skipping an inactive unit must not skip a running stale one. Precedence
/// is unchanged: `Stale` is still the news.
#[test]
fn an_inactive_unit_cannot_hide_a_running_stale_one() {
    let fleet = ReleaseFleet {
        selected_release: Some("v0.15.0".into()),
        members: vec![
            member(
                ProductProcessKind::Systemd("ramshared-cascade.service".into()),
                RunningInstallStatus::Unknown,
                Some(false),
            ),
            member(
                ProductProcessKind::Systemd("ramshared-supervisor.service".into()),
                pending(),
                Some(true),
            ),
        ],
    };
    assert_eq!(fleet.aggregate(), ReleaseCoherence::Stale);
}

/// The three-generation defect classifies as `NotInstalled`, not
/// `UpdatePending*`: a process that kept running after `current` moved
/// names a release directory that is no longer selected, so its path
/// matches neither install. Reporting that as "unknown" would bury the
/// exact defect this campaign closes. A **product unit** in that state is
/// stale.
#[test]
fn old_release_product_unit_is_stale_not_unknown() {
    let fleet = ReleaseFleet {
        selected_release: Some("v0.15.0".into()),
        members: vec![
            member(
                ProductProcessKind::Systemd("ramshared-supervisor.service".into()),
                RunningInstallStatus::NotInstalled,
                Some(true),
            ),
            member(
                ProductProcessKind::Systemd("ramshared-cascade-health.service".into()),
                RunningInstallStatus::Unknown,
                Some(true),
            ),
        ],
    };
    assert_eq!(fleet.aggregate(), ReleaseCoherence::Stale);

    let plan = plan_joint_move(&fleet);
    assert_eq!(plan.restart, vec!["ramshared-supervisor.service"]);
}

/// A client is observed and refreshes itself; it never drives the fleet
/// answer. One source-tree `top` must not stamp an installed fleet stale.
#[test]
fn a_non_installed_client_does_not_drive_the_fleet_answer() {
    let fleet = ReleaseFleet {
        selected_release: Some("v0.15.0".into()),
        members: vec![
            member(
                ProductProcessKind::Systemd("a.service".into()),
                installed(),
                Some(true),
            ),
            member(
                ProductProcessKind::Client,
                RunningInstallStatus::NotInstalled,
                Some(true),
            ),
            member(
                ProductProcessKind::Client,
                RunningInstallStatus::Unknown,
                Some(true),
            ),
        ],
    };
    assert_eq!(fleet.aggregate(), ReleaseCoherence::Coherent);

    let plan = plan_joint_move(&fleet);
    assert!(plan.restart.is_empty(), "a client is never moved");
}

#[test]
fn joint_move_restarts_stale_active_units_and_never_starts_inactive_ones() {
    let fleet = ReleaseFleet {
        selected_release: Some("v0.15.0".into()),
        members: vec![
            // Active and stale -> restarted.
            member(
                ProductProcessKind::Systemd("stale.service".into()),
                pending(),
                Some(true),
            ),
            // Inactive and stale -> left alone. This is the operator's
            // "do not come up by itself" rule.
            member(
                ProductProcessKind::Systemd("off.service".into()),
                pending(),
                Some(false),
            ),
            // Active and current -> untouched.
            member(
                ProductProcessKind::Systemd("current.service".into()),
                installed(),
                Some(true),
            ),
            // A client is never in the move.
            member(ProductProcessKind::Client, pending(), Some(true)),
            // Unclassifiable -> left alone, never a guess.
            member(
                ProductProcessKind::Systemd("unknown.service".into()),
                RunningInstallStatus::Unknown,
                Some(true),
            ),
            // Activity never observed -> nothing is claimed either way.
            member(
                ProductProcessKind::Systemd("unobserved.service".into()),
                pending(),
                None,
            ),
        ],
    };
    let plan = plan_joint_move(&fleet);
    assert_eq!(plan.restart, vec!["stale.service"]);
    assert_eq!(plan.already_current, vec!["current.service"]);
    assert_eq!(plan.left_inactive, vec!["off.service"]);
    // An active unit whose status is neither known-stale nor installed is
    // still named. Omitting it would make "no line" indistinguishable from
    // "the move never saw this unit", which is the defect this bucket
    // exists to close.
    assert_eq!(plan.left_unjudged, vec!["unknown.service"]);
    assert_eq!(plan.unobserved, vec!["unobserved.service"]);

    let mut effect = RecordingEffect::default();
    let report = apply_joint_move(&plan, &mut effect);
    assert_eq!(effect.restarted, vec!["stale.service".to_string()]);
    assert!(
        effect.started.is_empty(),
        "a joint move never starts a unit"
    );
    assert!(effect.killed.is_empty(), "a joint move never kills a unit");
    assert_eq!(
        report.0,
        vec![
            ("current.service".to_string(), MoveOutcome::AlreadyCurrent),
            ("off.service".to_string(), MoveOutcome::LeftInactive),
            ("unknown.service".to_string(), MoveOutcome::LeftUnjudged),
            ("unobserved.service".to_string(), MoveOutcome::Unobserved),
            ("stale.service".to_string(), MoveOutcome::Restarted),
        ]
    );
}

/// A completed `Type=oneshot` `RemainAfterExit=yes` unit is `active` with
/// `MainPID=0`. There is nothing to move and nothing to claim, so the move
/// leaves it alone — but it is still named, so it cannot vanish from the
/// report (the report contract: every planned unit appears exactly once).
#[test]
fn joint_move_names_a_completed_oneshot_instead_of_dropping_it() {
    let fleet = ReleaseFleet {
        selected_release: Some("v0.15.0".into()),
        members: vec![
            member_without_process(
                ProductProcessKind::Systemd("ramshared-host-gate.service".into()),
                pending(),
                Some(true),
            ),
            member(
                ProductProcessKind::Systemd("stale.service".into()),
                pending(),
                Some(true),
            ),
        ],
    };
    let plan = plan_joint_move(&fleet);
    assert_eq!(plan.left_no_image, vec!["ramshared-host-gate.service"]);
    assert_eq!(plan.restart, vec!["stale.service"]);
    assert!(plan.left_unjudged.is_empty());
    assert!(plan.already_current.is_empty());
    assert!(plan.left_inactive.is_empty());
    assert!(plan.unobserved.is_empty());

    let mut effect = RecordingEffect::default();
    let report = apply_joint_move(&plan, &mut effect);
    assert_eq!(effect.restarted, vec!["stale.service".to_string()]);
    assert_eq!(
        report.0,
        vec![
            (
                "ramshared-host-gate.service".to_string(),
                MoveOutcome::LeftNoImage
            ),
            ("stale.service".to_string(), MoveOutcome::Restarted),
        ]
    );
}

/// A replay of an empty plan is a no-op.
#[test]
fn joint_move_replay_restarts_nothing() {
    let fleet = ReleaseFleet {
        selected_release: Some("v0.15.0".into()),
        members: vec![
            member(
                ProductProcessKind::Systemd("a.service".into()),
                installed(),
                Some(true),
            ),
            member(
                ProductProcessKind::Systemd("b.service".into()),
                installed(),
                Some(true),
            ),
        ],
    };
    let plan = plan_joint_move(&fleet);
    assert!(plan.restart.is_empty());

    let mut first = RecordingEffect::default();
    apply_joint_move(&plan, &mut first);
    let mut second = RecordingEffect::default();
    apply_joint_move(&plan, &mut second);
    assert!(first.restarted.is_empty());
    assert!(second.restarted.is_empty());
}

/// A unit that does not come back is named with what was last observed, and
/// the rest of the move still happens.
#[test]
fn joint_move_names_a_unit_that_did_not_come_back() {
    let plan = JointMovePlan {
        restart: vec!["good.service".into(), "bad.service".into()],
        already_current: Vec::new(),
        left_inactive: Vec::new(),
        left_no_image: Vec::new(),
        left_unjudged: Vec::new(),
        unobserved: Vec::new(),
    };
    let mut effect = RecordingEffect {
        fail_on: Some("bad.service".to_string()),
        observed: "ActiveState=activating;SubState=dead".to_string(),
        ..RecordingEffect::default()
    };
    let report = apply_joint_move(&plan, &mut effect);

    assert_eq!(report.failed_units(), vec!["bad.service"]);
    let bad = report
        .0
        .iter()
        .find(|(unit, _)| unit == "bad.service")
        .expect("failed unit reported");
    assert_eq!(
        bad.1,
        MoveOutcome::Failed {
            observed: "ActiveState=activating;SubState=dead".to_string()
        }
    );
    assert_eq!(effect.restarted, vec!["good.service".to_string()]);
}

/// The effect surface has no force. A unit that will not stop is left
/// running and named — never escalated into (SPEC DT-7).
#[test]
fn joint_move_does_not_escalate_when_a_unit_will_not_stop() {
    let plan = JointMovePlan {
        restart: vec!["wedged.service".into()],
        already_current: Vec::new(),
        left_inactive: Vec::new(),
        left_no_image: Vec::new(),
        left_unjudged: Vec::new(),
        unobserved: Vec::new(),
    };
    let mut effect = RecordingEffect {
        fail_on: Some("wedged.service".to_string()),
        observed: "ActiveState=active;SubState=running".to_string(),
        ..RecordingEffect::default()
    };
    let report = apply_joint_move(&plan, &mut effect);

    assert_eq!(effect.killed, Vec::<String>::new());
    assert_eq!(effect.stopped, Vec::<String>::new());
    assert_eq!(effect.started, Vec::<String>::new());
    assert_eq!(report.failed_units(), vec!["wedged.service"]);
}

#[test]
fn client_update_notice_names_both_releases() {
    let notice = client_update_notice(Some("v0.14.1-584-g0649a3a4"), Some("v0.14.1-593-g38c38651"));
    assert_eq!(
        notice,
        ClientUpdateNotice::Pending {
            from: "v0.14.1-584-g0649a3a4".into(),
            to: "v0.14.1-593-g38c38651".into(),
        }
    );
    assert_eq!(
        notice.banner(),
        "Update installed · v0.14.1-584-g0649a3a4 → v0.14.1-593-g38c38651"
    );

    let done = completed_update_notice(Some("v0.14.1-584-g0649a3a4"), "v0.14.1-593-g38c38651");
    assert_eq!(
        done.banner(),
        "Updated v0.14.1-584-g0649a3a4 → v0.14.1-593-g38c38651"
    );
}

#[derive(Default)]
pub(super) struct FakeView {
    active: std::collections::BTreeMap<String, Option<bool>>,
    pids: std::collections::BTreeMap<String, u32>,
    executables: std::collections::BTreeMap<u32, PathBuf>,
    starts: std::collections::BTreeMap<u32, SystemTime>,
    selected: Option<SystemTime>,
}

impl ProductProcessView for FakeView {
    fn unit_is_active(&self, unit: &str) -> Option<bool> {
        self.active.get(unit).copied().flatten()
    }

    fn unit_main_pid(&self, unit: &str) -> Option<u32> {
        self.pids.get(unit).copied()
    }

    fn process_executable(&self, pid: u32) -> Option<ObservedExecutable> {
        ObservedExecutable::open(self.executables.get(&pid)?.clone())
    }

    fn unit_observation_still_current(&self, _: &str) -> bool {
        true
    }

    fn process_start_instant(&self, pid: u32) -> Option<SystemTime> {
        self.starts.get(&pid).copied()
    }

    fn selected_release_instant(&self) -> Option<SystemTime> {
        self.selected
    }
}

/// Records what a joint move actually did. Deliberately offers only
/// `try_restart`, so a test cannot prove the move starts or kills by
/// calling a method that does not exist.
#[derive(Default)]
pub(super) struct RecordingEffect {
    restarted: Vec<String>,
    started: Vec<String>,
    stopped: Vec<String>,
    killed: Vec<String>,
    fail_on: Option<String>,
    observed: String,
}

impl ReleaseCoherenceEffect for RecordingEffect {
    fn try_restart(&mut self, unit: &str) -> Result<(), RestartFailure> {
        if self.fail_on.as_deref() == Some(unit) {
            return Err(RestartFailure::Refused(format!("{unit} did not come back")));
        }
        self.restarted.push(unit.to_string());
        Ok(())
    }

    fn observed_state(&mut self, _unit: &str) -> String {
        self.observed.clone()
    }
}

#[path = "tests/fleet_and_systemd.rs"]
mod fleet_and_systemd;

#[path = "tests/uncertain_restart.rs"]
mod uncertain_restart;

#[path = "tests/sync_custody.rs"]
mod sync_custody;
