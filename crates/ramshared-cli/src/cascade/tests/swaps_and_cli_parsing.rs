// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/tests/swaps_and_cli_parsing.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

/// The published record keeps an aged cache sample as itself, while the
/// admission collapse still refuses it — the pairing the honesty rule
/// wants. `null` here must still mean "the worker never spoke".
#[test]
fn cache_record_keeps_an_aged_sample_while_the_alarm_still_refuses_it() {
    let document = serde_json::json!({
        "gpu_cache": {
            "schema_version": 1,
            "sampled_at_unix_ms": 1000,
            "codec": {
                "capability": "available",
                "state": "ready",
                "refusal_reason": null
            },
            "logical_cached_bytes": 268435456,
            "physical_cache_slab_bytes": 8388608,
            "codec_workspace_bytes": 1048576,
            "compressed_payload_bytes": 33554432,
            "raw_payload_bytes": 4194304,
            "metadata_bytes": 65536,
            "raw_bypass_bytes": 16777216,
            "codec_integrity_errors": 0,
            "codec_decode_errors": 1,
            "codec_timeouts": 0
        }
    });

    // The record is the sample, aged or not, carrying its own timestamp.
    let record = cache_record(Some(&document)).expect("an aged sample is still a record");
    assert_eq!(record.logical_cached_bytes, 268435456);
    assert_eq!(
        record.sampled_at_unix_ms, 1000,
        "the record keeps the producer's clock, never a re-stamped one"
    );
    assert!(
        !record.is_fresh_at(6001, ramshared_vram::TELEMETRY_MAX_AGE_MS),
        "the fixture must actually be aged for this to prove anything"
    );

    // The alarm built on the admission collapse wants exactly "present but
    // not trustworthy", so it still fires for that same document. Both
    // behaviours are correct and they are deliberately not the same call.
    assert!(gpu_cache_telemetry_invalid_or_stale(
        Some(&document),
        Some(6001)
    ));

    // Absence is the other fact and stays absence. A missing key, a JSON
    // `null` and a body that will not parse all publish `None` — never a
    // zero-filled sample the worker never produced.
    let missing_key = serde_json::json!({ "ok": true });
    assert_eq!(cache_record(Some(&missing_key)), None);
    let nulled = serde_json::json!({ "gpu_cache": null });
    assert_eq!(cache_record(Some(&nulled)), None);
    let nonsense = serde_json::json!({ "gpu_cache": { "logical_cached_bytes": 16 } });
    assert_eq!(cache_record(Some(&nonsense)), None);
    assert_eq!(cache_record(None), None);
}

#[test]
fn gpu_cache_absent_or_null_is_honest_not_stale() {
    assert!(!gpu_cache_telemetry_invalid_or_stale(None, Some(5000)));

    let absent_key = serde_json::json!({
        "schema_version": 1,
        "ok": true,
        "cache_state": "ACTIVE"
    });
    assert!(!gpu_cache_telemetry_invalid_or_stale(
        Some(&absent_key),
        Some(5000)
    ));

    // `"gpu_cache": null` is "no cache telemetry". It must not alarm, and
    // it must never be read as a measured empty cache.
    let not_measured = serde_json::json!({
        "schema_version": 1,
        "ok": true,
        "gpu_cache": null
    });
    assert!(!gpu_cache_telemetry_invalid_or_stale(
        Some(&not_measured),
        Some(5000)
    ));
    assert!(!gpu_cache_telemetry_invalid_or_stale(
        Some(&not_measured),
        None
    ));
}

#[test]
fn gpu_cache_published_but_untrustworthy_is_stale() {
    let trusted = serde_json::json!({
        "gpu_cache": {
            "schema_version": 1,
            "sampled_at_unix_ms": 1000,
            "codec": {
                "capability": "available",
                "state": "ready",
                "refusal_reason": null
            },
            "logical_cached_bytes": 268435456,
            "physical_cache_slab_bytes": 8388608,
            "codec_workspace_bytes": 1048576,
            "compressed_payload_bytes": 33554432,
            "raw_payload_bytes": 4194304,
            "metadata_bytes": 65536,
            "raw_bypass_bytes": 16777216,
            "codec_integrity_errors": 0,
            "codec_decode_errors": 1,
            "codec_timeouts": 0
        }
    });
    assert!(!gpu_cache_telemetry_invalid_or_stale(
        Some(&trusted),
        Some(5000)
    ));

    // Outside the shared 5s trust window: stale. A stale occupancy figure
    // silently republished is the "stale green" the dashboard refuses.
    assert!(gpu_cache_telemetry_invalid_or_stale(
        Some(&trusted),
        Some(6001)
    ));

    // Unknown schema: refused, never reinterpreted.
    let mut unknown_version = trusted.clone();
    unknown_version["gpu_cache"]["schema_version"] = serde_json::json!(99);
    assert!(gpu_cache_telemetry_invalid_or_stale(
        Some(&unknown_version),
        Some(5000)
    ));

    // Published but malformed: invalid.
    let malformed = serde_json::json!({"gpu_cache": {"logical_cached_bytes": 16}});
    assert!(gpu_cache_telemetry_invalid_or_stale(
        Some(&malformed),
        Some(5000)
    ));

    // Published object with no clock reading: cannot be trusted.
    assert!(gpu_cache_telemetry_invalid_or_stale(Some(&trusted), None));
}

#[test]
fn stress_readiness_refuses_missing_control_plane_and_cache() {
    let mut snapshot = ready_snapshot();
    assert!(stress_readiness_from_snapshot(&snapshot).is_ok());
    assert!(stress_continuation_from_snapshot(&snapshot).is_ok());
    snapshot.control_state = ControlState::Guarded;
    assert!(stress_readiness_from_snapshot(&snapshot).is_err());
    assert!(stress_continuation_from_snapshot(&snapshot).is_ok());
    snapshot.control_state = ControlState::Critical;
    assert!(stress_readiness_from_snapshot(&snapshot).is_err());
    assert!(stress_continuation_from_snapshot(&snapshot).is_err());
    snapshot.control_state = ControlState::Emergency;
    assert!(stress_continuation_from_snapshot(&snapshot).is_err());
    snapshot.control_state = ControlState::SafeMode;
    assert!(stress_continuation_from_snapshot(&snapshot).is_err());
    snapshot.control_state = ControlState::Healthy;
    snapshot.cache_state = CacheState::Unavailable;
    assert!(stress_readiness_from_snapshot(&snapshot).is_err());
    let err = stress_continuation_from_snapshot(&snapshot)
        .expect_err("a non-serving cache must refuse continuation");
    assert!(
        refused_tokens(&err).contains(&"cache_state_refuses"),
        "a non-serving cache must name cache_state_refuses: {err}"
    );
    assert!(
        err.contains("cache_state: Unavailable"),
        "the refusal must name the observed cache state: {err}"
    );
    // `Restricted` is the cache reclaiming under pressure — the product's
    // own promise. Admission still requires a fully healthy cache, but a
    // running pressure workload must survive the reaction it induces.
    snapshot.cache_state = CacheState::Restricted;
    assert!(stress_readiness_from_snapshot(&snapshot).is_err());
    assert!(
        stress_continuation_from_snapshot(&snapshot).is_ok(),
        "continuation must tolerate a cache that restricted under the load the run itself creates"
    );
    snapshot.cache_state = CacheState::Active;
    snapshot.guardian_state = GuardianState::Blocked;
    assert!(stress_readiness_from_snapshot(&snapshot).is_err());
    let err = stress_continuation_from_snapshot(&snapshot)
        .expect_err("a blocked guardian must refuse continuation");
    assert!(
        refused_tokens(&err).contains(&"guardian_unhealthy"),
        "a non-healthy guardian must name guardian_unhealthy: {err}"
    );
    snapshot.guardian_state = GuardianState::Healthy;
    snapshot
        .measurement_errors
        .push("cache_status_not_current".into());
    assert!(stress_readiness_from_snapshot(&snapshot).is_err());
    assert!(stress_continuation_from_snapshot(&snapshot).is_err());
}

#[test]
fn pressure_gate_tolerates_admission_close_but_not_escalation() {
    // The supervisor's first protective step is CloseAdmission. A pressure
    // qualification is its own pressure source and will trip that step, so
    // continuation must survive it. Destructive escalation is the real
    // stop signal, and a degraded cascade must abort either gate.
    let snapshot = &mut ready_snapshot();
    snapshot.control_state = ControlState::Guarded;

    assert!(!PressureGate::Continuation.control_state_refuses(ControlState::Guarded));
    assert!(PressureGate::Continuation.control_state_refuses(ControlState::Critical));
    assert!(PressureGate::Continuation.control_state_refuses(ControlState::Emergency));
    assert!(PressureGate::Continuation.control_state_refuses(ControlState::SafeMode));
    assert!(PressureGate::Admission.control_state_refuses(ControlState::Guarded));
    assert!(!PressureGate::Admission.control_state_refuses(ControlState::Healthy));

    // Admission closes under our own pressure; continuation keeps going.
    assert!(stress_continuation_from_snapshot(snapshot).is_ok());
    assert!(stress_readiness_from_snapshot(snapshot).is_err());

    // Structural degradation aborts continuation even while only guarded,
    // and the refusal names the predicate that fired. A gate that refuses
    // with an empty reason list is undiagnosable from its own artifact,
    // which is exactly how a three-tier stress halt was lost once already.
    snapshot.order_ok = false;
    let err = stress_continuation_from_snapshot(snapshot)
        .expect_err("a disordered tier chain must refuse continuation");
    assert!(
        refused_tokens(&err).contains(&"tier_order"),
        "the refusal must name tier_order, not just an empty reason list: {err}"
    );
    snapshot.order_ok = true;
    snapshot.capacity_guaranteed = Some(false);
    let err = stress_continuation_from_snapshot(snapshot)
        .expect_err("an unguaranteed capacity must refuse continuation");
    assert!(
        refused_tokens(&err).contains(&"capacity_not_guaranteed"),
        "the refusal must name capacity_not_guaranteed: {err}"
    );
    // The snapshot's own states travel with the refusal, so a reader can
    // tell a cache that went non-Active from a guardian that went unhealthy
    // without re-deriving the snapshot.
    assert!(
        err.contains("cache_state: Active") && err.contains("guardian_state: Healthy"),
        "the refusal must carry the snapshot states it observed: {err}"
    );
}

#[test]
fn capacity_guarantee_witness_is_witness_backed_and_never_folds_a_stat_failure_to_false() {
    // A measured regular file is a positive backing.
    assert_eq!(capacity_guaranteed_from_stat(Ok(true)), Some(true));
    // A measured non-file at the marker path is a refutation.
    assert_eq!(capacity_guaranteed_from_stat(Ok(false)), Some(false));
    // Measured absence is a refutation: the marker is the product's
    // positive guarantee file, so its absence is a fact about it.
    assert_eq!(
        capacity_guaranteed_from_stat(Err(std::io::Error::from(std::io::ErrorKind::NotFound))),
        Some(false)
    );
    // Every other stat failure is a witness this frame never took.
    // `Path::is_file()` used to fold these to `false`, publishing a
    // measured `capacity_not_guaranteed` on a frame that never looked.
    for kind in [
        std::io::ErrorKind::PermissionDenied,
        std::io::ErrorKind::TimedOut,
        std::io::ErrorKind::Interrupted,
    ] {
        assert_eq!(
            capacity_guaranteed_from_stat(Err(std::io::Error::from(kind))),
            None,
            "a {kind:?} on the way to the marker must be never-taken, not a refutation"
        );
    }
}

#[test]
fn pressure_gate_never_publishes_capacity_not_guaranteed_from_an_unmeasured_guarantee() {
    let mut snapshot = ready_snapshot();
    snapshot.capacity_guaranteed = None;
    snapshot
        .measurement_errors
        .push("capacity_guarantee_unreadable".into());
    let err = stress_continuation_from_snapshot(&snapshot)
        .expect_err("an unmeasured capacity guarantee must refuse continuation");
    let tokens = refused_tokens(&err);
    assert!(
        tokens.contains(&"capacity_guarantee_unmeasured"),
        "a never-taken stat must name capacity_guarantee_unmeasured: {err}"
    );
    assert!(
        !tokens.contains(&"capacity_not_guaranteed"),
        "a never-taken stat must not publish the measured-refutation token: {err}"
    );
}

#[test]
fn pressure_gate_capacity_not_guaranteed_is_witness_backed_by_a_measured_refutation() {
    let mut snapshot = ready_snapshot();
    snapshot.capacity_guaranteed = Some(false);
    let err = stress_continuation_from_snapshot(&snapshot)
        .expect_err("a measured absent capacity marker must refuse continuation");
    let tokens = refused_tokens(&err);
    assert!(
        tokens.contains(&"capacity_not_guaranteed"),
        "a measured absent marker must name capacity_not_guaranteed: {err}"
    );
    assert!(
        !tokens.contains(&"capacity_guarantee_unmeasured"),
        "a measured refutation must not borrow the never-taken arm: {err}"
    );

    // The measured positive refuses nothing on this witness: the gate is
    // open, and no capacity token appears at all.
    snapshot.capacity_guaranteed = Some(true);
    assert!(
        stress_continuation_from_snapshot(&snapshot).is_ok(),
        "a measured capacity file must not refuse the gate on its own"
    );
}

#[test]
fn canonicalize_swap_path_table() {
    assert_eq!(canonicalize_swap_path("/nbd0"), "/dev/nbd0");
    assert_eq!(canonicalize_swap_path("/dev/nbd0"), "/dev/nbd0");
    assert_eq!(canonicalize_swap_path("nbd0"), "/dev/nbd0");
    assert_eq!(canonicalize_swap_path("/zram0"), "/dev/zram0");
    assert_eq!(canonicalize_swap_path("  /ublkb0  "), "/dev/ublkb0");
    assert_eq!(canonicalize_swap_path(""), "");
}

#[test]
fn allowlist_rejects_disk_paths() {
    assert!(is_allowlisted_managed_path("/dev/nbd0"));
    assert!(is_allowlisted_managed_path("/zram0"));
    assert!(is_allowlisted_managed_path("/dev/ublkb0"));
    assert!(is_allowlisted_managed_path("/dev/mapper/ramshared0"));
    assert!(!is_allowlisted_managed_path("/dev/sdc"));
    assert!(!is_allowlisted_managed_path("/dev/sdb"));
    assert!(!is_allowlisted_managed_path("/dev/nbd0-backup"));
    assert!(!is_allowlisted_managed_path("/tmp/nbd0"));
    assert!(!is_allowlisted_managed_path("/swap/ramshared-backup"));
}

#[test]
fn malformed_managed_paths_are_refused() {
    for path in [
        "/dev/dev/nbd0",
        "/dev//nbd0",
        "//nbd0",
        "/tmp/../dev/nbd0",
        "/dev/nbd0/extra",
    ] {
        assert!(!is_allowlisted_managed_path(path), "accepted {path}");
    }
}

#[test]
fn status_daemon_requires_owned_pid_record_and_exact_comm() {
    assert_eq!(
        daemon_alive_pid_from(Some("42\n"), |pid| (pid == 42)
            .then(|| "ramsharedd\n".into())),
        (true, Some(42))
    );
    for record in [
        None,
        Some(""),
        Some("0"),
        Some("-1"),
        Some("42 43"),
        Some("abc"),
    ] {
        assert_eq!(
            daemon_alive_pid_from(record, |_| Some("ramsharedd".into())),
            (false, None)
        );
    }
    assert_eq!(
        daemon_alive_pid_from(Some("42"), |_| Some("foreign-daemon\n".into())),
        (false, None)
    );
    assert_eq!(daemon_alive_pid_from(Some("42"), |_| None), (false, None));
}

#[test]
fn similar_swap_names_are_not_managed_devices() {
    let entries = parse_proc_swaps(
        "Filename Type Size Used Priority\n\
             /swap/nbd0-backup file 1024 99 -2\n\
             /dev/nbd01 partition 1024 7 -3\n",
    );
    assert!(!entries[0].is_managed_or_orphan_vram_tier());
    assert!(entries[1].is_managed_or_orphan_vram_tier());
    assert!(!active_vram_block_swap(&entries[..1]));
    assert!(!is_nbd_device_path("/swap/nbd0-backup"));
    assert_ne!(entries[1].canonical_path(), "/dev/nbd0");
}

#[test]
fn orphan_plan_zero_used_is_detected_without_recovery() {
    let e = parse_proc_swaps(
        "Filename Type Size Used Priority\n\
             /dev/sdc partition 8388608 100 -2\n\
             /zram0 partition 1048576 0 200\n\
             /nbd0 partition 1048576 0 100\n",
    );
    assert_eq!(
        plan_orphan_action(&e, false),
        OrphanPlan::DetectedUnboundZeroUsed
    );
    assert_eq!(plan_orphan_action(&e, true), OrphanPlan::None);
}

#[test]
fn orphan_plan_dirty_nbd_is_refuse() {
    let e = parse_proc_swaps(
        "Filename Type Size Used Priority\n\
             /dev/nbd0 partition 1048576 4096 100\n\
             /dev/sdc partition 8388608 0 -2\n",
    );
    assert_eq!(
        plan_orphan_action(&e, false),
        OrphanPlan::RefuseDirtyBackend
    );
}

#[test]
fn orphan_plan_clean_disk_only_is_none() {
    let e = parse_proc_swaps(
        "Filename Type Size Used Priority\n\
             /dev/sdc partition 8388608 0 -2\n",
    );
    assert_eq!(plan_orphan_action(&e, false), OrphanPlan::None);
}

#[test]
fn legacy_migration_plan_accepts_single_clean_nbd_and_zram() {
    let entries = parse_proc_swaps(
        "Filename Type Size Used Priority\n\
             /dev/sdb partition 4194304 0 -2\n\
             /dev/zram0 partition 1048572 4140 100\n\
             /dev/nbd0 partition 4194300 0 50\n",
    );

    let plan = plan_legacy_migration(&entries).expect("eligible legacy plan");

    assert_eq!(plan.nbd.canonical_path(), "/dev/nbd0");
    assert_eq!(plan.nbd.used_kb, 0);
    assert_eq!(
        plan.zram.as_ref().map(SwapEntry::canonical_path),
        Some("/dev/zram0".into())
    );
}

#[test]
fn legacy_migration_plan_refuses_ghost_dirty_duplicate_or_ublk() {
    for fixture in [
        "Filename Type Size Used Priority\n/dev/nbd0\\040(deleted) partition 1024 0 50\n",
        "Filename Type Size Used Priority\n/dev/nbd0 partition 1024 1 50\n",
        "Filename Type Size Used Priority\n/dev/nbd0 partition 1024 0 50\n/dev/nbd1 partition 1024 0 50\n",
        "Filename Type Size Used Priority\n/dev/nbd0 partition 1024 0 50\n/dev/ublkb0 partition 1024 0 40\n",
    ] {
        assert!(
            plan_legacy_migration(&parse_proc_swaps(fixture)).is_err(),
            "{fixture}"
        );
    }
}

#[test]
fn defaults_to_auto_resolved_nbd_on_wsl2_or_nbd_swap_dev() {
    let args = parse(&[]).unwrap();
    // Default is Auto; on WSL2 it resolves to Nbd, then admission refuses it.
    // Off-WSL2 without /dev/ublk-control also → Nbd; with control → Ublk.
    assert!(matches!(args.transport, Transport::Nbd | Transport::Ublk));
    if is_wsl2() {
        assert_eq!(args.transport, Transport::Nbd);
    }
    assert_eq!(args.swap_dev, "/dev/nbd0");
    assert_eq!(args.connections, 1);
}

#[test]
fn auto_transport_flag_resolves_like_default() {
    let args = parse(&["--transport", "auto"]).unwrap();
    if is_wsl2() {
        assert_eq!(args.transport, Transport::Nbd);
    } else {
        assert!(matches!(args.transport, Transport::Nbd | Transport::Ublk));
    }
}

#[test]
fn parses_ublk_transport_and_generic_swap_dev() {
    // Parse recognises the tokens and records them verbatim. Whether the
    // recorded transport may run is `check_transport`'s decision, made on
    // the effect path and on every host — not this layer's.
    let args = parse(&["--transport", "ublk", "--swap-dev", "/dev/ublkb0"]).unwrap();
    assert_eq!(args.transport, Transport::Ublk);
    assert_eq!(args.swap_dev, "/dev/ublkb0");
}

#[test]
fn keeps_legacy_nbd_arg_as_swap_dev_alias() {
    let args = parse(&["--nbd", "/dev/nbd3"]).unwrap();
    assert_eq!(args.transport, Transport::Nbd);
    assert_eq!(args.swap_dev, "/dev/nbd3");
}

#[test]
fn rejects_multi_connection_ublk_for_single_ring_design() {
    // Pure design constraint, host-independent: ublk is one ring here.
    let err = ensure_single_ring_ublk(Transport::Ublk, 2).unwrap_err();
    assert!(err.to_string().contains("--connections"));
    assert!(ensure_single_ring_ublk(Transport::Ublk, 1).is_ok());
    assert!(ensure_single_ring_ublk(Transport::Nbd, 8).is_ok());

    // Through parse, the surface the peer actually hits. The constraint
    // follows the transport that will be honoured, so it fires on every
    // host — parse does not wait for the effect-path transport gate.
    let err = parse(&["--transport", "ublk", "--connections", "2"]).unwrap_err();
    assert!(
        err.to_string().contains("--connections"),
        "single-ring constraint must surface: {err}"
    );
}

#[test]
fn parse_swaps_normal_and_ghost_backslash() {
    let text = "\
Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority
/dev/sdb                                partition\t8388608\t\t100\t\t-2
/dev/ublkb0\\040(deleted)                partition\t524284\t\t117504\t\t-3
/dev/zram0                              partition\t1048576\t\t0\t\t200
";
    let e = parse_proc_swaps(text);
    assert_eq!(e.len(), 3);
    assert!(e[1].is_ghost());
    assert!(e[1].is_managed_or_orphan_vram_tier());
    assert_eq!(e[1].used_kb, 117504);
    assert!(!e[0].is_managed_or_orphan_vram_tier());
    assert!(e[2].is_managed_or_orphan_vram_tier());
}

#[test]
fn parse_swaps_ghost_with_real_space() {
    let text = "\
Filename Type Size Used Priority
/dev/ublkb0 (deleted) partition 524284 10 -3
";
    let e = parse_proc_swaps(text);
    assert_eq!(e.len(), 1);
    assert!(e[0].is_ghost());
    assert!(e[0].filename.contains("ublkb0"));
}

#[test]
fn is_ghost_swap_filename_names_only_a_lost_device() {
    // Both kernel spellings of "the block device behind this path is gone".
    assert!(is_ghost_swap_filename("/dev/zram0 (deleted)"));
    assert!(is_ghost_swap_filename("/dev/ublkb0\\040(deleted)"));
    // A live path is not a ghost, and a filename that merely mentions the
    // word is not one either — the marker is the parenthesised form.
    assert!(!is_ghost_swap_filename("/dev/zram0"));
    assert!(!is_ghost_swap_filename("/dev/nbd0"));
    assert!(!is_ghost_swap_filename("/swap/deleted-notes"));
    assert!(!is_ghost_swap_filename("/swap/notes (archived)"));
    // The predicate is the entry rule too: one spelling, not two.
    let ghost = SwapEntry {
        filename: "/dev/zram0 (deleted)".to_string(),
        size_kb: 1,
        used_kb: 0,
        priority: 200,
    };
    assert_eq!(ghost.is_ghost(), is_ghost_swap_filename(&ghost.filename));
}

#[test]
fn fallback_swap_device_names_are_roles_not_host_devices() {
    // Tier 3 is whatever `/proc/swaps` puts in the fallback role. The
    // fixture names are synthetic on purpose: no host block-device name is
    // a product constant.
    let text = "\
Filename Type Size Used Priority
/dev/zram0 partition 1048576 0 200
/dev/nbd0 partition 8388608 100 100
/dev/ublkb0 partition 524284 10 -3
/dev/ramshared0 partition 8388608 0 100
/dev/fixture0 partition 16777216 0 -2
fixture1 partition 4194304 0 -2
/swap/swapfile file 12582912 0 -2
/dev/fixture2\\040(deleted) partition 4194304 0 -2
";
    let entries = parse_proc_swaps(text);
    let names = fallback_swap_device_names(&entries);

    assert_eq!(
        names,
        ["fixture0".to_string(), "fixture1".to_string()]
            .into_iter()
            .collect(),
        "only live fallback-role block devices contribute a diskstats name"
    );
}

#[test]
fn fallback_swap_device_names_join_path_to_kernel_basename() {
    // `/proc/diskstats` field 3 is a bare kernel name; `/proc/swaps` may
    // spell the same device as `/dev/…`, `/…` or bare. `has_device_path_shape`
    // accepts all three, so the join is the basename after the last `/`.
    for spelling in ["/dev/fixture0", "/fixture0", "fixture0"] {
        let text = format!("Filename Type Size Used Priority\n{spelling} partition 4096 0 -2\n");
        let entries = parse_proc_swaps(&text);
        let names = fallback_swap_device_names(&entries);
        assert_eq!(
            names,
            ["fixture0".to_string()].into_iter().collect(),
            "spelling {spelling} must join to the diskstats basename"
        );
    }
}

#[test]
fn fallback_swap_device_names_reject_swap_file_paths() {
    // A swap file has no diskstats row: the kernel attributes its I/O to
    // the filesystem's disk, which is not the fallback swap device. A
    // multi-component file path therefore contributes nothing, so the
    // root/data disk can never be claimed as tier 3.
    let text = "\
Filename Type Size Used Priority
/swap/swapfile file 12582912 0 -2
/mnt/fixture/swap\\040file file 12582912 0 -2
";
    let entries = parse_proc_swaps(text);
    assert!(
        fallback_swap_device_names(&entries).is_empty(),
        "swap-file paths must not name a diskstats device"
    );
}
