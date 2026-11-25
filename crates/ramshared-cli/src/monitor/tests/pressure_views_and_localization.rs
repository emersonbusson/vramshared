// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/tests/pressure_views_and_localization.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;

/// The same rule one notch up: a hold large enough to print a number but
/// too small to fill a cell at the bar width. The percent is honest
/// already; the bar was the half that lied.
#[test]
fn gpu_budget_row_draws_one_cell_for_a_hold_that_rounds_down() {
    let status = serde_json::from_value::<Map<String, Value>>(serde_json::json!({
        "gpu_budget": {
            "schema_version": 1,
            "adapter": { "backend": "cuda", "key": "gpu-0", "luid": null },
            "total_bytes": 64 * 1024 * 1024 * 1024u64,
            "budget_bytes": 64 * 1024 * 1024 * 1024u64,
            "used_bytes": 2 * 1024 * 1024 * 1024u64,
            "available_bytes": 62 * 1024 * 1024 * 1024u64,
            "source": "driver_reported",
            "sampled_at_unix_ms": 1000
        }
    }))
    .expect("valid fixture status");

    let sample = gpu_observation_from_status(&status).expect("a 2 GiB hold is a real sample");
    assert_eq!(sample.used_percent_of_budget(), Some(3));

    let row = gpu_budget_row(Some(&sample), 1000);
    assert!(row.contains(" 3%"), "the percent is the exact ratio: {row}");
    assert!(
        row.contains(&format!("[{}{}]", "█", "░".repeat(19))),
        "3% of 20 cells rounds to none, and a non-zero hold still draws one: {row}"
    );
}

/// The three faces of a free figure. The number floors because it is an
/// allocation bound; the third state is what keeps a floored-to-zero
/// remainder from reading as an exhausted budget.
#[test]
fn free_bound_label_keeps_the_bound_below_the_exact_remainder() {
    assert_eq!(free_bound_label(256 * 1024, 0), "<1 MiB");
    assert_eq!(free_bound_label(0, 0), "0 MiB");
    assert_eq!(free_bound_label(1024 * 1024, 1), "1 MiB");
    // A fractional remainder above one MiB publishes the floor, never the
    // ceil: 1 MiB plus a byte is one MiB to take, not two.
    assert_eq!(free_bound_label(1024 * 1024 + 1, 1), "1 MiB");
}

/// The speedup row is a ratio against the fallback disk tier measured in
/// the same observation. No constant baseline exists, and the fallback
/// tier never reports a multiple of itself.
#[test]
fn computes_tier_speedup_against_the_measured_fallback_baseline() {
    let idle_io = TierIoStats::default();
    let fallback = TierIoStats {
        min_mbs: Some(15.0),
        avg_mbs: Some(20.0),
        max_mbs: Some(120.0),
        ..TierIoStats::default()
    };
    assert_eq!(
        compute_tier_speedup(&idle_io, cascade::SwapTierRole::Zram, &fallback),
        "Awaiting measured I/O"
    );
    assert_eq!(
        compute_tier_speedup(&idle_io, cascade::SwapTierRole::Vram, &fallback),
        "Awaiting measured I/O"
    );
    assert_eq!(
        compute_tier_speedup(&idle_io, cascade::SwapTierRole::Fallback, &fallback),
        "Awaiting measured I/O"
    );

    // Denominator is the fallback's average throughput: 20 MiB/s in this
    // fixture, so 100/460/860 measure 5.0x / 23.0x / 43.0x.
    let zram_active = TierIoStats {
        min_mbs: Some(100.0),
        avg_mbs: Some(460.0),
        max_mbs: Some(860.0),
        ..TierIoStats::default()
    };
    let z_txt = compute_tier_speedup(&zram_active, cascade::SwapTierRole::Zram, &fallback);
    assert!(
        z_txt.contains("Min: 5.0x") && z_txt.contains("Avg: 23.0x") && z_txt.contains("Max: 43.0x"),
        "zram ratios must come from the measured baseline: {z_txt}"
    );
    assert!(
        z_txt.contains("vs fallback disk"),
        "the comparison must name its baseline: {z_txt}"
    );

    let vram_active = TierIoStats {
        min_mbs: Some(40.0),
        avg_mbs: Some(200.0),
        max_mbs: Some(600.0),
        ..TierIoStats::default()
    };
    let v_txt = compute_tier_speedup(&vram_active, cascade::SwapTierRole::Vram, &fallback);
    assert!(
        v_txt.contains("Min: 2.0x") && v_txt.contains("Avg: 10.0x") && v_txt.contains("Max: 30.0x"),
        "vram ratios must come from the measured baseline: {v_txt}"
    );

    // A slow tier is measured, not awaiting: 2 MiB/s over a 20 MiB/s
    // baseline is a real 0.1x, and the old `>= 5.0` floor used to hide it
    // behind a string that claimed nothing had been sampled. A ratio of
    // 0.05 rounds below the display resolution and prints `<0.1x`, never
    // a measured `0.0x`.
    let slow = TierIoStats {
        min_mbs: Some(1.0),
        avg_mbs: Some(2.0),
        max_mbs: Some(3.0),
        ..TierIoStats::default()
    };
    let slow_txt = compute_tier_speedup(&slow, cascade::SwapTierRole::Zram, &fallback);
    assert!(
        slow_txt.contains("Min: <0.1x") && slow_txt.contains("Avg: 0.1x"),
        "a measured slow tier must report its ratio, not claim it is unmeasured: {slow_txt}"
    );
    assert!(
        !slow_txt.contains("Awaiting measured I/O"),
        "a tier with measured throughput is not awaiting anything: {slow_txt}"
    );

    // The fallback tier is the baseline: measured throughput, never a
    // fabricated `1.0x` multiple of itself.
    let d_txt = compute_tier_speedup(&fallback, cascade::SwapTierRole::Fallback, &fallback);
    assert!(
        d_txt.contains("baseline 15..20..120 MiB/s"),
        "the fallback row reports its measured throughput: {d_txt}"
    );
    assert!(
        !d_txt.contains("1.0x"),
        "the baseline must never claim a multiple of itself: {d_txt}"
    );
}

/// Without a measured fallback there is no denominator, so there is no
/// ratio — "not measured", never a number.
#[test]
fn tier_speedup_without_a_measured_fallback_is_not_measured() {
    let zram_active = TierIoStats {
        min_mbs: Some(100.0),
        avg_mbs: Some(460.0),
        max_mbs: Some(860.0),
        ..TierIoStats::default()
    };
    let unmeasured_fallback = TierIoStats::default();
    assert_eq!(
        compute_tier_speedup(
            &zram_active,
            cascade::SwapTierRole::Zram,
            &unmeasured_fallback
        ),
        "not measured (no fallback disk baseline)"
    );

    let quiet_fallback = TierIoStats {
        avg_mbs: Some(MIN_BASELINE_TIER_MBS - 0.5),
        max_mbs: Some(MIN_BASELINE_TIER_MBS - 0.5),
        ..TierIoStats::default()
    };
    assert_eq!(
        compute_tier_speedup(&zram_active, cascade::SwapTierRole::Zram, &quiet_fallback),
        "not measured (no fallback disk baseline)"
    );
}

/// A non-zero measurement never publishes as the zero its display face's
/// rounding would print. Each face keeps its own zero arm — `n/a` for a
/// ratio that does not apply, `0` / `0.0` / `0.00` for a measured zero,
/// `not measured` for an estimate the model never produced — and a real
/// figure below that face's resolution prints `<` the smallest token the
/// face can show instead.
#[test]
fn a_non_zero_measurement_never_publishes_as_a_display_zero() {
    // Ratio, one decimal. A non-applicable ratio is not a measured zero.
    assert_eq!(speedup_label(0.0), "n/a");
    assert_eq!(speedup_label(-1.0), "n/a");
    assert_eq!(speedup_label(0.04), "<0.1x");
    assert_eq!(speedup_label(0.3), "0.3x");
    assert_eq!(speedup_label(23.0), "23.0x");

    // MiB/s, whole. A measured idle is `0`; a real rate under 1 MiB/s is
    // not, and it is not `PAGING_MIN_MBS` wearing another name either.
    assert_eq!(display_mbs(0.0), "0");
    assert_eq!(display_mbs(0.4), "<1");
    assert_eq!(display_mbs(85.0), "85");

    // MiB/s, one decimal — the rate cell and both peak faces. The rate
    // cell's own absence arm is separate and stays `n/a`.
    assert_eq!(display_mbs_tenths(0.0), "0.0");
    assert_eq!(display_mbs_tenths(0.04), "<0.1");
    assert_eq!(display_mbs_tenths(0.3), "0.3");
    assert_eq!(display_mbs_tenths(85.0), "85.0");
    assert_eq!(format_rate_cell(Some(0.0)), "0.0");
    assert_eq!(format_rate_cell(Some(0.04)), "<0.1");
    assert_eq!(format_rate_cell(None), "n/a");

    // Milliseconds, whole. A `0` duration is a measured zero; a duration
    // under 1 ms is not one.
    assert_eq!(display_ms(0.0), "0");
    assert_eq!(display_ms(0.4), "<1");
    assert_eq!(display_ms(2890.0), "2890");

    // Milliseconds, hundredths — the p50 and p99 faces. A p99 that never
    // saw a fault is a measured `0.00`; a p99 under the display
    // resolution is not.
    assert_eq!(display_ms_hundredths(0.0), "0.00");
    assert_eq!(display_ms_hundredths(0.004), "<0.01");
    assert_eq!(display_ms_hundredths(0.3), "0.30");
    assert_eq!(display_ms_hundredths(2.5), "2.50");

    // GB/s, hundredths.
    assert_eq!(display_gbs(0.0), "0.00");
    assert_eq!(display_gbs(0.004), "<0.01");
    assert_eq!(display_gbs(0.75), "0.75");
    assert_eq!(display_gbs(2.72), "2.72");

    // Seconds, hundredths — the unit-activation face on the boot row.
    assert_eq!(display_seconds_hundredths(0.0), "0.00");
    assert_eq!(display_seconds_hundredths(0.004), "<0.01");
    assert_eq!(display_seconds_hundredths(0.01), "0.01");
    assert_eq!(display_seconds_hundredths(2.89), "2.89");

    // Microsecond estimate. Zero is an estimate the model never produced,
    // not a measured zero, so its face is the absence face.
    assert_eq!(display_pf_lat(0.0), "not measured");
    assert_eq!(display_pf_lat(-1.0), "not measured");
    assert_eq!(display_pf_lat(0.004), "estimated <0.01 µs");
    assert_eq!(display_pf_lat(12.0), "estimated 12.00 µs");
}

#[test]
fn dashboard_renders_cleanly_across_multiple_terminal_resolutions() {
    let mut sample = observation(true, true);
    sample.errors = vec!["gpu_dropped".to_string()];
    sample.control_plane.boot_tier_latency_ms = Some(2890);
    let resolutions = [(80, 24), (100, 30), (140, 40), (200, 50), (240, 60)];

    for (w, h) in resolutions {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_dashboard(frame, &sample, &DashHistory::default()))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            rendered.contains("Host RAM") || rendered.contains("RAM"),
            "Failed at {w}x{h}"
        );
        assert!(
            rendered.contains("Memory Tiers") || rendered.contains("Swap"),
            "Failed at {w}x{h}"
        );
    }
}

#[test]
fn edge_case_draw_and_pct_helpers() {
    assert_eq!(memory_used_pct(&MemoryObservation::default()), None);
    assert_eq!(make_bar(0, 10), "[░░░░░░░░░░]");
    assert_eq!(make_bar(100, 10), "[██████████]");

    let mut sample_no_gpu = observation(false, false);
    sample_no_gpu.gpu = None;
    sample_no_gpu.status.remove("tiers");

    let backend = ratatui::backend::TestBackend::new(120, 30);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| draw_dashboard(frame, &sample_no_gpu, &DashHistory::default()))
        .unwrap();
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Active worker GPU budget unavailable"));
    assert!(rendered.contains("Swap Tiers: not available"));
}

#[test]
fn tier_latency_estimates_are_identified_in_ui_data() {
    let mut control_plane = ControlPlaneObservation::default();
    update_tier_latencies(&mut control_plane, 0, 0, 0);

    let rendered = format_tier_latency(&control_plane.disk_io, "disk");
    assert!(
        rendered.starts_with("estimated:"),
        "derived latency was not identified as an estimate: {rendered}"
    );

    let serialized = serde_json::to_value(&control_plane).unwrap();
    assert_eq!(serialized["disk_io"]["latency_source"], "estimated");

    let mut legacy_json = serde_json::to_value(TierIoStats::default()).unwrap();
    let legacy_object = legacy_json.as_object_mut().unwrap();
    legacy_object.remove("latency_source");
    legacy_object.insert("min_lat_us".to_string(), 85.0.into());
    legacy_object.insert("avg_lat_us".to_string(), 180.0.into());
    legacy_object.insert("max_lat_us".to_string(), 1200.0.into());
    let legacy = serde_json::from_value::<TierIoStats>(legacy_json).unwrap();
    assert_eq!(legacy.latency_source, LatencySource::Unavailable);
    assert_eq!(
        format_tier_latency(&legacy, "legacy disk"),
        "not measured (legacy disk)"
    );
}

#[test]
fn parses_diskstats_and_startup_ms() {
    // `fixture0` stands in for whatever block device `/proc/swaps` puts in
    // the fallback role. No host device name is a product constant.
    let fallback: std::collections::BTreeSet<String> =
        ["fixture0".to_string()].into_iter().collect();
    let stats = " 252       0 zram0 10 0 200 0 20 0 400 0 0 0 0\n  43       0 nbd0 5 0 100 0 15 0 300 0 0 0 0\n   8      32 fixture0 2 0 40 0 4 0 80 0 0 0 0\n";
    let (tot_r, tot_w) = parse_swap_diskstats(stats, &fallback, &std::collections::BTreeMap::new());
    assert_eq!(tot_r, (200 + 100 + 40) * 512);
    assert_eq!(tot_w, (400 + 300 + 80) * 512);

    let (z, v, d) = parse_per_tier_diskstats(stats, &fallback, &std::collections::BTreeMap::new());
    assert_eq!(z.read_bytes, 200 * 512);
    assert_eq!(z.write_bytes, 400 * 512);
    assert_eq!(v.read_bytes, 100 * 512);
    assert_eq!(v.write_bytes, 300 * 512);
    assert_eq!(d.read_bytes, 40 * 512);
    assert_eq!(d.write_bytes, 80 * 512);

    let show_out =
        "InactiveExitTimestampMonotonic=1000000\nActiveEnterTimestampMonotonic=3890000\n";
    assert_eq!(parse_unit_startup_ms(show_out), Some(2890));
    assert_eq!(parse_unit_startup_ms(""), None);

    assert_eq!(parse_uptime_seconds("1540.25 3080.50"), Some(1540));
    assert_eq!(parse_uptime_seconds("invalid"), None);

    assert_eq!(sanitize_label("my-app_1.0@daemon!", 10), "my-app_1.0");
    assert_eq!(
        sanitize_cgroup("/system.slice/test.service", 20),
        "/system.slice/test.s"
    );
    let temp_empty = std::env::temp_dir().join(format!("test-scopes-{}", std::process::id()));
    let _ = fs::create_dir_all(&temp_empty);
    assert_eq!(count_scope_dirs(&temp_empty), 0);
    assert_eq!(read_reservation_totals(&temp_empty), (0, 0));
    let _ = fs::remove_dir_all(&temp_empty);

    let io_sample = TierIoStats {
        min_lat_us: 0.04,
        avg_lat_us: 0.08,
        max_lat_us: 0.15,
        latency_source: LatencySource::Measured,
        ..TierIoStats::default()
    };
    let lat_str = format_tier_latency(&io_sample, "In-RAM LZ4");
    assert_eq!(lat_str, "measured: 0.04µs..0.08µs..0.15µs (In-RAM LZ4)");

    let io_disk = TierIoStats {
        min_lat_us: 85.0,
        avg_lat_us: 180.0,
        max_lat_us: 1200.0,
        latency_source: LatencySource::Measured,
        ..TierIoStats::default()
    };
    let disk_lat_str = format_tier_latency(&io_disk, "Host VHDX");
    assert_eq!(
        disk_lat_str,
        "measured: 85.00µs..180.00µs..1.2ms (Host VHDX)"
    );
    assert_eq!(
        format_tier_latency(&TierIoStats::default(), "GPU cache"),
        "not measured (GPU cache)"
    );

    let mem_txt = "MemTotal:       20480 kB\nMemAvailable:   16384 kB\nSwapTotal:       4096 kB\nSwapFree:        2048 kB\n";
    let mem = parse_meminfo(mem_txt);
    assert_eq!(mem.total_kib, Some(20480));
    assert_eq!(mem.available_kib, Some(16384));
    assert_eq!(mem.swap_total_kib, Some(4096));
    assert_eq!(mem.swap_free_kib, Some(2048));

    let temp_log_dir = std::env::temp_dir().join(format!("test-mon-log-{}", std::process::id()));
    let log_file = temp_log_dir.join("test.log");
    let hb_file = temp_log_dir.join("test.hb");
    assert!(append_rotating(&log_file, "line1", 10).is_ok());
    assert!(append_rotating(&log_file, "line2_long_string_to_rotate", 10).is_ok());
    assert!(write_atomic(&hb_file, "heartbeat_data").is_ok());
    let _ = fs::remove_dir_all(&temp_log_dir);

    let compact_opts = MonitorOptions {
        compact: true,
        once: true,
        interval_ms: 100,
        history_seconds: 30,
        jsonl: false,
        output: None,
        heartbeat: None,
        benchmark_evidence: None,
        language: Language::En,
    };
    assert!(run_compact(&compact_opts).is_ok());

    let jsonl_opts = MonitorOptions {
        compact: false,
        once: true,
        interval_ms: 100,
        history_seconds: 30,
        jsonl: true,
        output: None,
        heartbeat: None,
        benchmark_evidence: None,
        language: Language::En,
    };
    assert!(run_jsonl(&jsonl_opts).is_ok());
}

#[test]
fn diskstats_tier3_is_membership_in_the_fallback_set_not_exclusion() {
    // A disk that is not in the fallback set is not tier 3, whatever else
    // it is. Classifying diskstats by exclusion would attribute the root
    // filesystem's disk to the fallback swap tier.
    let fallback: std::collections::BTreeSet<String> =
        ["fixture0".to_string()].into_iter().collect();
    let stats = "\
 252       0 zram0 10 0 200 0 20 0 400 0 0 0 0
  43       0 nbd0 5 0 100 0 15 0 300 0 0 0 0
   8      32 fixture0 2 0 40 0 4 0 80 0 0 0 0
   8      48 fixtureroot 4 0 4000 0 8 0 8000 0 0 0 0
   8      64 fixturedata 4 0 4000 0 8 0 8000 0 0 0 0
";
    let (tot_r, tot_w) = parse_swap_diskstats(stats, &fallback, &std::collections::BTreeMap::new());
    assert_eq!(
        tot_r,
        (200 + 100 + 40) * 512,
        "non-fallback disks must not join the swap-tier total"
    );
    assert_eq!(tot_w, (400 + 300 + 80) * 512);

    let (z, v, d) = parse_per_tier_diskstats(stats, &fallback, &std::collections::BTreeMap::new());
    assert_eq!(z.read_bytes, 200 * 512);
    assert_eq!(v.read_bytes, 100 * 512);
    assert_eq!(
        d.read_bytes,
        40 * 512,
        "only the role-derived fallback device is tier 3"
    );
    assert_eq!(d.write_bytes, 80 * 512);
}

#[test]
fn diskstats_with_empty_fallback_set_omits_tier3_rather_than_guessing() {
    // No swap table (or one that failed to parse) means no tier-3 identity.
    // The honest result is zero tier-3 I/O, not an inferred device.
    let stats = "\
 252       0 zram0 10 0 200 0 20 0 400 0 0 0 0
   8      32 fixture0 2 0 40 0 4 0 80 0 0 0 0
";
    let empty = std::collections::BTreeSet::new();
    let (tot_r, tot_w) = parse_swap_diskstats(stats, &empty, &std::collections::BTreeMap::new());
    assert_eq!(
        tot_r,
        200 * 512,
        "managed tiers still count without a swap table"
    );
    assert_eq!(tot_w, 400 * 512);

    let (z, v, d) = parse_per_tier_diskstats(stats, &empty, &std::collections::BTreeMap::new());
    assert_eq!(z.read_bytes, 200 * 512);
    assert_eq!(v.read_bytes, 0);
    assert_eq!(
        d.read_bytes + d.write_bytes,
        0,
        "an unidentified disk is never claimed as tier 3"
    );
}

#[test]
fn diskstats_fallback_membership_is_case_insensitive_on_the_kernel_name() {
    let fallback: std::collections::BTreeSet<String> =
        ["fixture0".to_string()].into_iter().collect();
    let stats = "   8      32 Fixture0 2 0 40 0 4 0 80 0 0 0 0\n";
    let (tot_r, tot_w) = parse_swap_diskstats(stats, &fallback, &std::collections::BTreeMap::new());
    assert_eq!(tot_r, 40 * 512);
    assert_eq!(tot_w, 80 * 512);
}

#[test]
fn diskstats_dm_row_is_classified_by_its_alias_not_its_kernel_name() {
    // `/proc/diskstats` names a device-mapper target `dm-N`. Without the
    // alias join the row is an unknown disk and its I/O is dropped.
    let mut aliases = std::collections::BTreeMap::new();
    aliases.insert("dm-3".to_string(), "ramshared0".to_string());
    aliases.insert("dm-7".to_string(), "fixture0".to_string());
    let fallback: std::collections::BTreeSet<String> =
        ["fixture0".to_string()].into_iter().collect();
    let stats = "\
253   3 dm-3 1 0 2 0 3 0 4 0 0 0 0 0
253   7 dm-7 5 0 6 0 7 0 8 0 0 0 0 0
";
    // dm-3 aliases to a tier-2 logical name; dm-7 aliases to the fallback.
    let (z, v, d) = parse_per_tier_diskstats(stats, &fallback, &aliases);
    assert_eq!(z.read_bytes + z.write_bytes, 0);
    assert_eq!(
        (v.read_bytes, v.write_bytes),
        (2 * 512, 4 * 512),
        "a dm row whose alias is the logical device is Tier 2"
    );
    assert_eq!(
        (d.read_bytes, d.write_bytes),
        (6 * 512, 8 * 512),
        "a dm row whose alias is the fallback swap is Tier 3"
    );

    let (tot_r, tot_w) = parse_swap_diskstats(stats, &fallback, &aliases);
    assert_eq!(tot_r, (2 + 6) * 512);
    assert_eq!(tot_w, (4 + 8) * 512);
}

#[test]
fn diskstats_lookalike_names_do_not_flip_a_tier() {
    // Negative: a name that merely *holds* a managed-tier token is not
    // that tier. Prefix matching classified `nbdnotes` as Tier 2 and
    // `zrambackup` as Tier 1.
    let mut aliases = std::collections::BTreeMap::new();
    aliases.insert("dm-9".to_string(), "fixturecache".to_string());
    let empty: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let stats = "\
  43   0 nbdnotes 1 0 2 0 3 0 4 0 0 0 0 0
 252   0 zrambackup 1 0 2 0 3 0 4 0 0 0 0 0
253   9 dm-9 1 0 2 0 3 0 4 0 0 0 0 0
";
    let (z, v, d) = parse_per_tier_diskstats(stats, &empty, &aliases);
    assert_eq!(
        (z.read_bytes + z.write_bytes, v.read_bytes + v.write_bytes),
        (0, 0),
        "a lookalike spelling is never a managed tier"
    );
    assert_eq!(
        d.read_bytes + d.write_bytes,
        0,
        "and an unknown name is never Tier 3 either"
    );
}

/// The enforced reserve floor is labeled as VRAM capacity, not
/// read as a system-RAM threshold.
#[test]
fn monitor_labels_enforced_reserve_floor_as_vram() {
    let (enforced_mib, source, sealed_min_mib, percent, runtime_mib) =
        vram_reserve_fields(Some(6144 * 1024 * 1024), 4016 * 1024 * 1024);

    // The shared three-term formula, recomputed here independently.
    // WDDM shape: budget < total. `helper_capacity` takes the min.
    // Non-literal inputs so the min is not a compile-time no-op.
    //
    // The published face is `div_ceil` because the enforced floor is a
    // **reserve** floor — see `reserve_floor_mib`. On this fixture the
    // sealed 2048 MiB minimum dominates and the result happens to be whole
    // MiB, so the direction is not observable here;
    // `reserve_floor_face_ceils_a_fractional_mib_floor` pins it.
    let total_bytes = 6144 * 1024 * 1024u64;
    let budget_bytes = 4016 * 1024 * 1024u64;
    let capacity = total_bytes.min(budget_bytes);
    assert_eq!(capacity, budget_bytes, "capacity is min(total, budget)");
    let configured = (2048 * 1024 * 1024u64).max(capacity * 20 / 100);
    let expected =
        (configured.max(capacity.div_ceil(5)) + runtime_mib * 1024 * 1024).div_ceil(1024 * 1024);
    assert_eq!(
        enforced_mib, expected,
        "enforced floor must match the shared formula"
    );
    assert!(enforced_mib > 0);

    // Source is honest: the monitor cannot observe a daemon override.
    assert_eq!(source, "sealed-manifest");
    assert_eq!(sealed_min_mib, 2048);
    assert_eq!(percent, 20);
    assert_eq!(runtime_mib, 256);

    // The label is `vram_reserve_*` on every field (DT-10). The driver
    // figures are bytes, as the telemetry carries them.
    let encoded = serde_json::to_value(GpuObservation {
        adapter: ramshared_vram::GpuAdapterIdentity {
            node_mask: None,
            backend: "cuda".into(),
            key: "test".into(),
            luid: None,
        },
        source: GpuBudgetSource::DriverReported,
        sampled_at_unix_ms: 1_000,
        total_bytes: Some(6144 * 1024 * 1024),
        budget_bytes: 4016 * 1024 * 1024,
        used_bytes: 2128 * 1024 * 1024,
        free_bytes: 1888 * 1024 * 1024,
        vram_reserve_enforced_mib: enforced_mib,
        vram_reserve_source: source,
        vram_reserve_sealed_min_mib: sealed_min_mib,
        vram_reserve_sealed_percent: percent,
        vram_reserve_runtime_buffer_mib: runtime_mib,
    })
    .expect("serializable");
    let map = encoded.as_object().expect("object");
    let reserve_keys: Vec<&str> = map
        .keys()
        .filter(|k| k.contains("reserve"))
        .map(String::as_str)
        .collect();
    assert!(
        !reserve_keys.is_empty(),
        "the observation must carry reserve fields"
    );
    for key in &reserve_keys {
        assert!(
            key.starts_with("vram_reserve_"),
            "reserve field {key} must be labeled as VRAM"
        );
    }
}

#[test]
fn the_per_tier_io_comes_from_the_device_counters_when_the_kernel_omits_the_device() {
    let root = std::env::temp_dir().join(format!("ramshared-dt4-stat-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let sys_block = root.join("sys/block");
    fs::create_dir_all(sys_block.join("zram6")).unwrap();
    fs::create_dir_all(sys_block.join("nbd0")).unwrap();
    // Kernel `/proc/diskstats` omits `zram6` completely and lists an outdated
    // `nbd0` row while `sdd` (a non-swap host volume) must stay ignored.
    fs::write(
        sys_block.join("zram6/stat"),
        "10 0 2000 1 20 0 4000 2 0 1 2 0 0 0 0 0 0\n",
    )
    .unwrap();
    // Device counters win over conflicting `/proc/diskstats` when both exist.
    fs::write(
        sys_block.join("nbd0/stat"),
        "5 0 300 1 8 0 700 2 0 1 2 0 0 0 0 0 0\n",
    )
    .unwrap();

    let entries = vec![
        crate::cascade::SwapEntry {
            filename: "/dev/zram6".to_string(),
            size_kb: 1024,
            used_kb: 512,
            priority: 200,
        },
        crate::cascade::SwapEntry {
            filename: "/dev/nbd0".to_string(),
            size_kb: 2048,
            used_kb: 256,
            priority: 100,
        },
        crate::cascade::SwapEntry {
            filename: "/dev/sdc".to_string(),
            size_kb: 4096,
            used_kb: 128,
            priority: -2,
        },
        // Missing device `zram9` with neither sysfs nor diskstats must stay absent (0, not fail).
        crate::cascade::SwapEntry {
            filename: "/dev/zram9".to_string(),
            size_kb: 512,
            used_kb: 0,
            priority: 150,
        },
    ];
    let fallback: std::collections::BTreeSet<String> = ["sdc".to_string()].into_iter().collect();
    // `nbd0` in diskstats has stale smaller values; `sdc` relies on diskstats cross-check.
    let diskstats = "\
  43   0 nbd0 1 0 10 0 1 0 20 0 0 0 0 0\n\
   8  32 sdc 2 0 50 0 4 0 90 0 0 0 0 0\n\
   8  48 sdd 9 0 9999 0 9 0 9999 0 0 0 0 0\n";

    let (zram, vram, disk, tot_r, tot_w, source) = collect_per_tier_io(
        &sys_block,
        &entries,
        diskstats,
        &fallback,
        &std::collections::BTreeMap::new(),
    );
    let _ = fs::remove_dir_all(&root);

    assert_eq!(zram.read_bytes, 2000 * 512);
    assert_eq!(zram.write_bytes, 4000 * 512);
    assert_eq!(vram.read_bytes, 300 * 512);
    assert_eq!(vram.write_bytes, 700 * 512);
    assert_eq!(disk.read_bytes, 50 * 512);
    assert_eq!(disk.write_bytes, 90 * 512);
    assert_eq!(
        tot_r,
        zram.read_bytes + vram.read_bytes + disk.read_bytes,
        "lifetime read_bytes comes from the same single source"
    );
    assert_eq!(
        tot_w,
        zram.write_bytes + vram.write_bytes + disk.write_bytes,
        "lifetime write_bytes comes from the same single source"
    );
    assert_eq!(source, Some("sysfs_stat+diskstats_crosscheck"));
}

#[test]
fn a_measured_zero_renders_as_a_present_zero() {
    assert_eq!(ssd_baseline_row(Some(0)), "at baseline");
    assert_eq!(ssd_baseline_row(Some(1_500)), "+1,500 KiB");
    assert_eq!(ssd_baseline_row(None), "not measured");
}
