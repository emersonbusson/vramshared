// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/lease_supervisor/custody_tests.rs
#![allow(clippy::unwrap_used)]
use super::*;
use crate::host_gate::{LeaseToken, SealedOrigin};
use std::sync::atomic::AtomicUsize;
use std::thread::JoinHandle;
use std::time::Instant;

struct Pending {
    handle: Option<JoinHandle<()>>,
    abandoned: Arc<Mutex<Vec<JoinHandle<()>>>>,
}
impl CampaignHandle for Pending {
    fn join_within(&mut self, _: Duration) -> bool {
        if !self.handle.as_ref().unwrap().is_finished() {
            return false;
        }
        self.handle.take().unwrap().join().unwrap();
        true
    }
    fn request_stop(&self) {}
}
impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.abandoned.lock().unwrap().push(handle);
        }
    }
}
struct Cleanup {
    stop: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
    supervisor: Option<JoinHandle<()>>,
    abandoned: Arc<Mutex<Vec<JoinHandle<()>>>>,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.release.store(true, Ordering::Release);
        if let Some(handle) = self.supervisor.take() {
            handle.join().unwrap();
        }
        for handle in self.abandoned.lock().unwrap().drain(..) {
            handle.join().unwrap();
        }
    }
}
fn origin() -> SealedOrigin {
    SealedOrigin {
        logical_capacity_mib: 4096,
        partuuid: "11111111-2222-3333-4444-555555555555".into(),
        origin_vhdx: "/dev/null".into(),
    }
}
#[test]
fn lease_session_waits_for_actual_campaign_before_replacement() {
    let shared = SharedAuthority::new();
    shared
        .slot()
        .lock()
        .unwrap()
        .adopt_verified_origin(origin());
    let lost = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let started = Arc::new(AtomicUsize::new(0));
    let abandoned = Arc::new(Mutex::new(Vec::new()));
    let mut cleanup = Cleanup {
        stop: stop.clone(),
        release: release.clone(),
        supervisor: None,
        abandoned: abandoned.clone(),
    };
    let attempt_count = Arc::new(AtomicUsize::new(0));
    let attempts = attempt_count.clone();
    let campaign_count = started.clone();
    let supervisor = Supervisor::new(
        shared.clone(),
        lost,
        Arc::new(AtomicBool::new(false)),
        Arc::new(Mutex::new(None)),
        move |session: SessionAuthority| {
            let count = attempts.fetch_add(1, Ordering::AcqRel) + 1;
            session.mutate(|slot| {
                slot.accept_lease(
                    LeaseToken {
                        lease_id: count as u32,
                        deadline_ms: u64::MAX / 2,
                    },
                    origin(),
                    0,
                )
                .unwrap()
            });
            Some(Granted {
                session: (),
                host_memory: Arc::new(Mutex::new(None)),
            })
        },
        move |_: Granted<()>, _: SessionAuthority, lost: Arc<AtomicBool>| {
            let released = release.clone();
            let handle = std::thread::spawn(move || {
                while !released.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(1));
                }
            });
            campaign_count.fetch_add(1, Ordering::AcqRel);
            lost.store(true, Ordering::Release);
            Ok(Box::new(Pending {
                handle: Some(handle),
                abandoned: abandoned.clone(),
            }) as Box<dyn CampaignHandle>)
        },
        Duration::from_millis(1),
        LeaseLog::capture().0,
    );
    cleanup.supervisor = Some(std::thread::spawn(move || supervisor.run(stop)));
    let deadline = Instant::now() + Duration::from_secs(2);
    while started.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(started.load(Ordering::Acquire) > 0);
    std::thread::sleep(Duration::from_millis(50));
    assert!(shared.slot().lock().unwrap().admit_origin_io().is_ok());
    assert_eq!(
        attempt_count.load(Ordering::Acquire),
        1,
        "pending actual campaign cannot be replaced"
    );
    assert!(
        cleanup.abandoned.lock().unwrap().is_empty(),
        "pending handles stay with their session"
    );
    cleanup.release.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(2);
    while attempt_count.load(Ordering::Acquire) < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        attempt_count.load(Ordering::Acquire) >= 2,
        "settled campaign permits re-acquisition"
    );
}

#[test]
fn lease_start_failure_panic_and_cancelled_accept_revoke_authority() {
    for outcome in ["failure", "panic", "cancelled"] {
        let shared = SharedAuthority::new();
        let stop = Arc::new(AtomicBool::new(false));
        let signal = stop.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let campaign_calls = calls.clone();
        let supervisor = Supervisor::new(
            shared.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None)),
            move |session: SessionAuthority| {
                session.mutate(|slot| {
                    slot.accept_lease(
                        LeaseToken {
                            lease_id: 1,
                            deadline_ms: u64::MAX / 2,
                        },
                        origin(),
                        0,
                    )
                    .unwrap()
                });
                if outcome == "cancelled" {
                    signal.store(true, Ordering::Release);
                }
                Some(Granted {
                    session: (),
                    host_memory: Arc::new(Mutex::new(None)),
                })
            },
            move |_: Granted<()>,
                  _: SessionAuthority,
                  _: Arc<AtomicBool>|
                  -> Result<Box<dyn CampaignHandle>, String> {
                campaign_calls.fetch_add(1, Ordering::AcqRel);
                if outcome == "panic" {
                    panic!("controlled campaign startup panic");
                }
                Err("controlled campaign startup failure".into())
            },
            Duration::from_millis(1),
            LeaseLog::capture().0,
        );
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            supervisor.run(stop.clone())
        }));
        assert_eq!(result.is_err(), outcome == "panic");
        assert_eq!(
            calls.load(Ordering::Acquire),
            usize::from(outcome != "cancelled")
        );
        assert!(shared.slot().lock().unwrap().admit_cache(0).is_err());
        assert!(shared.slot().lock().unwrap().admit_origin_io().is_ok());
    }
}
