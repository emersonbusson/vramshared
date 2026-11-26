// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/dashboard/header.rs
//! Main dashboard health, swap, cache and telemetry status.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

use super::super::classify::observation_sample_is_stale;
use super::super::i18n::{
    Language, pressure_class, pressure_label, reason_label, status_label, text,
};
use super::super::model::{MemoryScope, Observation};
use super::super::render::{DashboardMetadata, format_uptime_seconds};
use super::gpu_status::{GpuPanelDelay, gpu_panel_delay};
use super::{GIB, KIB, cache_snapshot, pick};

pub(super) fn draw_header(
    frame: &mut Frame<'_>,
    area: Rect,
    observation: &Observation,
    metadata: &DashboardMetadata,
    language: Language,
) {
    let text = text(language);
    let cache = cache_snapshot(observation);
    let overall_raw = observation.string("overall_state");
    let overall = if overall_raw.is_empty() {
        "UNKNOWN".to_string()
    } else if overall_raw == "GUARDED" {
        "DEGRADED".to_string()
    } else {
        overall_raw.to_string()
    };
    let overall_label = status_label(language, &overall);
    let gpu_delay = gpu_panel_delay(observation);
    let stale_sample = observation_sample_is_stale(observation);
    let cause = match gpu_delay {
        Some(GpuPanelDelay::Telemetry) => Some(
            pick(
                language,
                "GPU telemetry delayed",
                "telemetria da GPU atrasada",
            )
            .to_string(),
        ),
        Some(GpuPanelDelay::CacheWorker) => Some(
            pick(
                language,
                "GPU cache unavailable",
                "cache da GPU indisponível",
            )
            .to_string(),
        ),
        None if stale_sample => Some(text.stale.to_string()),
        None => degraded_cause(observation, language),
    };
    let cause = cause.map(|cause| format!(" — {cause}")).unwrap_or_default();
    let swap = status_label(language, observation.string("protection_state"));
    let origin = status_label(language, observation.string("origin_state"));
    let cache_label = match cache.state.as_str() {
        "ACTIVE" => {
            let hit = cache
                .hit_rate()
                .map(|pct| format!("{pct}%"))
                .unwrap_or_else(|| text.not_measured.to_string());
            format!("{} · {}", text.active, hit)
        }
        "OFF" => {
            let reason = cache
                .reason
                .as_deref()
                .map(|reason| reason_label(language, reason))
                .map(|reason| format!(" — {reason}"))
                .unwrap_or_default();
            format!("{}{reason}", text.off)
        }
        state => status_label(language, state),
    };
    let uptime = observation
        .control_plane
        .uptime_seconds
        .map(format_uptime_seconds)
        .unwrap_or_else(|| text.not_measured.to_string());
    let pressure = if observation.control_plane.memory_psi_available {
        let class = pressure_class(observation.control_plane.memory_psi_full_avg10);
        pressure_label(language, class).to_string()
    } else {
        text.unavailable.to_string()
    };
    let title = if metadata.version_line.trim().is_empty() {
        format!("RamShared {}", env!("CARGO_PKG_VERSION"))
    } else {
        metadata.version_line.clone()
    };
    let pressure_name = match observation.memory_scope {
        MemoryScope::LinuxHost => text.pressure,
        MemoryScope::Wsl | MemoryScope::Wsl2 => pick(language, "Guest pressure", "Pressão guest"),
    };
    let right_header = format!("{} {uptime}", text.host_up);
    let inner_width = usize::from(area.width.saturating_sub(2));
    let header_gap = inner_width
        .saturating_sub(title.chars().count() + right_header.chars().count())
        .max(2);
    let lines = vec![
        Line::from(format!("{title}{}{right_header}", " ".repeat(header_gap))),
        Line::from(format!(
            "{}: {overall_label}{cause}   {}: {swap}   {}: {cache_label}",
            text.overall,
            pick(language, "RamShared swap", "Swap RamShared"),
            text.cache_gpu
        )),
        Line::from(format!(
            "{}: {origin}   {}: {pressure} [p]",
            text.origin, pressure_name
        )),
    ];
    let color = match overall.as_str() {
        "HEALTHY" if !stale_sample => Color::Green,
        "HEALTHY" | "DEGRADED" | "UNKNOWN" => Color::Yellow,
        "BLOCKED" if gpu_delay.is_some() && !stale_sample => Color::Yellow,
        _ => Color::Red,
    };
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().fg(color))
            .block(Block::default().borders(Borders::ALL).title("RamShared")),
        area,
    );
}

pub(super) fn degraded_cause(observation: &Observation, language: Language) -> Option<String> {
    let overall = observation.string("overall_state");
    if !overall.eq_ignore_ascii_case("GUARDED") && !overall.eq_ignore_ascii_case("DEGRADED") {
        return None;
    }
    let text = text(language);
    let full = observation.control_plane.memory_psi_full_avg10;
    let mem = &observation.mem;
    if let (Some(total), Some(available)) = (
        mem.total_kib.and_then(|value| value.checked_mul(KIB)),
        mem.available_kib.and_then(|value| value.checked_mul(KIB)),
    ) {
        let reserve = crate::supervisor::memory_policy::control_reserve_bytes(total);
        let recovery =
            reserve.saturating_add(crate::supervisor::memory_policy::RECOVERY_MARGIN_BYTES);
        if available < reserve {
            return Some(format!(
                "{} ({}/{} GiB)",
                text.reserve,
                language.number(format!("{:.2}", available as f64 / GIB as f64)),
                language.number(format!("{:.2}", reserve as f64 / GIB as f64))
            ));
        }
        if available < recovery {
            return Some(format!(
                "{} ({}/{} GiB)",
                text.recovery,
                language.number(format!("{:.2}", available as f64 / GIB as f64)),
                language.number(format!("{:.2}", recovery as f64 / GIB as f64))
            ));
        }
    }
    if observation.control_plane.memory_psi_available
        && full >= crate::supervisor::memory_policy::GUARDED_PSI_FULL_AVG10_PERCENT
    {
        return Some(format!("PSI full {:.2}%", full));
    }
    // A guarded control plane with no measured pressure is not evidence of a
    // reserve shortage. This used to fall back to "control reserve" for every
    // GUARDED verdict, which turned a fail-closed boot (no daemon, or the
    // supervisor's recovery window) into a false memory-pressure claim.
    None
}
