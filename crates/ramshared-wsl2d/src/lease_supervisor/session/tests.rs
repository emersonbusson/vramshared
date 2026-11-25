// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/lease_supervisor/session/tests.rs
#![allow(clippy::unwrap_used)]
use super::*;
use crate::host_gate::{LeaseToken, SealedOrigin, SharedAuthority};
fn sample() -> AuthenticatedHostMemory {
    use ramshared_ipc::gpu_catalog::{CatalogSession, CatalogTiming};
    use ramshared_ipc::{HandshakeTranscript, HeartbeatAck, HostChallenge, HostMemoryTelemetry};
    let now_ms = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    let memory = HostMemoryTelemetry {
        sampled_at_unix_ms: now_ms,
        total_bytes: 8 << 30,
        available_bytes: 4 << 30,
        commit_total_bytes: 1 << 30,
        commit_limit_bytes: 10 << 30,
        page_size_bytes: 4096,
        vmmem_wsl_working_set_bytes: None,
    };
    let secret = b"session-memory-test-key";
    let transcript = HandshakeTranscript {
        min_version: 2,
        max_version: 4,
        boot_id: "boot".into(),
        distro_id: "test".into(),
        guest_nonce: [1; 32],
    };
    let challenge = HostChallenge {
        accepted_version: 4,
        heartbeat_secs: 1,
        lease_timeout_secs: 5,
        host_nonce: [2; 32],
    };
    let mut host = CatalogSession::new(secret, &transcript, &challenge, 1).unwrap();
    let mut guest = CatalogSession::new(secret, &transcript, &challenge, 1).unwrap();
    let ack = HeartbeatAck {
        timestamp_ms: now_ms,
        lease_remaining_ms: 5_000,
        host_memory: Some(memory),
    };
    let wire = host.seal(secret, &ack, None).unwrap();
    let now = std::time::Instant::now();
    guest
        .open_verified(
            secret,
            &ack,
            &wire,
            4,
            CatalogTiming {
                started: now,
                received: now,
            },
        )
        .unwrap()
        .host_memory
        .unwrap()
}
fn fixture() -> (SharedAuthority, SessionRun) {
    let shared = SharedAuthority::new();
    let session = shared.begin_session();
    session.mutate(|slot| {
        slot.accept_lease(
            LeaseToken {
                lease_id: 1,
                deadline_ms: u64::MAX / 2,
            },
            SealedOrigin {
                logical_capacity_mib: 4096,
                partuuid: "11111111-2222-3333-4444-555555555555".into(),
                origin_vhdx: "/dev/null".into(),
            },
            0,
        )
        .unwrap()
    });
    let owned = SessionRun::new(
        session,
        Arc::new(AtomicBool::new(false)),
        Arc::new(Mutex::new(None)),
        LeaseLog::capture().0,
        Duration::from_millis(1),
    );
    (shared, owned)
}
#[test]
fn session_stop_revokes_cache_and_joins_refresher() {
    let (shared, mut owned) = fixture();
    owned
        .start_refresher(Arc::new(Mutex::new(Some(sample()))))
        .unwrap();
    let until = std::time::Instant::now() + Duration::from_secs(2);
    while owned.memory.lock().unwrap().is_none() && std::time::Instant::now() < until {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(owned.memory.lock().unwrap().is_some());
    owned.request_stop();
    assert!(shared.slot().lock().unwrap().admit_cache(0).is_err());
    assert!(shared.slot().lock().unwrap().admit_origin_io().is_ok());
    assert!(owned.memory.lock().unwrap().is_none());
    owned.finish();
    owned.finish();
    assert!(owned.refresher.is_none());
    assert!(owned.lost.load(Ordering::Acquire));
}
#[test]
fn stale_session_stop_cannot_clear_newer_memory_or_lease() {
    let (shared, mut owned) = fixture();
    let newer = shared.begin_session();
    newer.mutate(|slot| {
        slot.accept_lease(
            LeaseToken {
                lease_id: 2,
                deadline_ms: u64::MAX / 2,
            },
            slot.origin().unwrap().clone(),
            0,
        )
        .unwrap()
    });
    *owned.memory.lock().unwrap() = Some(SessionHostMemory::new(newer.clone(), sample()));
    owned.finish();
    assert!(owned.memory.lock().unwrap().is_some());
    assert!(shared.slot().lock().unwrap().admit_cache(0).is_ok());
    assert!(!owned.lost.load(Ordering::Acquire));
}

#[test]
fn host_memory_snapshot_refuses_superseded_session_even_if_lease_id_repeats() {
    let (shared, owned) = fixture();
    let memory = SessionHostMemory::new(owned.session.clone(), sample());
    let now = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    assert!(memory.snapshot(&shared.slot()).is_some());
    let unrelated = SharedAuthority::new();
    assert!(memory.snapshot(&unrelated.slot()).is_none());
    let replacement = shared.begin_session();
    replacement.mutate(|slot| {
        slot.accept_lease(
            LeaseToken {
                lease_id: 1,
                deadline_ms: u64::MAX / 2,
            },
            slot.origin().unwrap().clone(),
            0,
        )
        .unwrap();
    });
    assert!(shared.slot().lock().unwrap().admit_cache(now).is_ok());
    assert!(memory.snapshot(&shared.slot()).is_none());
}

#[test]
fn host_memory_snapshot_refuses_busy_or_retired_session() {
    let (shared, owned) = fixture();
    let memory = SessionHostMemory::new(owned.session.clone(), sample());
    let slot = shared.slot();
    let locked = slot.lock().unwrap();
    assert!(memory.snapshot(&slot).is_none());
    drop(locked);
    assert!(memory.snapshot(&slot).is_some());
    owned.request_stop();
    assert!(memory.snapshot(&slot).is_none());
}

#[test]
fn stopped_session_rejects_late_authority_without_replacement_generation() {
    let (shared, owned) = fixture();
    let late = owned.session.clone();
    let before = shared.slot().lock().unwrap().clone();
    owned.request_stop();
    assert!(
        !late.publish(&before),
        "a pending old heartbeat cannot restore a stopped lease"
    );
    assert!(shared.slot().lock().unwrap().admit_cache(0).is_err());
    assert!(shared.slot().lock().unwrap().admit_origin_io().is_ok());
}

#[test]
fn session_drop_retains_pending_campaign_through_panic() {
    struct Held {
        child: Option<std::thread::JoinHandle<()>>,
        stopping: Arc<AtomicBool>,
    }
    impl CampaignHandle for Held {
        fn request_stop(&self) {
            self.stopping.store(true, Ordering::Release);
        }
        fn join_within(&mut self, _: Duration) -> bool {
            if !self.child.as_ref().unwrap().is_finished() {
                return false;
            }
            self.child.take().unwrap().join().unwrap();
            true
        }
    }
    struct Cleanup {
        release: Arc<AtomicBool>,
        parent: Option<std::thread::JoinHandle<bool>>,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            self.release.store(true, Ordering::Release);
            if let Some(parent) = self.parent.take() {
                let _ = parent.join();
            }
        }
    }
    let (shared, mut owned) = fixture();
    let late = owned.session.clone();
    let before = shared.slot().lock().unwrap().clone();
    let release = Arc::new(AtomicBool::new(false));
    let stopping = Arc::new(AtomicBool::new(false));
    let gate = release.clone();
    let child = std::thread::spawn(move || {
        while !gate.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    owned.attach(Box::new(Held {
        child: Some(child),
        stopping: stopping.clone(),
    }));
    let mut cleanup = Cleanup {
        release: release.clone(),
        parent: Some(std::thread::spawn(move || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                let _owned = owned;
                panic!("controlled session failure");
            }))
            .is_err()
        })),
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !stopping.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(stopping.load(Ordering::Acquire));
    assert!(!cleanup.parent.as_ref().unwrap().is_finished());
    assert!(!late.publish(&before));
    assert!(shared.slot().lock().unwrap().admit_cache(0).is_err());
    assert!(shared.slot().lock().unwrap().admit_origin_io().is_ok());
    release.store(true, Ordering::Release);
    assert!(cleanup.parent.take().unwrap().join().unwrap());
}
