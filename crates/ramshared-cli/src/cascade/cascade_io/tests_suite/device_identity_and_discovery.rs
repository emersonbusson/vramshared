// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/tests_suite/device_identity_and_discovery.rs
#![allow(clippy::expect_used)]

use super::*;

pub(in crate::cascade::cascade_io) fn bound_device_observation_enforces_sysfs_and_nbd_owner_policy()
{
    let live_nbd = bound_device_from_observation(BoundDeviceObservation {
        path: "/dev/nbd7".into(),
        expected_kind: ManagedDeviceKind::Nbd,
        owner_policy: NbdOwnerPolicy::RequireLive,
        dev_t: "43:7".into(),
        sysfs_path: "/fixture/sys/class/block/nbd7".into(),
        sysfs_dev_t: "43:7".into(),
        live_owner: Some("4242-100".into()),
        owner_is_absent: false,
    })
    .expect("matching sysfs identity and live owner are accepted");
    assert_eq!(
        live_nbd.kernel_owner_instance_id.as_deref(),
        Some("4242-100")
    );

    let ownerless_nbd = bound_device_from_observation(BoundDeviceObservation {
        path: "/dev/nbd7".into(),
        expected_kind: ManagedDeviceKind::Nbd,
        owner_policy: NbdOwnerPolicy::PermitAbsent,
        dev_t: "43:7".into(),
        sysfs_path: "/fixture/sys/class/block/nbd7".into(),
        sysfs_dev_t: "43:7".into(),
        live_owner: None,
        owner_is_absent: true,
    })
    .expect("explicitly absent owner is allowed for NBD observation");
    assert_eq!(ownerless_nbd.kernel_owner_instance_id, None);

    assert!(
        bound_device_from_observation(BoundDeviceObservation {
            path: "/dev/nbd7".into(),
            expected_kind: ManagedDeviceKind::Nbd,
            owner_policy: NbdOwnerPolicy::RequireLive,
            dev_t: "43:7".into(),
            sysfs_path: "/fixture/sys/class/block/nbd7".into(),
            sysfs_dev_t: "43:7".into(),
            live_owner: None,
            owner_is_absent: true,
        })
        .is_err()
    );
    assert!(
        bound_device_from_observation(BoundDeviceObservation {
            path: "/dev/nbd7".into(),
            expected_kind: ManagedDeviceKind::Nbd,
            owner_policy: NbdOwnerPolicy::PermitAbsent,
            dev_t: "43:7".into(),
            sysfs_path: "/fixture/sys/class/block/nbd7".into(),
            sysfs_dev_t: "43:8".into(),
            live_owner: None,
            owner_is_absent: true,
        })
        .is_err()
    );

    let zram = bound_device_from_observation(BoundDeviceObservation {
        path: "/dev/zram2".into(),
        expected_kind: ManagedDeviceKind::Zram,
        owner_policy: NbdOwnerPolicy::RequireLive,
        dev_t: "252:2".into(),
        sysfs_path: "/fixture/sys/class/block/zram2".into(),
        sysfs_dev_t: "252:2".into(),
        live_owner: None,
        owner_is_absent: false,
    })
    .expect("non-NBD devices do not require an NBD owner");
    assert_eq!(zram.kernel_owner_instance_id, None);
}

pub(in crate::cascade::cascade_io) fn production_effect_binding_refuses_non_block_files_after_exact_revalidation()
 {
    let identity = BoundDeviceIdentity {
        kind: ManagedDeviceKind::Nbd,
        path: "/dev/null".into(),
        dev_t: "43:7".into(),
        sysfs_path: "/sys/devices/virtual/block/nbd7".into(),
        sysfs_dev_t: "43:7".into(),
        kernel_owner_instance_id: Some("4242-100".into()),
    };
    let validations = std::cell::Cell::new(0);
    let result = open_device_for_effect_with(
        &identity,
        |_| {
            validations.set(validations.get() + 1);
            Ok(())
        },
        |path| fs::File::open(path),
    );
    assert!(matches!(result, Err(CascadeError::Precondition(_))));
    assert_eq!(validations.get(), 1, "identity is checked before opening");

    let character_metadata = fs::metadata("/dev/null").expect("stat harmless character device");
    assert!(matches!(
        validate_pinned_effect_metadata(&character_metadata, "1:3"),
        Err(CascadeError::Precondition(_))
    ));

    let missing = BoundDeviceIdentity {
        path: "/dev/ramshared-test-device-that-does-not-exist".into(),
        ..identity
    };
    assert!(matches!(
        open_device_for_effect_with(&missing, |_| Ok(()), |path| fs::File::open(path)),
        Err(CascadeError::Precondition(_))
    ));
}

#[test]
fn sysfs_bound_device_observation_checks_node_owner_and_dev_t() {
    let fixture = TestDir::new();
    let class_block = fixture.path.join("sys/class/block");
    let nbd = class_block.join("nbd7");
    fs::create_dir_all(&nbd).expect("create sysfs-shaped NBD fixture");
    fs::write(nbd.join("dev"), "43:7\n").expect("write NBD dev_t");
    fs::write(nbd.join("pid"), "4242\n").expect("write NBD owner PID");
    let named = DetachedNbdNode {
        is_symlink: false,
        is_block_device: true,
        dev_t: "43:7".into(),
    };
    let observe = |named: DetachedNbdNode, class_block: &Path| {
        observe_bound_device_in_sysfs(
            "/dev/nbd7".into(),
            ManagedDeviceKind::Nbd,
            NbdOwnerPolicy::PermitAbsent,
            named,
            class_block,
            |pid| Some(format!("{pid}-100")),
            |_| unreachable!("a live owner identity needs no /proc probe"),
        )
    };

    let live = observe(named.clone(), &class_block).expect("matching live NBD is accepted");
    assert_eq!(live.dev_t, "43:7");
    assert_eq!(live.sysfs_dev_t, "43:7");
    assert_eq!(live.kernel_owner_instance_id.as_deref(), Some("4242-100"));

    fs::write(nbd.join("dev"), "43:8\n").expect("write mismatching dev_t");
    assert!(observe(named.clone(), &class_block).is_err());
    fs::remove_file(nbd.join("dev")).expect("remove NBD dev_t fixture");
    assert!(observe(named.clone(), &class_block).is_err());
    fs::write(nbd.join("dev"), "43:7\n").expect("restore NBD dev_t");
    fs::write(nbd.join("dev"), "invalid\n").expect("write malformed dev_t");
    assert!(observe(named.clone(), &class_block).is_err());
    fs::write(nbd.join("dev"), "43:7\n").expect("restore NBD dev_t");
    fs::write(nbd.join("pid"), "invalid\n").expect("write malformed owner PID");
    assert!(observe(named.clone(), &class_block).is_err());
    fs::remove_file(nbd.join("pid")).expect("remove NBD owner PID");
    assert!(observe(named.clone(), &class_block).is_err());
    fs::create_dir(nbd.join("pid")).expect("replace owner PID with unreadable directory");
    assert!(observe(named.clone(), &class_block).is_err());
    fs::remove_dir(nbd.join("pid")).expect("remove unreadable owner PID fixture");
    fs::write(nbd.join("pid"), "0\n").expect("write ownerless NBD PID");
    assert!(observe(named.clone(), &class_block).is_err());
    fs::write(nbd.join("pid"), "4242\n").expect("restore NBD owner PID");

    assert!(
        observe(
            DetachedNbdNode {
                is_symlink: true,
                ..named.clone()
            },
            &class_block,
        )
        .is_err()
    );
    assert!(
        observe(
            DetachedNbdNode {
                is_block_device: false,
                ..named.clone()
            },
            &class_block,
        )
        .is_err()
    );
    assert!(observe(named.clone(), &class_block.join("missing")).is_err());

    let ownerless = observe_bound_device_in_sysfs(
        "/dev/nbd7".into(),
        ManagedDeviceKind::Nbd,
        NbdOwnerPolicy::PermitAbsent,
        named.clone(),
        &class_block,
        |_| None,
        |_| Ok(true),
    )
    .expect("absent owner process can be tolerated by explicit NBD policy");
    assert_eq!(ownerless.kernel_owner_instance_id, None);
    assert!(
        observe_bound_device_in_sysfs(
            "/dev/nbd7".into(),
            ManagedDeviceKind::Nbd,
            NbdOwnerPolicy::PermitAbsent,
            named.clone(),
            &class_block,
            |_| None,
            |_| Ok(false),
        )
        .is_err()
    );
    assert!(
        observe_bound_device_in_sysfs(
            "/dev/nbd7".into(),
            ManagedDeviceKind::Nbd,
            NbdOwnerPolicy::PermitAbsent,
            named.clone(),
            &class_block,
            |_| None,
            |_| Err(CascadeError::Precondition("owner lookup refused".into())),
        )
        .is_err()
    );

    let zram = class_block.join("zram3");
    fs::create_dir_all(&zram).expect("create sysfs-shaped zram fixture");
    fs::write(zram.join("dev"), "252:3\n").expect("write zram dev_t");
    let zram_identity = observe_bound_device_in_sysfs(
        "/dev/zram3".into(),
        ManagedDeviceKind::Zram,
        NbdOwnerPolicy::RequireLive,
        DetachedNbdNode {
            is_symlink: false,
            is_block_device: true,
            dev_t: "252:3".into(),
        },
        &class_block,
        |_| unreachable!("non-NBD devices do not inspect a daemon owner"),
        |_| unreachable!("non-NBD devices do not inspect an owner process"),
    )
    .expect("non-NBD device does not require NBD ownership");
    assert_eq!(zram_identity.kind, ManagedDeviceKind::Zram);
}

#[test]
// TestName: cascade_parallel_timeout_teardown_publishes_immutable_fixture_and_retains_evidence
fn cascade_parallel_timeout_teardown_publishes_immutable_fixture_and_retains_evidence() {
    let results = std::thread::scope(|scope| {
        let workers = (0..2)
            .map(|_| scope.spawn(run_timeout_teardown_fixture_with_held_predecessor))
            .collect::<Vec<_>>();
        workers
            .into_iter()
            .map(|worker| match worker.join() {
                Ok(result) => result,
                Err(_) => Err("timeout fixture worker panicked".to_string()),
            })
            .collect::<Vec<_>>()
    });
    for (index, result) in results.into_iter().enumerate() {
        if let Err(error) = result {
            panic!("parallel timeout fixture {index} failed: {error}");
        }
    }
}

#[test]
// TestName: controlled_child_is_held_for_the_body_and_reaped_on_drop
fn controlled_child_is_held_for_the_body_and_reaped_on_drop() {
    let mut daemon = controlled_child();
    let pid = daemon.id();
    assert!(
        !process_is_gone_or_zombie(pid),
        "the owner must hold a live child for the whole body"
    );
    assert!(
        daemon
            .try_wait()
            .unwrap_or_else(|error| panic!("inspect controlled child: {error}"))
            .is_none(),
        "the fixture child must not expire on a short wall-clock deadline"
    );
    drop(daemon);
    assert!(
        process_is_gone_or_zombie(pid),
        "dropping the owner must reap the child rather than leave it to a timer"
    );
}

pub(in crate::cascade::cascade_io) fn bounded_command_captures_stdout_and_rejects_nonzero() {
    let fixture = TestDir::new();
    let success = fixture.program("success", "#!/bin/sh\nprintf ' /dev/zram7\\n'\n");
    let output = run_fixture_script_bounded(&success, Duration::from_millis(250))
        .unwrap_or_else(|error| panic!("bounded success command: {error}"));
    assert_eq!(output, "/dev/zram7");

    let failure = fixture.program(
        "failure",
        "#!/bin/sh\nprintf 'fixture failure' >&2\nexit 12\n",
    );
    let error = error_from(
        run_fixture_script_bounded(&failure, Duration::from_millis(250)),
        "non-zero child must fail",
    );
    let message = error.to_string();
    assert!(
        message.contains("12") && message.contains("fixture failure"),
        "{message}"
    );

    let signal = fixture.program("signal", "#!/bin/sh\nkill -TERM $$\n");
    let error = error_from(
        run_fixture_script_bounded(&signal, Duration::from_millis(250)),
        "signal-terminated child must fail",
    );
    assert!(
        error.to_string().contains("terminated by signal"),
        "{error}"
    );

    let oversized = fixture.program("oversized", "#!/bin/sh\nhead -c 65537 /dev/zero\n");
    let error = error_from(
        run_fixture_script_bounded(&oversized, Duration::from_millis(250)),
        "bounded output must reject an oversized stream",
    );
    assert!(error.to_string().contains("output exceeded"), "{error}");
}

pub(in crate::cascade::cascade_io) fn bounded_command_times_out_and_reaps_its_direct_child() {
    let observed_pid = Cell::new(None);
    let started = Instant::now();
    let error = error_from(
        run_command_bounded_for_with_spawn(
            "/bin/sleep",
            &["10"],
            Duration::from_millis(40),
            |pid| observed_pid.set(Some(pid)),
        ),
        "sleeping direct child must time out",
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(error.to_string().contains("timed out"), "{error}");

    let pid = observed_pid
        .get()
        .unwrap_or_else(|| panic!("bounded command did not report its direct child PID"));
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "timed-out direct child {pid} was not reaped"
    );
}

// TestName: bounded_command_contains_descendant_that_inherits_output
pub(in crate::cascade::cascade_io) fn bounded_command_contains_descendant_that_inherits_output() {
    let fixture = TestDir::new();
    let descendant_pid_file = fixture.path.join("inherited-output-descendant.pid");
    let program = fixture.program(
        "inherited-output",
        &format!(
            "#!/bin/sh\n(sleep 10) &\nprintf '%s' \"$!\" > '{}'\nprintf 'ready\\n'\nexit 0\n",
            descendant_pid_file.display()
        ),
    );

    let started = Instant::now();
    let error = error_from(
        run_fixture_script_bounded(&program, Duration::from_millis(100)),
        "an inherited output pipe must not be accepted as command success",
    );
    let descendant_pid = fs::read_to_string(&descendant_pid_file)
        .unwrap_or_else(|read_error| panic!("read descendant PID: {read_error}"))
        .parse::<u32>()
        .unwrap_or_else(|parse_error| panic!("parse descendant PID: {parse_error}"));
    let descendant_path = PathBuf::from(format!("/proc/{descendant_pid}"));
    let deadline = Instant::now() + Duration::from_millis(250);
    while descendant_path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let contained = !descendant_path.exists();
    if !contained {
        let raw_pid = i32::try_from(descendant_pid)
            .unwrap_or_else(|_| panic!("descendant PID is outside pidfd range"));
        if let Some(pid) = Pid::from_raw(raw_pid) {
            let _ = rustix::process::kill_process(pid, Signal::KILL);
        }
    }

    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(
        error.to_string().contains("output"),
        "unexpected inherited-pipe error: {error}"
    );
    assert!(contained, "owned inherited-output descendant survived");
}

pub(in crate::cascade::cascade_io) fn zram_output_requires_exact_block_identity() {
    assert_eq!(
        parse_zram_device(" /dev/zram7\n"),
        Some("/dev/zram7".to_string())
    );
    for invalid in [
        "",
        "/dev/zram",
        "/dev/zram7-extra",
        "/tmp/zram7",
        "/dev/zram-7",
        "/dev/zram7 /dev/zram8",
    ] {
        assert_eq!(parse_zram_device(invalid), None, "{invalid:?}");
    }
}

pub(in crate::cascade::cascade_io) fn transport_refusal_is_fail_closed_before_command() {
    let error = error_from(
        check_transport_for(Transport::Ublk, true, false),
        "WSL2 ublk must refuse before a command is started",
    );
    assert!(error.to_string().contains("refused"), "{error}");
    let error = error_from(
        check_transport_for(Transport::Nbd, true, false),
        "WSL2 NBD swap must refuse before a command is started",
    );
    assert!(error.to_string().contains("NBD"), "{error}");
    assert!(check_transport_for(Transport::Nbd, false, false).is_ok());
}

#[test]
fn product_daemon_command_requires_origin_cache() {
    let command = build_daemon_command(
        "/opt/ramshared/current/bin/ramsharedd",
        4096,
        1024,
        "/run/ramshared/wsl2d.sock",
        "/dev/nbd0",
        "/dev/disk/by-partuuid/11111111-2222-3333-4444-555555555555",
        false,
        None,
    );
    let arguments = command
        .get_args()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let environment = command
        .get_envs()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.map(|item| item.to_string_lossy().into_owned()),
            )
        })
        .collect::<Vec<_>>();
    assert!(
        arguments
            .windows(2)
            .any(|pair| { pair == ["--origin-manifest", ORIGIN_CONFIG_FILE,] })
    );
    assert!(environment.iter().any(|(key, value)| {
        key == "RAMSHARED_VRAM_CACHE_CAP_MIB" && value.as_deref() == Some("1024")
    }));
}

#[test]
fn provisioning_is_explicit_and_separate_from_normal_up() {
    let normal_source = include_str!("../nbd_attach.rs");
    let connect_start = normal_source
        .find("fn connect_nbd_with")
        .unwrap_or_else(|| panic!("connect_nbd_with source is missing"));
    assert!(!normal_source[connect_start..].contains("mkswap"));

    let provisioner = include_str!("../../../../../../scripts/safety/provision-origin-swap.sh");
    assert!(provisioner.contains("RAMSHARED_ORIGIN_PROVISION_APPROVAL"));
    assert!(provisioner.contains("/sbin/mkswap -L RAMSHARED -U"));
    assert!(provisioner.contains("FOREIGN_SIGNATURE_PRESENT"));
}

pub(in crate::cascade::cascade_io) fn failed_readiness_terminates_only_spawned_child() {
    let fixture = TestDir::new();
    let daemon = fixture.program(
        "ramsharedd",
        "#!/bin/sh\nprintf '%s' \"$$\" > \"$0.pid\"\nexec sleep 10\n",
    );
    let paths = RuntimePaths::under(&fixture.path);
    let error = error_from(
        spawn_daemon_with_deadline(
            &as_program(&daemon),
            64,
            64,
            "/dev/nbd0",
            "/dev/disk/by-partuuid/11111111-2222-3333-4444-555555555555",
            &paths,
            Duration::from_millis(40),
            false,
            None,
        ),
        "a daemon without the configured socket must fail readiness",
    );
    assert!(error.to_string().contains("socket"), "{error}");

    let pid_path = fixture.path.join("ramsharedd.pid");
    let pid = fs::read_to_string(&pid_path)
        .unwrap_or_else(|read_error| panic!("read fixture daemon pid: {read_error}"))
        .trim()
        .parse::<u32>()
        .unwrap_or_else(|parse_error| panic!("parse fixture daemon pid: {parse_error}"));
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "readiness cleanup left direct child {pid} alive"
    );
    assert!(!paths.pid_file.exists(), "stale runtime pid file");
    assert!(!paths.socket.exists(), "unexpected socket path");
}

#[test]
fn origin_mode_refuses_missing_daemon_cache_identity_before_nbd_attach() {
    let fixture = TestDir::new();
    let daemon = fixture.program(
            "ramsharedd",
            "#!/bin/sh\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = \"--sock\" ]; then touch \"$2\"; fi\n  shift\ndone\nexec /bin/sleep 60\n",
        );
    let paths = RuntimePaths::under(&fixture.path);
    match spawn_daemon_with_deadline(
        &as_program(&daemon),
        1024,
        1024,
        "/dev/nbd0",
        "/dev/disk/by-partuuid/11111111-2222-3333-4444-555555555555",
        &paths,
        Duration::from_millis(80),
        false,
        None,
    ) {
        Ok(mut child) => {
            let _ = terminate_spawned_child(&mut child);
            panic!("origin mode accepted a daemon without a current cache identity");
        }
        Err(error) => {
            assert!(error.to_string().contains("identity"), "{error}");
        }
    }
    assert!(
        !paths.pid_file.exists(),
        "failed origin startup retained a PID record"
    );
}

pub(in crate::cascade::cascade_io) fn connect_nbd_preserves_primary_error_and_rolls_back_once() {
    let fixture = TestDir::new();
    let paths = RuntimePaths::under(&fixture.path);
    let marker_parent = paths.forensics_markers[0]
        .parent()
        .unwrap_or_else(|| panic!("test marker has no parent"));
    fs::create_dir_all(marker_parent)
        .unwrap_or_else(|error| panic!("create test marker parent: {error}"));
    fs::write(&paths.forensics_markers[0], "armed")
        .unwrap_or_else(|error| panic!("write test marker: {error}"));
    let mut daemon = controlled_child();
    prepare_connect_identity(&paths, daemon.id());
    let _seams = ParentSeams::install("Filename Type Size Used Priority\n", 0);
    let nbd = bound_device_fixture("/dev/nbd0", ManagedDeviceKind::Nbd);
    set_live_device_snapshots([Ok(vec![nbd]), Ok(Vec::new())]);
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
        ("nbd-client -d /dev/nbd0".into(), Ok(String::new())),
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
        "signature error must be returned after rollback",
    );
    assert!(
        error.to_string().contains("fixture signature failure"),
        "primary error was replaced: {error}"
    );
    assert_eq!(
        runner.calls(),
        vec![
            attach,
            "blkid -s TYPE -o value /dev/nbd0".into(),
            "nbd-client -d /dev/nbd0".into(),
        ]
    );
    assert!(
        daemon
            .try_wait()
            .unwrap_or_else(|wait_error| panic!("inspect controlled child: {wait_error}"))
            .is_some(),
        "rollback did not reap the exact spawned child"
    );
    assert!(
        !paths.forensics_markers[0].exists(),
        "marker was not disarmed"
    );
}
