// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/benchmark/tests/worker_and_reporting.rs
use super::*;

#[test]
fn benchmark_enforces_preconditions_selection_caps_and_refusals() {
    let root = make_temp_root("preconditions");
    let now = 1_760_000_000_000;
    let (v0, m0, mm0) = make_volume_dir(&root, "v0");
    let dev0 = make_native_device_at(0, &v0, m0, &mm0, BENCHMARK_REQUIRED_FREE_BYTES, now);
    let plan_req = BenchmarkRequest {
        action: BenchmarkAction::Plan,
        use_case: BenchmarkUseCase::Swap,
        json: false,
        profile_path: None,
        batch: Vec::new(),
    };

    // 1. WSL2 refused before any disk write.
    let wsl = fixture_snapshot();
    assert!(
        build_benchmark_plan(&wsl, &plan_req, None, &healthy_runtime(), now)
            .unwrap_err()
            .to_string()
            .contains("WSL2")
    );

    // 2. Active RamShared swap device or active cascade state refused.
    let mut active_swap = make_native_snapshot(vec![dev0.clone()], now);
    active_swap.swaps.push(SwapDevice {
        filename: "/dev/ramshared0".into(),
        kind: "partition".into(),
        size_kib: 1024,
        used_kib: 0,
        priority: 100,
    });
    assert!(build_benchmark_plan(&active_swap, &plan_req, None, &healthy_runtime(), now).is_err());

    let mut active_health = healthy_runtime();
    active_health.ramshared_off = false;
    assert!(
        build_benchmark_plan(
            &make_native_snapshot(vec![dev0.clone()], now),
            &plan_req,
            None,
            &active_health,
            now
        )
        .is_err()
    );

    // 3. Missing/high PSI memory pressure or low free RAM refused.
    let missing_psi = RuntimeHealthSnapshot {
        psi_full_avg10: None,
        ..healthy_runtime()
    };
    assert!(
        build_benchmark_plan(
            &make_native_snapshot(vec![dev0.clone()], now),
            &plan_req,
            None,
            &missing_psi,
            now
        )
        .is_err()
    );

    let high_psi = RuntimeHealthSnapshot {
        psi_full_avg10: Some(2.0),
        ..healthy_runtime()
    };
    assert!(
        build_benchmark_plan(
            &make_native_snapshot(vec![dev0.clone()], now),
            &plan_req,
            None,
            &high_psi,
            now
        )
        .is_err()
    );

    let mut low_ram = make_native_snapshot(vec![dev0.clone()], now);
    low_ram.guest_memory.available_bytes = Some(128 * 1024 * 1024);
    assert!(build_benchmark_plan(&low_ram, &plan_req, None, &healthy_runtime(), now).is_err());

    // 4. Stale snapshot & insufficient volume reserve refused.
    assert!(
        build_benchmark_plan(
            &make_native_snapshot(vec![dev0.clone()], now - 60_000),
            &plan_req,
            None,
            &healthy_runtime(),
            now
        )
        .is_err()
    );
    let low_disk = make_native_device_at(0, &v0, m0, &mm0, BENCHMARK_REQUIRED_FREE_BYTES - 1, now);
    assert!(
        build_benchmark_plan(
            &make_native_snapshot(vec![low_disk], now),
            &plan_req,
            None,
            &healthy_runtime(),
            now
        )
        .is_err()
    );

    // 5. > 8 eligible volumes requires explicit --batch or --profile selection; selecting <= 8 works!
    let mut nine_devs = Vec::with_capacity(9);
    for i in 0..9 {
        let (vd, md, mmd) = make_volume_dir(&root, &format!("v{i}"));
        nine_devs.push(make_native_device_at(
            i,
            &vd,
            md,
            &mmd,
            BENCHMARK_REQUIRED_FREE_BYTES,
            now,
        ));
    }
    let nine_snap = make_native_snapshot(nine_devs, now);
    assert!(
        build_benchmark_plan(&nine_snap, &plan_req, None, &healthy_runtime(), now)
            .unwrap_err()
            .to_string()
            .contains("max 8 per run")
    );

    let batch_req = BenchmarkRequest {
        batch: vec!["fs-uuid-0".into(), "fs-uuid-1".into()],
        ..plan_req.clone()
    };
    let batch_plan = build_benchmark_plan(&nine_snap, &batch_req, None, &healthy_runtime(), now)
        .expect("explicit batch of 2 succeeds");
    assert_eq!(batch_plan.volumes.len(), 2);
    assert_eq!(
        batch_plan.max_total_write_bytes,
        2 * PER_VOLUME_WRITE_CAP_BYTES
    );

    let profile_toml = r#"
schema_version = 1

[[targets]]
kind = "linux_swapfile"
filesystem_uuid = "fs-uuid-2"
device_identity = "wwn:wwn-2"
managed_relative_path = "var/lib/ramshared/swap/bench.swap"
bytes = 1048576
priority = 100
"#;
    let prof_req = BenchmarkRequest {
        profile_path: Some("/mock/profile.toml".into()),
        ..plan_req.clone()
    };
    let prof_plan = build_benchmark_plan(
        &nine_snap,
        &prof_req,
        Some(profile_toml),
        &healthy_runtime(),
        now,
    )
    .expect("profile selects matching target");
    assert_eq!(prof_plan.volumes.len(), 1);
    assert_eq!(prof_plan.volumes[0].filesystem_uuid, "fs-uuid-2");

    // Empty profile & bad batch refused cleanly.
    let empty_profile = "schema_version = 1\n";
    assert!(
        build_benchmark_plan(
            &nine_snap,
            &prof_req,
            Some(empty_profile),
            &healthy_runtime(),
            now
        )
        .is_err()
    );

    let dup_batch = BenchmarkRequest {
        batch: vec!["fs-uuid-0".into(), "fs-uuid-0".into()],
        ..plan_req
    };
    assert!(build_benchmark_plan(&nine_snap, &dup_batch, None, &healthy_runtime(), now).is_err());

    fs::remove_dir_all(&root).expect("cleanup");
}

#[test]
fn benchmark_ranking_rotation_and_conflict_yields_tie() {
    let make_vol = |idx: usize| BenchmarkVolume {
        filesystem_uuid: format!("uuid-{idx}"),
        device_identity: format!("wwn:disk-{idx}"),
        display_name: format!("disk-{idx}"),
        mountpoint: format!("/mnt/v{idx}"),
        mount_id: 100 + idx as u64,
        major_minor: format!("259:{idx}"),
        filesystem: "ext4".into(),
        free_bytes: BENCHMARK_REQUIRED_FREE_BYTES,
        required_free_bytes: BENCHMARK_REQUIRED_FREE_BYTES,
        capacity_observed_unix_ms: 1_760_000_000_000,
    };
    let v0 = make_vol(0);
    let v1 = make_vol(1);
    let v2 = make_vol(2);

    // Rotation shifts starting volume each round.
    let sched = rotate_schedule(&[v0.clone(), v1.clone(), v2]);
    assert_eq!(sched[0][0], v0.stable_id());
    assert_eq!(sched[1][0], v1.stable_id());
    assert_ne!(sched[2][0], sched[0][0]);

    // Clear swap leader: vol 1 is >10% faster sequentially AND no worse on 4KiB p99.
    let r0 = summarize_volume(
        &v0,
        &[
            synthetic_latencies_us(0, 0),
            synthetic_latencies_us(0, 1),
            synthetic_latencies_us(0, 2),
        ],
    )
    .unwrap();
    let r1 = summarize_volume(
        &v1,
        &[
            synthetic_latencies_us(1, 0),
            synthetic_latencies_us(1, 1),
            synthetic_latencies_us(1, 2),
        ],
    )
    .unwrap();
    assert_eq!(
        r1.small_write_sample_count,
        ROUNDS_PER_VOLUME * SMALL_WRITES_PER_SAMPLE
    );
    let rec_swap = recommend_leader(BenchmarkUseCase::Swap, &[r0.clone(), r1.clone()]);
    assert_eq!(rec_swap.status, "recommended");
    assert_eq!(
        rec_swap.recommended_stable_id.as_deref(),
        Some(v1.stable_id().as_str())
    );

    let rec_origin = recommend_leader(BenchmarkUseCase::Origin, &[r0.clone(), r1]);
    assert_eq!(rec_origin.status, "recommended");
    assert_eq!(
        rec_origin.recommended_stable_id.as_deref(),
        Some(v1.stable_id().as_str())
    );

    // Conflict: vol 2 has higher seq throughput (>=10%) than vol 0, but MUCH slower 4KiB p99 -> MUST report tie.
    let conflict_vol2 = summarize_volume(
        &v1,
        &[
            sample_outcome(120_000_000, vec![2_000_000; SMALL_WRITES_PER_SAMPLE], "bb"),
            sample_outcome(120_000_000, vec![2_000_000; SMALL_WRITES_PER_SAMPLE], "bb"),
            sample_outcome(120_000_000, vec![2_000_000; SMALL_WRITES_PER_SAMPLE], "bb"),
        ],
    )
    .unwrap();
    let conflict_rec =
        recommend_leader(BenchmarkUseCase::Swap, &[r0.clone(), conflict_vol2.clone()]);
    assert_eq!(conflict_rec.status, "tie");
    assert!(conflict_rec.recommended_stable_id.is_none());

    // < 10% margin on durable 4 KiB swap -> tie.
    let near_tie_vol = summarize_volume(
        &v1,
        &[
            sample_outcome(300_000_000, vec![440_000; SMALL_WRITES_PER_SAMPLE], "cc"),
            sample_outcome(300_000_000, vec![440_000; SMALL_WRITES_PER_SAMPLE], "cc"),
            sample_outcome(300_000_000, vec![440_000; SMALL_WRITES_PER_SAMPLE], "cc"),
        ],
    )
    .unwrap();
    let near_rec = recommend_leader(BenchmarkUseCase::Swap, &[r0, near_tie_vol]);
    assert_eq!(near_rec.status, "tie");
    assert!(near_rec.recommended_stable_id.is_none());

    let origin_base = summarize_volume(
        &v0,
        &[
            synthetic_latencies_us(0, 0),
            synthetic_latencies_us(0, 1),
            synthetic_latencies_us(0, 2),
        ],
    )
    .unwrap();
    let origin_conflict = recommend_leader(BenchmarkUseCase::Origin, &[origin_base, conflict_vol2]);
    assert_eq!(origin_conflict.status, "tie");
}

#[test]
fn benchmark_plan_consent_audit_frontier_and_abort_paths() {
    let root = make_temp_root("orchestration");
    let state_dir = root.join("state");
    let (v0, m0, mm0) = make_volume_dir(&root, "v0");
    let (v1, m1, mm1) = make_volume_dir(&root, "v1");
    let now = 1_760_000_000_000;
    let uid = current_uid();
    let dev0 = make_native_device_at(0, &v0, m0, &mm0, BENCHMARK_REQUIRED_FREE_BYTES, now);
    let dev1 = make_native_device_at(1, &v1, m1, &mm1, BENCHMARK_REQUIRED_FREE_BYTES, now);
    let snapshot = make_native_snapshot(vec![dev0.clone(), dev1], now);

    // 1. Read-only --plan leaves zero state files or audit logs.
    let mut plan_out = Vec::new();
    let mut plan_env = make_test_env(
        state_dir.clone(),
        snapshot.clone(),
        healthy_runtime(),
        now,
        Box::new(FileAuditSink::new(state_dir.clone(), uid)),
        Box::new(StubWorkerRunner::from_outcomes(Vec::new())),
    );
    let plan_req = BenchmarkRequest {
        action: BenchmarkAction::Plan,
        use_case: BenchmarkUseCase::Swap,
        json: false,
        profile_path: None,
        batch: Vec::new(),
    };
    run_with_env(&plan_req, &mut plan_env, &mut plan_out).expect("plan succeeds");
    assert!(
        String::from_utf8(plan_out)
            .unwrap()
            .contains("Read-only preview.")
    );
    assert!(!state_dir.exists());

    // 2. Non-TTY InteractiveOrConfirm refuses cleanly without --confirm.
    let interactive_req = BenchmarkRequest {
        action: BenchmarkAction::InteractiveOrConfirm,
        ..plan_req.clone()
    };
    assert!(
        run_with_env(&interactive_req, &mut plan_env, &mut Vec::new())
            .unwrap_err()
            .to_string()
            .contains("--confirm")
    );

    // Interactive TTY consent decline vs confirm through run_with_env.
    let plan = build_benchmark_plan(&snapshot, &plan_req, None, &healthy_runtime(), now).unwrap();
    assert!(render_plan_text(&plan).contains("Rounds: 3"));

    let mut tty_decline_env = make_test_env(
        state_dir.clone(),
        snapshot.clone(),
        healthy_runtime(),
        now,
        Box::new(FileAuditSink::new(state_dir.clone(), uid)),
        Box::new(StubWorkerRunner::from_outcomes(Vec::new())),
    );
    tty_decline_env.is_tty = true;
    tty_decline_env.consent_reader = Box::new(Cursor::new(b"n\n".to_vec()));
    assert!(
        run_with_env(&interactive_req, &mut tty_decline_env, &mut Vec::new())
            .unwrap_err()
            .to_string()
            .contains("declined")
    );

    let mut tty_accept_env = make_test_env(
        state_dir.clone(),
        snapshot.clone(),
        healthy_runtime(),
        now,
        Box::new(FileAuditSink::new(state_dir.clone(), uid)),
        Box::new(StubWorkerRunner::from_outcomes(vec![
            Ok(WorkerRunOutcome::Completed(synthetic_latencies_us(0, 0))),
            Ok(WorkerRunOutcome::Completed(synthetic_latencies_us(1, 0))),
            Ok(WorkerRunOutcome::Completed(synthetic_latencies_us(1, 1))),
            Ok(WorkerRunOutcome::Completed(synthetic_latencies_us(0, 1))),
            Ok(WorkerRunOutcome::Completed(synthetic_latencies_us(0, 2))),
            Ok(WorkerRunOutcome::Completed(synthetic_latencies_us(1, 2))),
        ])),
    );
    tty_accept_env.is_tty = true;
    tty_accept_env.consent_reader = Box::new(Cursor::new(b"BENCHMARK\n".to_vec()));
    let mut prompt_out = Vec::new();
    run_with_env(&interactive_req, &mut tty_accept_env, &mut prompt_out)
        .expect("interactive BENCHMARK consent runs bounded benchmark");
    assert!(
        fs::read_to_string(state_dir.join("audit.jsonl"))
            .unwrap()
            .contains("\"consent\":\"interactive_tty\"")
    );

    // 3. Intent audit failure aborts BEFORE calling worker_runner.
    let confirm_req = BenchmarkRequest {
        action: BenchmarkAction::Confirmed,
        ..plan_req
    };
    let mut intent_env = make_test_env(
        state_dir.clone(),
        snapshot.clone(),
        healthy_runtime(),
        now,
        Box::new(FailingAuditSink {
            fail_on_action: "benchmark_intent",
            inner: FileAuditSink::new(state_dir.clone(), uid),
        }),
        Box::new(StubWorkerRunner::from_outcomes(Vec::new())),
    );
    assert!(
        run_with_env(&confirm_req, &mut intent_env, &mut Vec::new())
            .unwrap_err()
            .to_string()
            .contains("benchmark_intent")
    );

    // 4. Sample result audit failure preserves lease + returns manual_recovery_required!
    let mut result_fail_env = make_test_env(
        state_dir.clone(),
        snapshot.clone(),
        healthy_runtime(),
        now,
        Box::new(FailingAuditSink {
            fail_on_action: "benchmark_sample_result",
            inner: FileAuditSink::new(state_dir.clone(), uid),
        }),
        Box::new(StubWorkerRunner::from_outcomes(vec![Ok(
            WorkerRunOutcome::Completed(synthetic_latencies_us(0, 0)),
        )])),
    );
    let err = run_with_env(&confirm_req, &mut result_fail_env, &mut Vec::new())
        .unwrap_err()
        .to_string();
    assert!(err.contains("manual_recovery_required"));
    assert!(read_existing_lease(&state_dir, uid).unwrap().is_some());
    // Clear the preserved lease via --recover.
    let recover_req = BenchmarkRequest {
        action: BenchmarkAction::Recover,
        use_case: BenchmarkUseCase::Swap,
        json: false,
        profile_path: None,
        batch: Vec::new(),
    };
    run_with_env(&recover_req, &mut plan_env, &mut Vec::new())
        .expect("recover clears preserved lease");

    // 5. Full 2-volume confirmed run in text output mode & mid-run PSI failure abort.
    let stub = StubWorkerRunner::from_outcomes(vec![
        Ok(WorkerRunOutcome::Completed(synthetic_latencies_us(0, 0))),
        Ok(WorkerRunOutcome::Completed(synthetic_latencies_us(1, 0))),
        Ok(WorkerRunOutcome::Completed(synthetic_latencies_us(1, 1))),
        Ok(WorkerRunOutcome::Completed(synthetic_latencies_us(0, 1))),
        Ok(WorkerRunOutcome::Completed(synthetic_latencies_us(0, 2))),
        Ok(WorkerRunOutcome::Completed(synthetic_latencies_us(1, 2))),
    ]);
    let mut text_env = make_test_env(
        state_dir.clone(),
        snapshot.clone(),
        healthy_runtime(),
        now,
        Box::new(FileAuditSink::new(state_dir.clone(), uid)),
        Box::new(stub),
    );
    let mut report_out = Vec::new();
    run_with_env(&confirm_req, &mut text_env, &mut report_out).expect("full 2-vol run succeeds");
    let rendered = String::from_utf8(report_out).unwrap();
    assert!(rendered.contains("Recommendation (recommended):"));
    assert!(rendered.contains("cleanup verified=true"));

    // Mid-run health degradation in round 1 aborts and emits benchmark_aborted audit event.
    let call_count = AtomicUsize::new(0);
    let mut mid_fail_env = make_test_env(
        state_dir.clone(),
        snapshot,
        healthy_runtime(),
        now,
        Box::new(FileAuditSink::new(state_dir.clone(), uid)),
        Box::new(StubWorkerRunner::from_outcomes(vec![Ok(
            WorkerRunOutcome::Completed(synthetic_latencies_us(0, 0)),
        )])),
    );
    mid_fail_env.probe_health = Box::new(|| {
        if call_count.fetch_add(1, Ordering::Relaxed) >= 2 {
            RuntimeHealthSnapshot {
                ramshared_off: false,
                ..healthy_runtime()
            }
        } else {
            healthy_runtime()
        }
    });
    assert!(run_with_env(&confirm_req, &mut mid_fail_env, &mut Vec::new()).is_err());
    let audit_log = fs::read_to_string(state_dir.join("audit.jsonl")).unwrap();
    assert!(audit_log.contains("\"result\":\"aborted\""));

    // Check Plan + report text render helper and revalidate_volume directly.
    let single_snap = make_native_snapshot(vec![dev0], now);
    assert!(revalidate_volume(&single_snap, &plan.volumes[0], &healthy_runtime(), now).is_ok());

    fs::remove_dir_all(&root).expect("cleanup");
}

#[test]
fn benchmark_custody_rejects_symlinks_traversal_and_bad_permissions() {
    let root = make_temp_root("custody-sec");
    let real_dir = root.join("real-state");
    fs::create_dir_all(&real_dir).unwrap();
    fs::set_permissions(&real_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let uid = current_uid();

    assert!(open_verified_dir_chain(Path::new("relative/dir"), false, uid).is_err());
    let link_dir = root.join("symlink-state");
    symlink(&real_dir, &link_dir).unwrap();
    assert!(open_verified_dir_chain(&link_dir, false, uid).is_err());

    let world_writable = root.join("weak-dir");
    fs::create_dir_all(&world_writable).unwrap();
    fs::set_permissions(&world_writable, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(open_verified_dir_chain(&world_writable, false, uid).is_err());

    let symlink_audit = real_dir.join("audit.jsonl");
    symlink("/dev/null", &symlink_audit).unwrap();
    let mut audit = FileAuditSink::new(real_dir.clone(), uid);
    let ev = AuditEvent {
        schema_version: 1,
        tx_id: "tx-sec".into(),
        timestamp_unix_ms: 1_760_000_000_000,
        action: "benchmark_intent".into(),
        use_case: BenchmarkUseCase::Swap,
        plan_hash: "hash".into(),
        target_identities: Vec::new(),
        source_sample_timestamps: Vec::new(),
        per_volume_cap_bytes: PER_VOLUME_WRITE_CAP_BYTES,
        run_cap_bytes: RUN_WRITE_CAP_BYTES,
        consent: "confirmed_flag".into(),
        result: "intent_recorded".into(),
        round: None,
        bytes_written: None,
        readback_sha256: None,
        cleanup_verified: None,
        pending_recovery: None,
    };
    assert!(audit.record_event(&ev).is_err());

    // prove_process_exited rejects live EPERM error vs NotFound ok.
    let lease = WorkerLease {
        schema_version: 1,
        tx_id: "tx-p".into(),
        plan_hash: "p".into(),
        volume_filesystem_uuid: "u".into(),
        volume_device_identity: "d".into(),
        mountpoint: "/tmp".into(),
        mount_id: 1,
        major_minor: "0:0".into(),
        temp_relative_path: relative_temp_path("bench.bin"),
        byte_ceiling: SAMPLE_PAYLOAD_BYTES,
        status: WorkerLeaseStatus::Active,
        boot_id: "boot-a".into(),
        pid: 123,
        process_start_time: 456,
        created_unix_ms: 1,
        file_dev: None,
        file_ino: None,
        detail: None,
    };
    assert!(
        prove_process_exited(&lease, "boot-a", &|_| {
            Err(io::Error::from_raw_os_error(
                rustix::io::Errno::PERM.raw_os_error(),
            ))
        })
        .is_err()
    );
    assert!(prove_process_exited(&lease, "boot-other", &|_| Ok(456)).is_ok());

    // verify_or_cleanup_volume_temp cleans existing file and ignores absent file.
    let (v_dir, m_id, mm) = make_volume_dir(&root, "vol-clean");
    let vol = BenchmarkVolume {
        filesystem_uuid: "uuid-c".into(),
        device_identity: "wwn:c".into(),
        display_name: "vol-c".into(),
        mountpoint: v_dir.to_string_lossy().into_owned(),
        mount_id: m_id,
        major_minor: mm,
        filesystem: "ext4".into(),
        free_bytes: BENCHMARK_REQUIRED_FREE_BYTES,
        required_free_bytes: BENCHMARK_REQUIRED_FREE_BYTES,
        capacity_observed_unix_ms: 1,
    };
    verify_or_cleanup_volume_temp(&vol, "bench-absent.bin", None, uid)
        .expect("absent temp file is clean");

    // recover_lease handles no-lease case cleanly.
    assert!(
        recover_lease(
            &root.join("nonexistent-state"),
            uid,
            "boot-a",
            &read_linux_proc_start_time,
            |_| Ok(()),
        )
        .unwrap()
        .is_none()
    );

    fs::remove_dir_all(&root).expect("cleanup");
}

fn timing_fixture() -> (BenchmarkVolume, Vec<WorkerSampleOutcome>) {
    let volume = BenchmarkVolume {
        filesystem_uuid: "fixture-uuid".into(),
        device_identity: "fixture-device".into(),
        display_name: "fixture".into(),
        mountpoint: "/fixture".into(),
        mount_id: 1,
        major_minor: "1:1".into(),
        filesystem: "ext4".into(),
        free_bytes: BENCHMARK_REQUIRED_FREE_BYTES,
        required_free_bytes: BENCHMARK_REQUIRED_FREE_BYTES,
        capacity_observed_unix_ms: 1,
    };
    let sample = sample_outcome(100_000_000, vec![100_000; SMALL_WRITES_PER_SAMPLE], "de");
    (volume, vec![sample; ROUNDS_PER_VOLUME])
}

#[test]
fn benchmark_rejects_zero_sequential_duration() {
    let (volume, mut samples) = timing_fixture();
    assert!(summarize_volume(&volume, &samples).is_ok());
    samples[0].seq_elapsed_ns = 0;
    assert!(
        summarize_volume(&volume, &samples)
            .unwrap_err()
            .to_string()
            .contains("zero-duration")
    );
}

#[test]
fn benchmark_rejects_zero_small_write_duration() {
    let (volume, mut samples) = timing_fixture();
    samples[1].small_write_latencies_ns[0] = 0;
    assert!(
        summarize_volume(&volume, &samples)
            .unwrap_err()
            .to_string()
            .contains("zero-duration")
    );
}
