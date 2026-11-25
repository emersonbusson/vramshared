// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/tests/glossary_and_frame_rendering.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;

#[test]
fn unmeasured_tier_shapes_render_as_zero_bar_and_absent_latency() {
    assert_eq!(make_tier_bar(3, 0, 10), make_bar(0, 10));
    let io = TierIoStats {
        latency_source: LatencySource::Estimated,
        min_lat_us: 0.0,
        avg_lat_us: 2.5,
        max_lat_us: 7.5,
        ..TierIoStats::default()
    };
    assert_eq!(
        format_tier_latency(&io, "probe"),
        "estimated: n/a..2.50µs..7.50µs (probe)"
    );
}

#[test]
fn tier_fill_of_a_zero_capacity_tier_is_not_a_percentage() {
    // `used > 0` against a zero capacity is an inconsistency: there is no
    // denominator, so there is no fill to report. `<1%` would claim a small
    // measurement the division never made, and `0%` would claim the tier is
    // empty. The same rule is what `speedup_label` applies to a ratio over
    // a missing baseline.
    assert_eq!(tier_fill_pct(123, 0), None);
    assert_eq!(tier_fill_pct(0, 0), None);
    assert_eq!(tier_fill_label(123, None), "n/a");
    assert_eq!(tier_fill_label(0, None), "n/a");
    assert_eq!(tier_peak_label(123, 0), "n/a");

    // A real capacity keeps both readings: an empty tier is a measured `0%`,
    // and a nonzero fill that rounds down to zero at this width is `<1%`.
    assert_eq!(tier_fill_pct(0, 195), Some(0));
    assert_eq!(tier_fill_label(0, Some(0)), " 0%");
    assert_eq!(tier_fill_pct(1, 195), Some(0));
    assert_eq!(tier_fill_label(1, Some(0)), "<1%");
    assert_eq!(tier_fill_pct(40, 195), Some(20));
    assert_eq!(tier_fill_label(40, Some(20)), "20%");
    assert_eq!(tier_peak_label(40, 195), "( 20%)");
    // The peak keeps the same two rules: a nonzero share that rounds down
    // to zero is `<1%`, and a real empty peak is the measured `0%`.
    assert_eq!(tier_peak_label(1, 195), "(<1%)");
    assert_eq!(tier_peak_label(0, 195), "(  0%)");
}

#[test]
fn the_tier_fill_percentage_reads_the_exact_kib_figures() {
    // 1536 / 2048 KiB is 75%. Converted to whole MiB first — `(x + 512) /
    // 1024`, the round-to-nearest this replaces — both sides become 2 and
    // the panel printed 100%. The percentage is a ratio of the numbers the
    // wire carries; the MiB beside it is a face and never an input.
    assert_eq!(tier_fill_pct(1536, 2048), Some(75));
    assert_eq!(tier_fill_pct(2, 2), Some(100), "the rounded halves lie");

    let mut sample = observation(true, true);
    sample.status.insert(
        "tiers".into(),
        serde_json::json!({
            "zram": { "present": true, "size_kib": 2048, "used_kib": 1536 },
            "vram": { "present": false, "size_kib": 0, "used_kib": 0 },
            "disk": { "present": false, "size_kib": 0, "used_kib": 0 }
        }),
    );

    let backend = TestBackend::new(220, 48);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| draw_dashboard(frame, &sample, &DashHistory::default()))
        .expect("render exact-ratio tier");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(
        rendered.contains("75% (    2 MiB / 2 MiB )"),
        "the fill is the exact ratio and the MiB beside it is the face: {rendered}"
    );
    assert!(
        !rendered.contains("100% (    2 MiB / 2 MiB )"),
        "100% is what the rounded halves claim: {rendered}"
    );
}

#[test]
fn a_sub_mib_tier_reading_stays_active_and_reads_under_one_percent() {
    // 511 KiB against 2 GiB is a real hold. Round-to-nearest publishes it
    // as `0 MiB`, which made the panel say ARMED and ` 0%` — a tier that
    // is holding pages told the operator it was idle. The status and the
    // sub-percent marker both read the exact figure; only the printed MiB
    // is the face, and the face must not publish zero from non-zero.
    let used_kib = 511u64;
    let size_kib = 2048 * 1024u64;
    assert_eq!(published_mib(used_kib), 1, "a face never publishes zero");
    assert_eq!(tier_fill_pct(used_kib, size_kib), Some(0));
    assert_eq!(tier_fill_label(used_kib, Some(0)), "<1%");

    let mut sample = observation(true, true);
    sample.status.insert(
        "tiers".into(),
        serde_json::json!({
            "zram": { "present": true, "size_kib": size_kib, "used_kib": used_kib },
            "vram": { "present": false, "size_kib": 0, "used_kib": 0 },
            "disk": { "present": false, "size_kib": 0, "used_kib": 0 }
        }),
    );

    let backend = TestBackend::new(220, 48);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| draw_dashboard(frame, &sample, &DashHistory::default()))
        .expect("render sub-MiB tier");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(
        rendered.contains("🟢  ACTIVE [1st Target]"),
        "a present non-zero hold is active whatever its face rounds to: {rendered}"
    );
    assert!(
        !rendered.contains("🟢  ARMED [1st Target]"),
        "ARMED claims the tier holds nothing: {rendered}"
    );
    assert!(
        rendered.contains("<1% (    1 MiB / 2048 MiB )"),
        "{rendered}"
    );
    assert!(
        !rendered.contains(" 0% (    1 MiB / 2048 MiB )"),
        "`0%` claims an empty tier: {rendered}"
    );
}

#[test]
fn a_zero_capacity_tier_renders_no_percentage_on_the_tier_panel() {
    let mut sample = observation(true, true);
    sample.status.insert(
        "tiers".into(),
        serde_json::json!({
            "zram": { "present": true, "size_kib": 0, "used_kib": 1024 },
            "vram": { "present": true, "size_kib": 200_000, "used_kib": 1024 },
            "disk": { "present": true, "size_kib": 200_000, "used_kib": 0 }
        }),
    );

    let backend = TestBackend::new(220, 48);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| draw_dashboard(frame, &sample, &DashHistory::default()))
        .expect("render zero-capacity tier");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    // The row that has used bytes and no capacity is the one under test.
    assert!(
        rendered.contains("n/a (    1 MiB / 0 MiB )"),
        "a zero-capacity tier has no fill percentage: {rendered}"
    );
    assert!(
        !rendered.contains("<1% (    1 MiB / 0 MiB )"),
        "`<1%` claims a measurement the division never made: {rendered}"
    );
    assert!(
        rendered.contains("Peak:    1 MiB n/a"),
        "a peak against a zero capacity is not a percentage either: {rendered}"
    );
    // The control cases keep their real readings: a sub-percent fill on a
    // present capacity is `<1%`, and a present empty tier is a measured 0%.
    //
    // `200_000` KiB is `195.3125` MiB, so its face is `196`. That is the
    // publish conversion doing its job: floor and round-to-nearest both
    // say `195`, and a face that understates a measurement is how a real
    // reading ends up looking smaller than it was.
    assert!(
        rendered.contains("<1% (    1 MiB / 196 MiB )"),
        "the capacity face is div_ceil, not floor: {rendered}"
    );
    assert!(
        rendered.contains(" 0% (    0 MiB / 196 MiB )"),
        "{rendered}"
    );
}

#[test]
fn format_tier_latency_reports_not_measured_for_unavailable_and_empty_samples() {
    // `Unavailable` is a decision, not a fallback: a tier that carries values
    // but no source (a legacy snapshot) must never be rendered as measured.
    let unavailable = TierIoStats {
        latency_source: LatencySource::Unavailable,
        min_lat_us: 85.0,
        avg_lat_us: 180.0,
        max_lat_us: 1200.0,
        ..TierIoStats::default()
    };
    assert_eq!(
        format_tier_latency(&unavailable, "legacy disk"),
        "not measured (legacy disk)"
    );

    // A reported source with no usable sample is the other "not measured"
    // shape. It must not fall through to a `n/a..n/a..n/a` triple.
    for source in [LatencySource::Estimated, LatencySource::Measured] {
        let empty = TierIoStats {
            latency_source: source,
            ..TierIoStats::default()
        };
        assert_eq!(
            format_tier_latency(&empty, "probe"),
            "not measured (probe)",
            "empty sample with a reported source must not render a latency triple"
        );
    }
}

/// The two sample sets diverge exactly where a tier runs below the busy
/// floor: the directional peaks saw the window, the busy trio did not.
#[test]
fn tier_accumulator_first_sample_and_empty_average() {
    let mut acc = TierAccumulator::default();
    assert_eq!(acc.avg_mbs(), None);
    // Below the 5.0 MiB/s measurement floor the window is accepted but no
    // busy sample exists. The directional peaks still took it.
    acc.record(1.0, 2.0);
    assert_eq!(acc.windows, 1);
    assert_eq!(acc.count, 0);
    assert_eq!(acc.min_mbs, 0.0);
    assert_eq!(acc.avg_mbs(), None);
    let mut stats = TierIoStats::default();
    acc.apply_to_plane_io(&mut stats);
    assert_eq!(stats.read_peak_mbs, Some(1.0));
    assert_eq!(stats.write_peak_mbs, Some(2.0));
    assert_eq!(stats.max_mbs, None);
    // First accepted busy sample seeds the minimum instead of minimising
    // with 0.
    acc.record(2.0, 4.0);
    assert_eq!(acc.min_mbs, 6.0);
    assert_eq!(acc.avg_mbs(), Some(6.0));
}

#[test]
fn ensure_parent_creates_missing_directories() {
    let root = std::env::temp_dir().join(format!("ramshared-ensure-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let nested = root.join("a").join("b").join("snapshot.json");
    ensure_parent(&nested).expect("create nested parents");
    assert!(root.join("a").join("b").is_dir());
    // A bare file name has no parent directory; the helper is a no-op.
    ensure_parent(Path::new("snapshot.json")).expect("no parent is fine");
    fs::remove_dir_all(&root).expect("remove ensure fixture");
}

#[test]
fn dashboard_renders_notices_and_risk_states() {
    let metadata = DashboardMetadata {
        version_line: "v".into(),
        running_line: "r".into(),
        install_lines: Vec::new(),
        restart_executable: None,
        runtime_notice: Some("runtime notice".into()),
        update_notice: Some("update notice".into()),
    };
    // Each case pins the product verdict and the protection claim
    // separately. Folding them into one match is what printed
    // `Protection: BLOCKED` beside a live `protection_state: READY`, so a
    // case that only asserts the status half would stay green through that
    // regression.
    for (mut sample, expected_status, expected_protection) in [
        {
            let mut sample = observation(true, true);
            sample
                .status
                .insert("protection_state".into(), Value::from("READY"));
            (sample, "Overall: READY", "Swap: READY")
        },
        {
            let mut sample = observation(true, true);
            sample
                .status
                .insert("protection_state".into(), Value::from("AT_RISK"));
            (sample, "Overall: DEGRADED", "Swap: AT RISK")
        },
        {
            // Alive daemon but the cascade reports not-ok: the product is
            // blocked while the protection plane still claims ACTIVE. The
            // two axes must render as two answers.
            let mut sample = observation(false, false);
            sample
                .status
                .insert("protection_state".into(), Value::from("ACTIVE"));
            sample.status.insert(
                "daemon".into(),
                serde_json::json!({ "alive": true, "pid": 3 }),
            );
            (sample, "Overall: BLOCKED", "Swap: ACTIVE")
        },
        {
            let mut sample = observation(true, true);
            sample
                .status
                .insert("protection_state".into(), Value::from("SOMETHING_ELSE"));
            (sample, "Overall: UNKNOWN", "Swap: UNKNOWN")
        },
    ] {
        let _ = &mut sample;
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                draw_dashboard_with_metadata(frame, &sample, &DashHistory::default(), &metadata)
            })
            .expect("render notices");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("update notice"), "{rendered}");
        assert!(rendered.contains("runtime notice"), "{rendered}");
        assert!(rendered.contains(expected_status), "{rendered}");
        assert!(rendered.contains(expected_protection), "{rendered}");
        if expected_status == "Overall: BLOCKED" {
            assert!(!rendered.contains("Swap: BLOCKED"), "{rendered}");
        }
    }
}

#[test]
fn tier_labels_cover_off_subpercent_and_unbenchmarked_shapes() {
    let mut sample = observation(true, true);
    sample.status.insert(
        "tiers".into(),
        serde_json::json!({
            "zram": { "present": true, "size_kib": 200_000, "used_kib": 1024 },
            "vram": { "present": true, "size_kib": 200_000, "used_kib": 1024 },
            "disk": { "present": false, "size_kib": 0, "used_kib": 0 }
        }),
    );
    // Reclaim has throughput but no latency histogram yet.
    sample.control_plane.reclaim_speed_gbs = 0.75;
    sample.control_plane.reclaim_duration_ms = 30.0;
    sample.control_plane.benchmark_p50_lat_ms = 0.0;
    sample.control_plane.benchmark_p99_lat_ms = 0.0;
    sample.control_plane.benchmark_status = "pending".into();

    let backend = TestBackend::new(220, 48);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| draw_dashboard(frame, &sample, &DashHistory::default()))
        .expect("render off/subpercent shapes");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("TIER 3: WSL2 System Disk"), "{rendered}");
    assert!(rendered.contains("0.75 GB/s (30 ms)"), "{rendered}");
    assert!(rendered.contains("pending"), "{rendered}");
    // zram and vram each show a non-zero used size under 1%.
    let subpercent = rendered.matches("<1%").count();
    assert!(subpercent >= 2, "{rendered}");
}

#[test]
fn tier_labels_cover_armed_off_paging_and_peak_shapes() {
    let mut sample = observation(true, true);
    // zram present but idle -> ARMED; vram absent -> OFF; disk present
    // with a tiny used size -> <1%; high rates -> paging and peak.
    sample.status.insert(
        "tiers".into(),
        serde_json::json!({
            "zram": { "present": true, "size_kib": 2048, "used_kib": 0 },
            "vram": { "present": false, "size_kib": 0, "used_kib": 0 },
            "disk": { "present": true, "size_kib": 200_000, "used_kib": 1024 }
        }),
    );
    sample.control_plane.zram_io.read_mbs = Some(12.0);
    sample.control_plane.zram_io.write_mbs = Some(3.0);
    sample.control_plane.zram_io.read_peak_mbs = Some(12.0);
    sample.control_plane.zram_io.write_peak_mbs = Some(3.0);
    sample.control_plane.zram_io.max_mbs = Some(15.0);
    sample.control_plane.zram_io.avg_mbs = Some(9.0);
    sample.control_plane.swap_read_mbs = Some(2.0);
    sample.control_plane.swap_write_mbs = Some(1.0);
    // Peaks and rates only mean something once a window produced them.
    sample.control_plane.rate_windows = 1;
    sample.control_plane.swap_peak_mbs = Some(3.0);
    sample.control_plane.swap_read_peak_mbs = Some(2.0);
    sample.control_plane.swap_write_peak_mbs = Some(1.0);
    sample.control_plane.reclaim_speed_gbs = 1.5;
    sample.control_plane.reclaim_duration_ms = 40.0;
    sample.control_plane.benchmark_p50_lat_ms = 2.5;
    sample.control_plane.benchmark_p99_lat_ms = 6.0;
    sample.control_plane.benchmark_status = "qualified".into();
    sample.control_plane.estimated_page_fault_lat_us = 12.0;

    let backend = TestBackend::new(220, 48);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| draw_dashboard(frame, &sample, &DashHistory::default()))
        .expect("render tier shapes");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    // Assert on the ASCII label bodies: wide emoji occupy a second cell,
    // so the assembled buffer is not the source literal.
    assert!(rendered.contains("ARMED [1st Target]"), "{rendered}");
    assert!(rendered.contains("TIER 2: MANAGED SWAP"), "{rendered}");
    assert!(rendered.contains("<1%"), "{rendered}");
    assert!(rendered.contains("Paging Active"), "{rendered}");
    assert!(rendered.contains("Burst:"), "{rendered}");
    assert!(rendered.contains("P50 2.50ms"), "{rendered}");
    assert!(rendered.contains("estimated 12.00"), "{rendered}");
}

#[test]
fn swap_tier_device_names_groups_by_role_and_dedupes_shared_basenames() {
    // The header's identity comes from this fold, so the fold has to be
    // exact about three things the header would otherwise get wrong: a
    // tier can be served by more than one device, two paths can share one
    // kernel name, and a device the kernel already lost has no name to
    // print. `/dev/foo/bar` and `/dev/bar` are two distinct swap
    // identities that both present as the kernel name `bar`, which is why
    // the list is a deduplicated set of basenames rather than one entry
    // per row. Ghosts are covered separately: `parse_proc_swaps` refuses a
    // snapshot that repeats an identity, so a live `nbd1` and a ghost
    // `nbd1` cannot both be present in one table.
    let swaps = "Filename             Type       Size       Used  Priority\n\
/dev/zram0           partition  2048       100   200\n\
/dev/zram1           partition  1024       0     150\n\
/dev/nbd0            partition  4096       50    100\n\
/dev/bar             partition  8192       0     -2\n\
/dev/foo/bar         file       4096       0     -3\n\
/srv/swap.bin        file       8192       0     -4\n";
    let entries = cascade::parse_proc_swaps(swaps).expect("strict swap fixture");
    assert_eq!(entries.len(), 6, "{swaps}");

    let names = observations::swap_tier_device_names(&entries);
    assert_eq!(names.zram, vec!["zram0".to_string(), "zram1".to_string()]);
    assert_eq!(names.vram, vec!["nbd0".to_string()]);
    // Two rows, one kernel name: `bar` once, and the swap file by its
    // basename.
    assert_eq!(names.disk, vec!["bar".to_string(), "swap.bin".to_string()]);
}

#[test]
fn swap_tier_device_names_skips_a_ghost_device() {
    // A ghost names a device the kernel already lost. It has no live
    // identity to print and no live I/O behind its row, so the header
    // drops it rather than naming a device that is not there.
    let swaps = "Filename             Type       Size       Used  Priority\n\
/dev/nbd1 (deleted)  partition  1024       0     -2\n\
/dev/zram0           partition  2048       100   200\n";
    let entries = cascade::parse_proc_swaps(swaps).expect("strict swap fixture");
    assert_eq!(entries.len(), 2, "{swaps}");
    assert!(entries[0].is_ghost(), "{swaps}");

    let names = observations::swap_tier_device_names(&entries);
    assert!(
        names.vram.is_empty(),
        "ghost must not name a device: {names:?}"
    );
    assert_eq!(names.zram, vec!["zram0".to_string()]);
}

#[test]
fn tier_priority_label_distinguishes_a_number_from_its_absence() {
    // A number is the observed swap priority. Absence is a word, never a
    // product constant: a host whose tier 2 sits at 150 must not be told
    // it sits at 100 by a hard-coded header.
    assert_eq!(tier_priority_label(Some(200)), "Priority: 200");
    assert_eq!(tier_priority_label(Some(100)), "Priority: 100");
    assert_eq!(tier_priority_label(Some(-2)), "Priority:  -2");
    assert_eq!(tier_priority_label(Some(150)), "Priority: 150");
    assert_eq!(tier_priority_label(None), "priority unreported");
}

/// The tier presence row owns five states. `⚫ OFF` is the measured
/// absent arm and is never borrowed for a `present` field that never
/// arrived; `⚪ use unknown` is a present tier whose occupancy pair the
/// wire never completed and is not a measured empty.
#[test]
fn tier_presence_status_distinguishes_absence_from_a_measured_off() {
    assert_eq!(
        tier_presence_status(Some(false), Some(0), Some(2048), "HOLDING", "EMPTY"),
        "⚫ OFF"
    );
    assert_eq!(
        tier_presence_status(Some(false), Some(1024), Some(2048), "HOLDING", "EMPTY"),
        "⚫ OFF"
    );
    assert_eq!(
        tier_presence_status(Some(false), None, None, "HOLDING", "EMPTY"),
        "⚫ OFF"
    );
    assert_eq!(
        tier_presence_status(Some(true), Some(1024), Some(2048), "HOLDING", "EMPTY"),
        "HOLDING"
    );
    assert_eq!(
        tier_presence_status(Some(true), Some(0), Some(2048), "HOLDING", "EMPTY"),
        "EMPTY"
    );
    assert_eq!(
        tier_presence_status(Some(true), None, Some(2048), "HOLDING", "EMPTY"),
        "⚪ use unknown"
    );
    assert_eq!(
        tier_presence_status(None, Some(1024), Some(2048), "HOLDING", "EMPTY"),
        "⚪ not measured"
    );
    assert_eq!(
        tier_presence_status(None, Some(0), Some(2048), "HOLDING", "EMPTY"),
        "⚪ not measured"
    );
    assert_eq!(
        tier_presence_status(None, None, None, "HOLDING", "EMPTY"),
        "⚪ not measured"
    );
}

/// A present tier whose figure pair did not arrive publishes the
/// unmeasured face — never `0` and never `ARMED`-holding-nothing. This is
/// the verified test case face: a missing `used_kib` / `size_kib` key is
/// absence, not a measured zero occupancy.
#[test]
fn tier_presence_never_publishes_armed_from_an_absent_figure_key() {
    // Both keys missing: presence was measured, occupancy was not.
    assert_eq!(
        tier_presence_status(Some(true), None, None, "HOLDING", "EMPTY"),
        "⚪ use unknown"
    );
    // Either key alone is still an incomplete pair. A measured `used_kib`
    // of 0 without its capacity is not "armed tier holding nothing" — the
    // pair the producer never completed cannot support that claim.
    assert_eq!(
        tier_presence_status(Some(true), Some(0), None, "HOLDING", "EMPTY"),
        "⚪ use unknown"
    );
    assert_eq!(
        tier_presence_status(Some(true), Some(1024), None, "HOLDING", "EMPTY"),
        "⚪ use unknown"
    );
    assert_eq!(
        tier_presence_status(Some(true), None, Some(2048), "HOLDING", "EMPTY"),
        "⚪ use unknown"
    );
}

/// A present tier whose figures did arrive publishes those measured
/// figures, including a measured `0`.
#[test]
fn tier_presence_publishes_the_measured_pair_including_a_measured_zero() {
    assert_eq!(
        tier_presence_status(Some(true), Some(1024), Some(2048), "HOLDING", "EMPTY"),
        "HOLDING"
    );
    assert_eq!(
        tier_presence_status(Some(true), Some(0), Some(2048), "HOLDING", "EMPTY"),
        "EMPTY"
    );
    // A measured zero of a zero-capacity pair is still a measurement.
    assert_eq!(
        tier_presence_status(Some(true), Some(0), Some(0), "HOLDING", "EMPTY"),
        "EMPTY"
    );
}

/// A tier object the wire never gave a `present` field did not observe an
/// absent tier. The row must say so rather than borrow the measured-off
/// face.
#[test]
fn tier_row_never_publishes_a_measured_off_from_an_absent_presence() {
    let mut sample = observation(true, true);
    if let Some(tiers) = sample
        .status
        .get_mut("tiers")
        .and_then(Value::as_object_mut)
        && let Some(zram) = tiers.get_mut("zram").and_then(Value::as_object_mut)
    {
        zram.remove("present");
    }
    let backend = TestBackend::new(120, 40);
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

    assert!(rendered.contains("priority unreported ── ⚪"), "{rendered}");
    // The OFF face is two cells wider than its symbol, so a pin on
    // `⚫ OFF` would pass even when the measured-off face is on screen.
    assert!(!rendered.contains("OFF"), "{rendered}");
}
