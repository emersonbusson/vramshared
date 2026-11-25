// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/heartbeat_campaign.rs
//! Actual heartbeat/watchdog thread custody with cooperative cancellation.
use ramshared_ipc::vsock::VsockStreamTrait;
use ramshared_wsl2d::{
    control_plane::{HeartbeatLoop, HeartbeatLoopStop, NegotiatedCadence, VsockControlPlane},
    host_catalog::SharedHostCatalog,
    host_gate::SessionAuthority,
    lease_supervisor::CampaignHandle,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub(super) struct HeartbeatCampaign {
    stop: Arc<AtomicBool>,
    watchdog: Option<JoinHandle<()>>,
    beat: Option<JoinHandle<()>>,
}
impl HeartbeatCampaign {
    fn new(stop: Arc<AtomicBool>) -> Self {
        Self {
            stop,
            watchdog: None,
            beat: None,
        }
    }
    fn wait(&mut self, budget: Duration) -> bool {
        let started = Instant::now();
        loop {
            for handle in [&mut self.watchdog, &mut self.beat] {
                if handle.as_ref().is_some_and(JoinHandle::is_finished)
                    && let Some(handle) = handle.take()
                {
                    let _ = handle.join();
                }
            }
            if self.watchdog.is_none() && self.beat.is_none() {
                return true;
            }
            if started.elapsed() >= budget {
                return false;
            }
            std::thread::sleep(
                Duration::from_millis(10).min(budget.saturating_sub(started.elapsed())),
            );
        }
    }
}
impl CampaignHandle for HeartbeatCampaign {
    fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
    fn join_within(&mut self, deadline: Duration) -> bool {
        self.wait(deadline)
    }
}
impl Drop for HeartbeatCampaign {
    fn drop(&mut self) {
        self.request_stop();
        while !self.wait(Duration::from_millis(100)) {}
    }
}
fn sleep_until_cancelled(interval: Duration, stop: &AtomicBool) {
    let deadline = Instant::now().checked_add(interval);
    while !stop.load(Ordering::Acquire) {
        let Some(remaining) = deadline.map(|d| d.saturating_duration_since(Instant::now())) else {
            return;
        };
        if remaining.is_zero() {
            return;
        }
        std::thread::sleep(remaining.min(Duration::from_millis(10)));
    }
}

/// Any child exit, including unwinding, revokes writers and cancels its peer.
struct ThreadExit {
    session: SessionAuthority,
    lost: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    host_catalog: Option<SharedHostCatalog>,
}
impl Drop for ThreadExit {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.session.retire(|slot| {
            slot.on_lease_expired();
            self.lost.store(true, Ordering::Release);
            if let Some(catalog) = &self.host_catalog {
                catalog.revoke();
            }
        });
    }
}
type ThreadTask = Box<dyn FnOnce() + Send + 'static>;

fn run_watchdog(
    exit: ThreadExit,
    liveness: Arc<std::sync::atomic::AtomicU64>,
    lease_window_ms: u64,
) {
    while !exit.stop.load(Ordering::Acquire) {
        sleep_until_cancelled(Duration::from_millis(500), &exit.stop);
        if exit.stop.load(Ordering::Acquire) {
            return;
        }
        let Some(now) = super::unix_time_ms() else {
            return;
        };
        let last = liveness.load(Ordering::Relaxed);
        if last != 0 && now.saturating_sub(last) > lease_window_ms.saturating_add(1_000) {
            return;
        }
    }
}

pub(super) fn spawn_heartbeat(
    plane: VsockControlPlane<Box<dyn VsockStreamTrait>>,
    session: SessionAuthority,
    cadence: NegotiatedCadence,
    lease_lost: Arc<AtomicBool>,
) -> Result<HeartbeatCampaign, String> {
    spawn_heartbeat_with(plane, session, cadence, lease_lost, |name, run| {
        std::thread::Builder::new().name(name.into()).spawn(run)
    })
}

/// Maximum heartbeat sleep when a host GPU catalog is attached.
///
/// The authenticated host GPU catalog expires strictly inside
/// `GPU_BUDGET_MAX_AGE_MS` (5 000 ms) minus probe age and transit RTT, while
/// Windows refreshes the native GPU probe every ~1 000 ms. Capping the guest
/// beat sleep to 500 ms guarantees multiple renewal windows before expiry even
/// when the peer negotiates a 5 s lease heartbeat or the guest experiences CPU
/// scheduling jitter under heavy swap pressure.
const HOST_CATALOG_HEARTBEAT_CAP_MS: u64 = ramshared_vram::GPU_BUDGET_MAX_AGE_MS / 10;

pub(super) fn effective_heartbeat_interval_ms(
    cadence: NegotiatedCadence,
    has_host_catalog: bool,
) -> u64 {
    let negotiated_ms = cadence.heartbeat_secs.saturating_mul(1000);
    if negotiated_ms == 0 || !has_host_catalog {
        return negotiated_ms;
    }
    negotiated_ms.min(HOST_CATALOG_HEARTBEAT_CAP_MS)
}

fn spawn_heartbeat_with(
    mut plane: VsockControlPlane<Box<dyn VsockStreamTrait>>,
    session: SessionAuthority,
    cadence: NegotiatedCadence,
    lease_lost: Arc<AtomicBool>,
    mut spawn: impl FnMut(&str, ThreadTask) -> std::io::Result<JoinHandle<()>>,
) -> Result<HeartbeatCampaign, String> {
    let host_catalog = plane.shared_host_catalog().cloned();
    let interval_ms = effective_heartbeat_interval_ms(cadence, host_catalog.is_some());
    let lease_window_ms = cadence.lease_timeout_secs.saturating_mul(1000);
    let beats = HeartbeatLoop::new(interval_ms, 3).map_err(|e| e.to_string())?;
    let stop = Arc::new(AtomicBool::new(false));
    let mut campaign = HeartbeatCampaign::new(stop.clone());
    let liveness = plane.liveness_handle();
    let watchdog_exit = ThreadExit {
        session: session.clone(),
        lost: lease_lost.clone(),
        stop: stop.clone(),
        host_catalog: host_catalog.clone(),
    };
    campaign.watchdog = Some(
        spawn(
            "ramshared-lease-watchdog",
            Box::new(move || run_watchdog(watchdog_exit, liveness, lease_window_ms)),
        )
        .map_err(|e| e.to_string())?,
    );
    let beat_stop = stop.clone();
    let beat_exit = ThreadExit {
        session: session.clone(),
        lost: lease_lost.clone(),
        stop: stop.clone(),
        host_catalog: host_catalog.clone(),
    };
    campaign.beat = Some(
        spawn(
            "ramshared-lease-beat",
            Box::new(move || {
                let _exit = beat_exit;
                let result = beats.run_until(
                    &mut plane,
                    u32::MAX,
                    || super::unix_time_ms().unwrap_or(0),
                    |ms| sleep_until_cancelled(Duration::from_millis(ms), &beat_stop),
                    || beat_stop.load(Ordering::Acquire),
                );
                if matches!(result, HeartbeatLoopStop::Failure { .. }) {
                    session.publish(plane.authority());
                    if let Some(catalog) = &host_catalog {
                        session.mutate(|_| catalog.revoke());
                    }
                    session.set_lease_lost_if_current(&lease_lost);
                }
            }),
        )
        .map_err(|e| e.to_string())?,
    );
    Ok(campaign)
}

#[cfg(test)]
mod tests;
