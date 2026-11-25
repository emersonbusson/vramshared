// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main_tests/broker_circuit_and_wddm.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;

#[test]
fn broker_worker_circuit_breaker_trips_after_three_consecutive_io_errors() {
    let (jobs_tx, jobs_rx) = std::sync::mpsc::sync_channel(16);
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    let shutdown = std::sync::Arc::new(AtomicBool::new(false));
    let stop = BrokerShutdown::new(std::sync::Arc::clone(&shutdown), jobs_tx.clone());
    let (demote_tx, demote_rx) = std::sync::mpsc::channel();
    let worker_rt = BrokerWorkerRuntime {
        geom: vec![(0, 4096)],
        jobs_rx,
        demote_tx,
        shutdown,
        shutdown_wake_pending: std::sync::Arc::clone(&stop.wake_pending),
        shutdown_wake_tx: stop.wake_tx.clone(),
        slice_io: std::sync::Arc::new(vec![SliceIoCounters::default()]),
        vram: std::sync::Arc::new(VramGauge::default()),
    };

    let make_job = |handle: u64| {
        WMsg::Job(ramshared_wsl2d::conn::Job {
            export: 0,
            req: ramshared_block::Request {
                flags: 0,
                cmd: Command::Write,
                handle,
                offset: 0,
                len: 512,
            },
            payload: vec![0x5A; 512],
            credit: fixture_credit(),
            reply: reply_tx.clone(),
        })
    };

    let (backend, _fail_flag) = TestFailingBackend::new(4096, true);

    std::thread::scope(|scope| {
        let worker = scope.spawn(move || {
            serve_broker_jobs_with_poll_and_reply_hook(
                backend,
                worker_rt,
                |_| None,
                Duration::from_millis(5),
                || {},
            )
        });
        let _shutdown_guard = BrokerWorkerShutdownGuard(&stop);

        // Job 1 fails: no demote
        jobs_tx.send(make_job(1)).unwrap();
        let r1 = reply_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(
            u32::from_be_bytes([r1.reply[4], r1.reply[5], r1.reply[6], r1.reply[7]]),
            ramshared_block::protocol::NBD_EIO
        );
        assert_eq!(
            demote_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        );

        // Job 2 fails: no demote
        jobs_tx.send(make_job(2)).unwrap();
        let r2 = reply_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(
            u32::from_be_bytes([r2.reply[4], r2.reply[5], r2.reply[6], r2.reply[7]]),
            ramshared_block::protocol::NBD_EIO
        );
        assert_eq!(
            demote_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        );

        // Job 3 fails: trips circuit breaker!
        jobs_tx.send(make_job(3)).unwrap();
        let r3 = reply_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(
            u32::from_be_bytes([r3.reply[4], r3.reply[5], r3.reply[6], r3.reply[7]]),
            ramshared_block::protocol::NBD_EIO
        );
        let reason = demote_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("circuit breaker fired demote");
        assert_eq!(reason, DemoteReason::IoErrorBurst);

        // Shutdown worker cleanly
        stop.request();
        worker.join().expect("worker joins cleanly");
    });
}

#[test]
fn broker_worker_circuit_breaker_resets_on_success() {
    let (jobs_tx, jobs_rx) = std::sync::mpsc::sync_channel(16);
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    let shutdown = std::sync::Arc::new(AtomicBool::new(false));
    let stop = BrokerShutdown::new(std::sync::Arc::clone(&shutdown), jobs_tx.clone());
    let (demote_tx, demote_rx) = std::sync::mpsc::channel();
    let worker_rt = BrokerWorkerRuntime {
        geom: vec![(0, 4096)],
        jobs_rx,
        demote_tx,
        shutdown,
        shutdown_wake_pending: std::sync::Arc::clone(&stop.wake_pending),
        shutdown_wake_tx: stop.wake_tx.clone(),
        slice_io: std::sync::Arc::new(vec![SliceIoCounters::default()]),
        vram: std::sync::Arc::new(VramGauge::default()),
    };

    let (backend, fail_flag) = TestFailingBackend::new(4096, true);

    let make_job = |handle: u64| {
        WMsg::Job(ramshared_wsl2d::conn::Job {
            export: 0,
            req: ramshared_block::Request {
                flags: 0,
                cmd: Command::Write,
                handle,
                offset: 0,
                len: 512,
            },
            payload: vec![0x5A; 512],
            credit: fixture_credit(),
            reply: reply_tx.clone(),
        })
    };

    std::thread::scope(|scope| {
        let worker = scope.spawn(move || {
            serve_broker_jobs_with_poll_and_reply_hook(
                backend,
                worker_rt,
                |_| None,
                Duration::from_millis(5),
                || {},
            )
        });
        let _shutdown_guard = BrokerWorkerShutdownGuard(&stop);

        // 2 failures
        jobs_tx.send(make_job(1)).unwrap();
        let _ = reply_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        jobs_tx.send(make_job(2)).unwrap();
        let _ = reply_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(
            demote_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        );

        // Success resets streak
        fail_flag.store(false, Ordering::SeqCst);
        jobs_tx.send(make_job(3)).unwrap();
        let r3 = reply_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(
            u32::from_be_bytes([r3.reply[4], r3.reply[5], r3.reply[6], r3.reply[7]]),
            ramshared_block::protocol::NBD_OK
        );
        assert_eq!(
            demote_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        );

        // 2 more failures: total is 4 failures, but streak is only 2 -> no demote
        fail_flag.store(true, Ordering::SeqCst);
        jobs_tx.send(make_job(4)).unwrap();
        let _ = reply_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        jobs_tx.send(make_job(5)).unwrap();
        let _ = reply_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(
            demote_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        );

        // 3rd consecutive failure in this streak -> trips
        jobs_tx.send(make_job(6)).unwrap();
        let _ = reply_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let reason = demote_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("circuit breaker fired");
        assert_eq!(reason, DemoteReason::IoErrorBurst);

        stop.request();
        worker.join().expect("worker joins cleanly");
    });
}

#[test]
fn broker_worker_idle_heartbeat_detects_gpu_loss() {
    let (jobs_tx, jobs_rx) = std::sync::mpsc::sync_channel(16);
    let shutdown = std::sync::Arc::new(AtomicBool::new(false));
    let stop = BrokerShutdown::new(std::sync::Arc::clone(&shutdown), jobs_tx.clone());
    let (demote_tx, demote_rx) = std::sync::mpsc::channel();
    let worker_rt = BrokerWorkerRuntime {
        geom: vec![(0, 4096)],
        jobs_rx,
        demote_tx,
        shutdown,
        shutdown_wake_pending: std::sync::Arc::clone(&stop.wake_pending),
        shutdown_wake_tx: stop.wake_tx.clone(),
        slice_io: std::sync::Arc::new(vec![SliceIoCounters::default()]),
        vram: std::sync::Arc::new(VramGauge::default()),
    };

    std::thread::scope(|scope| {
        let worker = scope.spawn(move || {
            serve_broker_jobs_with_poll_heartbeat_and_reply_hook(
                RamBackend::new(4096),
                worker_rt,
                |_| None,
                || Some(DemoteReason::GpuLost),
                Duration::from_millis(5),
                Duration::from_millis(5),
                || {},
            )
        });

        // Idle worker should trigger heartbeat on first tick and send GpuLost
        let reason = demote_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("heartbeat sent GpuLost");
        assert_eq!(reason, DemoteReason::GpuLost);

        stop.request();
        worker.join().expect("worker joins cleanly");
    });
}

#[test]
fn broker_worker_idle_heartbeat_healthy_does_not_demote() {
    let (jobs_tx, jobs_rx) = std::sync::mpsc::sync_channel(16);
    let shutdown = std::sync::Arc::new(AtomicBool::new(false));
    let stop = BrokerShutdown::new(std::sync::Arc::clone(&shutdown), jobs_tx.clone());
    let (demote_tx, demote_rx) = std::sync::mpsc::channel();
    let worker_rt = BrokerWorkerRuntime {
        geom: vec![(0, 4096)],
        jobs_rx,
        demote_tx,
        shutdown,
        shutdown_wake_pending: std::sync::Arc::clone(&stop.wake_pending),
        shutdown_wake_tx: stop.wake_tx.clone(),
        slice_io: std::sync::Arc::new(vec![SliceIoCounters::default()]),
        vram: std::sync::Arc::new(VramGauge::default()),
    };

    std::thread::scope(|scope| {
        let worker = scope.spawn(move || {
            serve_broker_jobs_with_poll_heartbeat_and_reply_hook(
                RamBackend::new(4096),
                worker_rt,
                |_| None,
                || None,
                Duration::from_millis(5),
                Duration::from_millis(5),
                || {},
            )
        });

        // Sleep a short time and ensure no demote was triggered
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(
            demote_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        );

        stop.request();
        worker.join().expect("worker joins cleanly");
    });
}

#[test]
fn daemon_probe_and_origin_helpers_refuse_without_live_device_access() {
    assert_eq!(linux_device_number(0x0801), "8:1");
    assert!(unix_time_ms().is_some());
    assert!(read_sealed_origin_manifest("/tmp/not-the-origin-manifest").is_err());
    assert!(collect_device_and_parent(Path::new("/"), &mut Vec::new()).is_err());
    let non_utf8 = std::path::PathBuf::from(OsString::from_vec(vec![0xff]));
    assert!(block_identity_value(&non_utf8, "UUID").is_err());

    let mut broker_without_arbiter = AppArgs::parse_from(&daemon_argv(&[
        "ramsharedd",
        "--backend",
        "ram",
        "--slices",
        "1",
        "--slice-mb",
        "1",
        "--arbiter-listen",
        "127.0.0.1:7777",
    ]))
    .expect("valid broker fixture");
    broker_without_arbiter.arbiter_addr = None;
    assert!(select_daemon_action(broker_without_arbiter).is_err());

    let mut no_slices_with_listener =
        AppArgs::parse_from(&daemon_argv(&["ramsharedd", "--transport", "ublk"]))
            .expect("valid ublk fixture");
    no_slices_with_listener.listen_nbd_addr = Some("127.0.0.1:10809".parse().unwrap());
    assert!(select_daemon_action(no_slices_with_listener).is_err());
    let ublk = AppArgs::parse_from(&daemon_argv(&["ramsharedd", "--transport", "ublk"]))
        .expect("valid ublk fixture");
    assert!(matches!(
        select_daemon_action_for(ublk, Some(false)),
        Ok(DaemonAction::Ublk(_))
    ));

    let root =
        std::env::temp_dir().join(format!("ramshared-reclaim-fallback-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create reclaim fallback fixture");
    let cache_target = root.join("cache-target.json");
    let reclaim = root.join("reclaim.json");
    std::fs::write(
        &reclaim,
        serde_json::json!({
            "schema_version": 1,
            "reason": "control_pressure",
            "daemon_instance_id": "fixture-daemon",
            "issued_at_unix_ms": 1_000,
        })
        .to_string(),
    )
    .expect("write reclaim fallback fixture");
    assert!(critical_cache_reclaim_requested_at(
        &cache_target,
        &reclaim,
        "fixture-daemon",
        1_001,
    ));
    std::fs::remove_dir_all(root).expect("remove exact reclaim fallback fixture");
}

#[test]
fn sparse_nbd_budget_cannot_use_an_unbound_sole_dxg_adapter() {
    struct NoRawGpu(std::cell::Cell<usize>);
    impl VramProvider for &NoRawGpu {
        type Mem<'a>
            = TestMemory
        where
            Self: 'a;
        fn alloc(&self, _: usize) -> Result<Self::Mem<'_>, ramshared_vram::VramError> {
            panic!("originless product NBD attempted raw GPU allocation")
        }
        fn mem_info(&self) -> Result<(u64, u64), ramshared_vram::VramError> {
            self.0.set(self.0.get() + 1);
            Err(ramshared_vram::VramError::Provider(
                "raw GPU probe must not run".into(),
            ))
        }
    }
    let provider = NoRawGpu(std::cell::Cell::new(0));
    let socket = std::env::temp_dir().join(format!(
        "ramshared-originless-product-{}.sock",
        std::process::id()
    ));
    assert!(!socket.exists());
    let mut starter = ProductionNbdRuntimeStarter;
    let error = run_origin_nbd_with_startup(
        &provider,
        None,
        4096,
        socket.to_str().unwrap().into(),
        false,
        "/dev/ramshared-originless-refusal".into(),
        1,
        &mut starter,
    )
    .unwrap_err();
    assert_eq!(
        provider.0.get(),
        0,
        "product NBD reached raw GPU probing without an authoritative origin"
    );
    assert!(
        run_nbd(
            None,
            4096,
            socket.to_str().unwrap().into(),
            false,
            "/dev/ramshared-originless-refusal".into(),
            1
        )
        .unwrap_err()
        .to_string()
        .contains("authoritative origin")
    );
    assert!(error.to_string().contains("authoritative origin"));
    assert!(
        !socket.exists(),
        "origin refusal must precede socket publication"
    );
}

#[test]
fn product_nbd_execution_does_not_fall_back_without_origin() {
    let args = AppArgs::parse_from(&daemon_argv(&["ramsharedd", "--backend", "ram"])).unwrap();
    let error = ProductionDaemonRunner
        .execute(DaemonAction::Nbd(args))
        .unwrap_err();
    assert!(
        error.to_string().contains("authoritative origin"),
        "execution used a backend-specific fallback instead of requiring the origin: {error}"
    );
}

#[test]
fn nbd_budget_gate_requires_a_fresh_successful_chunk_admission() {
    struct SampleProvider(RefCell<Option<Result<NbdBudgetSnapshot, String>>>);

    impl NbdBudgetProvider for SampleProvider {
        fn snapshot(&self) -> Result<NbdBudgetSnapshot, String> {
            self.0
                .borrow_mut()
                .take()
                .expect("one injected WDDM sample")
        }
    }

    let accepted = SampleProvider(RefCell::new(Some(Ok(NbdBudgetSnapshot {
        budget: 8 * GIB,
        current_usage: 0,
        sampled_at: Instant::now(),
    }))));
    let gate = NbdBudgetGate {
        provider: &accepted,
        config: AutotierConfig::default(),
    };
    assert!(gate.allow_commit(0, 1024 * 1024).is_ok());

    let stale = SampleProvider(RefCell::new(Some(Ok(NbdBudgetSnapshot {
        budget: 8 * GIB,
        current_usage: 0,
        sampled_at: Instant::now() - Duration::from_secs(60),
    }))));
    let gate = NbdBudgetGate {
        provider: &stale,
        config: AutotierConfig::default(),
    };
    assert!(gate.allow_commit(0, 1024 * 1024).is_err());

    let exhausted = SampleProvider(RefCell::new(Some(Ok(NbdBudgetSnapshot {
        budget: 1,
        current_usage: 1,
        sampled_at: Instant::now(),
    }))));
    let gate = NbdBudgetGate {
        provider: &exhausted,
        config: AutotierConfig::default(),
    };
    assert!(gate.allow_commit(0, 1024 * 1024).is_err());

    let unavailable = SampleProvider(RefCell::new(Some(Err("injected DXG error".into()))));
    let gate = NbdBudgetGate {
        provider: &unavailable,
        config: AutotierConfig::default(),
    };
    assert!(gate.allow_commit(0, 1024 * 1024).is_err());
}

fn native_wddm_unavailable(
    _: ramshared_dxg::BudgetRequest,
) -> Result<ramshared_dxg::DxgBudgetProvider, ramshared_dxg::DxgError> {
    Err(ramshared_dxg::DxgError::Unavailable(
        "injected native Linux without DXG".into(),
    ))
}

#[test]
fn gpu_candidate_requires_fresh_adapter_bound_budget() {
    let config = GpuWorkerConfig::default();
    let valid = BudgetSnapshotProvider {
        snapshot: adapter_budget_snapshot(Some(test_gpu_identity()), Instant::now()),
    };
    let candidate = gpu_candidate(
        &valid,
        GpuBackendKind::Vulkan,
        2,
        config,
        native_wddm_unavailable,
    )
    .unwrap()
    .expect("fresh adapter-bound budget must admit the bounded candidate");
    assert_eq!(candidate.backend, GpuBackendKind::Vulkan);
    assert_eq!(candidate.ordinal, 2);
    assert_eq!(candidate.identity, test_gpu_identity());
    assert_eq!(candidate.safe_target_bytes, config.target_bytes);

    let missing_identity = BudgetSnapshotProvider {
        snapshot: adapter_budget_snapshot(None, Instant::now()),
    };
    assert!(
        gpu_candidate(
            &missing_identity,
            GpuBackendKind::Cuda,
            0,
            config,
            native_wddm_unavailable
        )
        .is_err()
    );

    let stale = BudgetSnapshotProvider {
        snapshot: adapter_budget_snapshot(
            Some(test_gpu_identity()),
            Instant::now() - Duration::from_secs(30),
        ),
    };
    assert!(
        gpu_candidate(
            &stale,
            GpuBackendKind::Cuda,
            0,
            config,
            native_wddm_unavailable
        )
        .is_err()
    );
}

#[test]
fn selected_gpu_adapter_is_revalidated_before_worker_use() {
    let config = GpuWorkerConfig::default();
    let now = Instant::now();
    let expected = test_gpu_identity();
    assert!(
        revalidate_selected_adapter(
            adapter_budget_snapshot(Some(expected.clone()), now),
            &expected,
            config,
        )
        .is_ok()
    );

    let mut other = expected.clone();
    other.key = "uuid:replacement".into();
    assert!(
        revalidate_selected_adapter(adapter_budget_snapshot(Some(other), now), &expected, config,)
            .unwrap_err()
            .contains("identity changed")
    );

    assert!(
        revalidate_selected_adapter(
            adapter_budget_snapshot(Some(expected.clone()), now - Duration::from_secs(30),),
            &expected,
            config,
        )
        .unwrap_err()
        .contains("fresh safe cache budget")
    );
}

#[test]
fn selected_provider_without_codec_starts_raw_only() {
    use ramshared_block::GpuCacheWorker;
    use ramshared_vram::{CodecCapability, CodecState};

    // Compression is explicitly enabled, but the selected provider has no
    // `cache_codec()` capability. DT-2 / RF-7: the worker must be raw-only
    // and never attempt compression or fault the codec.
    let provider = BudgetSnapshotProvider {
        snapshot: adapter_budget_snapshot(Some(test_gpu_identity()), Instant::now()),
    };
    let config = GpuWorkerConfig {
        target_bytes: 8 * 1024 * 1024,
        chunk_bytes: 64,
        reserve_floor_bytes: 0,
        compression_enabled: true,
    };
    let mut worker = GpuCacheWorker::new(&provider, config);

    worker.handle_update(0, &[0x5a; 64]);

    assert_eq!(
        worker.compressed_entries_count(),
        0,
        "a provider without a codec must never publish compressed extents"
    );
    assert_eq!(
        worker.codec_faults(),
        0,
        "raw-only is the absence of a codec, not a codec failure"
    );
    let telemetry = worker.cache_telemetry(1_000);
    assert_eq!(telemetry.codec.capability, CodecCapability::RawOnly);
    assert_eq!(telemetry.codec.state, CodecState::RawOnly);
    assert_eq!(telemetry.compressed_payload_bytes, 0);
    assert_eq!(telemetry.codec_integrity_errors, 0);

    // The raw cache still serves: raw-only is a working product, not an
    // empty one (DT-9).
    assert_eq!(worker.handle_read(0, 64).as_deref(), Some(&[0x5a; 64][..]));
    assert_eq!(worker.cached_bytes(), 64);
    assert_eq!(
        telemetry.logical_cached_bytes, 64,
        "logical cache bytes are cache occupancy, not RAM"
    );
}

#[test]
fn selected_codec_provider_revalidates_exact_adapter() {
    use ramshared_block::GpuCacheWorker;

    // A codec-capable provider is selected and starts with compression.
    let provider = CodecCapableProvider {
        snapshot: adapter_budget_snapshot(Some(test_gpu_identity()), Instant::now()),
        codec: ramshared_vram::FakeCodec::new(),
    };
    let config = GpuWorkerConfig {
        target_bytes: 8 * 1024 * 1024,
        chunk_bytes: 64,
        reserve_floor_bytes: 0,
        compression_enabled: true,
    };
    let mut worker = GpuCacheWorker::new(&provider, config);
    worker.handle_update(0, &[0x11; 64]);
    let telemetry = worker.cache_telemetry(1_000);
    assert_ne!(
        telemetry.codec.capability,
        ramshared_vram::CodecCapability::RawOnly,
        "a codec-capable provider must not silently degrade to raw-only"
    );

    // Codec capability must not bypass exact-adapter revalidation: the
    // same identity and a fresh safe budget are still required.
    let expected = test_gpu_identity();
    assert!(
        revalidate_selected_adapter(
            adapter_budget_snapshot(Some(expected.clone()), Instant::now()),
            &expected,
            config,
        )
        .is_ok()
    );

    let mut replacement = expected.clone();
    replacement.key = "uuid:codec-imposter".into();
    assert!(
        revalidate_selected_adapter(
            adapter_budget_snapshot(Some(replacement), Instant::now()),
            &expected,
            config,
        )
        .unwrap_err()
        .contains("identity changed"),
        "codec capability must never waive the identity check"
    );

    assert!(
        revalidate_selected_adapter(
            adapter_budget_snapshot(
                Some(expected.clone()),
                Instant::now() - Duration::from_secs(30),
            ),
            &expected,
            config,
        )
        .unwrap_err()
        .contains("fresh safe cache budget"),
        "codec capability must never waive the fresh-budget check"
    );

    // A provider that loses its safe cache budget is refused even though it
    // still advertises a codec.
    let starving = CodecCapableProvider {
        snapshot: GpuBudgetSnapshot {
            total_bytes: Some(8 * GIB),
            budget_bytes: 8 * GIB,
            used_bytes: 8 * GIB,
            adapter: Some(expected.clone()),
            source: ramshared_vram::GpuBudgetSource::DriverReported,
            sampled_at: Instant::now(),
        },
        codec: ramshared_vram::FakeCodec::new(),
    };
    assert!(
        revalidate_selected_adapter(starving.budget_snapshot().unwrap(), &expected, config,)
            .unwrap_err()
            .contains("fresh safe cache budget")
    );
}

#[test]
fn selected_gpu_worker_uses_allocator_budget_when_dxg_is_unavailable() {
    let provider = BudgetSnapshotProvider {
        snapshot: adapter_budget_snapshot(Some(test_gpu_identity()), Instant::now()),
    };
    let config = GpuWorkerConfig::default();
    let selected = candidate_from_budget(
        provider.snapshot.clone(),
        GpuBackendKind::Cuda,
        0,
        config,
        native_wddm_unavailable,
    )
    .unwrap()
    .unwrap();
    let (worker, peer) = std::os::unix::net::UnixStream::pair().unwrap();
    drop(peer);
    run_gpu_worker_with_selected_wddm(
        worker,
        provider,
        &selected,
        worker_config_for_candidate(config, selected.safe_target_bytes),
        native_wddm_unavailable,
    )
    .expect("closed IPC peer must make the bounded worker exit cleanly");
}

#[test]
fn daemon_plan_routes_validated_actions_without_starting_a_backend() {
    struct CapturingRunner(Option<DaemonAction>);

    impl DaemonActionRunner for CapturingRunner {
        fn execute(&mut self, action: DaemonAction) -> Result<(), Box<dyn std::error::Error>> {
            self.0 = Some(action);
            Ok(())
        }
    }

    let args = AppArgs::parse_from(&daemon_argv(&[
        "ramsharedd",
        "--backend",
        "ram",
        "--slices",
        "1",
        "--slice-mb",
        "1",
        "--arbiter-listen",
        "127.0.0.1:7777",
    ]))
    .expect("validated broker argv");
    let mut runner = CapturingRunner(None);
    run_with(args, &mut runner, select_daemon_action, || Ok(()), |_| {})
        .expect("capturing runner must receive a plan");
    assert!(matches!(
        runner.0,
        Some(DaemonAction::Broker(AppArgs {
            slices: 1,
            backend: BackendKind::Ram,
            ..
        }))
    ));

    let single_ram = AppArgs::parse_from(&daemon_argv(&["ramsharedd", "--backend", "ram"]))
        .expect("argv itself is syntactically valid");
    assert!(
        select_daemon_action(single_ram).is_err(),
        "single NBD RAM must refuse before a backend is selected"
    );
}
