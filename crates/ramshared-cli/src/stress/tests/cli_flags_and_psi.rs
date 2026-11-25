// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/stress/tests/cli_flags_and_psi.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

#[test]
// TestName: tier1_compression_line_names_the_host_cost_beside_the_compressed_form
fn tier1_compression_line_names_the_host_cost_beside_the_compressed_form() {
    // A device that stored 3000 bytes of user data as a 1000-byte compressed
    // form and paid 1200 bytes of host memory for it. The compressed form is
    // what the data became; the cost is what the host actually paid, and an
    // operator reading only the first two numbers would budget against the
    // wrong figure.
    let sample = ZramCompression {
        orig_data_bytes: 3000,
        compr_data_bytes: 1000,
        mem_used_bytes: 1200,
        huge_pages: Some(2),
        same_pages: Some(0),
        compressor: None,
    };
    let line = format_tier1_compression(Some(&sample));
    assert_eq!(
        line,
        "3.00:1 at peak (3000 -> 1000 B, cost 1200 B, compressor unreported, 2 incompressible)"
    );

    // Negative: the cost is a labelled third figure, not an alias of the
    // compressed form. A reader must not be able to take `1000` as the
    // host's bill when `1200` is.
    assert!(
        line.contains("cost 1200 B"),
        "the host cost must be named beside the compressed form: {line}"
    );

    // A kernel that omitted the huge-page column keeps that wording; the
    // cost is still measured and still printed.
    let unreported = ZramCompression {
        orig_data_bytes: 3000,
        compr_data_bytes: 1000,
        mem_used_bytes: 1200,
        huge_pages: None,
        same_pages: Some(0),
        compressor: None,
    };
    assert_eq!(
        format_tier1_compression(Some(&unreported)),
        "3.00:1 at peak (3000 -> 1000 B, cost 1200 B, compressor unreported, incompressible unreported)"
    );

    // The compressor is part of the measurement and is named, the same
    // fact the dashboard prints. Dropping it would leave the two surfaces
    // disagreeing about what a sample carries.
    let named = ZramCompression {
        orig_data_bytes: 3000,
        compr_data_bytes: 1000,
        mem_used_bytes: 1200,
        huge_pages: Some(2),
        same_pages: Some(0),
        compressor: Some("zstd".to_string()),
    };
    assert_eq!(
        format_tier1_compression(Some(&named)),
        "3.00:1 at peak (3000 -> 1000 B, cost 1200 B, zstd, 2 incompressible)"
    );

    // An empty name is not a name: it falls back to the unreported
    // wording rather than printing a blank field.
    let empty_name = ZramCompression {
        compressor: Some(String::new()),
        ..named
    };
    assert_eq!(
        format_tier1_compression(Some(&empty_name)),
        "3.00:1 at peak (3000 -> 1000 B, cost 1200 B, compressor unreported, 2 incompressible)"
    );

    // A missing sample and a pair of counts the kernel cannot produce are
    // the two `not measured` faces. A sample that was taken and holds
    // nothing is a measurement of emptiness — the byte counts read `0`
    // deliberately — and says so. Collapsing the two published a measured
    // zero as if the harness had never looked. `0.00:1` is still never
    // printed: there is no ratio to report.
    assert_eq!(format_tier1_compression(None), "not measured");
    assert_eq!(
        format_tier1_compression(Some(&ZramCompression::default())),
        "holding nothing (no ratio yet)"
    );

    // A ratio below 1:1 is a real measurement — the data expanded — and is
    // printed, never refused.
    let expanded = ZramCompression {
        orig_data_bytes: 830,
        compr_data_bytes: 1000,
        mem_used_bytes: 1200,
        huge_pages: Some(0),
        same_pages: Some(0),
        compressor: Some("zstd".to_string()),
    };
    assert_eq!(
        format_tier1_compression(Some(&expanded)),
        "0.83:1 at peak (830 -> 1000 B, cost 1200 B, zstd, 0 incompressible)"
    );
}

#[test]
fn tier1_compression_line_separates_emptiness_same_filled_and_an_impossible_pair() {
    // A device holding data whose stored pages are all same-filled:
    // `mm_stat` reports `orig > 0, compr == 0`. That is the
    // most-compressed state the tier can be in. Printing `holding
    // nothing` for it told the operator the device was empty while the
    // very counts that proved otherwise were thrown away on the same line.
    // Same-filled is internal to one page and is not deduplication — zram
    // never compares pages against each other — so the face refuses that
    // vocabulary.
    let all_same_filled = ZramCompression {
        orig_data_bytes: 4096,
        compr_data_bytes: 0,
        mem_used_bytes: 64,
        huge_pages: Some(0),
        same_pages: Some(1),
        compressor: Some("zstd".to_string()),
    };
    let line = format_tier1_compression(Some(&all_same_filled));
    assert!(
        !line.contains("holding nothing"),
        "data held with an empty compressed form is not emptiness: {line}"
    );
    assert!(line.contains("all same-filled"), "{line}");
    assert!(
        !line.contains("identical") && !line.contains("dedup"),
        "same-filled is not deduplication: {line}"
    );
    // The measured body survives: the counts that prove the state are the
    // ones the line keeps.
    assert!(line.contains("4096 -> 0 B"), "{line}");
    assert!(line.contains("cost 64 B"), "{line}");

    // Emptiness is the other side and keeps its own word.
    assert_eq!(
        format_tier1_compression(Some(&ZramCompression::default())),
        "holding nothing (no ratio yet)"
    );

    // The mirror pair cannot occur: every path that adds to `compr` also
    // stores a page, and `orig` is that stored-page count scaled by the
    // page size. A tear or corruption has no reading.
    let torn = ZramCompression {
        orig_data_bytes: 0,
        compr_data_bytes: 1000,
        mem_used_bytes: 1200,
        huge_pages: Some(0),
        same_pages: Some(0),
        compressor: Some("zstd".to_string()),
    };
    assert_eq!(format_tier1_compression(Some(&torn)), "not measured");
}

/// The same rule the dashboard's compression row applies: a `same_pages`
/// counter that was measured and reads zero refutes the all-same-filled
/// name, and a sample that disagrees with itself publishes `not measured`.
#[test]
fn tier1_compression_line_refuses_the_same_filled_name_when_the_counter_contradicts_it() {
    let contradicted = ZramCompression {
        orig_data_bytes: 12288,
        compr_data_bytes: 0,
        mem_used_bytes: 4096,
        huge_pages: Some(0),
        same_pages: Some(0),
        compressor: Some("zstd".to_string()),
    };
    assert_eq!(
        format_tier1_compression(Some(&contradicted)),
        "not measured",
        "a measured zero same-page count must refute the classification"
    );

    let corroborated = ZramCompression {
        same_pages: Some(3),
        ..contradicted.clone()
    };
    assert!(
        format_tier1_compression(Some(&corroborated)).contains("all same-filled"),
        "a measured non-zero count corroborates the bytes"
    );

    let unreported = ZramCompression {
        same_pages: None,
        ..contradicted
    };
    assert!(
        format_tier1_compression(Some(&unreported)).contains("all same-filled"),
        "an unreported counter is not a measured zero"
    );
}

#[test]
fn active_cycle_stays_on_bounded_injected_backing() -> std::io::Result<()> {
    let log_path = std::env::temp_dir().join(format!(
        "ramshared-stress-cycle-{}-{}.log",
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
        hold_sec: 1,
        min_order_7_chunks: 0,
        telemetry_log: log_path.display().to_string(),
        ..StressOptions::default()
    };
    let environment = StubStressEnvironment::new([]);

    let result = run_with_environment(&options, &environment);
    let telemetry = fs::read_to_string(&log_path);
    let _ = fs::remove_file(&log_path);
    let telemetry = telemetry?;

    assert!(result.is_ok(), "bounded active cycle failed: {result:?}");
    assert!(telemetry.contains("RAM:"));
    assert!(
        environment.latency_probes.get() > 1,
        "hold phase should sample cycles"
    );
    assert_eq!(environment.requested_allocations.borrow().len(), 1);
    assert_eq!(environment.backing_allocations.borrow().as_slice(), &[4096]);
    Ok(())
}

#[test]
fn cascade_stress_refuses_missing_physical_cache_before_allocation() {
    assert!(require_physical_cache_before_cascade(true, None, None).is_err());
    assert!(require_physical_cache_before_cascade(true, None, None).is_err());
    assert!(require_physical_cache_before_cascade(false, None, None).is_ok());
    assert!(
        require_physical_cache_before_cascade(
            true,
            None,
            Some(CacheSample {
                cached_kib: 0,
                target_kib: 2048 * 1024,
                at_target: false,
            }),
        )
        .is_ok()
    );
    assert!(
        require_physical_cache_before_cascade(
            true,
            Some(4096),
            Some(CacheSample {
                cached_kib: 256 * 1024,
                target_kib: 1024 * 1024,
                at_target: false,
            }),
        )
        .is_err()
    );
}

#[test]
fn parses_stress_cli_arguments_with_battery() {
    let args = vec![
        "--start".to_string(),
        "5".to_string(),
        "--target".to_string(),
        "80".to_string(),
        "--step".to_string(),
        "2".to_string(),
        "--interval-ms".to_string(),
        "500".to_string(),
        "--hold-sec".to_string(),
        "3".to_string(),
        "--min-ram-mb".to_string(),
        "800".to_string(),
        "--battery".to_string(),
        "--log".to_string(),
        "/tmp/test-telemetry.log".to_string(),
        "--json".to_string(),
    ];
    let opts = parse_stress_args(&args).unwrap_or_default();
    assert_eq!(opts.start_pct, 5);
    assert_eq!(opts.target_pct, 80);
    assert_eq!(opts.step_pct, 2);
    assert_eq!(opts.interval_ms, 500);
    assert_eq!(opts.hold_sec, 3);
    assert_eq!(opts.min_ram_mb, 800);
    assert!(opts.battery);
    assert_eq!(opts.telemetry_log, "/tmp/test-telemetry.log");
    assert!(opts.json);
}

#[test]
fn cascade_flags_do_not_raise_the_pressure_abort_limit() {
    for (args, native_limit) in [
        (vec!["--cascade".to_string()], 20.0),
        (
            vec!["--tier3-target-pct".to_string(), "99".to_string()],
            20.0,
        ),
        (
            vec![
                "--cascade".to_string(),
                "--max-psi-full".to_string(),
                "50".to_string(),
            ],
            50.0,
        ),
    ] {
        let opts = parse_stress_args(&args).unwrap_or_default();
        assert!(opts.cascade);
        assert_eq!(
            opts.max_psi_full,
            if is_wsl2() { 10.0 } else { native_limit }
        );
    }
}

#[test]
fn rejects_thread_count_exceeding_physical_limits() {
    let args = vec!["--threads".to_string(), "999999".to_string()];
    let res = parse_stress_args(&args);
    match res {
        Err(err) => assert!(err.contains("exceeds physical hardware limit")),
        Ok(_) => panic!("expected thread limit validation error"),
    }
}

#[test]
fn parses_stress_cli_argument_errors() {
    assert!(parse_stress_args(&["--start".to_string()]).is_err());
    assert!(parse_stress_args(&["--start".to_string(), "invalid".to_string()]).is_err());
    assert!(parse_stress_args(&["--target".to_string()]).is_err());
    assert!(parse_stress_args(&["--target".to_string(), "invalid".to_string()]).is_err());
    assert!(parse_stress_args(&["--step".to_string()]).is_err());
    assert!(parse_stress_args(&["--step".to_string(), "invalid".to_string()]).is_err());
    assert!(parse_stress_args(&["--interval-ms".to_string()]).is_err());
    assert!(parse_stress_args(&["--interval-ms".to_string(), "invalid".to_string()]).is_err());
    assert!(parse_stress_args(&["--hold-sec".to_string()]).is_err());
    assert!(parse_stress_args(&["--hold-sec".to_string(), "invalid".to_string()]).is_err());
    assert!(parse_stress_args(&["--min-ram-mb".to_string()]).is_err());
    assert!(parse_stress_args(&["--min-ram-mb".to_string(), "invalid".to_string()]).is_err());
    assert!(parse_stress_args(&["--log".to_string()]).is_err());
    assert!(parse_stress_args(&["--unknown-flag".to_string()]).is_err());
}

#[test]
fn rejects_invalid_tier_targets_and_unsafe_profile_combinations() {
    for args in [
        vec!["--tier3-target-pct"],
        vec!["--tier3-target-pct", "101"],
        vec!["--tier1-target-pct"],
        vec!["--tier1-target-pct", "invalid"],
        vec!["--tier2-target-pct"],
        vec!["--tier2-target-pct", "invalid"],
        vec!["--physical-cache-target-mib"],
        vec!["--physical-cache-target-mib", "invalid"],
        vec!["--min-order-7-chunks"],
        vec!["--min-order-7-chunks", "invalid"],
        vec!["--tier3-only", "--cascade", "--tier3-target-pct", "99"],
        vec!["--tier3-only"],
        vec![
            "--tier3-only",
            "--tier3-target-pct",
            "99",
            "--physical-cache-target-mib",
            "1",
        ],
        vec!["--full-three-tier", "--start", "101"],
        vec!["--tier1-target-pct", "101"],
    ] {
        let args = args.into_iter().map(str::to_string).collect::<Vec<_>>();
        assert!(
            parse_stress_args(&args).is_err(),
            "accepted invalid args: {args:?}"
        );
    }
}

#[test]
fn computes_telemetry_reading_accurately() {
    let s0 = compute_telemetry_reading(0.01, Some(0.0), 1000, None, Some(0));
    assert!(s0.pressure_index >= 1.0);

    let s1 = compute_telemetry_reading(0.01, Some(0.0), 1000, Some(20000), Some(0));
    assert!(s1.pressure_index >= 1.0 && s1.pressure_index < 3.0);
    assert!(s1.classification.contains("NOMINAL"));

    let s2 = compute_telemetry_reading(0.5, Some(1.0), 10000, Some(20000), Some(300));
    assert!(s2.classification.contains("TIER-1"));

    let s3 = compute_telemetry_reading(1.5, Some(4.0), 14000, Some(20000), Some(1000));
    assert!(s3.classification.contains("TIER-2"));

    let s4 = compute_telemetry_reading(2.0, Some(5.0), 16000, Some(20000), Some(1500));
    assert!(s4.classification.contains("HIGH PRESSURE"));

    let s5 = compute_telemetry_reading(10.0, Some(25.0), 19000, Some(20000), Some(6000));
    assert!(s5.classification.contains("CEILING INTERLOCK"));
    assert!(s5.gauge.contains('█'));
}

#[test]
fn appends_telemetry_log_file() {
    let path = "/tmp/test-ramshared-telemetry-unit.log";
    let reading = compute_telemetry_reading(0.05, Some(1.2), 2000, Some(20000), Some(100));
    append_telemetry_log(path, &reading);
    let content = fs::read_to_string(path).unwrap_or_default();
    assert!(content.contains("IDX"));
    let _ = fs::remove_file(path);
}

#[test]
fn cascade_flag_selects_qualification_mode_without_live_pressure() {
    let parsed_res = parse_stress_args(&["--cascade".to_string()]);
    assert!(parsed_res.is_ok());
    let parsed_cascade = parsed_res.unwrap_or_default();
    assert!(parsed_cascade.cascade);
    assert!(parsed_cascade.battery);
}

#[test]
fn full_three_tier_profile_requires_exact_simultaneous_targets() {
    let opts = parse_stress_args(&["--full-three-tier".to_string()])
        .unwrap_or_else(|error| panic!("full profile must parse: {error}"));
    assert_eq!((opts.tier1_target_pct, opts.tier2_target_pct), (100, 100));
    assert_eq!(opts.tier3_target_pct, Some(99));
    assert_eq!(opts.physical_cache_target_mib, None);
    assert!(opts.cascade && opts.battery);
    let cache = Some(CacheSample {
        cached_kib: 4096 * 1024,
        target_kib: 4096 * 1024,
        at_target: true,
    });
    assert!(full_tier_snapshot(Some(100), Some(100), Some(99), &opts, cache).is_some());
    assert!(full_tier_snapshot(Some(99), Some(100), Some(99), &opts, cache).is_none());
    assert!(full_tier_snapshot(Some(100), Some(99), Some(99), &opts, cache).is_none());
    assert!(full_tier_snapshot(Some(100), Some(100), Some(98), &opts, cache).is_none());
    assert!(
        full_tier_snapshot(
            Some(100),
            Some(100),
            Some(99),
            &opts,
            Some(CacheSample {
                cached_kib: 4095 * 1024,
                target_kib: 4096 * 1024,
                at_target: false,
            })
        )
        .is_none()
    );

    let qualified = full_tier_snapshot(
        Some(100),
        Some(100),
        Some(99),
        &opts,
        Some(CacheSample {
            cached_kib: 3072 * 1024,
            target_kib: 2560 * 1024,
            at_target: true,
        }),
    )
    .unwrap_or_else(|| panic!("qualified tiers return the sample from this observation"));
    assert_eq!(qualified.target_mib(), 2560);
    assert_eq!(qualified.cached_mib(), 3072);
}

#[test]
fn tier3_only_requires_storage_swap_but_not_gpu_cache() {
    let options = parse_stress_args(&[
        "--tier3-only".to_string(),
        "--tier3-target-pct".to_string(),
        "99".to_string(),
    ])
    .unwrap_or_else(|error| panic!("tier3-only options must parse: {error}"));

    assert!(options.tier3_only);
    assert!(!options.cascade);
    assert_eq!(options.tier3_target_pct, Some(99));
    // Measured and empty: the table was read and carried no storage tier.
    assert!(require_tier3_only_capacity(true, Some(TierCapacityStats::default())).is_err());
    assert!(
        require_tier3_only_capacity(
            true,
            Some(TierCapacityStats {
                total_kib: 1024,
                used_kib: 0,
            })
        )
        .is_ok()
    );
    assert!(require_tier3_only_capacity(false, Some(TierCapacityStats::default())).is_ok());
    // Unmeasured is its own refusal and must not wear the "no device"
    // face: the operator has to be told the table could not be read, not
    // that it was read and found empty. Both refuse, and only one of them
    // is a measurement.
    let unmeasured = require_tier3_only_capacity(true, None)
        .expect_err("an unreadable swap table must refuse a tier3-only run");
    assert!(
        unmeasured.contains("unmeasured"),
        "the refusal must name the absence, not the empty-device case: {unmeasured}"
    );
    let empty = require_tier3_only_capacity(true, Some(TierCapacityStats::default()))
        .expect_err("a measured empty table must refuse a tier3-only run");
    assert!(
        !empty.contains("unmeasured"),
        "a measured empty table is a reading and must not claim absence: {empty}"
    );
    // Disabled is not a licence to skip the distinction: it returns Ok
    // whatever the reading was, including its absence.
    assert!(require_tier3_only_capacity(false, None).is_ok());
}

#[test]
fn tier3_only_target_ignores_gpu_and_higher_tier_fill() {
    assert!(tier3_only_target_reached(Some(99), 99, 1));
    assert!(!tier3_only_target_reached(Some(99), 99, 0));
    assert!(!tier3_only_target_reached(Some(98), 99, 1));
    // A tier with no swap device has no percent, so it is not at target —
    // `None` is absence, not a measured `0` that a target of `0` would
    // accept.
    assert!(!tier3_only_target_reached(None, 99, 1));
    assert!(!tier3_only_target_reached(None, 0, 1));
}

#[test]
fn full_profile_uses_active_cache_target_not_fixed_size() {
    let options = parse_stress_args(&["--full-three-tier".to_string()])
        .unwrap_or_else(|error| panic!("full profile must parse: {error}"));
    assert_eq!(options.physical_cache_target_mib, None);

    let active_cache = CacheSample {
        cached_kib: 512 * 1024,
        target_kib: 1536 * 1024,
        at_target: false,
    };
    assert_eq!(
        full_profile_cache_target(None, Some(active_cache)),
        Ok(1536)
    );
    assert_eq!(
        full_profile_cache_target(Some(1024), Some(active_cache)),
        Ok(1024)
    );
    assert!(full_profile_cache_target(Some(2048), Some(active_cache)).is_err());
}

#[test]
fn helper_probes_execute_safely() {
    // Live probes: the point is that they return, and that a present
    // reading really is a reading. Absence is allowed and is `None`.
    if let Some(mem) = read_mem_info() {
        assert!(mem.total_kib > 0 || mem.avail_kib == 0);
    }
    let _ = read_psi_full();
    let _ = read_swap_tiers();
    if let Some((c1, c2, c3)) = read_swap_tier_capacities() {
        let _ = format!("{c1:?} {c2:?} {c3:?}");
    }
    let lat = probe_allocation_latency_ms();
    assert!(lat >= 0.0);
    let _ = read_buddyinfo_reading();
    let _ = read_tier_disk_total_bytes();
    let _ = is_wsl2();
    let _ = read_sysctl_min_free_kib();
}

#[test]
fn parse_psi_full_avg10_fails_closed_on_missing_or_invalid_samples() {
    assert_eq!(
        crate::supervisor::parse_psi_full_avg10(
            "some avg10=12.00 avg60=3.00 avg300=1.00 total=10\nfull avg10=9.50 avg60=2.00 avg300=1.00 total=5\n"
        ),
        Some(9.5)
    );
    for invalid in [
        "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\n",
        "full avg10=invalid avg60=0.00 avg300=0.00 total=0\n",
        "full avg10=NaN avg60=0.00 avg300=0.00 total=0\n",
        "full avg10=100.01 avg60=0.00 avg300=0.00 total=0\n",
        "full avg10=1.00 avg60=0.00 avg300=0.00 total=0\nfull avg10=2.00 avg60=0.00 avg300=0.00 total=0\n",
    ] {
        assert_eq!(
            crate::supervisor::parse_psi_full_avg10(invalid),
            None,
            "invalid sample: {invalid:?}"
        );
    }
}

#[test]
fn required_stress_psi_does_not_turn_missing_telemetry_into_zero_pressure() {
    assert_eq!(stress_psi_sample(Some(9.5), true), StressPsi::Measured(9.5));
    assert_eq!(
        stress_psi_sample(Some(9.5), false),
        StressPsi::Measured(9.5)
    );

    // A required absence is fatal. It is not a measurement of anything.
    assert_eq!(stress_psi_sample(None, true), StressPsi::Invalid);

    // A tolerated absence is the absence it is. The fold this test exists
    // to pin was `Some(0.0)` here — a measured idle nobody observed,
    // which held the pressure interlock open for the rest of the run.
    assert_eq!(stress_psi_sample(None, false), StressPsi::Uninformed);
}

#[test]
fn a_psi_figure_outside_the_psi_range_is_not_published_as_one() {
    // A number that cannot be a PSI full reading is treated as the
    // absence it is, never as the measurement it would pretend to be.
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.5, 100.5] {
        assert_eq!(stress_psi_sample(Some(bad), true), StressPsi::Invalid);
        assert_eq!(stress_psi_sample(Some(bad), false), StressPsi::Uninformed);
    }
}

#[test]
fn an_unmeasured_psi_is_published_as_absence_not_as_zero() {
    // The gauge may fold an absent PSI into a *contribution* of zero,
    // exactly as it folds an absent swap denominator. The measurement
    // column may not: `None` stays `None`.
    let absent = compute_telemetry_reading(0.1, None, 100, Some(1000), Some(10));
    assert_eq!(absent.psi_full, None);

    let measured_idle = compute_telemetry_reading(0.1, Some(0.0), 100, Some(1000), Some(10));
    assert_eq!(measured_idle.psi_full, Some(0.0));

    // Both contribute nothing to the index, and only one of them is a
    // reading of zero pressure. The index cannot tell them apart — that
    // is what makes publishing the distinction matter.
    assert_eq!(absent.pressure_index, measured_idle.pressure_index);

    assert_eq!(psi_mark(None), "n/a");
    assert_eq!(psi_mark(Some(0.0)), "0.0%");
    assert!(
        !psi_mark(None).contains('%'),
        "the unit must never attach to an absence marker"
    );
}

#[test]
fn an_unmeasured_psi_neither_fires_the_veto_nor_grants_the_licence() {
    // The two interlock directions treat absence differently, and both
    // are right: a veto needs an observation of danger, a licence needs
    // an observation of safety. Neither may be reached by a `0.0` that
    // nobody measured.
    assert!(psi_at_or_over_limit(Some(20.0), 20.0));
    assert!(!psi_at_or_over_limit(Some(19.9), 20.0));
    assert!(!psi_at_or_over_limit(None, 20.0), "absence is not a veto");

    assert!(psi_below_limit(Some(19.9), 20.0));
    assert!(!psi_below_limit(Some(20.0), 20.0));
    assert!(!psi_below_limit(None, 20.0), "absence is not a licence");
}
