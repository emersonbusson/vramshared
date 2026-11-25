// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/render/tiers.rs
//! Tier status shaping and memory-tiers panel rendering.

use super::*;

/// Divisor of the whole-MiB **publish** conversion.
///
/// Comparisons never divide by this. See [`published_mib`].
const KIB_PER_MIB: u64 = 1024;

/// One tier's cell in the `tiers` status object.
///
/// `used_kib` and `size_kib` hold the unit the wire carries — the exact
/// figures the status document published, unrounded. Every comparison this
/// panel makes runs on those: the fill percentage, the bar fill, the peak
/// share, and the ACTIVE/ARMED status all read `used_kib > 0` or
/// `used_kib / size_kib`, and rounding first would move each of them.
/// 1536 / 2048 KiB is 75%, which round-to-nearest reports as 100%, and a
/// present 511 KiB reading rounds to `0 MiB` and would print ARMED. Both
/// fields are `Option`: a field that never arrived is absence and is never
/// a measured zero, which is what [`tier_usage_row`] refuses to invent.
///
/// [`published_mib`] is the whole-MiB **publish** face. The display names
/// MiB, so some rounding is unavoidable at that boundary — but it is
/// `div_ceil`, never floor and never round-to-nearest, because a non-zero
/// measurement must not publish as zero. That face is never fed back into a
/// comparison.
///
/// `prio` is the kernel's swap priority — the highest among the tier's rows,
/// which is exactly the figure the order check compared — or `None` when the
/// document carries none, which [`tier_priority_label`] names rather than
/// filling in with a product constant.
struct TierCell {
    /// Whether the wire said this tier is present. `None` is a `present`
    /// field that never arrived and is never a measured absence — see
    /// [`tier_presence_status`].
    present: Option<bool>,
    /// The wire's exact hold and capacity. `None` is a field that never
    /// arrived and is never a measured zero — see [`tier_usage_row`].
    used_kib: Option<u64>,
    size_kib: Option<u64>,
    prio: Option<i32>,
}

/// The tier's usage line: fill bar, percentage, MiB pair, and the peak.
///
/// Every figure on the left half is the wire's own `used_kib` / `size_kib`
/// pair. When either field never arrived the pair is not a zero pair — the
/// old code folded both into `0` and published a measured `0%` over an empty
/// bar beside `0 MiB / 0 MiB`, presenting a tier the wire never spoke about
/// as one that holds nothing and has no capacity. The left half then reads
/// `not measured`.
///
/// The peak on the right is the maximum over the run's occupancy samples: the
/// control-plane accumulator plus this frame's hold when it has one. With
/// neither, there is no sample and the peak reads `not measured` — an empty
/// maximum is not a measured `0 MiB`. Its percentage needs capacity and is
/// dropped rather than filled with `n/a` beside a number that is present.
pub(in crate::monitor) fn tier_usage_row(
    used_kib: Option<u64>,
    size_kib: Option<u64>,
    observed_peak_kib: Option<u64>,
    bar_len: u64,
) -> String {
    let peak_kib = match (used_kib, observed_peak_kib) {
        (Some(used), Some(observed)) => Some(observed.max(used)),
        (Some(used), None) => Some(used),
        (None, observed) => observed,
    };

    let hold = match (used_kib, size_kib) {
        (Some(used_kib), Some(size_kib)) => {
            let bar = make_tier_bar(used_kib, size_kib, bar_len);
            let pct_str = tier_fill_label(used_kib, tier_fill_pct(used_kib, size_kib));
            format!(
                "{bar} {pct_str} ( {:>4} MiB / {} MiB )",
                published_mib(used_kib),
                published_mib(size_kib),
            )
        }
        _ => "not measured".to_string(),
    };

    let peak = match (peak_kib, size_kib) {
        (Some(peak_kib), Some(size_kib)) => format!(
            "Peak: {:>4} MiB {}",
            published_mib(peak_kib),
            tier_peak_label(peak_kib, size_kib)
        ),
        (Some(peak_kib), None) => format!("Peak: {:>4} MiB", published_mib(peak_kib)),
        (None, _) => "Peak: not measured".to_string(),
    };

    format!("{hold} │ {peak}")
}

/// The whole-MiB **publish** conversion of a KiB figure — the one place this
/// panel rounds capacity.
///
/// `div_ceil`, never floor and never round-to-nearest. The display names MiB,
/// so some rounding is unavoidable at that boundary, but a floor division
/// turns a present 512 KiB reading into a measured `0 MiB`, and round-to-
/// nearest moves a fill percentage off the true ratio (1536 / 2048 KiB is
/// 75%, not the 100% its rounded MiB halves report).
///
/// This is a **face**. It is never fed back into a comparison, a percentage
/// or a bar fill: those run on the exact KiB figures the wire carries. See
/// [`TierCell`].
pub(in crate::monitor) fn published_mib(kib: u64) -> u64 {
    kib.div_ceil(KIB_PER_MIB)
}

/// The header's priority clause: the observed swap priority, or a word for its
/// absence.
///
/// The status document folds each tier's priority from `/proc/swaps` and
/// publishes `null` when it has none. Printing the number a product constant
/// would guess is the defect this replaces — a host whose tier 2 sits at 150
/// would be told it sits at 100, and nothing on the screen would disagree.
pub(in crate::monitor) fn tier_priority_label(prio: Option<i32>) -> String {
    match prio {
        Some(prio) => format!("Priority: {prio:>3}"),
        None => "priority unreported".to_string(),
    }
}

/// How a tier's header line reads its operating state.
///
/// `⚫ OFF` is the **measured** absent arm. It is not the same fact as a
/// `present` field that never arrived: the old `if !present` folded that
/// hole into the refutation's word and printed `⚫ OFF` for a tier the wire
/// never spoke about. [`tier_priority_label`] already refuses to fill the
/// same hole with a product constant; this face refuses to fill it with a
/// state nobody measured.
///
/// The two positive arms are the tier's own words, passed in, because the
/// policy is one (absent → OFF; present and holding → one word; present and
/// empty → the other) and the labels are the three tiers' names. Both arms
/// name an occupancy, so both need the figure pair to arrive: `used_kib`
/// says what is held and `size_kib` says against what. A present tier
/// missing either key has no occupancy to name, and the old fold of a
/// missing `used_kib` / `size_kib` to `0` published `ARMED` — a confident
/// "armed tier holding nothing" — from a figure the producer never sent.
///
/// The two absence arms are two different facts and do not share a word:
/// `⚪ not measured` is a presence that never arrived, `⚪ use unknown` is a
/// measured presence whose occupancy figures are incomplete. Both are short
/// on purpose — this sits at the end of the tier header line beside
/// `priority unreported`, and the Memory Tiers panel is roughly 70 columns,
/// so a truncated claim is worse than a short honest one.
pub(in crate::monitor) fn tier_presence_status(
    present: Option<bool>,
    used_kib: Option<u64>,
    size_kib: Option<u64>,
    holding: &'static str,
    empty: &'static str,
) -> &'static str {
    match (present, used_kib, size_kib) {
        (Some(false), _, _) => "⚫ OFF",
        (Some(true), Some(0), Some(_)) => empty,
        (Some(true), Some(_), Some(_)) => holding,
        (Some(true), _, _) => "⚪ use unknown",
        (None, _, _) => "⚪ not measured",
    }
}

/// The Tier 2 header status when it is bound as zram's writeback backing
/// device rather than appearing in `/proc/swaps`.
pub(in crate::monitor) fn writeback_tier_status(
    tier_state: Option<&str>,
    used_kib: Option<u64>,
) -> &'static str {
    match tier_state {
        Some("degraded") => "🟠 DEGRADED [Writeback]",
        Some("tier_lost") => "🔴 TIER LOST [Writeback]",
        Some("pump_stopped") => "🟡 PUMP STOPPED [Writeback]",
        Some("bound") if used_kib.is_some_and(|kib| kib > 0) => "🟢 ACTIVE [Writeback]",
        Some("bound") => "🟢 ARMED [Writeback]",
        _ => "🟢 BOUND [Writeback]",
    }
}

/// The Tier 1 status face, degraded taking precedence (SPEC DT-3).
///
/// A live zram tier whose backing device is `none` serves from RAM and must
/// not read `ACTIVE`: the state comes from the one derivation the status
/// document publishes, and this face does not re-derive it. A tier that
/// cannot name its authoritative copy reports the degradation instead of
/// the holding label.
pub(in crate::monitor) fn zram_tier_status(
    state: Option<&str>,
    present: Option<bool>,
    used_kib: Option<u64>,
    size_kib: Option<u64>,
) -> &'static str {
    if state == Some("degraded") {
        return "🟠 DEGRADED [1st Target]";
    }
    tier_presence_status(
        present,
        used_kib,
        size_kib,
        "🟢 ACTIVE [1st Target]",
        "🟢 ARMED [1st Target]",
    )
}

/// Where a tier's authoritative copy lives, or that the tier has none.
///
/// Every clause is an observation or a named absence (SPEC DT-6). A volatile
/// tier whose only copy is itself cannot report an authority — that is the
/// orphan-page condition this whole SPEC exists to make impossible, and the
/// row says so rather than printing a location the operator would trust.
///
/// `Cache of <x>` is a fast copy of something that still exists below.
/// `<x> only` means the tier is the sole copy and losing it loses the page.
/// `unmeasured` is the absence of a reading, and is not `0` or `none`.
pub(in crate::monitor) fn tier_authority_label(
    holding: &str,
    present: Option<bool>,
    lower_present: Option<bool>,
    lower_name: &str,
) -> String {
    match (present, lower_present) {
        // Nothing was observed about this tier at all.
        (None, _) => "unmeasured".to_string(),
        (Some(false), _) => "unmeasured".to_string(),
        // The tier is live and the tier below it is live too: this is a
        // cache, and the copy below is the authority.
        (Some(true), Some(true)) => format!("cache of {lower_name}"),
        // The tier is live but there is no observed lower tier. It is the
        // only copy we can see, and the row says so rather than inventing
        // an authority the observation did not carry.
        (Some(true), _) => format!("{holding} only"),
    }
}

/// The header's parenthetical for a tier's observed swap devices, or nothing
/// at all.
///
/// ` (zram0)` when the swap table named one, ` (zram0+zram1)` when a tier is
/// served by several. A tier with no live swap device prints no parenthetical
/// rather than a name the observation did not carry.
pub(in crate::monitor) fn tier_device_label(names: &[String]) -> String {
    if names.is_empty() {
        return String::new();
    }
    format!(" ({})", names.join("+"))
}

/// The allocation-order row: does this frame's swap-priority witness hold
/// the product's placement order?
///
/// The order itself is the legend at the foot of the dashboard (ADR-0003's
/// `zram → VRAM → VHDX`); this row reports the **frame's witness** for it,
/// the `order_ok` the status document folds from `/proc/swaps`. The old
/// footer printed the order as an unconditional `ALLOCATION GUARANTEE` on
/// frames that carry the very witness that can refute it. A claim the same
/// screen can contradict is a claim the reader learns to skip.
pub(in crate::monitor) fn allocation_order_row(order_ok: Option<bool>) -> String {
    match order_ok {
        Some(true) => "holding".to_string(),
        Some(false) => "refuted".to_string(),
        None => "not measured".to_string(),
    }
}

/// The SSD-vs-activation-baseline row.
///
/// `disk_growth_kib` is what the fallback SSD swap holds **above** what it
/// already held when this activation began. `Some(0)` is the measured
/// at-baseline state the old footer asserted unconditionally as "SSD usage
/// is cold WSL2 boot baseline"; a positive growth is this frame refuting
/// that assertion; `None` is no attributable activation baseline, which is
/// absence and never a zero growth.
pub(in crate::monitor) fn ssd_baseline_row(growth_kib: Option<u64>) -> String {
    match growth_kib {
        Some(0) => "at baseline".to_string(),
        Some(kib) => format!("+{} KiB", format_grouped_number(kib)),
        None => "not measured".to_string(),
    }
}

/// The daemon liveness row.
///
/// `daemon.alive` is a measurement the status document either published or
/// did not. `Some(true)` and `Some(false)` are the two states that were
/// observed; `None` is the field never arriving, which is a liveness that
/// was never taken and never a daemon that was found stopped. The old row
/// folded `None` into `false` and published `🔴 STOPPED` for a frame that
/// carries no daemon witness at all.
pub(in crate::monitor) fn daemon_status_row(alive: Option<bool>) -> String {
    match alive {
        Some(true) => "🟢 RUNNING".to_string(),
        Some(false) => "🔴 STOPPED".to_string(),
        None => "⚪ not measured".to_string(),
    }
}

/// The safety-guard row: this frame's witness for the fail-closed guard.
///
/// "Fail-Closed (Zero Panic)" is the product's **lint policy** — a build-time
/// claim about the source, not something this frame measures. The old row
/// printed it unconditionally beside an `Anomaly Counter` that can go
/// non-zero, which is a claim the same screen can contradict; a reader
/// learns to skip those. The policy belongs in the legend, not in the
/// measured band.
///
/// This row reports the witness instead: the `guardian_state` the status
/// document folds from the guard plane. `SAFE_MODE` is the guard having
/// engaged — the fail-closed behaviour under way — and is not the healthy
/// arm and not a refutation. A state the face has no word for is not
/// absence either, so it gets its own short arm rather than borrowing
/// `not measured`.
pub(in crate::monitor) fn safety_guard_row(guardian_state: Option<&str>) -> String {
    match guardian_state {
        Some("HEALTHY") => "holding".to_string(),
        Some("SAFE_MODE") => "engaged".to_string(),
        Some("BLOCKED") => "refuted".to_string(),
        Some(_) => "unrecognized".to_string(),
        None => "not measured".to_string(),
    }
}

pub(in crate::monitor) fn draw_tiers(frame: &mut Frame<'_>, area: Rect, observation: &Observation) {
    let width = area.width;
    let bar_len = ((width as u64) / BAR_WIDTH_DIVISOR).clamp(BAR_MIN_CELLS, BAR_MAX_CELLS);
    let sep_len = (width.saturating_sub(4) as usize).max(20);
    let sep = "─".repeat(sep_len);
    // The tier panel is the one place a reader sees a rate and a lifetime
    // total side by side. Without this the panel keeps printing confident
    // speeds while the control panel on the same screen says `Read: stale`.
    let sample_stale = observation_sample_is_stale(observation);
    // Peaks and rates are derived from accepted windows; zero means the run
    // has not produced one yet and every derived figure below is unmeasured.
    let rate_windows = observation.control_plane.rate_windows;

    let tiers = observation
        .value("tiers")
        .and_then(Value::as_object)
        .map(|tiers| {
            // Helper that reads one tier's exact figures off the status
            // document. Nothing is rounded here: `TierCell` holds the KiB the
            // wire carries and converts only at the publish face.
            let get = |name: &str| -> TierCell {
                let tier = tiers.get(name).and_then(Value::as_object);
                let present = tier.and_then(|t| t.get("present")).and_then(Value::as_bool);
                let used_kib = tier.and_then(|t| t.get("used_kib")).and_then(Value::as_u64);
                let size_kib = tier.and_then(|t| t.get("size_kib")).and_then(Value::as_u64);
                let prio = tier
                    .and_then(|t| t.get("prio"))
                    .and_then(Value::as_i64)
                    .and_then(|p| i32::try_from(p).ok());
                TierCell {
                    present,
                    used_kib,
                    size_kib,
                    prio,
                }
            };

            let zram = get("zram");
            let vram = get("vram");
            let disk = get("disk");

            let zram_status = zram_tier_status(
                tiers
                    .get("zram")
                    .and_then(Value::as_object)
                    .and_then(|tier| tier.get("state"))
                    .and_then(Value::as_str),
                zram.present,
                zram.used_kib,
                zram.size_kib,
            );

            // SPEC DT-1: the tier's role is a fact of the running system, and
            // the header has to say it. In the writeback topology the tier is
            // the zram's writeback target: it is not a swap device, it never
            // appears in the swap table, and "unmeasured" would describe a
            // tier that is present and busy. The row names the role instead.
            let writeback_target = observation
                .value("tier_roles")
                .and_then(|roles| roles.get("writeback_target_is_tier"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let wb_used_kib = writeback_target
                .then(|| {
                    observation
                        .value("capacity")
                        .and_then(|cap| cap.get("ssd_origin_written_kib"))
                        .and_then(Value::as_u64)
                })
                .flatten();
            let wb_size_kib = writeback_target
                .then(|| {
                    observation
                        .value("capacity")
                        .and_then(|cap| cap.get("logical_capacity_kib"))
                        .and_then(Value::as_u64)
                })
                .flatten();
            let vram_cell = if writeback_target && vram.present != Some(true) {
                TierCell {
                    present: Some(true),
                    used_kib: wb_used_kib.or(vram.used_kib),
                    size_kib: wb_size_kib.or(vram.size_kib),
                    prio: vram.prio,
                }
            } else {
                vram
            };
            let vram_status = if writeback_target && vram_cell.prio.is_none() {
                writeback_tier_status(
                    observation.value("tier_state").and_then(Value::as_str),
                    wb_used_kib,
                )
            } else {
                tier_presence_status(
                    vram_cell.present,
                    vram_cell.used_kib,
                    vram_cell.size_kib,
                    "🟢 ACTIVE [2nd Target]",
                    "🟢 ARMED [2nd Target]",
                )
            };
            let origin_cache_mode = observation.value("tier_roles")
                .and_then(|roles| roles.get("origin_cache_mode"))
                .and_then(Value::as_bool);
            let tier_name = match origin_cache_mode {
                Some(true) if writeback_target => "ORIGIN BACKING",
                Some(true) => "ORIGIN SWAP",
                Some(false) => "GPU VRAM",
                None => "MANAGED SWAP",
            };
            let v_role = if writeback_target {
                " [writeback target]"
            } else {
                ""
            };
            let v_auth = if writeback_target {
                "the zram's writeback target".to_string()
            } else if origin_cache_mode == Some(true) && vram_cell.present == Some(true) {
                "authoritative origin".to_string()
            } else if origin_cache_mode != Some(false) {
                "authority unverified".to_string()
            } else {
                tier_authority_label("vram", vram_cell.present, zram.present, "the zram tier")
            };

            let disk_status = tier_presence_status(
                disk.present,
                disk.used_kib,
                disk.size_kib,
                "🔵 COLD BOOT BASELINE",
                "🔵 STANDBY [3rd Fallback]",
            );

            // The speedup row compares against the fallback disk tier in this
            // same observation. It is the baseline, so it is passed to every
            // call and reports itself as throughput rather than as a multiple.
            let fallback_io = &observation.control_plane.disk_io;
            let zram_speedup = sample_derived(
                sample_stale,
                compute_tier_speedup(
                    &observation.control_plane.zram_io,
                    cascade::SwapTierRole::Zram,
                    fallback_io,
                ),
            );
            let vram_speedup = sample_derived(
                sample_stale,
                compute_tier_speedup(
                    &observation.control_plane.vram_io,
                    cascade::SwapTierRole::Vram,
                    fallback_io,
                ),
            );
            let disk_speedup = sample_derived(
                sample_stale,
                compute_tier_speedup(fallback_io, cascade::SwapTierRole::Fallback, fallback_io),
            );

            let z_lat = sample_derived(
                sample_stale,
                format_tier_latency(&observation.control_plane.zram_io, "ZRAM"),
            );
            let v_lat = sample_derived(
                sample_stale,
                format_tier_latency(&observation.control_plane.vram_io, match origin_cache_mode {
                    Some(true) => "origin swap",
                    Some(false) => "GPU cache",
                    None => "managed swap",
                }),
            );
            let d_lat = sample_derived(
                sample_stale,
                format_tier_latency(&observation.control_plane.disk_io, "disk"),
            );

            let z_use = tier_usage_row(
                zram.used_kib,
                zram.size_kib,
                observation.control_plane.zram_peak_used_kib,
                bar_len,
            );
            let v_use = tier_usage_row(
                vram_cell.used_kib,
                vram_cell.size_kib,
                observation.control_plane.vram_peak_used_kib,
                bar_len,
            );
            let d_use = tier_usage_row(
                disk.used_kib,
                disk.size_kib,
                observation.control_plane.disk_peak_used_kib,
                bar_len,
            );

            let format_speed = |io: &TierIoStats, cell: &TierCell| {
                if sample_stale {
                    // Same two tokens the control panel prints, so the screen
                    // never says one rate is stale and its twin is live.
                    return format!("Read: {STALE_SAMPLE_ROW} │ Write: {STALE_SAMPLE_ROW}");
                }
                let read_cell = format_rate_cell(io.read_mbs);
                let write_cell = format_rate_cell(io.write_mbs);
                if !io.has_rate_sample() {
                    // No accepted window for this frame: the idle below is a
                    // measurement, and this is not one.
                    return format!(
                        "Read: {read_cell:>5} │ Write: {write_cell:>5} MB/s (not measured)"
                    );
                }
                // Both directions are measured here, so the negation of the
                // paging predicate is exactly the quiet floor — one boundary,
                // already decided, not a second test of the same fact.
                let rates = format!("Read: {read_cell:>5} │ Write: {write_cell:>5} MB/s");
                if tier_is_paging(io) {
                    return format!("{rates} (⚡ Paging Active)");
                }
                match (cell.used_kib, cell.size_kib) {
                    // The predicate is the exact figure pair; only the printed
                    // hold is the MiB face. A present 511 KiB reading retains.
                    // Both faces name an occupancy, so both need `size_kib`
                    // too: Standby is "holding nothing of a capacity" and
                    // Retaining is "holding this much of one". A pair the
                    // wire never completed cannot support either claim.
                    (Some(0), Some(_)) => format!("{rates} (💤 Standby)"),
                    (Some(used_kib), Some(_)) => format!(
                        "{rates} (💤 Retaining {used_mib:>4} MiB)",
                        used_mib = published_mib(used_kib)
                    ),
                    // Retaining and Standby are both claims about the hold
                    // against its capacity. With either figure never published
                    // neither can be named, and the rates alone are what this
                    // frame measured.
                    _ => rates,
                }
            };
            let z_speed = format_speed(&observation.control_plane.zram_io, &zram);
            let v_speed = format_speed(&observation.control_plane.vram_io, &vram_cell);
            let d_speed = format_speed(&observation.control_plane.disk_io, &disk);

            let format_rate = |io: &TierIoStats| {
                if sample_stale {
                    return STALE_SAMPLE_ROW.to_string();
                }
                if rate_windows == 0 {
                    // The peaks below only accumulate from an accepted window.
                    // Before the first one there is nothing to call idle — and
                    // a later refused window must not hide the peaks a previous
                    // one already produced, so this asks whether a window has
                    // ever been accepted, not whether this frame has a rate.
                    return "not measured".to_string();
                }
                // Directional peaks take every window; the busy trio only the
                // windows that reached the floor. A half-populated pair is not
                // a sample, so absence of either side is absence of the row.
                let (Some(read_peak), Some(write_peak)) = (io.read_peak_mbs, io.write_peak_mbs)
                else {
                    return "not measured".to_string();
                };
                let moved = io.max_mbs.is_some_and(|mbs| mbs >= BUSY_MOVED_MIN_MBS)
                    || read_peak >= MOVED_DIRECTIONAL_MIN_MBS
                    || write_peak >= MOVED_DIRECTIONAL_MIN_MBS;
                if !moved {
                    return "Idle (Awaiting Workload)".to_string();
                }
                // Burst and Avg are busy-band figures. A tier that moved but
                // never reached the floor has real directional peaks and no
                // busy statistic, so the clause is dropped rather than filled
                // with a zero nobody measured.
                // Every figure here is a real rate: `display_mbs` keeps a
                // sub-MiB one from publishing as the whole-MiB zero the
                // rounding would otherwise print.
                match (io.max_mbs, io.avg_mbs) {
                    (Some(burst), Some(avg)) => format!(
                        "Peak: ⬇️ {:>4} │ ⬆️ {:>4} │ Burst: {:>4} MB/s (Avg: {:>3})",
                        display_mbs(read_peak),
                        display_mbs(write_peak),
                        display_mbs(burst),
                        display_mbs(avg),
                    ),
                    _ => format!(
                        "Peak: ⬇️ {:>4} │ ⬆️ {:>4} MB/s",
                        display_mbs(read_peak),
                        display_mbs(write_peak),
                    ),
                }
            };
            let z_rate = format_rate(&observation.control_plane.zram_io);
            let v_rate = format_rate(&observation.control_plane.vram_io);
            let d_rate = format_rate(&observation.control_plane.disk_io);

            let format_vol = |rb: u64, wb: u64| {
                format!(
                    "⬇️ {} Read │ ⬆️ {} Written",
                    format_volume_bytes(rb),
                    format_volume_bytes(wb)
                )
            };
            let z_vol = format_vol(
                observation.control_plane.zram_io.read_bytes,
                observation.control_plane.zram_io.write_bytes,
            );

            let v_vol = format_vol(
                observation.control_plane.vram_io.read_bytes,
                observation.control_plane.vram_io.write_bytes,
            );
            let d_vol = format_vol(
                observation.control_plane.disk_io.read_bytes,
                observation.control_plane.disk_io.write_bytes,
            );

            // Every clause in these three headers is an observed fact or a
            // named absence. The device name comes from the monitor's own
            // swap-table read; the priority from the status document's fold of
            // the same table, so the header and the order check cannot
            // disagree about what the host is doing.
            let z_dev = tier_device_label(&observation.tier_devices.zram);
            let v_dev = tier_device_label(&observation.tier_devices.vram);
            let d_dev = tier_device_label(&observation.tier_devices.disk);
            let z_prio = tier_priority_label(zram.prio);
            let v_prio = if writeback_target && vram_cell.prio.is_none() {
                "writeback backing".to_string()
            } else {
                tier_priority_label(vram_cell.prio)
            };
            let d_prio = tier_priority_label(disk.prio);

            format!(
                concat!(
                    " ╔══ 📦 TIER 1: RAM Swap{z_dev} ── {z_prio} ── {zram_s} ── {z_auth}\n",
                    " ║   ├─ Memory Usage:       {z_use}\n",
                    " ║   ├─ Real-Time Speed:    {z_speed}\n",
                    " ║   ├─ Throughput Stats:   {z_rate}\n",
                    " ║   ├─ Lifetime Traffic:   {z_vol}\n",
                    " ║   ├─ I/O Latency:         {z_lat}\n",
                    " ║   └─ Speedup Factor:     {zram_speedup}\n",
                    " ╠{sep}\n",
                    " ║   🚀 TIER 2: {tier_name}{v_dev}{v_role} ── {v_prio} ── {vram_s} ── {v_auth}\n",
                    " ║   ├─ Memory Usage:       {v_use}\n",
                    " ║   ├─ Real-Time Speed:    {v_speed}\n",
                    " ║   ├─ Throughput Stats:   {v_rate}\n",
                    " ║   ├─ Lifetime Traffic:   {v_vol}\n",
                    " ║   ├─ I/O Latency:         {v_lat}\n",
                    " ║   └─ Speedup Factor:     {vram_speedup}\n",
                    " ╠{sep}\n",
                    " ║   💾 TIER 3: WSL2 System Disk{d_dev} ── {d_prio} ── {disk_s} ── {d_auth}\n",
                    " ║   ├─ Memory Usage:       {d_use}\n",
                    " ║   ├─ Real-Time Speed:    {d_speed}\n",
                    " ║   ├─ Throughput Stats:   {d_rate}\n",
                    " ║   ├─ Lifetime Traffic:   {d_vol}\n",
                    " ║   ├─ I/O Latency:         {d_lat}\n",
                    " ║   └─ Speedup Factor:     {disk_speedup}\n",
                    " ╚{sep}",
                ),
                z_auth = tier_authority_label("zram", zram.present, disk.present, "the SSD tier"),
                v_auth = v_auth,
                d_auth = tier_authority_label("disk", disk.present, None, "nothing below"),
                zram_s = zram_status,
                z_dev = z_dev,
                z_prio = z_prio,
                z_use = z_use,
                z_speed = z_speed,
                z_rate = z_rate,
                z_vol = z_vol,
                z_lat = z_lat,
                zram_speedup = zram_speedup,
                sep = sep,
                vram_s = vram_status,
                tier_name = tier_name,
                v_dev = v_dev,
                v_role = v_role,
                v_prio = v_prio,
                v_use = v_use,
                v_speed = v_speed,
                v_rate = v_rate,
                v_vol = v_vol,
                v_lat = v_lat,
                vram_speedup = vram_speedup,
                disk_s = disk_status,
                d_dev = d_dev,
                d_prio = d_prio,
                d_use = d_use,
                d_speed = d_speed,
                d_rate = d_rate,
                d_vol = d_vol,
                d_lat = d_lat,
                disk_speedup = disk_speedup,
            )
        })
        .unwrap_or_else(|| " Swap Tiers: not available".to_string());

    // Occupancy bars and lifetime totals are levels: they keep their
    // last-known value and the title carries the notice for the panel.
    let tiers_title = stale_panel_title("Memory Tiers (Swap Priority & Speedup)", sample_stale);
    frame.render_widget(
        Paragraph::new(tiers).block(Block::default().borders(Borders::ALL).title(tiers_title)),
        area,
    );
}
