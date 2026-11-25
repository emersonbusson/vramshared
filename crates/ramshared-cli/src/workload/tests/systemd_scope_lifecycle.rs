// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/workload/tests/systemd_scope_lifecycle.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;

#[test]
// TestName: system_scope_terminal_watcher_retains_full_reservation_until_exact_unit_finishes
fn system_scope_terminal_watcher_retains_full_reservation_until_exact_unit_finishes() {
    let root = fixture();
    let supervisor = OwnerIdentity::current().unwrap();
    fs::write(
        root.join("supervisor.json"),
        healthy_supervisor_status(&supervisor, now_ms()).to_string(),
    )
    .unwrap();
    fs::write(
        root.join("lease.json"),
        valid_host_resume_lease(&supervisor.boot_id).to_string(),
    )
    .unwrap();
    let ledger_root = root.join("ledger");
    publish_open_admission_state(&ledger_root, &supervisor);
    let status = Arc::new(BlockingScopeStatusAdapter {
        state: Mutex::new(BlockingScopeStatus {
            terminal_poll_started: false,
            terminal: false,
            queries: 0,
            units: Vec::new(),
        }),
        changed: Condvar::new(),
    });
    let runner = Arc::new(SystemExecutionRunner {
        status: status.clone(),
        launches: Arc::new(AtomicUsize::new(0)),
    });
    let worker_root = root.clone();
    let worker_ledger = ledger_root.clone();
    let worker_runner = runner.clone();
    let worker = std::thread::spawn(move || {
        run_with(
            WorkloadRequest {
                class: WorkloadClass::Interactive,
                memory_max_bytes: 12 * GIB,
                command: vec!["/bin/true".into()],
            },
            "MemTotal: 16777216 kB\n",
            &*worker_runner,
            &worker_ledger,
            &worker_root.join("safe"),
            &worker_root.join("lease.json"),
            &worker_root.join("supervisor.json"),
        )
    });

    status.wait_until_terminal_poll();
    let second = match admit_reservation_at(
        &ledger_root,
        &root.join("safe"),
        &root.join("lease.json"),
        &root.join("supervisor.json"),
        OwnerIdentity::current().unwrap(),
        WorkloadClass::Interactive,
        12 * GIB,
        12 * GIB,
        &SystemOwnerProbe,
        &OccupiedScopeStatus,
    ) {
        Ok(_) => panic!("active exact scope admitted a second full-ceiling reservation"),
        Err(error) => error,
    };
    assert!(
        second.contains("aggregate workload budget exceeded"),
        "{second}"
    );
    assert_eq!(
        load_ledger(&LedgerPaths::new(&ledger_root).ledger)
            .unwrap()
            .reservations
            .len(),
        1,
        "the acknowledged active unit was not retained in aggregate accounting"
    );

    status.allow_terminal();
    worker.join().unwrap().unwrap();
    assert_eq!(runner.launches.load(AtomicOrdering::SeqCst), 1);
    let observed_units = &status.state.lock().unwrap().units;
    assert!(observed_units.len() >= 2);
    assert!(observed_units[0].starts_with("ramshared-interactive-"));
    assert!(observed_units[0].ends_with(".scope"));
    assert!(
        load_ledger(&LedgerPaths::new(&ledger_root).ledger)
            .unwrap()
            .reservations
            .is_empty(),
        "terminal completion did not release exactly once"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: system_scope_identity_change_retains_reservation_for_containment
fn system_scope_identity_change_retains_reservation_for_containment() {
    let root = fixture();
    let supervisor = OwnerIdentity::current().unwrap();
    fs::write(
        root.join("supervisor.json"),
        healthy_supervisor_status(&supervisor, now_ms()).to_string(),
    )
    .unwrap();
    fs::write(
        root.join("lease.json"),
        valid_host_resume_lease(&supervisor.boot_id).to_string(),
    )
    .unwrap();
    let ledger_root = root.join("ledger");
    publish_open_admission_state(&ledger_root, &supervisor);
    let runner = SystemExecutionRunner {
        status: Arc::new(ChangingScopeIdentityAdapter {
            queries: AtomicUsize::new(0),
        }),
        launches: Arc::new(AtomicUsize::new(0)),
    };

    let error = run_with(
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
    )
    .unwrap_err();

    assert!(error.contains("invocation identity changed"), "{error}");
    assert_eq!(runner.launches.load(AtomicOrdering::SeqCst), 1);
    assert_eq!(
        load_ledger(&LedgerPaths::new(&ledger_root).ledger)
            .unwrap()
            .reservations
            .len(),
        1,
        "replacement scope identity released live containment evidence"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: system_scope_status_error_retains_reservation_for_containment
fn system_scope_status_error_retains_reservation_for_containment() {
    let root = fixture();
    let supervisor = OwnerIdentity::current().unwrap();
    fs::write(
        root.join("supervisor.json"),
        healthy_supervisor_status(&supervisor, now_ms()).to_string(),
    )
    .unwrap();
    fs::write(
        root.join("lease.json"),
        valid_host_resume_lease(&supervisor.boot_id).to_string(),
    )
    .unwrap();
    let ledger_root = root.join("ledger");
    publish_open_admission_state(&ledger_root, &supervisor);
    let runner = SystemExecutionRunner {
        status: Arc::new(ErroringScopeStatusAdapter {
            queries: AtomicUsize::new(0),
        }),
        launches: Arc::new(AtomicUsize::new(0)),
    };

    let error = run_with(
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
    )
    .unwrap_err();

    assert!(error.contains("terminal state"), "{error}");
    assert_eq!(runner.launches.load(AtomicOrdering::SeqCst), 1);
    assert_eq!(
        load_ledger(&LedgerPaths::new(&ledger_root).ledger)
            .unwrap()
            .reservations
            .len(),
        1,
        "status-query containment failure released live scope evidence"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: scope_ack_timeout_reaps_group_and_owned_descendant
fn scope_ack_timeout_reaps_group_and_owned_descendant() {
    /// Never yields an identity and never reports terminal: the control
    /// surface is permanently unable to bind the start `InvocationID`.
    struct NeverIdentifyingAdapter;
    impl ScopeStatusAdapter for NeverIdentifyingAdapter {
        fn query(&self, unit: &str) -> Result<ScopeStatus, String> {
            Ok(ScopeStatus {
                id: unit.to_string(),
                invocation_id: String::new(),
                load_state: "loaded".into(),
                active_state: "active".into(),
                sub_state: "running".into(),
                result: "success".into(),
            })
        }
    }

    let root = fixture();
    let descendant_pid_file = root.join("ack-descendant.pid");
    let mut runner = Command::new("/bin/sh");
    runner
        .arg("-c")
        .arg("sleep 10 & printf '%s' \"$!\" > \"$1\"; wait")
        .arg("ramshared-ack-fixture")
        .arg(&descendant_pid_file);
    bounded_process::configure_process_group(&mut runner);
    let runner = runner.spawn().unwrap();
    let direct_pid = runner.id();
    let pid_deadline = std::time::Instant::now() + Duration::from_secs(1);
    let mut descendant_pid = None;
    while std::time::Instant::now() < pid_deadline {
        let parsed_pid = fs::read_to_string(&descendant_pid_file)
            .ok()
            .and_then(|content| content.trim().parse::<u32>().ok());
        if let Some(pid) = parsed_pid {
            descendant_pid = Some(pid);
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let descendant_pid = descendant_pid.expect("descendant pid file must be populated");

    let mut execution = SystemScopeExecution::with_status_adapter(
        runner,
        "ramshared-ack-fixture.scope".into(),
        Arc::new(NeverIdentifyingAdapter),
    );
    let start = execution.acknowledge_start();
    let error = match start {
        ScopeStart::UnsafeContainment(error) => error,
        ScopeStart::Acknowledged(identity) => {
            panic!("identity was bound without a control surface: {identity:?}")
        }
    };
    // Dropping the execution is the containment boundary: a workload
    // whose identity could never be bound is not left running unreaped.
    drop(execution);
    std::thread::sleep(Duration::from_millis(50));
    let direct_gone = !Path::new(&format!("/proc/{direct_pid}")).exists();
    let descendant_gone = !Path::new(&format!("/proc/{descendant_pid}")).exists();

    if (!direct_gone || !descendant_gone)
        && let Some(group) = rustix::process::Pid::from_raw(direct_pid as i32)
    {
        let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    }
    fs::remove_dir_all(root).unwrap();

    assert!(
        error.contains("never observed"),
        "unbound identity must name the condition: {error}"
    );
    assert!(direct_gone, "contained runner was not reaped");
    assert!(
        descendant_gone,
        "owned acknowledgement descendant survived its process group"
    );
}

#[test]
// TestName: not_found_scope_while_runner_is_alive_is_not_a_finished_workload
fn not_found_scope_while_runner_is_alive_is_not_a_finished_workload() {
    /// Answers `not-found` until the configured poll count, then a fully
    /// bound running scope — the real start window, where systemd has not
    /// registered the unit yet and the projection is identical to a
    /// collected one.
    struct StartInFlightAdapter {
        not_found_polls: AtomicUsize,
    }
    impl ScopeStatusAdapter for StartInFlightAdapter {
        fn query(&self, unit: &str) -> Result<ScopeStatus, String> {
            if self.not_found_polls.fetch_add(1, AtomicOrdering::SeqCst) < 3 {
                return Ok(ScopeStatus {
                    id: unit.to_string(),
                    invocation_id: String::new(),
                    load_state: "not-found".into(),
                    active_state: "inactive".into(),
                    sub_state: "dead".into(),
                    result: "success".into(),
                });
            }
            Ok(ScopeStatus::active(unit))
        }
    }

    let mut runner = Command::new("/bin/sleep");
    runner.arg("3");
    bounded_process::configure_process_group(&mut runner)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let runner = runner.spawn().unwrap();
    let mut execution = SystemScopeExecution::with_status_adapter(
        runner,
        "ramshared-start-in-flight-fixture.scope".into(),
        Arc::new(StartInFlightAdapter {
            not_found_polls: AtomicUsize::new(0),
        }),
    );
    let start = execution.acknowledge_start();
    // Dropping here is the containment boundary for the fixture.
    drop(execution);

    match start {
        ScopeStart::Acknowledged(Some(id)) => assert_eq!(id, FIXTURE_INVOCATION_ID),
        ScopeStart::Acknowledged(None) => {
            panic!("a still-alive runner was read as a finished workload")
        }
        ScopeStart::UnsafeContainment(error) => {
            panic!("start window refused identity: {error}")
        }
    }
}

#[test]
// TestName: not_found_scope_after_runner_exit_is_a_finished_workload
fn not_found_scope_after_runner_exit_is_a_finished_workload() {
    /// The collected projection: identical to the not-yet-registered one.
    struct CollectedScopeAdapter;
    impl ScopeStatusAdapter for CollectedScopeAdapter {
        fn query(&self, unit: &str) -> Result<ScopeStatus, String> {
            Ok(ScopeStatus {
                id: unit.to_string(),
                invocation_id: String::new(),
                load_state: "not-found".into(),
                active_state: "inactive".into(),
                sub_state: "dead".into(),
                result: "success".into(),
            })
        }
    }

    let mut runner = Command::new("/bin/true");
    bounded_process::configure_process_group(&mut runner)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut runner = runner.spawn().unwrap();
    // The workload is over before the identity bind starts, and the exit
    // is still ours: the zombie leader has not been reaped, which is the
    // only situation in which `not-found` may be read as finished.
    loop {
        if bounded_process::child_has_exited(&mut runner)
            .expect("observing the fixture runner must not fail")
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let mut execution = SystemScopeExecution::with_status_adapter(
        runner,
        "ramshared-collected-fixture.scope".into(),
        Arc::new(CollectedScopeAdapter),
    );
    let start = execution.acknowledge_start();
    drop(execution);

    assert_eq!(
        start,
        ScopeStart::Acknowledged(None),
        "a collected scope with no start identity must release by owner identity"
    );
}

#[test]
// TestName: transition_guard_recovers_verified_dead_owner_record
fn transition_guard_recovers_verified_dead_owner_record() {
    let root = fixture();
    let paths = LedgerPaths::new(&root);
    fs::create_dir_all(&paths.root).unwrap();
    let stale = owner(99, 1);
    fs::write(
        &paths.transition,
        serde_json::to_vec(&TransitionOwnerRecord {
            schema_version: TRANSITION_OWNER_SCHEMA_VERSION,
            owner: stale,
        })
        .unwrap(),
    )
    .unwrap();

    let guard = acquire_lock(&paths, &owner(10, 1), &FakeProbe { dead_pid: Some(99) })
        .unwrap_or_else(|error| panic!("verified-dead transition must recover: {error}"));
    let recovered: TransitionOwnerRecord =
        serde_json::from_slice(&fs::read(&paths.transition).unwrap()).unwrap();
    assert_eq!(recovered.owner.pid, 10);
    drop(guard);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn recovery_releases_gates_only_with_a_current_resume_lease() {
    for with_host_gate in [true, false] {
        let root = fixture();
        let paths = recovery_fixture(&root);
        seed_recovery(&paths, with_host_gate);
        recover_resume_at(&paths).unwrap();
        assert!(!paths.guest_gate.exists());
        assert!(!paths.pending.exists());
        assert!(paths.resume_lease.is_file());
        assert!(!paths.host_gate_dir.join("Ubuntu-24.04.json").exists());
        let current = OwnerIdentity::current().unwrap();
        let lease = verify_host_resume_lease_at(&paths.resume_lease, &current.boot_id)
            .unwrap_or_else(|error| panic!("recovery must mint an admissible lease: {error}"));
        assert_eq!(lease.source, HostResumeLeaseSource::SupervisorResume);
        assert_eq!(lease.hardware_health, HostHardwareHealth::Clean);
        assert_eq!(lease.boot_id, current.boot_id);
        let status = recover_status_at(&paths.guest_gate, &paths.resume_lease).unwrap();
        assert!(status.contains(r#""safe_mode":false"#));
        assert!(status.contains(r#""resume_lease":true"#));
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn a_refused_hardware_verdict_blocks_resume() {
    let root = fixture();
    let paths = recovery_fixture(&root);
    seed_recovery(&paths, true);
    let owner = OwnerIdentity::current().unwrap();
    let mut document = valid_hardware_health(&owner.boot_id);
    document["mce_kill_count"] = serde_json::json!(83269);
    document["verdict"] = serde_json::json!("refused_mce");
    fs::write(&paths.hardware_health, format!("{document}\n")).unwrap();
    let error = recover_resume_at(&paths).unwrap_err();
    assert!(error.contains("hardware health is refused"), "{error}");
    assert!(!paths.resume_lease.exists());
    assert!(
        paths.guest_gate.exists(),
        "a refusal must keep the safe-mode gate"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_missing_hardware_proof_blocks_resume() {
    let root = fixture();
    let paths = recovery_fixture(&root);
    seed_recovery(&paths, true);
    fs::remove_file(&paths.hardware_health).unwrap();
    let error = recover_resume_at(&paths).unwrap_err();
    assert!(
        error.contains("hardware health proof is missing"),
        "{error}"
    );
    assert!(!paths.resume_lease.exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn an_overridden_hardware_verdict_is_carried_into_the_lease() {
    let root = fixture();
    let paths = recovery_fixture(&root);
    seed_recovery(&paths, true);
    let owner = OwnerIdentity::current().unwrap();
    let mut document = valid_hardware_health(&owner.boot_id);
    document["mce_kill_count"] = serde_json::json!(1);
    document["verdict"] = serde_json::json!("clean_override");
    fs::write(&paths.hardware_health, format!("{document}\n")).unwrap();
    recover_resume_at(&paths).unwrap();
    let lease = verify_host_resume_lease_at(&paths.resume_lease, &owner.boot_id)
        .unwrap_or_else(|error| panic!("override must mint an admissible lease: {error}"));
    assert_eq!(lease.hardware_health, HostHardwareHealth::CleanOverride);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn recovery_refuses_unhealthy_or_mismatched_evidence() {
    let root = fixture();
    let paths = recovery_fixture(&root);
    seed_recovery(&paths, true);
    let mut guarded: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&paths.supervisor_state).unwrap()).unwrap();
    guarded["control_state"] = serde_json::json!("GUARDED");
    guarded["healthy_samples"] = serde_json::json!(59);
    fs::write(&paths.supervisor_state, guarded.to_string()).unwrap();
    assert!(
        recover_resume_at(&paths)
            .unwrap_err()
            .contains("60 consecutive")
    );

    seed_recovery(&paths, true);
    let mismatched_gate = serde_json::json!({
        "schema_version": 1,
        "incident_id": "f".repeat(32),
        "distro": "Ubuntu-24.04",
        "prior_boot_id": "prior-boot",
        "reason": "guardian_proven_guest_inaccessible",
        "timestamp_utc": "2026-08-22T00:00:00Z",
    });
    fs::write(
        paths.host_gate_dir.join("Ubuntu-24.04.json"),
        format!("{mismatched_gate}\n"),
    )
    .unwrap();
    assert!(
        recover_resume_at(&paths)
            .unwrap_err()
            .contains("do not match")
    );

    fs::remove_file(paths.host_gate_dir.join("Ubuntu-24.04.json")).unwrap();
    assert!(
        recover_resume_at(&paths)
            .unwrap_err()
            .contains("recovery transaction is unavailable")
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn recovery_refuses_bad_schema_wrong_boot_incident_identity_or_freshness() {
    for (name, mutate) in [
        (
            "bad_schema",
            Box::new(|paths: &RecoveryPaths| {
                fs::write(&paths.guest_gate, r#"{"schema_version":9}"#).unwrap();
            }) as Box<dyn Fn(&RecoveryPaths)>,
        ),
        (
            "wrong_boot",
            Box::new(|paths: &RecoveryPaths| {
                let mut gate: serde_json::Value =
                    serde_json::from_str(&fs::read_to_string(&paths.guest_gate).unwrap()).unwrap();
                gate["boot_id"] = serde_json::json!("foreign-boot");
                fs::write(&paths.guest_gate, gate.to_string()).unwrap();
            }),
        ),
        (
            "wrong_incident",
            Box::new(|paths: &RecoveryPaths| {
                let host_gate = paths.host_gate_dir.join("Ubuntu-24.04.json");
                let mut gate: serde_json::Value =
                    serde_json::from_str(&fs::read_to_string(&host_gate).unwrap()).unwrap();
                gate["incident_id"] = serde_json::json!("f".repeat(32));
                fs::write(host_gate, gate.to_string()).unwrap();
            }),
        ),
        (
            "foreign_identity",
            Box::new(|paths: &RecoveryPaths| {
                let mut status: serde_json::Value =
                    serde_json::from_str(&fs::read_to_string(&paths.supervisor_state).unwrap())
                        .unwrap();
                status["supervisor_identity"]["pid"] = serde_json::json!(u32::MAX);
                fs::write(&paths.supervisor_state, status.to_string()).unwrap();
            }),
        ),
        (
            "stale_supervisor",
            Box::new(|paths: &RecoveryPaths| {
                let mut status: serde_json::Value =
                    serde_json::from_str(&fs::read_to_string(&paths.supervisor_state).unwrap())
                        .unwrap();
                status["written_at_unix_ms"] = serde_json::json!(0);
                fs::write(&paths.supervisor_state, status.to_string()).unwrap();
            }),
        ),
    ] {
        let root = fixture();
        let paths = recovery_fixture(&root);
        seed_recovery(&paths, true);
        mutate(&paths);
        assert!(
            recover_resume_at(&paths).is_err(),
            "{name} recovery evidence issued a lease"
        );
        assert!(
            !paths.resume_lease.exists(),
            "{name} created a resume lease"
        );
        assert!(paths.guest_gate.exists(), "{name} released the guest gate");
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn workload_classes_keep_distinct_default_memory_limits() {
    for (name, class, expected_mib) in [
        ("interactive", WorkloadClass::Interactive, 2 * 1024),
        ("build", WorkloadClass::Build, 6 * 1024),
        ("browser-test", WorkloadClass::BrowserTest, 4 * 1024),
        ("batch", WorkloadClass::Batch, 8 * 1024),
    ] {
        assert_eq!(WorkloadClass::parse(name), Some(class));
        assert_eq!(class.as_str(), name);
        assert_eq!(class.default_memory_mib(), expected_mib);

        let request =
            parse_run_args(&["--class".into(), name.into(), "--".into(), "true".into()]).unwrap();
        assert_eq!(request.class, class);
        assert_eq!(request.memory_max_bytes, expected_mib * MIB_BYTES);
    }
}
