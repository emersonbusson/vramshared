// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/control_plane/heartbeat_loop.rs
//! Heartbeat failure/tick policy with cancellation before further effects.
use super::{ControlPlane, ControlPlaneError};

/// Bounded lease-maintenance campaign.
///
/// Never spins forever: it stops after `ticks` beats or after
/// `max_consecutive_failures` consecutive failures (invariant check— the safe
/// default is to stop and leave the authority fail-closed, not to hammer a
/// dead peer). Time and sleep are injected so the loop is deterministic under
/// test and can be driven from a real clock in production.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeartbeatLoop {
    pub interval_ms: u64,
    pub max_consecutive_failures: u32,
}

/// Why a [`HeartbeatLoop`] campaign stopped.
#[derive(Debug, PartialEq, Eq)]
pub enum HeartbeatLoopStop {
    /// Cancellation observed before another heartbeat or idle wait.
    Cancelled { ticks: u32 },
    /// The requested number of beats was attempted and every one succeeded.
    TicksExhausted { ticks: u32 },
    /// The consecutive-failure budget ran out. The plane is already fail-closed.
    Failure {
        ticks: u32,
        consecutive_failures: u32,
        error: ControlPlaneError,
    },
}

impl HeartbeatLoop {
    pub fn new(interval_ms: u64, max_consecutive_failures: u32) -> Result<Self, ControlPlaneError> {
        if interval_ms == 0 {
            return Err(ControlPlaneError::Protocol(
                "heartbeat interval must be non-zero".into(),
            ));
        }
        if max_consecutive_failures == 0 {
            return Err(ControlPlaneError::Protocol(
                "heartbeat failure budget must be non-zero".into(),
            ));
        }
        Ok(Self {
            interval_ms,
            max_consecutive_failures,
        })
    }

    /// Run at most `ticks` beats. `now_ms` supplies the observation time for
    /// each beat; `sleep_ms` receives the configured interval between beats
    /// (not after the last one).
    pub fn run(
        &self,
        plane: &mut dyn ControlPlane,
        ticks: u32,
        now_ms: impl FnMut() -> u64,
        sleep_ms: impl FnMut(u64),
    ) -> HeartbeatLoopStop {
        self.run_until(plane, ticks, now_ms, sleep_ms, || false)
    }

    pub fn run_until(
        &self,
        plane: &mut dyn ControlPlane,
        ticks: u32,
        mut now_ms: impl FnMut() -> u64,
        mut sleep_ms: impl FnMut(u64),
        mut cancelled: impl FnMut() -> bool,
    ) -> HeartbeatLoopStop {
        let mut consecutive_failures: u32 = 0;
        for tick in 0..ticks {
            if cancelled() {
                return HeartbeatLoopStop::Cancelled { ticks: tick };
            }
            match plane.heartbeat(now_ms()) {
                Ok(()) => consecutive_failures = 0,
                Err(error) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    if consecutive_failures >= self.max_consecutive_failures {
                        return HeartbeatLoopStop::Failure {
                            ticks: tick + 1,
                            consecutive_failures,
                            error,
                        };
                    }
                }
            }
            if cancelled() {
                return HeartbeatLoopStop::Cancelled { ticks: tick + 1 };
            }
            if tick + 1 < ticks {
                sleep_ms(self.interval_ms);
            }
        }
        HeartbeatLoopStop::TicksExhausted { ticks }
    }
}

#[cfg(test)]
mod tests;
