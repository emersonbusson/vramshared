// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/render/panels.rs
//! Core ratatui panel renderers for the monitor dashboard.

use super::*;

#[cfg(test)]
pub(in crate::monitor) fn draw_dashboard(
    frame: &mut Frame<'_>,
    observation: &Observation,
    history: &DashHistory,
) {
    let mut identity_cache = InstallationIdentityCache::default();
    draw_dashboard_with_metadata(
        frame,
        observation,
        history,
        &DashboardMetadata::current(
            &mut identity_cache,
            ProcessInstallOrigin::Unknown,
            false,
            None,
        ),
    );
}

/// Split a second count into whole hours, minutes and seconds for the header.
///
/// The seconds field is always `0..=59`. An uptime that only prints minutes
/// becomes unreadable past a few hours — a multi-hour host renders
/// `1331m 30s`, which reads as a broken counter — so the hours are named
/// once the total has them.
pub(in crate::monitor) fn format_uptime_seconds(uptime: u64) -> String {
    let hours = uptime / 3600;
    let minutes = (uptime % 3600) / 60;
    let seconds = uptime % 60;
    match (hours, minutes) {
        (0, 0) => format!("{seconds}s"),
        (0, _) => format!("{minutes}m {seconds:02}s"),
        (_, _) => format!("{hours}h {minutes:02}m {seconds:02}s"),
    }
}

/// The dashboard header reports two independent questions and never folds
/// them into one verdict.
///
/// `status_text` answers "is the product healthy right now?" from `ok` /
/// `overall_state`. A released cache blocks the product even while the
/// protection plane is ready, so this can be red while protection is not.
///
/// `protection_text` answers "what does the protection plane itself claim?".
/// Folding both into one match is what printed `Protection: BLOCKED` beside
/// a live `protection_state: READY`.
///
/// A positive protection claim needs a live daemon to back it. A stale
/// `ACTIVE`, `READY` or `AT_RISK` with no daemon is not protection — the
/// standing stale-green rule refuses to print it as one.
///
/// The daemon state is a three-state witness, not a boolean. `Some(false)`
/// is a **measured** down daemon and refutes the claim to `BLOCKED`. `None`
/// is a liveness the frame never took, which is neither a backing nor a
/// refutation: the positive word is still refused (nothing backs it) and so
/// is `BLOCKED` (nothing refuted it). Folding `None` into `false` printed
/// `Protection: BLOCKED` beside a protection plane this frame never looked
/// at — the same defect the allocation footer closed when it stopped
/// asserting a guarantee the same screen could refute.
pub(in crate::monitor) fn dashboard_header_verdict(
    protection_state: &str,
    daemon_alive: Option<bool>,
    status_ok: bool,
) -> (&'static str, Color, &'static str) {
    let (status_text, state_color) = match (protection_state, daemon_alive, status_ok) {
        ("ACTIVE", Some(true), true) => ("🟢 STATUS: OPERATIONAL & PROTECTED", Color::Green),
        ("READY", Some(true), true) => ("🟡 STATUS: ARMED & READY", Color::Yellow),
        ("OFF", _, _) => ("⚪ STATUS: OFF", Color::DarkGray),
        ("AT_RISK", Some(true), _) => ("🟠 STATUS: AT RISK", Color::Yellow),
        // A positive claim needs a measured-live daemon behind it. A
        // measured-down daemon is a refutation, and so is a product that
        // reports not-ok. An unmeasured daemon is neither and falls through
        // to the fold's own absence arm rather than borrowing the
        // refutation's word.
        ("BLOCKED", _, _)
        | ("ACTIVE" | "READY" | "AT_RISK", Some(false), _)
        | ("ACTIVE" | "READY", _, false) => ("🔴 STATUS: BLOCKED", Color::Red),
        _ => ("🟡 STATUS: UNKNOWN", Color::Yellow),
    };
    let protection_text = match (protection_state, daemon_alive) {
        ("ACTIVE", Some(true)) => "ACTIVE",
        ("READY", Some(true)) => "READY",
        ("AT_RISK", Some(true)) => "AT RISK",
        ("OFF", _) => "OFF",
        ("BLOCKED", _) => "BLOCKED",
        ("ACTIVE" | "READY" | "AT_RISK", Some(false)) => "BLOCKED",
        _ => "UNKNOWN",
    };
    (status_text, state_color, protection_text)
}

/// Plain-language cause for a cache refusal. The wire reason keeps its
/// precise internal wording; the dashboard names the operator-facing fact.
pub(in crate::monitor) fn plain_cache_reason(reason: &str) -> String {
    if reason.contains("lease expired") {
        "host lease expired".to_string()
    } else if reason.contains("safe mode after disconnect") {
        "host lease lost".to_string()
    } else if reason.contains("requires a live vsock lease") {
        "no host lease".to_string()
    } else if reason.contains("paused by control-plane memory pressure") {
        "paused by emergency RAM pressure (auto-recovering)".to_string()
    } else {
        reason.to_string()
    }
}

pub(in crate::monitor) fn draw_dashboard_with_metadata(
    frame: &mut Frame<'_>,
    observation: &Observation,
    _history: &DashHistory,
    metadata: &DashboardMetadata,
) {
    let mut header_lines = vec![
        Line::from(metadata.version_line.as_str()),
        Line::from(metadata.running_line.as_str()),
    ];
    if let Some(notice) = metadata.update_notice.as_deref() {
        header_lines.push(Line::from(notice));
    }
    for install_line in &metadata.install_lines {
        header_lines.push(Line::from(install_line.as_str()));
    }
    if let Some(notice) = metadata.runtime_notice.as_deref() {
        header_lines.push(Line::from(notice));
    }

    let daemon_alive = observation.daemon_alive();

    let protection_state = observation.string("protection_state");
    let status_ok = observation.bool_value("ok") == Some(true);
    let (status_text, old_color, protection_text) =
        dashboard_header_verdict(protection_state, daemon_alive, status_ok);
    let cache_state = observation.string("cache_state");
    let cache_state = if cache_state.is_empty() || cache_state.eq_ignore_ascii_case("unknown") {
        if observation
            .value("cache_unavailable_reason")
            .and_then(Value::as_str)
            .is_some_and(|reason| !reason.trim().is_empty())
        {
            "OFF"
        } else {
            "UNKNOWN"
        }
    } else {
        cache_state
    };
    let origin_state = observation.string("origin_state");
    let origin_state = if origin_state.is_empty() {
        "UNKNOWN"
    } else {
        origin_state
    };
    let overall_state = observation.string("overall_state");
    let cache_reason = observation
        .value("cache_unavailable_reason")
        .and_then(Value::as_str)
        .map(plain_cache_reason);
    let cache_text = match cache_state {
        "ACTIVE" => "GPU cache: ACTIVE".to_string(),
        _ => match cache_reason.as_deref() {
            Some(reason) => format!("GPU cache: {cache_state} ({reason})"),
            None => format!("GPU cache: {cache_state}"),
        },
    };
    let overall_is_unknown =
        overall_state.is_empty() || overall_state.eq_ignore_ascii_case("unknown");
    let overall_text = if overall_is_unknown {
        match status_text {
            text if text.contains("OPERATIONAL & PROTECTED") => "HEALTHY",
            text if text.contains("ARMED & READY") => "READY",
            text if text.contains("STATUS: OFF") => "OFF",
            text if text.contains("AT RISK") => "DEGRADED",
            text if text.contains("STATUS: BLOCKED") => "BLOCKED",
            _ => "UNKNOWN",
        }
    } else {
        match overall_state {
            "GUARDED" => "DEGRADED",
            "HEALTHY" => "HEALTHY",
            "BLOCKED" => "BLOCKED",
            "CRITICAL" => "CRITICAL",
            "EMERGENCY" => "EMERGENCY",
            "SAFE_MODE" => "SAFE MODE",
            _ => overall_state,
        }
    };
    let state_color = if overall_is_unknown {
        old_color
    } else {
        match overall_state {
            "HEALTHY" => Color::Green,
            "BLOCKED" | "CRITICAL" | "EMERGENCY" | "SAFE_MODE" => Color::Red,
            _ => Color::Yellow,
        }
    };

    // Labelled: this is how long the machine has been up, not how long the
    // cascade has been armed. Unlabelled it reads as the second. A boot under
    // one second old is a real zero and is not worth a number; an unreadable
    // counter is not that, and says so rather than borrowing the zero's
    // silence.
    let live_uptime = match observation.control_plane.uptime_seconds {
        Some(uptime) if uptime > 0 => format!("up {}", format_uptime_seconds(uptime)),
        Some(_) => "up".to_string(),
        None => "uptime not measured".to_string(),
    };
    // The verdict above is about the product's own state and is not withdrawn
    // by a failed telemetry refresh. What must not happen is a reader taking
    // that verdict as a live sample: the collection-staleness dimension is a
    // separate fact and is marked beside it.
    let stale_note = if observation_sample_is_stale(observation) {
        " (sample stale)".to_string()
    } else {
        String::new()
    };
    header_lines.push(Line::from(format!(
        "Overall: {overall_text}{stale_note} │ Swap: {protection_text} │ {cache_text} │ Origin: {origin_state} │ ⏱️ host {live_uptime}"
    )));

    // Content plus the two borders. The install block shrinks when the
    // installs agree on one image, so the height follows it.
    let header_height = u16::try_from(header_lines.len() + 2).unwrap_or(8);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height),
            Constraint::Percentage(35),
            Constraint::Percentage(55),
            Constraint::Length(2),
        ])
        .split(frame.area());
    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[1]);
    let bottom = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(rows[2]);

    let header = Paragraph::new(header_lines)
        .style(Style::default().fg(state_color))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("System Overview"),
        );
    frame.render_widget(header, rows[0]);

    draw_memory(frame, top[0], observation);
    draw_gpu(frame, top[1], observation);
    draw_tiers(frame, bottom[0], observation);
    draw_control(frame, bottom[1], observation);
    frame.render_widget(
        Paragraph::new(" [q / Esc]: exit │ Swap tiers: zram → managed tier → fallback"),
        rows[3],
    );
}

pub(in crate::monitor) fn draw_memory(
    frame: &mut Frame<'_>,
    area: Rect,
    observation: &Observation,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(1)])
        .split(area);
    let memory = &observation.mem;
    // Both fills are ratios of the exact KiB figures, never of their whole-MiB
    // faces. Rounding first is how 1536 / 2048 KiB printed `100%` on the tier
    // panel — the same division, one panel over. The two percentages come from
    // the two named readers so this row and the sparkline cannot drift apart.
    //
    // The two pairs fail independently: a missing SwapTotal no longer hides a
    // measured RAM pair behind one "telemetry unavailable", and vice versa.
    let ram_pair = memory.ram_pair();
    let swap_pair = memory.swap_pair();
    let ram_pct = memory_used_pct(memory);

    // The whole-MiB figures are the publish face — `div_ceil`, so a present
    // sub-MiB hold can never print as a measured zero. See [`published_mib`].
    let bar_len: u64 = BAR_MAX_CELLS;
    let sample_stale = observation_sample_is_stale(observation);
    let pressure_text = format_memory_pressure(&observation.control_plane, sample_stale);
    // Both rows are levels: they keep their last-known value and the panel
    // title carries the one collection-staleness mark. Marking the row label
    // instead was a second wording beside the control panel's `Read: stale`.
    let ram_label = observation.memory_scope.ram_label();
    let ram_summary = match ram_pair {
        Some((total_kib, available_kib)) => {
            let ram_used_kib = total_kib.saturating_sub(available_kib);
            let bar = make_tier_bar(ram_used_kib, total_kib, bar_len);
            let used_pct_str = tier_fill_label(ram_used_kib, ram_pct);
            let used_mib_text = format_grouped_number(published_mib(ram_used_kib));
            let total_mib_text = format_grouped_number(published_mib(total_kib));
            format!(" {ram_label}:  {bar} {used_pct_str} ({used_mib_text} / {total_mib_text} MiB)")
        }
        None => format!(" {ram_label}: telemetry unavailable"),
    };
    let swap_summary = match swap_pair {
        Some((swap_total_kib, swap_free_kib)) => {
            let swap_used_kib = swap_total_kib.saturating_sub(swap_free_kib);
            let swap_pct = tier_fill_pct(swap_used_kib, swap_total_kib);
            let swap_bar = make_tier_bar(swap_used_kib, swap_total_kib, bar_len);
            let swap_pct_str = tier_fill_label(swap_used_kib, swap_pct);
            let swap_used_mib_text = format_grouped_number(published_mib(swap_used_kib));
            let swap_total_mib_text = format_grouped_number(published_mib(swap_total_kib));
            format!(
                " Total Swap: {swap_bar} {swap_pct_str} ({swap_used_mib_text} / {swap_total_mib_text} MiB)"
            )
        }
        None => " Total Swap: telemetry unavailable".to_string(),
    };
    let text = format!("{ram_summary}\n{swap_summary}\n{pressure_text}");
    frame.render_widget(
        Paragraph::new(text).block(Block::default().borders(Borders::ALL).title(
            stale_panel_title(observation.memory_scope.panel_title(), sample_stale),
        )),
        chunks[0],
    );
    // The current compression state, not a percent-over-time sparkline: the
    // row below is what zram actually did (original -> stored), with the
    // ratio and every measured counter beside it.
    let compression = compression_row(observation.value("zram_compression"), bar_len, sample_stale);
    frame.render_widget(
        Paragraph::new(format!(" {compression}"))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Compressed RAM swap"),
            )
            .wrap(Wrap { trim: true }),
        chunks[1],
    );
}

/// The VRAM-budget block: one adapter budget sample, or its absence.
///
/// A budget occupancy is a **level**, so an aged sample is kept on screen and
/// marked — the level policy [`stale_panel_title`] states. It is never
/// rewritten into the absence wording: that is only for a body this panel
/// cannot show at all (no sample, a sample
/// this reader cannot use, or a budget with no denominator). Collapsing an
/// aged reading into "unavailable" told an operator the driver never spoke
/// when it spoke seconds ago.
///
/// The same holds for the sample's **source**. A provider estimate is a real
/// figure of a less authoritative kind and is shown marked
/// [`ESTIMATE_SAMPLE_ROW`], not rewritten into the absence wording — which
/// would say the provider never spoke when it published an estimate. Only the
/// driver's own report is allocated against; that decision is
/// [`ramshared_vram::GpuBudgetTelemetry::trusted_available_at`] and is
/// deliberately not this row's.
///
/// The marks sit on the budget line because every figure in the block is that
/// one sample; a reader who sees them knows the adapter name, the bar and the
/// free figure beside it are the same reading, last taken at
/// [`GpuObservation::sampled_at_unix_ms`].
///
/// A failed **collection** is a different fact and is carried by the panel
/// title instead (see [`STALE_SAMPLE_NOTICE`]): the level keeps its
/// last-known value there too, and the title is what says the screen stopped
/// refreshing. This row therefore never consults
/// [`observation_sample_is_stale`] — one notice per fact, no second wording.
///
/// `now_unix_ms` is the frame's freshness clock — the caller passes the
/// observation's own [`Observation::epoch_ms`], never a wall-clock read at
/// draw time. See [`Observation::epoch_ms`] for why one frame carries one
/// clock.
pub(in crate::monitor) fn gpu_budget_row(gpu: Option<&GpuObservation>, now_unix_ms: u64) -> String {
    const ABSENT: &str = " Active worker GPU budget unavailable";
    let Some(gpu) = gpu else {
        return ABSENT.to_string();
    };
    // Occupancy from the exact byte figures, never from the whole-MiB views
    // below. Rounding first would move the number off the ratio the driver
    // reported (1536 / 2048 bytes is 75%; its rounded MiB halves say 50%).
    //
    // A budget of zero has no denominator, so there is no occupancy ratio to
    // draw and none to print. The bar draws empty — nothing is held against a
    // capacity the driver did not report — and the slot names the missing
    // ratio rather than inventing a `0%`, which would claim a fullness
    // measured against a denominator that does not exist. The rest of the row
    // is still the driver's figures and still prints: a measured zero budget
    // is a product state, not the panel's absence.
    let bar_len: u64 = BAR_MAX_CELLS;
    // The bar and the occupancy label are the tier panel's two faces, not a
    // second implementation of them. Routing through [`make_tier_bar`] and
    // [`tier_fill_label`] is what keeps this row under the same two honesty
    // rules those own: a sub-1% hold draws one cell and prints `<1%`, and a
    // hold that rounds down to zero is never a measured `0%`. The inline form
    // this replaces dropped both.
    let bar = make_tier_bar(gpu.used_bytes, gpu.budget_bytes, bar_len);
    // This panel's absence word is its own and is not `tier_fill_label`'s
    // `n/a`: a zero budget is a **measured** zero capacity the driver
    // reported, and the word names that state rather than a missing ratio in
    // the abstract. The three-state idiom is the shape, not the literal.
    let occupancy = match gpu.used_percent_of_budget() {
        None => "no denominator".to_string(),
        Some(pct) => tier_fill_label(gpu.used_bytes, Some(pct)),
    };
    let physical_total = gpu
        .total_mib()
        .map_or_else(|| "unknown".to_string(), |total| format!("{total} MiB"));
    // Two independent facts can hold at once and each keeps its own mark, in
    // the order they became true: the source is a property of the body from
    // the moment it was published, the age a property of the clock. Merging
    // them into one word would hide which of the two failed. The age mark is
    // the **sample's own age**, not the collection's: the record can go cold
    // while the refresh is still succeeding (the worker stopped publishing).
    // Both stay on this line because every figure in the block is that one
    // sample.
    let mut marks = String::new();
    if gpu.source != GpuBudgetSource::DriverReported {
        marks.push_str(&format!(" ({ESTIMATE_SAMPLE_ROW})"));
    }
    if !gpu.is_fresh_at(now_unix_ms, GPU_BUDGET_MAX_AGE_MS) {
        marks.push_str(&format!(" ({STALE_SAMPLE_ROW})"));
    }
    // The occupancy on this line is VRAM budget **occupancy**. It is worded as
    // such so it can never be read as compute utilization — that is the
    // busy-time row below, which is a different quantity.
    //
    // The two figures beside the bar are whole MiB because the display names
    // MiB. They come from `*_mib()`, which ceilings a non-zero reading so a
    // present sub-MiB hold or budget can never print as a measured zero. The
    // free figure on the line below is a different face of the same rounding
    // rule and points the other way; see [`free_bound_label`].
    format!(
        " GPU: {} ({})\n Usable memory:  {bar} {occupancy} ({} / {} MiB){marks}\n Free:           {} within usable\n Total:          {physical_total}",
        gpu.adapter.backend,
        gpu.adapter.key,
        gpu.used_mib(),
        gpu.budget_mib(),
        free_bound_label(gpu.free_bytes, gpu.free_mib()),
    )
}

pub(in crate::monitor) fn draw_gpu(frame: &mut Frame<'_>, area: Rect, observation: &Observation) {
    // One freshness clock for the whole panel: every aged figure in this
    // frame is measured against the observation's own stamp, bound once here
    // so a third row cannot pick a second clock by accident.
    let now_unix_ms = observation.epoch_ms;
    let budget_text = gpu_budget_row(observation.gpu.as_ref(), now_unix_ms);
    // Busy time rides beside the budget, not inside it: the panel shows both
    // "how much VRAM" and "how hard is the adapter working", each labelled
    // with its own quantity so neither can be mistaken for the other.
    //
    // Busy time is a rate over a sample window, so a failed refresh must not
    // leave it reading as live beside the control panel's `Read: stale`; it
    // spells its own age `stale` too. The budget occupancy is a level and
    // keeps its last-known value under either cause, marked in its own row
    // when the sample aged and by the panel title when the collection failed.
    let sample_stale = observation_sample_is_stale(observation);
    let utilization_text = utilization_row(
        observation.value("gpu_utilization"),
        20,
        sample_stale,
        now_unix_ms,
    );
    let gpu_title = stale_panel_title("GPU", sample_stale);
    let cache_state = observation.string("cache_state");
    let cache_reason = observation
        .value("cache_unavailable_reason")
        .and_then(Value::as_str)
        .map(plain_cache_reason);
    let utilization_present = observation
        .value("gpu_utilization")
        .is_some_and(|value| !value.is_null());
    let cache =
        crate::monitor::observations::cache_telemetry_record(observation.value("gpu_cache"));
    let mut lines = vec![budget_text];
    if utilization_present {
        lines.push(utilization_text);
    }
    if cache_state == "ACTIVE"
        && cache
            .as_ref()
            .is_some_and(|cache| cache.compressed_payload_bytes > 0)
    {
        lines.push(cache_compaction_row(observation.value("gpu_cache")));
    }
    let text = lines.join("\n");
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(7), Constraint::Min(1)])
        .split(area);
    frame.render_widget(
        Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL).title(gpu_title))
            .wrap(Wrap { trim: true }),
        rows[0],
    );
    let cache_text = if cache_state == "ACTIVE" {
        match cache {
            Some(cache) => format!(
                " GPU cache: ACTIVE\n Held: {}\n Stored raw: {} · codec: {:?}",
                format_volume_bytes(cache.logical_cached_bytes).trim(),
                format_volume_bytes(cache.raw_bypass_bytes).trim(),
                cache.codec.state,
            ),
            None => " GPU cache: ACTIVE\n Telemetry pending".to_string(),
        }
    } else {
        format!(
            " GPU cache: {cache_state}{}\n Swap and origin are unaffected.\n Start the host authority to re-enable the cache.",
            cache_reason
                .as_deref()
                .map(|reason| format!(" — {reason}"))
                .unwrap_or_default(),
        )
    };
    frame.render_widget(
        Paragraph::new(cache_text)
            .block(Block::default().borders(Borders::ALL).title("GPU cache"))
            .wrap(Wrap { trim: true }),
        rows[1],
    );
}

pub(in crate::monitor) fn draw_control(
    frame: &mut Frame<'_>,
    area: Rect,
    observation: &Observation,
) {
    let width = area.width;
    let sep_len = (width.saturating_sub(4) as usize).max(20);
    let sep = "─".repeat(sep_len);

    let pid = observation
        .value("daemon")
        .and_then(|d| d.get("pid"))
        .and_then(Value::as_u64)
        .map(|p| p.to_string())
        .unwrap_or_else(|| "-".to_string());

    let daemon_row = daemon_status_row(observation.daemon_alive());

    let errors = if observation.errors.is_empty() {
        "None".to_string()
    } else {
        format!("{}", observation.errors.len())
    };
    let sample_age_ms = observation.sample_age_ms;

    let swap_in = page_io_side(observation.control_plane.swap_in_pages);
    let swap_out = page_io_side(observation.control_plane.swap_out_pages);
    let sample_stale = observation_sample_is_stale(observation);
    let rate_windows = observation.control_plane.rate_windows;
    let read_mbs = if sample_stale {
        STALE_SAMPLE_ROW.to_string()
    } else {
        format_rate_cell(observation.control_plane.swap_read_mbs)
    };
    let write_mbs = if sample_stale {
        STALE_SAMPLE_ROW.to_string()
    } else {
        format_rate_cell(observation.control_plane.swap_write_mbs)
    };
    // The figure is the unit's activation duration — how long `systemctl`
    // took to take the tier unit from inactive to active. It is a duration
    // and only a duration: it is not a readiness state, and the face must not
    // publish one. Whether the tier is usable right now is the daemon row's
    // fact, measured separately; a duration that happened to complete says
    // nothing about the present.
    let boot_info = match observation.control_plane.boot_tier_latency_ms {
        Some(ms) => format!("{}s", display_seconds_hundredths(ms as f64 / 1000.0)),
        // Never invent a measurement: absent telemetry renders as unmeasured.
        None => "not measured".to_string(),
    };

    // A peak is a level figure, so a failed refresh keeps the last one and the
    // panel carries the stale notice. A run that has not produced a window has
    // no peak at all: those three publish `null` — an empty maximum, not a
    // measured zero. Both witnesses are asked together, because a populated
    // triple with no accepted window would still be describing nothing, and a
    // half-populated triple is not a sample.
    let peak_info = match (
        rate_windows,
        observation.control_plane.swap_peak_mbs,
        observation.control_plane.swap_read_peak_mbs,
        observation.control_plane.swap_write_peak_mbs,
    ) {
        (windows, Some(peak_mbs), Some(swap_read_peak), Some(swap_write_peak)) if windows > 0 => {
            format!(
                "🚀 {:>5} MB/s (⬇️ {:>4} │ ⬆️ {:>4} MB/s)",
                display_mbs_tenths(peak_mbs),
                display_mbs(swap_read_peak),
                display_mbs(swap_write_peak),
            )
        }
        _ => "not measured".to_string(),
    };
    // A derived state, so a frozen sample takes the derived marker rather
    // than a state word the rates can no longer support. The slot keeps its
    // parenthetical shape across all four faces; the panel title carries
    // [`STALE_SAMPLE_NOTICE`] for the panel as a whole, so this is the
    // figure's own face and not a second notice.
    let speed_state = if sample_stale {
        format!("({STALE_SAMPLE_ROW})")
    } else {
        match (
            observation.control_plane.swap_read_mbs,
            observation.control_plane.swap_write_mbs,
        ) {
            (None, _) | (_, None) => "(not measured)".to_string(),
            (Some(r), Some(w)) if r < SWAP_STANDBY_MAX_MBS && w < SWAP_STANDBY_MAX_MBS => {
                "(💤 Standby)".to_string()
            }
            (Some(_), Some(_)) => "(⚡ Active)".to_string(),
        }
    };
    // `pswpin` / `pswpout` are base-page counts, not bytes; `page_io_side`
    // applies the unit contract named beside `parse_vmstat` and carries each
    // side's own absence face.
    let fault_rates = format_page_fault_rates(
        observation.control_plane.pgfault_per_sec,
        observation.control_plane.pgmajfault_per_sec,
    );

    let bench_info = if observation.control_plane.reclaim_speed_gbs > 0.0 {
        // Every figure below is a measurement, so each goes through the
        // display face for its unit and resolution. A figure that rounds to
        // this row's zero is below the display resolution and prints `<` the
        // smallest token the face can show — never a zero nobody measured.
        let speed = display_gbs(observation.control_plane.reclaim_speed_gbs);
        let duration = display_ms(observation.control_plane.reclaim_duration_ms);
        if observation.control_plane.benchmark_p50_lat_ms > 0.0 {
            format!(
                "⚡ {speed} GB/s ({duration} ms │ P50 {}ms / P99 {}ms) │ {}",
                display_ms_hundredths(observation.control_plane.benchmark_p50_lat_ms),
                display_ms_hundredths(observation.control_plane.benchmark_p99_lat_ms),
                observation.control_plane.benchmark_status
            )
        } else {
            format!(
                "⚡ {speed} GB/s ({duration} ms) │ {}",
                observation.control_plane.benchmark_status
            )
        }
    } else if observation.control_plane.benchmark_status == BENCHMARK_STATUS_PATH_NOT_FOUND {
        "Benchmark evidence path not found".to_string()
    } else {
        "Awaiting benchmark evidence".to_string()
    };

    let pf_lat_info = display_pf_lat(observation.control_plane.estimated_page_fault_lat_us);

    // Both footer facts come from the status document's own fold, never from
    // a constant. `order_ok` is the swap-priority witness; `disk_growth_kib`
    // is what the fallback SSD swap holds above this activation's baseline.
    // A JSON `null` on either is absence and lands on the face's unmeasured
    // arm, never on a zero.
    let order_info = allocation_order_row(observation.bool_value("order_ok"));
    let ssd_baseline_info = ssd_baseline_row(
        observation
            .value("activation")
            .and_then(|activation| activation.get("disk_growth_kib"))
            .and_then(Value::as_u64),
    );
    let safety_guard = safety_guard_row(observation.guardian_state());

    let text = format!(
        concat!(
            " Daemon Status:            {daemon_row} (PID {pid})\n",
            " Boot Initialization:      ⏱️  {boot_info}\n",
            " Telemetry Sample Age:     {sample_age_ms} ms\n",
            " Safety Guard:             🛡️  {safety_guard}\n",
            " Swap I/O Protocol:        ⚡ Synchronous Zero-Copy (.rw_page)\n",
            " Reclaim Performance:      {bench_info}\n",
            " Estimated Page Fault Latency: {pf_lat_info}\n",
            " {sep}\n",
            " Real-Time Speed:          Read: {read_mbs:>5} │ Write: {write_mbs:>5} MB/s {speed_state}\n",
            " Peak Recorded Speed:      {peak_info}\n",
            " Cumulative Page I/O:      In: {swap_in} │ Out: {swap_out}\n",
            " Page Faults Rate:         📊 {fault_rates}\n",
            " Anomaly Counter:          {errors}\n",
            " {sep}\n",
            " ⚡ Allocation Order:  {order_info}\n",
            " SSD vs Baseline:     {ssd_baseline_info}",
        ),
        daemon_row = daemon_row,
        pid = pid,
        boot_info = boot_info,
        sample_age_ms = sample_age_ms,
        safety_guard = safety_guard,
        bench_info = bench_info,
        pf_lat_info = pf_lat_info,
        read_mbs = read_mbs,
        write_mbs = write_mbs,
        speed_state = speed_state,
        peak_info = peak_info,
        swap_in = swap_in,
        swap_out = swap_out,
        fault_rates = fault_rates,
        errors = errors,
        order_info = order_info,
        ssd_baseline_info = ssd_baseline_info,
        sep = sep,
    );

    frame.render_widget(
        Paragraph::new(text).block(
            Block::default()
                .borders(Borders::ALL)
                .title(stale_panel_title("Diagnostics & Live Stats", sample_stale)),
        ),
        area,
    );
}
