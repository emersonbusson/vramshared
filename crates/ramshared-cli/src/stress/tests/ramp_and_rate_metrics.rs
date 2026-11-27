// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/stress/tests/ramp_and_rate_metrics.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

#[test]
fn run_stops_at_an_unreachable_memory_floor_before_allocating() {
    for json in [true, false] {
        let log_path = std::env::temp_dir().join(format!(
            "ramshared-stress-floor-{}-{}-{json}.log",
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
            min_ram_mb: u64::MAX,
            telemetry_log: log_path.display().to_string(),
            json,
            min_order_7_chunks: 0,
            ..StressOptions::default()
        };

        let result = run(&options);
        let telemetry = fs::read_to_string(&log_path).unwrap_or_default();
        let _ = fs::remove_file(&log_path);

        assert!(
            result.is_ok(),
            "read-only safety-floor run failed: {result:?}"
        );
        assert!(
            telemetry.contains("RAM"),
            "the safety-floor branch should record its read-only sample"
        );
    }
}

#[test]
fn run_uses_only_injected_telemetry_and_bounded_allocation() -> std::io::Result<()> {
    let log_path = std::env::temp_dir().join(format!(
        "ramshared-stress-injected-{}-{}.log",
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
    let environment = StubStressEnvironment::new([
        BuddyinfoReading::Measured(0),
        BuddyinfoReading::Measured(64),
    ]);

    let result = run_with_environment(&options, &environment);
    let telemetry = fs::read_to_string(&log_path);
    let _ = fs::remove_file(&log_path);
    let telemetry = telemetry?;

    assert!(result.is_ok(), "bounded injected run failed: {result:?}");
    assert!(telemetry.contains("RAM:"));
    assert_eq!(environment.compaction_calls.get(), 1);
    // One step is 1% of the measured total, taken in the unit that total
    // was measured in. The stub holds 8 GiB = 8_388_608 KiB, so the step
    // is 83_886 KiB (integer division rounds the bound down) and the
    // request is those bytes. The old expectation, 81 MiB, was 1% of the
    // whole-MiB total (8192) truncated to a whole MiB -- a different
    // number than the policy's percent of what was measured.
    assert_eq!(
        environment.requested_allocations.borrow().as_slice(),
        &[83_886 * 1024]
    );
    assert_eq!(environment.backing_allocations.borrow().as_slice(), &[4096]);
    Ok(())
}

#[test]
fn halting_sample_still_counts_toward_swap_peaks() -> std::io::Result<()> {
    let log_path = std::env::temp_dir().join(format!(
        "ramshared-stress-peak-halt-{}-{}.log",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    // One ramp step. The init sample is low; the step sample is the highest
    // swap the run will see, and its PSI exceeds the limit, so the run
    // breaks on Zero Hang Protection before that step's allocation. The
    // peak must still record the halting sample.
    let options = StressOptions {
        start_pct: 1,
        target_pct: 1,
        step_pct: 1,
        interval_ms: 1,
        hold_sec: 0,
        min_order_7_chunks: 1,
        max_psi_full: 10.0,
        telemetry_log: log_path.display().to_string(),
        ..StressOptions::default()
    };
    let environment = StubStressEnvironment::new([BuddyinfoReading::Measured(64)])
        .with_swap_samples([
            SwapTiersKib::default(),
            // A sample `/proc/swaps` could really produce: 20 MiB resident
            // on Tier 1, 100 on Tier 2 and 88 on the fallback, for 208 of
            // total swap. The free fourth slot the old tuple offered let a
            // stub claim 208 of total with only 20 in any role, which no
            // fold of real rows can emit.
            SwapTiersKib {
                zram_kib: 20 * 1024,
                vram_kib: 100 * 1024,
                ssd_kib: 88 * 1024,
            },
        ])
        .with_psi_samples([0.0, 12.0]);

    let result = run_with_environment(&options, &environment);
    let telemetry = fs::read_to_string(&log_path);
    let _ = fs::remove_file(&log_path);
    let telemetry = telemetry?;

    let report = match result {
        Ok(report) => report,
        Err(error) => panic!("halting injected run must still report: {error}"),
    };
    assert!(
        environment.requested_allocations.borrow().is_empty(),
        "Zero Hang Protection must break before the step allocation"
    );
    assert_eq!(
        report.peak_swap_mb,
        Some(208),
        "the halting sample must count toward the swap peak"
    );
    assert_eq!(report.tier1_zram_mb, Some(20));
    assert_eq!(
        report.tier2_logical_swap_mb,
        Some(100),
        "the total is the fold of the three roles, not a fourth free number"
    );
    assert_eq!(report.tier3_ssd_mb, Some(88));
    assert!(
        telemetry.contains("Swap:   208MB"),
        "peak_swap_mb must equal the maximum telemetry Swap sample: {telemetry}"
    );
    Ok(())
}

/// Report-level pairing: the published tier
/// percent is the peak over the capacity co-sampled with it, not a
/// post-run capacity re-read. The third capacity sample is a vanished
/// device — exactly the read a post-run denominator would take — and it
/// must not reach the percent.
#[test]
fn report_tier_percents_use_the_capacity_co_sampled_with_the_peak() -> std::io::Result<()> {
    let log_path = std::env::temp_dir().join(format!(
        "ramshared-stress-paired-cap-{}-{}.log",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    // Same halting shape as `halting_sample_still_counts_toward_swap_peaks`:
    // one ramp step whose sample is the peak, then a PSI break before the
    // step allocation and before the mid-loop capacity re-read.
    let options = StressOptions {
        start_pct: 1,
        target_pct: 1,
        step_pct: 1,
        interval_ms: 1,
        hold_sec: 0,
        min_order_7_chunks: 1,
        max_psi_full: 10.0,
        telemetry_log: log_path.display().to_string(),
        ..StressOptions::default()
    };
    let empty = TierCapacityStats::default();
    let environment = StubStressEnvironment::new([BuddyinfoReading::Measured(64)])
        .with_swap_samples([
            SwapTiersKib::default(),
            SwapTiersKib {
                zram_kib: 20 * 1024,
                vram_kib: 100 * 1024,
                ssd_kib: 88 * 1024,
            },
        ])
        .with_capacity_samples([
            // Admission read: no swap device required.
            (empty, empty, empty),
            // The peak sample's capacities — the denominators the peaks
            // are taken against. 20 of 40 MiB, 100 of 200, 88 of 176.
            (
                TierCapacityStats {
                    total_kib: 40 * 1024,
                    used_kib: 20 * 1024,
                },
                TierCapacityStats {
                    total_kib: 200 * 1024,
                    used_kib: 100 * 1024,
                },
                TierCapacityStats {
                    total_kib: 176 * 1024,
                    used_kib: 88 * 1024,
                },
            ),
            // A vanished device at report time. If this became the
            // denominator the percents would publish `None`.
            (empty, empty, empty),
        ])
        .with_psi_samples([0.0, 12.0]);

    let result = run_with_environment(&options, &environment);
    let _ = fs::remove_file(&log_path);
    let report = match result {
        Ok(report) => report,
        Err(error) => panic!("paired-capacity run must still report: {error}"),
    };
    assert_eq!(
        report.tier1_zram_pct,
        Some(50),
        "tier-1 percent is 20 of the 40 MiB capacity from the peak sample"
    );
    assert_eq!(report.tier2_logical_swap_pct, Some(50));
    assert_eq!(report.tier3_ssd_pct, Some(50));
    assert_eq!(report.tier1_zram_mb, Some(20));
    Ok(())
}

#[test]
fn halting_sample_still_counts_toward_zram_compression() -> std::io::Result<()> {
    let log_path = std::env::temp_dir().join(format!(
        "ramshared-stress-mmstat-halt-{}-{}.log",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    // Same shape as the swap-peak case: one ramp step, and the step's
    // sample is the one Zero Hang Protection halts on. The compression
    // counters are the device's current occupancy, so this is the only
    // sample that still has data behind it — by report time the buffers
    // are gone, and so are the counters. One scripted sample, because a
    // one-step halting run folds exactly once; displacement across samples
    // is the fold helper's own test.
    let options = StressOptions {
        start_pct: 1,
        target_pct: 1,
        step_pct: 1,
        interval_ms: 1,
        hold_sec: 0,
        min_order_7_chunks: 1,
        max_psi_full: 10.0,
        telemetry_log: log_path.display().to_string(),
        ..StressOptions::default()
    };
    let saturated = ZramCompression {
        orig_data_bytes: 30_000,
        backed_data_bytes: Some(0),
        compr_data_bytes: 10_000,
        mem_used_bytes: 12_000,
        huge_pages: Some(16),
        same_pages: Some(24),
        compressor: None,
    };
    let environment = StubStressEnvironment::new([BuddyinfoReading::Measured(64)])
        .with_swap_samples([
            SwapTiersKib::default(),
            SwapTiersKib {
                zram_kib: 20 * 1024,
                vram_kib: 100 * 1024,
                ssd_kib: 88 * 1024,
            },
        ])
        .with_psi_samples([0.0, 12.0])
        .with_zram_compression_samples([Some(saturated.clone())]);

    let result = run_with_environment(&options, &environment);
    let telemetry = fs::read_to_string(&log_path);
    let _ = fs::remove_file(&log_path);
    let _ = telemetry?;

    let report = match result {
        Ok(report) => report,
        Err(error) => panic!("halting injected run must still report: {error}"),
    };
    assert!(
        environment.requested_allocations.borrow().is_empty(),
        "Zero Hang Protection must break before the step allocation"
    );
    assert_eq!(
        report.tier1_zram_compression,
        Some(saturated),
        "the halting sample's mm_stat must survive into the report"
    );
    assert_eq!(
        report
            .tier1_zram_compression
            .and_then(|sample| sample.compression_ratio()),
        Some(3.0)
    );
    Ok(())
}

#[test]
fn ramp_keeps_the_most_saturated_compression_sample() -> std::io::Result<()> {
    let log_path = std::env::temp_dir().join(format!(
        "ramshared-stress-mmstat-ramp-{}-{}.log",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    // Two ramp steps, no safety break, so both samples fold. The first
    // holds less uncompressed data than the second; the report must carry
    // the second. Extra later reads see an exhausted queue and return
    // `None`, which the fold must not mistake for a lower peak.
    let options = StressOptions {
        start_pct: 1,
        target_pct: 2,
        step_pct: 1,
        interval_ms: 1,
        hold_sec: 0,
        min_order_7_chunks: 1,
        telemetry_log: log_path.display().to_string(),
        ..StressOptions::default()
    };
    let small = ZramCompression {
        orig_data_bytes: 1_000,
        backed_data_bytes: Some(0),
        compr_data_bytes: 400,
        mem_used_bytes: 512,
        huge_pages: Some(0),
        same_pages: Some(0),
        compressor: None,
    };
    let saturated = ZramCompression {
        orig_data_bytes: 30_000,
        backed_data_bytes: Some(0),
        compr_data_bytes: 10_000,
        mem_used_bytes: 12_000,
        huge_pages: Some(16),
        same_pages: Some(24),
        compressor: None,
    };
    let environment = StubStressEnvironment::new([BuddyinfoReading::Measured(64)])
        .with_swap_samples([
            SwapTiersKib::default(),
            SwapTiersKib {
                zram_kib: 10 * 1024,
                vram_kib: 45 * 1024,
                ssd_kib: 45 * 1024,
            },
            SwapTiersKib {
                zram_kib: 20 * 1024,
                vram_kib: 90 * 1024,
                ssd_kib: 90 * 1024,
            },
        ])
        .with_zram_compression_samples([Some(small), Some(saturated.clone())]);

    let result = run_with_environment(&options, &environment);
    let telemetry = fs::read_to_string(&log_path);
    let _ = fs::remove_file(&log_path);
    let _ = telemetry?;

    let report = match result {
        Ok(report) => report,
        Err(error) => panic!("two-step injected run must report: {error}"),
    };
    assert_eq!(
        report.tier1_zram_compression,
        Some(saturated),
        "the most saturated sample must win, and an exhausted queue must not roll it back"
    );
    Ok(())
}

#[test]
fn unmeasured_zram_compression_stays_unmeasured() -> std::io::Result<()> {
    let log_path = std::env::temp_dir().join(format!(
        "ramshared-stress-mmstat-absent-{}-{}.log",
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
    // The tier is doing real swap work, but no `mm_stat` sample arrives —
    // which is exactly the state a teardown leaves behind. The report must
    // say so rather than publish a zero ratio.
    let environment = StubStressEnvironment::new([BuddyinfoReading::Measured(64)])
        .with_swap_samples([
            SwapTiersKib::default(),
            SwapTiersKib {
                zram_kib: 20 * 1024,
                vram_kib: 100 * 1024,
                ssd_kib: 88 * 1024,
            },
        ]);

    let result = run_with_environment(&options, &environment);
    let telemetry = fs::read_to_string(&log_path);
    let _ = fs::remove_file(&log_path);
    let _ = telemetry?;

    let report = match result {
        Ok(report) => report,
        Err(error) => panic!("injected run must still report: {error}"),
    };
    assert_eq!(
        report.tier1_zram_mb,
        Some(20),
        "the tier itself was measured"
    );
    assert_eq!(
        report.tier1_zram_compression, None,
        "an absent mm_stat sample is not a zero compression measurement"
    );
    assert_eq!(
        report
            .tier1_zram_compression
            .and_then(|sample| sample.compression_ratio()),
        None
    );
    Ok(())
}

#[test]
fn a_refused_rate_window_keeps_the_delta_for_the_next_one() {
    const MIB: u64 = 1024 * 1024;
    let mut rates = TierRateSampler::new((0, 0, 0));

    // A window at the floor is refused: the comparison is strictly
    // greater, so exactly `RATE_WINDOW_MIN_DT_SECS` cannot be rounded
    // into acceptance. A zero-width window is refused for the same
    // reason — it would divide by nothing.
    assert!(!rates.advance((2 * MIB, 0, 0), RATE_WINDOW_MIN_DT_SECS));
    assert!(!rates.advance((2 * MIB, 0, 0), 0.0));
    assert_eq!(rates.windows, 0);
    assert_eq!(rates.peak_zram_mbs(), None);
    assert_eq!(rates.peak_vram_mbs(), None);
    assert_eq!(rates.peak_ssd_mbs(), None);

    // The refused windows did not consume the delta: the next accepted
    // window measures the whole gap, not the tail of it. Before the
    // sampler existed each site moved the counters forward on every
    // sample regardless, so the traffic a refused window spanned was
    // silently never measured.
    assert!(rates.advance((4 * MIB, 8 * MIB, MIB), 1.0));
    assert_eq!(rates.windows, 1);
    assert_eq!(rates.peak_zram_mbs(), Some(4.0));
    assert_eq!(rates.peak_vram_mbs(), Some(8.0));
    assert_eq!(rates.peak_ssd_mbs(), Some(1.0));

    // A later slower window never lowers a peak it did not beat.
    assert!(rates.advance((5 * MIB, 8 * MIB, MIB), 1.0));
    assert_eq!(rates.windows, 2);
    assert_eq!(rates.peak_zram_mbs(), Some(4.0));
}

#[test]
fn no_rate_window_publishes_absence_not_a_measured_zero() {
    const MIB: u64 = 1024 * 1024;

    // No window accepted: there is no maximum to publish, and `0.0`
    // would claim a measured idle nobody observed.
    let empty = TierRateSampler::new((0, 0, 0));
    assert_eq!(empty.windows, 0);
    assert_eq!(empty.peak_zram_mbs(), None);
    assert_eq!(empty.peak_vram_mbs(), None);
    assert_eq!(empty.peak_ssd_mbs(), None);

    // A window that really did see no traffic is the opposite claim and
    // is a number. Collapsing it into `None` would say the harness never
    // looked at a tier it watched the whole run.
    let mut idle = TierRateSampler::new((5 * MIB, 5 * MIB, 5 * MIB));
    assert!(idle.advance((5 * MIB, 5 * MIB, 5 * MIB), 1.0));
    assert_eq!(idle.windows, 1);
    assert_eq!(idle.peak_zram_mbs(), Some(0.0));
    assert_eq!(idle.peak_vram_mbs(), Some(0.0));
    assert_eq!(idle.peak_ssd_mbs(), Some(0.0));
}

#[test]
fn published_peaks_agree_with_the_window_count() {
    let log_path = std::env::temp_dir().join(format!(
        "ramshared-stress-rate-windows-{}-{}.log",
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
    // The stub reports no tier traffic at all, so an accepted window
    // measures a real `0.0` and a refused one leaves nothing to publish.
    // Which of the two happens depends on how long the run's own sleep
    // took, so the invariant is what is pinned: the published form and
    // the witness never disagree.
    let environment = StubStressEnvironment::new([BuddyinfoReading::Measured(64)]);
    let result = run_with_environment(&options, &environment);
    let _ = fs::remove_file(&log_path);
    let report = result.expect("a stubbed run must still report");

    let published = [
        report.tier1_throughput_mbs,
        report.tier2_nbd_throughput_mbs,
        report.tier3_throughput_mbs,
    ];
    if report.rate_windows == 0 {
        assert_eq!(
            published,
            [None, None, None],
            "a maximum over no window is not a number"
        );
    } else {
        assert_eq!(
            published,
            [Some(0.0), Some(0.0), Some(0.0)],
            "the stub saw no traffic, so an accepted window is a measured idle"
        );
    }

    // The GPU-DMA pair is never produced by this harness.
    assert_eq!(report.tier2_throughput_mbs, None);
    assert_eq!(report.tier2_speedup_vs_ssd, None);
}

#[test]
fn format_throughput_mbs_separates_absence_from_a_measured_idle() {
    // No window is the harness's own absence wording — the same word the
    // compression line uses for a sample that was never taken.
    assert_eq!(format_throughput_mbs(None), "not measured");
    // A window that saw no traffic is a measurement and prints as a
    // number. The previous `{:.1}` over a plain `f64` printed `0.0 MB/s`
    // for both claims.
    assert_eq!(format_throughput_mbs(Some(0.0)), "0.0 MB/s");
    assert_eq!(format_throughput_mbs(Some(78.24)), "78.2 MB/s");
    assert_eq!(format_throughput_mbs(Some(78.26)), "78.3 MB/s");
}

/// The fill face names its denominator when it has a ratio, and names its
/// refusal when it does not — never a bare `0%`.
#[test]
fn format_cache_fill_names_the_denominator_or_the_refusal() {
    assert_eq!(format_cache_fill(Some(50), 400), "50% of 400 MB target");
    // The measured emptiness is a percentage and keeps its denominator.
    assert_eq!(format_cache_fill(Some(0), 400), "0% of 400 MB target");
    // The two refusals share one face: no sample, or no denominator. The
    // previous `unwrap_or(0)` printed `0% of 0 MB target` for both, which
    // claims a fullness measured against a target the document never gave.
    assert_eq!(format_cache_fill(None, 0), "fill not measured");
    assert_eq!(format_cache_fill(None, 400), "fill not measured");
    assert_ne!(
        format_cache_fill(None, 0),
        "0% of 0 MB target",
        "a refusal must never wear the fabricated-zero face"
    );
}

/// The two tier-percent faces name the same refusal with different words,
/// and a measured zero is a percentage on both. The unpadded compact mark
/// is deliberate: its callers own the padding.
#[test]
fn tier_percent_faces_name_the_refusal_or_the_measured_percent() {
    assert_eq!(tier_pct_mark(Some(50)), "50%");
    assert_eq!(tier_pct_mark(Some(0)), "0%", "a measured zero stays a 0%");
    assert_eq!(tier_pct_mark(None), "n/a");
    assert_ne!(
        tier_pct_mark(None),
        "0%",
        "a refusal must never wear the fabricated-zero face"
    );

    assert_eq!(format_tier_capacity(Some(50)), "50% capacity");
    assert_eq!(
        format_tier_capacity(Some(0)),
        "0% capacity",
        "a measured zero stays a 0% of a real capacity"
    );
    assert_eq!(format_tier_capacity(None), "no swap device");
    assert_ne!(
        format_tier_capacity(None),
        "0% capacity",
        "a refusal must never claim an emptiness measured against a \
             capacity that is not there"
    );

    // The compact mark is unpadded so a diagnostic string does not inherit
    // the flight log's column spaces.
    assert_eq!(tier_pct_mark(Some(5)), "5%");
    assert_ne!(tier_pct_mark(Some(5)), "  5%");
}
