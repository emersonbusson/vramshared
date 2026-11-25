// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/supervisor/tests/recovery_suite.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

#[test]
fn runtime_cycle_and_apply_error_paths_preserve_state_and_report_failures() {
    assert_eq!(parse_psi_full_avg10("full avg10=1.5 avg10=2.0"), None);

    let root = fixture();
    let runtime = root.join("runtime");
    let ledger_root = root.join("admission");
    let state_path = root.join("state.json");
    let identity = OwnerIdentity::current().unwrap();
    let runner = FakeUnitRunner {
        calls: RefCell::new(Vec::new()),
        fail: false,
    };
    let mut supervisor = Supervisor::default();

    // Healthy tick publishes state and admission gate cleanly in a temp tree.
    run_cycle_with(
        &runtime,
        &ledger_root,
        &state_path,
        &mut supervisor,
        &identity,
        &runner,
        || {
            Ok((
                PressureSample {
                    memory_available_bytes: 2 * GIB,
                    control_reserve_bytes: 512 * 1024 * 1024,
                    psi_full_avg10: 0.0,
                    sample_delay_ms: 0,
                    elapsed_ms: 1_000,
                },
                10_000,
            ))
        },
    )
    .unwrap();
    assert!(state_path.exists());

    // Passing a directory as `state_path` surfaces `DecisionApplyError` on both
    // Healthy and Guarded decisions and propagates out of `run_cycle_with`.
    let bad_state_dir = root.join("state-as-dir");
    fs::create_dir_all(&bad_state_dir).unwrap();
    let healthy = SupervisorDecision {
        state: SupervisorState::Healthy,
        actions: Vec::new(),
        healthy_samples: 1,
    };
    let paths = ActionPaths {
        runtime: &runtime,
        ledger: &ledger_root.join("reservations.json"),
        daemon_instance_id: None,
        issued_at_unix_ms: Some(11_000),
    };
    let healthy_pub_err = apply_decision_with(
        &bad_state_dir,
        &ledger_root,
        &healthy,
        &paths,
        &runner,
        &identity,
        11_000,
    )
    .unwrap_err();
    assert!(!healthy_pub_err.to_string().is_empty());

    // Deterministic ledger failure under a Healthy decision aborts the cycle.
    let bad_ledger = root.join("ledger-as-file");
    fs::write(&bad_ledger, "not a directory").unwrap();
    let healthy_ledger_err = apply_decision_with(
        &state_path,
        &bad_ledger,
        &healthy,
        &paths,
        &runner,
        &identity,
        12_000,
    )
    .unwrap_err();
    assert!(!healthy_ledger_err.to_string().is_empty());

    // Guarded decision propagates state publish error through `run_cycle_with`.
    let guarded_err = run_cycle_with(
        &runtime,
        &ledger_root,
        &bad_state_dir,
        &mut supervisor,
        &identity,
        &runner,
        || {
            Ok((
                PressureSample {
                    memory_available_bytes: 256 * 1024 * 1024,
                    control_reserve_bytes: 512 * 1024 * 1024,
                    psi_full_avg10: 0.0,
                    sample_delay_ms: 0,
                    elapsed_ms: 2_000,
                },
                13_000,
            ))
        },
    )
    .unwrap_err();
    assert!(
        guarded_err.contains("definitive state publication failed"),
        "{guarded_err}"
    );

    // Missing timestamps and absent victims refuse deterministically in actions.
    let no_ts_paths = ActionPaths {
        runtime: &runtime,
        ledger: &ledger_root.join("reservations.json"),
        daemon_instance_id: None,
        issued_at_unix_ms: None,
    };
    let err_results = execute_actions_with(
        &SupervisorDecision {
            state: SupervisorState::Emergency,
            actions: vec![
                SupervisorAction::FreezeDiscardable,
                SupervisorAction::ThawDiscardable,
                SupervisorAction::TerminateDiscardable,
                SupervisorAction::KillDiscardable,
                SupervisorAction::ReduceVramCache,
                SupervisorAction::RequestReclaim,
            ],
            healthy_samples: 0,
        },
        &no_ts_paths,
        &runner,
        || Ok(()),
    );
    assert!(
        err_results
            .iter()
            .all(|r| r.status == SupervisorActionStatus::Failed)
    );
    assert!(query_unit_invocation_id("not-a-valid-unit").is_err());

    fs::remove_dir_all(root).unwrap();
}
