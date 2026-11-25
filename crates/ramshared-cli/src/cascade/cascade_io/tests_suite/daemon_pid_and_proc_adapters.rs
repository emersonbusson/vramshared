// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/tests_suite/daemon_pid_and_proc_adapters.rs
#![allow(clippy::expect_used)]

use super::*;

#[test]
fn down_removes_status_that_a_subsequent_cli_could_treat_as_current() {
    let fixture = TestDir::new();
    let daemon = spawn_fixture_daemon(&fixture, "exit 0");
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir)
        .unwrap_or_else(|error| panic!("create test runtime: {error}"));
    let devices = seal_runtime_lifecycle(&paths, daemon.id(), true);
    let cache_status = paths.cache_status_file.clone();
    let supervisor_status = paths.supervisor_status_file.clone();
    fs::write(&supervisor_status, r#"{"control_state":"HEALTHY"}"#)
        .unwrap_or_else(|error| panic!("write supervisor status: {error}"));
    let _seams = ParentSeams::install(
        "Filename Type Size Used Priority\n/dev/nbd0 partition 1024 0 100\n/dev/zram0 partition 1024 0 200\n",
        0,
    );
    install_two_tier_down_snapshots(&devices, true, true);
    let runner = ScriptedRunner::new(vec![
        ("swapoff -- /dev/nbd0".into(), Ok(String::new())),
        ("swapoff -- /dev/zram0".into(), Ok(String::new())),
        ("zramctl -r /dev/zram0".into(), Ok(String::new())),
        ("nbd-client -d /dev/nbd0".into(), Ok(String::new())),
    ]);

    down_with_runtime(&runner, &paths)
        .unwrap_or_else(|error| panic!("isolated cascade down: {error}"));

    assert!(
        !cache_status.exists(),
        "down left current-looking cache status"
    );
    assert!(
        !supervisor_status.exists(),
        "down left current-looking supervisor status"
    );
}

#[test]
fn down_refuses_unverifiable_daemon_and_preserves_runtime_evidence() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir)
        .unwrap_or_else(|error| panic!("create test runtime: {error}"));
    fs::write(&paths.swap_dev_file, "/dev/nbd0")
        .unwrap_or_else(|error| panic!("write test swap record: {error}"));
    fs::write(&paths.zram_dev_file, "/dev/zram0")
        .unwrap_or_else(|error| panic!("write test zram record: {error}"));
    fs::write(&paths.pid_file, "not-a-pid")
        .unwrap_or_else(|error| panic!("write malformed PID: {error}"));
    fs::write(&paths.cache_status_file, r#"{"cache_state":"ACTIVE"}"#)
        .unwrap_or_else(|error| panic!("write cache status: {error}"));
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    let runner = ScriptedRunner::new(vec![
        ("swapoff -- /dev/nbd0".into(), Ok(String::new())),
        ("swapoff -- /dev/zram0".into(), Ok(String::new())),
        ("zramctl -r /dev/zram0".into(), Ok(String::new())),
        ("nbd-client -d /dev/nbd0".into(), Ok(String::new())),
    ]);

    assert!(down_with_runtime(&runner, &paths).is_err());
    assert!(
        paths.pid_file.exists(),
        "PID evidence was deleted after refusal"
    );
    assert!(
        paths.cache_status_file.exists(),
        "cache evidence was deleted after daemon-stop refusal"
    );
}

#[test]
// TestName: nbd_disconnect_refusal_preserves_daemon_and_runtime_evidence
fn nbd_disconnect_refusal_preserves_daemon_and_runtime_evidence() {
    let fixture = TestDir::new();
    let daemon = spawn_fixture_daemon(&fixture, "exit 0");
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir)
        .unwrap_or_else(|error| panic!("create test runtime: {error}"));
    let devices = seal_runtime_lifecycle(&paths, daemon.id(), true);
    arm_forensics_at(&paths);
    let _seams = ParentSeams::install(
        "Filename Type Size Used Priority\n/dev/nbd0 partition 1024 0 100\n",
        0,
    );
    install_two_tier_down_snapshots(&devices, true, false);
    let runner = ScriptedRunner::new(vec![
        ("swapoff -- /dev/nbd0".into(), Ok(String::new())),
        ("zramctl -r /dev/zram0".into(), Ok(String::new())),
        (
            "nbd-client -d /dev/nbd0".into(),
            Err(CascadeError::Shell {
                cmd: "nbd-client -d /dev/nbd0".into(),
                msg: "manufactured NBD disconnect refusal".into(),
            }),
        ),
    ]);

    let error = error_from(
        down_with_runtime(&runner, &paths),
        "an NBD disconnect refusal must refuse down",
    );
    assert_eq!(
        error.to_string(),
        "command `nbd-client -d /dev/nbd0` failed: manufactured NBD disconnect refusal"
    );
    assert!(
        !process_is_gone_or_zombie(daemon.id()),
        "daemon was stopped"
    );
    assert!(paths.swap_dev_file.exists(), "swap record was deleted");
    assert!(paths.zram_dev_file.exists(), "zram record was deleted");
    assert!(paths.pid_file.exists(), "PID evidence was deleted");
    assert!(
        paths.cache_status_file.exists(),
        "cache evidence was deleted"
    );
    assert!(
        paths.forensics_markers[0].exists(),
        "forensics marker was deleted"
    );
    assert_eq!(
        runner.calls(),
        vec![
            "swapoff -- /dev/nbd0",
            "zramctl -r /dev/zram0",
            "nbd-client -d /dev/nbd0",
        ]
    );
}

#[test]
// TestName: daemon_stop_refuses_mismatched_instance_identity
fn daemon_stop_refuses_mismatched_instance_identity() {
    let fixture = TestDir::new();
    let daemon = spawn_fixture_daemon(&fixture, "exit 0");
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir)
        .unwrap_or_else(|error| panic!("create test runtime: {error}"));
    fs::write(&paths.pid_file, daemon.id().to_string())
        .unwrap_or_else(|error| panic!("write fixture daemon PID: {error}"));
    write_daemon_identity_status(&paths, daemon.id(), "999999-999999");
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);

    let error = error_from(
        stop_daemon_gracefully_at(&paths, Duration::from_millis(100)),
        "a stale daemon instance identity must refuse TERM",
    );
    assert_eq!(
        error.to_string(),
        "daemon stop refused: cache status does not identify the exact current ramsharedd process; runtime evidence retained"
    );
    assert!(
        !process_is_gone_or_zombie(daemon.id()),
        "stale PID received TERM"
    );
    assert!(
        paths.pid_file.exists(),
        "PID evidence was deleted after refusal"
    );
    assert!(
        paths.cache_status_file.exists(),
        "cache identity evidence was deleted after refusal"
    );
}

#[test]
// TestName: daemon_stop_refuses_stale_incomplete_or_unhealthy_cache_status
fn daemon_stop_refuses_stale_incomplete_or_unhealthy_cache_status() {
    for cache_status in [
            r#"{"schema_version":1,"daemon_instance_id":"placeholder"}"#.to_string(),
            r#"{"schema_version":1,"daemon_instance_id":"placeholder","written_at_unix_ms":0,"ok":true,"origin_state":"READY","cache_state":"ACTIVE","logical_capacity_kib":1,"vram_cached_kib":0,"gpu_headroom_kib":null,"ssd_origin_written_kib":1,"cache_fallback_reads":0,"cache_invalidations":0,"cache_releases":0,"cache_target_kib":0}"#.to_string(),
            format!(
                r#"{{"schema_version":1,"daemon_instance_id":"placeholder","written_at_unix_ms":{},"ok":false,"origin_state":"FAILED","cache_state":"STUCK","logical_capacity_kib":1,"vram_cached_kib":0,"gpu_headroom_kib":null,"ssd_origin_written_kib":1,"cache_fallback_reads":0,"cache_invalidations":0,"cache_releases":0,"cache_target_kib":0}}"#,
                unix_time_ms().unwrap_or_default()
            ),
        ] {
            let fixture = TestDir::new();
            let daemon = spawn_fixture_daemon(&fixture, "exit 0");
            let paths = RuntimePaths::under(&fixture.path);
            fs::create_dir_all(&paths.runtime_dir)
                .unwrap_or_else(|error| panic!("create test runtime: {error}"));
            fs::write(&paths.pid_file, daemon.id().to_string())
                .unwrap_or_else(|error| panic!("write fixture daemon PID: {error}"));
            let daemon_instance_id = daemon_instance_id_from_pid(daemon.id())
                .unwrap_or_else(|| panic!("derive fixture daemon identity"));
            fs::write(
                &paths.cache_status_file,
                cache_status.replace("placeholder", &daemon_instance_id),
            )
            .unwrap_or_else(|error| panic!("write cache status: {error}"));
            let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);

            assert!(stop_daemon_gracefully_at(&paths, Duration::from_millis(100)).is_err());
            assert!(
                !process_is_gone_or_zombie(daemon.id()),
                "unhealthy cache status authorized TERM"
            );
            assert!(paths.pid_file.exists(), "PID evidence was deleted");
            assert!(paths.cache_status_file.exists(), "cache evidence was deleted");
        }
}

#[test]
// TestName: pinned_test_clock_leaves_the_freshness_window_intact
fn pinned_test_clock_leaves_the_freshness_window_intact() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir)
        .unwrap_or_else(|error| panic!("create test runtime: {error}"));
    let pid = std::process::id();
    let instance_id = daemon_instance_id_from_pid(pid)
        .unwrap_or_else(|| panic!("derive current process identity"));
    let pinned = unix_time_ms().unwrap_or_default();
    TEST_UNIX_TIME_MS.with(|clock| clock.set(Some(pinned)));

    // A status written at the pinned instant is fresh: the fixture is not
    // racing its own heartbeat window.
    write_daemon_identity_status_at(&paths, pid, &instance_id, pinned);
    assert!(
        cache_status_has_current_daemon_identity(&paths, pid),
        "a status written at the pinned instant must be accepted"
    );

    // One millisecond past the production window is still stale. The pin
    // removes the race between writer and reader; it does not widen the
    // bound a live daemon has to meet.
    write_daemon_identity_status_at(
        &paths,
        pid,
        &instance_id,
        pinned.saturating_sub(CONTROL_STATUS_MAX_AGE_MS + 1),
    );
    assert!(
        !cache_status_has_current_daemon_identity(&paths, pid),
        "the pinned clock must still refuse a status older than the window"
    );

    // The last millisecond inside the window is accepted.
    write_daemon_identity_status_at(
        &paths,
        pid,
        &instance_id,
        pinned.saturating_sub(CONTROL_STATUS_MAX_AGE_MS - 1),
    );
    assert!(
        cache_status_has_current_daemon_identity(&paths, pid),
        "a status just inside the window must be accepted"
    );
}

#[test]
// TestName: daemon_signal_revalidates_instance_immediately_before_term
fn daemon_signal_revalidates_instance_immediately_before_term() {
    let fixture = TestDir::new();
    let daemon = spawn_fixture_daemon(&fixture, "exit 0");
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir)
        .unwrap_or_else(|error| panic!("create test runtime: {error}"));
    write_daemon_identity_status(
        &paths,
        daemon.id(),
        &daemon_instance_id_from_pid(daemon.id())
            .unwrap_or_else(|| panic!("derive fixture daemon identity")),
    );
    let expected = "replacement-instance".to_string();

    let error = error_from(
        signal_verified_daemon(&paths, daemon.id(), &expected),
        "a replacement daemon instance must not receive TERM",
    );
    assert!(
        error
            .to_string()
            .contains("changed before pidfd acquisition"),
        "{error}"
    );
    assert!(
        !process_is_gone_or_zombie(daemon.id()),
        "replacement process received TERM"
    );
}

#[test]
// TestName: pidfd_signal_remains_bound_after_validated_target_is_replaced
fn pidfd_signal_remains_bound_after_validated_target_is_replaced() {
    let fixture = TestDir::new();
    let mut daemon = spawn_fixture_daemon(&fixture, "exit 0");
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir)
        .unwrap_or_else(|error| panic!("create test runtime: {error}"));
    let expected = daemon_instance_id_from_pid(daemon.id())
        .unwrap_or_else(|| panic!("derive fixture daemon identity"));
    fs::write(&paths.pid_file, daemon.id().to_string())
        .unwrap_or_else(|error| panic!("write test daemon PID: {error}"));
    write_daemon_identity_status(&paths, daemon.id(), &expected);

    let target = open_verified_daemon_pidfd(&paths, daemon.id(), &expected)
        .unwrap_or_else(|error| panic!("validated daemon must produce pidfd: {error}"));
    daemon
        .0
        .kill()
        .unwrap_or_else(|error| panic!("stop validated fixture daemon: {error}"));
    daemon
        .0
        .wait()
        .unwrap_or_else(|error| panic!("reap validated fixture daemon: {error}"));
    let mut foreign = Command::new("/bin/sleep")
        .arg("10")
        .spawn()
        .unwrap_or_else(|error| panic!("spawn foreign replacement fixture: {error}"));
    let foreign_instance = daemon_instance_id_from_pid(foreign.id())
        .unwrap_or_else(|| panic!("derive foreign process identity"));
    fs::write(&paths.pid_file, foreign.id().to_string())
        .unwrap_or_else(|error| panic!("write foreign replacement PID: {error}"));
    write_daemon_identity_status(&paths, foreign.id(), &foreign_instance);

    let error = error_from(
        signal_daemon_pidfd(&target),
        "a pidfd for the exited validated target must not signal a replacement",
    );
    assert!(error.to_string().contains("pidfd TERM"), "{error}");
    assert!(
        foreign
            .try_wait()
            .unwrap_or_else(|error| panic!("inspect foreign replacement: {error}"))
            .is_none(),
        "foreign process received TERM"
    );
    foreign
        .kill()
        .unwrap_or_else(|error| panic!("stop foreign replacement: {error}"));
    foreign
        .wait()
        .unwrap_or_else(|error| panic!("reap foreign replacement: {error}"));
}

#[test]
// TestName: daemon_stop_allows_matching_instance_identity
fn daemon_stop_allows_matching_instance_identity() {
    let fixture = TestDir::new();
    let daemon = spawn_fixture_daemon(&fixture, "exit 0");
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir)
        .unwrap_or_else(|error| panic!("create test runtime: {error}"));
    fs::write(&paths.pid_file, daemon.id().to_string())
        .unwrap_or_else(|error| panic!("write fixture daemon PID: {error}"));
    let daemon_instance_id = daemon_instance_id_from_pid(daemon.id())
        .unwrap_or_else(|| panic!("derive fixture daemon identity"));
    write_daemon_identity_status(&paths, daemon.id(), &daemon_instance_id);
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);

    stop_daemon_gracefully_at(&paths, Duration::from_secs(1))
        .unwrap_or_else(|error| panic!("matching daemon identity must stop: {error}"));
    assert!(
        process_is_gone_or_zombie(daemon.id()),
        "matching daemon did not stop"
    );
    assert!(
        !paths.pid_file.exists(),
        "stopped daemon retained PID record"
    );
}

#[test]
fn down_timeout_preserves_runtime_evidence_and_refuses_success() {
    let fixture = TestDir::new();
    let daemon = spawn_fixture_daemon(&fixture, "");
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir)
        .unwrap_or_else(|error| panic!("create test runtime: {error}"));
    let devices = seal_runtime_lifecycle(&paths, daemon.id(), true);
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    install_two_tier_down_snapshots(&devices, false, false);
    assert!(
        read_swaps().expect("strict injected swaps").is_empty(),
        "timeout fixture must start swap-free"
    );
    struct RecordingRunner(RefCell<Vec<String>>);

    impl CommandRunner for RecordingRunner {
        fn run(&self, command: &str, args: &[&str]) -> Result<String, CascadeError> {
            self.0.borrow_mut().push(command_label(command, args));
            Ok(String::new())
        }
    }

    let runner = RecordingRunner(RefCell::new(Vec::new()));

    let error = error_from(
        down_with_runtime(&runner, &paths),
        "a daemon that misses the stop deadline must fail down",
    );
    assert!(error.to_string().contains("did not exit"), "{error}");
    assert!(
        paths.pid_file.exists(),
        "PID evidence was deleted after timeout"
    );
    assert!(
        paths.cache_status_file.exists(),
        "cache evidence was deleted after daemon-stop timeout"
    );
    assert_eq!(
        runner.0.borrow().as_slice(),
        ["zramctl -r /dev/zram0", "nbd-client -d /dev/nbd0"]
    );
}

pub(in crate::cascade::cascade_io) fn daemon_pid_requires_positive_pid_and_exact_identity() {
    assert!(daemon_pid_matches(42, "ramsharedd\n"));
    assert!(!daemon_pid_matches(0, "ramsharedd\n"));
    assert!(!daemon_pid_matches(42, "ramsharedd-helper\n"));
    assert!(!daemon_pid_matches(42, "other\n"));
}

#[test]
fn cascade_error_displays_all_variants_properly() {
    let err1 = CascadeError::Io("file not found".into());
    assert!(err1.to_string().contains("file not found"));

    let err2 = CascadeError::Arg("bad arg".into());
    assert!(err2.to_string().contains("bad arg"));

    let err3 = CascadeError::Precondition("precondition failed".into());
    assert!(err3.to_string().contains("precondition failed"));

    let err4 = CascadeError::UnsafeContainment("unsafe containment".into());
    assert!(err4.to_string().contains("unsafe containment"));

    let err5 = CascadeError::Shell {
        cmd: "zramctl".into(),
        msg: "failed".into(),
    };
    assert!(err5.to_string().contains("command `zramctl` failed"));
}

#[test]
fn helper_predicates_and_conversions_cover_all_branches() {
    assert!(canonical_boot_id("11111111-2222-4333-8444-555555555555"));
    assert!(!canonical_boot_id("invalid-boot-id"));

    assert!(canonical_invocation_id("0123456789abcdef0123456789abcdef"));
    assert!(!canonical_invocation_id("short"));
    assert!(!canonical_invocation_id("0123456789abcdef0123456789abcdeg"));

    assert_eq!(
        device_kind_for_path("/dev/nbd0"),
        Some(ManagedDeviceKind::Nbd)
    );
    assert_eq!(
        device_kind_for_path("/dev/ublkb0"),
        Some(ManagedDeviceKind::Ublk)
    );
    assert_eq!(
        device_kind_for_path("/dev/zram0"),
        Some(ManagedDeviceKind::Zram)
    );
    assert_eq!(device_kind_for_path("/dev/sda"), None);

    assert_eq!(command_label("test", &["a", "b"]), "test a b");
    assert_eq!(command_label("test", &[]), "test");
}

pub(in crate::cascade::cascade_io) fn proc_identity_and_socket_metadata_adapters_share_fail_closed_validation()
-> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(
        current_boot_id_with(|| { Ok(" 11111111-2222-4333-8444-555555555555\n".into()) })?,
        "11111111-2222-4333-8444-555555555555"
    );
    assert!(matches!(
        current_boot_id_with(|| Ok("not-a-boot-id".into())),
        Err(CascadeError::Precondition(_))
    ));
    assert!(matches!(
        current_boot_id_with(|| Err(std::io::Error::from(std::io::ErrorKind::NotFound))),
        Err(CascadeError::Io(_))
    ));

    let valid_id = b"INVOCATION_ID=0123456789ABCDEF0123456789ABCDEF\0".to_vec();
    assert_eq!(
        daemon_invocation_id_with(4321, |pid| {
            assert_eq!(pid, 4321);
            Ok(valid_id.clone())
        })?,
        "0123456789abcdef0123456789abcdef"
    );
    for bytes in [
            b"OTHER=value\0".to_vec(),
            b"INVOCATION_ID=short\0".to_vec(),
            b"INVOCATION_ID=0123456789abcdef0123456789abcdef\0INVOCATION_ID=0123456789abcdef0123456789abcdef\0".to_vec(),
            b"INVOCATION_ID=\xff\0".to_vec(),
        ] {
            assert!(matches!(
                daemon_invocation_id_with(4321, |_| Ok(bytes)),
                Err(CascadeError::Precondition(_))
            ));
        }
    assert!(matches!(
        daemon_invocation_id_with(4321, |_| {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        }),
        Err(CascadeError::Precondition(_))
    ));

    let fixture = TestDir::new();
    let socket_path = fixture.path.join("export.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&socket_path)?;
    let socket_metadata = fs::symlink_metadata(&socket_path)?;
    let socket = socket_identity_from_metadata(&socket_path, &socket_metadata, true)?;
    assert_eq!(socket.path, socket_path.to_string_lossy());
    assert_eq!(socket.inode, socket_metadata.ino());

    let regular_path = fixture.path.join("regular-file");
    fs::write(&regular_path, b"fixture")?;
    let regular_metadata = fs::symlink_metadata(&regular_path)?;
    assert!(matches!(
        socket_identity_from_metadata(&regular_path, &regular_metadata, true),
        Err(CascadeError::Precondition(_))
    ));
    let character_device = fs::metadata("/dev/null")?;
    assert!(matches!(
        validate_pinned_effect_metadata(&character_device, "1:3"),
        Err(CascadeError::Precondition(_))
    ));

    let symlink_path = fixture.path.join("socket-link");
    std::os::unix::fs::symlink(&socket_path, &symlink_path)?;
    let symlink_metadata = fs::symlink_metadata(&symlink_path)?;
    assert!(matches!(
        socket_identity_from_metadata(&symlink_path, &symlink_metadata, true),
        Err(CascadeError::Precondition(_))
    ));
    Ok(())
}

pub(in crate::cascade::cascade_io) fn system_command_runner_uses_bounded_capture_for_success_and_spawn_refusal()
-> Result<(), Box<dyn std::error::Error>> {
    let runner = SystemCommandRunner;
    assert_eq!(
        runner.run("/bin/printf", &["%s", "safe-fixture"])?,
        "safe-fixture"
    );
    assert_eq!(
        runner.run_bounded("/bin/true", &[], Duration::from_secs(1))?,
        ""
    );
    assert!(matches!(
        runner.run("/missing/ramshared-command-fixture", &[]),
        Err(CascadeError::Shell { .. })
    ));
    Ok(())
}

#[test]
fn legacy_runtime_records_are_compatible_validates_files() {
    let dir = TestDir::new();
    let paths = RuntimePaths::under(&dir.path);
    fs::create_dir_all(&paths.runtime_dir).expect("create runtime dir");

    let legacy_with_zram = LegacyMigrationPlan {
        nbd: SwapEntry {
            filename: "/dev/nbd0".into(),
            size_kb: 4_194_300,
            used_kb: 0,
            priority: 50,
        },
        zram: Some(SwapEntry {
            filename: "/dev/zram0".into(),
            size_kb: 1_048_572,
            used_kb: 0,
            priority: 100,
        }),
    };
    let daemon = LegacyDaemonIdentity {
        pid: 4242,
        instance_id: "4242-100".into(),
        executable: PathBuf::from("/usr/local/bin/ramsharedd"),
        proof: LegacyDaemonProof::BinaryMatch,
    };

    // None present is compatible (optional legacy records)
    assert!(legacy_runtime_records_are_compatible(&paths, &legacy_with_zram, &daemon).is_ok());

    // When a record is written by non-root, legacy_runtime_record_value rejects it (not root-owned)
    fs::write(&paths.pid_file, "4242\n").expect("write pid");
    if rustix::process::getuid().as_raw() == 0 {
        fs::write(&paths.swap_dev_file, "/dev/nbd0\n").expect("write swap");
        fs::write(&paths.zram_dev_file, "/dev/zram0\n").expect("write zram");
        assert!(legacy_runtime_records_are_compatible(&paths, &legacy_with_zram, &daemon).is_ok());

        // Mismatched PID
        fs::write(&paths.pid_file, "9999\n").expect("write bad pid");
        assert!(legacy_runtime_records_are_compatible(&paths, &legacy_with_zram, &daemon).is_err());
    } else {
        assert!(legacy_runtime_records_are_compatible(&paths, &legacy_with_zram, &daemon).is_err());
    }
}

#[test]
fn process_socket_and_listener_probes_cover_live_and_dead_pids() {
    let my_pid = std::process::id();
    let inodes = process_socket_inodes(my_pid);
    assert!(inodes.is_ok());

    assert!(process_socket_inodes(999_999_999).is_err());

    let listener_inodes = legacy_listener_socket_inodes();
    assert!(listener_inodes.is_ok());

    let owns = process_owns_legacy_listener(my_pid);
    assert!(owns.is_ok());
}

#[test]
fn read_legacy_daemon_observation_covers_current_process_and_refuses_missing() {
    let my_pid = std::process::id();
    let obs = read_legacy_daemon_observation(my_pid).expect("current process observation");
    assert_eq!(obs.uid, rustix::process::getuid().as_raw());
    assert!(obs.executable_sha256.is_some());
    assert!(!obs.arguments.is_empty());

    assert!(read_legacy_daemon_observation(999_999_999).is_err());
}

#[test]
fn revalidate_and_stop_legacy_daemon_refuse_unverifiable_daemons() {
    let fake_dead = LegacyDaemonIdentity {
        pid: 999_999_999,
        instance_id: "999999999-100".into(),
        executable: PathBuf::from("/bin/sh"),
        proof: LegacyDaemonProof::BinaryMatch,
    };
    assert!(revalidate_legacy_daemon(&fake_dead).is_err());
    assert!(stop_legacy_daemon(&fake_dead).is_err());

    let fake_legacy = LegacyDaemonIdentity {
        pid: 999_999_999,
        instance_id: "999999999-100".into(),
        executable: PathBuf::from("/bin/sh"),
        proof: LegacyDaemonProof::LegacyPathHashMatch {
            legacy_executable: PathBuf::from("/nonexistent/ramsharedd"),
            executable_sha256: [0u8; 32],
            slice_mb: 4096,
        },
    };
    assert!(revalidate_legacy_daemon(&fake_legacy).is_err());
}

#[test]
fn revalidate_legacy_bound_device_checks_exact_identity() {
    let nbd = bound_device_fixture("/dev/nbd0", ManagedDeviceKind::Nbd);
    assert!(revalidate_legacy_bound_device(&nbd).is_ok());

    let mut mutated = nbd;
    mutated.dev_t = "99:99".into();
    assert!(revalidate_legacy_bound_device(&mutated).is_err());
}
