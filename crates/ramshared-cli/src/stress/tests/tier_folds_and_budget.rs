// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/stress/tests/tier_folds_and_budget.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

#[test]
fn disk_throughput_uses_the_active_swap_device() {
    // Fixture names stand in for whatever block devices the host has. The
    // assertion is membership: only the device the swap table puts in the
    // fallback role is tier 3.
    let swaps = "Filename Type Size Used Priority\n/dev/fixture0 partition 4194304 1000 -2\n";
    let diskstats = "8 16 fixture0 2 0 8 0 3 0 16 0 0 0 0 0\n8 32 fixtureroot 200 0 800 0 300 0 1600 0 0 0 0 0\n";
    assert_eq!(
        tier_disk_bytes_from(diskstats, swaps, &std::collections::BTreeMap::new()).2,
        24 * 512
    );
    let (z, v, d) = tier_disk_bytes_from(diskstats, swaps, &std::collections::BTreeMap::new());
    assert_eq!((z, v, d), (0, 0, 24 * 512));
}

#[test]
fn a_clean_ring_buffer_counts_zero_hung_tasks() {
    // A buffer that was read and holds nothing is a measured zero, which
    // is what `Some(0)` will carry once the observation half succeeds.
    assert_eq!(count_hung_task_lines(""), 0);
    assert_eq!(
        count_hung_task_lines("init: starting\n[  0.0] foo bar\n"),
        0
    );
}

#[test]
fn hung_task_lines_count_only_hung_task_reports() {
    // The counter must not fire on ordinary "blocked" prose, so a
    // published `Some(n)` is always a hung-task figure and never a
    // side-effect of a buffer existing.
    assert_eq!(count_hung_task_lines("blocked for more than"), 1);
    assert_eq!(count_hung_task_lines("INFO: task foo:123 D state"), 0);
    assert_eq!(
        count_hung_task_lines("hung_task: blocked for more than 120 seconds\nok\n"),
        1
    );
}

#[test]
fn an_unobserved_hung_task_figure_publishes_absence_not_zero() {
    // The defect this closes: a `dmesg` that could not run used to land in
    // the report as a measured `0`. The field's face has to carry the
    // difference all the way to publication.
    let unobserved = StressReport {
        kernel_d_state_hung_tasks: None,
        ..StressReport::default()
    };
    let measured = StressReport {
        kernel_d_state_hung_tasks: Some(0),
        ..StressReport::default()
    };
    let unobserved_json = serde_json::to_value(&unobserved).expect("report serializes");
    let measured_json = serde_json::to_value(&measured).expect("report serializes");
    assert!(unobserved_json["kernel_d_state_hung_tasks"].is_null());
    assert_eq!(measured_json["kernel_d_state_hung_tasks"], 0);
    assert_ne!(
        unobserved_json["kernel_d_state_hung_tasks"],
        measured_json["kernel_d_state_hung_tasks"]
    );
}

#[test]
fn stress_tier_folds_share_the_role_definition_not_substring_matching() {
    // A path that merely *contains* a managed-tier token is not that tier.
    // Substring matching let a file called `nbd-notes` inflate Tier 2.
    let swaps = "\
Filename Type Size Used Priority
/swap/nbd-notes file 1024 100 -2
/swap/zram-backup file 1024 200 -2
/dev/fixture0 partition 1024 400 -2
/dev/ramshared0 partition 1024 50 100
/dev/nbd0 partition 1024 10 100
/dev/zram0 partition 1024 5 200
";
    let tiers = swap_tiers_from(swaps);
    assert_eq!(tiers.zram_kib, 5, "only the real zram device is Tier 1");
    assert_eq!(
        tiers.vram_kib,
        50 + 10,
        "the logical device and nbd are Tier 2; a lookalike path is not"
    );
    assert_eq!(
        tiers.ssd_kib,
        100 + 200 + 400,
        "lookalike paths and the real fallback are Tier 3"
    );
    assert_eq!(
        tiers.total_kib(),
        tiers.zram_kib + tiers.vram_kib + tiers.ssd_kib
    );
}

#[test]
fn swap_tier_folds_exclude_ghost_rows_from_live_usage() {
    // A ghost is a well-formed `/proc/swaps` row whose block device the
    // kernel already lost. The status snapshot drops those rows before
    // tier folding; the measurement fold must drop the same rows, or the
    // two published tier tables disagree about one host. Both kernel
    // spellings of the marker are covered, and a clean row beside a ghost
    // still contributes.
    let swaps = "\
Filename Type Size Used Priority
/dev/zram0 (deleted) partition 1024 900 200
/dev/zram1 partition 1024 5 200
/dev/nbd0\\040(deleted) partition 1024 800 100
/dev/nbd1 partition 1024 10 100
/dev/fixture0 (deleted) partition 1024 700 -2
/dev/fixture1 partition 1024 100 -2
";
    let tiers = swap_tiers_from(swaps);
    assert_eq!(
        tiers.zram_kib, 5,
        "the real-space zram ghost contributes no Tier 1 usage"
    );
    assert_eq!(
        tiers.vram_kib, 10,
        "the \\040(deleted) nbd ghost contributes no Tier 2 usage"
    );
    assert_eq!(
        tiers.ssd_kib, 100,
        "the fallback ghost contributes no Tier 3 usage"
    );
    assert_eq!(
        tiers.total_kib(),
        tiers.zram_kib + tiers.vram_kib + tiers.ssd_kib
    );
}

#[test]
fn swap_tier_folds_tolerate_duplicate_rows_unlike_the_safety_parser() {
    // The measurement fold is not a safety proof: a duplicate row is
    // counted twice here, because two active swaps really can share a
    // path spelling. Only the cascade's strict `parse_proc_swaps` rejects
    // duplicates, and that refusal is not this fold's job to copy.
    let swaps = "\
Filename Type Size Used Priority
/dev/zram0 partition 1024 5 200
/dev/zram0 partition 1024 7 200
";
    let tiers = swap_tiers_from(swaps);
    assert_eq!(
        tiers.zram_kib,
        5 + 7,
        "both rows are live usage; the measurement fold does not dedupe"
    );
}

#[test]
fn swap_tier_folds_with_only_ghost_rows_report_no_usage() {
    // Negative: a host whose only swap rows are ghosts is a host with no
    // resident swap usage, not a host whose usage failed to parse.
    let swaps = "\
Filename Type Size Used Priority
/dev/zram0 (deleted) partition 1024 900 200
/dev/nbd0\\040(deleted) partition 1024 800 100
";
    let tiers = swap_tiers_from(swaps);
    assert_eq!(tiers, SwapTiersKib::default());
}

/// The whole-MiB face is a publish conversion, and it must not spend a
/// real measurement's non-zero on a rounding. This is the one conversion
/// the report makes, so the boundary is pinned here rather than at each
/// call site.
#[test]
fn published_mib_never_turns_a_present_reading_into_a_measured_zero() {
    assert_eq!(published_mib(0), 0, "a measured zero stays a zero");
    assert_eq!(
        published_mib(1),
        1,
        "one KiB is one KiB of something, and publishes as such"
    );
    assert_eq!(published_mib(511), 1);
    assert_eq!(published_mib(1024), 1);
    assert_eq!(published_mib(1025), 2);
    // Round-to-nearest is the other collapse: it is what turned 1..=511 KiB
    // into a published `0` for as long as the fold used it.
    assert_ne!(
        published_mib(511),
        (511 + 512) / 1024,
        "the old round-to-nearest is exactly the collapse this replaces"
    );
}

/// The fill ratio refuses rather than fabricate, and the comparison stays
/// in the unit the samples carry.
///
/// The defect was `unwrap_or(0)` on a zero denominator: a run that never
/// read a cache document published `tier2_vram_pct: 0`, which reads as
/// "the cache was empty" when the harness never looked. Both refusals
/// publish `None`; a `Some(0)` is only ever a measured emptiness against a
/// target that exists.
#[test]
fn cache_fill_pct_refuses_an_empty_sample_set_and_a_missing_denominator() {
    // A real ratio, computed from the exact KiB figures.
    assert_eq!(cache_fill_pct(3, 512, 2048), Some(25));
    // A measured emptiness against a target that exists is `Some(0)`, not
    // the refusal: the cache was looked at and held nothing.
    assert_eq!(cache_fill_pct(3, 0, 2048), Some(0));

    // Refusal one: no sample was ever taken. Previously `Some(0)`.
    assert_eq!(
        cache_fill_pct(0, 0, 0),
        None,
        "an empty sample set has no ratio to publish"
    );
    assert_eq!(
        cache_fill_pct(0, 512, 2048),
        None,
        "peaks without a single sample behind them are not a measurement"
    );

    // Refusal two: samples, but no denominator. Previously `Some(0)`.
    assert_eq!(
        cache_fill_pct(3, 512, 0),
        None,
        "a ratio against a zero target has no denominator"
    );

    // The comparison is in KiB. A ratio of two whole-MiB faces is not the
    // ratio the document reported (512 / 2048 KiB is 25%; its MiB halves
    // would say 50%).
    assert_ne!(
        cache_fill_pct(3, 512, 2048),
        cache_fill_pct(3, published_mib(512), published_mib(2048)),
        "the fill ratio must not be taken between two rounded faces"
    );
}

#[test]
fn swap_tier_folds_keep_sub_mib_occupancy_and_round_only_at_publication() {
    // Positive: a tier holding less than one MiB still publishes as one.
    // Negative: the same figure through the old fold published `0`.
    let swaps = "\
Filename Type Size Used Priority
/dev/zram0 partition 1024 511 200
/dev/nbd0 partition 1024 1 100
/swap/fixture0 file 1024 1 -2
";
    let tiers = swap_tiers_from(swaps);
    assert_eq!(tiers.zram_kib, 511, "the fold holds the unit it read");
    assert_eq!(tiers.vram_kib, 1);
    assert_eq!(tiers.ssd_kib, 1);
    assert_eq!(tiers.zram_mb(), 1, "511 KiB is not zero MiB of swap");
    assert_eq!(tiers.vram_mb(), 1);
    assert_eq!(tiers.ssd_mb(), 1);
    assert_eq!(
        (511 + 512) / 1024,
        0,
        "the rounding this replaces published that same 511 KiB as zero"
    );
}

#[test]
fn swap_tier_total_is_the_rounding_of_the_sum_not_the_sum_of_roundings() {
    // Three half-megabyte occupancies are 1.5 MiB of swap, which publishes
    // as 2. Rounding each role first and then adding said 3, which is a
    // figure no reading of this host ever produced.
    let swaps = "\
Filename Type Size Used Priority
/dev/zram0 partition 1024 512 200
/dev/nbd0 partition 1024 512 100
/swap/fixture0 file 1024 512 -2
";
    let tiers = swap_tiers_from(swaps);
    assert_eq!(tiers.total_kib(), 1536);
    assert_eq!(tiers.total_mb(), 2);
    assert_eq!(
        published_mib(512) + published_mib(512) + published_mib(512),
        3,
        "summing the per-role faces is the arithmetic this total refuses"
    );
}

#[test]
fn tier_capacity_percent_is_the_ratio_of_the_kib_figures() {
    // 1536 of 2048 KiB is 75%. Its rounded MiB halves are 2 of 2, which is
    // 100%; the older round-to-nearest halves were 2 of 2 as well. Either
    // way, a percentage taken after rounding is not the one measured.
    let three_quarters = TierCapacityStats {
        total_kib: 2048,
        used_kib: 1536,
    };
    assert_eq!(three_quarters.pct(), Some(75));
    assert_eq!(
        (published_mib(three_quarters.used_kib) * 100) / published_mib(three_quarters.total_kib),
        100,
        "the ratio of the rounded faces is the lie this method refuses"
    );

    let idle = TierCapacityStats {
        total_kib: 2 * 1024 * 1024,
        used_kib: 0,
    };
    assert_eq!(
        idle.pct(),
        Some(0),
        "zero of a real denominator is a measured 0"
    );

    let no_device = TierCapacityStats::default();
    assert_eq!(
        no_device.pct(),
        None,
        "no denominator is a refusal, not a measured 0: a 0 here is how \
             an absent tier publishes as an empty one"
    );
}

/// Safety case for the published percent's refusal. The numerator and the
/// denominator are co-sampled; a peak sample with no swap device has no
/// ratio at all. The old fold answered a vanished denominator with `0` —
/// a real occupancy published as an empty tier.
#[test]
fn capacity_share_refuses_a_missing_denominator_instead_of_publishing_zero() {
    assert_eq!(
        capacity_share_pct(4 * 1024 * 1024, 0),
        None,
        "a peak that held 4 GiB against a capacity that is not there has \
             no percent; 0% would claim it was empty"
    );

    // Positive: the same peak against a capacity that exists is a number,
    // and the comparison is in KiB.
    assert_eq!(
        capacity_share_pct(4 * 1024 * 1024, 8 * 1024 * 1024),
        Some(50)
    );

    // A measured empty tier against a real denominator is `Some(0)`, and
    // that is a different claim from the refusal above.
    assert_eq!(capacity_share_pct(0, 8 * 1024 * 1024), Some(0));
    assert_ne!(
        capacity_share_pct(0, 8 * 1024 * 1024),
        capacity_share_pct(0, 0)
    );
}

/// Pairing property: the denominator of a
/// published tier percent is the capacity from the same sample the peak
/// was taken in. A later vanished or resized device is a different moment
/// and must not become that denominator.
#[test]
fn peak_swap_fold_keeps_the_denominator_the_peak_was_taken_against() {
    let mut peak = PeakSwapOccupancy::default();
    // The device held 4 GiB of an 8 GiB capacity at the peak sample.
    peak.fold(4 * 1024 * 1024, 8 * 1024 * 1024);
    assert_eq!(peak.pct(), Some(50));

    // Later samples are below the peak and may report a resized or gone
    // device. The pair stays the peak observation.
    peak.fold(0, 0);
    assert_eq!(
        peak.pct(),
        Some(50),
        "a device gone after the peak is not the denominator of it"
    );
    peak.fold(1024, 16 * 1024 * 1024);
    assert_eq!(
        peak.pct(),
        Some(50),
        "a later larger capacity is a different moment"
    );

    // A strictly higher peak takes its own capacity with it.
    peak.fold(6 * 1024 * 1024, 12 * 1024 * 1024);
    assert_eq!(peak.pct(), Some(50));
    assert_eq!(peak.peak_kib, Some(6 * 1024 * 1024));
    assert_eq!(peak.capacity_kib, 12 * 1024 * 1024);
}

/// A present zero stays a zero that was measured: an empty tier with a
/// real denominator publishes `Some(0)`, never the no-device refusal.
#[test]
fn peak_swap_fold_keeps_a_measured_empty_tier_as_a_zero_of_a_real_denominator() {
    let mut peak = PeakSwapOccupancy::default();
    peak.fold(0, 2 * 1024 * 1024);
    assert_eq!(peak.pct(), Some(0));
    // A later vanished device must not convert that measured zero into
    // absence.
    peak.fold(0, 0);
    assert_eq!(peak.pct(), Some(0));
}

/// Negative: a peak sample with no denominator publishes absence, never
/// `0%`. This is the already-absorbed consequence of verified test case, kept
/// pinned while the pairing itself is closed.
#[test]
fn peak_swap_fold_publishes_absence_when_the_peak_sample_had_no_denominator() {
    let mut peak = PeakSwapOccupancy::default();
    peak.fold(4 * 1024 * 1024, 0);
    assert_eq!(peak.pct(), None);
    assert_ne!(peak.pct(), Some(0));
}

#[test]
fn tier_disk_bytes_refuses_non_swap_disks_even_when_named_like_one() {
    // Negative: a disk that is not in the swap table is never tier 3, and
    // a diskstats row that merely looks like a managed tier does not
    // become one without being in the swap table as that role.
    let swaps = "Filename Type Size Used Priority\n/dev/fixture0 partition 4194304 1000 -2\n";
    let diskstats = "\
  8  16 fixture0 2 0 8 0 3 0 16 0 0 0 0 0
  8  32 fixtureroot 200 0 800 0 300 0 1600 0 0 0 0 0
252   0 zram0 1 0 1 0 1 0 1 0 0 0 0 0
 43   0 nbd0 1 0 1 0 1 0 1 0 0 0 0 0
";
    let (z, v, d) = tier_disk_bytes_from(diskstats, swaps, &std::collections::BTreeMap::new());
    assert_eq!(z, 2 * 512, "zram diskstats rows are Tier 1 by role");
    assert_eq!(v, 2 * 512, "nbd diskstats rows are Tier 2 by role");
    assert_eq!(
        d,
        24 * 512,
        "only the fallback-role swap device is Tier 3; the root disk is not"
    );
}

#[test]
fn tier_disk_bytes_resolves_the_device_mapper_alias_for_tier_attribution() {
    // `/proc/diskstats` names a device-mapper target `dm-N`; the swap table
    // may name the same device by its mapper alias. Both spellings must
    // land in the same tier or the row is silently dropped.
    //
    // Row fields: `… name reads_completed reads_merged sectors_read
    // ms_reading writes_completed writes_merged sectors_written …`.
    // The row below is 8 sectors read + 16 written = 24 * 512 bytes.
    let mut aliases = std::collections::BTreeMap::new();
    aliases.insert("dm-3".to_string(), "fixture0".to_string());
    let diskstats = "253   3 dm-3 2 0 8 0 3 0 16 0 0 0 0 0\n";
    let expected = 24 * 512;

    // Swap table says the alias; diskstats says the kernel name.
    let swaps =
        "Filename Type Size Used Priority\n/dev/mapper/fixture0 partition 4194304 1000 -2\n";
    let (z, v, d) = tier_disk_bytes_from(diskstats, swaps, &aliases);
    assert_eq!((z, v, d), (0, 0, expected));

    // And the reverse spelling: swap table says the kernel name.
    let swaps_by_kernel = "Filename Type Size Used Priority\n/dev/dm-3 partition 4194304 1000 -2\n";
    let (z, v, d) = tier_disk_bytes_from(diskstats, swaps_by_kernel, &aliases);
    assert_eq!((z, v, d), (0, 0, expected), "either spelling matches");
}

#[test]
fn tier_disk_bytes_omits_a_dm_row_with_no_recorded_alias() {
    // Negative: a `dm-N` row with no alias in the map is classified as
    // itself, falls to the fallback role, and is not in the membership
    // set — so it contributes nothing. Omission, never a guessed tier.
    //
    // The unresolved `dm-3` row is 8 + 16 = 24 sectors; the named
    // fallback row is 4 + 8 = 12. Only the second may accumulate.
    let swaps = "Filename Type Size Used Priority\n/dev/fixture0 partition 4194304 1000 -2\n";
    let diskstats = "\
 253   3 dm-3 2 0 8 0 3 0 16 0 0 0 0 0
   8  16 fixture0 1 0 4 0 1 0 8 0 0 0 0 0
";
    let (z, v, d) = tier_disk_bytes_from(diskstats, swaps, &std::collections::BTreeMap::new());
    assert_eq!(
        (z, v, d),
        (0, 0, 12 * 512),
        "an unresolved dm row is omitted; only the named fallback device counts"
    );
}

#[test]
fn fold_keeps_the_sample_that_held_the_most_uncompressed_data() {
    let small = ZramCompression {
        orig_data_bytes: 1_000,
        backed_data_bytes: Some(0),
        compr_data_bytes: 400,
        mem_used_bytes: 512,
        huge_pages: Some(0),
        same_pages: Some(0),
        compressor: None,
    };
    let saturated = ZramCompression {
        orig_data_bytes: 9_000,
        backed_data_bytes: Some(0),
        compr_data_bytes: 3_000,
        mem_used_bytes: 3_500,
        huge_pages: Some(8),
        same_pages: Some(5),
        compressor: None,
    };
    let mut peak = None;
    fold_peak_zram_compression(&mut peak, None);
    assert_eq!(peak, None, "an absent sample must not become a measurement");
    // The fold takes ownership of its sample, and `ZramCompression` is not
    // `Copy` — it carries a compressor name. Each later step still needs
    // the value, so the fold borrows a clone and the asserts keep theirs.
    fold_peak_zram_compression(&mut peak, Some(small.clone()));
    assert_eq!(peak, Some(small.clone()));
    fold_peak_zram_compression(&mut peak, Some(small.clone()));
    assert_eq!(
        peak,
        Some(small.clone()),
        "an equal sample does not displace the peak"
    );
    fold_peak_zram_compression(&mut peak, Some(saturated.clone()));
    assert_eq!(peak, Some(saturated.clone()));
    fold_peak_zram_compression(&mut peak, Some(small));
    assert_eq!(
        peak,
        Some(saturated),
        "a later smaller sample does not roll back"
    );
}

/// The residency sample is admitted by the cascade's document gate, not a
/// second one. These two arms are the clauses that used to be restated
/// here and got it half right: a schema the gate refuses, and a write time
/// in the reader's future. The gate allows no future skew at all — a
/// document written after the clock is not this reader's live document,
/// whatever the gap — while the hand-rolled copy here used to tolerate a
/// full second of it.
#[test]
fn cache_residency_admission_is_the_shared_document_gate() {
    let mut status = serde_json::json!({
        "schema_version": 1,
        "ok": true,
        "origin_state": "READY",
        "cache_state": "ACTIVE",
        "daemon_instance_id": "73692-345270",
        "written_at_unix_ms": 10000,
        "vram_cached_kib": 1048576,
        "cache_target_kib": 4194304,
        "logical_capacity_kib": 4194304
    });
    assert!(parse_cache_status_sample(&status, 11000, "73692-345270").is_some());

    // One millisecond into the reader's future. The old copy admitted
    // this; the gate does not.
    assert!(parse_cache_status_sample(&status, 9999, "73692-345270").is_none());

    // A schema the gate refuses is a foreign document, whatever else it
    // carries. The old copy had no schema check and would have reduced it.
    status["schema_version"] = serde_json::json!(2);
    assert!(parse_cache_status_sample(&status, 11000, "73692-345270").is_none());
    status["schema_version"] = serde_json::json!(1);
    status.as_object_mut().unwrap().remove("schema_version");
    assert!(parse_cache_status_sample(&status, 11000, "73692-345270").is_none());
}

#[test]
fn cache_residency_requires_fresh_matching_daemon_identity() {
    let status = serde_json::json!({
        "schema_version": 1,
        "ok": true,
        "origin_state": "READY",
        "cache_state": "ACTIVE",
        "daemon_instance_id": "73692-345270",
        "written_at_unix_ms": 10000,
        "vram_cached_kib": 1048576,
        "cache_target_kib": 4194304,
        "logical_capacity_kib": 4194304
    });
    let Some(sample) = parse_cache_status_sample(&status, 11000, "73692-345270") else {
        panic!("fresh physical cache sample");
    };
    assert_eq!(sample.cached_mib(), 1024);
    assert_eq!(sample.target_mib(), 4096);
    assert!(parse_cache_status_sample(&status, 15000, "73692-345270").is_some());
    // The production window is inclusive on its last millisecond.
    assert!(parse_cache_status_sample(&status, 25000, "73692-345270").is_some());
    assert!(parse_cache_status_sample(&status, 25001, "73692-345270").is_none());
    assert!(parse_cache_status_sample(&status, 26000, "73692-345270").is_none());
    assert!(parse_cache_status_sample(&status, 11000, "73692-foreign").is_none());
}

#[test]
// TestName: a_sub_mib_cache_never_reads_as_empty
fn a_sub_mib_cache_never_reads_as_empty() {
    // The wire carries KiB. Collapsing to whole MiB at construction made a
    // measured 512 KiB occupancy read as `0`, and a sub-MiB target read as
    // `0` too — which then failed `target > 0` and hid a non-empty cache
    // from the qualification entirely.
    let status = serde_json::json!({
        "schema_version": 1,
        "ok": true,
        "origin_state": "READY",
        "cache_state": "ACTIVE",
        "daemon_instance_id": "73692-345270",
        "written_at_unix_ms": 10000,
        "vram_cached_kib": 512,
        "cache_target_kib": 512,
        "logical_capacity_kib": 4096
    });
    let Some(sample) = parse_cache_status_sample(&status, 11000, "73692-345270") else {
        panic!("a sub-MiB cache is a measured sample, not an absent one");
    };
    // Measured exactly in the unit the document names.
    assert_eq!((sample.cached_kib, sample.target_kib), (512, 512));
    assert!(sample.at_target, "512 KiB at a 512 KiB target is at target");

    // The publish conversion is the only rounding, and it rounds up: a
    // non-zero measurement can never publish as zero.
    assert_eq!(sample.cached_mib(), 1);
    assert_eq!(sample.target_mib(), 1);
    assert_ne!(sample.cached_mib(), 0);
    assert_ne!(sample.target_mib(), 0);

    // Comparisons stay in KiB, so the sub-MiB target is visible. Under a
    // floor-to-MiB sample it read `0`, `target_mib > 0` failed, and the
    // snapshot dropped the cache on the floor.
    let mut opts = StressOptions {
        tier3_target_pct: Some(15),
        ..StressOptions::default()
    };
    assert!(
        full_tier_snapshot(Some(95), Some(95), Some(15), &opts, Some(sample)).is_some(),
        "a present sub-MiB cache must survive the full-tier snapshot"
    );
    // A whole-MiB request is larger than a 512 KiB target and must still
    // refuse — the comparison is exact, not rounded away.
    opts.physical_cache_target_mib = Some(1);
    assert!(full_tier_snapshot(Some(95), Some(95), Some(15), &opts, Some(sample)).is_none());
}

#[test]
fn full_tier_claim_requires_simultaneous_physical_cache() {
    let mut opts = StressOptions {
        tier3_target_pct: Some(15),
        ..StressOptions::default()
    };
    let full = Some(CacheSample {
        cached_kib: 1024 * 1024,
        target_kib: 1024 * 1024,
        at_target: true,
    });
    let partial = Some(CacheSample {
        cached_kib: 256 * 1024,
        target_kib: 1024 * 1024,
        at_target: false,
    });
    assert!(full_tier_snapshot(Some(95), Some(95), Some(15), &opts, full).is_some());
    assert!(full_tier_snapshot(Some(95), Some(95), Some(15), &opts, partial).is_none());
    assert!(full_tier_snapshot(Some(95), Some(95), Some(15), &opts, None).is_none());
    assert!(full_tier_snapshot(Some(95), Some(95), Some(14), &opts, full).is_none());
    assert!(full_tier_snapshot(Some(94), Some(95), Some(15), &opts, full).is_none());
    assert!(full_tier_snapshot(Some(95), Some(94), Some(15), &opts, full).is_none());
    opts.physical_cache_target_mib = Some(4096);
    assert!(full_tier_snapshot(Some(95), Some(95), Some(15), &opts, full).is_none());
    // A tier whose ratio refused is a missing tier, not an empty one at
    // 0%: it cannot satisfy a target, so the claim cannot stand. The old
    // `unwrap_or(0)` fold read `None` as `0%` and refused on the `0 <
    // target` comparison — right answer, wrong reason, and the same fold
    // would have accepted a target of 0.
    assert!(full_tier_snapshot(None, Some(95), Some(15), &opts, full).is_none());
    assert!(full_tier_snapshot(Some(95), None, Some(15), &opts, full).is_none());
    assert!(full_tier_snapshot(Some(95), Some(95), None, &opts, full).is_none());
}
