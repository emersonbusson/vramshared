// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main_tests/origin_and_cli_flags.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;

#[test]
fn origin_and_cache_failures_are_sticky_until_exact_recovery() {
    struct RefusingProvider;
    impl VramProvider for RefusingProvider {
        type Mem<'a> = TestMemory;

        fn alloc(&self, _bytes: usize) -> Result<Self::Mem<'_>, ramshared_vram::VramError> {
            Err(ramshared_vram::VramError::Provider(
                "injected cache allocation refusal".into(),
            ))
        }

        fn mem_info(&self) -> Result<(u64, u64), ramshared_vram::VramError> {
            Err(ramshared_vram::VramError::Provider(
                "injected missing measurement".into(),
            ))
        }
    }

    let fail = std::rc::Rc::new(std::cell::Cell::new(true));
    let origin = TestOrigin {
        bytes: std::cell::RefCell::new(vec![0; BLOCK_SIZE as usize]),
        fail: std::rc::Rc::clone(&fail),
    };
    let provider = RefusingProvider;
    let mut backend = WriteThroughCacheBackend::new(&provider, origin, GIB, BLOCK_SIZE)
        .expect("valid origin-cache fixture");
    assert!(backend.write_at(0, &[0; BLOCK_SIZE as usize]).is_err());
    assert_eq!(backend.origin_state(), DurableOriginState::Failed);
    assert_eq!(
        backend
            .observe_gpu(None, Duration::from_secs(1), std::time::Instant::now())
            .target_bytes,
        0
    );
    assert_eq!(backend.cache_state(), OriginCacheState::Unavailable);
    assert!(!origin_cache_runtime_ok(
        backend.origin_state(),
        backend.cache_state()
    ));

    let status = OriginCacheStatus {
        configuration_generation: None,
        schema_version: 1,
        daemon_instance_id: "fixture-daemon".into(),
        written_at_unix_ms: Some(1_000),
        ok: origin_cache_runtime_ok(backend.origin_state(), backend.cache_state()),
        origin_state: backend.origin_state().as_str(),
        cache_state: backend.cache_state().as_str(),
        // Hold faces take the production conversion, never a bare shift: a
        // fixture that floors here would re-encode verified test case's bug and
        // silently agree with a reverted publisher.
        logical_capacity_kib: hold_kib(backend.size_bytes()),
        vram_cached_kib: Some(hold_kib(backend.cached_bytes())),
        gpu_headroom_kib: None,
        cache_members: None,
        gpu_budget: None,
        gpu_utilization: None,
        gpu_cache: None,
        host_memory: None,
        host_storage: None,
        control_plane_state: None,
        control_plane_lease_deadline_ms: None,
        ssd_origin_written_kib: hold_kib(backend.telemetry().origin_written_bytes),
        cache_hits: 0,
        cache_fallback_reads: backend.telemetry().fallback_reads,
        cache_invalidations: backend.telemetry().invalidations,
        cache_releases: backend.telemetry().releases,
        origin_cache_io: OriginCacheIo::from(backend.telemetry()),
        cache_target_kib: hold_kib(backend.target_bytes()),
        serve_threads: serve_pool::DEFAULT_SERVE_THREADS,
        serve_requests_total: 0,
        cache_unavailable_reason: None,
    };
    let serialized = serde_json::to_value(&status).unwrap();
    assert_eq!(serialized["origin_state"], "FAILED");
    assert_eq!(serialized["cache_state"], "UNAVAILABLE");
    assert_eq!(serialized["ok"], false);
    // An absent utilization sample is "not measured", never a measured idle
    // zero. The key stays present as `null` so a consumer can tell the two
    // apart instead of reading a fabricated `0%`.
    assert!(serialized["gpu_utilization"].is_null());
    assert_ne!(
        serialized["gpu_utilization"], 0,
        "an unmeasured utilization sample must not serialize as a zero percent"
    );

    let root = std::env::temp_dir().join(format!("ramshared-cache-status-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let path = root.join("nested/cache-status.json");
    write_origin_cache_status(&path, &status).unwrap();
    write_origin_cache_status(&path, &status).unwrap();
    let persisted: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(persisted["origin_state"], "FAILED");
    let blocker = root.join("not-a-directory");
    std::fs::write(&blocker, b"block").unwrap();
    assert!(write_origin_cache_status(&blocker.join("status.json"), &status).is_err());
    assert!(write_origin_cache_status(Path::new("/"), &status).is_err());
    std::fs::remove_dir_all(root).unwrap();

    fail.set(false);
    assert_eq!(
        backend.probe_origin().unwrap(),
        DurableOriginState::Degraded
    );
    assert_eq!(
        backend.probe_origin().unwrap(),
        DurableOriginState::Degraded
    );
    assert_eq!(backend.probe_origin().unwrap(), DurableOriginState::Ready);
    assert!(origin_cache_runtime_ok(
        backend.origin_state(),
        backend.cache_state()
    ));
}

#[test]
// TestName: serve_threads_defaults_to_four_and_refuses_out_of_range
// invariant check(boundary): the flag's edges are 1 and 16, and every value
// outside them is refused before any thread starts (SPEC DT-6).
fn serve_threads_defaults_to_four_and_refuses_out_of_range() {
    let default = AppArgs::parse_from(&daemon_argv(&["ramsharedd"])).expect("default argv");
    assert_eq!(
        default.serve_threads,
        serve_pool::DEFAULT_SERVE_THREADS,
        "the documented default is the pool constant, not a second literal"
    );
    assert_eq!(default.serve_threads, 4);

    let one = AppArgs::parse_from(&daemon_argv(&["ramsharedd", "--serve-threads", "1"]))
        .expect("N = 1 is the serialized rollback (DT-4)");
    assert_eq!(one.serve_threads, 1);

    let max = AppArgs::parse_from(&daemon_argv(&["ramsharedd", "--serve-threads", "16"]))
        .expect("N = 16 is the bound");
    assert_eq!(max.serve_threads, 16);

    for refused in ["0", "17", "many"] {
        let error =
            match AppArgs::parse_from(&daemon_argv(&["ramsharedd", "--serve-threads", refused])) {
                Err(error) => error,
                Ok(_) => {
                    panic!("--serve-threads {refused} must be refused before any thread starts")
                }
            };
        let text = error.to_string();
        assert!(
            text.contains("--serve-threads must be between 1 and 16")
                || text.contains("--serve-threads is invalid"),
            "the refusal must name the bound: {text}"
        );
    }

    let missing = match AppArgs::parse_from(&daemon_argv(&["ramsharedd", "--serve-threads"])) {
        Err(error) => error,
        Ok(_) => panic!("--serve-threads without a value must be refused"),
    };
    assert!(
        missing
            .to_string()
            .contains("--serve-threads requires a value")
    );
}

/// DT-7 check (b), ITEM-5: the runtime is bound to what the seal verified.
/// These inequalities can fail — a resolver that undercuts the minimum or
/// drops the percentage trips them.
#[test]
fn enforcement_binding_matches_verified_seal() {
    use ramshared_vram::ReserveFloorPolicy;

    // The sealed literals are one authority. Pin the attested values so a
    // change to them is a deliberate, test-visible act rather than drift.
    assert_eq!(SEALED_RESERVE_MIN_MIB, 2048);
    assert_eq!(SEALED_RESERVE_PERCENT, 20);
    assert_eq!(
        ramshared_block::sparse_vram::SEALED_RESERVE_MIN_MIB,
        SEALED_RESERVE_MIN_MIB
    );
    assert_eq!(
        ramshared_block::sparse_vram::SEALED_RESERVE_PERCENT,
        SEALED_RESERVE_PERCENT
    );

    let resolved =
        ReserveFloorPolicy::from_manifest(SEALED_RESERVE_MIN_MIB, SEALED_RESERVE_PERCENT)
            .expect("sealed literals are valid");
    enforce_reserve_policy_binding(&resolved, SEALED_RESERVE_MIN_MIB, SEALED_RESERVE_PERCENT)
        .expect("sealed resolution binds");

    // A raised minimum is still bound (raise-only).
    let raised = ReserveFloorPolicy {
        min_floor_bytes: SEALED_RESERVE_MIN_MIB * 2 * 1024 * 1024,
        sealed_percent: SEALED_RESERVE_PERCENT,
    };
    enforce_reserve_policy_binding(&raised, SEALED_RESERVE_MIN_MIB, SEALED_RESERVE_PERCENT)
        .expect("raising the floor is legal");

    // Undercutting the minimum trips the first inequality.
    let undercut = ReserveFloorPolicy {
        min_floor_bytes: 128 * 1024 * 1024,
        sealed_percent: SEALED_RESERVE_PERCENT,
    };
    let err =
        enforce_reserve_policy_binding(&undercut, SEALED_RESERVE_MIN_MIB, SEALED_RESERVE_PERCENT)
            .expect_err("a lower floor must fail startup");
    assert!(err.contains("undercuts"), "{err}");

    // Dropping or altering the percentage trips the second.
    let dropped = ReserveFloorPolicy {
        min_floor_bytes: SEALED_RESERVE_MIN_MIB * 1024 * 1024,
        sealed_percent: 5,
    };
    let err =
        enforce_reserve_policy_binding(&dropped, SEALED_RESERVE_MIN_MIB, SEALED_RESERVE_PERCENT)
            .expect_err("a dropped percentage must fail startup");
    assert!(err.contains("sealed percent"), "{err}");
}

/// DT-8, ITEM-5: an override below the sealed authority fails startup on
/// the existing failure path (`ExitCode::from(1)`), not a new exit class.
#[test]
fn low_reserve_override_fails_startup() {
    use ramshared_vram::{ReserveFloorEnv, ReserveFloorPolicy};

    let base = ReserveFloorPolicy::from_manifest(
        ramshared_block::sparse_vram::SEALED_RESERVE_MIN_MIB,
        ramshared_block::sparse_vram::SEALED_RESERVE_PERCENT,
    )
    .expect("sealed literals are valid");

    // 128 MiB — the value the old clamp silently accepted.
    let too_low = ReserveFloorEnv {
        env_mib: Some(128),
        alias_mib: None,
    };
    let err = ReserveFloorPolicy::resolve_with_env(&base, &too_low)
        .expect_err("an override below the seal must fail");
    assert!(err.to_string().contains("below"), "{err}");

    // Conflicting names also fail closed rather than picking one.
    let conflict = ReserveFloorEnv {
        env_mib: Some(4096),
        alias_mib: Some(8192),
    };
    assert!(ReserveFloorPolicy::resolve_with_env(&base, &conflict).is_err());

    // A raise-only override at or above the seal is accepted.
    let raise = ReserveFloorEnv {
        env_mib: Some(4096),
        alias_mib: None,
    };
    let ok = ReserveFloorPolicy::resolve_with_env(&base, &raise).expect("raise is legal");
    assert_eq!(ok.min_floor_bytes, 4096 * 1024 * 1024);
    assert_eq!(ok.sealed_percent, base.sealed_percent);
}

/// DT-10: a raise-only override that actually raises the enforced result
/// is logged as `lab-override-raise`; one that the percentage share already
/// covers is logged `sealed-manifest` with "(override subsumed)". The log
/// never claims the floor moved when it did not.
#[test]
fn high_reserve_override_logs_raise_only() {
    use ramshared_vram::{ReserveFloorEnv, ReserveFloorPolicy, ReserveFloorSource};

    let base = ReserveFloorPolicy::from_manifest(
        ramshared_block::sparse_vram::SEALED_RESERVE_MIN_MIB,
        ramshared_block::sparse_vram::SEALED_RESERVE_PERCENT,
    )
    .expect("sealed literals are valid");

    // A high override raises the floor above the sealed-only result.
    let high = ReserveFloorEnv {
        env_mib: Some(8192),
        alias_mib: None,
    };
    let raised = ReserveFloorPolicy::resolve_with_env(&base, &high).expect("raise is legal");
    // Real capacity: 8 GiB adapter. 20% = 1.6 GiB, so an 8 GiB override
    // raises the configured reserve above the sealed-only result.
    let capacity = 8 * 1024 * 1024 * 1024u64;
    assert_eq!(
        raised.source_label(&base, capacity),
        ReserveFloorSource::LabOverrideRaise
    );
    assert_eq!(
        raised.source_label(&base, capacity).to_string(),
        "lab-override-raise"
    );

    // An override equal to the sealed minimum changes nothing.
    let same = ReserveFloorEnv {
        env_mib: Some(ramshared_block::sparse_vram::SEALED_RESERVE_MIN_MIB),
        alias_mib: None,
    };
    let unchanged = ReserveFloorPolicy::resolve_with_env(&base, &same).expect("equal is legal");
    assert_eq!(unchanged.min_floor_bytes, base.min_floor_bytes);
    assert_eq!(
        unchanged.source_label(&base, capacity),
        ReserveFloorSource::SealedManifest
    );

    // An override accepted above the minimum but still under the 20%
    // share is subsumed: the source stays `sealed-manifest`.
    // capacity = u64::MAX saturates the share, so use a finite capacity
    // whose 20% share exceeds a modest raise.
    let modest = ReserveFloorEnv {
        env_mib: Some(ramshared_block::sparse_vram::SEALED_RESERVE_MIN_MIB + 1),
        alias_mib: None,
    };
    let subsumed = ReserveFloorPolicy::resolve_with_env(&base, &modest).expect("raise is legal");
    let capacity = 64 * 1024 * 1024 * 1024u64; // 64 GiB → 20% = 12.8 GiB
    assert_eq!(
        subsumed.source_label(&base, capacity),
        ReserveFloorSource::SealedManifest,
        "an override the percentage share already covers is not a raise"
    );
}

#[test]
fn origin_args_default_to_four_gib_and_enforce_one_to_twenty_four_gib() {
    let args = AppArgs::parse_from(&daemon_argv(&[
        "ramsharedd",
        "--origin-manifest",
        ORIGIN_MANIFEST_PATH,
    ]))
    .expect("sealed product origin argv");
    assert_eq!(args.size, 4 * GIB);
    assert_eq!(args.origin.as_deref(), Some(ORIGIN_MANIFEST_PATH));

    for size in ["1023", "24577"] {
        assert!(
            AppArgs::parse_from(&daemon_argv(&[
                "ramsharedd",
                "--origin-manifest",
                ORIGIN_MANIFEST_PATH,
                "--size",
                size,
            ]))
            .is_err()
        );
    }

    let ublk = AppArgs::parse_from(&daemon_argv(&[
        "ramsharedd",
        "--origin-manifest",
        ORIGIN_MANIFEST_PATH,
        "--transport",
        "ublk",
    ]))
    .unwrap();
    assert!(select_daemon_action(ublk).is_err());

    let broker = AppArgs::parse_from(&daemon_argv(&[
        "ramsharedd",
        "--origin-manifest",
        ORIGIN_MANIFEST_PATH,
        "--slices",
        "1",
        "--slice-mb",
        "1024",
        "--arbiter-listen",
        "127.0.0.1:7777",
    ]))
    .unwrap();
    assert!(select_daemon_action(broker).is_err());

    assert!(
        AppArgs::parse_from(&daemon_argv(&[
            "ramsharedd",
            "--origin-manifest",
            "/tmp/caller-controlled.conf",
        ]))
        .is_err()
    );
    assert!(
        AppArgs::parse_from(&daemon_argv(&[
            "ramsharedd",
            "--origin",
            "/dev/disk/by-partuuid/11111111-2222-4333-8444-555555555555",
        ]))
        .is_err()
    );
}

#[test]
fn origin_mode_refuses_to_start_without_a_valid_daemon_identity() {
    assert!(require_origin_daemon_identity(true, || None).is_err());
    assert_eq!(
        require_origin_daemon_identity(true, || Some("fixture-daemon".into())).unwrap(),
        Some("fixture-daemon".into())
    );
    assert_eq!(
        require_origin_daemon_identity(false, || None).unwrap(),
        None
    );
}

#[test]
fn daemon_residency_probe_and_latency_paths_are_fake_backed() {
    let mut canary = None;
    let mut baseline = Vec::new();
    let mut sampler = ResidencySampler::new(ResidencyConfig {
        latency_mult: 2,
        consecutive: 1,
        free_floor_bytes: 100,
    });
    let mut cadence = Cadence::new(1);
    let mut probe = CanaryProbe::new(TestMemory::new(CANARY_BYTES));
    let mut state = ResidencyCheckState {
        canary: &mut canary,
        baseline: &mut baseline,
        sampler: &mut sampler,
        cadence: &mut cadence,
        probe: &mut probe,
        free_floor_bytes: 100,
    };
    assert_eq!(
        residency_check(10, &mut state, || Some(99)),
        Some(DemoteReason::FreeFloor)
    );

    let mut canary = None;
    let mut baseline = Vec::new();
    let mut sampler = ResidencySampler::new(ResidencyConfig::default());
    let mut cadence = Cadence::new(u64::from(u32::MAX));
    let mut probe = CanaryProbe::new(TestMemory::new(CANARY_BYTES));
    let mut state = ResidencyCheckState {
        canary: &mut canary,
        baseline: &mut baseline,
        sampler: &mut sampler,
        cadence: &mut cadence,
        probe: &mut probe,
        free_floor_bytes: ResidencyConfig::default().free_floor_bytes,
    };
    for _ in 0..16 {
        assert_eq!(residency_check(10, &mut state, || Some(u64::MAX)), None);
    }
    assert_eq!(residency_check(1_000, &mut state, || Some(u64::MAX)), None);
    assert_eq!(residency_check(1_000, &mut state, || Some(u64::MAX)), None);
    assert_eq!(
        residency_check(1_000, &mut state, || Some(u64::MAX)),
        Some(DemoteReason::Latency)
    );
}

#[test]
fn daemon_zero_window_and_backend_labels_are_deterministic() {
    let mut backend = RamBackend::new(2 * 1024 * 1024);
    backend
        .write_at(0, &vec![0xA5; 2 * 1024 * 1024])
        .expect("fill heap backend");
    zero_window(&mut backend, 512, (1 << 20) + 512).expect("zero selected window");
    let mut zeroed = vec![0xFF; (1 << 20) + 512];
    backend
        .read_at(512, &mut zeroed)
        .expect("read zeroed window");
    assert!(zeroed.iter().all(|byte| *byte == 0));
    let mut untouched = [0u8; 512];
    backend.read_at(0, &mut untouched).expect("read prefix");
    assert!(untouched.iter().all(|byte| *byte == 0xA5));
    assert_eq!(BackendKind::Vram.label(), "vram");
    assert_eq!(BackendKind::Vulkan.label(), "vulkan");
    assert_eq!(BackendKind::Ram.label(), "ram");
    assert_eq!(BackendKind::Auto.label(), "auto");
}

#[test]
fn daemon_args_accept_broker_wiring_and_normalize_addresses() {
    let args = AppArgs::parse_from(&daemon_argv(&[
        "ramsharedd",
        "--size",
        "257",
        "--sock",
        "/tmp/ramshared-daemon-test.sock",
        "--backend",
        "ram",
        "--slices",
        "2",
        "--slice-mb",
        "64",
        "--listen-nbd",
        "tcp://127.0.0.1:10809",
        "--advertise-nbd",
        "127.0.0.1:10810",
        "--arbiter-listen",
        "127.0.0.1:7777",
        "--telemetry-jsonl",
        "/tmp/ramshared-daemon-test.jsonl",
    ]))
    .expect("safe broker-RAM argv must parse");

    assert_eq!(args.size, 257 * 1024 * 1024);
    assert_eq!(args.slice_bytes, 64 * 1024 * 1024);
    assert!(matches!(args.backend, BackendKind::Ram));
    assert!(matches!(args.transport, Transport::Nbd));
    assert_eq!(args.listen_nbd_addr.unwrap().to_string(), "127.0.0.1:10809");
    assert_eq!(args.arbiter_addr.unwrap().to_string(), "127.0.0.1:7777");
    assert_eq!(args.advertise_tcp, Some(("127.0.0.1".into(), 10810)));
    assert_eq!(
        args.telemetry_jsonl.unwrap(),
        std::path::PathBuf::from("/tmp/ramshared-daemon-test.jsonl")
    );
}

#[test]
fn daemon_args_refuse_invalid_or_unsafe_combinations_before_backend() {
    for argv in [
        daemon_argv(&["ramsharedd", "--unknown"]),
        daemon_argv(&["ramsharedd", "--slices", "1"]),
        daemon_argv(&[
            "ramsharedd",
            "--slices",
            "1",
            "--slice-mb",
            "1",
            "--arbiter-listen",
            "0.0.0.0:7777",
        ]),
        daemon_argv(&["ramsharedd", "--slices", "257", "--slice-mb", "1"]),
        daemon_argv(&["ramsharedd", "--advertise-nbd", "127.0.0.1:10809"]),
        daemon_argv(&[
            "ramsharedd",
            "--transport",
            "ublk",
            "--slices",
            "1",
            "--slice-mb",
            "1",
        ]),
    ] {
        assert!(
            AppArgs::parse_from(&argv).is_err(),
            "unsafe argv unexpectedly parsed: {argv:?}"
        );
    }
}

#[test]
fn daemon_args_cover_flag_boundaries_before_backend() {
    let parsed = AppArgs::parse_from(&daemon_argv(&[
        "ramsharedd",
        "--size",
        "3",
        "--sock",
        "/tmp/ramsharedd-boundary.sock",
        "--force",
        "--nbd",
        "/dev/nbd77",
        "--transport",
        "nbd",
        "--queue-depth",
        "2",
        "--backend",
        "vram",
    ]))
    .expect("all single-daemon scalar flags must parse before backend selection");
    assert_eq!(parsed.size, 3 * 1024 * 1024);
    assert_eq!(parsed.sock, "/tmp/ramsharedd-boundary.sock");
    assert!(parsed.force);
    assert_eq!(parsed.nbd_dev, "/dev/nbd77");
    assert_eq!(parsed.queue_depth, 2);
    assert!(matches!(parsed.transport, Transport::Nbd));
    assert!(matches!(parsed.backend, BackendKind::Vram));

    for argv in [
        daemon_argv(&["ramsharedd", "--size"]),
        daemon_argv(&["ramsharedd", "--size", "not-a-number"]),
        daemon_argv(&["ramsharedd", "--size", "18446744073709551615"]),
        daemon_argv(&["ramsharedd", "--sock"]),
        daemon_argv(&["ramsharedd", "--nbd"]),
        daemon_argv(&["ramsharedd", "--transport"]),
        daemon_argv(&["ramsharedd", "--transport", "tcp"]),
        daemon_argv(&["ramsharedd", "--queue-depth"]),
        daemon_argv(&["ramsharedd", "--queue-depth", "zero"]),
        daemon_argv(&["ramsharedd", "--backend"]),
        daemon_argv(&["ramsharedd", "--backend", "cpu"]),
        daemon_argv(&["ramsharedd", "--slices"]),
        daemon_argv(&["ramsharedd", "--slices", "many"]),
        daemon_argv(&["ramsharedd", "--slice-mb"]),
        daemon_argv(&["ramsharedd", "--slice-mb", "many"]),
        daemon_argv(&["ramsharedd", "--listen-nbd"]),
        daemon_argv(&["ramsharedd", "--listen-nbd", "not-an-address"]),
        daemon_argv(&["ramsharedd", "--arbiter-listen"]),
        daemon_argv(&["ramsharedd", "--arbiter-listen", "not-an-address"]),
        daemon_argv(&["ramsharedd", "--advertise-nbd"]),
        daemon_argv(&["ramsharedd", "--advertise-nbd", "not-an-address"]),
        daemon_argv(&["ramsharedd", "--telemetry-jsonl"]),
    ] {
        assert!(
            AppArgs::parse_from(&argv).is_err(),
            "invalid argv unexpectedly reached a backend plan: {argv:?}"
        );
    }

    let overflow = AppArgs::parse_from(&daemon_argv(&[
        "ramsharedd",
        "--slices",
        "1",
        "--slice-mb",
        "18446744073709551615",
        "--arbiter-listen",
        "127.0.0.1:7777",
    ]));
    assert!(
        overflow.is_err(),
        "slice MiB overflow must refuse before a plan"
    );
}

#[test]
fn daemon_broker_config_preserves_telemetry_and_exact_endpoints() {
    let telemetry = std::path::PathBuf::from("/tmp/ramshared-daemon-telemetry.jsonl");
    let cfg = build_broker_config(
        3,
        "/tmp/ramshared-daemon.sock",
        Some(("127.0.0.1".into(), 10810)),
        "127.0.0.1:7777".parse().expect("loopback address"),
        Some(telemetry.clone()),
    );

    assert_eq!(cfg.listen.to_string(), "127.0.0.1:7777");
    assert_eq!(
        cfg.endpoints.nbd_unix.as_deref(),
        Some("/tmp/ramshared-daemon.sock")
    );
    assert_eq!(cfg.endpoints.nbd_tcp, Some(("127.0.0.1".into(), 10810)));
    assert_eq!(cfg.telemetry_jsonl, Some(telemetry));
    assert_eq!(cfg.slice_io.len(), 3);
    assert_eq!(cfg.vram.free.load(Ordering::Relaxed), 0);
    assert_eq!(cfg.vram.total.load(Ordering::Relaxed), 0);
}

#[test]
fn daemon_broker_ram_binds_loopback_and_cleans_owned_socket() {
    static NEXT_SOCKET: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "rs-broker-{}-{}.sock",
        std::process::id(),
        NEXT_SOCKET.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let listeners = bind_broker_listeners(
        &path,
        Some("127.0.0.1:0".parse().expect("loopback address")),
    )
    .expect("temporary loopback listeners must bind");

    assert!(
        std::fs::symlink_metadata(&path)
            .expect("owned socket exists")
            .file_type()
            .is_socket()
    );
    assert!(
        listeners
            .tcp
            .as_ref()
            .expect("TCP listener requested")
            .local_addr()
            .expect("TCP listener address")
            .ip()
            .is_loopback()
    );

    listeners.cleanup();
    assert!(
        !path.exists(),
        "cleanup must remove only the Unix socket it bound"
    );
}

#[test]
fn daemon_broker_bind_conflict_refuses_and_preserves_existing_listener() {
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").expect("occupy loopback");
    let path = std::env::temp_dir().join(format!(
        "ramshared-daemon-conflict-{}.sock",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);

    assert!(
        bind_broker_listeners(
            &path,
            Some(occupied.local_addr().expect("occupied address"))
        )
        .is_err(),
        "an occupied broker port must fail before a worker starts"
    );
    assert!(
        !path.exists(),
        "a failed TCP bind must clean up the Unix socket created for this attempt"
    );
    let probe = std::net::TcpStream::connect(occupied.local_addr().expect("occupied address"));
    assert!(
        probe.is_ok(),
        "the pre-existing listener must remain usable"
    );
}
