// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/tests/residency_and_history_series.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;

#[test]
fn a_refused_window_does_not_hide_the_peaks_a_previous_one_recorded() {
    // `format_speed` is a per-frame fact, so a refused window really has
    // no rate for that frame. `format_rate` reports the run's peaks, and
    // those survive the refusal — a later gap must not blank them.
    let mut sample = observation(true, true);
    sample.control_plane.rate_windows = 3;
    sample.control_plane.zram_io.read_peak_mbs = Some(12.0);
    sample.control_plane.zram_io.write_peak_mbs = Some(3.0);
    sample.control_plane.zram_io.max_mbs = Some(15.0);
    sample.control_plane.zram_io.avg_mbs = Some(9.0);
    sample.control_plane.swap_peak_mbs = Some(15.0);
    sample.control_plane.swap_read_peak_mbs = Some(12.0);
    sample.control_plane.swap_write_peak_mbs = Some(3.0);
    // Three accepted windows wrote every tier together. The other two
    // measured idle: real directional peaks of zero, and no busy sample.
    sample.control_plane.vram_io.read_peak_mbs = Some(0.0);
    sample.control_plane.vram_io.write_peak_mbs = Some(0.0);
    sample.control_plane.disk_io.read_peak_mbs = Some(0.0);
    sample.control_plane.disk_io.write_peak_mbs = Some(0.0);
    // This frame's dt was refused: the rates are absent again.
    sample.control_plane.zram_io.read_mbs = None;
    sample.control_plane.zram_io.write_mbs = None;

    let backend = TestBackend::new(220, 50);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| draw_dashboard(frame, &sample, &DashHistory::default()))
        .expect("render dashboard");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(rendered.contains("Burst:"), "{rendered}");
    assert!(rendered.contains("Peak Recorded Speed:"), "{rendered}");
    assert!(
        !rendered.contains("Throughput Stats:   not measured"),
        "{rendered}"
    );
    assert!(
        !rendered.contains("Peak Recorded Speed:      not measured"),
        "{rendered}"
    );
    // The per-frame rate is still honestly absent.
    assert!(rendered.contains("MB/s (not measured)"), "{rendered}");
}

#[test]
fn dashboard_displays_missing_memory_counters_as_unavailable() {
    let mut sample = observation(false, false);
    sample.memory_scope = MemoryScope::Wsl2;
    sample.mem = parse_meminfo("MemTotal: 16384 kB\n");

    let backend = TestBackend::new(220, 50);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| draw_dashboard(frame, &sample, &DashHistory::default()))
        .expect("render dashboard");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(rendered.contains("WSL2 Guest RAM: telemetry unavailable"));
    assert!(rendered.contains("Total Swap: telemetry unavailable"));
    assert!(!rendered.contains("0% (0 / 0 MiB)"));
}

#[test]
fn total_swap_refuses_a_percentage_when_there_is_no_swap() {
    // A host with no swap is a measured state, not a missing panel: the
    // figures still print, and the percentage slot names the ratio that
    // does not exist instead of inventing a `0%` measured against it.
    //
    // The word `n/a` is the proof of the routing as well as the outcome —
    // only `tier_fill_label`'s `None` arm produces it, so a render that
    // says `n/a` has gone through `tier_fill_pct` and found no
    // denominator. `tier_fill_pct(0, 0) == None` pins that decision too.
    let mut sample = observation(true, false);
    sample.mem = MemoryObservation {
        total_kib: Some(16_384),
        available_kib: Some(8_192),
        swap_total_kib: Some(0),
        swap_free_kib: Some(0),
        ..MemoryObservation::default()
    };

    let backend = TestBackend::new(220, 48);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| draw_dashboard(frame, &sample, &DashHistory::default()))
        .expect("render no-swap host");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert_eq!(tier_fill_pct(0, 0), None);
    assert!(
        rendered.contains("n/a (0 / 0 MiB)"),
        "a host with no swap has no fill percentage: {rendered}"
    );
    // The whole string, not a bare ` 0%`: `100%` also ends in `0%`, and a
    // narrow negative would fire on a measurement that is fine.
    assert!(
        !rendered.contains(" 0% (0 / 0 MiB)"),
        "`0%` claims a fullness measured against a denominator that does not exist: {rendered}"
    );
}

#[test]
fn memory_fills_read_the_exact_kib_figures() {
    // Both rows used to round to whole MiB *before* the division. On these
    // figures the RAM row computed `50% (1 / 2 MiB)` — the difference of
    // two rounded faces, not the face of the difference — and the swap row
    // computed `100% (2 / 2 MiB)` for a tier holding 75%.
    //
    // The comparison is on exact KiB; the MiB an operator reads is the
    // `div_ceil` publish face. `1536` KiB faces as `2`, so both rows read
    // `75% (2 / 2 MiB)` — this panel names the unit once, at the end of
    // the pair, unlike the tier panel which repeats it per figure.
    let mut sample = observation(true, false);
    sample.mem = MemoryObservation {
        total_kib: Some(2048),
        available_kib: Some(512),
        swap_total_kib: Some(2048),
        swap_free_kib: Some(512),
        ..MemoryObservation::default()
    };

    let backend = TestBackend::new(220, 48);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| draw_dashboard(frame, &sample, &DashHistory::default()))
        .expect("render exact-kib fills");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert_eq!(memory_used_pct(&sample.mem), Some(75));
    assert_eq!(published_mib(1536), 2);
    assert!(
        rendered.contains("75% (2 / 2 MiB)"),
        "the fill is the ratio of the exact KiB figures: {rendered}"
    );
    assert!(
        !rendered.contains("100% (2 / 2 MiB)"),
        "`100%` is what the rounded swap halves claim: {rendered}"
    );
    assert!(
        !rendered.contains("50% (1 / 2 MiB)"),
        "`50% (1 / 2)` is the difference of rounded faces, not the measurement: {rendered}"
    );
}

/// The two pairs fail independently. A host whose swap counters never
/// arrived still shows its measured RAM figures; hiding both behind one
/// "telemetry unavailable" was the old collapse, and it threw away a
/// reading the same screen already had.
#[test]
fn dashboard_renders_the_measured_pair_when_the_other_pair_is_absent() {
    let mut sample = observation(true, false);
    sample.memory_scope = MemoryScope::Wsl2;
    sample.mem = MemoryObservation {
        total_kib: Some(16_384),
        available_kib: Some(8_192),
        ..MemoryObservation::default()
    };

    let backend = TestBackend::new(220, 50);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| draw_dashboard(frame, &sample, &DashHistory::default()))
        .expect("render half-absent pairs");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(
        rendered.contains("50%"),
        "a measured RAM pair still renders: {rendered}"
    );
    assert!(
        !rendered.contains("WSL2 Guest RAM: telemetry unavailable"),
        "an absent swap pair must not take the RAM reading with it: {rendered}"
    );
    assert!(
        rendered.contains("Total Swap: telemetry unavailable"),
        "an absent swap pair publishes its own absence: {rendered}"
    );
}

#[test]
fn cache_compaction_refuses_an_absent_sample_and_a_torn_pair() {
    assert_eq!(
        cache_compaction_row(None),
        "VRAM cache compaction: not measured"
    );
    // A stored form larger than the raw one is a tear, not a ratio under
    // 1.00:1. The row refuses rather than inventing a plausible number.
    let torn = cache_compaction_row(Some(&compaction_cache(1024, 4096)));
    assert_eq!(torn, "VRAM cache compaction: not measured");
    // Nothing has gone through the codec: there is no compaction to
    // report, and `1.00:1` would claim one.
    assert_eq!(
        cache_compaction_row(Some(&compaction_cache(0, 0))),
        "VRAM cache compaction: holding nothing"
    );
}

/// SPEC DT-3 matrix: the single derivation reads the zram sysfs
/// `backing_dev` attribute plus the swap table, and every face is a
/// reading or a named absence.
#[test]
fn zram_tier_state_derives_each_face_from_the_backing_attribute() {
    let (root, sys_block) = zram_state_fixture("matrix", Some("/dev/loop0"));
    // No live zram swap device: there is no Tier 1 to describe.
    assert_eq!(
        zram_tier_state_from(&sys_block, &[]),
        ZramTierState::Inactive
    );
    assert_eq!(
        zram_tier_state_from(
            &sys_block,
            &[crate::cascade::SwapEntry {
                filename: "/dev/sdc".to_string(),
                size_kb: 8388608,
                used_kb: 0,
                priority: -2,
            }]
        ),
        ZramTierState::Inactive
    );
    // A bound backing device is the active face.
    assert_eq!(
        zram_tier_state_from(&sys_block, &[zram_swap_entry()]),
        ZramTierState::Active
    );
    // `none` with a live zram swap device is the degraded face.
    fs::write(sys_block.join("zram0/backing_dev"), "none\n").unwrap();
    assert_eq!(
        zram_tier_state_from(&sys_block, &[zram_swap_entry()]),
        ZramTierState::Degraded
    );
    // An attribute that will not read is a named absence, never
    // invented from a peer device.
    fs::remove_file(sys_block.join("zram0/backing_dev")).unwrap();
    assert_eq!(
        zram_tier_state_from(&sys_block, &[zram_swap_entry()]),
        ZramTierState::Unmeasured
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_history_series_fills_from_an_observation_that_carries_the_data() {
    // The chart must not read `not measured` while the tier row on the
    // same frame shows a ratio. One push of a real observation must
    // leave the series non-empty.
    let mut obs = observation(true, true);
    obs.status.insert(
        "zram_compression".to_string(),
        serde_json::json!({
            "orig_data_bytes": 265936896u64,
            "backed_data_bytes": 0_u64,
            "compr_data_bytes": 97734684u64,
            "mem_used_bytes": 102027264u64,
            "huge_pages": 256u64,
            "same_pages": 400u64,
            "compressor": "lzo-rle",
        }),
    );
    let mut history = DashHistory::default();
    history.push_observation(&obs);
    assert_eq!(
        history.zram_saved.len(),
        1,
        "one real observation fills the series"
    );
    assert_eq!(history.zram_saved[0], 63);
}

#[test]
fn zram_compression_from_the_status_document_yields_a_saved_percent() {
    // The exact shape the status document publishes. The chart said
    // `not measured` while the tier row on the same screen showed a
    // ratio, which cannot both be right.
    let v = serde_json::json!({
        "orig_data_bytes": 265936896u64,
        "backed_data_bytes": 0_u64,
        "compr_data_bytes": 97734684u64,
        "mem_used_bytes": 102027264u64,
        "huge_pages": 256u64,
        "same_pages": 400u64,
        "compressor": "lzo-rle",
        "ratio": 2.72,
    });
    assert_eq!(
        compression_saved_pct(Some(&v)),
        Some(63),
        "265936896 -> 97734684 is 63% saved"
    );
}

#[test]
fn cache_occupancy_and_cache_compaction_are_different_quantities() {
    // Full of incompressible pages: high occupancy, no saving.
    let full = compaction_cache(4 * 1024 * 1024, 4 * 1024 * 1024);
    assert_eq!(cache_held_pct(Some(&full)), Some(50));
    assert_eq!(cache_saved_pct(Some(&full)), Some(0));
    // Nearly empty but crushed by the codec: low occupancy, high saving.
    let crushed = compaction_cache(1024 * 1024, 64 * 1024);
    assert_eq!(cache_held_pct(Some(&crushed)), Some(50));
    // 1 MiB -> 64 KiB is 93.75% saved; the chart takes whole percents.
    assert_eq!(cache_saved_pct(Some(&crushed)), Some(93));
}

#[test]
fn ram_history_tracks_available_percent_not_saturation() {
    let mut obs = observation(true, false);
    obs.mem.available_kib = Some(4_096);
    let mut history = DashHistory::default();
    history.push_observation(&obs);
    assert_eq!(history.ram.len(), 1);
    assert_eq!(history.ram[0], 25, "75% used means 25% available");
}

#[test]
fn memory_used_pct_refuses_an_absent_or_disagreeing_pair_instead_of_zero() {
    assert_eq!(memory_used_pct(&MemoryObservation::default()), None);

    let tear = MemoryObservation {
        total_kib: Some(4_096),
        available_kib: Some(8_192),
        ..MemoryObservation::default()
    };
    assert_eq!(memory_used_pct(&tear), None);

    let half_present = MemoryObservation {
        total_kib: Some(4_096),
        ..MemoryObservation::default()
    };
    assert_eq!(memory_used_pct(&half_present), None);

    // The positive face: a measured pair still computes, including the
    // measured empty fill of a machine that really is at 0% used.
    let idle = MemoryObservation {
        total_kib: Some(4_096),
        available_kib: Some(4_096),
        ..MemoryObservation::default()
    };
    assert_eq!(memory_used_pct(&idle), Some(0));
}

#[test]
fn dashboard_live_render_visual_check() {
    if let Ok(obs) = collect_observation(
        &Path::new(PRODUCT_ROOT).join("current/docs/benchmarks/history/latest.json"),
    ) {
        let backend = TestBackend::new(100, 36);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw_dashboard(frame, &obs, &DashHistory::default()))
            .expect("render dashboard");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Memory Tiers"));
        assert!(rendered.contains("Real-Time Speed"));
        assert!(rendered.contains("Throughput Stats"));
        assert!(rendered.contains("Lifetime Traffic"));
    }
}

#[test]
fn format_volume_bytes_formats_all_ranges() {
    assert_eq!(format_volume_bytes(500), "  500 B");
    assert_eq!(format_volume_bytes(1024 * 50), "   50 KB");
    assert_eq!(format_volume_bytes(1024 * 1024 * 250), "250.0 MB");
    assert_eq!(format_volume_bytes(1024 * 1024 * 1024 * 4), " 4.00 GB");
}

#[test]
// TestName: compact_capacity_cell_never_collapses_a_present_figure_to_zero
fn compact_capacity_cell_never_collapses_a_present_figure_to_zero() {
    // The defect: an ad-hoc `value / 1024` printed MiB, so every capacity
    // under one MiB read as `0 MiB` — a present measurement printed as if
    // the quantity were empty. The cell is trimmed for the one-line
    // compact form; the renderer's padding is for TUI cells.
    assert_eq!(compact_capacity_cell(Some(512)), "512 KB");
    assert_eq!(compact_capacity_cell(Some(1)), "1 KB");
    assert_ne!(
        compact_capacity_cell(Some(512)),
        "0 MiB",
        "a present sub-MiB capacity must never read as zero"
    );

    // Whole figures still render at the scale an operator expects, through
    // the same renderer the dashboard uses.
    assert_eq!(compact_capacity_cell(Some(2048)), "2.0 MB");
    // A measured zero is a real reading of emptiness and prints as one.
    assert_eq!(compact_capacity_cell(Some(0)), "0 B");

    // Absence is its own claim: a key the capacity document never carried
    // is `unknown`, not a measured zero.
    assert_eq!(compact_capacity_cell(None), "unknown");
}

/// One frame carries one freshness clock: the compact face ages the cache
/// sample at the observation's own [`Observation::epoch_ms`], never at the
/// wall clock the line happens to be printed on.
///
/// The fixture's stamp is `10_000` — centuries behind real time on
/// purpose. A sample current *at that stamp* must still render as the
/// figure. If the face re-read the clock at print time it would age the
/// sample against `now` and print `stale` for a reading that was live when
/// the frame was taken, which spends the sample-age mark (fact B) on the
/// collection-staleness fact (fact A) that belongs on the panel title.
#[test]
fn compact_report_ages_every_row_at_the_frame_stamp() {
    let frame_stamp = 10_000u64;
    let mut observation = observation(true, false);
    observation.epoch_ms = frame_stamp;
    observation.status.insert(
        "gpu_cache".to_string(),
        serde_json::to_value(sample_cache(4096, frame_stamp)).unwrap(),
    );

    let report = compact_report(&observation);
    // The capacity line is still the face's first line: the extraction is
    // a move of the two print statements, not a rewrite of the compact
    // shape.
    assert!(
        report.starts_with("VRAM cached:"),
        "the compact face keeps its capacity line, got: {report}"
    );
    assert!(
        report.contains("VRAM cache logical:"),
        "a sample current at the frame stamp renders the figure, got: {report}"
    );
    assert!(
        !report.contains("VRAM cache codec telemetry: stale"),
        "a sample current at the frame stamp must not be aged out by a \
             later wall clock: {report}"
    );

    // The same frame, aged at its own stamp: the row refuses it as `stale`.
    let aged_at_frame = sample_cache(4096, frame_stamp - TELEMETRY_MAX_AGE_MS - 1);
    observation.status.insert(
        "gpu_cache".to_string(),
        serde_json::to_value(aged_at_frame).unwrap(),
    );
    let report = compact_report(&observation);
    assert!(
        report.ends_with("VRAM cache codec telemetry: stale"),
        "a sample aged at the frame stamp is stale: {report}"
    );
}

#[test]
fn compression_row_fills_the_bar_with_the_compressed_form_not_the_host_cost() {
    // 4 GB of data held in a 1 GB compressed form that costs the host
    // 1.25 GB once the device's own bookkeeping is counted. The fill is a
    // quarter of the bar — `compr / orig` — so the empty three-quarters are
    // the stored-page shrinkage (`orig − compr` = 3 GB). The host's saving
    // is `orig − mem_used` = 2.75 GB, which is smaller and is not what the
    // bar encodes; the cost is a third number the bar does not encode.
    let sample = serde_json::json!({
        "orig_data_bytes": 4_u64 * 1024 * 1024 * 1024,
        "backed_data_bytes": 0_u64,
        "compr_data_bytes": 1024_u64 * 1024 * 1024,
        "mem_used_bytes": 1342177280_u64,
        "huge_pages": 12,
        "ratio": 4.0,
    });
    let row = compression_row(Some(&sample), 20, false);
    // A quarter of 20 is 5 filled cells; the rest is the stored-page
    // shrinkage, not the host's saving. The fill follows the compressed
    // form, never the cost: 1.25 / 4 would be a different bar and is
    // deliberately not the one drawn.
    assert!(
        row.starts_with(&format!("[{}{}]", "█".repeat(5), "░".repeat(15))),
        "a 4:1 sample must fill a quarter of the bar, got: {row}"
    );
    assert!(row.contains("4.00 GB data"), "got: {row}");
    assert!(row.contains("1.00 GB"), "got: {row}");
    assert!(row.contains("4.00:1"), "got: {row}");
    assert!(row.contains("12 incompressible"), "got: {row}");
    // The cost is labelled and is the figure the host actually pays.
    assert!(row.contains("cost  1.25 GB"), "got: {row}");
}

#[test]
fn compression_row_names_the_kernel_compressor_beside_the_numbers() {
    let sample = serde_json::json!({
        "orig_data_bytes": 3000_u64,
        "backed_data_bytes": 0_u64,
        "compr_data_bytes": 1000_u64,
        "mem_used_bytes": 1200_u64,
        "huge_pages": 2,
        "compressor": "zstd",
        "ratio": 3.0,
    });
    let row = compression_row(Some(&sample), 20, false);
    assert!(row.contains("zstd"), "got: {row}");
    assert!(!row.contains("compressor unreported"), "got: {row}");
    // The compressor is the kernel's answer, so the row never carries a
    // name the sample did not report.
    let other = serde_json::json!({
        "orig_data_bytes": 3000_u64,
        "backed_data_bytes": 0_u64,
        "compr_data_bytes": 1000_u64,
        "mem_used_bytes": 1200_u64,
        "huge_pages": 2,
        "compressor": "lzo-rle",
        "ratio": 3.0,
    });
    let other_row = compression_row(Some(&other), 20, false);
    assert!(other_row.contains("lzo-rle"), "got: {other_row}");
    assert!(!other_row.contains("zstd"), "got: {other_row}");
}

#[test]
fn compression_row_reports_an_unavailable_compressor_as_unreported() {
    // A compressor the kernel did not name is not a default. LZ4 is fourth
    // in the setup walk and must never be attributed by fallback.
    let absent = serde_json::json!({
        "orig_data_bytes": 3000_u64,
        "backed_data_bytes": 0_u64,
        "compr_data_bytes": 1000_u64,
        "mem_used_bytes": 1200_u64,
        "huge_pages": 2,
        "ratio": 3.0,
    });
    let row = compression_row(Some(&absent), 20, false);
    assert!(row.contains("compressor unreported"), "got: {row}");
    assert!(!row.contains("lz4"), "got: {row}");
    assert!(!row.contains("zstd"), "got: {row}");

    let null_name = serde_json::json!({
        "orig_data_bytes": 3000_u64,
        "backed_data_bytes": 0_u64,
        "compr_data_bytes": 1000_u64,
        "mem_used_bytes": 1200_u64,
        "huge_pages": 2,
        "compressor": null,
        "ratio": 3.0,
    });
    assert!(
        compression_row(Some(&null_name), 20, false).contains("compressor unreported"),
        "got: {}",
        compression_row(Some(&null_name), 20, false)
    );

    // An empty name is no name either.
    let empty_name = serde_json::json!({
        "orig_data_bytes": 3000_u64,
        "backed_data_bytes": 0_u64,
        "compr_data_bytes": 1000_u64,
        "mem_used_bytes": 1200_u64,
        "huge_pages": 2,
        "compressor": "",
        "ratio": 3.0,
    });
    assert!(
        compression_row(Some(&empty_name), 20, false).contains("compressor unreported"),
        "got: {}",
        compression_row(Some(&empty_name), 20, false)
    );
}

#[test]
fn compression_row_reports_an_unavailable_host_cost_as_unreported() {
    // The cost is the budget number. When the kernel did not report it the
    // row says so; printing the cost of zero bytes would claim the host
    // pays nothing for data the bar is holding.
    let absent = serde_json::json!({
        "orig_data_bytes": 3000_u64,
        "backed_data_bytes": 0_u64,
        "compr_data_bytes": 1000_u64,
        "huge_pages": 2,
        "compressor": "zstd",
        "ratio": 3.0,
    });
    let row = compression_row(Some(&absent), 20, false);
    assert!(row.contains("cost unreported"), "got: {row}");
    assert!(
        !row.contains(&format!("cost {}", format_volume_bytes(0))),
        "got: {row}"
    );

    let null_cost = serde_json::json!({
        "orig_data_bytes": 3000_u64,
        "backed_data_bytes": 0_u64,
        "compr_data_bytes": 1000_u64,
        "mem_used_bytes": null,
        "huge_pages": 2,
        "compressor": "zstd",
        "ratio": 3.0,
    });
    let null_row = compression_row(Some(&null_cost), 20, false);
    assert!(null_row.contains("cost unreported"), "got: {null_row}");
}

#[test]
fn compression_row_renders_the_graphic_when_only_the_optional_counters_are_absent() {
    // The bar rests on the two byte counts and the ratio. The cost, the
    // compressor and the `huge_pages` count describe that bar and are each
    // optional: missing one of them is not "not measured", it is a sample
    // with a named hole in it.
    let partial = serde_json::json!({
        "orig_data_bytes": 3000_u64,
        "backed_data_bytes": 0_u64,
        "compr_data_bytes": 1000_u64,
        "ratio": 3.0,
    });
    let row = compression_row(Some(&partial), 20, false);
    assert!(
        row.starts_with(&format!("[{}{}]", "█".repeat(6), "░".repeat(14))),
        "a 3:1 sample must fill a third of the bar, got: {row}"
    );
    assert!(row.contains("3.00:1"), "got: {row}");
    assert!(row.contains("cost unreported"), "got: {row}");
    assert!(row.contains("compressor unreported"), "got: {row}");
    assert!(row.contains("incompressible unreported"), "got: {row}");
}
