// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/supervisor/tests/shutdown_and_reconcile.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

#[test]
fn sample_parsers_and_atomic_publication_are_bounded() {
    assert_eq!(
        parse_meminfo("MemTotal: 16384 kB\nMemAvailable: 8192 kB\n"),
        Some((16 * 1024 * 1024, 8 * 1024 * 1024))
    );
    assert!(parse_meminfo("MemTotal: no\n").is_none());
    assert_eq!(
        parse_psi_full_avg10("some avg10=3.00\nfull avg10=2.50 avg60=1.00\n"),
        Some(2.5)
    );
    assert!(parse_psi_full_avg10("some avg10=1.0\n").is_none());
    assert!(parse_psi_full_avg10("full avg10=NaN\n").is_none());
    assert!(parse_psi_full_avg10("full avg10=100.01\n").is_none());
    assert!(parse_psi_full_avg10("full avg10=1.0\nfull avg10=2.0\n").is_none());

    let root = fixture();
    let state = root.join("nested/supervisor.json");
    let decision = SupervisorDecision {
        state: SupervisorState::Guarded,
        actions: vec![SupervisorAction::CloseAdmission],
        healthy_samples: 17,
    };
    let supervisor_identity = OwnerIdentity::current().unwrap();
    let action_results = [SupervisorActionResult::from_result(
        SupervisorAction::CloseAdmission,
        Ok(()),
    )];
    publish_at(
        &state,
        &decision,
        &action_results,
        Some("fixture-daemon"),
        &supervisor_identity,
        1_000,
    )
    .unwrap();
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&state).unwrap()).unwrap();
    assert_eq!(value["control_state"], "GUARDED");
    assert_eq!(value["healthy_samples"], 17);
    assert_eq!(value["action_results"][0]["action"], "close_admission");
    assert_eq!(value["daemon_instance_id"], "fixture-daemon");
    if std::path::Path::new("/proc/pressure/memory").exists() {
        assert!(collect_sample(Instant::now(), Instant::now()).is_ok());
    }
    assert!(run(&["--invalid".into()]).is_err());
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: close_admission_publishes_a_serialized_closed_gate_before_guarded_state
fn close_admission_publishes_a_serialized_closed_gate_before_guarded_state() {
    let root = fixture();
    let state_path = root.join("supervisor-state.json");
    let ledger_root = root.join("admission");
    let identity = OwnerIdentity::current().unwrap();
    let guarded = SupervisorDecision {
        state: SupervisorState::Guarded,
        actions: vec![SupervisorAction::CloseAdmission],
        healthy_samples: 0,
    };
    let runtime = root.join("runtime");
    let ledger = ledger_root.join("reservations.json");
    let action_paths = ActionPaths {
        runtime: &runtime,
        ledger: &ledger,
        daemon_instance_id: Some("fixture-daemon"),
        issued_at_unix_ms: Some(1_000),
    };
    let runner = FakeUnitRunner {
        calls: RefCell::new(Vec::new()),
        fail: false,
    };
    let guarded_results = apply_decision_with(
        &state_path,
        &ledger_root,
        &guarded,
        &action_paths,
        &runner,
        &identity,
        1_000,
    )
    .unwrap();
    assert_eq!(guarded_results[0].status, SupervisorActionStatus::Succeeded);
    let gate: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(ledger_root.join("admission-state.json")).unwrap(),
    )
    .unwrap();
    let state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&state_path).unwrap()).unwrap();
    assert_eq!(gate["admission_open"], false);
    assert_eq!(gate["control_state"], "GUARDED");
    assert_eq!(state["control_state"], "GUARDED");

    let healthy = SupervisorDecision {
        state: SupervisorState::Healthy,
        actions: Vec::new(),
        healthy_samples: 60,
    };
    apply_decision_with(
        &state_path,
        &ledger_root,
        &healthy,
        &action_paths,
        &runner,
        &identity,
        2_000,
    )
    .unwrap();
    let reopened: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(ledger_root.join("admission-state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(reopened["admission_open"], true);
    assert_eq!(reopened["control_state"], "HEALTHY");
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: critical_actions_remain_executable_when_admission_transition_busy
fn critical_actions_remain_executable_when_admission_transition_busy() {
    let root = fixture();
    let state_path = root.join("supervisor-state.json");
    let ledger_root = root.join("admission");
    fs::create_dir_all(&ledger_root).unwrap();
    let authority = File::open(&ledger_root).unwrap();
    authority.try_lock().unwrap();
    let runtime = root.join("runtime");
    let decision = SupervisorDecision {
        state: SupervisorState::Critical,
        actions: vec![
            SupervisorAction::CloseAdmission,
            SupervisorAction::ReduceVramCache,
            SupervisorAction::RequestReclaim,
        ],
        healthy_samples: 0,
    };
    let identity = OwnerIdentity::current().unwrap();
    let runner = FakeUnitRunner {
        calls: RefCell::new(Vec::new()),
        fail: false,
    };

    // The lock is held for the whole call, so the bounded intra-action
    // budget is exhausted and the busy is the definitive CloseAdmission
    // outcome. The control plane must still live: a `DecisionApplyError`
    // here is what `run()` turns into exit 1 and `Restart=on-failure`
    // crash-loops under ordinary reservation churn.
    let results = apply_decision_with(
        &state_path,
        &ledger_root,
        &decision,
        &ActionPaths {
            runtime: &runtime,
            ledger: &ledger_root.join("reservations.json"),
            daemon_instance_id: Some("fixture-daemon"),
            issued_at_unix_ms: Some(1_000),
        },
        &runner,
        &identity,
        1_000,
    )
    .expect(
        "an exhausted admission-lock budget terminated the control plane \
             instead of being recorded and retried on the next sample",
    );

    assert_eq!(results.len(), 3);
    assert!(runtime.join("cache-target.json").is_file());
    assert!(runtime.join("reclaim-request.json").is_file());
    let state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&state_path).unwrap()).unwrap();
    assert_eq!(state["control_state"], "CRITICAL");
    assert_eq!(state["action_results"].as_array().unwrap().len(), 3);
    let close = state["action_results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|result| result["action"] == "close_admission")
        .unwrap();
    assert_eq!(
        close["status"], "failed",
        "CloseAdmission published a synthetic success while its canonical lock was busy"
    );
    assert!(
        close["error"]
            .as_str()
            .is_some_and(|error| error.contains("busy")),
        "CloseAdmission did not retain the actual publication failure"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: deterministic_admission_close_failure_still_terminates_the_supervisor
fn deterministic_admission_close_failure_still_terminates_the_supervisor() {
    let root = fixture();
    let state_path = root.join("supervisor-state.json");
    // Not a directory: the lock acquisition fails on identity/path, which
    // is deterministic and must not be absorbed by the transient policy.
    let ledger_root = root.join("admission");
    fs::write(&ledger_root, b"not a ledger directory").unwrap();
    let runtime = root.join("runtime");
    let decision = SupervisorDecision {
        state: SupervisorState::Guarded,
        actions: vec![SupervisorAction::CloseAdmission],
        healthy_samples: 0,
    };
    let identity = OwnerIdentity::current().unwrap();
    let runner = FakeUnitRunner {
        calls: RefCell::new(Vec::new()),
        fail: false,
    };

    let error = apply_decision_with(
        &state_path,
        &ledger_root,
        &decision,
        &ActionPaths {
            runtime: &runtime,
            ledger: &ledger_root.join("reservations.json"),
            daemon_instance_id: Some("fixture-daemon"),
            issued_at_unix_ms: Some(1_000),
        },
        &runner,
        &identity,
        1_000,
    )
    .expect_err("a deterministic ledger failure was absorbed as if it were transient contention");

    let detail = error.to_string();
    assert!(detail.contains("admission close failed"), "{detail}");
    assert!(
        !detail.contains(workload::LEDGER_BUSY_SIGNATURE),
        "a deterministic failure must not be classified as transient busy: {detail}"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: transient_admission_lock_contention_does_not_terminate_the_supervisor
fn transient_admission_lock_contention_does_not_terminate_the_supervisor() {
    let root = fixture();
    let state_path = root.join("supervisor-state.json");
    let ledger_root = root.join("admission");
    fs::create_dir_all(&ledger_root).unwrap();
    let authority = std::fs::File::open(&ledger_root).unwrap();
    authority.try_lock().unwrap();
    let runtime = root.join("runtime");
    let decision = SupervisorDecision {
        state: SupervisorState::Guarded,
        actions: vec![SupervisorAction::CloseAdmission],
        healthy_samples: 0,
    };
    let identity = OwnerIdentity::current().unwrap();

    // A DecisionApplyError here is what makes `run()` return Err and lets
    // systemd restart the supervisor. A lock that another ledger actor is
    // about to release must not reach that outcome.
    let contended_state_path = state_path.clone();
    let contended_ledger_root = ledger_root.clone();
    let contended_runtime = runtime.clone();
    let contended_identity = identity.clone();
    let apply = std::thread::spawn(move || {
        let runner = FakeUnitRunner {
            calls: RefCell::new(Vec::new()),
            fail: false,
        };
        let action_paths = ActionPaths {
            runtime: &contended_runtime,
            ledger: &contended_ledger_root.join("reservations.json"),
            daemon_instance_id: Some("fixture-daemon"),
            issued_at_unix_ms: Some(1_000),
        };
        apply_decision_with(
            &contended_state_path,
            &contended_ledger_root,
            &decision,
            &action_paths,
            &runner,
            &contended_identity,
            1_000,
        )
    });
    std::thread::sleep(Duration::from_millis(40));
    drop(authority);
    let results = apply.join().unwrap().expect(
        "a transient admission lock contention terminated the supervisor \
             instead of being retried",
    );
    assert_eq!(results[0].status, SupervisorActionStatus::Succeeded);
    let gate: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(ledger_root.join("admission-state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(gate["admission_open"], false);
    assert_eq!(gate["control_state"], "GUARDED");
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
    let close = state["action_results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|result| result["action"] == "close_admission")
        .unwrap();
    assert_eq!(
        close["status"], "succeeded",
        "a retried transient must close admission for real, not record a failure"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: resuming_after_a_fresh_non_healthy_state_stays_closed
fn resuming_after_a_fresh_non_healthy_state_stays_closed() {
    let root = fixture();
    let state_path = root.join("supervisor-state.json");
    let now = 10_000u64;
    let published = serde_json::json!({
        "schema_version": 3,
        "control_state": "CRITICAL",
        "healthy_samples": 0,
        "action_results": [],
        "supervisor_identity": OwnerIdentity::current().unwrap(),
        "written_at_unix_ms": now.saturating_sub(2_000),
    });
    fs::write(&state_path, format!("{published}\n")).unwrap();

    let mut supervisor = Supervisor::resuming_from_prior_state(&state_path, now);
    assert_eq!(supervisor.state, SupervisorState::Guarded);
    let decision = supervisor.observe(PressureSample {
        memory_available_bytes: 16 * 1024 * 1024 * 1024,
        control_reserve_bytes: 4 * 1024 * 1024 * 1024,
        psi_full_avg10: 0.0,
        sample_delay_ms: 0,
        elapsed_ms: 1,
    });
    assert_eq!(
        decision.state,
        SupervisorState::Guarded,
        "a replaced control plane reopened admission and skipped the 60-sample recovery window"
    );
    assert!(decision.actions.contains(&SupervisorAction::CloseAdmission));
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: resuming_after_a_stale_or_absent_state_starts_healthy
fn resuming_after_a_stale_or_absent_state_starts_healthy() {
    let root = fixture();
    let state_path = root.join("supervisor-state.json");
    // Large enough that the 16 s gap below is a real subtraction, not a
    // saturating zero that the freshness bound would read as current.
    let now = 1_000_000u64;

    let fresh = Supervisor::resuming_from_prior_state(&state_path, now);
    assert_eq!(fresh.state, SupervisorState::Healthy);

    let stale = serde_json::json!({
        "schema_version": 3,
        "control_state": "CRITICAL",
        "healthy_samples": 0,
        "action_results": [],
        "supervisor_identity": OwnerIdentity::current().unwrap(),
        "written_at_unix_ms": now - 16_000,
    });
    fs::write(&state_path, format!("{stale}\n")).unwrap();
    let mut reboot = Supervisor::resuming_from_prior_state(&state_path, now);
    assert_eq!(
        reboot.state,
        SupervisorState::Healthy,
        "a real reboot must not inherit last boot's pressure state"
    );
    let decision = reboot.observe(PressureSample {
        memory_available_bytes: 16 * 1024 * 1024 * 1024,
        control_reserve_bytes: 4 * 1024 * 1024 * 1024,
        psi_full_avg10: 0.0,
        sample_delay_ms: 0,
        elapsed_ms: 1,
    });
    assert_eq!(decision.state, SupervisorState::Healthy);
    assert!(decision.actions.is_empty());
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: bounded_systemctl_adapter_is_fixture_scoped_under_parallel_execution
fn bounded_systemctl_adapter_is_fixture_scoped_under_parallel_execution() {
    let fixture = TestDir::new();
    let systemctl = fixture.program(
            "systemctl-fixture",
            "#!/bin/sh\ncase \"$1\" in\n  --version) exit 0 ;;\n  ramshared-invalid-command) exit 1 ;;\n  *) exit 2 ;;\nesac\n",
        );
    // Exit-code mapping is the subject here, not spawn latency. The budget
    // only has to bound the call, so it is generous enough to survive the
    // parallel suite spawning this shell on a loaded host.
    let budget = Duration::from_secs(5);
    assert!(run_systemctl_bounded_for(&systemctl, &["--version"], budget).is_ok());
    assert!(run_systemctl_bounded_for(&systemctl, &["ramshared-invalid-command"], budget).is_err());
}

#[test]
// TestName: bounded_systemctl_adapter_reaps_its_owned_timeout_fixture
fn bounded_systemctl_adapter_reaps_its_owned_timeout_fixture() {
    let fixture = TestDir::new();
    let pid_file = fixture.path.join("owned-systemctl.pid");
    let systemctl = fixture.program(
        "blocking-systemctl-fixture",
        "#!/bin/sh\nprintf '%s' \"$$\" > \"$1\"\nwhile :; do sleep 0.05; done\n",
    );
    let started = Instant::now();
    let error = run_systemctl_bounded_for(
        &systemctl,
        &[pid_file.to_str().unwrap()],
        Duration::from_millis(100),
    )
    .unwrap_err();
    assert!(error.contains("timed out"), "{error}");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "owned timeout cleanup exceeded one second"
    );
    let pid = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "owned timed-out fixture process was not reaped"
    );
}

#[test]
// TestName: bounded_systemctl_adapter_parallel_fixtures_stay_isolated
fn bounded_systemctl_adapter_parallel_fixtures_stay_isolated() {
    let start = std::sync::Arc::new(std::sync::Barrier::new(2));
    std::thread::scope(|scope| {
        let success_start = std::sync::Arc::clone(&start);
        let success = scope.spawn(move || {
            let fixture = TestDir::new();
            let systemctl = fixture.program(
                "successful-systemctl-fixture",
                "#!/bin/sh\nsleep 0.05\n[ \"$1\" = \"--version\" ]\n",
            );
            success_start.wait();
            run_systemctl_bounded_for(&systemctl, &["--version"], Duration::from_secs(2))
        });
        let timeout_start = std::sync::Arc::clone(&start);
        let timeout = scope.spawn(move || {
            let fixture = TestDir::new();
            let systemctl = fixture.program(
                "blocking-systemctl-fixture",
                "#!/bin/sh\nwhile :; do sleep 0.05; done\n",
            );
            timeout_start.wait();
            run_systemctl_bounded_for(&systemctl, &[], Duration::from_millis(100))
        });
        let success = success.join().unwrap();
        assert!(success.is_ok(), "successful fixture failed: {success:?}");
        let error = timeout.join().unwrap().unwrap_err();
        assert!(error.contains("timed out"), "{error}");
    });
}

#[test]
fn successful_action_results_commit_only_their_owned_state() {
    let mut supervisor = Supervisor::default();
    let action_results = [
        SupervisorActionResult {
            action: SupervisorAction::FreezeDiscardable,
            status: SupervisorActionStatus::Succeeded,
            error: None,
        },
        SupervisorActionResult {
            action: SupervisorAction::ThawDiscardable,
            status: SupervisorActionStatus::Succeeded,
            error: None,
        },
        SupervisorActionResult {
            action: SupervisorAction::TerminateDiscardable,
            status: SupervisorActionStatus::Succeeded,
            error: None,
        },
        SupervisorActionResult {
            action: SupervisorAction::KillDiscardable,
            status: SupervisorActionStatus::Succeeded,
            error: None,
        },
        SupervisorActionResult {
            action: SupervisorAction::CloseAdmission,
            status: SupervisorActionStatus::Succeeded,
            error: None,
        },
        SupervisorActionResult {
            action: SupervisorAction::ReduceVramCache,
            status: SupervisorActionStatus::Succeeded,
            error: None,
        },
        SupervisorActionResult {
            action: SupervisorAction::RequestReclaim,
            status: SupervisorActionStatus::Failed,
            error: Some(SupervisorActionError::new("retry later".into())),
        },
    ];

    supervisor.commit_action_results(&action_results, 1_234);

    assert!(!supervisor.discardable_frozen);
    assert!(!supervisor.discardable_freeze_pending);
    assert_eq!(
        supervisor.emergency_kill_eligible_ms,
        Some(1_234 + EMERGENCY_TERM_GRACE_MS)
    );
    assert!(supervisor.emergency_kill_sent);
}

#[test]
fn action_error_deserialization_accepts_only_sanitized_bounded_values() {
    assert_eq!(SupervisorState::Healthy.as_str(), "HEALTHY");
    assert_eq!(SupervisorState::Guarded.as_str(), "GUARDED");
    assert_eq!(SupervisorState::Critical.as_str(), "CRITICAL");
    assert_eq!(SupervisorState::Emergency.as_str(), "EMERGENCY");

    let sanitized = SupervisorActionError::new("first\tline".into());
    assert_eq!(&*sanitized, "first line");
    let encoded = serde_json::to_string(&sanitized).unwrap();
    assert_eq!(
        serde_json::from_str::<SupervisorActionError>(&encoded).unwrap(),
        sanitized
    );
    assert!(serde_json::from_str::<SupervisorActionError>("\"raw\\nline\"").is_err());
    let oversized = format!("\"{}\"", "x".repeat(ACTION_ERROR_MAX_BYTES + 1));
    assert!(serde_json::from_str::<SupervisorActionError>(&oversized).is_err());
}
