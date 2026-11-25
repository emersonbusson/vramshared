// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/tests_suite/legacy_migration_and_guards.rs
#![allow(clippy::expect_used)]

use super::*;

#[test]
// TestName: legacy_daemon_identity_matches_each_proof_and_refuses_mismatch
fn legacy_daemon_identity_matches_each_proof_and_refuses_mismatch() {
    let executable = PathBuf::from("/usr/local/bin/ramsharedd");
    let observation = LegacyDaemonObservation {
        uid: 0,
        executable_link: executable.clone(),
        canonical_executable: Some(executable.clone()),
        executable_sha256: Some([7u8; 32]),
        arguments: vec!["ramsharedd".into()],
        owns_legacy_listener: true,
    };
    let identity = |proof: LegacyDaemonProof| LegacyDaemonIdentity {
        pid: 4242,
        instance_id: "4242-100".into(),
        executable: executable.clone(),
        proof,
    };
    assert!(legacy_daemon_identity_matches(
        &identity(LegacyDaemonProof::BinaryMatch),
        Some("4242-100"),
        &observation
    ));
    assert!(
        !legacy_daemon_identity_matches(
            &identity(LegacyDaemonProof::BinaryMatch),
            Some("4242-101"),
            &observation
        ),
        "a different instance id must not match"
    );
    assert!(
        !legacy_daemon_identity_matches(
            &identity(LegacyDaemonProof::BinaryMatch),
            None,
            &observation
        ),
        "a missing instance id must not match"
    );
    let mut other = observation.clone();
    other.canonical_executable = Some(PathBuf::from("/tmp/other"));
    assert!(
        !legacy_daemon_identity_matches(
            &identity(LegacyDaemonProof::BinaryMatch),
            Some("4242-100"),
            &other
        ),
        "a different canonical executable must not match a BinaryMatch proof"
    );
}

#[test]
// TestName: validate_lifecycle_binding_refuses_invalid_schema_and_identity
fn validate_lifecycle_binding_refuses_invalid_schema_and_identity() {
    let nbd = bound_device_fixture("/dev/nbd0", ManagedDeviceKind::Nbd);
    let valid = lifecycle_binding_fixture(vec![nbd.clone()]);
    validate_lifecycle_binding(&valid).expect("the fixture binding is well formed");
    let cases: Vec<(&str, LifecycleBinding)> = vec![
        (
            "schema_version",
            LifecycleBinding {
                schema_version: 2,
                ..valid.clone()
            },
        ),
        (
            "boot_id",
            LifecycleBinding {
                boot_id: "nope".into(),
                ..valid.clone()
            },
        ),
        (
            "invocation",
            LifecycleBinding {
                daemon_invocation_id: "xyz".into(),
                ..valid.clone()
            },
        ),
        (
            "pid",
            LifecycleBinding {
                daemon_pid: 0,
                ..valid.clone()
            },
        ),
        (
            "ticks",
            LifecycleBinding {
                daemon_start_ticks: 0,
                ..valid.clone()
            },
        ),
        (
            "instance",
            LifecycleBinding {
                daemon_instance_id: "9-9".into(),
                ..valid.clone()
            },
        ),
        (
            "socket.path",
            LifecycleBinding {
                export_socket: BoundSocketIdentity {
                    path: String::new(),
                    filesystem_dev: 1,
                    inode: 1,
                },
                ..valid.clone()
            },
        ),
        (
            "socket.inode",
            LifecycleBinding {
                export_socket: BoundSocketIdentity {
                    path: "/run/x".into(),
                    filesystem_dev: 1,
                    inode: 0,
                },
                ..valid.clone()
            },
        ),
        (
            "origin.partuuid",
            LifecycleBinding {
                origin: BoundOriginIdentity {
                    partuuid: "bad".into(),
                    ..valid.origin.clone()
                },
                ..valid.clone()
            },
        ),
        (
            "origin.dev_t",
            LifecycleBinding {
                origin: BoundOriginIdentity {
                    partition_dev_t: "x".into(),
                    ..valid.origin.clone()
                },
                ..valid.clone()
            },
        ),
        (
            "origin.sha",
            LifecycleBinding {
                origin: BoundOriginIdentity {
                    host_manifest_sha256: "zz".into(),
                    ..valid.origin.clone()
                },
                ..valid.clone()
            },
        ),
    ];
    for (label, binding) in cases {
        let error = validate_lifecycle_binding(&binding)
            .expect_err(&format!("lifecycle binding must refuse {label}"));
        assert!(
            error.to_string().contains("invalid schema or identity"),
            "{label} produced the wrong refusal: {error}"
        );
    }
    let mut bad_device = nbd.clone();
    bad_device.dev_t = "1:1".into();
    let error = validate_lifecycle_binding(&lifecycle_binding_fixture(vec![bad_device]))
        .expect_err("a device whose dev_t disagrees with sysfs must be refused");
    assert!(error.to_string().contains("cardinality or identity"));
}

#[test]
// TestName: bound_device_record_round_trip_refuses_unsealed_shapes
fn bound_device_record_round_trip_refuses_unsealed_shapes() {
    let dir = TestDir::new();
    let path = dir.path.join("nbd-record.json");
    let nbd = bound_device_fixture("/dev/nbd0", ManagedDeviceKind::Nbd);
    write_bound_device_record(&path, &nbd).expect("write a sealed device record");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("seal the record");
    assert_eq!(
        read_bound_device_record(&path).expect("read the sealed record"),
        nbd
    );

    let world = dir.path.join("world.json");
    write_bound_device_record(&world, &nbd).expect("write the world-readable record");
    fs::set_permissions(&world, fs::Permissions::from_mode(0o644)).expect("widen the record");
    let error =
        read_bound_device_record(&world).expect_err("a world-readable record must be refused");
    assert!(error.to_string().contains("not a sealed regular file"));

    let empty = dir.path.join("empty.json");
    fs::write(&empty, b"").expect("write an empty record");
    fs::set_permissions(&empty, fs::Permissions::from_mode(0o600)).expect("seal the empty record");
    assert!(read_bound_device_record(&empty).is_err());

    let link = dir.path.join("link.json");
    std::os::unix::fs::symlink(&path, &link).expect("symlink the record");
    assert!(read_bound_device_record(&link).is_err());

    let garbage = dir.path.join("garbage.json");
    fs::write(&garbage, b"{\"kind\":\"nbd\"}").expect("write a malformed record");
    fs::set_permissions(&garbage, fs::Permissions::from_mode(0o600))
        .expect("seal the malformed record");
    assert!(read_bound_device_record(&garbage).is_err());
}

#[test]
// TestName: nbd_path_record_round_trip_refuses_non_nbd_targets
fn nbd_path_record_round_trip_refuses_non_nbd_targets() {
    let dir = TestDir::new();
    let path = dir.path.join("nbd-path");
    write_nbd_path_record(&path, "/dev/nbd0").expect("publish an exact NBD path record");
    assert_eq!(
        read_nbd_path_record(&path).expect("read the NBD path record"),
        "/dev/nbd0"
    );

    assert!(
        write_nbd_path_record(&path, "/dev/zram0").is_err(),
        "a zram target is not an NBD path"
    );
    assert!(
        write_nbd_path_record(&path, "/dev/nbd0/../nbd0").is_err(),
        "a non-canonical target is refused"
    );

    let oversized = dir.path.join("oversized");
    fs::write(&oversized, vec![b'x'; 5000]).expect("write an oversized record");
    assert!(read_nbd_path_record(&oversized).is_err());

    let foreign = dir.path.join("foreign");
    fs::write(&foreign, b"/dev/zram0").expect("write a foreign device record");
    assert!(read_nbd_path_record(&foreign).is_err());
}

#[test]
// TestName: forensics_markers_arm_disarm_and_remove_idempotently
fn forensics_markers_arm_disarm_and_remove_idempotently() {
    let dir = TestDir::new();
    let paths = RuntimePaths::under(&dir.path);
    let marker = &paths.forensics_markers[0];
    assert!(!marker.exists(), "the fixture starts unarmed");
    arm_forensics_at(&paths);
    assert!(marker.is_file(), "arming must publish the marker");
    arm_forensics_at(&paths);
    assert!(
        marker.is_file(),
        "re-arming must not destroy an existing regular marker"
    );

    fs::write(marker, b"").expect("empty the marker");
    fs::remove_file(marker).expect("remove the marker");
    std::os::unix::fs::symlink("/tmp", marker).expect("plant a symlink marker");
    arm_forensics_at(&paths);
    assert!(
        marker
            .symlink_metadata()
            .expect("stat the planted marker")
            .file_type()
            .is_symlink(),
        "arming must refuse to adopt a non-regular marker"
    );
    fs::remove_file(marker).expect("remove the planted symlink");

    arm_forensics_at(&paths);
    assert!(marker.is_file());
    disarm_forensics_at(&paths);
    assert!(!marker.exists(), "disarming must remove the marker");
    disarm_forensics_at(&paths);
    remove_runtime_file(marker);
    remove_runtime_file(dir.path.join("never-existed"));
}

#[test]
// TestName: boot_and_invocation_identity_readers_refuse_malformed_values
fn boot_and_invocation_identity_readers_refuse_malformed_values() {
    assert!(current_boot_id_with(|| Ok("11111111-2222-4333-8444-555555555555\n".into())).is_ok());
    assert!(current_boot_id_with(|| Ok("not-a-uuid".into())).is_err());
    assert!(current_boot_id_with(|| Err(std::io::Error::other("denied"))).is_err());

    let env = |body: &[u8]| {
        let owned = body.to_vec();
        daemon_invocation_id_with(1, move |_| Ok(owned))
    };
    assert!(env(b"INVOCATION_ID=0123456789abcdef0123456789abcdef\0").is_ok());
    assert!(
        env(b"PATH=/usr/bin\0").is_err(),
        "a daemon without INVOCATION_ID is refused"
    );
    assert!(
            env(b"INVOCATION_ID=0123456789abcdef0123456789abcdef\0INVOCATION_ID=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\0").is_err(),
            "two InvocationIDs are ambiguous and refused"
        );
    assert!(
        env(b"INVOCATION_ID=xyz\0").is_err(),
        "a non-canonical InvocationID is refused"
    );
    assert!(daemon_invocation_id_with(1, |_| Err(std::io::Error::other("denied"))).is_err());
}

#[test]
// TestName: sysfs_device_number_and_socket_identity_refuse_unsafe_shapes
fn sysfs_device_number_and_socket_identity_refuse_unsafe_shapes() {
    assert_eq!(
        parse_sysfs_device_number(" 8:33 \n").expect("canonical dev_t"),
        "8:33"
    );
    assert!(parse_sysfs_device_number("8").is_err());
    assert!(parse_sysfs_device_number("x:y").is_err());
    assert!(parse_sysfs_device_number("").is_err());

    let dir = TestDir::new();
    let regular = dir.path.join("regular");
    fs::write(&regular, b"x").expect("write a regular file");
    let metadata = fs::symlink_metadata(&regular).expect("stat the regular file");
    assert!(socket_identity_from_metadata(&regular, &metadata, true).is_err());
    assert_eq!(
        socket_identity_from_metadata(&regular, &metadata, false)
            .expect("a non-socket is accepted when the caller does not require one")
            .inode,
        metadata.ino()
    );

    let target = dir.path.join("target");
    let link = dir.path.join("link");
    fs::write(&target, b"x").expect("write the symlink target");
    std::os::unix::fs::symlink(&target, &link).expect("create the symlink");
    let link_meta = fs::symlink_metadata(&link).expect("stat the symlink");
    assert!(socket_identity_from_metadata(&link, &link_meta, false).is_err());
}

#[test]
// TestName: transport_nbd_and_ublk_policy_fails_closed_on_wsl2
fn transport_nbd_and_ublk_policy_fails_closed_on_wsl2() {
    assert!(
        check_transport_for(Transport::Auto, true, false)
            .expect_err("unresolved auto transport must not pass admission")
            .to_string()
            .contains("must be resolved")
    );
    for running_on_wsl2 in [true, false] {
        let error = check_transport_for(Transport::Ublk, running_on_wsl2, false)
            .expect_err("ublk is fail-closed on every platform until the full wire-up exists");
        assert!(
            error.to_string().contains("transport ublk"),
            "the refusal must name the transport (wsl2={running_on_wsl2}): {error}"
        );
    }
    let wsl2_error = check_transport_for(Transport::Nbd, true, false)
        .expect_err("NBD swap on WSL2 needs the operator's acknowledgment");
    assert!(wsl2_error.to_string().contains("NBD"));
    assert!(
        wsl2_error.to_string().contains("--accept-wsl2-nbd"),
        "the refusal names the acknowledgment: {wsl2_error}"
    );
    // The explicit acknowledgment admits a measured WSL2 host; the refusal never weakens the off-WSL2 path.
    check_transport_for(Transport::Nbd, true, true)
        .expect("the acknowledgment admits NBD on a measured WSL2 host");
    check_transport_for(Transport::Nbd, false, false).expect("NBD remains available off WSL2");
}

#[test]
// TestName: legacy_daemon_proofs_bind_exact_arguments_and_refuse_drift
fn legacy_daemon_proofs_bind_exact_arguments_and_refuse_drift() {
    let legacy = PathBuf::from("/usr/local/bin/ramsharedd");
    let hash = [9u8; 32];
    let arguments = |extra: &[&str]| {
        let mut args = vec![
            legacy.to_string_lossy().into_owned(),
            "--slices".into(),
            "1".into(),
            "--slice-mb".into(),
            "64".into(),
            "--listen-nbd".into(),
            "127.0.0.1:10809".into(),
        ];
        args.extend(extra.iter().map(|value| (*value).to_string()));
        args
    };
    let observation = |link: &str, args: Vec<String>| LegacyDaemonObservation {
        uid: 0,
        executable_link: PathBuf::from(link),
        canonical_executable: Some(legacy.clone()),
        executable_sha256: Some(hash),
        arguments: args,
        owns_legacy_listener: true,
    };

    let replaced = observation("/usr/local/bin/ramsharedd (deleted)", arguments(&[]));
    assert!(legacy_replaced_daemon_is_bound(&replaced, &legacy, 64));
    assert!(
        !legacy_replaced_daemon_is_bound(&replaced, &legacy, 128),
        "a different slice size must not match a ReplacedBinary proof"
    );
    let mut drifted = replaced.clone();
    drifted.arguments = arguments(&["--listen-nbd", "127.0.0.1:1"]);
    assert!(!legacy_replaced_daemon_is_bound(&drifted, &legacy, 64));

    let regular = observation("/usr/local/bin/ramsharedd", arguments(&[]));
    assert!(legacy_regular_daemon_is_bound(&regular, &legacy, &hash, 64));
    assert!(
        !legacy_regular_daemon_is_bound(&regular, &legacy, &[1u8; 32], 64),
        "a hash mismatch must not match a LegacyPathHashMatch proof"
    );

    let identity = |proof: LegacyDaemonProof| LegacyDaemonIdentity {
        pid: 4242,
        instance_id: "4242-100".into(),
        executable: legacy.clone(),
        proof,
    };
    assert!(legacy_daemon_identity_matches(
        &identity(LegacyDaemonProof::ReplacedBinary { slice_mb: 64 }),
        Some("4242-100"),
        &replaced
    ));
    assert!(!legacy_daemon_identity_matches(
        &identity(LegacyDaemonProof::ReplacedBinary { slice_mb: 64 }),
        Some("4242-100"),
        &regular
    ));
    assert!(legacy_daemon_identity_matches(
        &identity(LegacyDaemonProof::LegacyPathHashMatch {
            legacy_executable: legacy.clone(),
            executable_sha256: hash,
            slice_mb: 64,
        }),
        Some("4242-100"),
        &regular
    ));
}

#[test]
// TestName: legacy_runtime_record_value_refuses_every_unsealed_shape
fn legacy_runtime_record_value_refuses_every_unsealed_shape() {
    let dir = TestDir::new();
    let path = dir.path.join("swap-dev");

    fs::write(&path, b"/dev/nbd0").expect("write the record");
    std::os::unix::fs::symlink("/dev/null", dir.path.join("link")).expect("plant a symlink");
    assert!(
        legacy_runtime_record_value(&dir.path.join("link")).is_err(),
        "a symlink is not a sealed regular file"
    );

    if rustix::process::getuid().as_raw() == 0 {
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("widen the record");
        assert!(
            legacy_runtime_record_value(&path).is_err(),
            "a group- or world-writable record is not sealed even when root-owned"
        );

        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("seal the record");
        fs::write(&path, b"").expect("empty the record");
        assert!(
            legacy_runtime_record_value(&path).is_err(),
            "an empty record is malformed even when sealed"
        );

        fs::write(&path, b"/dev/nbd0\n/dev/nbd1").expect("write a multi-line record");
        assert!(
            legacy_runtime_record_value(&path).is_err(),
            "an embedded newline is malformed even when sealed"
        );

        fs::write(&path, b"  /dev/nbd0 \n").expect("rewrite the record");
        assert_eq!(
            legacy_runtime_record_value(&path).expect("a sealed record is readable"),
            Some("/dev/nbd0".into()),
            "whitespace is trimmed and the single line is published"
        );
    } else {
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("seal the record");
        assert!(
            legacy_runtime_record_value(&path).is_err(),
            "a non-root-owned record is refused before its content is read"
        );
    }
}

#[test]
// TestName: read_lifecycle_binding_refuses_symlink_and_invalid_payload
fn read_lifecycle_binding_refuses_symlink_and_invalid_payload() {
    let dir = TestDir::new();
    let paths = RuntimePaths::under(&dir.path);
    fs::create_dir_all(&paths.runtime_dir).expect("create the fixture runtime dir");
    assert!(
        read_lifecycle_binding(&paths).is_err(),
        "a missing binding is refused"
    );

    let nbd = bound_device_fixture("/dev/nbd0", ManagedDeviceKind::Nbd);
    let binding = lifecycle_binding_fixture(vec![nbd]);
    let payload = serde_json::to_vec(&binding).expect("serialize the fixture binding");
    fs::write(&paths.lifecycle_binding_file, &payload).expect("write the binding");
    fs::set_permissions(
        &paths.lifecycle_binding_file,
        fs::Permissions::from_mode(0o600),
    )
    .expect("seal the binding");
    assert!(read_lifecycle_binding(&paths).is_ok());

    let link = dir.path.join("binding-link");
    std::os::unix::fs::symlink(&paths.lifecycle_binding_file, &link).expect("symlink the binding");
    assert!(
        read_lifecycle_binding(&RuntimePaths {
            lifecycle_binding_file: link,
            ..RuntimePaths::under(&dir.path)
        })
        .is_err()
    );

    fs::write(&paths.lifecycle_binding_file, b"not json").expect("corrupt the binding");
    fs::set_permissions(
        &paths.lifecycle_binding_file,
        fs::Permissions::from_mode(0o600),
    )
    .expect("re-seal the binding");
    assert!(
        read_lifecycle_binding(&paths).is_err(),
        "an unparsable binding is refused"
    );

    let mut invalid = binding.clone();
    invalid.schema_version = 99;
    fs::write(
        &paths.lifecycle_binding_file,
        serde_json::to_vec(&invalid).expect("serialize"),
    )
    .expect("write the invalid binding");
    fs::set_permissions(
        &paths.lifecycle_binding_file,
        fs::Permissions::from_mode(0o600),
    )
    .expect("re-seal the binding");
    assert!(
        read_lifecycle_binding(&paths).is_err(),
        "a binding that fails schema validation must not be published to the caller"
    );
}

#[test]
// TestName: revalidate_daemon_and_socket_refuses_identity_drift
fn revalidate_daemon_and_socket_refuses_identity_drift() {
    let dir = TestDir::new();
    let paths = RuntimePaths::under(&dir.path);
    let daemon_pid = std::process::id();
    let devices = seal_runtime_lifecycle(&paths, daemon_pid, false);
    let binding = read_lifecycle_binding(&paths).expect("read the sealed fixture binding");
    assert_eq!(binding.devices, devices);

    revalidate_binding_identity(&binding).expect("the fresh fixture must revalidate");
    revalidate_peer_liveness(&binding, &paths).expect("the freshly sealed fixture must revalidate");

    let mut other_boot = binding.clone();
    other_boot.boot_id = "99999999-2222-4333-8444-555555555555".into();
    assert!(
        revalidate_binding_identity(&other_boot).is_err(),
        "a different kernel boot identity must refuse the binding"
    );

    let mut other_instance = binding.clone();
    other_instance.daemon_instance_id = "1-1".into();
    other_instance.daemon_start_ticks = 1;
    assert!(
        revalidate_peer_liveness(&other_instance, &paths).is_err(),
        "a different daemon instance must refuse the binding"
    );
}

#[test]
// TestName: pinned_effect_metadata_refuses_non_block_and_dev_t_drift
fn pinned_effect_metadata_refuses_non_block_and_dev_t_drift() {
    let dir = TestDir::new();
    let regular = dir.path.join("regular");
    fs::write(&regular, b"x").expect("write a regular file");
    let metadata = fs::symlink_metadata(&regular).expect("stat the regular file");
    assert!(
        validate_pinned_effect_metadata(&metadata, "0:0").is_err(),
        "a regular file is not a block device, so it cannot pin a managed device"
    );
    assert!(
        validate_pinned_effect_metadata(&metadata, "8:33").is_err(),
        "a dev_t mismatch is refused even if the type check were to pass"
    );
}

#[test]
// TestName: open_verified_daemon_pidfd_refuses_before_kernel_binding
fn open_verified_daemon_pidfd_refuses_before_kernel_binding() {
    let dir = TestDir::new();
    let paths = RuntimePaths::under(&dir.path);
    fs::create_dir_all(&paths.runtime_dir).expect("create the fixture runtime dir");
    let pid = std::process::id();
    let instance = daemon_instance_id_from_pid(pid).expect("derive the fixture instance id");
    let error = open_verified_daemon_pidfd(&paths, pid, "9-9")
        .expect_err("a different instance id must refuse before any pidfd is opened");
    assert!(error.to_string().contains("refusing ambiguous signal"));
    assert!(open_verified_daemon_pidfd(&paths, u32::MAX, &instance).is_err());
}

#[test]
// TestName: run_command_bounded_reports_exit_status_and_stderr
fn run_command_bounded_reports_exit_status_and_stderr() {
    let with_stderr = run_command_bounded("/bin/sh", &["-c", "echo boom >&2; exit 3"])
        .expect_err("a non-zero exit is an error");
    let message = with_stderr.to_string();
    assert!(
        message.contains("exit 3"),
        "the status code is named: {message}"
    );
    assert!(message.contains("boom"), "stderr is preserved: {message}");

    let silent = run_command_bounded("/bin/sh", &["-c", "exit 7"])
        .expect_err("a silent non-zero exit is still an error");
    assert!(
        silent.to_string().contains("exit 7"),
        "an empty stderr falls back to the bare status: {silent}"
    );

    let killed = run_command_bounded("/bin/sh", &["-c", "kill -9 $$"])
        .expect_err("a signalled child is an error");
    assert!(
        killed.to_string().contains("terminated by signal"),
        "a missing exit code is reported as a signal: {killed}"
    );
}

#[test]
// TestName: spawned_children_run_under_a_pinned_c_locale
fn spawned_children_run_under_a_pinned_c_locale() {
    // `classify_zram_allocation_failure` and the teardown swapoff absence
    // check read English `strerror` and gettext text. Under a non-C
    // `LC_MESSAGES` those translate and every failure falls through to the
    // arm that stops the walk, so the matchers are only sound if the child
    // is pinned. The child reports its own effective `LC_ALL`, not the
    // test process's, because the child's environment is what the
    // classifier depends on.
    let effective = run_command_bounded("/bin/sh", &["-c", "printf %s \"$LC_ALL\""])
        .expect("the fixture child must run");
    assert_eq!(
        effective, "C",
        "every classified child must see the locale the matchers assume"
    );

    // Negative: the pin is the spawn helper's, not an accident of this
    // test process. A grandchild of the pinned child inherits the same
    // value, which is the chain a shell-form `zramctl` wrapper would use.
    let grandchild = run_command_bounded("/bin/sh", &["-c", "sh -c 'printf %s \"$LC_ALL\"'"])
        .expect("the nested fixture child must run");
    assert_eq!(grandchild, "C", "the pin survives one exec level");
}
