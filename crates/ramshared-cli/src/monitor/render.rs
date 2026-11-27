// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/render.rs
//! Terminal rendering for the monitor.
//!
//! Pure text shaping and the widget layout that places it. This module
//! opens nothing, probes nothing and classifies nothing: bounded reads and
//! health decisions live in the parent module, the wire form lives in
//! [`super::model`], and the refresh/event loop lives beside them.
//!
//! Every item here is `pub(super)` rather than `pub(crate)`: the parent
//! composes these into a frame and the parent's tests pin their wording,
//! and nothing else in the crate may reach them.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::classify::observation_sample_is_stale;
use super::model::GpuObservation;
use super::observations::{
    BENCHMARK_STATUS_PATH_NOT_FOUND, VMSTAT_PAGE_BYTES, cache_telemetry_record,
};
use super::{
    ControlPlaneObservation, GPU_BUDGET_MAX_AGE_MS, LatencySource, MILLIS_PER_SECOND,
    MemoryObservation, Observation, TierIoStats, format_cache_telemetry_labels,
};
use crate::cascade;
use crate::installed_image::{
    BUILD_GIT_SHA, BUILD_TREE_STATE, InstallKind, InstallationIdentityCache, PRODUCT_ROOT,
    ProcessInstallOrigin, cli_install_entries, current_release_version,
    current_running_executable_metadata, current_running_executable_path,
    current_running_executable_sha256, effective_tui_reexec_origin, format_build_identity,
    format_installed_identity_lines, format_running_identity_line, restart_target,
    running_install_status_from_metadata, selected_release_version,
};
use crate::release_coherence::{client_banner, previous_release_hint};
use crate::zram_compression::{
    ZramCompression, all_stored_pages_same_filled, compression_ratio_of,
};
use ramshared_vram::{GpuBudgetSource, GpuUtilizationTelemetry, TELEMETRY_MAX_AGE_MS};

#[cfg_attr(not(test), allow(dead_code))]
mod panels;
#[cfg_attr(not(test), allow(dead_code))]
mod thresholds;
#[cfg_attr(not(test), allow(dead_code))]
mod tiers;

pub(super) use panels::*;
pub(super) use thresholds::*;
pub(super) use tiers::*;
/// The one spelling of a figure whose sample did not refresh.
///
/// A failed refresh freezes every number in the observation. A rate, a
/// latency or a ratio computed from that freeze must not be presented as a
/// live reading, and no panel may contradict the control panel's own marker
/// with a second wording of "stale". This is that marker's spelling wherever
/// a **derived** figure freezes: the value is replaced, because there is no
/// last-known rate to keep. Level figures are different — they keep their
/// last-known value and carry [`STALE_SAMPLE_NOTICE`] on their panel instead
/// (see [`stale_panel_title`]).
pub(super) const STALE_SAMPLE_ROW: &str = "stale";

/// The collection-staleness mark on a panel title.
///
/// One spelling, one place: a panel of level figures keeps every value it
/// holds and this is how the reader learns the screen stopped refreshing.
/// Built only by [`stale_panel_title`], so no panel can invent a second
/// wording — which is how `GPU (stale)`, `Host RAM (stale):` and
/// `Pressure: sample stale` used to sit on one screen beside the control
/// panel's own `Read: stale`.
pub(super) const STALE_SAMPLE_NOTICE: &str = "(⚠ Stale Sample)";

/// The mark that says a figure is a provider's estimate, not the driver's
/// report.
///
/// A property of the **body**, never of the clock, so it is a different axis
/// from [`STALE_SAMPLE_ROW`] and the two compose instead of competing: a
/// sample can be an estimate and cold at the same time, and both facts stay
/// on the line. One spelling, one place, for the same reason as the staleness
/// marks — a second wording of "this is not the driver's number" is how an
/// operator learns to distrust the whole panel.
///
/// This is not the panel's absence wording and never replaces it. Absence
/// says the provider published nothing this panel can show; this says it
/// published a real number of a less authoritative kind. Collapsing the two
/// told an operator the driver never spoke when it spoke an estimate.
pub(super) const ESTIMATE_SAMPLE_ROW: &str = "estimate";

/// A panel title carrying the one collection-staleness mark.
///
/// The counterpart of [`sample_derived`]: that one replaces a derived figure
/// when its sample froze, this one keeps every level on the panel and says
/// so on the title. See [`STALE_SAMPLE_NOTICE`].
pub(super) fn stale_panel_title(base: &str, sample_stale: bool) -> String {
    if sample_stale {
        format!("{base} {STALE_SAMPLE_NOTICE}")
    } else {
        base.to_string()
    }
}

/// Smallest dt, in seconds, the rate window accepts.
///
/// `last_io_sample` is seeded immediately before the first refresh, so the
/// first dt is near zero. That is not a rate over any window and must never be
/// accepted as one, whatever interval the operator configured.
pub(super) const MIN_RATE_WINDOW_SECS: f64 = 0.05;

/// How many configured refresh periods a rate window may span.
///
/// A blocked refresh leaves `last_io_sample` pointing before the gap, and the
/// average over that gap is a rate smeared across missed periods. Ten periods
/// is the ceiling the product has always applied at the default one-second
/// interval; expressed this way it also covers an interval the operator
/// explicitly configured, instead of a fixed ten seconds that silently
/// disabled every measurement above it.
pub(super) const RATE_WINDOW_PERIODS: f64 = 10.0;

/// Floor on [`RATE_WINDOW_PERIODS`], in seconds.
///
/// Preserves the historical ceiling for intervals at or under one second.
pub(super) const RATE_WINDOW_FLOOR_SECS: f64 = 10.0;

/// The dt window a rate sample must fall inside, for the configured interval.
///
/// A rate is a figure over the refresh period the operator asked for. The
/// window follows that period, so every interval the parser accepts can
/// actually produce a rate.
pub(super) fn rate_window_secs(interval_ms: u64) -> (f64, f64) {
    let interval_secs = interval_ms as f64 / MILLIS_PER_SECOND as f64;
    (
        MIN_RATE_WINDOW_SECS,
        (interval_secs * RATE_WINDOW_PERIODS).max(RATE_WINDOW_FLOOR_SECS),
    )
}

/// Render a figure derived from the current sample, or the one stale marker.
///
/// Rates, latencies and ratios computed from a frozen sample must not be
/// presented as live readings — the control panel already calls that same
/// freeze `Read: stale`, and a second screenful of confident numbers beside
/// it is the monitor contradicting itself. Level figures are different: they
/// keep their last-known value and carry [`STALE_SAMPLE_NOTICE`] on their
/// panel instead.
///
/// A formatter that owns an explicit non-measurement — a missing driver
/// source, an unpopulated counter — takes [`observation_sample_is_stale`]
/// itself and keeps that wording. Absence is more specific than a refresh
/// failure and must not be rewritten into one.
pub(super) fn sample_derived(sample_stale: bool, live: String) -> String {
    if sample_stale {
        STALE_SAMPLE_ROW.to_string()
    } else {
        live
    }
}

pub(super) fn format_memory_pressure(
    control_plane: &ControlPlaneObservation,
    sample_stale: bool,
) -> String {
    if sample_stale {
        // The derived marker's one spelling, not a second phrase. The row
        // label stays, so the line keeps its shape across the states.
        return format!("Pressure: {STALE_SAMPLE_ROW}");
    }
    if !control_plane.memory_psi_available {
        return "Pressure: PSI unavailable".to_string();
    }
    format!(
        "Pressure: PSI some={:.2}% full={:.2}%",
        control_plane.memory_psi_some_avg10, control_plane.memory_psi_full_avg10
    )
}

/// The cache-occupancy row, including its two refusals.
///
/// Sibling of [`utilization_row`] — same three states, same order, same
/// reason. Cache occupancy is a **level** figure with no projection field the
/// way `capacity.gpu_headroom_kib` is one for the budget, so the gate lives
/// here rather than on a sibling wire field.
///
/// - `not measured` when the worker never published a body this reader can use
///   — absent, `null`, unparsable, or an unknown schema. Absence is the more
///   specific truth and is never rewritten as `stale`.
/// - `stale`, when a real reading is not current: the sample aged outside
///   [`TELEMETRY_MAX_AGE_MS`], or this observation's own collection failed
///   ([`observation_sample_is_stale`]). Both staleness causes spell `stale` on
///   this row and stay apart one level up under their own names
///   (`sample_refresh_failed` vs `gpu_cache_telemetry_invalid_or_stale`).
/// - the figure, for a real current sample — including a measured zero, which
///   is a measurement and is rendered as such.
///
/// The field arrives here as the **record** via
/// [`crate::monitor::observations::cache_telemetry_record`], never as an
/// already-trusted sample. The admission collapse
/// ([`crate::published_budget::trusted_cache_field`]) stays alive for the alarm
/// that wants exactly "present but not trustworthy"; it is deliberately not
/// the source of this row. Collapsing the two here was the defect: an aged
/// reading printed as `omitted (stale or malformed)` erased a real sample and
/// told the operator the worker never spoke when it spoke seconds ago.
///
/// `now_unix_ms` is the frame's freshness clock — the caller passes the
/// observation's own [`Observation::epoch_ms`], never a wall-clock read at
/// print time. See [`Observation::epoch_ms`] for why one frame carries one
/// clock.
pub(super) fn cache_telemetry_row(
    field: Option<&Value>,
    sample_stale: bool,
    now_unix_ms: u64,
) -> String {
    const NOT_MEASURED: &str = "VRAM cache codec telemetry: not measured";
    const STALE: &str = "VRAM cache codec telemetry: stale";
    let Some(cache) = cache_telemetry_record(field) else {
        return NOT_MEASURED.to_string();
    };
    // Nonsense is absence before it is age: a body the reader cannot use is
    // never rewritten into a stale reading the worker actually produced.
    if !cache.is_well_formed() {
        return NOT_MEASURED.to_string();
    }
    if sample_stale || !cache.is_fresh_at(now_unix_ms, TELEMETRY_MAX_AGE_MS) {
        return STALE.to_string();
    }
    format_cache_telemetry_labels(&cache)
}

pub(super) fn format_volume_bytes(bytes: u64) -> String {
    let kib = bytes as f64 / 1024.0;
    let mib = kib / 1024.0;
    let gib = mib / 1024.0;
    if gib >= 1.0 {
        format!("{gib:>5.2} GB")
    } else if mib >= 1.0 {
        format!("{mib:>5.1} MB")
    } else if kib >= 1.0 {
        format!("{kib:>5.0} KB")
    } else {
        format!("{bytes:>5} B")
    }
}

/// Render one instantaneous rate for a fixed-width cell.
///
/// `None` is "no accepted rate window for this frame" and is not a measured
/// zero: it renders as `n/a` so a reader can tell an idle that was observed
/// from a rate nobody has computed yet. A present rate goes through
/// [`display_mbs_tenths`], so a real rate too small to print at one decimal is
/// never dressed in the measured-idle face.
pub(super) fn format_rate_cell(rate: Option<f64>) -> String {
    match rate {
        Some(mbs) => display_mbs_tenths(mbs),
        None => "n/a".to_string(),
    }
}

/// Render one side of the cumulative page-I/O row.
///
/// `pswpin` and `pswpout` are independent counters, so one side's absence is
/// not the other's: each publishes its own face. A present `0` is a measured
/// zero and prints `0 pgs (0 B)`; a counter `/proc/vmstat` did not report is
/// `not measured`. The byte figure is the page count times the unit named
/// beside [`VMSTAT_PAGE_BYTES`], not an independent byte reading.
pub(super) fn page_io_side(pages: Option<u64>) -> String {
    match pages {
        Some(count) => format!(
            "{count} pgs ({})",
            format_volume_bytes(count.saturating_mul(VMSTAT_PAGE_BYTES))
        ),
        None => "not measured".to_string(),
    }
}

/// Render the fault rates of one frame.
///
/// Both counters are written from the same accepted window, so an absent pair
/// is "no window yet" and not a measured zero — the same rule as the MB/s
/// rates. A present `0` is a measured idle.
pub(super) fn format_page_fault_rates(
    pgfault_per_sec: Option<u64>,
    pgmajfault_per_sec: Option<u64>,
) -> String {
    match (pgfault_per_sec, pgmajfault_per_sec) {
        (Some(total), Some(major)) => {
            let minor = total.saturating_sub(major);
            format!("{total}/s (Minor: {minor}/s │ Major: {major}/s)")
        }
        _ => "not measured".to_string(),
    }
}

/// Whether a tier's last accepted window shows it taking traffic.
///
/// This is the single predicate for the fact, and the tier row's quiet word is
/// its negation: at [`PAGING_MIN_MBS`] the tier is paging here and the row
/// says so, so one boundary cannot be read two ways on the same screen.
///
/// An absent rate is not a measured zero and not "active": with no window
/// there is no observation to classify, so the tier falls through to the
/// next one and the estimate lands on the no-traffic baseline. A half-measured
/// pair is still evidence in the direction that *was* measured — the row
/// refuses to publish a state word for a partial sample, which is a display
/// standard, not a claim that the tier is idle.
pub(super) fn tier_is_paging(io: &TierIoStats) -> bool {
    io.read_mbs.is_some_and(|mbs| mbs >= PAGING_MIN_MBS)
        || io.write_mbs.is_some_and(|mbs| mbs >= PAGING_MIN_MBS)
}

/// The RAM row's fill percentage, or `None` when the RAM pair has no reading.
///
/// A thin reader over [`tier_fill_pct`] and [`MemoryObservation::ram_pair`]:
/// it names which two fields are the numerator and the denominator, and
/// leaves every honesty rule of the ratio and of the pair to their one
/// decisions. It exists so the sparkline history and the row on screen cannot
/// compute two different fills out of one measurement. An absent or
/// self-disagreeing pair is `None`, never `Some(0)` — the fold this series
/// removes.
pub(super) fn memory_used_pct(memory: &MemoryObservation) -> Option<u64> {
    let (total, available) = memory.ram_pair()?;
    tier_fill_pct(total.saturating_sub(available), total)
}

/// The GPU budget's fill percentage, or `None` when there is no reading.
///
/// Occupancy of the driver-reported **budget**, not of physical VRAM: that
/// is the denominator the admission path uses and the only one the driver
/// publishes. An absent budget is `None`, never `Some(0)`.
pub(super) fn gpu_used_pct(gpu: Option<&GpuObservation>) -> Option<u64> {
    let gpu = gpu?;
    tier_fill_pct(gpu.used_bytes, gpu.budget_bytes)
}

/// The compression tier's saved percentage, or `None` when there is no
/// reading.
///
/// `100 * (orig - compr) / orig`: 0 is a tier holding everything
/// uncompressed, 100 is one holding only fill references. A high bar means
/// *more* compaction, which is the direction the operator cares about. The
/// two counts are the measurement and any published ratio is an arithmetic
/// consequence of them with no authority of its own. `orig == 0` is `None`:
/// there is no data held, so there is no compaction to report. A `compr`
/// larger than `orig` is a tear and is `None` too, never clamped into a
/// plausible-looking number.
pub(super) fn compression_saved_pct(field: Option<&Value>) -> Option<u64> {
    let compression: ZramCompression = serde_json::from_value(field?.clone()).ok()?;
    let orig = compression.orig_data_bytes;
    let compr = compression.compr_data_bytes;
    if orig == 0 || compr > orig {
        return None;
    }
    let saved = orig - compr;
    Some((saved.saturating_mul(100)) / orig)
}

/// The VRAM cache codec's saved percentage, or `None` when there is no
/// reading.
///
/// The cache's own compaction, which is a different quantity from the zram
/// one above: this is the codec that sits between the guest and the GPU
/// store. `100 * (raw - compressed) / raw`. `raw == 0` is `None` — nothing
/// went through the codec, so there is no compaction to report — and a
/// `compressed` larger than `raw` is a tear and is `None`, not a negative
/// saving forced into the chart.
pub(super) fn cache_saved_pct(field: Option<&Value>) -> Option<u64> {
    let cache = crate::monitor::observations::cache_telemetry_record(field)?;
    if !cache.is_well_formed() {
        return None;
    }
    let raw = cache.raw_payload_bytes;
    let compressed = cache.compressed_payload_bytes;
    if raw == 0 || compressed > raw {
        return None;
    }
    let saved = raw - compressed;
    Some((saved.saturating_mul(100)) / raw)
}

/// The cache's occupancy of its own slab, or `None` when there is no
/// reading.
///
/// "How much is in cache", asked as a ratio: the bytes the cache is holding
/// against the bytes its slab actually costs the host. A slab that allocates
/// nothing has no denominator and is `None`, not `Some(0)`.
pub(super) fn cache_held_pct(field: Option<&Value>) -> Option<u64> {
    let cache = crate::monitor::observations::cache_telemetry_record(field)?;
    if !cache.is_well_formed() {
        return None;
    }
    tier_fill_pct(cache.logical_cached_bytes, cache.physical_cache_slab_bytes)
}

/// The VRAM cache's compaction as a live `raw → stored` line.
///
/// This is the "4 GB became 1.4 GB" face the volume row does not give: the
/// two counters side by side with the ratio derived from them, refreshed
/// with every frame. Two quantities and one consequence — `raw` is what the
/// guest wrote into the cache, `stored` is what that data actually costs the
/// host after the codec, and the ratio is their arithmetic.
///
/// Three honest states, and no fourth:
/// - `not measured` when there is no usable cache sample.
/// - `holding nothing` when the sample is real and `raw == 0`: nothing has
///   gone through the codec, so there is no compaction to report. Inventing
///   `1.00:1` here would claim a ratio with no data behind it.
/// - the pair and the ratio otherwise. A `stored` larger than `raw` is a
///   tear and is `not measured`, never a ratio below `1.00:1` forced onto
///   the line.
///
/// Cache occupancy and cache compaction are different facts and must not
/// share a face: occupancy is how much the cache is **holding**, compaction
/// is how efficiently it is **storing** it. A cache can be full of
/// incompressible pages (high occupancy, no saving) or nearly empty but
/// holding pages the codec crushed (low occupancy, high saving).
pub(super) fn cache_compaction_row(field: Option<&Value>) -> String {
    const NOT_MEASURED: &str = "VRAM cache compaction: not measured";
    const HOLDING_NOTHING: &str = "VRAM cache compaction: holding nothing";
    let Some(cache) = crate::monitor::observations::cache_telemetry_record(field) else {
        return NOT_MEASURED.to_string();
    };
    if !cache.is_well_formed() {
        return NOT_MEASURED.to_string();
    }
    let raw = cache.raw_payload_bytes;
    let stored = cache.compressed_payload_bytes;
    if raw == 0 {
        return HOLDING_NOTHING.to_string();
    }
    if stored > raw {
        return NOT_MEASURED.to_string();
    }
    let ratio = raw as f64 / stored.max(1) as f64;
    format!(
        "VRAM cache compaction: {} → {} ({ratio:.2}:1)",
        format_volume_bytes(raw).trim(),
        format_volume_bytes(stored).trim(),
    )
}

/// The dashboard's history series, one per panel the operator asked to see
/// over time.
///
/// Each series holds **measured fills only**. A frame that carried no
/// reading for a series takes no sample in it — the rule the RAM series
/// already follows and the reason it holds `u64` rather than `Option<u64>`.
/// Pushing a `0` for an absent reading would claim a measurement of
/// emptiness nobody made, and a zero bar on a compression chart would read
/// as "compaction is perfectly ineffective" rather than "no data was held".
///
/// A series that has never been measured stays empty and its panel says
/// `not measured` instead of drawing an empty chart the reader would take
/// for a flat zero.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct DashHistory {
    pub(super) ram: VecDeque<u64>,
    pub(super) gpu: VecDeque<u64>,
    pub(super) zram_saved: VecDeque<u64>,
    pub(super) cache_saved: VecDeque<u64>,
    pub(super) cache_held: VecDeque<u64>,
    pub(super) cache_hit_rate: VecDeque<u64>,
    pub(super) cache_origin_reads: VecDeque<u64>,
    pub(super) cache_fill: VecDeque<u64>,
    pub(super) cache_active: VecDeque<bool>,
    pub(super) pressure_full: VecDeque<u64>,
}

impl DashHistory {
    /// Record one frame's readings, taking only the ones that were measured.
    pub(super) fn push_observation(&mut self, observation: &Observation) {
        if let Some(pct) = memory_used_pct(&observation.mem) {
            // The history answers "how much room is left", not "how full is
            // it": a saturated guest renders a flat 100% used line that says
            // nothing. Available percent keeps the same 0..100 axis and moves.
            self.ram.push_back(100u64.saturating_sub(pct));
        }
        if let Some(pct) = gpu_used_pct(observation.gpu.as_ref()) {
            self.gpu.push_back(pct);
        }
        if let Some(pct) = compression_saved_pct(observation.value("zram_compression")) {
            self.zram_saved.push_back(pct);
        }
        let cache = observation.value("gpu_cache");
        if let Some(pct) = cache_saved_pct(cache) {
            self.cache_saved.push_back(pct);
        }
        if let Some(pct) = cache_held_pct(cache) {
            self.cache_held.push_back(pct);
        }
        let hits = observation.value("cache_hits").and_then(Value::as_u64);
        let fallback = observation
            .value("cache_fallback_reads")
            .and_then(Value::as_u64);
        if let (Some(hits), Some(fallback)) = (hits, fallback) {
            let requests = hits.saturating_add(fallback);
            if let Some(rate) = hits.saturating_mul(100).checked_div(requests) {
                self.cache_hit_rate.push_back(rate);
            }
            self.cache_origin_reads.push_back(fallback);
        }
        let target_kib = observation.capacity_u64("cache_target_kib");
        let held = crate::monitor::observations::cache_telemetry_record(cache)
            .map(|cache| cache.logical_cached_bytes);
        if let (Some(target_kib), Some(held)) = (target_kib, held)
            && let Some(fill) = held
                .saturating_mul(100)
                .checked_div(target_kib.saturating_mul(1024))
        {
            self.cache_fill.push_back(fill);
        }
        self.cache_active
            .push_back(observation.string("cache_state") == "ACTIVE");
        if observation.control_plane.memory_psi_available {
            let full = observation.control_plane.memory_psi_full_avg10;
            self.pressure_full
                .push_back(full.round().clamp(0.0, 100.0) as u64);
        }
    }

    /// Trim every series to `limit` measured samples.
    pub(super) fn trim(&mut self, limit: usize) {
        for series in [
            &mut self.ram,
            &mut self.gpu,
            &mut self.zram_saved,
            &mut self.cache_saved,
            &mut self.cache_held,
            &mut self.cache_hit_rate,
            &mut self.cache_origin_reads,
            &mut self.cache_fill,
            &mut self.pressure_full,
        ] {
            while series.len() > limit {
                series.pop_front();
            }
        }
        while self.cache_active.len() > limit {
            self.cache_active.pop_front();
        }
    }
}

pub(super) fn format_grouped_number(value: u64) -> String {
    let mut grouped = String::new();
    for (index, digit) in value.to_string().bytes().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(char::from(digit));
    }
    grouped.chars().rev().collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DashboardMetadata {
    pub(super) version_line: String,
    pub(super) running_line: String,
    pub(super) install_lines: Vec<String>,
    pub(super) restart_executable: Option<PathBuf>,
    pub(super) runtime_notice: Option<String>,
    /// `Update installed · from → to` / `Updated from → to`. Names both
    /// releases so the operator can see which transition happened (SPEC
    /// DT-4 / NFR-5).
    pub(super) update_notice: Option<String>,
}

impl DashboardMetadata {
    pub(super) fn current(
        identity_cache: &mut InstallationIdentityCache,
        process_origin: ProcessInstallOrigin,
        allow_reexec: bool,
        runtime_notice: Option<&str>,
    ) -> Self {
        let (direct_executable, active_release_executable) = cli_install_entries();
        let (direct_executable, active_release_executable) = (
            direct_executable.as_path(),
            active_release_executable.as_path(),
        );
        let direct_install = identity_cache.read(direct_executable, InstallKind::Direct);
        let active_install =
            identity_cache.read(active_release_executable, InstallKind::ActiveRelease);
        let running_metadata = current_running_executable_metadata();
        let running_sha256 = current_running_executable_sha256();
        let running = running_install_status_from_metadata(
            running_metadata.as_ref(),
            process_origin,
            direct_executable,
            active_release_executable,
        );
        let reexec_origin = effective_tui_reexec_origin(
            process_origin,
            current_running_executable_path().as_deref(),
            Path::new(PRODUCT_ROOT),
        );
        let restart_executable = restart_target(
            reexec_origin,
            running_metadata.as_ref(),
            running_sha256.as_deref(),
            allow_reexec,
            direct_install.as_ref(),
            active_install.as_ref(),
        );
        let update_notice = client_banner(
            reexec_origin,
            current_release_version().as_deref(),
            selected_release_version().as_deref(),
            previous_release_hint().as_deref(),
        );
        Self {
            version_line: format_build_identity(
                env!("CARGO_PKG_VERSION"),
                BUILD_GIT_SHA,
                BUILD_TREE_STATE,
            ),
            running_line: format_running_identity_line(
                env!("CARGO_PKG_VERSION"),
                BUILD_GIT_SHA,
                BUILD_TREE_STATE,
                running,
            ),
            install_lines: format_installed_identity_lines(&[
                ("direct /usr/local", direct_install.as_ref()),
                ("active /opt/ramshared/current", active_install.as_ref()),
            ]),
            restart_executable,
            runtime_notice: runtime_notice.map(str::to_string),
            update_notice,
        }
    }
}
