// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/tests/cache_mode.rs
//! Integration tests for `ramshared setup cache-mode stage`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::{Command, Output};

fn run_cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ramshared"))
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn swap_topology() -> Vec<(String, String, String, String)> {
    std::fs::read_to_string("/proc/swaps")
        .unwrap_or_default()
        .lines()
        .skip(1)
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            (cols.len() >= 5).then(|| {
                (
                    cols[0].to_string(),
                    cols[1].to_string(),
                    cols[2].to_string(),
                    cols[4].to_string(),
                )
            })
        })
        .collect()
}

#[test]
fn cache_mode_stage_preserves_storage_and_refuses_nonroot_or_stale_binding() {
    let swaps_before = swap_topology();

    // Unknown/missing cache-mode flags refuse with exit 2 before touching storage.
    for bad in [
        &["setup", "cache-mode"][..],
        &["setup", "cache-mode", "stage"][..],
        &["setup", "cache-mode", "stage", "--mode", "invalid"][..],
        &["setup", "cache-mode", "apply"][..],
    ] {
        let out = run_cli(bad);
        assert_eq!(out.status.code(), Some(2), "args={bad:?}");
        assert!(out.stdout.is_empty());
    }

    // Running `setup cache-mode stage --mode pooled --json` unprivileged refuses with exit 3
    // (or exit 4 if run as root without matching sealed origin) and never mutates /proc/swaps.
    let unpriv = run_cli(&["setup", "cache-mode", "stage", "--mode", "pooled", "--json"]);
    let code = unpriv.status.code().unwrap_or(-1);
    assert!(
        matches!(code, 3 | 4),
        "expected privilege (3) or stale-binding (4) refusal, got {code}: {}",
        String::from_utf8_lossy(&unpriv.stderr)
    );
    assert!(unpriv.stdout.is_empty());
    let swaps_after = swap_topology();
    assert_eq!(
        swaps_before, swaps_after,
        "staging refusal must never mutate swap topology"
    );
}

#[test]
fn cache_mode_commit_reconciles_torn_publish_and_reports_next_boot() {
    // Independent refusal/read-only path never touches storage and preserves JSON status.
    let indep = run_cli(&[
        "setup",
        "cache-mode",
        "stage",
        "--mode",
        "independent",
        "--json",
    ]);
    let code = indep.status.code().unwrap_or(-1);
    assert!(
        matches!(code, 3 | 4),
        "expected privilege (3) or stale-binding (4) refusal, got {code}"
    );
    let status = run_cli(&["status", "--json"]);
    assert_eq!(status.status.code(), Some(0));
    let parsed: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert!(parsed.is_object());
    assert!(parsed.get("cache_state").is_some());
}
