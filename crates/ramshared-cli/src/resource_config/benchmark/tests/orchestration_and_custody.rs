// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/benchmark/tests/orchestration_and_custody.rs
use super::*;

#[test]
fn benchmark_binds_volume_caps_bytes_and_verifies_cleanup() {
    let root = make_temp_root("binds-and-cleanup");
    let state_dir = root.join("state");
    let (vol_dir, mount_id, major_minor) = make_volume_dir(&root, "vol-a");
    let now = 1_760_000_000_000;
    let dev_a = make_native_device_at(
        0,
        &vol_dir,
        mount_id,
        &major_minor,
        BENCHMARK_REQUIRED_FREE_BYTES + 1,
        now,
    );
    let snapshot = make_native_snapshot(vec![dev_a], now);
    let seen_rounds = Rc::new(RefCell::new(Vec::new()));
    let runner = RealSampleWorkerRunner {
        seen_rounds: Rc::clone(&seen_rounds),
    };
    let audit = Box::new(FileAuditSink::new(state_dir.clone(), current_uid()));
    let mut env = make_test_env(
        state_dir.clone(),
        snapshot,
        healthy_runtime(),
        now,
        audit,
        Box::new(runner),
    );

    let request = BenchmarkRequest {
        action: BenchmarkAction::Confirmed,
        use_case: BenchmarkUseCase::Swap,
        json: true,
        profile_path: None,
        batch: Vec::new(),
    };
    let mut out = Vec::new();
    run_with_env(&request, &mut env, &mut out).expect("real single-volume benchmark run succeeds");

    let rounds = seen_rounds.borrow();
    assert_eq!(rounds.len(), ROUNDS_PER_VOLUME);
    for (idx, (round, stable_id, cap)) in rounds.iter().enumerate() {
        assert_eq!(*round, idx);
        assert!(stable_id.contains('@'));
        assert_eq!(*cap, SAMPLE_PAYLOAD_BYTES);
    }

    let payload: serde_json::Value =
        serde_json::from_slice(&out).expect("JSON report parses cleanly");
    assert_eq!(payload["rounds"].as_u64(), Some(3));
    assert_eq!(payload["writes_performed"].as_bool(), Some(true));
    assert_eq!(payload["cleanup_verified"].as_bool(), Some(true));
    assert_eq!(
        payload["volumes"][0]["total_bytes_written"].as_u64(),
        Some(PER_VOLUME_WRITE_CAP_BYTES)
    );
    assert_eq!(
        payload["recommendation"]["status"].as_str(),
        Some("single_volume")
    );

    let bench_subdir = vol_dir.join("var/lib/ramshared/resource-config/benchmark");
    assert!(
        fs::read_dir(&bench_subdir)
            .expect("benchmark subdir exists")
            .next()
            .is_none(),
        "no temporary benchmark files may remain after cleanup"
    );
    assert!(
        read_existing_lease(&state_dir, current_uid())
            .expect("read lease")
            .is_none()
    );

    let audit_text = fs::read_to_string(state_dir.join("audit.jsonl")).expect("audit exists");
    let actions: Vec<serde_json::Value> = audit_text
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid audit json"))
        .collect();
    assert_eq!(actions.len(), 5, "intent + 3 sample results + outcome");
    assert_eq!(actions[0]["action"], "benchmark_intent");
    assert_eq!(actions[1]["action"], "benchmark_sample_result");
    assert_eq!(actions[4]["action"], "benchmark_outcome");
    assert_eq!(
        fs::metadata(state_dir.join("audit.jsonl"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    fs::remove_dir_all(&root).expect("cleanup test root");
}

#[test]
fn benchmark_lease_refuses_concurrent_or_unreaped_worker() {
    let root = make_temp_root("lease-custody");
    let state_dir = root.join("state");
    let (vol_dir, mount_id, major_minor) = make_volume_dir(&root, "vol-lease");
    let now = 1_760_000_000_000;
    let uid = current_uid();
    let boot = current_boot_id();
    let self_pid = std::process::id();
    let self_start = read_linux_proc_start_time(self_pid).expect("read own /proc stat");

    let dev = make_native_device_at(
        0,
        &vol_dir,
        mount_id,
        &major_minor,
        BENCHMARK_REQUIRED_FREE_BYTES,
        now,
    );
    let snapshot = make_native_snapshot(vec![dev], now);
    let plan_req = BenchmarkRequest {
        action: BenchmarkAction::Plan,
        use_case: BenchmarkUseCase::Swap,
        json: false,
        profile_path: None,
        batch: Vec::new(),
    };
    let plan = build_benchmark_plan(&snapshot, &plan_req, None, &healthy_runtime(), now)
        .expect("build plan");

    // Pre-create a temp sample file owned by this user on the confined volume directory.
    let rel_leaf = "bench-lease-r0-v0.bin";
    let bench_dir = vol_dir.join("var/lib/ramshared/resource-config/benchmark");
    fs::create_dir_all(&bench_dir).unwrap();
    for ancestor in [
        vol_dir.join("var"),
        vol_dir.join("var/lib"),
        vol_dir.join("var/lib/ramshared"),
        vol_dir.join("var/lib/ramshared/resource-config"),
        bench_dir.clone(),
    ] {
        fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let temp_file = bench_dir.join(rel_leaf);
    fs::write(&temp_file, b"leftover").unwrap();
    fs::set_permissions(&temp_file, fs::Permissions::from_mode(0o600)).unwrap();

    let mut lease = super::super::custody::initial_launching_lease(
        "tx-concurrent",
        &plan.plan_hash,
        &plan.volumes[0],
        &relative_temp_path(rel_leaf),
        ProcessIdentity {
            boot_id: &boot,
            pid: self_pid,
            start_time: self_start,
        },
        now,
    );
    acquire_launching_lease(&state_dir, uid, &lease).expect("first lease succeeds");
    assert!(
        acquire_launching_lease(&state_dir, uid, &lease).is_err(),
        "concurrent second lease must be refused"
    );

    // New confirmed run and read-only plan both refuse while lease exists.
    let mut env = make_test_env(
        state_dir.clone(),
        snapshot.clone(),
        healthy_runtime(),
        now,
        Box::new(FileAuditSink::new(state_dir.clone(), uid)),
        Box::new(StubWorkerRunner::from_outcomes(Vec::new())),
    );
    let confirm_req = BenchmarkRequest {
        action: BenchmarkAction::Confirmed,
        ..plan_req.clone()
    };
    let err = run_with_env(&confirm_req, &mut env, &mut Vec::new()).unwrap_err();
    assert!(err.to_string().contains("--recover"));

    let plan_err = run_with_env(&plan_req, &mut env, &mut Vec::new()).unwrap_err();
    assert!(
        plan_err
            .to_string()
            .contains("existing benchmark worker lease")
    );

    // Recovery refuses while the matching (pid, start_time, boot_id) is still live.
    let recover_req = BenchmarkRequest {
        action: BenchmarkAction::Recover,
        use_case: BenchmarkUseCase::Swap,
        json: true,
        profile_path: None,
        batch: Vec::new(),
    };
    let err = run_with_env(&recover_req, &mut env, &mut Vec::new()).unwrap_err();
    assert!(err.to_string().contains("still running or unreaped"));

    // Simulate boot/PID reuse or exited worker while keeping leftover file: recovery cleans file & lease.
    lease.process_start_time = self_start.saturating_add(999);
    lease.status = WorkerLeaseStatus::WorkerStuck;
    replace_lease(&state_dir, uid, &lease).expect("update stuck lease");

    let mut recover_out = Vec::new();
    run_with_env(&recover_req, &mut env, &mut recover_out)
        .expect("recovery succeeds once worker exited");
    let parsed: serde_json::Value = serde_json::from_slice(&recover_out).unwrap();
    assert_eq!(parsed["recovered"].as_bool(), Some(true));
    assert!(
        !temp_file.exists(),
        "recovery must unlink leftover sample file"
    );
    assert!(read_existing_lease(&state_dir, uid).unwrap().is_none());

    // Second recovery reports clean no-op in text mode.
    let recover_text_req = BenchmarkRequest {
        json: false,
        ..recover_req
    };
    let mut text_out = Vec::new();
    run_with_env(&recover_text_req, &mut env, &mut text_out).expect("clean no-op recover succeeds");
    assert!(
        String::from_utf8(text_out)
            .unwrap()
            .contains("recovered=false")
    );

    fs::remove_dir_all(&root).expect("cleanup");
}

#[test]
fn benchmark_workload_obeys_32mib_payload_ceiling() {
    assert_eq!(
        SEQ_WRITE_BYTES + (SMALL_WRITES_PER_SAMPLE as u64) * (SMALL_WRITE_BYTES as u64),
        SAMPLE_PAYLOAD_BYTES
    );
    assert_eq!(SAMPLE_PAYLOAD_BYTES, 32 * 1024 * 1024);
    assert_eq!(PER_VOLUME_WRITE_CAP_BYTES, 96 * 1024 * 1024);
    assert_eq!(
        RUN_WRITE_CAP_BYTES,
        (MAX_BENCHMARK_VOLUMES as u64) * PER_VOLUME_WRITE_CAP_BYTES
    );

    let mut seen_slots = HashSet::new();
    let mut prev = None;
    for step in 0..SMALL_WRITES_PER_SAMPLE {
        let slot = small_write_slot(step);
        assert!(slot < SMALL_WRITES_PER_SAMPLE);
        assert!(
            seen_slots.insert(slot),
            "slots must be distinct across all 256 writes"
        );
        if let Some(p) = prev {
            assert_ne!(
                slot,
                p + 1,
                "4 KiB offsets must be non-sequential at step {step}"
            );
        }
        prev = Some(slot);
    }

    let root = make_temp_root("payload-ceiling");
    let (vol_dir, mount_id, major_minor) = make_volume_dir(&root, "vol-cap");
    let volume = BenchmarkVolume {
        filesystem_uuid: "uuid-cap".into(),
        device_identity: "wwn:cap".into(),
        display_name: "vol-cap".into(),
        mountpoint: vol_dir.to_string_lossy().into_owned(),
        mount_id,
        major_minor,
        filesystem: "ext4".into(),
        free_bytes: BENCHMARK_REQUIRED_FREE_BYTES,
        required_free_bytes: BENCHMARK_REQUIRED_FREE_BYTES,
        capacity_observed_unix_ms: 1_760_000_000_000,
    };

    // Refuse oversized byte_ceiling and out-of-range round.
    let bad_cap = execute_worker_sample(&WorkerRequest {
        tx_id: "tx-bad".into(),
        round: 0,
        volume: volume.clone(),
        temp_leaf_name: "bench-bad.bin".into(),
        sample_seed: 7,
        byte_ceiling: SAMPLE_PAYLOAD_BYTES + 4096,
        owner_uid: current_uid(),
    });
    assert!(matches!(
        bad_cap,
        WorkerReply::Failed {
            cleanup_verified: true,
            ..
        }
    ));

    let bad_leaf = execute_worker_sample(&WorkerRequest {
        tx_id: "tx-bad-leaf".into(),
        round: 0,
        volume: volume.clone(),
        temp_leaf_name: "../escape.bin".into(),
        sample_seed: 7,
        byte_ceiling: SAMPLE_PAYLOAD_BYTES,
        owner_uid: current_uid(),
    });
    assert!(matches!(
        bad_leaf,
        WorkerReply::Failed {
            cleanup_verified: true,
            ..
        }
    ));

    let ok = execute_worker_sample(&WorkerRequest {
        tx_id: "tx-ok".into(),
        round: 2,
        volume,
        temp_leaf_name: "bench-ok.bin".into(),
        sample_seed: 42,
        byte_ceiling: SAMPLE_PAYLOAD_BYTES,
        owner_uid: current_uid(),
    });
    match ok {
        WorkerReply::Completed(outcome) => {
            assert_eq!(outcome.bytes_written, SAMPLE_PAYLOAD_BYTES);
            assert_eq!(
                outcome.small_write_latencies_ns.len(),
                SMALL_WRITES_PER_SAMPLE
            );
            assert_eq!(outcome.observed_sha256.len(), 64);
            assert!(outcome.cleanup_verified);
        }
        WorkerReply::Failed { detail, .. } => panic!("expected clean sample: {detail}"),
    }

    fs::remove_dir_all(&root).expect("cleanup");
}

#[test]
fn benchmark_subprocess_worker_real_handshake_and_timeout_recovery() {
    let root = make_temp_root("subprocess-worker");
    let state_dir = root.join("state");
    let (vol_dir, mount_id, major_minor) = make_volume_dir(&root, "vol-sub");
    let uid = current_uid();
    let boot = current_boot_id();
    let now = 1_760_000_000_000;
    let volume = BenchmarkVolume {
        filesystem_uuid: "uuid-sub".into(),
        device_identity: "wwn:sub".into(),
        display_name: "vol-sub".into(),
        mountpoint: vol_dir.to_string_lossy().into_owned(),
        mount_id,
        major_minor,
        filesystem: "ext4".into(),
        free_bytes: BENCHMARK_REQUIRED_FREE_BYTES,
        required_free_bytes: BENCHMARK_REQUIRED_FREE_BYTES,
        capacity_observed_unix_ms: now,
    };

    let worker_req = WorkerRequest {
        tx_id: "tx-subproc-1".into(),
        round: 0,
        volume: volume.clone(),
        temp_leaf_name: "bench-subproc-0.bin".into(),
        sample_seed: 99,
        byte_ceiling: SAMPLE_PAYLOAD_BYTES,
        owner_uid: uid,
    };

    // 0. Direct in-memory run_worker_from_io executes the real 32 MiB write + READY -> GO -> JSON protocol.
    let encoded_req = serde_json::to_string(&worker_req).unwrap();
    let io_input = format!("{encoded_req}\nGO\n");
    let mut io_out = Vec::new();
    run_worker_from_io(&mut io::Cursor::new(io_input.into_bytes()), &mut io_out)
        .expect("run_worker_from_io protocol succeeds");
    let io_text = String::from_utf8(io_out).unwrap();
    let mut lines = io_text.lines();
    assert_eq!(lines.next(), Some("READY"));
    let reply_json = lines.next().expect("worker emits serialized reply");
    let parsed_reply: WorkerReply = serde_json::from_str(reply_json).unwrap();
    assert!(matches!(
        parsed_reply,
        WorkerReply::Completed(ref s) if s.bytes_written == SAMPLE_PAYLOAD_BYTES && s.cleanup_verified
    ));

    // 1. SubprocessWorkerRunner full two-phase READY -> lease PID/starttime persisted -> GO -> reply.
    let self_start = read_linux_proc_start_time(std::process::id()).unwrap();
    let owner = ProcessIdentity {
        boot_id: &boot,
        pid: std::process::id(),
        start_time: self_start,
    };
    let mut lease = super::super::custody::initial_launching_lease(
        "tx-subproc-1",
        "plan-hash-1",
        &volume,
        &relative_temp_path(&worker_req.temp_leaf_name),
        owner,
        now,
    );
    acquire_launching_lease(&state_dir, uid, &lease).unwrap();

    let fake_reply = serde_json::to_string(&WorkerReply::Completed(sample_outcome(
        100_000_000,
        vec![90_000; SMALL_WRITES_PER_SAMPLE],
        "ee",
    )))
    .unwrap();
    let mut runner = SubprocessWorkerRunner::with_command(
        PathBuf::from("/bin/sh"),
        vec![
            "-c".into(),
            "IFS= read -r _req; printf 'READY\\n'; IFS= read -r go; [ \"$go\" = \"GO\" ] && printf '%s\\n' \"$BENCH_FAKE_REPLY\"".into(),
        ],
        vec![("BENCH_FAKE_REPLY".into(), fake_reply)],
        Duration::from_secs(10),
        Box::new(NativeProcessControl),
    );
    let mut audit = FileAuditSink::new(state_dir.clone(), uid);
    let run_ctx_swap = WorkerRunContext {
        state_dir: &state_dir,
        owner_uid: uid,
        current_boot_id: &boot,
        use_case: BenchmarkUseCase::Swap,
    };
    let outcome = runner
        .run_sample(&worker_req, run_ctx_swap, &mut lease, &mut audit)
        .expect("real subprocess handshake runs");
    assert!(matches!(
        outcome,
        WorkerRunOutcome::Completed(ref s) if s.bytes_written == SAMPLE_PAYLOAD_BYTES && s.cleanup_verified
    ));
    assert_ne!(lease.pid, std::process::id());
    assert_eq!(lease.status, WorkerLeaseStatus::Active);
    clear_verified_lease(&state_dir, uid, "tx-subproc-1").unwrap();

    // 2. Child hangs after GO: killed & reaped within deadline, verified clean -> WorkerRunOutcome::Failed.
    let mut timeout_lease = super::super::custody::initial_launching_lease(
        "tx-subproc-2",
        "plan-hash-2",
        &volume,
        &relative_temp_path("bench-subproc-1.bin"),
        owner,
        now,
    );
    acquire_launching_lease(&state_dir, uid, &timeout_lease).unwrap();
    let mut hang_runner = SubprocessWorkerRunner::with_command(
        PathBuf::from("/bin/sh"),
        vec![
            "-c".into(),
            "IFS= read -r _req; printf 'READY\\n'; IFS= read -r _go; sleep 10".into(),
        ],
        Vec::new(),
        Duration::from_millis(80),
        Box::new(NativeProcessControl),
    );
    let mut hang_req = worker_req.clone();
    hang_req.tx_id = "tx-subproc-2".into();
    hang_req.temp_leaf_name = "bench-subproc-1.bin".into();
    let hang_outcome = hang_runner
        .run_sample(&hang_req, run_ctx_swap, &mut timeout_lease, &mut audit)
        .expect("timeout outcome returns cleanly");
    assert!(matches!(
        hang_outcome,
        WorkerRunOutcome::Failed { ref detail } if detail.contains("exceeded 120s volume deadline")
    ));
    clear_verified_lease(&state_dir, uid, "tx-subproc-2").unwrap();

    // 3. Child hangs before READY with stuck process control -> persists WorkerStuck lease & audit!
    let mut stuck_lease = super::super::custody::initial_launching_lease(
        "tx-subproc-3",
        "plan-hash-3",
        &volume,
        &relative_temp_path("bench-subproc-2.bin"),
        owner,
        now,
    );
    acquire_launching_lease(&state_dir, uid, &stuck_lease).unwrap();
    let mut stuck_runner = SubprocessWorkerRunner::with_command(
        PathBuf::from("/bin/sh"),
        vec!["-c".into(), "sleep 10".into()],
        Vec::new(),
        Duration::from_millis(60),
        Box::new(StuckProcessControl),
    );
    let mut stuck_req = worker_req;
    stuck_req.tx_id = "tx-subproc-3".into();
    stuck_req.temp_leaf_name = "bench-subproc-2.bin".into();
    let stuck_outcome = stuck_runner
        .run_sample(
            &stuck_req,
            WorkerRunContext {
                use_case: BenchmarkUseCase::Origin,
                ..run_ctx_swap
            },
            &mut stuck_lease,
            &mut audit,
        )
        .expect("stuck worker persists recovery state");
    assert!(matches!(
        stuck_outcome,
        WorkerRunOutcome::WorkerStuck { .. }
    ));
    let saved = read_existing_lease(&state_dir, uid).unwrap().unwrap();
    assert_eq!(saved.status, WorkerLeaseStatus::WorkerStuck);

    fs::remove_dir_all(&root).expect("cleanup");
}
