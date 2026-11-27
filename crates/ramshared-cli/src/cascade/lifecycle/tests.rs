// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/lifecycle/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

pub(super) use super::*;

/// SPEC DT-3: the published Tier 1 object carries the derived state, so
/// a reader of `status --json` sees the same fact the dashboard row
/// shows. The other tiers carry no such key.
#[test]
fn zram_tier_object_publishes_the_derived_state() {
    let sample = TierSample {
        present: true,
        prio: Some(200),
        size_kib: 65_536,
        used_kib: 4096,
    };
    let degraded = zram_tier_json(&sample, ZramTierState::Degraded);
    assert!(degraded.contains("\"state\":\"degraded\""), "{degraded}");
    assert!(degraded.contains("\"used_kib\":4096"), "{degraded}");
    assert_eq!(
        zram_tier_json(&sample, ZramTierState::Active),
        "{\"present\":true,\"prio\":200,\"size_kib\":65536,\"used_kib\":4096,\"state\":\"active\"}"
    );
    // The tiers without a backing device keep the plain object.
    assert_eq!(
        tier_json(&sample),
        "{\"present\":true,\"prio\":200,\"size_kib\":65536,\"used_kib\":4096}"
    );
}

pub(super) fn base() -> CascadeSnapshot {
    CascadeSnapshot {
        origin_cache_mode: Some(false),
        zram: TierSample {
            present: true,
            prio: Some(200),
            size_kib: 2_097_148,
            used_kib: 0,
        },
        vram: TierSample {
            present: true,
            prio: Some(100),
            size_kib: 2_097_148,
            used_kib: 0,
        },
        disk: TierSample {
            present: true,
            prio: Some(-2),
            size_kib: 8_388_608,
            used_kib: 0,
        },
        ghost: false,
        order_ok: true,
        daemon_alive: true,
        daemon_pid: Some(1),
        capacity_guaranteed: Some(true),
        disk_baseline_kib: Some(0),
        demote: DemoteSnapshot::default(),
        active_kib: DEFAULT_ACTIVE_KIB,
        control_state: ControlState::Healthy,
        origin_state: OriginState::Ready,
        cache_state: CacheState::Active,
        guardian_state: GuardianState::Healthy,
        cache_target_kib: Some(33_792),
        logical_capacity_kib: Some(2_097_148),
        vram_cached_kib: Some(0),
        gpu_headroom_kib: Some(2_097_152),
        gpu_budget: None,
        gpu_utilization: None,
        host_gpu_activity: None,
        gpu_cache: None,
        origin_cache_io: None,
        cache_fallback_reads: None,
        cache_hits: None,
        host_memory: None,
        host_storage: None,
        cache_invalidations: None,
        cache_releases: None,
        zram_compression: None,
        zram_tier_state: ZramTierState::Inactive,
        ssd_origin_written_kib: Some(0),
        fallback_swap_used_kib: Some(0),
        release_coherence: ReleaseCoherence::Coherent,
        measurement_errors: Vec::new(),
        topology: TopologyFacts::default(),
        cache_unavailable_reason: None,
        control_plane_state: None,
        control_plane_lease_deadline_ms: None,
    }
}

#[test]
fn an_active_origin_cache_is_using_vram_not_off() {
    let mut snap = base();
    snap.vram.present = false;
    snap.vram.used_kib = 0;
    snap.zram.used_kib = 0;
    snap.disk.used_kib = 0;
    snap.topology = TopologyFacts::default();
    snap.cache_state = CacheState::Active;
    let view = derive_lifecycle(&snap);
    assert_eq!(view.phase, CascadePhase::UsingVram);
    assert_eq!(view.phase_reason, "origin_cache_active");
    assert!(view.ok);
    assert_eq!(protection_state(&view, &snap), ProtectionState::Active);
    assert_eq!(protection_reason(&view, &snap), "origin_cache_active");
    assert!(status_ok(&view, &snap));
}

#[test]
fn occupied_origin_swap_does_not_claim_vram_use() {
    let mut snap = base();
    snap.origin_cache_mode = Some(true);
    snap.vram.present = true;
    snap.vram.used_kib = DEFAULT_ACTIVE_KIB * 2;
    snap.cache_state = CacheState::Off;
    let view = derive_lifecycle(&snap);
    assert_eq!(view.phase, CascadePhase::UsingOrigin);
    assert_eq!(view.phase_reason, "origin_swap_used");
    assert_eq!(protection_reason(&view, &snap), "origin_swap_in_use");
    assert_ne!(view.phase, CascadePhase::UsingVram);
    snap.origin_cache_mode = None;
    let unknown = derive_lifecycle(&snap);
    assert_eq!(unknown.phase, CascadePhase::UsingTier);
    assert_eq!(unknown.phase_reason, "tier_swap_backing_unverified");
    snap.origin_cache_mode = Some(true);
    snap.vram.used_kib = 0;
    snap.zram.used_kib = 0;
    let armed = derive_lifecycle(&snap);
    assert_eq!(armed.phase_reason, "armed_low_tier_used");
    assert_eq!(protection_reason(&armed, &snap), "origin_tier_armed");
    snap.zram.used_kib = DEFAULT_ACTIVE_KIB;
    let zram = derive_lifecycle(&snap);
    assert_eq!(protection_reason(&zram, &snap), "origin_tier_ready");
}

/// SPEC DT-1: a writeback topology is not judged by the swap-tier rule.
///
/// The rule asks whether the VRAM **swap tier** is idle while the zram
/// fills. In the writeback topology the tier is not a swap device at all —
/// it is the zram's writeback target — so "no VRAM use" describes a
/// device that cannot be idle while the zram spills into it. The verdict
/// must be `Ready`, and the reason must name the topology.
#[test]
fn a_writeback_topology_is_not_judged_by_the_swap_tier_rules() {
    let mut snap = base();
    // The writeback topology: the tier is not in the swap table, the
    // zram's backing device is the tier, and the zram is nearly full.
    snap.vram.present = false;
    snap.vram.used_kib = 0;
    snap.zram.used_kib = snap.zram.size_kib * 99 / 100;
    snap.topology = TopologyFacts {
        zram_backing_dev: Some("/dev/nbd0".to_string()),
        tier_device: Some("nbd0".to_string()),
        writeback_target_is_tier: true,
    };
    let view = LifecycleView {
        phase: CascadePhase::UsingZram,
        phase_reason: "zram_used_ge_threshold",
        ok: true,
        reasons: Vec::new(),
    };
    assert_eq!(protection_state(&view, &snap), ProtectionState::Ready);
    assert_eq!(
        protection_reason(&view, &snap),
        "zram_writeback_target_ready"
    );
    assert!(status_ok(&view, &snap));
}

/// The same rule keeps its real-risk arm: a zram with **no** spill target
/// is a dead end whatever the tier table says, and the verdict must say
/// at risk.
#[test]
fn a_writeback_topology_without_a_spill_target_is_at_risk() {
    let mut snap = base();
    snap.vram.present = false;
    snap.zram.used_kib = snap.zram.size_kib * 99 / 100;
    snap.topology = TopologyFacts {
        zram_backing_dev: None,
        tier_device: Some("nbd0".to_string()),
        writeback_target_is_tier: true,
    };
    let view = LifecycleView {
        phase: CascadePhase::UsingZram,
        phase_reason: "zram_used_ge_threshold",
        ok: true,
        reasons: Vec::new(),
    };
    assert_eq!(protection_state(&view, &snap), ProtectionState::AtRisk);
    assert_eq!(
        protection_reason(&view, &snap),
        "zram_utilization_ge_80_without_spill_target"
    );
}

/// The swap posture is untouched: a zram above the floor with the VRAM
/// swap tier idle is still at risk, exactly as before this rule learned
/// about the writeback topology.
#[test]
fn a_writeback_topology_is_armed_and_not_reported_off() {
    // The tier in the writeback topology is not a swap device, so the
    // swap table cannot say the cascade is armed — the topology can. A
    // live, serving cascade must never read as "off".
    let mut snap = base();
    snap.vram.present = false;
    snap.cache_state = CacheState::Off;
    snap.topology = TopologyFacts {
        zram_backing_dev: Some("/dev/nbd0".to_string()),
        tier_device: Some("nbd0".to_string()),
        writeback_target_is_tier: true,
    };
    let view = derive_lifecycle(&snap);
    assert_eq!(view.phase, CascadePhase::Armed);
    assert!(view.ok);
    assert_eq!(protection_state(&view, &snap), ProtectionState::Ready);
}

#[test]
fn preparation_refuses_off_display_with_live_daemon_or_observation_errors() {
    let mut snap = base();
    snap.vram.present = false;
    snap.zram.present = false;
    snap.cache_state = CacheState::Off;
    assert_eq!(derive_lifecycle(&snap).phase, CascadePhase::Off);
    assert!(!super::super::status_view::preparation_snapshot_is_off(
        &snap
    ));
    snap.daemon_alive = false;
    snap.measurement_errors.push("unavailable".into());
    assert!(!super::super::status_view::preparation_snapshot_is_off(
        &snap
    ));
    snap.measurement_errors.clear();
    snap.zram.present = true;
    assert!(!super::super::status_view::preparation_snapshot_is_off(
        &snap
    ));
    snap.zram.present = false;
    snap.topology.writeback_target_is_tier = true;
    assert!(!super::super::status_view::preparation_snapshot_is_off(
        &snap
    ));
    snap.topology.writeback_target_is_tier = false;
    assert!(super::super::status_view::preparation_snapshot_is_off(
        &snap
    ));
}

#[test]
fn preparation_observes_injected_processes_records_and_units() {
    let root = std::env::temp_dir().join(format!("preparation-sources-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let proc_root = root.join("proc");
    std::fs::create_dir(&proc_root).unwrap();
    let record = root.join("pid");
    let mut snap = base();
    snap.vram.present = false;
    snap.zram.present = false;
    snap.daemon_alive = false;
    snap.cache_state = CacheState::Off;
    let check = |unit: fn(&str) -> Option<bool>| {
        super::super::preparation::preparation_from_sources(
            &snap,
            &[&record],
            &proc_root,
            None,
            super::super::preparation::PreparationUnits {
                units: &["fixture.service"],
                uncooperative_units: &["legacy.service"],
                observe_unit: unit,
                observe_mask: unit,
            },
        )
    };
    assert!(check(|_| Some(true)).unwrap());
    assert!(!check(|unit| Some(unit != "legacy.service")).unwrap());
    assert!(
        check(|unit| if unit == "legacy.service" {
            None
        } else {
            Some(true)
        })
        .is_err()
    );
    std::fs::write(&record, b"record").unwrap();
    assert!(!check(|_| Some(true)).unwrap());
    std::fs::remove_file(&record).unwrap();
    let process = proc_root.join("42");
    std::fs::create_dir(&process).unwrap();
    std::fs::write(
        process.join("stat"),
        "42 (ramsharedd) S 1 1 1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 1 0 123\n",
    )
    .unwrap();
    std::fs::write(process.join("comm"), "ramsharedd\n").unwrap();
    assert!(!check(|_| Some(true)).unwrap());
    std::fs::remove_dir_all(process).unwrap();
    assert!(check(|_| None).is_err());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn preparation_stops_observations_after_a_refusal() {
    let mut snap = base();
    snap.vram.present = false;
    snap.zram.present = false;
    snap.daemon_alive = false;
    snap.cache_state = CacheState::Off;
    let check = super::super::status_view::preparation_gate;
    assert!(check(&snap, &|| Ok(true), &|| Ok(false), &|| Ok(true)).unwrap());
    assert!(
        !check(
            &snap,
            &|| Ok(false),
            &|| panic!("process scan after refusal"),
            &|| panic!("unit query after refusal")
        )
        .unwrap()
    );
    assert!(
        !check(&snap, &|| Ok(true), &|| Ok(true), &|| panic!(
            "unit query after daemon"
        ))
        .unwrap()
    );
    assert!(!check(&snap, &|| Ok(true), &|| Ok(false), &|| Ok(false)).unwrap());
    assert!(
        check(
            &snap,
            &|| Err(super::super::CascadeError::Io("unknown".into())),
            &|| Ok(false),
            &|| Ok(true)
        )
        .is_err()
    );
    assert!(
        check(
            &snap,
            &|| Ok(true),
            &|| Err(super::super::CascadeError::Io("unknown".into())),
            &|| Ok(true)
        )
        .is_err()
    );
    assert!(
        check(&snap, &|| Ok(true), &|| Ok(false), &|| Err(
            super::super::CascadeError::Io("unknown".into())
        ))
        .is_err()
    );
    snap.daemon_alive = true;
    assert!(
        !check(
            &snap,
            &|| panic!("record query after active snapshot"),
            &|| panic!("process query"),
            &|| panic!("unit query")
        )
        .unwrap()
    );
}

#[test]
fn preparation_requires_every_unit_to_be_observed_quiescent() {
    let units = ["one.service", "two.service"];
    assert!(
        super::super::status_view::preparation_units_quiescent(&units, |_| Some(true)).unwrap()
    );
    assert!(
        !super::super::status_view::preparation_units_quiescent(&units, |unit| Some(
            unit != "two.service"
        ))
        .unwrap()
    );
    assert!(super::super::status_view::preparation_units_quiescent(&units, |_| None).is_err());
}

#[test]
fn preparation_refuses_present_linked_or_unobservable_runtime_records() {
    let root = std::env::temp_dir().join(format!("preparation-records-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let record = root.join("pid");
    assert!(super::super::status_view::preparation_records_absent(&[&record]).unwrap());
    std::fs::write(&record, b"stale").unwrap();
    assert!(!super::super::status_view::preparation_records_absent(&[&record]).unwrap());
    std::fs::remove_file(&record).unwrap();
    std::os::unix::fs::symlink("missing", &record).unwrap();
    assert!(!super::super::status_view::preparation_records_absent(&[&record]).unwrap());
    std::fs::remove_file(&record).unwrap();
    std::fs::write(&record, b"not a directory").unwrap();
    assert!(
        super::super::status_view::preparation_records_absent(&[&record.join("child")]).is_err()
    );
    std::fs::remove_dir_all(root).unwrap();
}

/// The swap posture is untouched: without the writeback target the tier
/// still has to be a swap device for the cascade to be armed.
#[test]
fn the_swap_posture_still_needs_the_tier_in_the_swap_table() {
    let mut snap = base();
    snap.vram.present = false;
    snap.cache_state = CacheState::Off;
    let view = derive_lifecycle(&snap);
    assert_eq!(view.phase, CascadePhase::Off);
    assert_eq!(view.phase_reason, "no_product_cascade");
}

#[test]
fn the_swap_posture_keeps_its_at_risk_arm() {
    let mut snap = base();
    snap.vram.used_kib = 0;
    snap.zram.used_kib = snap.zram.size_kib * 99 / 100;
    let view = LifecycleView {
        phase: CascadePhase::UsingZram,
        phase_reason: "zram_used_ge_threshold",
        ok: true,
        reasons: Vec::new(),
    };
    assert_eq!(protection_state(&view, &snap), ProtectionState::AtRisk);
    assert_eq!(
        protection_reason(&view, &snap),
        "zram_utilization_ge_80_without_vram_use"
    );
}

/// A published reason explains the loss, but does not make the cache serve.
/// Overall status must expose the degraded cache while protection stays active.
#[test]
fn explained_cache_unavailability_is_guarded_while_protection_remains_active() {
    let mut snap = base();
    snap.cache_state = CacheState::Off;
    snap.cache_unavailable_reason =
        Some("the isolated GPU cache worker exited while the cache was OFF".to_string());
    let view = LifecycleView {
        phase: CascadePhase::UsingVram,
        phase_reason: "vram_used_ge_threshold",
        ok: true,
        reasons: Vec::new(),
    };
    for state in [CacheState::Off, CacheState::Unavailable, CacheState::Stuck] {
        snap.cache_state = state;
        assert_eq!(overall_state(&view, &snap), OverallState::Guarded);
        assert!(!status_ok(&view, &snap));
        assert_eq!(protection_state(&view, &snap), ProtectionState::Active);
        let json = render_status_json(&view, &snap, "2026-11-16T00:00:00Z");
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["overall_state"], "GUARDED");
        assert_eq!(parsed["protection_state"], "ACTIVE");
        assert_eq!(parsed["ok"], false);
    }
}

/// The strict arm stays strict: an unexplained cache absence is still a
/// mystery worth blocking on.
#[test]
fn an_unexplained_cache_unavailability_is_still_blocked() {
    let mut snap = base();
    snap.cache_state = CacheState::Off;
    snap.cache_unavailable_reason = None;
    let view = LifecycleView {
        phase: CascadePhase::Armed,
        phase_reason: "armed",
        ok: true,
        reasons: Vec::new(),
    };
    assert_eq!(overall_state(&view, &snap), OverallState::Blocked);
    assert!(!status_ok(&view, &snap));
}

/// The published document carries the topology and the cache's cause: the
/// role is a fact a reader needs, and the schema moves with it.
#[test]
fn the_status_document_publishes_the_tier_role_and_the_cache_cause() {
    let mut snap = base();
    snap.topology = TopologyFacts {
        zram_backing_dev: Some("/dev/nbd0".to_string()),
        tier_device: Some("nbd0".to_string()),
        writeback_target_is_tier: true,
    };
    snap.cache_unavailable_reason = Some("the worker exited".to_string());
    let view = LifecycleView {
        phase: CascadePhase::UsingZram,
        phase_reason: "zram_used_ge_threshold",
        ok: true,
        reasons: Vec::new(),
    };
    let json = render_status_json(&view, &snap, "2026-10-20T00:00:00-03:00");
    assert!(json.contains("\"schema_version\":5"), "{json}");
    assert!(
        json.contains("\"capacity\":{\"guaranteed_kib\":2097148,\"cache_target_kib\":33792"),
        "{json}"
    );
    assert!(json.contains("\"tier\":\"writeback_target\""), "{json}");
    assert!(json.contains("\"writeback_target_is_tier\":true"), "{json}");
    assert!(
        json.contains("\"cache_unavailable_reason\":\"the worker exited\""),
        "{json}"
    );
}

#[test]
fn phase_off_when_no_tiers_no_daemon() {
    let s = CascadeSnapshot {
        origin_cache_mode: Some(false),
        zram: TierSample::default(),
        vram: TierSample::default(),
        disk: TierSample::default(),
        ghost: false,
        order_ok: true,
        daemon_alive: false,
        daemon_pid: None,
        capacity_guaranteed: Some(false),
        disk_baseline_kib: None,
        demote: DemoteSnapshot::default(),
        active_kib: DEFAULT_ACTIVE_KIB,
        control_state: ControlState::Healthy,
        origin_state: OriginState::Off,
        cache_state: CacheState::Off,
        guardian_state: GuardianState::Healthy,
        cache_target_kib: None,
        logical_capacity_kib: None,
        vram_cached_kib: None,
        gpu_headroom_kib: None,
        gpu_budget: None,
        gpu_utilization: None,
        host_gpu_activity: None,
        gpu_cache: None,
        origin_cache_io: None,
        cache_fallback_reads: None,
        cache_hits: None,
        host_memory: None,
        host_storage: None,
        cache_invalidations: None,
        cache_releases: None,
        zram_compression: None,
        zram_tier_state: ZramTierState::Inactive,
        ssd_origin_written_kib: None,
        fallback_swap_used_kib: Some(5_000),
        release_coherence: ReleaseCoherence::Coherent,
        measurement_errors: Vec::new(),
        topology: TopologyFacts::default(),
        cache_unavailable_reason: None,
        control_plane_state: None,
        control_plane_lease_deadline_ms: None,
    };

    let v = derive_lifecycle(&s);
    assert_eq!(v.phase, CascadePhase::Off);
    assert_eq!(v.phase_reason, "no_product_cascade");
}

#[test]
fn phase_armed_low_vram_used() {
    let mut s = base();
    s.vram.used_kib = 176;
    s.zram.used_kib = 100;
    let v = derive_lifecycle(&s);
    assert_eq!(v.phase, CascadePhase::Armed);
    assert_eq!(v.phase_reason, "armed_low_vram_used");
    assert!(v.ok);
}

#[test]
fn phase_using_zram() {
    let mut s = base();
    s.zram.used_kib = 50_000;
    s.vram.used_kib = 100;
    let v = derive_lifecycle(&s);
    assert_eq!(v.phase, CascadePhase::UsingZram);
}

#[test]
fn phase_using_vram() {
    let mut s = base();
    s.vram.used_kib = 10_000;
    s.zram.used_kib = 50_000;
    let v = derive_lifecycle(&s);
    assert_eq!(v.phase, CascadePhase::UsingVram);
}

#[test]
fn phase_using_disk() {
    let mut s = base();
    s.disk.used_kib = 5_000;
    s.vram.used_kib = 10_000;
    let v = derive_lifecycle(&s);
    assert_eq!(v.phase, CascadePhase::UsingDisk);
}

#[test]
fn status_off_does_not_attribute_preexisting_disk_pages_to_ramshared() {
    let mut s = base();
    s.zram = TierSample::default();
    s.vram = TierSample::default();
    s.daemon_alive = false;
    s.daemon_pid = None;
    s.capacity_guaranteed = Some(false);
    s.disk.used_kib = 5_000;
    s.disk_baseline_kib = None;

    let view = derive_lifecycle(&s);

    assert_eq!(view.phase, CascadePhase::Off);
    assert_eq!(disk_growth_kib(&s), None);
    assert_eq!(protection_state(&view, &s), ProtectionState::Off);
}

#[test]
fn disk_growth_after_activation_is_at_risk() {
    let mut s = base();
    s.disk_baseline_kib = Some(3_500);
    s.disk.used_kib = 5_000;

    let view = derive_lifecycle(&s);

    assert_eq!(disk_growth_kib(&s), Some(1_500));
    assert_eq!(view.phase, CascadePhase::UsingDisk);
    assert_eq!(protection_state(&view, &s), ProtectionState::AtRisk);
    assert!(!status_ok(&view, &s));
    let status: serde_json::Value =
        serde_json::from_str(&render_status_json(&view, &s, "2026-11-26T17:30:00-03:00")).unwrap();
    assert_eq!(status["protection_state"], "AT_RISK");
    assert_eq!(status["overall_state"], "GUARDED");

    s.control_state = ControlState::Emergency;
    assert_eq!(overall_state(&view, &s), OverallState::Emergency);
}

#[test]
fn phase_degraded_ghost() {
    let mut s = base();
    s.ghost = true;
    s.vram.used_kib = 99_000;
    let v = derive_lifecycle(&s);
    assert_eq!(v.phase, CascadePhase::Degraded);
    assert_eq!(v.phase_reason, "ghost");
    assert!(!v.ok);
    assert!(v.reasons.iter().any(|r| r == "ghost"));
}

#[test]
fn phase_degraded_order() {
    let mut s = base();
    s.order_ok = false;
    let v = derive_lifecycle(&s);
    assert_eq!(v.phase, CascadePhase::Degraded);
    assert_eq!(v.phase_reason, "priority_order_bad");
}

#[test]
fn phase_degraded_hot_vram_no_daemon() {
    let mut s = base();
    s.daemon_alive = false;
    s.daemon_pid = None;
    s.vram.used_kib = 50_000;
    let v = derive_lifecycle(&s);
    assert_eq!(v.phase, CascadePhase::Degraded);
    assert_eq!(v.phase_reason, "daemon_dead_hot_vram");
}

#[test]
fn unreadable_daemon_identity_does_not_claim_daemon_death() {
    let mut s = base();
    s.daemon_alive = false;
    s.daemon_pid = None;
    s.vram.used_kib = 50_000;
    s.measurement_errors
        .push("daemon_identity_unreadable".to_string());
    let view = derive_lifecycle(&s);
    assert_eq!(view.phase, CascadePhase::Degraded);
    assert_eq!(view.phase_reason, "daemon_identity_unreadable");
    assert!(
        !view
            .reasons
            .iter()
            .any(|reason| reason == "daemon_dead_hot_vram")
    );
    assert_eq!(overall_state(&view, &s), OverallState::Blocked);
}

#[test]
fn phase_demoting_only_when_flag() {
    let mut s = base();
    s.demote.in_progress = Some(true);
    s.vram.used_kib = 50_000;
    let v = derive_lifecycle(&s);
    assert_eq!(v.phase, CascadePhase::Demoting);
    // without flag → UsingVram
    s.demote.in_progress = Some(false);
    let v2 = derive_lifecycle(&s);
    assert_eq!(v2.phase, CascadePhase::UsingVram);
}

#[test]
fn render_status_json_is_witness_backed_for_in_progress() {
    let mut s = base();
    s.demote.in_progress = Some(true);
    let v = derive_lifecycle(&s);
    let j = render_status_json(&v, &s, "2026-07-14T00:00:00-03:00");
    assert!(j.contains("\"in_progress\":true"));

    s.demote.in_progress = Some(false);
    let v = derive_lifecycle(&s);
    let j = render_status_json(&v, &s, "2026-07-14T00:00:00-03:00");
    assert!(j.contains("\"in_progress\":false"));
}

#[test]
fn render_status_json_never_publishes_a_measured_refutation_from_an_absent_in_progress() {
    let s = base(); // demote: DemoteSnapshot::default() → in_progress: None
    let v = derive_lifecycle(&s);
    let j = render_status_json(&v, &s, "2026-07-14T00:00:00-03:00");
    assert!(j.contains("\"in_progress\":null"));
    assert!(!j.contains("\"in_progress\":false"));
}

#[test]
fn phase_derivation_never_invents_demoting_from_an_absent_in_progress() {
    let mut s = base();
    s.vram.used_kib = 50_000;
    s.demote.in_progress = None;
    let v = derive_lifecycle(&s);
    assert_ne!(v.phase, CascadePhase::Demoting);
    assert_ne!(v.phase_reason, "demote_in_progress");
}

#[test]
fn phase_derivation_never_invents_demoting_from_a_measured_refutation() {
    let mut s = base();
    s.vram.used_kib = 50_000;
    s.demote.in_progress = Some(false);
    let v = derive_lifecycle(&s);
    assert_ne!(v.phase, CascadePhase::Demoting);
    assert_eq!(v.phase, CascadePhase::UsingVram);
}

#[test]
fn priority_degraded_beats_using_vram() {
    let mut s = base();
    s.ghost = true;
    s.vram.used_kib = 99_000;
    s.demote.in_progress = Some(true);
    let v = derive_lifecycle(&s);
    assert_eq!(v.phase, CascadePhase::Degraded);
}

#[test]
fn active_threshold_from_env_invalid_defaults() {
    // Without relying on process env mutation (clippy deny set_var): pure default path.
    assert_eq!(DEFAULT_ACTIVE_KIB, 1024);
    let mut s = base();
    s.active_kib = 0; // treated as default inside derive for thr
    s.vram.used_kib = 500;
    // thr becomes DEFAULT when active_kib==0
    let v = derive_lifecycle(&s);
    assert_eq!(v.phase, CascadePhase::Armed);
}

#[test]
fn json_shape_golden_or_roundtrip() {
    let s = base();
    let v = derive_lifecycle(&s);
    let j = render_status_json(&v, &s, "2026-07-14T00:00:00-03:00");
    assert!(j.contains("\"phase\":\"Armed\""));
    assert!(j.contains("\"schema_version\":5"));
    assert!(j.contains("\"disk_baseline_kib\":0"));
    assert!(j.contains("\"disk_growth_kib\":0"));
    assert!(j.contains("\"activation\":{\"active\":true"));
    assert!(j.contains("\"protection_state\":\"READY\""));
    assert!(j.contains("\"topology_ok\":true"));
    assert!(j.contains("\"guaranteed_kib\":2097148"));
    assert!(j.contains("\"phase_reason\":\"armed_low_vram_used\""));
    assert!(j.contains("\"ok\":true"));
    assert!(j.contains("\"order_ok\":true"));
    assert!(j.contains("\"ghost\":false"));
    assert!(j.contains("\"thresholds_kib\":{\"active\":1024}"));
    // `base()` uses `DemoteSnapshot::default()`, which is `None` — never
    // taken. The golden pins the absence arm, not a measured refutation.
    assert!(j.contains("\"in_progress\":null"));
    assert!(j.starts_with('{') && j.ends_with('}'));
}

#[test]
fn status_ok_includes_the_rollup_the_json_reports() {
    // The human `ok:` line and the JSON `ok` field used to be computed
    // separately. The human one stopped at `view.ok && protection.is_ok()`
    // and so called a guarded control plane healthy while the JSON — the
    // surface the monitor reads — said false. Both now call `status_ok`.
    let mut snapshot = base();
    snapshot.control_state = ControlState::Guarded;
    let view = derive_lifecycle(&snapshot);
    assert!(
        view.ok,
        "tier order is fine here; that is topology_ok, not the verdict"
    );
    assert!(!status_ok(&view, &snapshot));
    let json = render_status_json(&view, &snapshot, "2026-10-02T00:00:00Z");
    assert!(json.contains("\"ok\":false"), "{json}");
    assert!(json.contains("\"topology_ok\":true"), "{json}");
}

#[test]
fn a_released_cache_or_a_measurement_error_is_not_ok() {
    // Both are the live shape on this host: `cache: OFF` beside
    // `protection: ACTIVE`, and a stale GPU-budget sample. Neither is a
    // healthy product, so neither may print `ok: true`.
    let mut released = base();
    released.cache_state = CacheState::Off;
    let view = derive_lifecycle(&released);
    assert!(view.ok);
    assert!(!status_ok(&view, &released));

    let mut unmeasured = base();
    unmeasured.measurement_errors = vec!["gpu_budget_telemetry_invalid_or_stale".to_string()];
    let view = derive_lifecycle(&unmeasured);
    assert!(view.ok);
    assert!(!status_ok(&view, &unmeasured));
}

#[test]
fn status_blocks_ineffective_tier_before_control_plane_exhaustion() {
    let mut snapshot = base();
    snapshot.zram.used_kib = snapshot.zram.size_kib * 85 / 100;
    let view = derive_lifecycle(&snapshot);
    let json = render_status_json(&view, &snapshot, "2026-08-20T00:00:00Z");
    assert!(json.contains("\"protection_state\":\"AT_RISK\""));
    assert!(json.contains("\"protection_reason\":\"zram_utilization_ge_80_without_vram_use\""));
    assert!(json.contains("\"ok\":false"));
    assert!(json.contains("\"zram_utilization_pct\":85"));
}

#[test]
fn a_missing_zram_tier_publishes_null_rather_than_a_measured_zero() {
    // `0` is the fill of a present tier holding nothing. A tier that is
    // absent, or present with no capacity, has no percentage at all, and
    // publishing `0` would invent one.
    let mut absent = base();
    absent.zram.present = false;
    assert_eq!(zram_utilization_pct(&absent), None);
    let view = derive_lifecycle(&absent);
    let json = render_status_json(&view, &absent, "2026-08-20T00:00:00Z");
    assert!(json.contains("\"zram_utilization_pct\":null"));
    assert!(
        !json.contains("\"zram_utilization_pct\":0"),
        "an absent tier must not be published as a measured zero"
    );

    let mut zero_capacity = base();
    zero_capacity.zram.size_kib = 0;
    assert_eq!(zram_utilization_pct(&zero_capacity), None);
    let view = derive_lifecycle(&zero_capacity);
    let json = render_status_json(&view, &zero_capacity, "2026-08-20T00:00:00Z");
    assert!(json.contains("\"zram_utilization_pct\":null"));

    // The measured zero is a different fact and keeps its number.
    let empty = base();
    assert_eq!(zram_utilization_pct(&empty), Some(0));
    let view = derive_lifecycle(&empty);
    let json = render_status_json(&view, &empty, "2026-08-20T00:00:00Z");
    assert!(json.contains("\"zram_utilization_pct\":0"));
    assert!(!json.contains("\"zram_utilization_pct\":null"));
}

#[test]
fn status_blocks_unverified_capacity() {
    let mut snapshot = base();
    snapshot.capacity_guaranteed = Some(false);
    let view = derive_lifecycle(&snapshot);
    let json = render_status_json(&view, &snapshot, "2026-08-20T00:00:00Z");
    assert!(json.contains("\"protection_state\":\"BLOCKED\""));
    assert!(json.contains("\"protection_reason\":\"capacity_not_guaranteed\""));
    assert!(json.contains("\"guaranteed_kib\":null"));
    assert!(json.contains("\"ok\":false"));
}

#[test]
fn status_json_never_publishes_capacity_not_guaranteed_from_an_unmeasured_guarantee() {
    // A stat that never arrived is not a measured absence. The face keeps
    // the two facts on separate arms so a reader can tell a frame that
    // looked and found nothing from one that could not look.
    let mut snapshot = base();
    snapshot.capacity_guaranteed = None;
    let view = derive_lifecycle(&snapshot);
    let json = render_status_json(&view, &snapshot, "2026-08-20T00:00:00Z");
    assert!(
        json.contains("\"protection_reason\":\"capacity_guarantee_unmeasured\""),
        "a never-taken stat must publish capacity_guarantee_unmeasured: {json}"
    );
    assert!(
        !json.contains("capacity_not_guaranteed"),
        "a never-taken stat must not borrow the measured-refutation word: {json}"
    );
    // Fail-closed still holds on the state itself: a guarantee this frame
    // never took refuses a positive protection claim.
    assert!(json.contains("\"protection_state\":\"BLOCKED\""));
}

#[test]
fn status_json_capacity_not_guaranteed_is_witness_backed_by_a_measured_refutation() {
    let mut snapshot = base();
    snapshot.capacity_guaranteed = Some(false);
    let view = derive_lifecycle(&snapshot);
    let json = render_status_json(&view, &snapshot, "2026-08-20T00:00:00Z");
    assert!(
        json.contains("\"protection_reason\":\"capacity_not_guaranteed\""),
        "a measured absent marker must publish capacity_not_guaranteed: {json}"
    );
    assert!(
        !json.contains("capacity_guarantee_unmeasured"),
        "a measured refutation must not borrow the never-taken arm: {json}"
    );
}

#[test]
fn status_json_guaranteed_kib_is_witness_backed_by_a_measured_capacity_file() {
    // The figure is backed only by a measured marker. Both a measured
    // refutation and a never-taken stat publish `null`; the word that
    // tells them apart is `protection_reason`, not the figure.
    let mut backed = base();
    backed.capacity_guaranteed = Some(true);
    let view = derive_lifecycle(&backed);
    let json = render_status_json(&view, &backed, "2026-08-20T00:00:00Z");
    assert!(
        json.contains("\"guaranteed_kib\":2097148"),
        "a measured capacity file must back the figure: {json}"
    );

    for unbacked in [Some(false), None] {
        let mut snapshot = base();
        snapshot.capacity_guaranteed = unbacked;
        let view = derive_lifecycle(&snapshot);
        let json = render_status_json(&view, &snapshot, "2026-08-20T00:00:00Z");
        assert!(
            json.contains("\"guaranteed_kib\":null"),
            "an unbacked guarantee ({unbacked:?}) must publish null, never a number: {json}"
        );
    }
}

#[test]
fn tiers_from_swap_names_order() {
    // `fixture0` stands in for whatever block device plays the fallback
    // role. No host device name is a product constant.
    let entries = vec![
        ("/dev/zram0", 100u64, 10u64, 200i32),
        ("/dev/nbd0", 200, 5, 100),
        ("/dev/fixture0", 300, 0, -2),
    ];
    let (z, v, d, ok) = tiers_from_swap_names(&entries);
    assert!(z.present && v.present && d.present);
    assert!(ok);
    assert_eq!(z.prio, Some(200));
    let bad = vec![
        ("/dev/zram0", 100u64, 0u64, 50i32),
        ("/dev/nbd0", 200, 0, 100),
    ];
    let (_, _, _, ok2) = tiers_from_swap_names(&bad);
    assert!(!ok2);
}

#[test]
fn tiers_from_swap_names_puts_logical_device_in_vram_not_fallback() {
    // The product logical device is the tier-2 VRAM cache behind NBD/ublk,
    // not the last-resort fallback sink. Classifying it as tier 3 inflates
    // fallback occupancy and hides real VRAM use.
    for logical in ["/dev/ramshared0", "ramshared0", "/dev/mapper/ramshared0"] {
        let entries = vec![
            ("/dev/zram0", 100u64, 0u64, 200i32),
            (logical, 200, 5, 100),
            ("/dev/fixture0", 300, 0, -2),
        ];
        let (z, v, d, _) = tiers_from_swap_names(&entries);
        assert_eq!(z.used_kib, 0, "{logical} is not tier 1");
        assert_eq!(v.used_kib, 5, "{logical} must be tier 2");
        assert_eq!(v.size_kib, 200);
        assert_eq!(d.used_kib, 0, "{logical} must not be counted as tier 3");
        assert_eq!(d.size_kib, 300);
    }
}

#[test]
fn json_escape_quotes() {
    assert_eq!(json_escape(r#"a"b"#), r#""a\"b""#);
}

#[path = "tests/status_and_protection.rs"]
mod status_and_protection;
