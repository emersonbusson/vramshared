// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/dashboard/compression.rs
//! Width-aware zram compression summary.

use super::super::i18n::{DashboardPhrase, dashboard_phrase};
use super::{Language, Observation, fmt_bytes, text};
use ratatui::text::Line;

const ZRAM_PAGE_BYTES: u64 = 4096;

pub(super) fn compression_line(
    observation: &Observation,
    language: Language,
    width: usize,
) -> Option<String> {
    let zram = observation.value("zram_compression")?;
    let original = zram.get("orig_data_bytes")?.as_u64()?;
    let stored = zram.get("compr_data_bytes")?.as_u64()?;
    let cost = zram.get("mem_used_bytes").and_then(|value| value.as_u64());
    let pages = zram.get("huge_pages").and_then(|value| value.as_u64());
    let compressor = zram
        .get("compressor")
        .and_then(|value| value.as_str())
        .unwrap_or("n/a");
    let ratio = if stored > 0 {
        Some(original as f64 / stored as f64)
    } else {
        None
    };
    let text = text(language);
    let ratio = ratio.map_or_else(
        || text.not_measured.to_string(),
        |ratio| format!("{}:1", language.number(format!("{ratio:.2}"))),
    );
    let saved = cost.map_or(0, |bytes| original.saturating_sub(bytes));
    let mut line = if stored == 0 {
        format!(" {}: {ratio}", text.zram)
    } else if saved > 0 {
        let with_saved = format!(
            " {} → {}  {ratio} ({} {})",
            fmt_bytes(original, language),
            fmt_bytes(stored, language),
            dashboard_phrase(language, DashboardPhrase::SavedLabel),
            fmt_bytes(saved, language),
        );
        if Line::from(with_saved.as_str()).width() <= width {
            with_saved
        } else {
            format!(
                " {} → {}  {ratio}",
                fmt_bytes(original, language),
                fmt_bytes(stored, language),
            )
        }
    } else {
        format!(
            " {} → {}  {ratio}",
            fmt_bytes(original, language),
            fmt_bytes(stored, language),
        )
    };
    if Line::from(line.as_str()).width() > width {
        line = format!(" {}: {ratio}", text.zram);
    }
    let pages_item = match pages {
        Some(pages) => {
            let one_to_one_bytes = pages.saturating_mul(ZRAM_PAGE_BYTES);
            format!(
                "{pages} {} ({})",
                dashboard_phrase(language, DashboardPhrase::PagesOneToOne),
                fmt_bytes(one_to_one_bytes, language)
            )
        }
        None => text.not_measured.to_string(),
    };
    for item in [
        format!(
            "{} {}",
            text.cost,
            cost.map_or_else(
                || text.not_measured.to_string(),
                |bytes| fmt_bytes(bytes, language)
            )
        ),
        compressor.to_string(),
        pages_item,
    ] {
        let candidate = format!("{line}  · {item}");
        if Line::from(candidate.as_str()).width() > width {
            break;
        }
        line = candidate;
    }
    Some(if Line::from(line.as_str()).width() <= width {
        line
    } else {
        String::new()
    })
}
