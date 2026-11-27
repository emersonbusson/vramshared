// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/worker_custody/memory_tests.rs
#![allow(clippy::unwrap_used)]
use super::*;
use ramshared_wsl2d::host_memory_budget::{
    HostScope, MemoryObservation, MemorySample, SharedMemoryReservations,
};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn pool() -> SharedMemoryReservations {
    let pool = SharedMemoryReservations::default();
    let now = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    assert!(pool.refresh(&MemoryObservation {
        guest: MemorySample {
            total_bytes: 200,
            available_bytes: 100,
            sampled_at_unix_ms: now
        },
        host: HostScope::Native,
        cgroups: Some(&[ramshared_wsl2d::host_memory_budget::CgroupSample {
            identity: ramshared_wsl2d::host_memory_budget::CgroupIdentity {
                device: 1,
                inode: 1
            },
            limit_bytes: None,
            current_bytes: Some(0),
            sampled_at_unix_ms: now,
        }]),
        guest_reserve_bytes: 0,
        host_reserve_bytes: 0,
        max_age_ms: 10_000,
    }));
    pool
}

#[test]
fn host_memory_follows_exact_child_until_exit_and_stop_settlement() {
    let pool = pool();
    let permit = pool.try_reserve(100).unwrap();
    let owner = Arc::new(WorkerCustody::default());
    let mut child = Command::new("/bin/sh")
        .args(["-c", "read line"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let input = child.stdin.take().unwrap();
    let resources = owner
        .reserve()
        .unwrap()
        .with_host_memory(permit)
        .unwrap()
        .bind(child);
    let mut process = ChildWorkerProcess::from_resources(resources.clone());
    drop(resources);
    assert!(pool.try_reserve(1).is_none());

    // A stop owner remains live even after the operating system reports exit.
    process.stop_active.store(true, Ordering::Release);
    drop(input);
    let deadline = Instant::now() + Duration::from_secs(3);
    while !process.try_wait().unwrap() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(pool.try_reserve(1).is_none());
    assert!(!owner.is_quiescent());
    process.stop_active.store(false, Ordering::Release);
    assert!(process.try_wait().unwrap());
    assert!(owner.is_quiescent());
    assert!(pool.try_reserve(100).is_some());
}

#[test]
fn pre_spawn_failure_releases_unbound_memory() {
    let pool = pool();
    let owner = Arc::new(WorkerCustody::default());
    let reservation = owner
        .reserve()
        .unwrap()
        .with_host_memory(pool.try_reserve(100).unwrap())
        .unwrap();
    assert!(pool.try_reserve(1).is_none());
    drop(reservation);
    assert!(pool.try_reserve(100).is_some());
}

#[test]
fn uncertain_identity_exit_or_reaper_keeps_memory_reserved() {
    let pool = pool();
    let custody = HostMemoryCustody::new(Some(pool.try_reserve(100).unwrap()));
    for (identity_lost, exited, stop, reaper) in [
        (true, true, false, false),
        (false, false, false, false),
        (false, true, true, false),
        (false, true, false, true),
    ] {
        assert!(
            !custody
                .release_if_settled(identity_lost, exited, stop, reaper)
                .unwrap()
        );
        assert!(pool.try_reserve(1).is_none());
    }
    assert!(
        custody
            .release_if_settled(false, true, false, false)
            .unwrap()
    );
    assert!(pool.try_reserve(100).is_some());
}
