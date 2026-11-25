// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/tests/dashboard_rows_and_tier_labels.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;

#[test]
// TestName: an_unreadable_counter_file_publishes_absence_not_zero
fn an_unreadable_counter_file_publishes_absence_not_zero() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-monitor-counter-file-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();

    assert_eq!(read_optional_u64_file(&root.join("missing")), None);

    let empty = root.join("empty");
    fs::write(&empty, "").unwrap();
    assert_eq!(read_optional_u64_file(&empty), None);

    let unparsable = root.join("unparsable");
    fs::write(&unparsable, "not-a-count\n").unwrap();
    assert_eq!(read_optional_u64_file(&unparsable), None);

    let zero = root.join("zero");
    fs::write(&zero, "0\n").unwrap();
    assert_eq!(read_optional_u64_file(&zero), Some(0));

    let counted = root.join("counted");
    fs::write(&counted, "  4096\n").unwrap();
    assert_eq!(read_optional_u64_file(&counted), Some(4096));

    fs::remove_dir_all(root).unwrap();
}

#[test]
// TestName: an_unreadable_uptime_publishes_absence_not_zero
fn an_unreadable_uptime_publishes_absence_not_zero() {
    // The reader already returned Option. What was wrong was the
    // collection site folding None into 0 before it ever reached the wire,
    // so a machine that just booted and a host whose `/proc/uptime` could
    // not be read published the same number.
    assert_eq!(parse_uptime_seconds("1540.25 3080.50"), Some(1540));
    assert_eq!(parse_uptime_seconds(""), None);
    assert_eq!(parse_uptime_seconds("invalid"), None);

    let measured = ControlPlaneObservation {
        uptime_seconds: parse_uptime_seconds("42.9 85.8"),
        ..ControlPlaneObservation::default()
    };
    assert_eq!(
        serde_json::to_value(&measured).expect("serializable control plane")["uptime_seconds"],
        serde_json::json!(42)
    );

    let unreadable = ControlPlaneObservation {
        uptime_seconds: parse_uptime_seconds(""),
        ..ControlPlaneObservation::default()
    };
    assert_eq!(
        serde_json::to_value(&unreadable).expect("serializable control plane")["uptime_seconds"],
        serde_json::Value::Null
    );
}

#[test]
// TestName: the_banner_distinguishes_an_unreadable_uptime_from_a_real_zero
fn the_banner_distinguishes_an_unreadable_uptime_from_a_real_zero() {
    // Asserted on the text between the separators, never on the ⏱️ glyph:
    // emoji in a ratatui buffer is a multi-cell symbol and a literal match
    // against it is a false negative waiting to happen.
    let cases = [
        (Some(90u64), "up 1m 30s"),
        (Some(0), "up"),
        (None, "uptime not measured"),
    ];
    for (uptime, expected) in cases {
        let mut sample = observation(true, false);
        sample.control_plane.uptime_seconds = uptime;
        let metadata = DashboardMetadata {
            version_line: "RamShared CLI v0.15.0 · 01234567 (clean)".to_string(),
            running_line: "Running: v0.15.0 · 01234567 (clean) · matches the install".to_string(),
            install_lines: vec!["Installed: identity unknown".to_string()],
            restart_executable: None,
            runtime_notice: None,
            update_notice: None,
        };
        let backend = TestBackend::new(180, 40);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                draw_dashboard_with_metadata(frame, &sample, &DashHistory::default(), &metadata)
            })
            .expect("render banner");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            rendered.contains(expected),
            "uptime {uptime:?} must render {expected:?}; got {rendered:?}"
        );
    }
}

#[test]
fn dashboard_labels_wsl2_guest_memory_and_formats_kib_as_mib() {
    let osrelease = "6.18.40.1-microsoft-standard-WSL2+";
    let mut sample = observation(false, false);
    sample.memory_scope = detect_memory_scope(osrelease, false);
    sample.mem.total_kib = Some(16_378_880);
    sample.mem.available_kib = Some(1_182_720);

    let metadata = DashboardMetadata {
        version_line: "RamShared CLI v0.15.0 · 01234567 (clean)".to_string(),
        running_line: "Running: v0.15.0 · 01234567 (clean) · matches the install".to_string(),
        install_lines: vec![
            "Installed: identity unknown · 2026-09-27 18:04:05 UTC".to_string(),
            "Missing: active /opt/ramshared/current".to_string(),
        ],
        restart_executable: None,
        runtime_notice: None,
        update_notice: None,
    };
    let backend = TestBackend::new(180, 40);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            draw_dashboard_with_metadata(frame, &sample, &DashHistory::default(), &metadata)
        })
        .expect("render WSL2 dashboard");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert_eq!(sample.memory_scope, MemoryScope::Wsl2);
    assert!(rendered.contains("WSL2 Guest RAM & Swap"));
    assert!(rendered.contains("WSL2 Guest RAM:"));
    assert!(rendered.contains("14,840 / 15,995 MiB"));
    assert!(rendered.contains("Swap: OFF"));
    assert!(!rendered.contains("Host RAM"));
}

#[test]
fn parses_unit_startup_ms_and_uptime() {
    let show_out =
        "InactiveExitTimestampMonotonic=157379553\nActiveEnterTimestampMonotonic=160268125\n";
    let ms = parse_unit_startup_ms(show_out);
    assert_eq!(ms, Some(2888));

    let uptime_out = "1234.56 7890.12\n";
    let uptime = parse_uptime_seconds(uptime_out);
    assert_eq!(uptime, Some(1234));
}

#[test]
fn parses_vmstat_swap_and_page_faults() {
    let vmstat = "pswpin 100\npswpout 200\npgfault 5000\npgmajfault 42\n";
    let (in_p, out_p, faults, maj_faults) = parse_vmstat(vmstat);
    assert_eq!(in_p, Some(100));
    assert_eq!(out_p, Some(200));
    assert_eq!(faults, Some(5000));
    assert_eq!(maj_faults, Some(42));
}

/// A counter `/proc/vmstat` did not report is absent, not zero.
///
/// This is the face the old `unwrap_or(0)` could not express: the body
/// below is exactly what an unreadable file looks like after the caller's
/// `unwrap_or_default()`, plus a partial body that drops only the fault
/// pair. Absence is per counter — one missing line does not make the
/// other three zero.
#[test]
fn parse_vmstat_publishes_an_unreported_counter_as_absent_not_zero() {
    assert_eq!(
        parse_vmstat(""),
        (None, None, None, None),
        "an empty body is four absences, never four measured zeros"
    );
    assert_eq!(
        parse_vmstat("pswpin 7\npswpout 9\n"),
        (Some(7), Some(9), None, None),
        "a body without the fault pair leaves that pair absent and the swap pair measured"
    );
    assert_eq!(
        parse_vmstat("pswpin 0\npswpout 0\npgfault 0\npgmajfault 0\n"),
        (Some(0), Some(0), Some(0), Some(0)),
        "a present zero is a measured zero and must stay Some(0)"
    );
    assert_eq!(
        parse_vmstat("pgfault not-a-number\n"),
        (None, None, None, None),
        "an unparseable value is absence, not a fabricated zero"
    );
}

#[test]
fn cumulative_page_io_renders_the_vmstat_page_counts_in_bytes() {
    // `pswpin` / `pswpout` are base-page counts. The byte figure on the
    // row is that count times the unit named beside `parse_vmstat`, not an
    // independent byte reading and not a literal at the display site.
    let mut sample = observation(true, true);
    sample.control_plane.swap_in_pages = Some(3);
    sample.control_plane.swap_out_pages = Some(1);

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

    // 3 × 4096 = 12 288 bytes prints as 12 KB; 1 × 4096 prints as 4 KB.
    assert!(rendered.contains("In: 3 pgs (   12 KB)"), "{rendered}");
    assert!(rendered.contains("Out: 1 pgs (    4 KB)"), "{rendered}");
    // A nonzero page count is never published as zero bytes.
    assert!(!rendered.contains("In: 3 pgs (    0 B)"), "{rendered}");
}

/// One side's absence is not the other's, and neither is a measured zero.
///
/// `pswpin` and `pswpout` are independent counters, so the row publishes
/// each side's own face. The measured zero keeps its real figure — that is
/// a reading — while an unreported counter says so instead of printing
/// `0 pgs (    0 B)`, which would claim the kernel counted nothing.
#[test]
fn page_io_side_separates_an_unreported_counter_from_a_measured_zero() {
    assert_eq!(page_io_side(Some(0)), "0 pgs (    0 B)");
    assert_eq!(page_io_side(Some(3)), "3 pgs (   12 KB)");
    assert_eq!(page_io_side(None), "not measured");
    assert_ne!(
        page_io_side(None),
        page_io_side(Some(0)),
        "an unreported counter must not wear the measured-zero face"
    );
}

/// The wire form keeps absence and a measured zero apart after the
/// widening from `u64`.
///
/// This is the recorded contract change: older envelopes carry a bare
/// number and still read back as `Some(n)`, while a counter this build did
/// not measure publishes `null`. `0` on the wire therefore means the
/// kernel reported zero, and nothing else.
#[test]
fn vmstat_totals_publish_absence_as_null_and_a_measured_zero_as_zero() {
    let mut sample = observation(true, true);
    sample.control_plane.swap_in_pages = None;
    sample.control_plane.pgfault_total = Some(0);
    let wire: Value = serde_json::to_value(&sample).expect("serializable observation");
    let plane = wire
        .get("control_plane")
        .and_then(Value::as_object)
        .expect("control_plane is on the wire");
    assert_eq!(plane["swap_in_pages"], Value::Null);
    assert_eq!(plane["pgfault_total"], Value::from(0_u64));
}

#[test]
fn rejects_unbounded_or_mutating_monitor_options() {
    assert!(MonitorOptions::parse(&["--interval-ms".into(), "10".into()]).is_err());
    assert!(MonitorOptions::parse(&["--history-seconds".into(), "99999".into()]).is_err());
    assert!(MonitorOptions::parse(&["--activate".into()]).is_err());
    assert!(MonitorOptions::parse(&["--output".into(), "/tmp/x".into()]).is_err());
    assert!(MonitorOptions::parse(&["--interval-ms".into()]).is_err());
    assert!(MonitorOptions::parse(&["--history-seconds".into(), "0".into()]).is_err());
    assert!(MonitorOptions::parse(&["--heartbeat".into(), "".into()]).is_err());
    assert!(MonitorOptions::parse(&["--unknown".into()]).is_err());
    assert!(MonitorOptions::parse(&["--compact".into()]).is_ok());

    let parsed = MonitorOptions::parse(&[
        "--jsonl".into(),
        "--interval-ms".into(),
        "250".into(),
        "--history-seconds".into(),
        "3600".into(),
        "--output".into(),
        "/tmp/out".into(),
        "--heartbeat".into(),
        "/tmp/heartbeat".into(),
        "--once".into(),
    ])
    .expect("bounded stream options");
    assert_eq!(parsed.interval_ms, 250);
    assert_eq!(parsed.history_seconds, 3_600);
}

#[test]
fn rotating_log_and_heartbeat_are_bounded_and_valid() {
    let root = std::env::temp_dir().join(format!("ramshared-monitor-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let log = root.join("health.jsonl");
    let heartbeat = root.join("heartbeat.json");
    append_rotating(&log, "{\"sample\":1}", 1).expect("first append");
    append_rotating(&log, "{\"sample\":2}", 1).expect("rotating append");
    assert_eq!(fs::read_to_string(&log).unwrap(), "{\"sample\":2}\n");
    assert_eq!(
        fs::read_to_string(root.join("health.jsonl.1")).unwrap(),
        "{\"sample\":1}\n"
    );
    write_atomic(&heartbeat, "{\"ok\":true}").expect("atomic heartbeat");
    let parsed: Value = serde_json::from_str(&fs::read_to_string(&heartbeat).unwrap()).unwrap();
    assert_eq!(parsed["ok"], true);
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn dashboard_renders_active_and_unavailable_gpu_planes() {
    for (mut sample, expected_memory_label) in [
        (observation(true, true), "WSL2 Guest RAM"),
        (observation(false, false), "Host RAM"),
    ] {
        sample.memory_scope = if expected_memory_label == "WSL2 Guest RAM" {
            MemoryScope::Wsl2
        } else {
            MemoryScope::LinuxHost
        };
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
        assert!(rendered.contains(expected_memory_label));
        assert!(rendered.contains("Memory Tiers") || rendered.contains("Swap Priority"));
        assert!(rendered.contains("Diagnostics") || rendered.contains("Info"));
        assert!(rendered.contains("Spill order") || rendered.contains("exit"));
        assert!(!rendered.contains("PCIe Hardware"));
        if sample.bool_value("ok") == Some(true) {
            assert!(rendered.contains("Overall: HEALTHY"));
            assert!(rendered.contains("Swap: ACTIVE"));
        } else {
            assert!(rendered.contains("Overall: OFF"));
            assert!(rendered.contains("Swap: OFF"));
        }
        if sample.gpu.is_some() {
            assert!(rendered.contains("fixture-uuid"));
            assert!(rendered.contains("6144 MiB"));
        } else {
            assert!(rendered.contains("Active worker GPU budget unavailable"));
        }
    }
}

#[test]
fn the_tier_row_names_the_writeback_topology_and_the_cache_cause() {
    // SPEC DT-1/DT-3: the face says what is true about the topology and
    // why the GPU cache is not serving. A writeback target is not an
    // unmeasured tier, and a cache with a published cause is not an
    // unexplained absence.
    let mut sample = observation(true, false);
    sample.status.insert(
        "tier_roles".to_string(),
        serde_json::json!({
            "zram": "swap_device",
            "tier": "writeback_target",
            "writeback_target_is_tier": true,
            "origin_cache_mode": true
        }),
    );
    sample.status.insert(
        "cache_unavailable_reason".to_string(),
        Value::String("the isolated GPU cache worker exited".to_string()),
    );
    let backend = TestBackend::new(160, 44);
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
        rendered.contains("writeback target"),
        "the tier row must name the role: {rendered}"
    );
    assert!(rendered.contains("ORIGIN BACKING"), "{rendered}");
    assert!(
        rendered.contains("GPU cache: OFF"),
        "the GPU panel must publish the cause: {rendered}"
    );
    assert!(
        rendered.contains("BOUND [Writeback]"),
        "writeback target must not render as OFF: {rendered}"
    );
    assert!(
        rendered.contains("writeback backing"),
        "writeback target header must explain non-swap priority: {rendered}"
    );
}

#[test]
fn writeback_tier_status_reflects_pump_health_and_backing_occupancy() {
    assert_eq!(
        writeback_tier_status(Some("bound"), Some(65_536)),
        "🟢 ACTIVE [Writeback]"
    );
    assert_eq!(
        writeback_tier_status(Some("bound"), Some(0)),
        "🟢 ARMED [Writeback]"
    );
    assert_eq!(
        writeback_tier_status(Some("degraded"), Some(65_536)),
        "🟠 DEGRADED [Writeback]"
    );
    assert_eq!(
        writeback_tier_status(Some("tier_lost"), None),
        "🔴 TIER LOST [Writeback]"
    );
    assert_eq!(
        writeback_tier_status(Some("pump_stopped"), None),
        "🟡 PUMP STOPPED [Writeback]"
    );
    assert_eq!(writeback_tier_status(None, None), "🟢 BOUND [Writeback]");

    let mut sample = observation(true, false);
    sample.status.insert(
        "tiers".to_string(),
        serde_json::json!({
            "zram": { "present": true, "prio": 200, "size_kib": 4_194_300, "used_kib": 1_048_576 },
            "vram": { "present": false, "prio": null, "size_kib": 0, "used_kib": 0 },
            "disk": { "present": true, "prio": -2, "size_kib": 8_388_608, "used_kib": 0 }
        }),
    );
    sample.status.insert(
        "tier_roles".to_string(),
        serde_json::json!({
            "zram": "swap_device",
            "tier": "writeback_target",
            "writeback_target_is_tier": true,
            "origin_cache_mode": true
        }),
    );
    sample
        .status
        .insert("tier_state".to_string(), Value::String("bound".to_string()));
    sample.status.insert(
        "capacity".to_string(),
        serde_json::json!({
            "logical_capacity_kib": 4_194_304,
            "ssd_origin_written_kib": 65_536
        }),
    );
    let backend = TestBackend::new(180, 44);
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
    assert!(rendered.contains("ACTIVE [Writeback]"), "{rendered}");
    assert!(rendered.contains("64 MiB / 4096 MiB"), "{rendered}");
}

#[test]
fn origin_backed_tier_is_not_labeled_as_gpu_memory() {
    let mut sample = observation(true, false);
    sample.status.insert(
        "tier_roles".to_string(),
        serde_json::json!({"origin_cache_mode": true, "writeback_target_is_tier": false}),
    );
    let backend = TestBackend::new(160, 44);
    let mut terminal = Terminal::new(backend).expect("test terminal");
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
    assert!(rendered.contains("TIER 2: ORIGIN SWAP"), "{rendered}");
    assert!(!rendered.contains("TIER 2: GPU VRAM"), "{rendered}");
}

#[test]
fn dashboard_reports_protection_off_when_cascade_is_disabled() {
    let sample = observation(false, false);
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

    assert!(rendered.contains("Overall: OFF"));
    assert!(rendered.contains("Swap: OFF"));
    assert!(rendered.contains("STOPPED"));
    assert!(!rendered.contains("ARMED & READY"));
    assert!(!rendered.contains("Swap: ACTIVE"));
}

#[test]
fn dashboard_blocks_stale_active_state_without_live_daemon() {
    let mut sample = observation(false, false);
    sample.status.insert(
        "protection_state".to_string(),
        Value::String("ACTIVE".to_string()),
    );
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

    assert!(rendered.contains("Overall: BLOCKED"));
    assert!(rendered.contains("Swap: BLOCKED"));
    assert!(!rendered.contains("Swap: ACTIVE"));
}

/// The live shape on a host whose cache was released while the cascade
/// stayed armed: the product is blocked and the protection plane is
/// ready. Both are true at once, and the header has to say both.
#[test]
fn dashboard_reports_the_protection_plane_own_state_when_the_product_is_blocked() {
    let mut sample = observation(true, true);
    sample
        .status
        .insert("protection_state".to_string(), Value::from("READY"));
    sample.status.insert("ok".to_string(), Value::from(false));
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

    assert!(rendered.contains("Overall: BLOCKED"), "{rendered}");
    assert!(rendered.contains("Swap: READY"), "{rendered}");
    assert!(!rendered.contains("Swap: BLOCKED"), "{rendered}");
}

/// The product verdict and the protection claim are separate axes. A
/// positive claim is printed only while a live daemon can back it, and
/// an unmeasured daemon is not a refutation of one.
#[test]
fn dashboard_header_verdict_separates_the_product_verdict_from_the_protection_claim() {
    for (protection_state, daemon_alive, status_ok, expected_status, expected_protection) in [
        (
            "ACTIVE",
            Some(true),
            true,
            "OPERATIONAL & PROTECTED",
            "ACTIVE",
        ),
        ("READY", Some(true), true, "ARMED & READY", "READY"),
        // The reproduced live shape: released cache beside a ready
        // protection plane.
        ("READY", Some(true), false, "STATUS: BLOCKED", "READY"),
        ("ACTIVE", Some(true), false, "STATUS: BLOCKED", "ACTIVE"),
        ("AT_RISK", Some(true), true, "AT RISK", "AT RISK"),
        ("OFF", Some(true), false, "STATUS: OFF", "OFF"),
        ("OFF", Some(false), false, "STATUS: OFF", "OFF"),
        ("BLOCKED", Some(true), false, "STATUS: BLOCKED", "BLOCKED"),
        // Stale green: a positive claim with no daemon to back it is
        // not protection. This is the measured-down refutation.
        ("ACTIVE", Some(false), true, "STATUS: BLOCKED", "BLOCKED"),
        ("READY", Some(false), true, "STATUS: BLOCKED", "BLOCKED"),
        ("AT_RISK", Some(false), true, "STATUS: BLOCKED", "BLOCKED"),
        (
            "SOMETHING_ELSE",
            Some(true),
            true,
            "STATUS: UNKNOWN",
            "UNKNOWN",
        ),
        ("", Some(true), true, "STATUS: UNKNOWN", "UNKNOWN"),
        // An unmeasured daemon backs nothing and refutes nothing. The
        // positive word is still refused and `BLOCKED` is never borrowed
        // for a liveness that was never taken. Where the product itself
        // reports not-ok the product axis alone is a measured refutation
        // and the status word goes `BLOCKED` while the protection claim
        // stays `UNKNOWN`.
        ("ACTIVE", None, true, "STATUS: UNKNOWN", "UNKNOWN"),
        ("READY", None, true, "STATUS: UNKNOWN", "UNKNOWN"),
        ("AT_RISK", None, true, "STATUS: UNKNOWN", "UNKNOWN"),
        ("ACTIVE", None, false, "STATUS: BLOCKED", "UNKNOWN"),
        ("READY", None, false, "STATUS: BLOCKED", "UNKNOWN"),
        // A state that needs no daemon is not rewritten by its absence.
        ("OFF", None, false, "STATUS: OFF", "OFF"),
        ("BLOCKED", None, true, "STATUS: BLOCKED", "BLOCKED"),
        ("SOMETHING_ELSE", None, true, "STATUS: UNKNOWN", "UNKNOWN"),
    ] {
        let (status_text, _, protection_text) =
            dashboard_header_verdict(protection_state, daemon_alive, status_ok);
        assert!(
            status_text.contains(expected_status),
            "{protection_state}/{daemon_alive:?}/{status_ok}: {status_text}"
        );
        assert_eq!(
            protection_text, expected_protection,
            "{protection_state}/{daemon_alive:?}/{status_ok}"
        );
    }
}

/// The daemon liveness row owns three states, and an absent witness is
/// never a daemon that was found stopped.
#[test]
fn daemon_status_row_distinguishes_absence_from_a_measured_stopped() {
    assert_eq!(daemon_status_row(Some(true)), "🟢 RUNNING");
    assert_eq!(daemon_status_row(Some(false)), "🔴 STOPPED");
    assert_eq!(daemon_status_row(None), "⚪ not measured");
}

/// A status document that published a daemon but never published its
/// liveness did not observe a stopped daemon. The Diagnostics row must
/// say so rather than borrow the measured-down face.
#[test]
fn daemon_row_never_publishes_a_stopped_daemon_from_an_absent_liveness() {
    let mut sample = observation(true, true);
    sample
        .status
        .insert("daemon".to_string(), serde_json::json!({ "pid": 7 }));
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
    assert!(!rendered.contains("STOPPED"), "{rendered}");
    assert!(!rendered.contains("RUNNING"), "{rendered}");
}

/// The safety-guard row is witness-backed, never the unconditional
/// product claim the old row printed beside a counter that can refute it.
#[test]
fn safety_guard_row_is_witness_backed_and_never_an_unconditional_zero_panic_claim() {
    assert_eq!(safety_guard_row(Some("HEALTHY")), "holding");
    assert_eq!(safety_guard_row(Some("SAFE_MODE")), "engaged");
    assert_eq!(safety_guard_row(Some("BLOCKED")), "refuted");
    assert_eq!(safety_guard_row(Some("WHAT_IS_THIS")), "unrecognized");
    assert_eq!(safety_guard_row(None), "not measured");
}

/// A frame whose document never spoke about the guard did not observe it
/// holding. The row must say so and must not print the product's lint
/// policy as if this screen had measured it.
#[test]
fn safety_row_never_publishes_the_lint_policy_as_a_measurement() {
    let mut sample = observation(true, true);
    sample.errors.push("sample_refresh_failed".to_string());
    sample.status.remove("guardian_state");
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

    assert!(rendered.contains("Safety Guard"), "{rendered}");
    // The row's own arm, not some other panel's absence face.
    assert!(rendered.contains("not measured"), "{rendered}");
    assert!(!rendered.contains("Zero Panic"), "{rendered}");
    assert!(!rendered.contains("Fail-Closed"), "{rendered}");
}

/// The witness the document did publish is what the row reports, and a
/// measured state never falls through to the absence arm.
#[test]
fn safety_row_publishes_the_guard_state_the_document_carried() {
    let mut sample = observation(true, true);
    sample
        .status
        .insert("guardian_state".to_string(), serde_json::json!("SAFE_MODE"));
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

    assert!(rendered.contains("engaged"), "{rendered}");
    assert!(rendered.contains("Safety Guard"), "{rendered}");
    assert!(!rendered.contains("Zero Panic"), "{rendered}");
    assert!(!rendered.contains("Fail-Closed"), "{rendered}");
}
