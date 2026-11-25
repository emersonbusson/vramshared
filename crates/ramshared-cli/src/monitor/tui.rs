// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/tui.rs
//! Interactive ratatui event loop and update re-exec handling.

mod handoff;

use super::*;
#[cfg(all(unix, test))]
pub(super) use handoff::reexec_command;
pub(super) use handoff::{
    TuiHandoffState, exec_restarted_process, leave_raw_mode_for_handoff, read_handoff_env,
};

#[cfg(test)]
pub(super) fn should_exit_tui(event_opt: Option<Event>) -> bool {
    if let Some(Event::Key(key)) = event_opt {
        key.kind == KeyEventKind::Press
            && (matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
                || (key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL)))
    } else {
        false
    }
}

pub(super) fn run_tui(options: &MonitorOptions) -> Result<(), MonitorError> {
    let process_origin = current_process_install_origin();
    let mut identity_cache = InstallationIdentityCache::default();
    let mut allow_reexec = true;
    let mut runtime_notice = None;
    let mut active_handoff = read_handoff_env();

    loop {
        let mut terminal = ratatui::init();
        let result = tui_loop_with_handoff(
            &mut terminal,
            options,
            process_origin,
            allow_reexec,
            runtime_notice.as_deref(),
            &mut identity_cache,
            IDENTITY_REFRESH_INTERVAL,
            &mut LiveTuiRuntime,
            &mut active_handoff,
        );

        match result {
            Ok(TuiExit::UserRequested) => {
                ratatui::restore();
                return Ok(());
            }
            Err(error) => {
                ratatui::restore();
                return Err(error);
            }
            Ok(TuiExit::Restart(executable)) => {
                // Keep the alternate screen active across in-place `exec` so an
                // update never flashes the user's shell scrollback.
                leave_raw_mode_for_handoff();
                let previous_release = current_release_version();
                let carried = active_handoff.take();
                match exec_restarted_process(
                    &executable,
                    previous_release.as_deref(),
                    carried.as_ref(),
                ) {
                    Ok(()) => unreachable!("exec replaces the process on success"),
                    Err(error) => {
                        allow_reexec = false;
                        active_handoff = carried;
                        runtime_notice = Some(format!(
                            "Update restart failed; continuing this process: {error}"
                        ));
                    }
                }
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum TuiExit {
    UserRequested,
    Restart(PathBuf),
}

/// The three OS-facing seams [`tui_loop`] refreshes through.
///
/// Production wires crossterm events, live host collection and the install
/// identity probe ([`LiveTuiRuntime`]). Tests script all three so the
/// display-honesty rules the loop holds are reachable without a terminal or a
/// live host — the same injection boundary `WorkerProcessControl` is in the
/// daemon supervisor. The loop's draw, publish and timing bounds do not move.
pub(super) trait TuiRuntime {
    /// The next input event, or `None` when `wait` elapses with none.
    fn next_event(&mut self, wait: Duration) -> Option<Event>;
    /// One host observation for this refresh.
    fn observe(&mut self, benchmark_evidence: &Path) -> Result<Observation, MonitorError>;
    /// Install identity and the restart target for this frame.
    fn metadata(
        &mut self,
        identity_cache: &mut InstallationIdentityCache,
        process_origin: ProcessInstallOrigin,
        allow_reexec: bool,
        runtime_notice: Option<&str>,
    ) -> DashboardMetadata;
}

/// Production seams: crossterm, [`collect_observation`] and
/// [`DashboardMetadata::current`]. Behaviour is exactly the pre-injection
/// loop: `poll` then `read`, live collection, the live identity probe.
pub(super) struct LiveTuiRuntime;

impl TuiRuntime for LiveTuiRuntime {
    fn next_event(&mut self, wait: Duration) -> Option<Event> {
        if event::poll(wait).unwrap_or(false) {
            event::read().ok()
        } else {
            None
        }
    }

    fn observe(&mut self, benchmark_evidence: &Path) -> Result<Observation, MonitorError> {
        collect_observation(benchmark_evidence)
    }

    fn metadata(
        &mut self,
        identity_cache: &mut InstallationIdentityCache,
        process_origin: ProcessInstallOrigin,
        allow_reexec: bool,
        runtime_notice: Option<&str>,
    ) -> DashboardMetadata {
        DashboardMetadata::current(identity_cache, process_origin, allow_reexec, runtime_notice)
    }
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)] // explicit seam boundary keeps the loop testable
pub(super) fn tui_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    options: &MonitorOptions,
    process_origin: ProcessInstallOrigin,
    allow_reexec: bool,
    runtime_notice: Option<&str>,
    identity_cache: &mut InstallationIdentityCache,
    identity_refresh_interval: Duration,
    runtime: &mut dyn TuiRuntime,
) -> Result<TuiExit, MonitorError> {
    let mut handoff = None;
    tui_loop_with_handoff(
        terminal,
        options,
        process_origin,
        allow_reexec,
        runtime_notice,
        identity_cache,
        identity_refresh_interval,
        runtime,
        &mut handoff,
    )
}

#[allow(clippy::too_many_arguments)] // explicit seam boundary preserves seamless re-exec UI state
pub(super) fn tui_loop_with_handoff<B: Backend>(
    terminal: &mut Terminal<B>,
    options: &MonitorOptions,
    process_origin: ProcessInstallOrigin,
    allow_reexec: bool,
    runtime_notice: Option<&str>,
    identity_cache: &mut InstallationIdentityCache,
    identity_refresh_interval: Duration,
    runtime: &mut dyn TuiRuntime,
    handoff_state: &mut Option<TuiHandoffState>,
) -> Result<TuiExit, MonitorError> {
    let mut dashboard_metadata =
        runtime.metadata(identity_cache, process_origin, allow_reexec, runtime_notice);
    // A pending self-refresh is taken **after** one frame is drawn, so the
    // operator sees which transition is happening before the process is
    // replaced (SPEC DT-4 / NFR-5).
    let mut pending_restart = dashboard_metadata.restart_executable.clone();
    let mut next_identity_refresh = Instant::now() + identity_refresh_interval;
    let history_limit = ((options.history_seconds * MILLIS_PER_SECOND) / options.interval_ms)
        .clamp(HISTORY_MIN_SAMPLES, HISTORY_MAX_SAMPLES) as usize;
    let mut ui = handoff_state
        .take()
        .unwrap_or_else(|| TuiHandoffState::with_language(options.language));
    ui.history.trim(history_limit);
    let interval = Duration::from_millis(options.interval_ms);
    let benchmark_evidence = options.benchmark_evidence_path();
    let mut next_sample = Instant::now();
    let mut observation = runtime.observe(&benchmark_evidence)?;
    let mut last_successful_sample = Instant::now();
    let mut last_io_sample = Some((
        observation.control_plane.swap_read_bytes,
        observation.control_plane.swap_write_bytes,
        observation.control_plane.zram_io.read_bytes,
        observation.control_plane.zram_io.write_bytes,
        observation.control_plane.vram_io.read_bytes,
        observation.control_plane.vram_io.write_bytes,
        observation.control_plane.disk_io.read_bytes,
        observation.control_plane.disk_io.write_bytes,
        Instant::now(),
    ));
    // The initial sample follows the same rule as every later one: an absent
    // pair is not a sample. Recording the options themselves would give the
    // first frame a fabricated pair to difference against.
    let mut last_faults_sample = match (
        observation.control_plane.pgfault_total,
        observation.control_plane.pgmajfault_total,
    ) {
        (Some(pf), Some(mpf)) => Some((pf, mpf)),
        _ => None,
    };

    loop {
        if Instant::now() >= next_sample {
            let refreshed = match runtime.observe(&benchmark_evidence) {
                Ok(new_observation) => {
                    observation = new_observation;
                    last_successful_sample = Instant::now();
                    true
                }
                Err(_) => false,
            };
            if refreshed {
                let now = Instant::now();
                if let Some((
                    last_rb,
                    last_wb,
                    last_z_rb,
                    last_z_wb,
                    last_v_rb,
                    last_v_wb,
                    last_d_rb,
                    last_d_wb,
                    last_t,
                )) = last_io_sample
                {
                    let dt = now.duration_since(last_t).as_secs_f64();
                    let (min_dt, max_dt) = rate_window_secs(options.interval_ms);
                    if (min_dt..=max_dt).contains(&dt) {
                        ui.rate_windows += 1;
                        let divisor = dt * 1_048_576.0;
                        let swap_r = (observation
                            .control_plane
                            .swap_read_bytes
                            .saturating_sub(last_rb) as f64)
                            / divisor;
                        let swap_w = (observation
                            .control_plane
                            .swap_write_bytes
                            .saturating_sub(last_wb) as f64)
                            / divisor;
                        let z_r = (observation
                            .control_plane
                            .zram_io
                            .read_bytes
                            .saturating_sub(last_z_rb) as f64)
                            / divisor;
                        let z_w = (observation
                            .control_plane
                            .zram_io
                            .write_bytes
                            .saturating_sub(last_z_wb) as f64)
                            / divisor;
                        let v_r = (observation
                            .control_plane
                            .vram_io
                            .read_bytes
                            .saturating_sub(last_v_rb) as f64)
                            / divisor;
                        let v_w = (observation
                            .control_plane
                            .vram_io
                            .write_bytes
                            .saturating_sub(last_v_wb) as f64)
                            / divisor;
                        let d_r = (observation
                            .control_plane
                            .disk_io
                            .read_bytes
                            .saturating_sub(last_d_rb) as f64)
                            / divisor;
                        let d_w = (observation
                            .control_plane
                            .disk_io
                            .write_bytes
                            .saturating_sub(last_d_wb) as f64)
                            / divisor;

                        let cp = &mut observation.control_plane;
                        cp.swap_read_mbs = Some(swap_r);
                        cp.swap_write_mbs = Some(swap_w);
                        cp.zram_io.read_mbs = Some(z_r);
                        cp.zram_io.write_mbs = Some(z_w);
                        cp.vram_io.read_mbs = Some(v_r);
                        cp.vram_io.write_mbs = Some(v_w);
                        cp.disk_io.read_mbs = Some(d_r);
                        cp.disk_io.write_mbs = Some(d_w);

                        let total_swap_speed = swap_r + swap_w;
                        ui.swap_peak_mbs = ui.swap_peak_mbs.max(total_swap_speed);
                        ui.swap_read_peak_mbs = ui.swap_read_peak_mbs.max(swap_r);
                        ui.swap_write_peak_mbs = ui.swap_write_peak_mbs.max(swap_w);

                        ui.zram_acc.record(z_r, z_w);
                        ui.zram_acc.apply_to_plane_io(&mut cp.zram_io);

                        ui.vram_acc.record(v_r, v_w);
                        ui.vram_acc.apply_to_plane_io(&mut cp.vram_io);

                        ui.disk_acc.record(d_r, d_w);
                        ui.disk_acc.apply_to_plane_io(&mut cp.disk_io);

                        // The pair is one sample. A rate is a delta over one
                        // window, so this frame's two counters and the previous
                        // pair all have to be present; an absent pair publishes
                        // no rate rather than a fabricated idle.
                        if let (Some(pf), Some(mpf), Some((last_pf, last_mpf))) =
                            (cp.pgfault_total, cp.pgmajfault_total, last_faults_sample)
                        {
                            cp.pgfault_per_sec =
                                Some((pf.saturating_sub(last_pf) as f64 / dt) as u64);
                            cp.pgmajfault_per_sec =
                                Some((mpf.saturating_sub(last_mpf) as f64 / dt) as u64);
                        }
                    }
                }

                if let Some(tiers) = observation.value("tiers").and_then(Value::as_object) {
                    // The peak is a maximum over exact KiB samples — the unit
                    // the wire carries. Converting each sample to whole MiB
                    // before the comparison would round them all first, and
                    // the maximum of rounded samples is not the rounded
                    // maximum. The MiB an operator reads is a publish
                    // conversion at the render boundary, never this fold.
                    //
                    // A tier that published no `used_kib` contributes no
                    // sample. Folding that hole into `0` would drag the peak
                    // down to a number nobody measured, which is the same
                    // defect the usage face refuses on the row itself.
                    for (name, peak) in [
                        ("zram", &mut ui.zram_peak_used_kib),
                        ("vram", &mut ui.vram_peak_used_kib),
                        ("disk", &mut ui.disk_peak_used_kib),
                    ] {
                        if let Some(used_kib) = tiers
                            .get(name)
                            .and_then(Value::as_object)
                            .and_then(|t| t.get("used_kib"))
                            .and_then(Value::as_u64)
                        {
                            *peak = Some(peak.map_or(used_kib, |p| p.max(used_kib)));
                        }
                    }
                }

                let cp = &mut observation.control_plane;
                cp.rate_windows = ui.rate_windows;
                // Swap-plane peaks are maxima over the accepted windows. With
                // no window they are an empty maximum and publish `null`, so
                // the first frame of a run never claims an idle nobody
                // observed. A later refused window keeps the peak a previous
                // one produced: the locals only grow inside an accepted window.
                if ui.rate_windows > 0 {
                    cp.swap_peak_mbs = Some(ui.swap_peak_mbs);
                    cp.swap_read_peak_mbs = Some(ui.swap_read_peak_mbs);
                    cp.swap_write_peak_mbs = Some(ui.swap_write_peak_mbs);
                } else {
                    cp.swap_peak_mbs = None;
                    cp.swap_read_peak_mbs = None;
                    cp.swap_write_peak_mbs = None;
                }
                cp.zram_peak_used_kib = ui.zram_peak_used_kib;
                cp.vram_peak_used_kib = ui.vram_peak_used_kib;
                cp.disk_peak_used_kib = ui.disk_peak_used_kib;
                ui.zram_acc.apply_to_plane_io(&mut cp.zram_io);
                ui.vram_acc.apply_to_plane_io(&mut cp.vram_io);
                ui.disk_acc.apply_to_plane_io(&mut cp.disk_io);

                update_tier_latencies(cp, ui.zram_acc.count, ui.vram_acc.count, ui.disk_acc.count);

                last_io_sample = Some((
                    cp.swap_read_bytes,
                    cp.swap_write_bytes,
                    cp.zram_io.read_bytes,
                    cp.zram_io.write_bytes,
                    cp.vram_io.read_bytes,
                    cp.vram_io.write_bytes,
                    cp.disk_io.read_bytes,
                    cp.disk_io.write_bytes,
                    now,
                ));
                // An absent pair is not a sample, so it clears rather than
                // keeps the previous one. Keeping it would let the next real
                // frame difference across a gap its `dt` does not span: the
                // delta would cover two windows while the divisor named one.
                last_faults_sample = match (cp.pgfault_total, cp.pgmajfault_total) {
                    (Some(pf), Some(mpf)) => Some((pf, mpf)),
                    _ => None,
                };
                if let Ok(flight_line) = serde_json::to_string(&observation) {
                    let flight_dir = flight_sample_dir();
                    publish_flight_sample(&mut observation, flight_dir.as_deref(), &flight_line);
                }
                // Every series holds measured fills only. A frame that
                // carried no reading for a series takes no sample in it:
                // pushing a `0` there would claim a measurement of emptiness
                // nobody made. See [`DashHistory`].
                ui.history.push_observation(&observation);
                ui.history.trim(history_limit);
                next_sample = Instant::now() + interval;
            } else {
                mark_observation_refresh_failed(&mut observation, last_successful_sample.elapsed());
                next_sample = Instant::now() + interval;
            }
        }
        observation.sample_age_ms = last_successful_sample
            .elapsed()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        if Instant::now() >= next_identity_refresh {
            dashboard_metadata =
                runtime.metadata(identity_cache, process_origin, allow_reexec, runtime_notice);
            if pending_restart.is_none() {
                pending_restart = dashboard_metadata.restart_executable.clone();
            }
            next_identity_refresh = Instant::now() + identity_refresh_interval;
        }
        // The same `history` this loop fills above: a fresh
        // `DashHistory::default()` here kept every chart at `not measured`
        // forever, with the series filled and never read.
        let _ = terminal.draw(|frame| {
            dashboard::draw(
                frame,
                &observation,
                &ui.history,
                &dashboard_metadata,
                ui.language,
                ui.screen,
            )
        });
        if let Some(executable) = pending_restart.take() {
            *handoff_state = Some(ui);
            return Ok(TuiExit::Restart(executable));
        }

        let wait = next_sample
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(100));
        let event = runtime.next_event(wait);
        if let Some(Event::Key(key)) = event.as_ref()
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Char('q') => return Ok(TuiExit::UserRequested),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(TuiExit::UserRequested);
                }
                KeyCode::Esc => {
                    if ui.screen == Screen::Main {
                        return Ok(TuiExit::UserRequested);
                    }
                    ui.screen = Screen::Main;
                }
                KeyCode::Char('l') => ui.language = ui.language.toggle(),
                KeyCode::Char('p') => {
                    ui.screen = if ui.screen == Screen::Pressure {
                        Screen::Main
                    } else {
                        Screen::Pressure
                    };
                }
                KeyCode::Char('d') => {
                    ui.screen = if ui.screen == Screen::Details {
                        Screen::Main
                    } else {
                        Screen::Details
                    };
                }
                KeyCode::Char('?') => {
                    ui.screen = if ui.screen == Screen::Help {
                        Screen::Main
                    } else {
                        Screen::Help
                    };
                }
                _ => {}
            }
        }
    }
}
