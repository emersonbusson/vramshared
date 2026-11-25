// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/lease_supervisor.rs
//! Lease session supervision (SPEC `control-plane-and-release-resilience`
//! DT-3/DT-4, RF-3/RF-4/RF-8).
//!
//! One lease is one *session*: attempt → mint → heartbeat → wait for loss →
//! actual join → next attempt. The loop lives here, behind the
//! [`Supervisor`] seams, so the production composition (`main.rs`) and the
//! named SPEC tests (T3/T11) drive the same code instead of a copy.
//!
//! The authority is mutated only by the session whose generation is current
//! ([`crate::host_gate::SessionAuthority`]); a superseded session's late
//! write is a no-op. Generation fencing complements owned thread custody:
//! a join observation timeout retains the same session until actual settlement.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ramshared_ipc::gpu_catalog::AuthenticatedHostMemory;

use crate::host_gate::{SessionAuthority, SharedAuthority};
mod memory;
mod session;
pub use memory::SessionHostMemory;
use session::SessionRun;

#[cfg(test)]
mod custody_tests;

/// The poll cadence while waiting for a lease loss or shutdown.
const LOSS_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// One granted lease: the transport session plus the slot that carries the
/// host's live memory sample.
pub struct Granted<S> {
    pub session: S,
    pub host_memory: Arc<Mutex<Option<AuthenticatedHostMemory>>>,
}

/// A running campaign remains owned until both native threads actually join.
pub trait CampaignHandle: Send {
    /// Observe/join completed work without consuming pending handles.
    fn join_within(&mut self, deadline: Duration) -> bool;
    fn request_stop(&self);
}

/// The transition log (RF-8): one state line per transition, never per beat.
///
/// Production writes to stderr with the daemon's `[ramsharedd]` prefix. Tests
/// capture the same lines so they can assert the *absence* of per-iteration
/// logging — the regression that produced the 1.5M-line storm.
#[derive(Clone)]
pub struct LeaseLog {
    capture: Option<Arc<Mutex<Vec<String>>>>,
}

impl LeaseLog {
    /// The production sink.
    pub fn stderr() -> Self {
        Self { capture: None }
    }

    /// A sink that records state lines for assertions (tests only).
    #[cfg(test)]
    pub fn capture() -> (Self, Arc<Mutex<Vec<String>>>) {
        let sink = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                capture: Some(Arc::clone(&sink)),
            },
            sink,
        )
    }

    fn emit(&self, line: impl Into<String>) {
        let line = line.into();
        match &self.capture {
            Some(sink) => {
                if let Ok(mut lines) = sink.lock() {
                    lines.push(line);
                }
            }
            None => eprintln!("[ramsharedd] {line}"),
        }
    }
}

/// The lease-session supervisor (DT-3).
///
/// `attempt` and `start_campaign` are the transport seams; everything else —
/// generation fencing, the retry cadence, retained join — is owned here so
/// both the daemon and the named tests exercise one implementation.
pub struct Supervisor<A, K> {
    shared: SharedAuthority,
    lease_lost: Arc<AtomicBool>,
    lease_seen: Arc<AtomicBool>,
    host_memory: Arc<Mutex<Option<SessionHostMemory>>>,
    attempt: A,
    start_campaign: K,
    retry_interval: Duration,
    join_ceiling: Duration,
    log: LeaseLog,
}

impl<A, K, S> Supervisor<A, K>
where
    A: FnMut(SessionAuthority) -> Option<Granted<S>>,
    K: FnMut(
        Granted<S>,
        SessionAuthority,
        Arc<AtomicBool>,
    ) -> Result<Box<dyn CampaignHandle>, String>,
{
    /// Build a supervisor over the shared authority and its observations.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        shared: SharedAuthority,
        lease_lost: Arc<AtomicBool>,
        lease_seen: Arc<AtomicBool>,
        host_memory: Arc<Mutex<Option<SessionHostMemory>>>,
        attempt: A,
        start_campaign: K,
        retry_interval: Duration,
        log: LeaseLog,
    ) -> Self {
        Self {
            shared,
            lease_lost,
            lease_seen,
            host_memory,
            attempt,
            start_campaign,
            retry_interval,
            join_ceiling: retry_interval.max(Duration::from_secs(5)),
            log,
        }
    }

    /// Re-open the accept window until `stop` is set.
    ///
    /// Each iteration mints one session, waits for its loss, retires its
    /// writers, and actually joins all children before the next attempt.
    /// A bounded join observation retains pending work; it cannot replace it.
    pub fn run(mut self, stop: Arc<AtomicBool>) {
        while !stop.load(Ordering::Acquire) {
            self.run_once(&stop);
        }
    }

    /// One full session. Returns `true` when a lease was minted.
    fn run_once(&mut self, stop: &Arc<AtomicBool>) -> bool {
        let session = self.shared.begin_session();
        self.lease_lost.store(false, Ordering::Release);
        let mut owned = SessionRun::new(
            session.clone(),
            self.lease_lost.clone(),
            self.host_memory.clone(),
            self.log.clone(),
            self.join_ceiling,
        );
        let Some(granted) = self.acquire(&session, stop) else {
            return false;
        };
        self.lease_seen.store(true, Ordering::Release);
        self.log.emit("vsock lease granted; the cache is admitted");

        let memory = granted.host_memory.clone();
        let result = (self.start_campaign)(granted, session.clone(), Arc::clone(&self.lease_lost))
            .and_then(|campaign| {
                owned.attach(campaign);
                owned.start_refresher(memory)
            });
        if let Err(error) = result {
            self.log.emit(format!(
                "vsock session startup failed; cache refused: {error}"
            ));
            stop.store(true, Ordering::Release);
            return false;
        }
        while !self.lease_lost.load(Ordering::Acquire) && !stop.load(Ordering::Acquire) {
            std::thread::sleep(LOSS_POLL_INTERVAL);
        }
        let lost = self.lease_lost.load(Ordering::Acquire);
        if lost {
            self.log.emit(format!(
                "vsock lease lost; the accept window re-opens in {} s",
                self.retry_interval.as_secs()
            ));
        }
        owned.finish();
        if lost {
            sleep_unless_stopped(self.retry_interval, stop);
        }
        true
    }

    /// Re-open the bounded accept window until an attempt mints a lease.
    fn acquire(&mut self, session: &SessionAuthority, stop: &AtomicBool) -> Option<Granted<S>> {
        loop {
            if stop.load(Ordering::Acquire) {
                return None;
            }
            let granted = (self.attempt)(session.clone());
            if let Some(granted) = granted {
                if stop.load(Ordering::Acquire) {
                    return None;
                }
                return Some(granted);
            }
            session.mutate(|slot| slot.enter_origin_only());
            self.log.emit(format!(
                "vsock lease attempt failed; the cache stays refused and the accept window \
                 re-opens in {} s",
                self.retry_interval.as_secs()
            ));
            sleep_unless_stopped(self.retry_interval, stop);
        }
    }
}

/// Sleep in small slices so a shutdown does not wait for the full cadence.
fn sleep_unless_stopped(duration: Duration, stop: &AtomicBool) {
    let slice = Duration::from_millis(10);
    let mut remaining = duration;
    while remaining > Duration::ZERO && !stop.load(Ordering::Acquire) {
        let step = remaining.min(slice);
        std::thread::sleep(step);
        remaining = remaining.saturating_sub(step);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::atomic::AtomicUsize;

    use super::*;
    use crate::host_gate::{ControlPlaneState, LeaseToken, SealedOrigin};

    type AttemptFn = Box<dyn FnMut(SessionAuthority) -> Option<Granted<usize>>>;
    type CampaignFn = Box<
        dyn FnMut(
            Granted<usize>,
            SessionAuthority,
            Arc<AtomicBool>,
        ) -> Result<Box<dyn CampaignHandle>, String>,
    >;
    type EndFn = Box<dyn Fn(usize, &AtomicBool, &AtomicBool) + Send + Sync>;

    fn origin() -> SealedOrigin {
        SealedOrigin {
            logical_capacity_mib: 4096,
            partuuid: "11111111-2222-3333-4444-555555555555".into(),
            origin_vhdx: "/dev/null".into(),
        }
    }

    fn empty_host_memory<T>() -> Arc<Mutex<Option<T>>> {
        Arc::new(Mutex::new(None))
    }

    /// A campaign whose only job is to drive the test's loss/shutdown flags.
    struct TestCampaign(Option<std::thread::JoinHandle<()>>);

    impl CampaignHandle for TestCampaign {
        fn join_within(&mut self, _deadline: Duration) -> bool {
            if self.0.as_ref().is_some_and(|h| !h.is_finished()) {
                return false;
            }
            if let Some(handle) = self.0.take() {
                let _ = handle.join();
            }
            true
        }
        fn request_stop(&self) {}
    }

    struct Fixture {
        supervisor: Supervisor<AttemptFn, CampaignFn>,
        attempts: Arc<AtomicUsize>,
        mints: Arc<AtomicUsize>,
        shared: SharedAuthority,
        stop: Arc<AtomicBool>,
        lines: Arc<Mutex<Vec<String>>>,
    }

    /// The attempt always mints an accepted lease; the campaign calls `end`
    /// with the mint ordinal to drive loss/shutdown.
    fn fixture(end: EndFn) -> Fixture {
        let shared = SharedAuthority::new();
        shared
            .slot()
            .lock()
            .expect("slot")
            .adopt_verified_origin(origin());
        let attempts = Arc::new(AtomicUsize::new(0));
        let mints = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let lease_lost = Arc::new(AtomicBool::new(false));
        let (log, lines) = LeaseLog::capture();

        let attempt_counter = Arc::clone(&attempts);
        let attempt: AttemptFn = Box::new(move |session: SessionAuthority| {
            let n = attempt_counter.fetch_add(1, Ordering::SeqCst) + 1;
            session.mutate(|slot| {
                slot.accept_lease(
                    LeaseToken {
                        lease_id: u32::try_from(n).unwrap_or(u32::MAX),
                        deadline_ms: u64::MAX / 2,
                    },
                    origin(),
                    0,
                )
                .expect("accept lease")
            });
            Some(Granted {
                session: n,
                host_memory: empty_host_memory(),
            })
        });

        let campaign_mints = Arc::clone(&mints);
        let campaign_stop = Arc::clone(&stop);
        let end = Arc::new(end);
        let start: CampaignFn = Box::new(
            move |granted: Granted<usize>, _session: SessionAuthority, lost: Arc<AtomicBool>| {
                let n = campaign_mints.fetch_add(1, Ordering::SeqCst) + 1;
                assert!(_session.lease_is_current(u32::try_from(n).unwrap()));
                let end = Arc::clone(&end);
                let stop = Arc::clone(&campaign_stop);
                let handle = std::thread::spawn(move || end(n, &lost, &stop));
                let _ = granted;
                Ok(Box::new(TestCampaign(Some(handle))) as Box<dyn CampaignHandle>)
            },
        );

        let supervisor = Supervisor::new(
            shared.clone(),
            lease_lost,
            Arc::new(AtomicBool::new(false)),
            empty_host_memory(),
            attempt,
            start,
            Duration::from_millis(20),
            log,
        );
        Fixture {
            supervisor,
            attempts,
            mints,
            shared,
            stop,
            lines,
        }
    }

    // SPEC control-plane-and-release-resilience T3 (RF-3/DT-3): after a
    // minted-then-lost lease the accept window re-opens and the second
    // attempt admits a live lease; shutdown then revokes it.
    #[test]
    fn lease_reacquisition_reopens_after_loss() {
        // First campaign loses the lease; the second ends the test.
        let fixture = fixture(Box::new(|n, lost, stop| {
            std::thread::sleep(Duration::from_millis(40));
            if n == 1 {
                lost.store(true, Ordering::Release);
            } else {
                stop.store(true, Ordering::Release);
            }
        }));
        fixture.supervisor.run(Arc::clone(&fixture.stop));

        assert_eq!(
            fixture.attempts.load(Ordering::SeqCst),
            2,
            "a lost lease must re-open the accept window exactly once more"
        );
        assert_eq!(
            fixture.mints.load(Ordering::SeqCst),
            2,
            "two sessions were minted"
        );
        assert_eq!(
            fixture.shared.slot().lock().expect("slot").state(),
            ControlPlaneState::OriginOnly,
            "shutdown revokes the restored lease while preserving the origin"
        );
    }

    // SPEC T11 (RF-8/DT-8): one state line per re-mint and none per
    // heartbeat/iteration — the log storm regression.
    #[test]
    fn re_minted_lease_logs_one_transition() {
        let fixture = fixture(Box::new(|n, lost, stop| {
            lost.store(true, Ordering::Release);
            if n == 3 {
                stop.store(true, Ordering::Release);
            }
        }));
        fixture.supervisor.run(Arc::clone(&fixture.stop));

        let minted = fixture.mints.load(Ordering::SeqCst);
        assert_eq!(minted, 3, "the fixture should mint three leases");
        let captured = fixture.lines.lock().expect("lines");
        let granted = captured
            .iter()
            .filter(|line| line.contains("vsock lease granted"))
            .count();
        let lost = captured
            .iter()
            .filter(|line| line.contains("vsock lease lost"))
            .count();
        assert_eq!(granted, minted, "one granted line per mint, not per beat");
        assert_eq!(lost, minted, "one lost line per loss, not per poll");
        assert!(
            !captured.iter().any(|line| line.contains("heartbeat")),
            "heartbeat ticks must never log: {captured:?}"
        );
    }

    // RF-3/DT-3 fail-closed unwind: a shutdown before any lease ends the
    // supervisor without minting, leaving the cache refused.
    #[test]
    fn shutdown_before_a_lease_mints_nothing() {
        let shared = SharedAuthority::new();
        shared
            .slot()
            .lock()
            .expect("slot")
            .adopt_verified_origin(origin());
        let stop = Arc::new(AtomicBool::new(true));
        let (log, lines) = LeaseLog::capture();
        let supervisor = Supervisor::new(
            shared.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            empty_host_memory(),
            |_session: SessionAuthority| -> Option<Granted<()>> { None },
            |_granted: Granted<()>,
             _session: SessionAuthority,
             _lost: Arc<AtomicBool>|
             -> Result<Box<dyn CampaignHandle>, String> {
                unreachable!("no campaign without a lease")
            },
            Duration::from_millis(10),
            log,
        );
        supervisor.run(Arc::clone(&stop));

        assert_eq!(
            shared.slot().lock().expect("slot").state(),
            ControlPlaneState::Disconnected,
            "a shutdown before any lease must not fabricate authority"
        );
        assert!(
            lines.lock().expect("lines").is_empty(),
            "a stopped supervisor must not log a transition"
        );
    }

    // RF-8: a failed attempt logs one refusal per attempt, never per poll.
    #[test]
    fn failed_attempts_log_once_per_attempt() {
        let shared = SharedAuthority::new();
        shared
            .slot()
            .lock()
            .expect("slot")
            .adopt_verified_origin(origin());
        let stop = Arc::new(AtomicBool::new(false));
        let attempts = Arc::new(AtomicUsize::new(0));
        let (log, lines) = LeaseLog::capture();
        let attempt_counter = Arc::clone(&attempts);
        let attempt_stop = Arc::clone(&stop);
        let supervisor = Supervisor::new(
            shared.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            empty_host_memory(),
            move |_session: SessionAuthority| -> Option<Granted<()>> {
                let n = attempt_counter.fetch_add(1, Ordering::SeqCst) + 1;
                if n == 3 {
                    attempt_stop.store(true, Ordering::Release);
                }
                None
            },
            |_granted: Granted<()>,
             _session: SessionAuthority,
             _lost: Arc<AtomicBool>|
             -> Result<Box<dyn CampaignHandle>, String> {
                unreachable!("no campaign without a lease")
            },
            Duration::from_millis(10),
            log,
        );
        supervisor.run(Arc::clone(&stop));

        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        let captured = lines.lock().expect("lines");
        assert_eq!(
            captured
                .iter()
                .filter(|line| line.contains("vsock lease attempt failed"))
                .count(),
            3,
            "one refusal line per attempt: {captured:?}"
        );
        assert_eq!(
            shared.slot().lock().expect("slot").state(),
            ControlPlaneState::OriginOnly,
            "a refused attempt keeps only the verified origin"
        );
    }
}
