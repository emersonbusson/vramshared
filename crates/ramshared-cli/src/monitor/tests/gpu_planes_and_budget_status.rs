// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/tests/gpu_planes_and_budget_status.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;

/// The header fold refuses the positive word without a backing daemon
/// and refuses `BLOCKED` without a refutation. An absent liveness is
/// neither.
#[test]
fn header_never_publishes_a_refutation_from_an_absent_daemon_liveness() {
    let mut sample = observation(true, true);
    sample.status.remove("daemon");
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

    assert!(rendered.contains("Overall: UNKNOWN"), "{rendered}");
    assert!(rendered.contains("Swap: UNKNOWN"), "{rendered}");
    assert!(!rendered.contains("Overall: BLOCKED"), "{rendered}");
    assert!(!rendered.contains("Swap: BLOCKED"), "{rendered}");
}

/// Hours, minutes and seconds are named once the total has them, and the
/// seconds field can never exceed 59.
#[test]
fn format_uptime_seconds_names_hours_and_never_overflows_the_seconds_field() {
    assert_eq!(format_uptime_seconds(0), "0s");
    assert_eq!(format_uptime_seconds(30), "30s");
    assert_eq!(format_uptime_seconds(59), "59s");
    assert_eq!(format_uptime_seconds(60), "1m 00s");
    assert_eq!(format_uptime_seconds(90), "1m 30s");
    assert_eq!(format_uptime_seconds(3599), "59m 59s");
    assert_eq!(format_uptime_seconds(3600), "1h 00m 00s");
    // The multi-hour host the header has to read at a glance: 22h 11m 30s
    // rather than a bare 1331m 30s.
    assert_eq!(format_uptime_seconds(79_890), "22h 11m 30s");
}

/// The seconds component is `0..=59` for every total. This is the pin
/// against a header that prints a three-digit second count.
#[test]
fn format_uptime_seconds_keeps_the_seconds_component_below_sixty() {
    for uptime in 0..=200_000u64 {
        let rendered = format_uptime_seconds(uptime);
        let seconds = rendered
            .rsplit(' ')
            .next()
            .and_then(|field| field.strip_suffix('s'))
            .and_then(|digits| digits.parse::<u64>().ok())
            .unwrap_or_else(|| panic!("no seconds field in {rendered:?}"));
        assert!(seconds < 60, "uptime {uptime} rendered {rendered:?}");
    }
}

#[test]
fn dashboard_does_not_invent_boot_tier_latency_when_unmeasured() {
    let mut sample = observation(true, true);
    sample.control_plane.boot_tier_latency_ms = None;
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

    assert!(rendered.contains("Boot Initialization"));
    assert!(rendered.contains("not measured"));
    assert!(!rendered.contains("3.12s"));
    assert!(!rendered.contains("Tier Ready"));
}

/// The boot figure is the tier unit's activation duration and only that.
/// It never publishes a readiness state: a duration that completed says
/// nothing about whether the tier is usable now, and the daemon row is
/// the one that measures that. It also never publishes a display zero —
/// a few milliseconds is a real activation and prints `<0.01s`, not the
/// `0.00s` the rounding would otherwise produce.
#[test]
fn boot_row_publishes_an_activation_duration_and_never_a_readiness_state() {
    let render_boot = |ms: Option<u64>| {
        let mut sample = observation(true, true);
        sample.control_plane.boot_tier_latency_ms = ms;
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
    };

    // A measured activation publishes its duration and no state word.
    let measured = render_boot(Some(2890));
    assert!(measured.contains("2.89s"), "{measured}");
    assert!(!measured.contains("Tier Ready"), "{measured}");

    // A sub-10 ms activation is real and must not round to the measured
    // zero's face.
    let fast = render_boot(Some(4));
    assert!(fast.contains("<0.01s"), "{fast}");
    assert!(!fast.contains("0.00s"), "{fast}");
    assert!(!fast.contains("Tier Ready"), "{fast}");

    // A measured zero is a measured zero and still carries no state word.
    let instant = render_boot(Some(0));
    assert!(instant.contains("0.00s"), "{instant}");
    assert!(!instant.contains("Tier Ready"), "{instant}");
}

/// The footer's two allocation facts come from the frame's own witnesses
/// and never from a banner. The old footer printed
/// `ALLOCATION GUARANTEE: All new writes fill RAM (1st) and VRAM (2nd)
/// before SSD.` and `SSD usage is cold WSL2 boot baseline.` on every
/// frame — including the frames whose swap priorities and SSD growth
/// refute both. A claim the same screen can contradict is a claim the
/// reader learns to skip.
#[test]
fn allocation_footer_is_derived_and_never_an_unconditional_guarantee() {
    let render_footer = |order_ok: Option<serde_json::Value>, growth: Option<serde_json::Value>| {
        let mut sample = observation(true, true);
        match order_ok {
            Some(value) => {
                sample.status.insert("order_ok".to_string(), value);
            }
            None => {
                sample.status.remove("order_ok");
            }
        }
        if let Some(activation) = sample.status.get_mut("activation")
            && let Some(obj) = activation.as_object_mut()
        {
            match growth {
                Some(value) => {
                    obj.insert("disk_growth_kib".to_string(), value);
                }
                None => {
                    obj.insert("disk_growth_kib".to_string(), Value::Null);
                }
            }
        }
        let backend = TestBackend::new(120, 50);
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
    };

    // The old banner is gone from every face of the footer.
    let witnessed = render_footer(Some(Value::from(true)), Some(Value::from(0u64)));
    assert!(!witnessed.contains("ALLOCATION GUARANTEE"), "{witnessed}");
    assert!(
        !witnessed.contains("All new writes fill RAM"),
        "{witnessed}"
    );
    assert!(
        !witnessed.contains("SSD usage is cold WSL2 boot baseline"),
        "{witnessed}"
    );

    // A held order and a measured at-baseline SSD publish both facts.
    // The order itself is the legend at the foot of the dashboard; this
    // row reports the frame's witness for it, and the tokens are short
    // enough to survive the Diagnostics panel at an 80-column terminal.
    assert!(
        witnessed.contains("Allocation Order:  holding"),
        "{witnessed}"
    );
    assert!(
        witnessed.contains("SSD vs Baseline:     at baseline"),
        "{witnessed}"
    );

    // A frame whose priorities refute the order says so and never prints
    // the order as though it were holding.
    let reordered = render_footer(Some(Value::from(false)), Some(Value::from(0u64)));
    assert!(
        reordered.contains("Allocation Order:  refuted"),
        "{reordered}"
    );
    assert!(
        !reordered.contains("Allocation Order:  holding"),
        "{reordered}"
    );

    // SSD growth above the baseline is the frame refuting "at baseline",
    // and the number is the field's own, not a rounded one.
    let grown = render_footer(Some(Value::from(true)), Some(Value::from(48u64)));
    assert!(grown.contains("SSD vs Baseline:     +48 KiB"), "{grown}");
    assert!(!grown.contains("at baseline"), "{grown}");

    // No attributable baseline is absence, never a zero growth.
    let unmeasured = render_footer(Some(Value::from(true)), None);
    assert!(
        unmeasured.contains("SSD vs Baseline:     not measured"),
        "{unmeasured}"
    );
    assert!(!unmeasured.contains("at baseline"), "{unmeasured}");

    // No order witness is absence too.
    let no_order = render_footer(None, Some(Value::from(0u64)));
    assert!(
        no_order.contains("Allocation Order:  not measured"),
        "{no_order}"
    );
}

#[test]
fn dashboard_marks_failed_refresh_values_as_stale() {
    let mut sample = observation(false, false);
    sample.control_plane.memory_psi_available = true;
    sample.control_plane.swap_read_mbs = Some(12.3);
    sample.control_plane.swap_write_mbs = Some(4.5);
    mark_observation_refresh_failed(&mut sample, Duration::from_millis(1250));

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

    assert!(rendered.contains("Pressure: stale"));
    assert!(rendered.contains("Read: stale"));
    assert!(rendered.contains("Write: stale"));
    assert!(rendered.contains("Telemetry Sample Age:     1250 ms"));
    assert!(!rendered.contains("Read: 12.3"));
    assert!(!rendered.contains("Write: 4.5"));
}

#[test]
fn dashboard_marks_tier_and_gpu_panels_stale_with_the_control_panel() {
    // The control panel already calls a failed refresh `Read: stale`. A
    // tier panel still printing confident speeds on the same screen is the
    // monitor contradicting itself, and a frozen rate is then
    // indistinguishable from a live one.
    let mut sample = observation(false, true);
    sample.control_plane.zram_io.read_mbs = Some(12.3);
    sample.control_plane.zram_io.write_mbs = Some(4.5);
    sample.control_plane.zram_io.read_peak_mbs = Some(20.0);
    sample.control_plane.zram_io.write_peak_mbs = Some(18.0);
    sample.control_plane.zram_io.max_mbs = Some(30.0);
    sample.control_plane.zram_io.avg_mbs = Some(12.0);
    sample.status.insert(
        "gpu_utilization".to_string(),
        serde_json::json!({ "gpu_pct": 55, "memory_pct": 40 }),
    );
    mark_observation_refresh_failed(&mut sample, Duration::from_millis(1250));

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

    assert!(
        !rendered.contains("Paging Active"),
        "a frozen tier rate must not be presented as live paging: {rendered}"
    );
    assert!(
        !rendered.contains("Burst:"),
        "a frozen throughput statistic must not be presented as live: {rendered}"
    );
    assert!(
        !rendered.contains("55%") && !rendered.contains("40%"),
        "a frozen GPU busy percentage must not be presented as live: {rendered}"
    );
    // Every panel of level figures carries the one collection-staleness
    // mark on its title, and it is the blessed spelling — not a second
    // wording each panel invented (`GPU (stale)`, `... (stale)`).
    assert!(rendered.contains("Swap Priority & Speedup) (⚠ Stale Sample)"));
    assert!(rendered.contains("GPU (⚠ Stale Sample)"));
    assert!(rendered.contains("Diagnostics & Live Stats (⚠ Stale Sample)"));
    assert!(rendered.contains("Host RAM & Swap (⚠ Stale Sample)"));
    // The levels keep their values and the rows carry no second mark: the
    // notice lives on the title once per panel.
    assert!(
        !rendered.contains("Host RAM (stale)"),
        "a level row must not carry a second wording of the panel notice: {rendered}"
    );
    assert!(
        rendered.contains("Overall: OFF (sample stale)"),
        "the verdict must carry the collection-staleness dimension: {rendered}"
    );
}

#[test]
fn rate_window_follows_the_configured_interval() {
    // The parser accepts `--interval-ms` up to 60_000, so every accepted
    // interval must be able to produce a rate. A window fixed at ten
    // seconds silently disabled every measurement above it while Lifetime
    // Traffic kept climbing beside a permanent `0.0 … Standby`.
    assert_eq!(rate_window_secs(250), (0.05, 10.0));
    assert_eq!(rate_window_secs(1_000), (0.05, 10.0));
    assert_eq!(rate_window_secs(15_000), (0.05, 150.0));
    assert_eq!(rate_window_secs(60_000), (0.05, 600.0));
}

#[test]
fn rate_window_still_refuses_a_dt_outside_its_bounds() {
    // The pre-loop seed of `last_io_sample` sits immediately before the
    // first refresh, so its dt is near zero and is not a rate over any
    // window. A blocked refresh leaves a dt spanning the gap; ten periods
    // is the ceiling, and a longer gap is not a per-interval rate.
    let (min_dt, max_dt) = rate_window_secs(15_000);
    assert!(!(min_dt..=max_dt).contains(&0.0));
    assert!(!(min_dt..=max_dt).contains(&0.049));
    assert!((min_dt..=max_dt).contains(&15.0));
    assert!((min_dt..=max_dt).contains(&150.0));
    assert!(!(min_dt..=max_dt).contains(&150.001));
}

#[test]
fn a_rate_with_no_window_is_absent_and_a_measured_idle_is_not() {
    // A rate is a byte delta over an accepted window. No window means the
    // rate was never computed, which is not the same fact as a window that
    // computed zero. The pair is written together, so half a pair is no
    // sample at all.
    let unmeasured = TierIoStats::default();
    assert!(!unmeasured.has_rate_sample());
    assert!(!tier_is_paging(&unmeasured));
    assert_eq!(format_rate_cell(unmeasured.read_mbs), "n/a");

    let half = TierIoStats {
        read_mbs: Some(12.0),
        ..TierIoStats::default()
    };
    assert!(!half.has_rate_sample());

    let measured_idle = TierIoStats {
        read_mbs: Some(0.0),
        write_mbs: Some(0.0),
        ..TierIoStats::default()
    };
    assert!(measured_idle.has_rate_sample());
    assert!(!tier_is_paging(&measured_idle));
    assert_eq!(format_rate_cell(measured_idle.read_mbs), "0.0");

    let paging = TierIoStats {
        read_mbs: Some(12.0),
        write_mbs: Some(0.0),
        ..TierIoStats::default()
    };
    assert!(tier_is_paging(&paging));
}

#[test]
fn fault_rates_with_no_window_are_absent_and_a_measured_zero_is_not() {
    assert_eq!(format_page_fault_rates(None, None), "not measured");
    assert_eq!(
        format_page_fault_rates(Some(42), Some(2)),
        "42/s (Minor: 40/s │ Major: 2/s)"
    );
    // A measured idle is a real observation and must survive as one.
    assert_eq!(
        format_page_fault_rates(Some(0), Some(0)),
        "0/s (Minor: 0/s │ Major: 0/s)"
    );
    // Both counters come from one window: a half pair is no sample.
    assert_eq!(format_page_fault_rates(Some(42), None), "not measured");
    assert_eq!(format_page_fault_rates(None, Some(2)), "not measured");
}

#[test]
fn a_rate_with_no_window_serializes_as_null_and_a_legacy_zero_still_loads() {
    // The rates are a published wire form. `null` is the established "not
    // measured" marker here; a present `0.0` is a measured idle and must
    // keep loading as one.
    let unmeasured = serde_json::to_value(TierIoStats::default()).unwrap();
    assert_eq!(unmeasured["read_mbs"], Value::Null);
    assert_eq!(unmeasured["write_mbs"], Value::Null);

    let mut legacy = serde_json::to_value(TierIoStats::default()).unwrap();
    let legacy_object = legacy.as_object_mut().unwrap();
    legacy_object.insert("read_mbs".to_string(), 0.0.into());
    legacy_object.insert("write_mbs".to_string(), 0.0.into());
    let loaded = serde_json::from_value::<TierIoStats>(legacy).unwrap();
    assert_eq!(loaded.read_mbs, Some(0.0));
    assert_eq!(loaded.write_mbs, Some(0.0));
    assert!(loaded.has_rate_sample());

    // An envelope that predates the widening simply lacks the keys.
    let mut older = serde_json::to_value(TierIoStats::default()).unwrap();
    let older_object = older.as_object_mut().unwrap();
    older_object.remove("read_mbs");
    older_object.remove("write_mbs");
    let loaded = serde_json::from_value::<TierIoStats>(older).unwrap();
    assert!(!loaded.has_rate_sample());

    let plane = serde_json::to_value(ControlPlaneObservation::default()).unwrap();
    assert_eq!(plane["swap_read_mbs"], Value::Null);
    assert_eq!(plane["pgfault_per_sec"], Value::Null);
    assert_eq!(plane["rate_windows"], 0);
}

/// The run-wide peaks are the same wire rule as the rates, applied to a
/// maximum instead of a window: an empty maximum publishes `null`, and a
/// present `0.0` is a measured idle that must keep loading as one.
#[test]
fn a_peak_with_no_window_serializes_as_null_and_a_measured_zero_still_loads() {
    let unmeasured = serde_json::to_value(TierIoStats::default()).unwrap();
    assert_eq!(unmeasured["read_peak_mbs"], Value::Null);
    assert_eq!(unmeasured["write_peak_mbs"], Value::Null);
    assert_eq!(unmeasured["min_mbs"], Value::Null);
    assert_eq!(unmeasured["avg_mbs"], Value::Null);
    assert_eq!(unmeasured["max_mbs"], Value::Null);
    assert_eq!(unmeasured["peak_mbs"], Value::Null);

    // An envelope from before the widening carried a bare `0.0`. That was
    // the bug's output, not its contract: it still loads as a measured
    // zero, which is what the number said.
    let mut legacy = serde_json::to_value(TierIoStats::default()).unwrap();
    let legacy_object = legacy.as_object_mut().unwrap();
    for key in [
        "read_peak_mbs",
        "write_peak_mbs",
        "min_mbs",
        "avg_mbs",
        "max_mbs",
        "peak_mbs",
    ] {
        legacy_object.insert(key.to_string(), 0.0.into());
    }
    let loaded = serde_json::from_value::<TierIoStats>(legacy).unwrap();
    assert_eq!(loaded.read_peak_mbs, Some(0.0));
    assert_eq!(loaded.max_mbs, Some(0.0));

    // An envelope that predates the peaks simply lacks the keys.
    let mut older = serde_json::to_value(TierIoStats::default()).unwrap();
    let older_object = older.as_object_mut().unwrap();
    for key in ["read_peak_mbs", "max_mbs"] {
        older_object.remove(key);
    }
    let loaded = serde_json::from_value::<TierIoStats>(older).unwrap();
    assert_eq!(loaded.read_peak_mbs, None);
    assert_eq!(loaded.max_mbs, None);

    let plane = serde_json::to_value(ControlPlaneObservation::default()).unwrap();
    assert_eq!(plane["swap_peak_mbs"], Value::Null);
    assert_eq!(plane["swap_read_peak_mbs"], Value::Null);
    assert_eq!(plane["swap_write_peak_mbs"], Value::Null);
    assert_eq!(plane["rate_windows"], 0);
}

#[test]
fn the_first_frame_reports_an_unmeasured_rate_instead_of_a_standby_idle() {
    // The pre-loop seed makes the first dt near zero, which is refused.
    // Nothing has been measured yet, and the panel used to answer that
    // with a confident `0.0 … Standby` and an `Idle (Awaiting Workload)`
    // nobody had observed.
    let sample = observation(true, true);
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

    assert!(
        rendered.contains("Read:   n/a │ Write:   n/a MB/s (not measured)"),
        "{rendered}"
    );
    // Every tier with no window says so, not just the first one.
    assert!(
        rendered.matches("MB/s (not measured)").count() >= 3,
        "{rendered}"
    );
    assert!(
        rendered.contains("Throughput Stats:   not measured"),
        "{rendered}"
    );
    assert!(
        rendered.contains("Peak Recorded Speed:      not measured"),
        "{rendered}"
    );
    // Assert the ASCII body only: the chart glyph is a wide symbol.
    assert!(rendered.contains("Page Faults Rate:"), "{rendered}");
    assert!(!rendered.contains("/s (Minor:"), "{rendered}");
    assert!(!rendered.contains("Standby"), "{rendered}");
    assert!(!rendered.contains("Idle (Awaiting Workload)"), "{rendered}");
    assert!(!rendered.contains("Paging Active"), "{rendered}");
    assert!(!rendered.contains("Read:   0.0"), "{rendered}");
}

#[test]
fn a_measured_idle_still_renders_the_standby_row() {
    // The negative arm: a window that really did see no traffic still
    // reports the idle it measured. Only the absence changes.
    let mut sample = observation(true, true);
    sample.control_plane.rate_windows = 1;
    sample.control_plane.swap_read_mbs = Some(0.0);
    sample.control_plane.swap_write_mbs = Some(0.0);
    // One accepted window that saw no traffic. The directional and
    // swap-plane peaks take every window, so their maximum over that one
    // window is a measured `0.0`. The busy trio's sample set is the
    // windows that reached the floor, which this one did not: that set is
    // empty and the trio is `null`, never a busy zero. The row still
    // reports the idle it measured.
    sample.control_plane.zram_io.read_mbs = Some(0.0);
    sample.control_plane.zram_io.write_mbs = Some(0.0);
    sample.control_plane.zram_io.read_peak_mbs = Some(0.0);
    sample.control_plane.zram_io.write_peak_mbs = Some(0.0);
    sample.control_plane.zram_io.max_mbs = None;
    sample.control_plane.zram_io.avg_mbs = None;
    sample.control_plane.swap_peak_mbs = Some(0.0);
    sample.control_plane.swap_read_peak_mbs = Some(0.0);
    sample.control_plane.swap_write_peak_mbs = Some(0.0);
    // One accepted window writes the rates of every tier together, so a
    // fixture that claims one cannot leave two of them unmeasured.
    sample.control_plane.vram_io.read_mbs = Some(0.0);
    sample.control_plane.vram_io.write_mbs = Some(0.0);
    sample.control_plane.vram_io.read_peak_mbs = Some(0.0);
    sample.control_plane.vram_io.write_peak_mbs = Some(0.0);
    sample.control_plane.vram_io.max_mbs = None;
    sample.control_plane.vram_io.avg_mbs = None;
    sample.control_plane.disk_io.read_mbs = Some(0.0);
    sample.control_plane.disk_io.write_mbs = Some(0.0);
    sample.control_plane.disk_io.read_peak_mbs = Some(0.0);
    sample.control_plane.disk_io.write_peak_mbs = Some(0.0);
    sample.control_plane.disk_io.max_mbs = None;
    sample.control_plane.disk_io.avg_mbs = None;

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

    assert!(
        rendered.contains("Read:   0.0 │ Write:   0.0 MB/s"),
        "{rendered}"
    );
    assert!(rendered.contains("Standby"), "{rendered}");
    assert!(rendered.contains("Idle (Awaiting Workload)"), "{rendered}");
    assert!(!rendered.contains("MB/s (not measured)"), "{rendered}");
    assert!(
        !rendered.contains("Throughput Stats:   not measured"),
        "{rendered}"
    );
}

/// The busy trio and the directional peaks are different sample sets, so
/// a tier that moved but never crossed the busy floor has real peaks and
/// no Burst/Avg statistic. The row shows what it has and drops the clause
/// rather than filling it with a zero nobody measured.
#[test]
fn a_tier_that_moved_below_the_busy_floor_drops_the_burst_clause() {
    let mut sample = observation(true, true);
    sample.control_plane.rate_windows = 2;
    // One accepted window writes every tier together, so the fixture keeps
    // the three directional peaks coherent and leaves the three busy
    // trios empty together.
    sample.control_plane.zram_io.read_mbs = Some(3.2);
    sample.control_plane.zram_io.write_mbs = Some(0.4);
    sample.control_plane.zram_io.read_peak_mbs = Some(4.0);
    sample.control_plane.zram_io.write_peak_mbs = Some(1.0);
    sample.control_plane.zram_io.max_mbs = None;
    sample.control_plane.zram_io.avg_mbs = None;
    sample.control_plane.vram_io.read_peak_mbs = Some(2.0);
    sample.control_plane.vram_io.write_peak_mbs = Some(0.5);
    sample.control_plane.disk_io.read_peak_mbs = Some(2.5);
    sample.control_plane.disk_io.write_peak_mbs = Some(0.5);

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

    assert!(
        rendered.contains("Peak:"),
        "the directional peaks are measured and must render: {rendered}"
    );
    assert!(
        !rendered.contains("Burst:"),
        "an empty busy set must not be printed as a burst of zero: {rendered}"
    );
    assert!(
        !rendered.contains("Idle (Awaiting Workload)"),
        "a tier that moved is not idle: {rendered}"
    );
    assert!(
        !rendered.contains("Throughput Stats:   not measured"),
        "{rendered}"
    );
}
