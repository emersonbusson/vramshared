// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/zram_compression/tests.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::time::{SystemTime, UNIX_EPOCH};

use super::*;

#[test]
fn zram_mm_stat_parses_the_kernel_columns_and_refuses_junk() {
    // The live shape a populated device reports: nine columns, of which
    // the first three are the ratio's inputs, the sixth is same_pages and
    // the eighth is huge_pages.
    assert_eq!(
        parse_zram_mm_stat("30912512 10297603 11972608 0 11972608 528 0 169 169"),
        Some(ZramCompression {
            orig_data_bytes: 30_912_512,
            compr_data_bytes: 10_297_603,
            mem_used_bytes: 11_972_608,
            huge_pages: Some(169),
            same_pages: Some(528),
            compressor: None,
        })
    );
    // Trailing columns are tolerated; a kernel that adds one still parses.
    assert_eq!(
        parse_zram_mm_stat("3000 1000 1200 0 1200 0 0 2 4 8"),
        Some(ZramCompression {
            orig_data_bytes: 3000,
            compr_data_bytes: 1000,
            mem_used_bytes: 1200,
            huge_pages: Some(2),
            same_pages: Some(0),
            compressor: None,
        })
    );
    // The counter columns are newer than the three the ratio needs. Their
    // absence must not cost us the ratio, and it must not invent a count:
    // `None` is "the kernel did not report it", not "the kernel reported 0".
    assert_eq!(
        parse_zram_mm_stat("3000 1000 1200 0 1200 0 0"),
        Some(ZramCompression {
            orig_data_bytes: 3000,
            compr_data_bytes: 1000,
            mem_used_bytes: 1200,
            huge_pages: None,
            same_pages: Some(0),
            compressor: None,
        })
    );
    // A line that stops before `same_pages` leaves both counters absent.
    assert_eq!(
        parse_zram_mm_stat("3000 1000 1200 0 1200"),
        Some(ZramCompression {
            orig_data_bytes: 3000,
            compr_data_bytes: 1000,
            mem_used_bytes: 1200,
            huge_pages: None,
            same_pages: None,
            compressor: None,
        })
    );
    // A measured zero is a different fact from an absent column.
    assert_eq!(
        parse_zram_mm_stat("3000 1000 1200 0 1200 0 0 0 0"),
        Some(ZramCompression {
            orig_data_bytes: 3000,
            compr_data_bytes: 1000,
            mem_used_bytes: 1200,
            huge_pages: Some(0),
            same_pages: Some(0),
            compressor: None,
        })
    );
    // A counter column that does not parse is an absent counter, not a
    // zero and not a reason to drop the ratio the leading columns still
    // support. Each counter demotes independently.
    assert_eq!(
        parse_zram_mm_stat("3000 1000 1200 0 1200 abc 0 2 2"),
        Some(ZramCompression {
            orig_data_bytes: 3000,
            compr_data_bytes: 1000,
            mem_used_bytes: 1200,
            huge_pages: Some(2),
            same_pages: None,
            compressor: None,
        })
    );
    assert_eq!(
        parse_zram_mm_stat("3000 1000 1200 0 1200 2 0 abc 2"),
        Some(ZramCompression {
            orig_data_bytes: 3000,
            compr_data_bytes: 1000,
            mem_used_bytes: 1200,
            huge_pages: None,
            same_pages: Some(2),
            compressor: None,
        })
    );
    // A negative or a float in the counter column is the same absence.
    assert_eq!(
        parse_zram_mm_stat("3000 1000 1200 0 1200 -1 0 2.5 2")
            .map(|sample| (sample.same_pages, sample.huge_pages)),
        Some((None, None))
    );
    assert_eq!(parse_zram_mm_stat(""), None);
    assert_eq!(parse_zram_mm_stat("not numbers"), None);
    assert_eq!(parse_zram_mm_stat("3000 1000"), None);
}

#[test]
fn zram_compression_counts_the_same_devices_the_tier_table_does() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-sys-block-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let zram0 = root.join("zram0");
    let zram1 = root.join("zram1");
    fs::create_dir_all(&zram0).expect("fixture dir");
    fs::create_dir_all(&zram1).expect("fixture dir");
    fs::write(zram0.join("mm_stat"), "3000 1000 1200 0 1200 7 0 2 2\n").expect("fixture mm_stat");
    fs::write(zram0.join("comp_algorithm"), "lzo-rle [zstd] lz4\n")
        .expect("fixture comp_algorithm");
    // zram1's kernel omits the huge_pages column entirely and names the
    // same compressor. An absent `comp_algorithm` is a different fact.
    fs::write(zram1.join("mm_stat"), "900 300 400 0 400 0 0\n").expect("fixture mm_stat");
    fs::write(zram1.join("comp_algorithm"), "lzo-rle [zstd] lz4\n")
        .expect("fixture comp_algorithm");
    // The nbd and host-fallback rows are other tiers; they must not
    // contribute, even though they have directories under the same root.
    fs::create_dir_all(root.join("nbd0")).expect("fixture dir");
    fs::write(root.join("nbd0").join("mm_stat"), "1 1 1 0 1 0 0 0 0\n").expect("fixture mm_stat");
    fs::write(root.join("nbd0").join("comp_algorithm"), "[lzo]\n").expect("fixture comp_algorithm");

    let swaps = "Filename Type Size Used Priority\n/dev/zram0 partition 2097148 1024 200\n/dev/nbd0 partition 4194300 0 100\n/dev/zram1 partition 1024 64 200\n/dev/sda3 partition 8388608 0 -2\n";
    assert_eq!(
        zram_swap_devices(swaps),
        BTreeSet::from(["zram0".to_string(), "zram1".to_string()])
    );
    // zram0's `huge_pages` column read 2 (two incompressible pages) and
    // its `same_pages` read 7. zram1's kernel stops before the
    // `huge_pages` column but does report `same_pages`, and that measured
    // zero belongs to the tier. So `same_pages` is the complete sum 7 + 0,
    // while `huge_pages` would cover only one of the two counted devices
    // and must publish `None` rather than zram0's 2 as if it described the
    // whole tier. Both Tier 1 devices name zstd; nbd0's `lzo` is another
    // tier and must not leak in.
    assert_eq!(
        zram_compression_from(&root, swaps),
        Some(ZramCompression {
            orig_data_bytes: 3900,
            compr_data_bytes: 1300,
            mem_used_bytes: 1600,
            huge_pages: None,
            same_pages: Some(7),
            compressor: Some("zstd".to_string()),
        })
    );

    // A device the tier table names but whose `mm_stat` is gone has not
    // answered. The survivors' bytes are not the tier total — see
    // `a_device_the_tier_table_names_but_that_does_not_answer_makes_the_tier_total_unmeasurable`.
    let _ = fs::remove_file(zram1.join("mm_stat"));
    assert_eq!(zram_compression_from(&root, swaps), None);
    let _ = fs::remove_file(zram0.join("mm_stat"));
    assert_eq!(zram_compression_from(&root, swaps), None);
    assert_eq!(
        zram_compression_from(&root, "Filename Type Size Used Priority\n"),
        None
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn zram_swap_devices_never_yields_a_ghost_basename() {
    // A ghost row is classified as Tier 1 on purpose — the role predicates
    // strip ` (deleted)` to find the device class — but its basename is
    // not a name `/sys/block` publishes. The reader must not carry it
    // forward as a lookup key, in either kernel spelling. A clean Tier 1
    // row beside a ghost is still yielded.
    let swaps = "\
Filename Type Size Used Priority
/dev/zram0 (deleted) partition 1024 900 200
/dev/zram1\\040(deleted) partition 1024 800 200
/dev/zram2 partition 1024 5 200
/dev/nbd0 partition 1024 1 100
/dev/fixture0 (deleted) partition 1024 2 -2
";
    assert_eq!(
        zram_swap_devices(swaps),
        BTreeSet::from(["zram2".to_string()])
    );

    // Negative: only ghosts means no Tier 1 devices at all, which the fold
    // publishes as "not measured" rather than as an empty sum.
    let only_ghosts = "\
Filename Type Size Used Priority
/dev/zram0 (deleted) partition 1024 900 200
/dev/zram1\\040(deleted) partition 1024 800 200
";
    assert!(zram_swap_devices(only_ghosts).is_empty());
}

#[test]
fn zram_compression_counts_a_repeated_basename_once() {
    // Two `/proc/swaps` rows can name one device: the same path listed
    // twice, or two spellings that share a basename (`/dev/zram0` beside
    // the bare `zram0` the path predicates also accept). The fold reads
    // `mm_stat` through the basename, so both rows are one device — and
    // summing each row would double every published byte figure while the
    // ratio still looked plausible, because both sides of it double
    // together.
    let root = std::env::temp_dir().join(format!(
        "ramshared-sys-block-dedup-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let zram0 = root.join("zram0");
    fs::create_dir_all(&zram0).expect("fixture dir");
    fs::write(zram0.join("mm_stat"), "3000 1000 1200 0 1200 7 0 2 2\n").expect("fixture mm_stat");
    fs::write(zram0.join("comp_algorithm"), "lzo-rle [zstd] lz4\n")
        .expect("fixture comp_algorithm");

    let once = "Filename Type Size Used Priority\n/dev/zram0 partition 2097148 1024 200\n";
    let expected = ZramCompression {
        orig_data_bytes: 3000,
        compr_data_bytes: 1000,
        mem_used_bytes: 1200,
        huge_pages: Some(2),
        same_pages: Some(7),
        compressor: Some("zstd".to_string()),
    };
    assert_eq!(zram_compression_from(&root, once), Some(expected.clone()));

    for repeated in [
        // The identical row twice.
        "Filename Type Size Used Priority\n\
         /dev/zram0 partition 2097148 1024 200\n\
         /dev/zram0 partition 2097148 1024 200\n",
        // Two spellings of one basename.
        "Filename Type Size Used Priority\n\
         /dev/zram0 partition 2097148 1024 200\n\
         zram0 partition 2097148 1024 200\n",
        // Three rows, one device.
        "Filename Type Size Used Priority\n\
         /dev/zram0 partition 2097148 1024 200\n\
         zram0 partition 2097148 1024 200\n\
         /dev/zram0 partition 2097148 1024 200\n",
    ] {
        // The mechanism, not only the outcome: the reader must yield one
        // sysfs key per device before the fold can ever sum it once.
        assert_eq!(
            zram_swap_devices(repeated),
            BTreeSet::from(["zram0".to_string()]),
            "a repeated basename is one sysfs key: {repeated:?}"
        );
        assert_eq!(
            zram_compression_from(&root, repeated),
            Some(expected.clone()),
            "a repeated basename is one device: {repeated:?}"
        );
        // The ratio is not the tell: doubling both sides of it leaves it
        // unchanged, which is why the totals are what the test pins.
        assert_eq!(
            zram_compression_from(&root, repeated).map(|s| (s.orig_data_bytes, s.compr_data_bytes)),
            Some((3000, 1000)),
            "the byte figures must be one device's worth: {repeated:?}"
        );
    }

    let _ = fs::remove_dir_all(&root);
}

#[test]
// TestName: a_counted_device_without_the_column_makes_the_tier_total_unmeasurable
fn a_counted_device_without_the_column_makes_the_tier_total_unmeasurable() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-sys-block-incomplete-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let zram0 = root.join("zram0");
    let zram1 = root.join("zram1");
    let zram2 = root.join("zram2");
    for device in [&zram0, &zram1, &zram2] {
        fs::create_dir_all(device).expect("fixture dir");
    }
    // Every line reports the three byte columns. zram1's kernel stops
    // before `huge_pages`; zram2 reports it again after zram1.
    fs::write(zram0.join("mm_stat"), "3000 1000 1200 0 1200 7 0 2 2\n").expect("fixture");
    fs::write(zram1.join("mm_stat"), "900 300 400 0 400 1 0\n").expect("fixture");
    fs::write(zram2.join("mm_stat"), "500 200 250 0 250 3 0 5 5\n").expect("fixture");
    let swaps = "Filename Type Size Used Priority\n\
                 /dev/zram0 partition 2097148 1024 200\n\
                 /dev/zram1 partition 1024 64 200\n\
                 /dev/zram2 partition 1024 64 200\n";

    let folded = zram_compression_from(&root, swaps).expect("three counted devices");
    // zram2 reported `huge_pages` after zram1 omitted it. A later device
    // that answered must not clear the incomplete mark: the total would
    // then cover zram0 and zram2 only, under a name claiming the tier.
    assert_eq!(
        folded.huge_pages, None,
        "a partial huge-page sum must not be published as a tier total"
    );
    // Every counted device reported `same_pages`, including zram1's 1, so
    // this one is a real sum and must not be demoted with the other.
    assert_eq!(
        folded.same_pages,
        Some(11),
        "7 + 1 + 3 is the complete same-page total for the counted tier"
    );

    // The latch works in the other direction too. `same_pages` precedes
    // `huge_pages` in the kernel's layout, so a line that stops early
    // demotes `huge_pages`; only a `same_pages` column that is present but
    // unreadable can demote `same_pages` while `huge_pages` still answers.
    // That is the junk-column case, and the fold must treat it the same
    // way: the incomplete mark is per counter, and either one is enough to
    // make that counter's tier total unmeasurable.
    fs::write(zram1.join("mm_stat"), "900 300 400 0 400 abc 0 8 8\n").expect("fixture");
    let folded = zram_compression_from(&root, swaps).expect("three counted devices again");
    assert_eq!(
        folded.same_pages, None,
        "a same_pages column that does not parse must demote the same-page total"
    );
    assert_eq!(
        folded.huge_pages,
        Some(15),
        "2 + 8 + 5 is the complete huge-page total; the other counter's junk must not demote it"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
// TestName: a_device_the_tier_table_names_but_that_does_not_answer_makes_the_tier_total_unmeasurable
fn a_device_the_tier_table_names_but_that_does_not_answer_makes_the_tier_total_unmeasurable() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-sys-block-silence-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let zram0 = root.join("zram0");
    let zram1 = root.join("zram1");
    for device in [&zram0, &zram1] {
        fs::create_dir_all(device).expect("fixture dir");
        fs::write(device.join("mm_stat"), "3000 1000 1200 0 1200 7 0 2 2\n").expect("fixture");
        fs::write(device.join("comp_algorithm"), "lzo-rle [zstd] lz4\n").expect("fixture");
    }
    // The swap table names both devices. That is the claim the published
    // total has to cover, and it keeps naming them while one goes silent.
    let swaps = "Filename Type Size Used Priority\n\
                 /dev/zram0 partition 2097148 1024 200\n\
                 /dev/zram1 partition 1024 64 200\n";

    // Positive control: both answer, so the total is the real sum.
    assert_eq!(
        zram_compression_from(&root, swaps),
        Some(ZramCompression {
            orig_data_bytes: 6000,
            compr_data_bytes: 2000,
            mem_used_bytes: 2400,
            huge_pages: Some(4),
            same_pages: Some(14),
            compressor: Some("zstd".to_string()),
        })
    );

    // Negative: a named device whose `mm_stat` is gone has not answered.
    // The survivor's 3000 bytes and its 2 incompressible pages are not the
    // tier total, and publishing them would also stop demoting
    // `huge_pages` — the silent device would cease to be a counted device
    // whose missing column latched the mark.
    let _ = fs::remove_file(zram1.join("mm_stat"));
    assert_eq!(
        zram_compression_from(&root, swaps),
        None,
        "a named device that does not answer mm_stat blanks the tier total"
    );

    // Negative: a body that does not parse is not an answer either. The
    // file's presence is not what counts — the device answering is.
    fs::write(zram1.join("mm_stat"), "not numbers\n").expect("fixture mm_stat");
    assert_eq!(
        zram_compression_from(&root, swaps),
        None,
        "a named device whose mm_stat body does not parse blanks the tier total"
    );

    // Negative: a body that stops before the three byte columns answers
    // nothing the ratio can stand on. Same refusal.
    fs::write(zram1.join("mm_stat"), "\n").expect("fixture mm_stat");
    assert_eq!(zram_compression_from(&root, swaps), None);

    // Positive: the device answering again restores the real sum. Silence
    // is not a latch — it is the absence of this frame's sample.
    fs::write(zram1.join("mm_stat"), "3000 1000 1200 0 1200 7 0 2 2\n").expect("fixture");
    assert_eq!(
        zram_compression_from(&root, swaps).map(|sample| sample.orig_data_bytes),
        Some(6000)
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_device_reporting_an_unproducible_pair_blanks_the_tier_total() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-sys-block-tear-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let zram0 = root.join("zram0");
    let zram1 = root.join("zram1");
    for device in [&zram0, &zram1] {
        fs::create_dir_all(device).expect("fixture dir");
        fs::write(device.join("comp_algorithm"), "lzo-rle [zstd] lz4\n").expect("fixture");
    }
    // zram0 reports a pair the kernel cannot produce: no data held while a
    // compressed form exists. zram1 is healthy. The swap table names both.
    fs::write(zram0.join("mm_stat"), "0 50 60 0 60 0 0 0 0\n").expect("fixture mm_stat");
    fs::write(zram1.join("mm_stat"), "1000 400 500 0 500 0 0 0 0\n").expect("fixture mm_stat");
    let swaps = "Filename Type Size Used Priority\n\
                 /dev/zram0 partition 2097148 1024 200\n\
                 /dev/zram1 partition 1024 64 200\n";

    assert_eq!(
        zram_compression_from(&root, swaps),
        None,
        "a named device whose byte counts describe no state blanks the tier total"
    );

    // The healthy device's bytes are not the tier total either. Summing
    // the survivors around the tear is the partial sum the silent-device
    // rule already refuses, and here it would also publish the tear's 50
    // compressed bytes inside a `2.22:1` that looks measured.
    assert_eq!(
        zram_compression_from(&root, swaps).map(|sample| sample.compr_data_bytes),
        None
    );

    // Mirror pair: data held, compressed form empty. Same-filled, not a
    // tear — the device answers and its bytes belong in the total.
    fs::write(zram0.join("mm_stat"), "3000 0 1200 0 1200 7 0 2 2\n").expect("fixture mm_stat");
    assert_eq!(
        zram_compression_from(&root, swaps)
            .map(|sample| (sample.orig_data_bytes, sample.compr_data_bytes)),
        Some((4000, 400))
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
// TestName: a_counted_device_without_an_algorithm_makes_the_tier_name_unmeasurable
fn a_counted_device_without_an_algorithm_makes_the_tier_name_unmeasurable() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-sys-block-comp-incomplete-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let zram0 = root.join("zram0");
    let zram1 = root.join("zram1");
    for device in [&zram0, &zram1] {
        fs::create_dir_all(device).expect("fixture dir");
        fs::write(device.join("mm_stat"), "3000 1000 1200 0 1200 0 0 2 2\n").expect("fixture");
    }
    fs::write(zram0.join("comp_algorithm"), "[zstd] lz4\n").expect("fixture");
    // zram1's `comp_algorithm` is gone: the kernel will not say what is
    // compressing those bytes, and those bytes are still in the totals.
    let swaps = "Filename Type Size Used Priority\n\
                 /dev/zram0 partition 2097148 1024 200\n\
                 /dev/zram1 partition 1024 64 200\n";

    // Negative: zram0's name cannot describe a tier whose other counted
    // device left the answer unread. Publishing `zstd` would claim zram1's
    // bytes are compressed under it.
    assert_eq!(
        zram_compression_from(&root, swaps).and_then(|sample| sample.compressor),
        None,
        "a name covering only the devices that answered is not a tier name"
    );

    // Positive: every counted device naming the algorithm is what
    // publishes it. Silence is not the way to get there — a device that
    // does not answer `mm_stat` blanks the whole sample instead.
    fs::write(zram1.join("comp_algorithm"), "lzo-rle [zstd] lz4\n").expect("fixture");
    assert_eq!(
        zram_compression_from(&root, swaps).and_then(|sample| sample.compressor),
        Some("zstd".to_string())
    );

    // A later device that answers must not clear the mark. Put zram1 back
    // in the counted set with no algorithm, then add a third device that
    // agrees with zram0: the tier still has one counted device it cannot
    // name, and the agreeing third must not paper over that.
    let _ = fs::remove_file(zram1.join("comp_algorithm"));
    fs::write(zram1.join("mm_stat"), "900 300 400 0 400 0 0 1 1\n").expect("fixture");
    let zram2 = root.join("zram2");
    fs::create_dir_all(&zram2).expect("fixture dir");
    fs::write(zram2.join("mm_stat"), "500 200 250 0 250 0 0 0 0\n").expect("fixture");
    fs::write(zram2.join("comp_algorithm"), "lz4 [zstd]\n").expect("fixture");
    let three = "Filename Type Size Used Priority\n\
                 /dev/zram0 partition 2097148 1024 200\n\
                 /dev/zram1 partition 1024 64 200\n\
                 /dev/zram2 partition 1024 64 200\n";
    assert_eq!(
        zram_compression_from(&root, three).and_then(|sample| sample.compressor),
        None
    );

    // Every counted device answering is what publishes the name — agreement
    // alone is not enough while one of them is unread. Restoring the
    // answer is the way to get there; silencing the device is not.
    fs::write(zram1.join("comp_algorithm"), "[zstd]\n").expect("fixture");
    assert_eq!(
        zram_compression_from(&root, three).and_then(|sample| sample.compressor),
        Some("zstd".to_string())
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn compression_ratio_is_absent_when_nothing_is_stored() {
    let held = ZramCompression {
        orig_data_bytes: 3000,
        compr_data_bytes: 1000,
        mem_used_bytes: 1200,
        huge_pages: Some(0),
        same_pages: Some(0),
        compressor: Some("zstd".to_string()),
    };
    assert_eq!(held.compression_ratio(), Some(3.0));
    // No data, or no compressed form of it: there is no ratio. Reporting
    // 0.0 or 1.0 would be a measurement the device never made.
    assert_eq!(ZramCompression::default().compression_ratio(), None);
    assert_eq!(
        ZramCompression {
            orig_data_bytes: 3000,
            ..ZramCompression::default()
        }
        .compression_ratio(),
        None
    );
}

/// The free function is the one derivation, and the typed method is that
/// same answer rather than a second formula. A display that holds the two
/// byte counts calls this and never reads the derived `ratio` off the wire.
#[test]
fn compression_ratio_of_is_the_single_derivation() {
    assert_eq!(compression_ratio_of(3000, 1000), Some(3.0));
    assert_eq!(compression_ratio_of(5, 2), Some(2.5));
    // Expansion is a measurement: the compressed form got bigger.
    assert_eq!(compression_ratio_of(100, 120), Some(100.0 / 120.0));

    // A zero on either side is "nothing to report", never 0.0 or 1.0.
    assert_eq!(compression_ratio_of(0, 0), None);
    assert_eq!(compression_ratio_of(3000, 0), None);
    assert_eq!(compression_ratio_of(0, 1000), None);

    // The typed method is a projection of the same rule, not a rival one.
    let held = ZramCompression {
        orig_data_bytes: 3000,
        compr_data_bytes: 1000,
        mem_used_bytes: 1200,
        huge_pages: Some(0),
        same_pages: Some(0),
        compressor: Some("zstd".to_string()),
    };
    assert_eq!(
        held.compression_ratio(),
        compression_ratio_of(held.orig_data_bytes, held.compr_data_bytes)
    );
}

/// Only one direction of the zero-pair is impossible. The tear is
/// `orig == 0` beside `compr > 0`; the mirror is same-filled data and is a
/// real measurement this predicate must keep accepting.
#[test]
fn unproducible_pair_covers_only_the_direction_the_kernel_cannot_produce() {
    assert!(unproducible_pair(0, 50));
    assert!(unproducible_pair(0, 1));

    // Same-filled: data held, compressed body empty. Not a tear.
    assert!(!unproducible_pair(3000, 0));
    // Nothing held and nothing stored. Not a tear.
    assert!(!unproducible_pair(0, 0));
    // A healthy pair. Not a tear.
    assert!(!unproducible_pair(3000, 1000));
    // Expansion is a measurement too.
    assert!(!unproducible_pair(100, 120));
}

/// The classification rests on the byte pair and is refuted only by a
/// counter that was measured and says zero. `None` is "not measured" and
/// never substitutes for that.
#[test]
fn all_stored_pages_same_filled_is_refuted_only_by_a_measured_zero() {
    // The byte pair alone: data held, compressed form empty.
    assert!(all_stored_pages_same_filled(12288, 0, None));
    // A counter that measured pages supports it.
    assert!(all_stored_pages_same_filled(12288, 0, Some(3)));

    // A measured zero contradicts the bytes: they say every stored page
    // was same-filled and the counter says none was.
    assert!(!all_stored_pages_same_filled(12288, 0, Some(0)));

    // Preconditions the callers already matched. A pair that is not
    // "data held, form empty" supports no same-filled claim at all.
    assert!(!all_stored_pages_same_filled(0, 0, None));
    assert!(!all_stored_pages_same_filled(0, 0, Some(0)));
    assert!(!all_stored_pages_same_filled(12288, 4096, None));
    assert!(!all_stored_pages_same_filled(12288, 4096, Some(3)));
}

#[test]
fn agreed_compressor_keeps_a_name_only_when_the_two_reads_agree() {
    // Agreement is the only admission: both reads returned the same name.
    assert_eq!(
        agreed_compressor(Some("zstd".to_string()), Some("zstd".to_string())),
        Some("zstd".to_string())
    );
    // A name that changed under the read is not a name for the bytes the
    // reader already holds. Same refusal as an unreadable algorithm.
    assert_eq!(
        agreed_compressor(Some("zstd".to_string()), Some("lzo-rle".to_string())),
        None
    );
    // Either side unreadable is refusal; `None` never becomes a default.
    assert_eq!(agreed_compressor(None, Some("zstd".to_string())), None);
    assert_eq!(agreed_compressor(Some("zstd".to_string()), None), None);
    assert_eq!(agreed_compressor(None, None), None);
}

#[test]
// TestName: the_compressor_comes_from_the_kernel_not_from_the_setup_request
fn the_compressor_comes_from_the_kernel_not_from_the_setup_request() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-sys-block-comp-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let zram0 = root.join("zram0");
    fs::create_dir_all(&zram0).expect("fixture dir");
    fs::write(zram0.join("mm_stat"), "3000 1000 1200 0 1200 0 0 2 2\n").expect("fixture mm_stat");
    let swaps = "Filename Type Size Used Priority\n/dev/zram0 partition 2097148 1024 200\n";

    // No `comp_algorithm` at all is an unmeasured compressor, never the
    // name the setup walk happened to request.
    assert_eq!(
        zram_compression_from(&root, swaps).and_then(|sample| sample.compressor),
        None
    );

    // The kernel's answer, bracketed in the list it offers.
    fs::write(zram0.join("comp_algorithm"), "lzo-rle lzo [lz4] zstd\n")
        .expect("fixture comp_algorithm");
    assert_eq!(
        zram_compression_from(&root, swaps).and_then(|sample| sample.compressor),
        Some("lz4".to_string())
    );

    // A line that brackets nothing names no compressor.
    fs::write(zram0.join("comp_algorithm"), "lzo-rle lzo lz4 zstd\n")
        .expect("fixture comp_algorithm");
    assert_eq!(
        zram_compression_from(&root, swaps).and_then(|sample| sample.compressor),
        None
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
// TestName: disagreeing_compressors_publish_no_single_name
fn disagreeing_compressors_publish_no_single_name() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-sys-block-disagree-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    for (device, algorithm) in [("zram0", "[zstd] lz4\n"), ("zram1", "zstd [lz4]\n")] {
        let dir = root.join(device);
        fs::create_dir_all(&dir).expect("fixture dir");
        fs::write(dir.join("mm_stat"), "1000 400 500 0 500 0 0 0 0\n").expect("fixture mm_stat");
        fs::write(dir.join("comp_algorithm"), algorithm).expect("fixture comp_algorithm");
    }
    let swaps = "Filename Type Size Used Priority\n/dev/zram0 partition 1024 10 200\n/dev/zram1 partition 1024 10 200\n";

    // Two counted devices under two algorithms: there is no single name
    // for the tier. Publishing one would say the other device uses it too.
    assert_eq!(
        zram_compression_from(&root, swaps),
        Some(ZramCompression {
            orig_data_bytes: 2000,
            compr_data_bytes: 800,
            mem_used_bytes: 1000,
            huge_pages: Some(0),
            same_pages: Some(0),
            compressor: None,
        })
    );

    // Agreement after a disagreement must not resurrect a name: the fold
    // has already seen two different answers and cannot unsee one.
    let zram2 = root.join("zram2");
    fs::create_dir_all(&zram2).expect("fixture dir");
    fs::write(zram2.join("mm_stat"), "500 200 250 0 250 0 0 0 0\n").expect("fixture mm_stat");
    fs::write(zram2.join("comp_algorithm"), "[zstd]\n").expect("fixture comp_algorithm");
    let three = "Filename Type Size Used Priority\n/dev/zram0 partition 1024 10 200\n/dev/zram1 partition 1024 10 200\n/dev/zram2 partition 1024 10 200\n";
    assert_eq!(
        zram_compression_from(&root, three).and_then(|sample| sample.compressor),
        None
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn the_published_shape_never_reports_an_unmeasured_field_as_zero() {
    let measured = ZramCompression {
        orig_data_bytes: 3000,
        compr_data_bytes: 1000,
        mem_used_bytes: 1200,
        huge_pages: Some(0),
        same_pages: Some(12),
        compressor: Some("zstd".to_string()),
    };
    let published: serde_json::Value =
        serde_json::from_str(&measured.to_status_json()).expect("published shape is JSON");
    assert_eq!(published["orig_data_bytes"], 3000);
    assert_eq!(published["compr_data_bytes"], 1000);
    assert_eq!(published["mem_used_bytes"], 1200);
    assert_eq!(published["ratio"], 3.0);
    assert_eq!(published["compressor"], "zstd");
    // A measured zero is a zero; a measured count is that count.
    assert_eq!(published["huge_pages"], 0);
    assert_eq!(published["same_pages"], 12);

    // Nothing measured: every derived field is null, never a zero that
    // reads like a sample the device did not take.
    let unmeasured = ZramCompression {
        orig_data_bytes: 3000,
        compr_data_bytes: 1000,
        mem_used_bytes: 1200,
        huge_pages: None,
        same_pages: None,
        compressor: None,
    };
    let published: serde_json::Value =
        serde_json::from_str(&unmeasured.to_status_json()).expect("published shape is JSON");
    assert_eq!(published["huge_pages"], serde_json::Value::Null);
    assert_eq!(published["same_pages"], serde_json::Value::Null);
    assert_eq!(published["compressor"], serde_json::Value::Null);
    assert_eq!(published["ratio"], 3.0);

    // The three byte counts are bare and read `0` for a device holding
    // nothing. That is a measurement of emptiness; "not measured at all"
    // is the surrounding `Option`, which is `null`.
    let empty = ZramCompression::default();
    let published: serde_json::Value =
        serde_json::from_str(&empty.to_status_json()).expect("published shape is JSON");
    assert_eq!(published["ratio"], serde_json::Value::Null);
    assert_eq!(published["huge_pages"], serde_json::Value::Null);
    assert_eq!(published["same_pages"], serde_json::Value::Null);
    assert_eq!(published["compressor"], serde_json::Value::Null);
    assert_eq!(published["orig_data_bytes"], 0);

    // The derived `ratio` is present in the same shape a derived `Serialize`
    // would give the stress report: one sample, one spelling.
    let via_derive = serde_json::to_value(&measured).expect("serializable sample");
    assert_eq!(via_derive["ratio"], 3.0);
    assert_eq!(via_derive["same_pages"], 12);
    assert_eq!(via_derive["compressor"], "zstd");
    // `pages_compacted` is allocator health, not compression quality, and
    // is deliberately absent from every spelling of this sample.
    assert!(published.get("pages_compacted").is_none());
    assert!(via_derive.get("pages_compacted").is_none());
}

#[test]
// TestName: the_published_saving_already_contains_the_same_filled_pages
fn the_published_saving_already_contains_the_same_filled_pages() {
    // A device whose every stored page was same-filled — each page's
    // elements all held one value — so the three of them were stored as
    // fill references. The kernel increments `orig_data_size` for every
    // stored page and `compr_data_size` only on the compressed path, so
    // an all-same-filled device reports `compr == 0` exactly: there is no
    // ratio here, and the whole of `orig - compr` is those references.
    const PAGE_SIZE: u64 = 4096;
    let sample = ZramCompression {
        orig_data_bytes: PAGE_SIZE * 3,
        compr_data_bytes: 0,
        mem_used_bytes: PAGE_SIZE,
        huge_pages: Some(0),
        same_pages: Some(3),
        compressor: Some("zstd".to_string()),
    };
    let published: serde_json::Value =
        serde_json::from_str(&sample.to_status_json()).expect("published shape is JSON");

    // Positive: the count is published beside the byte figures, so a
    // reader can see how much of the ratio came from the fill references
    // rather than from the compressor.
    assert_eq!(published["same_pages"], 3);

    // Negative: the saving is one number, `orig - compr`, and the shape
    // offers no second one to add on top of it. A reader who treats
    // `same_pages` as an extra saving and adds `same_pages * PAGE_SIZE`
    // would claim more saving than the device ever held — which is the
    // arithmetic the field's label exists to stop.
    let saving = sample.orig_data_bytes - sample.compr_data_bytes;
    let double_counted = saving + 3 * PAGE_SIZE;
    assert!(
        double_counted > sample.orig_data_bytes,
        "adding the same-filled count twice claims more saving than the data ever had: \
         {double_counted} > {}",
        sample.orig_data_bytes
    );
    for invented in [
        "saved_bytes",
        "same_pages_saved_bytes",
        "same_pages_bytes",
        "same_filled_saved_bytes",
    ] {
        assert!(
            published.get(invented).is_none(),
            "the sample must not publish a second saving field ({invented}) for a reader to add"
        );
    }
}
