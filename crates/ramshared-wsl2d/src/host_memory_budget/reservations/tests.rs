// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/host_memory_budget/reservations/tests.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]
use super::*;
use std::sync::Barrier;

macro_rules! observation {
    () => {{
        let now = u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap();
        MemoryObservation {
            guest: MemorySample {
                total_bytes: 1000,
                available_bytes: 900,
                sampled_at_unix_ms: now,
            },
            host: HostScope::Native,
            cgroups: Some(&[CgroupSample {
                identity: CgroupIdentity {
                    device: 1,
                    inode: 1,
                },
                limit_bytes: None,
                current_bytes: 0,
                sampled_at_unix_ms: now,
            }]),
            guest_reserve_bytes: 100,
            host_reserve_bytes: 0,
            max_age_ms: 5_000,
        }
    }};
}

macro_rules! refresh_observation {
    ($pool:expr) => {{
        let sample = observation!();
        $pool.refresh(&sample)
    }};
}

#[test]
fn empty_cgroup_observation_never_creates_a_reservation() {
    let pool = SharedMemoryReservations::default();
    let mut sample = observation!();
    sample.cgroups = Some(&[]);
    assert!(!pool.refresh(&sample));
    assert!(pool.try_reserve(1).is_none());

    let fresh = observation!();
    assert!(pool.refresh(&fresh));
    let held = pool.try_reserve(100).unwrap();
    sample.guest = fresh.guest;
    assert!(!pool.refresh(&sample));
    assert!(!held.is_current());
    assert!(pool.try_reserve(1).is_none());
    assert_eq!(pool.state.lock().unwrap().reserved, 100);
}

#[test]
fn concurrent_adapters_share_one_ceiling_and_release_custody() {
    let pool = SharedMemoryReservations::default();
    assert!(refresh_observation!(pool));
    let start = Arc::new(Barrier::new(9));
    let hold = Arc::new(Barrier::new(9));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let pool = pool.clone();
            let start = Arc::clone(&start);
            let hold = Arc::clone(&hold);
            std::thread::spawn(move || {
                start.wait();
                let deadline = Instant::now() + Duration::from_secs(1);
                let lease = loop {
                    if let Some(lease) = pool.try_reserve(200) {
                        break Some(lease);
                    }
                    if Instant::now() >= deadline {
                        break None;
                    }
                    std::thread::yield_now();
                };
                hold.wait();
                lease
            })
        })
        .collect();
    start.wait();
    hold.wait();
    let leases: Vec<_> = threads
        .into_iter()
        .filter_map(|thread| thread.join().unwrap())
        .collect();
    assert_eq!(leases.len(), 4);
    assert!(pool.try_reserve(1).is_none());
    drop(leases);
    assert!(pool.try_reserve(800).is_some());
}

#[test]
fn releasing_reservation_never_waits_for_busy_accounting() {
    let pool = SharedMemoryReservations::default();
    assert!(refresh_observation!(pool));
    let lease = pool.try_reserve(600).unwrap();
    let probe = lease.probe();
    let guard = pool.state.lock().unwrap();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        drop(lease);
        done_tx.send(()).unwrap();
    });
    let completed_while_locked = done_rx.recv_timeout(Duration::from_millis(100)).is_ok();
    drop(guard);
    thread.join().unwrap();
    assert!(
        completed_while_locked,
        "reservation drop waited for accounting"
    );
    assert!(!probe());
    assert!(
        pool.try_reserve(800).is_some(),
        "deferred release must be reconciled"
    );
}

#[test]
fn reserving_memory_never_waits_for_busy_accounting() {
    let pool = SharedMemoryReservations::default();
    assert!(refresh_observation!(pool));
    let guard = pool.state.lock().unwrap();
    let other = pool.clone();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        done_tx.send(other.try_reserve(1).is_none()).unwrap();
    });
    let result = done_rx.recv_timeout(Duration::from_millis(100));
    drop(guard);
    worker.join().unwrap();
    assert!(
        matches!(result, Ok(true)),
        "busy accounting must refuse without waiting"
    );
    assert!(pool.try_reserve(800).is_some());
}

#[test]
fn concurrent_deferred_releases_do_not_overcommit_or_lose_capacity() {
    let pool = SharedMemoryReservations::default();
    assert!(refresh_observation!(pool));
    let leases: Vec<_> = (0..4).map(|_| pool.try_reserve(200).unwrap()).collect();
    let guard = pool.state.lock().unwrap();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let workers: Vec<_> = leases
        .into_iter()
        .map(|lease| {
            let done = done_tx.clone();
            std::thread::spawn(move || {
                drop(lease);
                done.send(()).unwrap();
            })
        })
        .collect();
    let completed = (0..4)
        .filter(|_| done_rx.recv_timeout(Duration::from_millis(100)).is_ok())
        .count();
    assert_eq!(
        guard.reserved, 800,
        "releases remain charged while the lock is held"
    );
    drop(guard);
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(
        completed, 4,
        "all drops must finish while accounting is busy"
    );
    let replacement = pool.try_reserve(800).unwrap();
    assert!(pool.try_reserve(1).is_none());
    drop(replacement);
    assert!(pool.try_reserve(800).is_some());
}

#[test]
fn deferred_release_overflow_is_a_permanent_refusal() {
    let pool = SharedMemoryReservations::default();
    assert!(refresh_observation!(pool));
    let lease = pool.try_reserve(1).unwrap();
    pool.state
        .pending_releases
        .store(MAX_ACCOUNTED, Ordering::Release);
    drop(lease);
    assert_eq!(
        pool.state.pending_releases.load(Ordering::Acquire),
        RELEASE_FAULT
    );
    assert!(pool.try_reserve(1).is_none());
    assert!(!refresh_observation!(pool));
    assert!(pool.state.lock().unwrap().invalid);
}

#[test]
fn expired_invalid_and_reduced_observations_never_overcommit() {
    let pool = SharedMemoryReservations::default();
    assert!(pool.try_reserve(1).is_none());
    let mut sample = observation!();
    assert!(pool.refresh(&sample));
    let lease = pool.try_reserve(600).unwrap();
    assert!(pool.try_reserve(201).is_none());
    assert!(pool.try_reserve(u64::MAX).is_none());
    let expires = pool.state.lock().unwrap().expires.unwrap();
    assert!(pool.try_reserve_at(1, expires).is_none());

    sample.guest.available_bytes = 500;
    assert!(pool.refresh(&sample));
    assert!(pool.try_reserve(1).is_none());
    drop(lease);
    assert!(pool.try_reserve(400).is_some());
    sample.cgroups = None;
    assert!(!pool.refresh(&sample));
    assert!(pool.try_reserve(1).is_none());
    sample.cgroups = Some(&[]);
    assert!(
        !pool.refresh(&sample),
        "revocation cannot replay the same observation"
    );
}

#[test]
fn held_reservation_rechecks_capacity_expiry_and_revocation_without_waiting() {
    let pool = SharedMemoryReservations::default();
    let mut sample = observation!();
    assert!(pool.refresh(&sample));
    let held = pool.try_reserve(600).unwrap();
    assert!(held.is_current());
    let guard = pool.state.lock().unwrap();
    let expires = guard.expires.unwrap();
    assert!(
        !held.is_current(),
        "contended custody must not block an effect"
    );
    drop(guard);
    assert!(!held.is_current_at(expires));
    sample.guest.available_bytes = 500;
    assert!(pool.refresh(&sample));
    assert!(
        !held.is_current(),
        "a reduced ceiling cannot cover this claim"
    );
    sample.cgroups = None;
    assert!(!pool.refresh(&sample));
    assert!(
        !held.is_current(),
        "an invalid observation revokes admission"
    );
    assert_eq!(
        pool.state.lock().unwrap().reserved,
        600,
        "revocation retains custody"
    );
    drop(held);
    let mut state = pool.state.lock().unwrap();
    assert!(pool.state.settle(&mut state));
    assert_eq!(state.reserved, 0);
}

#[test]
fn observation_of_reservation_expires_with_exact_owner_not_replacement() {
    let pool = SharedMemoryReservations::default();
    assert!(refresh_observation!(pool));
    let held = pool.try_reserve(600).unwrap();
    let probe = held.probe();
    assert!(probe());
    drop(held);
    let replacement = pool.try_reserve(600).unwrap();
    assert!(!probe(), "another reservation must not revive an old probe");
    assert!(replacement.is_current());
}

#[test]
fn refresh_preserves_the_original_observation_deadline() {
    let pool = SharedMemoryReservations::default();
    let mut sample = observation!();
    sample.max_age_ms = 1_000;
    sample.guest.sampled_at_unix_ms -= 700;
    let start = Instant::now();
    assert!(pool.refresh(&sample));
    let expires = pool.state.lock().unwrap().expires.unwrap();
    assert!(expires <= start + Duration::from_millis(310));
    assert!(
        pool.try_reserve_at(1, start + Duration::from_millis(320))
            .is_none()
    );
}

#[test]
fn older_observation_cannot_increase_newer_headroom() {
    let pool = SharedMemoryReservations::default();
    let mut newest = observation!();
    newest.guest.available_bytes = 300;
    assert!(pool.refresh(&newest));
    let mut older = observation!();
    older.guest.sampled_at_unix_ms = newest.guest.sampled_at_unix_ms - 1;
    assert!(!pool.refresh(&older));
    assert!(pool.try_reserve(201).is_none());
    assert!(pool.try_reserve(200).is_some());
}

#[test]
fn lock_delay_and_poison_cannot_admit_memory() {
    let pool = SharedMemoryReservations::default();
    let mut sample = observation!();
    sample.max_age_ms = 200;
    assert!(pool.refresh(&sample));
    let lock = pool.state.lock().unwrap();
    let delayed = pool.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let task = std::thread::spawn(move || {
        ready_tx.send(()).unwrap();
        delayed.try_reserve(1)
    });
    ready_rx.recv().unwrap();
    std::thread::sleep(Duration::from_millis(250));
    drop(lock);
    assert!(task.join().unwrap().is_none());

    let poison = pool.clone();
    assert!(
        std::thread::spawn(move || {
            let _guard = poison.state.lock().unwrap();
            panic!("poison reservation fixture");
        })
        .join()
        .is_err()
    );
    assert!(!refresh_observation!(pool));
    assert!(pool.try_reserve(1).is_none());
}

#[test]
fn shared_ceiling_includes_host_commit_and_cgroup_ancestors() {
    use crate::host_memory_budget::HostSample;
    let pool = SharedMemoryReservations::default();
    let mut sample = observation!();
    let now = sample.guest.sampled_at_unix_ms;
    let groups = [CgroupSample {
        identity: crate::host_memory_budget::CgroupIdentity {
            device: 1,
            inode: 1,
        },
        limit_bytes: Some(350),
        current_bytes: 100,
        sampled_at_unix_ms: now,
    }];
    sample.cgroups = Some(&groups);
    sample.host = HostScope::Wsl(HostSample {
        physical: MemorySample {
            total_bytes: 1000,
            available_bytes: 800,
            sampled_at_unix_ms: now,
        },
        commit_total_bytes: 700,
        commit_limit_bytes: 1000,
    });
    sample.host_reserve_bytes = 200;
    assert!(pool.refresh(&sample));
    assert!(pool.try_reserve(101).is_none());
    assert!(pool.try_reserve(100).is_some());
}

#[test]
fn inconsistent_custody_permanently_refuses_new_grants() {
    let pool = SharedMemoryReservations::default();
    assert!(refresh_observation!(pool));
    let lease = pool.try_reserve(10).unwrap();
    pool.state.lock().unwrap().reserved = 0;
    assert!(!lease.is_current());
    drop(lease);
    assert!(!refresh_observation!(pool));
    assert!(pool.try_reserve(1).is_none());
}

#[test]
fn newer_guest_cannot_mask_an_older_host_budget() {
    use crate::host_memory_budget::HostSample;
    let pool = SharedMemoryReservations::default();
    let mut first = observation!();
    let now = first.guest.sampled_at_unix_ms;
    first.guest.sampled_at_unix_ms = now - 1_000;
    first.host = HostScope::Wsl(HostSample {
        physical: MemorySample {
            total_bytes: 1000,
            available_bytes: 100,
            sampled_at_unix_ms: now,
        },
        commit_total_bytes: 0,
        commit_limit_bytes: 1000,
    });
    assert!(pool.refresh(&first));
    let mut second = first;
    second.guest.sampled_at_unix_ms = now - 500;
    if let HostScope::Wsl(ref mut host) = second.host {
        host.physical.sampled_at_unix_ms = now - 500;
        host.physical.available_bytes = 900;
    }
    assert!(!pool.refresh(&second));
    assert!(pool.try_reserve(101).is_none());
    assert!(pool.try_reserve(100).is_some());
}

#[test]
fn newer_guest_cannot_mask_an_older_cgroup_budget() {
    let pool = SharedMemoryReservations::default();
    let mut first = observation!();
    let now = first.guest.sampled_at_unix_ms;
    first.guest.sampled_at_unix_ms = now - 1_000;
    let groups = [CgroupSample {
        identity: crate::host_memory_budget::CgroupIdentity {
            device: 1,
            inode: 1,
        },
        limit_bytes: Some(200),
        current_bytes: 0,
        sampled_at_unix_ms: now,
    }];
    first.cgroups = Some(&groups);
    assert!(pool.refresh(&first));
    let mut second = observation!();
    second.guest.sampled_at_unix_ms = now - 500;
    let older_groups = [CgroupSample {
        identity: crate::host_memory_budget::CgroupIdentity {
            device: 1,
            inode: 1,
        },
        limit_bytes: Some(900),
        current_bytes: 0,
        sampled_at_unix_ms: now - 500,
    }];
    second.cgroups = Some(&older_groups);
    assert!(!pool.refresh(&second));
    assert!(pool.try_reserve(101).is_none());
    assert!(pool.try_reserve(100).is_some());
}

#[test]
fn unchanged_source_epoch_cannot_raise_the_shared_ceiling() {
    use crate::host_memory_budget::HostSample;
    let pool = SharedMemoryReservations::default();
    let mut sample = observation!();
    let now = sample.guest.sampled_at_unix_ms;
    sample.host = HostScope::Wsl(HostSample {
        physical: MemorySample {
            total_bytes: 1000,
            available_bytes: 100,
            sampled_at_unix_ms: now,
        },
        commit_total_bytes: 0,
        commit_limit_bytes: 1000,
    });
    assert!(pool.refresh(&sample));
    if let HostScope::Wsl(ref mut host) = sample.host {
        host.physical.available_bytes = 900;
    }
    assert!(!pool.refresh(&sample));
    assert!(pool.try_reserve(101).is_none());
}

#[test]
fn changed_cgroup_shape_or_host_scope_does_not_reuse_epoch_custody() {
    let pool = SharedMemoryReservations::default();
    let sample = observation!();
    assert!(pool.refresh(&sample));
    let mut changed = observation!();
    let group = [CgroupSample {
        identity: crate::host_memory_budget::CgroupIdentity {
            device: 1,
            inode: 2,
        },
        limit_bytes: None,
        current_bytes: 0,
        sampled_at_unix_ms: changed.guest.sampled_at_unix_ms,
    }];
    changed.cgroups = Some(&group);
    assert!(!pool.refresh(&changed));
    changed.cgroups = sample.cgroups;
    changed.host = HostScope::Wsl(crate::host_memory_budget::HostSample {
        physical: changed.guest,
        commit_total_bytes: 0,
        commit_limit_bytes: 1000,
    });
    assert!(!pool.refresh(&changed));
}

#[test]
fn newer_observations_can_increase_shared_ceiling() {
    let pool = SharedMemoryReservations::default();
    let mut sample = observation!();
    sample.guest.sampled_at_unix_ms -= 1000;
    sample.guest.available_bytes = 300;
    let first_groups = [CgroupSample {
        identity: CgroupIdentity {
            device: 1,
            inode: 1,
        },
        limit_bytes: None,
        current_bytes: 0,
        sampled_at_unix_ms: sample.guest.sampled_at_unix_ms,
    }];
    sample.cgroups = Some(&first_groups);
    assert!(pool.refresh(&sample));
    assert!(pool.try_reserve(201).is_none());
    sample.guest.sampled_at_unix_ms += 1;
    sample.guest.available_bytes = 600;
    let next_groups = [CgroupSample {
        sampled_at_unix_ms: sample.guest.sampled_at_unix_ms,
        ..first_groups[0]
    }];
    sample.cgroups = Some(&next_groups);
    assert!(pool.refresh(&sample));
    assert!(pool.try_reserve(500).is_some());
}

#[test]
fn replaced_cgroup_at_same_position_cannot_inherit_observation_custody() {
    use crate::host_memory_budget::CgroupIdentity;
    let pool = SharedMemoryReservations::default();
    let mut first = observation!();
    let now = first.guest.sampled_at_unix_ms;
    first.guest.sampled_at_unix_ms -= 500;
    let original = [CgroupSample {
        identity: CgroupIdentity {
            device: 1,
            inode: 10,
        },
        limit_bytes: Some(200),
        current_bytes: 0,
        sampled_at_unix_ms: now - 500,
    }];
    first.cgroups = Some(&original);
    assert!(pool.refresh(&first));
    let mut replacement = observation!();
    let other = [CgroupSample {
        identity: CgroupIdentity {
            device: 1,
            inode: 11,
        },
        limit_bytes: Some(900),
        current_bytes: 0,
        sampled_at_unix_ms: now,
    }];
    replacement.cgroups = Some(&other);
    assert!(!pool.refresh(&replacement));
    assert!(
        pool.try_reserve(1).is_none(),
        "replacement revokes prior admission"
    );
}
