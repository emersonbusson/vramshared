// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/cache_admission.rs
//! Shared cache-authority observations and joined supervisor lifetime.
use super::{attempt_vsock_lease_cancellable, spawn_heartbeat, unix_time_ms};
use ramshared_wsl2d::{
    control_plane::{NegotiatedCadence, SharedHostActivity, VsockControlPlane},
    host_catalog::SharedHostCatalog,
    host_gate::{ControlPlaneAuthority, SealedOrigin, SessionAuthority, SharedAuthority},
    host_storage_state::SharedHostStorage,
    lease_supervisor::{CampaignHandle, Granted, LeaseLog, SessionHostMemory, Supervisor},
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::Duration;

/// SPEC `native-vsock-host-guest-control-plane` ITEM-3 / DT-9 production
/// composition: the shared cache-admission authority the backend consults.
///
/// The already-verified sealed origin is adopted immediately, so origin I/O
/// is available before any lease and survives lease loss (RF-3). One bounded
/// attempt is then made to reach the Windows peer over AF_VSOCK; no peer —
/// no port or key configured, connect failure, or handshake failure — leaves
/// the authority `origin_only`, and the cache gate refuses (DT-9: a host
/// with no vsock peer must not populate the VRAM cache). A leased attempt
/// hands the plane to the heartbeat thread; a failed beat drives the
/// authority to safe mode, which the gate turns into a cache revocation on
/// the next I/O.
pub(super) fn compose_cache_admission(sealed: SealedOrigin) -> ControlPlaneCacheAdmission {
    let attempt_sealed = sealed.clone();
    compose_cache_admission_with_stop(
        sealed,
        move |session, stop| {
            attempt_vsock_lease_cancellable(&attempt_sealed, session, stop).map(
                |(mut plane, cadence)| {
                    plane.set_root_vhd_observer(ramshared_block::vhd_identity::observe_root_vhd);
                    (plane, cadence)
                },
            )
        },
        LEASE_RETRY_INTERVAL,
    )
}

/// The cadence at which the guest re-opens its accept window after a lost lease
/// or handshake failure. Matches the host connector's 2 s retry cadence while
/// keeping `listen_vsock_cancellable` continuously open while awaiting a peer.
const LEASE_RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// The composition, with the lease attempt injected.
///
/// The lease attempt runs **off** the startup path. The host dials on its own
/// cadence and the guest's accept window is bounded, so a blocking attempt
/// here held the transport's readiness past the controller's readiness window
/// and failed an activation that had a peer waiting: the peer was alive and
/// the activation still said "daemon did not start". The origin is adopted
/// before any lease, so origin I/O never depends on this thread, and the
/// cache stays refused until a lease actually arrives.
#[cfg(test)]
pub(super) fn compose_cache_admission_with<F>(
    sealed: SealedOrigin,
    attempt: F,
    retry_interval: Duration,
) -> ControlPlaneCacheAdmission
where
    F: Fn(
            SessionAuthority,
        ) -> Option<(
            VsockControlPlane<Box<dyn ramshared_ipc::vsock::VsockStreamTrait>>,
            NegotiatedCadence,
        )> + Send
        + 'static,
{
    compose_cache_admission_with_stop(
        sealed,
        move |session, _stop| attempt(session),
        retry_interval,
    )
}

pub(super) fn compose_cache_admission_with_stop<F>(
    sealed: SealedOrigin,
    attempt: F,
    retry_interval: Duration,
) -> ControlPlaneCacheAdmission
where
    F: Fn(
            SessionAuthority,
            &AtomicBool,
        ) -> Option<(
            VsockControlPlane<Box<dyn ramshared_ipc::vsock::VsockStreamTrait>>,
            NegotiatedCadence,
        )> + Send
        + 'static,
{
    let shared = SharedAuthority::new();
    let authority = shared.slot();
    let lease_lost = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let lease_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let host_memory = Arc::new(Mutex::new(None));
    let host_catalog = SharedHostCatalog::new();
    let attempt_catalog = host_catalog.clone();
    let host_activity = SharedHostActivity::default();
    let attempt_activity = host_activity.clone();
    let host_storage = SharedHostStorage::new();
    let attempt_storage = host_storage.clone();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let attempt_stop = Arc::clone(&stop);
    // A poisoned lock is treated as "no authority": the admission below maps
    // it to a refusal, so the cache stays off rather than the process dying.
    if let Ok(mut slot) = authority.lock() {
        slot.adopt_verified_origin(sealed.clone());
    }
    // DT-3: one session supervisor owns the attempt → mint → wait → join
    // cycle. The window is re-opened on a cadence before the first lease *and*
    // after any lease loss, until the daemon exits. `begin_session` fences the
    // earlier session so a draining heartbeat/watchdog cannot clobber a newer
    // lease (DT-4).
    let supervisor = Supervisor::new(
        shared,
        Arc::clone(&lease_lost),
        Arc::clone(&lease_seen),
        Arc::clone(&host_memory),
        move |session| {
            attempt(session, &attempt_stop).map(|(mut plane, cadence)| {
                plane.attach_host_catalog(attempt_catalog.clone());
                plane.attach_host_activity(attempt_activity.clone());
                plane.attach_host_storage(attempt_storage.clone());
                Granted {
                    host_memory: plane.host_memory_handle(),
                    session: (plane, cadence),
                }
            })
        },
        |granted, session, lost| {
            let (plane, cadence) = granted.session;
            spawn_heartbeat(plane, session, cadence, lost)
                .map(|campaign| Box::new(campaign) as Box<dyn CampaignHandle>)
        },
        retry_interval,
        LeaseLog::stderr(),
    );
    let lifetime = start_supervisor(stop, move |stop| supervisor.run(stop));
    if lifetime.is_none() {
        lease_lost.store(true, Ordering::Release);
        if let Ok(mut slot) = authority.lock() {
            slot.enter_origin_only();
        }
    }
    ControlPlaneCacheAdmission {
        authority,
        lease_lost,
        lease_seen,
        host_memory,
        host_catalog,
        host_activity,
        host_storage,
        lifetime,
    }
}

/// The cache gate the backend consults (DT-9): the shared authority's own
/// verdict at the current wall clock.
#[derive(Clone)]
pub(super) struct ControlPlaneCacheAdmission {
    pub(super) authority: Arc<Mutex<ControlPlaneAuthority>>,
    pub(super) lease_lost: Arc<std::sync::atomic::AtomicBool>,
    /// True once a lease was minted at least once.
    ///
    /// The distinction matters at the cache's revocation point: a refusal
    /// before the first lease only means the peer has not arrived, and
    /// revoking there moved the cache to a state the product never recovers
    /// from. A refusal after a lease was granted is a revocation.
    pub(super) lease_seen: Arc<std::sync::atomic::AtomicBool>,
    pub(super) host_memory: Arc<Mutex<Option<SessionHostMemory>>>,
    pub(super) host_catalog: SharedHostCatalog,
    pub(super) host_activity: SharedHostActivity,
    pub(super) host_storage: SharedHostStorage,
    pub(super) lifetime: Option<Arc<SupervisorOwner>>,
}

impl ControlPlaneCacheAdmission {
    pub(super) fn lease_lost(&self) -> bool {
        self.lifetime.as_ref().is_some_and(|owner| owner.stopped())
            || self.lease_lost.load(std::sync::atomic::Ordering::Acquire)
    }

    pub(super) fn host_catalog(&self) -> &SharedHostCatalog {
        &self.host_catalog
    }

    /// Give an already-listening `vsock` supervisor a bounded startup window
    /// to deliver the first authenticated host GPU catalog before initial worker launch.
    pub(super) fn wait_for_initial_catalog(&self, timeout: Duration) -> bool {
        if timeout.is_zero() || self.lease_lost() {
            return self.host_catalog.try_has_current_report();
        }
        self.host_catalog.wait_for_current_report(timeout)
    }

    /// Whether the control plane currently allows restarting a dead worker.
    ///
    /// When a live control-plane supervisor thread is active (`lifetime.is_some()`),
    /// recovery requires an unexpired host GPU catalog so startup never burns
    /// respawn attempts before the first `vsock` handshake delivers GPU identity.
    /// Standalone unit fixtures (`lifetime.is_none()`) retain the `!lease_seen`
    /// fallback when `!lease_lost()`. Once a lease has been seen, recovery always
    /// requires a live, unexpired host GPU catalog.
    pub(super) fn allows_worker_recovery(&self) -> bool {
        !self.lease_lost()
            && (self.host_catalog.try_has_current_report()
                || (!self.lease_seen.load(std::sync::atomic::Ordering::Acquire)
                    && self.lifetime.is_none()))
    }

    pub(super) fn host_memory(&self) -> Option<ramshared_ipc::HostMemoryTelemetry> {
        let sample = self.host_memory.try_lock().ok()?.clone()?;
        if self.lease_lost() {
            return None;
        }
        let snapshot = sample.snapshot(&self.authority)?;
        (!self.lease_lost()).then_some(snapshot)
    }

    pub(super) fn host_storage(
        &self,
    ) -> Option<ramshared_ipc::storage_proof::HostStorageTelemetry> {
        self.host_storage.telemetry(self.lease_lost())
    }

    pub(super) fn bounded_storage_free_space(
        &self,
        root: &std::path::Path,
        guest_free: u64,
    ) -> Option<u64> {
        self.host_storage
            .bounded_free_bytes(root, guest_free, self.lease_lost())
    }

    /// The authority's own state, as the operator sees it.
    pub(super) fn control_state(&self) -> &'static str {
        self.authority
            .lock()
            .map(|authority| authority.state().as_str())
            .unwrap_or("poisoned")
    }

    /// The lease deadline the authority holds, when it holds one.
    pub(super) fn lease_deadline_ms(&self) -> Option<u64> {
        self.authority
            .lock()
            .ok()
            .and_then(|authority| authority.lease().map(|lease| lease.deadline_ms))
    }
}

impl ramshared_block::CacheAdmission for ControlPlaneCacheAdmission {
    fn admit_cache(&self) -> Result<(), String> {
        if self.lifetime.as_ref().is_some_and(|owner| owner.stopped()) {
            return Err("control-plane supervisor is stopped".into());
        }
        let now_ms = unix_time_ms().unwrap_or(0);
        self.authority
            .lock()
            .map_err(|_| "control-plane authority is poisoned".to_string())?
            .admit_cache(now_ms)
            .map_err(|error| error.to_string())
    }

    fn cache_revocation_pending(&self) -> bool {
        !self.lease_seen.load(std::sync::atomic::Ordering::Acquire) && !self.lease_lost()
    }
}

pub(super) struct SupervisorOwner {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}
impl SupervisorOwner {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
            || self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }
}
impl Drop for SupervisorOwner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
fn start_supervisor(
    stop: Arc<AtomicBool>,
    run: impl FnOnce(Arc<AtomicBool>) + Send + 'static,
) -> Option<Arc<SupervisorOwner>> {
    let signal = stop.clone();
    match std::thread::Builder::new()
        .name("ramshared-lease-supervisor".into())
        .spawn(move || run(signal))
    {
        Ok(handle) => Some(Arc::new(SupervisorOwner {
            stop,
            handle: Some(handle),
        })),
        Err(error) => {
            eprintln!("[ramsharedd] control-plane supervisor unavailable: {error}");
            None
        }
    }
}

#[cfg(test)]
mod tests;
