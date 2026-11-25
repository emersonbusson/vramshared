// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/dashboard/views.rs
//! Secondary dashboard screens: pressure, technical details, and glossary views.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use super::super::i18n::{
    DashboardPhrase, Language, dashboard_phrase, pressure_class, pressure_label, status_label, text,
};
use super::super::model::Observation;
use super::super::render::DashHistory;
use super::{
    cache_snapshot, codec_line, control_plane_line, fmt_bytes, fmt_rate, host_memory_snapshot,
    sparkline,
};

pub(super) fn draw_pressure(
    frame: &mut Frame<'_>,
    observation: &Observation,
    history: &DashHistory,
    language: Language,
) {
    let text = text(language);
    let cp = &observation.control_plane;
    let class = if cp.memory_psi_available {
        pressure_label(language, pressure_class(cp.memory_psi_full_avg10))
    } else {
        text.unavailable
    };
    let psi = |value: f64| {
        if cp.memory_psi_available {
            format!("{}%", language.number(format!("{value:.2}")))
        } else {
            text.unavailable.to_string()
        }
    };
    let mut lines = vec![
        Line::from(format!("{} — {}", text.guest_psi, class)),
        Line::from(format!(
            " full  {}   some  {}",
            psi(cp.memory_psi_full_avg10),
            psi(cp.memory_psi_some_avg10)
        )),
        Line::from(format!(
            " avg10 {} · avg60 {} · avg300 {}",
            psi(cp.memory_psi_full_avg10),
            psi(cp.memory_psi_full_avg60),
            psi(cp.memory_psi_full_avg300)
        )),
        Line::from(text.thresholds),
        Line::from(format!(
            " {} ({})  {}",
            text.pressure,
            dashboard_phrase(language, DashboardPhrase::RecentSamplesPct),
            sparkline(
                &history.pressure_full,
                100,
                usize::from(frame.area().width)
                    .saturating_sub(36)
                    .clamp(1, 48)
            )
        )),
        Line::from(""),
        Line::from(format!("{} — {}", text.host_windows, text.not_measured)),
        Line::from(dashboard_phrase(
            language,
            DashboardPhrase::HostTelemetryAbsent,
        )),
        Line::from(""),
        Line::from(format!(
            "{}: {}",
            text.supervisor,
            status_label(language, observation.string("overall_state"))
        )),
        Line::from(control_plane_line(observation, language)),
        Line::from(dashboard_phrase(
            language,
            DashboardPhrase::DegradedVerdictNote,
        )),
        Line::from(""),
    ];
    let host_lines = if let Some(host) = host_memory_snapshot(observation) {
        let age = host.age_ms(observation.epoch_ms);
        let stale = age.is_none_or(|age| age > 15_000);
        let state = if stale {
            format!(" ({})", text.stale)
        } else {
            String::new()
        };
        vec![
            Line::from(format!(
                " {}: {} / {} · commit {} · vmmemWSL {}{}",
                text.host_windows,
                fmt_bytes(host.available_bytes, language),
                fmt_bytes(host.total_bytes, language),
                host.commit_headroom_bytes()
                    .map(|bytes| fmt_bytes(bytes, language))
                    .unwrap_or_else(|| text.not_measured.to_string()),
                host.vmmem_wsl_working_set_bytes
                    .map(|bytes| fmt_bytes(bytes, language))
                    .unwrap_or_else(|| text.not_measured.to_string()),
                state
            )),
            Line::from(dashboard_phrase(
                language,
                DashboardPhrase::HostMemoryVsPsiNote,
            )),
            Line::from(""),
        ]
    } else {
        vec![
            Line::from(format!("{} — {}", text.host_windows, text.not_measured)),
            Line::from(dashboard_phrase(
                language,
                DashboardPhrase::HostTelemetryAbsent,
            )),
            Line::from(""),
        ]
    };
    lines.splice(6..9, host_lines);
    lines.extend(text.pressure_help.iter().map(|line| Line::from(*line)));
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(text.pressure_page),
            )
            .wrap(Wrap { trim: true }),
        frame.area(),
    );
    frame.render_widget(
        Paragraph::new(format!(
            " [l] {}   [Esc] {}",
            text.switch_language, text.back
        )),
        Rect {
            x: frame.area().x,
            y: frame.area().bottom().saturating_sub(1),
            width: frame.area().width,
            height: 1,
        },
    );
}

pub(super) fn draw_details(
    frame: &mut Frame<'_>,
    observation: &Observation,
    _history: &DashHistory,
    language: Language,
) {
    let text = text(language);
    let cache = cache_snapshot(observation);
    let target = cache
        .target_bytes
        .map(|bytes| fmt_bytes(bytes, language))
        .unwrap_or_else(|| text.not_measured.to_string());
    let held = cache
        .held_bytes
        .map(|bytes| fmt_bytes(bytes, language))
        .unwrap_or_else(|| text.not_measured.to_string());
    let fill = cache
        .fill_pct()
        .map(|pct| format!("{pct}%"))
        .unwrap_or_else(|| text.not_measured.to_string());
    let lines = vec![
        Line::from(text.cache_origin),
        Line::from(format!(
            " {}: {}",
            text.state,
            status_label(language, &cache.state)
        )),
        Line::from(format!(" {}: {target}", text.target)),
        Line::from(format!(" {}: {held} ({fill})", text.occupancy)),
        Line::from(format!(
            " {}: {} · {}: {} · workspace: {}",
            text.physical,
            fmt_bytes(cache.physical_used(), language),
            text.logical,
            fmt_bytes(cache.held_bytes.unwrap_or(0), language),
            fmt_bytes(cache.workspace_bytes, language)
        )),
        Line::from(format!(
            " {}: {}   {}: {}",
            text.hits,
            cache
                .hits
                .map(|value| value.to_string())
                .unwrap_or_else(|| text.not_measured.to_string()),
            text.origin_reads,
            cache
                .fallback
                .map(|value| value.to_string())
                .unwrap_or_else(|| text.not_measured.to_string())
        )),
        Line::from(codec_line(&cache, language)),
        Line::from(""),
        Line::from(format!("{}:", text.capacity)),
        Line::from(format!(
            " {}: {} · {}: {}",
            text.physical,
            target,
            text.logical_capacity,
            cache
                .logical_target()
                .map(|bytes| fmt_bytes(bytes, language))
                .unwrap_or_else(|| text.not_measured.to_string())
        )),
        Line::from(format!(
            " {}: {} · {}: {} · {}",
            text.target,
            target,
            dashboard_phrase(language, DashboardPhrase::CompressedBytesLabel),
            fmt_bytes(cache.compressed_payload_bytes, language),
            text.no_spill,
        )),
        Line::from(format!(
            " {}: {}",
            text.counters,
            dashboard_phrase(language, DashboardPhrase::CountersSummary)
        )),
        Line::from(dashboard_phrase(language, DashboardPhrase::RestartPlanHint)),
        Line::from(format!(
            " {}: {}: {} │ {}: {} MB/s",
            dashboard_phrase(language, DashboardPhrase::RealTimeSpeed),
            dashboard_phrase(language, DashboardPhrase::ReadLabel),
            fmt_rate(observation.control_plane.swap_read_mbs),
            dashboard_phrase(language, DashboardPhrase::WriteLabel),
            fmt_rate(observation.control_plane.swap_write_mbs)
        )),
        Line::from(format!(
            " {}: {}",
            dashboard_phrase(language, DashboardPhrase::PageFaultsRate),
            super::super::render::format_page_fault_rates(
                observation.control_plane.pgfault_per_sec,
                observation.control_plane.pgmajfault_per_sec
            )
        )),
        Line::from(format!(
            " {}: {} ms",
            dashboard_phrase(language, DashboardPhrase::SampleAge),
            observation.sample_age_ms
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(text.details))
            .wrap(Wrap { trim: true }),
        frame.area(),
    );
    frame.render_widget(
        Paragraph::new(format!(
            " [l] {}   [Esc] {}",
            text.switch_language, text.back
        )),
        Rect {
            x: frame.area().x,
            y: frame.area().bottom().saturating_sub(1),
            width: frame.area().width,
            height: 1,
        },
    );
}

pub(super) fn draw_help(frame: &mut Frame<'_>, language: Language) {
    let text = text(language);
    let mut lines = vec![Line::from(text.glossary.to_uppercase()), Line::from("")];
    lines.extend(text.glossary_text.iter().map(|line| Line::from(*line)));
    lines.push(Line::from(""));
    lines.push(Line::from(text.thresholds));
    lines.push(Line::from(""));
    lines.extend(text.pressure_help.iter().map(|line| Line::from(*line)));
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(text.glossary))
            .wrap(Wrap { trim: true }),
        frame.area(),
    );
    frame.render_widget(
        Paragraph::new(format!(
            " [l] {}   [Esc] {}",
            text.switch_language, text.back
        )),
        Rect {
            x: frame.area().x,
            y: frame.area().bottom().saturating_sub(1),
            width: frame.area().width,
            height: 1,
        },
    );
}
