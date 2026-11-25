// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/workload/tests/recovery_and_cleanup.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;

#[test]
fn exact_scope_status_parser_fails_closed_and_preserves_terminal_result() {
    let unit = "ramshared-interactive-fixture.scope";
    // A `.scope` has no main process. These are the properties systemd
    // actually projects onto one; `ExecMainCode` / `ExecMainStatus` are
    // service properties and are omitted from a scope's projection
    // entirely, which is the defect this parser used to encode.
    let valid = format!(
        "Id={unit}\nInvocationID={FIXTURE_INVOCATION_ID}\nLoadState=loaded\nActiveState=inactive\nSubState=dead\nResult=success\n"
    );
    let status = parse_scope_status(unit, &valid).unwrap();
    assert_eq!(status.id, unit);
    assert_eq!(status.invocation_id, FIXTURE_INVOCATION_ID);
    assert_eq!(
        status.lifecycle(),
        Ok(ScopeLifecycle::Terminal {
            result: "success".into()
        })
    );

    // `Result` is preserved verbatim: a scope whose command exited 7
    // still reports `Result=success`, so this field is the lifecycle
    // result and must never be read as a workload exit code.
    let failed = format!(
        "Id={unit}\nInvocationID={FIXTURE_INVOCATION_ID}\nLoadState=loaded\nActiveState=failed\nSubState=dead\nResult=signal\n"
    );
    let status = parse_scope_status(unit, &failed).unwrap();
    assert_eq!(
        status.lifecycle(),
        Ok(ScopeLifecycle::Terminal {
            result: "signal".into()
        })
    );

    // An empty InvocationID is a real value (systemd clears it once the
    // unit leaves the active state), not an omission and not a refusal.
    let cleared = format!(
        "Id={unit}\nInvocationID=\nLoadState=loaded\nActiveState=inactive\nSubState=dead\nResult=success\n"
    );
    let status = parse_scope_status(unit, &cleared).unwrap();
    assert!(!status.has_invocation_id());
    assert_eq!(
        status.lifecycle(),
        Ok(ScopeLifecycle::Terminal {
            result: "success".into()
        })
    );

    for malformed in [
        "not-a-property\n",
        "Unexpected=value\n",
        "Id=first\nId=second\n",
        "Id=ramshared-other.scope\n",
        "Id=ramshared-interactive-fixture.scope\nInvocationID=invalid\n",
        "Id=ramshared-interactive-fixture.scope\nInvocationID=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nLoadState=loaded\nActiveState=active\nSubState=running\nResult=success\nExecMainCode=exited\n",
        "Id=ramshared-interactive-fixture.scope\nInvocationID=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nLoadState=loaded\nActiveState=active\n",
    ] {
        assert!(
            parse_scope_status(unit, malformed).is_err(),
            "{malformed:?}"
        );
    }

    let active = ScopeStatus::active(unit);
    assert_eq!(active.lifecycle(), Ok(ScopeLifecycle::Running));
    let mut unloaded = ScopeStatus::active(unit);
    unloaded.load_state = "masked".into();
    assert!(unloaded.lifecycle().is_err());
    // A collected unit is terminal, not unknown: systemd keeps answering
    // for it after `CollectMode` reaps it.
    let mut collected = ScopeStatus::active(unit);
    collected.load_state = "not-found".into();
    collected.active_state = "inactive".into();
    collected.sub_state = "dead".into();
    assert_eq!(
        collected.lifecycle(),
        Ok(ScopeLifecycle::Terminal {
            result: "success".into()
        })
    );
    let mut unexpected = ScopeStatus::active(unit);
    unexpected.active_state = "stopping".into();
    assert!(unexpected.lifecycle().is_err());
}

#[test]
// TestName: scope_presence_is_the_process_state_not_the_unit_state
fn scope_presence_is_the_process_state_not_the_unit_state() {
    let unit = "ramshared-interactive-fixture.scope";
    let occupied = ScopeStatus::active(unit);
    assert_eq!(occupied.presence(), Ok(ScopePresence::Occupied));
    assert_eq!(occupied.lifecycle(), Ok(ScopeLifecycle::Running));
    // `active` says the unit is up; only `SubState` says whether anyone
    // is actually in the cgroup.
    assert_eq!(
        occupied.is_process_vacant(),
        Ok(false),
        "a live cgroup read as contained"
    );

    let mut empty_but_active = ScopeStatus::active(unit);
    empty_but_active.sub_state = "dead".into();
    assert_eq!(empty_but_active.presence(), Ok(ScopePresence::Empty));
    assert_eq!(empty_but_active.lifecycle(), Ok(ScopeLifecycle::Running));

    let terminal = ScopeStatus::terminal_success(unit);
    assert_eq!(terminal.presence(), Ok(ScopePresence::Empty));
    assert_eq!(
        terminal.is_process_vacant(),
        Ok(true),
        "a terminal scope is not contained"
    );

    let mut unknown = ScopeStatus::active(unit);
    unknown.sub_state = "waiting".into();
    let error = unknown.presence().unwrap_err();
    assert!(error.contains("scope sub state is waiting"), "{error}");
    assert!(
        unknown.is_process_vacant().is_err(),
        "an unknown process state must not be read as containment"
    );
}

#[test]
// TestName: empty_cgroup_after_reap_is_contained_but_start_in_flight_is_not
fn empty_cgroup_after_reap_is_contained_but_start_in_flight_is_not() {
    let unit = "ramshared-interactive-fixture.scope";
    // systemd can lag the unit-state flip after the last process exits:
    // the cgroup is already empty while `ActiveState` is still `active`
    // or `deactivating`. That is contained — the group holds nothing —
    // and the runner child's own exit status is already authoritative.
    for still_listed in ["active", "deactivating"] {
        let mut lagging = ScopeStatus::active(unit);
        lagging.active_state = still_listed.into();
        lagging.sub_state = "dead".into();
        assert!(lagging.empty_cgroup_is_contained(), "{still_listed}");
        assert_eq!(lagging.is_process_vacant(), Ok(true));
    }

    // A start or reload job in flight has a momentarily empty cgroup.
    // Releasing the reservation here would free the budget while the
    // workload is still about to land, so it is never containment.
    for in_flight in ["activating", "reloading"] {
        let mut starting = ScopeStatus::active(unit);
        starting.active_state = in_flight.into();
        starting.sub_state = "dead".into();
        assert!(!starting.empty_cgroup_is_contained(), "{in_flight}");
        assert_eq!(starting.is_process_vacant(), Ok(false));
    }

    // Post-start states only accept the empty cgroup; a scope still
    // holding processes is not contained no matter how far the unit
    // state has moved.
    for still_listed in ["active", "deactivating"] {
        let mut occupied = ScopeStatus::active(unit);
        occupied.active_state = still_listed.into();
        occupied.sub_state = "running".into();
        assert_eq!(
            occupied.is_process_vacant(),
            Ok(false),
            "{still_listed} scope holding processes read as contained"
        );
    }

    // Terminal and collected units are contained regardless of what the
    // cgroup observation says, because nothing can still be in them.
    let mut collected = ScopeStatus::active(unit);
    collected.load_state = "not-found".into();
    collected.active_state = "inactive".into();
    collected.sub_state = "running".into();
    assert_eq!(collected.is_process_vacant(), Ok(true));
}

#[test]
// TestName: only_the_ledger_busy_signature_is_treated_as_transient
fn only_the_ledger_busy_signature_is_treated_as_transient() {
    assert!(is_ledger_busy("reservation ledger transition is busy"));
    assert!(is_ledger_busy(
        "reservation ledger transition is busy or ambiguous"
    ));
    for deterministic in [
        "lock reservation ledger transition: Permission denied",
        "ledger authority is a symlink",
        "owner identity mismatch",
        "admission-state.json: No such file or directory",
        "",
    ] {
        assert!(
            !is_ledger_busy(deterministic),
            "deterministic ledger error misclassified as transient: {deterministic:?}"
        );
    }
}

#[test]
// TestName: deterministic_ledger_errors_fail_fast_after_one_attempt
fn deterministic_ledger_errors_fail_fast_after_one_attempt() {
    let attempts = AtomicUsize::new(0);
    let error = retry_transient_ledger_publish(|| {
        attempts.fetch_add(1, AtomicOrdering::SeqCst);
        Err("ledger authority is a symlink".to_string())
    })
    .unwrap_err();
    assert_eq!(error, "ledger authority is a symlink");
    assert_eq!(
        attempts.load(AtomicOrdering::SeqCst),
        1,
        "a deterministic ledger error must not consume the retry budget"
    );
}

#[test]
// TestName: transient_ledger_busy_exhausts_the_bounded_retry_budget
fn transient_ledger_busy_exhausts_the_bounded_retry_budget() {
    let attempts = AtomicUsize::new(0);
    let error = retry_transient_ledger_publish(|| {
        attempts.fetch_add(1, AtomicOrdering::SeqCst);
        Err("reservation ledger transition is busy or ambiguous".to_string())
    })
    .unwrap_err();
    assert!(
        error.contains("transition is busy"),
        "the original busy failure must be preserved after exhaustion: {error}"
    );
    assert_eq!(
        attempts.load(AtomicOrdering::SeqCst),
        8,
        "the busy signature must consume exactly the bounded retry budget"
    );
}

#[test]
// TestName: transient_ledger_busy_is_retried_until_the_contending_lock_releases
fn transient_ledger_busy_is_retried_until_the_contending_lock_releases() {
    let root = fixture();
    let identity = OwnerIdentity::current().unwrap();
    let authority = File::open(&root).unwrap();
    authority.try_lock().unwrap();
    // The main thread owns the release so a delayed dropper cannot outlast
    // the retry budget and turn a real transient into a flake.
    let contended_root = root.clone();
    let contended_identity = identity.clone();
    let publisher = std::thread::spawn(move || {
        publish_admission_state_at_with_transient_retry(
            &contended_root,
            &contended_identity,
            "GUARDED",
            7_000,
        )
    });
    std::thread::sleep(Duration::from_millis(40));
    drop(authority);
    publisher.join().unwrap().unwrap();
    let gate: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(root.join("admission-state.json")).unwrap())
            .unwrap();
    assert_eq!(gate["admission_open"], false);
    assert_eq!(gate["control_state"], "GUARDED");
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: a_failed_scope_status_query_names_the_tool_reason
fn a_failed_scope_status_query_names_the_tool_reason() {
    // Positive: the tool's own reason rides along with the exit code, so
    // an operator can see why the query failed instead of a bare number.
    assert_eq!(
        scope_status_query_failure(Some(4), b"Access denied\n"),
        "exact scope status query failed: 4: Access denied"
    );

    // Negative, both directions: a killed query has no exit code, and an
    // empty or whitespace-only reason falls back to the status alone
    // rather than printing a trailing separator with nothing after it.
    assert_eq!(
        scope_status_query_failure(None, b""),
        "exact scope status query failed: signal"
    );
    assert_eq!(
        scope_status_query_failure(Some(1), b"  \n\t"),
        "exact scope status query failed: 1"
    );

    // The reason is trimmed as a whole, never rewritten internally.
    assert_eq!(
        scope_status_query_failure(Some(1), b"  no such unit  \n"),
        "exact scope status query failed: 1: no such unit"
    );
}

#[test]
// TestName: a_missing_control_plane_is_never_read_as_a_missing_unit
fn a_missing_control_plane_is_never_read_as_a_missing_unit() {
    /// The exact error `run_scope_status_command` hands up when
    /// `systemctl` is not on `PATH`: `run_capture_command` classifies the
    /// spawn failure as `BinaryNotFound`, whose display text ends in
    /// `binary not found`. That phrase used to match the unit-absence
    /// signature `not found`, so a host with no systemd tools was read as
    /// a host whose unit had been collected.
    struct ControlPlaneGone;
    impl ScopeStatusAdapter for ControlPlaneGone {
        fn query(&self, _unit: &str) -> Result<ScopeStatus, String> {
            Err(
                "exact scope status query custody failed: spawn exact scope status query: binary not found"
                    .into(),
            )
        }
    }

    let mut runner = Command::new("/bin/true");
    bounded_process::configure_process_group(&mut runner)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let runner = runner.spawn().unwrap();
    let mut execution = SystemScopeExecution::with_status_adapter(
        runner,
        "ramshared-control-plane-gone-fixture.scope".into(),
        Arc::new(ControlPlaneGone),
    );

    // The start is never acknowledged without a bound identity. Reading
    // the transport failure as "the unit is not there" used to fall
    // through the keep-polling arm and return `Acknowledged(None)` once
    // the control budget expired.
    match execution.acknowledge_start() {
        ScopeStart::UnsafeContainment(error) => {
            assert!(
                error.contains("reservation retained"),
                "a missing control plane must retain the reservation: {error}"
            );
        }
        ScopeStart::Acknowledged(identity) => {
            panic!("a missing control plane was read as a start acknowledgement: {identity:?}")
        }
    }

    // The runner is gone, so completion reaches the terminal proof. A
    // collected unit is proved terminal from `LoadState=not-found` on a
    // successful projection; a transport failure proves nothing, and the
    // reservation stays. Reading it as terminal used to release it.
    match execution.wait_for_completion() {
        ScopeCompletion::UnsafeContainment(error) => {
            assert!(
                error.contains("reservation retained"),
                "a missing control plane must retain the reservation: {error}"
            );
        }
        ScopeCompletion::Terminal(exit) => {
            panic!("a missing control plane was read as a contained scope: {exit:?}")
        }
    }
}

#[test]
fn recover_resume_at_refuses_missing_supervisor_invalid_guest_identity_and_bad_pending_tx() {
    let root = fixture();
    let paths = recovery_fixture(&root);
    seed_recovery(&paths, false);

    // 1. Missing supervisor health evidence propagates exact availability error
    fs::remove_file(&paths.supervisor_state).unwrap();
    assert_eq!(
        recover_resume_at(&paths).unwrap_err(),
        "supervisor health evidence is unavailable"
    );

    // 2. Invalid guest safe-mode gate schema_version refuses cleanly
    seed_recovery(&paths, false);
    let owner = OwnerIdentity::current().unwrap();
    fs::write(
        &paths.guest_gate,
        serde_json::json!({
            "schema_version": 99,
            "incident_id": "0123456789abcdef0123456789abcdef",
            "distro": "Ubuntu-24.04",
            "boot_id": owner.boot_id,
        })
        .to_string(),
    )
    .unwrap();
    assert_eq!(
        recover_resume_at(&paths).unwrap_err(),
        "guest safe-mode identity is invalid"
    );

    // 3. Absent host gate + absent pending tx refuses cleanly
    seed_recovery(&paths, false);
    fs::remove_file(&paths.pending).unwrap();
    assert_eq!(
        recover_resume_at(&paths).unwrap_err(),
        "matching Windows safe-mode gate or recovery transaction is unavailable"
    );

    // 4. Stale/mismatched pending tx hits validate_recovery_transaction refusal
    seed_recovery(&paths, false);
    let mut bad_tx = valid_recovery_transaction(&owner);
    bad_tx["incident_id"] = serde_json::json!("fedcba9876543210fedcba9876543210");
    fs::write(&paths.pending, format!("{bad_tx}\n")).unwrap();
    assert_eq!(
        recover_resume_at(&paths).unwrap_err(),
        "recovery transaction is stale, foreign, or invalid"
    );

    // 4b. Valid pending tx against valid supervisor whose guest gate has another incident_id hits line 165
    seed_recovery(&paths, false);
    fs::write(
        &paths.guest_gate,
        serde_json::json!({
            "schema_version": 1,
            "incident_id": "fedcba9876543210fedcba9876543210",
            "distro": "Ubuntu-24.04",
            "boot_id": owner.boot_id,
        })
        .to_string(),
    )
    .unwrap();
    assert_eq!(
        recover_resume_at(&paths).unwrap_err(),
        "recovery transaction identity is invalid"
    );

    // 5. Non-NotFound read error (directory at file path) on pending/host gate refuses
    fs::remove_file(&paths.pending).unwrap();
    fs::create_dir(&paths.pending).unwrap();
    assert_eq!(
        recover_resume_at(&paths).unwrap_err(),
        "recovery transaction is unavailable"
    );
    fs::remove_dir(&paths.pending).unwrap();

    fs::create_dir_all(paths.host_gate_dir.join("Ubuntu-24.04.json")).unwrap();
    assert_eq!(
        recover_resume_at(&paths).unwrap_err(),
        "Windows safe-mode gate is unavailable"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn systemd_scope_execution_refuses_mismatched_unit_signal_exit_and_launch_errors() {
    use std::process::{Command, Stdio};

    struct SequenceScopeStatus {
        responses: Mutex<Vec<Result<ScopeStatus, String>>>,
    }

    impl ScopeStatusAdapter for SequenceScopeStatus {
        fn query(&self, _unit: &str) -> Result<ScopeStatus, String> {
            self.responses.lock().unwrap().remove(0)
        }
    }

    struct ErrorRunner {
        mode: u8,
    }

    impl ScopeRunner for ErrorRunner {
        fn launch(
            &self,
            unit: &str,
            _runner_args: &[String],
            _command: &[String],
        ) -> Result<Box<dyn ScopeExecution>, String> {
            match self.mode {
                0 => Err("simulated runner launch failure".into()),
                1 => {
                    struct ContainmentFailExec(&'static str);
                    impl ScopeExecution for ContainmentFailExec {
                        fn acknowledge_start(&mut self) -> ScopeStart {
                            ScopeStart::UnsafeContainment(self.0.into())
                        }
                        fn wait_for_completion(&mut self) -> ScopeCompletion {
                            ScopeCompletion::Terminal(Ok(Some(0)))
                        }
                    }
                    Ok(Box::new(ContainmentFailExec("start containment failed")))
                }
                _ => {
                    struct WaitFailExec(String);
                    impl ScopeExecution for WaitFailExec {
                        fn acknowledge_start(&mut self) -> ScopeStart {
                            ScopeStart::Acknowledged(None)
                        }
                        fn wait_for_completion(&mut self) -> ScopeCompletion {
                            ScopeCompletion::UnsafeContainment(self.0.clone())
                        }
                    }
                    Ok(Box::new(WaitFailExec(format!(
                        "wait containment failed for {unit}"
                    ))))
                }
            }
        }
    }

    // Read-only SystemScopeStatusAdapter query on nonexistent unit exercises live command path
    let _ = SystemScopeStatusAdapter.query("ramshared-nonexistent-test-probe.scope");

    // 1. bind_start_identity unit mismatch + invalid lifecycle
    for resp in [
        ScopeStatus {
            id: "wrong.scope".into(),
            invocation_id: String::new(),
            load_state: "loaded".into(),
            active_state: "active".into(),
            sub_state: "running".into(),
            result: "success".into(),
        },
        ScopeStatus {
            id: "target.scope".into(),
            invocation_id: String::new(),
            load_state: "bogus-load-state".into(),
            active_state: "active".into(),
            sub_state: "running".into(),
            result: "success".into(),
        },
    ] {
        let mut cmd = Command::new("/bin/true");
        bounded_process::configure_process_group(&mut cmd)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = cmd.spawn().unwrap();
        let mut exec = SystemScopeExecution::with_status_adapter(
            child,
            "target.scope".into(),
            Arc::new(SequenceScopeStatus {
                responses: Mutex::new(vec![Ok(resp)]),
            }),
        );
        assert!(matches!(
            exec.bind_start_identity(),
            ScopeStart::UnsafeContainment(_)
        ));
    }

    // 2. prove_scope_terminal retry (active -> vacant) and unit mismatch + invalid state
    let mut cmd = Command::new("/bin/true");
    bounded_process::configure_process_group(&mut cmd)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = cmd.spawn().unwrap();
    let mut retry_exec = SystemScopeExecution::with_status_adapter(
        child,
        "retry.scope".into(),
        Arc::new(SequenceScopeStatus {
            responses: Mutex::new(vec![
                Ok(ScopeStatus {
                    id: "retry.scope".into(),
                    invocation_id: String::new(),
                    load_state: "loaded".into(),
                    active_state: "active".into(),
                    sub_state: "running".into(),
                    result: "success".into(),
                }),
                Ok(ScopeStatus::terminal_success("retry.scope")),
            ]),
        }),
    );
    assert!(retry_exec.prove_scope_terminal().is_ok());

    for bad in [
        ScopeStatus {
            id: "wrong.scope".into(),
            invocation_id: String::new(),
            load_state: "not-found".into(),
            active_state: "inactive".into(),
            sub_state: "dead".into(),
            result: "success".into(),
        },
        ScopeStatus {
            id: "bad.scope".into(),
            invocation_id: String::new(),
            load_state: "bogus".into(),
            active_state: "active".into(),
            sub_state: "running".into(),
            result: "success".into(),
        },
    ] {
        let mut c = Command::new("/bin/true");
        bounded_process::configure_process_group(&mut c)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = c.spawn().unwrap();
        let mut exec = SystemScopeExecution::with_status_adapter(
            child,
            "bad.scope".into(),
            Arc::new(SequenceScopeStatus {
                responses: Mutex::new(vec![Ok(bad)]),
            }),
        );
        assert!(matches!(
            exec.wait_for_completion(),
            ScopeCompletion::UnsafeContainment(_)
        ));
    }

    // 3. Signal exit returns ScopeCompletion::Terminal(Ok(None))
    let mut sleep_cmd = Command::new("sleep");
    sleep_cmd.arg("5");
    bounded_process::configure_process_group(&mut sleep_cmd)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut sleep_child = sleep_cmd.spawn().unwrap();
    sleep_child.kill().unwrap();
    let mut sig_exec = SystemScopeExecution::with_status_adapter(
        sleep_child,
        "sig.scope".into(),
        Arc::new(VacantScopeStatus),
    );
    assert!(matches!(
        sig_exec.wait_for_completion(),
        ScopeCompletion::Terminal(Ok(None))
    ));

    // 4. run_with launch error, start unsafe containment, wait unsafe containment
    for mode in [0u8, 1, 2] {
        let root = fixture();
        let owner = OwnerIdentity::current().unwrap();
        fs::write(
            root.join("supervisor.json"),
            healthy_supervisor_status(&owner, now_ms()).to_string(),
        )
        .unwrap();
        fs::write(
            root.join("lease.json"),
            valid_host_resume_lease(&owner.boot_id).to_string(),
        )
        .unwrap();
        let ledger_root = root.join("ledger");
        publish_open_admission_state(&ledger_root, &owner);
        let runner = ErrorRunner { mode };
        let res = run_with(
            WorkloadRequest {
                class: WorkloadClass::Interactive,
                memory_max_bytes: GIB,
                command: vec!["/bin/true".into()],
            },
            "MemTotal: 16777216 kB\n",
            &runner,
            &ledger_root,
            &root.join("safe"),
            &root.join("lease.json"),
            &root.join("supervisor.json"),
        );
        assert!(res.is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
