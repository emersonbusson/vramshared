// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/tests/health_and_snapshot.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

#[test]
fn status_document_keeps_health_errors_as_a_json_array() {
    crate::cascade::TEST_SWAPS.with(|cell| {
        *cell.borrow_mut() = Some("Filename\tType\tSize\tUsed\tPriority\n".to_string());
    });
    let document = super::super::status_view::status_json_document();
    crate::cascade::TEST_SWAPS.with(|cell| *cell.borrow_mut() = None);
    let value: serde_json::Value = serde_json::from_str(&document.unwrap()).unwrap();
    assert!(
        value
            .get("health_errors")
            .is_some_and(serde_json::Value::is_array)
    );
}

#[test]
fn released_cache_reports_off_not_unavailable() {
    // A control-pressure release publishes `ok: false` beside an explicit
    // `"OFF"`. That is a deliberate release, not a dead cache, and must
    // stay distinguishable from `"UNAVAILABLE"`.
    assert_eq!(
        cache_state_from_status(Some("OFF"), true, false),
        CacheState::Off
    );
    assert_eq!(
        cache_state_from_status(Some("OFF"), true, true),
        CacheState::Off
    );
    assert_eq!(
        cache_state_from_status(Some("OFF"), false, false),
        CacheState::Off
    );
}

#[test]
fn stuck_verdict_survives_a_failed_ok_flag() {
    // `STUCK` is a named death signature. `ok: false` must not demote it
    // to the generic `UNAVAILABLE` bucket.
    assert_eq!(
        cache_state_from_status(Some("STUCK"), true, false),
        CacheState::Stuck
    );
    assert_eq!(
        cache_state_from_status(Some("UNAVAILABLE"), true, false),
        CacheState::Unavailable
    );
}

#[test]
fn ok_false_stops_a_lying_active_or_restricted() {
    // This is what `ok` is for: a status that claims to be serving while
    // reporting it is not healthy is not believed.
    assert_eq!(
        cache_state_from_status(Some("ACTIVE"), true, false),
        CacheState::Unavailable
    );
    assert_eq!(
        cache_state_from_status(Some("RESTRICTED"), true, false),
        CacheState::Unavailable
    );
    assert_eq!(
        cache_state_from_status(Some("ACTIVE"), true, true),
        CacheState::Active
    );
    assert_eq!(
        cache_state_from_status(Some("RESTRICTED"), true, true),
        CacheState::Restricted
    );
}

#[test]
fn missing_verdict_fails_closed_when_the_product_is_active() {
    assert_eq!(
        cache_state_from_status(None, true, false),
        CacheState::Unavailable
    );
    assert_eq!(
        cache_state_from_status(Some("NONSENSE"), true, true),
        CacheState::Unavailable
    );
    assert_eq!(cache_state_from_status(None, false, false), CacheState::Off);
}

#[test]
fn a_released_cache_renders_off_with_zero_cached_kib() {
    // This is the live shape after a control-pressure release: the tier is
    // still serving swap while the GPU cache is empty. The line must say
    // so in the same words the status JSON uses, or a reader will take
    // `protection: ACTIVE` as a claim about the cache.
    let snap = snapshot_for_cache_line(CacheState::Off, Some(0), Some(2853175296 / 1024));
    assert_eq!(
        cache_line(&snap),
        "cache: OFF (vram_cached_kib=0 gpu_headroom_kib=2786304; \
             separate claim from protection)"
    );
}

#[test]
fn absent_residency_numbers_render_as_null_not_as_zero() {
    // `null` and `0` mean different things: `0` is a measured empty cache,
    // `null` is "no sample". Collapsing them would let a missing
    // measurement read as a successful empty one.
    let snap = snapshot_for_cache_line(CacheState::Unavailable, None, None);
    assert_eq!(
        cache_line(&snap),
        "cache: UNAVAILABLE (vram_cached_kib=null gpu_headroom_kib=null; \
             separate claim from protection)"
    );
}

#[test]
fn every_cache_state_renders_its_own_label() {
    // A reader must be able to tell ACTIVE from RESTRICTED from STUCK on
    // one line. If these collapse, the status line cannot be the place a
    // user learns the cache died.
    for (state, label) in [
        (CacheState::Off, "OFF"),
        (CacheState::Active, "ACTIVE"),
        (CacheState::Restricted, "RESTRICTED"),
        (CacheState::Unavailable, "UNAVAILABLE"),
        (CacheState::Stuck, "STUCK"),
    ] {
        let line = cache_line(&snapshot_for_cache_line(state, Some(1), Some(2)));
        assert!(
            line.starts_with(&format!("cache: {label} ")),
            "state {state:?} rendered as {line}"
        );
    }
}

#[test]
fn the_cache_line_is_never_a_protection_claim() {
    // The whole reason this line exists: `protection: ACTIVE` and
    // `cache: ACTIVE` are two facts, and the status text must not let one
    // stand in for the other. The cache line names its own subject and
    // never emits a `protection:` label of its own.
    let snap = snapshot_for_cache_line(CacheState::Off, Some(0), Some(1));
    let line = cache_line(&snap);
    assert!(line.starts_with("cache: "), "{line}");
    assert!(!line.starts_with("protection"), "{line}");
    assert!(
        !line.contains("protection: "),
        "cache line must not emit a protection label: {line}"
    );
    assert!(
        line.contains("separate claim from protection"),
        "the line has to say the two are different claims: {line}"
    );
}

#[test]
fn the_overall_line_names_every_input_the_verdict_weighs() {
    // A reader who sees `ok: false` has to be able to see which input
    // drove it. Dropping one from the parenthetical is how a missing
    // input starts to read as fine.
    let mut snap = snapshot_for_cache_line(CacheState::Off, Some(0), None);
    snap.control_state = ControlState::Guarded;
    snap.origin_state = OriginState::Ready;
    snap.guardian_state = GuardianState::Healthy;
    let view = derive_lifecycle(&snap);
    assert_eq!(
        overall_line(&view, &snap),
        "overall: BLOCKED (control=GUARDED origin=READY guardian=HEALTHY cache=OFF \
             release=coherent)"
    );

    // Release coherence is weighed too, so it is named too.
    snap.release_coherence = ReleaseCoherence::Stale;
    let view = derive_lifecycle(&snap);
    assert!(
        overall_line(&view, &snap).contains("release=stale"),
        "the overall line does not name the fleet: {}",
        overall_line(&view, &snap)
    );
}

#[test]
fn the_ok_line_states_the_same_verdict_the_json_does() {
    // This is the snapshot the live host actually publishes: tiers in
    // order, VRAM tier serving, control plane guarded, cache released,
    // GPU budget unmeasured. The human line used to print `ok: true` here
    // while `status --json` printed `ok: false` for the same read.
    let mut snap = snapshot_for_cache_line(CacheState::Off, Some(0), None);
    snap.control_state = ControlState::Guarded;
    snap.zram = TierSample {
        present: true,
        prio: Some(200),
        size_kib: 2_097_148,
        used_kib: 1_722_644,
    };
    snap.vram = TierSample {
        present: true,
        prio: Some(100),
        size_kib: 4_194_300,
        used_kib: 132_500,
    };
    snap.disk = TierSample {
        present: true,
        prio: Some(-2),
        size_kib: 4_194_304,
        used_kib: 111_060,
    };
    snap.measurement_errors = vec!["gpu_budget_telemetry_invalid_or_stale".to_string()];
    let view = derive_lifecycle(&snap);

    let human = format!("ok: {}", status_ok(&view, &snap));
    let json = render_status_json(&view, &snap, "2026-10-02T00:21:47-03:00");
    assert_eq!(human, "ok: false");
    assert!(json.contains("\"ok\":false"), "{json}");
    assert!(json.contains("\"topology_ok\":true"), "{json}");
}

#[test]
fn gpu_budget_status_requires_fresh_driver_bound_telemetry() {
    let status = serde_json::json!({
        "gpu_budget": {
            "schema_version": 1,
            "adapter": {
                "backend": "vulkan",
                "key": "uuid:fixture",
                "luid": "aabbccdd:00001122"
            },
            "total_bytes": 8000,
            "budget_bytes": 6000,
            "used_bytes": 2000,
            "available_bytes": 4000,
            "source": "driver_reported",
            "sampled_at_unix_ms": 1000
        }
    });
    assert!(trusted_budget(&status, Some(5000)).is_some());
    assert!(trusted_budget(&status, Some(6001)).is_none());
    assert!(trusted_budget(&status, Some(999)).is_none());

    let mut local_only = status.clone();
    local_only["gpu_budget"]["source"] = serde_json::json!("provider_local_estimate");
    assert!(trusted_budget(&local_only, Some(5000)).is_none());

    let malformed = serde_json::json!({"gpu_budget": {"available_bytes": 4000}});
    assert!(trusted_budget(&malformed, Some(5000)).is_none());
}

#[test]
fn a_bound_but_stale_budget_is_still_the_record_and_not_a_never_bound_null() {
    // The record and the projection are different facts. Spending the
    // `null` semantic — "the daemon never bound a budget" — on a sample
    // that merely aged out would tell the operator the daemon never had
    // one, and would leave `gpu_budget_telemetry_invalid_or_stale` with no
    // sample to name.
    let status = serde_json::json!({
        "gpu_budget": {
            "schema_version": 1,
            "adapter": {
                "backend": "vulkan",
                "key": "uuid:fixture",
                "luid": "aabbccdd:00001122"
            },
            "total_bytes": 8000,
            "budget_bytes": 6000,
            "used_bytes": 2000,
            "available_bytes": 4000,
            "source": "driver_reported",
            "sampled_at_unix_ms": 1000
        }
    });

    // 6001 is past `GPU_BUDGET_MAX_AGE_MS` for a sample stamped at 1000.
    let (record, headroom) = budget_record_and_headroom(Some(&status), Some(6001));
    let sample = record.expect("a bound sample is kept even when it is too old to act on");
    assert_eq!(sample.budget_bytes, 6000);
    assert_eq!(sample.sampled_at_unix_ms, 1000);
    // The projection fails closed: do not act on it.
    assert_eq!(headroom, None);
    // And the alarm still has a sample to point at.
    assert!(gpu_budget_telemetry_invalid_or_stale(
        Some(&status),
        Some(6001)
    ));

    // A fresh sample is both the record and an actionable projection.
    let (fresh_record, fresh_headroom) = budget_record_and_headroom(Some(&status), Some(5000));
    assert!(fresh_record.is_some());
    assert_eq!(fresh_headroom, Some(alloc_bound_kib(4000)));

    // Never-bound is a different fact and stays absent in both slots.
    let never_bound = serde_json::json!({ "gpu_budget": null });
    let (no_record, no_headroom) = budget_record_and_headroom(Some(&never_bound), Some(5000));
    assert!(no_record.is_none());
    assert_eq!(no_headroom, None);
    // ... and it is not an alarm: `allocator_only` is valid telemetry.
    assert!(!gpu_budget_telemetry_invalid_or_stale(
        Some(&never_bound),
        Some(5000)
    ));

    // No document at all is also absent in both slots.
    let (missing, missing_headroom) = budget_record_and_headroom(None, Some(5000));
    assert!(missing.is_none());
    assert_eq!(missing_headroom, None);
}

/// A sub-KiB headroom projects `0` and keeps its remainder on the record.
///
/// verified test case's bound half: `available >> 10` floors, and that is the
/// right number for an allocation bound — rounding up would grant more
/// than the exact remainder. What makes the floor honest is the pair, not
/// the number: `capacity.gpu_headroom_kib: 0` beside `gpu_budget`'s raw
/// `available_bytes: 512` is "at most 0 KiB, and 512 bytes are still
/// there". Dropping the record would leave a reader unable to tell that
/// from a measured exhaustion, which is the fold the two-field document
/// exists to refuse.
#[test]
fn a_sub_kib_headroom_projects_zero_and_keeps_the_remainder_on_the_record() {
    let available_bytes = 512u64;
    let status = serde_json::json!({
        "gpu_budget": {
            "schema_version": 1,
            "adapter": {
                "backend": "vulkan",
                "key": "uuid:fixture",
                "luid": "aabbccdd:00001122"
            },
            "total_bytes": 8000,
            "budget_bytes": 6000,
            "used_bytes": 6000 - available_bytes,
            "available_bytes": available_bytes,
            "source": "driver_reported",
            "sampled_at_unix_ms": 1000
        }
    });

    let (record, headroom) = budget_record_and_headroom(Some(&status), Some(5000));
    let sample = record.expect("a bound sample is published beside its projection");
    assert_eq!(
        headroom,
        Some(0),
        "a 512-byte bound is at most 0 KiB, and that zero is a bound"
    );
    assert_eq!(
        sample.available_bytes, available_bytes,
        "the raw remainder is the third state and must survive the floor"
    );

    // A measured exhaustion is still a `0` — the record/projection pair
    // above does not turn every zero into a remainder in disguise. The
    // opposite rounding of a hold is proved where both faces live, in the
    // daemon's `holds_ceil_and_bounds_floor_on_the_same_sub_kib_remainder`.
    assert_eq!(alloc_bound_kib(0), 0);
    let exhausted = serde_json::json!({
        "gpu_budget": {
            "schema_version": 1,
            "adapter": {
                "backend": "vulkan",
                "key": "uuid:fixture",
                "luid": "aabbccdd:00001122"
            },
            "total_bytes": 8000,
            "budget_bytes": 6000,
            "used_bytes": 6000,
            "available_bytes": 0,
            "source": "driver_reported",
            "sampled_at_unix_ms": 1000
        }
    });
    let (record, headroom) = budget_record_and_headroom(Some(&exhausted), Some(5000));
    assert_eq!(headroom, Some(0));
    assert_eq!(
        record
            .expect("exhausted sample is still the record")
            .available_bytes,
        0
    );
}

#[test]
fn gpu_budget_absent_or_null_is_honest_not_stale() {
    // No status file at all: nothing was published, so there is nothing
    // stale to alarm about.
    assert!(!gpu_budget_telemetry_invalid_or_stale(None, Some(5000)));

    // A status file with no `gpu_budget` key is also an honest "no budget
    // bound" report.
    let absent_key = serde_json::json!({
        "schema_version": 1,
        "ok": true,
        "cache_state": "ACTIVE"
    });
    assert!(!gpu_budget_telemetry_invalid_or_stale(
        Some(&absent_key),
        Some(5000)
    ));

    // Explicit `"gpu_budget": null` is the daemon's `allocator_only`
    // guard with no budget bound. It is valid telemetry and must not be
    // classified as invalid or stale.
    let null_budget = serde_json::json!({
        "schema_version": 1,
        "ok": false,
        "cache_state": "UNAVAILABLE",
        "gpu_budget": null
    });
    assert!(!gpu_budget_telemetry_invalid_or_stale(
        Some(&null_budget),
        Some(5000)
    ));
    // The same is true when the clock cannot be read: an honest "no
    // budget bound" does not become stale just because time is unknown.
    assert!(!gpu_budget_telemetry_invalid_or_stale(
        Some(&null_budget),
        None
    ));
}

#[test]
fn gpu_budget_published_but_untrustworthy_is_stale() {
    let trusted = serde_json::json!({
        "gpu_budget": {
            "schema_version": 1,
            "adapter": {
                "backend": "vulkan",
                "key": "uuid:fixture",
                "luid": "aabbccdd:00001122"
            },
            "total_bytes": 8000,
            "budget_bytes": 6000,
            "used_bytes": 2000,
            "available_bytes": 4000,
            "source": "driver_reported",
            "sampled_at_unix_ms": 1000
        }
    });
    // Fresh, driver-bound, parseable: valid telemetry.
    assert!(!gpu_budget_telemetry_invalid_or_stale(
        Some(&trusted),
        Some(5000)
    ));

    // Outside the 5s trust window: stale.
    assert!(gpu_budget_telemetry_invalid_or_stale(
        Some(&trusted),
        Some(6001)
    ));

    // Published but malformed: invalid.
    let malformed = serde_json::json!({"gpu_budget": {"available_bytes": 4000}});
    assert!(gpu_budget_telemetry_invalid_or_stale(
        Some(&malformed),
        Some(5000)
    ));

    // Published but not driver-bound: untrustworthy.
    let mut local_only = trusted.clone();
    local_only["gpu_budget"]["source"] = serde_json::json!("provider_local_estimate");
    assert!(gpu_budget_telemetry_invalid_or_stale(
        Some(&local_only),
        Some(5000)
    ));

    // Published object with no clock reading: cannot be trusted.
    assert!(gpu_budget_telemetry_invalid_or_stale(Some(&trusted), None));
}

#[test]
fn gpu_utilization_absent_or_null_is_honest_not_stale() {
    assert!(!gpu_utilization_telemetry_invalid_or_stale(
        None,
        Some(5000)
    ));

    let absent_key = serde_json::json!({
        "schema_version": 1,
        "ok": true,
        "cache_state": "ACTIVE"
    });
    assert!(!gpu_utilization_telemetry_invalid_or_stale(
        Some(&absent_key),
        Some(5000)
    ));

    // `"gpu_utilization": null` is "not measured": the adapter had no
    // utilization source, or the driver refused the call. It is valid
    // telemetry and must never be read as a measured idle zero.
    let not_measured = serde_json::json!({
        "schema_version": 1,
        "ok": true,
        "gpu_utilization": null
    });
    assert!(!gpu_utilization_telemetry_invalid_or_stale(
        Some(&not_measured),
        Some(5000)
    ));
    assert!(!gpu_utilization_telemetry_invalid_or_stale(
        Some(&not_measured),
        None
    ));
}

#[test]
fn gpu_utilization_published_but_untrustworthy_is_stale() {
    let trusted = serde_json::json!({
        "gpu_utilization": {
            "schema_version": 1,
            "gpu_pct": 37,
            "memory_pct": 82,
            "sampled_at_unix_ms": 1000
        }
    });
    assert!(!gpu_utilization_telemetry_invalid_or_stale(
        Some(&trusted),
        Some(5000)
    ));

    // Outside the shared 5s trust window: stale. A stale busy-time sample
    // must not be dropped into a rendered `0%`.
    assert!(gpu_utilization_telemetry_invalid_or_stale(
        Some(&trusted),
        Some(6001)
    ));

    // Out of range is refused, never clamped into a plausible figure.
    let out_of_range = serde_json::json!({
        "gpu_utilization": {
            "schema_version": 1,
            "gpu_pct": 101,
            "memory_pct": 82,
            "sampled_at_unix_ms": 1000
        }
    });
    assert!(gpu_utilization_telemetry_invalid_or_stale(
        Some(&out_of_range),
        Some(5000)
    ));

    // Published but malformed: invalid.
    let malformed = serde_json::json!({"gpu_utilization": {"gpu_pct": 10}});
    assert!(gpu_utilization_telemetry_invalid_or_stale(
        Some(&malformed),
        Some(5000)
    ));

    // Published object with no clock reading: cannot be trusted.
    assert!(gpu_utilization_telemetry_invalid_or_stale(
        Some(&trusted),
        None
    ));
}

#[test]
fn a_stale_utilization_sample_is_still_the_record_and_not_a_never_measured_null() {
    // A reading that exists but is no longer current is not the same fact
    // as a reading the driver never produced. The composer keeps the
    // record; the display is what says `stale`.
    let status = serde_json::json!({
        "gpu_utilization": {
            "schema_version": 1,
            "gpu_pct": 37,
            "memory_pct": 82,
            "sampled_at_unix_ms": 1000
        }
    });

    let fresh = utilization_record(Some(&status)).expect("fresh sample is the record");
    assert_eq!(fresh.gpu_pct, 37);
    assert_eq!(fresh.memory_pct, 82);
    assert_eq!(fresh.sampled_at_unix_ms, 1_000);

    // The record does not consult the clock at all, so a sample that aged
    // past the trust window is still published as itself. Collapsing it to
    // `null` here is what told the operator the driver never spoke.
    let stale = utilization_record(Some(&status)).expect("aged sample is still the record");
    assert_eq!(stale.sampled_at_unix_ms, 1_000);
    assert!(!stale.trusted_at(6_001, crate::monitor::GPU_BUDGET_MAX_AGE_MS));
    // ...and the alarm still has a sample to name, which is only possible
    // because the record was not erased first.
    assert!(gpu_utilization_telemetry_invalid_or_stale(
        Some(&status),
        Some(6_001)
    ));

    // Absence stays absence: `null` and a missing key are "not measured",
    // and neither is inventable from an aged sample.
    let not_measured = serde_json::json!({ "gpu_utilization": null });
    assert!(utilization_record(Some(&not_measured)).is_none());
    let missing_key = serde_json::json!({ "ok": true });
    assert!(utilization_record(Some(&missing_key)).is_none());
    assert!(utilization_record(None).is_none());

    // Malformed still has no record to keep: it will not parse, so it is
    // absence rather than an aged reading.
    let malformed = serde_json::json!({ "gpu_utilization": { "gpu_pct": 10 } });
    assert!(utilization_record(Some(&malformed)).is_none());
}

/// Cache-event counters are cumulative counts, so the composer keeps a
/// measured zero exactly as it keeps a measured one. `None` is "this
/// document never carried the key" and is never published as a zero count.
#[test]
fn the_cache_event_counters_keep_a_measured_zero_and_refuse_to_invent_one() {
    let present = serde_json::json!({
        "cache_fallback_reads": 7,
        "cache_invalidations": 0,
        "cache_releases": 4
    });
    assert_eq!(
        cache_event_counters(Some(&present)),
        (Some(7), Some(0), Some(4)),
        "a measured zero is a real zero, not an absence"
    );

    // Absence stays absence: a missing key, a JSON `null` and a document
    // that never arrived all read `None`, and none of them is a zero count.
    let missing = serde_json::json!({ "ok": true });
    assert_eq!(cache_event_counters(Some(&missing)), (None, None, None));
    let nulled = serde_json::json!({
        "cache_fallback_reads": null,
        "cache_invalidations": null,
        "cache_releases": null
    });
    assert_eq!(cache_event_counters(Some(&nulled)), (None, None, None));
    assert_eq!(cache_event_counters(None), (None, None, None));

    // A counter that is not a non-negative integer is not coerced into one.
    // Reporting `0` here would invent a measurement the daemon never made.
    let nonsense = serde_json::json!({
        "cache_fallback_reads": "twelve",
        "cache_invalidations": -1,
        "cache_releases": 2.5
    });
    assert_eq!(cache_event_counters(Some(&nonsense)), (None, None, None));
}

#[test]
fn writeback_backing_and_pump_health_lines_report_measured_facts() {
    let mut snap = snapshot_for_cache_line(CacheState::Active, Some(1024), Some(4096));
    snap.origin_cache_mode = Some(true);
    snap.topology.zram_backing_dev = Some("/dev/nbd0".to_string());
    snap.topology.writeback_target_is_tier = true;
    snap.logical_capacity_kib = Some(4_194_304);
    snap.ssd_origin_written_kib = Some(65_536);

    assert_eq!(
        super::super::status_view::writeback_backing_line(&snap, Some(131_072)),
        "origin backing: bound=true role=writeback_target dev=/dev/nbd0 size_kib=4194304 spilled_kib=131072"
    );
    snap.origin_cache_mode = Some(false);
    assert_eq!(
        super::super::status_view::writeback_backing_line(&snap, None),
        "vram backing: bound=true role=writeback_target dev=/dev/nbd0 size_kib=4194304 spilled_kib=65536"
    );
    snap.origin_cache_mode = None;
    snap.logical_capacity_kib = None;
    snap.ssd_origin_written_kib = None;
    assert_eq!(
        super::super::status_view::writeback_backing_line(&snap, None),
        "tier backing: bound=true role=writeback_target dev=/dev/nbd0 size_kib=null spilled_kib=null"
    );

    let root = std::env::temp_dir().join(format!("ramshared-status-bdstat-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("zram4")).unwrap();
    std::fs::write(root.join("zram4/bd_stat"), "   1024   12   2048\n").unwrap();
    let entries = vec![SwapEntry {
        filename: "/dev/zram4".to_string(),
        size_kb: 4_194_300,
        used_kb: 1_000,
        priority: 200,
    }];
    assert_eq!(
        super::super::status_view::read_zram_bd_count_kib(&root, &entries),
        Some(4096)
    );
    std::fs::write(root.join("zram4/bd_stat"), "not-a-number\n").unwrap();
    assert_eq!(
        super::super::status_view::read_zram_bd_count_kib(&root, &entries),
        None
    );
    let _ = std::fs::remove_dir_all(&root);

    let measured = super::super::health_documents::PumpHealth {
        state: super::super::health_documents::PumpState::Bound,
        last_cycle_mib: Some(32),
        effective_mib_per_s: Some(240),
        p99_cycle_ms: Some(133),
        consecutive_slow_cycles: 0,
    };
    assert_eq!(
        super::super::status_view::pump_health_line(&measured),
        "pump: state=bound last_cycle_mib=32 effective_mib_per_s=240 p99_cycle_ms=133"
    );
    let unmeasured = super::super::health_documents::PumpHealth {
        state: super::super::health_documents::PumpState::Degraded,
        last_cycle_mib: None,
        effective_mib_per_s: None,
        p99_cycle_ms: None,
        consecutive_slow_cycles: 3,
    };
    assert_eq!(
        super::super::status_view::pump_health_line(&unmeasured),
        "pump: state=degraded last_cycle_mib=null effective_mib_per_s=null p99_cycle_ms=null"
    );
}
