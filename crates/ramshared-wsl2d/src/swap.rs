// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/swap.rs
//! Tier `swapoff` (DEMOTE path). Extracted from the daemon for reuse by both
//! transports: NBD (`main.rs`) and the ublk worker DT-3 (`ublk_server.rs`). The
//! swapoff runs **outside** of the path serving the swap (separate thread) — Discipline 3
//! (anti-deadlock): blocking the server during swapoff would hang the swap itself.
#![allow(unsafe_code)]

use crate::tier_release::{self, ReleaseOutcome};
use std::ffi::CString;
use std::io;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

/// First retry delay after a transient `swapoff` rejection.
///
/// Policy, not a measurement: it exists so a fast-failing `swapoff` cannot
/// spin the DEMOTE path on every loop tick. The schedule doubles up to
/// [`MAX_REJECT_DELAY`]; it does not claim anything about how long a real
/// drain takes.
const INITIAL_REJECT_DELAY: Duration = Duration::from_secs(5);

/// Ceiling on the transient-rejection backoff.
///
/// Bounded so a rejection that later becomes retryable (the absorber frees
/// up) is not starved by an unbounded schedule.
const MAX_REJECT_DELAY: Duration = Duration::from_secs(80);

/// Age at which a still-pending DEMOTE drain is reported stuck.
///
/// SPEC `wsl2-cascade-swap` §9.2: `T_demote` is a **detection** window, not an
/// abort. A `swapoff` blocked inside `try_to_unuse` is in D state and cannot be
/// cancelled from userspace — neither SIGTERM nor SIGKILL returns before the
/// syscall does — so nothing here may kill or abandon the child (Day-0 forbids
/// that class of workaround). The value is **not calibrated**: verified test case drained
/// ~2.8 GiB in 142 s, almost five windows. Treat it as stuck-detection only and
/// never fail a run solely because a healthy drain outlived it.
pub const DEMOTE_STUCK_DETECTION_WINDOW: Duration = Duration::from_secs(30);

/// Whether a still-pending `swapoff` has outlived [`DEMOTE_STUCK_DETECTION_WINDOW`]
/// and has not yet been reported stuck.
///
/// Pure, so the schedule is testable without waiting on a real D-state drain.
/// Reporting is edge-triggered: the caller clears the flag when the drain
/// reaches a terminal outcome, and this returns `true` at most once per flight.
pub fn demote_stuck_report_due(
    started: Option<Instant>,
    now: Instant,
    window: Duration,
    already_reported: bool,
) -> bool {
    if already_reported {
        return false;
    }
    match started {
        Some(started) => now.saturating_duration_since(started) >= window,
        None => false,
    }
}

/// Terminal classification of a DEMOTE `swapoff` attempt.
///
/// A bare `bool` cannot express the split the disciplines require: re-arming
/// the canary on every failure is the fast-fail retry loop, and it never
/// distinguishes "this will never work" from "not yet".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapoffOutcome {
    /// The shared owner verified target absence and settled its owned effects.
    Drained,
    /// The observation worker could not be started at all.
    /// Deterministic for the lifetime of this process: never retried
    ///.
    Unavailable,
    /// Admission was refused or the owned operation terminated without drain
    /// proof. A later observation may allow a backed-off retry.
    Rejected {
        /// Process exit code when available; `None` for an admission refusal.
        exit_code: Option<i32>,
    },
    /// The native `swapoff(2)` call failed. The errno is the classification:
    /// `EBUSY`/`ENOMEM` may clear, a missing device is already drained.
    SyscallFailed { errno: i32 },
}

impl SwapoffOutcome {
    /// only a transient rejection may be retried.
    ///
    /// `Drained` is not a failure, so it is not "transient" either; the
    /// authoritative re-arm decision is [`DemoteRearm::record`].
    pub const fn is_transient(self) -> bool {
        matches!(self, Self::Rejected { .. } | Self::SyscallFailed { .. })
    }
}

/// Re-arm policy for the DEMOTE canary after a terminal `swapoff` outcome.
///
/// One of these sits beside `demote_rx` in each transport loop. The canary may
/// spawn a new `swapoff` only when [`Self::may_attempt`] allows it, so a
/// deterministic failure parks the path and a transient rejection delays it.
/// Shared by both transports so the two loops cannot drift into different
/// retry behaviour.
#[derive(Debug, Clone, Copy)]
pub struct DemoteRearm {
    consecutive_rejections: u32,
    not_before: Option<Instant>,
    parked: bool,
}

impl Default for DemoteRearm {
    fn default() -> Self {
        Self::new()
    }
}

impl DemoteRearm {
    /// A fresh policy: the first attempt is always authorized.
    pub const fn new() -> Self {
        Self {
            consecutive_rejections: 0,
            not_before: None,
            parked: false,
        }
    }

    /// Delay before the next attempt after `consecutive_rejections` transient
    /// rejections. Doubles from [`INITIAL_REJECT_DELAY`] and saturates at
    /// [`MAX_REJECT_DELAY`]. Pure, so the schedule is testable without
    /// waiting.
    pub fn reject_delay(consecutive_rejections: u32) -> Duration {
        if consecutive_rejections == 0 {
            return Duration::ZERO;
        }
        // saturating: a very long streak must not overflow into a zero delay.
        let shifts = consecutive_rejections.saturating_sub(1).min(16);
        let delay = INITIAL_REJECT_DELAY.saturating_mul(1u32 << shifts);
        if delay > MAX_REJECT_DELAY {
            MAX_REJECT_DELAY
        } else {
            delay
        }
    }

    /// Record a terminal `swapoff` outcome.
    pub fn record(&mut self, outcome: SwapoffOutcome, now: Instant) {
        match outcome {
            SwapoffOutcome::Drained => {
                self.consecutive_rejections = 0;
                self.not_before = None;
                self.parked = false;
            }
            SwapoffOutcome::Unavailable => {
                // Deterministic: the binary cannot run here. Retrying only
                // spins the loop and hides the real problem.
                self.parked = true;
                self.not_before = None;
            }
            SwapoffOutcome::Rejected { .. } | SwapoffOutcome::SyscallFailed { .. } => {
                self.parked = false;
                self.consecutive_rejections = self.consecutive_rejections.saturating_add(1);
                self.not_before = Some(now + Self::reject_delay(self.consecutive_rejections));
            }
        }
    }

    /// Whether a new `swapoff` attempt is authorized at `now`.
    pub fn may_attempt_at(&self, now: Instant) -> bool {
        if self.parked {
            return false;
        }
        match self.not_before {
            None => true,
            Some(not_before) => now >= not_before,
        }
    }

    /// Whether a deterministic failure has permanently stopped the path.
    pub const fn is_parked(&self) -> bool {
        self.parked
    }

    /// Consecutive transient rejections since the last drain.
    pub const fn consecutive_rejections(&self) -> u32 {
        self.consecutive_rejections
    }
}

/// Linux swap-priority flag encoding.
const SWAP_FLAG_PREFER: libc::c_int = 0x8000;
const SWAP_FLAG_PRIO_MASK: libc::c_int = 0x7fff;

/// Direct `swapon(2)` with the same prefer-priority encoding as `swapon -p`.
pub fn swapon_syscall(dev: &str, priority: i16) -> io::Result<()> {
    if dev.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "device path is empty",
        ));
    }
    let path = CString::new(dev.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "device path contains NUL"))?;
    let flags = SWAP_FLAG_PREFER | (libc::c_int::from(priority) & SWAP_FLAG_PRIO_MASK);
    // SAFETY: `path` is a NUL-terminated CString that outlives the syscall.
    let result = unsafe { libc::swapon(path.as_ptr(), flags) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

pub fn activate_swap(dev: &str, priority: i16) -> bool {
    swapon_syscall(dev, priority).is_ok()
}

/// Starts `swapon` outside the NBD serving thread and returns only its terminal
/// result. `swapon` reads the exported device while checking its header, so the
/// single NBD worker must keep serving requests until this receiver completes.
pub fn spawn_activate_swap(dev: &str, priority: i16) -> std::io::Result<Receiver<bool>> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let dev = dev.to_string();
    std::thread::Builder::new()
        .name("ramshared-nbd-swapon".into())
        .spawn(move || {
            let ok = activate_swap(&dev, priority);
            let _ = tx.send(ok);
        })?;
    Ok(rx)
}

/// Owns one shared managed-tier flight off the serving thread. Pending custody
/// keeps both the flight and sender alive until the owner observes a terminal
/// result. A detection deadline never becomes permission to retry or free RAM.
pub fn spawn_swapoff(dev: &str) -> Receiver<SwapoffOutcome> {
    spawn_swapoff_inner(dev, false)
}

/// Attempt recovery only after the NBD peer is already lost; no safety claim.
pub fn spawn_peer_closed_swapoff(dev: &str) -> Receiver<SwapoffOutcome> {
    spawn_swapoff_inner(dev, true)
}

fn spawn_swapoff_inner(dev: &str, peer_closed: bool) -> Receiver<SwapoffOutcome> {
    let Some(mut flight) = tier_release::try_begin_release(dev) else {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let _ = tx.send(SwapoffOutcome::Rejected { exit_code: None });
        return rx;
    };
    spawn_swapoff_worker(move || {
        let outcome = if peer_closed {
            tier_release::release_managed_tier_after_peer_closed(
                &mut flight,
                DEMOTE_STUCK_DETECTION_WINDOW,
            )
        } else {
            tier_release::release_managed_tier_outcome(&mut flight, DEMOTE_STUCK_DETECTION_WINDOW)
        };
        await_managed_release(
            outcome,
            || {
                tier_release::resume_owned_absorber_cleanup(
                    &mut flight,
                    DEMOTE_STUCK_DETECTION_WINDOW,
                )
            },
            || std::thread::sleep(Duration::from_millis(100)),
        )
    })
}

fn await_managed_release(
    mut outcome: ReleaseOutcome,
    mut progress: impl FnMut() -> ReleaseOutcome,
    mut wait: impl FnMut(),
) -> SwapoffOutcome {
    loop {
        match outcome {
            ReleaseOutcome::Released => return SwapoffOutcome::Drained,
            ReleaseOutcome::Refused => return SwapoffOutcome::Rejected { exit_code: None },
            ReleaseOutcome::Pending => {
                wait();
                outcome = progress();
            }
        }
    }
}

fn spawn_swapoff_worker(
    run: impl FnOnce() -> SwapoffOutcome + Send + 'static,
) -> Receiver<SwapoffOutcome> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let failure_tx = tx.clone();
    if std::thread::Builder::new()
        .name("ramshared-managed-swapoff".into())
        .spawn(move || {
            let _ = tx.send(run());
        })
        .is_err()
    {
        let _ = failure_tx.send(SwapoffOutcome::Unavailable);
    }
    rx
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn executable_overrides_cannot_activate_or_claim_drained() {
        const CHILD_CASE: &str = "RAMSHARED_TEST_SWAP_OVERRIDE";
        if let Ok(case) = std::env::var(CHILD_CASE) {
            match case.as_str() {
                "swapon" => assert!(!activate_swap("", 10)),
                "swapoff" => assert_eq!(
                    spawn_swapoff("")
                        .recv_timeout(Duration::from_secs(1))
                        .unwrap(),
                    SwapoffOutcome::Rejected { exit_code: None },
                ),
                _ => panic!("unknown fixture case"),
            }
            return;
        }
        let results = ["swapon", "swapoff"].map(|case| {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "swap::tests::executable_overrides_cannot_activate_or_claim_drained",
                    "--nocapture",
                ])
                .env(CHILD_CASE, case)
                .env("RAMSHARED_SWAPON_BIN", "/bin/true")
                .env("RAMSHARED_SWAPOFF_BIN", "/bin/true")
                .output()
                .unwrap()
        });
        let failures = results
            .iter()
            .filter(|result| !result.status.success())
            .map(|result| {
                format!(
                    "{}{}",
                    String::from_utf8_lossy(&result.stdout),
                    String::from_utf8_lossy(&result.stderr)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(failures.is_empty(), "{failures}");
    }

    #[test]
    fn managed_swapoff_keeps_pending_receiver_and_flight_until_terminal() {
        use crate::tier_release::ReleaseOutcome;
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        struct Flight(Arc<AtomicBool>);
        impl Drop for Flight {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let held = Arc::new(AtomicBool::new(true));
        let flight = Flight(Arc::clone(&held));
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let outcome = spawn_swapoff_worker(move || {
            let _flight = flight;
            await_managed_release(
                ReleaseOutcome::Pending,
                || rx.recv_timeout(Duration::from_secs(2)).unwrap(),
                || {},
            )
        });
        assert!(matches!(
            outcome.recv_timeout(Duration::from_millis(20)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        assert!(held.load(Ordering::Acquire));
        tx.send(ReleaseOutcome::Pending).unwrap();
        assert!(matches!(
            outcome.recv_timeout(Duration::from_millis(20)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        tx.send(ReleaseOutcome::Released).unwrap();
        assert_eq!(
            outcome.recv_timeout(Duration::from_secs(2)).unwrap(),
            SwapoffOutcome::Drained
        );
        assert!(!held.load(Ordering::Acquire));
    }

    #[test]
    fn managed_swapoff_refusal_retains_the_backoff_contract() {
        let rx = spawn_swapoff_worker(|| {
            await_managed_release(
                crate::tier_release::ReleaseOutcome::Refused,
                || panic!("terminal refusal must not poll or rerun effects"),
                || panic!("terminal refusal must not wait"),
            )
        });
        let outcome = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(outcome, SwapoffOutcome::Rejected { exit_code: None });
        let now = Instant::now();
        let mut rearm = DemoteRearm::new();
        rearm.record(outcome, now);
        assert!(!rearm.may_attempt_at(now));
        assert!(rearm.may_attempt_at(now + INITIAL_REJECT_DELAY));
    }

    #[test]
    fn native_swapon_rejects_invalid_paths_before_the_syscall() {
        assert_eq!(
            swapon_syscall("", 10).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(swapon_syscall("bad\0path", 10).is_err());
    }

    #[test]
    fn test_activate_swap_invalid_dev() {
        assert!(!activate_swap("", 10));
    }

    #[test]
    fn test_spawn_activate_swap_invalid_dev() {
        let rx = spawn_activate_swap("", 10).expect("bounded activation supervisor starts");
        assert!(
            !rx.recv()
                .expect("activation worker reports terminal result")
        );
    }

    #[test]
    fn test_spawn_swapoff_invalid_dev() {
        let rx = spawn_swapoff("");
        let outcome = rx.recv().unwrap();
        assert!(
            matches!(outcome, SwapoffOutcome::Rejected { exit_code: None }),
            "expected a classified admission refusal, got {outcome:?}"
        );
    }

    #[test]
    fn test_spawn_swapoff_rejects_and_is_transient() {
        let rx = spawn_swapoff("");
        let outcome = rx.recv().unwrap();
        assert!(outcome.is_transient());
    }

    #[test]
    fn test_unavailable_is_deterministic_and_not_transient() {
        assert!(!SwapoffOutcome::Unavailable.is_transient());
        assert!(!SwapoffOutcome::Drained.is_transient());
        assert!(SwapoffOutcome::Rejected { exit_code: Some(2) }.is_transient());
        assert!(
            SwapoffOutcome::SyscallFailed {
                errno: libc::ENOENT
            }
            .is_transient()
        );
    }

    #[test]
    fn test_reject_delay_is_zero_before_any_rejection() {
        assert_eq!(DemoteRearm::reject_delay(0), Duration::ZERO);
    }

    #[test]
    fn test_reject_delay_doubles_and_saturates() {
        assert_eq!(DemoteRearm::reject_delay(1), Duration::from_secs(5));
        assert_eq!(DemoteRearm::reject_delay(2), Duration::from_secs(10));
        assert_eq!(DemoteRearm::reject_delay(3), Duration::from_secs(20));
        assert_eq!(DemoteRearm::reject_delay(4), Duration::from_secs(40));
        // reaches the ceiling and stays there, including on overflow-safe shifts
        assert_eq!(DemoteRearm::reject_delay(5), Duration::from_secs(80));
        assert_eq!(DemoteRearm::reject_delay(6), Duration::from_secs(80));
        assert_eq!(DemoteRearm::reject_delay(64), Duration::from_secs(80));
    }

    #[test]
    fn test_unavailable_parks_the_path() {
        let mut rearm = DemoteRearm::new();
        let now = Instant::now();
        assert!(rearm.may_attempt_at(now));
        rearm.record(SwapoffOutcome::Unavailable, now);
        assert!(rearm.is_parked());
        assert!(!rearm.may_attempt_at(now));
        assert!(!rearm.may_attempt_at(now + Duration::from_secs(3600)));
    }

    #[test]
    fn test_rejection_delays_the_next_attempt_then_allows_it() {
        let mut rearm = DemoteRearm::new();
        let now = Instant::now();
        rearm.record(SwapoffOutcome::Rejected { exit_code: Some(1) }, now);
        assert_eq!(rearm.consecutive_rejections(), 1);
        assert!(!rearm.may_attempt_at(now));
        assert!(!rearm.may_attempt_at(now + Duration::from_secs(4)));
        assert!(rearm.may_attempt_at(now + Duration::from_secs(5)));
    }

    #[test]
    fn test_drain_resets_the_streak() {
        let mut rearm = DemoteRearm::new();
        let now = Instant::now();
        rearm.record(SwapoffOutcome::Rejected { exit_code: Some(1) }, now);
        rearm.record(SwapoffOutcome::Rejected { exit_code: Some(1) }, now);
        assert_eq!(rearm.consecutive_rejections(), 2);
        rearm.record(SwapoffOutcome::Drained, now);
        assert_eq!(rearm.consecutive_rejections(), 0);
        assert!(!rearm.is_parked());
        assert!(rearm.may_attempt_at(now));
    }

    #[test]
    fn test_a_long_rejection_streak_still_allows_a_later_attempt() {
        // The ceiling exists so a transient rejection cannot starve a drain
        // that later becomes possible.
        let mut rearm = DemoteRearm::new();
        let now = Instant::now();
        for _ in 0..10 {
            rearm.record(SwapoffOutcome::Rejected { exit_code: Some(1) }, now);
        }
        assert!(rearm.may_attempt_at(now + Duration::from_secs(80)));
    }

    #[test]
    fn test_no_flight_is_never_stuck() {
        let now = Instant::now();
        assert!(!demote_stuck_report_due(
            None,
            now,
            DEMOTE_STUCK_DETECTION_WINDOW,
            false
        ));
    }

    #[test]
    fn test_flight_inside_the_window_is_not_stuck() {
        let now = Instant::now();
        let started = Some(now);
        assert!(!demote_stuck_report_due(
            started,
            now + Duration::from_secs(29),
            DEMOTE_STUCK_DETECTION_WINDOW,
            false
        ));
    }

    #[test]
    fn test_flight_at_the_window_is_stuck_once() {
        let now = Instant::now();
        let started = Some(now);
        assert!(demote_stuck_report_due(
            started,
            now + DEMOTE_STUCK_DETECTION_WINDOW,
            DEMOTE_STUCK_DETECTION_WINDOW,
            false
        ));
        // Edge-triggered: the caller reports once per flight.
        assert!(!demote_stuck_report_due(
            started,
            now + DEMOTE_STUCK_DETECTION_WINDOW * 2,
            DEMOTE_STUCK_DETECTION_WINDOW,
            true
        ));
    }
}
