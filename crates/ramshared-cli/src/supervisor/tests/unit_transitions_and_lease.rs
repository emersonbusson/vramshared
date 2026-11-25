// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/supervisor/tests/unit_transitions_and_lease.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

#[test]
// TestName: supervisor_fixture_roots_are_unique_and_avoid_shared_disk_sync
fn supervisor_fixture_roots_are_unique_and_avoid_shared_disk_sync() {
    let first = fixture();
    let second = fixture();
    assert_ne!(first, second);
    if std::path::Path::new("/dev/shm").is_dir() {
        assert_eq!(first.parent(), Some(std::path::Path::new("/dev/shm")));
        assert_eq!(second.parent(), Some(std::path::Path::new("/dev/shm")));
    }
    fs::remove_dir_all(first).unwrap();
    fs::remove_dir_all(second).unwrap();
}

#[test]
// TestName: supervisor_parallel_guarded_transitions_stay_durable_and_isolated
fn supervisor_parallel_guarded_transitions_stay_durable_and_isolated() {
    std::thread::scope(|scope| {
        let workers = (0..8)
            .map(|_| scope.spawn(run_isolated_guarded_transition_fixture))
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().unwrap();
        }
    });
}

#[test]
fn supervisor_transitions_and_hysteresis_are_exact() {
    let mut supervisor = Supervisor::default();
    for index in 0..2 {
        assert_eq!(
            supervisor.observe(sample(6, 4, 2.0, 0, index * 1000)).state,
            SupervisorState::Healthy
        );
    }
    assert_eq!(
        supervisor.observe(sample(6, 4, 2.0, 0, 2000)).state,
        SupervisorState::Guarded
    );
    for index in 3..7 {
        supervisor.observe(sample(6, 4, 5.0, 0, index * 1000));
    }
    assert_eq!(
        supervisor.observe(sample(6, 4, 5.0, 0, 7000)).state,
        SupervisorState::Critical
    );
    for index in 8..12 {
        supervisor.observe(sample(6, 4, 10.0, 0, index * 1000));
    }
    assert_eq!(
        supervisor.observe(sample(6, 4, 10.0, 0, 12000)).state,
        SupervisorState::Emergency
    );
    for index in 0..59 {
        assert_eq!(
            supervisor
                .observe(sample(6, 4, 0.0, 0, 13000 + index * 1000))
                .state,
            SupervisorState::Guarded
        );
    }
    assert_eq!(
        supervisor.observe(sample(6, 4, 0.0, 0, 72000)).state,
        SupervisorState::Healthy
    );
}

#[test]
fn critical_keeps_vram_cache_active_while_emergency_revokes_it() {
    let mut supervisor = Supervisor::default();
    let critical = supervisor.observe(sample(2, 4, 0.0, 0, 0));
    assert_eq!(critical.state, SupervisorState::Critical);
    assert!(
        !critical
            .actions
            .contains(&SupervisorAction::ReduceVramCache),
        "CRITICAL guest RAM pressure must keep Tier 2 VRAM cache active"
    );
    assert!(critical.actions.contains(&SupervisorAction::CloseAdmission));
    assert!(critical.actions.contains(&SupervisorAction::RequestReclaim));

    let emergency = supervisor.observe(sample(1, 4, 0.0, 0, 1_000));
    assert_eq!(emergency.state, SupervisorState::Emergency);
    assert!(
        emergency
            .actions
            .contains(&SupervisorAction::ReduceVramCache),
        "EMERGENCY control-plane starvation must still revoke clean VRAM cache"
    );
}

#[test]
fn supervisor_delay_enters_emergency() {
    let mut supervisor = Supervisor::default();
    let decision = supervisor.observe(sample(6, 4, 0.0, 3000, 0));
    assert_eq!(decision.state, SupervisorState::Emergency);
    assert!(
        decision
            .actions
            .contains(&SupervisorAction::ReduceVramCache)
    );
    assert!(decision.actions.contains(&SupervisorAction::RequestReclaim));
    assert!(
        decision
            .actions
            .contains(&SupervisorAction::TerminateDiscardable)
    );
    supervisor.commit_action_results(
        &[SupervisorActionResult::from_result(
            SupervisorAction::TerminateDiscardable,
            Ok(()),
        )],
        0,
    );
    let decision = supervisor.observe(sample(6, 4, 0.0, 3000, 5000));
    assert!(
        decision
            .actions
            .contains(&SupervisorAction::KillDiscardable)
    );
    assert!(
        decision
            .actions
            .contains(&SupervisorAction::ReduceVramCache)
    );
    assert!(decision.actions.contains(&SupervisorAction::RequestReclaim));
}

#[test]
// TestName: supervisor_state_advances_only_from_successful_action_results
fn supervisor_state_advances_only_from_successful_action_results() {
    let mut supervisor = Supervisor::default();
    let first = supervisor.observe(sample(1, 4, 0.0, 0, 0));
    assert!(
        first
            .actions
            .contains(&SupervisorAction::TerminateDiscardable)
    );

    let retry = supervisor.observe(sample(1, 4, 0.0, 0, 1_000));
    assert!(
        retry
            .actions
            .contains(&SupervisorAction::TerminateDiscardable),
        "an uncommitted TERM intent suppressed the required retry"
    );

    let mut freeze = Supervisor::default();
    let critical = freeze.observe(sample(2, 4, 0.0, 0, 0));
    assert!(
        critical
            .actions
            .contains(&SupervisorAction::FreezeDiscardable)
    );
    let emergency = freeze.observe(sample(1, 4, 0.0, 0, 1_000));
    assert!(
        !emergency
            .actions
            .contains(&SupervisorAction::ThawDiscardable),
        "an uncommitted freeze intent created a synthetic thaw"
    );
}

#[test]
fn sustained_emergency_renews_zero_cache_and_reclaim_requests() {
    let root = fixture();
    let runtime = root.join("runtime");
    let ledger = root.join("reservations.json");
    let runner = FakeUnitRunner {
        calls: RefCell::new(Vec::new()),
        fail: false,
    };
    let mut supervisor = Supervisor::default();

    let first = supervisor.observe(sample(6, 4, 0.0, 3_000, 0));
    execute_actions_with(
        &first,
        &ActionPaths {
            runtime: &runtime,
            ledger: &ledger,
            daemon_instance_id: Some("fixture-daemon"),
            issued_at_unix_ms: Some(1_000),
        },
        &runner,
        || Ok(()),
    );

    let sustained = supervisor.observe(sample(6, 4, 0.0, 3_000, 5_000));
    assert!(
        sustained
            .actions
            .contains(&SupervisorAction::ReduceVramCache)
    );
    assert!(
        sustained
            .actions
            .contains(&SupervisorAction::RequestReclaim)
    );
    execute_actions_with(
        &sustained,
        &ActionPaths {
            runtime: &runtime,
            ledger: &ledger,
            daemon_instance_id: Some("fixture-daemon"),
            issued_at_unix_ms: Some(2_000),
        },
        &runner,
        || Ok(()),
    );

    let target: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(runtime.join("cache-target.json"))
            .unwrap_or_else(|error| panic!("read renewed target: {error}")),
    )
    .unwrap_or_else(|error| panic!("parse renewed target: {error}"));
    let reclaim: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(runtime.join("reclaim-request.json"))
            .unwrap_or_else(|error| panic!("read renewed reclaim: {error}")),
    )
    .unwrap_or_else(|error| panic!("parse renewed reclaim: {error}"));
    assert_eq!(target["target_bytes"], 0);
    assert_eq!(target["issued_at_unix_ms"], 2_000);
    assert_eq!(reclaim["issued_at_unix_ms"], 2_000);
    fs::remove_dir_all(root).unwrap_or_else(|error| panic!("remove fixture: {error}"));
}

#[test]
fn recovery_samples_never_repeat_destructive_actions() {
    let mut supervisor = Supervisor::default();
    let first = supervisor.observe(sample(6, 4, 0.0, 3000, 0));
    assert!(
        first
            .actions
            .contains(&SupervisorAction::TerminateDiscardable)
    );

    let recovering = supervisor.observe(sample(6, 4, 0.0, 0, 6_000));
    assert_eq!(recovering.state, SupervisorState::Guarded);
    assert_eq!(recovering.actions, vec![SupervisorAction::CloseAdmission]);

    for index in 1..59 {
        let decision = supervisor.observe(sample(6, 4, 0.0, 0, 6_000 + index * 1_000));
        assert!(!decision.actions.iter().any(|action| matches!(
            action,
            SupervisorAction::TerminateDiscardable | SupervisorAction::KillDiscardable
        )));
    }
    let recovered = supervisor.observe(sample(6, 4, 0.0, 0, 65_000));
    assert_eq!(recovered.state, SupervisorState::Healthy);
    assert!(recovered.actions.is_empty());
}

#[test]
fn frozen_scope_is_thawed_before_emergency_termination() {
    let mut supervisor = Supervisor::default();
    let critical = supervisor.observe(sample(2, 4, 0.0, 0, 0));
    assert!(
        critical
            .actions
            .contains(&SupervisorAction::FreezeDiscardable)
    );
    supervisor.commit_action_results(
        &[SupervisorActionResult::from_result(
            SupervisorAction::FreezeDiscardable,
            Ok(()),
        )],
        0,
    );

    let emergency = supervisor.observe(sample(1, 4, 0.0, 0, 1_000));
    let thaw = emergency
        .actions
        .iter()
        .position(|action| action == &SupervisorAction::ThawDiscardable);
    let term = emergency
        .actions
        .iter()
        .position(|action| action == &SupervisorAction::TerminateDiscardable);
    assert!(matches!((thaw, term), (Some(thaw), Some(term)) if thaw < term));
}

#[test]
fn discard_priority_is_deterministic() {
    let mut classes = ["interactive", "batch", "build", "browser-test"];
    classes.sort_by_key(|class| discard_priority(class));
    assert_eq!(classes, ["batch", "browser-test", "build", "interactive"]);
}

#[test]
fn exact_memory_thresholds_take_priority_over_psi_streaks() {
    let mut supervisor = Supervisor::default();
    assert_eq!(
        supervisor.observe(sample(3, 4, 0.0, 0, 0)).state,
        SupervisorState::Guarded
    );
    assert_eq!(
        supervisor.observe(sample(2, 4, 0.0, 0, 1000)).state,
        SupervisorState::Critical
    );
    assert_eq!(
        supervisor.observe(sample(1, 4, 0.0, 0, 2000)).state,
        SupervisorState::Emergency
    );
}

#[test]
fn sanitized_victim_selection_uses_class_then_size() {
    let root = fixture();
    let ledger = root.join("reservations.json");
    fs::write(
        &ledger,
        ledger_fixture(&[
            ("build", 900, "ramshared-build-a.scope"),
            ("batch", 100, "ramshared-batch-small.scope"),
            ("batch", 800, "ramshared-batch-large.scope"),
            ("batch", 999, "foreign.service"),
        ]),
    )
    .unwrap();
    assert_eq!(
        select_victim(&ledger).unwrap().map(|victim| victim.unit),
        Some("ramshared-batch-large.scope".into())
    );
    assert!(valid_scope_unit("ramshared-build-a.scope"));
    assert!(!valid_scope_unit("../ramshared-build-a.scope"));
    assert!(!valid_scope_unit("foreign.service"));
    assert_eq!(discard_priority("unknown"), None);
    // An authority that has never issued a reservation is an empty table,
    // not a ledger failure. Before DT-2's initializer this returned
    // `reservation ledger is unavailable`, which is what made a CRITICAL
    // `freeze_discardable` report a fabricated failure on a host with
    // `managed_reservations == 0`.
    assert_eq!(
        select_victim(&root.join("never-used/reservations.json")).unwrap(),
        None,
        "an uninitialized authority has no victim and is not an error"
    );
    // A document that exists and is unreadable still fails closed.
    let corrupt = root.join("corrupt");
    fs::create_dir_all(&corrupt).unwrap();
    fs::write(corrupt.join("reservations.json"), "not-json").unwrap();
    assert!(
        select_victim(&corrupt.join("reservations.json"))
            .unwrap_err()
            .contains("reservation ledger is malformed"),
        "a corrupt ledger must still refuse"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: discard_priority_uses_durable_older_reservation_ordinal_tie_break
fn discard_priority_uses_durable_older_reservation_ordinal_tie_break() {
    let root = fixture();
    let ledger = root.join("reservations.json");
    let reservation = |id: &str, unit: &str, issued_ordinal: u64| {
        serde_json::json!({
            "id": id,
            "owner": {
                "boot_id": "fixture-boot",
                "pid": issued_ordinal as u32 + 1,
                "start_time": issued_ordinal + 1,
                "nonce": format!("fixture-{issued_ordinal}"),
            },
            "class": "batch",
            "memory_bytes": 4096,
            "unit": unit,
            "invocation_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "issued_ordinal": issued_ordinal,
        })
    };
    fs::write(
        &ledger,
        serde_json::json!({
            "schema_version": workload::RESERVATION_LEDGER_SCHEMA_VERSION,
            "next_ordinal": 3,
            "reservations": [
                reservation("older", "ramshared-batch-z.scope", 1),
                reservation("newer", "ramshared-batch-a.scope", 2),
            ],
        })
        .to_string(),
    )
    .unwrap();
    let selected = select_victim(&ledger).unwrap().unwrap().unit;
    fs::remove_dir_all(root).unwrap();

    assert_eq!(
        selected, "ramshared-batch-z.scope",
        "unit spelling replaced the durable older-token tie break"
    );
}

#[test]
fn action_executor_covers_cache_freeze_reclaim_thaw_and_kill() {
    let root = fixture();
    let runtime = root.join("runtime");
    let ledger = root.join("reservations.json");
    fs::write(
        &ledger,
        ledger_fixture(&[("batch", 4096, "ramshared-batch-fixture.scope")]),
    )
    .unwrap();
    let runner = FakeUnitRunner {
        calls: RefCell::new(Vec::new()),
        fail: false,
    };
    let decision = SupervisorDecision {
        state: SupervisorState::Emergency,
        healthy_samples: 0,
        actions: vec![
            SupervisorAction::CloseAdmission,
            SupervisorAction::ReduceVramCache,
            SupervisorAction::FreezeDiscardable,
            SupervisorAction::RequestReclaim,
            SupervisorAction::ThawDiscardable,
            SupervisorAction::TerminateDiscardable,
        ],
    };
    let outcomes = execute_actions_with(
        &decision,
        &ActionPaths {
            runtime: &runtime,
            ledger: &ledger,
            daemon_instance_id: Some("fixture-daemon"),
            issued_at_unix_ms: Some(1_000),
        },
        &runner,
        || Ok(()),
    );
    assert_eq!(outcomes.len(), decision.actions.len());
    assert!(
        outcomes
            .iter()
            .all(|outcome| outcome.status == SupervisorActionStatus::Succeeded)
    );
    assert_eq!(
        outcomes.last().map(|outcome| outcome.action),
        Some(SupervisorAction::TerminateDiscardable)
    );
    let kill = execute_actions_with(
        &SupervisorDecision {
            state: SupervisorState::Emergency,
            healthy_samples: 0,
            actions: vec![SupervisorAction::KillDiscardable],
        },
        &ActionPaths {
            runtime: &runtime,
            ledger: &ledger,
            daemon_instance_id: None,
            issued_at_unix_ms: Some(6_000),
        },
        &runner,
        || Ok(()),
    );
    assert_eq!(kill[0].status, SupervisorActionStatus::Succeeded);
    let cache_target: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(runtime.join("cache-target.json")).unwrap())
            .unwrap();
    assert_eq!(cache_target["target_bytes"], 0);
    assert_eq!(cache_target["daemon_instance_id"], "fixture-daemon");
    assert_eq!(cache_target["issued_at_unix_ms"], 1_000);
    let reclaim_request: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(runtime.join("reclaim-request.json")).unwrap())
            .unwrap();
    assert_eq!(reclaim_request["daemon_instance_id"], "fixture-daemon");
    assert_eq!(reclaim_request["issued_at_unix_ms"], 1_000);
    assert!(!runtime.join("frozen-scope.json").exists());
    let calls = runner.calls.borrow();
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[0][0], "freeze");
    assert_eq!(calls[1][0], "thaw");
    assert_eq!(calls[2][2], "--signal=TERM");
    assert_eq!(calls[3][2], "--signal=KILL");
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: freeze_persists_exact_pending_identity_before_effect_and_never_falls_back
fn freeze_persists_exact_pending_identity_before_effect_and_never_falls_back() {
    let root = fixture();
    let runtime = root.join("runtime");
    let frozen_path = runtime.join("frozen-scope.json");
    let ledger = root.join("reservations.json");
    fs::write(
        &ledger,
        ledger_fixture(&[("batch", 4096, "ramshared-batch-fixture.scope")]),
    )
    .unwrap();
    let freeze_runner = FreezeEvidenceRunner {
        frozen_path: frozen_path.clone(),
        saw_pending: RefCell::new(false),
    };
    let freeze = execute_actions_with(
        &SupervisorDecision {
            state: SupervisorState::Critical,
            healthy_samples: 0,
            actions: vec![SupervisorAction::FreezeDiscardable],
        },
        &ActionPaths {
            runtime: &runtime,
            ledger: &ledger,
            daemon_instance_id: None,
            issued_at_unix_ms: Some(1_000),
        },
        &freeze_runner,
        || Ok(()),
    );
    let pending_survived = frozen_path.is_file();

    if frozen_path.exists() {
        fs::remove_file(&frozen_path).unwrap();
    }
    let thaw_runner = FakeUnitRunner {
        calls: RefCell::new(Vec::new()),
        fail: false,
    };
    let thaw = execute_actions_with(
        &SupervisorDecision {
            state: SupervisorState::Emergency,
            healthy_samples: 0,
            actions: vec![SupervisorAction::ThawDiscardable],
        },
        &ActionPaths {
            runtime: &runtime,
            ledger: &ledger,
            daemon_instance_id: None,
            issued_at_unix_ms: Some(2_000),
        },
        &thaw_runner,
        || Ok(()),
    );
    let thaw_calls = thaw_runner.calls.borrow().len();
    fs::remove_dir_all(root).unwrap();

    assert_eq!(freeze[0].status, SupervisorActionStatus::Failed);
    assert!(
        *freeze_runner.saw_pending.borrow(),
        "freeze effect ran before durable pending identity evidence"
    );
    assert!(
        pending_survived,
        "failed freeze discarded the only exact recovery identity"
    );
    assert_eq!(thaw[0].status, SupervisorActionStatus::Failed);
    assert_eq!(
        thaw_calls, 0,
        "thaw fell back to a current ledger victim without frozen identity evidence"
    );
}

#[test]
// TestName: freeze_applied_write_failure_recovers_exact_pending_identity_after_restart
fn freeze_applied_write_failure_recovers_exact_pending_identity_after_restart() {
    let root = fixture();
    let runtime = root.join("runtime");
    let ledger = root.join("reservations.json");
    fs::write(
        &ledger,
        ledger_fixture(&[("batch", 4096, "ramshared-batch-original.scope")]),
    )
    .unwrap();
    let freeze_runner = FakeUnitRunner {
        calls: RefCell::new(Vec::new()),
        fail: false,
    };
    let write_hook = RuntimeWriteHookGuard::install(Box::new(|name, value| {
        if name == "frozen-scope.json" && value["phase"] == "applied" {
            Err("injected applied-record durability failure".into())
        } else {
            Ok(())
        }
    }));
    let freeze = execute_actions_with(
        &SupervisorDecision {
            state: SupervisorState::Critical,
            healthy_samples: 0,
            actions: vec![SupervisorAction::FreezeDiscardable],
        },
        &ActionPaths {
            runtime: &runtime,
            ledger: &ledger,
            daemon_instance_id: None,
            issued_at_unix_ms: Some(1_000),
        },
        &freeze_runner,
        || Ok(()),
    );
    drop(write_hook);
    assert_eq!(freeze[0].status, SupervisorActionStatus::Failed);
    let pending: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(runtime.join("frozen-scope.json")).unwrap())
            .unwrap();
    assert_eq!(pending["phase"], "pending");
    assert_eq!(pending["unit"], "ramshared-batch-original.scope");

    fs::write(
        &ledger,
        ledger_fixture(&[("batch", 8192, "ramshared-batch-replacement.scope")]),
    )
    .unwrap();
    let mut reconstructed = Supervisor::default();
    reconstructed
        .recover_action_state(&runtime, 2_000, 100)
        .unwrap();
    let decision = reconstructed.observe(sample(8, 4, 0.0, 0, 100));
    assert_eq!(decision.actions, vec![SupervisorAction::ThawDiscardable]);
    let thaw_runner = FakeUnitRunner {
        calls: RefCell::new(Vec::new()),
        fail: false,
    };
    let thaw = execute_actions_with(
        &decision,
        &ActionPaths {
            runtime: &runtime,
            ledger: &ledger,
            daemon_instance_id: None,
            issued_at_unix_ms: Some(2_000),
        },
        &thaw_runner,
        || Ok(()),
    );
    let calls = thaw_runner.calls.borrow();
    assert_eq!(thaw[0].status, SupervisorActionStatus::Succeeded);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0][0], "thaw");
    assert_eq!(calls[0][1], "ramshared-batch-original.scope");
    assert!(!runtime.join("frozen-scope.json").exists());
    drop(calls);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn unit_invocation_id_parser_and_query_reject_invalid_systemd_responses() {
    use super::super::actions::{parse_unit_invocation_id, query_unit_invocation_id};

    let unit = "ramshared-batch-valid.scope";
    let valid_inv = "0123456789abcdef0123456789abcdef";
    let ok = format!("Id={unit}\nInvocationID={valid_inv}\n");
    assert_eq!(parse_unit_invocation_id(unit, &ok).unwrap(), valid_inv);

    assert!(parse_unit_invocation_id(unit, "malformed-line").is_err());
    assert!(parse_unit_invocation_id(unit, "Other=1\n").is_err());
    assert!(parse_unit_invocation_id(unit, &format!("Id={unit}\nId={unit}\n")).is_err());
    assert!(
        parse_unit_invocation_id(unit, &format!("Id=other.scope\nInvocationID={valid_inv}\n"))
            .is_err()
    );
    assert!(parse_unit_invocation_id(unit, &format!("Id={unit}\nInvocationID=short\n")).is_err());
    assert!(query_unit_invocation_id("not-managed.service").is_err());
}
