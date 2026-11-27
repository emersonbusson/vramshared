// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/tests/workload_admission.rs
#![allow(clippy::unwrap_used)]

use std::process::Command;

#[test]
fn unprivileged_workload_commands_explain_the_authority_boundary() {
    if rustix::process::geteuid().is_root() {
        return;
    }

    for action in ["run", "session"] {
        let output = Command::new(env!("CARGO_BIN_EXE_ramshared"))
            .args([action, "--class", "batch", "--", "/bin/echo", "unexpected"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty(), "{action} launched its workload");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("workload admission requires root privileges"),
            "{action}: {stderr}"
        );
    }
}

#[test]
fn invalid_workload_arguments_report_usage_before_checking_authority() {
    let output = Command::new(env!("CARGO_BIN_EXE_ramshared"))
        .args(["run", "--", "/bin/true"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--class is required"));
}
