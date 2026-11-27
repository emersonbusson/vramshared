// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/cache_status.rs
//! Cache-status lifecycle observation, bounded publication and atomic wire output.
use super::worker_recovery::CacheWorkerObservation;
use super::{
    ControlPlaneCacheAdmission, GPU_BUDGET_MAX_AGE_MS, NbdRuntimeStarter, OriginCache,
    OriginCacheState, SERVE_IDLE_TICK, origin_cache_runtime_ok, unix_time_ms,
};
use ramshared_block::{
    AuthoritativeOriginBackend, BestEffortCache, BlockBackend, CacheMutation, FileOrigin,
    OriginCacheIo,
};
use ramshared_vram::{GpuBudgetTelemetry, GpuUtilizationTelemetry, WorkerCacheTelemetry};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

pub(super) struct PublicationContext<'a> {
    pub(super) configuration_generation: Option<u64>,
    pub(super) daemon_instance_id: &'a str,
    pub(super) serve_threads: usize,
    pub(super) serve_requests_total: u64,
    pub(super) worker: CacheWorkerObservation<'a>,
    pub(super) control_admission: Option<&'a ControlPlaneCacheAdmission>,
    pub(super) pooled_session: Option<&'a ramshared_wsl2d::pooled_cache::GuestCacheSession>,
}

#[derive(Default)]
pub(super) struct CacheStatusPublisher {
    last_attempt: Option<Instant>,
    last_status: Option<OriginCacheStatus>,
    last_target_bytes: u64,
    last_critical: bool,
    last_startup_pending: bool,
}
impl CacheStatusPublisher {
    pub(super) fn publication_due(&self) -> bool {
        self.due_at(Instant::now())
    }
    fn due_at(&self, now: Instant) -> bool {
        self.last_attempt
            .is_none_or(|at| now.saturating_duration_since(at) >= SERVE_IDLE_TICK)
    }
    fn unchanged(
        &self,
        cache: &AuthoritativeOriginBackend<FileOrigin, OriginCache>,
        context: &PublicationContext<'_>,
    ) -> bool {
        let Some(last) = self.last_status.as_ref() else {
            return false;
        };
        self.last_critical == context.worker.critical_reclaim
            && last.configuration_generation == context.configuration_generation
            && self.last_startup_pending == context.worker.startup_pending
            && last.cache_unavailable_reason.as_deref() == context.worker.unavailable_reason
            && last.origin_state == cache.origin_state().as_str()
            && last.control_plane_state
                == context
                    .control_admission
                    .map(ControlPlaneCacheAdmission::control_state)
            && cache
                .try_control_cache(|client| {
                    client.state().as_str() == last.cache_state
                        && client.target_bytes() == self.last_target_bytes
                        // An omitted member snapshot (busy at capture) is unknown,
                        // not a new lifecycle transition on every later job.
                        && last.cache_members.as_deref().is_none_or(|members|
                            client.member_lifecycle_matches(Some(members)))
                })
                .unwrap_or(true)
    }
    pub(super) fn publish<S: NbdRuntimeStarter + ?Sized>(
        &mut self,
        starter: &mut S,
        cache: &AuthoritativeOriginBackend<FileOrigin, OriginCache>,
        context: PublicationContext<'_>,
    ) -> Vec<(usize, bool)> {
        let pressure_revoked = apply_critical_reclaim(
            cache,
            context.worker.critical_reclaim,
            context.worker.startup_pending,
        );
        if !self.publication_due() && self.unchanged(cache, &context) {
            return pressure_revoked;
        }
        let status = snapshot(cache, &context);
        if let Some(state) = status.control_plane_state
            && self
                .last_status
                .as_ref()
                .and_then(|last| last.control_plane_state)
                != Some(state)
        {
            eprintln!(
                "[ramsharedd] control-plane state={state} lease_deadline_ms={:?}",
                status.control_plane_lease_deadline_ms
            );
        }
        self.last_target_bytes = cache.target_bytes();
        starter.publish_origin_cache(&status);
        // An attempted write consumes the interval too: a failed filesystem
        // operation must not be retried once per arriving NBD request.
        self.last_attempt = Some(Instant::now());
        self.last_critical = context.worker.critical_reclaim;
        self.last_startup_pending = context.worker.startup_pending;
        self.last_status = Some(status);
        pressure_revoked
    }
}

fn apply_critical_reclaim(
    cache: &AuthoritativeOriginBackend<FileOrigin, OriginCache>,
    critical_reclaim: bool,
    startup_pending: bool,
) -> Vec<(usize, bool)> {
    // RF-9: a release is an act on a **serving** cache. Once it is off there
    // is nothing to revoke and nothing to announce; the unconditional call
    // turned every loop pass into a "revoked" line.
    if !critical_reclaim {
        return Vec::new();
    }
    match cache.release_cache_inspected(
        |client| {
            let state = client.state();
            let cache_is_serving = matches!(
                state,
                OriginCacheState::Active | OriginCacheState::Restricted
            );
            let cancel_pending = startup_pending && state != OriginCacheState::Off;
            (cache_is_serving || cancel_pending).then(|| client.pressure_candidates())
        },
        OriginCache::pressure_off_members,
    ) {
        // No MiB figure: the isolation boundary reports the control
        // outcome only, never how much the cache held. Printing a
        // placeholder here published "reclaimed 0 MiB" as a measurement
        // the daemon never took.
        Ok(Some((CacheMutation::Accepted, members, off))) => {
            eprintln!("[ramsharedd] control pressure revoked the clean origin cache");
            return members
                .into_iter()
                .filter(|(index, _)| off.contains(index))
                .collect();
        }
        Ok(Some((CacheMutation::Skipped, members, off))) => {
            eprintln!(
                "[ramsharedd] control pressure found the clean origin cache already released"
            );
            // An in-flight startup cancelled by pressure is recoverable,
            // unlike an already-Off cache explicitly disabled elsewhere.
            return members
                .into_iter()
                .filter(|(index, starting)| *starting && off.contains(index))
                .collect();
        }
        Ok(Some((CacheMutation::Failed, members, off))) => {
            eprintln!("[ramsharedd] control cache release partially failed");
            return members
                .into_iter()
                .filter(|(index, _)| off.contains(index))
                .collect();
        }
        Err(error) => eprintln!(
            "[ramsharedd] control cache release was not acknowledged: {}",
            error.0
        ),
        Ok(None) => {}
    }
    Vec::new()
}

fn snapshot(
    cache: &AuthoritativeOriginBackend<FileOrigin, OriginCache>,
    context: &PublicationContext<'_>,
) -> OriginCacheStatus {
    let PublicationContext {
        configuration_generation,
        daemon_instance_id,
        serve_threads,
        serve_requests_total,
        worker,
        control_admission,
        pooled_session,
    } = context;
    let pooled_cache = pooled_session.map(|session| {
        let pooled_tel = cache.pooled_read().map(|p| {
            if p.active_epoch() == 0 && session.active_epoch() != 0 {
                session.retire_to_origin_only("pooled_epoch_retired");
            }
            p.telemetry()
        });
        session.status(pooled_tel)
    });
    let host_memory = control_admission.and_then(ControlPlaneCacheAdmission::host_memory);
    let host_storage = control_admission.and_then(ControlPlaneCacheAdmission::host_storage);
    let host_gpu_activity = control_admission.and_then(|admission| {
        if admission.lease_lost() {
            return None;
        }
        let sample = admission
            .host_activity
            .telemetry(Instant::now(), unix_time_ms()?)?;
        (!admission.lease_lost()).then_some(sample)
    });
    let critical_reclaim = worker.critical_reclaim;
    let physical_cached_bytes = cache.refresh_cached_bytes();
    let gpu_budget = cache.gpu_budget_telemetry();
    // Busy time is the sibling of the budget, not a field of it: the budget
    // answers "how much VRAM", this answers "how busy is the adapter over the
    // driver's trailing sample window". `None` is "not measured" and travels
    // as absence — never as a zero-percent idle sample the driver did not
    // report.
    let gpu_utilization = cache.gpu_utilization_telemetry();
    // DT-8: cache occupancy is published beside the adapter budget, never
    // merged into it and never labelled as RAM.
    let gpu_cache = cache.cache_telemetry();
    let gpu_headroom_kib = unix_time_ms().and_then(|now| {
        gpu_budget
            .as_ref()?
            .trusted_available_at(now, GPU_BUDGET_MAX_AGE_MS)
            .map(alloc_bound_kib)
    });
    let telemetry = cache.telemetry();
    OriginCacheStatus {
        configuration_generation: *configuration_generation,
        schema_version: 1,
        daemon_instance_id: (*daemon_instance_id).to_string(),
        // An unreadable clock is "not measured", never a write at the Unix
        // epoch. `None` publishes as `null` and the freshness gate refuses
        // the document for the same reason it refuses a stale one: neither
        // establishes that the measurements are live. `unwrap_or_default()`
        // used to claim 1970 here, which is a time the daemon never observed,
        // and it sent readers chasing staleness when the real fault was the
        // clock.
        written_at_unix_ms: unix_time_ms(),
        ok: (physical_cached_bytes.is_ok()
            || matches!(
                cache.cache_state(),
                OriginCacheState::Unavailable | OriginCacheState::Off
            ))
            && !critical_reclaim
            && origin_cache_runtime_ok(cache.origin_state(), cache.cache_state()),
        origin_state: cache.origin_state().as_str(),
        cache_state: cache.cache_state().as_str(),
        // SPEC DT-3: the cause beside the state. `None` publishes as `null`
        // — the daemon said nothing about the cache's ability to serve, which
        // is not the same news as a published cause.
        cache_unavailable_reason: worker.unavailable_reason.map(str::to_string),
        logical_capacity_kib: hold_kib(cache.size_bytes()),
        // An unreadable occupancy is "not measured", never a measured zero.
        // `Some(0)` is a real empty cache after a successful refresh; `None`
        // is the refresh failing, and collapsing the two would publish a
        // figure the daemon never observed.
        vram_cached_kib: published_cached_kib(physical_cached_bytes),
        gpu_headroom_kib,
        gpu_budget,
        cache_members: cache
            .try_control_cache(|client| client.member_statuses())
            .ok()
            .flatten(),
        gpu_utilization,
        host_gpu_activity,
        gpu_cache,
        host_memory,
        host_storage,
        control_plane_state: control_admission.map(ControlPlaneCacheAdmission::control_state),
        control_plane_lease_deadline_ms: control_admission
            .and_then(ControlPlaneCacheAdmission::lease_deadline_ms),
        ssd_origin_written_kib: hold_kib(telemetry.origin_written_bytes),
        cache_hits: telemetry.cache_hits,
        cache_fallback_reads: telemetry.fallback_reads,
        cache_invalidations: telemetry.invalidations,
        cache_releases: telemetry.releases,
        origin_cache_io: OriginCacheIo::from_isolated(telemetry),
        // A target is the cache's size view, not an admission instruction:
        // nothing allocates from this published figure (the cap stays in
        // bytes inside `safe_cache_target`). It is therefore a hold/size face
        // and takes [`hold_kib`], the same direction as the capacity and
        // occupancy beside it. `critical_reclaim` publishes a deliberate `0`
        // — the product is forcing the target down — which is a policy zero
        // at this unit, not a remainder floor.
        cache_target_kib: if critical_reclaim {
            0
        } else {
            hold_kib(cache.target_bytes())
        },
        serve_threads: *serve_threads,
        serve_requests_total: *serve_requests_total,
        pooled_cache,
    }
}

// Admission arithmetic stays in bytes. Public sizes and holdings round up;
// an allocation bound rounds down so it cannot grant extra capacity.
const KIB_BYTES: u64 = 1024;
pub(super) const fn hold_kib(bytes: u64) -> u64 {
    bytes.div_ceil(KIB_BYTES)
}
pub(super) const fn alloc_bound_kib(bytes: u64) -> u64 {
    bytes / KIB_BYTES
}
/// Failed occupancy refresh remains absent; a successful zero is measured empty.
pub(super) fn published_cached_kib(
    physical_cached_bytes: Result<u64, &'static str>,
) -> Option<u64> {
    physical_cached_bytes.ok().map(hold_kib)
}

#[derive(Clone, serde::Serialize)]
pub(super) struct OriginCacheStatus {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) configuration_generation: Option<u64>,
    pub(super) schema_version: u8,
    pub(super) daemon_instance_id: String,
    /// When this document was written, in Unix milliseconds. `None` is "not
    /// measured" — the clock read failed — and publishes as `null`, never as
    /// a `0` that would claim the document was written at the Unix epoch. A
    /// present value is the write time the daemon actually read.
    pub(super) written_at_unix_ms: Option<u64>,
    pub(super) ok: bool,
    pub(super) origin_state: &'static str,
    pub(super) cache_state: &'static str,
    /// Why the GPU cache is not serving, when it is not. `None` publishes as
    /// `null`: the daemon observed no cause, which is different from a cause
    /// it observed and could not explain.
    pub(super) cache_unavailable_reason: Option<String>,
    /// Logical cache capacity, in KiB — a **size**, so [`hold_kib`] (ceil).
    pub(super) logical_capacity_kib: u64,
    /// Physical occupancy of the VRAM cache, in KiB. `None` is "not
    /// measured" — the refresh failed — and publishes as `null`. A present
    /// `0` is a real empty cache after a successful refresh. A **hold**, so
    /// [`hold_kib`] (ceil): a sub-KiB occupancy holds something.
    pub(super) vram_cached_kib: Option<u64>,
    /// How much VRAM may still be taken, in KiB — an **allocation bound**, so
    /// [`alloc_bound_kib`] (floor). `None` is "not measured" or a sample that
    /// failed [`GpuBudgetTelemetry::trusted_available_at`]. A present `0` is
    /// "at most 0 KiB"; a sub-KiB remainder floors to that zero and the third
    /// state lives in [`Self::gpu_budget`]'s `available_bytes`, never here.
    pub(super) gpu_headroom_kib: Option<u64>,
    pub(super) gpu_budget: Option<GpuBudgetTelemetry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) cache_members: Option<Vec<ramshared_block::cache_group::CacheMemberStatus>>,
    /// Adapter busy time over the driver's trailing sample window (DT-8
    /// sibling of the budget). `None` is "not measured", never measured idle.
    pub(super) gpu_utilization: Option<GpuUtilizationTelemetry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) host_gpu_activity:
        Option<ramshared_vram::host_gpu_activity::HostGpuActivityTelemetry>,
    /// Cache occupancy and codec health (DT-8). Logical cache bytes are cache
    /// occupancy in the worker's address space, never guest or host RAM.
    pub(super) gpu_cache: Option<WorkerCacheTelemetry>,
    /// Last host-memory sample received on the authenticated heartbeat. This
    /// is a host figure, never guest RAM or PSI; `None` means the host did not
    /// publish one, not a measured zero.
    pub(super) host_memory: Option<ramshared_ipc::HostMemoryTelemetry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) host_storage: Option<ramshared_ipc::storage_proof::HostStorageTelemetry>,
    /// The control-plane authority's own state, as the operator sees it.
    ///
    /// The cache's state says whether the cache is serving; this says why
    /// the authority is or is not admitting it: the peer has not arrived, a
    /// lease is live, the lease was lost, or the transport failed.
    pub(super) control_plane_state: Option<&'static str>,
    /// The lease deadline the authority holds, in Unix milliseconds.
    /// `None` publishes as `null`: no lease, not a zero deadline.
    pub(super) control_plane_lease_deadline_ms: Option<u64>,
    /// Lifetime bytes the cache wrote through to the SSD origin, in KiB — an
    /// accumulated **hold**, so [`hold_kib`] (ceil): a sub-KiB write-through
    /// still wrote something.
    pub(super) ssd_origin_written_kib: u64,
    /// Successful cache-hit reads since this daemon instance started. A count,
    /// paired with `cache_fallback_reads` to derive a hit rate; bytes served
    /// stay in `origin_cache_io.cache_read_bytes`.
    pub(super) cache_hits: u64,
    pub(super) cache_fallback_reads: u64,
    pub(super) cache_invalidations: u64,
    pub(super) cache_releases: u64,
    /// Origin-plane I/O accounting with no top-level key. The four counters
    /// above keep their existing names and are not repeated here, so one fact
    /// has one published name. Always present: these are the daemon's own
    /// in-process counters, which a publish always observes.
    pub(super) origin_cache_io: OriginCacheIo,
    /// The cache's target size, in KiB — a **size** view of the target, so
    /// [`hold_kib`] (ceil). Nothing allocates from this published figure; the
    /// admission cap stays in bytes inside `safe_cache_target`. `0` here is
    /// either the `critical_reclaim` policy zero or a target that really is
    /// empty — never a sub-KiB remainder floor.
    pub(super) cache_target_kib: u64,
    /// Serve-pool worker count configured for this runtime (DT-6). Published
    /// beside the origin counters so a serving rate is derivable from one
    /// document (NFR-3).
    pub(super) serve_threads: usize,
    /// Jobs served since this daemon instance started, including the
    /// dispatcher's inline queue-full fallbacks. Monotonic.
    pub(super) serve_requests_total: u64,
    /// Optional pooled Hyper-V VRAM cache telemetry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) pooled_cache: Option<ramshared_wsl2d::pooled_cache::PooledGuestStatus>,
}

/// Each atomic replacement owns a unique staging path. Exclusive creation
/// refuses residue; a per-process name would let concurrent writers exchange
/// documents across destinations.
pub(super) fn cache_status_staging_path(parent: &Path) -> PathBuf {
    static NEXT_STAGING: AtomicUsize = AtomicUsize::new(0);
    let sequence = NEXT_STAGING.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(
        ".cache-status.{}.{sequence}.tmp",
        std::process::id()
    ))
}

pub(super) fn write_origin_cache_status(
    path: &Path,
    status: &OriginCacheStatus,
) -> Result<(), String> {
    let encode = |status| serde_json::to_vec(status).map_err(|error| error.to_string());
    let mut encoded = encode(status)?;
    if encoded.len() > ramshared_block::cache_group::MAX_CACHE_STATUS_BYTES
        && status.host_gpu_activity.is_some()
    {
        let mut without_activity = status.clone();
        without_activity.host_gpu_activity = None;
        encoded = encode(&without_activity)?;
    }
    if encoded.len() > ramshared_block::cache_group::MAX_CACHE_STATUS_BYTES {
        return Err("cache-status document exceeds its encoded size limit".into());
    }
    let parent = path
        .parent()
        .ok_or_else(|| "cache-status path has no parent".to_string())?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temporary = cache_status_staging_path(parent);
    // `create_new` rather than a truncating write: a staging path belongs to
    // this write alone, and reusing one is how an atomic replace turns into a
    // document swap. A name that already exists is an error, never a
    // coincidence to overwrite.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| error.to_string())?;
    let written = std::io::Write::write_all(&mut file, &encoded).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        error.to_string()
    });
    // Close before the rename so the name carries the bytes and nothing holds
    // the staging path open across the swap.
    drop(file);
    if let Err(error) =
        written.and_then(|()| std::fs::rename(&temporary, path).map_err(|error| error.to_string()))
    {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn publish_origin_cache_status<S: NbdRuntimeStarter + ?Sized>(
    starter: &mut S,
    cache: &AuthoritativeOriginBackend<FileOrigin, OriginCache>,
    daemon_instance_id: &str,
    serve_threads: usize,
    serve_requests_total: u64,
    worker: CacheWorkerObservation<'_>,
    control_admission: Option<&ControlPlaneCacheAdmission>,
) {
    apply_critical_reclaim(cache, worker.critical_reclaim, worker.startup_pending);
    starter.publish_origin_cache(&snapshot(
        cache,
        &PublicationContext {
            configuration_generation: None,
            daemon_instance_id,
            serve_threads,
            serve_requests_total,
            worker,
            control_admission,
            pooled_session: None,
        },
    ));
}

#[cfg(test)]
mod tests;
