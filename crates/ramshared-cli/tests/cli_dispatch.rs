// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/tests/cli_dispatch.rs
#![allow(clippy::unwrap_used)]

use std::fs;
use std::process::{Command, ExitStatus, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static TEMP_FILE_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn run_cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ramshared"))
        .args(args)
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_json_decision(status: &ExitStatus, value: &serde_json::Value) {
    let decision = value
        .get("decision")
        .and_then(serde_json::Value::as_str)
        .unwrap();
    assert!(matches!(decision, "ready" | "blocked"));
    assert_eq!(
        status.code(),
        Some(if decision == "ready" { 0 } else { 1 }),
        "the process exit must agree with its reported decision"
    );
}

fn assert_text_decision(status: &ExitStatus, text: &str) {
    let decision = text
        .lines()
        .find_map(|line| line.strip_prefix("Decision: "))
        .unwrap();
    assert!(matches!(decision, "ready" | "blocked"));
    assert_eq!(
        status.code(),
        Some(if decision == "ready" { 0 } else { 1 }),
        "the process exit must agree with its reported decision"
    );
}

#[test]
fn cli_help_and_unknown_command() {
    let help = run_cli(&["--help"]);
    assert_eq!(help.status.code(), Some(0));
    assert!(stderr(&help).contains("usage:"));

    let unknown = run_cli(&["not-a-command"]);
    assert_eq!(unknown.status.code(), Some(2));
    assert!(stderr(&unknown).contains("unsupported command: not-a-command"));
    assert!(stderr(&unknown).contains("usage:"));
}

#[test]
fn cli_resource_config_json_discovers_platform_resources_read_only() {
    let output = run_cli(&["config", "show", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert!(matches!(
        value["platform"].as_str(),
        Some("native_linux" | "wsl2")
    ));
    assert!(value["guest_memory"]["total_bytes"].is_number());
    assert!(value["swaps"].is_array());
    assert!(value["block_devices"].is_array());
    assert!(value["warnings"].is_array());
    assert!(value["gpu_budget_status"].is_string());
    assert!(value["observed_unix_ms"].as_u64().is_some());

    if value["platform"] == "wsl2" {
        assert!(value["windows"].is_object() || !value["warnings"].as_array().unwrap().is_empty());
        assert!(
            value["block_devices"]
                .as_array()
                .unwrap()
                .iter()
                .all(|device| device["eligible_for_file_storage"].as_bool() == Some(false)),
            "guest filesystem capacity must not imply host-volume capacity"
        );
        // The live mount table can produce different safe refusal reasons.
        // Deterministic inventory tests pin the host-volume binding contract.
    } else {
        assert!(value["windows"].is_null());
    }

    let mutation = run_cli(&["config", "apply"]);
    assert_eq!(mutation.status.code(), Some(2));
    assert!(stderr(&mutation).contains("invalid config option"));
}

#[test]
fn cli_resource_config_draft_refuses_non_tty_before_writing() {
    let path = std::env::temp_dir().join(format!(
        "ramshared-resource-draft-{}-{}.toml",
        std::process::id(),
        TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let path_text = path.to_string_lossy().into_owned();

    let output = run_cli(&["config", "draft", "--output", &path_text]);

    assert_ne!(output.status.code(), Some(0));
    assert!(stderr(&output).contains("needs a terminal"));
    assert!(
        !path.exists(),
        "non-interactive draft must not create a file"
    );
}

#[test]
fn cli_resource_config_plan_loads_an_explicit_profile_without_applying_it() {
    let path = std::env::temp_dir().join(format!(
        "ramshared-resource-profile-{}-{}.toml",
        std::process::id(),
        TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&path, "schema_version = 1\n[caps]\nzram_bytes = 0\n").unwrap();
    let path_text = path.to_string_lossy().into_owned();

    let output = run_cli(&["config", "plan", "--json", "--profile", &path_text]);
    let cleanup = fs::remove_file(&path);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        cleanup.is_ok(),
        "temporary profile was not modified or retained"
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema_version"].as_u64(), Some(1));
    assert_eq!(value["profile_state"].as_str(), Some("validated"));
    assert_eq!(
        value["status"].as_str(),
        Some("profile_loaded_no_storage_targets")
    );
    assert_eq!(
        value["unenforced_planned_caps"]["zram_bytes"].as_u64(),
        Some(0)
    );
    assert_eq!(value["writes_performed"].as_bool(), Some(false));
    assert_eq!(value["apply_enabled"].as_bool(), Some(false));
}

#[test]
fn cli_resource_config_benchmark_plan_and_refusals() {
    let invalid = run_cli(&["config", "benchmark", "--plan", "--confirm"]);
    assert_eq!(invalid.status.code(), Some(2));
    assert!(stderr(&invalid).contains("invalid config option"));

    let help = run_cli(&["--help"]);
    assert_eq!(help.status.code(), Some(0));
    assert!(stderr(&help).contains("ramshared config benchmark"));

    // Read-only `--plan` never writes and either prints preview or refuses cleanly on host preconditions.
    let plan = run_cli(&["config", "benchmark", "--plan"]);
    let code = plan.status.code().unwrap_or(1);
    let err = stderr(&plan);
    if code == 0 {
        let out = stdout(&plan);
        assert!(out.contains("Read-only preview. No files or settings were modified."));
    } else {
        assert!(
            err.contains("WSL2")
                || err.contains("no eligible native Linux")
                || err.contains("ramshared")
                || err.contains("active")
                || err.contains("pressure"),
            "unexpected benchmark --plan refusal detail: {err}"
        );
    }

    // Non-TTY interactive invocation (without --confirm) must refuse without writing.
    let non_tty = run_cli(&["config", "benchmark"]);
    assert_ne!(non_tty.status.code(), Some(0));
}

#[test]
fn cli_internal_benchmark_worker_executes_bounded_handshake_and_cleans_up() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};

    let vol_dir = std::env::temp_dir().join(format!(
        "ramshared-cli-bench-worker-{}-{}",
        std::process::id(),
        TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&vol_dir).unwrap();
    fs::set_permissions(&vol_dir, fs::Permissions::from_mode(0o700)).unwrap();

    // Resolve live mount_id and major:minor for `vol_dir` from `/proc/self/mountinfo`.
    let mountinfo = fs::read_to_string("/proc/self/mountinfo").unwrap();
    let canonical = fs::canonicalize(&vol_dir).unwrap();
    let canonical_str = canonical.to_string_lossy();
    let mut best_match: Option<(usize, u64, String)> = None;
    for line in mountinfo.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        if fields.len() < 5 {
            continue;
        }
        let mnt_id: u64 = fields[0].parse().unwrap();
        let maj_min = fields[2].to_string();
        let mnt_point = fields[4];
        let matched = mnt_point == "/"
            || canonical_str == mnt_point
            || canonical_str.starts_with(&format!("{mnt_point}/"));
        if matched && best_match.as_ref().is_none_or(|b| mnt_point.len() > b.0) {
            best_match = Some((mnt_point.len(), mnt_id, maj_min));
        }
    }
    let (_, mount_id, major_minor) = best_match.unwrap();
    let owner_uid: u32 = fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .and_then(|rest| rest.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_ramshared"))
        .arg("__config-benchmark-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    let req = serde_json::json!({
        "tx_id": "cli-e2e-worker",
        "round": 0,
        "volume": {
            "filesystem_uuid": "e2e-uuid",
            "device_identity": "wwn:e2e",
            "display_name": "e2e-vol",
            "mountpoint": canonical_str.as_ref(),
            "mount_id": mount_id,
            "major_minor": major_minor,
            "filesystem": "ext4",
            "free_bytes": 10_838_081_536u64,
            "required_free_bytes": 10_838_081_536u64,
            "capacity_observed_unix_ms": 1_760_000_000_000u64,
        },
        "temp_leaf_name": "bench-cli-e2e.bin",
        "sample_seed": 777u64,
        "byte_ceiling": 33_554_432u64,
        "owner_uid": owner_uid,
    });
    writeln!(stdin, "{req}").unwrap();
    stdin.flush().unwrap();

    let mut ready = String::new();
    reader.read_line(&mut ready).unwrap();
    assert_eq!(ready.trim(), "READY");

    writeln!(stdin, "GO").unwrap();
    stdin.flush().unwrap();
    drop(stdin);

    let mut reply_line = String::new();
    reader.read_line(&mut reply_line).unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "worker exited with {status:?}");

    let reply: serde_json::Value = serde_json::from_str(reply_line.trim()).unwrap();
    assert_eq!(reply["kind"].as_str(), Some("completed"));
    assert_eq!(reply["bytes_written"].as_u64(), Some(33_554_432));
    assert_eq!(reply["cleanup_verified"].as_bool(), Some(true));
    assert!(
        !vol_dir
            .join("var/lib/ramshared/resource-config/benchmark/bench-cli-e2e.bin")
            .exists(),
        "temp sample file must be cleaned up"
    );

    fs::remove_dir_all(&vol_dir).unwrap();
}

#[test]
fn cli_check_and_doctor_report_decision_json_and_text() {
    let check_json = run_cli(&["check", "--json"]);
    let check_value: serde_json::Value =
        serde_json::from_slice(&check_json.stdout).unwrap_or_else(|error| {
            panic!(
                "check JSON failed: {error}; status={:?}; stderr={}",
                check_json.status,
                stderr(&check_json)
            )
        });
    assert_json_decision(&check_json.status, &check_value);

    let check_text = run_cli(&["check"]);
    assert_text_decision(&check_text.status, &stdout(&check_text));

    let doctor_json = run_cli(&["doctor", "--json"]);
    let doctor_value: serde_json::Value = serde_json::from_slice(&doctor_json.stdout).unwrap();
    let doctor_check = doctor_value.get("check").unwrap();
    assert_json_decision(&doctor_json.status, doctor_check);
    assert!(
        doctor_value
            .get("recommendations")
            .and_then(serde_json::Value::as_array)
            .is_some()
    );

    let doctor_text = run_cli(&["doctor"]);
    assert_text_decision(&doctor_text.status, &stdout(&doctor_text));
    assert!(stdout(&doctor_text).contains("Recommendations:"));
}

#[test]
fn cli_status_flags_are_exact_and_read_only() {
    let status_json = run_cli(&["status", "--json"]);
    assert_eq!(status_json.status.code(), Some(0));
    let value: serde_json::Value = serde_json::from_slice(&status_json.stdout).unwrap();
    // The cascade status document's schema moves with its fields (the tier's
    // role and the cache's cause arrived with schema 5).
    assert_eq!(
        value
            .get("schema_version")
            .and_then(serde_json::Value::as_u64),
        Some(5)
    );
    assert!(
        value
            .get("phase")
            .and_then(serde_json::Value::as_str)
            .is_some()
    );
    assert!(
        value
            .get("order_ok")
            .and_then(serde_json::Value::as_bool)
            .is_some()
    );
    for plane in [
        "control_state",
        "cache_state",
        "origin_state",
        "guardian_state",
        "overall_state",
    ] {
        assert!(
            value
                .get(plane)
                .and_then(serde_json::Value::as_str)
                .is_some()
        );
    }

    for args in [
        &["status", "--unsafe"][..],
        &["status", "--json", "--json"][..],
    ] {
        let refusal = run_cli(args);
        assert_eq!(refusal.status.code(), Some(2));
        assert!(stderr(&refusal).contains("invalid status option"));
        assert!(refusal.stdout.is_empty());
    }
}

/// ITEM-7: the documented command must publish the health fragments that the
/// monitor's spliced document carries. Before this was wired, `status --json`
/// rendered only `render_status_json`, so every health key was absent from
/// the command the docs tell an operator to run.
#[test]
fn status_json_publishes_the_health_fragments() {
    let output = run_cli(&["status", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    for key in [
        "posture",
        "posture_reason",
        "tier_state",
        "spill_mib_per_cycle",
        "writeback_mib_per_s",
        "writeback_p99_ms",
        "consecutive_slow_cycles",
        "hardware_health",
        "mce_kill_count",
        "sigbus_mceerr_count",
        "whea_event_count",
        "health_errors",
    ] {
        assert!(
            value.get(key).is_some(),
            "ITEM-7 fragment `{key}` is missing: {value}"
        );
    }
    // The fragments are additive: the cascade document keeps its schema and
    // an absent health document is null, never a measured zero.
    assert_eq!(
        value
            .get("schema_version")
            .and_then(serde_json::Value::as_u64),
        Some(5)
    );
    assert!(value["health_errors"].is_array());
    assert!(
        value["tier_state"].is_null() || value["tier_state"].is_string(),
        "an absent pump document is null, not a fabricated state: {value}"
    );
}

#[test]
fn cli_monitor_once_emits_typed_read_only_observation() {
    let output = run_cli(&["monitor", "--jsonl", "--once"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema_version"], 5);
    assert_eq!(value["sample_age_ms"], 0);
    assert!(value["mem"]["available_kib"].as_u64().is_some());
    assert!(value["control_plane"]["memory_psi_full_avg10"].is_number());
    assert!(value["control_plane"]["memory_psi_full_avg60"].is_number());
    assert!(value["control_plane"]["memory_psi_full_avg300"].is_number());
    // The tier face is typed: `present` is the witness, and the figures only
    // exist when it is true. An absent tier publishes `null`, never a
    // measured `0` — so the assertion is the contract, not a bare `as_u64`
    // that an unlisted tier used to satisfy with a folded zero.
    let vram_tier = &value["tiers"]["vram"];
    assert!(
        vram_tier.get("used_kib").is_some(),
        "the tier face always names the figure: {vram_tier}"
    );
    if vram_tier["present"] == serde_json::json!(false) {
        assert!(
            vram_tier["used_kib"].is_null(),
            "an absent tier must publish null, never a measured zero: {vram_tier}"
        );
    } else {
        assert!(
            vram_tier["used_kib"].as_u64().is_some(),
            "a present tier publishes a number: {vram_tier}"
        );
    }
    assert!(value["activation"].get("disk_growth_kib").is_some());
}

#[test]
fn cli_diagnose_forwards_event_args() {
    let suffix = TEMP_FILE_COUNTER.fetch_add(1, Ordering::SeqCst);
    let path = std::env::temp_dir().join(format!(
        "ramshared-cli-dispatch-{}-{suffix}.jsonl",
        std::process::id()
    ));
    // Two events carrying a running total, so the counted demote is a real
    // 1 -> 2 step. A single line cannot produce a demote delta: the fold
    // needs two measurements, and this gate used to assert the one-line
    // fabrication that `c8ecac76` removed.
    fs::write(
        &path,
        "{\"t\":7,\"canario_demotes\":1,\"flag\":\"none\"}\n\
         {\"t\":8,\"canario_demotes\":2,\"flag\":\"none\"}\n",
    )
    .unwrap();

    let path_string = path.to_string_lossy().into_owned();
    let output = run_cli(&["diagnose", "--events", &path_string, "--json"]);
    let _ = fs::remove_file(&path);

    assert_eq!(output.status.code(), Some(0));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        value.get("samples").and_then(serde_json::Value::as_u64),
        Some(2)
    );
    assert_eq!(
        value.get("demotes").and_then(serde_json::Value::as_u64),
        Some(1)
    );
}

#[test]
fn cli_up_and_down_refuse_before_mutation() {
    let up_refusal = run_cli(&["up", "--zram", "invalid"]);
    assert_eq!(up_refusal.status.code(), Some(1));
    assert!(!stderr(&up_refusal).is_empty());

    let down_refusal = run_cli(&["down", "--force"]);
    assert_eq!(down_refusal.status.code(), Some(2));
    assert!(stderr(&down_refusal).contains("invalid down option"));
}

#[test]
fn cli_top_alias_and_refusals() {
    let top_once = run_cli(&["top", "--once", "--jsonl"]);
    assert_eq!(top_once.status.code(), Some(0));
    assert!(!stdout(&top_once).is_empty());

    let top_refusal = run_cli(&["top", "--bad-option"]);
    assert_eq!(top_refusal.status.code(), Some(2));
    assert!(stderr(&top_refusal).contains("invalid top option"));
}

#[test]
fn cli_stress_subcommand_and_json_report() {
    let output = run_cli(&[
        "stress",
        "--start",
        "1",
        "--target",
        "2",
        "--step",
        "1",
        "--interval-ms",
        "50",
        "--hold-sec",
        "0",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(0));
    let val: serde_json::Value =
        serde_json::from_slice(&output.stdout).unwrap_or(serde_json::Value::Null);
    assert_eq!(
        val.get("status").and_then(serde_json::Value::as_str),
        Some("INCONCLUSIVE")
    );
    assert!(val.get("reclaim_speed_gbs").is_some());
    assert!(val.get("avg_cycle_latency_ms").is_some());
    assert!(val.get("p50_cycle_latency_ms").is_some());
    assert!(val.get("p99_cycle_latency_ms").is_some());
    assert!(val.get("estimated_page_fault_lat_us").is_some());

    let refusal = run_cli(&["stress", "--invalid-flag"]);
    assert_eq!(refusal.status.code(), Some(2));
}

#[test]
fn cli_stress_battery_and_seismic_log() {
    let log_path = "/tmp/ramshared-seismic-battery-test.log";
    let output = run_cli(&[
        "stress",
        "--battery",
        "--start",
        "1",
        "--target",
        "2",
        "--step",
        "1",
        "--interval-ms",
        "50",
        "--hold-sec",
        "1",
        "--log",
        log_path,
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(0));
    let val: serde_json::Value =
        serde_json::from_slice(&output.stdout).unwrap_or(serde_json::Value::Null);
    assert_eq!(
        val.get("battery_mode").and_then(serde_json::Value::as_bool),
        Some(true)
    );
    assert!(val.get("peak_pressure_index").is_some());

    if let Ok(content) = fs::read_to_string(log_path) {
        assert!(content.contains("IDX"));
        let _ = fs::remove_file(log_path);
    }
}

#[test]
fn cli_stress_sustained_duration_json_contract() {
    let log_path = std::env::temp_dir().join(format!(
        "ramshared-sustained-cli-test-{}-{}.log",
        std::process::id(),
        TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let log_str = log_path.display().to_string();
    let output = run_cli(&[
        "stress",
        "--battery",
        "--start",
        "1",
        "--target",
        "1",
        "--step",
        "1",
        "--interval-ms",
        "50",
        "--hold-sec",
        "1",
        "--duration-sec",
        "1",
        "--log",
        &log_str,
        "--json",
    ]);
    let _ = fs::remove_file(&log_path);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let val: serde_json::Value =
        serde_json::from_slice(&output.stdout).unwrap_or(serde_json::Value::Null);
    assert_eq!(
        val.get("duration_sec").and_then(serde_json::Value::as_u64),
        Some(1)
    );
    assert!(
        val.get("elapsed_sec")
            .and_then(serde_json::Value::as_f64)
            .is_some_and(|secs| secs >= 0.8),
        "expected elapsed_sec >= 0.8: {val}"
    );
    for key in [
        "tier1_avg_throughput_mbs",
        "tier1_read_mb",
        "tier1_write_mb",
        "tier2_nbd_avg_throughput_mbs",
        "tier2_nbd_read_mb",
        "tier2_nbd_write_mb",
        "tier3_avg_throughput_mbs",
        "tier3_read_mb",
        "tier3_write_mb",
        "gpu_cache_telemetry",
    ] {
        assert!(
            val.get(key).is_some(),
            "missing sustained telemetry key `{key}` in {val}"
        );
    }

    let refusal = run_cli(&["stress", "--duration-sec", "0"]);
    assert_eq!(refusal.status.code(), Some(2));
}

#[test]
fn cli_diagnose_errors_semantic_codes() {
    let missing_args = run_cli(&["diagnose"]);
    assert_eq!(missing_args.status.code(), Some(22)); // EINVAL

    let missing_file = run_cli(&["diagnose", "--events", "/does/not/exist.jsonl"]);
    assert_eq!(missing_file.status.code(), Some(2)); // ENOENT

    let invalid_flag = run_cli(&["diagnose", "--invalid-flag"]);
    assert_eq!(invalid_flag.status.code(), Some(22)); // EINVAL

    use std::os::unix::fs::PermissionsExt;
    let path = std::env::temp_dir().join(format!(
        "ramshared-test-no-perm-{}.jsonl",
        std::process::id()
    ));
    std::fs::write(&path, "{}").unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o000);
    std::fs::set_permissions(&path, perms).unwrap();
    let permission_denied = run_cli(&["diagnose", "--events", path.to_str().unwrap()]);
    assert_eq!(permission_denied.status.code(), Some(13)); // EACCES
    let _ = std::fs::remove_file(path);
}

/// `diagnose export` is a new command surface, so every refusal it can raise —
/// including a malformed request — reports through the PRD mapping. The shared
/// errno-style codes belong to the existing `diagnose --events` form and must
/// not leak onto the new one (SPEC: "New command exits use the PRD mapping").
#[test]
fn new_command_refusals_use_the_customer_exit_mapping() {
    for args in [
        vec!["setup", "reconcile"],
        vec![
            "setup",
            "reconcile",
            "--plan",
            "p",
            "--approval",
            "a",
            "--force",
        ],
    ] {
        let refused = run_cli(&args);
        assert_eq!(refused.status.code(), Some(2));
        assert!(stderr(&refused).contains("unsupported_surface"));
    }
    // Missing --output: a request the surface cannot serve, exit 2.
    let missing_output = run_cli(&["diagnose", "export"]);
    assert_eq!(missing_output.status.code(), Some(2));
    assert!(
        stderr(&missing_output).contains("unsupported_surface"),
        "the refusal must name its reason, not just exit: {}",
        stderr(&missing_output)
    );

    // --output with no value.
    let missing_value = run_cli(&["diagnose", "export", "--output"]);
    assert_eq!(missing_value.status.code(), Some(2));

    // An unknown flag is still an unsupported request, never EINVAL 22.
    let unknown_flag = run_cli(&["diagnose", "export", "--bogus"]);
    assert_eq!(unknown_flag.status.code(), Some(2));
    assert!(!stderr(&unknown_flag).contains("22"));

    // The existing surface keeps its own contract unchanged.
    let existing_missing = run_cli(&["diagnose"]);
    assert_eq!(existing_missing.status.code(), Some(22));
    let existing_flag = run_cli(&["diagnose", "--invalid-flag"]);
    assert_eq!(existing_flag.status.code(), Some(22));
}

/// Plan parsing retains the customer exit vocabulary and cannot accept path-
/// shaped profile labels. Native apply has separate provider admission tests.
#[test]
fn setup_plan_surface_maps_request_errors_like_the_prd() {
    // Bare `setup` is a usage refusal, not a default plan.
    let bare = run_cli(&["setup"]);
    assert_eq!(bare.status.code(), Some(2));
    assert!(
        stderr(&bare).contains("unsupported_surface"),
        "the refusal must name its reason: {}",
        stderr(&bare)
    );

    // `plan` with no options names the missing required options.
    let no_options = run_cli(&["setup", "plan"]);
    assert_eq!(no_options.status.code(), Some(2));
    let missing_profile = run_cli(&["setup", "plan", "--output", "plan.json"]);
    assert_eq!(missing_profile.status.code(), Some(2));
    let missing_output = run_cli(&["setup", "plan", "--profile", "resource-profile"]);
    assert_eq!(missing_output.status.code(), Some(2));

    // An unknown flag is an unsupported request on this surface, never 22.
    let unknown_flag = run_cli(&[
        "setup",
        "plan",
        "--profile",
        "resource-profile",
        "--output",
        "plan.json",
        "--bogus",
    ]);
    assert_eq!(unknown_flag.status.code(), Some(2));
    assert!(
        !stderr(&unknown_flag).contains("22"),
        "the new surface must not leak the errno vocabulary: {}",
        stderr(&unknown_flag)
    );

    // A path-shaped profile name is refused before any resolution, so argv can
    // never carry a device path or an executable into the setup surface.
    for name in ["../escape", "/etc/ramshared/resource-profile", "a/b", ""] {
        let path_shaped = run_cli(&["setup", "plan", "--profile", name, "--output", "plan.json"]);
        assert_eq!(
            path_shaped.status.code(),
            Some(2),
            "profile name {name:?} must be refused"
        );
        assert!(
            stderr(&path_shaped).contains("unsupported_surface"),
            "profile name {name:?} must name its reason: {}",
            stderr(&path_shaped)
        );
    }

    // The documented happy shape parses. A profile that is not configured on
    // this host is refused later (exit 6 for absence, exit 2 for an
    // untrusted root) rather than at parse time — and never with the errno
    // vocabulary. The output goes to a temp path so a surprise success cannot
    // litter the working tree.
    let output = std::env::temp_dir().join(format!(
        "ramshared-cli-dispatch-setup-plan-{}.json",
        std::process::id()
    ));
    let documented = run_cli(&[
        "setup",
        "plan",
        "--profile",
        "absent-cli-dispatch-profile",
        "--output",
        output.to_str().unwrap(),
    ]);
    let code = documented.status.code().unwrap_or(1);
    assert_ne!(code, 0, "a profile that is not configured must refuse");
    assert_ne!(code, 22, "the new surface never uses the errno vocabulary");
    assert!(
        code == 2 || code == 6,
        "unexpected setup plan exit {code}: {}",
        stderr(&documented)
    );
    let _ = std::fs::remove_file(&output);
}

/// `service` is a new command surface and it admits exactly three subcommands:
/// `enable-boot --approval <file> [--json]`, `disable-boot [--json]` and
/// `status [--json]`. Every other request is `unsupported_surface` (exit 2),
/// never the shared errno vocabulary and never exit zero.
///
/// `status` is the one request this test may issue against the live host: it is
/// read-only, so it either answers with an observed boot set or reports
/// `observation_unavailable` (exit 6). It never prints an `off` policy it did
/// not observe — a missing sealed unit is a broken installation, not a
/// recorded off.
#[test]
fn service_surface_refuses_everything_but_three_subcommands_and_never_reports_a_false_policy() {
    // Bare `service` is a usage refusal, not a default status.
    let bare = run_cli(&["service"]);
    assert_eq!(bare.status.code(), Some(2));
    assert!(
        stderr(&bare).contains("unsupported_surface"),
        "the refusal must name its reason: {}",
        stderr(&bare)
    );

    // An unknown subcommand and an unknown flag are unsupported requests on
    // this surface, never EINVAL 22 and never exit zero.
    for refused in [
        vec!["service", "bogus"],
        vec!["service", "status", "--stop"],
        vec!["service", "disable-boot", "--json", "--json"],
        vec!["service", "enable-boot"],
        vec!["service", "enable-boot", "--approval"],
    ] {
        let output = run_cli(&refused);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{refused:?} must be unsupported_surface: {}",
            stderr(&output)
        );
        assert!(
            stderr(&output).contains("unsupported_surface"),
            "{refused:?} must name its reason: {}",
            stderr(&output)
        );
        assert!(
            !stderr(&output).contains("22"),
            "the new surface must not leak the errno vocabulary: {}",
            stderr(&output)
        );
    }

    // `status` is read-only and safe on any host. A host with the sealed boot
    // set answers 0 and reports what it observed; a host that cannot observe
    // the set reports observation_unavailable (exit 6). Neither outcome may
    // claim an `off` policy that was not observed.
    let status = run_cli(&["service", "status", "--json"]);
    let code = status.status.code().unwrap_or(1);
    assert!(
        code == 0 || code == 6,
        "unexpected service status exit {code}: {}",
        stderr(&status)
    );
    if code == 0 {
        let value: serde_json::Value = serde_json::from_str(&stdout(&status)).unwrap();
        assert_eq!(
            value
                .get("live_tier_untouched")
                .and_then(serde_json::Value::as_bool),
            Some(true),
            "a policy change never touches a live tier: {}",
            stdout(&status)
        );
        let boot_set = value
            .get("boot_set")
            .and_then(serde_json::Value::as_str)
            .unwrap();
        assert!(
            matches!(boot_set, "on" | "off" | "half"),
            "the boot set is reported as it was observed: {}",
            stdout(&status)
        );
        // The three enablement bits are observed independently and must agree
        // with the derived set name. A `boot_set` that disagrees with its own
        // members is a rendering lie, not an observation.
        let bits: Vec<bool> = ["cascade_enabled", "supervisor_enabled", "host_gate_enabled"]
            .iter()
            .map(|key| {
                value
                    .get(*key)
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or_else(|| panic!("{key} must be an observed boolean"))
            })
            .collect();
        let all_on = bits.iter().all(|on| *on);
        let all_off = bits.iter().all(|on| !*on);
        match boot_set {
            "on" => assert!(all_on, "an `on` set has every member enabled: {bits:?}"),
            "off" => assert!(all_off, "an `off` set has every member disabled: {bits:?}"),
            _ => assert!(
                !all_on && !all_off,
                "a `half` set is exactly a disagreement: {bits:?}"
            ),
        }
    } else {
        assert!(
            stderr(&status).contains("observation_unavailable"),
            "an unobservable policy keeps the stable PRD reason: {}",
            stderr(&status)
        );
        assert!(
            !stdout(&status).contains("\"policy\":\"off\""),
            "a missing observation must never render as an off policy: {}",
            stdout(&status)
        );
    }
}

#[test]
fn cli_up_plan_is_read_only_and_reports_no_effects() {
    let output = run_cli(&["up", "--plan", "--json"]);
    assert!(
        output.status.success(),
        "up --plan must succeed: {}",
        stderr(&output)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema_version"], serde_json::json!(1));
    assert_eq!(value["action"], serde_json::json!("up-plan"));
    assert_eq!(value["effects"], serde_json::json!([]));
    assert!(value["swap"].is_array());
    assert!(value["zram"].is_array());
}

#[test]
fn cli_install_status_and_refusals() {
    let bad = run_cli(&["install", "--unknown"]);
    assert_eq!(bad.status.code(), Some(2));
    assert!(stderr(&bad).contains("NBD_INSTALL_REASON=UNSUPPORTED_OPTION"));

    let conflict = run_cli(&["install", "--plan", "--auto-update"]);
    assert_eq!(conflict.status.code(), Some(2));
    assert!(stderr(&conflict).contains("NBD_INSTALL_REASON=CONFLICTING_MODES"));

    let status = run_cli(&[
        "install",
        "--status",
        "--json",
        "--policy",
        "/var/nonexistent-ramshared-policy/auto-update.conf",
        "--bundle-root",
        "/var/nonexistent-ramshared-bundles",
    ]);
    assert_eq!(status.status.code(), Some(0), "{}", stderr(&status));
    let status_json: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(
        status_json["policy"].as_str(),
        Some("/var/nonexistent-ramshared-policy/auto-update.conf")
    );
    assert!(status_json["enabled"].is_null());

    let no_policy = run_cli(&[
        "install",
        "--check",
        "--policy",
        "/var/nonexistent-ramshared-policy/auto-update.conf",
    ]);
    assert_eq!(no_policy.status.code(), Some(3));
    assert!(stderr(&no_policy).contains("auto-update"));

    let no_bundle = run_cli(&[
        "install",
        "--plan",
        "--bundle-root",
        "/var/nonexistent-ramshared-bundles",
    ]);
    assert_eq!(no_bundle.status.code(), Some(6));
    assert!(stderr(&no_bundle).contains("NBD_INSTALL_REASON=BUNDLE_NOT_FOUND"));
    assert!(stderr(&no_bundle).contains("NBD_INSTALL_STATE=REFUSED"));
}

#[test]
fn cli_package_check_quiescent_argv_and_privilege_contract() {
    let missing = run_cli(&["package"]);
    assert_eq!(missing.status.code(), Some(2));
    assert!(stderr(&missing).contains("check-quiescent"));

    let bogus = run_cli(&["package", "check-quiescent", "--action", "bogus"]);
    assert_eq!(bogus.status.code(), Some(2));

    let valid = run_cli(&[
        "package",
        "check-quiescent",
        "--action",
        "upgrade",
        "--json",
    ]);
    let code = valid.status.code().unwrap_or(99);
    if !stderr(&valid).contains("requires root privileges") {
        assert!(
            matches!(code, 0 | 1 | 3 | 4 | 5 | 6),
            "unexpected code {code}: {}",
            stderr(&valid)
        );
    } else {
        assert_eq!(code, 3);
    }
}
