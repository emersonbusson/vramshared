// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/benchmark/windows_host.rs
//! WSL2 Windows host storage benchmark delegation, persisted diagnostic store,
//! and `ramshared setup recommend` flow (`SPEC.md` DT-27..DT-28).

use std::collections::HashSet;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[path = "recommend.rs"]
pub(crate) mod recommend;
pub(crate) use recommend::run_setup_recommend;

use super::model::{
    BENCHMARK_REQUIRED_FREE_BYTES, BenchmarkPlan, BenchmarkRecommendation, BenchmarkVolume,
    PER_VOLUME_WRITE_CAP_BYTES, ROUNDS_PER_VOLUME, RUN_WRITE_CAP_BYTES, SAMPLE_PAYLOAD_BYTES,
    VOLUME_WORKER_DEADLINE, WorkerSampleOutcome, recommend_leader, summarize_volume,
};
use super::{BenchmarkAction, BenchmarkReport, BenchmarkRequest, BenchmarkUseCase};
use crate::operation_progress::{OperationKind, OperationProgressGuard, ProgressUpdate};
use crate::resource_config::{InventoryError, ResourceSnapshot, RuntimePlatform, unix_millis};

pub(crate) const LATEST_BENCHMARK_PATH: &str = "/var/lib/ramshared/resource-benchmark-latest.json";
pub(crate) const LATEST_BENCHMARK_PATH_ENV: &str = "RAMSHARED_BENCHMARK_LATEST_PATH";
pub(crate) const WINSVC_EXE_ENV: &str = "RAMSHARED_WINSVC_EXE";
const WIN_SUDO_ENV: &str = "WIN_SUDO";
const DEFAULT_WIN_SUDO_PATH: &str = "/mnt/c/Windows/System32/sudo.exe";
const DEFAULT_WIN_REG_PATH: &str = "/mnt/c/Windows/System32/reg.exe";
const MAX_DIAGNOSTIC_BYTES: u64 = 64 * 1024;
const MAX_WINDOWS_BENCHMARK_STDOUT_BYTES: usize = 4 * 1024 * 1024;
const WINDOWS_BENCHMARK_TIMEOUT: Duration = Duration::from_secs(600);
static LATEST_TMP_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum WindowsWinsvcTarget {
    Direct(PathBuf),
    ElevatedWindowsPath(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct PersistedVolumeDiagnostic {
    pub(crate) stable_id: String,
    pub(crate) filesystem_uuid: String,
    pub(crate) device_identity: String,
    pub(crate) display_name: String,
    pub(crate) sequential_median_mibs_per_sec: f64,
    pub(crate) sequential_stddev_mibs_per_sec: f64,
    pub(crate) small_write_median_us: f64,
    pub(crate) small_write_p99_us: f64,
    pub(crate) total_bytes_written: u64,
    pub(crate) cleanup_verified: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct PersistedRecommendationDiagnostic {
    pub(crate) use_case: BenchmarkUseCase,
    pub(crate) status: String,
    pub(crate) recommended_stable_id: Option<String>,
    pub(crate) rationale: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct LatestBenchmarkDiagnostic {
    pub(crate) schema_version: u32,
    pub(crate) recorded_at_unix_ms: u64,
    pub(crate) platform: String,
    pub(crate) tx_id: String,
    pub(crate) plan_hash: String,
    pub(crate) volumes: Vec<PersistedVolumeDiagnostic>,
    pub(crate) recommendation: PersistedRecommendationDiagnostic,
}

#[derive(Debug, Deserialize)]
struct WindowsVolumeCandidateWire {
    drive_letter: String,
    label: Option<String>,
    filesystem: String,
    free_bytes: u64,
    volume_id: String,
}

#[derive(Debug, Deserialize)]
struct WindowsSampleOutcomeWire {
    seq_elapsed_ns: u64,
    small_write_latencies_ns: Vec<u64>,
    bytes_written: u64,
    seq_bytes_written: u64,
    small_write_count: usize,
    expected_sha256: String,
    observed_sha256: String,
    cleanup_verified: bool,
}

#[derive(Debug, Deserialize)]
struct WindowsVolumeBenchmarkResultWire {
    drive_letter: String,
    label: Option<String>,
    filesystem: String,
    free_bytes: u64,
    volume_id: String,
    cleanup_verified: bool,
    samples: Vec<WindowsSampleOutcomeWire>,
}

#[derive(Debug, Deserialize)]
struct WindowsBenchmarkReportWire {
    schema_version: u32,
    tx_id: String,
    plan_hash: String,
    #[allow(dead_code)]
    use_case: BenchmarkUseCase,
    rounds: usize,
    writes_performed: bool,
    cleanup_verified: bool,
    eligible_volumes: Vec<WindowsVolumeCandidateWire>,
    volumes: Vec<WindowsVolumeBenchmarkResultWire>,
}

pub(crate) fn resolve_latest_benchmark_path() -> PathBuf {
    match env::var_os(LATEST_BENCHMARK_PATH_ENV) {
        Some(val) if !val.is_empty() => PathBuf::from(val),
        _ => PathBuf::from(LATEST_BENCHMARK_PATH),
    }
}

pub(super) fn try_persist_latest_report(report: &BenchmarkReport) {
    if !report.writes_performed || report.volumes.is_empty() || !report.cleanup_verified {
        return;
    }
    let diag = diagnostic_from_report(report, unix_millis());
    let _ = persist_latest_diagnostic_at(&resolve_latest_benchmark_path(), &diag);
}

pub(crate) fn diagnostic_from_report(
    report: &BenchmarkReport,
    recorded_at_unix_ms: u64,
) -> LatestBenchmarkDiagnostic {
    LatestBenchmarkDiagnostic {
        schema_version: 1,
        recorded_at_unix_ms,
        platform: report.platform.to_string(),
        tx_id: report.tx_id.clone(),
        plan_hash: report.plan_hash.clone(),
        volumes: report
            .volumes
            .iter()
            .map(|v| PersistedVolumeDiagnostic {
                stable_id: v.stable_id.clone(),
                filesystem_uuid: v.filesystem_uuid.clone(),
                device_identity: v.device_identity.clone(),
                display_name: v.display_name.clone(),
                sequential_median_mibs_per_sec: v.sequential_median_mibs_per_sec,
                sequential_stddev_mibs_per_sec: v.sequential_stddev_mibs_per_sec,
                small_write_median_us: v.small_write_median_us,
                small_write_p99_us: v.small_write_p99_us,
                total_bytes_written: v.total_bytes_written,
                cleanup_verified: v.cleanup_verified,
            })
            .collect(),
        recommendation: PersistedRecommendationDiagnostic {
            use_case: report.recommendation.use_case,
            status: report.recommendation.status.to_string(),
            recommended_stable_id: report.recommendation.recommended_stable_id.clone(),
            rationale: report.recommendation.rationale.clone(),
        },
    }
}

pub(crate) fn persist_latest_diagnostic_at(
    path: &Path,
    diag: &LatestBenchmarkDiagnostic,
) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| "diagnostic path has no parent directory".to_string())?;
    fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    let bytes = serde_json::to_vec(diag).map_err(|err| err.to_string())?;
    if bytes.len() as u64 > MAX_DIAGNOSTIC_BYTES {
        return Err("diagnostic payload exceeds 64 KiB cap".into());
    }
    let seq = LATEST_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let leaf = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("resource-benchmark-latest.json");
    let tmp = parent.join(format!(".{leaf}.tmp.{}.{seq}", std::process::id()));
    let write_res = (|| -> io::Result<()> {
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.set_permissions(fs::Permissions::from_mode(0o644))?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp);
        return Err(err.to_string());
    }
    Ok(())
}

pub(crate) fn read_latest_diagnostic_for(
    snapshot: &ResourceSnapshot,
) -> Option<LatestBenchmarkDiagnostic> {
    read_latest_diagnostic_from(&resolve_latest_benchmark_path(), snapshot)
}

pub(crate) fn read_latest_diagnostic_from(
    path: &Path,
    snapshot: &ResourceSnapshot,
) -> Option<LatestBenchmarkDiagnostic> {
    let meta = fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        return None;
    }
    if meta.len() > MAX_DIAGNOSTIC_BYTES {
        return None;
    }
    let file = fs::File::open(path).ok()?;
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.take(MAX_DIAGNOSTIC_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_DIAGNOSTIC_BYTES {
        return None;
    }
    let diag: LatestBenchmarkDiagnostic = serde_json::from_slice(&bytes).ok()?;
    if diag.schema_version != 1 || diag.volumes.is_empty() {
        return None;
    }
    let expected_platform = match snapshot.platform {
        RuntimePlatform::NativeLinux => "native_linux",
        RuntimePlatform::Wsl2 => "wsl2",
    };
    if diag.platform != expected_platform {
        return None;
    }

    // Verify every measured volume still exists on this machine so a stale report
    // from removed or different hardware is never presented as current.
    match snapshot.platform {
        RuntimePlatform::Wsl2 => {
            let windows = snapshot.windows.as_ref()?;
            let live_ids: HashSet<String> = windows
                .volumes
                .iter()
                .filter_map(|v| v.volume_id.as_deref())
                .map(|id| id.to_ascii_lowercase())
                .collect();
            if !diag
                .volumes
                .iter()
                .all(|v| live_ids.contains(&v.filesystem_uuid.to_ascii_lowercase()))
            {
                return None;
            }
        }
        RuntimePlatform::NativeLinux => {
            let live_uuids: HashSet<&str> = snapshot
                .block_devices
                .iter()
                .filter_map(|d| d.uuid.as_deref())
                .collect();
            if !diag
                .volumes
                .iter()
                .all(|v| live_uuids.contains(v.filesystem_uuid.as_str()))
            {
                return None;
            }
        }
    }

    Some(diag)
}

pub(crate) fn format_diagnostic_lines(diag: &LatestBenchmarkDiagnostic) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!(
        "Storage speed comparison ({}, use_case={:?}, tx={}):",
        diag.platform, diag.recommendation.use_case, diag.tx_id
    ));
    for v in &diag.volumes {
        lines.push(format!(
            "  {} [{}]: seq median {:.1} MiB/s (stddev {:.1}) | 4KiB flush median {:.1} us, p99 {:.1} us",
            v.display_name,
            v.stable_id,
            v.sequential_median_mibs_per_sec,
            v.sequential_stddev_mibs_per_sec,
            v.small_write_median_us,
            v.small_write_p99_us,
        ));
    }
    lines.push(format!(
        "  Recommended ({}): {} — {}",
        diag.recommendation.status,
        diag.recommendation
            .recommended_stable_id
            .as_deref()
            .unwrap_or("none"),
        diag.recommendation.rationale
    ));
    lines
}

fn extract_quoted_or_exe_path(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if let Some(after_open) = trimmed.strip_prefix('"') {
        let end = after_open.find('"')?;
        let suffix = after_open[end + 1..].trim();
        if !suffix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b' '))
        {
            return None;
        }
        return Some(after_open[..end].to_string());
    }
    trimmed
        .to_ascii_lowercase()
        .ends_with(".exe")
        .then(|| trimmed.to_string())
}

fn validate_windows_exe_path(win_path: &str) -> Option<String> {
    let exe = extract_quoted_or_exe_path(win_path)?;
    let b = exe.as_bytes();
    if b.len() < 8
        || !b[0].is_ascii_alphabetic()
        || b[1] != b':'
        || b[2] != b'\\'
        || !exe.to_ascii_lowercase().ends_with("ramshared-winsvc.exe")
        || !b[3..].iter().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, b'\\' | b'/' | b'.' | b'_' | b' ' | b'-')
        })
    {
        return None;
    }
    Some(exe)
}

fn parse_reg_image_path(stdout: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        let (_, rest) = line.trim().split_once("ImagePath")?;
        let val = rest
            .trim_start()
            .trim_start_matches("REG_EXPAND_SZ")
            .trim_start_matches("REG_SZ")
            .trim_start();
        validate_windows_exe_path(val)
    })
}

fn windows_path_to_wsl(path: &str) -> Option<PathBuf> {
    let exe_part = extract_quoted_or_exe_path(path)?;
    let mut chars = exe_part.chars();
    let drive = chars.next()?;
    if !drive.is_ascii_alphabetic() || chars.next() != Some(':') || chars.next() != Some('\\') {
        return None;
    }
    let rest: String = chars.map(|c| if c == '\\' { '/' } else { c }).collect();
    Some(PathBuf::from(format!(
        "/mnt/{}/{rest}",
        drive.to_ascii_lowercase()
    )))
}

pub(crate) fn windows_tool_program(bare_name: &str, system32_fallback: &str) -> String {
    if let Some(paths) = env::var_os("PATH") {
        for dir in env::split_paths(&paths) {
            let candidate = dir.join(bare_name);
            if fs::symlink_metadata(&candidate).is_ok_and(|m| m.file_type().is_file()) {
                return candidate.to_string_lossy().into_owned();
            }
        }
    }
    if Path::new(system32_fallback).exists() {
        system32_fallback.into()
    } else {
        bare_name.into()
    }
}

fn resolve_win_sudo(override_path: Option<&str>) -> Result<String, InventoryError> {
    let env_val = override_path
        .map(str::to_owned)
        .or_else(|| env::var(WIN_SUDO_ENV).ok())
        .filter(|v| !v.is_empty());
    if let Some(configured) = env_val {
        if fs::symlink_metadata(Path::new(&configured)).is_ok_and(|m| m.file_type().is_file()) {
            return Ok(configured);
        }
        return Err(InventoryError(format!(
            "{WIN_SUDO_ENV} points to missing or unsafe executable: {configured}"
        )));
    }
    if Path::new(DEFAULT_WIN_SUDO_PATH).exists() {
        return Ok(DEFAULT_WIN_SUDO_PATH.into());
    }
    Err(InventoryError(format!(
        "Windows elevation boundary not found ({WIN_SUDO_ENV} / {DEFAULT_WIN_SUDO_PATH}); run with elevated Windows access or set {WINSVC_EXE_ENV}"
    )))
}

fn classify_installed_win_path(win_path: &str, confirm: bool) -> WindowsWinsvcTarget {
    if !confirm
        && let Some(wsl) = windows_path_to_wsl(win_path)
        && fs::symlink_metadata(&wsl)
            .is_ok_and(|m| !m.file_type().is_symlink() && m.file_type().is_file())
    {
        return WindowsWinsvcTarget::Direct(wsl);
    }
    WindowsWinsvcTarget::ElevatedWindowsPath(win_path.to_owned())
}

fn find_windows_winsvc_target(confirm: bool) -> Result<WindowsWinsvcTarget, InventoryError> {
    if let Some(override_path) = env::var_os(WINSVC_EXE_ENV).filter(|v| !v.is_empty()) {
        let p = PathBuf::from(override_path);
        if fs::symlink_metadata(&p)
            .is_ok_and(|m| !m.file_type().is_symlink() && m.file_type().is_file())
        {
            return Ok(WindowsWinsvcTarget::Direct(p));
        }
        return Err(InventoryError(format!(
            "{WINSVC_EXE_ENV} points to missing or unsafe executable: {}",
            p.display()
        )));
    }
    let reg = windows_tool_program("reg.exe", DEFAULT_WIN_REG_PATH);
    if let Ok(out) = Command::new(reg)
        .args([
            "query",
            r"HKLM\SYSTEM\CurrentControlSet\Services\RamSharedWinSvc",
            "/v",
            "ImagePath",
        ])
        .output()
        && out.status.success()
        && let Some(valid) = parse_reg_image_path(&String::from_utf8_lossy(&out.stdout))
    {
        return Ok(classify_installed_win_path(&valid, confirm));
    }
    Err(InventoryError(
        "ramshared-winsvc.exe was not found on Windows host; install/repair RamSharedWinSvc first"
            .into(),
    ))
}

fn winsvc_benchmark_args(
    confirm: bool,
    request: &BenchmarkRequest,
) -> Result<Vec<String>, InventoryError> {
    let mut args = vec![
        "benchmark-volumes".into(),
        if confirm { "--confirm" } else { "--plan" }.into(),
        "--use-case".into(),
        match request.use_case {
            BenchmarkUseCase::Swap => "swap",
            BenchmarkUseCase::Origin => "origin",
        }
        .into(),
    ];
    if !request.batch.is_empty() {
        let mut drives = Vec::with_capacity(request.batch.len());
        for raw in &request.batch {
            let trimmed = raw.trim().trim_end_matches(':');
            if trimmed.len() != 1 || !trimmed.bytes().all(|b| b.is_ascii_alphabetic()) {
                return Err(InventoryError(format!(
                    "invalid Windows volume selector: {raw}"
                )));
            }
            drives.push(trimmed.to_ascii_uppercase());
        }
        args.push("--volumes".into());
        args.push(drives.join(","));
    }
    args.push("--json".into());
    Ok(args)
}

fn build_winsvc_command(
    target: &WindowsWinsvcTarget,
    confirm: bool,
    request: &BenchmarkRequest,
) -> Result<Command, InventoryError> {
    build_winsvc_command_with(target, confirm, request, None)
}

fn build_winsvc_command_with(
    target: &WindowsWinsvcTarget,
    confirm: bool,
    request: &BenchmarkRequest,
    win_sudo_override: Option<&str>,
) -> Result<Command, InventoryError> {
    let args = winsvc_benchmark_args(confirm, request)?;
    match target {
        WindowsWinsvcTarget::Direct(exe) => {
            let mut cmd = Command::new(exe);
            cmd.args(&args);
            Ok(cmd)
        }
        WindowsWinsvcTarget::ElevatedWindowsPath(win_path) => {
            let valid = validate_windows_exe_path(win_path).ok_or_else(|| {
                InventoryError("installed ramshared-winsvc.exe path failed validation".into())
            })?;
            let win_sudo = resolve_win_sudo(win_sudo_override)?;
            let ps_script = format!(
                "[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false); & '{valid}' {}",
                args.join(" ")
            );
            let mut cmd = Command::new(win_sudo);
            cmd.args([
                "powershell.exe",
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-NonInteractive",
                "-Command",
                &ps_script,
            ]);
            Ok(cmd)
        }
    }
}

fn invoke_windows_benchmark_exe(
    target: &WindowsWinsvcTarget,
    confirm: bool,
    request: &BenchmarkRequest,
) -> Result<WindowsBenchmarkReportWire, InventoryError> {
    let mut cmd = build_winsvc_command(target, confirm, request)?;

    let _progress = confirm.then(|| {
        OperationProgressGuard::start(
            OperationKind::StorageBenchmark,
            "benchmark_volume",
            ROUNDS_PER_VOLUME as u32,
        )
    });
    if let Some(guard) = _progress.as_ref() {
        guard.update(ProgressUpdate {
            phase: "benchmark_volume",
            step_index: 1,
            target: Some("Windows fixed NTFS volumes"),
            completed_bytes: 0,
            total_bytes: 0,
            throughput_mib_per_s: None,
        });
    }

    let output = crate::bounded_process::run_capture_command(
        &mut cmd,
        "ramshared-winsvc benchmark-volumes",
        WINDOWS_BENCHMARK_TIMEOUT,
        MAX_WINDOWS_BENCHMARK_STDOUT_BYTES,
        |_| {},
    )
    .map_err(|err| InventoryError(format!("Windows host volume benchmark failed: {err}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(InventoryError(format!(
            "ramshared-winsvc benchmark-volumes failed: {}",
            stderr.trim()
        )));
    }
    let text = String::from_utf8(output.stdout).map_err(|err| {
        InventoryError(format!(
            "Windows benchmark output is not valid UTF-8: {err}"
        ))
    })?;
    serde_json::from_str::<WindowsBenchmarkReportWire>(&text)
        .map_err(|err| InventoryError(format!("invalid Windows benchmark JSON payload: {err}")))
}

fn make_benchmark_volume(
    drive_letter: &str,
    label: Option<&str>,
    filesystem: &str,
    free_bytes: u64,
    volume_id: &str,
    observed_unix_ms: u64,
) -> BenchmarkVolume {
    let label = label.unwrap_or("unlabeled");
    BenchmarkVolume {
        filesystem_uuid: volume_id.to_owned(),
        device_identity: format!("{drive_letter}:"),
        display_name: format!("{drive_letter}: ({label})"),
        mountpoint: format!("{drive_letter}:\\"),
        mount_id: 0,
        major_minor: format!("{drive_letter}:"),
        filesystem: filesystem.to_owned(),
        free_bytes,
        required_free_bytes: BENCHMARK_REQUIRED_FREE_BYTES,
        capacity_observed_unix_ms: observed_unix_ms,
    }
}

pub(crate) fn run_wsl2_benchmark(
    snapshot: &ResourceSnapshot,
    request: &BenchmarkRequest,
    stdout: &mut dyn Write,
) -> Result<(), InventoryError> {
    if request.action == BenchmarkAction::Recover {
        return run_wsl2_benchmark_with(
            snapshot,
            request,
            Path::new("/nonexistent"),
            &resolve_latest_benchmark_path(),
            stdout,
        );
    }
    let target = find_windows_winsvc_target(request.action == BenchmarkAction::Confirmed)?;
    run_wsl2_benchmark_with_target(
        snapshot,
        request,
        &target,
        &resolve_latest_benchmark_path(),
        stdout,
    )
}

pub(crate) fn run_wsl2_benchmark_with(
    snapshot: &ResourceSnapshot,
    request: &BenchmarkRequest,
    exe: &Path,
    latest_path: &Path,
    stdout: &mut dyn Write,
) -> Result<(), InventoryError> {
    let target = WindowsWinsvcTarget::Direct(exe.to_path_buf());
    run_wsl2_benchmark_with_target(snapshot, request, &target, latest_path, stdout)
}

fn run_wsl2_benchmark_with_target(
    snapshot: &ResourceSnapshot,
    request: &BenchmarkRequest,
    target: &WindowsWinsvcTarget,
    latest_path: &Path,
    stdout: &mut dyn Write,
) -> Result<(), InventoryError> {
    if request.action == BenchmarkAction::Recover {
        let msg = crate::resource_config::benchmark::BenchmarkRecoveryReport {
            schema_version: 1,
            recovered: false,
            previous_tx_id: None,
            previous_status: None,
            detail: "Windows host benchmark cleans probe artifacts synchronously per invocation"
                .into(),
        };
        if request.json {
            let text = serde_json::to_string(&msg).map_err(|e| InventoryError(e.to_string()))?;
            writeln!(stdout, "{text}").map_err(|e| InventoryError(e.to_string()))?;
        } else {
            writeln!(stdout, "{}", msg.detail).map_err(|e| InventoryError(e.to_string()))?;
        }
        return Ok(());
    }

    let preview = invoke_windows_benchmark_exe(target, false, request)?;
    let volumes: Vec<BenchmarkVolume> = preview
        .eligible_volumes
        .iter()
        .map(|v| {
            make_benchmark_volume(
                &v.drive_letter,
                v.label.as_deref(),
                &v.filesystem,
                v.free_bytes,
                &v.volume_id,
                snapshot.observed_unix_ms,
            )
        })
        .collect();
    let schedule: Vec<Vec<String>> = (0..preview.rounds)
        .map(|round| {
            (0..volumes.len())
                .map(|off| volumes[(round + off) % volumes.len()].stable_id())
                .collect()
        })
        .collect();
    let max_total_write_bytes = (volumes.len() as u64)
        .saturating_mul(PER_VOLUME_WRITE_CAP_BYTES)
        .min(RUN_WRITE_CAP_BYTES);

    let plan = BenchmarkPlan {
        schema_version: 1,
        plan_hash: preview.plan_hash,
        platform: "wsl2",
        use_case: request.use_case,
        observed_unix_ms: snapshot.observed_unix_ms,
        rounds: preview.rounds,
        sample_payload_bytes: SAMPLE_PAYLOAD_BYTES,
        per_volume_write_cap_bytes: PER_VOLUME_WRITE_CAP_BYTES,
        max_total_write_bytes,
        per_volume_deadline_secs: VOLUME_WORKER_DEADLINE.as_secs(),
        required_free_bytes_per_volume: BENCHMARK_REQUIRED_FREE_BYTES,
        volumes,
        round_schedule: schedule,
        writes_performed: false,
    };

    if request.action == BenchmarkAction::Plan {
        return super::write_plan_output(&plan, request.json, stdout);
    }
    if request.action == BenchmarkAction::InteractiveOrConfirm {
        if request.json || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(InventoryError(
                "benchmark refused without explicit confirmation; review `--plan` then pass `--confirm`"
                    .into(),
            ));
        }
        let plan_text = super::render_plan_text(&plan);
        write!(stdout, "{plan_text}").map_err(|err| InventoryError(err.to_string()))?;
        write!(
            stdout,
            "Type BENCHMARK to confirm bounded host volume writes: "
        )
        .and_then(|_| stdout.flush())
        .map_err(|err| InventoryError(err.to_string()))?;
        let mut line = String::new();
        io::stdin()
            .read_line(&mut line)
            .map_err(|err| InventoryError(format!("cannot read confirmation: {err}")))?;
        if line.trim() != "BENCHMARK" {
            return Err(InventoryError("benchmark aborted by operator".into()));
        }
    }

    let measured = invoke_windows_benchmark_exe(target, true, request)?;
    if !measured.writes_performed || !measured.cleanup_verified {
        return Err(InventoryError(
            "Windows host benchmark did not confirm completed writes and verified cleanup".into(),
        ));
    }

    let mut summaries = Vec::with_capacity(measured.volumes.len());
    for wvol in &measured.volumes {
        let bv = make_benchmark_volume(
            &wvol.drive_letter,
            wvol.label.as_deref(),
            &wvol.filesystem,
            wvol.free_bytes,
            &wvol.volume_id,
            snapshot.observed_unix_ms,
        );
        let samples: Vec<WorkerSampleOutcome> = wvol
            .samples
            .iter()
            .map(|s| WorkerSampleOutcome {
                bytes_written: s.bytes_written,
                seq_bytes_written: s.seq_bytes_written,
                small_write_count: s.small_write_count,
                seq_elapsed_ns: s.seq_elapsed_ns,
                small_write_latencies_ns: s.small_write_latencies_ns.clone(),
                expected_sha256: s.expected_sha256.clone(),
                observed_sha256: s.observed_sha256.clone(),
                file_dev: 0,
                file_ino: 0,
                cleanup_verified: s.cleanup_verified && wvol.cleanup_verified,
            })
            .collect();
        summaries.push(summarize_volume(&bv, &samples)?);
    }

    let recommendation: BenchmarkRecommendation = recommend_leader(request.use_case, &summaries);
    let report = BenchmarkReport {
        schema_version: measured.schema_version,
        tx_id: measured.tx_id,
        plan_hash: measured.plan_hash,
        platform: "wsl2",
        use_case: request.use_case,
        rounds: measured.rounds,
        volumes: summaries,
        recommendation,
        writes_performed: true,
        cleanup_verified: true,
        status: "verified",
    };
    let _ =
        persist_latest_diagnostic_at(latest_path, &diagnostic_from_report(&report, unix_millis()));
    super::write_report_output(&report, request.json, stdout)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::resource_config::tests::fixture_snapshot;
    use std::sync::atomic::AtomicU64;

    #[test]
    fn persisted_diagnostic_validates_bounds_and_rejects_mismatched_hardware() {
        static SEQ: AtomicU64 = AtomicU64::new(1);
        let dir = std::env::temp_dir().join(format!(
            "ramshared-latest-bench-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("latest.json");

        let snap = fixture_snapshot();
        let diag = LatestBenchmarkDiagnostic {
            schema_version: 1,
            recorded_at_unix_ms: 1_700_000_000_000,
            platform: "wsl2".into(),
            tx_id: "win-123".into(),
            plan_hash: "abc".into(),
            volumes: vec![PersistedVolumeDiagnostic {
                stable_id: "vol-i@I:".into(),
                filesystem_uuid: "vol-i".into(),
                device_identity: "I:".into(),
                display_name: "I: (Data)".into(),
                sequential_median_mibs_per_sec: 3200.0,
                sequential_stddev_mibs_per_sec: 12.0,
                small_write_median_us: 140.0,
                small_write_p99_us: 260.0,
                total_bytes_written: 96 * 1024 * 1024,
                cleanup_verified: true,
            }],
            recommendation: PersistedRecommendationDiagnostic {
                use_case: BenchmarkUseCase::Swap,
                status: "recommended".into(),
                recommended_stable_id: Some("vol-i@I:".into()),
                rationale: "I: leads".into(),
            },
        };
        persist_latest_diagnostic_at(&path, &diag).unwrap();

        let loaded = read_latest_diagnostic_from(&path, &snap).expect("matching WSL2 volume");
        let lines = format_diagnostic_lines(&loaded);
        assert!(lines.iter().any(|l| l.contains("3200.0 MiB/s")));
        assert!(lines.iter().any(|l| l.contains("Recommended")));

        // Detached/unmatched volume fails closed.
        let mut wrong_vol = diag.clone();
        wrong_vol.volumes[0].filesystem_uuid = "vol-missing".into();
        persist_latest_diagnostic_at(&path, &wrong_vol).unwrap();
        assert!(read_latest_diagnostic_from(&path, &snap).is_none());

        // Platform mismatch fails closed.
        let mut wrong_plat = diag;
        wrong_plat.platform = "native_linux".into();
        persist_latest_diagnostic_at(&path, &wrong_plat).unwrap();
        assert!(read_latest_diagnostic_from(&path, &snap).is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn windows_path_translates_to_wsl_mount_cleanly() {
        assert_eq!(
            windows_path_to_wsl(
                "\"C:\\Program Files\\RamShared\\versions\\0.15.0\\ramshared-winsvc.exe\" authority-service"
            ),
            Some(PathBuf::from(
                "/mnt/c/Program Files/RamShared/versions/0.15.0/ramshared-winsvc.exe"
            ))
        );
        assert!(windows_path_to_wsl("relative.exe").is_none());

        let reg_out = "\r\nHKEY_LOCAL_MACHINE\\SYSTEM\\CurrentControlSet\\Services\\RamSharedWinSvc\r\n    ImagePath    REG_EXPAND_SZ    \"C:\\Program Files\\RamShared\\versions\\0.15.0\\ramshared-winsvc.exe\" authority-service\r\n";
        assert_eq!(
            parse_reg_image_path(reg_out).as_deref(),
            Some("C:\\Program Files\\RamShared\\versions\\0.15.0\\ramshared-winsvc.exe")
        );
        assert!(parse_reg_image_path("bad").is_none());
        assert!(validate_windows_exe_path("C:\\Program Files\\other.exe").is_none());
        assert!(
            validate_windows_exe_path("C:\\Program Files\\ramshared-winsvc.exe; calc").is_none()
        );

        let tool = windows_tool_program("sh", "/bin/sh");
        assert!(!tool.is_empty());
        assert_eq!(
            windows_tool_program("nonexistent-ramshared-tool", "/nonexistent/tool"),
            "nonexistent-ramshared-tool"
        );

        let dir = std::env::temp_dir().join(format!("ramshared-win-cmd-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let fake_sudo = dir.join("fake-sudo.sh");
        fs::write(&fake_sudo, "#!/bin/sh\nexit 0\n").unwrap();
        let fake_sudo_str = fake_sudo.to_str().unwrap();
        let elevated = WindowsWinsvcTarget::ElevatedWindowsPath(
            "C:\\Program Files\\RamShared\\versions\\0.15.0\\ramshared-winsvc.exe".into(),
        );
        let built = build_winsvc_command_with(
            &elevated,
            true,
            &BenchmarkRequest {
                action: BenchmarkAction::Confirmed,
                use_case: BenchmarkUseCase::Origin,
                json: true,
                profile_path: None,
                batch: vec!["C:".into(), "i".into()],
            },
            Some(fake_sudo_str),
        )
        .expect("elevated command builds cleanly");
        assert_eq!(built.get_program(), fake_sudo.as_os_str());
        assert!(resolve_win_sudo(Some("/nonexistent/sudo.exe")).is_err());
        assert!(
            winsvc_benchmark_args(
                true,
                &BenchmarkRequest {
                    action: BenchmarkAction::Confirmed,
                    use_case: BenchmarkUseCase::Swap,
                    json: true,
                    profile_path: None,
                    batch: vec!["bad-drive".into()],
                }
            )
            .is_err()
        );
        let _ = fs::remove_dir_all(&dir);
        assert!(matches!(
            classify_installed_win_path("C:\\Program Files\\RamShared\\ramshared-winsvc.exe", true),
            WindowsWinsvcTarget::ElevatedWindowsPath(_)
        ));
    }

    #[test]
    fn wsl2_benchmark_delegates_plan_confirm_recover_and_persists_diagnostic() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("ramshared-wsl2-stub-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("stub-winsvc.sh");
        let latest = dir.join("latest.json");

        let expected_hash = "a".repeat(64);
        let lats_fast: Vec<u64> = vec![100_000; super::super::model::SMALL_WRITES_PER_SAMPLE];
        let lats_slow: Vec<u64> = vec![400_000; super::super::model::SMALL_WRITES_PER_SAMPLE];
        let sample = |seq_ns: u64, lats: &Vec<u64>| {
            serde_json::json!({
                "seq_elapsed_ns": seq_ns,
                "small_write_latencies_ns": lats,
                "bytes_written": SAMPLE_PAYLOAD_BYTES,
                "seq_bytes_written": super::super::model::SEQ_WRITE_BYTES,
                "small_write_count": super::super::model::SMALL_WRITES_PER_SAMPLE,
                "expected_sha256": expected_hash,
                "observed_sha256": expected_hash,
                "cleanup_verified": true
            })
        };
        let plan_json = serde_json::json!({
            "schema_version": 1,
            "tx_id": "win-preview",
            "plan_hash": "deadbeef",
            "use_case": "swap",
            "rounds": 3,
            "writes_performed": false,
            "cleanup_verified": true,
            "eligible_volumes": [
                {"drive_letter": "C", "label": "System", "filesystem": "NTFS", "free_bytes": 100_000_000_000u64, "volume_id": "vol-c"},
                {"drive_letter": "I", "label": "Data", "filesystem": "NTFS", "free_bytes": 200_000_000_000u64, "volume_id": "vol-i"}
            ],
            "volumes": []
        })
        .to_string();
        let confirm_json = serde_json::json!({
            "schema_version": 1,
            "tx_id": "win-confirmed-1",
            "plan_hash": "deadbeef",
            "use_case": "swap",
            "rounds": 3,
            "writes_performed": true,
            "cleanup_verified": true,
            "eligible_volumes": [
                {"drive_letter": "C", "label": "System", "filesystem": "NTFS", "free_bytes": 100_000_000_000u64, "volume_id": "vol-c"},
                {"drive_letter": "I", "label": "Data", "filesystem": "NTFS", "free_bytes": 200_000_000_000u64, "volume_id": "vol-i"}
            ],
            "volumes": [
                {
                    "drive_letter": "C", "label": "System", "filesystem": "NTFS",
                    "free_bytes": 100_000_000_000u64, "volume_id": "vol-c", "cleanup_verified": true,
                    "samples": [sample(40_000_000, &lats_slow), sample(41_000_000, &lats_slow), sample(42_000_000, &lats_slow)]
                },
                {
                    "drive_letter": "I", "label": "Data", "filesystem": "NTFS",
                    "free_bytes": 200_000_000_000u64, "volume_id": "vol-i", "cleanup_verified": true,
                    "samples": [sample(10_000_000, &lats_fast), sample(10_500_000, &lats_fast), sample(10_200_000, &lats_fast)]
                }
            ]
        })
        .to_string();

        let plan_f = dir.join("plan.json");
        let conf_f = dir.join("confirm.json");
        fs::write(&plan_f, &plan_json).unwrap();
        fs::write(&conf_f, &confirm_json).unwrap();
        fs::write(
            &exe,
            format!(
                "#!/bin/sh\nif echo \"$*\" | grep -q -- '--confirm'; then cat '{}'; else cat '{}'; fi\n",
                conf_f.display(),
                plan_f.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();

        let snap = fixture_snapshot();

        let mut recover_out = Vec::new();
        run_wsl2_benchmark_with(
            &snap,
            &BenchmarkRequest {
                action: BenchmarkAction::Recover,
                use_case: BenchmarkUseCase::Swap,
                json: true,
                profile_path: None,
                batch: Vec::new(),
            },
            &exe,
            &latest,
            &mut recover_out,
        )
        .unwrap();
        assert!(
            String::from_utf8(recover_out)
                .unwrap()
                .contains("synchronously")
        );

        let mut plan_out = Vec::new();
        run_wsl2_benchmark_with(
            &snap,
            &BenchmarkRequest {
                action: BenchmarkAction::Plan,
                use_case: BenchmarkUseCase::Swap,
                json: false,
                profile_path: None,
                batch: Vec::new(),
            },
            &exe,
            &latest,
            &mut plan_out,
        )
        .unwrap();
        let plan_str = String::from_utf8(plan_out).unwrap();
        assert!(plan_str.contains("I: (Data)"));
        assert!(!latest.exists());

        let mut confirm_out = Vec::new();
        run_wsl2_benchmark_with(
            &snap,
            &BenchmarkRequest {
                action: BenchmarkAction::Confirmed,
                use_case: BenchmarkUseCase::Swap,
                json: false,
                profile_path: None,
                batch: vec!["C".into(), "I".into()],
            },
            &exe,
            &latest,
            &mut confirm_out,
        )
        .unwrap();
        let conf_str = String::from_utf8(confirm_out).unwrap();
        assert!(conf_str.contains("Recommended") || conf_str.contains("Recommendation"));
        assert!(latest.exists());

        let mut rec_out = Vec::new();
        let mut rec_err = Vec::new();
        let code = recommend::run_setup_recommend_with(
            &["--json".into()],
            &|| Ok(snap.clone()),
            &mut |_, _| Ok(()),
            &latest,
            &mut rec_out,
            &mut rec_err,
        );
        assert_eq!(code, 0);
        let rec_str = String::from_utf8(rec_out).unwrap();
        assert!(rec_str.contains("win-confirmed-1"));
        assert!(rec_str.contains("vol-i@I:"));

        let mut rec_text = Vec::new();
        assert_eq!(
            recommend::run_setup_recommend_with(
                &[],
                &|| Ok(snap.clone()),
                &mut |_, _| Ok(()),
                &latest,
                &mut rec_text,
                &mut rec_err,
            ),
            0
        );
        assert!(String::from_utf8(rec_text).unwrap().contains("Recommended"));

        assert_eq!(
            recommend::run_setup_recommend_with(
                &["--bogus".into()],
                &|| Ok(snap.clone()),
                &mut |_, _| Ok(()),
                &latest,
                &mut Vec::new(),
                &mut rec_err,
            ),
            2
        );

        // NativeLinux diagnostic match/mismatch and recommend branches.
        let mut native_snap = snap.clone();
        native_snap.platform = RuntimePlatform::NativeLinux;
        native_snap.windows = None;
        let mut dev = crate::resource_config::tests::fixture_block_device();
        dev.uuid = Some("uuid-nvme0".into());
        native_snap.block_devices = vec![dev];

        let mut native_rec_text = Vec::new();
        assert_eq!(
            recommend::run_setup_recommend_with(
                &[],
                &|| Ok(native_snap.clone()),
                &mut |_, _| Ok(()),
                &dir.join("absent.json"),
                &mut native_rec_text,
                &mut rec_err,
            ),
            0
        );
        let nt_str = String::from_utf8(native_rec_text).unwrap();
        assert!(nt_str.contains("Speed diagnostic: not measured yet"));

        let mut captured_req: Option<BenchmarkRequest> = None;
        assert_eq!(
            recommend::run_setup_recommend_with(
                &[
                    "--benchmark".into(),
                    "--confirm".into(),
                    "--use-case".into(),
                    "origin".into(),
                    "--volumes".into(),
                    "C,I".into(),
                    "--json".into(),
                ],
                &|| Ok(snap.clone()),
                &mut |req, _| {
                    captured_req = Some(req);
                    Ok(())
                },
                &latest,
                &mut Vec::new(),
                &mut rec_err,
            ),
            0
        );
        let req_obs = captured_req.expect("captured benchmark req");
        assert_eq!(req_obs.action, BenchmarkAction::Confirmed);
        assert_eq!(req_obs.use_case, BenchmarkUseCase::Origin);
        assert_eq!(req_obs.batch, vec!["C".to_string(), "I".to_string()]);

        assert_eq!(
            recommend::run_setup_recommend_with(
                &["--use-case".into(), "invalid".into()],
                &|| Ok(snap.clone()),
                &mut |_, _| Ok(()),
                &latest,
                &mut Vec::new(),
                &mut rec_err,
            ),
            2
        );
        assert_eq!(
            recommend::run_setup_recommend_with(
                &["--benchmark".into(), "--use-case".into(), "swap".into()],
                &|| Ok(snap.clone()),
                &mut |_, _| Err(InventoryError("boom".into())),
                &latest,
                &mut Vec::new(),
                &mut rec_err,
            ),
            1
        );
        assert_eq!(
            recommend::run_setup_recommend_with(
                &[],
                &|| Err(InventoryError("no snap".into())),
                &mut |_, _| Ok(()),
                &latest,
                &mut Vec::new(),
                &mut rec_err,
            ),
            1
        );

        let native_latest = dir.join("native-latest.json");
        let native_diag = LatestBenchmarkDiagnostic {
            schema_version: 1,
            recorded_at_unix_ms: 1_700_000_000_000,
            platform: "native_linux".into(),
            tx_id: "lin-1".into(),
            plan_hash: "xyz".into(),
            volumes: vec![PersistedVolumeDiagnostic {
                stable_id: "uuid-nvme0@/dev/nvme0n1p2".into(),
                filesystem_uuid: "uuid-nvme0".into(),
                device_identity: "/dev/nvme0n1p2".into(),
                display_name: "NVMe at /".into(),
                sequential_median_mibs_per_sec: 3500.0,
                sequential_stddev_mibs_per_sec: 10.0,
                small_write_median_us: 90.0,
                small_write_p99_us: 180.0,
                total_bytes_written: 96 * 1024 * 1024,
                cleanup_verified: true,
            }],
            recommendation: PersistedRecommendationDiagnostic {
                use_case: BenchmarkUseCase::Swap,
                status: "single_volume".into(),
                recommended_stable_id: Some("uuid-nvme0@/dev/nvme0n1p2".into()),
                rationale: "single".into(),
            },
        };
        persist_latest_diagnostic_at(&native_latest, &native_diag).unwrap();
        assert!(read_latest_diagnostic_from(&native_latest, &native_snap).is_some());

        // InteractiveOrConfirm without TTY refuses safely; failing exe returns error.
        assert!(
            run_wsl2_benchmark_with(
                &snap,
                &BenchmarkRequest {
                    action: BenchmarkAction::InteractiveOrConfirm,
                    use_case: BenchmarkUseCase::Swap,
                    json: true,
                    profile_path: None,
                    batch: Vec::new(),
                },
                &exe,
                &latest,
                &mut Vec::new(),
            )
            .is_err()
        );
        let bad_exe = dir.join("fail.sh");
        fs::write(&bad_exe, "#!/bin/sh\necho bad >&2\nexit 1\n").unwrap();
        fs::set_permissions(&bad_exe, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            run_wsl2_benchmark_with(
                &snap,
                &BenchmarkRequest {
                    action: BenchmarkAction::Plan,
                    use_case: BenchmarkUseCase::Swap,
                    json: true,
                    profile_path: None,
                    batch: Vec::new(),
                },
                &bad_exe,
                &latest,
                &mut Vec::new(),
            )
            .is_err()
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
