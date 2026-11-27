// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/tests/cache_panel_and_latency_histograms.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;

#[test]
fn compression_row_never_reports_an_unmeasured_sample_as_a_zero() {
    // No sample, or an explicit null: the kernel reported nothing.
    assert_eq!(compression_row(None, 20, false), "not measured");
    assert_eq!(
        compression_row(Some(&Value::Null), 20, false),
        "not measured"
    );

    // A byte count the kernel did not report is not a zero. Same rule as
    // the memory-events counters: absence stays absent.
    let missing_field = serde_json::json!({
        "orig_data_bytes": 4096_u64,
        "backed_data_bytes": 0_u64,
        "ratio": 4.0,
    });
    assert_eq!(
        compression_row(Some(&missing_field), 20, false),
        "not measured"
    );

    let junk = serde_json::json!({
        "orig_data_bytes": "4096",
        "backed_data_bytes": 0_u64,
        "compr_data_bytes": 1024,
        "ratio": 4.0,
    });
    assert_eq!(compression_row(Some(&junk), 20, false), "not measured");
}

/// The printed ratio is an arithmetic consequence of the two byte counts
/// the bar already uses, never a second reading off the wire.
///
/// A derived field can disagree with the counts it is supposed to
/// describe. When it does, a bar from the counts beside a number from the
/// field contradicts itself on one line — and a corrupt derived field used
/// to be able to erase a complete measurement entirely.
#[test]
fn compression_row_derives_the_ratio_from_the_two_byte_counts() {
    // A document that lies about its own derived field still renders the
    // measurement the counts carry. The bar and the number agree, and the
    // lie is not echoed.
    let liar = serde_json::json!({
        "orig_data_bytes": 4096_u64,
        "backed_data_bytes": 0_u64,
        "compr_data_bytes": 1024_u64,
        "mem_used_bytes": 1200_u64,
        "ratio": 99.0,
    });
    let row = compression_row(Some(&liar), 20, false);
    assert!(
        row.contains("4.00:1"),
        "the ratio must come from 4096/1024, not from the wire: {row}"
    );
    assert!(!row.contains("99.00:1"), "a lie is never echoed: {row}");

    // The same is true when the derived field is unusable rather than
    // false. This used to demote a complete measurement to `not measured`.
    let broken_ratio = serde_json::json!({
        "orig_data_bytes": 4096_u64,
        "backed_data_bytes": 0_u64,
        "compr_data_bytes": 1024_u64,
        "mem_used_bytes": 1200_u64,
        "ratio": 0.0,
    });
    assert!(
        compression_row(Some(&broken_ratio), 20, false).contains("4.00:1"),
        "a corrupt derived field must not erase a complete measurement"
    );
    let missing_ratio = serde_json::json!({
        "orig_data_bytes": 4096_u64,
        "backed_data_bytes": 0_u64,
        "compr_data_bytes": 1024_u64,
        "mem_used_bytes": 1200_u64,
        "ratio": null,
    });
    assert!(
        compression_row(Some(&missing_ratio), 20, false).contains("4.00:1"),
        "an absent derived field is not an absent measurement"
    );

    // Agreed or not, the printed number is always orig/compr at the row's
    // own precision. 3000/1000 and 5000/1000 pin both a whole and a
    // fractional answer so a rounding drift cannot hide.
    for (orig, compr, want) in [
        (3000_u64, 1000_u64, "3.00:1"),
        (5000, 1000, "5.00:1"),
        (5, 2, "2.50:1"),
    ] {
        let sample = serde_json::json!({
            "orig_data_bytes": orig,
            "backed_data_bytes": 0_u64,
            "compr_data_bytes": compr,
            "mem_used_bytes": 1_u64,
        });
        assert!(
            compression_row(Some(&sample), 20, false).contains(want),
            "{orig}/{compr} must print {want}, got: {}",
            compression_row(Some(&sample), 20, false)
        );
    }

    // A ratio below 1:1 is a real measurement — the compressed form got
    // bigger — and is printed, never refused as if it were nonsense.
    let expanded = serde_json::json!({
        "orig_data_bytes": 100_u64,
        "backed_data_bytes": 0_u64,
        "compr_data_bytes": 120_u64,
        "mem_used_bytes": 130_u64,
    });
    assert!(
        compression_row(Some(&expanded), 20, false).contains("0.83:1"),
        "expansion is a measurement: {}",
        compression_row(Some(&expanded), 20, false)
    );
}

/// Emptiness, same-filled and an impossible pair are three different facts
/// and must not wear the same sentence.
///
/// `holding nothing` is a **measurement**: the device really is holding
/// zero, and the row says so. `all same-filled` is a measurement too —
/// data is held and every stored page was same-filled (each page's elements
/// all hold one value, so the device keeps a fill reference and allocates
/// no compressed body), which is the most compressed the tier can be and
/// has no finite ratio to draw. Same-filled is an *internal* property of
/// one page and is **not** deduplication against other pages: zram does not
/// compare pages against each other, so the face must never say
/// `identical` or `dedup`. Only the mirror pair cannot both be true: no
/// data held while a compressed form exists, because every path that adds
/// to `compr` also stores a page. That pair is the one that refuses the
/// graphic.
#[test]
fn compression_row_separates_emptiness_same_filled_and_an_impossible_pair() {
    // Both counts present and zero: the device is measured empty. This is
    // the one arm that may say "holding nothing".
    let empty = serde_json::json!({
        "orig_data_bytes": 0_u64,
        "backed_data_bytes": 0_u64,
        "compr_data_bytes": 0_u64,
    });
    assert_eq!(
        compression_row(Some(&empty), 20, false),
        "holding nothing (no ratio yet)"
    );

    // Data held, compressed form empty: what `mm_stat` reports for a
    // device whose stored pages are all same-filled. It is the best
    // compression there is, not a torn sample, so it draws and names the
    // state rather than claiming no measurement.
    let all_same_filled = serde_json::json!({
        "orig_data_bytes": 3000_u64,
        "backed_data_bytes": 0_u64,
        "compr_data_bytes": 0_u64,
        "mem_used_bytes": 12_u64,
    });
    let same_filled_row = compression_row(Some(&all_same_filled), 20, false);
    assert!(
        same_filled_row.contains("all same-filled"),
        "the same-filled state is named, not refused: {same_filled_row}"
    );
    assert!(
        !same_filled_row.contains("not measured"),
        "a measurement must not be published as an absence: {same_filled_row}"
    );
    // The false model is a published lie about the mechanism, so the face
    // refuses its vocabulary. zram never compares pages against each
    // other; two pages filled with *different* values both land here and
    // are not identical to each other.
    assert!(
        !same_filled_row.contains("identical"),
        "same-filled is not deduplication: {same_filled_row}"
    );
    assert!(
        !same_filled_row.contains("dedup"),
        "same-filled is not deduplication: {same_filled_row}"
    );
    // The compressed form is empty, so the bar draws empty. Padding it
    // with one block would claim a body the device does not hold, and
    // `compression_row` never draws a bar in a refusal state, so an empty
    // bar here cannot be read as "unmeasured".
    assert!(
        same_filled_row.contains(&format!("[{}]", "░".repeat(20))),
        "an empty compressed form draws an empty bar: {same_filled_row}"
    );

    // Nothing held, compressed form non-empty: the mirror image, and the
    // only pair the kernel cannot produce.
    let torn_orig = serde_json::json!({
        "orig_data_bytes": 0_u64,
        "backed_data_bytes": 0_u64,
        "compr_data_bytes": 1000_u64,
    });
    assert_eq!(compression_row(Some(&torn_orig), 20, false), "not measured");
}

/// The bytes say every stored page was same-filled; a `same_pages` counter
/// that was measured and reads zero says none was. A sample that disagrees
/// with itself supports no classification and publishes `not measured`
/// rather than a name for a state it did not measure.
#[test]
fn compression_row_refuses_the_same_filled_name_when_the_counter_contradicts_it() {
    let contradicted = serde_json::json!({
        "orig_data_bytes": 12288_u64,
        "backed_data_bytes": 0_u64,
        "compr_data_bytes": 0_u64,
        "mem_used_bytes": 4096_u64,
        "huge_pages": 0,
        "same_pages": 0,
        "compressor": "zstd",
    });
    assert_eq!(
        compression_row(Some(&contradicted), 20, false),
        "not measured",
        "a measured zero same-page count must refute the classification"
    );

    // A counter that measured pages, or one the kernel did not report at
    // all, leaves the byte pair's reading standing. `None` is "not
    // measured", never a measured zero.
    let corroborated = serde_json::json!({
        "orig_data_bytes": 12288_u64,
        "backed_data_bytes": 0_u64,
        "compr_data_bytes": 0_u64,
        "mem_used_bytes": 4096_u64,
        "same_pages": 3,
    });
    assert!(
        compression_row(Some(&corroborated), 20, false).contains("all same-filled"),
        "a measured non-zero count corroborates the bytes"
    );
    let unreported = serde_json::json!({
        "orig_data_bytes": 12288_u64,
        "backed_data_bytes": 0_u64,
        "compr_data_bytes": 0_u64,
        "mem_used_bytes": 4096_u64,
    });
    assert!(
        compression_row(Some(&unreported), 20, false).contains("all same-filled"),
        "an unreported counter is not a measured zero"
    );
}

/// A ratio computed from a frozen sample is not a live reading.
///
/// The ratio slot follows [`sample_derived`] the way the speedup factors
/// and the latencies do on the same panel. The bar and the volume figures
/// stay: they draw and print two **level** counts, which keep their
/// last-known values and carry the panel's `(⚠ Stale Sample)` title, the
/// same treatment `make_tier_bar` gets for a used/total share.
#[test]
fn compression_row_freezes_the_derived_ratio_and_keeps_the_level_figures() {
    let sample = serde_json::json!({
        "orig_data_bytes": 4_u64 * 1024 * 1024 * 1024,
        "backed_data_bytes": 0_u64,
        "compr_data_bytes": 1024_u64 * 1024 * 1024,
        "mem_used_bytes": 1342177280_u64,
        "huge_pages": 12,
        "compressor": "zstd",
    });
    let live = compression_row(Some(&sample), 20, false);
    let frozen = compression_row(Some(&sample), 20, true);

    assert!(live.contains("4.00:1"), "got: {live}");
    assert!(
        frozen.contains("stale"),
        "a ratio from a frozen sample must not read as live: {frozen}"
    );
    assert!(
        !frozen.contains("4.00:1"),
        "the frozen ratio must not survive beside the marker: {frozen}"
    );

    // Levels keep their last-known values; the panel title carries the
    // notice, so the row must not invent a second wording of it.
    assert!(frozen.contains("4.00 GB data"), "got: {frozen}");
    assert!(frozen.contains("1.00 GB"), "got: {frozen}");
    assert!(frozen.contains("cost  1.25 GB"), "got: {frozen}");
    assert!(frozen.contains("12 incompressible"), "got: {frozen}");
    assert!(frozen.contains("zstd"), "got: {frozen}");
    assert!(
        frozen.starts_with(&format!("[{}{}]", "█".repeat(5), "░".repeat(15))),
        "the bar draws two level counts and keeps its shape: {frozen}"
    );
}

/// The row's own absence wordings are more specific than a refresh
/// failure and must not be rewritten into one. Same contract
/// [`sample_derived`] states for a formatter that owns an explicit
/// non-measurement.
#[test]
fn compression_row_keeps_its_absence_wordings_when_the_sample_is_stale() {
    let missing = serde_json::json!({
        "orig_data_bytes": 3000_u64,
        "backed_data_bytes": 0_u64,
    });
    assert_eq!(
        compression_row(Some(&missing), 20, true),
        "not measured",
        "a field the kernel did not report is an absence, not a refresh failure"
    );

    let empty = serde_json::json!({
        "orig_data_bytes": 0_u64,
        "backed_data_bytes": 0_u64,
        "compr_data_bytes": 0_u64,
    });
    assert_eq!(
        compression_row(Some(&empty), 20, true),
        "holding nothing (no ratio yet)",
        "a measured empty device is a level state the freeze does not rewrite"
    );

    assert_eq!(compression_row(None, 20, true), "not measured");
}

/// `all same-filled` is a derived classification of the pair — it stands
/// in for a ratio that does not exist — so it freezes with the ratio slot
/// rather than being presented as a live reading.
#[test]
fn compression_row_freezes_the_same_filled_classification_with_the_ratio() {
    let sample = serde_json::json!({
        "orig_data_bytes": 3000_u64,
        "backed_data_bytes": 0_u64,
        "compr_data_bytes": 0_u64,
        "mem_used_bytes": 12_u64,
    });
    let frozen = compression_row(Some(&sample), 20, true);
    assert!(
        frozen.contains("stale"),
        "a derived classification must not read as live: {frozen}"
    );
    assert!(
        !frozen.contains("all same-filled"),
        "the classification freezes with the ratio it stands in for: {frozen}"
    );
}

/// An optional counter the row cannot read is named as unreported — never
/// dropped, never zero, and never allowed to demote a usable graphic.
///
/// `as_u64` and `as_str` already refuse the wrong JSON type; what was
/// missing was a pin that says so, so a future rewrite could not quietly
/// start coercing `"3"` into a count or `1.5` into `1`.
#[test]
fn compression_row_names_an_optional_counter_it_cannot_read() {
    let base = serde_json::json!({
        "orig_data_bytes": 4096_u64,
        "backed_data_bytes": 0_u64,
        "compr_data_bytes": 1024_u64,
        "mem_used_bytes": 1200_u64,
    });

    // `huge_pages` wants a count. A string, a fraction and a negative are
    // each not a count, and each is named as unreported rather than
    // coerced. The graphic still renders: these three never gate the bar.
    for (name, value) in [
        ("string", serde_json::json!("3")),
        ("fraction", serde_json::json!(1.5)),
        ("negative", serde_json::json!(-1)),
        ("object", serde_json::json!({"count": 3})),
    ] {
        let mut sample = base.clone();
        sample["huge_pages"] = value;
        let row = compression_row(Some(&sample), 20, false);
        assert!(
            row.contains("incompressible unreported"),
            "{name} huge_pages must be named unreported, got: {row}"
        );
        assert!(
            row.contains("4.00:1"),
            "{name} huge_pages must not demote the graphic: {row}"
        );
    }

    // A measured zero is a measurement and is printed as one. Collapsing
    // it into "unreported" is the same lie as the reverse.
    let mut measured_zero = base.clone();
    measured_zero["huge_pages"] = serde_json::json!(0_u64);
    let row = compression_row(Some(&measured_zero), 20, false);
    assert!(
        row.contains("0 incompressible"),
        "a measured zero is a count: {row}"
    );
    assert!(
        !row.contains("incompressible unreported"),
        "a measured zero is not an absence: {row}"
    );

    // `compressor` wants a name. A number and an object are not names.
    for (name, value) in [
        ("number", serde_json::json!(3)),
        ("object", serde_json::json!({"name": "lzo"})),
        ("array", serde_json::json!(["lzo"])),
    ] {
        let mut sample = base.clone();
        sample["compressor"] = value;
        let row = compression_row(Some(&sample), 20, false);
        assert!(
            row.contains("compressor unreported"),
            "{name} compressor must be named unreported, got: {row}"
        );
        assert!(
            row.contains("4.00:1"),
            "{name} compressor must not demote the graphic: {row}"
        );
    }
}

/// `same_pages` and `pages_compacted` are producer-side facts and are
/// deliberately not columns of this row. `same_pages`' saving is already
/// inside `orig - compr`, so printing it here would double-count;
/// `pages_compacted` is allocator health, not compression quality.
///
/// This fixture is a **ratio** row (a compressed body exists), so the
/// state word is not one of its faces and the guard is the stronger one:
/// the counter is not mentioned at all. The `all same-filled` face is
/// named by
/// `compression_row_separates_emptiness_same_filled_and_an_impossible_pair`
/// and carries no count there either — it names *why* there is no finite
/// ratio. What stays forbidden in both faces is a count column for the
/// counter.
#[test]
fn compression_row_never_prints_the_same_filled_or_compaction_counters() {
    let sample = serde_json::json!({
        "orig_data_bytes": 4096_u64,
        "backed_data_bytes": 0_u64,
        "compr_data_bytes": 1024_u64,
        "mem_used_bytes": 1200_u64,
        "huge_pages": 3,
        "same_pages": 17,
        "pages_compacted": 9,
        "ratio": 4.0,
    });
    let row = compression_row(Some(&sample), 20, false);
    assert!(
        !row.contains("same"),
        "same_pages is not a row column: {row}"
    );
    assert!(
        !row.contains("compact"),
        "pages_compacted is not a row column: {row}"
    );
    assert!(
        !row.contains("identical"),
        "the ratio face never carries the dedup vocabulary either: {row}"
    );
}

#[test]
fn compression_row_distinguishes_holding_nothing_from_not_measured() {
    // The device is present and measured empty. There is no ratio yet, and
    // printing "not measured" would claim the kernel reported nothing at
    // all — while printing `1.00:1` would invent a compression claim.
    let empty = serde_json::json!({
        "orig_data_bytes": 0_u64,
        "backed_data_bytes": 0_u64,
        "compr_data_bytes": 0_u64,
        "mem_used_bytes": 0_u64,
        "huge_pages": 0,
        "ratio": null,
    });
    assert_eq!(
        compression_row(Some(&empty), 20, false),
        "holding nothing (no ratio yet)"
    );
}

/// A missing `huge_pages` column (the kernel's name for the incompressible
/// count) publishes its absence under the same noun a present count uses,
/// never as a fabricated `0`.
#[test]
fn compression_row_reports_an_unavailable_huge_pages_counter_as_unreported() {
    let sample = serde_json::json!({
        "orig_data_bytes": 3000_u64,
        "backed_data_bytes": 0_u64,
        "compr_data_bytes": 1000_u64,
        "mem_used_bytes": 1200_u64,
        "huge_pages": null,
        "ratio": 3.0,
    });
    let row = compression_row(Some(&sample), 20, false);
    assert!(row.contains("incompressible unreported"), "got: {row}");
    assert!(!row.contains("0 incompressible"), "got: {row}");
}

#[test]
fn utilization_row_shows_busy_time_as_a_graphic_and_names_the_sample_window() {
    let sample = serde_json::json!({
        "schema_version": 1,
        "gpu_pct": 15,
        "memory_pct": 62,
        "sampled_at_unix_ms": 1_000,
    });
    let row = utilization_row(Some(&sample), 20, false, 1_000);
    // 15% of 20 is 3 filled cells; 62% is 12.
    assert!(
        row.contains(&format!("[{}{}]", "█".repeat(3), "░".repeat(17))),
        "15% busy must fill 3 of 20 cells, got: {row}"
    );
    assert!(
        row.contains(&format!("[{}{}]", "█".repeat(12), "░".repeat(8))),
        "62% busy must fill 12 of 20 cells, got: {row}"
    );
    assert!(row.contains("15%"), "got: {row}");
    assert!(row.contains("62%"), "got: {row}");
    // The axis is the driver's trailing sample window, never an
    // instantaneous "now" the measurement does not support.
    assert!(row.contains("GPU busy (sample window)"), "got: {row}");
    assert!(row.contains("mem busy (sample window)"), "got: {row}");
    assert!(!row.contains("now"), "got: {row}");
}

/// The bar rule every other occupancy graphic owns: a non-zero busy
/// percentage that rounds down to zero cells at this width still draws
/// one. `pct_bar` used to drop it, so 3% busy printed an empty bar.
#[test]
fn utilization_row_draws_one_cell_for_a_busy_percent_that_rounds_down() {
    let sample = serde_json::json!({
        "schema_version": 1,
        "gpu_pct": 3,
        "memory_pct": 0,
        "sampled_at_unix_ms": 1_000,
    });
    let row = utilization_row(Some(&sample), 20, false, 1_000);
    // 3% of 20 cells is none; the measurement is not.
    assert!(
        row.contains(&format!("[{}{}]", "█", "░".repeat(19))),
        "3% busy must draw one of 20 cells, got: {row}"
    );
    // A measured idle on the other plane is still an empty bar — the bump
    // is for a non-zero measurement, not for every row.
    assert!(
        row.contains(&format!("[{}]", "░".repeat(20))),
        "0% busy is a measured empty bar, got: {row}"
    );
    assert!(row.contains("  3%"), "got: {row}");
    // The idle plane's `0` is a measurement, and the `{:>3}` width is
    // asserted whole so a `10%` can never stand in for it.
    assert!(row.contains("  0%"), "got: {row}");
}

#[test]
fn utilization_row_distinguishes_measured_idle_from_not_measured() {
    // Measured idle is a real sample: the driver answered `0` / `0`, and
    // the bars are drawn empty because that is the measurement.
    let idle = serde_json::json!({
        "schema_version": 1,
        "gpu_pct": 0,
        "memory_pct": 0,
        "sampled_at_unix_ms": 1_000,
    });
    let row = utilization_row(Some(&idle), 20, false, 1_000);
    assert!(
        row.contains("0%"),
        "a measured idle sample must show 0%: {row}"
    );
    assert!(
        row.contains(&format!("[{}]", "░".repeat(20))),
        "a measured idle sample draws an empty bar, not a missing one: {row}"
    );
    assert!(!row.contains("not measured"), "got: {row}");
}

#[test]
fn utilization_row_never_reports_an_unmeasured_sample_as_idle() {
    let unmeasured =
        " GPU busy (sample window): not measured\n mem busy (sample window): not measured";
    assert_eq!(utilization_row(None, 20, false, 1_000), unmeasured);
    assert_eq!(
        utilization_row(Some(&Value::Null), 20, false, 1_000),
        unmeasured
    );

    // A percent the driver did not report is not a zero.
    let missing_field = serde_json::json!({ "gpu_pct": 15 });
    assert_eq!(
        utilization_row(Some(&missing_field), 20, false, 1_000),
        unmeasured
    );

    // Out of range is refused, never clamped into a plausible figure. The
    // fixture is otherwise complete so the refusal is the body check and
    // not a parse failure.
    let out_of_range = serde_json::json!({
        "schema_version": 1,
        "gpu_pct": 101,
        "memory_pct": 82,
        "sampled_at_unix_ms": 1_000,
    });
    assert_eq!(
        utilization_row(Some(&out_of_range), 20, false, 1_000),
        unmeasured
    );

    // A percent that is not an integer is a malformed sample.
    let junk = serde_json::json!({
        "schema_version": 1,
        "gpu_pct": "15",
        "memory_pct": 82,
        "sampled_at_unix_ms": 1_000,
    });
    assert_eq!(utilization_row(Some(&junk), 20, false, 1_000), unmeasured);
}

#[test]
fn utilization_row_keeps_an_absent_source_when_the_sample_is_stale() {
    // A refresh failure is temporal; a driver with no utilization source is
    // not. The permanent absence is the more specific truth and must not be
    // rewritten into a staleness the reader would expect to clear.
    let unmeasured =
        " GPU busy (sample window): not measured\n mem busy (sample window): not measured";
    assert_eq!(utilization_row(None, 20, true, 1_000), unmeasured);
    let missing_field = serde_json::json!({ "gpu_pct": 15 });
    assert_eq!(
        utilization_row(Some(&missing_field), 20, true, 1_000),
        unmeasured
    );

    // A real sample that did not refresh is not presented as live, and the
    // two rows stay two rows so the panel does not resize between states.
    // The sample is otherwise complete and fresh so the only refusal is
    // the refresh.
    let sample = serde_json::json!({
        "schema_version": 1,
        "gpu_pct": 15,
        "memory_pct": 62,
        "sampled_at_unix_ms": 1_000,
    });
    let stale = utilization_row(Some(&sample), 20, true, 1_000);
    assert_eq!(
        stale,
        " GPU busy (sample window): stale\n mem busy (sample window): stale"
    );
    assert!(
        !stale.contains("15%"),
        "a frozen busy percentage must not read as live: {stale}"
    );
}

#[test]
fn utilization_row_marks_an_aged_reading_stale_instead_of_erasing_it() {
    // A reading that exists but is no longer current is not absence. The
    // composer keeps the record; this row is what refuses to present it as
    // live. Erasing it to `not measured` first told the operator the driver
    // never spoke when it spoke seconds ago.
    let aged = serde_json::json!({
        "schema_version": 1,
        "gpu_pct": 47,
        "memory_pct": 82,
        "sampled_at_unix_ms": 1_000,
    });
    let row = utilization_row(Some(&aged), 20, false, 1_000 + GPU_BUDGET_MAX_AGE_MS + 1);
    assert_eq!(
        row,
        " GPU busy (sample window): stale\n mem busy (sample window): stale"
    );
    assert!(
        !row.contains("47%"),
        "an aged busy percentage must not read as live: {row}"
    );
    assert!(
        !row.contains("not measured"),
        "an aged reading is a reading, not an absence: {row}"
    );

    // The last instant inside the window is still live, so the gate is the
    // window and not a boolean the caller fudges.
    let edge = utilization_row(Some(&aged), 20, false, 1_000 + GPU_BUDGET_MAX_AGE_MS);
    assert!(edge.contains("47%"), "got: {edge}");
}

#[test]
fn compression_row_reports_only_resident_data_during_writeback() {
    let sample = serde_json::json!({
        "orig_data_bytes": 774696960_u64,
        "backed_data_bytes": 729382912_u64,
        "compr_data_bytes": 11820671_u64,
        "mem_used_bytes": 15101952_u64,
        "ratio": 65.537,
    });
    let row = compression_row(Some(&sample), 20, false);
    assert!(row.contains("3.83:1"), "{row}");
    assert!(!row.contains("65.54:1"), "{row}");
    assert_eq!(compression_saved_pct(Some(&sample)), Some(73));

    let mut legacy = sample.clone();
    legacy
        .as_object_mut()
        .expect("compression object")
        .remove("backed_data_bytes");
    assert_eq!(compression_row(Some(&legacy), 20, false), "not measured");
    assert_eq!(compression_saved_pct(Some(&legacy)), None);

    let written_back = serde_json::json!({
        "orig_data_bytes": 8192_u64,
        "backed_data_bytes": 8192_u64,
        "compr_data_bytes": 0,
        "same_pages": 0,
    });
    assert_eq!(
        compression_row(Some(&written_back), 20, false),
        "no resident pages (backing storage in use)"
    );
}
