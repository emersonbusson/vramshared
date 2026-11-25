// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-exec/src/bounded_process/discovery_tests.rs
//! Bounded discovery custody moved from the retired CLI kernel-command adapter.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use std::fs;
use std::path::Path;

#[test]
fn probe_failure_is_unknown_not_zero_capacity() {
    let mut missing = Command::new("/nonexistent/ramshared-probe");
    let failed = run_capture_command(
        &mut missing,
        "missing probe",
        Duration::from_millis(200),
        1024,
        |_| {},
    );
    assert!(
        failed.is_err(),
        "missing observation cannot be a successful empty output"
    );
    let mut slow = Command::new("sh");
    slow.args(["-c", "exec sleep 30"]);
    let timed_out = run_capture_command(
        &mut slow,
        "slow probe",
        Duration::from_millis(100),
        1024,
        |_| {},
    );
    assert!(
        timed_out.is_err(),
        "timeout cannot be a successful zero-capacity sample"
    );
}

#[test]
fn discovery_timeout_reaps_owned_children_without_touching_backend() {
    let root = std::env::temp_dir().join(format!("ramshared-discovery-{}", std::process::id()));
    fs::create_dir(&root).expect("fixture dir");
    let backend = root.join("backend-marker");
    fs::write(&backend, b"untouched").expect("backend marker");
    let pid_file = root.join("child.pid");
    let mut command = Command::new("sh");
    command.args(["-c", "echo $$ > \"$1\"; exec sleep 30", "sh"]);
    command.arg(&pid_file);
    let observed = run_capture_command(
        &mut command,
        "discovery fixture",
        Duration::from_millis(200),
        1024,
        |_| {},
    );
    assert!(observed.is_err());
    let pid_text = fs::read_to_string(&pid_file).expect("owned child recorded pid");
    let pid: i32 = pid_text.trim().parse().expect("child pid");
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "timed-out discovery child must be reaped"
    );
    assert_eq!(fs::read(&backend).expect("backend marker"), b"untouched");
    fs::remove_dir_all(root).unwrap();
}
