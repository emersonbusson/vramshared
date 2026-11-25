// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/gpu_cache_worker/host_budget/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::gpu_worker_slab_bridge::SlabBridgeConfig;
use std::time::{SystemTime, UNIX_EPOCH};

const GIB: u64 = 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;

#[test]
fn host_observation_scales_across_6_to_256_gib_and_respects_cgroup_limits() {
    let generous_host = HostMemoryObservation {
        total_host_bytes: 512 * GIB,
        available_host_bytes: 256 * GIB,
        cgroup_headroom_bytes: None,
    };
    let bridge = SlabBridgeConfig::sealed();

    for gib in [6u64, 32, 96, 192, 256] {
        let usable = bridge.frame_count(gib * GIB, 0).unwrap();
        let (admitted, guard) =
            WorkerHostMetadataGuard::admit_frames(usable, 1, None, Some(generous_host));
        assert_eq!(
            admitted, usable,
            "{gib} GiB must admit all {usable} frames without a 1024-frame cap"
        );
        assert!(admitted > 1024);
        assert!(guard.limit_bytes() >= u64::from(admitted) * 64);
    }

    // Tight 384 MiB / 640 MiB cgroup limits cap admitted frames strictly below 256 GiB.
    let vram_256g = bridge.frame_count(256 * GIB, 0).unwrap();
    let tight_384m = HostMemoryObservation {
        total_host_bytes: 64 * GIB,
        available_host_bytes: 32 * GIB,
        cgroup_headroom_bytes: Some(384 * MIB),
    };
    let (admitted_384, _) =
        WorkerHostMetadataGuard::admit_frames(vram_256g, 1, None, Some(tight_384m));
    assert!(admitted_384 < vram_256g);
    assert!(admitted_384 > 1024);
}

#[test]
fn concurrent_adapters_share_host_metadata_ledger_and_release_on_drop() {
    let obs = HostMemoryObservation {
        total_host_bytes: 64 * GIB,
        available_host_bytes: 16 * GIB,
        cgroup_headroom_bytes: Some(512 * MIB),
    };
    let ledger = SharedHostMetadataBudget::with_ceiling_bytes(8 * MIB);
    let (frames_a, guard_a) =
        WorkerHostMetadataGuard::admit_frames(65_536, 2, Some(ledger.clone()), Some(obs));
    let (frames_b, guard_b) =
        WorkerHostMetadataGuard::admit_frames(65_536, 2, Some(ledger.clone()), Some(obs));
    assert!(frames_a > 0 && frames_b > 0);
    assert!(ledger.total_reserved_bytes() <= 8 * MIB);
    assert!(!guard_a.update_usage(16 * MIB));
    assert!(guard_a.update_usage(ledger.worker_reserved_bytes(0).saturating_add(4096)));

    drop(guard_a);
    assert_eq!(ledger.worker_reserved_bytes(0), 0);
    drop(guard_b);
    assert_eq!(ledger.total_reserved_bytes(), 0);
}

#[test]
fn parses_proc_meminfo_and_cgroup_v2_tightest_ancestor_limit() {
    let dir = std::env::temp_dir().join(format!(
        "ramshared-host-budget-cg-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let slice_dir = dir.join("ramshared.slice").join("worker-1");
    fs::create_dir_all(&slice_dir).unwrap();
    fs::write(dir.join("memory.max"), "max\n").unwrap();
    fs::write(dir.join("memory.current"), "1000\n").unwrap();
    fs::write(
        dir.join("ramshared.slice/memory.max"),
        format!("{}\n", 640 * MIB),
    )
    .unwrap();
    fs::write(
        dir.join("ramshared.slice/memory.current"),
        format!("{}\n", 256 * MIB),
    )
    .unwrap();
    fs::write(slice_dir.join("memory.max"), format!("{}\n", 512 * MIB)).unwrap();
    fs::write(slice_dir.join("memory.current"), format!("{}\n", 400 * MIB)).unwrap();

    let meminfo = format!(
        "MemTotal:       {} kB\nMemAvailable:   {} kB\n",
        16 * 1024 * 1024,
        8 * 1024 * 1024
    );
    let obs =
        HostMemoryObservation::from_proc_strings(&meminfo, "0::/ramshared.slice/worker-1\n", &dir)
            .unwrap();
    let _ = fs::remove_dir_all(&dir);

    assert_eq!(obs.total_host_bytes, 16 * GIB);
    assert_eq!(obs.available_host_bytes, 8 * GIB);
    assert_eq!(obs.cgroup_headroom_bytes, Some(112 * MIB));
    assert!(
        HostMemoryObservation::from_proc_strings(
            "MemTotal: 100 kB\nMemAvailable: 200 kB\n",
            "0::/\n",
            Path::new("/nonexistent")
        )
        .is_none()
    );
}
