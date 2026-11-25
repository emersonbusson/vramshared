// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/tests_suite/command_and_zram_fallback.rs
#![allow(clippy::expect_used)]

use super::*;

pub(in crate::cascade::cascade_io) fn connect_nbd_records_exact_preprovisioned_swap_before_activation()
 {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    let mut daemon = controlled_child();
    prepare_connect_identity(&paths, daemon.id());
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    set_live_device_snapshots([Ok(Vec::new())]);
    let socket = paths.socket.to_string_lossy().into_owned();
    let attach = expected_attach_label_conns(&socket, 2);
    let runner = ScriptedRunner::new(vec![
        (attach.clone(), Ok(String::new())),
        (
            "blkid -s TYPE -o value /dev/nbd0".into(),
            Ok("swap\n".into()),
        ),
        (
            "blkid -s UUID -o value /dev/nbd0".into(),
            Ok("99999999-8888-4777-8666-555555555555\n".into()),
        ),
        ("swapon -p 100 /dev/nbd0".into(), Ok(String::new())),
    ]);

    connect_nbd_with(
        &runner,
        daemon.child_mut(),
        &paths,
        NbdAttachConfig {
            connections: 2,
            ..nbd_attach_fixture()
        },
    )
    .expect("the exact sealed swap device should attach and activate");

    let binding = read_lifecycle_binding(&paths).expect("sealed lifecycle binding");
    assert_eq!(binding.devices.len(), 1);
    assert_eq!(binding.devices[0].path, "/dev/nbd0");
    assert_eq!(
        read_nbd_path_record(&paths.swap_dev_file).expect("NBD path record"),
        "/dev/nbd0"
    );
    let capacity =
        fs::read_to_string(&paths.capacity_status_file).expect("published capacity status");
    assert!(capacity.contains("logical_capacity_kib=65536"));
    assert_eq!(
        runner.calls(),
        vec![
            attach,
            "blkid -s TYPE -o value /dev/nbd0".into(),
            "blkid -s UUID -o value /dev/nbd0".into(),
            "swapon -p 100 /dev/nbd0".into(),
        ]
    );
    terminate_spawned_child(daemon.child_mut()).expect("stop the controlled test daemon");
}

pub(in crate::cascade::cascade_io) fn connect_nbd_refuses_invalid_target_and_zero_connections_before_runner()
 {
    for (swap_dev, connections) in [("/dev/zram7", 1), ("/dev/nbd0", 0)] {
        let fixture = TestDir::new();
        let paths = RuntimePaths::under(&fixture.path);
        let mut daemon = controlled_child();
        let runner = ScriptedRunner::new(Vec::new());

        let error = error_from(
            connect_nbd_with(
                &runner,
                daemon.child_mut(),
                &paths,
                NbdAttachConfig {
                    swap_dev,
                    connections,
                    ..nbd_attach_fixture()
                },
            ),
            "invalid attach inputs must fail before command execution",
        );

        assert!(matches!(
            error,
            CascadeError::Precondition(_) | CascadeError::Arg(_)
        ));
        assert!(runner.calls().is_empty());
        assert!(
            daemon
                .try_wait()
                .expect("observe refused fixture child")
                .is_some(),
            "refused attach must reap only its spawned fixture child"
        );
    }
}

#[test]
// TestName: nbd_rollback_unproven_child_termination_retains_forensics
fn nbd_rollback_unproven_child_termination_retains_forensics() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir)
        .unwrap_or_else(|error| panic!("create test runtime: {error}"));
    fs::create_dir_all(
        paths.forensics_markers[0]
            .parent()
            .unwrap_or_else(|| panic!("test marker has no parent")),
    )
    .unwrap_or_else(|error| panic!("create test forensics directory: {error}"));
    fs::write(&paths.swap_dev_file, "/dev/nbd0")
        .unwrap_or_else(|error| panic!("write test swap record: {error}"));
    fs::write(&paths.pid_file, "fixture-pid")
        .unwrap_or_else(|error| panic!("write test PID record: {error}"));
    fs::write(&paths.cache_status_file, "cache-evidence")
        .unwrap_or_else(|error| panic!("write test cache evidence: {error}"));
    fs::write(&paths.capacity_status_file, "capacity-evidence")
        .unwrap_or_else(|error| panic!("write test capacity evidence: {error}"));
    fs::write(&paths.forensics_markers[0], "armed")
        .unwrap_or_else(|error| panic!("arm test forensics: {error}"));
    let runner = ScriptedRunner::new(Vec::new());
    let mut child = UnprovenChild {
        termination_requests: 0,
    };

    let error = error_from(
        rollback_nbd_attach_with(&runner, false, true, "/dev/nbd0", &mut child, &paths),
        "unproven exact child termination must stop cleanup",
    );

    assert!(matches!(error, CascadeError::UnsafeContainment(_)));
    assert_eq!(child.termination_requests, 1);
    assert!(paths.swap_dev_file.exists());
    assert!(paths.pid_file.exists());
    assert!(paths.cache_status_file.exists());
    assert!(paths.capacity_status_file.exists());
    assert!(paths.forensics_markers[0].exists());
}

#[test]
// TestName: nbd_startup_disconnect_refusal_preserves_containment_evidence
fn nbd_startup_disconnect_refusal_preserves_containment_evidence() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir)
        .unwrap_or_else(|error| panic!("create test runtime: {error}"));
    fs::create_dir_all(
        paths.forensics_markers[0]
            .parent()
            .unwrap_or_else(|| panic!("test marker has no parent")),
    )
    .unwrap_or_else(|error| panic!("create marker parent: {error}"));
    fs::write(&paths.forensics_markers[0], "armed")
        .unwrap_or_else(|error| panic!("write marker: {error}"));
    fs::write(&paths.pid_file, "fixture-pid")
        .unwrap_or_else(|error| panic!("write PID evidence: {error}"));
    let mut daemon = controlled_child();
    prepare_connect_identity(&paths, daemon.id());
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    let nbd = bound_device_fixture("/dev/nbd0", ManagedDeviceKind::Nbd);
    set_live_device_snapshots([Ok(vec![nbd])]);
    let socket = paths.socket.to_string_lossy().into_owned();
    let attach = expected_attach_label(&socket);
    let runner = ScriptedRunner::new(vec![
        (attach.clone(), Ok(String::new())),
        (
            "blkid -s TYPE -o value /dev/nbd0".into(),
            Err(CascadeError::Shell {
                cmd: "blkid -s TYPE -o value /dev/nbd0".into(),
                msg: "fixture signature failure".into(),
            }),
        ),
        (
            "nbd-client -d /dev/nbd0".into(),
            Err(CascadeError::Shell {
                cmd: "nbd-client -d /dev/nbd0".into(),
                msg: "fixture detach failure".into(),
            }),
        ),
    ]);

    let error = error_from(
        connect_nbd_with(
            &runner,
            daemon.child_mut(),
            &paths,
            NbdAttachConfig {
                connections: 1,
                swap_dev: "/dev/nbd0",
                vram_prio: 100,
                disk_baseline_kib: 0,
                logical_capacity_kib: 64 * 1024,
                origin_partuuid: "11111111-2222-4333-8444-555555555555",
                expected_swap_uuid: "99999999-8888-4777-8666-555555555555",
                ..nbd_attach_fixture()
            },
        ),
        "an unproven NBD detach must be terminal",
    );
    assert!(error.to_string().contains("unsafe containment"), "{error}");
    assert!(
        daemon
            .try_wait()
            .unwrap_or_else(|wait_error| panic!("inspect child: {wait_error}"))
            .is_none(),
        "daemon was killed despite an unproven NBD detach"
    );
    assert!(paths.pid_file.exists(), "PID evidence was deleted");
    assert!(
        paths.forensics_markers[0].exists(),
        "forensics were disarmed"
    );
    assert_eq!(
        runner.calls(),
        vec![
            attach,
            "blkid -s TYPE -o value /dev/nbd0".into(),
            "nbd-client -d /dev/nbd0".into(),
        ]
    );
}

#[test]
fn nbd_startup_disconnect_postcheck_preserves_daemon_and_evidence() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    let mut daemon = controlled_child();
    let devices = seal_runtime_lifecycle(&paths, daemon.id(), false);
    arm_forensics_at(&paths);
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    set_nbd_detach_settle_timeout(Duration::from_millis(10));
    set_swap_snapshots([
        Ok("Filename Type Size Used Priority\n"),
        Ok("Filename Type Size Used Priority\n"),
        Ok("Filename Type Size Used Priority\n"),
    ]);
    // The settle observes on every poll; the owner must stay visible past
    // the bounded deadline so the proof fails instead of the exhausted
    // snapshot script fabricating the absent set.
    set_live_device_snapshots([Ok(devices.clone()), Ok(devices.clone()), Ok(devices)]);
    let runner = ScriptedRunner::new(vec![("nbd-client -d /dev/nbd0".into(), Ok(String::new()))]);

    let error = error_from(
        rollback_nbd_attach_with(&runner, true, true, "/dev/nbd0", daemon.child_mut(), &paths),
        "a successful command without exact disappearance proof must preserve containment",
    );

    assert!(error.to_string().contains("completion"), "{error}");
    assert_eq!(runner.calls(), vec!["nbd-client -d /dev/nbd0"]);
    assert!(
        daemon
            .try_wait()
            .unwrap_or_else(|wait_error| panic!("inspect controlled child: {wait_error}"))
            .is_none(),
        "unproven detach terminated the exact daemon"
    );
    assert!(paths.lifecycle_binding_file.exists());
    assert!(paths.swap_dev_file.exists());
    assert!(paths.pid_file.exists());
    assert!(paths.forensics_markers[0].exists());
}

pub(in crate::cascade::cascade_io) fn connect_nbd_refusal_terminates_exact_daemon_without_detach() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    let marker_parent = paths.forensics_markers[0]
        .parent()
        .unwrap_or_else(|| panic!("test marker has no parent"));
    fs::create_dir_all(marker_parent)
        .unwrap_or_else(|error| panic!("create test marker parent: {error}"));
    fs::write(&paths.forensics_markers[0], "armed")
        .unwrap_or_else(|error| panic!("write test marker: {error}"));
    let runner = ScriptedRunner::new(Vec::new());
    let mut daemon = controlled_child();

    let error = error_from(
        connect_nbd_with(
            &runner,
            daemon.child_mut(),
            &paths,
            NbdAttachConfig {
                connections: 0,
                swap_dev: "/dev/nbd0",
                vram_prio: 100,
                disk_baseline_kib: 0,
                logical_capacity_kib: 64 * 1024,
                origin_partuuid: "11111111-2222-4333-8444-555555555555",
                expected_swap_uuid: "99999999-8888-4777-8666-555555555555",
                ..nbd_attach_fixture()
            },
        ),
        "invalid connection count must stop the exact spawned daemon",
    );
    assert!(error.to_string().contains("connections"));
    assert!(
        runner.calls().is_empty(),
        "pre-attach refusal ran a command"
    );
    assert!(
        daemon
            .try_wait()
            .unwrap_or_else(|wait_error| panic!("inspect controlled child: {wait_error}"))
            .is_some(),
        "pre-attach refusal did not reap the exact spawned child"
    );
    assert!(!paths.forensics_markers[0].exists());
}

pub(in crate::cascade::cascade_io) fn connect_nbd_uncertain_swapon_preserves_backend_and_daemon() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir)
        .unwrap_or_else(|error| panic!("create test runtime: {error}"));
    let mut daemon = controlled_child();
    prepare_connect_identity(&paths, daemon.id());
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    let socket = paths.socket.to_string_lossy().into_owned();
    let attach = expected_attach_label(&socket);
    let runner = ScriptedRunner::new(vec![
        (attach.clone(), Ok(String::new())),
        ("blkid -s TYPE -o value /dev/nbd0".into(), Ok("swap".into())),
        (
            "blkid -s UUID -o value /dev/nbd0".into(),
            Ok("99999999-8888-4777-8666-555555555555".into()),
        ),
        (
            "swapon -p 100 /dev/nbd0".into(),
            Err(CascadeError::Shell {
                cmd: "swapon -p 100 /dev/nbd0".into(),
                msg: "fixture activate failure".into(),
            }),
        ),
    ]);

    let error = error_from(
        connect_nbd_with(
            &runner,
            daemon.child_mut(),
            &paths,
            NbdAttachConfig {
                connections: 1,
                swap_dev: "/dev/nbd0",
                vram_prio: 100,
                disk_baseline_kib: 0,
                logical_capacity_kib: 64 * 1024,
                origin_partuuid: "11111111-2222-4333-8444-555555555555",
                expected_swap_uuid: "99999999-8888-4777-8666-555555555555",
                ..nbd_attach_fixture()
            },
        ),
        "uncertain swapon must preserve the exact backend and daemon",
    );
    assert!(error.to_string().contains("swapon outcome"), "{error}");
    assert_eq!(
        runner.calls(),
        vec![
            attach,
            "blkid -s TYPE -o value /dev/nbd0".into(),
            "blkid -s UUID -o value /dev/nbd0".into(),
            "swapon -p 100 /dev/nbd0".into(),
        ]
    );
    assert!(
        daemon
            .try_wait()
            .unwrap_or_else(|wait_error| panic!("inspect controlled child: {wait_error}"))
            .is_none(),
        "uncertain swapon terminated the exact daemon"
    );
    assert!(paths.swap_dev_file.exists());
    assert!(paths.lifecycle_binding_file.exists());
}

#[test]
// TestName: nbd_effect_before_timeout_preserves_backend_and_seals_exact_evidence
fn nbd_effect_before_timeout_preserves_backend_and_seals_exact_evidence() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    let mut daemon = controlled_child();
    prepare_connect_identity(&paths, daemon.id());
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    let nbd = bound_device_fixture("/dev/nbd0", ManagedDeviceKind::Nbd);
    set_live_device_snapshots([Ok(vec![nbd.clone()]), Ok(vec![nbd])]);
    let socket = paths.socket.to_string_lossy().into_owned();
    let attach = expected_attach_label(&socket);
    let runner = ScriptedRunner::new(vec![(
        attach.clone(),
        Err(CascadeError::Shell {
            cmd: attach.clone(),
            msg: "timed out after effect fixture".into(),
        }),
    )]);

    let error = error_from(
        connect_nbd_with(&runner, daemon.child_mut(), &paths, nbd_attach_fixture()),
        "effect-before-timeout must return unsafe containment",
    );

    assert!(matches!(error, CascadeError::UnsafeContainment(_)));
    assert!(error.to_string().contains("took effect"), "{error}");
    assert_eq!(runner.calls(), vec![attach]);
    assert!(
        daemon
            .try_wait()
            .unwrap_or_else(|wait_error| panic!("inspect preserved daemon: {wait_error}"))
            .is_none(),
        "uncertain attach terminated the backend daemon"
    );
    let binding = read_lifecycle_binding(&paths)
        .unwrap_or_else(|read_error| panic!("read provisional lifecycle binding: {read_error}"));
    assert_eq!(binding.devices.len(), 1);
    assert_eq!(binding.devices[0].path, "/dev/nbd0");
    assert_eq!(
        read_nbd_path_record(&paths.swap_dev_file)
            .unwrap_or_else(|read_error| panic!("read provisional NBD path: {read_error}")),
        "/dev/nbd0"
    );
    terminate_spawned_child(daemon.child_mut())
        .unwrap_or_else(|stop_error| panic!("contain preserved fixture daemon: {stop_error}"));
}

#[test]
// TestName: nbd_failed_attach_terminates_backend_only_after_two_absence_snapshots
fn nbd_failed_attach_terminates_backend_only_after_two_absence_snapshots() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    let mut daemon = controlled_child();
    prepare_connect_identity(&paths, daemon.id());
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    set_live_device_snapshots([Ok(Vec::new()), Ok(Vec::new())]);
    let socket = paths.socket.to_string_lossy().into_owned();
    let attach = expected_attach_label(&socket);
    let runner = ScriptedRunner::new(vec![(
        attach.clone(),
        Err(CascadeError::Shell {
            cmd: attach.clone(),
            msg: "fixture attach failure".into(),
        }),
    )]);

    let error = error_from(
        connect_nbd_with(&runner, daemon.child_mut(), &paths, nbd_attach_fixture()),
        "proven absent attach must return the primary error",
    );

    assert!(
        error.to_string().contains("fixture attach failure"),
        "{error}"
    );
    assert_eq!(runner.calls(), vec![attach]);
    assert!(
        daemon
            .try_wait()
            .unwrap_or_else(|wait_error| panic!("inspect contained daemon: {wait_error}"))
            .is_some(),
        "backend was not terminated after two exact absence snapshots"
    );
    assert!(!paths.lifecycle_binding_file.exists());
    assert!(!paths.swap_dev_file.exists());
}

#[test]
// TestName: nbd_failed_attach_preserves_backend_when_target_status_is_not_absent
fn nbd_failed_attach_preserves_backend_when_target_status_is_not_absent() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    let mut daemon = controlled_child();
    prepare_connect_identity(&paths, daemon.id());
    let _seams = ParentSeams::install(
        "Filename Type Size Used Priority\n/dev/nbd0 partition 1024 0 100\n",
        0,
    );
    set_live_device_snapshots([Ok(Vec::new()), Ok(Vec::new())]);
    let socket = paths.socket.to_string_lossy().into_owned();
    let attach = expected_attach_label(&socket);
    let runner = ScriptedRunner::new(vec![(
        attach.clone(),
        Err(CascadeError::Shell {
            cmd: attach.clone(),
            msg: "fixture attach failure with active target status".into(),
        }),
    )]);

    let error = error_from(
        connect_nbd_with(&runner, daemon.child_mut(), &paths, nbd_attach_fixture()),
        "ambiguous target status must preserve the backend",
    );

    assert!(matches!(error, CascadeError::UnsafeContainment(_)));
    assert!(error.to_string().contains("absence is unproven"), "{error}");
    assert_eq!(runner.calls(), vec![attach]);
    assert!(
        daemon
            .try_wait()
            .unwrap_or_else(|wait_error| panic!("inspect preserved daemon: {wait_error}"))
            .is_none(),
        "ambiguous target status terminated the backend daemon"
    );
    terminate_spawned_child(daemon.child_mut())
        .unwrap_or_else(|stop_error| panic!("contain preserved fixture daemon: {stop_error}"));
}

pub(in crate::cascade::cascade_io) fn zram_fallback_refuses_unexpected_device_without_swapon() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    let runner = ScriptedRunner::new(vec![
        ("modprobe zram".into(), Ok(String::new())),
        ("zramctl --find".into(), Ok("/tmp/not-zram".into())),
    ]);

    let error = error_from(
        setup_zram_with(&runner, &paths, 64, 200, None),
        "non-zram output must fail before mkswap or swapon",
    );
    assert!(error.to_string().contains("unexpected device"), "{error}");
    assert!(
        error.to_string().contains("sysfs fallback is disabled"),
        "{error}"
    );
    assert_eq!(
        runner.calls().len(),
        2,
        "malformed allocator output is not evidence about the compressor; \
             the walk must stop rather than try another one"
    );
    assert!(!paths.zram_dev_file.exists());
}

/// The classifier's only job is to answer "may the walk change
/// compressor?". One spelling of the decision, proved against the tool's
/// own diagnostics rather than a restatement of the walk.
#[test]
fn zram_allocation_failure_classification_names_only_capability_answers() {
    let shell = |msg: &str| CascadeError::Shell {
        cmd: "zramctl --find".into(),
        msg: msg.into(),
    };
    assert_eq!(
        classify_zram_allocation_failure(&shell(
            "zramctl: /dev/zram0: failed to set algorithm: Invalid argument"
        )),
        ZramAllocationFailure::AlgorithmUnsupported
    );
    assert_eq!(
        classify_zram_allocation_failure(&shell(
            "zramctl: /dev/zram0: failed to reset: Device or resource busy"
        )),
        ZramAllocationFailure::DeviceTransient
    );
    assert_eq!(
        classify_zram_allocation_failure(&shell(
            "zramctl: /dev/zram0: failed to set algorithm: Device or resource busy"
        )),
        ZramAllocationFailure::DeviceTransient
    );
    assert_eq!(
        classify_zram_allocation_failure(&shell("zramctl: no free zram device found")),
        ZramAllocationFailure::DeviceTransient
    );
    assert_eq!(
        classify_zram_allocation_failure(&shell(
            "zramctl: /dev/zram0: failed to set disksize (2147483648 bytes): Cannot allocate memory"
        )),
        ZramAllocationFailure::Fatal
    );
    assert_eq!(
        classify_zram_allocation_failure(&CascadeError::Precondition("spawn failed".into())),
        ZramAllocationFailure::Fatal
    );
}

/// A device that is still in use says nothing about the compressor. The
/// walk retries the same name and lands on the preferred one; it never
/// silently downgrades to a lesser compressor to work around a device race.
#[test]
fn zram_busy_retries_the_same_compressor_and_never_downgrades() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    fs::create_dir_all(&paths.runtime_dir).expect("create isolated runtime");
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    let identity = bound_device_fixture("/dev/zram7", ManagedDeviceKind::Zram);
    set_live_device_snapshots([Ok(vec![identity.clone()])]);
    let mut responses = vec![("modprobe zram".into(), Ok(String::new()))];
    // One failure short of the budget: the final attempt is the one that
    // succeeds, so the test proves the retry reached the same compressor.
    for _ in 1..ZRAM_ALLOCATION_ATTEMPTS {
        responses.push((
            "zramctl --find".into(),
            Err(CascadeError::Shell {
                cmd: "zramctl --find".into(),
                msg: "zramctl: /dev/zram0: failed to reset: Device or resource busy".into(),
            }),
        ));
    }
    pin_zram_comp_algorithm(&paths, "/dev/zram7", "lzo-rle");
    responses.push(("zramctl --find".into(), Ok("/dev/zram7\n".into())));
    responses.push((
        "bash -c echo lzo-rle > /sys/block/zram7/comp_algorithm".into(),
        Ok(String::new()),
    ));
    responses.push((
        "bash -c echo 67108864 > /sys/block/zram7/disksize".into(),
        Ok(String::new()),
    ));
    responses.push(("mkswap /dev/zram7".into(), Ok(String::new())));
    responses.push(("swapon -p 200 /dev/zram7".into(), Ok(String::new())));
    let runner = ScriptedRunner::new(responses);

    let device = setup_zram_with(&runner, &paths, 64, 200, None)
        .expect("a retried device-state failure still reaches the preferred compressor");
    assert_eq!(device, "/dev/zram7");
    let calls = runner.calls();
    let attempts: Vec<&String> = calls
        .iter()
        .filter(|call| *call == "zramctl --find")
        .collect();
    assert_eq!(
        attempts.len(),
        usize::try_from(ZRAM_ALLOCATION_ATTEMPTS).expect("attempt budget fits usize")
    );
    // The retry never changed the compressor: the one algorithm write the
    // run performed names the preferred name, and there is no second.
    let algo_writes: Vec<&String> = calls
        .iter()
        .filter(|call| call.contains("comp_algorithm"))
        .collect();
    assert_eq!(
        algo_writes,
        vec![&"bash -c echo lzo-rle > /sys/block/zram7/comp_algorithm".to_string()]
    );
}
