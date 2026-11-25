// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/tests/secondary_screens_and_status.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;

/// The usage row owns the hold pair's absence. A `used_kib` or `size_kib`
/// the wire never published is not a measured zero pair: the left half
/// says `not measured`, while the peak — the run's occupancy accumulator —
/// still publishes when a sample produced one.
#[test]
fn tier_usage_row_never_publishes_a_measured_zero_from_absent_figures() {
    // Both fields present: the measured pair, with the peak taken against
    // the larger of the observed accumulator and the hold. The face
    // publishes whole MiB (ceil), so 512 KiB reads as 1 MiB — never 0.
    let measured = tier_usage_row(Some(512), Some(2048), Some(1024), 10);
    assert!(measured.contains("1 MiB / 2 MiB"), "{measured}");
    assert!(measured.contains("Peak:"), "{measured}");
    assert!(!measured.contains("not measured"), "{measured}");

    // Absent hold: no bar, no percentage, no MiB pair — and no empty-fill
    // arm over an empty bar beside `0 MiB / 0 MiB`. The accumulated peak
    // still publishes, because a previous frame's sample is a measurement.
    // The pin is the empty-fill arm ` 0% (` , not the bare `0%` substring
    // that a peak share like `50%` would trip.
    let no_used = tier_usage_row(None, Some(2048), Some(1024), 10);
    assert!(no_used.starts_with("not measured"), "{no_used}");
    assert!(no_used.contains("Peak:"), "{no_used}");
    assert!(!no_used.contains("0 MiB / 0 MiB"), "{no_used}");
    assert!(!no_used.contains(" 0% ("), "{no_used}");

    let no_size = tier_usage_row(Some(512), None, Some(1024), 10);
    assert!(no_size.starts_with("not measured"), "{no_size}");
    assert!(no_size.contains("Peak:"), "{no_size}");
    assert!(!no_size.contains("0 MiB / 0 MiB"), "{no_size}");

    // Neither hold field: the same honest half. The peak keeps its number
    // but drops its percentage — no denominator, no share.
    let no_pair = tier_usage_row(None, None, Some(1024), 10);
    assert!(no_pair.starts_with("not measured"), "{no_pair}");
    assert!(no_pair.contains("Peak:    1 MiB"), "{no_pair}");
    assert!(!no_pair.contains('%'), "{no_pair}");

    // An accumulator that never took a sample is not a peak of zero. Only
    // a measured hold can supply the number when the run has none.
    let no_peak = tier_usage_row(None, None, None, 10);
    assert!(no_peak.starts_with("not measured"), "{no_peak}");
    assert!(no_peak.contains("Peak: not measured"), "{no_peak}");
    assert!(!no_peak.contains("Peak:    0 MiB"), "{no_peak}");

    // A hold with no run-wide sample is itself the peak so far.
    let hold_is_the_peak = tier_usage_row(Some(512), Some(2048), None, 10);
    assert!(
        hold_is_the_peak.contains("Peak:    1 MiB"),
        "{hold_is_the_peak}"
    );
    assert!(
        !hold_is_the_peak.contains("not measured"),
        "{hold_is_the_peak}"
    );
}

/// A tier object the wire never gave `used_kib` / `size_kib` did not
/// observe an empty tier with no capacity. The row must say so rather
/// than borrow the measured-zero face or the ARMED-holding-nothing word.
#[test]
fn tier_row_never_publishes_a_measured_zero_from_absent_figures() {
    let mut sample = observation(true, true);
    if let Some(tiers) = sample
        .status
        .get_mut("tiers")
        .and_then(Value::as_object_mut)
        && let Some(zram) = tiers.get_mut("zram").and_then(Value::as_object_mut)
    {
        zram.remove("used_kib");
        zram.remove("size_kib");
    }
    // The run has not taken an occupancy sample either, so the peak on
    // that row is an empty maximum — not a peak of zero.
    sample.control_plane.zram_peak_used_kib = None;
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

    assert!(rendered.contains("not measured"), "{rendered}");
    assert!(rendered.contains("use unknown"), "{rendered}");
    assert!(!rendered.contains("0 MiB / 0 MiB"), "{rendered}");
    assert!(!rendered.contains("ARMED"), "{rendered}");
    assert!(rendered.contains("Peak: not measured"), "{rendered}");
    assert!(!rendered.contains("Peak:    0 MiB"), "{rendered}");
}

/// Either half of the occupancy pair alone is still an absent pair. A
/// measured `used_kib` of 0 without `size_kib` must not print `ARMED`,
/// and a measured `size_kib` without `used_kib` must not print a hold.
#[test]
fn tier_row_never_publishes_armed_when_only_one_figure_key_arrives() {
    fn render_without(keys: &[&str], peak: Option<u64>) -> String {
        let mut sample = observation(true, true);
        if let Some(tiers) = sample
            .status
            .get_mut("tiers")
            .and_then(Value::as_object_mut)
            && let Some(zram) = tiers.get_mut("zram").and_then(Value::as_object_mut)
        {
            for key in keys {
                zram.remove(*key);
            }
        }
        sample.control_plane.zram_peak_used_kib = peak;
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw_dashboard(frame, &sample, &DashHistory::default()))
            .expect("render dashboard");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    }

    for rendered in [
        render_without(&["size_kib"], Some(0)),
        render_without(&["used_kib"], None),
    ] {
        assert!(rendered.contains("use unknown"), "{rendered}");
        assert!(!rendered.contains("ARMED"), "{rendered}");
        assert!(!rendered.contains("0 MiB / 0 MiB"), "{rendered}");
        assert!(!rendered.contains(" 0% ("), "{rendered}");
    }
}

/// A measured absent tier keeps its own face. Removing a figure key must
/// not turn `⚫ OFF` into an unknown, and a measured empty pair must keep
/// publishing `ARMED`.
#[test]
fn tier_row_keeps_the_measured_off_face_and_the_measured_zero_face() {
    let mut off = observation(true, true);
    if let Some(tiers) = off.status.get_mut("tiers").and_then(Value::as_object_mut)
        && let Some(vram) = tiers.get_mut("vram").and_then(Value::as_object_mut)
    {
        vram.insert("present".into(), Value::Bool(false));
        vram.remove("used_kib");
        vram.remove("size_kib");
    }
    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| draw_dashboard(frame, &off, &DashHistory::default()))
        .expect("render dashboard");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("OFF"), "{rendered}");
    assert!(!rendered.contains("use unknown"), "{rendered}");

    let mut armed = observation(true, true);
    if let Some(tiers) = armed.status.get_mut("tiers").and_then(Value::as_object_mut)
        && let Some(vram) = tiers.get_mut("vram").and_then(Value::as_object_mut)
    {
        vram.insert("present".into(), Value::Bool(true));
        vram.insert("used_kib".into(), Value::from(0u64));
        vram.insert("size_kib".into(), Value::from(4096u64));
    }
    armed.control_plane.vram_peak_used_kib = Some(0);
    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| draw_dashboard(frame, &armed, &DashHistory::default()))
        .expect("render dashboard");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("ARMED [2nd"), "{rendered}");
    assert!(rendered.contains("0 MiB /"), "{rendered}");
    assert!(!rendered.contains("use unknown"), "{rendered}");
}

#[test]
fn tier_device_label_joins_several_devices_and_drops_an_empty_list() {
    // One name, or several joined; nothing at all when the observation
    // carried no device, so the header cannot invent one.
    assert_eq!(tier_device_label(&[]), "");
    assert_eq!(tier_device_label(&["nbd0".to_string()]), " (nbd0)");
    assert_eq!(
        tier_device_label(&["zram0".to_string(), "zram1".to_string()]),
        " (zram0+zram1)"
    );
}

#[test]
fn the_tier_header_prints_the_priorities_and_devices_it_observed() {
    // Deliberately not the numbers a product constant would guess, so the
    // test fails if the header ever falls back to them. The device names
    // are the observed basenames and the priorities are the figures the
    // status document folded from the same swap table.
    let mut sample = observation(true, true);
    sample.status.insert(
        "tiers".into(),
        serde_json::json!({
            "zram": { "present": true, "prio": 250, "size_kib": 2048, "used_kib": 512 },
            "vram": { "present": true, "prio": 150, "size_kib": 4096, "used_kib": 1024 },
            "disk": { "present": true, "prio": -5, "size_kib": 8192, "used_kib": 105 }
        }),
    );
    sample.tier_devices = TierDeviceNames {
        zram: vec!["zram9".to_string()],
        vram: vec!["ublk0".to_string()],
        disk: vec!["sdc".to_string()],
    };

    let backend = TestBackend::new(220, 48);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| draw_dashboard(frame, &sample, &DashHistory::default()))
        .expect("render observed tier headers");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(
        rendered.contains("TIER 1: RAM Swap (zram9) ── Priority: 250"),
        "{rendered}"
    );
    assert!(
        rendered.contains("TIER 2: MANAGED SWAP (ublk0) ── Priority: 150"),
        "{rendered}"
    );
    assert!(
        rendered.contains("TIER 3: WSL2 System Disk (sdc) ── Priority:  -5"),
        "{rendered}"
    );
    // The numbers and the device name the header used to hard-code. They
    // must be gone: the panel is not allowed to assert host facts it never
    // read.
    assert!(!rendered.contains("Priority: 200"), "{rendered}");
    assert!(!rendered.contains("Priority: 100"), "{rendered}");
    assert!(!rendered.contains("(nbd0)"), "{rendered}");
}

#[test]
fn the_tier_header_names_a_missing_priority_and_drops_an_unobserved_device() {
    // Absence is named, not filled in. `prio` absent and `prio: null` are
    // the same fact — the producer has no priority for this tier — and an
    // empty device list means the header drops the parenthetical rather
    // than printing a name it did not read.
    let cases = [
        serde_json::json!({
            "zram": { "present": true, "size_kib": 2048, "used_kib": 512 },
            "vram": { "present": true, "size_kib": 4096, "used_kib": 1024 },
            "disk": { "present": true, "size_kib": 8192, "used_kib": 105 }
        }),
        serde_json::json!({
            "zram": { "present": true, "prio": null, "size_kib": 2048, "used_kib": 512 },
            "vram": { "present": true, "prio": null, "size_kib": 4096, "used_kib": 1024 },
            "disk": { "present": true, "prio": null, "size_kib": 8192, "used_kib": 105 }
        }),
    ];
    for tiers in cases {
        let mut sample = observation(true, true);
        sample.status.insert("tiers".into(), tiers);
        sample.tier_devices = TierDeviceNames::default();

        let backend = TestBackend::new(220, 48);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw_dashboard(frame, &sample, &DashHistory::default()))
            .expect("render unreported tier headers");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(
            rendered.contains("TIER 2: MANAGED SWAP ── priority unreported"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("Priority:"),
            "a missing priority must not print a number: {rendered}"
        );
        assert!(
            !rendered.contains("(nbd0)"),
            "an unobserved device must not be named: {rendered}"
        );
    }
}

#[test]
fn the_classifier_and_the_tier_row_share_one_paging_floor() {
    // One fact, one boundary. At `PAGING_MIN_MBS` the tier is paging and
    // the row says so; below the floor the tier is not paging and the row
    // reports quiet. The page-fault latency estimate reads the same
    // predicate, so the screen cannot claim active paging on a tier the
    // estimate ignores — that was the registered comparator gap.
    let at_floor = TierIoStats {
        read_mbs: Some(PAGING_MIN_MBS),
        write_mbs: Some(0.0),
        ..TierIoStats::default()
    };
    assert!(
        tier_is_paging(&at_floor),
        "the floor itself is paging: the name is a minimum, so it is inclusive"
    );

    let below_floor = TierIoStats {
        read_mbs: Some(PAGING_MIN_MBS / 2.0),
        write_mbs: Some(0.0),
        ..TierIoStats::default()
    };
    assert!(
        !tier_is_paging(&below_floor),
        "half the floor is quiet, not paging"
    );

    let above_floor = TierIoStats {
        read_mbs: Some(PAGING_MIN_MBS * 2.0),
        write_mbs: Some(0.0),
        ..TierIoStats::default()
    };
    assert!(tier_is_paging(&above_floor), "above the floor is paging");

    // The write direction carries the same floor as the read direction;
    // either one reaching it is traffic.
    let write_at_floor = TierIoStats {
        read_mbs: Some(0.0),
        write_mbs: Some(PAGING_MIN_MBS),
        ..TierIoStats::default()
    };
    assert!(tier_is_paging(&write_at_floor));
}

#[test]
fn a_tier_at_the_paging_floor_renders_active_not_quiet() {
    // The row's state word is the negation of the classifier, not a
    // second test of the same fact. Pin both sides on one fixture so a
    // future split of the threshold cannot reopen the gap silently.
    let mut paging = observation(true, true);
    paging.control_plane.zram_io.read_mbs = Some(PAGING_MIN_MBS);
    paging.control_plane.zram_io.write_mbs = Some(0.0);
    paging.control_plane.rate_windows = 1;
    assert!(tier_is_paging(&paging.control_plane.zram_io));

    let backend = TestBackend::new(220, 48);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| draw_dashboard(frame, &paging, &DashHistory::default()))
        .expect("render the floor");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    // Assert on the ASCII label body: wide emoji occupy a second cell.
    assert!(rendered.contains("Paging Active"), "{rendered}");

    let mut quiet = observation(true, true);
    quiet.control_plane.zram_io.read_mbs = Some(PAGING_MIN_MBS / 2.0);
    quiet.control_plane.zram_io.write_mbs = Some(0.0);
    quiet.control_plane.rate_windows = 1;
    assert!(!tier_is_paging(&quiet.control_plane.zram_io));

    terminal
        .draw(|frame| draw_dashboard(frame, &quiet, &DashHistory::default()))
        .expect("render below the floor");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(
        !rendered.contains("Paging Active"),
        "below the floor the row must not claim paging: {rendered}"
    );
}
