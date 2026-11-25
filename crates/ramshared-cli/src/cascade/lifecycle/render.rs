// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/lifecycle/render.rs
//! JSON rendering for cascade lifecycle status snapshots.

use crate::monitor::ZramTierState;
use crate::zram_compression::ZramCompression;

use super::{
    CascadeSnapshot, LifecycleView, TierSample, disk_growth_kib, overall_state, protection_reason,
    protection_state, status_ok, zram_utilization_pct,
};

/// Escape a string for JSON.
pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// One tier's status face.
///
/// An absent tier publishes `null` for `size_kib` and `used_kib`, never `0`:
/// `0` is the measured value of a present tier holding nothing, and the two
/// are different facts. `present` is the witness — the figures only exist
/// when it is true.
pub(super) fn tier_json(t: &TierSample) -> String {
    let prio = match t.prio {
        Some(p) => p.to_string(),
        None => "null".into(),
    };
    let (size_kib, used_kib) = if t.present {
        (t.size_kib.to_string(), t.used_kib.to_string())
    } else {
        ("null".into(), "null".into())
    };
    format!(
        "{{\"present\":{},\"prio\":{},\"size_kib\":{},\"used_kib\":{}}}",
        if t.present { "true" } else { "false" },
        prio,
        size_kib,
        used_kib
    )
}

/// The Tier 1 object: `tier_json` plus the degraded face (SPEC DT-3).
///
/// The state is the derivation's own answer, published under its own key so
/// the dashboard row and any reader of the document see the same fact. The
/// other tiers carry no such key: their state is presence, and DT-3 is about
/// the tier whose authoritative copy can be unreachable while the tier still
/// serves.
pub(super) fn zram_tier_json(t: &TierSample, state: ZramTierState) -> String {
    let base = tier_json(t);
    let with_state = format!(",\"state\":\"{}\"", state.as_str());
    base.replacen('}', &format!("{with_state}}}"), 1)
}

/// Serialize lifecycle + snapshot to one JSON object (SPEC schema).
pub fn render_status_json(view: &LifecycleView, snap: &CascadeSnapshot, ts: &str) -> String {
    let protection = protection_state(view, snap);
    let protection_reason = protection_reason(view, snap);
    let overall = overall_state(view, snap);
    let ok = status_ok(view, snap);
    // A measured marker without a live daemon backs no guarantee figure, and
    // neither does a stat that never arrived. `null` on both `Some(false)`
    // and `None` is honest: the figure has no value in either case. The
    // distinguishing word is `protection_reason`, not the figure.
    let guaranteed_kib =
        if snap.daemon_alive && snap.vram.present && snap.capacity_guaranteed == Some(true) {
            snap.vram.size_kib.to_string()
        } else {
            "null".into()
        };
    // A missing tier publishes `null`, never `0`: `0` is the measured value
    // of a present tier holding nothing, and the two are different facts.
    let zram_utilization =
        zram_utilization_pct(snap).map_or_else(|| "null".to_string(), |pct| pct.to_string());
    let reasons = if view.reasons.is_empty() {
        "[]".into()
    } else {
        let parts: Vec<String> = view.reasons.iter().map(|r| json_escape(r)).collect();
        format!("[{}]", parts.join(","))
    };
    let demote_total = match snap.demote.total {
        Some(n) => n.to_string(),
        None => "null".into(),
    };
    let demote_reason = match &snap.demote.last_reason {
        Some(r) => json_escape(r),
        None => "null".into(),
    };
    let pid = match snap.daemon_pid {
        Some(p) => p.to_string(),
        None => "null".into(),
    };
    let activation_active =
        snap.daemon_alive && snap.vram.present && snap.capacity_guaranteed == Some(true);
    let disk_baseline_kib = snap
        .disk_baseline_kib
        .map_or_else(|| "null".to_string(), |value| value.to_string());
    let disk_growth_kib =
        disk_growth_kib(snap).map_or_else(|| "null".to_string(), |value| value.to_string());
    let number_or_null =
        |value: Option<u64>| value.map_or_else(|| "null".to_string(), |number| number.to_string());
    let measurement_errors = format!(
        "[{}]",
        snap.measurement_errors
            .iter()
            .map(|error| json_escape(error))
            .collect::<Vec<_>>()
            .join(",")
    );
    let gpu_budget_json = snap
        .gpu_budget
        .as_ref()
        .and_then(|budget| serde_json::to_string(budget).ok())
        .unwrap_or_else(|| "null".to_string());
    // Busy time is a sibling of the budget, never nested inside it. `None`
    // publishes `null`, which the dashboard reads as "not measured" — never as
    // a zero-percent idle sample.
    let gpu_utilization_json = snap
        .gpu_utilization
        .as_ref()
        .and_then(|sample| serde_json::to_string(sample).ok())
        .unwrap_or_else(|| "null".to_string());
    // Cache occupancy and codec health ride beside the budget and the busy
    // time. `None` publishes `null`, which reads as "no cache telemetry" —
    // never as a measured empty cache.
    let gpu_cache_json = snap
        .gpu_cache
        .as_ref()
        .and_then(|sample| serde_json::to_string(sample).ok())
        .unwrap_or_else(|| "null".to_string());
    // Origin-plane I/O accounting rides beside them. `None` publishes `null`,
    // which reads as "this producer did not measure it" — never as a
    // zero-filled accounting.
    let origin_cache_io_json = snap
        .origin_cache_io
        .as_ref()
        .and_then(|sample| serde_json::to_string(sample).ok())
        .unwrap_or_else(|| "null".to_string());
    let host_memory_json = snap
        .host_memory
        .as_ref()
        .and_then(|sample| serde_json::to_string(sample).ok())
        .unwrap_or_else(|| "null".to_string());
    let host_storage_json = snap
        .host_storage
        .as_ref()
        .and_then(|sample| serde_json::to_string(sample).ok())
        .unwrap_or_else(|| "null".to_string());
    // The compression sample owns its published shape (including the derived
    // ratio). `None` publishes `null`, which the dashboard reads as "not
    // measured" — never as a zero ratio or a zero byte count.
    let zram_compression_json = snap
        .zram_compression
        .as_ref()
        .map_or_else(|| "null".to_string(), ZramCompression::to_status_json);
    format!(
        "{{\"schema_version\":5,\"phase\":{phase},\"phase_reason\":{reason},\
\"protection_state\":{protection},\"protection_reason\":{protection_reason},\
\"control_state\":{control_state},\"cache_state\":{cache_state},\
\"cache_unavailable_reason\":{cache_unavailable_reason},\
\"control_plane_state\":{control_plane_state},\
\"control_plane_lease_deadline_ms\":{control_plane_lease_deadline_ms},\
\"origin_state\":{origin_state},\"guardian_state\":{guardian_state},\
\"overall_state\":{overall_state},\
\"release_coherence\":{release_coherence},\
\"ok\":{ok},\"topology_ok\":{topology_ok},\"reasons\":{reasons},\
\"tiers\":{{\"zram\":{z},\"vram\":{v},\"disk\":{d}}},\
\"tier_roles\":{{\"zram\":\"swap_device\",\"tier\":{tier_role},\"writeback_target_is_tier\":{writeback_target_is_tier},\"origin_cache_mode\":{origin_cache_mode}}},\
\"capacity\":{{\"guaranteed_kib\":{guaranteed_kib},\"cache_target_kib\":{cache_target_kib},\"logical_capacity_kib\":{logical_capacity_kib},\"vram_cached_kib\":{vram_cached_kib},\"gpu_headroom_kib\":{gpu_headroom_kib},\"ssd_origin_written_kib\":{ssd_origin_written_kib},\"fallback_swap_used_kib\":{fallback_swap_used_kib}}},\
\"activation\":{{\"active\":{activation_active},\"binary_version\":{binary_version},\"disk_baseline_kib\":{disk_baseline_kib},\"disk_growth_kib\":{disk_growth_kib}}},\
\"pressure\":{{\"zram_utilization_pct\":{zram_utilization}}},\
\"measurement_errors\":{measurement_errors},\
\"order_ok\":{order},\"ghost\":{ghost},\
\"daemon\":{{\"alive\":{alive},\"pid\":{pid}}},\
\"demote\":{{\"total\":{dt},\"last_reason\":{dr},\"in_progress\":{di}}},\
\"thresholds_kib\":{{\"active\":{thr}}},\
\"gpu_budget\":{gpu_budget},\
\"gpu_utilization\":{gpu_utilization},\
\"gpu_cache\":{gpu_cache},\
\"origin_cache_io\":{origin_cache_io},\
\"host_memory\":{host_memory},\
\"host_storage\":{host_storage},\
\"cache_hits\":{cache_hits},\"cache_fallback_reads\":{cache_fallback_reads},\"cache_invalidations\":{cache_invalidations},\"cache_releases\":{cache_releases},\
\"zram_compression\":{zram_compression},\
\"ts\":{ts}}}",
        phase = json_escape(view.phase.as_str()),
        reason = json_escape(view.phase_reason),
        protection = json_escape(protection.as_str()),
        protection_reason = json_escape(protection_reason),
        control_state = json_escape(snap.control_state.as_str()),
        cache_state = json_escape(snap.cache_state.as_str()),
        cache_unavailable_reason = snap
            .cache_unavailable_reason
            .as_deref()
            .map_or_else(|| "null".to_string(), json_escape),
        control_plane_state = snap
            .control_plane_state
            .as_deref()
            .map_or_else(|| "null".to_string(), json_escape),
        control_plane_lease_deadline_ms = snap
            .control_plane_lease_deadline_ms
            .map_or_else(|| "null".to_string(), |value| value.to_string()),
        origin_state = json_escape(snap.origin_state.as_str()),
        guardian_state = json_escape(snap.guardian_state.as_str()),
        tier_role = json_escape(snap.topology.tier_role().as_str()),
        origin_cache_mode = match snap.origin_cache_mode {
            Some(true) => "true",
            Some(false) => "false",
            None => "null",
        },
        writeback_target_is_tier = if snap.topology.writeback_target_is_tier {
            "true"
        } else {
            "false"
        },
        overall_state = json_escape(overall.as_str()),
        release_coherence = json_escape(snap.release_coherence.as_str()),
        ok = if ok { "true" } else { "false" },
        topology_ok = if view.ok { "true" } else { "false" },
        reasons = reasons,
        z = zram_tier_json(&snap.zram, snap.zram_tier_state),
        v = tier_json(&snap.vram),
        d = tier_json(&snap.disk),
        guaranteed_kib = guaranteed_kib,
        cache_target_kib = number_or_null(snap.cache_target_kib),
        logical_capacity_kib = number_or_null(snap.logical_capacity_kib),
        vram_cached_kib = number_or_null(snap.vram_cached_kib),
        gpu_headroom_kib = number_or_null(snap.gpu_headroom_kib),
        gpu_budget = gpu_budget_json,
        gpu_utilization = gpu_utilization_json,
        gpu_cache = gpu_cache_json,
        origin_cache_io = origin_cache_io_json,
        host_memory = host_memory_json,
        host_storage = host_storage_json,
        cache_hits = number_or_null(snap.cache_hits),
        cache_fallback_reads = number_or_null(snap.cache_fallback_reads),
        cache_invalidations = number_or_null(snap.cache_invalidations),
        cache_releases = number_or_null(snap.cache_releases),
        zram_compression = zram_compression_json,
        ssd_origin_written_kib = number_or_null(snap.ssd_origin_written_kib),
        fallback_swap_used_kib = number_or_null(snap.fallback_swap_used_kib),
        activation_active = if activation_active { "true" } else { "false" },
        binary_version = json_escape(env!("CARGO_PKG_VERSION")),
        disk_baseline_kib = disk_baseline_kib,
        disk_growth_kib = disk_growth_kib,
        zram_utilization = zram_utilization,
        measurement_errors = measurement_errors,
        order = if snap.order_ok { "true" } else { "false" },
        ghost = if snap.ghost { "true" } else { "false" },
        alive = if snap.daemon_alive { "true" } else { "false" },
        pid = pid,
        dt = demote_total,
        dr = demote_reason,
        di = match snap.demote.in_progress {
            Some(v) => {
                if v { "true" } else { "false" }
            }
            None => "null",
        },
        thr = snap.active_kib,
        ts = json_escape(ts),
    )
}
