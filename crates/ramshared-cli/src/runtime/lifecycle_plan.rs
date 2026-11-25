// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/runtime/lifecycle_plan.rs
// Read-only native lifecycle plan (SPEC zero-script-native-runtime ITEM-3).
// This module observes swap and zram state and computes the pre-swapoff
// headroom verdict. It never writes sysfs, calls swapon/swapoff, or changes a
// service. `up --plan` is the operator-visible surface.

use std::io::Write;
use std::path::PathBuf;

use serde::Serialize;

use super::linux_swap_sys::{SwapoffFeasibility, swapoff_feasibility};
use super::preflight_procfs::{
    MemoryPressure, ProcfsPreflight, StatVfs, SwapEntry as ProcfsSwapEntry, statvfs,
};
use super::zram_sysfs::{ZRAM_SYSFS_ROOT, ZramSysfs};

pub const PLAN_SCHEMA_VERSION: u8 = 1;

#[derive(Debug, Serialize)]
struct SwapRow {
    filename: String,
    kind: String,
    size_kib: u64,
    used_kib: u64,
    priority: i32,
}

#[derive(Debug, Serialize)]
struct ZramRow {
    id: u32,
    disksize_bytes: Option<String>,
    mem_used_bytes: Option<String>,
    backing_dev: Option<String>,
}

#[derive(Debug, Serialize)]
struct SwapoffPlan {
    target: String,
    verdict: &'static str,
    target_used_kib: u64,
    available_kib: u64,
    swap_free_kib: u64,
    required_kib: u64,
}

#[derive(Debug, Serialize)]
struct LifecyclePlan {
    schema_version: u8,
    action: &'static str,
    swap: Vec<SwapRow>,
    zram: Vec<ZramRow>,
    swapoff: Option<SwapoffPlan>,
    pressure: Option<MemoryPressure>,
    target_fs: Option<StatVfs>,
    effects: Vec<&'static str>,
}

impl LifecyclePlan {
    fn from_observations(
        swaps: &[ProcfsSwapEntry],
        feasibility: Option<SwapoffFeasibility>,
        pressure: Option<MemoryPressure>,
        target_fs: Option<StatVfs>,
    ) -> Self {
        let swap = swaps
            .iter()
            .map(|entry| SwapRow {
                filename: entry.filename.clone(),
                kind: entry.kind.clone(),
                size_kib: entry.size_kib,
                used_kib: entry.used_kib,
                priority: entry.priority,
            })
            .collect();
        let swapoff = swaps.first().map(|entry| {
            let (verdict, numbers) = match feasibility {
                Some(feasibility) if feasibility.safe => ("safe", feasibility),
                Some(feasibility) => ("refused", feasibility),
                None => (
                    "unknown",
                    SwapoffFeasibility {
                        target_used_kib: entry.used_kib,
                        target_free_kib: 0,
                        available_kib: 0,
                        swap_free_kib: 0,
                        required_kib: 0,
                        safe: false,
                    },
                ),
            };
            SwapoffPlan {
                target: entry.filename.clone(),
                verdict,
                target_used_kib: numbers.target_used_kib,
                available_kib: numbers.available_kib,
                swap_free_kib: numbers.swap_free_kib,
                required_kib: numbers.required_kib,
            }
        });
        Self {
            schema_version: PLAN_SCHEMA_VERSION,
            action: "up-plan",
            swap,
            zram: observe_zram(),
            swapoff,
            pressure,
            target_fs,
            effects: Vec::new(),
        }
    }
}

/// Observe the host and build the read-only plan. No mutating call exists in
/// this path: `/proc` and sysfs are opened read-only and the swapoff decision
/// is the same pure arithmetic used before the real syscall.
fn build() -> Result<LifecyclePlan, String> {
    let mut preflight = ProcfsPreflight::new();
    let swaps = preflight.read_swaps().map_err(|error| error.to_string())?;
    let mem = preflight
        .read_meminfo()
        .map_err(|error| error.to_string())?;
    let feasibility = swaps.first().and_then(|entry| {
        swapoff_feasibility(&PathBuf::from(&entry.filename), &mem, &swaps, None).ok()
    });
    // Pressure and the target filesystem are independent measurements: an
    // unreadable source is `None`, never a fabricated zero.
    let pressure = preflight.read_pressure().ok();
    let target_fs = swaps
        .first()
        .and_then(|entry| statvfs(std::path::Path::new(&entry.filename)).ok());
    Ok(LifecyclePlan::from_observations(
        &swaps,
        feasibility,
        pressure,
        target_fs,
    ))
}

fn observe_zram() -> Vec<ZramRow> {
    observe_zram_at(std::path::Path::new(ZRAM_SYSFS_ROOT), true)
}

fn observe_zram_at(root: &std::path::Path, require_magic: bool) -> Vec<ZramRow> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(id) = name
            .to_str()
            .and_then(|name| name.strip_prefix("zram"))
            .and_then(|id| id.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(zram) = ZramSysfs::open_root_checked(root, id, require_magic) else {
            continue;
        };
        rows.push(ZramRow {
            id,
            disksize_bytes: zram.read_attr("disksize").ok(),
            mem_used_bytes: zram.read_attr("mem_used_total").ok(),
            backing_dev: zram.read_attr("backing_dev").ok(),
        });
    }
    rows.sort_by_key(|row| row.id);
    rows
}

/// Recognize the exact read-only plan spellings. Anything else continues to
/// the existing `up` parser, so no ordinary `up` invocation changes shape.
pub(crate) fn plan_json_request(args: &[String]) -> Option<bool> {
    match args {
        [one] if one == "--plan" => Some(false),
        [a, b] if (a == "--plan" && b == "--json") || (a == "--json" && b == "--plan") => {
            Some(true)
        }
        _ => None,
    }
}

pub(crate) fn render(json: bool, stdout: &mut dyn Write) -> Result<(), String> {
    let plan = build()?;
    render_plan(&plan, json, stdout)
}

fn render_plan(plan: &LifecyclePlan, json: bool, stdout: &mut dyn Write) -> Result<(), String> {
    if json {
        let payload = serde_json::to_string(&plan)
            .map_err(|error| format!("up plan serialization failed: {error}"))?;
        writeln!(stdout, "{payload}").map_err(|error| error.to_string())?;
        return Ok(());
    }
    writeln!(
        stdout,
        "up plan (read-only, schema {})",
        plan.schema_version
    )
    .map_err(|error| error.to_string())?;
    for row in &plan.swap {
        writeln!(
            stdout,
            "swap: {} type={} size={} KiB used={} KiB prio={}",
            row.filename, row.kind, row.size_kib, row.used_kib, row.priority
        )
        .map_err(|error| error.to_string())?;
    }
    for row in &plan.zram {
        writeln!(
            stdout,
            "zram{}: disksize={} mem_used={} backing_dev={}",
            row.id,
            row.disksize_bytes.as_deref().unwrap_or("?"),
            row.mem_used_bytes.as_deref().unwrap_or("?"),
            row.backing_dev.as_deref().unwrap_or("?")
        )
        .map_err(|error| error.to_string())?;
    }
    match &plan.swapoff {
        Some(swapoff) => writeln!(
            stdout,
            "swapoff: {} target={} used={} KiB available={} KiB swap_free={} KiB required={} KiB",
            swapoff.verdict,
            swapoff.target,
            swapoff.target_used_kib,
            swapoff.available_kib,
            swapoff.swap_free_kib,
            swapoff.required_kib
        )
        .map_err(|error| error.to_string())?,
        None => writeln!(stdout, "swapoff: no active swap").map_err(|error| error.to_string())?,
    }
    writeln!(stdout, "effects: none").map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn swap(used_kib: u64) -> ProcfsSwapEntry {
        ProcfsSwapEntry {
            filename: "/dev/ramshared-plan-test".to_string(),
            kind: "partition".to_string(),
            size_kib: 1024,
            used_kib,
            priority: -2,
            dev: None,
            ino: None,
        }
    }

    #[test]
    fn plan_marks_a_safe_swapoff_and_returns_no_effects() {
        let feasibility = SwapoffFeasibility {
            target_used_kib: 100,
            target_free_kib: 0,
            available_kib: 1024,
            swap_free_kib: 0,
            required_kib: 100
                + ((ramshared_tier::MIN_DRAIN_RESERVE_BYTES + ramshared_tier::DRAIN_MARGIN_BYTES)
                    / 1024),
            safe: true,
        };
        let plan = LifecyclePlan::from_observations(&[swap(100)], Some(feasibility), None, None);
        assert_eq!(plan.schema_version, PLAN_SCHEMA_VERSION);
        assert_eq!(plan.action, "up-plan");
        assert!(plan.effects.is_empty());
        assert_eq!(plan.swapoff.as_ref().unwrap().verdict, "safe");
        assert_eq!(plan.swap.len(), 1);
    }

    #[test]
    fn plan_marks_an_insufficient_swapoff_as_refused_without_mutating() {
        let feasibility = SwapoffFeasibility {
            target_used_kib: 2 * 1024 * 1024,
            target_free_kib: 0,
            available_kib: 256 * 1024,
            swap_free_kib: 0,
            required_kib: 2 * 1024 * 1024
                + ((ramshared_tier::MIN_DRAIN_RESERVE_BYTES + ramshared_tier::DRAIN_MARGIN_BYTES)
                    / 1024),
            safe: false,
        };
        let plan = LifecyclePlan::from_observations(
            &[swap(2 * 1024 * 1024)],
            Some(feasibility),
            None,
            None,
        );
        assert_eq!(plan.swapoff.as_ref().unwrap().verdict, "refused");
        assert!(plan.effects.is_empty());
    }

    #[test]
    fn plan_spellings_are_exact_and_read_only() {
        assert_eq!(plan_json_request(&["--plan".to_string()]), Some(false));
        assert_eq!(
            plan_json_request(&["--plan".to_string(), "--json".to_string()]),
            Some(true)
        );
        assert_eq!(
            plan_json_request(&["--json".to_string(), "--plan".to_string()]),
            Some(true)
        );
        assert_eq!(plan_json_request(&["--vram".to_string()]), None);
        assert_eq!(
            plan_json_request(&["--plan".to_string(), "--vram".to_string()]),
            None
        );
    }

    #[test]
    fn plan_marks_unknown_when_feasibility_is_absent() {
        let plan = LifecyclePlan::from_observations(&[swap(100)], None, None, None);
        assert_eq!(plan.swapoff.as_ref().unwrap().verdict, "unknown");
        assert!(plan.effects.is_empty());
    }

    #[test]
    fn render_plan_text_and_json_are_total() {
        let feasibility = SwapoffFeasibility {
            target_used_kib: 100,
            target_free_kib: 0,
            available_kib: 1024,
            swap_free_kib: 0,
            required_kib: 100
                + ((ramshared_tier::MIN_DRAIN_RESERVE_BYTES + ramshared_tier::DRAIN_MARGIN_BYTES)
                    / 1024),
            safe: true,
        };
        let plan = LifecyclePlan::from_observations(&[swap(100)], Some(feasibility), None, None);
        let mut text = Vec::new();
        render_plan(&plan, false, &mut text).unwrap();
        let text = String::from_utf8(text).unwrap();
        assert!(text.contains("up plan (read-only"));
        assert!(text.contains("swapoff: safe"));
        assert!(text.contains("effects: none"));

        let mut json = Vec::new();
        render_plan(&plan, true, &mut json).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(value["action"], serde_json::json!("up-plan"));
        assert_eq!(value["effects"], serde_json::json!([]));

        let empty = LifecyclePlan::from_observations(&[], None, None, None);
        let mut text = Vec::new();
        render_plan(&empty, false, &mut text).unwrap();
        assert!(String::from_utf8(text).unwrap().contains("no active swap"));
    }

    #[test]
    fn observe_zram_reads_a_temp_root() {
        let root = std::env::temp_dir().join(format!("ramshared-zram-plan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("zram0")).unwrap();
        for (name, value) in [
            ("disksize", "1048576"),
            ("mem_used_total", "4096"),
            ("backing_dev", "/dev/nbd0"),
        ] {
            std::fs::write(root.join("zram0").join(name), value).unwrap();
        }
        let rows = observe_zram_at(&root, false);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, 0);
        assert_eq!(rows[0].disksize_bytes.as_deref(), Some("1048576"));
        assert_eq!(rows[0].mem_used_bytes.as_deref(), Some("4096"));
        assert_eq!(rows[0].backing_dev.as_deref(), Some("/dev/nbd0"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn build_observes_the_host_without_effects() {
        let plan = build().unwrap();
        assert_eq!(plan.schema_version, PLAN_SCHEMA_VERSION);
        assert_eq!(plan.action, "up-plan");
        assert!(plan.effects.is_empty());
    }
}
