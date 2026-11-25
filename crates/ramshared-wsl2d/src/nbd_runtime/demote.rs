// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/nbd_runtime/demote.rs
//! NBD serve-loop demote, recovery activation, and shutdown coordination state.

use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use ramshared_wsl2d::LiveCount;
use ramshared_wsl2d::autotier::{RecoveryTracker, backend_release_allowed};
use ramshared_wsl2d::gpu_budget::SERVE_IDLE_TICK;
use ramshared_wsl2d::swap::{DemoteRearm, SwapoffOutcome, demote_stuck_report_due};

use crate::shutdown::ShutdownPermit;

use super::residency::{demote_spawn_authorized, demote_status};
use super::starter::NbdRuntimeStarter;

pub(crate) const RECOVERY_ACTIVATION_OBSERVATION_DEADLINE: Duration = Duration::from_secs(30);
pub(crate) const RECOVERY_ACTIVATION_POLL_TICK: Duration = Duration::from_millis(100);
const GPU_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);

/// Terminal observation for a recovery `swapon` child. `Pending` intentionally
/// remains non-terminal after the observation deadline: the child may still be
/// reading the NBD export, so freeing the backend would recreate the deadlock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RecoveryActivationPoll {
    Idle,
    Pending,
    Succeeded,
    Failed,
}

/// Owns at most one recovery activation child receiver. A failed healthy epoch
/// remains parked until an unhealthy or nonempty observation begins a new
/// epoch, preventing retry storms against the same NBD export.
#[derive(Default)]
pub(crate) struct RecoveryActivation {
    result_rx: Option<Receiver<bool>>,
    failed_epoch: bool,
    shutdown_requested: bool,
    started_at: Option<Instant>,
    deadline_reported: bool,
}

impl RecoveryActivation {
    pub(crate) fn start(&mut self, result_rx: Receiver<bool>) -> Result<(), &'static str> {
        if self.result_rx.is_some() {
            return Err("recovery activation is already pending");
        }
        if self.failed_epoch {
            return Err("recovery activation is parked for this healthy epoch");
        }
        self.result_rx = Some(result_rx);
        self.started_at = Some(Instant::now());
        self.deadline_reported = false;
        Ok(())
    }

    pub(crate) fn poll(&mut self) -> RecoveryActivationPoll {
        let Some(rx) = self.result_rx.take() else {
            return RecoveryActivationPoll::Idle;
        };
        match rx.try_recv() {
            Ok(true) => {
                self.started_at = None;
                self.deadline_reported = false;
                self.failed_epoch = false;
                RecoveryActivationPoll::Succeeded
            }
            Ok(false) | Err(TryRecvError::Disconnected) => {
                self.started_at = None;
                self.deadline_reported = false;
                self.failed_epoch = true;
                RecoveryActivationPoll::Failed
            }
            Err(TryRecvError::Empty) => {
                self.result_rx = Some(rx);
                RecoveryActivationPoll::Pending
            }
        }
    }

    pub(crate) fn launch_allowed(&mut self, healthy: bool, tier_empty: bool) -> bool {
        if !healthy || !tier_empty {
            self.failed_epoch = false;
            return false;
        }
        self.result_rx.is_none() && !self.failed_epoch
    }

    pub(crate) fn is_pending(&self) -> bool {
        self.result_rx.is_some()
    }

    pub(crate) fn mark_dispatch_failure(&mut self) {
        self.failed_epoch = true;
        self.started_at = None;
        self.deadline_reported = false;
    }

    pub(crate) fn request_shutdown(&mut self) {
        self.shutdown_requested = true;
    }

    pub(crate) fn backend_release_allowed(&self) -> bool {
        !self.shutdown_requested || self.result_rx.is_none()
    }

    pub(crate) fn take_observation_deadline_exceeded(&mut self) -> bool {
        let overdue = self.result_rx.is_some()
            && self.started_at.is_some_and(|started| {
                started.elapsed() >= RECOVERY_ACTIVATION_OBSERVATION_DEADLINE
            });
        if overdue && !self.deadline_reported {
            self.deadline_reported = true;
            return true;
        }
        false
    }
}

/// Shared state for NBD `swapoff` draining, recovery activation, and shutdown custody.
pub(super) struct NbdServeState {
    pub(super) demoted: bool,
    pub(super) demote_rx: Option<Receiver<SwapoffOutcome>>,
    pub(super) demote_rearm: DemoteRearm,
    pub(super) demote_started_at: Option<Instant>,
    pub(super) demote_stuck_reported: bool,
    pub(super) swapoff_attempted: bool,
    pub(super) swapoff_confirmed: bool,
    pub(super) recovery: RecoveryTracker,
    pub(super) recovery_activation: RecoveryActivation,
    pub(super) shutdown_requested: bool,
    pub(super) next_shutdown_attempt: Instant,
    pub(super) live: LiveCount,
    pub(super) last_gpu_heartbeat: Instant,
    pub(super) demotes_total: u64,
    pub(super) last_demote_reason: Option<String>,
}

impl NbdServeState {
    pub(super) fn new(starter: &mut dyn NbdRuntimeStarter) -> Self {
        starter.publish_demote(&demote_status(0, &None, false, None));
        Self {
            demoted: false,
            demote_rx: None,
            demote_rearm: DemoteRearm::new(),
            demote_started_at: None,
            demote_stuck_reported: false,
            swapoff_attempted: false,
            swapoff_confirmed: false,
            recovery: RecoveryTracker::new(3),
            recovery_activation: RecoveryActivation::default(),
            shutdown_requested: false,
            next_shutdown_attempt: Instant::now(),
            live: LiveCount::new(),
            last_gpu_heartbeat: Instant::now(),
            demotes_total: 0,
            last_demote_reason: None,
        }
    }

    pub(super) fn recv_tick(&self) -> Duration {
        if self.recovery_activation.is_pending() {
            RECOVERY_ACTIVATION_POLL_TICK
        } else {
            SERVE_IDLE_TICK
        }
    }

    pub(super) fn request_shutdown(&mut self) {
        self.shutdown_requested = true;
        self.recovery_activation.request_shutdown();
    }

    pub(super) fn demote_spawn_authorized(&self) -> bool {
        demote_spawn_authorized(&self.demote_rearm, &self.demote_rx, self.demoted)
    }

    pub(super) fn spawn_swapoff(
        &mut self,
        reason: &str,
        nbd_dev: &str,
        starter: &mut dyn NbdRuntimeStarter,
    ) {
        self.spawn_swapoff_with_mode(reason, nbd_dev, starter, false);
    }

    fn spawn_swapoff_with_mode(
        &mut self,
        reason: &str,
        nbd_dev: &str,
        starter: &mut dyn NbdRuntimeStarter,
        peer_closed: bool,
    ) {
        self.last_demote_reason = Some(reason.into());
        self.demote_rx = Some(if peer_closed {
            starter.spawn_peer_closed_swapoff(nbd_dev)
        } else {
            starter.spawn_swapoff(nbd_dev)
        });
        self.swapoff_attempted = true;
        self.demote_started_at = Some(Instant::now());
        self.demote_stuck_reported = false;
        self.publish_demote(true, None, starter);
    }

    fn publish_demote(
        &self,
        in_progress: bool,
        pending_age_secs: Option<u64>,
        starter: &mut dyn NbdRuntimeStarter,
    ) {
        starter.publish_demote(&demote_status(
            self.demotes_total,
            &self.last_demote_reason,
            in_progress,
            pending_age_secs,
        ));
    }

    pub(super) fn poll_recovery_activation(
        &mut self,
        nbd_dev: &str,
        starter: &mut dyn NbdRuntimeStarter,
    ) {
        match self.recovery_activation.poll() {
            RecoveryActivationPoll::Succeeded => {
                self.demoted = false;
                self.swapoff_attempted = false;
                self.swapoff_confirmed = false;
                self.recovery.reset();
                eprintln!("[ramsharedd] RECOVERING -> available: swapon {nbd_dev} prio=100");
                if self.shutdown_requested {
                    // A shutdown drain is one-shot, not the canary retry loop:
                    // only a deterministic `Unavailable` suppresses it. A pending
                    // transient backoff must not drop the last chance to empty
                    // the tier before exit.
                    if self.demote_rearm.is_parked() {
                        eprintln!(
                            "[ramsharedd] DEMOTE: swapoff path parked; cannot drain {nbd_dev} on shutdown"
                        );
                    } else {
                        self.spawn_swapoff("RecoveryShutdown", nbd_dev, starter);
                    }
                }
            }
            RecoveryActivationPoll::Failed => {
                self.recovery.reset();
                eprintln!("[ramsharedd] RECOVERING: swapon {nbd_dev} failed; parked");
            }
            RecoveryActivationPoll::Idle | RecoveryActivationPoll::Pending => {}
        }
    }

    pub(super) fn poll_demote_rx(
        &mut self,
        nbd_dev: &str,
        starter: &mut dyn NbdRuntimeStarter,
        after_message: bool,
    ) {
        let Some(rx) = self.demote_rx.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(outcome) => {
                if after_message {
                    self.recovery.reset();
                }
                if matches!(outcome, SwapoffOutcome::Drained) {
                    self.demoted = true;
                    self.swapoff_confirmed = true;
                    self.demotes_total = self.demotes_total.saturating_add(1);
                    self.publish_demote(false, None, starter);
                    let label = if after_message {
                        "parked"
                    } else {
                        "canary disarmed"
                    };
                    eprintln!("[ramsharedd] DEMOTE: swapoff {nbd_dev} OK ({label})");
                } else {
                    self.publish_demote(false, None, starter);
                    let suffix = if after_message {
                        ""
                    } else {
                        "; next attempt delayed"
                    };
                    eprintln!(
                        "[ramsharedd] DEMOTE: swapoff {nbd_dev} did not drain ({outcome:?}){suffix}"
                    );
                }
                self.demote_started_at = None;
                self.demote_stuck_reported = false;
                self.demote_rearm.record(outcome, Instant::now());
            }
            Err(TryRecvError::Empty) => {
                // SPEC §9.2: a drain that outlives the detection window is reported
                // stuck — publish the pending DEMOTE and its age without aborting
                // the D-state child or disconnecting the device.
                if after_message
                    && demote_stuck_report_due(
                        self.demote_started_at,
                        Instant::now(),
                        starter.demote_stuck_window(),
                        self.demote_stuck_reported,
                    )
                {
                    let age_secs = self
                        .demote_started_at
                        .map(|started| started.elapsed().as_secs())
                        .unwrap_or(0);
                    eprintln!(
                        "[ramsharedd] DEMOTE: swapoff {nbd_dev} did not finish in {age_secs}s \
                         ({:?}); drain still in progress (SPEC §9.2: detection, not abort)",
                        self.last_demote_reason
                    );
                    self.publish_demote(true, Some(age_secs), starter);
                    self.demote_stuck_reported = true;
                }
                self.demote_rx = Some(rx);
            }
            Err(TryRecvError::Disconnected) => {
                if after_message {
                    self.recovery.reset();
                }
                self.demote_started_at = None;
                self.demote_stuck_reported = false;
                self.demote_rearm
                    .record(SwapoffOutcome::Rejected { exit_code: None }, Instant::now());
                self.publish_demote(false, None, starter);
                let suffix = if after_message {
                    ""
                } else {
                    "; next attempt delayed"
                };
                eprintln!("[ramsharedd] DEMOTE: swapoff worker disconnected{suffix}");
            }
        }
    }

    pub(super) fn maybe_claim_shutdown(
        &mut self,
        nbd_dev: &str,
        starter: &mut dyn NbdRuntimeStarter,
    ) -> Option<ShutdownPermit> {
        if self
            .recovery_activation
            .take_observation_deadline_exceeded()
        {
            eprintln!(
                "[ramsharedd] RECOVERING: swapon {nbd_dev} observation exceeded {}s; \
                 keeping NBD backend alive",
                RECOVERY_ACTIVATION_OBSERVATION_DEADLINE.as_secs()
            );
        }
        if !self.shutdown_requested
            || !self.recovery_activation.backend_release_allowed()
            || self.demote_rx.is_some()
        {
            return None;
        }
        let used_kb = starter.nbd_used_kb(nbd_dev);
        let absent = starter.nbd_swap_is_explicitly_absent(nbd_dev);
        if backend_release_allowed(
            self.swapoff_attempted,
            self.swapoff_confirmed,
            used_kb,
            absent,
        ) {
            return starter.try_claim_shutdown(nbd_dev);
        }
        if !self.swapoff_confirmed
            && Instant::now() >= self.next_shutdown_attempt
            && self.demote_rearm.may_attempt_at(Instant::now())
        {
            self.spawn_swapoff("Shutdown", nbd_dev, starter);
            self.next_shutdown_attempt = Instant::now() + starter.teardown_retry_delay();
        }
        None
    }

    pub(super) fn handle_closed(
        &mut self,
        nbd_dev: &str,
        starter: &mut dyn NbdRuntimeStarter,
    ) -> bool {
        if !self.live.on_close() {
            return false;
        }
        // The tier's peer connection is gone at this point. A tier that stays
        // armed without a peer turns every page fault on it into a SIGBUS, so
        // the peer releases it before it lingers.
        if !self.swapoff_confirmed
            && self.demote_rx.is_none()
            && !starter.nbd_swap_is_explicitly_absent(nbd_dev)
            && self.demote_rearm.may_attempt_at(Instant::now())
        {
            eprintln!(
                "[ramsharedd] tier connection closed; release requested, outcome unconfirmed"
            );
            self.spawn_swapoff_with_mode("PeerClosed", nbd_dev, starter, true);
        }
        true
    }

    pub(super) fn heartbeat_due(&mut self) -> bool {
        if !self.demoted
            && self.demote_rx.is_none()
            && self.last_gpu_heartbeat.elapsed() >= GPU_HEARTBEAT_INTERVAL
        {
            self.last_gpu_heartbeat = Instant::now();
            true
        } else {
            false
        }
    }
}
