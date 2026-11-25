// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/observations.rs
//! Bounded reads that turn host state into snapshot model values.
//!
//! This module is the observation side of the monitor: pure text parsers over
//! `/proc` and `/sys` content, small file readers, and the adapters that fold
//! a cascade status map or a benchmark-evidence document into model fields.
//! It opens nothing for writing, decides no health or pressure state, applies
//! no policy and renders no terminal output.
//!
//! The DT-10 reserve derivation stays in the parent module: it is policy over
//! [`ramshared_vram`], not snapshot shape. This module calls up to it.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use super::model::{
    CgroupMemoryObservation, ControlPlaneObservation, HyperVBalloonObservation, MemoryEvents,
    MemoryObservation, ProcessMemoryTotals, ProcessObservation, TierDeviceNames,
};
use crate::cascade::{SwapEntry, SwapTierRole, swap_tier_role};

#[cfg_attr(not(test), allow(dead_code))]
mod block_io;
#[cfg_attr(not(test), allow(dead_code))]
mod telemetry_docs;
pub(super) use block_io::collect_per_tier_io;
#[cfg(test)]
pub(super) use block_io::{parse_per_tier_diskstats, parse_swap_diskstats};
#[cfg_attr(not(test), allow(unused_imports))]
pub(super) use telemetry_docs::{
    BENCHMARK_STATUS_AWAITING, BENCHMARK_STATUS_PATH_NOT_FOUND, cache_telemetry_record,
    gpu_observation_from_status, read_benchmark_qualification, read_reservation_totals,
};

pub(super) fn unix_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

pub(super) fn parse_meminfo(text: &str) -> MemoryObservation {
    let optional_value = |name: &str| parse_meminfo_value(text, name);
    // Each pair is one measurement and fails closed together. A missing half
    // and a pair that disagrees with itself both have no reading, and a tear
    // is the same face as absence: `None`, never a fabricated `0`. This is the
    // same predicate `resource_config.rs` already applies to these four
    // counters. Measured empty swap (`SwapTotal: 0`, `SwapFree: 0`) stays
    // `Some(0)`; `MemTotal: 0` is a tear and has no reading.
    let (total_kib, available_kib) =
        match (optional_value("MemTotal"), optional_value("MemAvailable")) {
            (Some(total), Some(available)) if total > 0 && available <= total => {
                (Some(total), Some(available))
            }
            _ => (None, None),
        };
    let (swap_total_kib, swap_free_kib) =
        match (optional_value("SwapTotal"), optional_value("SwapFree")) {
            (Some(total), Some(free)) if free <= total => (Some(total), Some(free)),
            _ => (None, None),
        };
    MemoryObservation {
        total_kib,
        available_kib,
        swap_total_kib,
        swap_free_kib,
        anon_pages_kib: optional_value("AnonPages"),
        shmem_kib: optional_value("Shmem"),
        slab_kib: optional_value("Slab"),
        s_unreclaim_kib: optional_value("SUnreclaim"),
        dirty_kib: optional_value("Dirty"),
        writeback_kib: optional_value("Writeback"),
    }
}

fn parse_meminfo_value(text: &str, name: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let (key, rest) = line.split_once(':')?;
        (key == name)
            .then(|| rest.split_whitespace().next()?.parse::<u64>().ok())
            .flatten()
    })
}

pub(super) fn parse_vmstat_value(text: &str, name: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        (fields.next()? == name)
            .then(|| fields.next()?.parse::<u64>().ok())
            .flatten()
    })
}

pub(super) fn parse_hyperv_balloon(
    debugfs: &str,
    nr_balloon_pages: Option<u64>,
    debugfs_status: &str,
) -> HyperVBalloonObservation {
    let mut balloon = HyperVBalloonObservation {
        debugfs_status: debugfs_status.to_owned(),
        nr_balloon_pages,
        ..HyperVBalloonObservation::default()
    };
    for line in debugfs.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "host_version" => balloon.host_version = nonempty(value),
            "capabilities" => balloon.capabilities = nonempty(value),
            "state" => balloon.state = nonempty(value),
            "page_size" => balloon.page_size = value.parse().ok(),
            "pages_added" => balloon.pages_added = value.parse().ok(),
            "pages_onlined" => balloon.pages_onlined = value.parse().ok(),
            "pages_ballooned" => balloon.pages_ballooned = value.parse().ok(),
            "total_pages_committed" => balloon.total_pages_committed = value.parse().ok(),
            "max_dynamic_page_count" => balloon.max_dynamic_page_count = value.parse().ok(),
            _ => {}
        }
    }

    balloon
}

fn nonempty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

pub(super) fn read_hyperv_balloon_file(path: &Path) -> (String, &'static str) {
    match fs::read_to_string(path) {
        Ok(contents) => (contents, "readable"),
        Err(error) => {
            let reason = match error.kind() {
                std::io::ErrorKind::PermissionDenied => "permission_denied",
                std::io::ErrorKind::NotFound => "not_found",
                _ => "read_error",
            };
            (String::new(), reason)
        }
    }
}

pub(super) fn collect_cgroup_memory(root: &Path) -> CgroupMemoryObservation {
    let current_bytes = read_optional_u64_file(&root.join("memory.current"));
    let events = fs::read_to_string(root.join("memory.events"))
        .ok()
        .map(|text| parse_memory_events(&text));
    let root_direct_processes = fs::read_to_string(root.join("cgroup.procs"))
        .ok()
        .map(|text| text.lines().filter(|line| !line.trim().is_empty()).count() as u64);

    let mut subgroup_current_bytes = 0u64;
    let mut subgroups_with_memory = 0u64;
    if current_bytes.is_none() {
        for entry in fs::read_dir(root)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_ok_and(|file_type| file_type.is_dir()) {
                continue;
            }
            if let Some(current) = read_optional_u64_file(&entry.path().join("memory.current")) {
                subgroup_current_bytes = subgroup_current_bytes.saturating_add(current);
                subgroups_with_memory = subgroups_with_memory.saturating_add(1);
            }
        }
    }

    let status = if current_bytes.is_some() {
        "root"
    } else if subgroups_with_memory > 0 {
        "partial"
    } else {
        "unavailable"
    };
    CgroupMemoryObservation {
        status: status.to_owned(),
        current_bytes,
        events,
        subgroup_current_bytes: (subgroups_with_memory > 0).then_some(subgroup_current_bytes),
        subgroups_with_memory,
        root_direct_processes,
    }
}

pub(super) fn parse_memory_pressure(text: &str) -> ControlPlaneObservation {
    fn averages(line: Option<&str>) -> Option<[f64; 3]> {
        let line = line?;
        let mut values = [None; 3];
        for field in line.split_whitespace().skip(1) {
            for (index, name) in ["avg10=", "avg60=", "avg300="].iter().enumerate() {
                if let Some(raw) = field.strip_prefix(name) {
                    if values[index].is_some() {
                        return None;
                    }
                    let value = raw.parse::<f64>().ok()?;
                    if !value.is_finite() || !(0.0..=100.0).contains(&value) {
                        return None;
                    }
                    values[index] = Some(value);
                }
            }
        }
        Some([values[0]?, values[1]?, values[2]?])
    }

    let some = averages(text.lines().find(|line| line.starts_with("some ")));
    let full = averages(text.lines().find(|line| line.starts_with("full ")));
    let some_values = some.unwrap_or([0.0; 3]);
    let full_values = full.unwrap_or([0.0; 3]);
    ControlPlaneObservation {
        memory_psi_available: some.is_some() && full.is_some(),
        memory_psi_some_avg10: some_values[0],
        memory_psi_some_avg60: some_values[1],
        memory_psi_some_avg300: some_values[2],
        memory_psi_full_avg10: full_values[0],
        memory_psi_full_avg60: full_values[1],
        memory_psi_full_avg300: full_values[2],
        ..ControlPlaneObservation::default()
    }
}

/// Base-page size that `/proc/vmstat`'s `pswpin` / `pswpout` counters are
/// counted in.
///
/// The counters are page counts, not byte counts, so a byte figure derived
/// from them needs this factor. It is named beside the reader that produces
/// the page counts rather than left as a literal at the display site, so the
/// unit contract is declared once and a reader of [`parse_vmstat`] learns the
/// unit from the same place. This is the guest's base page size — the one
/// `mm` accounts swap against — not the huge-page size and not the DMA
/// registration granularity `ramshared-cuda` aligns host buffers to.
pub(super) const VMSTAT_PAGE_BYTES: u64 = 4096;

/// Read the swap and page-fault counters out of a `/proc/vmstat` body.
///
/// Every counter is `Option` on the same terms as [`parse_memory_events`]: a
/// key the kernel did not export, a value that does not parse and a body that
/// could not be read all mean **not measured** — never measured zero. A
/// present `0` is a real zero and is kept as `Some(0)`.
///
/// The previous shape returned `unwrap_or(0)` for each counter, so an
/// unreadable `/proc/vmstat` (the caller's `read_to_string(...).unwrap_or_default()`
/// hands this an empty body) published four measured zeros. Those zeros were
/// then differenced against the next real reading by the rate fold, which
/// produced both a fabricated idle and, on the following frame, a fabricated
/// spike. Absence is a per-counter fact: one missing line does not make the
/// other three zero.
pub(super) fn parse_vmstat(text: &str) -> (Option<u64>, Option<u64>, Option<u64>, Option<u64>) {
    (
        parse_vmstat_value(text, "pswpin"),
        parse_vmstat_value(text, "pswpout"),
        parse_vmstat_value(text, "pgfault"),
        parse_vmstat_value(text, "pgmajfault"),
    )
}

/// Kernel basenames of the live swap devices each tier is served by.
///
/// The tier header prints this, so it must be what `/proc/swaps` listed rather
/// than a product constant: tier 2 is an `nbd` node on one host and a `ublk` or
/// logical device on another. Identity is the basename after the last `/` —
/// the same join the `diskstats` classification uses. A tier can be served by
/// more than one device, so the header joins them; two entries can also share
/// a basename (`/dev/foo/bar` and `/dev/bar` are different devices with the
/// same kernel name), so the fold deduplicates rather than printing a name
/// twice. Sorting makes the header deterministic.
///
/// A ghost is skipped: the kernel already marked the device `(deleted)`, so it
/// has no live name to print and no live I/O behind the row either. Its bytes
/// are already excluded from the tier folds for the same reason.
/// The state of Tier 1 as the kernel reports it (SPEC DT-3).
///
/// `backing_dev` of `none` with a live zram swap device is the degraded
/// face: the tier serves from RAM and the authoritative copy below is
/// unreachable. A tier that cannot name its authoritative copy does not
/// report `active`. This is the single derivation; the dashboard row and
/// `status --json` both read its result and neither re-derives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ZramTierState {
    /// Live zram swap device(s) with a backing device bound.
    Active,
    /// Live zram swap device(s) whose backing device is `none`.
    Degraded,
    /// No live zram swap device: there is no Tier 1 to describe.
    Inactive,
    /// A live zram device whose backing attribute could not be read. The
    /// state is a named absence, never invented from a peer device.
    Unmeasured,
}

impl ZramTierState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Degraded => "degraded",
            Self::Inactive => "inactive",
            Self::Unmeasured => "unmeasured",
        }
    }
}

/// Derive Tier 1's state from the zram sysfs `backing_dev` attribute plus
/// the swap table (SPEC DT-3).
///
/// `sys_block` is the zram sysfs root (`/sys/block` in production, a
/// fixture in tests). Every live zram swap device is checked: one whose
/// attribute will not read makes the whole state `unmeasured` rather than
/// letting the devices that did answer speak for it. Any `none` among the
/// answers is the degraded face.
pub(crate) fn zram_tier_state_from(sys_block: &Path, entries: &[SwapEntry]) -> ZramTierState {
    let mut seen = false;
    let mut degraded = false;
    for entry in entries {
        if entry.is_ghost() || swap_tier_role(&entry.filename) != SwapTierRole::Zram {
            continue;
        }
        let Some(name) = Path::new(&entry.filename)
            .file_name()
            .and_then(|name| name.to_str())
        else {
            continue;
        };
        seen = true;
        match fs::read_to_string(sys_block.join(name).join("backing_dev")) {
            Ok(text) => {
                let value = text.trim();
                if value == "none" {
                    degraded = true;
                } else if value.is_empty() {
                    return ZramTierState::Unmeasured;
                }
            }
            Err(_) => return ZramTierState::Unmeasured,
        }
    }
    if !seen {
        return ZramTierState::Inactive;
    }
    if degraded {
        ZramTierState::Degraded
    } else {
        ZramTierState::Active
    }
}

pub(super) fn swap_tier_device_names(entries: &[SwapEntry]) -> TierDeviceNames {
    let mut names = TierDeviceNames::default();
    for entry in entries {
        if entry.is_ghost() {
            continue;
        }
        let Some(basename) = Path::new(&entry.filename)
            .file_name()
            .and_then(|name| name.to_str())
        else {
            continue;
        };
        let target = match swap_tier_role(&entry.filename) {
            SwapTierRole::Zram => &mut names.zram,
            SwapTierRole::Vram => &mut names.vram,
            SwapTierRole::Fallback => &mut names.disk,
        };
        target.push(basename.to_string());
    }
    for list in [&mut names.zram, &mut names.vram, &mut names.disk] {
        list.sort();
        list.dedup();
    }
    names
}

pub fn parse_unit_startup_ms(show_output: &str) -> Option<u64> {
    let mut inactive_exit = None;
    let mut active_enter = None;

    for line in show_output.lines() {
        if line.is_empty() {
            if let (Some(start), Some(end)) = (inactive_exit, active_enter)
                && end > start
                && start > 0
            {
                return Some((end - start) / 1000);
            }
            inactive_exit = None;
            active_enter = None;
            continue;
        }
        if let Some(val) = line.strip_prefix("InactiveExitTimestampMonotonic=") {
            inactive_exit = val.trim().parse::<u64>().ok();
        } else if let Some(val) = line.strip_prefix("ActiveEnterTimestampMonotonic=") {
            active_enter = val.trim().parse::<u64>().ok();
        }
    }

    if let (Some(start), Some(end)) = (inactive_exit, active_enter)
        && end > start
        && start > 0
    {
        return Some((end - start) / 1000);
    }
    None
}

pub fn parse_uptime_seconds(uptime_str: &str) -> Option<u64> {
    uptime_str
        .split_whitespace()
        .next()
        .and_then(|s| s.parse::<f64>().ok())
        .map(|f| f as u64)
}

pub(super) fn query_unit_startup_ms() -> Option<u64> {
    let output = Command::new("systemctl")
        .args([
            "show",
            "ramshared-vram-tier.service",
            "ramshared-vram.service",
            "--property=ActiveEnterTimestampMonotonic,InactiveExitTimestampMonotonic",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8_lossy(&output.stdout);
    parse_unit_startup_ms(&text)
}

/// Read the named counters out of a cgroup `memory.events` body.
///
/// Every counter is `Option` because a key the kernel did not export, a value
/// that does not parse and a body that could not be read all mean **not
/// measured** — never measured zero. A present `0` is a real zero and is kept
/// as `Some(0)`. The first line whose leading token is the name is the
/// reading; a later duplicate line is not a second chance to find a value.
pub(super) fn parse_memory_events(text: &str) -> MemoryEvents {
    let value = |name: &str| -> Option<u64> {
        for line in text.lines() {
            let mut fields = line.split_whitespace();
            if fields.next() != Some(name) {
                continue;
            }
            return fields.next().and_then(|raw| raw.parse::<u64>().ok());
        }
        None
    };
    MemoryEvents {
        high: value("high"),
        max: value("max"),
        oom: value("oom"),
        oom_kill: value("oom_kill"),
    }
}

/// Read one decimal counter from a file.
///
/// `None` for an unreadable file, an empty body and an unparseable body alike:
/// none of those measured zero. A readable `0` is `Some(0)`.
pub(super) fn read_optional_u64_file(path: &Path) -> Option<u64> {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse().ok())
}

pub(super) fn count_scope_dirs(path: &Path) -> u64 {
    fs::read_dir(path)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".scope"))
        .count() as u64
}

pub(super) fn collect_process_snapshot(
    proc_root: &Path,
    limit: usize,
) -> (Vec<ProcessObservation>, ProcessMemoryTotals) {
    let mut processes = fs::read_dir(proc_root)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .bytes()
                .all(|byte| byte.is_ascii_digit())
        })
        .filter_map(|entry| process_observation(&entry.path()))
        .collect::<Vec<_>>();
    let totals = fold_process_memory_totals(&processes);
    // Largest measured footprint first; processes whose footprint never
    // arrived sort last so a missing figure cannot outrank a measured one.
    processes.sort_by_key(|process| std::cmp::Reverse(process.footprint_kib()));
    processes.truncate(limit);
    (processes, totals)
}

/// Fail-closed totals: each figure is `Some` only when every visible process
/// contributed that figure. rss and swap fail independently — a missing
/// VmSwap never withholds the rss total, and a partial sum is never published
/// as a whole.
fn fold_process_memory_totals(processes: &[ProcessObservation]) -> ProcessMemoryTotals {
    let mut totals = ProcessMemoryTotals {
        visible_processes: processes.len() as u64,
        rss_kib: Some(0),
        swap_kib: Some(0),
    };
    for process in processes {
        totals.rss_kib = match (totals.rss_kib, process.rss_kib) {
            (Some(sum), Some(rss)) => Some(sum.saturating_add(rss)),
            _ => None,
        };
        totals.swap_kib = match (totals.swap_kib, process.swap_kib) {
            (Some(sum), Some(swap)) => Some(sum.saturating_add(swap)),
            _ => None,
        };
    }
    totals
}

fn process_observation(path: &Path) -> Option<ProcessObservation> {
    let comm = fs::read_to_string(path.join("comm")).ok()?;
    let comm = sanitize_label(comm.trim(), 64);
    let cgroup_text = fs::read_to_string(path.join("cgroup")).unwrap_or_default();
    let cgroup = cgroup_text
        .lines()
        .find_map(|line| line.split_once("::").map(|(_, value)| value))
        .map(|value| sanitize_cgroup(value, 256))
        .unwrap_or_else(|| "/".into());
    let unit = cgroup
        .split('/')
        .rev()
        .find(|component| {
            component.ends_with(".scope")
                || component.ends_with(".service")
                || component.ends_with(".slice")
        })
        .map(|value| sanitize_label(value, 128))
        .unwrap_or_else(|| "unknown".into());
    let status = fs::read_to_string(path.join("status")).unwrap_or_default();
    // Absence is `None`, never a fabricated `0`: a kernel that never exported
    // VmRSS did not observe a process holding no memory.
    let status_kib = |name: &str| {
        status.lines().find_map(|line| {
            let (key, rest) = line.split_once(':')?;
            (key == name)
                .then(|| rest.split_whitespace().next()?.parse::<u64>().ok())
                .flatten()
        })
    };
    let stat = fs::read_to_string(path.join("stat")).unwrap_or_default();
    let cpu_ticks = stat
        .rsplit_once(") ")
        .map(|(_, fields)| fields.split_whitespace().collect::<Vec<_>>())
        .and_then(|fields| {
            Some(fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?)
        });
    let io = fs::read_to_string(path.join("io")).unwrap_or_default();
    let io_value = |name: &str| {
        io.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key == name)
                .then(|| value.trim().parse::<u64>().ok())
                .flatten()
        })
    };
    Some(ProcessObservation {
        comm,
        unit,
        managed: cgroup.contains("ramshared-workloads"),
        cgroup,
        rss_kib: status_kib("VmRSS"),
        swap_kib: status_kib("VmSwap"),
        cpu_ticks,
        read_bytes: io_value("read_bytes"),
        write_bytes: io_value("write_bytes"),
    })
}

pub(super) fn sanitize_label(value: &str, limit: usize) -> String {
    value
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-' | '@')
        })
        .take(limit)
        .collect()
}

pub(super) fn sanitize_cgroup(value: &str, limit: usize) -> String {
    value
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, '/' | '.' | '_' | '-' | '@' | ':')
        })
        .take(limit)
        .collect()
}

#[cfg(test)]
mod observation_honesty_tests {
    #![allow(clippy::unwrap_used)]

    use super::parse_meminfo;

    #[test]
    fn memory_observation_emits_none_for_missing_fields_never_zero() {
        let observation =
            parse_meminfo("MemTotal: 1024 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\nDirty: 0 kB\n");
        assert_eq!(observation.total_kib, None);
        assert_eq!(observation.available_kib, None);
        assert_eq!(observation.swap_total_kib, Some(0));
        assert_eq!(observation.swap_free_kib, Some(0));
        assert_eq!(observation.dirty_kib, Some(0));
        let json = serde_json::to_value(&observation).unwrap();
        assert!(json["available_kib"].is_null());
        assert_eq!(json["swap_total_kib"], serde_json::json!(0));
        let complete = parse_meminfo(
            "MemTotal: 2048 kB
MemAvailable: 1024 kB
SwapTotal: 0 kB
SwapFree: 0 kB
",
        );
        assert_eq!(complete.total_kib, Some(2048));
        assert_eq!(complete.available_kib, Some(1024));
    }
}
