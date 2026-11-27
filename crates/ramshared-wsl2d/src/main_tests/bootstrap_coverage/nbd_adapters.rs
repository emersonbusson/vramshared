// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main_tests/bootstrap_coverage/nbd_adapters.rs
use super::super::*;
use super::*;
use ramshared_block::OriginCacheIo;

#[test]
fn production_nbd_adapters_and_starter_defaults_cover_synthetic_paths() {
    let root = temp_test_dir("nbd-adapters");
    let swaps_path = root.join("swaps");
    let demote_status_path = root.join("demote-status.json");
    let cache_status_path = root.join("cache-status.json");
    let manifest_path = root.join("missing.manifest");

    std::fs::write(
        &swaps_path,
        "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n/dev/nbd7\t\t\t\tpartition\t\t65536\t\t256\t\t10\n",
    )
    .expect("write swaps");

    fn failing_signals() -> Result<(), Box<dyn std::error::Error>> {
        Err("synthetic signal failure".into())
    }
    fn ok_signals() -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }

    static TEST_NBD_SHUTDOWN: AtomicBool = AtomicBool::new(false);

    let adapters = ProductionNbdRuntimeAdapters {
        origin_manifest_path: manifest_path,
        swaps_path: swaps_path.clone(),
        demote_status_path: demote_status_path.clone(),
        origin_cache_status_path: cache_status_path.clone(),
        cli_cap_mib: Some("512".into()),
        shutdown_flag: &TEST_NBD_SHUTDOWN,
        shutdown_poll_interval: Duration::from_millis(1),
        install_signals: failing_signals,
    };
    let mut prod_starter = ProductionNbdRuntimeStarter;

    // origin_cache_target_bytes + origin_cache_target_bytes_from_manifest checks.
    assert!(
        adapters
            .origin_cache_target_bytes(512 * 1024 * 1024)
            .is_err()
    );
    assert!(prod_starter.origin_cache_target_bytes(4096).is_err());
    let manifest = origin_identity::SealedOriginManifest::test_fixture(
        "/dev/sdb1".into(),
        "11111111-2222-3333-4444-555555555555".into(),
        "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
        "8:17".into(),
        "8:16".into(),
        "99999999-8888-7777-6666-555555555555".into(),
        1024,
        512,
    );
    assert!(
        origin_cache_target_bytes_from_manifest(&manifest, 256 * 1024 * 1024, None)
            .expect_err("cap > logical size must refuse")
            .to_string()
            .contains("exceeds logical")
    );
    assert!(
        origin_cache_target_bytes_from_manifest(&manifest, 1024 * 1024 * 1024, Some("256"))
            .expect_err("CLI cap mismatch must refuse")
            .to_string()
            .contains("CLI cache cap")
    );
    assert_eq!(
        origin_cache_target_bytes_from_manifest(&manifest, 1024 * 1024 * 1024, Some("512"))
            .expect("matching manifest cap succeeds"),
        512 * 1024 * 1024
    );

    prod_starter
        .lock_memory(true, false)
        .expect("force mlock succeeds");
    assert!(prod_starter.lock_memory(true, true).is_err());
    let acc_sock = root.join("acc.sock");
    let (acc_listener, _acc_guard) =
        bind_owned_unix_listener(&acc_sock).expect("bind acceptor socket");
    let (acc_tx, acc_rx) = std::sync::mpsc::sync_channel(1);
    prod_starter
        .start_acceptor(acc_listener, Arc::new(Vec::new()), 0, acc_tx.clone())
        .expect("start_acceptor spawns cleanly");
    drop(acc_rx);

    assert!(adapters.start_shutdown_bridge(acc_tx.clone()).is_err());
    let adapters_ok = ProductionNbdRuntimeAdapters {
        install_signals: ok_signals,
        ..adapters
    };
    let bridge = adapters_ok
        .start_shutdown_bridge(acc_tx)
        .expect("ok signals starts bridge")
        .expect("bridge present");
    drop(bridge);

    assert_eq!(adapters_ok.nbd_used_kb("/dev/nbd7"), 256);
    assert!(!adapters_ok.nbd_swap_is_explicitly_absent("/dev/nbd7"));
    std::fs::write(
        &swaps_path,
        "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n",
    )
    .expect("write empty swaps");
    assert!(adapters_ok.nbd_swap_is_explicitly_absent("/dev/nbd7"));
    std::fs::write(&swaps_path, "malformed\n").expect("write malformed swaps");
    assert_eq!(adapters_ok.nbd_used_kb("/dev/nbd7"), u64::MAX);
    assert!(!adapters_ok.nbd_swap_is_explicitly_absent("/dev/nbd7"));
    let _ = prod_starter.nbd_used_kb("/dev/ramshared-nonexistent-nbd");
    let _ = prod_starter.nbd_swap_is_explicitly_absent("/dev/ramshared-nonexistent-nbd");

    adapters_ok.publish_demote(&ramshared_wsl2d::DemoteStatusFile {
        total: 1,
        last_reason: Some("Test".into()),
        in_progress: false,
        pending_age_secs: None,
    });
    adapters_ok.publish_origin_cache(&OriginCacheStatus {
        configuration_generation: None,
        schema_version: 1,
        daemon_instance_id: "1-1".into(),
        written_at_unix_ms: Some(1_700_000_000_000),
        ok: true,
        origin_state: "Ready",
        cache_state: "Disabled",
        cache_unavailable_reason: None,
        logical_capacity_kib: 1024,
        vram_cached_kib: Some(0),
        gpu_headroom_kib: None,
        gpu_budget: None,
        cache_members: None,
        gpu_utilization: None,
        gpu_cache: None,
        host_memory: None,
        host_gpu_activity: None,
        host_storage: None,
        control_plane_state: None,
        control_plane_lease_deadline_ms: None,
        ssd_origin_written_kib: 0,
        cache_hits: 0,
        cache_fallback_reads: 0,
        cache_invalidations: 0,
        cache_releases: 0,
        origin_cache_io: OriginCacheIo::default(),
        cache_target_kib: 0,
        serve_threads: 1,
        serve_requests_total: 0,
    });
    let _ = prod_starter.elapsed_us(Instant::now());
    let rx = prod_starter.spawn_swapoff(root.join("nonexistent-nbd").to_string_lossy().as_ref());
    assert!(!matches!(
        rx.recv_timeout(Duration::from_secs(5))
            .expect("swapoff finishes"),
        SwapoffOutcome::Drained
    ));
    let _ = prod_starter.spawn_recovery_activation("/dev/ramshared-nonexistent-nbd", 100);
    let _ = prod_starter.try_claim_shutdown("/dev/nbd7");

    // Minimal NbdRuntimeStarter covers default trait methods.
    struct DefaultStarter;
    impl NbdRuntimeStarter for DefaultStarter {
        fn try_claim_shutdown(&mut self, _: &str) -> Option<ShutdownPermit> {
            None
        }
        fn lock_memory(&mut self, _: bool, _: bool) -> Result<(), Box<dyn std::error::Error>> {
            Ok(())
        }
        fn start_acceptor(
            &mut self,
            _: std::os::unix::net::UnixListener,
            _: Arc<Vec<ramshared_block::handshake::Export>>,
            _: u16,
            _: std::sync::mpsc::SyncSender<WMsg>,
        ) -> Result<(), Box<dyn std::error::Error>> {
            Ok(())
        }
        fn start_shutdown_bridge(
            &mut self,
            _: std::sync::mpsc::SyncSender<WMsg>,
        ) -> Result<Option<NbdShutdownBridge>, Box<dyn std::error::Error>> {
            Ok(None)
        }
        fn nbd_used_kb(&mut self, _: &str) -> u64 {
            0
        }
        fn publish_demote(&mut self, _: &ramshared_wsl2d::DemoteStatusFile) {}
        fn elapsed_us(&mut self, _: Instant) -> u64 {
            0
        }
        fn spawn_swapoff(&mut self, _: &str) -> std::sync::mpsc::Receiver<SwapoffOutcome> {
            let (_tx, rx) = std::sync::mpsc::channel();
            rx
        }
        fn spawn_recovery_activation(
            &mut self,
            _: &str,
            _: i16,
        ) -> Result<std::sync::mpsc::Receiver<bool>, Box<dyn std::error::Error>> {
            let (_tx, rx) = std::sync::mpsc::channel();
            Ok(rx)
        }
    }

    let mut starter = DefaultStarter;
    assert!(
        starter
            .startup_budget(false)
            .expect("default budget")
            .is_none()
    );
    assert_eq!(starter.global_free_bytes(Duration::ZERO), None);
    assert_eq!(
        starter
            .origin_cache_target_bytes(8192)
            .expect("default target"),
        8192
    );
    assert_eq!(starter.teardown_retry_delay(), Duration::from_secs(5));
    assert!(starter.origin_worker_plan(8192, 0).is_ok());

    // Socket-parent validation, canonical identity, publish error, and disarmed cleanup paths.
    assert!(open_socket_parent(Path::new("")).is_err());
    assert!(open_socket_parent(&root.join("../escape/sock")).is_err());
    let file_not_dir = root.join("regular-file");
    std::fs::write(&file_not_dir, b"x").expect("write regular file");
    assert!(open_socket_parent(&file_not_dir.join("sock")).is_err());
    assert!(prepare_unix_socket_path(&file_not_dir).is_err());

    assert_eq!(canonical_nbd_identity("/tmp/nbd0"), None);
    assert_eq!(canonical_nbd_identity("sub/nbd0"), None);
    assert_eq!(canonical_nbd_identity("nbdx"), None);
    assert_eq!(
        canonical_ublk_identity("/dev/ublkb0"),
        Some("/dev/ublkb0".to_string())
    );
    assert_eq!(
        canonical_ublk_identity("/ublkb1"),
        Some("/dev/ublkb1".to_string())
    );
    assert_eq!(
        canonical_ublk_identity("ublkb2"),
        Some("/dev/ublkb2".to_string())
    );
    assert_eq!(canonical_ublk_identity("/tmp/ublkb0"), None);
    assert_eq!(canonical_ublk_identity("sub/ublkb0"), None);
    assert_eq!(canonical_ublk_identity("ublkb"), None);
    assert_eq!(canonical_ublk_identity("ublkbx"), None);
    assert!(
        nbd_used_kb_from_text(
            "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n/dev/nbd7\t\t\t\tpartition\t\t65536\t\t256\t\t10\n/dev/nbd7\t\t\t\tpartition\t\t65536\t\t0\t\t10\n",
            "/dev/nbd7"
        )
        .is_err()
    );

    let bad_adapters = ProductionNbdRuntimeAdapters {
        demote_status_path: file_not_dir.join("cannot/demote.json"),
        origin_cache_status_path: file_not_dir.join("cannot/cache.json"),
        ..adapters_ok
    };
    bad_adapters.publish_demote(&ramshared_wsl2d::DemoteStatusFile {
        total: 0,
        last_reason: None,
        in_progress: false,
        pending_age_secs: None,
    });
    bad_adapters.publish_origin_cache(&OriginCacheStatus {
        configuration_generation: None,
        schema_version: 1,
        daemon_instance_id: "1-1".into(),
        written_at_unix_ms: Some(1_700_000_000_000),
        ok: true,
        origin_state: "Ready",
        cache_state: "Disabled",
        cache_unavailable_reason: None,
        logical_capacity_kib: 1024,
        vram_cached_kib: Some(0),
        gpu_headroom_kib: None,
        gpu_budget: None,
        cache_members: None,
        gpu_utilization: None,
        gpu_cache: None,
        host_memory: None,
        host_gpu_activity: None,
        host_storage: None,
        control_plane_state: None,
        control_plane_lease_deadline_ms: None,
        ssd_origin_written_kib: 0,
        cache_hits: 0,
        cache_fallback_reads: 0,
        cache_invalidations: 0,
        cache_releases: 0,
        origin_cache_io: OriginCacheIo::default(),
        cache_target_kib: 0,
        serve_threads: 1,
        serve_requests_total: 0,
    });

    let disarm_sock = root.join("disarm.sock");
    let (disarm_listener, mut disarm_guard) =
        bind_owned_unix_listener(&disarm_sock).expect("bind disarm sock");
    drop(disarm_listener);
    disarm_guard.armed = false;
    disarm_guard.remove_if_owned();
    assert!(disarm_sock.exists());

    let _ = std::fs::remove_dir_all(&root);
}
