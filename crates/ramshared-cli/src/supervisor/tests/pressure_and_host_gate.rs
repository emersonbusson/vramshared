// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/supervisor/tests/pressure_and_host_gate.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

#[test]
// TestName: emergency_kill_is_bound_to_the_successfully_termed_identity_across_ledger_change
fn emergency_kill_is_bound_to_the_successfully_termed_identity_across_ledger_change() {
    let root = fixture();
    let runtime = root.join("runtime");
    let ledger = root.join("reservations.json");
    fs::write(
        &ledger,
        ledger_fixture(&[("batch", 4096, "ramshared-batch-first.scope")]),
    )
    .unwrap();
    let runner = FakeUnitRunner {
        calls: RefCell::new(Vec::new()),
        fail: false,
    };
    let paths = ActionPaths {
        runtime: &runtime,
        ledger: &ledger,
        daemon_instance_id: None,
        issued_at_unix_ms: Some(1_000),
    };
    let term = execute_actions_with(
        &SupervisorDecision {
            state: SupervisorState::Emergency,
            healthy_samples: 0,
            cache_recovery_samples: 0,
            actions: vec![SupervisorAction::TerminateDiscardable],
        },
        &paths,
        &runner,
        || Ok(()),
    );
    let durable_target = runtime.join("emergency-target.json").is_file();
    fs::write(
        &ledger,
        ledger_fixture(&[("batch", 8192, "ramshared-batch-replacement.scope")]),
    )
    .unwrap();
    let kill = execute_actions_with(
        &SupervisorDecision {
            state: SupervisorState::Emergency,
            healthy_samples: 0,
            cache_recovery_samples: 0,
            actions: vec![SupervisorAction::KillDiscardable],
        },
        &ActionPaths {
            runtime: &runtime,
            ledger: &ledger,
            daemon_instance_id: None,
            issued_at_unix_ms: Some(7_000),
        },
        &runner,
        || Ok(()),
    );
    let calls = runner.calls.borrow().clone();
    fs::remove_dir_all(root).unwrap();

    assert_eq!(term[0].status, SupervisorActionStatus::Succeeded);
    assert!(
        durable_target,
        "successful TERM did not persist its exact target"
    );
    assert_eq!(kill[0].status, SupervisorActionStatus::Succeeded);
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[0].last().map(String::as_str),
        Some("ramshared-batch-first.scope")
    );
    assert_eq!(
        calls[1].last().map(String::as_str),
        Some("ramshared-batch-first.scope"),
        "KILL retargeted a newly selected ledger victim"
    );
}

#[test]
// TestName: reconstructed_supervisor_kills_only_durable_termed_identity_after_grace
fn reconstructed_supervisor_kills_only_durable_termed_identity_after_grace() {
    let root = fixture();
    let runtime = root.join("runtime");
    let ledger = root.join("reservations.json");
    let durable_victim = VictimIdentity {
        unit: "ramshared-batch-before-restart.scope".into(),
        invocation_id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
    };
    write_emergency_target(
        &runtime,
        EmergencyPhase::Termed,
        &durable_victim,
        Some(1_000),
        1_000,
    )
    .unwrap();
    fs::write(
        &ledger,
        ledger_fixture(&[("batch", 8192, "ramshared-batch-after-restart.scope")]),
    )
    .unwrap();

    let mut reconstructed = Supervisor::default();
    reconstructed
        .recover_action_state(&runtime, 7_000, 250)
        .unwrap();
    let decision = reconstructed.observe(sample(0, 4, 0.0, 0, 250));
    assert!(
        decision
            .actions
            .contains(&SupervisorAction::KillDiscardable),
        "elapsed durable TERM grace did not reconstruct KILL eligibility"
    );
    assert!(
        !decision
            .actions
            .contains(&SupervisorAction::TerminateDiscardable)
    );
    let runner = FakeUnitRunner {
        calls: RefCell::new(Vec::new()),
        fail: false,
    };
    let results = execute_actions_with(
        &decision,
        &ActionPaths {
            runtime: &runtime,
            ledger: &ledger,
            daemon_instance_id: None,
            issued_at_unix_ms: Some(7_000),
        },
        &runner,
        || Ok(()),
    );
    let kill = results
        .iter()
        .find(|result| result.action == SupervisorAction::KillDiscardable)
        .unwrap();
    assert_eq!(kill.status, SupervisorActionStatus::Succeeded);
    let calls = runner.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].last().map(String::as_str),
        Some("ramshared-batch-before-restart.scope")
    );
    drop(calls);
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: supervisor_action_errors_are_bounded_single_line_and_control_free
fn supervisor_action_errors_are_bounded_single_line_and_control_free() {
    let root = fixture();
    let ledger = root.join("reservations.json");
    fs::write(
        &ledger,
        ledger_fixture(&[("batch", 4096, "ramshared-batch-fixture.scope")]),
    )
    .unwrap();
    let results = execute_actions_with(
        &SupervisorDecision {
            state: SupervisorState::Critical,
            healthy_samples: 0,
            cache_recovery_samples: 0,
            actions: vec![SupervisorAction::FreezeDiscardable],
        },
        &ActionPaths {
            runtime: &root.join("runtime"),
            ledger: &ledger,
            daemon_instance_id: None,
            issued_at_unix_ms: Some(1_000),
        },
        &HugeErrorRunner,
        || Ok(()),
    );
    let error = results[0].error.as_deref().unwrap().to_string();
    fs::remove_dir_all(root).unwrap();

    assert!(
        error.len() <= 1_024,
        "action error exceeded its type ceiling"
    );
    assert!(
        error.contains("[truncated]"),
        "truncated action error omitted its explicit marker"
    );
    assert!(
        !error.chars().any(char::is_control),
        "action error retained a control character"
    );
    assert_eq!(error.lines().count(), 1);
}

#[test]
// TestName: supervisor_publishes_every_action_outcome_in_execution_order
fn supervisor_publishes_every_action_outcome_in_execution_order() {
    let root = fixture();
    let runner = FakeUnitRunner {
        calls: RefCell::new(Vec::new()),
        fail: false,
    };
    let decision = SupervisorDecision {
        state: SupervisorState::Critical,
        healthy_samples: 0,
        cache_recovery_samples: 0,
        actions: vec![
            SupervisorAction::ReduceVramCache,
            SupervisorAction::CloseAdmission,
        ],
    };
    let action_results = execute_actions_with(
        &decision,
        &ActionPaths {
            runtime: &root.join("runtime"),
            ledger: &root.join("missing-ledger.json"),
            daemon_instance_id: None,
            issued_at_unix_ms: None,
        },
        &runner,
        || Ok(()),
    );
    let state = root.join("supervisor-state.json");
    let identity = OwnerIdentity::current().unwrap();
    publish_at(&state, &decision, &action_results, None, &identity, 1_000).unwrap();
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&state).unwrap()).unwrap();

    assert_eq!(value["schema_version"], 3);
    assert!(
        value.get("last_action").is_none(),
        "the lossy compatibility field must be removed"
    );
    let results = value["action_results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["action"], "reduce_vram_cache");
    assert_eq!(results[0]["status"], "failed");
    assert!(results[0]["error"].as_str().unwrap().contains("daemon"));
    assert_eq!(results[1]["action"], "close_admission");
    assert_eq!(results[1]["status"], "succeeded");
    assert!(results[1]["error"].is_null());
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: stale_invocation_id_refuses_all_systemctl_actions
fn stale_invocation_id_refuses_all_systemctl_actions() {
    let root = fixture();
    let runtime = root.join("runtime");
    let ledger = root.join("reservations.json");
    fs::create_dir_all(&runtime).unwrap();
    fs::write(
        &ledger,
        ledger_fixture(&[("batch", 4096, "ramshared-batch-fixture.scope")]),
    )
    .unwrap();
    fs::write(
        runtime.join("frozen-scope.json"),
        serde_json::json!({
            "schema_version": 3,
            "phase": "pending",
            "unit": "ramshared-batch-fixture.scope",
            "invocation_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "updated_at_unix_ms": 1_000,
        })
        .to_string(),
    )
    .unwrap();
    let runner = StaleIdentityRunner {
        calls: RefCell::new(Vec::new()),
        identity_queries: RefCell::new(0),
    };
    let decision = SupervisorDecision {
        state: SupervisorState::Emergency,
        healthy_samples: 0,
        cache_recovery_samples: 0,
        actions: vec![
            SupervisorAction::FreezeDiscardable,
            SupervisorAction::ThawDiscardable,
            SupervisorAction::TerminateDiscardable,
            SupervisorAction::KillDiscardable,
        ],
    };

    let results = execute_actions_with(
        &decision,
        &ActionPaths {
            runtime: &runtime,
            ledger: &ledger,
            daemon_instance_id: None,
            issued_at_unix_ms: Some(1_000),
        },
        &runner,
        || Ok(()),
    );

    assert_eq!(results.len(), 4);
    assert!(
        results
            .iter()
            .all(|result| result.status == SupervisorActionStatus::Failed)
    );
    assert!(results[..3].iter().all(|result| {
        result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("InvocationID changed"))
    }));
    assert!(
        results[3]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("successful TERM outcome"))
    );
    assert_eq!(*runner.identity_queries.borrow(), 3);
    assert!(runner.calls.borrow().is_empty());
    assert!(runtime.join("frozen-scope.json").exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: unsupported_ledger_schema_refuses_scope_cleanup_without_runner_action
fn unsupported_ledger_schema_refuses_scope_cleanup_without_runner_action() {
    let root = fixture();
    let ledger = root.join("reservations.json");
    let contents = serde_json::json!({
        "schema_version": 999,
        "next_ordinal": 2,
        "reservations": [{
            "id": "fixture-reservation",
            "owner": {
                "boot_id": "fixture-boot",
                "pid": 1,
                "start_time": 1,
                "nonce": "fixture-nonce",
            },
            "class": "batch",
            "memory_bytes": 4096,
            "unit": "ramshared-batch-fixture.scope",
            "invocation_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "issued_ordinal": 1,
        }],
    })
    .to_string();
    fs::write(&ledger, &contents).unwrap();
    let runner = FakeUnitRunner {
        calls: RefCell::new(Vec::new()),
        fail: false,
    };
    let decision = SupervisorDecision {
        state: SupervisorState::Emergency,
        healthy_samples: 0,
        cache_recovery_samples: 0,
        actions: vec![
            SupervisorAction::FreezeDiscardable,
            SupervisorAction::TerminateDiscardable,
            SupervisorAction::KillDiscardable,
        ],
    };
    let results = execute_actions_with(
        &decision,
        &ActionPaths {
            runtime: &root.join("runtime"),
            ledger: &ledger,
            daemon_instance_id: None,
            issued_at_unix_ms: Some(1_000),
        },
        &runner,
        || Ok(()),
    );
    assert_eq!(results.len(), 3);
    assert!(
        results
            .iter()
            .all(|result| result.status == SupervisorActionStatus::Failed)
    );
    assert!(results[..2].iter().all(|result| {
        result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("unsupported reservation ledger schema"))
    }));
    assert!(
        results[2]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("successfully terminated"))
    );
    assert!(runner.calls.borrow().is_empty());
    assert_eq!(fs::read_to_string(&ledger).unwrap(), contents);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn critical_requests_require_a_fresh_daemon_cache_status() {
    let root = fixture();
    let runtime = root.join("runtime");
    fs::create_dir_all(&runtime).unwrap();
    fs::write(
        runtime.join("cache-status.json"),
        r#"{"schema_version":1,"daemon_instance_id":"daemon-42","written_at_unix_ms":1000}"#,
    )
    .unwrap();
    assert_eq!(
        request_daemon_instance_at(&runtime, 1_001).unwrap(),
        "daemon-42"
    );
    assert!(request_daemon_instance_at(&runtime, 16_001).is_err());
    fs::write(runtime.join("cache-status.json"), "not-json").unwrap();
    assert!(request_daemon_instance_at(&runtime, 1_001).is_err());
    assert!(!valid_daemon_instance_id("foreign/daemon"));
    fs::remove_dir_all(root).unwrap();
}

/// A body a peer can rewrite is not the daemon's body, and neither is one
/// reached through a link a peer can repoint. Both are refused before any
/// `daemon_instance_id` is read out of them — the bare `read_to_string`
/// this replaces accepted both, and the id it found went straight into a
/// critical control request.
///
/// The modes are set after the write because the create-time mode is
/// filtered through the umask and would silently land back on 0644. These
/// are the two trust halves a single-uid host can stage through the
/// filesystem; the owner-matches-parent half is pinned on the pure decision
/// in [`crate::published_budget`].
#[test]
fn critical_requests_refuse_an_untrusted_cache_status() {
    use std::os::unix::fs::PermissionsExt;

    let root = fixture();
    let runtime = root.join("runtime");
    fs::create_dir_all(&runtime).unwrap();
    let path = runtime.join("cache-status.json");
    let body = r#"{"schema_version":1,"daemon_instance_id":"daemon-42","written_at_unix_ms":1000}"#;

    fs::write(&path, body).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
    assert!(
        request_daemon_instance_at(&runtime, 1_001).is_err(),
        "a world-writable cache status must be refused before its id is used"
    );

    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let target = runtime.join("well-formed.json");
    fs::write(&target, body).unwrap();
    fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert!(
        request_daemon_instance_at(&runtime, 1_001).is_err(),
        "a cache status reached through a symlink must be refused"
    );

    // And the same directory with an honest document still answers — the
    // refusal above is about trust, not about the path being broken.
    fs::remove_file(&path).unwrap();
    fs::write(&path, body).unwrap();
    assert_eq!(
        request_daemon_instance_at(&runtime, 1_001).unwrap(),
        "daemon-42"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn action_executor_records_refusals_without_touching_a_unit() {
    let root = fixture();
    let runner = FakeUnitRunner {
        calls: RefCell::new(Vec::new()),
        fail: true,
    };
    let decision = SupervisorDecision {
        state: SupervisorState::Critical,
        healthy_samples: 0,
        cache_recovery_samples: 0,
        actions: vec![SupervisorAction::FreezeDiscardable],
    };
    let unused_ledger = root.join("never-used/reservations.json");
    let paths = ActionPaths {
        runtime: &root,
        ledger: &unused_ledger,
        daemon_instance_id: None,
        issued_at_unix_ms: Some(1_000),
    };
    let unused = execute_actions_with(&decision, &paths, &runner, || Ok(()));
    assert_eq!(unused.len(), 1);
    assert!(
        unused[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("no_discardable_scope")),
        "an authority that never issued a reservation is an empty victim \
             set, not a ledger failure: {:?}",
        unused[0].error
    );
    assert!(
        runner.calls.borrow().is_empty(),
        "no victim means no systemctl call"
    );

    let corrupt_ledger = root.join("corrupt");
    fs::create_dir_all(&corrupt_ledger).unwrap();
    fs::write(corrupt_ledger.join("reservations.json"), "not-json").unwrap();
    let corrupt = execute_actions_with(
        &decision,
        &ActionPaths {
            runtime: &root,
            ledger: &corrupt_ledger.join("reservations.json"),
            daemon_instance_id: None,
            issued_at_unix_ms: Some(1_000),
        },
        &runner,
        || Ok(()),
    );
    assert_eq!(corrupt.len(), 1);
    assert!(
        corrupt[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("reservation ledger is malformed")),
        "a corrupt ledger must still refuse: {:?}",
        corrupt[0].error
    );
    assert!(runner.calls.borrow().is_empty());

    let ledger = root.join("reservations.json");
    fs::write(
        &ledger,
        ledger_fixture(&[("batch", 1, "ramshared-batch-fixture.scope")]),
    )
    .unwrap();
    let failed = execute_actions_with(
        &decision,
        &ActionPaths {
            runtime: &root,
            ledger: &ledger,
            daemon_instance_id: None,
            issued_at_unix_ms: Some(1_000),
        },
        &runner,
        || Ok(()),
    );
    assert_eq!(failed.len(), 1);
    assert!(
        failed[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("fixture systemctl refusal"))
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: freeze_discardable_without_managed_reservations_is_not_a_ledger_failure
fn freeze_discardable_without_managed_reservations_is_not_a_ledger_failure() {
    // The reproduced symptom (customer-installation IMPL row 2026-10-05):
    // CRITICAL issued `close_admission`, `reduce_vram_cache` and
    // `request_reclaim` successfully, and `freeze_discardable` reported
    // `reservation ledger is unavailable` while `managed_reservations`
    // was 0. The supervisor was not broken and the ledger was not
    // corrupt: the reservation table had simply never been issued this
    // boot, and the action named a failure that had not happened.
    //
    // Both halves of the defect are pinned here. An empty table (the
    // document exists and holds no reservation) and an uninitialized
    // authority (no document yet) must report the same honest refusal,
    // and neither may ever surface as a ledger error.
    let root = fixture();
    let runner = FakeUnitRunner {
        calls: RefCell::new(Vec::new()),
        fail: false,
    };
    let decision = SupervisorDecision {
        state: SupervisorState::Critical,
        healthy_samples: 0,
        cache_recovery_samples: 0,
        actions: vec![SupervisorAction::FreezeDiscardable],
    };
    for (label, ledger) in [
        ("uninitialized", root.join("never-used/reservations.json")),
        ("empty", {
            let empty = root.join("empty");
            fs::create_dir_all(&empty).unwrap();
            fs::write(empty.join("reservations.json"), ledger_fixture(&[])).unwrap();
            empty.join("reservations.json")
        }),
    ] {
        let outcomes = execute_actions_with(
            &decision,
            &ActionPaths {
                runtime: &root,
                ledger: &ledger,
                daemon_instance_id: None,
                issued_at_unix_ms: Some(1_000),
            },
            &runner,
            || Ok(()),
        );
        assert_eq!(outcomes.len(), 1, "{label}");
        assert_eq!(
            outcomes[0].action,
            SupervisorAction::FreezeDiscardable,
            "{label}"
        );
        assert_eq!(
            outcomes[0].status,
            SupervisorActionStatus::Failed,
            "{label}: nothing was frozen, so the freeze did not happen"
        );
        let error = outcomes[0].error.as_deref().unwrap_or_default();
        assert!(
            error.contains("no_discardable_scope"),
            "{label} must name the real condition, got {error:?}"
        );
        assert!(
            !error.contains("reservation ledger"),
            "{label} must not invent a ledger failure, got {error:?}"
        );
        assert!(
            runner.calls.borrow().is_empty(),
            "{label} must not touch a unit without a victim"
        );
    }
    fs::remove_dir_all(root).unwrap();
}
