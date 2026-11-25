// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/tests/benchmark_and_compact_views.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;

#[test]
fn utilization_row_never_rewrites_nonsense_as_an_aged_reading() {
    // Malformed is the driver speaking in a shape the product cannot use,
    // which is absence — never a reading that went stale. Checking the body
    // before the age keeps "nonsense" from being laundered into "old".
    let nonsense = serde_json::json!({
        "schema_version": 1,
        "gpu_pct": 101,
        "memory_pct": 82,
        "sampled_at_unix_ms": 1_000,
    });
    let unmeasured =
        " GPU busy (sample window): not measured\n mem busy (sample window): not measured";
    assert_eq!(
        utilization_row(
            Some(&nonsense),
            20,
            false,
            1_000 + GPU_BUDGET_MAX_AGE_MS + 1
        ),
        unmeasured
    );

    let unknown_schema = serde_json::json!({
        "schema_version": 2,
        "gpu_pct": 15,
        "memory_pct": 62,
        "sampled_at_unix_ms": 1_000,
    });
    assert_eq!(
        utilization_row(Some(&unknown_schema), 20, false, 1_000),
        unmeasured
    );
}

#[test]
fn tier_accumulator_tracks_directional_peaks() {
    let mut acc = TierAccumulator::default();
    acc.record(10.0, 20.0);
    acc.record(35.0, 5.0);
    assert_eq!(acc.read_max_mbs, 35.0);
    assert_eq!(acc.write_max_mbs, 20.0);
    assert_eq!(acc.max_mbs, 40.0);
    let mut stats = TierIoStats::default();
    acc.apply_to_plane_io(&mut stats);
    assert_eq!(stats.read_peak_mbs, Some(35.0));
    assert_eq!(stats.write_peak_mbs, Some(20.0));
    assert_eq!(stats.max_mbs, Some(40.0));
}

/// An accumulator that has never been recorded publishes absence, not the
/// zero its `f64` fields used to wear.
#[test]
fn tier_accumulator_without_a_window_publishes_absence() {
    let acc = TierAccumulator::default();
    assert_eq!(acc.windows, 0);
    assert_eq!(acc.count, 0);
    assert_eq!(acc.avg_mbs(), None);
    let mut stats = TierIoStats::default();
    acc.apply_to_plane_io(&mut stats);
    assert_eq!(stats.read_peak_mbs, None);
    assert_eq!(stats.write_peak_mbs, None);
    assert_eq!(stats.min_mbs, None);
    assert_eq!(stats.avg_mbs, None);
    assert_eq!(stats.max_mbs, None);
    assert_eq!(stats.peak_mbs, None);
}

#[test]
fn once_stream_writes_matching_log_and_atomic_heartbeat() {
    let root = std::env::temp_dir().join(format!("ramshared-monitor-once-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let output = root.join("health.jsonl");
    let heartbeat = root.join("heartbeat.json");
    let options = MonitorOptions {
        jsonl: true,
        once: true,
        output: Some(output.clone()),
        heartbeat: Some(heartbeat.clone()),
        ..MonitorOptions::default()
    };

    run(&options).expect("one typed observation");

    let log: Value = serde_json::from_str(&fs::read_to_string(output).unwrap()).unwrap();
    let current: Value = serde_json::from_str(&fs::read_to_string(heartbeat).unwrap()).unwrap();
    assert_eq!(log["schema_version"], 5);
    assert_eq!(log["epoch_ms"], current["epoch_ms"]);
    assert!(log["mem"]["total_kib"].as_u64().is_some());
    assert!(log["mem"].get("anon_pages_kib").is_some());
    assert!(matches!(
        log["hyperv_balloon"]["debugfs_status"].as_str(),
        Some("readable" | "permission_denied" | "not_found" | "read_error")
    ));
    assert!(matches!(
        log["cgroup_memory"]["status"].as_str(),
        Some("root" | "partial" | "unavailable")
    ));
    assert!(
        log["process_totals"]["visible_processes"]
            .as_u64()
            .is_some()
    );
    assert!(log["control_plane"]["memory_psi_some_avg10"].is_number());
    fs::remove_dir_all(root).expect("remove stream fixture");
}

#[test]
fn helper_failures_are_explicit() {
    assert_eq!(memory_used_pct(&MemoryObservation::default()), None);
    assert_eq!(
        format!("{}", MonitorError::Io("x".into())),
        "monitor I/O: x"
    );
    assert_eq!(
        format!("{}", MonitorError::Json("x".into())),
        "monitor JSON: x"
    );
    assert_eq!(
        format!("{}", MonitorError::Terminal("x".into())),
        "monitor terminal: x"
    );
    let sample = observation(false, false);
    assert_eq!(sample.bool_value("missing"), None);
    assert_eq!(sample.string("missing"), "unknown");
}

#[test]
fn monitor_v4_records_full_pressure_and_sanitized_topn() {
    let root = std::env::temp_dir().join(format!("ramshared-monitor-proc-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let process = root.join("123");
    fs::create_dir_all(&process).unwrap();
    fs::write(process.join("comm"), "build worker\n").unwrap();
    fs::write(
        process.join("cgroup"),
        "0::/ramshared-workloads.slice/ramshared-build-fixture.scope\n",
    )
    .unwrap();
    fs::write(process.join("status"), "VmRSS:\t2048 kB\nVmSwap:\t64 kB\n").unwrap();
    fs::write(
        process.join("stat"),
        "123 (build worker) S 0 0 0 0 0 0 0 0 0 0 3 4 0\n",
    )
    .unwrap();
    fs::write(process.join("io"), "read_bytes: 10\nwrite_bytes: 20\n").unwrap();
    fs::write(process.join("cmdline"), "--token=do-not-persist").unwrap();
    let (top, totals) = collect_process_snapshot(&root, 10);
    let serialized = serde_json::to_string(&top).unwrap();
    assert_eq!(top.len(), 1);
    assert_eq!(top[0].comm, "buildworker");
    assert_eq!(totals.visible_processes, 1);
    assert_eq!(totals.rss_kib, Some(2048));
    assert_eq!(totals.swap_kib, Some(64));
    assert!(top[0].managed);
    assert!(!serialized.contains("do-not-persist"));
    let mut outside = top[0].clone();
    outside.managed = false;
    outside.rss_kib = Some(600 * 1024);
    outside.swap_kib = Some(0);
    assert_eq!(
        classify_unmanaged_memory_usage(&[outside]),
        ("UNMANAGED_MEMORY", 600 * 1024, 1)
    );
    assert_eq!(classify_unmanaged_memory_usage(&top), ("NONE", 0, 0));
    let pressure = parse_memory_pressure(
        "some avg10=1 avg60=2 avg300=3 total=1\nfull avg10=4 avg60=5 avg300=6 total=2\n",
    );
    assert_eq!(
        (
            pressure.memory_psi_full_avg10,
            pressure.memory_psi_full_avg60,
            pressure.memory_psi_full_avg300,
        ),
        (4.0, 5.0, 6.0)
    );
    // Unreported counters are absent, not zero: this body has no fault
    // pair, and `parse_vmstat_publishes_an_unreported_counter_as_absent_not_zero`
    // is where that face is pinned in full.
    assert_eq!(
        parse_vmstat("pswpin 7\npswpout 9\n"),
        (Some(7), Some(9), None, None)
    );
    let events = parse_memory_events("high 1\nmax 2\noom 3\noom_kill 4\n");
    assert_eq!(
        (events.high, events.max, events.oom, events.oom_kill),
        (Some(1), Some(2), Some(3), Some(4))
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn monitor_uses_fresh_adapter_bound_budget_from_active_worker() {
    let status = serde_json::from_value::<Map<String, Value>>(serde_json::json!({
            "gpu_budget": {
                "schema_version": 1,
                "adapter": { "backend": "vulkan", "key": "pci-0000:03:00.0", "luid": "aabbccdd:00001122" },
                "total_bytes": 8_589_934_592u64,
                "budget_bytes": 6_442_450_944u64,
                "used_bytes": 2_147_483_648u64,
                "available_bytes": 4_294_967_296u64,
                "source": "driver_reported",
                "sampled_at_unix_ms": 1000
            }
        })).expect("valid fixture status");

    let sample = gpu_observation_from_status(&status).expect("fresh provider budget");
    assert_eq!(sample.adapter.backend, "vulkan");
    assert_eq!(sample.adapter.key, "pci-0000:03:00.0");
    assert_eq!(sample.sampled_at_unix_ms, 1000);
    // The record keeps the unit the telemetry carries. The whole-MiB
    // figures beside it are a view derived from these, never a
    // re-statement of them.
    assert_eq!(sample.total_bytes, Some(8_589_934_592));
    assert_eq!(sample.budget_bytes, 6_442_450_944);
    assert_eq!(sample.used_bytes, 2_147_483_648);
    assert_eq!(sample.free_bytes, 4_294_967_296);
    assert_eq!(sample.total_mib(), Some(8192));
    assert_eq!(sample.budget_mib(), 6144);
    assert_eq!(sample.used_mib(), 2048);
    assert_eq!(sample.free_mib(), 4096);
}

#[test]
fn monitor_refuses_malformed_and_unidentified_gpu_budgets() {
    let mut status = serde_json::from_value::<Map<String, Value>>(serde_json::json!({
        "gpu_budget": {
            "schema_version": 1,
            "adapter": { "backend": "cuda", "key": "gpu-0", "luid": null },
            "total_bytes": 6144,
            "budget_bytes": 6144,
            "used_bytes": 1024,
            "available_bytes": 5120,
            "source": "driver_reported",
            "sampled_at_unix_ms": 1000
        }
    }))
    .expect("valid fixture status");

    // An adapter this reader cannot name is absence, not an aged reading.
    status["gpu_budget"]["adapter"]["key"] = Value::String(String::new());
    assert!(gpu_observation_from_status(&status).is_none());

    // A body that will not parse is no sample at all.
    status["gpu_budget"] = serde_json::json!({ "available_bytes": 5120 });
    assert!(gpu_observation_from_status(&status).is_none());
    assert!(gpu_observation_from_status(&Map::new()).is_none());
}

/// An estimate is a real figure of a less authoritative kind, not a torn
/// one, so it is kept as a record and the row marks it. Refusing it at
/// the projection collapsed it into the panel's absence wording, which
/// says the provider never spoke when it published an estimate.
///
/// Only the driver's own report is allocated against; that is
/// [`ramshared_vram::GpuBudgetTelemetry::trusted_available_at`] and is
/// deliberately not this display path.
#[test]
fn monitor_keeps_an_estimate_budget_record_and_marks_it() {
    // Real byte figures, not MiB counts in a byte field: the row's whole
    // point is that the exact bytes carry the ratio and the MiB pair is a
    // publish conversion.
    let status = serde_json::from_value::<Map<String, Value>>(serde_json::json!({
        "gpu_budget": {
            "schema_version": 1,
            "adapter": { "backend": "cuda", "key": "gpu-0", "luid": null },
            "total_bytes": 6_442_450_944u64,
            "budget_bytes": 6_442_450_944u64,
            "used_bytes": 2_147_483_648u64,
            "available_bytes": 4_294_967_296u64,
            "source": "provider_local_estimate",
            "sampled_at_unix_ms": 1000
        }
    }))
    .expect("valid fixture status");

    let sample = gpu_observation_from_status(&status).expect("an estimate is a well-formed record");
    assert_eq!(sample.source, GpuBudgetSource::ProviderLocalEstimate);
    assert_eq!(sample.budget_bytes, 6_442_450_944);
    assert_eq!(sample.free_bytes, 4_294_967_296);

    // The figures stay and the row names what they are.
    let row = gpu_budget_row(Some(&sample), 1000);
    assert!(
        row.contains("(estimate)"),
        "an estimate is marked, not hidden: {row}"
    );
    assert!(
        !row.contains("unavailable"),
        "an estimate is not an absence: {row}"
    );
    assert!(
        row.contains("2048 / 6144 MiB"),
        "the figures stay on screen: {row}"
    );

    // A driver reading carries no estimate mark — the mark names a
    // difference, so it must not appear on the ordinary case.
    let mut driver_status = status.clone();
    driver_status["gpu_budget"]["source"] = Value::String("driver_reported".into());
    let driver = gpu_observation_from_status(&driver_status).expect("the driver record parses");
    let driver_row = gpu_budget_row(Some(&driver), 1000);
    assert!(
        !driver_row.contains("estimate"),
        "a driver reading is not an estimate: {driver_row}"
    );

    // Two facts can hold at once and each keeps its own mark: the source
    // is a property of the body, the age a property of the clock.
    let aged_at = 1000 + GPU_BUDGET_MAX_AGE_MS + 1;
    let aged_row = gpu_budget_row(Some(&sample), aged_at);
    assert!(
        aged_row.contains("(estimate) (stale)"),
        "an aged estimate keeps both facts, in the order they became true: {aged_row}"
    );
}

/// An aged budget is a **record**, not an absence (DT-8). The worker
/// spoke; only the reading went cold. Collapsing it to the absence
/// wording told an operator the driver never published a budget at all.
#[test]
fn monitor_keeps_an_aged_budget_record_and_marks_it_stale() {
    let status = serde_json::from_value::<Map<String, Value>>(serde_json::json!({
        "gpu_budget": {
            "schema_version": 1,
            "adapter": { "backend": "cuda", "key": "gpu-0", "luid": null },
            "total_bytes": 6_442_450_944u64,
            "budget_bytes": 6_442_450_944u64,
            "used_bytes": 2_147_483_648u64,
            "available_bytes": 4_294_967_296u64,
            "source": "driver_reported",
            "sampled_at_unix_ms": 1000
        }
    }))
    .expect("valid fixture status");

    // Well past `GPU_BUDGET_MAX_AGE_MS` (5_000): the record survives.
    let aged_at = 1000 + GPU_BUDGET_MAX_AGE_MS + 1;
    let sample = gpu_observation_from_status(&status).expect("aged budget is still a record");
    assert_eq!(sample.sampled_at_unix_ms, 1000);
    assert_eq!(sample.budget_bytes, 6_442_450_944);
    assert_eq!(sample.used_bytes, 2_147_483_648);
    assert_eq!(sample.free_bytes, 4_294_967_296);

    // The level keeps its last-known value and is marked `stale`, never
    // rewritten into the absence wording.
    let stale_row = gpu_budget_row(Some(&sample), aged_at);
    assert!(
        stale_row.contains("(stale)"),
        "an aged level must be marked, not hidden: {stale_row}"
    );
    assert!(
        !stale_row.contains("unavailable"),
        "an aged reading is not an absence: {stale_row}"
    );
    assert!(
        stale_row.contains("gpu-0"),
        "the level keeps its last-known value: {stale_row}"
    );
    assert!(
        stale_row.contains("2048 / 6144 MiB"),
        "the occupancy figures stay on screen: {stale_row}"
    );

    // Inside the window the same record is current and unmarked.
    let fresh_row = gpu_budget_row(Some(&sample), 1000 + GPU_BUDGET_MAX_AGE_MS);
    assert!(
        !fresh_row.contains("stale"),
        "a current level carries no stale mark: {fresh_row}"
    );
    assert!(fresh_row.contains("2048 / 6144 MiB"));

    // Absence is a different fact and keeps its own wording.
    assert_eq!(
        gpu_budget_row(None, aged_at),
        " Active worker GPU budget unavailable"
    );
}

/// A present sub-MiB reading must never publish as a measured zero
/// (`e60859c2` discipline). Floor-dividing at construction is exactly how
/// a real 512 KiB budget became a `0 MiB` bar with a denominator of zero.
#[test]
fn gpu_budget_sub_mib_reading_never_publishes_as_a_measured_zero() {
    let status = serde_json::from_value::<Map<String, Value>>(serde_json::json!({
        "gpu_budget": {
            "schema_version": 1,
            "adapter": { "backend": "cuda", "key": "gpu-0", "luid": null },
            "total_bytes": 8 * 1024 * 1024u64,
            "budget_bytes": 512 * 1024u64,
            "used_bytes": 256 * 1024u64,
            "available_bytes": 256 * 1024u64,
            "source": "driver_reported",
            "sampled_at_unix_ms": 1000
        }
    }))
    .expect("valid fixture status");

    let sample = gpu_observation_from_status(&status).expect("sub-MiB budget is a real sample");
    // The record keeps the exact figures. The hold and capacity faces
    // would each be a measured zero under a floor division, so they
    // ceiling. The free figure is an allocation bound and floors instead:
    // 256 KiB is not 1 MiB to take.
    assert_eq!(sample.budget_bytes, 524_288);
    assert_eq!(sample.used_bytes, 262_144);
    assert_eq!(sample.free_bytes, 262_144);
    assert_eq!(sample.budget_mib(), 1);
    assert_eq!(sample.used_mib(), 1);
    assert_eq!(sample.free_mib(), 0);
    assert_eq!(sample.total_mib(), Some(8));

    // And the display shows the same, with no zero denominator. The bound
    // publishes its third state rather than rounding up to `1 MiB` —
    // which would grant four times the remainder — or down to `0 MiB`,
    // which would say the budget is exhausted when it is not.
    let row = gpu_budget_row(Some(&sample), 1000);
    assert!(
        row.contains("1 / 1 MiB"),
        "a present sub-MiB reading publishes as non-zero: {row}"
    );
    assert!(
        row.contains(" Free:           <1 MiB within usable"),
        "a sub-MiB remainder is a bound below one MiB, not one MiB: {row}"
    );
    assert!(
        !row.contains("0 / 0"),
        "a non-zero measurement must never publish as zero: {row}"
    );
    assert!(
        !row.contains(" Available:      0 MiB"),
        "a sub-MiB remainder is not an exhausted budget: {row}"
    );
}

/// The mirror of the rule above: a **measured** zero is still a zero.
/// `div_ceil` must not inflate an idle or a fully-occupied budget just to
/// avoid ever printing `0`.
#[test]
fn gpu_budget_measured_zero_still_publishes_as_zero() {
    let mut status = serde_json::from_value::<Map<String, Value>>(serde_json::json!({
        "gpu_budget": {
            "schema_version": 1,
            "adapter": { "backend": "cuda", "key": "gpu-0", "luid": null },
            "total_bytes": 2048 * 1024 * 1024u64,
            "budget_bytes": 2048 * 1024 * 1024u64,
            "used_bytes": 0u64,
            "available_bytes": 2048 * 1024 * 1024u64,
            "source": "driver_reported",
            "sampled_at_unix_ms": 1000
        }
    }))
    .expect("valid fixture status");

    // Idle: nothing is drawn against the budget. `0` is what the driver
    // reported, not an absence.
    let idle = gpu_observation_from_status(&status).expect("idle budget is a real sample");
    assert_eq!(idle.used_bytes, 0);
    assert_eq!(idle.used_mib(), 0);
    assert_eq!(idle.used_percent_of_budget(), Some(0));
    let idle_row = gpu_budget_row(Some(&idle), 1000);
    assert!(
        idle_row.contains("0 / 2048 MiB"),
        "a measured idle still publishes as zero: {idle_row}"
    );

    // Fully occupied: headroom is a measured zero on the other side.
    status["gpu_budget"]["used_bytes"] = serde_json::json!(2048 * 1024 * 1024u64);
    status["gpu_budget"]["available_bytes"] = serde_json::json!(0u64);
    let full = gpu_observation_from_status(&status).expect("full budget is a real sample");
    assert_eq!(full.free_bytes, 0);
    assert_eq!(full.free_mib(), 0);
    assert_eq!(full.used_percent_of_budget(), Some(100));
    let full_row = gpu_budget_row(Some(&full), 1000);
    assert!(
        full_row.contains("2048 / 2048 MiB"),
        "a fully occupied budget keeps its figures: {full_row}"
    );
    assert!(
        full_row.contains(" Free:           0 MiB within usable"),
        "a measured zero headroom publishes as zero: {full_row}"
    );
}

/// A **measured** zero budget is a record, not an absence. The driver
/// spoke: it reported no allocatable capacity. Publishing that as the
/// panel's absence wording told an operator the driver never published a
/// budget at all, which is the same conflation the aged-budget rule
/// refuses in the other direction.
///
/// What such a sample cannot carry is an occupancy *ratio*. That refusal
/// is named here rather than turned into a `0%`, which would claim a
/// fullness measured against a denominator of zero.
#[test]
fn monitor_keeps_a_measured_zero_budget_and_invents_no_percent() {
    let mut status = serde_json::from_value::<Map<String, Value>>(serde_json::json!({
        "gpu_budget": {
            "schema_version": 1,
            "adapter": { "backend": "cuda", "key": "gpu-0", "luid": null },
            "total_bytes": 8 * 1024 * 1024 * 1024u64,
            "budget_bytes": 0u64,
            "used_bytes": 0u64,
            "available_bytes": 0u64,
            "source": "driver_reported",
            "sampled_at_unix_ms": 1000
        }
    }))
    .expect("valid fixture status");

    let sample =
        gpu_observation_from_status(&status).expect("a measured zero budget is a real sample");
    assert_eq!(sample.budget_bytes, 0);
    assert_eq!(sample.used_bytes, 0);
    assert_eq!(sample.free_bytes, 0);
    assert_eq!(sample.budget_mib(), 0);
    assert_eq!(sample.used_mib(), 0);
    assert_eq!(sample.free_mib(), 0);
    // A zero budget does not erase the capacity the driver did report.
    assert_eq!(sample.total_mib(), Some(8192));
    // No denominator, so no occupancy ratio exists for this sample.
    assert_eq!(sample.used_percent_of_budget(), None);

    let row = gpu_budget_row(Some(&sample), 1000);
    assert!(
        row.contains("0 / 0 MiB"),
        "the measured figures publish as measured zeros: {row}"
    );
    assert!(
        row.contains(" Free:           0 MiB within usable"),
        "headroom publishes as a measured zero too: {row}"
    );
    assert!(
        row.contains("no denominator"),
        "the missing ratio is named, not invented: {row}"
    );
    assert!(
        !row.contains('%'),
        "a zero budget carries no occupancy percent: {row}"
    );
    assert!(
        !row.contains("unavailable"),
        "a measured zero is not an absence: {row}"
    );
    assert!(
        row.contains("8192 MiB"),
        "a zero budget does not erase the adapter's capacity: {row}"
    );

    // The same rule for the other half of the old refusal: a zero
    // *physical* total is a figure the driver published, and the row
    // keeps it as a measured zero rather than dropping the sample.
    status["gpu_budget"]["total_bytes"] = serde_json::json!(0u64);
    let no_total =
        gpu_observation_from_status(&status).expect("a zero physical total is still a real sample");
    assert_eq!(no_total.total_bytes, Some(0));
    assert_eq!(no_total.total_mib(), Some(0));
    let no_total_row = gpu_budget_row(Some(&no_total), 1000);
    assert!(
        no_total_row.contains(" Total:          0 MiB"),
        "a measured zero capacity publishes as zero: {no_total_row}"
    );
}

/// The occupancy percent is the ratio the driver reported, taken on the
/// exact bytes. Neither the floor nor the ceiling whole-MiB view of these
/// figures gives that ratio, which is why the percent is computed before
/// the publish conversion and never from it.
#[test]
fn gpu_budget_occupancy_percent_is_the_exact_byte_ratio() {
    // 1536 KiB of a 2048 KiB budget is 75%. Floor-MiB halves say 1 / 2 =
    // 50%; ceiling-MiB halves say 2 / 2 = 100%. Both are wrong.
    let status = serde_json::from_value::<Map<String, Value>>(serde_json::json!({
        "gpu_budget": {
            "schema_version": 1,
            "adapter": { "backend": "cuda", "key": "gpu-0", "luid": null },
            "total_bytes": 4096 * 1024 * 1024u64,
            "budget_bytes": 2048 * 1024u64,
            "used_bytes": 1536 * 1024u64,
            "available_bytes": 512 * 1024u64,
            "source": "driver_reported",
            "sampled_at_unix_ms": 1000
        }
    }))
    .expect("valid fixture status");

    let sample = gpu_observation_from_status(&status).expect("odd-MiB budget is a real sample");
    assert_eq!(sample.used_percent_of_budget(), Some(75));

    // The publish views are exactly the two figures that would each have
    // moved the number: they are views, never inputs to the percent.
    assert_eq!(sample.used_mib(), 2);
    assert_eq!(sample.budget_mib(), 2);
    assert_eq!(sample.used_bytes / 1_048_576, 1);
    assert_eq!(sample.budget_bytes / 1_048_576, 2);

    let row = gpu_budget_row(Some(&sample), 1000);
    assert!(
        row.contains("75%"),
        "the row shows the exact occupancy, not a rounded one: {row}"
    );
}

/// A present hold that rounds down to zero cells at the bar width still
/// draws one cell and still prints `<1%`. The inline bar this replaces
/// published both as if the budget were empty.
#[test]
fn gpu_budget_row_keeps_a_sub_1_percent_hold_visible() {
    let status = serde_json::from_value::<Map<String, Value>>(serde_json::json!({
        "gpu_budget": {
            "schema_version": 1,
            "adapter": { "backend": "cuda", "key": "gpu-0", "luid": null },
            "total_bytes": 16 * 1024 * 1024 * 1024u64,
            "budget_bytes": 10 * 1024 * 1024 * 1024u64,
            "used_bytes": 1u64,
            "available_bytes": 10 * 1024 * 1024 * 1024u64 - 1,
            "source": "driver_reported",
            "sampled_at_unix_ms": 1000
        }
    }))
    .expect("valid fixture status");

    let sample = gpu_observation_from_status(&status).expect("a one-byte hold is a real sample");
    assert_eq!(sample.used_percent_of_budget(), Some(0));

    let row = gpu_budget_row(Some(&sample), 1000);
    assert!(
        row.contains("<1%"),
        "a non-zero hold must not publish as a measured 0%: {row}"
    );
    assert!(
        row.contains(&format!("[{}{}]", "█", "░".repeat(19))),
        "a non-zero hold must not publish as an empty bar: {row}"
    );
    assert!(
        !row.contains(" 0%"),
        "the measured-empty face is reserved for a hold of zero: {row}"
    );
}
