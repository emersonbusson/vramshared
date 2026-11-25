// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/tests/daemon_cli.rs
//! Nonactivating public daemon controls. Ordinary cases refuse before providers;
//! the opt-in private root fixture permits context-free metadata only.

#![allow(clippy::expect_used)]

use std::process::Command;

fn daemon(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ramsharedd"))
        .args(args)
        .output()
        .expect("start ramsharedd refusal child")
}

#[test]
fn internal_budget_probe_refuses_unowned_stdin_and_activation_arguments() {
    for args in [
        vec!["__gpu_budget_probe"],
        vec!["__gpu_budget_probe", "--force"],
        vec!["__gpu_budget_probe", "--origin-manifest", "/tmp/unowned"],
    ] {
        let output = daemon(&args);
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("[ramsharedd] error:"), "{stderr}");
        assert!(
            !stderr.contains("gpu_adapter_selected")
                && !stderr.contains("transmitting")
                && !stderr.contains("listening on"),
            "{stderr}"
        );
    }
}

const ROOT_METADATA_CASE: &str =
    "root_metadata_entry_proves_inherited_authority_in_private_namespace";
const ROOT_METADATA_NAMESPACE: &str = "RAMSHARED_METADATA_TEST_PARENT_NAMESPACE";

fn launch_private_metadata_case(namespace: u64) {
    let mut command = Command::new("sudo");
    command
        .args([
            "-n",
            "unshare",
            "--mount",
            "--pid",
            "--fork",
            "--kill-child",
            "--propagation",
            "private",
            "/usr/bin/env",
        ])
        .arg(format!(
            "{ROOT_METADATA_NAMESPACE}={namespace}:{}",
            rustix::process::getpgrp().as_raw_nonzero()
        ));
    if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
        let profile_path = std::path::Path::new(&profile);
        let target = std::env::var_os("CARGO_TARGET_DIR").expect("owned coverage target");
        assert!(profile_path.is_absolute());
        assert_eq!(
            profile_path.extension().and_then(|s| s.to_str()),
            Some("profraw")
        );
        assert!(
            profile_path
                .parent()
                .expect("profile parent")
                .canonicalize()
                .expect("existing profile parent")
                .starts_with(std::fs::canonicalize(target).expect("owned coverage directory"))
        );
        let mut assignment = std::ffi::OsString::from("LLVM_PROFILE_FILE=");
        assignment.push(profile);
        command.arg(assignment);
    }
    let output = command
        .arg(std::env::current_exe().expect("integration test image"))
        .args([
            "--exact",
            ROOT_METADATA_CASE,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .output()
        .expect("run private root metadata fixture");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
}

fn assert_private_metadata_namespace(namespace: u64, parent: &std::ffi::OsStr) {
    use std::os::unix::fs::MetadataExt;
    let (parent_namespace, parent_group) = parent
        .to_str()
        .expect("native launcher identity")
        .split_once(':')
        .expect("namespace/group identity");
    assert_eq!(rustix::process::geteuid().as_raw(), 0);
    assert_eq!(
        std::process::id(),
        1,
        "namespace init retains all descendants through exit"
    );
    assert_ne!(
        namespace.to_string(),
        parent_namespace,
        "never mount over the caller's /run"
    );
    // Keep the caller's proc mount: it exposes outer IDs for ancestry/group
    // checks while waitid/kill and the probe use their native PID namespace.
    let stat = std::fs::read_to_string("/proc/self/stat").expect("actual outer process group");
    assert_eq!(
        stat.rsplit_once(')')
            .expect("kernel comm field")
            .1
            .split_whitespace()
            .nth(2)
            .expect("kernel group field"),
        parent_group
    );
    let mut process = "self".to_string();
    let mut proven = false;
    for _ in 0..16 {
        let status = std::fs::read_to_string(format!("/proc/{process}/status"))
            .expect("actual launcher ancestry");
        let pid = status
            .lines()
            .find_map(|s| s.strip_prefix("PPid:"))
            .expect("kernel parent field")
            .trim()
            .parse::<u32>()
            .expect("native parent PID");
        assert!(pid > 0);
        let observed = std::fs::metadata(format!("/proc/{pid}/ns/mnt"))
            .expect("ancestor namespace")
            .ino();
        if observed != namespace {
            assert_eq!(observed.to_string(), parent_namespace);
            proven = true;
            break;
        }
        process = pid.to_string();
    }
    assert!(
        proven,
        "require a live ancestor in the caller's mount namespace"
    );
    let mountinfo =
        std::fs::read_to_string("/proc/self/mountinfo").expect("native mount propagation");
    assert!(mountinfo.lines().all(|line| {
        line.split(" - ")
            .next()
            .expect("mount fields")
            .split_whitespace()
            .skip(6)
            .all(|field| !field.starts_with("shared:") && !field.starts_with("master:"))
    }));
}

fn mount_private_metadata_runtime(namespace: u64, parent: &std::ffi::OsStr) {
    use std::os::unix::fs::DirBuilderExt;
    assert_private_metadata_namespace(namespace, parent);
    assert!(
        Command::new("mount")
            .args([
                "-t",
                "tmpfs",
                "-o",
                "mode=0755,size=1m",
                "ramshared-metadata-fixture",
                "/run"
            ])
            .status()
            .expect("mount private ephemeral runtime")
            .success()
    );
    std::fs::DirBuilder::new()
        .mode(0o755)
        .create("/run/ramshared")
        .expect("create private runtime");
}

fn exercise_private_metadata_entry() {
    use std::process::Stdio;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    let lease = ramshared_exec::inherited_lease::acquire(
        std::path::Path::new(ramshared_ipc::budget_observation::LEASE_PATH),
        0,
    )
    .expect("acquire private canonical probe lease");
    static OWNER: std::sync::OnceLock<ramshared_exec::retained_process::ServingCommandOwner> =
        std::sync::OnceLock::new();
    let owner = OWNER.get_or_init(Default::default);
    let mut probe = Command::new(env!("CARGO_BIN_EXE_ramsharedd"));
    probe.arg("__gpu_budget_probe").stdin(Stdio::from(lease));
    let result = owner.run_recorded(
        &mut probe,
        Duration::from_millis(ramshared_vram::GPU_BUDGET_MAX_AGE_MS / 2),
        ramshared_ipc::budget_observation::MAX_BYTES,
    );
    // A timeout never allows the namespace owner to abandon its actual child.
    while owner.try_shutdown().is_none() {
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = result.expect("native metadata process completed within its observation budget");
    assert!(output.output.stderr.is_empty());
    let now = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("current wall time")
            .as_millis(),
    )
    .expect("representable wall time");
    let report = ramshared_ipc::budget_observation::Report::decode(&output.output.stdout, now)
        .expect("bounded current metadata response");
    assert_eq!(report.pid, output.pid);
    match ramshared_block::platform::probe_platform().platform {
        Some(ramshared_block::platform::PlatformKind::NativeLinux) => {
            assert!(report.unavailable_reason.is_some() || !report.budgets.is_empty());
        }
        _ => {
            assert!(report.budgets.is_empty());
            assert!(
                report
                    .unavailable_reason
                    .as_deref()
                    .is_some_and(|s| s.contains("WSL allocator mapping"))
            );
        }
    }
}

#[test]
#[ignore = "metadata only; requires sudo and a private root mount namespace"]
fn root_metadata_entry_proves_inherited_authority_in_private_namespace() {
    use std::os::unix::fs::MetadataExt;
    let namespace = std::fs::metadata("/proc/self/ns/mnt")
        .expect("current mount namespace")
        .ino();
    match std::env::var_os(ROOT_METADATA_NAMESPACE) {
        None => launch_private_metadata_case(namespace),
        Some(parent) => {
            mount_private_metadata_runtime(namespace, &parent);
            exercise_private_metadata_entry();
        }
    }
}

#[test]
#[ignore = "nonactivating; requires a measured current WSL platform"]
fn standalone_ublk_public_entrypoint_refuses_before_native_startup_on_wsl() {
    use ramshared_block::platform::{PlatformKind, probe_platform};
    assert!(
        matches!(
            probe_platform().platform,
            Some(PlatformKind::Wsl1 | PlatformKind::Wsl2)
        ),
        "measured WSL prerequisite"
    );
    let output = daemon(&[
        "--transport",
        "ublk",
        "--backend",
        "ram",
        "--force",
        "--accept-wsl2-nbd",
    ]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("refused: standalone ublk on WSL"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("memory locked") && !stderr.contains("ublk device:"),
        "{stderr}"
    );
}

#[test]
fn daemon_process_refusals_exit_before_backend() {
    for args in [
        ["--unknown"].as_slice(),
        ["--backend", "ram"].as_slice(),
        ["--transport", "ublk", "--backend", "vulkan"].as_slice(),
        ["--slices", "1", "--slice-mb", "1"].as_slice(),
    ] {
        let output = daemon(args);
        assert_eq!(output.status.code(), Some(1), "args={args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("[ramsharedd] error:"),
            "args={args:?}: {stderr}"
        );
    }
}

#[test]
fn native_origin_public_args_refuse_noncanonical_or_mixed_sources_before_io() {
    let path = ramshared_config::native_origin::default_native_origin_path();
    let path = path.to_str().expect("canonical manifest path");
    for args in [
        vec!["--native-origin-manifest", "/tmp/not-a-protected-origin"],
        vec!["--native-origin-manifest", path, "--force"],
        vec!["--native-origin-manifest", path, "--transport", "ublk"],
        vec![
            "--native-origin-manifest",
            path,
            "--origin-manifest",
            "/tmp/mixed",
        ],
    ] {
        let output = daemon(&args);
        assert_eq!(output.status.code(), Some(1));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("[ramsharedd] error:"), "{stderr}");
        assert!(stderr.contains("native origin"), "{stderr}");
    }
}

#[test]
fn isolated_worker_public_entry_rejects_invalid_identity_and_descriptor_before_driver() {
    let missing = i32::MAX.to_string();
    for args in [
        vec!["__gpu_worker", "--fd", &missing, "--reserve-floor", "0"],
        vec![
            "__gpu_worker",
            "--fd",
            "7",
            "--reserve-floor",
            "0",
            "--adapter-identity",
            "{}",
        ],
        vec![
            "__gpu_worker",
            "--fd",
            "7",
            "--reserve-floor",
            "0",
            "--adapter-identity",
            "null",
        ],
    ] {
        let output = daemon(&args);
        assert_eq!(output.status.code(), Some(1));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("[ramsharedd] error:"), "{stderr}");
        assert!(!stderr.contains("gpu_adapter_selected"));
        assert!(!stderr.contains("budget rejected"));
    }
}

#[test]
fn daemon_version_prints_package_version_and_exits_zero() {
    let output = daemon(&["--version"]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with("ramsharedd "),
        "expected ramsharedd version banner, got: {stdout}"
    );
}
