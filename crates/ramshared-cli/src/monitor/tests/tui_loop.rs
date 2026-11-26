// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/tests/tui_loop.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;

// ---- tui_loop contract tests ------------------------------------
//
// The loop is driven through `Terminal<TestBackend>` and a scripted
// `TuiRuntime`. Each test is named after the display-honesty rule it
// holds, with a positive and a negative half.

fn empty_dashboard_metadata() -> DashboardMetadata {
    DashboardMetadata {
        version_line: "test-version".to_string(),
        running_line: "test-running".to_string(),
        install_lines: Vec::new(),
        restart_executable: None,
        runtime_notice: None,
        update_notice: None,
    }
}

fn quit_key() -> Event {
    press_key(KeyCode::Char('q'), KeyModifiers::NONE)
}

fn press_key(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(ratatui::crossterm::event::KeyEvent::new(code, modifiers))
}

fn tui_options() -> MonitorOptions {
    MonitorOptions {
        interval_ms: 100,
        history_seconds: 1,
        ..MonitorOptions::default()
    }
}

fn observation_with_counters(
    swap_read: u64,
    swap_write: u64,
    pgfault: Option<u64>,
    pgmajfault: Option<u64>,
) -> Observation {
    let mut sample = observation(true, false);
    sample.control_plane.swap_read_bytes = swap_read;
    sample.control_plane.swap_write_bytes = swap_write;
    sample.control_plane.pgfault_total = pgfault;
    sample.control_plane.pgmajfault_total = pgmajfault;
    sample
}

/// Scripted seams for [`tui_loop`]. Events are delivered in order; once
/// they run out, `next_event` sleeps the requested wait (so sample
/// intervals elapse the way production `event::poll` does) and synthesizes
/// a quit only when the observation script is also exhausted — a harness
/// fail-safe, never a product behaviour.
struct ScriptedRuntime {
    events: VecDeque<Event>,
    observations: VecDeque<Result<Observation, MonitorError>>,
    metadata: VecDeque<DashboardMetadata>,
    observe_calls: usize,
}

impl ScriptedRuntime {
    fn new() -> Self {
        Self {
            events: VecDeque::new(),
            observations: VecDeque::new(),
            metadata: VecDeque::new(),
            observe_calls: 0,
        }
    }

    fn with_events(mut self, events: Vec<Event>) -> Self {
        self.events = events.into();
        self
    }

    fn with_observations(mut self, observations: Vec<Observation>) -> Self {
        self.observations = observations.into_iter().map(Ok).collect();
        self
    }

    fn with_failed_observations(mut self, count: usize) -> Self {
        for _ in 0..count {
            self.observations
                .push_back(Err(MonitorError::Io("scripted refresh failure".into())));
        }
        self
    }

    fn with_metadata(mut self, frames: Vec<DashboardMetadata>) -> Self {
        self.metadata = frames.into();
        self
    }
}

impl TuiRuntime for ScriptedRuntime {
    fn next_event(&mut self, wait: Duration) -> Option<Event> {
        if let Some(event) = self.events.pop_front() {
            return Some(event);
        }
        if self.observations.is_empty() {
            return Some(quit_key());
        }
        std::thread::sleep(wait.min(Duration::from_millis(20)));
        None
    }

    fn observe(&mut self, _benchmark_evidence: &Path) -> Result<Observation, MonitorError> {
        self.observe_calls += 1;
        self.observations
            .pop_front()
            .unwrap_or_else(|| Err(MonitorError::Io("observation script exhausted".into())))
    }

    fn metadata(
        &mut self,
        _identity_cache: &mut InstallationIdentityCache,
        _process_origin: ProcessInstallOrigin,
        _allow_reexec: bool,
        _runtime_notice: Option<&str>,
    ) -> DashboardMetadata {
        self.metadata
            .pop_front()
            .unwrap_or_else(empty_dashboard_metadata)
    }
}

fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
    let buffer = terminal.backend().buffer();
    let width = buffer.area().width as usize;
    buffer
        .content()
        .chunks(width.max(1))
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn fault_rates_row(rendered: &str) -> String {
    let marker = "Page Faults Rate:";
    let start = rendered
        .find(marker)
        .unwrap_or_else(|| panic!("fault rates row missing from {rendered}"));
    let rest = &rendered[start + marker.len()..];
    let end = rest.find('\n').unwrap_or(rest.len());
    rest[..end].to_string()
}

fn sample_age_ms_row(rendered: &str) -> u64 {
    let marker = "Telemetry Sample Age:";
    let start = rendered
        .find(marker)
        .unwrap_or_else(|| panic!("sample age row missing from {rendered}"));
    let rest = &rendered[start + marker.len()..];
    let end = rest.find(" ms").unwrap_or(rest.len());
    rest[..end]
        .trim()
        .rsplit(' ')
        .next()
        .expect("age number")
        .parse()
        .expect("numeric sample age")
}

fn run_tui_loop(
    runtime: &mut ScriptedRuntime,
    identity_refresh_interval: Duration,
) -> (Result<TuiExit, MonitorError>, Terminal<TestBackend>, usize) {
    let backend = TestBackend::new(220, 50);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut identity_cache = InstallationIdentityCache::default();
    let result = tui_loop(
        &mut terminal,
        &tui_options(),
        ProcessInstallOrigin::Unknown,
        false,
        None,
        &mut identity_cache,
        identity_refresh_interval,
        runtime,
    );
    let observe_calls = runtime.observe_calls;
    (result, terminal, observe_calls)
}

#[test]
fn language_toggle_switches_the_main_and_pressure_views_to_pt_br() {
    let mut runtime = ScriptedRuntime::new()
        .with_observations(vec![observation(true, false), observation(true, false)])
        .with_events(vec![
            press_key(KeyCode::Char('l'), KeyModifiers::NONE),
            press_key(KeyCode::Char('p'), KeyModifiers::NONE),
        ]);
    let (result, terminal, _) = run_tui_loop(&mut runtime, IDENTITY_REFRESH_INTERVAL);
    assert!(matches!(result, Ok(TuiExit::UserRequested)));
    let rendered = buffer_text(&terminal);
    assert!(rendered.contains("PRESSÃO"), "{rendered}");
    assert!(rendered.contains("GUEST WSL2"), "{rendered}");
    assert!(rendered.contains("[l] English"), "{rendered}");
}

/// An absent fault pair is never a measured zero. The pair is one sample:
/// `last_faults_sample` starts as `None` when either counter is absent, so
/// the first complete pair is a baseline and not a rate.
#[test]
fn an_absent_fault_pair_publishes_as_absent_never_as_a_measured_zero() {
    // Positive: the pair is incomplete on every sample, so the face stays
    // "not measured" — never `0/s`.
    let mut runtime = ScriptedRuntime::new().with_observations(vec![
        observation_with_counters(0, 0, Some(100), None),
        observation_with_counters(0, 0, Some(110), None),
        observation_with_counters(0, 0, Some(130), None),
    ]);
    runtime = runtime.with_events(vec![press_key(KeyCode::Char('d'), KeyModifiers::NONE)]);
    let (result, terminal, _) = run_tui_loop(&mut runtime, IDENTITY_REFRESH_INTERVAL);
    assert!(matches!(result, Ok(TuiExit::UserRequested)));
    let rendered = buffer_text(&terminal);
    let row = fault_rates_row(&rendered);
    assert!(
        row.contains("not measured"),
        "an incomplete pair must publish absent, got: {row}"
    );
    assert!(
        !row.contains("/s"),
        "an absent pair must not publish a rate: {row}"
    );

    // Negative: both counters present across a co-sampled pair publish a
    // real rate, and that rate is not the absence face.
    let mut runtime = ScriptedRuntime::new().with_observations(vec![
        observation_with_counters(0, 0, Some(100), Some(10)),
        observation_with_counters(0, 0, Some(100), Some(10)),
        observation_with_counters(0, 0, Some(160), Some(22)),
    ]);
    runtime = runtime.with_events(vec![press_key(KeyCode::Char('d'), KeyModifiers::NONE)]);
    let (result, terminal, _) = run_tui_loop(&mut runtime, IDENTITY_REFRESH_INTERVAL);
    assert!(matches!(result, Ok(TuiExit::UserRequested)));
    let row = fault_rates_row(&buffer_text(&terminal));
    assert!(
        row.contains("/s (Minor:"),
        "a co-sampled pair must publish rates, got: {row}"
    );
    assert!(
        !row.contains("not measured"),
        "a present pair must not publish absent: {row}"
    );
}

/// The first complete pair is the baseline. Recording the incomplete
/// options themselves would fabricate a denominator and publish a rate
/// from a mix of this frame's peak and a pair the kernel never sent.
#[test]
fn the_first_complete_fault_pair_is_a_baseline_and_not_a_published_rate() {
    let mut runtime = ScriptedRuntime::new().with_observations(vec![
        observation_with_counters(0, 0, Some(100), None),
        observation_with_counters(0, 0, Some(100), None),
        // First complete pair: becomes the baseline only.
        observation_with_counters(0, 0, Some(200), Some(5)),
    ]);
    runtime = runtime.with_events(vec![press_key(KeyCode::Char('d'), KeyModifiers::NONE)]);
    let (result, terminal, _) = run_tui_loop(&mut runtime, IDENTITY_REFRESH_INTERVAL);
    assert!(matches!(result, Ok(TuiExit::UserRequested)));
    let row = fault_rates_row(&buffer_text(&terminal));
    assert!(
        row.contains("not measured"),
        "the first complete pair must not publish a rate, got: {row}"
    );
    assert!(
        !row.contains("/s"),
        "a baseline pair is not a measured rate: {row}"
    );
}

/// Rates are a delta over one accepted window. The pair of counters and
/// the timestamp come from the same frame — never a new peak differenceed
/// against an old denominator.
#[test]
fn rates_publish_only_from_one_co_sampled_pair_and_never_a_mixed_window() {
    // Window B→C is the only accepted one (A→B is the immediate resample
    // whose dt falls under the floor). The published swap rate must match
    // (C.bytes - B.bytes) / dt for the co-sampled pair.
    let mut runtime = ScriptedRuntime::new().with_observations(vec![
        observation_with_counters(0, 0, Some(1), Some(1)),
        observation_with_counters(0, 0, Some(2), Some(1)),
        // 2 MiB over the window on the swap plane.
        observation_with_counters(2 * 1_048_576, 0, Some(3), Some(1)),
    ]);
    runtime = runtime.with_events(vec![press_key(KeyCode::Char('d'), KeyModifiers::NONE)]);
    let (result, terminal, _) = run_tui_loop(&mut runtime, IDENTITY_REFRESH_INTERVAL);
    assert!(matches!(result, Ok(TuiExit::UserRequested)));
    let rendered = buffer_text(&terminal);
    let speed_line = rendered
        .lines()
        .find(|line| line.contains("Real-Time Speed:"))
        .expect("speed row");
    // An accepted window publishes a figure. `n/a` is the unmeasured arm
    // and `stale` is the aged-sample arm; neither is a co-sampled rate.
    assert!(
        !speed_line.contains("n/a"),
        "an accepted window must publish rates, got: {speed_line}"
    );
    assert!(
        !speed_line.contains("stale"),
        "a live window must not freeze as stale, got: {speed_line}"
    );
    assert!(
        speed_line.contains("MB/s"),
        "the speed row keeps its unit: {speed_line}"
    );
    // The fault pair publishes from the same window.
    let row = fault_rates_row(&rendered);
    assert!(
        row.contains("/s (Minor:"),
        "a co-sampled fault pair must publish rates, got: {row}"
    );
    // Absent pair on a later frame cannot reuse the earlier denominator.
    let mut runtime = ScriptedRuntime::new().with_observations(vec![
        observation_with_counters(0, 0, Some(1), Some(1)),
        observation_with_counters(0, 0, Some(1), Some(1)),
        observation_with_counters(2 * 1_048_576, 0, Some(99), None),
    ]);
    runtime = runtime.with_events(vec![press_key(KeyCode::Char('d'), KeyModifiers::NONE)]);
    let (result, terminal, _) = run_tui_loop(&mut runtime, IDENTITY_REFRESH_INTERVAL);
    assert!(matches!(result, Ok(TuiExit::UserRequested)));
    let row = fault_rates_row(&buffer_text(&terminal));
    assert!(
        row.contains("not measured"),
        "a dropped counter clears the pair instead of mixing windows: {row}"
    );
}

/// `sample_age_ms` is the age of `last_successful_sample`, not a second
/// clock read of the host and not the observation's wall-clock stamp.
#[test]
fn sample_age_ms_is_derived_from_the_last_successful_sample_not_the_host_clock() {
    let mut aged = observation_with_counters(0, 0, None, None);
    // A host wall-clock stamp that would make a fabricated age enormous.
    aged.epoch_ms = 1_700_000_000_000;

    let mut runtime = ScriptedRuntime::new()
        .with_observations(vec![aged])
        .with_failed_observations(2)
        .with_events(vec![press_key(KeyCode::Char('d'), KeyModifiers::NONE)]);
    let (result, terminal, _) = run_tui_loop(&mut runtime, IDENTITY_REFRESH_INTERVAL);
    assert!(matches!(result, Ok(TuiExit::UserRequested)));
    let rendered = buffer_text(&terminal);
    let age = sample_age_ms_row(&rendered);
    // After at least one full interval of failed refreshes the age is the
    // elapsed time since the successful sample — milliseconds of runtime,
    // not the 1.7e12 ms a wall-clock subtraction would invent.
    assert!(
        age >= 1,
        "a failed refresh must age the last success, got {age} ms"
    );
    assert!(
        age < 60_000,
        "sample age is not a host wall-clock figure, got {age} ms"
    );

    // Negative half: a successful refresh resets the age to ~0 even
    // though the observation carries the same huge epoch stamp.
    let mut stamped = observation_with_counters(0, 0, None, None);
    stamped.epoch_ms = 1_700_000_000_000;
    let mut runtime = ScriptedRuntime::new()
        .with_observations(vec![stamped.clone(), stamped])
        .with_events(vec![press_key(KeyCode::Char('d'), KeyModifiers::NONE)]);
    let (result, terminal, _) = run_tui_loop(&mut runtime, IDENTITY_REFRESH_INTERVAL);
    assert!(matches!(result, Ok(TuiExit::UserRequested)));
    let age = sample_age_ms_row(&buffer_text(&terminal));
    assert!(
        age < 100,
        "a fresh success must not keep a stale age, got {age} ms"
    );
}

/// The exit keys are exactly `q`, `Esc` and `Ctrl-C` **on press**.
/// Nothing else leaves the loop.
#[test]
fn the_exit_keys_are_exactly_q_esc_and_ctrl_c_on_press() {
    // Positive: each of the three keys exits on press.
    for event in [
        press_key(KeyCode::Char('q'), KeyModifiers::NONE),
        press_key(KeyCode::Esc, KeyModifiers::NONE),
        press_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
    ] {
        let mut runtime = ScriptedRuntime::new()
            .with_observations(vec![observation(true, false)])
            .with_events(vec![event]);
        let (result, _, _) = run_tui_loop(&mut runtime, IDENTITY_REFRESH_INTERVAL);
        assert!(
            matches!(result, Ok(TuiExit::UserRequested)),
            "the key must exit on press"
        );
    }

    // Negative: a non-exit key does not leave the loop. The run only
    // ends when the observation script is exhausted (harness quit), so
    // extra observations prove the loop kept going past `x`.
    let mut runtime = ScriptedRuntime::new()
        .with_observations(vec![
            observation(true, false),
            observation(true, false),
            observation(true, false),
        ])
        .with_events(vec![
            press_key(KeyCode::Char('x'), KeyModifiers::NONE),
            press_key(KeyCode::Char('Q'), KeyModifiers::NONE),
            press_key(KeyCode::Char('c'), KeyModifiers::NONE),
            Event::Key(ratatui::crossterm::event::KeyEvent::new_with_kind(
                KeyCode::Char('q'),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            )),
        ]);
    let (result, _, observe_calls) = run_tui_loop(&mut runtime, IDENTITY_REFRESH_INTERVAL);
    assert!(matches!(result, Ok(TuiExit::UserRequested)));
    assert!(
        observe_calls >= 3,
        "non-exit keys must not leave the loop; observed {observe_calls} samples"
    );
}

/// A pending restart is taken only after a frame is drawn (SPEC DT-4 /
/// NFR-5), so the operator sees which transition is happening before the
/// process is replaced.
#[test]
fn a_pending_restart_is_armed_only_after_a_frame_is_drawn() {
    let restart = PathBuf::from("/opt/ramshared/current/bin/ramshared");
    let mut metadata = empty_dashboard_metadata();
    metadata.version_line = "TRANSITION-FRAME-IDENTITY".to_string();
    metadata.restart_executable = Some(restart.clone());

    // Positive: the transition frame is on screen before the loop returns
    // `Restart`. The identity refresh is the production interval, so the
    // armed identity is still the one drawn.
    let mut runtime = ScriptedRuntime::new()
        .with_observations(vec![observation(true, false)])
        .with_metadata(vec![metadata]);
    let (result, terminal, _) = run_tui_loop(&mut runtime, IDENTITY_REFRESH_INTERVAL);
    let exit = result.expect("tui exit");
    assert_eq!(exit, TuiExit::Restart(restart.clone()));
    let rendered = buffer_text(&terminal);
    assert!(
        rendered.contains("TRANSITION-FRAME-IDENTITY"),
        "the operator must see the transition frame before restart: {rendered}"
    );

    // Negative: with no restart target the loop keeps running and the
    // process is not replaced — the operator stays on the dashboard.
    let mut runtime = ScriptedRuntime::new()
        .with_observations(vec![observation(true, false), observation(true, false)]);
    let (result, _, observe_calls) = run_tui_loop(&mut runtime, IDENTITY_REFRESH_INTERVAL);
    assert!(matches!(result, Ok(TuiExit::UserRequested)));
    assert!(
        observe_calls >= 2,
        "a run without a restart target must keep sampling, got {observe_calls}"
    );
}

/// Identity refresh re-arms `pending_restart` only when one is not
/// already pending: the operator-visible transition is the one they were
/// already shown, never a target that appeared later.
#[test]
fn identity_refresh_rearms_pending_restart_only_when_none_is_pending() {
    let first = PathBuf::from("/restart/first");
    let second = PathBuf::from("/restart/second");

    // Positive: nothing pending at start, the refresh arms the new target.
    let mut unarmed = empty_dashboard_metadata();
    unarmed.restart_executable = None;
    let mut armed_later = empty_dashboard_metadata();
    armed_later.restart_executable = Some(second.clone());
    let mut runtime = ScriptedRuntime::new()
        .with_observations(vec![observation(true, false)])
        .with_metadata(vec![unarmed, armed_later]);
    let (result, _, _) = run_tui_loop(&mut runtime, Duration::ZERO);
    assert_eq!(
        result.expect("tui exit"),
        TuiExit::Restart(second.clone()),
        "a refresh that arms a previously unarmed restart owns the transition"
    );

    // Negative: already pending, the refresh must not overwrite it.
    let mut pending_first = empty_dashboard_metadata();
    pending_first.restart_executable = Some(first.clone());
    let mut later_other = empty_dashboard_metadata();
    later_other.restart_executable = Some(second.clone());
    let mut runtime = ScriptedRuntime::new()
        .with_observations(vec![observation(true, false)])
        .with_metadata(vec![pending_first, later_other]);
    let (result, _, _) = run_tui_loop(&mut runtime, Duration::ZERO);
    assert_eq!(
        result.expect("tui exit"),
        TuiExit::Restart(first),
        "an already-pending restart is the transition the operator was shown"
    );
}

// ---- pre-960 residue -------------------------------------------

/// `Default` is the explicit untuned profile, not a zeroed struct.
#[test]
fn monitor_options_default_is_the_explicit_untuned_profile() {
    let options = MonitorOptions::default();
    assert!(!options.jsonl);
    assert!(!options.compact);
    assert!(!options.once);
    assert_eq!(options.interval_ms, DEFAULT_INTERVAL_MS);
    assert_eq!(options.history_seconds, DEFAULT_HISTORY_SECONDS);
    assert!(options.output.is_none());
    assert!(options.heartbeat.is_none());
    assert!(options.benchmark_evidence.is_none());
}

/// The WSL probe is the binfmt entry first and the environment only as a
/// fallback — and the fallback is evaluated when the path is absent.
#[test]
fn wsl_interop_falls_back_to_the_environment_only_when_binfmt_is_absent() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-monitor-wsl-interop-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let present = root.join("WSLInterop");
    fs::write(&present, "enabled\n").unwrap();
    let missing = root.join("absent");
    let env_marker = Some(OsStr::new("1"));

    // Positive: the binfmt entry is the strong signal, with or without
    // the environment fallback.
    assert!(wsl_interop_present(&present, env_marker));
    assert!(wsl_interop_present(&present, None));

    // Negative half of the strong signal, positive half of the fallback:
    // no binfmt entry, but the environment carries the marker.
    assert!(wsl_interop_present(&missing, env_marker));

    // Negative: neither signal.
    assert!(!wsl_interop_present(&missing, None));

    fs::remove_dir_all(&root).unwrap();
}

/// Each absent measurement is its own named gap. Collapsing them would
/// tell the operator the same sentence about two different facts.
///
/// A measured empty swap is not a gap: the pair reads `Some((0, 0))` and
/// names nothing.
#[test]
fn measurement_errors_name_each_absent_surface_separately() {
    let full = MemoryObservation {
        total_kib: Some(16_384),
        available_kib: Some(8_192),
        swap_total_kib: Some(4_096),
        swap_free_kib: Some(2_048),
        ..MemoryObservation::default()
    };
    assert!(measurement_errors(true, &full).is_empty());

    let no_psi = measurement_errors(false, &full);
    assert_eq!(no_psi, vec!["memory_psi_unavailable".to_string()]);

    let no_ram = MemoryObservation {
        swap_total_kib: Some(4_096),
        swap_free_kib: Some(2_048),
        ..MemoryObservation::default()
    };
    assert_eq!(
        measurement_errors(true, &no_ram),
        vec!["memory_telemetry_unavailable".to_string()]
    );

    let no_swap = MemoryObservation {
        total_kib: Some(16_384),
        available_kib: Some(8_192),
        ..MemoryObservation::default()
    };
    assert_eq!(
        measurement_errors(true, &no_swap),
        vec!["swap_telemetry_unavailable".to_string()]
    );

    let none = measurement_errors(false, &MemoryObservation::default());
    assert_eq!(
        none,
        vec![
            "memory_psi_unavailable".to_string(),
            "memory_telemetry_unavailable".to_string(),
            "swap_telemetry_unavailable".to_string(),
        ]
    );

    let empty_swap = MemoryObservation {
        total_kib: Some(16_384),
        available_kib: Some(8_192),
        swap_total_kib: Some(0),
        swap_free_kib: Some(0),
        ..MemoryObservation::default()
    };
    assert!(measurement_errors(true, &empty_swap).is_empty());
}

/// `run` dispatches the compact face when asked; the once flag stops the
/// loop after one printed report.
#[test]
fn run_dispatches_the_compact_face_once_without_entering_the_tui() {
    let options = MonitorOptions {
        compact: true,
        once: true,
        ..MonitorOptions::default()
    };
    run(&options).expect("one compact report");
}

/// Until a busy window exists the latency band is the idle profile, and a
/// measured latency outranks the projection entirely.
#[test]
fn tier_latency_projection_uses_the_idle_band_until_a_busy_window_exists() {
    let (min_us, avg_us, max_us) = project_tier_latency(&ZRAM_LATENCY_ESTIMATE, 0.0, 0.0, 0);
    assert_eq!(min_us, ZRAM_LATENCY_ESTIMATE.min_us);
    assert_eq!(avg_us, ZRAM_LATENCY_ESTIMATE.idle_avg_us);
    assert_eq!(max_us, ZRAM_LATENCY_ESTIMATE.max_floor_us);

    let (min_us, avg_us, max_us) = project_tier_latency(&DISK_LATENCY_ESTIMATE, 20.0, 40.0, 3);
    assert_eq!(min_us, DISK_LATENCY_ESTIMATE.min_us);
    assert!(avg_us > DISK_LATENCY_ESTIMATE.min_us);
    assert!(max_us >= DISK_LATENCY_ESTIMATE.max_floor_us);

    let mut plane = TierIoStats {
        latency_source: LatencySource::Measured,
        avg_lat_us: 42.0,
        ..TierIoStats::default()
    };
    project_onto_plane(&mut plane, &VRAM_LATENCY_ESTIMATE, 5);
    assert_eq!(plane.avg_lat_us, 42.0, "a measured latency is left alone");
    assert_eq!(plane.latency_source, LatencySource::Measured);

    let mut plane = TierIoStats::default();
    project_onto_plane(&mut plane, &VRAM_LATENCY_ESTIMATE, 0);
    assert_eq!(plane.latency_source, LatencySource::Estimated);
    assert_eq!(plane.min_lat_us, VRAM_LATENCY_ESTIMATE.min_us);

    let mut cp = ControlPlaneObservation::default();
    update_tier_latencies(&mut cp, 0, 0, 0);
    assert_eq!(
        cp.estimated_page_fault_lat_us, RESIDENT_PAGE_FAULT_LAT_US,
        "with no paging tier the estimate is the resident baseline"
    );
}

/// An accumulator with no windows publishes absence for both families,
/// not zeros borrowed from the other's empty set.
#[test]
fn tier_accumulator_publishes_absence_when_no_window_was_recorded() {
    let mut acc = TierAccumulator::default();
    let mut plane = TierIoStats::default();
    acc.apply_to_plane_io(&mut plane);
    assert_eq!(plane.min_mbs, None);
    assert_eq!(plane.avg_mbs, None);
    assert_eq!(plane.max_mbs, None);
    assert_eq!(plane.peak_mbs, None);
    assert_eq!(plane.read_peak_mbs, None);
    assert_eq!(plane.write_peak_mbs, None);

    // Windows below the busy floor still move the directional peaks and
    // still leave the busy trio empty.
    acc.record(1.0, 1.0);
    acc.apply_to_plane_io(&mut plane);
    assert_eq!(plane.read_peak_mbs, Some(1.0));
    assert_eq!(plane.write_peak_mbs, Some(1.0));
    assert_eq!(plane.min_mbs, None);
    assert_eq!(plane.avg_mbs, None);
    assert_eq!(plane.max_mbs, None);
    assert_eq!(acc.avg_mbs(), None);
}

/// The flight-sample directory refuses a base that cannot be created and a
/// directory someone else owns — `None` means no private directory, never
/// "the sample is empty".
#[test]
fn flight_sample_dir_refuses_foreign_owners_and_unusable_bases() {
    use std::os::unix::fs::MetadataExt;

    let root = std::env::temp_dir().join(format!(
        "ramshared-flight-dir-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let uid = fs::metadata("/proc/self").unwrap().uid();

    // Positive: a fresh base creates a private directory this user owns.
    let created = flight_sample_dir_in(&root, uid).expect("fresh directory");
    assert_eq!(fs::metadata(&created).unwrap().uid(), uid);
    assert!(created.ends_with(format!("ramshared-{uid}")));

    // Already-exists is accepted when the owner matches.
    assert_eq!(
        flight_sample_dir_in(&root, uid).expect("existing directory"),
        created
    );

    // Foreign owner: the leaf exists but is not ours.
    let foreign_base = root.join("foreign");
    fs::create_dir(&foreign_base).unwrap();
    fs::create_dir(foreign_base.join("ramshared-99999")).unwrap();
    assert_eq!(flight_sample_dir_in(&foreign_base, 99999), None);

    // Unusable base: nothing to create under.
    assert_eq!(flight_sample_dir_in(&root.join("missing-base"), uid), None);

    fs::remove_dir_all(&root).unwrap();
}

fn terminal_with_old_screen_cells() -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(110, 32)).expect("test terminal");
    let previous_cell = &ratatui::buffer::Cell::new("☂");
    ratatui::backend::Backend::draw(
        terminal.backend_mut(),
        (0..32).flat_map(|y| (0..110).map(move |x| (x, y, previous_cell))),
    )
    .expect("existing alternate-screen contents");
    terminal
}

#[test]
fn tui_resize_repaints_when_viewport_dimensions_are_already_current() {
    let mut terminal = terminal_with_old_screen_cells();
    let mut runtime = ScriptedRuntime::new()
        .with_observations(vec![observation(true, false), observation(true, false)])
        .with_events(vec![Event::Resize(110, 32), quit_key()]);
    let exit = tui_loop(
        &mut terminal,
        &tui_options(),
        ProcessInstallOrigin::Unknown,
        false,
        None,
        &mut InstallationIdentityCache::default(),
        IDENTITY_REFRESH_INTERVAL,
        &mut runtime,
    )
    .expect("tui exit");
    assert_eq!(exit, TuiExit::UserRequested);
    assert!(!buffer_text(&terminal).contains('☂'));
}

#[test]
fn tui_handoff_preserves_language_screen_and_history_across_restart() {
    let restart = PathBuf::from("/opt/ramshared/current/bin/ramshared");
    let unarmed = empty_dashboard_metadata();
    let mut armed = empty_dashboard_metadata();
    armed.version_line = "NEW-RELEASE-FRAME".to_string();
    armed.restart_executable = Some(restart.clone());

    let mut runtime = ScriptedRuntime::new()
        .with_observations(vec![observation(true, false), observation(true, false)])
        .with_events(vec![
            press_key(KeyCode::Char('l'), KeyModifiers::NONE),
            press_key(KeyCode::Char('p'), KeyModifiers::NONE),
        ])
        .with_metadata(vec![unarmed.clone(), unarmed.clone(), unarmed, armed]);
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(110, 32)).unwrap();
    let mut cache = InstallationIdentityCache::default();
    let mut handoff = None;

    let first_exit = tui_loop_with_handoff(
        &mut terminal,
        &tui_options(),
        ProcessInstallOrigin::ActiveRelease,
        true,
        None,
        &mut cache,
        Duration::ZERO,
        &mut runtime,
        &mut handoff,
    )
    .expect("first exit");
    assert_eq!(first_exit, TuiExit::Restart(restart));
    let carried = handoff.clone().expect("handoff captured on restart");
    assert_eq!(carried.language, Language::PtBr);
    assert_eq!(carried.screen, Screen::Pressure);
    assert!(
        !carried.history.ram.is_empty(),
        "handoff must carry recorded history across exec"
    );

    let encoded = carried.encode().expect("encode handoff");
    let decoded = TuiHandoffState::decode(&encoded).expect("decode handoff");
    let mut restored_handoff = Some(decoded);
    let mut second_runtime = ScriptedRuntime::new()
        .with_observations(vec![observation(true, false)])
        .with_events(vec![quit_key()]);
    let mut second_terminal = terminal_with_old_screen_cells();
    let second_exit = tui_loop_with_handoff(
        &mut second_terminal,
        &tui_options(),
        ProcessInstallOrigin::ActiveRelease,
        true,
        None,
        &mut cache,
        Duration::from_secs(60),
        &mut second_runtime,
        &mut restored_handoff,
    )
    .expect("second exit");
    assert_eq!(second_exit, TuiExit::UserRequested);
    let rendered = buffer_text(&second_terminal);
    assert!(!rendered.contains('☂'), "old screen cells survived restart");
    let pt = crate::monitor::i18n::text(Language::PtBr);
    assert!(
        rendered.contains(pt.pressure) || rendered.contains("WSL2"),
        "expected pressure screen rendered in resumed session: {rendered}"
    );
    assert!(
        rendered.contains("English"),
        "expected PtBr toggle hint in resumed pressure view: {rendered}"
    );
}
