// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/dashboard/operation_banner.rs
//! Dynamic live progress banner for drains, startup/teardown, installs, and benchmarks.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use super::super::i18n::{
    DashboardPhrase, Language, dashboard_phrase, operation_kind_label, operation_phase_label,
    reason_label,
};
use super::super::model::Observation;
use super::super::render::make_bar;
use super::fmt_bytes;
use crate::operation_progress::{OperationProgressV1, unix_epoch_ms};

const BANNER_BAR_WIDTH: u64 = 16;
const KIB: u64 = 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct OperationBannerContent {
    pub(super) title: String,
    pub(super) primary_line: String,
    pub(super) secondary_line: String,
    pub(super) border_color: Color,
}

fn demote_drain_state(observation: &Observation) -> Option<(Option<&str>, Option<u64>)> {
    let demote = observation.value("demote")?;
    let active = demote.get("in_progress").and_then(|v| v.as_bool())?;
    if !active {
        return None;
    }
    let reason = demote.get("last_reason").and_then(|v| v.as_str());
    let cached_bytes = observation
        .capacity_u64("vram_cached_kib")
        .map(|kib| kib.saturating_mul(KIB));
    Some((reason, cached_bytes))
}

fn cache_starting_active(observation: &Observation) -> bool {
    let state = observation
        .value("cache_state")
        .and_then(|value| value.as_str());
    matches!(state, Some("Starting" | "Draining"))
}

pub(super) fn has_active_operation(observation: &Observation) -> bool {
    observation.operation_progress.is_some()
        || demote_drain_state(observation).is_some()
        || cache_starting_active(observation)
}

fn build_cli_operation_banner(
    progress: &OperationProgressV1,
    language: Language,
    now_unix_ms: u64,
) -> OperationBannerContent {
    let title = if let Some(upgrade) = progress.upgrade.as_ref() {
        let label = match language {
            Language::En => "UPDATE",
            Language::PtBr => "ATUALIZAÇÃO",
        };
        format!(" {label} · {} → {} ", upgrade.previous, upgrade.candidate)
    } else {
        format!(
            " {} · {} ",
            dashboard_phrase(language, DashboardPhrase::OperationBannerTitle),
            operation_kind_label(language, progress.kind)
        )
    };
    let pct = progress.percent();
    let bar = make_bar(u64::from(pct), BANNER_BAR_WIDTH);
    let step_label = dashboard_phrase(language, DashboardPhrase::StepLabel);
    let phase_label = operation_phase_label(language, &progress.phase);
    let primary_line = if let Some(target) = progress.target.as_deref() {
        format!(
            " {bar} {pct:>3}% · {phase_label} ({target}) · {step_label} {}/{}",
            progress.step_index, progress.total_steps
        )
    } else {
        format!(
            " {bar} {pct:>3}% · {phase_label} · {step_label} {}/{}",
            progress.step_index, progress.total_steps
        )
    };

    let mut details = Vec::new();
    if progress.total_bytes > 0 {
        details.push(format!(
            "{} / {}",
            fmt_bytes(progress.completed_bytes, language),
            fmt_bytes(progress.total_bytes, language)
        ));
    }
    if let Some(rate) = progress.throughput_mib_per_s {
        details.push(format!("{rate:.1} MiB/s"));
    }
    details.push(format!("{}s", progress.elapsed_secs(now_unix_ms)));
    let secondary_line = format!(" {}", details.join(" · "));

    OperationBannerContent {
        title,
        primary_line,
        secondary_line,
        border_color: Color::Cyan,
    }
}

pub(super) fn build_operation_banner(
    observation: &Observation,
    language: Language,
) -> Option<OperationBannerContent> {
    if let Some(progress) = observation.operation_progress.as_ref() {
        let now_ms = observation.epoch_ms.max(unix_epoch_ms());
        return Some(build_cli_operation_banner(progress, language, now_ms));
    }
    if let Some((reason, remaining_bytes)) = demote_drain_state(observation) {
        let title = format!(
            " {} ",
            dashboard_phrase(language, DashboardPhrase::OperationBannerTitle)
        );
        let headline = dashboard_phrase(language, DashboardPhrase::DemoteInProgress);
        let reason_txt = reason
            .map(|r| reason_label(language, r))
            .unwrap_or_default();
        let primary_line = if reason_txt.is_empty() {
            format!(" {headline}")
        } else {
            format!(" {headline} · {reason_txt}")
        };
        let remaining_label = dashboard_phrase(language, DashboardPhrase::RemainingLabel);
        let secondary_line = remaining_bytes
            .map(|bytes| format!(" {remaining_label}: {}", fmt_bytes(bytes, language)))
            .unwrap_or_default();
        return Some(OperationBannerContent {
            title,
            primary_line,
            secondary_line,
            border_color: Color::Yellow,
        });
    }
    if cache_starting_active(observation) {
        let title = format!(
            " {} ",
            dashboard_phrase(language, DashboardPhrase::OperationBannerTitle)
        );
        let primary_line = format!(
            " {}",
            dashboard_phrase(language, DashboardPhrase::CacheStarting)
        );
        return Some(OperationBannerContent {
            title,
            primary_line,
            secondary_line: String::new(),
            border_color: Color::Cyan,
        });
    }
    None
}

pub(super) fn active_action_summary(
    observation: &Observation,
    language: Language,
) -> Option<String> {
    let banner = build_operation_banner(observation, language)?;
    let trimmed = banner.primary_line.trim();
    if banner.secondary_line.trim().is_empty() {
        Some(trimmed.to_string())
    } else {
        Some(format!("{trimmed} ({})", banner.secondary_line.trim()))
    }
}

pub(super) fn draw_operation_banner(
    frame: &mut Frame<'_>,
    area: Rect,
    observation: &Observation,
    language: Language,
) {
    let Some(banner) = build_operation_banner(observation, language) else {
        return;
    };
    let lines = if banner.secondary_line.is_empty() {
        vec![Line::from(banner.primary_line)]
    } else {
        vec![
            Line::from(banner.primary_line),
            Line::from(banner.secondary_line),
        ]
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(banner.border_color))
                    .title(banner.title)
                    .title_style(
                        Style::default()
                            .fg(banner.border_color)
                            .add_modifier(Modifier::BOLD),
                    ),
            )
            .wrap(Wrap { trim: true }),
        area,
    );
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::collections::BTreeMap;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use serde_json::json;

    use super::*;
    use crate::monitor::dashboard::Screen;
    use crate::monitor::dashboard::draw;
    use crate::monitor::model::{
        CgroupMemoryObservation, ControlPlaneObservation, HyperVBalloonObservation,
        MemoryObservation, MemoryScope, ProcessMemoryTotals, TierDeviceNames,
    };
    use crate::monitor::render::{DashHistory, DashboardMetadata};
    use crate::operation_progress::OperationKind;

    fn sample_observation() -> Observation {
        let mut status = BTreeMap::new();
        status.insert("cache_state".into(), json!("ACTIVE"));
        Observation {
            status,
            epoch_ms: 1_700_000_010_000,
            sample_age_ms: 0,
            memory_scope: MemoryScope::Wsl2,
            mem: MemoryObservation::default(),
            cgroup_memory: CgroupMemoryObservation::default(),
            hyperv_balloon: HyperVBalloonObservation::default(),
            process_totals: ProcessMemoryTotals::default(),
            control_plane: ControlPlaneObservation::default(),
            gpu: None,
            tier_devices: TierDeviceNames::default(),
            operation_progress: None,
            top_processes: Vec::new(),
            errors: Vec::new(),
        }
    }

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn dashboard_renders_live_operation_progress_and_demote_drain_banner() {
        let mut obs = sample_observation();
        assert!(!has_active_operation(&obs));

        obs.operation_progress = Some(OperationProgressV1 {
            schema_version: 1,
            operation_id: "1-2-3".into(),
            kind: OperationKind::Teardown,
            phase: "swapoff".into(),
            step_index: 2,
            total_steps: 4,
            target: Some("/dev/nbd0".into()),
            completed_bytes: 512 * 1024 * 1024,
            total_bytes: 1024 * 1024 * 1024,
            throughput_mib_per_s: Some(320.5),
            started_at_unix_ms: 1_700_000_005_000,
            updated_at_unix_ms: 1_700_000_010_000,
            ttl_ms: 15_000,
            owner_pid: 99,
            owner_start_ticks: 111,
            upgrade: None,
        });
        assert!(has_active_operation(&obs));

        let history = DashHistory::default();
        let metadata = DashboardMetadata {
            version_line: "v0.15.0-test".into(),
            running_line: "test-host".into(),
            install_lines: Vec::new(),
            restart_executable: None,
            runtime_notice: None,
            update_notice: None,
        };
        let mut terminal = Terminal::new(TestBackend::new(132, 36)).expect("terminal");
        terminal
            .draw(|f| draw(f, &obs, &history, &metadata, Language::PtBr, Screen::Main))
            .expect("draw pt-br");
        let text_pt = buffer_text(&terminal);
        assert!(text_pt.contains(dashboard_phrase(
            Language::PtBr,
            DashboardPhrase::OperationBannerTitle
        )));
        assert!(text_pt.contains("50%"));
        assert!(text_pt.contains("/dev/nbd0"));
        assert!(text_pt.contains("320.5 MiB/s"));
        assert!(text_pt.contains(&format!(
            "{} 2/4",
            dashboard_phrase(Language::PtBr, DashboardPhrase::StepLabel)
        )));

        let progress = obs.operation_progress.as_mut().expect("live progress");
        progress.upgrade = crate::operation_progress::UpgradeDrainProgress::new(
            "v1.0.3-rc.13".into(),
            "v1.0.3-rc.14".into(),
        );
        let update_banner = build_operation_banner(&obs, Language::PtBr).expect("update banner");
        assert!(
            update_banner
                .title
                .contains("ATUALIZAÇÃO · v1.0.3-rc.13 → v1.0.3-rc.14")
        );
        assert!(update_banner.primary_line.contains("50%"));
        assert!(update_banner.secondary_line.contains("320.5 MiB/s"));
        assert_eq!(update_banner.border_color, Color::Cyan);

        // Clear CLI operation, test daemon VRAM cache demote drain banner.
        obs.operation_progress = None;
        obs.status.insert(
            "demote".into(),
            json!({
                "total": 4u64,
                "last_reason": "watermark_high",
                "in_progress": true
            }),
        );
        obs.status.insert(
            "capacity".into(),
            json!({
                "vram_cached_kib": 262_144u64
            }),
        );
        assert!(has_active_operation(&obs));

        terminal
            .draw(|f| draw(f, &obs, &history, &metadata, Language::En, Screen::Main))
            .expect("draw en demote");
        let text_en = buffer_text(&terminal);
        assert!(text_en.contains("ACTIVE OPERATION"));
        assert!(text_en.contains("Draining VRAM cache to SSD"));
        assert!(text_en.contains("remaining"));

        // Verify compressed GPU cache panel & 3-tier VRAM/NBD row on main screen.
        obs.status.insert("cache_state".into(), json!("ACTIVE"));
        obs.status.insert("cache_hits".into(), json!(176u64));
        obs.status
            .insert("cache_fallback_reads".into(), json!(48u64));
        obs.status.insert(
            "origin_cache_io".into(),
            json!({
                "batched_writes": 118_524u64,
                "cache_read_bytes": 720_896u64
            }),
        );
        obs.status.insert(
            "tiers".into(),
            json!({
                "zram": {"used_kib": 4_194_300u64, "size_kib": 4_194_300u64},
                "vram": {"used_kib": 143_360u64, "size_kib": 4_194_300u64},
                "disk": {"used_kib": 698_856u64, "size_kib": 13_631_484u64}
            }),
        );
        obs.status.insert(
            "capacity".into(),
            json!({
                "cache_target_kib": 33_792u64,
                "logical_capacity_kib": 4_194_304u64,
                "vram_cached_kib": 33_672u64,
                "gpu_headroom_kib": 2_786_304u64
            }),
        );
        obs.status.insert(
            "gpu_cache".into(),
            json!({
                "schema_version": 1u8,
                "sampled_at_unix_ms": 1_700_000_010_000u64,
                "codec": {"capability": "available", "state": "ready", "refusal_reason": null},
                "logical_cached_bytes": 44_257_280u64,
                "physical_cache_slab_bytes": 33_685_504u64,
                "codec_workspace_bytes": 794_624u64,
                "compressed_payload_bytes": 1_102_109u64,
                "raw_payload_bytes": 0u64,
                "metadata_bytes": null,
                "raw_bypass_bytes": 0u64,
                "codec_integrity_errors": 0u64,
                "codec_decode_errors": 0u64,
                "codec_timeouts": 0u64
            }),
        );
        terminal
            .draw(|f| draw(f, &obs, &history, &metadata, Language::En, Screen::Main))
            .expect("draw en compressed cache");
        let text_cache = buffer_text(&terminal);
        assert!(text_cache.contains("NBD 140.0 MiB / 4.0 GiB"));
        assert!(text_cache.contains("GPU CUDA/PTX LZ4"));
        assert!(text_cache.contains("40.16:1"));
        assert!(text_cache.contains("78% (176)"));
        assert!(text_cache.contains("GPU writes  118524"));

        // Verify Details screen renders real dynamic target without hardcoded 4 GiB / 33 MiB,
        // and Help/Glossary clarifies zram 1:1 pages and GPU CUDA/PTX LZ4.
        obs.status.insert(
            "capacity".into(),
            json!({
                "cache_target_kib": 131_072u64,
                "logical_capacity_kib": 4_194_304u64,
                "vram_cached_kib": 33_672u64,
                "gpu_headroom_kib": 2_786_304u64
            }),
        );
        terminal
            .draw(|f| {
                draw(
                    f,
                    &obs,
                    &history,
                    &metadata,
                    Language::PtBr,
                    Screen::Details,
                )
            })
            .expect("draw pt-br details");
        let text_details = buffer_text(&terminal);
        assert!(text_details.contains("128,0 MiB"), "{text_details}");
        assert!(!text_details.contains("4 GiB @2:1"), "{text_details}");
        assert!(!text_details.contains("33 MiB"), "{text_details}");
        assert!(text_details.contains("Leitura:"), "{text_details}");

        terminal
            .draw(|f| draw(f, &obs, &history, &metadata, Language::PtBr, Screen::Help))
            .expect("draw pt-br glossary");
        let text_help = buffer_text(&terminal);
        assert!(text_help.contains("págs 1:1"), "{text_help}");
        assert!(text_help.contains("CUDA PTX LZ4"), "{text_help}");

        // Verify idle codec-ready state avoids misleading "not measured" phrasing.
        obs.status.insert(
            "gpu_cache".into(),
            json!({
                "schema_version": 1u8,
                "sampled_at_unix_ms": 1_700_000_010_000u64,
                "codec": {"capability": "available", "state": "ready", "refusal_reason": null},
                "logical_cached_bytes": 0u64,
                "physical_cache_slab_bytes": 33_685_504u64,
                "codec_workspace_bytes": 794_624u64,
                "compressed_payload_bytes": 0u64,
                "raw_payload_bytes": 0u64,
                "metadata_bytes": null,
                "raw_bypass_bytes": 0u64,
                "codec_integrity_errors": 0u64,
                "codec_decode_errors": 0u64,
                "codec_timeouts": 0u64
            }),
        );
        terminal
            .draw(|f| draw(f, &obs, &history, &metadata, Language::PtBr, Screen::Main))
            .expect("draw pt-br idle codec");
        let text_idle = buffer_text(&terminal);
        assert!(
            text_idle.contains(dashboard_phrase(
                Language::PtBr,
                DashboardPhrase::CodecAwaitingTier2
            )),
            "{text_idle}"
        );

        // Verify high-capacity GPU budgets (e.g. 96 GiB total / 1.5 TiB logical) scale cleanly.
        assert_eq!(fmt_bytes(96 * 1024 * 1024 * 1024, Language::En), "96.0 GiB");
        assert_eq!(
            fmt_bytes(1536 * 1024 * 1024 * 1024, Language::PtBr),
            "1,5 TiB"
        );
    }
}
