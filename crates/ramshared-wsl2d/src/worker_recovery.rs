// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/worker_recovery.rs
//! Recovery decisions and nonblocking handoff for the isolated cache worker.
use super::cache_control::RecoveryCache;
use super::{
    IsolatedWorkerSupervisor, NbdRuntimeStarter, OriginCache, OriginCacheState, WorkerChildHandle,
    WorkerProcessControl, WorkerSpawnParams, worker_startup,
};
use ramshared_block::{AuthoritativeOriginBackend, DisabledCache, FileOrigin, IpcCacheClient};
use std::process::ExitStatus;
use std::time::{Duration, Instant};

#[cfg(test)]
pub(super) use super::worker_retry::{WORKER_RESPAWN_COOLDOWN, WORKER_RESPAWN_HEALTHY_AFTER};
pub(super) use super::worker_retry::{WORKER_RESPAWN_MAX_CONSECUTIVE, WorkerRespawnBudget};

/// The live isolated GPU worker and everything needed to rebuild it.
pub(super) struct IsolatedWorkerRuntime {
    pub(super) cache_member: usize,
    pub(super) startup: Option<worker_startup::Startup>,
    pub(super) supervisor: Option<IsolatedWorkerSupervisor>,
    pub(super) params: WorkerSpawnParams,
    pub(super) budget: WorkerRespawnBudget,
    /// Why the GPU cache stopped serving, when it did (SPEC DT-3).
    ///
    /// A worker that exits leaves the cache unable to serve, and the operator
    /// needs the **cause** beside the state: a bare "the cache is off" says
    /// nothing about whether the product is in trouble or the platform simply
    /// cannot host the cache. `None` means the cache is serving or no cause
    /// has been observed yet.
    pub(super) unavailable_reason: Option<String>,
    /// Whether the cache was released because the lease was lost. The next
    /// live lease rebuilds the worker; a deliberate release (pressure or an
    /// operator off) does not set this.
    pub(super) lease_lost_release: bool,
    /// Whether this cooldown's fallback transition has been announced (SPEC
    /// swap-drain-safety DT-7). The serve loop revisits the recovery path on
    /// every job; the announcement is a **transition**, so it fires once per
    /// dead-worker episode and never becomes per-job log traffic. Reset by a
    /// successful respawn.
    pub(super) permanent_fallback_logged: bool,
}

/// Announce the retry-cooldown transition at most once per episode
/// (SPEC swap-drain-safety RF-8). Returns `true` exactly when the caller
/// should log and disable the cache.
pub(super) fn note_permanent_fallback(runtime: &mut IsolatedWorkerRuntime) -> bool {
    if runtime.permanent_fallback_logged {
        return false;
    }
    runtime.permanent_fallback_logged = true;
    true
}

/// Whether the isolated GPU worker handle is gone or its process has exited.
///
/// `try_wait` both observes and reaps an exited child, so a `true` here means
/// the process is no longer a zombie. A missing handle counts as exited: the
/// cache cannot be serving without one.
pub(super) fn gpu_worker_has_exited(runtime: &mut IsolatedWorkerRuntime) -> bool {
    match runtime.supervisor.as_mut().and_then(|s| s.child.as_mut()) {
        Some(WorkerChildHandle::Process(process)) => {
            process.stop_is_quiescent() && matches!(process.try_wait(), Ok(true))
        }
        #[cfg(test)]
        Some(WorkerChildHandle::Thread { handle, .. }) => handle
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished),
        None => true,
    }
}

/// Release the supervisor handle for a worker that is no longer running.
///
/// A deliberate `release_cache` and a crash both leave the child exited.
/// Until the handle is dropped its `Drop`/`shutdown` wait has not run, and the
/// process stays a zombie for the daemon's lifetime. This never spawns a
/// replacement and never touches the cache mirror.
pub(super) fn reap_exited_gpu_worker(runtime: &mut IsolatedWorkerRuntime) {
    if !gpu_worker_has_exited(runtime) {
        return;
    }
    if let Some(previous) = runtime.supervisor.as_mut()
        && !previous.shutdown()
    {
        return;
    }
    let _ = runtime.supervisor.take();
}

/// The reason the daemon publishes when the GPU cache is not serving.
///
/// The cause is a **fact** about the cache's ability to serve, and it is the
/// difference between "the cache is off" (which says nothing) and "the cache
/// is off because the worker exited" (which the operator can act on). A cache
/// that is still serving has no cause to publish, and a cache that is off
/// while its worker is still alive was released deliberately — the release
/// path publishes its own reason.
pub(super) fn gpu_cache_unavailable_reason(
    state: OriginCacheState,
    worker_exited: bool,
    exit: Option<(u32, ExitStatus)>,
) -> Option<String> {
    if !worker_exited
        || matches!(
            state,
            OriginCacheState::Active | OriginCacheState::Restricted
        )
    {
        return None;
    }
    Some(match exit {
        Some((pid, status)) => format!(
            "the isolated GPU cache worker exited while the cache was {} (pid {pid}, {status})",
            state.as_str()
        ),
        None => format!(
            "the isolated GPU cache worker exited while the cache was {}",
            state.as_str()
        ),
    })
}

/// Rebuild a dead GPU worker, or retain origin fallback during a retry cooldown.
///
/// The origin is always authoritative, so a dead worker costs cache hits and
/// never correctness. What this must never do is leave a zombie child or run
/// two workers at once: the previous supervisor is always taken and shut down
/// before any respawn attempt (#17: a replayed recovery yields one live
/// worker, not two).
///
/// A deliberate Off remains released. Off caused by lease loss may be rebuilt
/// only after a live lease returns. Unavailable/Stuck and an exited worker
/// under a still-serving mirror are the explicit death signatures.
#[cfg(test)]
pub(super) fn recover_isolated_gpu_worker_if_failed<S: NbdRuntimeStarter + ?Sized>(
    starter: &mut S,
    cache: &AuthoritativeOriginBackend<FileOrigin, OriginCache>,
    runtime: &mut IsolatedWorkerRuntime,
    lease_live: bool,
) {
    recover_isolated_gpu_worker_guarded(
        starter,
        cache,
        runtime,
        lease_live,
        std::sync::Arc::new(|| true),
    );
}

pub(super) fn recover_isolated_gpu_worker_guarded<S: NbdRuntimeStarter + ?Sized>(
    starter: &mut S,
    cache: &AuthoritativeOriginBackend<FileOrigin, OriginCache>,
    runtime: &mut IsolatedWorkerRuntime,
    lease_live: bool,
    authority: worker_startup::StartupAuthority,
) {
    let index = runtime.cache_member;
    let _ = cache.try_control_cache(|cache| {
        if let Some(mut selected) = RecoveryCache::select(cache, index) {
            recover_selected_worker(starter, &mut selected, runtime, lease_live, authority);
        }
    });
}

fn recover_selected_worker<S: NbdRuntimeStarter + ?Sized>(
    starter: &mut S,
    cache: &mut RecoveryCache<'_>,
    runtime: &mut IsolatedWorkerRuntime,
    lease_live: bool,
    authority: worker_startup::StartupAuthority,
) {
    if runtime.params.expected_adapter.is_none() {
        runtime.params.expected_adapter = cache.bound_adapter_identity();
    }
    let worker_exited = gpu_worker_has_exited(runtime);
    let state = cache.cache_state();
    if !worker_exited && runtime.startup.is_none() {
        runtime.budget.mark_healthy_if_settled(state);
    }
    if poll_startup(cache, runtime, lease_live, state) {
        return;
    }

    let live_generation = runtime
        .params
        .host_catalog
        .as_ref()
        .map(|catalog| catalog.live_generation());
    if runtime
        .budget
        .note_lease_state(lease_live, !matches!(live_generation, Some(None)))
    {
        runtime.permanent_fallback_logged = false;
    }

    let native_driver = if runtime.params.host_catalog.is_none()
        && (worker_exited || runtime.supervisor.is_none() || state != OriginCacheState::Active)
    {
        starter.native_gpu_driver_observation()
    } else {
        None
    };
    if let Some(obs) = native_driver.as_ref()
        && runtime
            .budget
            .note_native_driver_state(obs.ready, &obs.epoch)
    {
        runtime.permanent_fallback_logged = false;
    }

    let recover_worker =
        worker_recovery_requested(state, worker_exited, lease_live, runtime.lease_lost_release);
    if runtime.unavailable_reason.is_none() && worker_exited {
        let exit = runtime
            .supervisor
            .as_ref()
            .and_then(|supervisor| supervisor.child.as_ref())
            .and_then(|child| match child {
                WorkerChildHandle::Process(process) => {
                    process.exit_status().map(|status| (process.id(), status))
                }
                #[cfg(test)]
                WorkerChildHandle::Thread { .. } => None,
            });
        if (!recover_worker || exit.is_some())
            && let Some(reason) = gpu_cache_unavailable_reason(state, worker_exited, exit)
        {
            eprintln!("[ramsharedd] {reason}; the origin remains authoritative");
            runtime.unavailable_reason = Some(reason);
        }
    }
    if !recover_worker {
        reap_exited_gpu_worker(runtime);
        return;
    }

    if let Some(previous) = runtime.supervisor.as_mut()
        && !previous.shutdown()
    {
        return;
    }
    let _ = runtime.supervisor.take();

    if let Some(Some(generation)) = live_generation
        && generation > runtime.budget.last_catalog_generation
    {
        runtime.budget.note_catalog_generation(generation);
        runtime.permanent_fallback_logged = false;
    }

    if matches!(live_generation, Some(None)) {
        if runtime.unavailable_reason.is_none() {
            runtime.unavailable_reason =
                Some("awaiting fresh authenticated host GPU catalog".into());
        }
        return;
    }

    if let Some(obs) = native_driver
        && !obs.ready
    {
        if runtime.unavailable_reason.is_none() {
            runtime.unavailable_reason = Some(
                obs.detail
                    .unwrap_or_else(|| "awaiting Linux GPU driver readiness".into()),
            );
        }
        return;
    }

    if runtime.budget.refresh_cooldown_at(Instant::now()) {
        runtime.permanent_fallback_logged = false;
    }
    if !runtime.budget.allows_respawn() {
        // RF-8: a transition message, not per-job traffic. The loop calls
        // this on every serve job; unguarded, one announcement became
        // 1,043,543 lines of log written from the loop that serves swap.
        if note_permanent_fallback(runtime) {
            let attempts = runtime.budget.respawns_since_healthy;
            runtime.unavailable_reason.get_or_insert_with(|| {
                format!("GPU cache is in retry cooldown after {attempts} worker respawns")
            });
            eprintln!(
                "[ramsharedd] isolated GPU cache worker left {} after {} respawns; \
                     origin fallback continues during a bounded retry cooldown",
                state.as_str(),
                runtime.budget.respawns_since_healthy
            );
            cache.replace_cache(OriginCache::Disabled(DisabledCache));
        }
        return;
    }

    match starter.begin_isolated_worker_start_guarded(&runtime.params, authority) {
        Ok(startup) => {
            runtime.startup = Some(startup);
            runtime.unavailable_reason = Some("isolated GPU cache worker startup pending".into());
            cache.replace_cache(OriginCache::Starting { released: false });
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
        Err(error) => startup_failed(cache, runtime, error.to_string()),
    }
}

fn worker_recovery_requested(
    state: OriginCacheState,
    worker_exited: bool,
    lease_live: bool,
    lease_lost_release: bool,
) -> bool {
    let death = matches!(
        state,
        OriginCacheState::Unavailable | OriginCacheState::Stuck
    ) || (worker_exited
        && matches!(
            state,
            OriginCacheState::Active | OriginCacheState::Restricted
        ));
    lease_live && (death || (lease_lost_release && worker_exited && state == OriginCacheState::Off))
}

fn poll_startup(
    cache: &mut RecoveryCache<'_>,
    runtime: &mut IsolatedWorkerRuntime,
    lease_live: bool,
    state: OriginCacheState,
) -> bool {
    if runtime.startup.is_none() {
        return false;
    }
    if !lease_live || state == OriginCacheState::Off {
        cancel_pending_startup(
            runtime,
            "worker startup cancelled after cache authority changed",
        );
    }
    let Some(startup) = runtime.startup.as_mut() else {
        return false;
    };
    if startup.is_cancelled() {
        if startup.is_quiescent() {
            drop(runtime.startup.take());
        }
        return true;
    }
    match startup.poll() {
        Ok(None) => {}
        Ok(Some((client, supervisor))) => {
            drop(runtime.startup.take());
            install_started_worker(cache, runtime, client, supervisor);
        }
        Err(error) => {
            drop(runtime.startup.take());
            startup_failed(cache, runtime, error);
        }
    }
    true
}

fn install_started_worker(
    cache: &mut RecoveryCache<'_>,
    runtime: &mut IsolatedWorkerRuntime,
    client: IpcCacheClient,
    supervisor: IsolatedWorkerSupervisor,
) {
    if let Err(error) = super::gpu_worker_request::validate_startup_binding(&client) {
        drop(client);
        drop(supervisor);
        startup_failed(cache, runtime, error);
        return;
    }
    let pid = supervisor.pid();
    runtime.budget.note_spawned();
    runtime.permanent_fallback_logged = false;
    runtime.lease_lost_release = false;
    runtime.unavailable_reason = None;
    runtime.supervisor = Some(supervisor);
    if runtime.params.expected_adapter.is_none() {
        runtime.params.expected_adapter = client.bound_adapter_identity().cloned();
    }
    cache.replace_cache(OriginCache::Ipc(Box::new(client)));
    eprintln!(
        "[ramsharedd] isolated GPU cache worker respawned pid={pid} (respawn {} of {})",
        runtime.budget.respawns_since_healthy, WORKER_RESPAWN_MAX_CONSECUTIVE
    );
}

fn startup_failed(
    cache: &mut RecoveryCache<'_>,
    runtime: &mut IsolatedWorkerRuntime,
    error: String,
) {
    runtime.budget.note_spawn_failed();
    runtime.unavailable_reason = Some(format!("isolated GPU cache worker startup failed: {error}"));
    cache.replace_cache(OriginCache::Disabled(DisabledCache));
    eprintln!(
        "[ramsharedd] isolated GPU cache worker respawn failed: {error} (attempt {} of {})",
        runtime.budget.respawns_since_healthy, WORKER_RESPAWN_MAX_CONSECUTIVE
    );
}

/// Called only after serving stops. Every member shares this exit budget.
pub(super) fn shutdown_worker_runtimes(
    runtimes: &mut [IsolatedWorkerRuntime],
    timeout: Duration,
    poll_interval: Duration,
) -> bool {
    let deadline = Instant::now().checked_add(timeout);
    loop {
        let mut complete = true;
        // Do not short-circuit: every creator must be cancelled and every
        // native child must receive a stop request before any waiting.
        for runtime in &mut *runtimes {
            complete &= poll_worker_shutdown(runtime);
        }
        let Some(deadline) = deadline else {
            return false;
        };
        if complete {
            return true;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || poll_interval.is_zero() {
            return false;
        }
        std::thread::sleep(remaining.min(poll_interval));
    }
}

fn poll_worker_shutdown(runtime: &mut IsolatedWorkerRuntime) -> bool {
    if let Some(startup) = runtime.startup.as_mut() {
        startup.cancel();
    }
    // A pending creator cannot prevent an already owned child from stopping.
    let child_done = runtime
        .supervisor
        .as_mut()
        .is_none_or(IsolatedWorkerSupervisor::shutdown);
    let creator_done = runtime.startup.as_ref().is_none_or(|s| s.is_quiescent());
    if creator_done {
        drop(runtime.startup.take());
    }
    creator_done && child_done
}

#[cfg(test)]
pub(super) fn shutdown_worker_runtime(
    runtime: &mut IsolatedWorkerRuntime,
    timeout: Duration,
    poll_interval: Duration,
) -> bool {
    shutdown_worker_runtimes(std::slice::from_mut(runtime), timeout, poll_interval)
}

pub(super) struct CacheWorkerObservation<'a> {
    pub(super) critical_reclaim: bool,
    pub(super) unavailable_reason: Option<&'a str>,
    pub(super) startup_pending: bool,
}

pub(super) fn cancel_pending_startup(runtime: &mut IsolatedWorkerRuntime, reason: &str) {
    if let Some(startup) = runtime.startup.as_mut()
        && !startup.is_cancelled()
    {
        startup.cancel();
        runtime.budget.note_spawn_failed();
        runtime.unavailable_reason = Some(reason.into());
    }
}
