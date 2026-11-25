// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/status_view.rs
//! Capacity guarantee probes, demote telemetry readers, daemon PID lookup, and `ramshared status` rendering.
//!
//! Extracted from `cascade/mod.rs` to isolate status inspection and output formatting from snapshot construction.

use super::*;
use std::fs;

pub(super) fn capacity_field(key: &str) -> Option<String> {
    let text = fs::read_to_string(CAPACITY_STATUS_FILE).ok()?;
    text.lines().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        (name.trim() == key).then(|| value.trim().to_string())
    })
}

pub(super) fn capacity_field_u64(key: &str) -> Option<u64> {
    capacity_field(key)?.parse().ok()
}

pub(super) fn read_capacity_disk_baseline() -> Option<u64> {
    let text = fs::read_to_string(CAPACITY_STATUS_FILE).ok()?;
    text.lines().find_map(|line| {
        line.strip_prefix("disk_baseline_kib=")
            .and_then(|value| value.trim().parse::<u64>().ok())
    })
}

/// Classify one capacity-marker stat into the three-state witness.
///
/// `Some(true)` is a measured positive: the marker path is a regular file.
/// `Some(false)` is a measured refutation: the marker was looked for and is
/// absent, or the path exists and is not a regular file. `None` is never
/// taken: `stat` failed for a reason that is neither absence nor a
/// classified wrong type.
///
/// `Path::is_file()` folds **every** metadata failure into `false`, so an
/// `EACCES` / `EIO` / `ESTALE` on the way to the marker published a measured
/// `capacity_not_guaranteed` on a frame that never looked. `NotFound` is
/// deliberately not that: the marker is the product's positive guarantee
/// file, so its measured absence *is* a refutation.
pub(super) fn capacity_guaranteed_from_stat(stat: Result<bool, std::io::Error>) -> Option<bool> {
    match stat {
        Ok(is_file) => Some(is_file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
    }
}

/// The capacity marker's witness for this frame.
///
/// `fs::metadata` rather than `symlink_metadata`, so the marker keeps the
/// follow-symlinks semantics `Path::is_file()` gave it. This slice is
/// witness honesty, not a change to what counts as the marker.
pub(super) fn observe_capacity_guaranteed() -> Option<bool> {
    capacity_guaranteed_from_stat(
        fs::metadata(CAPACITY_STATUS_FILE).map(|metadata| metadata.is_file()),
    )
}

/// Read `/run/ramshared/demote-status.json` if present (daemon ITEM-3).
/// Absence of the file or an unparseable body yields a snapshot whose
/// fields are all `None` ("never taken"), never a measured refutation.
pub(super) fn read_demote_snapshot() -> DemoteSnapshot {
    read_demote_snapshot_at(Path::new(DEMOTE_STATUS_FILE))
}

pub(super) fn read_demote_snapshot_at(path: &Path) -> DemoteSnapshot {
    let Some(bytes) = crate::release_identity::read_regular_bytes(
        path,
        health_documents::HEALTH_DOCUMENT_MAX_BYTES,
    ) else {
        return DemoteSnapshot::default();
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return DemoteSnapshot::default();
    };
    parse_demote_status_file(&text).unwrap_or_default()
}

/// Parse the daemon status as a complete JSON object; missing or non-boolean
/// progress remains unknown, never a measured false.
pub(super) fn parse_demote_status_file(text: &str) -> Option<DemoteSnapshot> {
    #[derive(serde::Deserialize)]
    struct DemoteDocument {
        total: u64,
        #[serde(default)]
        last_reason: Option<serde_json::Value>,
        #[serde(default)]
        in_progress: Option<serde_json::Value>,
    }
    let document: DemoteDocument = serde_json::from_str(text).ok()?;
    Some(DemoteSnapshot {
        total: Some(document.total),
        last_reason: document
            .last_reason
            .as_ref()
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        in_progress: document
            .in_progress
            .as_ref()
            .and_then(serde_json::Value::as_bool),
    })
}

pub(super) fn daemon_alive_pid_from(
    pid_record: Option<&str>,
    read_comm: impl FnOnce(u32) -> Option<String>,
) -> (bool, Option<u32>) {
    let Some(pid) = pid_record
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.chars().any(char::is_whitespace))
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|pid| *pid > 0)
    else {
        return (false, None);
    };
    match read_comm(pid).map(|comm| comm.trim().to_string()) {
        Some(comm) if comm == "ramsharedd" => (true, Some(pid)),
        _ => (false, None),
    }
}

pub(super) fn daemon_alive_pid() -> (bool, Option<u32>) {
    let record = fs::read_to_string(PID_FILE).ok();
    daemon_alive_pid_from(record.as_deref(), |pid| {
        fs::read_to_string(format!("/proc/{pid}/comm")).ok()
    })
}

/// The identity of the daemon that is alive right now, or `None` when there is
/// none.
///
/// Not the same as an unknown identity: `None` here means no daemon is running,
/// so no document on disk can be the live one's and every reader must treat
/// what it finds as a leftover from an earlier instance. This is the single
/// production reader of `/proc/<pid>/stat` for this purpose; callers gate their
/// documents with [`cache_status_is_current_daemon`] rather than growing their
/// own identity clause.
pub(crate) fn current_daemon_instance_id() -> Option<String> {
    let (_alive, pid) = daemon_alive_pid();
    pid.and_then(daemon_instance_id_from_pid)
}

pub(super) fn status_timestamp() -> String {
    // Prefer local ISO via `date -Is` when available; else unix epoch.
    sh("date", &["-Is"]).unwrap_or_else(|_| {
        use std::time::{SystemTime, UNIX_EPOCH};
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        format!("{secs}")
    })
}

/// `ramshared status` / `status --json`. Read-only. SPEC cascade-lifecycle-observability.
pub fn status(as_json: bool) -> Result<(), CascadeError> {
    if std::env::var("RAMSHARED_STATUS_LEGACY").ok().as_deref() == Some("1") {
        println!("{}", sh("swapon", &["--show"])?);
        return Ok(());
    }

    if as_json {
        // The documented surface is the spliced document: `status --json`
        // must carry the ITEM-7 health fragments, not only the monitor's
        // copy. A refused document is reported in `health_errors`, never a
        // reason to fail the command.
        println!("{}", status_json_document()?);
        return Ok(());
    }

    let entries = read_swaps()?;
    let snap = build_cascade_snapshot(&entries);
    let view = derive_lifecycle(&snap);

    // Human text (SPEC ITEM-2)
    let protection = protection_state(&view, &snap);
    let protection_reason = protection_reason(&view, &snap);
    println!("phase: {} ({})", view.phase.as_str(), view.phase_reason);
    println!("protection: {} ({protection_reason})", protection.as_str());
    // Cache residency is a separate claim from tier protection. `protection:
    // ACTIVE` means the VRAM tier is attached and serving swap pages; it does
    // not mean the GPU cache is populated. Both lines are printed so the two
    // cannot be read as one.
    println!("{}", cache_line(&snap));
    // The roll-up inputs are printed before the verdict they produce. `ok` is
    // the same verdict the JSON reports and the monitor reads; if the human
    // line stopped at the tier order it would call a guarded control plane or
    // a released cache healthy while the product itself reports BLOCKED.
    println!("{}", overall_line(&view, &snap));
    println!("ok: {}", status_ok(&view, &snap));
    println!("topology_ok: {}", view.ok);
    if !snap.measurement_errors.is_empty() {
        println!("measurement_errors: {}", snap.measurement_errors.join(", "));
    }
    if !view.reasons.is_empty() {
        println!("reasons: {}", view.reasons.join(", "));
    }
    print_tier("zram", &snap.zram);
    if snap.topology.writeback_target_is_tier {
        let bd_count_kib = read_zram_bd_count_kib(Path::new("/sys/block"), &entries);
        println!("{}", writeback_backing_line(&snap, bd_count_kib));
    } else {
        print_tier(
            match snap.origin_cache_mode {
                Some(true) => "origin swap",
                Some(false) => "vram",
                None => "managed swap",
            },
            &snap.vram,
        );
    }
    print_tier("disk", &snap.disk);
    let (pump_path, _, _, _) = health_documents::runtime_paths();
    if let Ok(Some(pump)) = health_documents::read_pump_health(&pump_path) {
        println!("{}", pump_health_line(&pump));
    }
    println!(
        "daemon: {} pid={}",
        if snap.daemon_alive { "alive" } else { "dead" },
        snap.daemon_pid
            .map(|p| p.to_string())
            .unwrap_or_else(|| "null".into())
    );
    println!(
        "demote: total={} last_reason={} in_progress={}",
        snap.demote
            .total
            .map(|n| n.to_string())
            .unwrap_or_else(|| "?".into()),
        snap.demote.last_reason.as_deref().unwrap_or("?"),
        snap.demote
            .in_progress
            .map(|v| if v { "true" } else { "false" }.to_string())
            .unwrap_or_else(|| "?".into()),
    );
    println!(
        "ghost: {} order_ok: {} active_kib: {}",
        snap.ghost, snap.order_ok, snap.active_kib
    );
    // Keep legacy table for operators
    if let Ok(table) = sh("swapon", &["--show"])
        && !table.is_empty()
    {
        println!();
        println!("{table}");
    }

    let ghosts = ghost_vram_swaps(&entries);
    if !ghosts.is_empty() {
        eprintln!("[status] WARNING: ghost swap detected:");
        for g in ghosts {
            eprintln!(
                "  {} size_kb={} used_kb={} prio={}",
                g.filename, g.size_kb, g.used_kb, g.priority
            );
        }
        eprintln!(
            "  action: capture evidence, run `wsl --terminate {}`, then ramshared down/up",
            sealed_distro_or_unknown()
        );
    }
    Ok(())
}

pub(crate) use super::preparation::preparation_is_off;
#[cfg(test)]
pub(super) use super::preparation::{
    preparation_gate, preparation_records_absent, preparation_snapshot_is_off,
    preparation_units_quiescent,
};

pub(crate) fn status_json_document() -> Result<String, CascadeError> {
    let entries = read_swaps()?;
    let snap = build_cascade_snapshot(&entries);
    let view = derive_lifecycle(&snap);
    let document = render_status_json(&view, &snap, &status_timestamp());

    // ITEM-7: the health documents are additive fragments, never a reason to
    // fail `status`. A refused document is reported, not coerced into zero.
    let (pump_path, hardware_path, selector, approval_dir) = health_documents::runtime_paths();
    let mut health_errors = Vec::new();
    let pump = match health_documents::read_pump_health(&pump_path) {
        Ok(pump) => pump,
        Err(error) => {
            health_errors.push(format!("pump-health: {error}"));
            None
        }
    };
    let hardware = match health_documents::read_hardware_health(&hardware_path) {
        Ok(hardware) => hardware,
        Err(error) => {
            health_errors.push(format!("hardware-health: {error}"));
            None
        }
    };
    let mut posture = health_documents::status_posture(&selector, &approval_dir);
    if posture.posture.is_none()
        && posture.reason == "token_unreadable"
        && let Some(cached) = root_status_cache::verified_posture()
    {
        posture = cached;
    }
    let fragments = health_documents::render_json_fragments(
        pump.as_ref(),
        hardware.as_ref(),
        &posture,
        &health_errors,
    )
    .map_err(|error| CascadeError::Io(error.to_string()))?;
    // `render_status_json` owns a flat object; the fragments are spliced
    // before its closing brace so the schema version is not re-written.
    let body = document
        .strip_suffix('}')
        .ok_or_else(|| CascadeError::Io("status document is not an object".into()))?;
    Ok(format!("{body},{fragments}}}"))
}

pub(super) fn print_tier(name: &str, t: &TierSample) {
    let prio = t
        .prio
        .map(|p| p.to_string())
        .unwrap_or_else(|| "null".into());
    println!(
        "{name}: present={} prio={prio} size_kib={} used_kib={}",
        t.present, t.size_kib, t.used_kib
    );
}

/// Read the currently spilled page count (`bd_count * 4 KiB`) from the first
/// live zram device's `bd_stat` sysfs attribute. Absent or malformed returns
/// `None`, never a fabricated zero.
pub(super) fn read_zram_bd_count_kib(sys_block: &Path, entries: &[SwapEntry]) -> Option<u64> {
    entries
        .iter()
        .filter(|entry| !entry.is_ghost() && swap_tier_role(&entry.filename) == SwapTierRole::Zram)
        .filter_map(|entry| {
            Path::new(&entry.filename)
                .file_name()
                .and_then(|name| name.to_str())
        })
        .find_map(|name| {
            let text = fs::read_to_string(sys_block.join(name).join("bd_stat")).ok()?;
            let blocks: u64 = text.split_whitespace().next()?.parse().ok()?;
            Some(blocks.saturating_mul(4))
        })
}

/// Truthful human-readable line for Tier 2 when it is bound as zram's
/// writeback backing device rather than a standalone swap device.
pub(super) fn writeback_backing_line(snap: &CascadeSnapshot, bd_count_kib: Option<u64>) -> String {
    let label = match snap.origin_cache_mode {
        Some(true) => "origin backing",
        Some(false) => "vram backing",
        None => "tier backing",
    };
    let dev = snap
        .topology
        .zram_backing_dev
        .as_deref()
        .unwrap_or("unknown");
    let size = snap
        .logical_capacity_kib
        .map_or_else(|| "null".to_string(), |v| v.to_string());
    let spilled = bd_count_kib
        .or(snap.ssd_origin_written_kib)
        .map_or_else(|| "null".to_string(), |v| v.to_string());
    format!(
        "{label}: bound=true role=writeback_target dev={dev} size_kib={size} spilled_kib={spilled}"
    )
}

/// Human-readable one-line summary of `/run/ramshared/pump-health.json`.
pub(super) fn pump_health_line(pump: &health_documents::PumpHealth) -> String {
    let cycle = pump
        .last_cycle_mib
        .map_or_else(|| "null".to_string(), |v| v.to_string());
    let rate = pump
        .effective_mib_per_s
        .map_or_else(|| "null".to_string(), |v| v.to_string());
    let p99 = pump
        .p99_cycle_ms
        .map_or_else(|| "null".to_string(), |v| v.to_string());
    format!(
        "pump: state={} last_cycle_mib={cycle} effective_mib_per_s={rate} p99_cycle_ms={p99}",
        pump.state.as_str()
    )
}

/// One human-readable line for GPU cache residency.
///
/// Deliberately separate from `protection:`: a healthy cascade can have the
/// VRAM tier serving swap with the cache released (`OFF`), and a populated
/// cache is not what `protection: ACTIVE` claims. Rendering both is what stops
/// the two being read as the same fact.
pub(super) fn cache_line(snap: &CascadeSnapshot) -> String {
    let cached = snap
        .vram_cached_kib
        .map_or_else(|| "null".to_string(), |v| v.to_string());
    let headroom = snap
        .gpu_headroom_kib
        .map_or_else(|| "null".to_string(), |v| v.to_string());
    format!(
        "cache: {} (vram_cached_kib={cached} gpu_headroom_kib={headroom}; separate claim from protection)",
        snap.cache_state.as_str()
    )
}

/// One human-readable line for the severity roll-up behind `ok`.
///
/// Lists every input `overall_state` weighs. Leaving one out is how a reader
/// comes to treat the missing input as fine — the same conflation the `cache:`
/// line exists to prevent, one level up.
pub(super) fn overall_line(view: &LifecycleView, snap: &CascadeSnapshot) -> String {
    format!(
        "overall: {} (control={} origin={} guardian={} cache={} release={})",
        overall_state(view, snap).as_str(),
        snap.control_state.as_str(),
        snap.origin_state.as_str(),
        snap.guardian_state.as_str(),
        snap.cache_state.as_str(),
        snap.release_coherence.as_str(),
    )
}
