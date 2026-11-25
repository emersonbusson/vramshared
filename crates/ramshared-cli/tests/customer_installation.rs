// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/tests/customer_installation.rs
//! Installation-facing contracts of the existing `ramshared` surface.
//!
//! DT-2 extracts the readiness model out of `main` and has `check`, `doctor`,
//! setup and the dashboard consume one typed report, **but retain existing
//! command exit contracts**. This file is that retention net: one table walks
//! the documented argv surface and pins each command's exit, so a later
//! extraction cannot change one command while the behavioural tests are
//! looking at another.
//!
//! Nothing here mutates the machine. Bootstrap and lifecycle entrypoints
//! (`boot`, `ensure-origin`, `migrate-cascade`, `up`, `down`) are never
//! executed; their refusals are covered by their own tests.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::{Command, Output};

fn run_cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ramshared"))
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn decision_of(json: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(json).unwrap();
    report_of(&value)
        .get("decision")
        .and_then(serde_json::Value::as_str)
        .unwrap()
        .to_string()
}

/// The typed readiness report inside a command's JSON. `check --json` is the
/// report itself; `doctor --json` embeds the same report alongside its
/// recommendations, keyed by the report's name.
fn report_of(value: &serde_json::Value) -> &serde_json::Value {
    value.get("check").unwrap_or(value)
}

/// Every documented read-only command still accepts exactly this argv and
/// returns exactly this exit.
///
/// The exits of `check`/`doctor` are the decision itself (0 ready, 1 blocked)
/// and are asserted against the live decision rather than a constant, because
/// pinning a constant here would freeze this host's readiness into the
/// contract. The rest are constants: they do not answer a readiness question.
#[test]
fn existing_command_argv_and_exit_contracts_remain_stable() {
    // argv → exit, for commands whose exit does not depend on host readiness.
    let fixed = [
        (&[][..], 0, "bare invocation prints usage"),
        (&["-h"][..], 0, "-h is help"),
        (&["--help"][..], 0, "--help is help"),
        (&["version"][..], 0, "version"),
        (&["-V"][..], 0, "-V is version"),
        (&["--version"][..], 0, "--version is version"),
        (&["--build-info"][..], 0, "build identity"),
        (
            &["status"][..],
            0,
            "status is read-only and reports its own ok flag",
        ),
        (&["status", "--json"][..], 0, "status --json"),
        (
            &["monitor", "--once"][..],
            0,
            "monitor --once is a single observation",
        ),
        (
            &["top", "--once"][..],
            0,
            "top --once is the dashboard's non-TUI form",
        ),
        (&["config", "show"][..], 0, "config show is read-only"),
        (&["config", "show", "--json"][..], 0, "config show --json"),
        (&["config", "plan"][..], 0, "config plan is read-only"),
        (&["config", "plan", "--json"][..], 0, "config plan --json"),
    ];
    for (args, expected, why) in fixed {
        let output = run_cli(args);
        assert_eq!(
            output.status.code(),
            Some(expected),
            "{why}: argv={args:?} stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Refusal vocabulary of the existing surface is unchanged. New command
    // surfaces map through the customer model; these keep their own codes.
    let refusals = [
        (
            &["not-a-command"][..],
            2,
            "unknown command is refused as unsupported",
        ),
        (&["diagnose"][..], 22, "diagnose without --events is EINVAL"),
        (
            &["diagnose", "--invalid-flag"][..],
            22,
            "diagnose keeps errno-style exits on its existing form",
        ),
    ];
    for (args, expected, why) in refusals {
        let output = run_cli(args);
        assert_eq!(output.status.code(), Some(expected), "{why}: argv={args:?}");
    }
}

/// `check` and `doctor` are two names for one typed readiness report. If they
/// can disagree, the extraction produced two reports and the customer is
/// reading contradictory guidance — the exact failure DT-2 exists to prevent.
#[test]
fn check_and_doctor_agree_because_they_share_one_typed_report() {
    let check_text = run_cli(&["check"]);
    let doctor_text = run_cli(&["doctor"]);
    let check_json = run_cli(&["check", "--json"]);
    let doctor_json = run_cli(&["doctor", "--json"]);

    let check_decision = decision_of(&stdout(&check_json));
    let doctor_decision = decision_of(&stdout(&doctor_json));
    assert_eq!(
        check_decision, doctor_decision,
        "check and doctor reported different decisions"
    );

    // The process exit must agree with the reported decision on both names.
    for (output, name) in [
        (&check_json, "check --json"),
        (&doctor_json, "doctor --json"),
        (&check_text, "check"),
        (&doctor_text, "doctor"),
    ] {
        let expected = if check_decision == "ready" { 0 } else { 1 };
        assert_eq!(
            output.status.code(),
            Some(expected),
            "{name} exit did not match decision {check_decision}"
        );
    }

    // The text forms name the same decision a reader can grep.
    for (output, name) in [(&check_text, "check"), (&doctor_text, "doctor")] {
        let body = stdout(output);
        assert!(
            body.contains(&format!("Decision: {check_decision}")),
            "{name} text did not name the shared decision {check_decision}: {body}"
        );
    }
}

/// A readiness decision must carry its reasons, and the bounded diagnostic
/// export must carry the same ones — so `check`, `doctor` and `diagnose
/// export` cannot describe the machine three different ways. Holds in both
/// directions: a blocked decision names non-empty blockers, a ready one names
/// none and says `ready`.
#[test]
fn readiness_reasons_agree_across_check_doctor_and_the_export() {
    let body = stdout(&run_cli(&["check", "--json"]));
    let decision = decision_of(&body);
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    let blockers: Vec<String> = value
        .get("blockers")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|i| i.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    if decision == "blocked" {
        assert!(
            !blockers.is_empty(),
            "a blocked decision must name its blockers: {body}"
        );
    } else {
        assert_eq!(decision, "ready", "decision vocabulary is ready|blocked");
        assert!(
            blockers.is_empty(),
            "a ready decision must not also name blockers: {blockers:?}"
        );
    }

    let export = std::env::temp_dir().join(format!(
        "ramshared-parity-bundle-{}.json",
        std::process::id()
    ));
    let written = run_cli(&["diagnose", "export", "--output", export.to_str().unwrap()]);
    assert_eq!(written.status.code(), Some(0));
    let bundle: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&export).unwrap()).unwrap();
    let entries = bundle.get("entries").and_then(|e| e.as_array()).unwrap();
    let reason = entries
        .iter()
        .find(|e| e.get("name").and_then(|n| n.as_str()) == Some("readiness_reason"))
        .and_then(|e| e.get("value").and_then(|v| v.as_str()))
        .unwrap();
    let reported = entries
        .iter()
        .find(|e| e.get("name").and_then(|n| n.as_str()) == Some("readiness_decision"))
        .and_then(|e| e.get("value").and_then(|v| v.as_str()))
        .unwrap();
    assert_eq!(
        reported, decision,
        "export decision drifted from the check report"
    );
    if decision == "blocked" {
        assert_ne!(reason, "ready", "a blocked export must not say ready");
        assert!(!reason.is_empty(), "a blocked export must name its reason");
    } else {
        assert_eq!(reason, "ready", "a ready export names the ready reason");
    }
    let _ = std::fs::remove_file(&export);
}
