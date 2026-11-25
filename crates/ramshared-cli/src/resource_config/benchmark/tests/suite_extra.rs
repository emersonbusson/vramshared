// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/benchmark/tests/suite_extra.rs
use super::*;

#[test]
fn benchmark_worker_error_and_confinement_paths_are_handled_cleanly() {
    let root = make_temp_root("worker-errors");
    let state_dir = root.join("state");
    let (vol_dir, mount_id, major_minor) = make_volume_dir(&root, "vol-err");
    let uid = current_uid();
    let boot = current_boot_id();
    let now = 1_760_000_000_000;
    let volume = BenchmarkVolume {
        filesystem_uuid: "uuid-err".into(),
        device_identity: "wwn:err".into(),
        display_name: "vol-err".into(),
        mountpoint: vol_dir.to_string_lossy().into_owned(),
        mount_id,
        major_minor,
        filesystem: "ext4".into(),
        free_bytes: BENCHMARK_REQUIRED_FREE_BYTES,
        required_free_bytes: BENCHMARK_REQUIRED_FREE_BYTES,
        capacity_observed_unix_ms: now,
    };
    let req = WorkerRequest {
        tx_id: "tx-err-1".into(),
        round: 0,
        volume: volume.clone(),
        temp_leaf_name: "bench-err-0.bin".into(),
        sample_seed: 12,
        byte_ceiling: SAMPLE_PAYLOAD_BYTES,
        owner_uid: uid,
    };

    // 1. run_worker_from_io rejects bad JSON, aborted GO handshake, and I/O failures.
    struct FailingReader;
    impl std::io::Read for FailingReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("injected read error"))
        }
    }
    struct FailingWriter;
    impl std::io::Write for FailingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("injected flush error"))
        }
    }
    assert!(run_worker_from_io(&mut FailingReader, &mut Vec::new()).is_err());
    assert!(
        run_worker_from_io(&mut Cursor::new(b"{bad json}\n".to_vec()), &mut Vec::new()).is_err()
    );
    let encoded_req = serde_json::to_string(&req).unwrap();
    assert!(
        run_worker_from_io(
            &mut Cursor::new(format!("{encoded_req}\nGO\n").into_bytes()),
            &mut FailingWriter
        )
        .is_err()
    );
    let aborted_io = format!("{encoded_req}\nABORT\n");
    assert!(
        run_worker_from_io(&mut Cursor::new(aborted_io.into_bytes()), &mut Vec::new()).is_err()
    );

    let mut audit = FileAuditSink::new(state_dir.clone(), uid);
    let run_ctx = WorkerRunContext {
        state_dir: &state_dir,
        owner_uid: uid,
        current_boot_id: &boot,
        use_case: BenchmarkUseCase::Swap,
    };
    let mut lease = super::super::custody::initial_launching_lease(
        "tx-err-1",
        "plan-hash",
        &volume,
        &relative_temp_path(&req.temp_leaf_name),
        ProcessIdentity {
            boot_id: &boot,
            pid: std::process::id(),
            start_time: read_linux_proc_start_time(std::process::id()).unwrap(),
        },
        now,
    );
    acquire_launching_lease(&state_dir, uid, &lease).unwrap();

    // 2. Missing executable fails spawn cleanly.
    let mut bad_bin = SubprocessWorkerRunner::with_command(
        PathBuf::from("/nonexistent/ramshared-missing-worker"),
        Vec::new(),
        Vec::new(),
        Duration::from_secs(1),
        Box::new(NativeProcessControl),
    );
    assert!(
        bad_bin
            .run_sample(&req, run_ctx, &mut lease, &mut audit)
            .is_err()
    );

    // 3. Invalid READY token fails before disk write cleanly.
    let mut bad_ready = SubprocessWorkerRunner::with_command(
        PathBuf::from("/bin/sh"),
        vec!["-c".into(), "IFS= read -r _req; printf 'NOPE\\n'".into()],
        Vec::new(),
        Duration::from_secs(2),
        Box::new(NativeProcessControl),
    );
    assert!(matches!(
        bad_ready.run_sample(&req, run_ctx, &mut lease, &mut audit).unwrap(),
        WorkerRunOutcome::Failed { ref detail } if detail.contains("READY handshake")
    ));

    // 4. Child exits abnormally without emitting reply -> Failed after cleanup check.
    let mut silent_exit = SubprocessWorkerRunner::with_command(
        PathBuf::from("/bin/sh"),
        vec![
            "-c".into(),
            "IFS= read -r _req; printf 'READY\\n'; IFS= read -r _go; exit 1".into(),
        ],
        Vec::new(),
        Duration::from_secs(2),
        Box::new(NativeProcessControl),
    );
    assert!(matches!(
        silent_exit.run_sample(&req, run_ctx, &mut lease, &mut audit).unwrap(),
        WorkerRunOutcome::Failed { ref detail } if detail.contains("without reply")
    ));

    // 5. Child emits WorkerReply::Failed -> WorkerRunOutcome::Failed.
    let failed_json = serde_json::to_string(&WorkerReply::Failed {
        detail: "simulated io refusal".into(),
        cleanup_verified: true,
    })
    .unwrap();
    let mut failed_reply_runner = SubprocessWorkerRunner::with_command(
        PathBuf::from("/bin/sh"),
        vec![
            "-c".into(),
            "IFS= read -r _req; printf 'READY\\n'; IFS= read -r _go; printf '%s\\n' \"$R\"".into(),
        ],
        vec![("R".into(), failed_json)],
        Duration::from_secs(2),
        Box::new(NativeProcessControl),
    );
    assert!(matches!(
        failed_reply_runner
            .run_sample(&req, run_ctx, &mut lease, &mut audit)
            .unwrap(),
        WorkerRunOutcome::Failed { ref detail } if detail == "simulated io refusal"
    ));

    // 6. Malformed worker reply JSON -> Err.
    let mut bad_reply_runner = SubprocessWorkerRunner::with_command(
        PathBuf::from("/bin/sh"),
        vec![
            "-c".into(),
            "IFS= read -r _req; printf 'READY\\n'; IFS= read -r _go; printf 'not-json\\n'".into(),
        ],
        Vec::new(),
        Duration::from_secs(2),
        Box::new(NativeProcessControl),
    );
    assert!(
        bad_reply_runner
            .run_sample(&req, run_ctx, &mut lease, &mut audit)
            .is_err()
    );

    // 7. observe_exit error stops group + cleans up cleanly.
    let mut obs_err_runner = SubprocessWorkerRunner::with_command(
        PathBuf::from("/bin/sh"),
        vec![
            "-c".into(),
            "IFS= read -r _req; printf 'READY\\n'; IFS= read -r _go; sleep 5".into(),
        ],
        Vec::new(),
        Duration::from_secs(2),
        Box::new(ObserveErrorProcessControl),
    );
    assert!(matches!(
        obs_err_runner
            .run_sample(&req, run_ctx, &mut lease, &mut audit)
            .unwrap(),
        WorkerRunOutcome::Failed { ref detail } if detail.contains("observe benchmark worker")
    ));
    clear_verified_lease(&state_dir, uid, "tx-err-1").unwrap();

    // 8. Leftover non-0600 file in benchmark dir causes cleanup verification failure -> WorkerStuck,
    //    then chmod 0600 lets verify_or_cleanup_volume_temp unlink it; weak dir mode fails open.
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
    let bad_file = bench_dir.join(&req.temp_leaf_name);
    fs::write(&bad_file, b"bad").unwrap();
    fs::set_permissions(&bad_file, fs::Permissions::from_mode(0o644)).unwrap();

    acquire_launching_lease(&state_dir, uid, &lease).unwrap();
    assert!(matches!(
        silent_exit
            .run_sample(&req, run_ctx, &mut lease, &mut audit)
            .unwrap(),
        WorkerRunOutcome::WorkerStuck { ref detail } if detail.contains("cleanup verification failed")
    ));
    assert!(verify_or_cleanup_volume_temp(&volume, &req.temp_leaf_name, None, uid).is_err());

    fs::set_permissions(&bad_file, fs::Permissions::from_mode(0o600)).unwrap();
    verify_or_cleanup_volume_temp(&volume, &req.temp_leaf_name, None, uid)
        .expect("0600 owned leftover file cleans up");
    assert!(!bad_file.exists());

    fs::set_permissions(vol_dir.join("var"), fs::Permissions::from_mode(0o777)).unwrap();
    assert!(matches!(
        execute_worker_sample(&req),
        WorkerReply::Failed {
            cleanup_verified: true,
            ..
        }
    ));

    fs::remove_dir_all(&root).expect("cleanup");
}

#[test]
#[ignore = "requires root privileges (sudo -n) for uid=0 live ext4 3-round config benchmark qualification"]
fn root_live_ext4_config_benchmark_three_rounds_and_audit() {
    use sha2::{Digest, Sha256};

    assert_eq!(
        current_uid(),
        0,
        "must run as root (uid=0) for privileged live ext4 benchmark qualification"
    );
    let exe_bytes = fs::read("/proc/self/exe").expect("read /proc/self/exe");
    let exe_sha256: String = Sha256::digest(&exe_bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let pid = std::process::id();
    let start_ticks = read_linux_proc_start_time(pid).expect("read proc start_time");

    let root = make_temp_root("root-live-ext4");
    let state_dir = root.join("state");
    let (v0, m0, mm0) = make_volume_dir(&root, "ext4-vol0");
    let (v1, m1, mm1) = make_volume_dir(&root, "ext4-vol1");
    let now = crate::resource_config::unix_millis();

    let vfs0 = rustix::fs::fstatvfs(File::open(&v0).unwrap()).unwrap();
    let live_free = vfs0
        .f_bavail
        .saturating_mul(vfs0.f_frsize)
        .max(BENCHMARK_REQUIRED_FREE_BYTES);
    let live_total = vfs0.f_blocks.saturating_mul(vfs0.f_frsize).max(live_free);

    let mut dev0 = make_native_device_at(0, &v0, m0, &mm0, live_free, now);
    dev0.size_bytes = Some(live_total);
    dev0.mounts[0].total_bytes = Some(live_total);
    let mut dev1 = make_native_device_at(1, &v1, m1, &mm1, live_free, now);
    dev1.size_bytes = Some(live_total);
    dev1.mounts[0].total_bytes = Some(live_total);
    let snapshot = make_native_snapshot(vec![dev0, dev1], now);

    let seen_rounds = Rc::new(RefCell::new(Vec::new()));
    let runner = RealSampleWorkerRunner {
        seen_rounds: Rc::clone(&seen_rounds),
    };
    let audit = FileAuditSink::new(state_dir.clone(), 0);
    let mut env = make_test_env(
        state_dir.clone(),
        snapshot,
        healthy_runtime(),
        now,
        Box::new(audit),
        Box::new(runner),
    );

    let req = BenchmarkRequest {
        action: BenchmarkAction::Confirmed,
        use_case: BenchmarkUseCase::Origin,
        json: true,
        profile_path: None,
        batch: Vec::new(),
    };
    let mut out = Vec::new();
    run_with_env(&req, &mut env, &mut out).expect("3-round root live ext4 benchmark must succeed");

    let report: serde_json::Value = serde_json::from_slice(&out).expect("parse benchmark JSON");
    assert_eq!(report["status"].as_str(), Some("completed"));
    assert_eq!(report["rounds"].as_u64(), Some(3));
    assert_eq!(report["writes_performed"].as_bool(), Some(true));
    assert_eq!(report["cleanup_verified"].as_bool(), Some(true));
    assert!(
        read_existing_lease(&state_dir, 0)
            .expect("read lease")
            .is_none(),
        "lease must be cleared after verified 3-round completion"
    );

    let rounds = seen_rounds.borrow();
    assert_eq!(rounds.len(), 6, "2 volumes x 3 rounds must execute");

    let audit_text = fs::read_to_string(state_dir.join("audit.jsonl")).expect("read audit.jsonl");
    let audit_lines: Vec<&str> = audit_text.lines().collect();
    assert_eq!(
        audit_lines.len(),
        8,
        "1 intent + 6 sample_completed + 1 run_completed"
    );

    let vol0 = &report["volumes"][0];
    let seq_mib_s = vol0["sequential_median_mibs_per_sec"]
        .as_f64()
        .unwrap_or(0.0);
    let p50_us = vol0["small_write_median_us"].as_f64().unwrap_or(0.0);
    let p99_us = vol0["small_write_p99_us"].as_f64().unwrap_or(0.0);
    println!(
        "ROOT_LIVE_EXT4_CONFIG_BENCHMARK=PASS BINARY_MATCH=PASS exe_sha256={exe_sha256} pid={pid} start_ticks={start_ticks} volumes=2 rounds=3 total_bytes={} vol0_seq_mib_s={seq_mib_s:.2} vol0_4k_p50_us={p50_us:.1} vol0_4k_p99_us={p99_us:.1} audit_events={}",
        6 * SAMPLE_PAYLOAD_BYTES,
        audit_lines.len()
    );

    fs::remove_dir_all(&root).expect("cleanup root benchmark test dir");
}

#[test]
fn lease_custody_and_worker_sample_prechecks_refuse_invalid_states() {
    use super::super::custody::{initial_launching_lease, open_verified_mount_root};

    let root = make_temp_root("custody-refusals");
    let state_dir = root.join("state");
    let (vol_dir, mount_id, major_minor) = make_volume_dir(&root, "vol0");
    let uid = current_uid();
    let volume = BenchmarkVolume {
        filesystem_uuid: "uuid-precheck-0".into(),
        device_identity: "wwn:precheck-0".into(),
        display_name: "vol0".into(),
        mountpoint: vol_dir.to_string_lossy().into_owned(),
        mount_id,
        major_minor: major_minor.clone(),
        filesystem: "ext4".into(),
        free_bytes: BENCHMARK_REQUIRED_FREE_BYTES,
        required_free_bytes: BENCHMARK_REQUIRED_FREE_BYTES,
        capacity_observed_unix_ms: 1_000,
    };

    assert!(
        open_verified_mount_root(
            &volume.mountpoint,
            mount_id.wrapping_add(1),
            &major_minor,
            uid,
        )
        .is_err()
    );

    let mut oversized_ceiling = initial_launching_lease(
        "tx-1",
        "hash-1",
        &volume,
        ".ramshared/benchmark-tmp/sample-0.bin",
        ProcessIdentity {
            boot_id: "boot-1",
            pid: 10,
            start_time: 20,
        },
        1_000,
    );
    oversized_ceiling.byte_ceiling = SAMPLE_PAYLOAD_BYTES + 4096;
    assert!(acquire_launching_lease(&state_dir, uid, &oversized_ceiling).is_err());

    assert!(clear_verified_lease(&state_dir, uid, "tx-1").is_ok());
    let mut lease = oversized_ceiling;
    lease.byte_ceiling = SAMPLE_PAYLOAD_BYTES;
    acquire_launching_lease(&state_dir, uid, &lease).unwrap();
    assert!(clear_verified_lease(&state_dir, uid, "wrong-tx").is_err());
    lease.status = WorkerLeaseStatus::WorkerStuck;
    replace_lease(&state_dir, uid, &lease).unwrap();
    assert!(clear_verified_lease(&state_dir, uid, "tx-1").is_err());

    let lease_file = state_dir.join("benchmark-worker-lease.json");
    fs::write(&lease_file, b"{malformed").unwrap();
    fs::set_permissions(&lease_file, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(read_existing_lease(&state_dir, uid).is_err());
    fs::write(&lease_file, vec![b'x'; 70 * 1024]).unwrap();
    fs::set_permissions(&lease_file, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(read_existing_lease(&state_dir, uid).is_err());

    let mut req = WorkerRequest {
        tx_id: "tx-1".into(),
        volume: volume.clone(),
        round: 1,
        sample_seed: 7,
        temp_leaf_name: "bench-sample-precheck.bin".into(),
        byte_ceiling: u64::MAX,
        owner_uid: uid,
    };
    assert!(matches!(
        execute_worker_sample(&req),
        WorkerReply::Failed {
            cleanup_verified: true,
            ..
        }
    ));

    let bench_tmp = vol_dir
        .join("var")
        .join("lib")
        .join("ramshared")
        .join("resource-config")
        .join("benchmark");
    fs::create_dir_all(&bench_tmp).unwrap();
    for ancestor in [
        vol_dir.join("var"),
        vol_dir.join("var/lib"),
        vol_dir.join("var/lib/ramshared"),
        vol_dir.join("var/lib/ramshared/resource-config"),
        bench_tmp.clone(),
    ] {
        fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let leaf_path = bench_tmp.join("bench-sample-precheck.bin");
    fs::write(&leaf_path, b"collision").unwrap();
    fs::set_permissions(&leaf_path, fs::Permissions::from_mode(0o600)).unwrap();
    req.byte_ceiling = SAMPLE_PAYLOAD_BYTES;
    assert!(matches!(
        execute_worker_sample(&req),
        WorkerReply::Failed {
            cleanup_verified: true,
            ..
        }
    ));
    assert!(
        verify_or_cleanup_volume_temp(&volume, "bench-sample-precheck.bin", Some((0, 0)), uid)
            .is_err()
    );
    verify_or_cleanup_volume_temp(&volume, "bench-sample-precheck.bin", None, uid).unwrap();
    assert!(verify_or_cleanup_volume_temp(&volume, "sample-precheck.bin", None, uid).is_err());

    // Symlink leaf, unreadable dir statat, mount_id mismatch in worker, and file-as-dir refusal.
    let mut bad_mount_req = req.clone();
    bad_mount_req.volume.mount_id = mount_id.wrapping_add(1);
    bad_mount_req.byte_ceiling = SAMPLE_PAYLOAD_BYTES;
    assert!(matches!(
        execute_worker_sample(&bad_mount_req),
        WorkerReply::Failed {
            cleanup_verified: true,
            ..
        }
    ));

    let sym_leaf = bench_tmp.join("bench-symlink.bin");
    std::os::unix::fs::symlink(vol_dir.join("var"), &sym_leaf).unwrap();
    assert!(verify_or_cleanup_volume_temp(&volume, "bench-symlink.bin", None, uid).is_err());
    let _ = fs::remove_file(&sym_leaf);

    fs::set_permissions(&bench_tmp, fs::Permissions::from_mode(0o400)).unwrap();
    assert!(verify_or_cleanup_volume_temp(&volume, "bench-stat-err.bin", None, uid).is_err());
    fs::set_permissions(&bench_tmp, fs::Permissions::from_mode(0o000)).unwrap();
    assert!(verify_or_cleanup_volume_temp(&volume, "bench-stat-err.bin", None, uid).is_err());
    fs::set_permissions(&bench_tmp, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(verify_or_cleanup_volume_temp(&volume, "bench-symlink.bin", None, uid).is_err());
    fs::set_permissions(&bench_tmp, fs::Permissions::from_mode(0o700)).unwrap();

    fs::remove_dir_all(vol_dir.join("var/lib")).unwrap();
    fs::write(vol_dir.join("var/lib"), b"not-a-dir").unwrap();
    assert!(verify_or_cleanup_volume_temp(&volume, "bench-notdir.bin", None, uid).is_err());

    fs::remove_dir_all(&root).unwrap();
}
