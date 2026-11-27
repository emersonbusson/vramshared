// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/tests.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

pub(super) use super::model::{
    CgroupMemoryObservation, GpuObservation, HyperVBalloonObservation, MemoryScope,
    ProcessMemoryTotals, TierDeviceNames,
};

pub(super) use super::observations::{
    BENCHMARK_STATUS_AWAITING, parse_unit_startup_ms, sanitize_cgroup, sanitize_label,
};

pub(super) use super::*;

pub(super) use crate::workload;

pub(super) use ratatui::Terminal;

pub(super) use ratatui::backend::TestBackend;

pub(super) fn observation(ok: bool, with_gpu: bool) -> Observation {
    let status = serde_json::from_value::<BTreeMap<String, Value>>(serde_json::json!({
        "schema_version": 4,
        "ok": ok,
        "phase": if ok { "UsingVram" } else { "Off" },
        "protection_state": if ok { "ACTIVE" } else { "OFF" },
        "activation": {
            "active": ok,
            "binary_version": "test",
            "disk_baseline_kib": if ok { Value::from(100) } else { Value::Null },
            "disk_growth_kib": if ok { Value::from(5) } else { Value::Null }
        },
        "capacity": { "guaranteed_kib": if ok { Value::from(1024) } else { Value::Null } },
        "daemon": { "alive": ok, "pid": if ok { Value::from(7) } else { Value::Null } },
        "tiers": {
            "zram": { "present": ok, "size_kib": 2048, "used_kib": 512 },
            "vram": { "present": ok, "size_kib": 4096, "used_kib": 1024 },
            "disk": { "present": true, "size_kib": 8192, "used_kib": 105 }
        }
    }))
    .expect("fixture status");
    Observation {
        status,
        epoch_ms: 1,
        sample_age_ms: 0,
        memory_scope: MemoryScope::LinuxHost,
        mem: MemoryObservation {
            total_kib: Some(16_384),
            available_kib: Some(8_192),
            swap_total_kib: Some(8_192),
            swap_free_kib: Some(4_096),
            ..MemoryObservation::default()
        },
        cgroup_memory: CgroupMemoryObservation::default(),
        hyperv_balloon: HyperVBalloonObservation::default(),
        process_totals: ProcessMemoryTotals::default(),
        control_plane: ControlPlaneObservation {
            memory_psi_some_avg10: 1.0,
            memory_psi_full_avg10: 0.1,
            ..ControlPlaneObservation::default()
        },
        gpu: with_gpu.then(|| {
            gpu_observation_from_status(
                &serde_json::from_value(serde_json::json!({
                    "gpu_budget": {
                        "schema_version": 1,
                        "adapter": { "backend": "vulkan", "key": "fixture-uuid", "luid": null },
                        "total_bytes": 6_442_450_944u64,
                        "budget_bytes": 6_442_450_944u64,
                        "used_bytes": 2_147_483_648u64,
                        "available_bytes": 4_294_967_296u64,
                        "source": "driver_reported",
                        "sampled_at_unix_ms": 1000
                    }
                }))
                .expect("fixture status"),
            )
            .expect("fixture GPU budget")
        }),
        tier_devices: TierDeviceNames::default(),
        operation_progress: None,
        top_processes: Vec::new(),
        errors: Vec::new(),
    }
}

pub(super) fn reservation_ledger_fixture(schema_version: u32) -> String {
    serde_json::json!({
        "schema_version": schema_version,
        "next_ordinal": 2,
        "reservations": [{
            "id": "reservation-1",
            "owner": {
                "boot_id": "fixture-boot",
                "pid": 42,
                "start_time": 7,
                "nonce": "fixture-nonce"
            },
            "class": "interactive",
            "memory_bytes": 4096,
            "unit": "ramshared-interactive-fixture.scope",
            "invocation_id": null,
            "issued_ordinal": 1
        }]
    })
    .to_string()
}

pub(super) fn monitor_ledger_path(name: &str, contents: Option<&str>) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "ramshared-monitor-ledger-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let path = root.join("reservations.json");
    if let Some(contents) = contents {
        fs::write(&path, contents).unwrap();
    }
    (root, path)
}

pub(super) fn benchmark_evidence_fixture() -> Value {
    serde_json::json!({
        "schema_version": "ramshared-evidence/v1",
        "run_id": "wsl2-qualified-monitor-fixture-001",
        "source": {
            "commit": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "dirty": false,
            "dirty_entry_count": 0,
            "harness_revision": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
        },
        "candidate": { "binary_sha256": "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd" },
        "workload": { "runs": 3 },
        "comparison": { "qualified": true },
        "lifecycle": {
            "binary_match": true,
            "legitimate": { "verdict": "PASS" },
            "refusals": [{ "name": "invalid_target_refused", "verdict": "PASS" }],
            "cleanup": { "complete": true },
            "residue": 0
        },
        "artifacts": [{
            "path": "docs/benchmarks/evidence/qualified-run.json",
            "bytes": 1,
            "sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        }],
        "decision": { "verdict": "PASS", "promotable": true },
        "metrics": {
            "reclaim_speed_gbs": {
                "unit": "GB/s", "samples": [11.0, 12.0, 13.0], "n": 3, "median": 12.0
            },
            "reclaim_duration_ms": {
                "unit": "ms", "samples": [20.0, 22.0, 24.0], "n": 3, "median": 22.0
            },
            "p50_cycle_latency_ms": {
                "unit": "ms", "samples": [0.2, 0.3, 0.4], "n": 3, "median": 0.3
            },
            "p99_cycle_latency_ms": {
                "unit": "ms", "samples": [1.0, 1.1, 1.2], "n": 3, "p99_nearest_rank": 1.2
            }
        }
    })
}

pub(super) fn monitor_benchmark_path(name: &str, contents: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "ramshared-monitor-benchmark-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let path = root.join("evidence.json");
    fs::write(&path, contents).unwrap();
    (root, path)
}

/// The negative face of the fill derivation: an absent or self-disagreeing
/// RAM pair has no percentage. `None` is that absence; `Some(0)` would
/// claim an empty machine. The same rule the sparkline already applies —
/// a frame with no reading takes no sample.
/// A cache sample with coherent payload counters, so the compaction
/// faces can be exercised on a real ratio.
pub(super) fn compaction_cache(raw_bytes: u64, compressed_bytes: u64) -> Value {
    serde_json::to_value(WorkerCacheTelemetry {
        schema_version: 1,
        sampled_at_unix_ms: 1_000,
        codec: ramshared_vram::CodecTelemetry::new(
            ramshared_vram::CodecCapability::Available,
            ramshared_vram::CodecState::Ready,
            None,
        ),
        logical_cached_bytes: raw_bytes,
        physical_cache_slab_bytes: raw_bytes.saturating_mul(2).max(1),
        codec_workspace_bytes: 1024 * 1024,
        compressed_payload_bytes: compressed_bytes,
        raw_payload_bytes: raw_bytes,
        metadata_bytes: Some(64 * 1024),
        device_metadata_lower_bound_bytes: None,
        raw_bypass_bytes: 0,
        codec_integrity_errors: 0,
        codec_decode_errors: 0,
        codec_timeouts: 0,
    })
    .expect("cache sample serializes")
}

#[test]
fn cache_compaction_row_draws_the_shrinkage_bar_from_the_two_counts() {
    let four_gib = 4 * 1024 * 1024 * 1024;
    let one_and_a_bit_gib = 1_503_238_553;
    let line = cache_compaction_row(Some(&compaction_cache(four_gib, one_and_a_bit_gib)));
    assert!(
        line.starts_with("VRAM cache compaction: "),
        "the row names its quantity: {line}"
    );
    assert!(line.contains('→'), "the row shows the pair: {line}");
    assert!(line.contains(":1)"), "the row derives the ratio: {line}");
}

#[test]
fn tier_row_names_the_authoritative_copy_or_its_absence() {
    // A live tier above a live tier is a cache of it.
    assert_eq!(
        tier_authority_label("zram", Some(true), Some(true), "the SSD tier"),
        "cache of the SSD tier"
    );
    // A live tier with no observed tier below is the only copy we can
    // see, and the row says so rather than inventing an authority.
    assert_eq!(
        tier_authority_label("zram", Some(true), None, "the SSD tier"),
        "zram only"
    );
    // Nothing observed, or the tier is not there: absence, never a
    // location the operator would trust.
    assert_eq!(
        tier_authority_label("zram", None, Some(true), "x"),
        "unmeasured"
    );
    assert_eq!(
        tier_authority_label("zram", Some(false), Some(true), "x"),
        "unmeasured"
    );
}

#[test]
fn tier_row_never_claims_active_without_a_named_device() {
    // A tier with no swap device names nothing. The row may say it is
    // present, but the authority label cannot claim a lower copy the
    // observation did not carry.
    assert_eq!(
        tier_authority_label("vram", Some(true), Some(false), "the zram tier"),
        "vram only"
    );
    // A tier with no swap device names nothing.
    assert_eq!(
        tier_presence_status(Some(true), Some(0), Some(1024), "holding", "empty"),
        "empty"
    );
}

pub(super) fn zram_state_fixture(name: &str, backing: Option<&str>) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "ramshared-zram-state-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let sys_block = root.join("sys/block");
    fs::create_dir_all(sys_block.join("zram0")).unwrap();
    if let Some(value) = backing {
        fs::write(sys_block.join("zram0/backing_dev"), format!("{value}\n")).unwrap();
    }
    (root, sys_block)
}

pub(super) fn zram_swap_entry() -> crate::cascade::SwapEntry {
    crate::cascade::SwapEntry {
        filename: "/dev/zram0".to_string(),
        size_kb: 65536,
        used_kb: 0,
        priority: 200,
    }
}

/// SPEC ITEM-1, discipline #16: a tier whose backing device was
/// detached keeps serving from RAM and reports the degradation instead
/// of ACTIVE. The state comes from the one derivation; the row does not
/// re-derive it.
#[test]
fn zram_backing_dev_detach_keeps_the_tier_serving_and_reports_degraded() {
    let (root, sys_block) = zram_state_fixture("detach", Some("none"));
    let state = zram_tier_state_from(&sys_block, &[zram_swap_entry()]);
    assert_eq!(state, ZramTierState::Degraded);
    // The tier still holds pages: the row draws the degraded face and
    // never the holding label the detach just refuted.
    assert_eq!(
        zram_tier_status(Some(state.as_str()), Some(true), Some(42), Some(64)),
        "🟠 DEGRADED [1st Target]"
    );
    // Without the state the row keeps its measured faces; the degraded
    // override is not a blanket replacement.
    assert_eq!(
        zram_tier_status(None, Some(true), Some(42), Some(64)),
        "🟢 ACTIVE [1st Target]"
    );
    assert_eq!(zram_tier_status(None, Some(false), None, None), "⚫ OFF");
    let _ = fs::remove_dir_all(&root);
}

/// SPEC ITEM-5, discipline #16: removing the volatile half degrades the
/// tier; it does not lose it. The state stays `degraded` — not
/// `inactive` and not `unmeasured` — so the RAM copy in the tier remains
/// described and a page in it stays recoverable. The byte-for-byte
/// proof is verified test case and verified test case; this pins the face that must not
/// report the tier as gone.
#[test]
fn a_page_in_the_tier_is_recoverable_after_the_volatile_half_is_removed() {
    let (root, sys_block) = zram_state_fixture("recover", Some("none"));
    let state = zram_tier_state_from(&sys_block, &[zram_swap_entry()]);
    assert_ne!(state, ZramTierState::Inactive);
    assert_ne!(state, ZramTierState::Unmeasured);
    assert_eq!(state, ZramTierState::Degraded);
    // The row keeps the tier's measured occupancy beside the degraded
    // state: the tier serves, and its numbers are not withdrawn.
    assert_eq!(
        zram_tier_status(Some(state.as_str()), Some(true), Some(4096), Some(65536)),
        "🟠 DEGRADED [1st Target]"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn tier_row_names_the_device_when_it_was_observed() {
    // The positive face: a live tier with data reports as holding.
    assert_eq!(
        tier_presence_status(Some(true), Some(512), Some(1024), "holding", "empty"),
        "holding"
    );
}

#[test]
fn an_unmeasured_tier_draws_not_measured_not_an_empty_bar() {
    // No sample at all: absence. Not an empty bar, which would read as
    // a measured zero.
    assert_eq!(
        tier_presence_status(None, None, None, "holding", "empty"),
        "⚪ not measured"
    );
}

#[test]
fn a_measured_empty_tier_draws_an_empty_bar_and_says_so() {
    // A real sample that says zero. That is a measurement, and it is
    // not the same fact as not having one.
    assert_eq!(
        tier_presence_status(Some(true), Some(0), Some(1024), "holding", "empty"),
        "empty"
    );
}

#[test]
fn a_compaction_row_refuses_a_torn_pair_rather_than_drawing_a_ratio() {
    // stored > raw is a tear, not a ratio under 1.00:1. The row refuses
    // rather than inventing a plausible number.
    let torn = cache_compaction_row(Some(&compaction_cache(1024, 4096)));
    assert_eq!(torn, "VRAM cache compaction: not measured");
}

#[test]
fn compression_row_draws_the_shrinkage_bar_from_the_two_counts() {
    // The bar is drawn from orig and compr; the ratio beside it is
    // derived from the same two, so a bar and a number on one line
    // cannot contradict each other.
    let sample = serde_json::json!({
        "orig_data_bytes": 4000,
        "backed_data_bytes": 0_u64,
        "compr_data_bytes": 1400,
        "mem_used_bytes": 1600,
        "same_pages": 0,
        "huge_pages": 0,
        "compressor": "lzo-rle",
    });
    let row = compression_row(Some(&sample), 10, false);
    assert!(row.contains('█'), "the bar is drawn: {row}");
    assert!(row.contains('░'), "the shrinkage is drawn: {row}");
    assert!(
        row.contains("2.86"),
        "the ratio is derived from the counts: {row}"
    );
}

#[test]
fn an_acknowledged_write_is_never_acknowledged_without_a_lower_copy() {
    // The invariant ITEM-5 owns: a volatile tier is only ever reported
    // as a cache of something that exists below. The label cannot say
    // `cache of X` when X was not observed — that would be
    // acknowledging a lower copy nobody named.
    assert_eq!(
        tier_authority_label("vram", Some(true), Some(true), "the zram tier"),
        "cache of the zram tier"
    );
    // No observed lower tier: it is the only copy we can see. The row
    // says so, which is what stops an operator trusting a copy that is
    // not there.
    assert_eq!(
        tier_authority_label("vram", Some(true), None, "the zram tier"),
        "vram only"
    );
}

#[test]
fn the_volatile_tier_never_reports_active_without_a_named_authority() {
    // A tier that is present but has no observed authority is not a
    // cache. Claiming `cache of …` here would be the orphan-page
    // condition dressed up as a status line.
    for (present, lower) in [(None, Some(true)), (Some(false), Some(true)), (None, None)] {
        assert_eq!(
            tier_authority_label("vram", present, lower, "the zram tier"),
            "unmeasured",
            "present={present:?} lower={lower:?}"
        );
    }
}

#[test]
fn a_history_series_holds_measured_fills_only() {
    let mut history = DashHistory::default();
    // No cache sample at all: the series must stay empty rather than
    // grow a zero bar that claims a measurement of emptiness.
    history.push_observation(&observation(false, false));
    assert!(history.cache_saved.is_empty());
    assert!(history.cache_held.is_empty());
    assert!(history.zram_saved.is_empty());
}

pub(super) fn sample_cache(
    logical_cached_bytes: u64,
    sampled_at_unix_ms: u64,
) -> WorkerCacheTelemetry {
    use ramshared_vram::{CodecCapability, CodecState, CodecTelemetry};

    WorkerCacheTelemetry {
        schema_version: 1,
        sampled_at_unix_ms,
        codec: CodecTelemetry::new(
            CodecCapability::Available,
            CodecState::Ready,
            Some("codec-subdeadline"),
        ),
        logical_cached_bytes,
        physical_cache_slab_bytes: 8 * 1024 * 1024,
        codec_workspace_bytes: 1024 * 1024,
        compressed_payload_bytes: 32 * 1024 * 1024,
        raw_payload_bytes: 4 * 1024 * 1024,
        metadata_bytes: Some(64 * 1024),
        device_metadata_lower_bound_bytes: None,
        raw_bypass_bytes: 16 * 1024 * 1024,
        codec_integrity_errors: 2,
        codec_decode_errors: 1,
        codec_timeouts: 3,
    }
}

#[path = "tests/benchmark_and_compact_views.rs"]
mod benchmark_and_compact_views;
#[path = "tests/cache_panel_and_latency_histograms.rs"]
mod cache_panel_and_latency_histograms;
#[path = "tests/control_plane_and_stale_samples.rs"]
mod control_plane_and_stale_samples;
#[path = "tests/dashboard_rows_and_tier_labels.rs"]
mod dashboard_rows_and_tier_labels;
#[path = "tests/glossary_and_frame_rendering.rs"]
mod glossary_and_frame_rendering;
#[path = "tests/gpu_planes_and_budget_status.rs"]
mod gpu_planes_and_budget_status;
#[path = "tests/pressure_views_and_localization.rs"]
mod pressure_views_and_localization;
#[path = "tests/residency_and_history_series.rs"]
mod residency_and_history_series;
#[path = "tests/secondary_screens_and_status.rs"]
mod secondary_screens_and_status;
#[path = "tests/telemetry_and_memory_diagnostics.rs"]
mod telemetry_and_memory_diagnostics;
#[path = "tests/tui_loop.rs"]
mod tui_loop;
