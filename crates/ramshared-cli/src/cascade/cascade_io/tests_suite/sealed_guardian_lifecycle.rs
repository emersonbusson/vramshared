// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/tests_suite/sealed_guardian_lifecycle.rs
#![allow(clippy::expect_used)]

use super::*;

#[test]
fn select_windows_elevation_boundary_is_fail_closed() {
    // Explicit override wins, and a broken override never silently falls
    // back to the default — that would hide an operator's misconfiguration
    // behind a call that then fails as HCS/E_ACCESSDENIED.
    assert_eq!(
        select_windows_elevation_boundary(Some("/custom/sudo.exe"), true, true)
            .expect("explicit usable boundary"),
        "/custom/sudo.exe"
    );
    let broken = select_windows_elevation_boundary(Some("/missing/sudo.exe"), false, true)
        .expect_err("a broken WIN_SUDO must refuse");
    assert!(broken.to_string().contains(WINDOWS_ELEVATION_ENV));

    assert_eq!(
        select_windows_elevation_boundary(None, false, true).expect("default boundary"),
        WINDOWS_ELEVATION_BOUNDARY
    );
    let none =
        select_windows_elevation_boundary(None, false, false).expect_err("no boundary must refuse");
    assert!(none.to_string().contains("elevation boundary"));
    assert!(none.to_string().contains("HCS/E_ACCESSDENIED"));
}

#[test]
fn ensure_origin_attached_refuses_when_no_elevation_boundary_is_available() {
    // the happy-path mocks cannot see the Windows
    // administrator boundary. An unelevated `wsl.exe --mount` is a call
    // that cannot succeed, so the attach must refuse before invoking it.
    let dir = TestDir::new();
    let absent_device = dir.path.join("absent-device");
    let (manifest, manifest_hash) = write_test_host_manifest(&dir);

    struct NoHostCall;
    impl CommandRunner for NoHostCall {
        fn run(&self, _command: &str, _args: &[&str]) -> Result<String, CascadeError> {
            panic!("a refused attach must not invoke the host");
        }
    }
    let error = ensure_origin_attached(
        &NoHostCall,
        absent_device.to_str().expect("utf-8 path"),
        "11111111-2222-4333-8444-555555555555",
        &manifest,
        &manifest_hash,
        missing_elevation,
    )
    .expect_err("a missing elevation boundary must refuse the attach");
    assert!(error.to_string().contains("elevation boundary"));
}

/// DT-19. One bounded elevated host call, and the argv is exactly the
/// product-owned payload's `ensure` action. The guest never reaches for
/// `schtasks`, never opens a shell and never types a task name.
#[test]
fn ensure_guardian_starts_the_named_task_through_the_product_payload() {
    struct RecordingRunner(RefCell<Vec<String>>);
    impl CommandRunner for RecordingRunner {
        fn run(&self, command: &str, args: &[&str]) -> Result<String, CascadeError> {
            self.0.borrow_mut().push(command_label(command, args));
            Ok(String::new())
        }
    }
    let runner = RecordingRunner(RefCell::new(Vec::new()));
    let distro = "Fixture-Ubuntu";
    let user_sid = "S-1-5-21-1-2-3-1001";
    let task_name = "Fixture.Task.Name.v1";
    ensure_guardian_running(&runner, distro, user_sid, task_name, fake_elevation)
        .expect("a sealed identity under a usable boundary must start");
    let calls = runner.0.borrow().clone();
    assert_eq!(calls.len(), 1, "exactly one bounded host start");
    assert_eq!(
        calls[0],
        concat!(
            "/mnt/c/ProgramData/RamShared/bin/ramshared-host-launch.exe ",
            "--timeout-ms 10000 --sudo /fake/windows/sudo.exe -- ",
            "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe ",
            "-NoProfile -NonInteractive -ExecutionPolicy Bypass -File ",
            "C:\\ProgramData\\RamShared\\guardian\\Watch-RamSharedWsl.ps1 ",
            "-Action ensure -Run -Distro Fixture-Ubuntu ",
            "-UserSid S-1-5-21-1-2-3-1001 -TaskName Fixture.Task.Name.v1"
        )
    );
    // `-File` binds argv as script parameters. A `-Command` anywhere here
    // would re-lex the distro, SID and task name as PowerShell source and
    // regress the DT-1 rule.
    assert!(calls[0].contains(" -File "));
    assert!(!calls[0].contains("-Command"));
    assert!(!calls[0].contains("cmd.exe"));
    assert!(!calls[0].contains("schtasks"));
    // `wsl.exe` is not how a scheduled task is started on the host.
    assert!(!calls[0].contains("wsl.exe"));
}

/// DT-19. A guest that cannot read its sealed identity must not ask the
/// host to start anything at all.
#[test]
fn ensure_guardian_refuses_an_unreadable_sealed_configuration_before_any_host_call() {
    struct NoHostCall;
    impl CommandRunner for NoHostCall {
        fn run(&self, _command: &str, _args: &[&str]) -> Result<String, CascadeError> {
            panic!("an unreadable sealed configuration must not reach the host");
        }
    }
    let dir = TestDir::new();
    let missing = dir.path.join("absent-guardian-config.json");
    let error = ensure_sealed_guardian_running_from(&NoHostCall, &missing, no_elevation)
        .expect_err("a missing sealed configuration must refuse");
    assert!(error.to_string().contains("not admissible"));
    assert!(error.to_string().contains("guardian_identity_unavailable"));

    let malformed = dir.path.join("malformed-guardian-config.json");
    fs::write(&malformed, b"not-json").expect("write malformed config");
    let error = ensure_sealed_guardian_running_from(&NoHostCall, &malformed, no_elevation)
        .expect_err("a malformed sealed configuration must refuse");
    assert!(error.to_string().contains("guardian_identity_malformed"));

    let wrong_task = dir.path.join("wrong-task-guardian-config.json");
    fs::write(
            &wrong_task,
            br#"{"schema_version":2,"distro":"Fixture-Ubuntu","user_sid":"S-1-5-21-1-2-3-1001","task_name":"Some.Other.Task.v9","guardian_policy":{}}"#,
        )
        .expect("write wrong-task config");
    let error = ensure_sealed_guardian_running_from(&NoHostCall, &wrong_task, no_elevation)
        .expect_err("a document naming another task must refuse");
    assert!(
        error
            .to_string()
            .contains("guardian_identity_schema_mismatch")
    );
}

/// DT-19. The start takes the task name the sealed document authorized —
/// a value, never a literal typed at the start site. The fixture name is
/// deliberately not the product constant: if the argv carried a hardcoded
/// name, this assertion would fail.
#[test]
fn ensure_guardian_passes_the_sealed_task_name_and_never_a_literal() {
    struct RecordingRunner(RefCell<Vec<String>>);
    impl CommandRunner for RecordingRunner {
        fn run(&self, command: &str, args: &[&str]) -> Result<String, CascadeError> {
            self.0.borrow_mut().push(command_label(command, args));
            Ok(String::new())
        }
    }
    let runner = RecordingRunner(RefCell::new(Vec::new()));
    let fixture_task = "Fixture.Task.Name.v1";
    assert_ne!(
        fixture_task,
        crate::cascade::sealed_identity::GUARDIAN_TASK_NAME,
        "the fixture must not be the product constant"
    );
    ensure_guardian_running(
        &runner,
        "Fixture-Ubuntu",
        "S-1-5-21-1-2-3-1001",
        fixture_task,
        fake_elevation,
    )
    .expect("a fixture task name is a valid parameter");
    let calls = runner.0.borrow().clone();
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0].contains(&format!("-TaskName {fixture_task}")),
        "the argv must carry the name it was given: {}",
        calls[0]
    );
    assert!(
        !calls[0].contains(crate::cascade::sealed_identity::GUARDIAN_TASK_NAME),
        "the start must not substitute a literal for the sealed name: {}",
        calls[0]
    );
    // One opaque element, never a re-lexed fragment.
    let name_count = calls[0].matches(fixture_task).count();
    assert_eq!(name_count, 1, "the task name appears exactly once");
}

/// an unelevated `powershell.exe` start is a call that
/// cannot succeed, so the verb must refuse before invoking it.
#[test]
fn ensure_guardian_refuses_when_no_elevation_boundary_is_available() {
    struct NoHostCall;
    impl CommandRunner for NoHostCall {
        fn run(&self, _command: &str, _args: &[&str]) -> Result<String, CascadeError> {
            panic!("a refused start must not invoke the host");
        }
    }
    let dir = TestDir::new();
    let config = write_guardian_config(&dir);
    let error = ensure_sealed_guardian_running_from(&NoHostCall, &config, missing_elevation)
        .expect_err("a missing elevation boundary must refuse the start");
    assert!(error.to_string().contains("elevation boundary"));
}

/// DT-17/DT-19. The guest starts the producer; it never mints the proof
/// that verifies it. The only effect is one bounded host call, and the
/// guardian source carries no publisher or state write on this path.
#[test]
fn ensure_guardian_never_writes_a_health_proof() {
    struct RecordingRunner(RefCell<Vec<String>>);
    impl CommandRunner for RecordingRunner {
        fn run(&self, command: &str, args: &[&str]) -> Result<String, CascadeError> {
            self.0.borrow_mut().push(command_label(command, args));
            Ok(String::new())
        }
    }
    let runner = RecordingRunner(RefCell::new(Vec::new()));
    ensure_guardian_running(
        &runner,
        "Fixture-Ubuntu",
        "S-1-5-21-1-2-3-1001",
        "Fixture.Task.Name.v1",
        fake_elevation,
    )
    .expect("the start succeeds");
    let calls = runner.0.borrow().clone();
    assert_eq!(calls.len(), 1);
    for forbidden in [
        "health.json",
        "guardian-state",
        "Publish",
        "Write-AtomicJson",
        "--write",
        "--publish",
        "--proof",
    ] {
        assert!(
            !calls[0].contains(forbidden),
            "the start argv must never touch a proof surface: {forbidden}"
        );
    }

    // Structural half: the ensure path's own source has no write primitive
    // and no publisher. Span is `host_origin.rs`'s ensure functions.
    let source = include_str!("../host_origin.rs");
    let start = source
        .find("fn ensure_sealed_guardian_running_from")
        .expect("ensure_sealed_guardian_running_from source is missing");
    let span = &source[start..];
    for forbidden in [
        "fs::write",
        "fs::OpenOptions",
        "File::create",
        "GUARDIAN_STATE_DIR",
        "health.json",
        "write_all",
    ] {
        assert!(
            !span.contains(forbidden),
            "the ensure path must never write a health proof: {forbidden}"
        );
    }
}

/// DT-19. The verb is a safe retry: re-issuing it is the same single
/// bounded request, with no accumulating guest state. The host half of the
/// idempotency is the payload's own decision (`Get-GuardianEnsureDecision`
/// returns `NOOP` while the task runs) plus `MultipleInstances IgnoreNew`.
#[test]
fn ensure_guardian_is_idempotent_when_the_task_is_already_running() {
    struct RecordingRunner(RefCell<Vec<String>>);
    impl CommandRunner for RecordingRunner {
        fn run(&self, command: &str, args: &[&str]) -> Result<String, CascadeError> {
            self.0.borrow_mut().push(command_label(command, args));
            Ok(String::new())
        }
    }
    let runner = RecordingRunner(RefCell::new(Vec::new()));
    for _ in 0..2 {
        ensure_guardian_running(
            &runner,
            "Fixture-Ubuntu",
            "S-1-5-21-1-2-3-1001",
            "Fixture.Task.Name.v1",
            fake_elevation,
        )
        .expect("each invocation is the same admissible request");
    }
    let calls = runner.0.borrow().clone();
    assert_eq!(calls.len(), 2, "one bounded call per invocation");
    assert_eq!(calls[0], calls[1], "a retry is the identical request");
}

/// DT-19. The guest only ever asks the payload to `ensure`. It never
/// passes a verb that would create, register, enable or install a task, so
/// an attended stop stays authoritative and install keeps registration
/// ownership. The refusal itself is the payload's
/// `Get-GuardianEnsureDecision` (`REFUSE` on absent or disabled).
#[test]
fn ensure_guardian_refuses_a_disabled_or_absent_task_without_creating_it() {
    struct RecordingRunner(RefCell<Vec<String>>);
    impl CommandRunner for RecordingRunner {
        fn run(&self, command: &str, args: &[&str]) -> Result<String, CascadeError> {
            self.0.borrow_mut().push(command_label(command, args));
            Ok(String::new())
        }
    }
    let runner = RecordingRunner(RefCell::new(Vec::new()));
    ensure_guardian_running(
        &runner,
        "Fixture-Ubuntu",
        "S-1-5-21-1-2-3-1001",
        "Fixture.Task.Name.v1",
        fake_elevation,
    )
    .expect("the ensure request is admissible");
    let calls = runner.0.borrow().clone();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].contains("-Action ensure"));
    for forbidden in [
        "-Action install",
        "-Action activate",
        "-Action watch",
        "Register-ScheduledTask",
        "Enable-ScheduledTask",
        "New-ScheduledTask",
        "schtasks",
        "/create",
        "/run",
    ] {
        assert!(
            !calls[0].contains(forbidden),
            "ensure must never create, enable or register a task: {forbidden}"
        );
    }

    // The payload's own decision is what refuses; the guest half is that
    // it never asks for anything else.
    let payload = include_str!("../../../../../../scripts/windows/Watch-RamSharedWsl.ps1");
    let decision_start = payload
        .find("function Get-GuardianEnsureDecision")
        .expect("Get-GuardianEnsureDecision is missing");
    let decision_end = payload[decision_start..]
        .find("function Ensure-GuardianTaskRunning")
        .map(|offset| decision_start + offset)
        .expect("ensure decision boundary is missing");
    let decision = &payload[decision_start..decision_end];
    assert!(decision.contains("task_absent"));
    assert!(decision.contains("task_definition_disabled"));
    assert!(decision.contains("REFUSE"));
}

#[test]
fn ensure_sealed_origin_attached_is_idempotent_when_the_sealed_device_is_present() {
    // The sealed test fixture's origin path is the device the test seam
    // already treats as present. The boot-gate entry point must be a no-op
    // there: re-attaching an attached VHD is a host mutation with no gain.
    struct NoHostCall;
    impl CommandRunner for NoHostCall {
        fn run(&self, _command: &str, _args: &[&str]) -> Result<String, CascadeError> {
            panic!("an attached sealed origin must not reach the host");
        }
    }
    let res = ensure_sealed_origin_attached_from(
        &NoHostCall,
        Path::new("/nonexistent/manifest.json"),
        no_elevation,
    );
    assert!(res.is_ok());
}

#[test]
fn ensure_sealed_origin_attached_refuses_an_unreadable_sealed_configuration() {
    // verified test case: the host gate must not invent an origin identity when the
    // sealed configuration is missing or corrupt. Fail closed before any
    // host call.
    struct NoHostCall;
    impl CommandRunner for NoHostCall {
        fn run(&self, _command: &str, _args: &[&str]) -> Result<String, CascadeError> {
            panic!("an unreadable sealed configuration must not reach the host");
        }
    }
    TEST_ORIGIN_CONFIG
        .with(|cell| *cell.borrow_mut() = Some(Err("sealed origin manifest is missing".into())));
    let error = ensure_sealed_origin_attached_from(
        &NoHostCall,
        Path::new("/nonexistent/manifest.json"),
        no_elevation,
    )
    .expect_err("an unreadable sealed configuration must refuse");
    assert!(
        error
            .to_string()
            .contains("sealed origin manifest is missing")
    );
    TEST_ORIGIN_CONFIG.with(|cell| *cell.borrow_mut() = None);
}

#[test]
fn ensure_sealed_origin_attached_forwards_the_sealed_identity_to_one_bounded_attach() {
    // The boot gate and `up` must reach the same attach implementation.
    // This proves the gate entry point is not a stub: it reads the sealed
    // identity and drives the one bounded host mount.
    let dir = TestDir::new();
    let partuuid = "22222222-3333-4444-5555-666666666666";
    let absent_device = PathBuf::from(format!("/dev/disk/by-partuuid/{partuuid}"));
    let manifest = dir.path.join("origin-manifest.json");
    let contents = format!(
            r#"{{"origin_vhdx":"C:\\ProgramData\\RamShared\\ramshared-origin.vhdx","partuuid":"{partuuid}"}}"#
        )
        .into_bytes();
    fs::write(&manifest, &contents).expect("write manifest");
    let manifest_hash: String = Sha256::digest(&contents)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let sealed = sealed_origin_test_fixture()
        .replace(
            "origin_path=/dev/disk/by-partuuid/11111111-2222-4333-8444-555555555555",
            &format!("origin_path=/dev/disk/by-partuuid/{partuuid}"),
        )
        .replace(
            "partuuid=11111111-2222-4333-8444-555555555555",
            &format!("partuuid={partuuid}"),
        )
        .replace(
            "host_manifest_sha256=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            &format!("host_manifest_sha256={manifest_hash}"),
        );
    TEST_ORIGIN_CONFIG.with(|cell| *cell.borrow_mut() = Some(Ok(sealed)));

    struct MountRunner {
        calls: RefCell<Vec<String>>,
    }
    impl CommandRunner for MountRunner {
        fn run(&self, command: &str, args: &[&str]) -> Result<String, CascadeError> {
            self.calls.borrow_mut().push(command_label(command, args));
            Ok(String::new())
        }
    }
    let runner = MountRunner {
        calls: RefCell::new(Vec::new()),
    };
    let res = ensure_sealed_origin_attached_from(&runner, &manifest, fake_elevation);
    // The production device node cannot be created in a test, so the
    // bounded attach is issued and the presence check is what fails
    // closed. Either way the host call must have happened exactly once.
    assert!(res.is_err());
    assert!(
        absent_device.ends_with(partuuid),
        "the test must target the sealed partition identity"
    );
    let calls = runner.calls.borrow();
    assert_eq!(calls.len(), 1, "exactly one bounded host attach: {calls:?}");
    assert!(
        calls[0]
            .ends_with("--mount --vhd C:\\ProgramData\\RamShared\\ramshared-origin.vhdx --bare"),
        "the attach must be the sealed bare VHD mount: {calls:?}"
    );
    assert!(
        calls[0].starts_with(concat!(
            "/mnt/c/ProgramData/RamShared/bin/ramshared-host-launch.exe ",
            "--timeout-ms 10000 --sudo /fake/windows/sudo.exe -- wsl.exe --mount"
        )),
        "the attach must run through the windowless launcher and the Windows              elevation boundary: {calls:?}"
    );
    TEST_ORIGIN_CONFIG.with(|cell| *cell.borrow_mut() = None);
}

#[test]
// TestName: linux_device_number_encodes_glibc_major_minor_layout
fn linux_device_number_encodes_glibc_major_minor_layout() {
    // glibc gnu_dev_makedev layout, checked against gnu_dev_major/minor.
    assert_eq!(linux_device_number(0x0000_0000_0000_0000), "0:0");
    assert_eq!(linux_device_number(0x0000_0000_0000_0801), "8:1");
    assert_eq!(linux_device_number(0x0000_0000_0000_0821), "8:33");
    assert_eq!(linux_device_number(0x0000_0000_0000_0100), "1:0");
    assert_eq!(linux_device_number(0x0000_0000_0001_0000), "256:0");
}

#[test]
// TestName: parse_zram_device_accepts_exact_nodes_and_rejects_shapes
fn parse_zram_device_accepts_exact_nodes_and_rejects_shapes() {
    assert_eq!(parse_zram_device("/dev/zram0"), Some("/dev/zram0".into()));
    assert_eq!(
        parse_zram_device("  /dev/zram12\n"),
        Some("/dev/zram12".into())
    );
    assert_eq!(parse_zram_device("/dev/zram"), None);
    assert_eq!(parse_zram_device("/dev/zramx"), None);
    assert_eq!(parse_zram_device("/dev/zram0p1"), None);
    assert_eq!(parse_zram_device("/dev/nbd0"), None);
    assert_eq!(parse_zram_device(""), None);
}

#[test]
// TestName: exact_single_device_delta_admits_one_new_matching_kind
fn exact_single_device_delta_admits_one_new_matching_kind() {
    let nbd = bound_device_fixture("/dev/nbd0", ManagedDeviceKind::Nbd);
    let zram = bound_device_fixture("/dev/zram0", ManagedDeviceKind::Zram);
    let nbd7 = bound_device_fixture("/dev/nbd7", ManagedDeviceKind::Nbd);
    let nbd_only = std::slice::from_ref(&nbd);
    let zram_only = std::slice::from_ref(&zram);
    assert_eq!(
        exact_single_device_delta(
            &[nbd.clone(), zram.clone()],
            &[nbd.clone(), zram.clone(), nbd7.clone()],
            ManagedDeviceKind::Nbd
        ),
        Some(nbd7.clone())
    );
    assert_eq!(
        exact_single_device_delta(
            nbd_only,
            &[nbd.clone(), zram.clone()],
            ManagedDeviceKind::Nbd
        ),
        None,
        "the added device must be the requested kind"
    );
    assert_eq!(
        exact_single_device_delta(&[], nbd_only, ManagedDeviceKind::Nbd),
        Some(nbd.clone())
    );
    assert_eq!(
        exact_single_device_delta(nbd_only, nbd_only, ManagedDeviceKind::Nbd),
        None,
        "no delta is not a single addition"
    );
    assert_eq!(
        exact_single_device_delta(
            nbd_only,
            &[nbd.clone(), nbd.clone()],
            ManagedDeviceKind::Nbd
        ),
        None,
        "duplicates on either side invalidate the set"
    );
    assert_eq!(
        exact_single_device_delta(nbd_only, zram_only, ManagedDeviceKind::Nbd),
        None,
        "a replacement is not an addition"
    );
}

#[test]
// TestName: option_value_once_refuses_repeated_options
fn option_value_once_refuses_repeated_options() {
    let args = |values: &[&str]| {
        values
            .iter()
            .map(|value| (*value).to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        option_value_once(&args(&["--daemon", "ramsharedd"]), "--daemon"),
        Some("ramsharedd")
    );
    assert_eq!(option_value_once(&args(&[]), "--daemon"), None);
    assert_eq!(option_value_once(&args(&["--daemon"]), "--daemon"), None);
    assert_eq!(
        option_value_once(&args(&["--daemon", "a", "--daemon", "b"]), "--daemon"),
        None,
        "a duplicated option is ambiguous and must be refused"
    );
}

#[test]
// TestName: deleted_executable_path_strips_the_kernel_marker
fn deleted_executable_path_strips_the_kernel_marker() {
    assert_eq!(
        deleted_executable_path(Path::new("/usr/local/bin/ramsharedd (deleted)")),
        Some(PathBuf::from("/usr/local/bin/ramsharedd"))
    );
    assert_eq!(
        deleted_executable_path(Path::new("/usr/local/bin/ramsharedd")),
        None
    );
    assert_eq!(
        deleted_executable_path(Path::new("not-utf8-\u{FFFD} (deleted)")),
        Some(PathBuf::from("not-utf8-\u{FFFD}"))
    );
}

#[test]
// TestName: legacy_runtime_record_values_match_all_topologies
fn legacy_runtime_record_values_match_all_topologies() {
    // A missing record is vacuously consistent — it cannot contradict the
    // admitted topology. A present record must name that exact topology.
    assert!(legacy_runtime_record_values_match(
        None,
        None,
        None,
        42,
        "/dev/nbd0",
        None
    ));
    assert!(legacy_runtime_record_values_match(
        Some("42"),
        Some("/dev/nbd0"),
        None,
        42,
        "/dev/nbd0",
        None
    ));
    assert!(legacy_runtime_record_values_match(
        Some(" 42 "),
        Some(" /dev/nbd0 "),
        Some("/dev/zram0"),
        42,
        "/dev/nbd0",
        Some("/dev/zram0")
    ));
    assert!(
        legacy_runtime_record_values_match(None, None, None, 42, "/dev/nbd0", Some("/dev/zram0")),
        "an absent zram record cannot contradict an expected zram tier"
    );
    assert!(!legacy_runtime_record_values_match(
        Some("43"),
        None,
        None,
        42,
        "/dev/nbd0",
        None
    ));
    assert!(!legacy_runtime_record_values_match(
        None,
        Some("/dev/nbd1"),
        None,
        42,
        "/dev/nbd0",
        None
    ));
    assert!(
        !legacy_runtime_record_values_match(None, None, Some("/dev/zram0"), 42, "/dev/nbd0", None),
        "a zram record with no expected zram tier is a topology mismatch"
    );
}
