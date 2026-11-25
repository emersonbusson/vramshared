// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/workload/tests/scope_admission_and_custody.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;

#[test]
// TestName: unknown_unit_state_keeps_a_dead_owners_reservation
fn unknown_unit_state_keeps_a_dead_owners_reservation() {
    // An unknown observation is never proof of absence. A failed query
    // cannot distinguish "collected" from "the status surface is down",
    // so the row stays counted and the budget stays charged.
    let root = fixture();
    let alive = FakeProbe { dead_pid: None };
    let first = acquire_reservation_at(
        &root,
        &root.join("safe"),
        &root.join("lease"),
        owner(10, 1),
        WorkloadClass::Build,
        6,
        10,
        &alive,
        &OccupiedScopeStatus,
    )
    .unwrap();

    let owner_dead = FakeProbe { dead_pid: Some(10) };
    let second = acquire_reservation_at(
        &root,
        &root.join("safe"),
        &root.join("lease"),
        owner(11, 1),
        WorkloadClass::Batch,
        5,
        10,
        &owner_dead,
        &UnknownScopeStatus,
    );
    assert!(
        second.is_err(),
        "an unobservable unit released a dead owner's budget"
    );
    let published = load_ledger(&LedgerPaths::new(&root).ledger).unwrap();
    assert_eq!(
        published.reservations.len(),
        1,
        "a row whose unit could not be observed was dropped"
    );
    assert_eq!(published.reservations[0].id, first.id);
    assert!(
        reservation_snapshots(&root).is_empty(),
        "an unobservable unit was preserved as recovered evidence"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn managed_scope_uses_aggregate_ceiling() {
    let budget = calculate_budget(16 * GIB, 1).unwrap();
    assert_eq!(budget.control_plane_reserve_bytes, 4 * GIB);
    assert_eq!(budget.memory_max_bytes, 12 * GIB);
    assert_eq!(budget.memory_high_bytes, 12 * GIB - (16 * GIB).div_ceil(10));
    let request = parse_run_args(&[
        "--class".into(),
        "build".into(),
        "--memory-max".into(),
        "4096".into(),
        "--".into(),
        "/bin/true".into(),
    ])
    .unwrap();
    let args = build_runner_args("test.scope", &request, budget);
    assert!(
        args.iter()
            .any(|argument| argument == "Slice=ramshared-workloads.slice")
    );
    assert!(
        args.iter()
            .any(|argument| argument == "MemoryMax=4294967296")
    );
}

#[test]
fn safe_mode_marker_blocks_admission_even_when_a_lease_exists() {
    let root = fixture();
    let safe = root.join("safe");
    fs::write(&safe, "{}").unwrap();
    fs::write(root.join("lease"), "{}").unwrap();
    let probe = FakeProbe { dead_pid: None };
    assert!(
        acquire_reservation_at(
            &root,
            &safe,
            &root.join("lease"),
            owner(10, 1),
            WorkloadClass::Interactive,
            1,
            10,
            &probe,
            &OccupiedScopeStatus,
        )
        .is_err()
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn parser_budget_and_owner_adapters_cover_legitimate_and_refusal_paths() {
    assert!(calculate_budget(0, 0).is_err());
    assert!(calculate_budget(4 * GIB, 0).is_err());
    assert_eq!(
        parse_meminfo("MemTotal: 16384 kB\n").unwrap(),
        16 * 1024 * 1024
    );
    assert!(parse_meminfo("MemAvailable: 1 kB\n").is_err());
    assert!(process_start_time(std::process::id()).is_some());
    let current = OwnerIdentity::current().unwrap();
    assert!(SystemOwnerProbe.is_alive(&current));

    for (class, expected_mib) in [
        ("interactive", 2048),
        ("build", 6144),
        ("browser-test", 4096),
        ("batch", 8192),
    ] {
        let request = parse_run_args(&[
            "--class".into(),
            class.into(),
            "--".into(),
            "/bin/true".into(),
        ])
        .unwrap();
        assert_eq!(request.memory_max_bytes, expected_mib * MIB_BYTES);
        assert_eq!(request.class.as_str(), class);
    }
    for args in [
        vec![],
        vec!["--class".into(), "unknown".into()],
        vec!["--memory-max".into(), "0".into()],
        vec!["--class".into(), "build".into(), "--".into()],
        vec!["--unexpected".into()],
    ] {
        assert!(parse_run_args(&args).is_err());
    }
}

#[test]
// TestName: supported_reservation_ledger_allows_admission_and_release
fn supported_reservation_ledger_allows_admission_and_release() {
    for result in [
        Ok(Some(0)),
        Ok(Some(7)),
        Ok(None),
        Err("fixture spawn refusal".into()),
    ] {
        let root = fixture();
        let runner = FakeRunner {
            result: result.clone(),
            calls: RefCell::new(Vec::new()),
        };
        let control = OwnerIdentity::current().unwrap();
        fs::write(
            root.join("supervisor.json"),
            format!("{}\n", healthy_supervisor_status(&control, now_ms())),
        )
        .unwrap();
        fs::write(
            root.join("lease"),
            valid_host_resume_lease(&control.boot_id).to_string(),
        )
        .unwrap();
        publish_open_admission_state(&root.join("ledger"), &control);
        let request = WorkloadRequest {
            class: WorkloadClass::Build,
            memory_max_bytes: GIB,
            command: vec!["/bin/true".into()],
        };
        let outcome = run_with(
            request,
            "MemTotal: 16777216 kB\n",
            &runner,
            &root.join("ledger"),
            &root.join("safe"),
            &root.join("lease"),
            &root.join("supervisor.json"),
        );
        assert_eq!(outcome.is_ok(), result == Ok(Some(0)));
        assert_eq!(runner.calls.borrow().len(), 1);
        let ledger = load_ledger(&root.join("ledger/reservations.json")).unwrap();
        assert!(ledger.reservations.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    let root = fixture();
    let mut guarded = current_healthy_supervisor_status();
    guarded["control_state"] = serde_json::json!("GUARDED");
    fs::write(root.join("supervisor.json"), guarded.to_string()).unwrap();
    let runner = FakeRunner {
        result: Ok(Some(0)),
        calls: RefCell::new(Vec::new()),
    };
    let refused = run_with(
        WorkloadRequest {
            class: WorkloadClass::Interactive,
            memory_max_bytes: GIB,
            command: vec!["/bin/true".into()],
        },
        "MemTotal: 16777216 kB\n",
        &runner,
        &root.join("ledger"),
        &root.join("safe"),
        &root.join("lease"),
        &root.join("supervisor.json"),
    );
    assert!(refused.unwrap_err().contains("GUARDED"));
    assert!(runner.calls.borrow().is_empty());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn workload_admission_refuses_missing_malformed_stale_or_foreign_supervisor_evidence() {
    for (name, evidence) in [
        ("missing", None),
        ("malformed", Some("{".to_string())),
        (
            "stale",
            Some(healthy_supervisor_status(&OwnerIdentity::current().unwrap(), 0).to_string()),
        ),
        (
            "foreign",
            Some(
                healthy_supervisor_status(
                    &OwnerIdentity {
                        boot_id: "foreign-boot".into(),
                        ..OwnerIdentity::current().unwrap()
                    },
                    now_ms(),
                )
                .to_string(),
            ),
        ),
    ] {
        let root = fixture();
        if let Some(evidence) = evidence {
            fs::write(root.join("supervisor.json"), evidence).unwrap();
        }
        let runner = FakeRunner {
            result: Ok(Some(0)),
            calls: RefCell::new(Vec::new()),
        };
        let result = run_with(
            WorkloadRequest {
                class: WorkloadClass::Interactive,
                memory_max_bytes: GIB,
                command: vec!["/bin/true".into()],
            },
            "MemTotal: 16777216 kB\n",
            &runner,
            &root.join("ledger"),
            &root.join("safe"),
            &root.join("lease"),
            &root.join("supervisor.json"),
        );
        assert!(
            result.is_err(),
            "{name} supervisor evidence admitted a workload"
        );
        assert!(
            runner.calls.borrow().is_empty(),
            "{name} started a workload"
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn malformed_or_foreign_host_resume_lease_never_admits_a_workload() {
    let owner = OwnerIdentity::current().unwrap();
    for (name, lease) in [
        ("malformed", "{".to_string()),
        ("empty", serde_json::json!({}).to_string()),
        (
            "unknown_schema",
            serde_json::json!({
                "schema_version": 2,
                "boot_id": owner.boot_id,
                "source": "fresh_sealed_guardian_proof",
            })
            .to_string(),
        ),
        (
            "unknown_source",
            serde_json::json!({
                "schema_version": 1,
                "boot_id": owner.boot_id,
                "source": "some_other_issuer",
            })
            .to_string(),
        ),
        (
            // A lease from another boot is dead on this one — that binding
            // is the lease's lifetime (DT-11), not a wall-clock deadline.
            "foreign_boot",
            serde_json::json!({
                "schema_version": 1,
                "boot_id": "00000000-0000-0000-0000-000000000000",
                "source": "fresh_sealed_guardian_proof",
            })
            .to_string(),
        ),
    ] {
        let root = fixture();
        fs::write(
            root.join("supervisor.json"),
            healthy_supervisor_status(&owner, now_ms()).to_string(),
        )
        .unwrap();
        fs::write(root.join("lease"), lease).unwrap();
        publish_open_admission_state(&root.join("ledger"), &owner);
        let runner = FakeRunner {
            result: Ok(Some(0)),
            calls: RefCell::new(Vec::new()),
        };
        let result = run_with(
            WorkloadRequest {
                class: WorkloadClass::Interactive,
                memory_max_bytes: GIB,
                command: vec!["/bin/true".into()],
            },
            "MemTotal: 16777216 kB\n",
            &runner,
            &root.join("ledger"),
            &root.join("safe"),
            &root.join("lease"),
            &root.join("supervisor.json"),
        );
        assert!(result.is_err(), "{name} resume lease admitted a workload");
        assert!(
            runner.calls.borrow().is_empty(),
            "{name} started a workload"
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn host_gate_lease_document_admits_one_workload() {
    let root = fixture();
    let owner = OwnerIdentity::current().unwrap();
    fs::write(
        root.join("supervisor.json"),
        healthy_supervisor_status(&owner, now_ms()).to_string(),
    )
    .unwrap();
    fs::write(
        root.join("lease"),
        valid_host_resume_lease(&owner.boot_id).to_string(),
    )
    .unwrap();
    publish_open_admission_state(&root.join("ledger"), &owner);
    let runner = FakeRunner {
        result: Ok(Some(0)),
        calls: RefCell::new(Vec::new()),
    };
    run_with(
        WorkloadRequest {
            class: WorkloadClass::Interactive,
            memory_max_bytes: GIB,
            command: vec!["/bin/true".into()],
        },
        "MemTotal: 16777216 kB\n",
        &runner,
        &root.join("ledger"),
        &root.join("safe"),
        &root.join("lease"),
        &root.join("supervisor.json"),
    )
    .unwrap();
    assert_eq!(runner.calls.borrow().len(), 1);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn minted_host_resume_lease_round_trips_for_both_issuers() {
    // One contract, two issuers: `ramshared-host-gate.service` and
    // `recover --resume` write the same shape and the same reader accepts
    // both. A consumer that invents its own fields breaks here, not in
    // production.
    let root = fixture();
    let boot_id = current_boot_id().unwrap();
    for source in [
        HostResumeLeaseSource::FreshSealedGuardianProof,
        HostResumeLeaseSource::SupervisorResume,
    ] {
        let path = root.join(format!("{source:?}.json"));
        mint_host_resume_lease(&path, &boot_id, source, HostHardwareHealth::Clean)
            .unwrap_or_else(|error| panic!("mint: {error}"));
        let lease = verify_host_resume_lease_at(&path, &boot_id)
            .unwrap_or_else(|error| panic!("{source:?} lease refused: {error}"));
        assert_eq!(lease.source, source);
        assert_eq!(lease.boot_id, boot_id);
        assert_eq!(lease.hardware_health, HostHardwareHealth::Clean);
        assert_eq!(lease.schema_version, HOST_RESUME_LEASE_SCHEMA_VERSION);
        assert_eq!(
            verify_host_resume_lease_at(&path, "foreign-boot").unwrap_err(),
            "host-resume lease is not bound to this boot"
        );
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: workload_admission_refuses_missing_boot_ephemeral_lease
fn workload_admission_refuses_missing_boot_ephemeral_lease() {
    let root = fixture();
    let supervisor = OwnerIdentity::current().unwrap();
    fs::write(
        root.join("supervisor.json"),
        healthy_supervisor_status(&supervisor, now_ms()).to_string(),
    )
    .unwrap();
    publish_open_admission_state(&root.join("ledger"), &supervisor);
    let runner = FakeRunner {
        result: Ok(Some(0)),
        calls: RefCell::new(Vec::new()),
    };

    let error = run_with(
        WorkloadRequest {
            class: WorkloadClass::Interactive,
            memory_max_bytes: GIB,
            command: vec!["/bin/true".into()],
        },
        "MemTotal: 16777216 kB\n",
        &runner,
        &root.join("ledger"),
        &root.join("safe"),
        &root.join("missing-lease.json"),
        &root.join("supervisor.json"),
    )
    .unwrap_err();

    assert!(error.contains("resume lease"), "{error}");
    assert!(
        runner.calls.borrow().is_empty(),
        "workload started without lease"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: workload_admission_requires_serialized_open_gate
fn workload_admission_requires_serialized_open_gate() {
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
    let runner = FakeRunner {
        result: Ok(Some(0)),
        calls: RefCell::new(Vec::new()),
    };

    let error = run_with(
        WorkloadRequest {
            class: WorkloadClass::Interactive,
            memory_max_bytes: GIB,
            command: vec!["/bin/true".into()],
        },
        "MemTotal: 16777216 kB\n",
        &runner,
        &root.join("ledger"),
        &root.join("safe"),
        &root.join("lease.json"),
        &root.join("supervisor.json"),
    )
    .unwrap_err();

    assert!(error.contains("admission state"), "{error}");
    assert!(
        runner.calls.borrow().is_empty(),
        "closed transition admitted a scope"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: workload_start_serializes_against_close_admission_transition
fn workload_start_serializes_against_close_admission_transition() {
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
    let runner = TransitionBlockingRunner {
        ledger_root: Arc::new(ledger_root.clone()),
        supervisor: Arc::new(supervisor.clone()),
        calls: RefCell::new(0),
    };

    run_with(
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
    .unwrap();

    assert_eq!(*runner.calls.borrow(), 1);
    assert!(
        LedgerPaths::new(&ledger_root).transition.exists(),
        "workload transition protocol must retain its crash-safe owner record"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: scope_invocation_id_is_persisted
fn scope_invocation_id_is_persisted() {
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
    let runner = StartPublishingRunner {
        ledger_root: Arc::new(ledger_root.clone()),
        supervisor: Arc::new(supervisor),
        start_calls: RefCell::new(0),
        wait_calls: Arc::new(AtomicUsize::new(0)),
    };

    run_with(
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
    .unwrap();

    assert_eq!(*runner.start_calls.borrow(), 1);
    assert_eq!(runner.wait_calls.load(AtomicOrdering::SeqCst), 1);
    fs::remove_dir_all(root).unwrap();
}
