// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/lifecycle/tests/status_and_protection.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

#[test]
fn vram_without_daemon_low_used_is_degraded() {
    let mut s = base();
    s.daemon_alive = false;
    s.vram.used_kib = 10;
    let v = derive_lifecycle(&s);
    assert_eq!(v.phase, CascadePhase::Degraded);
    assert_eq!(v.phase_reason, "vram_tier_without_daemon");
}

#[test]
fn schema_v5_worst_plane_controls_ok() {
    let mut snapshot = base();
    snapshot.control_state = ControlState::Critical;
    let view = derive_lifecycle(&snapshot);
    let json = render_status_json(&view, &snapshot, "2026-08-20T00:00:00Z");
    assert!(json.contains("\"schema_version\":5"));
    assert!(json.contains("\"control_state\":\"CRITICAL\""));
    assert!(json.contains("\"overall_state\":\"CRITICAL\""));
    assert!(json.contains("\"ok\":false"));
}

#[test]
fn status_json_publishes_adapter_bound_gpu_budget() {
    let mut snapshot = base();
    snapshot.gpu_budget = Some(GpuBudgetTelemetry {
        schema_version: 1,
        adapter: Some(ramshared_vram::GpuAdapterIdentity {
            node_mask: None,
            backend: "vulkan".into(),
            key: "uuid:fixture".into(),
            luid: Some("aabbccdd:00001122".into()),
        }),
        total_bytes: Some(8_000),
        budget_bytes: 6_000,
        used_bytes: 2_000,
        available_bytes: 4_000,
        source: ramshared_vram::GpuBudgetSource::DriverReported,
        sampled_at_unix_ms: 1_000,
    });
    let json = render_status_json(
        &derive_lifecycle(&snapshot),
        &snapshot,
        "2026-08-20T00:00:00Z",
    );
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid status JSON");
    assert_eq!(parsed["gpu_budget"]["adapter"]["backend"], "vulkan");
    assert_eq!(parsed["gpu_budget"]["adapter"]["luid"], "aabbccdd:00001122");
    assert_eq!(parsed["gpu_budget"]["available_bytes"], 4_000);
}

/// Busy time is published beside the budget, never inside it and never as
/// a capacity. `None` is "not measured" and publishes `null`; a measured
/// idle `0` is a real sample and publishes as `0`.
#[test]
fn status_json_publishes_gpu_utilization_and_never_invents_idle() {
    let mut snapshot = base();
    snapshot.gpu_utilization = Some(GpuUtilizationTelemetry {
        schema_version: 1,
        gpu_pct: 0,
        memory_pct: 0,
        sampled_at_unix_ms: 1_000,
    });
    let json = render_status_json(
        &derive_lifecycle(&snapshot),
        &snapshot,
        "2026-08-20T00:00:00Z",
    );
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid status JSON");
    assert_eq!(parsed["gpu_utilization"]["gpu_pct"], 0);
    assert_eq!(parsed["gpu_utilization"]["memory_pct"], 0);
    assert!(
        parsed["gpu_budget"].get("gpu_utilization").is_none(),
        "utilization must not be nested inside the budget object"
    );
    assert!(
        parsed["capacity"].get("gpu_utilization").is_none(),
        "busy time must not be published as a capacity"
    );

    snapshot.gpu_utilization = None;
    let json = render_status_json(
        &derive_lifecycle(&snapshot),
        &snapshot,
        "2026-08-20T00:00:00Z",
    );
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid status JSON");
    assert_eq!(parsed["gpu_utilization"], serde_json::Value::Null);
}

/// Cache occupancy is published beside the budget and the busy time, and
/// `None` is "no cache telemetry" — never a measured empty cache. The
/// monitor's `gpu_cache` reader was previously always `None` outside tests
/// because this key was never emitted at all.
#[test]
fn status_json_publishes_gpu_cache_and_never_invents_an_empty_cache() {
    let mut snapshot = base();
    snapshot.gpu_cache = Some(WorkerCacheTelemetry {
        schema_version: 1,
        sampled_at_unix_ms: 1_000,
        codec: ramshared_vram::CodecTelemetry::new(
            ramshared_vram::CodecCapability::Available,
            ramshared_vram::CodecState::Ready,
            None,
        ),
        logical_cached_bytes: 256 * 1024 * 1024,
        physical_cache_slab_bytes: 8 * 1024 * 1024,
        codec_workspace_bytes: 1024 * 1024,
        compressed_payload_bytes: 32 * 1024 * 1024,
        raw_payload_bytes: 4 * 1024 * 1024,
        metadata_bytes: Some(64 * 1024),
        device_metadata_lower_bound_bytes: None,
        raw_bypass_bytes: 16 * 1024 * 1024,
        codec_integrity_errors: 0,
        codec_decode_errors: 1,
        codec_timeouts: 0,
    });
    let json = render_status_json(
        &derive_lifecycle(&snapshot),
        &snapshot,
        "2026-08-20T00:00:00Z",
    );
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid status JSON");
    assert_eq!(
        parsed["gpu_cache"]["logical_cached_bytes"],
        256 * 1024 * 1024
    );
    assert_eq!(parsed["gpu_cache"]["codec_decode_errors"], 1);
    assert!(
        parsed["gpu_budget"].get("gpu_cache").is_none(),
        "cache occupancy must not be nested inside the budget object"
    );

    snapshot.gpu_cache = None;
    let json = render_status_json(
        &derive_lifecycle(&snapshot),
        &snapshot,
        "2026-08-20T00:00:00Z",
    );
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid status JSON");
    assert_eq!(parsed["gpu_cache"], serde_json::Value::Null);
}

/// The origin-plane I/O accounting the daemon measures is published beside
/// the GPU samples. `None` is "this producer did not measure it" and
/// publishes `null`, never a zero-filled accounting — and a present zero
/// count is a real zero, never collapsed away.
#[test]
fn status_json_publishes_origin_cache_io_and_never_invents_an_accounting() {
    let mut snapshot = base();
    snapshot.origin_cache_io = Some(OriginCacheIo {
        origin_syncs: 3,
        batched_writes: 5,
        cache_read_bytes: 8_192,
        cache_read_failures: 0,
        cache_write_failures: 1,
        promotion_refusals: Some(0),
        allocation_failures: Some(2),
        valid_blocks: Some(0),
    });
    let json = render_status_json(
        &derive_lifecycle(&snapshot),
        &snapshot,
        "2026-08-20T00:00:00Z",
    );
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid status JSON");
    assert_eq!(parsed["origin_cache_io"]["origin_syncs"], 3);
    assert_eq!(parsed["origin_cache_io"]["batched_writes"], 5);
    assert_eq!(parsed["origin_cache_io"]["cache_read_bytes"], 8_192);
    assert_eq!(parsed["origin_cache_io"]["cache_read_failures"], 0);
    assert_eq!(parsed["origin_cache_io"]["cache_write_failures"], 1);
    assert_eq!(parsed["origin_cache_io"]["promotion_refusals"], 0);
    assert_eq!(parsed["origin_cache_io"]["allocation_failures"], 2);
    assert_eq!(parsed["origin_cache_io"]["valid_blocks"], 0);
    assert!(
        parsed["gpu_budget"].get("origin_cache_io").is_none(),
        "origin I/O must not be nested inside the budget object"
    );
    assert!(
        parsed.get("cache_write_bytes").is_none(),
        "write-through traffic is the origin write figure; no second byte counter is invented"
    );

    snapshot.origin_cache_io = None;
    let json = render_status_json(
        &derive_lifecycle(&snapshot),
        &snapshot,
        "2026-08-20T00:00:00Z",
    );
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid status JSON");
    assert_eq!(parsed["origin_cache_io"], serde_json::Value::Null);
}

/// The cache-event counters ride under their own top-level names. They are
/// counts, not KiB sizes and not members of the origin-plane accounting, so
/// `capacity` and `origin_cache_io` must not swallow them — and `None` is
/// "this producer did not publish the key", never a zero count.
#[test]
fn status_json_publishes_the_cache_event_counters_under_their_own_names() {
    let mut snapshot = base();
    snapshot.origin_cache_io = Some(OriginCacheIo {
        origin_syncs: 1,
        batched_writes: 1,
        cache_read_bytes: 0,
        cache_read_failures: 0,
        cache_write_failures: 0,
        promotion_refusals: Some(0),
        allocation_failures: Some(0),
        valid_blocks: Some(0),
    });
    snapshot.cache_fallback_reads = Some(7);
    snapshot.cache_invalidations = Some(0);
    snapshot.cache_releases = Some(4);
    let json = render_status_json(
        &derive_lifecycle(&snapshot),
        &snapshot,
        "2026-08-20T00:00:00Z",
    );
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid status JSON");
    assert_eq!(parsed["cache_fallback_reads"], 7);
    assert_eq!(
        parsed["cache_invalidations"], 0,
        "a measured zero is a real zero, not an absence"
    );
    assert_eq!(parsed["cache_releases"], 4);
    for key in [
        "cache_fallback_reads",
        "cache_invalidations",
        "cache_releases",
    ] {
        assert!(
            parsed["origin_cache_io"].get(key).is_none(),
            "{key} is a document-level count, never a member of the origin-plane accounting"
        );
        assert!(
            parsed["capacity"].get(key).is_none(),
            "{key} is a count, not a KiB size; capacity must not swallow it"
        );
    }

    // Absence publishes `null`, never `0`. A consumer that reads a zero
    // here would be told the cache never fell through, invalidated or
    // released anything, when the truth is the producer never said.
    snapshot.cache_fallback_reads = None;
    snapshot.cache_invalidations = None;
    snapshot.cache_releases = None;
    let json = render_status_json(
        &derive_lifecycle(&snapshot),
        &snapshot,
        "2026-08-20T00:00:00Z",
    );
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid status JSON");
    for key in [
        "cache_fallback_reads",
        "cache_invalidations",
        "cache_releases",
    ] {
        assert_eq!(parsed[key], serde_json::Value::Null, "{key} is never 0");
    }
}

#[test]
fn status_json_publishes_zram_compression_and_never_invents_a_zero() {
    let mut snapshot = base();
    snapshot.zram_compression = Some(ZramCompression {
        orig_data_bytes: 3_000,
        compr_data_bytes: 1_000,
        mem_used_bytes: 1_200,
        huge_pages: Some(2),
        same_pages: Some(11),
        compressor: Some("zstd".to_string()),
    });
    let json = render_status_json(
        &derive_lifecycle(&snapshot),
        &snapshot,
        "2026-10-06T00:00:00Z",
    );
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid status JSON");
    assert_eq!(parsed["zram_compression"]["orig_data_bytes"], 3_000);
    assert_eq!(parsed["zram_compression"]["compr_data_bytes"], 1_000);
    assert_eq!(parsed["zram_compression"]["mem_used_bytes"], 1_200);
    assert_eq!(parsed["zram_compression"]["huge_pages"], 2);
    assert_eq!(parsed["zram_compression"]["same_pages"], 11);
    assert_eq!(parsed["zram_compression"]["ratio"], 3.0);
    assert_eq!(parsed["zram_compression"]["compressor"], "zstd");

    // A column the kernel omitted is `null`, not `0`. A reader that
    // treats `null` and `0` as the same thing would claim a measured
    // zero-incompressibles or zero-identity count the device never reported.
    snapshot.zram_compression = Some(ZramCompression {
        orig_data_bytes: 3_000,
        compr_data_bytes: 1_000,
        mem_used_bytes: 1_200,
        huge_pages: None,
        same_pages: None,
        compressor: None,
    });
    let json = render_status_json(
        &derive_lifecycle(&snapshot),
        &snapshot,
        "2026-10-06T00:00:00Z",
    );
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid status JSON");
    assert_eq!(
        parsed["zram_compression"]["huge_pages"],
        serde_json::Value::Null
    );
    assert_eq!(
        parsed["zram_compression"]["same_pages"],
        serde_json::Value::Null
    );
    assert_eq!(parsed["zram_compression"]["ratio"], 3.0);

    // No device, or the counters are gone: the whole sample is `null`.
    // A zero-filled object here would read as "compression is perfect".
    snapshot.zram_compression = None;
    let json = render_status_json(
        &derive_lifecycle(&snapshot),
        &snapshot,
        "2026-10-06T00:00:00Z",
    );
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid status JSON");
    assert_eq!(parsed["zram_compression"], serde_json::Value::Null);
}

#[test]
fn using_vram_never_masks_critical_pressure() {
    let mut snapshot = base();
    snapshot.vram.used_kib = 32_768;
    snapshot.control_state = ControlState::Emergency;
    let view = derive_lifecycle(&snapshot);
    assert_eq!(view.phase, CascadePhase::UsingVram);
    assert_eq!(overall_state(&view, &snapshot), OverallState::Emergency);
}

#[test]
fn origin_failure_and_stuck_cache_are_never_green() {
    let mut snapshot = base();
    snapshot.origin_state = OriginState::Failed;
    snapshot.cache_state = CacheState::Stuck;
    let view = derive_lifecycle(&snapshot);
    assert_eq!(overall_state(&view, &snapshot), OverallState::Blocked);
    assert!(!overall_state(&view, &snapshot).is_ok());
}

#[test]
fn schema_v5_distinguishes_logical_cache_origin_and_fallback_swap() {
    let mut snapshot = base();
    snapshot.logical_capacity_kib = Some(4 * 1024 * 1024);
    snapshot.vram_cached_kib = Some(128 * 1024);
    snapshot.ssd_origin_written_kib = Some(256 * 1024);
    snapshot.fallback_swap_used_kib = Some(64 * 1024);
    let view = derive_lifecycle(&snapshot);
    let json = render_status_json(&view, &snapshot, "2026-08-20T00:00:00Z");
    for field in [
        "\"logical_capacity_kib\":4194304",
        "\"vram_cached_kib\":131072",
        "\"ssd_origin_written_kib\":262144",
        "\"fallback_swap_used_kib\":65536",
    ] {
        assert!(json.contains(field), "missing {field}: {json}");
    }
}

#[test]
fn fallback_swap_occupancy_is_witness_backed_and_never_a_measured_zero_from_an_unlisted_tier() {
    // A listed fallback holding nothing is a measurement and keeps `Some(0)`.
    let empty = TierSample {
        present: true,
        prio: Some(-2),
        size_kib: 8_388_608,
        used_kib: 0,
    };
    assert_eq!(fallback_swap_occupancy_kib(&empty), Some(0));

    // A listed fallback with pages is that measurement.
    let occupied = TierSample {
        used_kib: 5_000,
        ..empty.clone()
    };
    assert_eq!(fallback_swap_occupancy_kib(&occupied), Some(5_000));

    // An unlisted tier has no occupancy to report. `Some(0)` would claim
    // a measured empty fallback — a different fact from "the table does
    // not list one".
    let absent = TierSample::default();
    assert_eq!(fallback_swap_occupancy_kib(&absent), None);
    assert_ne!(
        fallback_swap_occupancy_kib(&absent),
        Some(0),
        "an unlisted tier must not wear the measured-empty face"
    );
}

#[test]
fn tier_json_never_publishes_measured_zeros_from_an_absent_tier() {
    let absent = TierSample::default();
    let face = tier_json(&absent);
    assert_eq!(
        face, "{\"present\":false,\"prio\":null,\"size_kib\":null,\"used_kib\":null}",
        "an absent tier has no figures to report: {face}"
    );
    assert!(
        !face.contains("\":0"),
        "an absent tier must not publish a measured zero: {face}"
    );
}

#[test]
fn tier_json_is_witness_backed_on_a_present_tier() {
    // A present tier holding nothing is a measurement and keeps its zeros.
    let empty = TierSample {
        present: true,
        prio: Some(200),
        size_kib: 1024,
        used_kib: 0,
    };
    assert_eq!(
        tier_json(&empty),
        "{\"present\":true,\"prio\":200,\"size_kib\":1024,\"used_kib\":0}"
    );

    // An occupied present tier keeps its figures.
    let occupied = TierSample {
        used_kib: 42,
        ..empty
    };
    assert_eq!(
        tier_json(&occupied),
        "{\"present\":true,\"prio\":200,\"size_kib\":1024,\"used_kib\":42}"
    );
}

#[test]
fn status_json_fallback_swap_is_witness_backed_by_a_listed_tier() {
    // The figure is the last-resort SSD fallback swap occupancy, not
    // origin write-through and not the logical device's occupancy. It is
    // present only when the tier is.
    let mut backed = base();
    backed.fallback_swap_used_kib = fallback_swap_occupancy_kib(&TierSample {
        present: true,
        prio: Some(-2),
        size_kib: 8_388_608,
        used_kib: 64 * 1024,
    });
    let view = derive_lifecycle(&backed);
    let json = render_status_json(&view, &backed, "2026-08-20T00:00:00Z");
    assert!(
        json.contains("\"fallback_swap_used_kib\":65536"),
        "a listed fallback must publish its occupancy: {json}"
    );

    let mut unbacked = base();
    unbacked.fallback_swap_used_kib = fallback_swap_occupancy_kib(&TierSample::default());
    let view = derive_lifecycle(&unbacked);
    let json = render_status_json(&view, &unbacked, "2026-08-20T00:00:00Z");
    assert!(
        json.contains("\"fallback_swap_used_kib\":null"),
        "an unlisted fallback must publish null, never a measured zero: {json}"
    );
    assert!(
        !json.contains("\"fallback_swap_used_kib\":0"),
        "an unlisted fallback must not wear the measured-empty face: {json}"
    );
}

// ── RF-10 display honesty (DT-6) ────────────────────────────────────

#[test]
fn status_text_reports_off_and_blocked_without_live_daemon() {
    // the dashboard describes reality. Without a live daemon
    // the text surface must report `off` / `blocked` — never `active`.
    //
    // Case 1: clean off (no tiers, no daemon) → phase Off, protection Off.
    let clean = CascadeSnapshot {
        origin_cache_mode: Some(false),
        zram: TierSample::default(),
        vram: TierSample::default(),
        disk: TierSample::default(),
        ghost: false,
        order_ok: true,
        daemon_alive: false,
        daemon_pid: None,
        capacity_guaranteed: Some(false),
        disk_baseline_kib: None,
        demote: DemoteSnapshot::default(),
        active_kib: DEFAULT_ACTIVE_KIB,
        control_state: ControlState::Healthy,
        origin_state: OriginState::Off,
        cache_state: CacheState::Off,
        guardian_state: GuardianState::Healthy,
        cache_target_kib: None,
        logical_capacity_kib: None,
        vram_cached_kib: None,
        gpu_headroom_kib: None,
        gpu_budget: None,
        gpu_utilization: None,
        gpu_cache: None,
        origin_cache_io: None,
        cache_fallback_reads: None,
        cache_hits: None,
        host_memory: None,
        host_storage: None,
        cache_invalidations: None,
        cache_releases: None,
        zram_compression: None,
        zram_tier_state: ZramTierState::Inactive,
        ssd_origin_written_kib: None,
        fallback_swap_used_kib: None,
        release_coherence: ReleaseCoherence::Coherent,
        measurement_errors: Vec::new(),
        topology: TopologyFacts::default(),
        cache_unavailable_reason: None,
        control_plane_state: None,
        control_plane_lease_deadline_ms: None,
    };

    let view = derive_lifecycle(&clean);
    assert_eq!(view.phase, CascadePhase::Off);
    let prot = protection_state(&view, &clean);
    assert_eq!(prot, ProtectionState::Off);
    assert_eq!(prot.as_str(), "OFF");

    // Case 2: half cascade (vram swap still mounted, daemon dead) →
    // protection Blocked. This is the state that must never read Active.
    let mut half = clean.clone();
    half.vram.present = true;
    half.vram.prio = Some(100);
    half.vram.size_kib = 2_097_148;
    half.capacity_guaranteed = Some(false);
    half.cache_state = CacheState::Off;
    let view = derive_lifecycle(&half);
    let prot = protection_state(&view, &half);
    assert_eq!(prot, ProtectionState::Blocked);
    assert_eq!(prot.as_str(), "BLOCKED");
    let overall = overall_state(&view, &half);
    assert!(!overall.is_ok(), "half cascade without daemon is not ok");
}

// ── RF-10 / DT-6 release coherence ──────────────────────────────────

#[test]
fn stale_fleet_is_never_reported_healthy() {
    // DT-6: a running supervisor on a previous release is enforcing a
    // different contract than the installed one. `HEALTHY` next to that is
    // the defect this campaign exists to close — not a cosmetic issue.
    //
    // the same snapshot, one field changed. Everything else
    // is the clean healthy baseline, so the only reason the verdict moves
    // is the fleet.
    let healthy = base();
    assert_eq!(
        overall_state(&derive_lifecycle(&healthy), &healthy),
        OverallState::Healthy
    );

    let mut stale = base();
    stale.release_coherence = ReleaseCoherence::Stale;
    let view = derive_lifecycle(&stale);
    let overall = overall_state(&view, &stale);
    assert_ne!(
        overall,
        OverallState::Healthy,
        "stale fleet renders HEALTHY"
    );
    assert!(
        !status_ok(&view, &stale),
        "stale fleet leaves the single ok verdict true"
    );
    assert_eq!(overall, OverallState::Critical);
    let json = render_status_json(&view, &stale, "2026-10-06T00:00:00Z");
    assert!(
        json.contains("\"release_coherence\":\"stale\""),
        "status document does not name the fleet: {json}"
    );
    assert!(
        json.contains("\"overall_state\":\"CRITICAL\""),
        "status document reports HEALTHY beside a stale fleet: {json}"
    );

    // An unprovable fleet is not `HEALTHY` either, but it is not the same
    // news as proven wrong bytes.
    let mut unknown = base();
    unknown.release_coherence = ReleaseCoherence::Unknown;
    let view = derive_lifecycle(&unknown);
    assert_eq!(overall_state(&view, &unknown), OverallState::Guarded);
    assert!(!status_ok(&view, &unknown));

    // Coherent is the only value that may coexist with `HEALTHY`.
    let mut coherent = base();
    coherent.release_coherence = ReleaseCoherence::Coherent;
    assert_eq!(
        overall_state(&derive_lifecycle(&coherent), &coherent),
        OverallState::Healthy
    );
}

#[test]
fn status_json_never_publishes_active_without_live_daemon() {
    // invariant checkDT-6: the `--json` surface must never claim
    // `"activation":{"active":true}` or `"cache_state":"ACTIVE"` when the
    // daemon is not alive. A stale green is worse than a red.
    let snap = CascadeSnapshot {
        origin_cache_mode: Some(false),
        zram: TierSample::default(),
        vram: TierSample::default(),
        disk: TierSample::default(),
        ghost: false,
        order_ok: true,
        daemon_alive: false,
        daemon_pid: None,
        capacity_guaranteed: Some(true),
        disk_baseline_kib: Some(0),
        demote: DemoteSnapshot::default(),
        active_kib: DEFAULT_ACTIVE_KIB,
        control_state: ControlState::Healthy,
        origin_state: OriginState::Ready,
        cache_state: CacheState::Active,
        guardian_state: GuardianState::Healthy,
        cache_target_kib: Some(33_792),
        logical_capacity_kib: Some(2_097_148),
        vram_cached_kib: Some(0),
        gpu_headroom_kib: Some(2_097_152),
        gpu_budget: None,
        gpu_utilization: None,
        gpu_cache: None,
        origin_cache_io: None,
        cache_fallback_reads: None,
        cache_hits: None,
        host_memory: None,
        host_storage: None,
        cache_invalidations: None,
        cache_releases: None,
        zram_compression: None,
        zram_tier_state: ZramTierState::Inactive,
        ssd_origin_written_kib: Some(0),
        fallback_swap_used_kib: Some(0),
        release_coherence: ReleaseCoherence::Coherent,
        measurement_errors: Vec::new(),
        topology: TopologyFacts::default(),
        cache_unavailable_reason: None,
        control_plane_state: None,
        control_plane_lease_deadline_ms: None,
    };

    let view = derive_lifecycle(&snap);
    let json = render_status_json(&view, &snap, "2026-09-30T00:00:00Z");
    assert!(
        json.contains("\"activation\":{\"active\":false"),
        "activation must be false without a live daemon: {json}"
    );
    assert!(
        !json.contains("\"activation\":{\"active\":true"),
        "activation must never be true without a live daemon: {json}"
    );
}
