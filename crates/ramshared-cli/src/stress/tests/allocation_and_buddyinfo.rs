// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/stress/tests/allocation_and_buddyinfo.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

#[test]
fn a_required_run_refuses_an_unreadable_psi_instead_of_inventing_one() {
    // The WSL2 fixture world requires PSI. Absence is a refusal to start,
    // not a green light at zero pressure.
    let log_path = std::env::temp_dir().join(format!(
        "ramshared-stress-psi-required-{}-{}.log",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let options = StressOptions {
        start_pct: 1,
        target_pct: 1,
        telemetry_log: log_path.display().to_string(),
        ..StressOptions::default()
    };
    let environment =
        StubStressEnvironment::new([BuddyinfoReading::Measured(64)]).with_unreadable_psi();

    let result = run_with_environment(&options, &environment);
    let _ = fs::remove_file(&log_path);

    let err = result.expect_err("a required run must refuse unreadable PSI");
    assert!(
        err.contains("PSI"),
        "the refusal must name the telemetry it is missing: {err}"
    );
}

#[test]
fn an_optional_run_publishes_unmeasured_psi_as_absence_not_as_zero() {
    // A platform that does not require PSI may run without it. When it
    // does, every published PSI figure is the absence it is. The fold
    // this pins was the run loop turning `Uninformed` into `Some(0.0)`,
    // which the telemetry log then printed as a measured idle.
    let log_path = std::env::temp_dir().join(format!(
        "ramshared-stress-psi-optional-{}-{}.log",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let options = StressOptions {
        start_pct: 1,
        target_pct: 1,
        hold_sec: 0,
        // Unreachable floor: the run records its read-only sample and
        // stops before allocating, which is enough to write telemetry.
        min_ram_mb: u64::MAX,
        telemetry_log: log_path.display().to_string(),
        ..StressOptions::default()
    };
    let environment = StubStressEnvironment::new([BuddyinfoReading::Measured(64)])
        .without_wsl2()
        .with_unreadable_psi();

    let result = run_with_environment(&options, &environment);
    let telemetry = fs::read_to_string(&log_path).unwrap_or_default();
    let _ = fs::remove_file(&log_path);

    assert!(
        result.is_ok(),
        "an optional run must not refuse on absent PSI: {result:?}"
    );
    assert!(
        telemetry.contains("PSI:"),
        "the run must still record its sample: {telemetry:?}"
    );
    assert!(
        telemetry.contains("n/a"),
        "absent PSI must be published as absence: {telemetry:?}"
    );
    assert!(
        !telemetry.contains("0.0%"),
        "absent PSI must never be published as a measured 0.0%: {telemetry:?}"
    );
}

#[test]
fn missing_min_free_sysctl_does_not_lower_the_physical_memory_floor() {
    // A missing or unparseable sysctl answers 0, which raises
    // `target - min_free` rather than lowering it. That is the
    // conservative direction and the claim this test makes.
    assert_eq!(parse_min_free_kbytes(None), 0);
    assert_eq!(parse_min_free_kbytes(Some("not-a-number")), 0);
    assert_eq!(
        required_memavailable_floor_kib(600 * KIB_PER_MIB, 0, 100 * KIB_PER_MIB),
        600 * KIB_PER_MIB
    );
    assert_eq!(
        required_memavailable_floor_kib(600 * KIB_PER_MIB, 64 * KIB_PER_MIB, 100 * KIB_PER_MIB),
        536 * KIB_PER_MIB
    );
}

#[test]
fn min_free_kbytes_is_held_in_the_unit_the_sysctl_publishes() {
    assert_eq!(parse_min_free_kbytes(Some("65536\n")), 65536);
    // The old fold floor-divided to MiB and read a real 1023 KiB
    // reservation as 0. That is the lie this names; a measured non-zero
    // is never published as zero.
    assert_eq!(parse_min_free_kbytes(Some("1023")), 1023);
    assert_ne!(parse_min_free_kbytes(Some("1023")), 0);
}

#[test]
fn mem_info_from_keeps_a_present_reading_in_the_unit_it_was_read() {
    // The old fold rounded at the fold with `(kib + 512) / 1024`, so a
    // present 511 KiB `MemAvailable` was already `0` by the time
    // anything compared it, and a 1537 KiB one was already `2`. The fold
    // now holds what `/proc/meminfo` said; the rounding is a publication
    // decision taken later, at [`published_mib`].
    let text = "\
MemTotal:       16384000 kB
MemFree:         1024000 kB
MemAvailable:        511 kB
";
    let reading = mem_info_from(text).expect("both keys are present");
    assert_eq!(reading.avail_kib, 511);
    let text = "MemTotal: 8388608 kB\nMemAvailable: 1537 kB\n";
    let reading = mem_info_from(text).expect("both keys are present");
    assert_eq!(reading.avail_kib, 1537);
    assert_eq!(reading.total_kib, 8388608);
    // A present zero is a measured zero and stays one.
    let text = "MemTotal: 8388608 kB\nMemAvailable: 0 kB\n";
    let reading = mem_info_from(text).expect("both keys are present");
    assert_eq!(reading.avail_kib, 0);
}

/// A document that does not carry a measurement of a figure has not
/// measured it. Folding the missing key into a `0` would publish a
/// measured empty figure for a reading the file never made — the same
/// defect as treating an unreadable file as a measured zero, one layer
/// later.
#[test]
fn mem_info_from_refuses_a_document_that_did_not_measure_both_figures() {
    assert_eq!(
        mem_info_from("MemAvailable: 511 kB\n"),
        None,
        "a total the document never published is not a total of zero"
    );
    assert_eq!(
        mem_info_from("MemTotal: 8388608 kB\n"),
        None,
        "an availability the document never published is not an availability of zero"
    );
    assert_eq!(
        mem_info_from("MemTotal: not-a-number\nMemAvailable: 511 kB\n"),
        None,
        "an unparseable figure is not a measured zero"
    );
    assert_eq!(
        mem_info_from("MemTotal: 8388608 kB\nMemAvailable: not-a-number\n"),
        None,
        "an unparseable availability is not a measured zero"
    );
    assert_eq!(mem_info_from(""), None);
    // The negative face of the test above: a present zero survives, so
    // `None` is not being handed out for every low figure.
    assert_ne!(
        mem_info_from("MemTotal: 8388608 kB\nMemAvailable: 0 kB\n"),
        None,
        "a measured zero is a reading and must not become an absence"
    );
}

/// An unreadable `/proc/meminfo` is not a reading of zero available RAM.
///
/// The conservative half of the rule is behaviour: the ramp treats an
/// unmeasured headroom as at the floor and refuses to allocate on it. The
/// honesty half is publication: the report says the post-reclaim figure
/// was never measured instead of printing a `0` a reader would take for a
/// measured empty machine. A measured empty swap table beside it is still
/// `Some(0)`, because that is a different face of a different file.
#[test]
fn absent_meminfo_is_not_a_measured_zero_ram() -> std::io::Result<()> {
    let log_path = std::env::temp_dir().join(format!(
        "ramshared-stress-meminfo-absent-{}-{}.log",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let options = StressOptions {
        start_pct: 1,
        target_pct: 5,
        step_pct: 1,
        interval_ms: 1,
        hold_sec: 0,
        min_order_7_chunks: 0,
        telemetry_log: log_path.display().to_string(),
        ..StressOptions::default()
    };
    let environment =
        StubStressEnvironment::new([BuddyinfoReading::Measured(64)]).with_unreadable_meminfo();

    let result = run_with_environment(&options, &environment);
    let _ = fs::remove_file(&log_path);

    let report = match result {
        Ok(report) => report,
        Err(error) => panic!("an absent meminfo must still report: {error}"),
    };
    assert_eq!(
        report.post_reclaim_free_ram_mb, None,
        "an unreadable /proc/meminfo is not a reading of zero free RAM"
    );
    assert_ne!(
        report.post_reclaim_free_ram_mb,
        Some(0),
        "absence must never wear the measured-zero face"
    );
    assert!(
        environment.requested_allocations.borrow().is_empty(),
        "an unmeasured headroom is treated as at the floor, so the ramp must not allocate"
    );
    // The swap table was readable and empty: a different file, a different
    // face. Its emptiness is a measurement and stays one.
    assert_eq!(
        report.tier1_zram_mb,
        Some(0),
        "a readable empty swap table is a measured zero, not an absence"
    );
    Ok(())
}

/// The two faces of `/proc/swaps`: a table that could not be read is an
/// absence, and a table that was read and carried no rows is a measured
/// zero. Collapsing them is how "we never measured" came out looking like
/// "we measured and the tiers were empty".
#[test]
fn unreadable_swap_table_is_absent_while_an_empty_one_is_a_measured_zero() {
    let empty = SwapTiersKib::default();
    assert_eq!(
        StubStressEnvironment::new([BuddyinfoReading::Measured(64)]).read_swap_tiers(),
        Some(empty),
        "an empty table is a reading of zero occupancy"
    );
    assert_eq!(
        StubStressEnvironment::new([BuddyinfoReading::Measured(64)]).read_swap_tier_capacities(),
        Some((
            TierCapacityStats::default(),
            TierCapacityStats::default(),
            TierCapacityStats::default(),
        )),
        "an empty table is a reading of no devices"
    );
    assert_eq!(
        StubStressEnvironment::new([BuddyinfoReading::Measured(64)])
            .with_unreadable_swap_table()
            .read_swap_tiers(),
        None,
        "an unreadable table is not a reading of zero occupancy"
    );
    assert_eq!(
        StubStressEnvironment::new([BuddyinfoReading::Measured(64)])
            .with_unreadable_swap_table()
            .read_swap_tier_capacities(),
        None,
        "an unreadable table is not a reading of no devices"
    );
}

/// A run that never read the swap table cannot publish a measured peak for
/// any tier, and it cannot claim the fallback started empty. Every
/// occupancy field says so, including the two the comparison gate reads
/// (`peak_swap_mb`, `tier3_ssd_mb`): a synthesized `0` there would claim
/// an empty tier nobody observed, and on `tier3_ssd_mb` it would suppress
/// the unintended-fallback alarm rather than leave the question open.
#[test]
fn a_run_that_never_read_the_swap_table_publishes_absence_not_zeros() -> std::io::Result<()> {
    let log_path = std::env::temp_dir().join(format!(
        "ramshared-stress-swaps-absent-{}-{}.log",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let options = StressOptions {
        start_pct: 1,
        target_pct: 1,
        step_pct: 1,
        interval_ms: 1,
        hold_sec: 0,
        min_order_7_chunks: 1,
        telemetry_log: log_path.display().to_string(),
        ..StressOptions::default()
    };
    let environment =
        StubStressEnvironment::new([BuddyinfoReading::Measured(64)]).with_unreadable_swap_table();

    let result = run_with_environment(&options, &environment);
    let telemetry = fs::read_to_string(&log_path);
    let _ = fs::remove_file(&log_path);
    let telemetry = telemetry?;

    let report = match result {
        Ok(report) => report,
        Err(error) => panic!("an unreadable swap table must still report: {error}"),
    };
    assert_eq!(
        report.tier1_zram_mb, None,
        "no sample folded in, so there is no Tier-1 peak"
    );
    assert_eq!(report.tier2_logical_swap_mb, None);
    // The two fields the comparison gate reads. Both must carry the
    // absence through as `null` rather than a `0` the gate would read as
    // a measured empty tier — on `tier3_ssd_mb` that `0` is what used to
    // suppress the unintended-fallback alarm.
    assert_eq!(
        report.peak_swap_mb, None,
        "an unmeasured total is not a measured empty tier"
    );
    assert_eq!(
        report.tier3_ssd_mb, None,
        "an unmeasured fallback is not a measured empty fallback"
    );
    assert_eq!(
        report.tier3_ssd_initial_mb, None,
        "an unmeasured baseline is not a fallback that started empty"
    );
    assert_eq!(
        report.tier1_zram_pct, None,
        "a peak that was never taken has no percent"
    );
    assert_eq!(
        report.physical_cache_samples, 0,
        "the run must not claim cache samples it never took"
    );
    assert!(
        telemetry.contains("Swap: unmeasured"),
        "the flight log must name the absence rather than a measured zero: {telemetry}"
    );
    Ok(())
}

/// The three whole-MiB marks separate an absent figure from a measured
/// zero, and the table cell keeps the unit off an absence marker.
#[test]
fn mb_marks_separate_an_absent_figure_from_a_measured_zero() {
    assert_eq!(mb_mark(Some(208)), "  208MB");
    assert_eq!(mb_mark(Some(0)), "    0MB", "a measured zero is a figure");
    assert_eq!(mb_mark(None), "unmeasured");
    assert_ne!(
        mb_mark(Some(0)),
        mb_mark(None),
        "a measured zero and an absence must never share a face"
    );

    assert_eq!(mb_cell(Some(208)), "     208 MB");
    assert_eq!(mb_cell(Some(0)), "       0 MB");
    assert_eq!(mb_cell(None), " unmeasured");
    assert_eq!(mb_cell(Some(208)).len(), 11);
    assert_eq!(mb_cell(Some(0)).len(), 11);
    assert_eq!(mb_cell(None).len(), 11, "the table column must not shift");
    assert!(
        !mb_cell(None).contains("MB"),
        "the unit must never attach to an absence marker"
    );
}

#[test]
fn formats_stress_telemetry_without_live_pressure() {
    let reading = compute_telemetry_reading(0.1, Some(0.5), 100, Some(1000), Some(10))
        .with_tier_pcts(Some(10), Some(20), Some(30));
    assert_eq!(reading.tier1_zram_pct, Some(10));
    assert_eq!(reading.tier2_logical_swap_pct, Some(20));
    assert_eq!(reading.tier3_ssd_pct, Some(30));

    // An unmeasured tier publishes absence, never a `0` a reader would
    // take for an empty device.
    let blank = compute_telemetry_reading(0.1, Some(0.5), 100, Some(1000), Some(10));
    assert_eq!(blank.tier1_zram_pct, None);
    assert_eq!(blank.tier2_logical_swap_pct, None);
    assert_eq!(blank.tier3_ssd_pct, None);
}

#[test]
fn parse_all_stress_flags_and_help() {
    let all_args = vec![
        "--start".to_string(),
        "5".to_string(),
        "--target".to_string(),
        "90".to_string(),
        "--step".to_string(),
        "2".to_string(),
        "--interval-ms".to_string(),
        "50".to_string(),
        "--hold-sec".to_string(),
        "3".to_string(),
        "--min-ram-mb".to_string(),
        "300".to_string(),
        "--max-psi-full".to_string(),
        "65.5".to_string(),
        "--max-latency-ms".to_string(),
        "15.2".to_string(),
        "--tier3-target-pct".to_string(),
        "75".to_string(),
        "--log".to_string(),
        "/tmp/test-tel.log".to_string(),
        "--json".to_string(),
        "--battery".to_string(),
        "--cascade".to_string(),
    ];
    let parsed = parse_stress_args(&all_args).unwrap_or_default();
    assert_eq!(parsed.start_pct, 5);
    assert_eq!(parsed.target_pct, 90);
    assert_eq!(parsed.step_pct, 2);
    assert_eq!(parsed.interval_ms, 50);
    assert_eq!(parsed.hold_sec, 3);
    assert_eq!(parsed.min_ram_mb, 300);
    assert!((parsed.max_psi_full - if is_wsl2() { 10.0 } else { 65.5 }).abs() < 0.01);
    assert!((parsed.max_latency_ms - 15.2).abs() < 0.01);
    assert_eq!(parsed.tier3_target_pct, Some(75));
    assert_eq!(parsed.telemetry_log, "/tmp/test-tel.log");
    assert!(parsed.json);
    assert!(parsed.battery);
    assert!(parsed.cascade);

    let unknown_res = parse_stress_args(&["--unknown-flag".to_string()]);
    assert!(unknown_res.is_err());
}

#[test]
fn benchmark_archiving_and_formatting_tests() {
    let now = SystemTime::now();
    let formatted = format_system_time(now);
    assert!(!formatted.is_empty());
    assert!(formatted.contains('_'));

    let report = StressReport {
        metric_version: 2,
        battery_mode: true,
        cascade_mode: true,
        tier3_only: false,
        tier1_target_pct: 95,
        tier2_target_pct: 95,
        tier3_target_pct: 15,
        physical_cache_required_mib: 0,
        max_safe_pct: 90,
        total_allocated_mb: 1000,
        peak_swap_mb: Some(500),
        tier1_zram_mb: Some(200),
        tier1_zram_pct: Some(50),
        tier1_zram_compression: Some(ZramCompression {
            orig_data_bytes: 30_912_512,
            backed_data_bytes: Some(0),
            compr_data_bytes: 10_297_603,
            mem_used_bytes: 11_972_608,
            huge_pages: Some(169),
            same_pages: Some(528),
            compressor: Some("zstd".to_string()),
        }),
        tier2_vram_mb: Some(200),
        tier2_vram_pct: Some(50),
        tier2_logical_swap_mb: Some(200),
        tier2_logical_swap_pct: Some(50),
        tier2_nbd_throughput_mbs: Some(500.0),
        tier2_physical_cache_target_mb: 400,
        simultaneous_physical_cache_target_mib: 0,
        simultaneous_physical_cache_mib: 0,
        simultaneous_full_tiers: false,
        physical_cache_samples: 1,
        tier3_ssd_mb: Some(100),
        tier3_ssd_initial_mb: Some(0),
        tier3_ssd_pct: Some(25),
        tier1_throughput_mbs: Some(1000.0),
        // The producer never populates the GPU-DMA pair; this fixture
        // matches it so a reader cannot mistake the synthetic value for
        // a reachable one.
        tier2_throughput_mbs: None,
        tier3_throughput_mbs: Some(20.0),
        tier2_speedup_vs_ssd: None,
        rate_windows: 1,
        peak_pressure_index: 10.0,
        telemetry_readings_count: 5,
        active_io_cycles_completed: 2,
        reclaim_duration_ms: Some(1.0),
        reclaim_speed_gbs: Some(1000.0),
        buffer_drop_duration_ms: 1.0,
        post_reclaim_free_ram_mb: Some(8000),
        status: "PASS_ZERO_PANIC".to_string(),
        avg_cycle_latency_ms: 0.05,
        p50_cycle_latency_ms: 0.02,
        p90_cycle_latency_ms: 0.08,
        p99_cycle_latency_ms: 0.15,
        max_cycle_latency_ms: 0.50,
        estimated_page_fault_lat_us: Some(0.85),
        host_vram_min_free_mb: Some(2048),
        vram_evicted_chunks_count: Some(0),
        dma_watchdog_trips_count: Some(0),
        tier3_spill_samples: Some(3),
        tier3_spill_with_free_upper_tiers: Some(0),
        vram_eviction_p99_latency_ms: Some(0.0),
        kernel_d_state_hung_tasks: Some(0),
        ..StressReport::default()
    };
    archive_and_compare_benchmark(&report, false);
    archive_and_compare_benchmark(&report, true);
}

#[test]
fn computes_latency_percentiles_accurately() {
    let (avg, p50, p90, p99, max) = compute_latency_percentiles(&[]);
    assert_eq!(avg, 0.0);
    assert_eq!(p50, 0.0);
    assert_eq!(p90, 0.0);
    assert_eq!(p99, 0.0);
    assert_eq!(max, 0.0);

    let latencies = vec![0.01, 0.02, 0.03, 0.04, 0.05, 0.10, 0.20, 0.50, 1.00, 2.00];
    let (avg, p50, p90, p99, max) = compute_latency_percentiles(&latencies);
    assert!(avg > 0.0);
    assert!((0.05..=0.20).contains(&p50));
    assert!(p90 >= p50);
    assert!(p99 >= 1.00);
    assert_eq!(max, 2.00);
}

#[test]
fn safe_allocation_never_crosses_the_physical_floor() {
    // Fixtures are written as whole-MiB policy times 1024 so the policy
    // reading stays legible; the comparison itself is in KiB.
    assert_eq!(
        safe_allocation_kib(
            true,
            true,
            Some(600 * KIB_PER_MIB),
            600 * KIB_PER_MIB,
            128 * KIB_PER_MIB
        ),
        0
    );
    // 16 MiB reserve with tier-3 active: 616 - 600 leaves exactly that
    // reserve, so nothing is spendable.
    assert_eq!(
        safe_allocation_kib(
            true,
            true,
            Some(616 * KIB_PER_MIB),
            600 * KIB_PER_MIB,
            128 * KIB_PER_MIB
        ),
        0
    );
    assert_eq!(
        safe_allocation_kib(
            true,
            true,
            Some(617 * KIB_PER_MIB),
            600 * KIB_PER_MIB,
            128 * KIB_PER_MIB
        ),
        KIB_PER_MIB
    );
    // Capped by TIER3_MAX_STEP, not by the 33 MiB of headroom.
    assert_eq!(
        safe_allocation_kib(
            true,
            true,
            Some(649 * KIB_PER_MIB),
            600 * KIB_PER_MIB,
            128 * KIB_PER_MIB
        ),
        8 * KIB_PER_MIB
    );
    // 50 MiB reserve before tier-3: 601 - 600 leaves 1 MiB, below it.
    assert_eq!(
        safe_allocation_kib(
            true,
            false,
            Some(601 * KIB_PER_MIB),
            600 * KIB_PER_MIB,
            128 * KIB_PER_MIB
        ),
        0
    );
    assert_eq!(
        safe_allocation_kib(
            true,
            false,
            Some(651 * KIB_PER_MIB),
            600 * KIB_PER_MIB,
            128 * KIB_PER_MIB
        ),
        KIB_PER_MIB
    );
    assert_eq!(
        safe_allocation_kib(
            true,
            false,
            Some(600 * KIB_PER_MIB),
            600 * KIB_PER_MIB,
            128 * KIB_PER_MIB
        ),
        0
    );
    assert_eq!(
        safe_allocation_kib(
            false,
            false,
            Some(600 * KIB_PER_MIB),
            600 * KIB_PER_MIB,
            128 * KIB_PER_MIB
        ),
        0
    );
    // A sub-MiB headroom is the case the whole-MiB fold could not
    // express. The bound is what is actually left, so the step stops
    // before the floor; rounding up to 1 MiB would cross it.
    assert_eq!(
        safe_allocation_kib(
            true,
            true,
            Some(616 * KIB_PER_MIB + 512),
            600 * KIB_PER_MIB,
            128 * KIB_PER_MIB
        ),
        512
    );
}

#[test]
fn tier3_target_requires_the_requested_tier3_percentage() {
    assert!(!tier3_target_reached(Some(98), 99));
    assert!(tier3_target_reached(Some(99), 99));
    // Absence is not a measured zero: a missing denominator cannot be "at
    // target", even at a target of 0.
    assert!(!tier3_target_reached(None, 99));
    assert!(!tier3_target_reached(None, 0));
}

#[test]
fn tier3_pacing_has_a_wsl2_floor() {
    assert_eq!(step_interval_ms(true, true, 200), 500);
    assert_eq!(step_interval_ms(true, true, 800), 800);
    assert_eq!(step_interval_ms(true, false, 200), 200);
    assert_eq!(step_interval_ms(false, true, 200), 200);
}
