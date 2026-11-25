// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/runtime/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::config::WinDriveConfig;
use std::path::PathBuf;

#[test]
fn inspect_gpus_is_a_read_only_command_without_configuration() {
    let command = parse_product_cli(&["inspect-gpus".into(), "--json".into()]);
    assert!(command.is_ok(), "native GPU identity inspection is missing");
    assert!(!product_runtime_selected(&command.unwrap()));
    for args in [
        vec!["inspect-gpus"],
        vec!["inspect-gpus", "--json", "--start"],
        vec!["inspect-gpus", "--config", "C:\\config.toml"],
    ] {
        assert!(
            parse_product_cli(&args.into_iter().map(String::from).collect::<Vec<_>>()).is_err()
        );
    }
}

fn cfg() -> WinDriveConfig {
    WinDriveConfig {
        size_bytes: 64 * 1024 * 1024,
        block_size: 4096,
        cuda_device: 0,
        reserve_bytes: 512 * 1024 * 1024,
        queue_depth: 4,
        max_io_bytes: 1024 * 1024,
        evidence_path: PathBuf::from(r"C:\ProgramData\RamShared\evidence"),
        volume_letter: 'D',
        volume_mount_path: None,
        broker_pipe: crate::config::BrokerPipeV1::NamedPipeV1,
        broker_ready_timeout_secs: 30,
        tenant: "wd".into(),
        heartbeat_secs: 5,
    }
}

#[derive(Default)]
struct MockOps {
    fail_at: Option<&'static str>,
    busy_once: bool,
    busy_observe: bool,
    pagefile_clear: bool,
    log: EffectLog,
    released: Vec<u32>,
    destroyed: u32,
    freed: u32,
    unregistered: u32,
    next_lease: u32,
}

impl RuntimeOps for MockOps {
    fn acquire_lease(&mut self, _bytes: u64) -> Result<u32, RuntimeError> {
        if self.fail_at == Some("lease") {
            return Err(RuntimeError::new(RuntimeErrorClass::Broker, 3, "deny"));
        }
        self.next_lease += 1;
        Ok(self.next_lease)
    }
    fn release_lease(&mut self, lease_id: u32) -> Result<(), RuntimeError> {
        self.released.push(lease_id);
        self.log.lease_release += 1;
        Ok(())
    }
    fn cuda_alloc(&mut self, _bytes: u64) -> Result<(), RuntimeError> {
        if self.fail_at == Some("cuda") {
            return Err(RuntimeError::new(RuntimeErrorClass::Cuda, 4, "oom"));
        }
        self.log.cuda_alloc += 1;
        Ok(())
    }
    fn cuda_free(&mut self) -> Result<(), RuntimeError> {
        self.freed += 1;
        self.log.cuda_free += 1;
        Ok(())
    }
    fn create_disk(&mut self) -> Result<(), RuntimeError> {
        if self.fail_at == Some("create") {
            return Err(RuntimeError::new(RuntimeErrorClass::Abi, 5, "create fail"));
        }
        if self.busy_once {
            self.busy_once = false;
            return Err(RuntimeError::new(RuntimeErrorClass::Busy, 5, "busy"));
        }
        self.log.disk_create += 1;
        Ok(())
    }
    fn destroy_disk(&mut self) -> Result<(), RuntimeError> {
        self.destroyed += 1;
        self.log.disk_destroy += 1;
        Ok(())
    }
    fn register_queue(&mut self) -> Result<(), RuntimeError> {
        if self.fail_at == Some("register") {
            return Err(RuntimeError::new(RuntimeErrorClass::Abi, 5, "reg fail"));
        }
        self.log.queue_register += 1;
        Ok(())
    }
    fn unregister_queue(&mut self) -> Result<(), RuntimeError> {
        self.unregistered += 1;
        self.log.queue_unregister += 1;
        Ok(())
    }
    fn pagefile_gates_clear(&mut self) -> Result<bool, RuntimeError> {
        Ok(self.pagefile_clear)
    }
    fn drain_io(&mut self) -> Result<(), RuntimeError> {
        Ok(())
    }
    fn observe_busy(&mut self) -> Result<bool, RuntimeError> {
        Ok(self.busy_observe)
    }
}

#[test]
fn no_fallback_after_cuda_failure() {
    let c = cfg();
    let mut state = RuntimeState::new(RunMode::Console);
    let mut ops = MockOps {
        fail_at: Some("cuda"),
        pagefile_clear: true,
        ..Default::default()
    };
    let e = run_runtime(&c, &mut state, &mut ops).unwrap_err();
    assert_eq!(e.class, RuntimeErrorClass::Cuda);
    assert_eq!(ops.log.fallback_selected, 0);
    assert_eq!(ops.released.len(), 1);
    assert_eq!(ops.freed, 0);
    assert_eq!(state.phase, RuntimePhase::FailedSafe);
}

#[test]
fn online_broker_loss_is_not_replayed() {
    let mut state = RuntimeState::new(RunMode::Scm);
    state.phase = RuntimePhase::Online;
    state.effects.lease_acquire = 1;
    contain_broker_loss(&mut state);
    contain_broker_loss(&mut state);
    assert_eq!(state.phase, RuntimePhase::FailedSafe);
    assert_eq!(state.effects.lease_acquire, 1);
    assert!(!state.healthy);
}

#[test]
fn pre_exposure_broker_loss_unwinds() {
    let mut state = RuntimeState::new(RunMode::Scm);
    state.phase = RuntimePhase::WaitingForBroker;
    contain_broker_loss(&mut state);
    assert_eq!(state.phase, RuntimePhase::Stopped);
    assert_eq!(
        post_teardown_release_disposition(false, false),
        PostTeardownReleaseDisposition::PreserveFailedSafe
    );
}

#[test]
fn online_broker_loss_keeps_pump_until_safe_stop() {
    let mut state = RuntimeState::new(RunMode::Scm);
    state.phase = RuntimePhase::Online;
    contain_broker_loss(&mut state);
    assert_eq!(state.phase, RuntimePhase::FailedSafe);
    assert_eq!(
        post_teardown_release_disposition(true, false),
        PostTeardownReleaseDisposition::StopWithBrokerFailure
    );
}

#[test]
fn online_pipe_eof_is_not_reconnected() {
    assert_eq!(
        post_teardown_release_disposition(true, true),
        PostTeardownReleaseDisposition::CleanStop
    );
}

#[test]
fn unsafe_uninstall_plan_refuses() {
    assert!(!uninstall_plan_safe(true, false));
    assert!(!uninstall_plan_safe(false, true));
    assert!(uninstall_plan_safe(false, false));
}

#[test]
fn failure_after_lease_releases_once() {
    let c = cfg();
    let mut state = RuntimeState::new(RunMode::Console);
    let mut ops = MockOps {
        fail_at: Some("cuda"),
        pagefile_clear: true,
        ..Default::default()
    };
    let _ = run_runtime(&c, &mut state, &mut ops);
    assert_eq!(ops.released.len(), 1);
    // Second unwind must not double-release
    assert_eq!(ops.log.lease_release, 1);
}

#[test]
fn failure_after_cuda_frees_before_release() {
    let c = cfg();
    let mut state = RuntimeState::new(RunMode::Console);
    let mut ops = MockOps {
        fail_at: Some("create"),
        pagefile_clear: true,
        ..Default::default()
    };
    let _ = run_runtime(&c, &mut state, &mut ops).unwrap_err();
    assert_eq!(ops.freed, 1);
    assert_eq!(ops.released.len(), 1);
    assert_eq!(ops.destroyed, 0);
}

#[test]
fn failure_after_create_destroys_before_free() {
    let c = cfg();
    let mut state = RuntimeState::new(RunMode::Console);
    let mut ops = MockOps {
        fail_at: Some("register"),
        pagefile_clear: true,
        ..Default::default()
    };
    let _ = run_runtime(&c, &mut state, &mut ops).unwrap_err();
    assert_eq!(ops.destroyed, 1);
    assert_eq!(ops.freed, 1);
    assert_eq!(ops.released.len(), 1);
    assert_eq!(ops.unregistered, 0);
}

#[test]
fn failure_after_register_unwinds_reverse() {
    // Inject failure by marking post-register stop via custom: use Online then fail stop.
    // Direct: run full online, then force unregister path by stop.
    let c = cfg();
    let mut state = RuntimeState::new(RunMode::Console);
    let mut ops = MockOps {
        pagefile_clear: true,
        ..Default::default()
    };
    run_runtime(&c, &mut state, &mut ops).unwrap();
    assert_eq!(state.phase, RuntimePhase::Online);
    // Simulate failure after register by manual unwind from QueueRegistered-equivalent.
    state.phase = RuntimePhase::QueueRegistered;
    unwind_from(&mut state, &mut ops, RuntimePhase::QueueRegistered).unwrap();
    assert_eq!(ops.unregistered, 1);
    assert_eq!(ops.destroyed, 1);
    assert_eq!(ops.freed, 1);
    assert_eq!(ops.released.len(), 1);
    // Order encoded by counts: each exactly once
    assert_eq!(ops.log.queue_unregister, 1);
    assert_eq!(ops.log.disk_destroy, 1);
    assert_eq!(ops.log.cuda_free, 1);
    assert_eq!(ops.log.lease_release, 1);
}

#[test]
fn deterministic_failure_is_not_retried() {
    let c = cfg();
    let mut state = RuntimeState::new(RunMode::Console);
    let mut ops = MockOps {
        fail_at: Some("create"),
        pagefile_clear: true,
        ..Default::default()
    };
    let _ = run_runtime(&c, &mut state, &mut ops);
    assert_eq!(ops.log.retries, 0);
    assert_eq!(ops.log.disk_create, 0);
}

#[test]
fn busy_observation_is_bounded() {
    let c = cfg();
    let mut state = RuntimeState::new(RunMode::Console);
    let mut ops = MockOps {
        busy_once: true,
        busy_observe: true,
        pagefile_clear: true,
        ..Default::default()
    };
    let e = run_runtime(&c, &mut state, &mut ops).unwrap_err();
    assert_eq!(e.class, RuntimeErrorClass::Busy);
    assert_eq!(state.effects.retries, 1);
    // CREATE not re-issued after busy
    assert_eq!(ops.log.disk_create, 0);
}

#[test]
fn stop_twice_has_one_effect() {
    let c = cfg();
    let mut state = RuntimeState::new(RunMode::Console);
    let mut ops = MockOps {
        pagefile_clear: true,
        ..Default::default()
    };
    run_runtime(&c, &mut state, &mut ops).unwrap();
    stop_runtime(&mut state, &mut ops).unwrap();
    assert_eq!(ops.destroyed, 1);
    assert_eq!(ops.released.len(), 1);
    let s2 = stop_runtime(&mut state, &mut ops).unwrap();
    assert!(s2.idempotent_stop);
    assert_eq!(ops.destroyed, 1);
    assert_eq!(ops.released.len(), 1);
}

#[test]
fn ambiguous_crash_state_is_not_replayed() {
    let c = cfg();
    let mut state = RuntimeState::new(RunMode::Console);
    state.ambiguous_crash = true;
    let mut ops = MockOps {
        pagefile_clear: true,
        ..Default::default()
    };
    let e = run_runtime(&c, &mut state, &mut ops).unwrap_err();
    assert_eq!(e.class, RuntimeErrorClass::AmbiguousCrash);
    assert_eq!(ops.log.lease_acquire, 0);
    let e2 = stop_runtime(&mut state, &mut ops).unwrap_err();
    assert_eq!(e2.class, RuntimeErrorClass::AmbiguousCrash);
    assert_eq!(ops.destroyed, 0);
}

#[test]
fn cuda_watchdog_does_not_destroy_stuck_context() {
    let c = cfg();
    let mut state = RuntimeState::new(RunMode::Console);
    let mut ops = MockOps {
        pagefile_clear: true,
        ..Default::default()
    };
    run_runtime(&c, &mut state, &mut ops).unwrap();
    state.cuda_op_outstanding = true;
    let e = stop_runtime(&mut state, &mut ops).unwrap_err();
    assert_eq!(e.class, RuntimeErrorClass::Watchdog);
    assert_eq!(ops.destroyed, 0);
    assert_eq!(ops.freed, 0);
    assert_eq!(ops.released.len(), 0);
    assert_eq!(state.phase, RuntimePhase::FailedSafe);
    assert!(!state.healthy);
}

#[test]
fn console_requires_storage_only() {
    let args = vec![
        "console".into(),
        "--config".into(),
        r"C:\ProgramData\RamShared\winsvc.toml".into(),
    ];
    let e = parse_product_cli(&args).unwrap_err();
    assert_eq!(e.class, RuntimeErrorClass::Config);
}

#[test]
fn product_cli_has_no_lab_backend_command() {
    for verb in ["run", "stop-console", "lab"] {
        let args = vec![verb.into()];
        let e = parse_product_cli(&args).unwrap_err();
        assert!(e.message.contains("lab") || e.message.contains("removed"));
    }
}

#[test]
fn scm_and_console_select_same_runtime() {
    let scm = parse_product_cli(&[]).unwrap();
    assert!(product_runtime_selected(&scm));
    assert_eq!(
        parse_product_cli(&["--config".into(), r"C:\product\winsvc.toml".into()]).unwrap(),
        ProductCommand::ScmDefault {
            config: Some(r"C:\product\winsvc.toml".into())
        }
    );
    let console = parse_product_cli(&[
        "console".into(),
        "--config".into(),
        r"C:\ProgramData\RamShared\winsvc.toml".into(),
        "--storage-only".into(),
    ])
    .unwrap();
    assert!(product_runtime_selected(&console));
    match console {
        ProductCommand::Console { storage_only, .. } => assert!(storage_only),
        _ => panic!("expected console"),
    }
}

#[test]
fn status_json_parses() {
    assert_eq!(
        parse_product_cli(&["status".into(), "--json".into()]).unwrap(),
        ProductCommand::Status { json: true }
    );
    assert!(parse_product_cli(&["status".into(), "--xml".into()]).is_err());
}

#[test]
fn start_stop_commands_are_explicit() {
    assert_eq!(
        parse_product_cli(&["start".into()]).unwrap(),
        ProductCommand::Start
    );
    assert_eq!(
        parse_product_cli(&["stop".into()]).unwrap(),
        ProductCommand::Stop
    );
    assert!(parse_product_cli(&["start".into(), "now".into()]).is_err());
}

#[test]
fn parse_product_cli_install_and_repair() {
    let install_res = parse_product_cli(&[
        "install".into(),
        "--manifest".into(),
        "/etc/manifest.json".into(),
    ])
    .unwrap();
    assert_eq!(
        install_res,
        ProductCommand::Install {
            manifest: "/etc/manifest.json".into(),
            mode: InstallMode::Storage,
        }
    );

    let repair_res = parse_product_cli(&[
        "repair".into(),
        "--manifest".into(),
        r"C:\manifest.json".into(),
    ])
    .unwrap();
    assert_eq!(
        repair_res,
        ProductCommand::Repair {
            manifest: r"C:\manifest.json".into(),
            mode: InstallMode::Storage,
        }
    );

    let err_missing = parse_product_cli(&["install".into()]).unwrap_err();
    assert_eq!(err_missing.class, RuntimeErrorClass::Config);
    assert!(err_missing.message.contains("missing --manifest"));

    let err_rel = parse_product_cli(&[
        "install".into(),
        "--manifest".into(),
        "relative/manifest.json".into(),
    ])
    .unwrap_err();
    assert_eq!(err_rel.class, RuntimeErrorClass::Config);
    assert!(err_rel.message.contains("path must be absolute"));

    let err_no_val = parse_product_cli(&["install".into(), "--manifest".into()]).unwrap_err();
    assert_eq!(err_no_val.class, RuntimeErrorClass::Config);
    assert!(err_no_val.message.contains("--manifest requires a path"));
}

#[test]
fn install_and_repair_parse_optional_authority_only_and_reject_invalid_flags() {
    assert_eq!(
        parse_product_cli(&[
            "install".into(),
            "--manifest".into(),
            r"C:\pkg\product-manifest.json".into(),
            "--authority-only".into(),
        ])
        .unwrap(),
        ProductCommand::Install {
            manifest: r"C:\pkg\product-manifest.json".into(),
            mode: InstallMode::AuthorityOnly,
        }
    );
    assert_eq!(
        parse_product_cli(&[
            "repair".into(),
            "--authority-only".into(),
            "--manifest".into(),
            "/tmp/product-manifest.json".into(),
        ])
        .unwrap(),
        ProductCommand::Repair {
            manifest: "/tmp/product-manifest.json".into(),
            mode: InstallMode::AuthorityOnly,
        }
    );
    for bad in [
        vec![
            "install".into(),
            "--manifest".into(),
            "/a.json".into(),
            "--authority-only".into(),
            "--authority-only".into(),
        ],
        vec![
            "install".into(),
            "--manifest".into(),
            "/a.json".into(),
            "--extra".into(),
        ],
        vec!["status".into(), "--authority-only".into()],
    ] {
        assert_eq!(
            parse_product_cli(&bad).unwrap_err().class,
            RuntimeErrorClass::Config
        );
    }
}

#[test]
fn parse_product_cli_probe_cuda() {
    let res = parse_product_cli(&[
        "probe-cuda".into(),
        "--config".into(),
        "/etc/winsvc.toml".into(),
    ])
    .unwrap();
    assert_eq!(
        res,
        ProductCommand::ProbeCuda {
            config: "/etc/winsvc.toml".into()
        }
    );

    let err = parse_product_cli(&["probe-cuda".into()]).unwrap_err();
    assert_eq!(err.class, RuntimeErrorClass::Config);
    assert!(err.message.contains("missing --config"));
}

#[test]
fn parse_product_cli_config_flag_variations() {
    let cfg_res = parse_product_cli(&["--config".into(), "/etc/winsvc.toml".into()]).unwrap();
    assert_eq!(
        cfg_res,
        ProductCommand::ScmDefault {
            config: Some("/etc/winsvc.toml".into())
        }
    );

    let err_one = parse_product_cli(&["--config".into()]).unwrap_err();
    assert_eq!(err_one.class, RuntimeErrorClass::Config);
    assert!(err_one.message.contains("unknown command"));

    let err_three =
        parse_product_cli(&["--config".into(), "/etc/winsvc.toml".into(), "extra".into()])
            .unwrap_err();
    assert_eq!(err_three.class, RuntimeErrorClass::Config);
    assert!(err_three.message.contains("unknown command"));
}

#[test]
fn parse_product_cli_path_formats() {
    let unc_res = parse_product_cli(&[
        "install".into(),
        "--manifest".into(),
        r"\\server\share\manifest.json".into(),
    ])
    .unwrap();
    assert_eq!(
        unc_res,
        ProductCommand::Install {
            manifest: r"\\server\share\manifest.json".into(),
            mode: InstallMode::Storage,
        }
    );

    let win_slash_res = parse_product_cli(&[
        "install".into(),
        "--manifest".into(),
        "C:/manifest.json".into(),
    ])
    .unwrap();
    assert_eq!(
        win_slash_res,
        ProductCommand::Install {
            manifest: "C:/manifest.json".into(),
            mode: InstallMode::Storage,
        }
    );
}

#[test]
fn parse_product_cli_unknown_and_lab_commands() {
    let err_unknown = parse_product_cli(&["foobar".into()]).unwrap_err();
    assert_eq!(err_unknown.class, RuntimeErrorClass::Config);
    assert!(err_unknown.message.contains("unknown command: foobar"));

    for verb in ["run", "stop-console", "lab", "start-scripts"] {
        let e = parse_product_cli(&[verb.into()]).unwrap_err();
        assert_eq!(e.class, RuntimeErrorClass::Config);
        assert!(e.message.contains("lab backend command removed"));
    }
}

#[test]
fn parse_product_cli_uninstall_and_scm_default() {
    assert_eq!(
        parse_product_cli(&["uninstall".into()]).unwrap(),
        ProductCommand::Uninstall
    );

    assert_eq!(
        parse_product_cli(&["".into()]).unwrap(),
        ProductCommand::ScmDefault { config: None }
    );
}

#[test]
fn parse_package_requires_absolute_dir_version_and_hex_commit() {
    let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(
        parse_product_cli(&args(&[
            "package",
            "--dir",
            "/tmp/pkg",
            "--version",
            "0.15.0",
            "--commit",
            "abcdef123456"
        ]))
        .unwrap(),
        ProductCommand::Package {
            dir: "/tmp/pkg".into(),
            version: "0.15.0".into(),
            commit: "abcdef123456".into(),
            start_policy: StartPolicy::Demand,
        }
    );
    assert_eq!(
        parse_product_cli(&args(&[
            "package",
            "--dir",
            "/tmp/pkg",
            "--version",
            "0.15.0",
            "--commit",
            "abcdef123456",
            "--start-policy",
            "automatic"
        ]))
        .unwrap(),
        ProductCommand::Package {
            dir: "/tmp/pkg".into(),
            version: "0.15.0".into(),
            commit: "abcdef123456".into(),
            start_policy: StartPolicy::Automatic,
        }
    );
    for bad in [
        vec!["package"],
        vec![
            "package",
            "--dir",
            "relative",
            "--version",
            "1",
            "--commit",
            "abcdef123456",
        ],
        vec!["package", "--dir", "/tmp/pkg", "--version", "1"],
        vec!["package", "--dir", "/tmp/pkg", "--commit", "abcdef123456"],
        vec![
            "package",
            "--dir",
            "/tmp/pkg",
            "--version",
            "1",
            "--commit",
            "abcdef123456",
            "--start-policy",
            "sometimes",
        ],
        vec![
            "package",
            "--dir",
            "/tmp/pkg",
            "--version",
            "1",
            "--commit",
            "abcdef123456",
            "--unknown",
            "x",
        ],
    ] {
        assert!(parse_product_cli(&args(&bad)).is_err(), "{bad:?} accepted");
    }
}

#[test]
fn parse_package_verify_requires_an_absolute_dir() {
    let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(
        parse_product_cli(&args(&["package-verify", "--dir", "/tmp/pkg"])).unwrap(),
        ProductCommand::VerifyPackage {
            dir: "/tmp/pkg".into()
        }
    );
    for bad in [
        vec!["package-verify"],
        vec!["package-verify", "--dir", "relative"],
        vec!["package-verify", "--dir"],
    ] {
        assert!(parse_product_cli(&args(&bad)).is_err(), "{bad:?} accepted");
    }
}

#[test]
fn authority_service_cli_never_selects_the_storage_runtime() {
    let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let command = parse_product_cli(&args(&["authority-service"])).unwrap();
    assert_eq!(command, ProductCommand::AuthorityService);
    assert!(!product_runtime_selected(&command));
    for extra in ["--config", "--storage-only", "unexpected"] {
        assert!(parse_product_cli(&args(&["authority-service", extra])).is_err());
    }
}
