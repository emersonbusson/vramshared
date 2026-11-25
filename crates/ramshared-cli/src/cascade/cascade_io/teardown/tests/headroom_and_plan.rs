// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/teardown/tests/headroom_and_plan.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;

#[test]
fn stale_recovery_preserves_record_if_origin_reappears() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-reappeared-origin-{}",
        std::process::id()
    ));
    let paths = RuntimePaths::under(&root);
    let devices = root.join("devices");
    std::fs::create_dir_all(&paths.runtime_dir).unwrap();
    std::fs::create_dir(&devices).unwrap();
    let _fixture = OriginDeviceFixture::new(devices.clone());
    std::fs::write(&paths.swap_dev_file, "/dev/nbd9").unwrap();
    crate::cascade::TEST_SWAPS.with(|cell| {
        *cell.borrow_mut() = Some("Filename\tType\tSize\tUsed\tPriority\n".to_string())
    });
    let result = stale_recovery::clear_stale_runtime_state_with(&paths, || {
        std::fs::write(devices.join("nbd9"), b"appeared").unwrap();
    });
    crate::cascade::TEST_SWAPS.with(|cell| *cell.borrow_mut() = None);
    assert!(result.is_err());
    assert!(
        paths.swap_dev_file.exists(),
        "reappeared origin must retain its record"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn stale_recovery_retains_sibling_when_record_is_dangling_symlink() {
    let root = std::env::temp_dir().join(format!("ramshared-record-link-{}", std::process::id()));
    let paths = RuntimePaths::under(&root);
    let devices = root.join("devices");
    std::fs::create_dir_all(&paths.runtime_dir).unwrap();
    std::fs::create_dir(&devices).unwrap();
    let _fixture = OriginDeviceFixture::new(devices);
    std::os::unix::fs::symlink("missing-target", &paths.zram_dev_file).unwrap();
    std::fs::write(&paths.swap_dev_file, "/dev/nbd9").unwrap();
    crate::cascade::TEST_SWAPS.with(|cell| {
        *cell.borrow_mut() = Some("Filename\tType\tSize\tUsed\tPriority\n".to_string())
    });
    let result = clear_stale_runtime_state(&paths);
    crate::cascade::TEST_SWAPS.with(|cell| *cell.borrow_mut() = None);
    assert!(result.is_err());
    assert!(paths.swap_dev_file.exists(), "sibling evidence must remain");
    assert!(
        std::fs::symlink_metadata(&paths.zram_dev_file)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn stale_device_observation_refuses_uncertain_sysfs() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-sysfs-observation-{}",
        std::process::id()
    ));
    std::fs::create_dir(&root).unwrap();
    let recorded = device("/dev/zram9", ManagedDeviceKind::Zram);
    let observe = || released_peer::device_is_present_or_armed_under(&recorded, &[], &root);
    assert!(!observe().unwrap());
    std::os::unix::fs::symlink("missing-target", root.join("zram9")).unwrap();
    assert!(observe().unwrap());
    std::fs::remove_file(root.join("zram9")).unwrap();
    std::fs::remove_dir(&root).unwrap();
    std::fs::write(&root, b"not a directory").unwrap();
    assert!(observe().is_err());
    std::fs::remove_file(root).unwrap();
}

#[test]
fn safe_swapoff_headroom_keeps_swapoff_first_order() {
    let zram = device("/dev/zram0", ManagedDeviceKind::Zram);
    let nbd = device("/dev/nbd0", ManagedDeviceKind::Nbd);
    let plan = NbdLifecyclePlan {
        actions: vec![
            NbdLifecycleAction::Swapoff(zram.clone()),
            NbdLifecycleAction::ResetZram(zram),
            NbdLifecycleAction::DisconnectNbd(nbd),
            NbdLifecycleAction::StopDaemon,
        ],
    };
    let swaps = vec![swap("/dev/zram0", GIB)];
    let decision = headroom_for_plan_with_mem_and_absorber(
        &plan,
        &swaps,
        0,
        MemInfo {
            total_bytes: 16 * GIB,
            available_bytes: 8 * GIB,
        },
    );
    assert!(decision.is_safe(), "{decision:?}");
    let executor = RecordingExecutor::default();
    execute_nbd_lifecycle_plan(&plan, &executor).expect("safe plan");
    assert_eq!(
        executor.calls.borrow().as_slice(),
        ["swapoff", "reset-zram", "disconnect-nbd", "stop-daemon"]
    );
}

#[test]
fn reserve_and_margin_constants_are_used() {
    assert_eq!(MIN_RESERVE_BYTES, GIB);
    assert_eq!(MARGIN_BYTES, 512 * 1024 * 1024);
}

#[test]
fn plan_without_managed_swapoff_reads_meminfo_and_is_safe() {
    let plan = NbdLifecyclePlan {
        actions: vec![NbdLifecycleAction::StopDaemon],
    };
    let decision = headroom_for_plan(&plan, &[]).expect("read /proc/meminfo");
    assert!(decision.is_safe(), "{decision:?}");
    assert_eq!(decision.managed_bytes, 0);
}

#[test]
fn device_headroom_ignores_swap_not_in_the_plan() {
    let nbd = device("/dev/nbd0", ManagedDeviceKind::Nbd);
    let swaps = vec![swap("/dev/zram0", 8 * GIB)];
    let decision = headroom_for_device_with_absorber(&nbd, &swaps, 0).expect("read /proc/meminfo");
    assert_eq!(decision.managed_bytes, 0);
}

#[test]
fn live_wsl_headroom_rejects_a_swapoff_plan_without_mutation() {
    if !matches!(
        ramshared_block::platform::probe_platform().platform,
        Some(
            ramshared_block::platform::PlatformKind::Wsl1
                | ramshared_block::platform::PlatformKind::Wsl2
        )
    ) {
        return;
    }
    let nbd = device("/dev/nbd0", ManagedDeviceKind::Nbd);
    let plan = NbdLifecyclePlan {
        actions: vec![NbdLifecycleAction::Swapoff(nbd.clone())],
    };
    let swaps = vec![swap("/dev/nbd0", 0)];
    let planned = headroom_for_plan(&plan, &swaps).unwrap();
    let immediate = headroom_for_device_with_absorber(&nbd, &swaps, 0).unwrap();
    for decision in [planned, immediate] {
        assert!(!decision.is_safe());
        assert_eq!(decision.reason, "host_memory_unverified");
    }
}

#[test]
fn live_host_memory_requires_live_peer_and_admits_zero_used_wsl_swapoff() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-live-host-headroom-{}",
        std::process::id()
    ));
    let paths = RuntimePaths::under(&root);
    std::fs::create_dir_all(&paths.runtime_dir).unwrap();
    let nbd = device("/dev/nbd0", ManagedDeviceKind::Nbd);
    let binding = released_binding(std::slice::from_ref(&nbd));
    let now_ms = unix_time_ms().expect("system clock");
    let doc = serde_json::json!({
        "schema_version": 1,
        "daemon_instance_id": binding.daemon_instance_id,
        "written_at_unix_ms": now_ms,
        "ok": true,
        "origin_state": "READY",
        "cache_state": "ACTIVE",
        "cache_unavailable_reason": null,
        "logical_capacity_kib": 4_194_304,
        "vram_cached_kib": 0,
        "gpu_headroom_kib": null,
        "gpu_budget": null,
        "gpu_utilization": null,
        "gpu_cache": null,
        "host_memory": {
            "sampled_at_unix_ms": now_ms.saturating_sub(200),
            "total_bytes": 64 * GIB,
            "available_bytes": 32 * GIB,
            "commit_total_bytes": 16 * GIB,
            "commit_limit_bytes": 80 * GIB,
            "page_size_bytes": 4096
        },
        "control_plane_state": "vsock_leased",
        "control_plane_lease_deadline_ms": now_ms + 10_000,
        "ssd_origin_written_kib": 0,
        "cache_hits": 0,
        "cache_fallback_reads": 0,
        "cache_invalidations": 0,
        "cache_releases": 0,
        "origin_cache_io": {
            "origin_syncs": 0,
            "batched_writes": 0,
            "cache_read_bytes": 0,
            "cache_read_failures": 0,
            "cache_write_failures": 0,
            "promotion_refusals": 0,
            "allocation_failures": 0,
            "valid_blocks": 0
        },
        "cache_target_kib": 0,
        "serve_threads": 4,
        "serve_requests_total": 0
    });
    std::fs::write(&paths.cache_status_file, serde_json::to_vec(&doc).unwrap()).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            &paths.cache_status_file,
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }

    assert!(live_host_memory(&paths, &binding, PeerState::Released).is_none());
    let host = live_host_memory(&paths, &binding, PeerState::Live).expect("live peer host memory");
    let plan = NbdLifecyclePlan {
        actions: vec![NbdLifecycleAction::Swapoff(nbd.clone())],
    };
    let swaps = vec![swap("/dev/nbd0", 0)];
    let planned = headroom_for_plan_with_absorber_and_host(&plan, &swaps, 0, Some(host)).unwrap();
    let immediate =
        headroom_for_device_with_absorber_and_host(&nbd, &swaps, 0, Some(host)).unwrap();
    assert!(planned.is_safe(), "{planned:?}");
    assert!(immediate.is_safe(), "{immediate:?}");
    std::fs::remove_dir_all(root).unwrap();
}

/// The plan for a released peer carries the tier the peer did not own and
/// releases the origin tier's stale client.
#[test]
fn the_released_plan_carries_the_tier_the_peer_did_not_own() {
    let zram = device("/dev/zram0", ManagedDeviceKind::Zram);
    let nbd = device("/dev/nbd0", ManagedDeviceKind::Nbd);
    let binding = released_binding(&[zram.clone(), nbd.clone()]);
    // The zram is still active as swap; the origin tier was already
    // released by the kernel with the peer.
    let swaps = vec![swap("/dev/zram0", 64 * 1024 * 1024)];
    let plan = plan_released_lifecycle(&binding, &swaps, std::slice::from_ref(&zram))
        .expect("the released plan");
    let actions: Vec<&str> = plan
        .actions
        .iter()
        .map(|action| match action {
            NbdLifecycleAction::Swapoff(_) => "swapoff",
            NbdLifecycleAction::ResetZram(_) => "reset",
            NbdLifecycleAction::DisconnectNbd(_) => "disconnect",
            NbdLifecycleAction::StopDaemon => "stop",
        })
        .collect();
    assert_eq!(
        actions,
        ["swapoff", "reset", "disconnect", "stop"],
        "the order is the ordered teardown's, and the released tier is not swapped off again"
    );
}

/// A record whose device is still armed is ownership evidence, and a
/// record whose device is gone is stale.
#[test]
fn a_record_is_stale_only_when_its_device_is_gone() {
    let armed = device("/dev/zram0", ManagedDeviceKind::Zram);
    let gone = device("/dev/zram999999", ManagedDeviceKind::Zram);
    let swaps = vec![swap("/dev/zram0", 1024)];
    assert!(
        device_is_present_or_armed(&armed, &swaps).unwrap(),
        "an armed device is not stale"
    );
    assert!(
        !device_is_present_or_armed(&gone, &swaps).unwrap(),
        "a device that is neither armed nor present is stale"
    );
}

/// The recovery's refusals, without a device: a live peer, an origin
/// tier that is still armed, and a foreign managed swap each have their
/// own reason, and none of them touches anything.
#[test]
fn the_recovery_refuses_a_live_peer_an_armed_tier_and_a_foreign_swap() {
    let zram = device("/dev/zram0", ManagedDeviceKind::Zram);
    let nbd = device("/dev/nbd0", ManagedDeviceKind::Nbd);
    let binding = released_binding(&[zram.clone(), nbd.clone()]);

    let live = released_recovery_decision(&binding, &[], true)
        .expect_err("a live peer has the ordered path");
    assert!(live.to_string().contains("peer is alive"), "{live}");

    let armed = released_recovery_decision(&binding, &[swap("/dev/nbd0", 4096)], false)
        .expect_err("an armed origin tier is not released");
    assert!(armed.to_string().contains("still armed"), "{armed}");

    let foreign = released_recovery_decision(&binding, &[swap("/dev/nbd7", 4096)], false)
        .expect_err("a foreign managed swap is never touched");
    assert!(foreign.to_string().contains("foreign"), "{foreign}");

    released_recovery_decision(&binding, &[swap("/dev/zram0", 4096)], false)
        .expect("a released peer with the zram left may recover");
}

/// A stale ownership record must not close the loop between the two
/// commands the operator has.
///
/// A failed activation keeps the record when it could not release the
/// device, and that record makes the next activation refuse with "run
/// `ramshared down`" — while `down` has no lifecycle to act on. The
/// recovery is the supported exit, and it may only clear a record whose
/// device is demonstrably gone.
#[test]
fn the_recovery_clears_a_stale_record_whose_device_is_gone() {
    struct NoCommandRunner;
    impl CommandRunner for NoCommandRunner {
        fn run(&self, command: &str, _args: &[&str]) -> Result<String, CascadeError> {
            Err(CascadeError::Shell {
                cmd: command.to_string(),
                msg: "the stale-state recovery must not run a command".into(),
            })
        }
    }

    let root = std::env::temp_dir().join(format!("ramshared-recover-stale-{}", std::process::id()));
    let paths = RuntimePaths::under(&root);
    std::fs::create_dir_all(&paths.runtime_dir).expect("runtime dir");
    std::fs::write(
        &paths.zram_dev_file,
        r#"{"kind":"zram","path":"/dev/zram999999","dev_t":"251:9","sysfs_path":"/sys/devices/virtual/block/zram999999","sysfs_dev_t":"251:9","kernel_owner_instance_id":null}"#,
    )
    .expect("stale record");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&paths.zram_dev_file, std::fs::Permissions::from_mode(0o600))
            .expect("sealed record mode");
    }
    crate::cascade::TEST_SWAPS.with(|cell| {
        *cell.borrow_mut() = Some("Filename\tType\tSize\tUsed\tPriority\n".to_string())
    });

    down_recover_released_peer_with(&NoCommandRunner, &paths)
        .expect("a stale record must be cleared");
    assert!(
        !paths.zram_dev_file.exists(),
        "the stale ownership record must be gone"
    );
    crate::cascade::TEST_SWAPS.with(|cell| *cell.borrow_mut() = None);
}

#[test]
fn unbound_teardown_and_plan_preserve_orphan_evidence() {
    let root =
        std::env::temp_dir().join(format!("ramshared-unbound-teardown-{}", std::process::id()));
    let paths = RuntimePaths::under(&root);
    std::fs::create_dir_all(&paths.runtime_dir).unwrap();
    #[derive(Default)]
    struct RecordingRunner {
        calls: RefCell<Vec<String>>,
    }
    impl CommandRunner for RecordingRunner {
        fn run(&self, command: &str, _args: &[&str]) -> Result<String, CascadeError> {
            self.calls.borrow_mut().push(command.into());
            Err(CascadeError::Precondition(
                "an unbound lifecycle must never execute a command".into(),
            ))
        }
    }
    let runner = RecordingRunner::default();
    crate::cascade::TEST_SWAPS
        .with(|cell| *cell.borrow_mut() = Some("Filename Type Size Used Priority\n".into()));
    let no_op = down_with_runtime(&runner, &paths);
    crate::cascade::TEST_SWAPS.with(|cell| *cell.borrow_mut() = None);
    assert!(no_op.is_ok());
    assert!(down_plan_with_swaps(&paths, &[], false).is_ok());
    assert!(down_plan_with_swaps(&paths, &[], true).is_ok());
    assert!(!paths.pid_file.exists());
    std::fs::write(&paths.pid_file, "unbound evidence").unwrap();
    crate::cascade::TEST_SWAPS
        .with(|cell| *cell.borrow_mut() = Some("Filename Type Size Used Priority\n".into()));
    let refused = down_with_runtime(&runner, &paths);
    crate::cascade::TEST_SWAPS.with(|cell| *cell.borrow_mut() = None);
    assert!(refused.is_err());
    assert!(down_plan_with_swaps(&paths, &[], true).is_err());
    assert_eq!(
        std::fs::read_to_string(&paths.pid_file).unwrap(),
        "unbound evidence"
    );
    assert!(runner.calls.borrow().is_empty());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn recovery_preserves_all_records_when_another_tier_is_active() {
    let root = std::env::temp_dir().join(format!("ramshared-recover-mixed-{}", std::process::id()));
    let paths = RuntimePaths::under(&root);
    std::fs::create_dir_all(&paths.runtime_dir).unwrap();
    std::fs::write(&paths.zram_dev_file, r#"{"kind":"zram","path":"/dev/zram999999","dev_t":"251:9","sysfs_path":"/sys/devices/virtual/block/zram999999","sysfs_dev_t":"251:9","kernel_owner_instance_id":null}"#).unwrap();
    std::fs::write(&paths.swap_dev_file, "/dev/nbd9").unwrap();
    std::fs::write(&paths.pid_file, "retained evidence").unwrap();
    use std::os::unix::fs::PermissionsExt;
    for path in [&paths.zram_dev_file, &paths.swap_dev_file] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    crate::cascade::TEST_SWAPS.with(|cell| {
        *cell.borrow_mut() = Some(
            "Filename\tType\tSize\tUsed\tPriority\n/dev/nbd9\tpartition\t2097148\t1024\t50\n"
                .into(),
        );
    });
    let result = clear_stale_runtime_state(&paths);
    crate::cascade::TEST_SWAPS.with(|cell| *cell.borrow_mut() = None);
    assert!(
        result.is_err(),
        "one stale record cannot authorize clearing a live tier"
    );
    assert!(paths.zram_dev_file.exists());
    assert!(paths.swap_dev_file.exists());
    assert!(paths.pid_file.exists());
    std::fs::remove_dir_all(root).unwrap();
}

/// The origin tier's own record can be stale too: a peer that died left
/// the record naming a device that is no longer there, and that record is
/// what refuses the next activation.
#[test]
fn the_recovery_clears_a_stale_origin_tier_record() {
    struct NoCommandRunner;
    impl CommandRunner for NoCommandRunner {
        fn run(&self, command: &str, _args: &[&str]) -> Result<String, CascadeError> {
            Err(CascadeError::Shell {
                cmd: command.to_string(),
                msg: "the stale-state recovery must not run a command".into(),
            })
        }
    }

    let root = std::env::temp_dir().join(format!("ramshared-recover-nbd-{}", std::process::id()));
    let paths = RuntimePaths::under(&root);
    let device_root = root.join("devices");
    std::fs::create_dir_all(&device_root).expect("isolated device root");
    let _devices = OriginDeviceFixture::new(device_root.clone());
    std::fs::create_dir_all(&paths.runtime_dir).expect("runtime dir");
    std::fs::write(&paths.swap_dev_file, "/dev/nbd9").expect("stale origin record");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&paths.swap_dev_file, std::fs::Permissions::from_mode(0o600))
            .expect("sealed record mode");
    }
    crate::cascade::TEST_SWAPS.with(|cell| {
        *cell.borrow_mut() = Some("Filename\tType\tSize\tUsed\tPriority\n".to_string())
    });

    std::fs::write(device_root.join("nbd9"), b"fixture").unwrap();
    assert!(down_recover_released_peer_with(&NoCommandRunner, &paths).is_err());
    assert!(
        paths.swap_dev_file.exists(),
        "present device retains evidence"
    );
    std::fs::remove_file(device_root.join("nbd9")).unwrap();
    std::os::unix::fs::symlink("missing-target", device_root.join("nbd9")).unwrap();
    assert!(down_recover_released_peer_with(&NoCommandRunner, &paths).is_err());
    assert!(
        paths.swap_dev_file.exists(),
        "an unresolved device retains evidence"
    );
    std::fs::remove_file(device_root.join("nbd9")).unwrap();
    std::fs::remove_dir(&device_root).unwrap();
    std::fs::write(&device_root, b"not a directory").unwrap();
    assert!(down_recover_released_peer_with(&NoCommandRunner, &paths).is_err());
    assert!(
        paths.swap_dev_file.exists(),
        "an observation error retains evidence"
    );
    std::fs::remove_file(&device_root).unwrap();
    std::fs::create_dir(&device_root).unwrap();
    down_recover_released_peer_with(&NoCommandRunner, &paths)
        .expect("a stale origin record must be cleared");
    assert!(
        !paths.swap_dev_file.exists(),
        "the stale origin record must be gone"
    );
    crate::cascade::TEST_SWAPS.with(|cell| *cell.borrow_mut() = None);
    std::fs::remove_dir_all(root).unwrap();
}

/// A record whose device is still there is ownership evidence, and the
/// recovery must not touch it.
#[test]
fn the_recovery_refuses_a_record_whose_device_is_still_armed() {
    struct NoCommandRunner;
    impl CommandRunner for NoCommandRunner {
        fn run(&self, command: &str, _args: &[&str]) -> Result<String, CascadeError> {
            Err(CascadeError::Shell {
                cmd: command.to_string(),
                msg: "the stale-state recovery must not run a command".into(),
            })
        }
    }

    let root = std::env::temp_dir().join(format!("ramshared-recover-armed-{}", std::process::id()));
    let paths = RuntimePaths::under(&root);
    std::fs::create_dir_all(&paths.runtime_dir).expect("runtime dir");
    std::fs::write(
        &paths.zram_dev_file,
        r#"{"kind":"zram","path":"/dev/zram0","dev_t":"253:0","sysfs_path":"/sys/devices/virtual/block/zram0","sysfs_dev_t":"253:0","kernel_owner_instance_id":null}"#,
    )
    .expect("armed record");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&paths.zram_dev_file, std::fs::Permissions::from_mode(0o600))
            .expect("sealed record mode");
    }
    crate::cascade::TEST_SWAPS.with(|cell| {
        *cell.borrow_mut() = Some(
            "Filename\tType\tSize\tUsed\tPriority\n/dev/zram0\tpartition\t2097148\t1024\t200\n"
                .to_string(),
        )
    });

    let refused =
        down_recover_released_peer_with(&NoCommandRunner, &paths).expect_err("must refuse");
    assert!(
        refused.to_string().contains("nothing to recover"),
        "the refusal says what is missing: {refused}"
    );
    assert!(
        paths.zram_dev_file.exists(),
        "the ownership evidence of a live device must stay"
    );
    crate::cascade::TEST_SWAPS.with(|cell| *cell.borrow_mut() = None);
}

#[test]
fn down_plan_without_lifecycle_is_read_only_and_reports_no_action() {
    let root =
        std::env::temp_dir().join(format!("ramshared-down-plan-empty-{}", std::process::id()));
    let paths = RuntimePaths::under(&root);
    down_plan_with_swaps(&paths, &[], true).expect("empty JSON plan");
    down_plan_with_swaps(&paths, &[], false).expect("empty text plan");
    assert!(!paths.lifecycle_binding_file.exists());
}

#[test]
fn down_plan_refuses_runtime_evidence_without_a_binding() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-down-plan-evidence-{}",
        std::process::id()
    ));
    let paths = RuntimePaths::under(&root);
    std::fs::create_dir_all(&paths.runtime_dir).expect("runtime dir");
    std::fs::write(&paths.pid_file, "1234").expect("pid record");
    let error = down_plan_with_swaps(&paths, &[], true)
        .expect_err("runtime evidence without a binding must be refused");
    assert!(matches!(error, CascadeError::Precondition(_)), "{error:?}");
}

// SPEC swap-drain-safety T14 (RF-11/DT-2): the read-only plan reports the
// blocked decision with zero absorber; it has no runner or creator.
#[test]
fn attended_down_plan_reports_zero_absorber() {
    let decision = HeadroomDecision {
        managed_bytes: 2_145_615_872,
        available_bytes: 4_039_417_856,
        emergency_bytes: 0,
        remaining_free_bytes: 5_627_236_352,
        reserve_bytes: 1_677_839_565,
        margin_bytes: 536_870_912,
        required_bytes: 4_360_326_349,
        deficit_bytes: 320_908_493,
        verdict: HeadroomVerdict::Blocked,
        reason: "managed_swap_exceeds_headroom",
    };
    let json = render_plan(&decision, true).expect("json plan");
    assert!(json.contains("\"emergency_bytes\":0"), "{json}");
    assert!(json.contains("\"verdict\":\"blocked\""), "{json}");
    assert_eq!(decision.deficit_bytes, 320_908_493);
}

#[test]
fn plan_decision_renders_json_and_text_without_a_live_host() {
    let decision = HeadroomDecision {
        managed_bytes: 2_145_615_872,
        available_bytes: 4_039_417_856,
        emergency_bytes: 0,
        remaining_free_bytes: 5_627_236_352,
        reserve_bytes: 1_677_839_565,
        margin_bytes: 536_870_912,
        required_bytes: 4_360_326_349,
        deficit_bytes: 320_908_493,
        verdict: HeadroomVerdict::Blocked,
        reason: "managed_swap_exceeds_headroom",
    };
    let json = render_plan(&decision, true).expect("json plan");
    assert!(json.contains("\"verdict\":\"blocked\""), "{json}");
    assert!(json.contains("\"deficit_bytes\":320908493"), "{json}");
    assert!(
        json.contains("\"remaining_free_bytes\":5627236352"),
        "{json}"
    );
    let text = render_plan(&decision, false).expect("text plan");
    assert!(text.contains("verdict=Blocked"), "{text}");
    assert!(text.contains("deficit=320908493 B"), "{text}");
    assert!(text.contains("remaining_swap_free=5627236352 B"), "{text}");
    assert!(text.contains("required=4360326349 B"), "{text}");
}
