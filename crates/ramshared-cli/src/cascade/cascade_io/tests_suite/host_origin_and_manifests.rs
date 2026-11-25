// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/tests_suite/host_origin_and_manifests.rs
#![allow(clippy::expect_used)]

use super::*;

#[test]
fn read_kernel_free_floor_kib_reads_sysfs_when_available() {
    if Path::new("/proc/sys/vm/min_free_kbytes").exists() {
        let floor = read_kernel_free_floor_kib().expect("read floor");
        assert!(floor > 0);
    }
}

#[test]
fn authorize_legacy_effect_validates_ghosts_and_presence() {
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
            used_kb: 0,
            priority: 100,
        }),
    };

    // Ghost swap present
    {
        let _seams = ParentSeams::install(
            "Filename\tType\tSize\tUsed\tPriority\n/dev/nbd0 (deleted)\tpartition\t4194300\t0\t50\n",
            0,
        );
        assert!(authorize_legacy_effect(&legacy, &nbd).is_err());
    }

    // Unexpected swap present
    {
        let _seams = ParentSeams::install(
            "Filename\tType\tSize\tUsed\tPriority\n/dev/nbd0\tpartition\t4194300\t0\t50\n/dev/nbd9\tpartition\t4194300\t0\t40\n",
            0,
        );
        assert!(authorize_legacy_effect(&legacy, &nbd).is_err());
    }

    // Target device absent
    {
        let _seams = ParentSeams::install(
            "Filename\tType\tSize\tUsed\tPriority\n/dev/zram0\tpartition\t1048572\t0\t100\n",
            0,
        );
        assert!(authorize_legacy_effect(&legacy, &nbd).is_err());
    }

    // Both present and matching
    {
        let _seams = ParentSeams::install(
            "Filename\tType\tSize\tUsed\tPriority\n/dev/nbd0\tpartition\t4194300\t0\t50\n/dev/zram0\tpartition\t1048572\t0\t100\n",
            0,
        );
        assert!(authorize_legacy_effect(&legacy, &nbd).is_ok());
        assert!(authorize_legacy_effect(&legacy, &zram).is_ok());
    }
}

#[test]
fn runtime_legacy_migration_executor_exercises_all_transition_steps() {
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
            used_kb: 0,
            priority: 100,
        }),
    };
    let daemon = LegacyDaemonIdentity {
        pid: 999_999_999,
        instance_id: "999999999-100".into(),
        executable: PathBuf::from("/bin/sh"),
        proof: LegacyDaemonProof::BinaryMatch,
    };

    let runner = ScriptedRunner::new(vec![
        ("swapoff -- /dev/nbd0".into(), Ok(String::new())),
        ("zramctl -r /dev/zram0".into(), Ok(String::new())),
        ("nbd-client -d /dev/nbd0".into(), Ok(String::new())),
    ]);

    let executor = RuntimeLegacyMigrationExecutor {
        runner: &runner,
        legacy: &legacy,
        daemon: &daemon,
    };

    // Swapoff sequence with seams
    {
        let _seams = ParentSeams::install("", 0);
        set_swap_snapshots([
            Ok("Filename\tType\tSize\tUsed\tPriority\n/dev/nbd0\tpartition\t4194300\t0\t50\n"),
            Ok("Filename\tType\tSize\tUsed\tPriority\n"),
        ]);
        assert!(executor.swapoff(&nbd).is_ok());
    }

    // Reset zram when swap absent
    {
        let _seams = ParentSeams::install("Filename\tType\tSize\tUsed\tPriority\n", 0);
        assert!(executor.reset_zram(&zram).is_ok());
        assert!(executor.disconnect_nbd(&nbd).is_ok());
    }

    // Stop daemon fails for dead PID
    assert!(executor.stop_daemon().is_err());
}

#[test]
fn migrate_legacy_cascade_fails_closed_without_active_legacy_setup() {
    // Custody under an isolated layout: this assertion is about the
    // missing legacy setup, and a test must never flock `/run/ramshared`.
    let dir = TestDir::new();
    assert!(
        migrate_legacy_cascade_at(&crate::customer::transition::TransitionPaths::under(
            &dir.path
        ))
        .is_err()
    );
}

#[test]
fn read_lifecycle_binding_refuses_unsealed_or_malformed() {
    let dir = TestDir::new();
    let paths = RuntimePaths::under(&dir.path);
    assert!(read_lifecycle_binding(&paths).is_err());

    fs::create_dir_all(&paths.runtime_dir).expect("create runtime dir");
    fs::write(&paths.lifecycle_binding_file, b"not valid json").expect("write binding file");
    fs::set_permissions(
        &paths.lifecycle_binding_file,
        fs::Permissions::from_mode(0o600),
    )
    .expect("set mode");
    assert!(read_lifecycle_binding(&paths).is_err());
}

#[test]
fn up_with_args_rejects_malformed_arguments() {
    assert!(up_with_args(&["--unknown".to_string()]).is_err());
    assert!(up_with_args(&["--vram-mb".to_string(), "invalid".to_string()]).is_err());
    assert!(up_with_args(&["--zram-mb".to_string(), "-5".to_string()]).is_err());
}

pub(in crate::cascade::cascade_io) fn up_with_args_refuses_ublk_before_swap_or_device_access() {
    // Custody under an isolated layout: the refusal under test is the
    // transport gate, and a test must never flock `/run/ramshared`.
    let dir = TestDir::new();
    let error = up_with_args_at(
        &crate::customer::transition::TransitionPaths::under(&dir.path),
        &["--transport".to_string(), "ublk".to_string()],
    )
    .expect_err("product ublk is a fail-closed refusal");

    let message = error.to_string();
    assert!(
        matches!(error, CascadeError::Precondition(_)),
        "a named ublk request must fail closed at the transport gate: {message}"
    );
    assert!(
        message.contains(ublk_refusal(is_wsl2())),
        "the refusal must be the transport gate's own wording: {message}"
    );
}

/// ITEM-6: a cascade mutation entrypoint must acquire customer-mutation
/// custody, so a competing holder refuses it before any effect.
#[test]
fn cascade_mutation_entrypoints_refuse_while_customer_custody_is_held() {
    use crate::customer::transition::{OperationOwner, TransitionPaths, acquire_transition};

    let dir = TestDir::new();
    let paths = TransitionPaths::under(&dir.path);
    let held =
        acquire_transition(OperationOwner::Package, &paths).expect("the first holder admits");

    let up = up_with_args_at(&paths, &[])
        .expect_err("`up` must not start while another mutation holds custody");
    assert!(
        up.to_string().contains("active_session"),
        "`up` must refuse with the stable busy reason: {up}"
    );

    let migrate = migrate_legacy_cascade_at(&paths)
        .expect_err("legacy migration must not start under foreign custody");
    assert!(
        migrate.to_string().contains("active_session"),
        "legacy migration must refuse with the stable busy reason: {migrate}"
    );

    drop(held);
    // Custody is released by process death or by the holder dropping, so a
    // later admission is not permanently poisoned by this test.
    let freed = acquire_transition(OperationOwner::Up, &paths);
    assert!(
        freed.is_ok(),
        "custody must be free once the holder drops: {freed:?}"
    );
}

/// ITEM-6 / DT-6: a durable `PackagePending` record refuses every other
/// mutation owner across hook process lifetimes.
#[test]
fn cascade_mutation_entrypoints_refuse_while_a_package_record_is_pending() {
    use crate::customer::transition::{
        OperationOwner, PackageAction, PackagePending, TransitionPaths, acquire_transition,
        enroll_pending,
    };

    let dir = TestDir::new();
    let paths = TransitionPaths::under(&dir.path);
    let package = acquire_transition(OperationOwner::Package, &paths)
        .expect("the package owner admits past no record");
    enroll_pending(
        &package,
        &PackagePending {
            target_release: None,
            action: PackageAction::Upgrade,
            old_hash: "a".repeat(64),
            new_hash: "b".repeat(64),
            issued_unix: 1,
        },
    )
    .expect("enroll the durable record");
    drop(package);

    let up = up_with_args_at(&paths, &[])
        .expect_err("`up` must refuse while a package transaction is pending");
    assert!(
        up.to_string().contains("recovery_required"),
        "`up` must refuse with the recovery reason: {up}"
    );
    assert!(
        up.to_string().contains("is pending"),
        "`up` must name the pending record: {up}"
    );

    let migrate = migrate_legacy_cascade_at(&paths)
        .expect_err("legacy migration must refuse while a package transaction is pending");
    assert!(
        migrate.to_string().contains("recovery_required"),
        "legacy migration must refuse with the recovery reason: {migrate}"
    );
}

/// ITEM-6: a nested `up` under an already held guard reaches the effect
/// gates and never re-acquires custody. A second `flock` of the same lock
/// from the same process would be refused as `active_session` against its
/// own caller, so the refusal the nested call *does* return must be an
/// effect gate, not custody.
#[test]
fn nested_up_borrows_the_held_guard_and_reaches_the_effect_gates() {
    use crate::customer::transition::{OperationOwner, TransitionPaths, acquire_transition};

    let dir = TestDir::new();
    let paths = TransitionPaths::under(&dir.path);
    let held = acquire_transition(OperationOwner::Boot, &paths).expect("boot custody admits");

    let error = up_with_args_held(&held, &["--transport".to_string(), "ublk".to_string()])
        .expect_err("product ublk is a fail-closed refusal");
    let message = error.to_string();
    assert!(
        !message.contains("active_session"),
        "a nested `up` must borrow the held guard, not re-acquire it: {message}"
    );
    assert!(
        message.contains(ublk_refusal(is_wsl2())),
        "the nested `up` must reach the transport gate: {message}"
    );

    // Custody is still held across the nested call — the borrow never
    // released it, so a competing acquire stays refused.
    let competing = acquire_transition(OperationOwner::Up, &paths)
        .expect_err("the nested call must not release the caller's custody");
    assert!(
        competing.to_string().contains("active_session"),
        "the caller's custody must survive the nested call: {competing}"
    );
}

/// ITEM-6: the production mutation entrypoints consult customer-mutation
/// custody before their first effect. On a non-root process the product
/// layout refuses that admission — the observable that proves the
/// ordering, because a refusal at custody means no device or daemon work
/// started.
///
/// As root this test must not invoke those wrappers: root owns the product
/// layout and they would proceed into live effects. The `_at` twins above
/// cover their bodies under an isolated layout.
#[test]
fn production_mutation_entrypoints_acquire_custody_before_any_effect() {
    if rustix::process::geteuid().as_raw() == 0 {
        return;
    }

    let refusals = [
        ("up_with_args", up_with_args(&[])),
        ("down", down()),
        ("migrate_legacy_cascade", migrate_legacy_cascade()),
        (
            "ensure_sealed_origin_attached",
            ensure_sealed_origin_attached(),
        ),
    ];
    for (name, result) in refusals {
        let error = result.expect_err("a non-root process must not own product mutation custody");
        let message = error.to_string();
        assert!(
            message.contains("customer mutation custody refused"),
            "{name} must refuse at custody before any effect: {message}"
        );
    }
}

pub(in crate::cascade::cascade_io) fn up_with_config_refuses_missing_safety_net_before_runtime_setup()
 {
    const EMPTY: &str = "Filename Type Size Used Priority\n";
    let _seams = ParentSeams::install(EMPTY, 0);
    set_live_device_snapshots([Ok(Vec::new())]);
    TEST_MEM_AVAILABLE.with(|cell| *cell.borrow_mut() = Some(0));

    let mut args = up_args_fixture();
    args.vram_mb = 1024;
    let error = up_with_config(args)
        .expect_err("the safety-net preflight must refuse before runtime setup");

    if is_wsl2() {
        assert!(
            error.to_string().contains("NBD") && error.to_string().contains("--accept-wsl2-nbd"),
            "WSL2 transport admission must refuse before safety-net probing: {error}"
        );
    } else {
        assert!(
            error.to_string().contains("no DEMOTE safety net"),
            "preflight returned an earlier error: {error}"
        );
    }
    assert!(
        SH_SCRIPT.with(|queue| queue.borrow().is_empty()),
        "a failed preflight must not invoke a product command"
    );
}

#[test]
fn ensure_origin_attached_is_noop_when_device_present() {
    let dir = TestDir::new();
    let present_device = dir.path.join("present-device");
    fs::write(&present_device, b"block").expect("write present device");

    struct NoOpRunner(RefCell<Vec<String>>);
    impl CommandRunner for NoOpRunner {
        fn run(&self, command: &str, args: &[&str]) -> Result<String, CascadeError> {
            self.0.borrow_mut().push(command_label(command, args));
            Ok(String::new())
        }
    }
    let runner = NoOpRunner(RefCell::new(Vec::new()));
    let res = ensure_origin_attached(
        &runner,
        present_device.to_str().expect("valid utf-8 path"),
        "11111111-2222-4333-8444-555555555555",
        Path::new("/nonexistent/manifest.json"),
        "",
        no_elevation,
    );
    assert!(res.is_ok());
    assert!(runner.0.borrow().is_empty());
}

#[test]
fn ensure_origin_attached_issues_bounded_mount_when_absent() {
    let dir = TestDir::new();
    let absent_device = dir.path.join("absent-device");
    let (manifest, manifest_hash) = write_test_host_manifest(&dir);

    struct MountRunner {
        target: PathBuf,
        calls: RefCell<Vec<String>>,
    }
    impl CommandRunner for MountRunner {
        fn run(&self, command: &str, args: &[&str]) -> Result<String, CascadeError> {
            self.calls.borrow_mut().push(command_label(command, args));
            // Simulate host mount exposing the target device
            fs::write(&self.target, b"mounted").expect("write target device");
            Ok(String::new())
        }
    }
    let runner = MountRunner {
        target: absent_device.clone(),
        calls: RefCell::new(Vec::new()),
    };
    let res = ensure_origin_attached(
        &runner,
        absent_device.to_str().expect("valid utf-8 path"),
        "11111111-2222-4333-8444-555555555555",
        &manifest,
        &manifest_hash,
        fake_elevation,
    );
    assert!(res.is_ok());
    let calls = runner.calls.borrow().clone();
    assert_eq!(calls.len(), 1);
    // DT-11: the sealed launcher is the program; the elevation boundary
    // and `wsl.exe` are argv elements it creates windowless. A shell or
    // `powershell.exe -Command` anywhere here would reinterpret the host
    // path and regress DT-1.
    assert_eq!(
        calls[0],
        concat!(
            "/mnt/c/ProgramData/RamShared/bin/ramshared-host-launch.exe ",
            "--timeout-ms 10000 --sudo /fake/windows/sudo.exe -- ",
            "wsl.exe --mount --vhd ",
            "C:\\ProgramData\\RamShared\\ramshared-origin.vhdx --bare"
        )
    );
    assert!(!calls[0].contains("cmd.exe"));
    assert!(!calls[0].contains("powershell"));
    assert!(!calls[0].contains("-Command"));
}

#[test]
fn ensure_origin_attached_refuses_unsealed_manifest_before_host_call() {
    let dir = TestDir::new();
    let device = dir.path.join("absent-device");
    let manifest = dir.path.join("origin-manifest.json");
    fs::write(&manifest, br#"{"origin_vhdx":"C:\\Other\\disk.vhdx"}"#).expect("write manifest");
    struct NoHostCall;
    impl CommandRunner for NoHostCall {
        fn run(&self, _: &str, _: &[&str]) -> Result<String, CascadeError> {
            panic!("unsealed manifest must not invoke the host");
        }
    }
    let error = ensure_origin_attached(
        &NoHostCall,
        device.to_str().expect("utf-8 path"),
        "11111111-2222-4333-8444-555555555555",
        &manifest,
        &"0".repeat(64),
        no_elevation,
    )
    .expect_err("hash mismatch must refuse attachment");
    assert!(error.to_string().contains("manifest"));
}

pub(in crate::cascade::cascade_io) fn ensure_origin_attached_rejects_unreadable_and_noncanonical_manifests_before_host_call()
 {
    struct NoHostCall;
    impl CommandRunner for NoHostCall {
        fn run(&self, _: &str, _: &[&str]) -> Result<String, CascadeError> {
            panic!("invalid origin manifest must not invoke the host");
        }
    }

    let dir = TestDir::new();
    let missing_device = dir.path.join("missing-device");
    let missing_manifest = dir.path.join("missing-manifest.json");
    let missing_error = ensure_origin_attached(
        &NoHostCall,
        missing_device.to_str().expect("UTF-8 fixture path"),
        "11111111-2222-4333-8444-555555555555",
        &missing_manifest,
        &"a".repeat(64),
        no_elevation,
    )
    .expect_err("missing manifest must refuse before host attach");
    assert!(missing_error.to_string().contains("unavailable"));

    let malformed_cases: [(&str, &[u8], &str); 4] = [
            ("invalid-json", b"not-json", "invalid"),
            (
                "missing-path",
                br#"{"partuuid":"11111111-2222-4333-8444-555555555555"}"#,
                "VHDX path is missing",
            ),
            (
                "wrong-partuuid",
                br#"{"origin_vhdx":"C:\\safe\\origin.vhdx","partuuid":"aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"}"#,
                "PARTUUID differs",
            ),
            (
                "unsafe-path",
                br#"{"origin_vhdx":"C:\\safe;echo\\origin.vhdx","partuuid":"11111111-2222-4333-8444-555555555555"}"#,
                "forbidden shell characters",
            ),
        ];
    for (label, contents, expected_error) in malformed_cases {
        let manifest = dir.path.join(format!("{label}.json"));
        fs::write(&manifest, contents).expect("write manifest fixture");
        let hash = Sha256::digest(contents)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let error = ensure_origin_attached(
            &NoHostCall,
            missing_device.to_str().expect("UTF-8 fixture path"),
            "11111111-2222-4333-8444-555555555555",
            &manifest,
            &hash,
            no_elevation,
        )
        .expect_err("noncanonical manifest must be rejected");
        assert!(
            error.to_string().contains(expected_error),
            "{label} returned an unexpected error: {error}"
        );
    }
}

#[test]
fn ensure_origin_attached_fails_closed_on_timeout_or_mismatch() {
    let dir = TestDir::new();
    let absent_device = dir.path.join("absent-device");
    let (manifest, manifest_hash) = write_test_host_manifest(&dir);

    struct FailingRunner(RefCell<Vec<String>>);
    impl CommandRunner for FailingRunner {
        fn run(&self, command: &str, args: &[&str]) -> Result<String, CascadeError> {
            self.0.borrow_mut().push(command_label(command, args));
            // Deliberately do NOT create the target device to simulate timeout/failure
            Ok(String::new())
        }
    }
    let runner = FailingRunner(RefCell::new(Vec::new()));
    let res = ensure_origin_attached(
        &runner,
        absent_device.to_str().expect("valid utf-8 path"),
        "11111111-2222-4333-8444-555555555555",
        &manifest,
        &manifest_hash,
        fake_elevation,
    );
    assert!(res.is_err());
    assert!(
        res.expect_err("expected error on timeout")
            .to_string()
            .contains("did not appear")
    );

    // Validation of dangerous windows path characters
    assert!(validate_windows_origin_path("C:\\safe\\origin.vhdx").is_ok());
    assert!(validate_windows_origin_path("invalid-drive-path").is_err());
    assert!(validate_windows_origin_path("C:\\path;rm -rf").is_err());
    assert!(validate_windows_origin_path("C:\\path&echo").is_err());
    assert!(validate_windows_origin_path("C:\\path%TEMP%\\origin.vhdx").is_err());
    assert!(validate_windows_origin_path("C:\\path^echo\\origin.vhdx").is_err());
    assert!(validate_windows_origin_path("C:\\path\norigin.vhdx").is_err());
}

#[test]
fn ensure_origin_attached_propagates_host_mount_failure() {
    let dir = TestDir::new();
    let device = dir.path.join("appeared-despite-error");
    let (manifest, manifest_hash) = write_test_host_manifest(&dir);

    struct FailedMount(PathBuf);
    impl CommandRunner for FailedMount {
        fn run(&self, _command: &str, _args: &[&str]) -> Result<String, CascadeError> {
            fs::write(&self.0, b"unexpected-device").expect("write fixture");
            Err(CascadeError::Precondition("host mount failed".into()))
        }
    }
    let error = ensure_origin_attached(
        &FailedMount(device.clone()),
        device.to_str().expect("utf-8 path"),
        "11111111-2222-4333-8444-555555555555",
        &manifest,
        &manifest_hash,
        fake_elevation,
    )
    .expect_err("host mount failure must not be ignored");
    assert!(error.to_string().contains("host mount failed"));
}

#[test]
fn the_host_argv_uses_the_windowless_launcher() {
    // DT-11: the sealed launcher is the program. It creates the elevation
    // boundary with CREATE_NO_WINDOW, and `wsl.exe` plus the host path are
    // argv elements nothing re-parses. `powershell.exe -Command` would
    // cross the same boundary but hand the host path to a shell.
    let boundary = HostBoundary::new(WINDOWS_HOST_LAUNCHER, "/fake/windows/sudo.exe".into());
    let (program, args) = host_disk_mount_argv(&boundary, "C:\\safe\\origin.vhdx");
    assert_eq!(program, WINDOWS_HOST_LAUNCHER);
    assert_eq!(
        args,
        vec![
            "--timeout-ms",
            "10000",
            "--sudo",
            "/fake/windows/sudo.exe",
            "--",
            "wsl.exe",
            "--mount",
            "--vhd",
            "C:\\safe\\origin.vhdx",
            "--bare"
        ]
    );
    assert_eq!(args[5], WSL_WINDOWS_PROGRAM);
    assert!(!args.iter().any(|arg| arg.contains("powershell")));
    assert!(!args.iter().any(|arg| arg.contains("cmd.exe")));
    assert!(!args.iter().any(|arg| *arg == "-Command" || *arg == "-c"));
    // The host path must survive as exactly one opaque element.
    assert_eq!(
        args.iter()
            .filter(|arg| arg.contains("origin.vhdx"))
            .count(),
        1
    );
    // The elevation boundary names the existing boundary; it never appears
    // as the process image.
    assert_ne!(program, "/fake/windows/sudo.exe");
}

/// without the sealed launcher there is no windowless path,
/// and a direct `sudo.exe`/`powershell.exe` fallback would reintroduce the
/// visible console. The refusal is stable and names the missing component.
#[test]
fn a_missing_launcher_is_a_refusal() {
    let error = host_launcher_program(false).expect_err("a missing launcher must refuse");
    assert!(error.to_string().contains(VISIBLE_ELEVATION_REFUSED));
    assert!(error.to_string().contains(HOST_LAUNCHER_WINDOWS_PATH));
    assert_eq!(
        host_launcher_program(true).expect("a present launcher is usable"),
        WINDOWS_HOST_LAUNCHER
    );
}
