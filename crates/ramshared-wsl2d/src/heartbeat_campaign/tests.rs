// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/heartbeat_campaign/tests.rs
#![allow(clippy::unwrap_used)]
use super::*;
use ramshared_ipc::vsock::MockVsockStream;
use ramshared_wsl2d::host_gate::{LeaseToken, SealedOrigin, SharedAuthority};
type Plane = VsockControlPlane<Box<dyn VsockStreamTrait>>;
fn input() -> (Plane, SessionAuthority, SharedAuthority, Arc<AtomicBool>) {
    let shared = SharedAuthority::new();
    let session = shared.begin_session();
    let (stream, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
    let plane = VsockControlPlane::new(
        Box::new(MockVsockStream(stream)) as Box<dyn VsockStreamTrait>,
        vec![1; 32],
        "test-boot".into(),
        "test-distro".into(),
        SealedOrigin {
            logical_capacity_mib: 4096,
            partuuid: "11111111-2222-3333-4444-555555555555".into(),
            origin_vhdx: "/dev/null".into(),
        },
        Duration::from_millis(10),
    );
    session.mutate(|slot| {
        slot.accept_lease(
            LeaseToken {
                lease_id: 1,
                deadline_ms: u64::MAX / 2,
            },
            plane.authority().origin().unwrap().clone(),
            0,
        )
        .unwrap()
    });
    (plane, session, shared, Arc::new(AtomicBool::new(false)))
}
fn cadence() -> NegotiatedCadence {
    NegotiatedCadence {
        heartbeat_secs: 1,
        lease_timeout_secs: 2,
    }
}
fn assert_revoked(shared: &SharedAuthority, lost: &AtomicBool) {
    assert!(lost.load(Ordering::Acquire));
    assert!(shared.slot().lock().unwrap().admit_cache(0).is_err());
    assert!(shared.slot().lock().unwrap().admit_origin_io().is_ok());
}
#[test]
fn heartbeat_cancellation_stops_idle_and_zero_liveness_watchdog() {
    let (plane, session, shared, lost) = input();
    let mut campaign = spawn_heartbeat(plane, session, cadence(), lost.clone()).unwrap();
    campaign.request_stop();
    assert!(campaign.join_within(Duration::from_secs(2)));
    assert!(campaign.watchdog.is_none() && campaign.beat.is_none());
    assert_revoked(&shared, &lost);
}
#[test]
fn heartbeat_timeout_retains_both_handles_until_actual_exit() {
    let release = Arc::new(AtomicBool::new(false));
    struct Release(Arc<AtomicBool>);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let mut campaign = HeartbeatCampaign::new(Arc::new(AtomicBool::new(false)));
    let _release = Release(release.clone());
    for handle in [&mut campaign.watchdog, &mut campaign.beat] {
        let gate = release.clone();
        *handle = Some(std::thread::spawn(move || {
            while !gate.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
        }));
    }
    assert!(!campaign.join_within(Duration::from_millis(1)));
    assert!(campaign.watchdog.is_some() && campaign.beat.is_some());
    release.store(true, Ordering::Release);
    assert!(campaign.join_within(Duration::from_secs(2)));
}

#[test]
fn heartbeat_expiry_and_transport_failure_join_and_revoke() {
    for expired in [true, false] {
        let (plane, session, shared, lost) = input();
        if expired {
            plane.liveness_handle().store(1, Ordering::Release);
        }
        let mut campaign =
            spawn_heartbeat(plane, session.clone(), cadence(), lost.clone()).unwrap();
        assert!(
            campaign.join_within(Duration::from_secs(4)),
            "children settled for expired={expired}"
        );
        assert!(!session.is_current());
        assert_revoked(&shared, &lost);
    }
}

#[test]
fn heartbeat_startup_failure_joins_prior_thread_and_retires_session() {
    use std::sync::atomic::AtomicUsize;
    for fail_at in [1, 2] {
        let (plane, session, shared, lost) = input();
        let finished = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(AtomicBool::new(false));
        let mut calls = 0;
        let result =
            spawn_heartbeat_with(plane, session.clone(), cadence(), lost.clone(), |_, run| {
                calls += 1;
                if calls == fail_at {
                    if fail_at == 2 {
                        let until = Instant::now() + Duration::from_secs(2);
                        while !started.load(Ordering::Acquire) && Instant::now() < until {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        assert!(started.load(Ordering::Acquire));
                    }
                    return Err(std::io::Error::other("controlled spawn failure"));
                }
                let done = finished.clone();
                let live = started.clone();
                std::thread::Builder::new().spawn(move || {
                    live.store(true, Ordering::Release);
                    run();
                    done.fetch_add(1, Ordering::AcqRel);
                })
            });
        assert!(result.is_err());
        assert_eq!(finished.load(Ordering::Acquire), fail_at - 1);
        assert!(!session.is_current());
        assert_revoked(&shared, &lost);
    }
}

#[test]
fn heartbeat_thread_panic_cancels_peer_and_retires_session() {
    for panic_at in [1, 2] {
        let (plane, session, shared, lost) = input();
        let mut calls = 0;
        let mut campaign =
            spawn_heartbeat_with(plane, session.clone(), cadence(), lost.clone(), |_, run| {
                calls += 1;
                let should_panic = calls == panic_at;
                std::thread::Builder::new().spawn(move || {
                    if should_panic {
                        let _owned_task = run;
                        panic!("controlled child startup panic");
                    }
                    run();
                })
            })
            .unwrap();
        assert!(campaign.join_within(Duration::from_secs(2)));
        assert!(!session.is_current());
        assert_revoked(&shared, &lost);
    }
}

#[test]
fn heartbeat_invalid_cadence_creates_no_threads() {
    let (plane, session, _, lost) = input();
    let result = spawn_heartbeat_with(
        plane,
        session,
        NegotiatedCadence {
            heartbeat_secs: 0,
            lease_timeout_secs: 2,
        },
        lost,
        |_, _| panic!("invalid policy cannot start native work"),
    );
    assert!(result.is_err());
    sleep_until_cancelled(Duration::ZERO, &AtomicBool::new(false));
}

#[test]
fn host_catalog_caps_heartbeat_interval_strictly_below_budget_max_age() {
    let five_sec = NegotiatedCadence {
        heartbeat_secs: 5,
        lease_timeout_secs: 15,
    };
    let with_catalog = effective_heartbeat_interval_ms(five_sec, true);
    assert_eq!(with_catalog, HOST_CATALOG_HEARTBEAT_CAP_MS);
    assert!(with_catalog < ramshared_vram::GPU_BUDGET_MAX_AGE_MS);
    assert_eq!(effective_heartbeat_interval_ms(five_sec, false), 5_000);
    // Refusal boundary: zero cadence remains zero so HeartbeatLoop::new rejects it.
    assert_eq!(
        effective_heartbeat_interval_ms(
            NegotiatedCadence {
                heartbeat_secs: 0,
                lease_timeout_secs: 15,
            },
            true
        ),
        0
    );
}
