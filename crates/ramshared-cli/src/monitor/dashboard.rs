// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/dashboard.rs
//! Operator dashboard: a compact story-first main screen plus pressure,
//! details and glossary views.

mod cache_panel;
mod compression;
mod gpu_status;
mod header;
mod host_memory;
mod operation_banner;
mod views;

use cache_panel::{cache_snapshot, codec_line, draw_cache};
use compression::compression_line;
#[cfg(test)]
use header::degraded_cause;
use header::draw_header;
use host_memory::{host_memory_line, host_memory_snapshot};
use operation_banner::{active_action_summary, draw_operation_banner, has_active_operation};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
#[cfg(test)]
use ratatui::style::Color;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use serde::{Deserialize, Serialize};
use views::{draw_details, draw_help, draw_pressure};

use crate::zram_compression::{compression_ratio_of, resident_compression_from_status};

use super::classify::observation_sample_is_stale;
use super::i18n::{
    DashboardPhrase, Language, dashboard_phrase, pressure_class, pressure_label, reason_label,
    status_label, text,
};
use super::model::{MemoryScope, Observation};
use super::render::{
    DashHistory, DashboardMetadata, UtilizationStatus, classify_utilization, make_bar,
    make_tier_bar, tier_fill_pct,
};

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;
const TIB: u64 = 1024 * GIB;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(super) enum Screen {
    #[default]
    Main,
    Pressure,
    Details,
    Help,
}

pub(super) fn draw(
    frame: &mut Frame<'_>,
    observation: &Observation,
    history: &DashHistory,
    metadata: &DashboardMetadata,
    language: Language,
    screen: Screen,
) {
    match screen {
        Screen::Main => draw_main(frame, observation, history, metadata, language),
        Screen::Pressure => draw_pressure(frame, observation, history, language),
        Screen::Details => draw_details(frame, observation, history, language),
        Screen::Help => draw_help(frame, language),
    }
}

fn draw_main(
    frame: &mut Frame<'_>,
    observation: &Observation,
    history: &DashHistory,
    metadata: &DashboardMetadata,
    language: Language,
) {
    let text = text(language);
    let show_banner = has_active_operation(observation);
    let constraints = if show_banner {
        vec![
            Constraint::Length(5),
            Constraint::Length(4),
            Constraint::Length(9),
            Constraint::Min(7),
            Constraint::Length(2),
            Constraint::Length(1),
        ]
    } else {
        vec![
            Constraint::Length(5),
            Constraint::Length(9),
            Constraint::Min(7),
            Constraint::Length(2),
            Constraint::Length(1),
        ]
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(frame.area());
    draw_header(frame, rows[0], observation, metadata, language);
    let offset = if show_banner {
        draw_operation_banner(frame, rows[1], observation, language);
        1
    } else {
        0
    };

    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[1 + offset]);
    draw_memory(frame, top[0], observation, language);
    draw_gpu(frame, top[1], observation, language);
    draw_cache(frame, rows[2 + offset], observation, history, language);
    draw_action(frame, rows[3 + offset], observation, language);
    frame.render_widget(
        Paragraph::new(format!(
            " [p] {}   [d] {}   [l] {}   [?] {}   [q] {}",
            text.pressure, text.details, text.switch_language, text.glossary, text.quit
        )),
        rows[4 + offset],
    );
}

fn draw_memory(frame: &mut Frame<'_>, area: Rect, observation: &Observation, language: Language) {
    let text = text(language);
    let mem = &observation.mem;
    let bar_width = if area.width < 50 { 5 } else { 14 };
    let mut lines = Vec::new();
    if let Some((total, available)) = mem.ram_pair() {
        let used = total.saturating_sub(available);
        let pct = tier_fill_pct(used, total);
        let used_gib = language.number(format!("{:.1}", used as f64 * KIB as f64 / GIB as f64));
        let total_gib = language.number(format!("{:.1}", total as f64 * KIB as f64 / GIB as f64));
        lines.push(Line::from(format!(
            " {}   {} {}   {used_gib} / {total_gib} GiB",
            text.ram,
            make_tier_bar(used, total, bar_width),
            pct.map_or_else(|| "n/a".to_string(), |pct| format!("{pct}%")),
        )));
    } else {
        lines.push(Line::from(format!(" {}: {}", text.ram, text.unavailable)));
    }
    let wsl = matches!(
        observation.memory_scope,
        MemoryScope::Wsl | MemoryScope::Wsl2
    );
    if wsl {
        lines.push(Line::from(host_memory_line(observation, language)));
    }
    if let Some((total, free)) = mem.swap_pair() {
        let used = total.saturating_sub(free);
        let pct = tier_fill_pct(used, total);
        let used_gib = language.number(format!("{:.1}", used as f64 * KIB as f64 / GIB as f64));
        let total_gib = language.number(format!("{:.1}", total as f64 * KIB as f64 / GIB as f64));
        lines.push(Line::from(format!(
            " {}  {} {}   {used_gib} / {total_gib} GiB",
            text.swap,
            make_tier_bar(used, total, bar_width),
            pct.map_or_else(|| "n/a".to_string(), |pct| format!("{pct}%")),
        )));
    } else {
        lines.push(Line::from(format!(" {}: {}", text.swap, text.unavailable)));
    }
    let zram = tier_bytes(observation, "zram");
    let vram = tier_bytes(observation, "vram");
    let disk = tier_bytes(observation, "disk");
    if let (Some((z_used, z_size)), Some((d_used, d_size))) = (zram, disk) {
        let zram_full = format!(
            "{} {} / {}",
            text.zram,
            fmt_bytes(z_used, language),
            fmt_bytes(z_size, language)
        );
        let disk_full = format!(
            "{} {} / {}",
            text.ssd,
            fmt_bytes(d_used, language),
            fmt_bytes(d_size, language)
        );
        let max_w = usize::from(area.width.saturating_sub(2));
        let nbd_label = dashboard_phrase(language, DashboardPhrase::Tier2SwapLabel);
        let chosen = if let Some((v_used, v_size)) = vram {
            let wide = format!(
                " {zram_full} · {nbd_label} {} / {} · {disk_full}",
                fmt_bytes(v_used, language),
                fmt_bytes(v_size, language)
            );
            let compact = format!(
                " {} {} · {nbd_label} {} / {} · {} {}",
                text.zram,
                fmt_bytes(z_used, language),
                fmt_bytes(v_used, language),
                fmt_bytes(v_size, language),
                text.ssd,
                fmt_bytes(d_used, language)
            );
            if wide.chars().count() <= max_w {
                wide
            } else if compact.chars().count() <= max_w {
                compact
            } else {
                format!(" {zram_full}   {disk_full}")
            }
        } else {
            let combined = format!(" {zram_full}   {disk_full}");
            if combined.chars().count() <= max_w {
                combined
            } else {
                format!(" {zram_full}")
            }
        };
        lines.push(Line::from(chosen));
    }
    let pressure = if observation.control_plane.memory_psi_available {
        pressure_label(
            language,
            pressure_class(observation.control_plane.memory_psi_full_avg10),
        )
    } else {
        text.unavailable
    };
    let pressure_name = if wsl {
        pick(language, "Guest pressure", "Pressão guest")
    } else {
        text.pressure
    };
    lines.push(Line::from(format!(" {}: {} [p]", pressure_name, pressure)));
    lines.push(Line::from(format!(
        " {} — {} ({})",
        text.compression, text.zram, text.cpu
    )));
    if let Some(row) = compression_line(
        observation,
        language,
        usize::from(area.width.saturating_sub(2)),
    ) {
        lines.push(Line::from(row));
    } else {
        lines.push(Line::from(format!(" {}", text.not_measured)));
    }
    let scope = match observation.memory_scope {
        MemoryScope::LinuxHost => pick(language, "Linux host", "host Linux"),
        MemoryScope::Wsl => pick(language, "WSL guest", "guest WSL"),
        MemoryScope::Wsl2 => pick(language, "WSL2 guest", "guest WSL2"),
    };
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("{} · {scope}", text.memory)),
        ),
        area,
    );
}

fn draw_gpu(frame: &mut Frame<'_>, area: Rect, observation: &Observation, language: Language) {
    let text = text(language);
    let mut lines = Vec::new();
    if let Some(gpu) = observation.gpu.as_ref() {
        lines.push(Line::from(format!(
            " {} {} · {} {}",
            text.total,
            gpu.total_bytes
                .map(|bytes| fmt_bytes(bytes, language))
                .unwrap_or_else(|| text.unavailable.to_string()),
            text.usable,
            fmt_bytes(gpu.budget_bytes, language)
        )));
        lines.push(Line::from(format!(
            " {} {} {} / {}",
            text.used,
            make_tier_bar(gpu.used_bytes, gpu.budget_bytes, 14),
            fmt_bytes(gpu.used_bytes, language),
            fmt_bytes(gpu.budget_bytes, language)
        )));
        lines.push(Line::from(format!(
            " {} {}",
            text.free,
            fmt_bytes(gpu.free_bytes, language)
        )));
        lines.push(Line::from(format!(
            " {} {}",
            pick(language, "Safety floor", "Reserva de segurança"),
            fmt_bytes(gpu.vram_reserve_enforced_mib.saturating_mul(MIB), language)
        )));
    } else {
        lines.push(Line::from(format!(" {}: {}", text.gpu, text.unavailable)));
    }
    let inner_width = usize::from(area.width.saturating_sub(2));
    lines.push(Line::from(gpu_busy_line(
        observation,
        language,
        inner_width,
    )));
    if let Some(gpu) = observation.gpu.as_ref()
        && !gpu.is_fresh_at(observation.epoch_ms, ramshared_vram::GPU_BUDGET_MAX_AGE_MS)
    {
        lines.push(Line::from(format!(" {}", text.stale)));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(text.gpu))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn gpu_busy_line(observation: &Observation, language: Language, inner_width: usize) -> String {
    let text = text(language);
    let sample_stale = observation_sample_is_stale(observation);
    match classify_utilization(
        observation.value("gpu_utilization"),
        sample_stale,
        observation.epoch_ms,
    ) {
        UtilizationStatus::NotMeasured => format!(" {}", text.gpu_busy),
        UtilizationStatus::Stale => format!(
            " {}: {}",
            pick(language, "GPU busy", "GPU ocupada"),
            text.stale
        ),
        UtilizationStatus::Measured(util) => {
            let label = pick(language, "NVML busy", "NVML ocupada");
            let mut busiest = (pick(language, "kernels", "kernels"), util.gpu_pct);
            for (name, reading) in [
                (pick(language, "enc", "cod"), util.encoder_pct),
                (pick(language, "dec", "dec"), util.decoder_pct),
            ] {
                if let Some(pct) = reading
                    && pct > busiest.1
                {
                    busiest = (name, pct);
                }
            }
            let (engine, pct) = busiest;
            let bar_len = if inner_width >= 49 { 14 } else { 6 };
            let bar = make_bar(u64::from(pct), bar_len);
            format!(" {label} {engine} {bar} {pct}% · mem {}%", util.memory_pct)
        }
    }
}

/// The "now" line of the footer: what the cache is doing right now.
///
/// The origin's state is rendered from the observation's own state word, in
/// the console's language. A hardcoded word would put one language's
/// adjective inside the other language's sentence.
fn action_now_line(observation: &Observation, language: Language) -> String {
    let text = text(language);
    if let Some(summary) = active_action_summary(observation, language) {
        return format!("{} {summary}.", text.now.to_uppercase());
    }
    let cache = cache_snapshot(observation);
    if !cache.active() {
        return format!(
            "{} {} — {}.",
            text.now.to_uppercase(),
            status_label(language, &cache.state),
            cache
                .reason
                .as_deref()
                .map(|reason| reason_label(language, reason))
                .unwrap_or_else(|| text.not_measured.to_string())
        );
    }
    let hit = cache
        .hit_rate()
        .map(|pct| format!("{pct}%"))
        .unwrap_or_else(|| text.not_measured.to_string());
    let fallback = cache
        .fallback
        .map(|value| value.to_string())
        .unwrap_or_else(|| text.not_measured.to_string());
    format!(
        "{} {} ({hit}; {} {fallback}); {}; {} {}.",
        text.now.to_uppercase(),
        pick(
            language,
            "cache active, hit rate",
            "cache ativo, taxa de acertos"
        ),
        text.origin_reads,
        zram_ratio_line(observation, language),
        text.origin,
        status_label(language, observation.string("origin_state")),
    )
}

fn draw_action(frame: &mut Frame<'_>, area: Rect, observation: &Observation, language: Language) {
    let text = text(language);
    let now = action_now_line(observation, language);
    let disk_pct = tier_bytes(observation, "disk")
        .and_then(|(used, size)| tier_fill_pct(used, size))
        .map(|pct| format!("{pct}%"))
        .unwrap_or_else(|| text.not_measured.to_string());
    let action = format!(
        "{}  {}.",
        text.action,
        match language {
            Language::En => format!("watch SSD at {disk_pct} and the control reserve"),
            Language::PtBr => format!("acompanhar o SSD em {disk_pct} e a reserva do controle"),
        }
    );
    frame.render_widget(
        Paragraph::new(vec![Line::from(now), Line::from(action)]).wrap(Wrap { trim: true }),
        area,
    );
}

/// The control-plane authority's own line.
///
/// The cache's state says whether the cache serves; this says why the
/// authority admits it: the peer has not arrived, a lease is live, the lease
/// was lost, or the transport failed. An absent state is "not published",
/// never a fabricated one.
fn control_plane_line(observation: &Observation, language: Language) -> String {
    let text = text(language);
    let state = observation
        .value("control_plane_state")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let Some(state) = state else {
        return format!("{}: {}", text.control_plane, text.not_measured);
    };
    match observation
        .value("control_plane_lease_deadline_ms")
        .and_then(|value| value.as_u64())
    {
        Some(deadline) => {
            let remaining = deadline
                .saturating_sub(observation.epoch_ms)
                .saturating_div(1000);
            format!(
                "{}: {} · {} {} s",
                text.control_plane, state, text.lease_deadline, remaining
            )
        }
        None => format!("{}: {}", text.control_plane, state),
    }
}

fn tier_bytes(observation: &Observation, name: &str) -> Option<(u64, u64)> {
    let tier = observation.value("tiers")?.get(name)?;
    Some((
        tier.get("used_kib")?.as_u64()?.saturating_mul(KIB),
        tier.get("size_kib")?.as_u64()?.saturating_mul(KIB),
    ))
}

fn zram_ratio_line(observation: &Observation, language: Language) -> String {
    let ratio = observation
        .value("zram_compression")
        .and_then(resident_compression_from_status)
        .and_then(|(original, stored)| compression_ratio_of(original, stored));
    ratio.map_or_else(
        || text(language).not_measured.to_string(),
        |ratio| format!("zram {}:1", language.number(format!("{ratio:.2}"))),
    )
}

fn fmt_rate(rate: Option<f64>) -> String {
    rate.map_or_else(|| "n/a".to_string(), |rate| format!("{rate:.2}"))
}

pub(super) fn fmt_bytes(bytes: u64, language: Language) -> String {
    let value = if bytes >= TIB {
        format!("{:.1} TiB", bytes as f64 / TIB as f64)
    } else if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    };
    language.number(value)
}

fn sparkline(values: &std::collections::VecDeque<u64>, max: u64, width: usize) -> String {
    const SYMBOLS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if values.is_empty() {
        return "—".to_string();
    }
    let start = values.len().saturating_sub(width);
    values
        .iter()
        .skip(start)
        .map(|value| {
            let index = if max == 0 {
                0
            } else {
                ((u128::from((*value).min(max)) * (SYMBOLS.len() - 1) as u128) / u128::from(max))
                    as usize
            };
            SYMBOLS[index]
        })
        .collect()
}

fn state_lane(history: &DashHistory, width: usize) -> String {
    if history.cache_active.is_empty() {
        return "—".to_string();
    }
    let start = history.cache_active.len().saturating_sub(width);
    history
        .cache_active
        .iter()
        .skip(start)
        .map(|active| if *active { '█' } else { '░' })
        .collect()
}

fn pick(language: Language, en: &'static str, pt: &'static str) -> &'static str {
    match language {
        Language::En => en,
        Language::PtBr => pt,
    }
}

#[cfg(test)]
mod tests;
