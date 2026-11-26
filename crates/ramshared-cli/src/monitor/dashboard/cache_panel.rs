// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/dashboard/cache_panel.rs
//! Cache snapshot extraction, codec line formatting, and cache panel rendering.

use ramshared_vram::{CodecState, WorkerCacheTelemetry};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use super::super::i18n::{
    DashboardPhrase, Language, dashboard_phrase, reason_label, status_label, text,
};
use super::super::model::Observation;
use super::super::observations::cache_telemetry_record;
use super::super::render::{DashHistory, make_tier_bar};
use super::{KIB, fmt_bytes, sparkline, state_lane};

#[derive(Clone, Debug, Default)]
pub(super) struct CacheSnapshot {
    pub(super) state: String,
    pub(super) reason: Option<String>,
    pub(super) target_bytes: Option<u64>,
    /// The logical origin capacity the cache serves. This is the `capacity`
    /// document's `logical_capacity_kib`, not the codec-expanded cache target
    /// derived by [`Self::logical_target`].
    pub(super) logical_capacity_bytes: Option<u64>,
    pub(super) held_bytes: Option<u64>,
    pub(super) compressed_payload_bytes: u64,
    pub(super) raw_payload_bytes: u64,
    pub(super) raw_bypass_bytes: u64,
    pub(super) physical_slab_bytes: u64,
    pub(super) workspace_bytes: u64,
    pub(super) hits: Option<u64>,
    pub(super) fallback: Option<u64>,
    pub(super) batched_writes: Option<u64>,
    pub(super) served_bytes: Option<u64>,
    pub(super) headroom_bytes: Option<u64>,
    pub(super) codec: Option<WorkerCacheTelemetry>,
    pub(super) effective_ratio: Option<f64>,
    pub(super) payload_compression_ratio: Option<f64>,
    pub(super) codec_active: bool,
}

impl CacheSnapshot {
    pub(super) fn active(&self) -> bool {
        self.state == "ACTIVE"
    }

    pub(super) fn fill_pct(&self) -> Option<u64> {
        let target = self.target_bytes?;
        let held = self.held_bytes?;
        if target == 0 {
            return None;
        }
        let stored = self.payload_stored_bytes();
        let basis = if self.codec_active && stored > 0 {
            stored
        } else {
            held
        };
        Some(basis.saturating_mul(100) / target)
    }

    pub(super) fn payload_stored_bytes(&self) -> u64 {
        self.compressed_payload_bytes
            .saturating_add(self.raw_payload_bytes)
            .saturating_add(self.raw_bypass_bytes)
    }

    pub(super) fn physical_used(&self) -> u64 {
        let payload = self.payload_stored_bytes();
        if self.codec_active {
            payload
        } else {
            self.physical_slab_bytes
                .saturating_add(self.raw_bypass_bytes)
        }
    }

    pub(super) fn hit_rate(&self) -> Option<u64> {
        let hits = self.hits?;
        let fallback = self.fallback?;
        let total = hits.saturating_add(fallback);
        // `then_some` evaluates its argument eagerly, so the division ran
        // before the `total > 0` guard and the dashboard panicked with
        // "attempt to divide by zero" on an empty cache. The guard comes
        // first.
        if total == 0 {
            return None;
        }
        Some(hits.saturating_mul(100) / total)
    }

    pub(super) fn logical_target(&self) -> Option<u64> {
        let target = self.target_bytes?;
        if self.codec_active {
            if let Some(ratio) = self.payload_compression_ratio.filter(|r| *r > 0.0) {
                return Some((target as f64 * ratio) as u64);
            }
            if let Some(ratio) = self.effective_ratio.filter(|r| *r > 0.0) {
                return Some((target as f64 * ratio) as u64);
            }
        }
        Some(target)
    }
}

pub(super) fn cache_snapshot(observation: &Observation) -> CacheSnapshot {
    let reason = observation
        .value("cache_unavailable_reason")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string);
    let mut state = observation.string("cache_state").to_string();
    if state.is_empty() || state.eq_ignore_ascii_case("unknown") {
        state = if reason.is_some() {
            "OFF".to_string()
        } else {
            "UNKNOWN".to_string()
        };
    }
    let cache = cache_telemetry_record(observation.value("gpu_cache"));
    let target_bytes = observation
        .capacity_u64("cache_target_kib")
        .map(|kib| kib.saturating_mul(KIB));
    let logical_capacity_bytes = observation
        .capacity_u64("logical_capacity_kib")
        .map(|kib| kib.saturating_mul(KIB));
    let held_bytes = cache
        .as_ref()
        .map(|cache| cache.logical_cached_bytes)
        .or_else(|| {
            observation
                .value("vram_cached_kib")
                .and_then(|value| value.as_u64())
                .map(|kib| kib.saturating_mul(KIB))
        });
    let compressed_payload_bytes = cache
        .as_ref()
        .map_or(0, |cache| cache.compressed_payload_bytes);
    let raw_payload_bytes = cache.as_ref().map_or(0, |cache| cache.raw_payload_bytes);
    let raw_bypass_bytes = cache.as_ref().map_or(0, |cache| cache.raw_bypass_bytes);
    let physical_slab_bytes = cache
        .as_ref()
        .map_or(0, |cache| cache.physical_cache_slab_bytes);
    let workspace_bytes = cache
        .as_ref()
        .map_or(0, |cache| cache.codec_workspace_bytes);
    let hits = observation
        .value("cache_hits")
        .and_then(|value| value.as_u64());
    let fallback = observation
        .value("cache_fallback_reads")
        .and_then(|value| value.as_u64());
    let batched_writes = observation
        .value("origin_cache_io")
        .and_then(|value| value.get("batched_writes"))
        .and_then(|value| value.as_u64());
    let served_bytes = observation
        .value("origin_cache_io")
        .and_then(|value| value.get("cache_read_bytes"))
        .and_then(|value| value.as_u64());
    let headroom_bytes = observation
        .capacity_u64("gpu_headroom_kib")
        .map(|kib| kib.saturating_mul(KIB));
    let codec_active = cache
        .as_ref()
        .is_some_and(|cache| cache.codec.state == CodecState::Ready);
    let payload_stored = compressed_payload_bytes
        .saturating_add(raw_payload_bytes)
        .saturating_add(raw_bypass_bytes);
    let payload_compression_ratio = if codec_active {
        let logical = held_bytes.unwrap_or(0) as f64;
        let stored = payload_stored as f64;
        (logical > 0.0 && stored > 0.0).then(|| logical / stored)
    } else {
        None
    };
    let physical_pool = physical_slab_bytes.saturating_add(raw_bypass_bytes);
    let effective_ratio = if codec_active {
        if let Some(ratio) = payload_compression_ratio {
            Some(ratio)
        } else {
            let logical = held_bytes.unwrap_or(0) as f64;
            let physical = physical_pool.saturating_add(workspace_bytes) as f64;
            (logical > 0.0 && physical > 0.0).then(|| logical / physical)
        }
    } else {
        None
    };
    CacheSnapshot {
        state,
        reason,
        target_bytes,
        logical_capacity_bytes,
        held_bytes,
        compressed_payload_bytes,
        raw_payload_bytes,
        raw_bypass_bytes,
        physical_slab_bytes,
        workspace_bytes,
        hits,
        fallback,
        batched_writes,
        served_bytes,
        headroom_bytes,
        codec: cache,
        effective_ratio,
        payload_compression_ratio,
        codec_active,
    }
}

pub(super) fn draw_cache(
    frame: &mut Frame<'_>,
    area: Rect,
    observation: &Observation,
    history: &DashHistory,
    language: Language,
) {
    let text = text(language);
    let cache = cache_snapshot(observation);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(text.cache_origin);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(62),
            Constraint::Length(2),
            Constraint::Min(0),
        ])
        .split(area.inner(ratatui::layout::Margin::new(1, 1)));
    let hit_label = format!(" {} [0–100%] ", text.hits);
    let origin_label = format!(
        " {} [{}] ",
        text.origin_reads,
        dashboard_phrase(language, DashboardPhrase::ZeroToPeak)
    );
    let graph_width = usize::from(columns[0].width)
        .saturating_sub(hit_label.chars().count().max(origin_label.chars().count()));
    let hit_spark = sparkline(&history.cache_hit_rate, 100, graph_width);
    let fallback_spark = sparkline(
        &history.cache_origin_reads,
        history
            .cache_origin_reads
            .iter()
            .copied()
            .max()
            .unwrap_or(1)
            .max(1),
        graph_width,
    );
    let hit = match (cache.hit_rate(), cache.hits) {
        (Some(pct), Some(count)) => format!("{pct}% ({count})"),
        (Some(pct), None) => format!("{pct}%"),
        _ => text.not_measured.to_string(),
    };
    let fallback = cache
        .fallback
        .map(|value| value.to_string())
        .unwrap_or_else(|| text.not_measured.to_string());
    let served = cache
        .served_bytes
        .map(|bytes| fmt_bytes(bytes, language))
        .unwrap_or_else(|| text.not_measured.to_string());
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
    let mut right = vec![
        Line::from(text.now.to_uppercase()),
        Line::from(format!(" {}       {hit}", text.hits)),
        Line::from(format!(" {}  {fallback}", text.origin_reads)),
        Line::from(format!(" {}    {served}", text.served)),
    ];
    if let Some(writes) = cache.batched_writes {
        right.push(Line::from(format!(
            " {}  {writes}",
            dashboard_phrase(language, DashboardPhrase::BatchWrites)
        )));
    }
    right.extend([
        Line::from(text.capacity.to_uppercase()),
        Line::from(format!(" {}  {target}", text.target)),
        Line::from(format!(
            " {}  {}",
            text.logical_capacity,
            cache
                .logical_capacity_bytes
                .or_else(|| cache.logical_target())
                .map(|bytes| fmt_bytes(bytes, language))
                .unwrap_or_else(|| text.not_measured.to_string())
        )),
        Line::from(format!(
            " {}       {}",
            text.free_capacity,
            cache
                .headroom_bytes
                .map(|bytes| fmt_bytes(bytes, language))
                .unwrap_or_else(|| text.not_measured.to_string())
        )),
    ]);
    let physical = cache.codec.as_ref().map(|_| cache.physical_used());
    let physical_bar = match (physical, cache.target_bytes) {
        (Some(used), Some(target)) if target > 0 => make_tier_bar(used, target, 18),
        _ => "—".into(),
    };
    let logical_denom = cache.logical_target().or_else(|| {
        (cache.codec_active && cache.held_bytes == Some(0))
            .then_some(cache.target_bytes)
            .flatten()
    });
    let logical_bar = match (cache.held_bytes, logical_denom) {
        (Some(used), Some(target)) if target > 0 => make_tier_bar(used, target, 18),
        _ => "—".into(),
    };
    let top_summary = format_top_summary(&cache, &target, &held, &fill, language);
    let slab_note = if cache.physical_slab_bytes > 0 {
        format!(
            " ({} {})",
            dashboard_phrase(language, DashboardPhrase::SlabLabel),
            fmt_bytes(cache.physical_slab_bytes, language)
        )
    } else {
        String::new()
    };
    let logical_limit_text = if let Some(ratio_target) = cache.logical_target() {
        fmt_bytes(ratio_target, language)
    } else if cache.codec_active && cache.held_bytes == Some(0) {
        dashboard_phrase(language, DashboardPhrase::CodecAwaitingTier2).to_string()
    } else {
        text.not_measured.to_string()
    };
    let samples_label = if cache.active() {
        dashboard_phrase(language, DashboardPhrase::CumulativeSamples)
    } else if history.cache_hit_rate.is_empty() && history.cache_origin_reads.is_empty() {
        dashboard_phrase(language, DashboardPhrase::CheckCacheOrigin)
    } else {
        super::pick(language, "Last recorded samples", "Últimas amostras")
    };
    let left = vec![
        Line::from(top_summary),
        Line::from(format!("{hit_label}{hit_spark}")),
        Line::from(format!("{origin_label}{fallback_spark}")),
        Line::from(samples_label),
        Line::from(format!(
            " {} {} {} / {}{}",
            text.physical,
            physical_bar,
            physical
                .map(|bytes| fmt_bytes(bytes, language))
                .unwrap_or_else(|| text.not_measured.to_string()),
            target,
            slab_note
        )),
        Line::from(format!(
            " {} {} {} / {logical_limit_text}",
            text.logical, logical_bar, held,
        )),
        Line::from(codec_line(&cache, language)),
        Line::from(format!(" {} {}", text.state, state_lane(history, 40))),
        Line::from(dashboard_phrase(language, DashboardPhrase::CacheLegend)),
        Line::from(dashboard_phrase(
            language,
            DashboardPhrase::CacheReadThroughNote,
        )),
    ];
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(left).wrap(Wrap { trim: true }), columns[0]);
    frame.render_widget(Paragraph::new(right).wrap(Wrap { trim: true }), columns[2]);
}

fn format_top_summary(
    cache: &CacheSnapshot,
    target: &str,
    held: &str,
    fill: &str,
    language: Language,
) -> String {
    let text = text(language);
    if !cache.active() {
        let reason = cache
            .reason
            .as_deref()
            .map(|reason| reason_label(language, reason))
            .unwrap_or_else(|| text.not_measured.to_string());
        return format!("{} · {reason}", status_label(language, &cache.state));
    }
    if let Some(ratio) = cache.payload_compression_ratio {
        let ratio_str = format!("{}:1", language.number(format!("{ratio:.2}")));
        let stored_str = fmt_bytes(cache.payload_stored_bytes(), language);
        return format!(
            " {} · {} {target} · {} {held} -> {stored_str} VRAM ({ratio_str}, {fill})",
            text.active, text.target, text.occupancy,
        );
    }
    if cache.codec_active && cache.held_bytes == Some(0) {
        return format!(
            " {} · {} {target} · {}",
            text.active,
            text.target,
            dashboard_phrase(language, DashboardPhrase::CodecAwaitingTier2),
        );
    }
    format!(
        " {} · {} {target} · {} {held} ({fill})",
        text.active, text.target, text.occupancy,
    )
}

pub(super) fn codec_line(cache: &CacheSnapshot, language: Language) -> String {
    let text = text(language);
    let Some(codec) = cache.codec.as_ref() else {
        return format!(" {}: {}", text.codec, text.not_measured);
    };
    match codec.codec.state {
        CodecState::RawOnly => format!(
            " {}: RawOnly — {}; {}",
            text.codec, text.no_codec, text.ratio_unavailable
        ),
        CodecState::Ready => {
            let codec_name = dashboard_phrase(language, DashboardPhrase::GpuCodecName);
            let ws = fmt_bytes(cache.workspace_bytes, language);
            if let Some(ratio_val) = cache.effective_ratio {
                let ratio = format!("{}:1", language.number(format!("{ratio_val:.2}")));
                let held_str = cache
                    .held_bytes
                    .filter(|b| *b > 0)
                    .map(|b| format!("{} -> ", fmt_bytes(b, language)))
                    .unwrap_or_default();
                format!(
                    " {}: {codec_name} · {} {held_str}{} · {} {ratio} · {} {ws}",
                    text.codec,
                    text.physical,
                    fmt_bytes(cache.physical_used(), language),
                    dashboard_phrase(language, DashboardPhrase::EffectiveRatio),
                    dashboard_phrase(language, DashboardPhrase::Workspace),
                )
            } else {
                format!(
                    " {}: {codec_name} · {} · {} {} · {} {ws}",
                    text.codec,
                    dashboard_phrase(language, DashboardPhrase::CodecAwaitingTier2),
                    dashboard_phrase(language, DashboardPhrase::SlabLabel),
                    fmt_bytes(cache.physical_slab_bytes, language),
                    dashboard_phrase(language, DashboardPhrase::Workspace),
                )
            }
        }
        state => format!(
            " {}: {}",
            text.codec,
            status_label(language, state.as_str())
        ),
    }
}
