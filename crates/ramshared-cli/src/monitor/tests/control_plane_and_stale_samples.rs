// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/tests/control_plane_and_stale_samples.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;

/// A reserve floor that is not whole MiB publishes the larger whole MiB.
///
/// The enforced floor is generally fractional once a capacity-derived term
/// beats the sealed 2048 MiB minimum, which starts around a 10 GiB
/// capacity: `configured_reserve_bytes` floors `capacity * percent / 100`
/// and the safety term is `capacity.div_ceil(5)`. Floor division then
/// publishes a figure **below** the amount that must stay free — the one
/// direction a reserve floor must never round. `div_ceil` overshoots by at
/// most one MiB and is the conservative face.
#[test]
fn reserve_floor_face_ceils_a_fractional_mib_floor() {
    // 24 GiB capacity: the 20% term beats the sealed minimum, so the
    // enforced floor lands between two whole MiB.
    let (enforced_mib, source, sealed_min_mib, percent, runtime_mib) =
        vram_reserve_fields(Some(24576 * 1024 * 1024), 24576 * 1024 * 1024);

    let capacity = 24576 * 1024 * 1024u64;
    let configured = (2048 * 1024 * 1024u64).max(capacity * 20 / 100);
    let enforced_bytes = configured.max(capacity.div_ceil(5)) + runtime_mib * 1024 * 1024;
    assert_eq!(
        enforced_bytes, 5_422_396_212,
        "fixture arithmetic: 24 GiB at 20% with the safety term and the 256 MiB buffer"
    );
    assert_ne!(
        enforced_bytes % (1024 * 1024),
        0,
        "the fixture must be fractional or it proves nothing about the direction"
    );

    // The true floor is 5171 MiB plus 209 716 bytes. Publishing `5171`
    // understates it; `5172` is the face that never authorizes more than
    // the policy allows.
    assert_eq!(
        enforced_mib, 5172,
        "a reserve floor never publishes the smaller MiB"
    );
    assert_ne!(
        enforced_mib, 5171,
        "floor division would understate the floor by 209 716 bytes"
    );

    // The exact-MiB terms are unchanged by the conversion: it is the
    // identity on a whole-MiB input.
    assert_eq!(source, "sealed-manifest");
    assert_eq!(sealed_min_mib, 2048);
    assert_eq!(percent, 20);
    assert_eq!(runtime_mib, 256);
}

/// The ceiling is the identity on a whole-MiB floor, so the direction is
/// never a gratuitous extra MiB on the common case.
#[test]
fn reserve_floor_face_is_the_identity_on_a_whole_mib_floor() {
    // The sealed 2048 MiB minimum dominates here and the enforced floor
    // lands on exact 2304 MiB.
    let (enforced_mib, _source, _sealed_min_mib, _percent, _runtime_mib) =
        vram_reserve_fields(Some(6144 * 1024 * 1024), 4016 * 1024 * 1024);
    assert_eq!(enforced_mib, 2304);
    assert_eq!(reserve_floor_mib(2304 * 1024 * 1024), 2304);
    // A non-zero sub-MiB floor still never publishes as zero.
    assert_eq!(reserve_floor_mib(1), 1);
    assert_eq!(reserve_floor_mib(0), 0);
}

#[test]
fn monitor_labels_logical_cache_separately_from_ram() {
    const LOGICAL: u64 = 256 * 1024 * 1024;
    let line = format_cache_telemetry_labels(&sample_cache(LOGICAL, 1_000));

    // The logical figure is labelled as VRAM cache occupancy.
    assert!(
        line.contains("VRAM cache logical: 256.0 MB"),
        "logical cache bytes must be labelled as VRAM cache occupancy, got: {line}"
    );
    assert!(line.contains("VRAM cache slabs: 8.0 MB"), "{line}");
    assert!(line.contains("codec workspace: 1.0 MB"), "{line}");

    // No non-VRAM RAM label exists anywhere on the line. "VRAM" itself is
    // the only permitted occurrence of that substring.
    let without_vram = line.to_uppercase().replace("VRAM", "");
    assert!(
        !without_vram.contains("RAM"),
        "cache telemetry must never be labelled as RAM: {line}"
    );
    assert!(!line.contains("host RAM"), "{line}");
    assert!(!line.contains("guest RAM"), "{line}");
    assert!(!line.contains("available RAM"), "{line}");

    // Codec counters are present and named as codec counters, not memory.
    assert!(line.contains("codec errors: integrity=2 decode=1 timeout=3"));
}

/// verified test case (zram 6): the VRAM-cache tier stores raw pages at 1:1
/// (DT-9 compression off by default). Its two store counters name cache
/// residency, not a compression mechanism — the way `huge_pages` was
/// renamed to `incompressible` because that is what the counter is.
#[test]
fn cache_telemetry_labels_name_residency_not_compression() {
    const LOGICAL: u64 = 256 * 1024 * 1024;
    let line = format_cache_telemetry_labels(&sample_cache(LOGICAL, 1_000));

    // Residency vocabulary is on the line and carries the measured bytes.
    assert!(line.contains("encoded cached:"), "{line}");
    assert!(line.contains("raw cached:"), "{line}");
    assert!(line.contains("encoded cached: 32.0 MB"), "{line}");
    assert!(line.contains("raw cached: 4.0 MB"), "{line}");

    // Compression vocabulary is gone. The wire keys stay
    // `compressed_payload_bytes` / `raw_payload_bytes` in `ramshared-vram`;
    // the operator-facing noun is what this row owns.
    assert!(!line.contains("compressed payload"), "{line}");
    assert!(!line.contains("raw payload"), "{line}");
    assert!(!line.contains("compressed"), "{line}");

    // No compression ratio is derivable from these two counters alone and
    // none is published. A `x:1` face or a saving claim on this row would
    // invent a figure the producer never sent.
    assert!(!line.contains(":1"), "{line}");
    assert!(!line.contains("ratio"), "{line}");
    assert!(!line.contains("saving"), "{line}");
    assert!(!line.contains("compression"), "{line}");

    // The codec refusal reason is published beside the codec state, so the
    // operator can see why the codec is not taking work.
    assert!(
        line.contains("(codec-subdeadline)"),
        "the refusal reason must be published beside the codec state: {line}"
    );
    assert!(line.contains("codec: available/ready"), "{line}");
}

#[test]
fn cache_telemetry_never_renders_a_present_measurement_as_zero() {
    // A 512 KiB logical cache is a real occupancy figure. An integer divide
    // by MiB collapsed it to `0 MiB`, which reads as "the cache is empty"
    // while the line is reporting bytes it is holding. The fixture's own
    // 64 KiB metadata had the same collapse baked in unnoticed.
    let cache = sample_cache(512 * 1024, 1_000);
    let line = format_cache_telemetry_labels(&cache);
    assert!(
        line.contains("VRAM cache logical: 512 KB"),
        "a sub-MiB measurement must keep its size, got: {line}"
    );
    assert!(line.contains("metadata: 64 KB"), "{line}");
    let mut unknown = cache.clone();
    unknown.metadata_bytes = None;
    let unknown_line = format_cache_telemetry_labels(&unknown);
    assert!(
        unknown_line.contains("metadata: unavailable"),
        "{unknown_line}"
    );
    unknown.device_metadata_lower_bound_bytes = Some(1024);
    let partial = format_cache_telemetry_labels(&unknown);
    assert!(
        partial.contains("metadata: at least 1 KB (partial)"),
        "{partial}"
    );
    assert!(
        !line.contains("0 MiB"),
        "a present measurement must never render as a zero MiB: {line}"
    );

    // A measured zero is the other fact and still reads as zero bytes.
    let empty = sample_cache(0, 1_000);
    let line = format_cache_telemetry_labels(&empty);
    assert!(line.contains("VRAM cache logical: 0 B"), "{line}");
}

/// The record keeps an aged sample; the row spells the two refusals apart.
///
/// Replaces the old `monitor_omits_stale_codec_telemetry`, which asserted
/// the defect: that a stale sample "must be omitted". Erasing it spent the
/// `not measured` spelling — "the worker never spoke" — on a reading the
/// worker actually produced.
#[test]
fn cache_telemetry_row_spells_stale_apart_from_not_measured() {
    let now = 10_000u64;

    // The record is parse-only and keeps whatever the daemon published.
    let aged = sample_cache(4096, now - TELEMETRY_MAX_AGE_MS - 1);
    let aged_value = serde_json::to_value(&aged).unwrap();
    let record = cache_telemetry_record(Some(&aged_value)).expect("an aged sample is a record");
    assert_eq!(
        record.logical_cached_bytes, 4096,
        "the aged sample is kept as itself, not erased"
    );
    assert!(!record.is_fresh_at(now, TELEMETRY_MAX_AGE_MS));

    // The row is the display gate, and it refuses age as `stale` — never
    // as the figure and never as `not measured`.
    assert_eq!(
        cache_telemetry_row(Some(&aged_value), false, now),
        "VRAM cache codec telemetry: stale",
        "an aged reading is stale, not absent"
    );

    // A current sample is the figure. A measured zero is still a figure.
    let fresh = sample_cache(4096, now);
    let fresh_value = serde_json::to_value(&fresh).unwrap();
    let row = cache_telemetry_row(Some(&fresh_value), false, now);
    assert!(
        row.starts_with("VRAM cache logical:"),
        "a current sample renders the figure, got: {row}"
    );
    assert!(row.contains("codec-subdeadline"), "{row}");
    let empty_row = cache_telemetry_row(
        Some(&serde_json::to_value(sample_cache(0, now)).unwrap()),
        false,
        now,
    );
    assert!(empty_row.contains("VRAM cache logical: 0 B"), "{empty_row}");

    // The observation's own collection failure is the other staleness
    // cause and spells the same word on this row.
    assert_eq!(
        cache_telemetry_row(Some(&fresh_value), true, now),
        "VRAM cache codec telemetry: stale",
        "a failed refresh is stale even when the sample itself is current"
    );

    // Nonsense is absence before it is age: unknown schema, unparsable
    // bodies and a missing field all read `not measured`, and never `stale`.
    let mut unknown = sample_cache(4096, now);
    unknown.schema_version = 9;
    let unknown_value = serde_json::to_value(&unknown).unwrap();
    assert_eq!(
        cache_telemetry_row(Some(&unknown_value), false, now),
        "VRAM cache codec telemetry: not measured"
    );
    assert_eq!(
        cache_telemetry_row(Some(&Value::String("nope".into())), false, now),
        "VRAM cache codec telemetry: not measured"
    );
    assert_eq!(
        cache_telemetry_row(None, false, now),
        "VRAM cache codec telemetry: not measured"
    );
    assert_eq!(
        cache_telemetry_row(Some(&Value::Null), false, now),
        "VRAM cache codec telemetry: not measured"
    );

    // The two refusals are distinct strings. A collapse back into one
    // ambiguous phrase is the defect this test exists to refuse.
    assert_ne!(
        cache_telemetry_row(Some(&aged_value), false, now),
        cache_telemetry_row(None, false, now),
        "stale and not measured must never share a spelling"
    );
}

/// DT-4 / DT-8 / NFR-5 and invariant check.
///
/// The client names **both** releases on the transition: the target is the
/// selected release's executable, and the release being left travels only
/// as a display hint on the child command. And a client that already tried
/// and failed re-execs **once**: `allow_reexec` is the guard, and once it is
/// false no further restart is planned — the process stays alive and keeps
/// reporting, rather than exiting on a failed re-exec.
#[cfg(unix)]
#[test]
fn client_refresh_names_both_releases_and_reexecs_once() {
    let target = PathBuf::from("/opt/ramshared/releases/v1/bin/ramshared");
    let argv0 = std::ffi::OsStr::new("ramshared");
    let args = [std::ffi::OsString::from("top")];
    let leaving = "v0.14.0-593-g38c38651";

    let command = reexec_command(&target, argv0, &args, Some(leaving));
    assert_eq!(command.get_program(), target.as_os_str());
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        vec![std::ffi::OsStr::new("top")]
    );
    let hinted: Vec<_> = command
        .get_envs()
        .filter(|(key, _)| *key == PREVIOUS_RELEASE_HINT_ENV)
        .collect();
    assert_eq!(hinted.len(), 1, "the hint is set exactly once");
    assert_eq!(
        hinted[0]
            .1
            .map(|value| value.to_string_lossy().into_owned()),
        Some(leaving.to_string())
    );

    // Without a previous release there is no hint to carry — the env is
    // absent, not empty. An empty hint would render a blank "from".
    let plain = reexec_command(&target, argv0, &args, None);
    assert!(
        plain
            .get_envs()
            .all(|(key, _)| key != PREVIOUS_RELEASE_HINT_ENV),
        "no previous release means no hint env at all"
    );

    // after a failed re-exec the client must not plan a
    // second one. `allow_reexec` is the one-shot guard.
    let root = std::env::temp_dir().join(format!(
        "ramshared-client-refresh-once-{}",
        std::process::id()
    ));
    let installed_bin = root.join("bin/ramshared");
    let running_bin = root.join("running/ramshared");
    fs::create_dir_all(installed_bin.parent().expect("installed parent"))
        .expect("create installed parent");
    fs::create_dir_all(running_bin.parent().expect("running parent"))
        .expect("create running parent");
    fs::write(&installed_bin, b"installed bytes").expect("write installed");
    fs::write(&running_bin, b"running bytes").expect("write running");
    let running_metadata = fs::metadata(&running_bin).expect("stat running");
    let running_sha256 = crate::installed_image::sha256_file(&running_bin).expect("hash running");
    let installed = InstalledIdentity {
        executable: installed_bin.clone(),
        version: Some("0.15.0".to_string()),
        source_commit: Some("1234567890abcdef1234567890abcdef12345678".to_string()),
        source_tree_state: Some("clean".to_string()),
        installed_at_utc: Some("2026-10-06T00:00:00Z".to_string()),
        executable_sha256: Some(
            crate::installed_image::sha256_file(&installed_bin).expect("hash installed"),
        ),
    };
    let planned = restart_target(
        ProcessInstallOrigin::Direct,
        Some(&running_metadata),
        Some(running_sha256.as_str()),
        true,
        Some(&installed),
        None,
    );
    assert_eq!(planned, Some(installed_bin.clone()), "first pass plans it");
    let after_failed_reexec = restart_target(
        ProcessInstallOrigin::Direct,
        Some(&running_metadata),
        Some(running_sha256.as_str()),
        false,
        Some(&installed),
        None,
    );
    assert_eq!(
        after_failed_reexec, None,
        "the one-shot guard must not plan a second re-exec"
    );

    fs::remove_dir_all(&root).expect("remove client-refresh fixture");
}

#[test]
fn update_tier_latencies_estimates_from_measured_io_only() {
    // Counts above zero take the throughput-scaled formula; the zero-count
    // constants are already exercised by the accumulator tests. The page
    // fault estimate follows the deepest tier that is actually moving data:
    // with all three busy the assertion below takes the disk figure, which
    // is the worst latency a fault can reach here, not the cheapest.
    let mut cp = ControlPlaneObservation {
        zram_io: TierIoStats {
            avg_mbs: Some(40.0),
            max_mbs: Some(80.0),
            ..TierIoStats::default()
        },
        vram_io: TierIoStats {
            avg_mbs: Some(10.0),
            max_mbs: Some(20.0),
            ..TierIoStats::default()
        },
        disk_io: TierIoStats {
            avg_mbs: Some(2.0),
            max_mbs: Some(4.0),
            read_mbs: Some(1.0),
            ..TierIoStats::default()
        },
        ..ControlPlaneObservation::default()
    };
    update_tier_latencies(&mut cp, 1, 1, 1);
    assert_eq!(cp.zram_io.latency_source, LatencySource::Estimated);
    assert_eq!(cp.vram_io.latency_source, LatencySource::Estimated);
    assert_eq!(cp.disk_io.latency_source, LatencySource::Estimated);
    assert!(cp.zram_io.avg_lat_us > cp.zram_io.min_lat_us);
    assert!(cp.zram_io.max_lat_us > cp.zram_io.avg_lat_us);
    assert!(cp.vram_io.avg_lat_us > cp.vram_io.min_lat_us);
    assert!(cp.vram_io.max_lat_us > cp.vram_io.avg_lat_us);
    assert!(cp.disk_io.avg_lat_us > cp.disk_io.min_lat_us);
    assert!(cp.disk_io.max_lat_us > cp.disk_io.avg_lat_us);
    assert_eq!(cp.estimated_page_fault_lat_us, cp.disk_io.avg_lat_us);

    let mut vram_only = ControlPlaneObservation {
        vram_io: TierIoStats {
            write_mbs: Some(1.0),
            ..TierIoStats::default()
        },
        ..ControlPlaneObservation::default()
    };
    update_tier_latencies(&mut vram_only, 0, 1, 0);
    assert_eq!(
        vram_only.estimated_page_fault_lat_us,
        vram_only.vram_io.avg_lat_us
    );

    let mut zram_only = ControlPlaneObservation {
        zram_io: TierIoStats {
            write_mbs: Some(1.0),
            ..TierIoStats::default()
        },
        ..ControlPlaneObservation::default()
    };
    update_tier_latencies(&mut zram_only, 1, 0, 0);
    assert_eq!(
        zram_only.estimated_page_fault_lat_us,
        zram_only.zram_io.avg_lat_us
    );

    // The pairwise cases pin the **order**, which the single-tier cases
    // cannot: a chain rewritten cheapest-first would still answer zram for
    // zram and VRAM for VRAM. Deepest paging tier wins, so the disk figure
    // is the one whenever the disk is moving, and VRAM wins only over zram.
    let mut disk_and_vram = ControlPlaneObservation {
        vram_io: TierIoStats {
            write_mbs: Some(1.0),
            ..TierIoStats::default()
        },
        disk_io: TierIoStats {
            write_mbs: Some(1.0),
            ..TierIoStats::default()
        },
        ..ControlPlaneObservation::default()
    };
    update_tier_latencies(&mut disk_and_vram, 0, 1, 1);
    assert_eq!(
        disk_and_vram.estimated_page_fault_lat_us, disk_and_vram.disk_io.avg_lat_us,
        "a disk that is moving is the latency a fault can reach"
    );

    let mut vram_and_zram = ControlPlaneObservation {
        zram_io: TierIoStats {
            write_mbs: Some(1.0),
            ..TierIoStats::default()
        },
        vram_io: TierIoStats {
            write_mbs: Some(1.0),
            ..TierIoStats::default()
        },
        ..ControlPlaneObservation::default()
    };
    update_tier_latencies(&mut vram_and_zram, 1, 1, 0);
    assert_eq!(
        vram_and_zram.estimated_page_fault_lat_us, vram_and_zram.vram_io.avg_lat_us,
        "with the disk quiet, VRAM is deeper than zram"
    );

    // And the all-three case is the same rule one step further, not a
    // special case: with every plane moving it takes the disk figure.
    let mut all_three = ControlPlaneObservation {
        zram_io: TierIoStats {
            write_mbs: Some(1.0),
            ..TierIoStats::default()
        },
        vram_io: TierIoStats {
            write_mbs: Some(1.0),
            ..TierIoStats::default()
        },
        disk_io: TierIoStats {
            write_mbs: Some(1.0),
            ..TierIoStats::default()
        },
        ..ControlPlaneObservation::default()
    };
    update_tier_latencies(&mut all_three, 1, 1, 1);
    assert_eq!(
        all_three.estimated_page_fault_lat_us,
        all_three.disk_io.avg_lat_us
    );
}

#[test]
fn the_latency_profiles_hold_the_catalogued_estimate_band() {
    // The value gate. These figures are engineering estimates, not host
    // measurements — see `TierLatencyEstimate` — and the named profiles are
    // where they live now. An edit to one is deliberate and this test has
    // to see it; the formula tests cite the profiles rather than restating
    // these digits.
    assert_eq!(ZRAM_LATENCY_ESTIMATE.min_us, 0.04);
    assert_eq!(ZRAM_LATENCY_ESTIMATE.idle_avg_us, 0.08);
    assert_eq!(ZRAM_LATENCY_ESTIMATE.avg_slope_at_mbs, 2000.0);
    assert_eq!(ZRAM_LATENCY_ESTIMATE.avg_slope_us, 0.08);
    assert_eq!(ZRAM_LATENCY_ESTIMATE.max_slope_at_mbs, 1000.0);
    assert_eq!(ZRAM_LATENCY_ESTIMATE.max_slope_us, 0.15);
    assert_eq!(ZRAM_LATENCY_ESTIMATE.max_floor_us, 0.15);

    assert_eq!(VRAM_LATENCY_ESTIMATE.min_us, 0.85);
    assert_eq!(VRAM_LATENCY_ESTIMATE.idle_avg_us, 1.45);
    assert_eq!(VRAM_LATENCY_ESTIMATE.avg_slope_at_mbs, 1000.0);
    assert_eq!(VRAM_LATENCY_ESTIMATE.avg_slope_us, 1.20);
    assert_eq!(VRAM_LATENCY_ESTIMATE.max_slope_at_mbs, 500.0);
    assert_eq!(VRAM_LATENCY_ESTIMATE.max_slope_us, 2.00);
    assert_eq!(VRAM_LATENCY_ESTIMATE.max_floor_us, 3.20);

    assert_eq!(DISK_LATENCY_ESTIMATE.min_us, 85.0);
    assert_eq!(DISK_LATENCY_ESTIMATE.idle_avg_us, 180.0);
    assert_eq!(DISK_LATENCY_ESTIMATE.avg_slope_at_mbs, 100.0);
    assert_eq!(DISK_LATENCY_ESTIMATE.avg_slope_us, 120.0);
    assert_eq!(DISK_LATENCY_ESTIMATE.max_slope_at_mbs, 50.0);
    assert_eq!(DISK_LATENCY_ESTIMATE.max_slope_us, 600.0);
    assert_eq!(DISK_LATENCY_ESTIMATE.max_floor_us, 1200.0);

    assert_eq!(RESIDENT_PAGE_FAULT_LAT_US, 0.85);

    // The three tiers stay an order of magnitude apart, which is the
    // ranking the dashboard exists to show. Compile-time: a profile edit
    // that flattens the ordering is a build failure, not a runtime one.
    const {
        assert!(ZRAM_LATENCY_ESTIMATE.max_floor_us < VRAM_LATENCY_ESTIMATE.min_us);
        assert!(VRAM_LATENCY_ESTIMATE.max_floor_us < DISK_LATENCY_ESTIMATE.min_us);
    }
}

#[test]
fn one_projection_method_draws_every_latency_band() {
    // Positive: a tier with a busy window scales with throughput and stays
    // inside its band. Negative: zero busy windows is a quiet frame, not an
    // absent tier — it projects to the idle figure and the floor rather
    // than to a scaled curve built from throughputs never seen as busy.
    let (min_us, avg_us, max_us) = project_tier_latency(&ZRAM_LATENCY_ESTIMATE, 40.0, 80.0, 1);
    assert_eq!(min_us, ZRAM_LATENCY_ESTIMATE.min_us);
    assert_eq!(
        avg_us,
        ZRAM_LATENCY_ESTIMATE.min_us
            + (40.0 / ZRAM_LATENCY_ESTIMATE.avg_slope_at_mbs) * ZRAM_LATENCY_ESTIMATE.avg_slope_us
    );
    // Below the slope's knee the floor binds: the maximum never publishes
    // under `max_floor_us` even when the scaled curve is still under it.
    let scaled_max = ZRAM_LATENCY_ESTIMATE.idle_avg_us
        + (80.0 / ZRAM_LATENCY_ESTIMATE.max_slope_at_mbs) * ZRAM_LATENCY_ESTIMATE.max_slope_us;
    assert!(
        scaled_max < ZRAM_LATENCY_ESTIMATE.max_floor_us,
        "this fixture must sit under the knee or it stops testing the floor"
    );
    assert_eq!(max_us, ZRAM_LATENCY_ESTIMATE.max_floor_us);

    // Past the knee the scaled curve is the answer and the floor is not.
    let (min_us, avg_us, max_us) = project_tier_latency(&ZRAM_LATENCY_ESTIMATE, 2000.0, 1000.0, 1);
    assert_eq!(min_us, ZRAM_LATENCY_ESTIMATE.min_us);
    assert_eq!(
        avg_us,
        ZRAM_LATENCY_ESTIMATE.min_us + ZRAM_LATENCY_ESTIMATE.avg_slope_us
    );
    assert_eq!(
        max_us,
        ZRAM_LATENCY_ESTIMATE.idle_avg_us + ZRAM_LATENCY_ESTIMATE.max_slope_us
    );

    // Zero throughput keeps the base alone: the slope term vanishes.
    let (min_us, avg_us, max_us) = project_tier_latency(&DISK_LATENCY_ESTIMATE, 0.0, 0.0, 1);
    assert_eq!(min_us, DISK_LATENCY_ESTIMATE.min_us);
    assert_eq!(avg_us, DISK_LATENCY_ESTIMATE.min_us);
    // The floor still binds when the slope adds nothing.
    assert_eq!(max_us, DISK_LATENCY_ESTIMATE.max_floor_us);

    // No busy window this frame: the idle figure and the floor, and
    // `min_us` is still published — it is the tier's device-class floor,
    // not a claim about this frame's traffic.
    let (min_us, avg_us, max_us) = project_tier_latency(&VRAM_LATENCY_ESTIMATE, 0.0, 0.0, 0);
    assert_eq!(min_us, VRAM_LATENCY_ESTIMATE.min_us);
    assert_eq!(avg_us, VRAM_LATENCY_ESTIMATE.idle_avg_us);
    assert_eq!(max_us, VRAM_LATENCY_ESTIMATE.max_floor_us);
}

#[test]
fn a_measured_latency_survives_the_projection() {
    // A measured figure outranks a throughput-scaled constant. The
    // projection must not clobber one, and the rest of the record must
    // stay untouched either way.
    let mut measured = TierIoStats {
        latency_source: LatencySource::Measured,
        min_lat_us: 7.5,
        avg_lat_us: 7.5,
        max_lat_us: 7.5,
        avg_mbs: Some(40.0),
        max_mbs: Some(80.0),
        ..TierIoStats::default()
    };
    project_onto_plane(&mut measured, &ZRAM_LATENCY_ESTIMATE, 1);
    assert_eq!(measured.latency_source, LatencySource::Measured);
    assert_eq!(measured.min_lat_us, 7.5);
    assert_eq!(measured.avg_lat_us, 7.5);
    assert_eq!(measured.max_lat_us, 7.5);

    // Unmeasured is still estimated, with the same values as before.
    let mut unmeasured = TierIoStats {
        avg_mbs: Some(40.0),
        max_mbs: Some(80.0),
        ..TierIoStats::default()
    };
    project_onto_plane(&mut unmeasured, &ZRAM_LATENCY_ESTIMATE, 1);
    assert_eq!(unmeasured.latency_source, LatencySource::Estimated);
    assert_eq!(unmeasured.min_lat_us, ZRAM_LATENCY_ESTIMATE.min_us);
    assert!(unmeasured.avg_lat_us > unmeasured.min_lat_us);
    assert!(unmeasured.max_lat_us > unmeasured.avg_lat_us);

    // The whole record is projected or nothing is: a Measured tier keeps
    // its latency and still feeds the page-fault estimate as published.
    let mut cp = ControlPlaneObservation {
        disk_io: TierIoStats {
            latency_source: LatencySource::Measured,
            min_lat_us: 7.5,
            avg_lat_us: 7.5,
            max_lat_us: 7.5,
            write_mbs: Some(1.0),
            ..TierIoStats::default()
        },
        ..ControlPlaneObservation::default()
    };
    update_tier_latencies(&mut cp, 0, 0, 1);
    assert_eq!(cp.disk_io.latency_source, LatencySource::Measured);
    assert_eq!(cp.disk_io.avg_lat_us, 7.5);
    assert_eq!(cp.estimated_page_fault_lat_us, 7.5);
    assert_eq!(cp.zram_io.latency_source, LatencySource::Estimated);
}

#[test]
fn should_exit_tui_accepts_only_press_quit_chords() {
    use ratatui::crossterm::event::KeyEvent;

    let press = |code, modifiers| {
        Event::Key(KeyEvent::new_with_kind(
            code,
            modifiers,
            KeyEventKind::Press,
        ))
    };
    let release = |code, modifiers| {
        Event::Key(KeyEvent::new_with_kind(
            code,
            modifiers,
            KeyEventKind::Release,
        ))
    };

    assert!(!should_exit_tui(None));
    assert!(!should_exit_tui(Some(Event::FocusGained)));
    assert!(should_exit_tui(Some(press(
        KeyCode::Char('q'),
        KeyModifiers::NONE
    ))));
    assert!(should_exit_tui(Some(press(
        KeyCode::Esc,
        KeyModifiers::NONE
    ))));
    assert!(should_exit_tui(Some(press(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL
    ))));
    assert!(!should_exit_tui(Some(release(
        KeyCode::Char('q'),
        KeyModifiers::NONE
    ))));
    assert!(!should_exit_tui(Some(press(
        KeyCode::Char('x'),
        KeyModifiers::NONE
    ))));
    assert!(!should_exit_tui(Some(press(
        KeyCode::Char('c'),
        KeyModifiers::NONE
    ))));
    // Uppercase `Q` is not the exit key; `q` with any modifiers still is.
    assert!(!should_exit_tui(Some(press(
        KeyCode::Char('Q'),
        KeyModifiers::NONE
    ))));
    assert!(should_exit_tui(Some(press(
        KeyCode::Char('q'),
        KeyModifiers::CONTROL | KeyModifiers::ALT
    ))));
}
