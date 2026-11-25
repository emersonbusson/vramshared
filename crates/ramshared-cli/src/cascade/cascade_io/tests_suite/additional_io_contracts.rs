// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/tests_suite/additional_io_contracts.rs
#![allow(clippy::expect_used)]

use super::*;

#[test]
// TestName: revalidate_daemon_and_socket_refuses_each_identity_field
fn revalidate_daemon_and_socket_refuses_each_identity_field() {
    let dir = TestDir::new();
    let paths = RuntimePaths::under(&dir.path);
    let binding_device = seal_runtime_lifecycle(&paths, std::process::id(), false);
    let pristine = read_lifecycle_binding(&paths).expect("read the sealed fixture binding");
    assert_eq!(pristine.devices, binding_device);
    revalidate_binding_identity(&pristine).expect("the fresh fixture must revalidate");
    revalidate_peer_liveness(&pristine, &paths)
        .expect("the freshly sealed fixture must revalidate");

    let mut invocation = pristine.clone();
    invocation.daemon_invocation_id = "deadbeefdeadbeefdeadbeefdeadbeef".into();
    let error = revalidate_peer_liveness(&invocation, &paths)
        .expect_err("a different InvocationID must refuse the binding");
    assert!(
        error.to_string().contains("InvocationID"),
        "the refusal names the field: {error}"
    );

    let cache = pristine.clone();
    write_daemon_identity_status(&paths, cache.daemon_pid, "1-1");
    let error = revalidate_peer_liveness(&cache, &paths)
        .expect_err("a stale cache status must refuse the binding");
    assert!(
        error.to_string().contains("cache status"),
        "the refusal names the field: {error}"
    );
    write_daemon_identity_status(&paths, cache.daemon_pid, &cache.daemon_instance_id);

    let mut socket = pristine.clone();
    socket.export_socket.inode = socket.export_socket.inode.wrapping_add(1);
    let error = revalidate_peer_liveness(&socket, &paths)
        .expect_err("a different export socket must refuse the binding");
    assert!(
        error.to_string().contains("export socket"),
        "the refusal names the field: {error}"
    );

    let mut origin = pristine.clone();
    origin.origin.partuuid = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into();
    let error = revalidate_binding_identity(&origin)
        .expect_err("a different sealed origin must refuse the binding");
    assert!(
        error.to_string().contains("sealed origin manifest"),
        "the refusal names the field: {error}"
    );
}

#[test]
// TestName: legacy_runtime_record_value_reports_stat_errors
fn legacy_runtime_record_value_reports_stat_errors() {
    let dir = TestDir::new();
    let regular = dir.path.join("regular");
    fs::write(&regular, b"x").expect("write a regular file");
    let not_a_directory = regular.join("child");
    assert!(
        legacy_runtime_record_value(&not_a_directory).is_err(),
        "an ENOTDIR stat failure is reported, not silently treated as absent"
    );
}

#[test]
fn system_command_runner_validates_inputs_and_fails_safely_on_absent_targets() {
    let dir = TestDir::new();
    let runner = SystemCommandRunner;
    assert!(
        runner
            .run_bounded("/bin/true", &[], Duration::from_secs(2))
            .is_ok()
    );
    assert_eq!(zram_id_from_path(Path::new("/dev/zram0")), Some(0));
    assert_eq!(zram_id_from_path(Path::new("/dev/zram12")), Some(12));
    assert_eq!(zram_id_from_path(Path::new("/dev/sda")), None);
    assert_eq!(zram_id_from_path(Path::new("/dev/zramX")), None);
    assert_eq!(fresh_swap_uuid().len(), 16);

    let hot_add = dir.path.join("hot_add");
    fs::write(&hot_add, "7\n").expect("write hot_add fixture");
    assert_eq!(
        zram_hot_add_from(&hot_add, &|| Err(CascadeError::Io("unused".into())))
            .expect("valid hot_add"),
        "/dev/zram7"
    );
    fs::write(&hot_add, "bad\n").expect("write bad hot_add fixture");
    assert!(zram_hot_add_from(&hot_add, &|| Ok("/dev/zram1".into())).is_err());
    let missing_hot_add = dir.path.join("missing_hot_add");
    assert_eq!(
        zram_hot_add_from(&missing_hot_add, &|| Ok("/dev/zram2".into())).expect("fallback hot_add"),
        "/dev/zram2"
    );

    let missing = dir.path.join("nonexistent-swap");
    assert!(runner.native_swapoff(&missing, None).is_err());
    assert!(runner.native_zram_reset(Path::new("/dev/sda")).is_err());
    assert!(
        runner
            .native_zram_reset(Path::new("/dev/zram999999"))
            .is_err()
    );
    assert!(
        runner
            .native_zram_bind_backing("/dev/sda", "/dev/null")
            .is_err()
    );
    assert!(
        runner
            .native_zram_bind_backing("/dev/zram999999", "/dev/null")
            .is_err()
    );
    assert!(
        runner
            .native_zram_set_algorithm("/dev/sda", "zstd")
            .is_err()
    );
    assert!(
        runner
            .native_zram_set_algorithm("/dev/zram999999", "zstd")
            .is_err()
    );
    assert!(
        runner
            .native_zram_set_size("/dev/sda", "/dev/null", 4)
            .is_err()
    );
    assert!(
        runner
            .native_zram_set_size("/dev/zram999999", "/dev/null", 4)
            .is_err()
    );
    assert!(runner.native_swap_activate(&missing, "bad").is_err());
    assert!(runner.native_swap_activate(&missing, "100").is_err());
    assert!(
        runner
            .native_mkswap_and_swapon(&missing, 65_536, "bad")
            .is_err()
    );
    assert!(
        runner
            .native_mkswap_and_swapon(&missing, 65_536, "10")
            .is_err()
    );
}

#[test]
fn nbd_attach_reconciliation_and_writeback_edges_preserve_containment() {
    let dir = TestDir::new();
    let paths = RuntimePaths::under(&dir.path);
    fs::create_dir_all(&paths.runtime_dir).expect("create runtime");
    let mut daemon = controlled_child();
    let runner = ScriptedRunner::new(Vec::new());
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);

    // Attached rollback without binding / device / readable swaps refuses safely.
    assert!(
        rollback_nbd_attach_with(
            &runner,
            true,
            false,
            "/dev/nbd0",
            daemon.child_mut(),
            &paths
        )
        .is_err()
    );
    let zram_only = bound_device_fixture("/dev/zram0", ManagedDeviceKind::Zram);
    seal_runtime_lifecycle(&paths, daemon.id(), true);
    let mut wrong_binding = read_lifecycle_binding(&paths).expect("read binding");
    wrong_binding.devices = vec![zram_only];
    fs::write(
        &paths.lifecycle_binding_file,
        serde_json::to_vec(&wrong_binding).expect("encode wrong binding"),
    )
    .expect("write wrong binding");
    assert!(
        rollback_nbd_attach_with(
            &runner,
            true,
            false,
            "/dev/nbd0",
            daemon.child_mut(),
            &paths
        )
        .is_err()
    );

    // Preprovisioned signature mismatches refuse.
    let nbd = bound_device_fixture("/dev/nbd0", ManagedDeviceKind::Nbd);
    let mismatch_runner = ScriptedRunner::new(vec![
        ("blkid -s TYPE -o value /dev/nbd0".into(), Ok("ext4".into())),
        (
            "blkid -s UUID -o value /dev/nbd0".into(),
            Ok("99999999-8888-4777-8666-555555555555".into()),
        ),
    ]);
    assert!(
        verify_preprovisioned_swap(
            &mismatch_runner,
            &nbd,
            "99999999-8888-4777-8666-555555555555",
        )
        .is_err()
    );

    // Uncertain attach reconciliation branches: live query failure, ambiguous delta, stable owned device.
    let dir2 = TestDir::new();
    let paths2 = RuntimePaths::under(&dir2.path);
    let mut daemon2 = controlled_child();
    prepare_connect_identity(&paths2, daemon2.id());
    let socket2 = paths2.socket.to_string_lossy().into_owned();
    let fail_cmd = |cmd: &str| {
        Err(CascadeError::Shell {
            cmd: cmd.into(),
            msg: "transient attach fail".into(),
        })
    };

    // Branch 1: first live snapshot after failure errors -> UnsafeContainment.
    set_live_device_snapshots([Err("snapshot 1 err")]);
    let r1 = ScriptedRunner::new(vec![(
        expected_attach_label(&socket2),
        fail_cmd("nbd-client"),
    )]);
    assert!(matches!(
        connect_nbd_with(&r1, daemon2.child_mut(), &paths2, nbd_attach_fixture()),
        Err(CascadeError::UnsafeContainment(_))
    ));

    // Branch 2: second live snapshot after failure errors -> UnsafeContainment.
    set_live_device_snapshots([Ok(Vec::new()), Err("snapshot 2 err")]);
    let r2 = ScriptedRunner::new(vec![(
        expected_attach_label(&socket2),
        fail_cmd("nbd-client"),
    )]);
    assert!(matches!(
        connect_nbd_with(&r2, daemon2.child_mut(), &paths2, nbd_attach_fixture()),
        Err(CascadeError::UnsafeContainment(_))
    ));

    // Branch 3: divergent first vs second snapshots -> ambiguous UnsafeContainment.
    let mut nbd_owned = nbd.clone();
    nbd_owned.kernel_owner_instance_id = Some(format!("{}-100", daemon2.id()));
    set_live_device_snapshots([Ok(Vec::new()), Ok(vec![nbd_owned.clone()])]);
    let r3 = ScriptedRunner::new(vec![(
        expected_attach_label(&socket2),
        fail_cmd("nbd-client"),
    )]);
    assert!(matches!(
        connect_nbd_with(&r3, daemon2.child_mut(), &paths2, nbd_attach_fixture()),
        Err(CascadeError::UnsafeContainment(_))
    ));

    // Branch 4: two consistent post-attach snapshots seal evidence & preserve daemon.
    set_live_device_snapshots([Ok(vec![nbd_owned.clone()]), Ok(vec![nbd_owned])]);
    let r4 = ScriptedRunner::new(vec![(
        expected_attach_label(&socket2),
        fail_cmd("nbd-client"),
    )]);
    let err4 = connect_nbd_with(&r4, daemon2.child_mut(), &paths2, nbd_attach_fixture())
        .expect_err("uncertain attach must seal containment error");
    assert!(err4.to_string().contains("evidence was sealed"), "{err4}");

    // Branch 5: writeback mode (`swap_the_tier = false`) completes without swapon.
    let dir3 = TestDir::new();
    let paths3 = RuntimePaths::under(&dir3.path);
    let mut daemon3 = controlled_child();
    prepare_connect_identity(&paths3, daemon3.id());
    let socket3 = paths3.socket.to_string_lossy().into_owned();
    set_live_device_snapshots([]);
    let r5 = ScriptedRunner::new(vec![
        (expected_attach_label(&socket3), Ok(String::new())),
        ("blkid -s TYPE -o value /dev/nbd0".into(), Ok("swap".into())),
        (
            "blkid -s UUID -o value /dev/nbd0".into(),
            Ok("99999999-8888-4777-8666-555555555555".into()),
        ),
    ]);
    let mut cfg = nbd_attach_fixture();
    cfg.swap_the_tier = false;
    connect_nbd_with(&r5, daemon3.child_mut(), &paths3, cfg).expect("writeback attach");
    assert!(paths3.lifecycle_binding_file.exists());
    assert!(paths3.swap_dev_file.exists());
}
