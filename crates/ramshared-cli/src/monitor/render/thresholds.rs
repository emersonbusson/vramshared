// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/render/thresholds.rs
//! Claim thresholds, bar geometry, and rate/latency formatting helpers.

use super::*;

/// Cell budget shared by the memory, GPU and tier bars.
///
/// The memory and GPU bars take it whole. The tier bar spends it
/// responsively — see [`BAR_WIDTH_DIVISOR`] and [`BAR_MIN_CELLS`] — and its
/// ceiling is this same budget, so a wide terminal never grows a longer bar
/// than the fixed panels.
pub(in crate::monitor) const BAR_MAX_CELLS: u64 = 20;

/// Terminal columns consumed by one cell of the responsive tier bar.
pub(in crate::monitor) const BAR_WIDTH_DIVISOR: u64 = 8;

/// Smallest responsive tier bar. A narrow terminal still gets a readable bar
/// rather than a one-cell stub.
pub(in crate::monitor) const BAR_MIN_CELLS: u64 = 8;

pub(in crate::monitor) fn make_bar(pct: u64, len: u64) -> String {
    bar_cells(pct.saturating_mul(len) / 100, pct > 0, len)
}

/// Draw `filled` cells of `len`, or one cell when the measurement is non-zero
/// and the share rounded down to nothing.
///
/// That bump is the whole reason this exists. A non-zero measurement must
/// never publish as an empty bar: an empty bar reads as idle, and a tier
/// holding one byte of a 10 GiB budget is not idle. `measured_nonzero` is the
/// caller's witness, not an inference from `filled` — a missing denominator
/// passes `false`, because there is no measurement to rescue and an empty bar
/// is then the blessed unmeasured shape.
fn bar_cells(filled: u64, measured_nonzero: bool, len: u64) -> String {
    let mut filled = filled.min(len);
    if filled == 0 && measured_nonzero {
        filled = 1;
    }
    let empty = len.saturating_sub(filled);
    format!(
        "[{}{}]",
        "█".repeat(filled as usize),
        "░".repeat(empty as usize)
    )
}

/// A fill bar from an exact `used` / `size` pair — the tier panel's cells and
/// the memory panel's two rows all draw through here.
///
/// The fill is a share of the exact figures, never of their whole-MiB faces,
/// so a sub-1% hold still draws one cell. A `size` of zero has no denominator,
/// so the bar draws empty — nothing is held against a capacity that does not
/// exist.
pub(in crate::monitor) fn make_tier_bar(used: u64, size: u64, len: u64) -> String {
    if size == 0 {
        return bar_cells(0, false, len);
    }
    bar_cells(used.saturating_mul(len) / size, used > 0, len)
}

/// A tier's fill percentage, or `None` when the tier has no capacity.
///
/// A percentage is a numerator over a denominator. With `size == 0` there is
/// no denominator, so there is no percentage: `used > 0` against a zero
/// capacity is an inconsistency, not a small fill. `Some(0)` is the measured
/// empty fill of a tier that has capacity and holds nothing.
pub(in crate::monitor) fn tier_fill_pct(used: u64, size: u64) -> Option<u64> {
    if size == 0 {
        return None;
    }
    Some(used.saturating_mul(100) / size)
}

/// How a tier's fill reads beside its bar.
///
/// `n/a` when [`tier_fill_pct`] found no denominator — the same honesty rule
/// [`speedup_label`] uses for a ratio over a missing baseline. `<1%` when a
/// real nonzero fill rounds down to zero at this width; that one is a
/// measurement, and printing `0%` there would claim the tier is empty.
pub(in crate::monitor) fn tier_fill_label(used: u64, pct: Option<u64>) -> String {
    match pct {
        None => "n/a".to_string(),
        Some(0) if used > 0 => "<1%".to_string(),
        Some(pct) => format!("{pct:>2}%"),
    }
}

/// A peak's share of the tier capacity, as the parenthetical on the row.
///
/// Same two honesty rules as [`tier_fill_label`]: no capacity, no percentage;
/// a nonzero share that rounds down to zero is `<1%`, not a measured `0%`.
pub(in crate::monitor) fn tier_peak_label(peak: u64, size: u64) -> String {
    match tier_fill_pct(peak, size) {
        None => "n/a".to_string(),
        Some(0) if peak > 0 => "(<1%)".to_string(),
        Some(pct) => format!("({pct:>3}%)"),
    }
}

/// How a within-budget **free figure** reads beside its row.
///
/// The number comes from [`GpuObservation::free_mib`], which **floors**,
/// because `free_bytes` is an allocation bound ("you may take at most this")
/// and rounding up would grant more than the exact remainder. That is the
/// opposite direction from the hold and capacity faces on the same row, and
/// the two must not share a helper.
///
/// A positive remainder that floors to zero is not a zero either: publishing
/// `0 MiB` would say the budget is exhausted when it still has a sub-MiB
/// remainder in it. So that case takes the third state, `<1 MiB` — true, and
/// granting nothing a reader could overdraw. A measured zero stays `0 MiB`.
pub(in crate::monitor) fn free_bound_label(free_bytes: u64, free_mib: u64) -> String {
    if free_mib > 0 {
        format!("{free_mib} MiB")
    } else if free_bytes > 0 {
        "<1 MiB".to_string()
    } else {
        "0 MiB".to_string()
    }
}

/// The Tier 1 compression row: how much data the compressed-RAM tier holds,
/// the size of its compressed form, and what that form actually costs the host.
///
/// The bar's full width is the uncompressed data and its filled part is the
/// **compressed form's** share of it, so a long bar with a short fill is
/// compression working. The empty remainder is the stored-page shrinkage,
/// `orig − compr`: how much smaller the stored pages are than the data they
/// hold. It is **not** what compression gave back to the host. That figure is
/// `orig − mem_used` and it is smaller, because the device's own bookkeeping
/// still occupies memory — it is the difference between the two labelled
/// figures on this row, never the bar.
///
/// The fill is **not** the cost. `mm_stat` reports three different byte
/// quantities and they are not interchangeable: `orig_data_size` is the
/// uncompressed data held, `compr_data_size` is the size of its compressed
/// form, and `mem_used_total` is the memory the device has really allocated
/// for that form — the operator's budget number, which is larger than
/// `compr_data_size` because the device's own bookkeeping sits on top. The
/// bar encodes `compr / orig`; the cost is printed as its own labelled figure
/// and never used as the fill, because conflating the two is how "compression
/// saved 3 GB" came out as "the data costs 1 GB" while the host paid 1.25 GB.
///
/// This is the only tier that compresses — Tier 2 stores raw pages 1:1 — so
/// the row is rendered here and nowhere else. The compressor is the kernel's
/// `comp_algorithm` answer, never the name the setup walk asked for.
///
/// The ratio is **derived** from the two byte counts this row already parsed,
/// through [`compression_ratio_of`] — the same derivation
/// [`ZramCompression::compression_ratio`](crate::zram_compression::ZramCompression)
/// applies at serialization time. It is deliberately not read back off the
/// wire: the published `ratio` is a derived convenience field, and a bar that
/// comes from the counts beside a number that comes from the field can
/// contradict itself on one line. The counts are the measurement; the ratio is
/// an arithmetic consequence of them and has no authority of its own.
///
/// That derivation is why the ratio slot alone follows [`sample_derived`].
/// A failed refresh freezes every number in the observation, and a ratio
/// computed from that freeze is not a live reading — the same treatment the
/// speedup factors and the latencies get on this panel. The bar and the
/// volume figures stay: they draw and print the two **level** counts, which
/// keep their last-known values and carry [`STALE_SAMPLE_NOTICE`] on the
/// panel title, exactly as [`make_tier_bar`] does for a used/total share. The
/// two refusal states also keep their own wording — absence is more specific
/// than a refresh failure and must not be rewritten into one, which is what
/// [`sample_derived`]'s own contract says about a formatter that owns an
/// explicit non-measurement.
///
/// Three honest states for the graphic, and no fourth:
/// - `not measured` when the kernel reported nothing, or reported a pair of
///   counts it cannot produce: no data held while a compressed form exists.
///   Every path that adds to `compr` also stores a page, and `orig` is that
///   stored-page count scaled by the page size, so the mirror pair is a tear
///   or corruption and has no reading. A zero-filled bar in its place would
///   read as "compression is perfect".
/// - `holding nothing` when the device is present and measured empty — both
///   counts present and zero. There is no ratio yet, and inventing `1.00:1`
///   would claim one.
/// - the graphic, when data is held. Either the compressed form holds
///   something, which makes a ratio derivable by construction, or it holds
///   nothing and the row names `all same-filled` in the ratio's place — still
///   the graphic, still one face.
///
/// A zero on one side is therefore two different facts, and which side it is
/// on decides which. `compr == 0` with data held is what `mm_stat` reports
/// when every stored page was **same-filled** — each page's elements all hold
/// one value, so the device stores a fill reference and allocates nothing.
/// Those pages count toward `orig` and never toward `compr`: the kernel
/// increments `orig_data_size` for every stored page and increments
/// `compr_data_size` only on the compressed path. This is not deduplication:
/// zram does not compare pages against each other, and two pages filled with
/// *different* values are both same-filled and not identical. That is the
/// most-compressed state the tier can be in, not a torn sample, and the row
/// draws it — but only while the `same_pages` counter does not contradict it.
/// A measured zero there says no page was same-filled while the bytes say
/// every one was, and a sample that disagrees with itself is `not measured`.
/// See [`all_stored_pages_same_filled`].
///
/// The two refusal states are different facts and must not share a face.
/// `holding nothing` is a **measurement of emptiness**: the device really is
/// holding zero, and that is worth seeing. `not measured` is the opposite
/// claim — the row has no figure it can stand behind — and it is what a
/// missing field and the impossible pair both get. Printing `holding
/// nothing` for `orig > 0, compr == 0` told the operator the device was
/// empty while the very counts that proved otherwise were thrown away on the
/// same line. That is the per-field collapse the honesty rule forbids: a
/// present measurement that does not fit the claim is not the same as no
/// measurement at all, and the row refuses the graphic rather than rewrite
/// what it saw. The same rule [`utilization_row`] applies to a driver sample
/// "a reader cannot use".
///
/// `cost`, the compressor name and the `huge_pages` count are each optional
/// on top of that: an unmeasured one is named as unreported beside the
/// graphic, never dropped and never zero. They do not gate the bar — a sample
/// missing only those counters is still a measurement. A corrupt or absent
/// wire `ratio` is not one of these states either: it is ignored, because the
/// row never trusted it in the first place.
pub(in crate::monitor) fn compression_row(
    sample: Option<&Value>,
    bar_len: u64,
    sample_stale: bool,
) -> String {
    let Some(sample) = sample.filter(|value| !value.is_null()) else {
        return "not measured".to_string();
    };
    let Some((orig, compr)) = resident_compression_from_status(sample) else {
        return "not measured".to_string();
    };
    // `compression_ratio_of` refuses any pair with a zero on either side,
    // because a ratio needs two non-zero counts. Which side is zero is a
    // different fact on each, and the row names them apart. See the state
    // list above. The two refusal states return before any staleness rule
    // touches them: they are the row's own non-measurement wordings, and
    // absence is more specific than a refresh failure.
    let ratio_text = match compression_ratio_of(orig, compr) {
        None if orig == 0
            && sample
                .get("backed_data_bytes")
                .and_then(Value::as_u64)
                .is_some_and(|bytes| bytes > 0) =>
        {
            return "no resident pages (backing storage in use)".to_string();
        }
        Some(ratio) => sample_derived(sample_stale, format!("{ratio:.2}:1")),
        None if orig == 0 && compr == 0 => {
            return "holding nothing (no ratio yet)".to_string();
        }
        None if compr == 0 => {
            // Data held, compressed form empty. Under the kernel's accounting
            // that is every stored page same-filled — `orig` counts them and
            // `compr` is only incremented on the compressed path — but a
            // `same_pages` counter that was measured and reads zero
            // contradicts it. A sample that disagrees with itself has no
            // classification; see `all_stored_pages_same_filled`.
            if !all_stored_pages_same_filled(
                orig,
                compr,
                sample.get("same_pages").and_then(Value::as_u64),
            ) {
                return "not measured".to_string();
            }
            // Still a derived classification of the pair, so it freezes with
            // the ratio it stands in for.
            sample_derived(sample_stale, "all same-filled".to_string())
        }
        None => return "not measured".to_string(),
    };

    // Filled = the compressed form's share of the uncompressed data. The
    // remainder of the bar is the stored-page shrinkage, `orig − compr` —
    // visible without reading the numbers. It is neither the cost nor the
    // host's saving: `mem_used` is the cost and `orig − mem_used` is the
    // saving, and the row prints the two labelled figures an operator
    // subtracts rather than encoding either as the bar.
    //
    // The bar keeps its shape across a refresh failure because it draws two
    // level counts, the way `make_tier_bar` draws a used/total share. The
    // ratio beside it is the derived figure and freezes; see `sample_derived`.
    let mut filled = (compr.saturating_mul(bar_len) / orig).min(bar_len);
    // A tiny but present compressed form must not draw as an empty bar. A
    // compressed form that really is empty draws as empty: padding it with a
    // block would claim a body the device does not hold, and this row never
    // draws a bar in a refusal state, so an empty bar here cannot be read as
    // "unmeasured".
    if filled == 0 && compr > 0 {
        filled = 1;
    }
    let empty = bar_len.saturating_sub(filled);
    let bar = format!(
        "[{}{}]",
        "█".repeat(filled as usize),
        "░".repeat(empty as usize)
    );

    // Optional counters, each named when unmeasured. A missing one is not a
    // zero and does not demote the graphic to `not measured`: the bar rests on
    // the two byte counts and the ratio, and these three describe them.
    let cost = sample
        .get("mem_used_bytes")
        .and_then(Value::as_u64)
        .map_or_else(
            || "cost unreported".to_string(),
            |bytes| format!("cost {}", format_volume_bytes(bytes)),
        );
    let compressor = sample
        .get("compressor")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .map_or_else(|| "compressor unreported".to_string(), str::to_string);
    // The wire field keeps the kernel's own column name (`huge_pages`); the
    // face names what that column counts. docs.kernel.org: "the number of
    // incompressible pages". It is not THP, so the operator-facing noun is
    // `incompressible` on both faces — a present count and an unreported one.
    let huge_pages = sample
        .get("huge_pages")
        .and_then(Value::as_u64)
        .map_or_else(
            || "incompressible unreported".to_string(),
            |count| format!("{count} incompressible"),
        );
    format!(
        "{bar} {} data → {}  ({ratio_text} · {cost} · {compressor} · {huge_pages})",
        format_volume_bytes(orig),
        format_volume_bytes(compr),
    )
}

/// The adapter busy-time rows: how busy the GPU was over the driver's trailing
/// sample window, and how busy its device-memory bus was.
///
/// These are **busy-time** percentages, not capacity. `gpu_pct` is the percent
/// of the window with at least one kernel executing; `memory_pct` is the
/// percent of the window the device-memory bus was busy. Neither may share a
/// meter with `vram_cached_kib` or `gpu_headroom_kib` — those answer "how much
/// VRAM is in use", these answer "how hard is the adapter working".
///
/// Two honest states, plus the staleness marker:
/// - `not measured` when the driver had no utilization source, refused the
///   call, or published something a reader cannot use. A zero-filled bar
///   here would read as an idle adapter the driver never reported.
/// - the graphic, for a real current sample — including a measured idle `0` / `0`,
///   which is a measurement and is drawn as such.
/// - `stale`, when a real reading is not current. Busy time is a rate over
///   the driver's trailing window, so a frozen percentage presented as live
///   would contradict the control panel's own `Read: stale` on the same
///   screen. A sample the driver never had stays `not measured` — absence is
///   the more specific truth and is never rewritten as `stale`.
///
/// The `gpu_utilization` field arrives here as the **record**, not as an
/// already-trusted figure. `build_cascade_snapshot` reduces it through
/// `utilization_record`, which keeps whatever the daemon published (absent,
/// `null` and unparsable still publish `null`) and never spends the absence
/// marker on a reading that merely aged out. Collapsing those two was the
/// defect: `null` means never measured, and erasing a real 47% sample into it
/// told the operator the driver never spoke when it spoke seconds ago.
///
/// So the trust decision is applied here, at the one place that has to decide
/// how the reading is shown, and it is split the way
/// [`GpuUtilizationTelemetry`] splits it:
/// - [`GpuUtilizationTelemetry::is_well_formed`] refuses the body — unknown
///   schema, or a percent outside its domain — and that refusal is `not
///   measured`, because the driver published nonsense rather than a reading.
/// - [`GpuUtilizationTelemetry::is_fresh_at`] refuses the age, and that
///   refusal is `stale`, because the reading exists and is simply not current.
///
/// Both staleness causes spell `stale` on this row — the figure is not current
/// either way — and stay apart one level up, where they already have distinct
/// names: `sample_refresh_failed` for this observation's own collection, and
/// `gpu_utilization_telemetry_invalid_or_stale` for a sample the daemon
/// published that cannot be trusted. The window is
/// [`GPU_BUDGET_MAX_AGE_MS`], the same one the budget sample uses at the same
/// heartbeat, so there is one definition of "live" for the pair.
///
/// The axis says "sample window", never "now": the driver reports
/// percent-of-time over its trailing window, not an instantaneous load. The
/// row count is fixed so the panel does not resize between states.
///
/// `now_unix_ms` is the frame's freshness clock — the caller passes the
/// observation's own [`Observation::epoch_ms`], never a wall-clock read at
/// draw time. See [`Observation::epoch_ms`] for why one frame carries one
/// clock.
/// Validated, freshness-checked GPU utilization state for monitor panels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::monitor) enum UtilizationStatus {
    NotMeasured,
    Stale,
    Measured(GpuUtilizationTelemetry),
}

/// Classifies `gpu_utilization` once for every monitor view: unreadable/null
/// yields [`UtilizationStatus::NotMeasured`], aged or refresh-failed yields
/// [`UtilizationStatus::Stale`], and only current well-formed telemetry returns
/// [`UtilizationStatus::Measured`].
pub(in crate::monitor) fn classify_utilization(
    sample: Option<&Value>,
    sample_stale: bool,
    now_unix_ms: u64,
) -> UtilizationStatus {
    let Some(sample) = sample.filter(|value| !value.is_null()) else {
        return UtilizationStatus::NotMeasured;
    };
    // A body a reader cannot use — unparsable, unknown schema, or a percent
    // outside its domain — is absence, not an aged reading. Refused rather
    // than clamped into a plausible figure, and checked before the freshness
    // gate so "the driver spoke nonsense" is never rewritten as "the driver
    // went quiet".
    let Ok(typed) = serde_json::from_value::<GpuUtilizationTelemetry>(sample.clone()) else {
        return UtilizationStatus::NotMeasured;
    };
    if !typed.is_well_formed() {
        return UtilizationStatus::NotMeasured;
    }
    // A real reading that is not current is `stale`, never `not measured`:
    // the record exists and this row only refuses to present it as live.
    if sample_stale || !typed.is_fresh_at(now_unix_ms, GPU_BUDGET_MAX_AGE_MS) {
        return UtilizationStatus::Stale;
    }
    UtilizationStatus::Measured(typed)
}

pub(in crate::monitor) fn utilization_row(
    sample: Option<&Value>,
    bar_len: u64,
    sample_stale: bool,
    now_unix_ms: u64,
) -> String {
    // Same bar rule as every other occupancy graphic: a non-zero busy
    // percentage that rounds down to zero cells at this width still draws one,
    // because an empty bar reads as idle. A measured `0` still draws empty.
    let pct_bar = |pct: u8| make_bar(u64::from(pct), bar_len);
    let not_measured =
        " GPU busy (sample window): not measured\n mem busy (sample window): not measured";
    let stale_row = " GPU busy (sample window): stale\n mem busy (sample window): stale";
    match classify_utilization(sample, sample_stale, now_unix_ms) {
        UtilizationStatus::NotMeasured => not_measured.to_string(),
        UtilizationStatus::Stale => stale_row.to_string(),
        UtilizationStatus::Measured(typed) => {
            let mut rows = format!(
                " GPU busy (sample window): {} {:>3}% kernels\n mem busy (sample window): {} {:>3}%",
                pct_bar(typed.gpu_pct),
                typed.gpu_pct,
                pct_bar(typed.memory_pct),
                typed.memory_pct,
            );
            for (engine, reading) in [("encode", typed.encoder_pct), ("decode", typed.decoder_pct)]
            {
                if let Some(pct) = reading {
                    rows.push_str(&format!("\n {engine} busy: {} {pct:>3}%", pct_bar(pct)));
                }
            }
            rows
        }
    }
}

/// Smallest fallback-disk throughput that can serve as a speedup baseline.
/// A ratio over a near-zero denominator is noise presented as a number.
///
/// This gates a **denominator**. It shares its value with
/// [`BUSY_WINDOW_MIN_MBS`] (a sample set) and [`BUSY_MOVED_MIN_MBS`] (a
/// display claim) and is deliberately not either of them: a baseline of
/// 5 MiB/s is usable, a window of 5 MiB/s is busy, and a published busy max
/// of 5 MiB/s shows movement — three decisions, three thresholds.
pub(in crate::monitor) const MIN_BASELINE_TIER_MBS: f64 = 5.0;

/// Published busy maximum at which the rate row may claim a tier moved on
/// the busy dimension.
///
/// This gates a **display claim** and it reads a published form rather than
/// an accumulator. A live accumulator only writes a busy max from windows at
/// or above [`BUSY_WINDOW_MIN_MBS`], so a wire value below this floor is not
/// one of those and must not open the Burst clause.
pub(in crate::monitor) const BUSY_MOVED_MIN_MBS: f64 = 5.0;

/// Directional peak at which the rate row may claim a tier moved in that
/// direction.
///
/// Independent of the busy dimension: a tier can carry 2 MiB/s in one
/// direction without ever reaching [`BUSY_WINDOW_MIN_MBS`] combined, and
/// that is still movement worth printing.
pub(in crate::monitor) const MOVED_DIRECTIONAL_MIN_MBS: f64 = 1.0;

/// Instantaneous rate at which a tier is treated as taking traffic.
///
/// At or above this the tier is paging; strictly below it the tier row reports
/// quiet, or retaining when it still holds MiB. One number and one partition:
/// [`tier_is_paging`] is the single predicate and the tier row's state word is
/// its negation, so the screen and the page-fault latency estimate cannot
/// disagree at the boundary. There is deliberately no second constant for the
/// quiet side — that was two names for one fact and left the boundary open.
pub(in crate::monitor) const PAGING_MIN_MBS: f64 = 0.1;

/// Instantaneous **swap-plane** rate below which the control row reports
/// standby.
///
/// A different plane from [`PAGING_MIN_MBS`], not the same fact under another
/// name: this reads the swap totals, not a per-tier rate. The swap row has no
/// "retaining" variant and its wording is pinned by tests. Strictly below, so
/// a swap plane sitting exactly at the number is Active, matching the
/// direction the tier rows take at the same rate.
pub(in crate::monitor) const SWAP_STANDBY_MAX_MBS: f64 = 0.1;

/// Ratio below which the speedup row prints `<0.1x`.
///
/// A real multiplier that rounds to zero at one decimal is below the display
/// resolution, the same honesty rule the occupancy percent uses for
/// `"<1%"`. This is a ratio, not a rate: it is not [`PAGING_MIN_MBS`] wearing
/// another name.
pub(in crate::monitor) const SPEEDUP_DISPLAY_MIN: f64 = 0.1;

/// Throughput below which [`display_mbs`] prints `<1`.
///
/// A real rate that rounds to zero in whole MiB/s is below the display
/// resolution — the same rule as [`SPEEDUP_DISPLAY_MIN`] at a different unit
/// and a different rounding.
pub(in crate::monitor) const MBPS_DISPLAY_MIN: f64 = 1.0;

/// Rate below which [`display_mbs_tenths`] prints `<0.1`.
///
/// The one-decimal MiB/s face's own resolution. It shares its **value** with
/// [`PAGING_MIN_MBS`] and with [`SPEEDUP_DISPLAY_MIN`] and shares nothing
/// else with either: that one decides whether a tier is paging, the ratio one
/// decides whether a multiplier can be printed, and this one decides whether
/// a rate can be printed. Three 0.1s, three policies.
pub(in crate::monitor) const RATE_TENTHS_DISPLAY_MIN: f64 = 0.1;

/// Duration below which [`display_ms`] prints `<1`.
pub(in crate::monitor) const MS_DISPLAY_MIN: f64 = 1.0;

/// Latency below which [`display_ms_hundredths`] prints `<0.01`.
pub(in crate::monitor) const MS_HUNDREDTHS_DISPLAY_MIN: f64 = 0.01;

/// Throughput below which [`display_gbs`] prints `<0.01`.
pub(in crate::monitor) const GBS_DISPLAY_MIN: f64 = 0.01;

/// Estimate below which [`display_pf_lat`] prints `<0.01`.
///
/// The estimate face's own resolution. It is not [`MS_HUNDREDTHS_DISPLAY_MIN`]
/// wearing another name: that one decides whether a millisecond figure can be
/// printed, this one decides whether a microsecond estimate can be.
pub(in crate::monitor) const US_HUNDREDTHS_DISPLAY_MIN: f64 = 0.01;

/// Duration below which [`display_seconds_hundredths`] prints `<0.01`.
pub(in crate::monitor) const S_HUNDREDTHS_DISPLAY_MIN: f64 = 0.01;

/// One ratio on the speedup row. `0.0` is never printed as `0.0x`: a real
/// ratio that rounds to zero at one decimal is below the display resolution,
/// the same honesty rule the occupancy percent uses for `"<1%"`.
pub(in crate::monitor) fn speedup_label(multiplier: f64) -> String {
    if multiplier <= 0.0 {
        "n/a".to_string()
    } else if multiplier < SPEEDUP_DISPLAY_MIN {
        "<0.1x".to_string()
    } else {
        format!("{multiplier:.1}x")
    }
}

/// One throughput figure on the baseline row, in whole MiB/s.
pub(in crate::monitor) fn display_mbs(value: f64) -> String {
    if value <= 0.0 {
        "0".to_string()
    } else if value < MBPS_DISPLAY_MIN {
        "<1".to_string()
    } else {
        format!("{value:.0}")
    }
}

/// One MiB/s figure at the one-decimal rate face's resolution.
///
/// `0.0` is the measured idle and is the only zero this face prints. A real
/// rate that rounds to `0.0` at one decimal is below the display resolution
/// and prints `<0.1` — the same honesty rule [`display_mbs`] applies at the
/// whole-MiB resolution and [`speedup_label`] applies to a ratio. Printing it
/// as `0.0` would dress a measurement in the measured-idle face.
pub(in crate::monitor) fn display_mbs_tenths(value: f64) -> String {
    if value <= 0.0 {
        "0.0".to_string()
    } else if value < RATE_TENTHS_DISPLAY_MIN {
        "<0.1".to_string()
    } else {
        format!("{value:.1}")
    }
}

/// One duration figure in whole milliseconds.
///
/// `0` is a measured zero. A real duration that rounds to `0` at this
/// resolution is below it and prints `<1` — never `0`, which would claim the
/// work took no time at all.
pub(in crate::monitor) fn display_ms(value: f64) -> String {
    if value <= 0.0 {
        "0".to_string()
    } else if value < MS_DISPLAY_MIN {
        "<1".to_string()
    } else {
        format!("{value:.0}")
    }
}

/// One latency figure in hundredths of a millisecond.
///
/// `0.00` is a measured zero. A real latency that rounds to `0.00` is below
/// the display resolution and prints `<0.01`.
pub(in crate::monitor) fn display_ms_hundredths(value: f64) -> String {
    if value <= 0.0 {
        "0.00".to_string()
    } else if value < MS_HUNDREDTHS_DISPLAY_MIN {
        "<0.01".to_string()
    } else {
        format!("{value:.2}")
    }
}

/// One throughput figure in hundredths of a GB/s.
///
/// `0.00` is a measured zero. A real speed that rounds to `0.00` is below the
/// display resolution and prints `<0.01`.
pub(in crate::monitor) fn display_gbs(value: f64) -> String {
    if value <= 0.0 {
        "0.00".to_string()
    } else if value < GBS_DISPLAY_MIN {
        "<0.01".to_string()
    } else {
        format!("{value:.2}")
    }
}

/// One duration figure in hundredths of a second.
///
/// `0.00` is a measured zero. A real duration that rounds to `0.00` s is
/// below the display resolution and prints `<0.01` — never a `0.00` that
/// would claim the work took no time at all. The unit activation the boot row
/// carries is one of these: a few milliseconds is a real activation and must
/// not print as `0.00s`.
pub(in crate::monitor) fn display_seconds_hundredths(value: f64) -> String {
    if value <= 0.0 {
        "0.00".to_string()
    } else if value < S_HUNDREDTHS_DISPLAY_MIN {
        "<0.01".to_string()
    } else {
        format!("{value:.2}")
    }
}

/// The page-fault latency estimate row, in microseconds.
///
/// `<= 0.0` is an estimate the model never produced and is **not** a measured
/// zero — it publishes `not measured`, the same absence face the peaks and the
/// boot row use. A positive estimate that rounds to `0.00` µs is below the
/// display resolution and publishes `<0.01`, never a `0.00` that would claim
/// the estimate found no latency at all.
///
/// The `estimated` label lives here because this is the one figure whose
/// number is a projection rather than a reading: the row must never present
/// it as a measured latency.
pub(in crate::monitor) fn display_pf_lat(value: f64) -> String {
    if value <= 0.0 {
        "not measured".to_string()
    } else if value < US_HUNDREDTHS_DISPLAY_MIN {
        "estimated <0.01 µs".to_string()
    } else {
        format!("estimated {value:.2} µs")
    }
}

/// Throughput of this tier as a multiple of the fallback disk tier measured
/// in the **same** observation.
///
/// The baseline is a measurement, never a constant. This used to divide every
/// tier by `ssd_baseline = 20.0` — a figure no observation produced — so each
/// multiplier was measured MiB/s over a guess, and the fallback row printed a
/// fixed `1.0x` that no sample supported. Comparison in one load snapshot is
/// the benchmark rule: two numbers taken at different moments are not a
/// comparison.
///
/// The denominator is the fallback tier's **average** throughput in this
/// snapshot, gated by [`MIN_BASELINE_TIER_MBS`]. `SwapTierRole::Fallback`
/// **is** the baseline: it reports its measured throughput, never a multiple
/// of itself.
///
/// Both sides are busy-band statistics. An empty busy set is an empty
/// statistic, so this reports absence instead of dividing by — or printing —
/// a zero that nobody measured.
pub(in crate::monitor) fn compute_tier_speedup(
    io: &TierIoStats,
    role: cascade::SwapTierRole,
    fallback: &TierIoStats,
) -> String {
    let (Some(min), Some(avg), Some(max)) = (io.min_mbs, io.avg_mbs, io.max_mbs) else {
        return "Awaiting measured I/O".to_string();
    };
    if role == cascade::SwapTierRole::Fallback {
        return format!(
            "🐢 baseline {}..{}..{} MiB/s (WSL2 fallback disk)",
            display_mbs(min),
            display_mbs(avg),
            display_mbs(max)
        );
    }
    let Some(baseline) = fallback.avg_mbs.filter(|mbs| *mbs >= MIN_BASELINE_TIER_MBS) else {
        return "not measured (no fallback disk baseline)".to_string();
    };
    let glyph = match role {
        cascade::SwapTierRole::Zram => "⚡",
        cascade::SwapTierRole::Vram => "🚀",
        cascade::SwapTierRole::Fallback => "🐢",
    };
    format!(
        "{glyph} Min: {} │ Avg: {} │ Max: {} (vs fallback disk)",
        speedup_label(min / baseline),
        speedup_label(avg / baseline),
        speedup_label(max / baseline),
    )
}

pub(in crate::monitor) fn format_tier_latency(io: &TierIoStats, suffix: &str) -> String {
    // One decision point for "not measured": either the tier never reported a
    // source, or it reported one with no usable sample. An earlier two-branch
    // version returned from here and then matched the same variant again; that
    // second arm was unreachable.
    let source = match io.latency_source {
        LatencySource::Estimated => "estimated",
        LatencySource::Measured => "measured",
        LatencySource::Unavailable => return format!("not measured ({suffix})"),
    };
    if io.min_lat_us <= 0.0 && io.avg_lat_us <= 0.0 && io.max_lat_us <= 0.0 {
        return format!("not measured ({suffix})");
    }

    let display = |value: f64| {
        if value <= 0.0 {
            "n/a".to_string()
        } else if value >= 1000.0 {
            format!("{:.1}ms", value / 1000.0)
        } else {
            format!("{value:.2}µs")
        }
    };
    format!(
        "{source}: {}..{}..{} ({suffix})",
        display(io.min_lat_us),
        display(io.avg_lat_us),
        display(io.max_lat_us),
    )
}
