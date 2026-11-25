// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/tests_suite/nbd_teardown_and_clean_state.rs
#![allow(clippy::expect_used)]

use super::*;

#[test]
fn execute_legacy_migration_transition_covers_all_action_variants() {
    struct MockExecutor {
        swapoff_calls: RefCell<usize>,
        reset_zram_calls: RefCell<usize>,
        disconnect_nbd_calls: RefCell<usize>,
        stop_daemon_calls: RefCell<usize>,
        fail_on: Option<String>,
    }

    impl LegacyMigrationExecutor for MockExecutor {
        fn swapoff(&self, device: &BoundDeviceIdentity) -> Result<(), CascadeError> {
            *self.swapoff_calls.borrow_mut() += 1;
            if self.fail_on.as_deref() == Some("swapoff") {
                Err(CascadeError::Precondition(format!(
                    "swapoff failed for {}",
                    device.path
                )))
            } else {
                Ok(())
            }
        }
        fn reset_zram(&self, device: &BoundDeviceIdentity) -> Result<(), CascadeError> {
            *self.reset_zram_calls.borrow_mut() += 1;
            if self.fail_on.as_deref() == Some("reset_zram") {
                Err(CascadeError::Precondition(format!(
                    "reset_zram failed for {}",
                    device.path
                )))
            } else {
                Ok(())
            }
        }
        fn disconnect_nbd(&self, device: &BoundDeviceIdentity) -> Result<(), CascadeError> {
            *self.disconnect_nbd_calls.borrow_mut() += 1;
            if self.fail_on.as_deref() == Some("disconnect_nbd") {
                Err(CascadeError::Precondition(format!(
                    "disconnect_nbd failed for {}",
                    device.path
                )))
            } else {
                Ok(())
            }
        }
        fn stop_daemon(&self) -> Result<(), CascadeError> {
            *self.stop_daemon_calls.borrow_mut() += 1;
            if self.fail_on.as_deref() == Some("stop_daemon") {
                Err(CascadeError::Precondition("stop_daemon failed".into()))
            } else {
                Ok(())
            }
        }
    }

    let nbd = bound_device_fixture("/dev/nbd0", ManagedDeviceKind::Nbd);
    let zram = bound_device_fixture("/dev/zram0", ManagedDeviceKind::Zram);
    let transition = LegacyMigrationTransition {
        actions: vec![
            LegacyMigrationAction::Swapoff(zram.clone()),
            LegacyMigrationAction::Swapoff(nbd.clone()),
            LegacyMigrationAction::ResetZram(zram.clone()),
            LegacyMigrationAction::DisconnectNbd(nbd.clone()),
            LegacyMigrationAction::StopDaemon,
        ],
    };

    let executor = MockExecutor {
        swapoff_calls: RefCell::new(0),
        reset_zram_calls: RefCell::new(0),
        disconnect_nbd_calls: RefCell::new(0),
        stop_daemon_calls: RefCell::new(0),
        fail_on: None,
    };

    assert!(execute_legacy_migration_transition(&transition, &executor).is_ok());
    assert_eq!(*executor.swapoff_calls.borrow(), 2);
    assert_eq!(*executor.reset_zram_calls.borrow(), 1);
    assert_eq!(*executor.disconnect_nbd_calls.borrow(), 1);
    assert_eq!(*executor.stop_daemon_calls.borrow(), 1);

    let failing_reset = MockExecutor {
        swapoff_calls: RefCell::new(0),
        reset_zram_calls: RefCell::new(0),
        disconnect_nbd_calls: RefCell::new(0),
        stop_daemon_calls: RefCell::new(0),
        fail_on: Some("reset_zram".into()),
    };
    assert!(execute_legacy_migration_transition(&transition, &failing_reset).is_err());

    let failing_disc = MockExecutor {
        swapoff_calls: RefCell::new(0),
        reset_zram_calls: RefCell::new(0),
        disconnect_nbd_calls: RefCell::new(0),
        stop_daemon_calls: RefCell::new(0),
        fail_on: Some("disconnect_nbd".into()),
    };
    assert!(execute_legacy_migration_transition(&transition, &failing_disc).is_err());

    let failing_stop = MockExecutor {
        swapoff_calls: RefCell::new(0),
        reset_zram_calls: RefCell::new(0),
        disconnect_nbd_calls: RefCell::new(0),
        stop_daemon_calls: RefCell::new(0),
        fail_on: Some("stop_daemon".into()),
    };
    assert!(execute_legacy_migration_transition(&transition, &failing_stop).is_err());
}

#[test]
fn legacy_drain_budget_checks_overflow_and_boundaries() {
    assert!(legacy_drain_budget_is_sufficient(0, 1024, 0));
    assert!(!legacy_drain_budget_is_sufficient(u64::MAX, 1024, 1024));
    assert!(!legacy_drain_budget_is_sufficient(1024, 1024, u64::MAX));
}

#[test]
fn down_refuses_foreign_live_device_without_running_a_command() {
    let fixture = TestDir::new();
    let daemon = spawn_fixture_daemon(&fixture, "exit 0");
    let paths = RuntimePaths::under(&fixture.path);
    let devices = seal_runtime_lifecycle(&paths, daemon.id(), false);
    let mut observed = devices.clone();
    observed.push(bound_device_fixture("/dev/nbd1", ManagedDeviceKind::Nbd));
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    set_swap_snapshots([Ok("Filename Type Size Used Priority\n")]);
    set_live_device_snapshots([Ok(observed)]);
    let runner = ScriptedRunner::new(Vec::new());

    let error = error_from(
        down_with_runtime(&runner, &paths),
        "foreign device discovery must refuse the whole teardown",
    );

    assert!(error.to_string().contains("foreign live"), "{error}");
    assert!(runner.calls().is_empty());
    assert!(paths.lifecycle_binding_file.exists());
    assert!(!process_is_gone_or_zombie(daemon.id()));
}

#[test]
// TestName: down_resumes_a_partially_released_lifecycle_without_mutating_devices
fn down_resumes_a_partially_released_lifecycle_without_mutating_devices() {
    // A previous teardown may already have released the bound devices
    // (zram reset, export disconnected) while the daemon and the runtime
    // records survived. The second `down` must resume — stop the daemon
    // and clean the records — instead of demanding the released devices
    // still be live, which made the cascade unrecoverable.
    let fixture = TestDir::new();
    let daemon = spawn_fixture_daemon(&fixture, "exit 0");
    let paths = RuntimePaths::under(&fixture.path);
    let _devices = seal_runtime_lifecycle(&paths, daemon.id(), false);
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    set_swap_snapshots([
        Ok("Filename Type Size Used Priority\n"),
        Ok("Filename Type Size Used Priority\n"),
    ]);
    set_live_device_snapshots([Ok(Vec::new()), Ok(Vec::new())]);
    let runner = ScriptedRunner::new(Vec::new());

    down_with_runtime(&runner, &paths).unwrap_or_else(|error| {
        panic!("a partially released lifecycle must resume to daemon stop: {error}")
    });

    assert!(runner.calls().is_empty());
    assert!(!paths.lifecycle_binding_file.exists());
    assert!(!paths.swap_dev_file.exists());
    assert!(process_is_gone_or_zombie(daemon.id()));
}

#[test]
// TestName: down_settles_a_disconnect_completion_the_kernel_cannot_show_at_once
fn down_settles_a_disconnect_completion_the_kernel_cannot_show_at_once() {
    // `nbd-client -d` acknowledged the request, but the kernel still named
    // the old owner for the first two observations. A single observation
    // would refuse a detach that already succeeded; the bounded settle
    // must keep observing until the exact absence is visible.
    let fixture = TestDir::new();
    let daemon = spawn_fixture_daemon(&fixture, "exit 0");
    let paths = RuntimePaths::under(&fixture.path);
    let devices = seal_runtime_lifecycle(&paths, daemon.id(), false);
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    install_two_tier_down_snapshots(&devices, true, false);
    let runner = ScriptedRunner::new(vec![
        ("swapoff -- /dev/nbd0".into(), Ok(String::new())),
        ("nbd-client -d /dev/nbd0".into(), Ok(String::new())),
    ]);

    down_with_runtime(&runner, &paths)
        .unwrap_or_else(|error| panic!("a deferred owner clear must settle: {error}"));

    assert_eq!(
        runner.calls(),
        vec!["swapoff -- /dev/nbd0", "nbd-client -d /dev/nbd0"]
    );
    assert!(!paths.lifecycle_binding_file.exists());
    assert!(!paths.swap_dev_file.exists());
    assert!(process_is_gone_or_zombie(daemon.id()));
}

#[test]
// TestName: down_bounds_a_disconnect_completion_that_never_settles
fn down_bounds_a_disconnect_completion_that_never_settles() {
    let fixture = TestDir::new();
    let daemon = spawn_fixture_daemon(&fixture, "exit 0");
    let paths = RuntimePaths::under(&fixture.path);
    let devices = seal_runtime_lifecycle(&paths, daemon.id(), false);
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    set_nbd_detach_settle_timeout(Duration::from_millis(40));
    install_two_tier_down_snapshots(&devices, true, false);
    let nbd = devices
        .iter()
        .filter(|device| device.kind == ManagedDeviceKind::Nbd)
        .cloned()
        .collect::<Vec<_>>();
    let mut live = vec![Ok(nbd); 8];
    live.push(Ok(Vec::new()));
    set_live_device_snapshots(live);
    let runner = ScriptedRunner::new(vec![
        ("swapoff -- /dev/nbd0".into(), Ok(String::new())),
        ("nbd-client -d /dev/nbd0".into(), Ok(String::new())),
    ]);

    let error = error_from(
        down_with_runtime(&runner, &paths),
        "an owner that never clears must fail the bounded settle",
    );

    assert!(
        error
            .to_string()
            .contains("disconnect completion is unproven"),
        "{error}"
    );
    assert_eq!(
        runner.calls(),
        vec!["swapoff -- /dev/nbd0", "nbd-client -d /dev/nbd0"]
    );
    assert!(!process_is_gone_or_zombie(daemon.id()));
    assert!(paths.lifecycle_binding_file.exists());
    assert!(paths.swap_dev_file.exists());
}

#[test]
// TestName: detach_settle_never_retries_a_foreign_live_device
fn detach_settle_never_retries_a_foreign_live_device() {
    // Containment is a refusal, not a transition to wait for. A foreign
    // device that appears while the settle is polling must fail on that
    // first observation; if the settle retried it, the exhausted snapshot
    // queue would fabricate the empty set and succeed.
    let fixture = TestDir::new();
    let daemon = spawn_fixture_daemon(&fixture, "exit 0");
    let paths = RuntimePaths::under(&fixture.path);
    let devices = seal_runtime_lifecycle(&paths, daemon.id(), false);
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    install_two_tier_down_snapshots(&devices, true, false);
    let nbd = devices
        .iter()
        .filter(|device| device.kind == ManagedDeviceKind::Nbd)
        .cloned()
        .collect::<Vec<_>>();
    let mut foreign = nbd.clone();
    foreign.push(bound_device_fixture("/dev/nbd1", ManagedDeviceKind::Nbd));
    set_live_device_snapshots([Ok(nbd.clone()), Ok(nbd), Ok(foreign)]);
    let runner = ScriptedRunner::new(vec![
        ("swapoff -- /dev/nbd0".into(), Ok(String::new())),
        ("nbd-client -d /dev/nbd0".into(), Ok(String::new())),
    ]);

    let error = error_from(
        down_with_runtime(&runner, &paths),
        "a foreign device during the settle must refuse the teardown",
    );

    assert!(error.to_string().contains("foreign live"), "{error}");
    assert!(!process_is_gone_or_zombie(daemon.id()));
    assert!(paths.lifecycle_binding_file.exists());
}

#[test]
fn zram_reset_stage_mismatch_stops_before_nbd_disconnect() {
    let fixture = TestDir::new();
    let daemon = spawn_fixture_daemon(&fixture, "exit 0");
    let paths = RuntimePaths::under(&fixture.path);
    let devices = seal_runtime_lifecycle(&paths, daemon.id(), true);
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    let full = devices.clone();
    set_live_device_snapshots([Ok(full.clone()), Ok(full.clone()), Ok(full)]);
    let runner = ScriptedRunner::new(vec![("zramctl -r /dev/zram0".into(), Ok(String::new()))]);

    let error = error_from(
        down_with_runtime(&runner, &paths),
        "zram reset without exact stage transition must stop the teardown",
    );

    assert!(error.to_string().contains("reset completion"), "{error}");
    assert_eq!(runner.calls(), vec!["zramctl -r /dev/zram0"]);
    assert!(paths.lifecycle_binding_file.exists());
    assert!(paths.swap_dev_file.exists());
    assert!(paths.zram_dev_file.exists());
    assert!(!process_is_gone_or_zombie(daemon.id()));
}

#[test]
fn zram_rollback_requires_recorded_identity_before_any_command() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir)
        .unwrap_or_else(|error| panic!("create test runtime: {error}"));
    let runner = ScriptedRunner::new(Vec::new());

    assert!(!rollback_zram_tier(&runner, &paths, "/dev/zram7", true));
    assert!(runner.calls().is_empty());
}

#[test]
fn down_refuses_ambiguous_live_identity_without_running_a_command() {
    let fixture = TestDir::new();
    let daemon = spawn_fixture_daemon(&fixture, "exit 0");
    let paths = RuntimePaths::under(&fixture.path);
    let devices = seal_runtime_lifecycle(&paths, daemon.id(), false);
    let mut retargeted = devices[0].clone();
    retargeted.dev_t = "43:9".into();
    retargeted.sysfs_dev_t = "43:9".into();
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    set_swap_snapshots([Ok("Filename Type Size Used Priority\n")]);
    set_live_device_snapshots([Ok(vec![devices[0].clone(), retargeted])]);
    let runner = ScriptedRunner::new(Vec::new());

    let error = error_from(
        down_with_runtime(&runner, &paths),
        "ambiguous device identity must refuse the whole teardown",
    );

    assert!(error.to_string().contains("ambiguous"), "{error}");
    assert!(runner.calls().is_empty());
    assert!(paths.lifecycle_binding_file.exists());
    assert!(!process_is_gone_or_zombie(daemon.id()));
}

#[test]
fn down_refuses_foreign_managed_swap_without_running_a_command() {
    let fixture = TestDir::new();
    let daemon = spawn_fixture_daemon(&fixture, "exit 0");
    let paths = RuntimePaths::under(&fixture.path);
    let devices = seal_runtime_lifecycle(&paths, daemon.id(), false);
    const FOREIGN_SWAP: &str = "Filename Type Size Used Priority\n/dev/nbd1 partition 1024 0 100\n";
    let _seams = ParentSeams::install(FOREIGN_SWAP, 0);
    set_swap_snapshots([Ok(FOREIGN_SWAP)]);
    set_live_device_snapshots([Ok(devices)]);
    let runner = ScriptedRunner::new(Vec::new());

    let error = error_from(
        down_with_runtime(&runner, &paths),
        "foreign active swap must refuse the whole teardown",
    );

    assert!(
        error.to_string().contains("foreign or ambiguous"),
        "{error}"
    );
    assert!(runner.calls().is_empty());
    assert!(paths.lifecycle_binding_file.exists());
    assert!(!process_is_gone_or_zombie(daemon.id()));
}

#[test]
fn down_refuses_mismatched_runtime_record_without_running_a_command() {
    let fixture = TestDir::new();
    let daemon = spawn_fixture_daemon(&fixture, "exit 0");
    let paths = RuntimePaths::under(&fixture.path);
    let devices = seal_runtime_lifecycle(&paths, daemon.id(), false);
    remove_runtime_file(&paths.swap_dev_file);
    write_nbd_path_record(&paths.swap_dev_file, "/dev/nbd9")
        .expect("write retargeted NBD path record fixture");
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    set_swap_snapshots([Ok("Filename Type Size Used Priority\n")]);
    set_live_device_snapshots([Ok(devices)]);
    let runner = ScriptedRunner::new(Vec::new());

    let error = error_from(
        down_with_runtime(&runner, &paths),
        "a mismatched runtime record must refuse the whole teardown",
    );

    assert!(
        error.to_string().contains("runtime record differs"),
        "{error}"
    );
    assert!(runner.calls().is_empty());
    assert!(paths.lifecycle_binding_file.exists());
    assert!(!process_is_gone_or_zombie(daemon.id()));
}

#[test]
fn down_refuses_unreadable_or_malformed_swap_snapshot_before_mutation() {
    for snapshot in [
        Err("manufactured unreadable /proc/swaps"),
        Ok("Filename Type Size Used Priority\n/dev/nbd0 partition broken 0 100\n"),
    ] {
        let fixture = TestDir::new();
        let paths = RuntimePaths::under(&fixture.path);
        fs::create_dir_all(&paths.runtime_dir)
            .unwrap_or_else(|error| panic!("create evidence runtime: {error}"));
        fs::write(&paths.pid_file, "preserve-me")
            .unwrap_or_else(|error| panic!("write evidence fixture: {error}"));
        let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
        set_swap_snapshots([snapshot]);
        let runner = ScriptedRunner::new(Vec::new());

        assert!(down_with_runtime(&runner, &paths).is_err());
        assert!(runner.calls().is_empty());
        assert!(paths.pid_file.exists());
    }
}

#[test]
fn active_zero_use_swapoff_failure_preserves_backend_and_evidence() {
    let fixture = TestDir::new();
    let daemon = spawn_fixture_daemon(&fixture, "exit 0");
    let paths = RuntimePaths::under(&fixture.path);
    let devices = seal_runtime_lifecycle(&paths, daemon.id(), false);
    const ACTIVE_ZERO: &str = "Filename Type Size Used Priority\n/dev/nbd0 partition 1024 0 100\n";
    let _seams = ParentSeams::install(ACTIVE_ZERO, 0);
    set_swap_snapshots([Ok(ACTIVE_ZERO), Ok(ACTIVE_ZERO)]);
    set_live_device_snapshots([Ok(devices.clone()), Ok(devices)]);
    let runner = ScriptedRunner::new(vec![(
        "swapoff -- /dev/nbd0".into(),
        Err(CascadeError::Shell {
            cmd: "swapoff -- /dev/nbd0".into(),
            msg: "manufactured swapoff refusal".into(),
        }),
    )]);

    let error = error_from(
        down_with_runtime(&runner, &paths),
        "zero-use active swap is still active and must preserve its backend on failure",
    );

    assert!(error.to_string().contains("backend preserved"), "{error}");
    assert_eq!(runner.calls(), vec!["swapoff -- /dev/nbd0"]);
    assert!(paths.lifecycle_binding_file.exists());
    assert!(paths.swap_dev_file.exists());
    assert!(!process_is_gone_or_zombie(daemon.id()));
}

#[test]
fn uncertain_swapoff_absence_proof_preserves_backend_and_evidence() {
    let fixture = TestDir::new();
    let daemon = spawn_fixture_daemon(&fixture, "exit 0");
    let paths = RuntimePaths::under(&fixture.path);
    let devices = seal_runtime_lifecycle(&paths, daemon.id(), false);
    const ACTIVE_ZERO: &str = "Filename Type Size Used Priority\n/dev/nbd0 partition 1024 0 100\n";
    let _seams = ParentSeams::install(ACTIVE_ZERO, 0);
    set_swap_snapshots([
        Ok(ACTIVE_ZERO),
        Ok(ACTIVE_ZERO),
        Err("manufactured unreadable fresh absence proof"),
    ]);
    set_live_device_snapshots([Ok(devices.clone()), Ok(devices)]);
    let runner = ScriptedRunner::new(vec![(
        "swapoff -- /dev/nbd0".into(),
        Err(CascadeError::Shell {
            cmd: "swapoff -- /dev/nbd0".into(),
            msg: "No such file or directory".into(),
        }),
    )]);

    let error = error_from(
        down_with_runtime(&runner, &paths),
        "an unreadable fresh snapshot must not convert swapoff uncertainty into success",
    );

    assert!(error.to_string().contains("backend preserved"), "{error}");
    assert_eq!(runner.calls(), vec!["swapoff -- /dev/nbd0"]);
    assert!(paths.lifecycle_binding_file.exists());
    assert!(paths.swap_dev_file.exists());
    assert!(!process_is_gone_or_zombie(daemon.id()));
}

#[test]
// TestName: nbd_disconnect_refusal_stops_before_daemon_stop
fn nbd_disconnect_refusal_stops_before_daemon_stop() {
    struct DisconnectRefusingExecutor {
        calls: RefCell<Vec<String>>,
    }

    impl NbdLifecycleExecutor for DisconnectRefusingExecutor {
        fn swapoff(&self, device: &BoundDeviceIdentity) -> Result<(), CascadeError> {
            self.calls
                .borrow_mut()
                .push(format!("swapoff {}", device.path));
            Ok(())
        }

        fn reset_zram(&self, device: &BoundDeviceIdentity) -> Result<(), CascadeError> {
            self.calls
                .borrow_mut()
                .push(format!("reset-zram {}", device.path));
            Ok(())
        }

        fn disconnect_nbd(&self, device: &BoundDeviceIdentity) -> Result<(), CascadeError> {
            self.calls
                .borrow_mut()
                .push(format!("disconnect-nbd {}", device.path));
            Err(CascadeError::Shell {
                cmd: format!("nbd-client -d {}", device.path),
                msg: "manufactured NBD disconnect refusal".into(),
            })
        }

        fn stop_daemon(&self) -> Result<(), CascadeError> {
            self.calls.borrow_mut().push("stop-daemon".into());
            Ok(())
        }
    }

    let nbd = bound_device_fixture("/dev/nbd0", ManagedDeviceKind::Nbd);
    let binding = lifecycle_binding_fixture(vec![nbd.clone()]);
    let swaps =
        parse_proc_swaps("Filename Type Size Used Priority\n/dev/nbd0 partition 1024 0 100\n")
            .expect("strict swap fixture");
    let plan = plan_nbd_lifecycle(&binding, &swaps, &[nbd]).expect("authorized lifecycle plan");
    let executor = DisconnectRefusingExecutor {
        calls: RefCell::new(Vec::new()),
    };

    let error = error_from(
        execute_nbd_lifecycle_plan(&plan, &executor),
        "a refused NBD disconnect must stop the injected plan",
    );
    assert!(
        error
            .to_string()
            .contains("manufactured NBD disconnect refusal")
    );
    assert_eq!(
        executor.calls.into_inner(),
        vec!["swapoff /dev/nbd0", "disconnect-nbd /dev/nbd0"]
    );
}

pub(in crate::cascade::cascade_io) fn down_with_runtime_preserves_swapoff_first_and_cleans_temp_state()
 {
    let fixture = TestDir::new();
    let daemon = spawn_fixture_daemon(&fixture, "exit 0");
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir)
        .unwrap_or_else(|error| panic!("create test runtime: {error}"));
    fs::create_dir_all(
        paths.forensics_markers[0]
            .parent()
            .unwrap_or_else(|| panic!("test marker has no parent")),
    )
    .unwrap_or_else(|error| panic!("create test forensics: {error}"));
    fs::write(&paths.forensics_markers[0], "armed")
        .unwrap_or_else(|error| panic!("write test marker: {error}"));
    let devices = seal_runtime_lifecycle(&paths, daemon.id(), true);
    let _seams = ParentSeams::install(
        "Filename Type Size Used Priority\n/dev/nbd0 partition 1024 0 100\n/dev/zram0 partition 1024 0 200\n",
        0,
    );
    install_two_tier_down_snapshots(&devices, true, true);
    let runner = ScriptedRunner::new(vec![
        ("swapoff -- /dev/nbd0".into(), Ok(String::new())),
        ("swapoff -- /dev/zram0".into(), Ok(String::new())),
        ("zramctl -r /dev/zram0".into(), Ok(String::new())),
        ("nbd-client -d /dev/nbd0".into(), Ok(String::new())),
    ]);

    down_with_runtime(&runner, &paths)
        .unwrap_or_else(|error| panic!("isolated cascade down: {error}"));
    assert_eq!(
        runner.calls(),
        vec![
            "swapoff -- /dev/nbd0",
            "swapoff -- /dev/zram0",
            "zramctl -r /dev/zram0",
            "nbd-client -d /dev/nbd0",
        ]
    );
    assert_eq!(
        runner.bounded_calls.borrow().as_slice(),
        &[
            ("swapoff -- /dev/nbd0".to_string(), Duration::from_secs(120)),
            (
                "swapoff -- /dev/zram0".to_string(),
                Duration::from_secs(120)
            ),
        ]
    );
    assert!(!paths.swap_dev_file.exists());
    assert!(!paths.zram_dev_file.exists());
    assert!(!paths.forensics_markers[0].exists());
}

/// SPEC #18 bounded teardown: a measured-dead NBD peer refuses `down`
/// before any kernel swapoff is attempted, so the uninterruptible stall
/// a dead peer causes never starts. The swap device
/// and the ownership records are preserved for the restart-then-`down`
/// recovery, and the refusal names the situation.
#[test]
fn down_refuses_boundedly_when_the_nbd_peer_is_measured_dead() {
    let fixture = TestDir::new();
    let mut daemon = spawn_fixture_daemon(&fixture, "exit 0");
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir)
        .unwrap_or_else(|error| panic!("create test runtime: {error}"));
    let devices = seal_runtime_lifecycle(&paths, daemon.id(), true);
    terminate_spawned_child(daemon.child_mut())
        .unwrap_or_else(|error| panic!("stop the fixture daemon: {error}"));
    let _seams = ParentSeams::install(
        "Filename Type Size Used Priority\n/dev/nbd0 partition 1024 0 100\n/dev/zram0 partition 1024 0 200\n",
        0,
    );
    install_two_tier_down_snapshots(&devices, true, true);
    let runner = ScriptedRunner::new(Vec::new());

    let error = error_from(
        down_with_runtime(&runner, &paths),
        "a measured-dead peer must refuse before any swapoff",
    );
    assert!(
        error
            .to_string()
            .contains("NBD peer (ramsharedd) is not running"),
        "the refusal names the dead peer: {error}"
    );
    assert!(
        runner.calls().is_empty(),
        "no swapoff (or any command) is attempted against a dead peer: {:?}",
        runner.calls()
    );
    assert!(
        paths.swap_dev_file.exists(),
        "the NBD path record survives for the recovery"
    );
    assert!(
        paths.zram_dev_file.exists(),
        "the zram record survives for the recovery"
    );
}
