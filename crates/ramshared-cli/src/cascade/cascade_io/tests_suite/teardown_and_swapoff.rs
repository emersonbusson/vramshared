// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/tests_suite/teardown_and_swapoff.rs
#![allow(clippy::expect_used)]

use super::*;

#[test]
fn legacy_migration_executor_preserves_swapoff_first_order() {
    struct RecordingExecutor(RefCell<Vec<String>>);

    impl LegacyMigrationExecutor for RecordingExecutor {
        fn swapoff(&self, device: &BoundDeviceIdentity) -> Result<(), CascadeError> {
            self.0.borrow_mut().push(format!("swapoff {}", device.path));
            Ok(())
        }

        fn reset_zram(&self, device: &BoundDeviceIdentity) -> Result<(), CascadeError> {
            self.0
                .borrow_mut()
                .push(format!("reset-zram {}", device.path));
            Ok(())
        }

        fn disconnect_nbd(&self, device: &BoundDeviceIdentity) -> Result<(), CascadeError> {
            self.0
                .borrow_mut()
                .push(format!("disconnect-nbd {}", device.path));
            Ok(())
        }

        fn stop_daemon(&self) -> Result<(), CascadeError> {
            self.0.borrow_mut().push("stop-daemon".into());
            Ok(())
        }
    }

    let nbd = bound_device_fixture("/dev/nbd0", ManagedDeviceKind::Nbd);
    let zram = bound_device_fixture("/dev/zram0", ManagedDeviceKind::Zram);
    let legacy = LegacyMigrationPlan {
        nbd: SwapEntry {
            filename: "/dev/nbd0".into(),
            size_kb: 4_194_300,
            used_kb: 0,
            priority: 50,
        },
        zram: Some(SwapEntry {
            filename: "/dev/zram0".into(),
            size_kb: 1_048_572,
            used_kb: 4_140,
            priority: 100,
        }),
    };
    let transition = plan_legacy_migration_transition(&legacy, nbd, Some(zram))
        .expect("bound legacy transition");
    let executor = RecordingExecutor(RefCell::new(Vec::new()));

    execute_legacy_migration_transition(&transition, &executor).expect("ordered legacy transition");

    assert_eq!(
        executor.0.into_inner(),
        vec![
            "swapoff /dev/zram0",
            "swapoff /dev/nbd0",
            "reset-zram /dev/zram0",
            "disconnect-nbd /dev/nbd0",
            "stop-daemon",
        ]
    );
    assert!(legacy_drain_budget_is_sufficient(
        1_048_572,
        2 * 1024 * 1024 * 1024,
        524_288
    ));
    assert!(!legacy_drain_budget_is_sufficient(
        1_048_572,
        1_500 * 1024 * 1024,
        524_288
    ));
}

#[test]
fn legacy_migration_executor_stops_on_first_refusal() {
    struct RefusingExecutor(RefCell<Vec<String>>);

    impl LegacyMigrationExecutor for RefusingExecutor {
        fn swapoff(&self, device: &BoundDeviceIdentity) -> Result<(), CascadeError> {
            self.0.borrow_mut().push(format!("swapoff {}", device.path));
            Err(CascadeError::UnsafeContainment(
                "manufactured swapoff refusal".into(),
            ))
        }

        fn reset_zram(&self, _device: &BoundDeviceIdentity) -> Result<(), CascadeError> {
            self.0.borrow_mut().push("reset-zram".into());
            Ok(())
        }

        fn disconnect_nbd(&self, _device: &BoundDeviceIdentity) -> Result<(), CascadeError> {
            self.0.borrow_mut().push("disconnect-nbd".into());
            Ok(())
        }

        fn stop_daemon(&self) -> Result<(), CascadeError> {
            self.0.borrow_mut().push("stop-daemon".into());
            Ok(())
        }
    }

    let nbd = bound_device_fixture("/dev/nbd0", ManagedDeviceKind::Nbd);
    let legacy = LegacyMigrationPlan {
        nbd: SwapEntry {
            filename: "/dev/nbd0".into(),
            size_kb: 4_194_300,
            used_kb: 0,
            priority: 50,
        },
        zram: None,
    };
    let transition =
        plan_legacy_migration_transition(&legacy, nbd, None).expect("bound legacy transition");
    let executor = RefusingExecutor(RefCell::new(Vec::new()));

    let error = error_from(
        execute_legacy_migration_transition(&transition, &executor),
        "first legacy effect must stop the transition",
    );

    assert!(error.to_string().contains("manufactured swapoff refusal"));
    assert_eq!(executor.0.into_inner(), vec!["swapoff /dev/nbd0"]);
}

#[test]
fn legacy_migration_rejects_replaced_daemon_identity() {
    let identity = LegacyDaemonIdentity {
        pid: 42,
        instance_id: "42-100".into(),
        executable: PathBuf::from("/opt/ramshared/bin/ramsharedd"),
        proof: LegacyDaemonProof::BinaryMatch,
    };
    let observation = LegacyDaemonObservation {
        uid: 0,
        executable_link: PathBuf::from("/opt/ramshared/bin/ramsharedd"),
        canonical_executable: Some(PathBuf::from("/opt/ramshared/bin/ramsharedd")),
        executable_sha256: None,
        arguments: Vec::new(),
        owns_legacy_listener: false,
    };

    assert!(legacy_daemon_identity_matches(
        &identity,
        Some("42-100"),
        &observation,
    ));
    assert!(!legacy_daemon_identity_matches(
        &identity,
        Some("42-101"),
        &observation,
    ));
    assert!(!legacy_daemon_identity_matches(
        &identity,
        Some("42-100"),
        &LegacyDaemonObservation {
            canonical_executable: Some(PathBuf::from("/opt/ramshared/bin/replaced")),
            ..observation
        },
    ));
}

#[test]
fn legacy_daemon_proc_enumeration_uses_injected_fixture_root() {
    let fixture = TestDir::new();
    for (pid, comm) in [
        ("123", " ramsharedd \n"),
        ("124", "other\n"),
        ("0", "ramsharedd\n"),
    ] {
        let process = fixture.path.join(pid);
        fs::create_dir_all(&process).expect("create synthetic process directory");
        fs::write(process.join("comm"), comm).expect("write synthetic process comm");
    }
    fs::create_dir(fixture.path.join("not-a-pid")).expect("create non-PID entry");
    let observed = std::cell::RefCell::new(Vec::new());
    let observations = legacy_daemon_observations_at(
        &fixture.path,
        |pid| fs::read_to_string(fixture.path.join(pid.to_string()).join("comm")),
        |pid| {
            observed.borrow_mut().push(pid);
            Ok(LegacyDaemonObservation {
                uid: 0,
                executable_link: PathBuf::from("/fixture/ramsharedd"),
                canonical_executable: Some(PathBuf::from("/fixture/ramsharedd")),
                executable_sha256: None,
                arguments: Vec::new(),
                owns_legacy_listener: false,
            })
        },
    )
    .expect("synthetic proc tree enumerates without touching host /proc");
    assert_eq!(*observed.borrow(), [123]);
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].0, 123);
    assert!(
        legacy_daemon_observations_at(
            &fixture.path.join("missing"),
            |_| unreachable!("unreadable root fails before reading comm"),
            |_| unreachable!("unreadable root fails before process observation"),
        )
        .is_err()
    );
    assert!(
        legacy_daemon_observations_at(
            &fixture.path,
            |pid| fs::read_to_string(fixture.path.join(pid.to_string()).join("comm")),
            |_| Err(CascadeError::Precondition(
                "fixture observation failure".into()
            )),
        )
        .is_err()
    );
}

#[test]
fn legacy_daemon_selection_requires_one_proven_process_and_instance() {
    let executable = PathBuf::from("/fixture/bin/ramsharedd");
    let observation = |path: &str| LegacyDaemonObservation {
        uid: 0,
        executable_link: PathBuf::from(path),
        canonical_executable: Some(PathBuf::from(path)),
        executable_sha256: None,
        arguments: Vec::new(),
        owns_legacy_listener: false,
    };
    let binary_match = (
        42,
        "ramsharedd".into(),
        observation("/fixture/bin/ramsharedd"),
    );

    let selected = select_legacy_daemon_identity(
        &executable,
        None,
        [0x5a; 32],
        4096,
        [
            (
                0,
                "ramsharedd".into(),
                observation("/fixture/bin/ramsharedd"),
            ),
            (7, "other".into(), observation("/fixture/bin/foreign")),
            binary_match.clone(),
        ],
        |path| Some(path.to_path_buf()),
        |pid| Some(format!("{pid}-100")),
    )
    .expect("one exact binary and invocation identity are admitted");
    assert_eq!(selected.pid, 42);
    assert_eq!(selected.proof, LegacyDaemonProof::BinaryMatch);

    let no_match = select_legacy_daemon_identity(
        &executable,
        None,
        [0x5a; 32],
        4096,
        [(7, "other".into(), observation("/fixture/bin/foreign"))],
        |path| Some(path.to_path_buf()),
        |pid| Some(format!("{pid}-100")),
    )
    .expect_err("unrelated processes are ignored, leaving no proven daemon");
    assert!(no_match.to_string().contains("requires one BINARY_MATCH"));

    let foreign = select_legacy_daemon_identity(
        &executable,
        None,
        [0x5a; 32],
        4096,
        [(43, "ramsharedd".into(), observation("/fixture/bin/foreign"))],
        |path| Some(path.to_path_buf()),
        |pid| Some(format!("{pid}-100")),
    )
    .expect_err("a foreign daemon blocks migration");
    assert!(foreign.to_string().contains("foreign ramsharedd"));

    let multiple = select_legacy_daemon_identity(
        &executable,
        None,
        [0x5a; 32],
        4096,
        [
            binary_match.clone(),
            (
                43,
                "ramsharedd".into(),
                observation("/fixture/bin/ramsharedd"),
            ),
        ],
        |path| Some(path.to_path_buf()),
        |pid| Some(format!("{pid}-100")),
    )
    .expect_err("multiple proven daemon processes are ambiguous");
    assert!(multiple.to_string().contains("multiple BINARY_MATCH"));

    let missing_instance = select_legacy_daemon_identity(
        &executable,
        None,
        [0x5a; 32],
        4096,
        [binary_match],
        |path| Some(path.to_path_buf()),
        |_| None,
    )
    .expect_err("identity loss refuses an otherwise matching process");
    assert!(missing_instance.to_string().contains("start identity"));
}

#[test]
fn legacy_replaced_daemon_requires_bound_root_listener() {
    let identity = LegacyDaemonIdentity {
        pid: 42,
        instance_id: "42-100".into(),
        executable: PathBuf::from("/usr/local/bin/ramsharedd"),
        proof: LegacyDaemonProof::ReplacedBinary { slice_mb: 4096 },
    };
    let observation = LegacyDaemonObservation {
        uid: 0,
        executable_link: PathBuf::from("/usr/local/bin/ramsharedd (deleted)"),
        canonical_executable: None,
        executable_sha256: None,
        arguments: vec![
            "/usr/local/bin/ramsharedd".into(),
            "--slices".into(),
            "1".into(),
            "--slice-mb".into(),
            "4096".into(),
            "--listen-nbd".into(),
            "127.0.0.1:10809".into(),
        ],
        owns_legacy_listener: true,
    };

    assert!(legacy_daemon_identity_matches(
        &identity,
        Some("42-100"),
        &observation,
    ));
    assert!(!legacy_daemon_identity_matches(
        &identity,
        Some("42-100"),
        &LegacyDaemonObservation {
            owns_legacy_listener: false,
            ..observation.clone()
        },
    ));
    assert!(!legacy_daemon_identity_matches(
        &identity,
        Some("42-100"),
        &LegacyDaemonObservation {
            uid: 1000,
            ..observation.clone()
        },
    ));
    assert!(!legacy_daemon_identity_matches(
        &identity,
        Some("42-100"),
        &LegacyDaemonObservation {
            arguments: vec!["/usr/local/bin/ramsharedd".into()],
            ..observation
        },
    ));
}

#[test]
fn legacy_regular_daemon_requires_the_sealed_binary_hash_and_listener() {
    let digest = [0x5a; 32];
    let legacy_executable = PathBuf::from("/usr/local/bin/ramsharedd");
    let observation = LegacyDaemonObservation {
        uid: 0,
        executable_link: legacy_executable.clone(),
        canonical_executable: Some(legacy_executable.clone()),
        executable_sha256: Some(digest),
        arguments: vec![
            "/usr/local/bin/ramsharedd".into(),
            "--backend".into(),
            "auto".into(),
            "--slices".into(),
            "1".into(),
            "--slice-mb".into(),
            "4096".into(),
            "--listen-nbd".into(),
            "127.0.0.1:10809".into(),
        ],
        owns_legacy_listener: true,
    };

    assert!(legacy_regular_daemon_is_bound(
        &observation,
        &legacy_executable,
        &digest,
        4096,
    ));
    assert!(!legacy_regular_daemon_is_bound(
        &LegacyDaemonObservation {
            executable_sha256: Some([0xa5; 32]),
            ..observation
        },
        &legacy_executable,
        &digest,
        4096,
    ));
}

#[test]
fn nbd_device_permits_confirmed_absent_kernel_owner() {
    assert_eq!(
        nbd_owner_policy_for_kind(ManagedDeviceKind::Nbd),
        NbdOwnerPolicy::PermitAbsent
    );
    assert_eq!(
        nbd_owner_policy_for_kind(ManagedDeviceKind::Zram),
        NbdOwnerPolicy::RequireLive
    );
}

#[test]
fn retire_legacy_runtime_records_removes_stale_legacy_evidence() {
    let dir = TestDir::new();
    let paths = RuntimePaths::under(&dir.path);
    fs::create_dir_all(&paths.runtime_dir).expect("create runtime dir fixture");
    fs::write(&paths.pid_file, "123\n").expect("write pid fixture");
    fs::write(&paths.swap_dev_file, "/dev/nbd0\n").expect("write swap fixture");
    fs::write(&paths.zram_dev_file, "/dev/zram0\n").expect("write zram fixture");
    fs::write(&paths.capacity_status_file, "1\n").expect("write capacity fixture");
    fs::write(&paths.socket, "sock\n").expect("write socket fixture");

    assert!(paths.pid_file.exists());
    assert!(paths.swap_dev_file.exists());
    assert!(paths.zram_dev_file.exists());
    assert!(paths.capacity_status_file.exists());
    assert!(paths.socket.exists());

    retire_legacy_runtime_records(&paths);

    assert!(!paths.pid_file.exists());
    assert!(!paths.swap_dev_file.exists());
    assert!(!paths.zram_dev_file.exists());
    assert!(!paths.capacity_status_file.exists());
    assert!(!paths.socket.exists());
}

#[test]
fn legacy_nbd_owner_policy_accepts_only_confirmed_absence() {
    let live =
        nbd_kernel_owner_identity(Some("5939-100".into()), false, NbdOwnerPolicy::RequireLive)
            .expect("live NBD owner must remain admissible");
    assert_eq!(live.as_deref(), Some("5939-100"));

    assert_eq!(
        nbd_kernel_owner_identity(None, true, NbdOwnerPolicy::PermitAbsent)
            .expect("a confirmed absent owner is legacy-only admissible"),
        None
    );
    assert!(nbd_kernel_owner_identity(None, true, NbdOwnerPolicy::RequireLive).is_err());
    assert!(nbd_kernel_owner_identity(None, false, NbdOwnerPolicy::PermitAbsent).is_err());
}

#[test]
fn legacy_runtime_records_require_exact_legacy_values() {
    assert!(legacy_runtime_record_values_match(
        Some("527\n"),
        Some("/dev/nbd0\n"),
        Some("/dev/zram0\n"),
        527,
        "/dev/nbd0",
        Some("/dev/zram0"),
    ));
    assert!(legacy_runtime_record_values_match(
        None,
        None,
        None,
        527,
        "/dev/nbd0",
        Some("/dev/zram0"),
    ));
    assert!(!legacy_runtime_record_values_match(
        Some("528"),
        Some("/dev/nbd0"),
        Some("/dev/zram0"),
        527,
        "/dev/nbd0",
        Some("/dev/zram0"),
    ));
    assert!(!legacy_runtime_record_values_match(
        Some("527"),
        Some("/dev/nbd1"),
        Some("/dev/zram0"),
        527,
        "/dev/nbd0",
        Some("/dev/zram0"),
    ));
    assert!(!legacy_runtime_record_values_match(
        Some("527"),
        Some("/dev/nbd0"),
        Some("/dev/zram0"),
        527,
        "/dev/nbd0",
        None,
    ));
}

#[test]
fn deleted_executable_path_handles_deleted_and_normal_links() {
    assert_eq!(
        deleted_executable_path(Path::new("/opt/ramshared/bin/ramsharedd (deleted)")),
        Some(PathBuf::from("/opt/ramshared/bin/ramsharedd"))
    );
    assert_eq!(
        deleted_executable_path(Path::new("/opt/ramshared/bin/ramsharedd")),
        None
    );
    assert_eq!(deleted_executable_path(Path::new("ramsharedd")), None);
}

#[test]
fn sha256_file_hashes_exact_content_and_reports_missing_file() {
    let dir = TestDir::new();
    let file_path = dir.path.join("test_bin");
    let content = b"sealed binary payload for test";
    fs::write(&file_path, content).expect("write temp binary");

    let hash = sha256_file(&file_path).expect("hash regular file");
    let expected: [u8; 32] = Sha256::digest(content).into();
    assert_eq!(hash, expected);

    let missing = dir.path.join("nonexistent_bin");
    assert!(sha256_file(&missing).is_err());
}

#[test]
fn legacy_runtime_record_value_validates_file_shape_and_content() {
    let dir = TestDir::new();
    let missing = dir.path.join("missing_record");
    assert_eq!(
        legacy_runtime_record_value(&missing).expect("missing record"),
        None
    );

    let valid = dir.path.join("valid_record");
    fs::write(&valid, "42\n").expect("write valid record");
    let result = legacy_runtime_record_value(&valid);
    if rustix::process::getuid().as_raw() == 0 {
        assert_eq!(result.expect("valid record"), Some("42".into()));
    } else {
        assert!(result.is_err());
    }

    let malformed = dir.path.join("malformed_record");
    fs::write(&malformed, "\n").expect("write empty record");
    assert!(legacy_runtime_record_value(&malformed).is_err());
}

#[test]
fn plan_legacy_migration_transition_refuses_invalid_topologies() {
    let nbd = bound_device_fixture("/dev/nbd0", ManagedDeviceKind::Nbd);
    let zram = bound_device_fixture("/dev/zram0", ManagedDeviceKind::Zram);
    let wrong_path = bound_device_fixture("/dev/nbd1", ManagedDeviceKind::Nbd);

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
            used_kb: 4_140,
            priority: 100,
        }),
    };

    assert!(
        plan_legacy_migration_transition(&legacy_with_zram, zram.clone(), Some(zram.clone()))
            .is_err()
    );
    assert!(
        plan_legacy_migration_transition(&legacy_with_zram, wrong_path, Some(zram.clone()))
            .is_err()
    );
    assert!(plan_legacy_migration_transition(&legacy_with_zram, nbd.clone(), None).is_err());

    let legacy_nbd_only = LegacyMigrationPlan {
        nbd: SwapEntry {
            filename: "/dev/nbd0".into(),
            size_kb: 4_194_300,
            used_kb: 0,
            priority: 50,
        },
        zram: None,
    };
    assert!(plan_legacy_migration_transition(&legacy_nbd_only, nbd.clone(), Some(zram)).is_err());

    let plan = plan_legacy_migration_transition(&legacy_nbd_only, nbd, None)
        .expect("plan nbd-only transition");
    assert_eq!(plan.actions.len(), 3);
}

#[test]
fn legacy_regular_daemon_proof_validates_matching_and_refuses_mismatch() {
    let digest = [0x7c; 32];
    let legacy_executable = PathBuf::from("/usr/local/bin/ramsharedd");
    let observation = LegacyDaemonObservation {
        uid: 0,
        executable_link: legacy_executable.clone(),
        canonical_executable: Some(legacy_executable.clone()),
        executable_sha256: Some(digest),
        arguments: vec![
            "/usr/local/bin/ramsharedd".into(),
            "--backend".into(),
            "auto".into(),
            "--slices".into(),
            "1".into(),
            "--slice-mb".into(),
            "4096".into(),
            "--listen-nbd".into(),
            "127.0.0.1:10809".into(),
        ],
        owns_legacy_listener: true,
    };

    assert!(
        legacy_regular_daemon_proof(&observation, Some(&legacy_executable), &digest, 4096)
            .is_some()
    );

    assert!(legacy_regular_daemon_proof(&observation, None, &digest, 4096).is_none());

    assert!(
        legacy_regular_daemon_proof(&observation, Some(&legacy_executable), &[0x11; 32], 4096)
            .is_none()
    );
}

#[test]
fn canonical_root_owned_daemon_refuses_nonexistent_and_unsealed() {
    let dir = TestDir::new();
    let nonexistent = dir.path.join("missing_daemon");
    assert!(canonical_root_owned_daemon(nonexistent.to_str().expect("valid utf-8")).is_err());

    let regular = dir.path.join("unsealed_daemon");
    fs::write(&regular, b"binary content").expect("write daemon test file");
    let result = canonical_root_owned_daemon(regular.to_str().expect("valid utf-8"));
    if rustix::process::getuid().as_raw() != 0 {
        assert!(result.is_err());
    }
}
