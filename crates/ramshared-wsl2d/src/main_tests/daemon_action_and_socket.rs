// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main_tests/daemon_action_and_socket.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;

#[test]
fn daemon_action_selector_covers_transport_backend_origin_matrix() {
    fn args(
        transport: Transport,
        backend: BackendKind,
        origin: Option<&str>,
        slices: u16,
        arbiter: Option<&str>,
        listen: Option<&str>,
    ) -> AppArgs {
        AppArgs {
            size: DEFAULT_ORIGIN_SIZE,
            origin: origin.map(str::to_owned),
            native_origin: None,
            sock: "/tmp/ramshared-action-selector.sock".into(),
            force: false,
            nbd_dev: "/dev/ramshared-test-nbd".into(),
            transport,
            accept_wsl2_nbd: false,
            serve_threads: serve_pool::DEFAULT_SERVE_THREADS,
            zram_pump_tier: None,
            queue_depth: 1,
            backend,
            slices,
            slice_bytes: u64::from(slices) * 1024 * 1024,
            listen_nbd_addr: listen.map(|value| value.parse().unwrap()),
            arbiter_addr: arbiter.map(|value| value.parse().unwrap()),
            advertise_tcp: None,
            telemetry_jsonl: None,
        }
    }

    let origin = Some(ORIGIN_MANIFEST_PATH);
    assert!(
        select_daemon_action(args(
            Transport::Nbd,
            BackendKind::Vulkan,
            origin,
            1,
            Some("127.0.0.1:7777"),
            None,
        ))
        .is_err()
    );
    assert!(
        select_daemon_action(args(Transport::Nbd, BackendKind::Ram, None, 1, None, None,)).is_err()
    );
    assert!(
        select_daemon_action(args(
            Transport::Nbd,
            BackendKind::Vram,
            None,
            1,
            Some("127.0.0.1:7777"),
            None,
        ))
        .is_err()
    );
    assert!(matches!(
        select_daemon_action(args(
            Transport::Nbd,
            BackendKind::Ram,
            None,
            1,
            Some("127.0.0.1:7777"),
            None,
        )),
        Ok(DaemonAction::Broker(_))
    ));
    assert!(
        select_daemon_action(args(
            Transport::Nbd,
            BackendKind::Vram,
            None,
            0,
            Some("127.0.0.1:7777"),
            None,
        ))
        .is_err()
    );
    assert!(
        select_daemon_action(args(
            Transport::Nbd,
            BackendKind::Vram,
            None,
            0,
            None,
            Some("127.0.0.1:10809"),
        ))
        .is_err()
    );
    assert!(
        select_daemon_action(args(Transport::Nbd, BackendKind::Ram, None, 0, None, None,)).is_err()
    );
    assert!(
        select_daemon_action(args(
            Transport::Ublk,
            BackendKind::Vulkan,
            None,
            0,
            None,
            None,
        ))
        .is_err()
    );
    assert!(matches!(
        select_daemon_action_for(
            args(Transport::Nbd, BackendKind::Vram, origin, 0, None, None,),
            Some(false),
        ),
        Ok(DaemonAction::Nbd(_))
    ));
    assert!(
        select_daemon_action(args(Transport::Nbd, BackendKind::Vram, None, 0, None, None,))
            .is_err()
    );
    assert!(
        select_daemon_action(args(
            Transport::Ublk,
            BackendKind::Vram,
            origin,
            0,
            None,
            None,
        ))
        .is_err()
    );
    assert!(matches!(
        select_daemon_action_for(
            args(Transport::Ublk, BackendKind::Vram, None, 0, None, None,),
            Some(false)
        ),
        Ok(DaemonAction::Ublk(_))
    ));
}

#[test]
// TestName: wsl2_probe_silence_still_refuses_the_product_nbd_swap_path
fn wsl2_probe_silence_still_refuses_the_product_nbd_swap_path() {
    let args = || {
        AppArgs::parse_from(&daemon_argv(&[
            "ramsharedd",
            "--origin-manifest",
            ORIGIN_MANIFEST_PATH,
        ]))
        .expect("sealed origin manifest args")
    };

    // `None` is a probe that never ran. It must take the same hard refusal as a
    // measured WSL2 host: silence is not a measured non-WSL kernel, and the
    // refusal exists after a confirmed NBD timeout, swap read errors, and
    // SIGBUS. Collapsing `None` to `false` is the fail-open `boot.rs` named on
    // the legacy-ublk arm.
    let silent = match select_daemon_action_for(args(), None) {
        Err(error) => error,
        Ok(_) => panic!("an unprobed platform must not admit the product NBD swap path"),
    };
    let message = silent.to_string();
    assert!(message.contains("NBD"), "{message}");
    assert!(
        message.contains("keep the configured WSL fallback swap active"),
        "{message}"
    );
    assert!(message.contains("--accept-wsl2-nbd"), "{message}");
    assert!(message.contains("unknown"), "{message}");
    assert!(
        !message.contains("Pass --accept-wsl2-nbd to arm"),
        "{message}"
    );

    // Only a measured absence admits the path without the acknowledgment,
    // and the acknowledgment never admits an unprobed platform.
    assert!(matches!(
        select_daemon_action_for(args(), Some(false)),
        Ok(DaemonAction::Nbd(_))
    ));
    let mut acknowledged = args();
    acknowledged.accept_wsl2_nbd = true;
    assert!(
        select_daemon_action_for(acknowledged, None).is_err(),
        "silence is still not a measured non-WSL kernel"
    );
}

#[test]
fn daemon_version_request_requires_one_exact_public_flag() {
    for flag in ["--version", "-V", "version"] {
        assert!(daemon_version_requested(&daemon_argv(&[
            "ramsharedd",
            flag
        ])));
    }
    for args in [
        daemon_argv(&["ramsharedd"]),
        daemon_argv(&["ramsharedd", "--version", "extra"]),
        daemon_argv(&["ramsharedd", "--Version"]),
    ] {
        assert!(!daemon_version_requested(&args));
    }
}

#[test]
fn zero_window_clears_only_the_requested_range_and_propagates_backend_errors() {
    let size = (1 << 20) + 64;
    let mut backend = RamBackend::new(size);
    let initial = vec![0xA5; size];
    backend.write_at(0, &initial).unwrap();
    zero_window(&mut backend, 8, (1 << 20) + 16).unwrap();

    let mut actual = vec![0; size];
    backend.read_at(0, &mut actual).unwrap();
    assert_eq!(&actual[..8], &initial[..8]);
    assert!(actual[8..(1 << 20) + 24].iter().all(|byte| *byte == 0));
    assert_eq!(&actual[(1 << 20) + 24..], &initial[(1 << 20) + 24..]);
    assert!(zero_window(&mut backend, size as u64, 1).is_err());

    let (mut failing, _) = TestFailingBackend::new(4096, true);
    assert!(zero_window(&mut failing, 0, 512).is_err());
}

#[test]
fn daemon_gpu_legacy_broker_refuses_before_backend_initialization() {
    for backend in ["auto", "vram", "vulkan"] {
        let args = AppArgs::parse_from(&daemon_argv(&[
            "ramsharedd",
            "--backend",
            backend,
            "--slices",
            "1",
            "--slice-mb",
            "64",
            "--arbiter-listen",
            "127.0.0.1:7777",
        ]))
        .expect("legacy broker argv is syntactically valid");
        let error = match select_daemon_action(args) {
            Ok(_) => panic!("direct GPU broker must refuse before loading a GPU provider"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("--origin-manifest"),
            "backend {backend} refusal should route GPU caching through the authoritative-origin path: {error}"
        );
    }
}

#[test]
fn daemon_ublk_vulkan_refuses_before_device_mutation() {
    let args = AppArgs::parse_from(&daemon_argv(&[
        "ramsharedd",
        "--transport",
        "ublk",
        "--backend",
        "vulkan",
    ]))
    .expect("the syntax is valid so the planner owns this refusal");
    let error = match select_daemon_action(args) {
        Ok(_) => panic!("ublk Vulkan must refuse before selecting any device backend"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("ublk with --backend vulkan"),
        "refusal must identify the unsupported transport/backend pair"
    );
}

#[test]
fn daemon_production_runner_refuses_safe_terminal_actions_before_platform_load() {
    fn terminal_args(transport: Transport, backend: BackendKind) -> AppArgs {
        AppArgs {
            size: DEFAULT_SIZE,
            origin: None,
            native_origin: None,
            sock: "/tmp/ramsharedd-terminal.sock".into(),
            force: false,
            nbd_dev: "/dev/ramshared-test-nbd".into(),
            transport,
            accept_wsl2_nbd: false,
            serve_threads: serve_pool::DEFAULT_SERVE_THREADS,
            zram_pump_tier: None,
            queue_depth: 1,
            backend,
            slices: 0,
            slice_bytes: 0,
            listen_nbd_addr: None,
            arbiter_addr: None,
            advertise_tcp: None,
            telemetry_jsonl: None,
        }
    }

    let mut runner = ProductionDaemonRunner;
    let nbd = runner.execute(DaemonAction::Nbd(terminal_args(
        Transport::Nbd,
        BackendKind::Ram,
    )));
    assert!(
        nbd.expect_err("originless product NBD is terminally refused")
            .to_string()
            .contains("authoritative origin")
    );

    let ublk = runner.execute(DaemonAction::Ublk(terminal_args(
        Transport::Ublk,
        BackendKind::Vulkan,
    )));
    assert!(
        ublk.expect_err("Vulkan ublk is terminally refused")
            .to_string()
            .contains("ublk with --backend vulkan")
    );

    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time is after Unix epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "ramshared-production-runner-refusal-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir(&dir).expect("create isolated temporary refusal directory");
    let sock = dir.join("owned-by-test-regular-file");
    std::fs::write(&sock, b"do-not-replace").expect("create regular preflight file");
    let gpu_broker = runner.execute(DaemonAction::Broker(AppArgs {
        size: DEFAULT_SIZE,
        origin: None,
        native_origin: None,
        sock: sock.to_string_lossy().into_owned(),
        force: false,
        nbd_dev: "/dev/ramshared-test-nbd".into(),
        transport: Transport::Nbd,
        accept_wsl2_nbd: false,
        serve_threads: serve_pool::DEFAULT_SERVE_THREADS,
        zram_pump_tier: None,
        queue_depth: 1,
        backend: BackendKind::Vram,
        slices: 1,
        slice_bytes: BLOCK_SIZE as u64,
        listen_nbd_addr: None,
        arbiter_addr: Some("127.0.0.1:7777".parse().expect("loopback arbiter address")),
        advertise_tcp: None,
        telemetry_jsonl: None,
    }));
    assert!(
        gpu_broker
            .expect_err("direct runner must refuse GPU broker before provider load")
            .to_string()
            .contains("--origin-manifest")
    );
    let broker = runner.execute(DaemonAction::Broker(AppArgs {
        size: DEFAULT_SIZE,
        origin: None,
        native_origin: None,
        sock: sock.to_string_lossy().into_owned(),
        force: false,
        nbd_dev: "/dev/ramshared-test-nbd".into(),
        transport: Transport::Nbd,
        accept_wsl2_nbd: false,
        serve_threads: serve_pool::DEFAULT_SERVE_THREADS,
        zram_pump_tier: None,
        queue_depth: 1,
        backend: BackendKind::Ram,
        slices: 1,
        slice_bytes: BLOCK_SIZE as u64,
        listen_nbd_addr: None,
        arbiter_addr: Some("127.0.0.1:7777".parse().expect("loopback arbiter address")),
        advertise_tcp: None,
        telemetry_jsonl: None,
    }));
    assert!(
        broker
            .expect_err("broker-RAM must refuse a regular socket path")
            .to_string()
            .contains("refusing to replace existing socket path"),
        "the production runner must preserve a non-socket path before acceptor startup"
    );
    assert_eq!(
        std::fs::read(&sock).expect("regular preflight file remains readable"),
        b"do-not-replace"
    );
    std::fs::remove_dir_all(&dir).expect("remove isolated temporary refusal directory");
}

#[test]
// TestName: private_listener_accepts_only_documented_untrusted_network_ranges
fn private_listener_accepts_only_documented_untrusted_network_ranges() {
    for accepted in [
        "127.0.0.1:7777",
        "tcp://10.1.2.3:10809",
        "172.16.0.1:10809",
        "172.31.255.254:10809",
        "192.168.0.50:10809",
        "100.64.0.1:10809",
        "100.127.255.254:10809",
        "[::1]:10809",
        "[fd12:3456::1]:10809",
    ] {
        assert!(
            parse_private_listen(accepted).is_ok(),
            "documented private listener was refused: {accepted}"
        );
    }
    for refused in [
        "0.0.0.0:10809",
        "8.8.8.8:10809",
        "192.0.2.1:10809",
        "198.51.100.1:10809",
        "203.0.113.1:10809",
        "224.0.0.1:10809",
        "169.254.1.1:10809",
        "100.63.255.255:10809",
        "100.128.0.0:10809",
        "[::]:10809",
        "[2001:4860:4860::8888]:10809",
        "[2001:db8::1]:10809",
        "[ff02::1]:10809",
        "[fe80::1]:10809",
        "[::ffff:127.0.0.1]:10809",
    ] {
        assert!(
            parse_private_listen(refused).is_err(),
            "undocumented listener range was accepted: {refused}"
        );
    }
}

#[test]
fn private_listen_rejects_garbage() {
    assert!(parse_private_listen("nao-eh-addr").is_err());
    assert!(parse_private_listen("127.0.0.1").is_err()); // sem porta
}

#[test]
fn slice_flags_reject_ublk_with_slices() {
    assert!(validate_slice_flags(2, 64, true).is_err()); // DT-3
    assert!(validate_slice_flags(0, 0, true).is_ok()); // ublk single ok
}

#[test]
fn slice_flags_require_slice_mb() {
    assert!(validate_slice_flags(2, 0, false).is_err());
    assert!(validate_slice_flags(2, 64, false).is_ok());
    assert!(validate_slice_flags(0, 0, false).is_ok()); // single-mode ok
}

#[test]
fn slice_flags_cap_protects_status_line() {
    // MED-1: --slices above ramshared_broker::slices::MAX_SLICES would blow the StatusReply (MAX_LINE_BYTES 64 KiB).
    assert!(validate_slice_flags(ramshared_broker::slices::MAX_SLICES, 64, false).is_ok());
    assert!(validate_slice_flags(ramshared_broker::slices::MAX_SLICES + 1, 64, false).is_err());
}

#[test]
fn sparse_free_floor_requests_swapoff_but_latency_does_not() {
    assert!(sparse_residency_requests_swapoff(DemoteReason::FreeFloor));
    assert!(sparse_residency_requests_swapoff(DemoteReason::Corruption));
    assert!(!sparse_residency_requests_swapoff(DemoteReason::Latency));
}

#[test]
fn sparse_residency_uses_configured_reserve_floor() {
    let cfg = sparse_residency_config(512 * 1024 * 1024);
    assert_eq!(cfg.free_floor_bytes, 512 * 1024 * 1024);
    assert_eq!(cfg.latency_mult, ResidencyConfig::default().latency_mult);
    assert_eq!(cfg.consecutive, ResidencyConfig::default().consecutive);
}

#[test]
fn probe_residency_reason_has_priority_over_latency() {
    assert_eq!(
        choose_residency_reason(Some(DemoteReason::Latency), Some(DemoteReason::FreeFloor)),
        Some(DemoteReason::FreeFloor)
    );
    assert_eq!(
        choose_residency_reason(Some(DemoteReason::Latency), Some(DemoteReason::Corruption)),
        Some(DemoteReason::Corruption)
    );
}

#[test]
fn probe_sample_logs_low_free_or_degraded_state() {
    assert!(should_log_probe_sample(
        Some(true),
        Some(128),
        512,
        1,
        false
    ));
    assert!(should_log_probe_sample(None, Some(1024), 512, 0, false));
    assert!(should_log_probe_sample(Some(true), None, 512, 0, false));
    assert!(!should_log_probe_sample(
        Some(true),
        Some(2048),
        512,
        0,
        false
    ));
    assert!(should_log_probe_sample(
        Some(true),
        Some(2048),
        512,
        0,
        true
    ));
}

#[test]
fn global_free_floor_demote_requires_committed_tier_and_streak() {
    let mut streak = 0;
    assert!(!observe_global_free_floor(
        Some(128),
        512,
        0,
        &mut streak,
        3
    ));
    assert_eq!(streak, 0);

    assert!(!observe_global_free_floor(
        Some(128),
        512,
        1024,
        &mut streak,
        3
    ));
    assert_eq!(streak, 1);
    assert!(!observe_global_free_floor(
        Some(128),
        512,
        1024,
        &mut streak,
        3
    ));
    assert!(observe_global_free_floor(
        Some(128),
        512,
        1024,
        &mut streak,
        3
    ));
}

#[test]
fn global_free_floor_streak_resets_on_a_measured_headroom_sample() {
    let mut streak = 2;
    assert!(!observe_global_free_floor(
        Some(2048),
        512,
        1024,
        &mut streak,
        3
    ));
    assert_eq!(streak, 0);
}

#[test]
fn global_free_floor_streak_survives_an_unmeasured_sample() {
    // An unreadable probe is not a reading of headroom. The fold this
    // replaces reset the streak to 0 on `None`, so a host whose global probe
    // interleaved failures could never reach `required` measured samples
    // below the floor and never demoted on this criterion.
    let mut streak = 2;
    assert!(!observe_global_free_floor(None, 512, 1024, &mut streak, 3));
    assert_eq!(streak, 2, "absence must not erase measured pressure");
    // It is not a reading of pressure either: it does not advance the streak
    // toward the demote.
    assert!(!observe_global_free_floor(None, 512, 1024, &mut streak, 3));
    assert_eq!(streak, 2, "absence must not stand in for a sample");
    // The demote still needs `required` actual measurements. Absence delayed
    // it by contributing no sample; it did not discard the ones that were.
    assert!(observe_global_free_floor(
        Some(128),
        512,
        1024,
        &mut streak,
        3
    ));
    assert_eq!(streak, 3);
}

#[test]
fn an_unmeasured_global_free_does_not_license_recovery() {
    // `global_healthy` gates spawn_recovery_activation. Its old spelling,
    // `is_none_or`, treated an unreadable global probe as healthy GPU
    // headroom and let the tier be put back into service on a GPU whose free
    // memory was never read. A licence needs its measurement.
    assert!(!global_free_at_or_above_floor(None, 512));
    assert!(!global_free_at_or_above_floor(Some(511), 512));
    assert!(global_free_at_or_above_floor(Some(512), 512));
    assert!(global_free_at_or_above_floor(Some(2048), 512));
}

#[test]
fn nbd_used_parser_requires_exact_device_identity() {
    let swaps = "\
Filename\t\t\tType\t\tSize\t\tUsed\t\tPriority
/dev/nbd01                              partition\t1024\t\t0\t\t-2
/tmp/nbd0-backup                       file\t\t1024\t\t0\t\t-3
/dev/nbd0                              partition\t1024\t\t37\t\t-4
";
    assert_eq!(nbd_used_kb_from_text(swaps, "/dev/nbd0"), Ok(37));
    assert_eq!(nbd_used_kb_from_text(swaps, "/dev/nbd01"), Ok(0));
    assert_eq!(nbd_used_kb_from_text(swaps, "/dev/nbd9"), Ok(0));
}

#[test]
fn nbd_used_parser_is_fail_safe_for_duplicate_and_deleted_rows() {
    let swaps = "\
Filename\t\t\tType\t\tSize\t\tUsed\t\tPriority
/dev/nbd0                              partition\t1024\t\t0\t\t-2
/dev/nbd0\\040(deleted)                 partition\t1024\t\t55\t\t-3
";
    assert!(nbd_used_kb_from_text(swaps, "nbd0").is_err());
}

#[test]
fn nbd_swap_absence_is_not_a_zero_used_active_entry() {
    let header = "Filename Type Size Used Priority\n";
    let active_zero = "Filename Type Size Used Priority\n/dev/nbd0 partition 1024 0 100\n";
    let deleted_zero =
        "Filename Type Size Used Priority\n/dev/nbd0\\040(deleted) partition 1024 0 100\n";
    assert_eq!(
        nbd_swap_is_explicitly_absent_from_text(header, "/dev/nbd0"),
        Ok(true)
    );
    assert_eq!(
        nbd_swap_is_explicitly_absent_from_text(active_zero, "/dev/nbd0"),
        Ok(false)
    );
    assert_eq!(
        nbd_swap_is_explicitly_absent_from_text(deleted_zero, "/dev/nbd0"),
        Ok(false)
    );
    assert!(nbd_swap_is_explicitly_absent_from_text(header, "/dev/not-nbd").is_err());
}

#[test]
fn strict_swap_parser_rejects_malformed_or_ambiguous_snapshots() {
    let bad_header = "Filename Type Size Used\n";
    let bad_numeric = "Filename Type Size Used Priority\n/dev/nbd0 partition 1024 unknown -2\n";
    let bad_type = "Filename Type Size Used Priority\n/dev/ublkb7 mystery 1024 0 -2\n";
    let duplicate = "\
Filename Type Size Used Priority
/dev/ublkb7 partition 1024 0 -2
/dev/ublkb7 partition 1024 0 -3
";

    for snapshot in [bad_header, bad_numeric, bad_type, duplicate] {
        assert!(
            strict_exact_swap_state_from_text(snapshot, "/dev/ublkb7", canonical_ublk_identity)
                .is_err(),
            "malformed or ambiguous /proc/swaps data must fail closed"
        );
    }
}

#[test]
fn unix_socket_setup_refuses_regular_file_and_symlink() {
    let dir = std::env::temp_dir().join(format!("ramshared-socket-path-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir(&dir).expect("create socket test directory");
    let regular = dir.join("regular");
    std::fs::write(&regular, b"keep").expect("create regular file");
    assert!(prepare_unix_socket_path(&regular).is_err());
    assert_eq!(
        std::fs::read(&regular).expect("regular file preserved"),
        b"keep"
    );

    let link = dir.join("link");
    std::os::unix::fs::symlink(&regular, &link).expect("create symlink");
    assert!(prepare_unix_socket_path(&link).is_err());
    assert!(
        std::fs::symlink_metadata(&link)
            .expect("symlink preserved")
            .file_type()
            .is_symlink()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
// TestName: second_daemon_refuses_existing_socket_and_preserves_original_listener
fn second_daemon_refuses_existing_socket_and_preserves_original_listener() {
    let dir =
        std::env::temp_dir().join(format!("ramshared-existing-socket-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("daemon.sock");
    let original = UnixListener::bind(&path).unwrap();

    let refused = prepare_unix_socket_path(&path).is_err();
    let connectable = std::os::unix::net::UnixStream::connect(&path).is_ok();
    drop(original);
    let _ = std::fs::remove_file(&path);
    std::fs::remove_dir_all(&dir).unwrap();

    assert!(refused, "a second daemon removed the live socket pathname");
    assert!(
        connectable,
        "the original listener stopped being connectable after second-daemon refusal"
    );
}

#[test]
// TestName: stale_unix_socket_requires_explicit_cleanup_and_is_never_unlinked_on_startup
fn stale_unix_socket_requires_explicit_cleanup_and_is_never_unlinked_on_startup() {
    let dir = std::env::temp_dir().join(format!("ramshared-stale-socket-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("daemon.sock");
    drop(UnixListener::bind(&path).unwrap());

    assert!(bind_owned_unix_listener(&path).is_err());
    assert!(
        std::fs::symlink_metadata(&path)
            .expect("stale socket pathname remains")
            .file_type()
            .is_socket()
    );

    std::fs::remove_file(&path).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
// TestName: old_socket_cleanup_preserves_aba_replacement_identity
fn old_socket_cleanup_preserves_aba_replacement_identity() {
    let dir = std::env::temp_dir().join(format!("ramshared-socket-aba-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("daemon.sock");
    let (original, old_guard) = bind_owned_unix_listener(&path).unwrap();
    drop(original);
    std::fs::remove_file(&path).unwrap();
    let replacement = UnixListener::bind(&path).unwrap();

    drop(old_guard);
    let replacement_survived = path.exists();
    let connectable = std::os::unix::net::UnixStream::connect(&path).is_ok();
    drop(replacement);
    let _ = std::fs::remove_file(&path);
    std::fs::remove_dir_all(&dir).unwrap();

    assert!(
        replacement_survived && connectable,
        "cleanup for an old socket identity removed its ABA replacement"
    );
}
