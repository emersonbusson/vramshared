// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor.rs
//! Read-only RamShared observability stream and terminal dashboard.

#[cfg(test)]
use std::collections::VecDeque;
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use serde_json::{Map, Value};

use crate::cascade;
use crate::installed_image::{
    InstallationIdentityCache, PRODUCT_ROOT, ProcessInstallOrigin, current_process_install_origin,
    current_release_version,
};
#[cfg(test)]
use crate::installed_image::{
    InstalledIdentity, RunningInstallStatus, format_build_identity,
    format_installed_identity_lines, format_running_identity_line, restart_target,
};
#[cfg(test)]
use crate::release_coherence::PREVIOUS_RELEASE_HINT_ENV;
use classify::{classify_unmanaged_memory_usage, detect_memory_scope, observation_sample_is_stale};
use dashboard::Screen;
#[cfg(test)]
use observations::{BENCHMARK_STATUS_PATH_NOT_FOUND, cache_telemetry_record};
use ramshared_vram::WorkerCacheTelemetry;
#[cfg(test)]
use ramshared_vram::{GpuBudgetSource, TELEMETRY_MAX_AGE_MS};
use render::*;
#[cfg(test)]
use std::collections::BTreeMap;

mod classify;
mod compact;
mod dashboard;
mod gpu_activity;
mod i18n;
mod latency;
#[cfg_attr(not(test), allow(dead_code))]
mod model;
#[cfg_attr(not(test), allow(dead_code))]
mod observations;
#[cfg_attr(not(test), allow(dead_code))]
mod render;
mod reserve;
mod tui;

#[cfg_attr(not(test), allow(unused_imports))]
use compact::*;
pub use i18n::Language;
#[cfg_attr(not(test), allow(unused_imports))]
use latency::*;
#[cfg_attr(not(test), allow(unused_imports))]
use reserve::*;
#[cfg_attr(not(test), allow(unused_imports))]
use tui::*;

pub use model::{
    ControlPlaneObservation, LatencySource, MemoryObservation, Observation, TierIoStats,
};
pub use observations::parse_uptime_seconds;
pub(crate) use observations::{ZramTierState, zram_tier_state_from};
use observations::{
    collect_cgroup_memory, collect_per_tier_io, collect_process_snapshot, count_scope_dirs,
    gpu_observation_from_status, parse_hyperv_balloon, parse_meminfo, parse_memory_events,
    parse_memory_pressure, parse_vmstat, parse_vmstat_value, query_unit_startup_ms,
    read_benchmark_qualification, read_hyperv_balloon_file, read_optional_u64_file,
    read_reservation_totals, unix_epoch_ms,
};
#[cfg(test)]
use observations::{parse_per_tier_diskstats, parse_swap_diskstats};

const DEFAULT_INTERVAL_MS: u64 = 1_000;
const DEFAULT_HISTORY_SECONDS: u64 = 300;
const MIN_INTERVAL_MS: u64 = 250;
const MAX_HISTORY_SECONDS: u64 = 3_600;

/// Milliseconds in a second.
///
/// The monitor takes an interval in milliseconds and a history in seconds;
/// this is the only place that difference is resolved. Both call sites go
/// through it and neither may restate the number.
const MILLIS_PER_SECOND: u64 = 1_000;

/// Smallest history the sparkline holds.
///
/// One sample, so the series is never empty and the capacity is never zero.
const HISTORY_MIN_SAMPLES: u64 = 1;

/// Largest history the sparkline holds.
///
/// A longer window is truncated to this many samples rather than growing
/// without bound across a long session.
const HISTORY_MAX_SAMPLES: u64 = 10_000;

const DEFAULT_MAX_LOG_BYTES: u64 = 50 * 1024 * 1024;

/// How often the dashboard re-reads install identity and the restart target.
///
/// This is a display refresh, not a measurement cadence: the observation
/// stream has its own interval. It only bounds how long a stale restart hint
/// can stay on screen before the next probe (SPEC DT-4 / NFR-5).
const IDENTITY_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// Budget sample freshness window, in milliseconds.
///
/// The product authority is [`ramshared_vram::GPU_BUDGET_MAX_AGE_MS`], beside
/// the predicate it governs. Re-exported here so every CLI reader of a budget
/// sample names the window instead of restating it.
pub(crate) use ramshared_vram::GPU_BUDGET_MAX_AGE_MS;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MonitorOptions {
    pub jsonl: bool,
    pub compact: bool,
    pub interval_ms: u64,
    pub history_seconds: u64,
    pub output: Option<PathBuf>,
    pub heartbeat: Option<PathBuf>,
    pub benchmark_evidence: Option<PathBuf>,
    pub once: bool,
    pub language: Language,
}

impl Default for MonitorOptions {
    fn default() -> Self {
        Self {
            jsonl: false,
            compact: false,
            interval_ms: DEFAULT_INTERVAL_MS,
            history_seconds: DEFAULT_HISTORY_SECONDS,
            output: None,
            heartbeat: None,
            benchmark_evidence: None,
            once: false,
            language: Language::En,
        }
    }
}

impl MonitorOptions {
    /// The qualification record this monitor reads.
    ///
    /// Never CWD-relative: the same binary answering a measured fact
    /// differently depending on where it was launched is a defect, not a
    /// preference. The operator can name the record explicitly; otherwise it
    /// is looked up inside the installed release, which is the authority this
    /// process is bound to. A record the install does not carry publishes
    /// "evidence path not found", never a work-in-progress status.
    pub(crate) fn benchmark_evidence_path(&self) -> PathBuf {
        self.benchmark_evidence.clone().unwrap_or_else(|| {
            Path::new(PRODUCT_ROOT).join("current/docs/benchmarks/history/latest.json")
        })
    }
}

#[cfg(test)]
#[path = "monitor_pressure_tests.rs"]
mod pressure_classification_tests;

impl MonitorOptions {
    pub fn parse(args: &[String]) -> Result<Self, ()> {
        let mut options = Self::default();
        let mut index = 0;
        while index < args.len() {
            match args[index].as_str() {
                "--jsonl" => options.jsonl = true,
                "--compact" => options.compact = true,
                "--once" => options.once = true,
                "--interval-ms" => {
                    index += 1;
                    options.interval_ms = parse_bounded(args.get(index), MIN_INTERVAL_MS, 60_000)?;
                }
                "--history-seconds" => {
                    index += 1;
                    options.history_seconds =
                        parse_bounded(args.get(index), 1, MAX_HISTORY_SECONDS)?;
                }
                "--output" => {
                    index += 1;
                    options.output = Some(parse_path(args.get(index))?);
                }
                "--heartbeat" => {
                    index += 1;
                    options.heartbeat = Some(parse_path(args.get(index))?);
                }
                "--benchmark-evidence" => {
                    index += 1;
                    options.benchmark_evidence = Some(parse_path(args.get(index))?);
                }
                "--lang" => {
                    index += 1;
                    options.language = args
                        .get(index)
                        .and_then(|value| Language::parse(value))
                        .ok_or(())?;
                }
                _ => return Err(()),
            }
            index += 1;
        }
        if (options.output.is_some() || options.heartbeat.is_some()) && !options.jsonl {
            return Err(());
        }
        Ok(options)
    }
}

fn parse_bounded(value: Option<&String>, minimum: u64, maximum: u64) -> Result<u64, ()> {
    value
        .and_then(|text| text.parse::<u64>().ok())
        .filter(|number| (minimum..=maximum).contains(number))
        .ok_or(())
}

fn parse_path(value: Option<&String>) -> Result<PathBuf, ()> {
    value
        .filter(|text| !text.is_empty() && !text.contains('\0'))
        .map(PathBuf::from)
        .ok_or(())
}

#[derive(Debug)]
pub enum MonitorError {
    Io(String),
    Json(String),
    #[allow(dead_code)]
    Terminal(String),
}

impl fmt::Display for MonitorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(message) => write!(formatter, "monitor I/O: {message}"),
            Self::Json(message) => write!(formatter, "monitor JSON: {message}"),
            Self::Terminal(message) => write!(formatter, "monitor terminal: {message}"),
        }
    }
}

pub fn collect_observation(benchmark_evidence: &Path) -> Result<Observation, MonitorError> {
    let status_json =
        cascade::status_json_document().map_err(|error| MonitorError::Io(error.to_string()))?;
    let status_map = serde_json::from_str::<Map<String, Value>>(&status_json)
        .map_err(|error| MonitorError::Json(error.to_string()))?;
    let osrelease = fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let memory_scope = detect_memory_scope(
        &osrelease,
        wsl_interop_present(
            Path::new("/proc/sys/fs/binfmt_misc/WSLInterop"),
            std::env::var_os("WSL_INTEROP").as_deref(),
        ),
    );
    let meminfo = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let pressure = fs::read_to_string("/proc/pressure/memory").unwrap_or_default();
    let vmstat = fs::read_to_string("/proc/vmstat").unwrap_or_default();
    let diskstats = fs::read_to_string("/proc/diskstats").unwrap_or_default();
    let swaps_text = fs::read_to_string("/proc/swaps").unwrap_or_default();
    // One parse feeds both consumers of the swap table: the tier-3 fallback
    // name set for the I/O join, and the per-tier device names the tier header
    // prints. An unreadable or malformed table yields an empty set, so tier-3
    // I/O is honestly omitted instead of guessed from unrelated disks and the
    // header names nothing rather than a product constant.
    let swap_entries = cascade::parse_proc_swaps(&swaps_text).unwrap_or_default();
    let fallback_swap_names = cascade::fallback_swap_device_names(&swap_entries);
    let tier_devices = observations::swap_tier_device_names(&swap_entries);
    // `/proc/diskstats` names a device-mapper target by its `dm-N` kernel
    // name. The alias map is the other half of the path↔name join: it lets the
    // tier classifier see through to the alias the role predicates speak.
    // An unreadable sysfs root yields an empty map, so a host without the
    // observation classifies exactly as before.
    let dm_aliases = cascade::dm_name_aliases(Path::new("/sys/block"));
    let uptime = fs::read_to_string("/proc/uptime").unwrap_or_default();
    let (balloon_debugfs, balloon_debugfs_status) =
        read_hyperv_balloon_file(Path::new("/sys/kernel/debug/hv-balloon"));
    let events = fs::read_to_string("/sys/fs/cgroup/ramshared-workloads.slice/memory.events")
        .unwrap_or_default();
    let gpu = gpu_observation_from_status(&status_map);
    let mut control_plane = parse_memory_pressure(&pressure);
    let mem = parse_meminfo(&meminfo);
    let errors = measurement_errors(control_plane.memory_psi_available, &mem);
    let (swap_in_pages, swap_out_pages, pgfault_total, pgmajfault_total) = parse_vmstat(&vmstat);
    let (zram_io, vram_io, disk_io, swap_read_bytes, swap_write_bytes, tier_io_source) =
        collect_per_tier_io(
            Path::new("/sys/block"),
            &swap_entries,
            &diskstats,
            &fallback_swap_names,
            &dm_aliases,
        );
    control_plane.swap_in_pages = swap_in_pages;
    control_plane.swap_out_pages = swap_out_pages;
    control_plane.swap_read_bytes = swap_read_bytes;
    control_plane.swap_write_bytes = swap_write_bytes;
    control_plane.zram_io = zram_io;
    control_plane.vram_io = vram_io;
    control_plane.disk_io = disk_io;
    control_plane.tier_io_source = tier_io_source.map(str::to_string);
    control_plane.pgfault_total = pgfault_total;
    control_plane.pgmajfault_total = pgmajfault_total;
    control_plane.uptime_seconds = parse_uptime_seconds(&uptime);
    control_plane.boot_tier_latency_ms = query_unit_startup_ms();
    control_plane.memory_events = parse_memory_events(&events);
    control_plane.active_scopes =
        count_scope_dirs(Path::new("/sys/fs/cgroup/ramshared-workloads.slice"));
    control_plane.docker_memory_current_bytes = read_optional_u64_file(Path::new(
        "/sys/fs/cgroup/ramshared-workloads.slice/ramshared-workloads-docker.slice/memory.current",
    ));
    let (reservations, reserved_bytes) =
        read_reservation_totals(Path::new("/run/ramshared/admission/reservations.json"));
    control_plane.managed_reservations = reservations;
    control_plane.managed_reserved_bytes = reserved_bytes;
    let (reclaim_speed, reclaim_duration, bench_p50, bench_p99, bench_status) =
        read_benchmark_qualification(benchmark_evidence);
    control_plane.reclaim_speed_gbs = reclaim_speed;
    control_plane.reclaim_duration_ms = reclaim_duration;
    control_plane.benchmark_p50_lat_ms = bench_p50;
    control_plane.benchmark_p99_lat_ms = bench_p99;
    control_plane.benchmark_status = bench_status;
    let (top_processes, process_totals) = collect_process_snapshot(Path::new("/proc"), 10);
    let (unmanaged_state, unmanaged_kib, unmanaged_count) =
        classify_unmanaged_memory_usage(&top_processes);
    control_plane.unmanaged_memory_state = unmanaged_state.into();
    control_plane.unmanaged_memory_kib = unmanaged_kib;
    control_plane.unmanaged_processes = unmanaged_count;

    Ok(Observation {
        status: status_map.into_iter().collect(),
        epoch_ms: unix_epoch_ms(),
        sample_age_ms: 0,
        memory_scope,
        mem,
        cgroup_memory: collect_cgroup_memory(Path::new("/sys/fs/cgroup")),
        hyperv_balloon: parse_hyperv_balloon(
            &balloon_debugfs,
            parse_vmstat_value(&vmstat, "nr_balloon_pages"),
            balloon_debugfs_status,
        ),
        process_totals,
        control_plane,
        gpu,
        tier_devices,
        operation_progress: crate::operation_progress::read_live_operation_progress(),
        top_processes,
        errors,
    })
}

fn mark_observation_refresh_failed(observation: &mut Observation, age: Duration) {
    observation.sample_age_ms = age.as_millis().min(u128::from(u64::MAX)) as u64;
    if !observation
        .errors
        .iter()
        .any(|error| error == "sample_refresh_failed")
    {
        observation.errors.push("sample_refresh_failed".to_string());
    }
}

pub fn run(options: &MonitorOptions) -> Result<(), MonitorError> {
    if options.compact {
        return run_compact(options);
    }
    if options.jsonl || options.once {
        return run_jsonl(options);
    }
    run_tui(options)
}

pub(crate) fn version_status_lines() -> String {
    let mut identity_cache = InstallationIdentityCache::default();
    let metadata = DashboardMetadata::current(
        &mut identity_cache,
        current_process_install_origin(),
        false,
        None,
    );
    format!(
        "{}\n{}\n{}{}{}",
        metadata.version_line,
        metadata.running_line,
        metadata
            .update_notice
            .as_deref()
            .map(|notice| format!("{notice}\n"))
            .unwrap_or_default(),
        metadata.install_lines.join("\n"),
        metadata
            .runtime_notice
            .as_deref()
            .map(|notice| format!("\n{notice}"))
            .unwrap_or_default()
    )
}

#[cfg(test)]
mod benchmark_evidence_tests {
    #![allow(clippy::unwrap_used)]

    use super::observations::read_benchmark_qualification;
    use super::tests::{benchmark_evidence_fixture, monitor_benchmark_path};
    use serde_json::Value;
    use std::fs;

    #[test]
    // TestName: monitor_benchmark_rejects_legacy_unqualified_status
    fn monitor_benchmark_rejects_legacy_unqualified_status() {
        let legacy = serde_json::json!({
            "status": "PASS_ZERO_PANIC",
            "reclaim_speed_gbs": 14.4,
            "reclaim_duration_ms": 1127.0,
            "p50_cycle_latency_ms": 0.0005,
            "p99_cycle_latency_ms": 0.0023
        })
        .to_string();
        let (root, path) = monitor_benchmark_path("legacy", &legacy);

        assert_eq!(
            read_benchmark_qualification(&path),
            (0.0, 0.0, 0.0, 0.0, "AWAITING_QUALIFICATION".to_string())
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    // TestName: monitor_benchmark_accepts_promotable_v1_evidence
    fn monitor_benchmark_accepts_promotable_v1_evidence() {
        let evidence = benchmark_evidence_fixture().to_string();
        let (root, path) = monitor_benchmark_path("qualified", &evidence);

        assert_eq!(
            read_benchmark_qualification(&path),
            (12.0, 22.0, 0.3, 1.2, "PASS".to_string())
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    // TestName: monitor_benchmark_rejects_nonpromotable_evidence
    fn monitor_benchmark_rejects_nonpromotable_evidence() {
        let mut evidence = benchmark_evidence_fixture();
        evidence["comparison"]["qualified"] = Value::Bool(false);
        evidence["decision"]["verdict"] = Value::String("BASELINE".to_string());
        evidence["decision"]["promotable"] = Value::Bool(false);
        let contents = evidence.to_string();
        let (root, path) = monitor_benchmark_path("baseline", &contents);

        assert_eq!(
            read_benchmark_qualification(&path),
            (0.0, 0.0, 0.0, 0.0, "AWAITING_QUALIFICATION".to_string())
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    // TestName: monitor_benchmark_rejects_dirty_or_incomplete_evidence
    fn monitor_benchmark_rejects_dirty_or_incomplete_evidence() {
        let mut dirty = benchmark_evidence_fixture();
        dirty["source"]["dirty"] = Value::Bool(true);
        let dirty_contents = dirty.to_string();
        let (dirty_root, dirty_path) = monitor_benchmark_path("dirty", &dirty_contents);
        assert_eq!(
            read_benchmark_qualification(&dirty_path),
            (0.0, 0.0, 0.0, 0.0, "AWAITING_QUALIFICATION".to_string())
        );
        fs::remove_dir_all(dirty_root).unwrap();

        let mut incomplete = benchmark_evidence_fixture();
        incomplete["metrics"]["reclaim_speed_gbs"] = Value::Null;
        let incomplete_contents = incomplete.to_string();
        let (incomplete_root, incomplete_path) =
            monitor_benchmark_path("incomplete", &incomplete_contents);
        assert_eq!(
            read_benchmark_qualification(&incomplete_path),
            (0.0, 0.0, 0.0, 0.0, "AWAITING_QUALIFICATION".to_string())
        );
        fs::remove_dir_all(incomplete_root).unwrap();

        let mut forged = benchmark_evidence_fixture();
        forged["metrics"]["reclaim_speed_gbs"]["median"] = Value::from(900.0);
        let forged_contents = forged.to_string();
        let (forged_root, forged_path) = monitor_benchmark_path("forged", &forged_contents);
        assert_eq!(
            read_benchmark_qualification(&forged_path),
            (0.0, 0.0, 0.0, 0.0, "AWAITING_QUALIFICATION".to_string())
        );
        fs::remove_dir_all(forged_root).unwrap();
    }
}

#[cfg(test)]
#[path = "monitor/tests.rs"]
mod tests;
