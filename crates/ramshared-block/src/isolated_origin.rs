// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/isolated_origin.rs
//! Authoritative origin I/O with a bounded, revocable cache boundary.
//!
//! The origin never depends on a cache response for correctness. Cache reads
//! have a hard deadline; cache mutations are non-blocking and any queue,
//! transport, protocol, or timeout fault permanently revokes that client.

use crate::origin_cache::{CacheState, CacheTelemetry, OriginState, OriginStorage};
use crate::{BlockBackend, IoError, WriteOptions};
use ramshared_vram::{
    GpuAdapterIdentity, GpuBudgetTelemetry, GpuUtilizationTelemetry, WorkerCacheTelemetry,
};
use std::sync::Mutex;

mod cache_client;
mod range_lock;

pub(crate) use cache_client::mutation_frames;
pub use cache_client::{
    AllowCacheAdmission, BestEffortCache, BoundedCache, BoundedCacheClient, CacheAdmission,
    CacheMutation, CacheRead, CacheReleaseOutcome, DisabledCache, IsolatedCacheControl,
    IsolatedCacheRequest, IsolatedCacheWorker, MAX_CACHE_MUTATION_BYTES, RefuseCacheAdmission,
    isolated_cache_channel,
};

pub struct AuthoritativeOriginBackend<O, C> {
    origin: O,
    /// Cache frames share this lock. Range guards coordinate the complete
    /// origin/cache transaction; state counters use their own short lock.
    cache: Mutex<C>,
    /// Serializes overlapping origin/cache transactions, not unrelated regions.
    ranges: range_lock::RangeLocks,
    /// Excludes concurrent writes while a durability barrier is in progress.
    sync_gate: std::sync::RwLock<()>,
    size: u64,
    block: u32,
    state: Mutex<BackendState>,
    admission: Box<dyn CacheAdmission>,
}

struct BackendState {
    telemetry: CacheTelemetry,
    origin_state: OriginState,
    origin_probe_successes: u8,
    origin_dirty: bool,
    /// The authority's latest refusal, kept so the daemon can publish the
    /// **cause** beside the cache state instead of only the worker exit.
    admission_refusal: Option<String>,
}

impl<O: OriginStorage, C: BestEffortCache> AuthoritativeOriginBackend<O, C> {
    /// Construct with the fail-closed default: the cache is refused until a
    /// caller supplies an authority through [`Self::new_with_admission`].
    pub fn new(origin: O, cache: C, size: u64, block: u32) -> Result<Self, IoError> {
        Self::new_with_admission(origin, cache, size, block, RefuseCacheAdmission)
    }

    pub fn new_with_admission(
        origin: O,
        cache: C,
        size: u64,
        block: u32,
        admission: impl CacheAdmission + 'static,
    ) -> Result<Self, IoError> {
        if size == 0 || block == 0 || !size.is_multiple_of(block as u64) {
            return Err(IoError("invalid authoritative origin geometry".into()));
        }
        Ok(Self {
            origin,
            cache: Mutex::new(cache),
            ranges: range_lock::RangeLocks::default(),
            sync_gate: std::sync::RwLock::new(()),
            size,
            block,
            state: Mutex::new(BackendState {
                telemetry: CacheTelemetry::default(),
                origin_state: OriginState::Ready,
                origin_probe_successes: 0,
                origin_dirty: false,
                admission_refusal: None,
            }),
            admission: Box::new(admission),
        })
    }

    /// Whether the cache may be used right now (DT-9), revoking what is
    /// there when the authority refuses.
    ///
    /// An unleased cache must not serve or be populated, and leaving it
    /// `Active` would advertise a cache the authority refuses — so a refusal
    /// revokes it. The revoke is idempotent and safe on every I/O path.
    fn cache_admitted(&self) -> bool {
        let refusal = match self.admission.admit_cache() {
            Ok(()) => {
                if let Ok(mut state) = self.state.lock() {
                    state.admission_refusal = None;
                }
                return true;
            }
            Err(reason) => reason,
        };
        if let Ok(mut state) = self.state.lock()
            && state.admission_refusal.is_none()
        {
            state.admission_refusal = Some(refusal);
        }
        // A refusal that only means "the peer has not arrived yet" must not
        // revoke: the cache is not serving, and a revoke would move it to a
        // state the product never recovers from.
        if self.admission.cache_revocation_pending() {
            return false;
        }
        // Revoke only a cache that could serve. An already Off/Unavailable/
        // Stuck cache is left as its own honest state — attempting a revoke
        // there would send a no-op control frame and can degrade the state.
        let could_serve = self
            .cache
            .lock()
            .map(|cache| matches!(cache.state(), CacheState::Active | CacheState::Restricted))
            .unwrap_or(false);
        if could_serve {
            let _ = self.revoke_cache();
        }
        false
    }

    /// Why the authority last refused the cache, or `None` while it admits.
    ///
    /// This is the admission verdict itself, not the worker's exit mechanism:
    /// a host with no vsock peer must be reported as "no live lease", not as
    /// "a worker exited", which is only the consequence of the refusal.
    pub fn admission_refusal_reason(&self) -> Option<String> {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.admission_refusal.clone())
    }

    pub fn origin_state(&self) -> OriginState {
        self.state
            .lock()
            .map(|state| state.origin_state)
            .unwrap_or(OriginState::Failed)
    }

    pub fn cache_state(&self) -> CacheState {
        self.cache
            .lock()
            .map(|cache| cache.state())
            .unwrap_or(CacheState::Unavailable)
    }

    /// Swap in a fresh best-effort cache after the backing worker was replaced.
    ///
    /// The origin stays authoritative and its telemetry is untouched: the new
    /// cache starts empty and warms from origin reads, so a respawn can never
    /// serve pre-death bytes and never loses the `fallback_reads` /
    /// `invalidations` accounting that documents the fall back.
    pub fn replace_cache(&self, cache: C) {
        if let Ok(mut current) = self.cache.lock() {
            *current = cache;
        }
    }

    /// Run bounded control on the exact cache under its existing serialization
    /// boundary. Busy/poisoned ownership refuses without waiting or replacing it.
    pub fn try_control_cache<R>(
        &self,
        operation: impl FnOnce(&mut C) -> R,
    ) -> Result<R, &'static str> {
        let mut cache = self
            .cache
            .try_lock()
            .map_err(|_| "cache control is busy or poisoned")?;
        Ok(operation(&mut cache))
    }

    pub fn cached_bytes(&self) -> u64 {
        self.cache
            .lock()
            .map(|cache| cache.cached_bytes())
            .unwrap_or(0)
    }

    pub fn refresh_cached_bytes(&self) -> Result<u64, &'static str> {
        self.cache
            .lock()
            .map_err(|_| "cache client is poisoned")?
            .refresh_cached_bytes()
    }

    pub fn target_bytes(&self) -> u64 {
        self.cache
            .lock()
            .map(|cache| cache.target_bytes())
            .unwrap_or(0)
    }

    pub fn bound_adapter_identity(&self) -> Option<GpuAdapterIdentity> {
        self.cache.lock().ok()?.bound_adapter_identity()
    }

    pub fn gpu_budget_telemetry(&self) -> Option<GpuBudgetTelemetry> {
        self.cache.lock().ok()?.gpu_budget_telemetry()
    }

    /// Adapter busy time from the underlying cache client.
    pub fn gpu_utilization_telemetry(&self) -> Option<GpuUtilizationTelemetry> {
        self.cache.lock().ok()?.gpu_utilization_telemetry()
    }

    /// Cache occupancy and codec health from the underlying cache client.
    pub fn cache_telemetry(&self) -> Option<WorkerCacheTelemetry> {
        self.cache.lock().ok()?.cache_telemetry()
    }

    pub fn telemetry(&self) -> CacheTelemetry {
        self.state
            .lock()
            .map(|state| state.telemetry)
            .unwrap_or_default()
    }

    /// Revokes the best-effort cache and reports the control outcome that was
    /// observed.
    ///
    /// No byte figure is returned. The isolated cache is revoked wholesale and
    /// the client never reports how much it held, so the amount is absent at
    /// this boundary — not zero
    /// (`release_cache_succeeds_only_after_dedicated_control_acknowledgement`).
    ///
    /// `telemetry.releases` counts only a cache that was actually revoked.
    /// `AlreadyReleased` means the cache was already `Off`/`Unavailable` and
    /// [`revoke_cache`] sent nothing, so counting it would publish a release
    /// for a no-op. That is not academic: the control plane keeps a
    /// `control_pressure` request live under memory pressure, this runs on the
    /// per-job serving path, and an ungated counter therefore grows with I/O
    /// while `cache_state` is `OFF` and `vram_cached_kib` is `0`. The sibling
    /// `invalidations` counter is gated the same way in [`revoke_cache`] and
    /// reported 1 beside 1.5M of these.
    pub fn release_cache(&self) -> Result<CacheReleaseOutcome, IoError> {
        self.release_cache_observed(|_| ())
            .map(|(outcome, ())| outcome)
    }

    /// Capture member identity under the same cache lock as the revocation.
    /// The observation is returned only when the control operation succeeds.
    pub fn release_cache_observed<R>(
        &self,
        observe: impl FnOnce(&C) -> R,
    ) -> Result<(CacheReleaseOutcome, R), IoError> {
        self.release_cache_if_observed(|cache| Some(observe(cache)))?
            .ok_or_else(|| IoError("cache release observation was absent".into()))
    }

    /// Revoke only if the observation authorizes it, without dropping the lock
    /// between the observation and the cache mutation.
    pub fn release_cache_if_observed<R>(
        &self,
        observe: impl FnOnce(&C) -> Option<R>,
    ) -> Result<Option<(CacheReleaseOutcome, R)>, IoError> {
        let Some((mutation, observed, ())) = self.release_cache_inspected(observe, |_| ())? else {
            return Ok(None);
        };
        match mutation {
            CacheMutation::Accepted => Ok(Some((CacheReleaseOutcome::Revoked, observed))),
            CacheMutation::Skipped => Ok(Some((CacheReleaseOutcome::AlreadyReleased, observed))),
            CacheMutation::Failed => Err(IoError(
                "cache release acknowledgement was unavailable".into(),
            )),
        }
    }

    /// Inspect both sides of a release under one lock, including partial failure.
    pub fn release_cache_inspected<R, S>(
        &self,
        before: impl FnOnce(&C) -> Option<R>,
        after: impl FnOnce(&C) -> S,
    ) -> Result<Option<(CacheMutation, R, S)>, IoError> {
        let (observed, mutation, result, was_live) = {
            let mut cache = self
                .cache
                .lock()
                .map_err(|_| IoError("cache release acknowledgement was unavailable".into()))?;
            let Some(observed) = before(&cache) else {
                return Ok(None);
            };
            let was_live = !matches!(cache.state(), CacheState::Off | CacheState::Unavailable);
            let mutation = cache.disable();
            (observed, mutation, after(&cache), was_live)
        };
        if was_live && let Ok(mut state) = self.state.lock() {
            state.telemetry.invalidations = state.telemetry.invalidations.saturating_add(1);
        }
        if mutation == CacheMutation::Accepted
            && let Ok(mut state) = self.state.lock()
        {
            state.telemetry.releases = state.telemetry.releases.saturating_add(1);
        }
        Ok(Some((mutation, observed, result)))
    }

    pub fn probe_origin(&self) -> Result<OriginState, IoError> {
        let _sync = self
            .sync_gate
            .write()
            .map_err(|_| IoError("origin sync gate poisoned".into()))?;
        let mut block = vec![0; self.block as usize];
        let result = self
            .origin
            .read_exact_at(0, &mut block)
            .and_then(|()| self.origin.sync_data());
        if let Err(error) = result {
            self.mark_origin_failed();
            return Err(error);
        }
        let Ok(mut state) = self.state.lock() else {
            return Err(IoError("backend state is poisoned".into()));
        };
        state.origin_dirty = false;
        state.telemetry.origin_syncs = state.telemetry.origin_syncs.saturating_add(1);
        if matches!(
            state.origin_state,
            OriginState::Failed | OriginState::Degraded
        ) {
            state.origin_probe_successes = state.origin_probe_successes.saturating_add(1);
            if state.origin_probe_successes >= 3 {
                state.origin_state = OriginState::Ready;
                state.origin_probe_successes = 0;
            } else {
                state.origin_state = OriginState::Degraded;
            }
        }
        let answer = state.origin_state;
        drop(state);
        Ok(answer)
    }

    fn check_range(&self, offset: u64, len: usize) -> Result<(), IoError> {
        offset
            .checked_add(len as u64)
            .filter(|end| *end <= self.size)
            .map(|_| ())
            .ok_or_else(|| IoError("authoritative origin I/O is out of range".into()))
    }

    fn require_ready_origin(&self) -> Result<(), IoError> {
        let ready = self
            .state
            .lock()
            .map(|state| state.origin_state == OriginState::Ready)
            .unwrap_or(false);
        if ready {
            Ok(())
        } else {
            Err(IoError(
                "origin authority is unavailable pending three read+sync probes".into(),
            ))
        }
    }

    fn revoke_cache(&self) -> CacheMutation {
        let Ok(mut cache) = self.cache.lock() else {
            return CacheMutation::Failed;
        };
        if !matches!(cache.state(), CacheState::Off | CacheState::Unavailable)
            && let Ok(mut state) = self.state.lock()
        {
            state.telemetry.invalidations = state.telemetry.invalidations.saturating_add(1);
        }
        cache.disable()
    }

    fn mark_origin_failed(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.origin_state = OriginState::Failed;
            state.origin_probe_successes = 0;
        }
        let _ = self.revoke_cache();
    }

    fn write_origin(&self, offset: u64, data: &[u8]) -> Result<(), IoError> {
        if let Err(error) = self.origin.write_all_at(offset, data) {
            self.mark_origin_failed();
            return Err(error);
        }
        if let Ok(mut state) = self.state.lock() {
            state.origin_dirty = true;
            state.telemetry.origin_written_bytes = state
                .telemetry
                .origin_written_bytes
                .saturating_add(data.len() as u64);
        }
        Ok(())
    }

    fn sync_dirty_origin(&self) -> Result<(), IoError> {
        let dirty = self
            .state
            .lock()
            .map(|state| state.origin_dirty)
            .unwrap_or(false);
        if !dirty {
            return Ok(());
        }
        if let Err(error) = self.origin.sync_data() {
            self.mark_origin_failed();
            return Err(error);
        }
        if let Ok(mut state) = self.state.lock() {
            state.origin_dirty = false;
            state.telemetry.origin_syncs = state.telemetry.origin_syncs.saturating_add(1);
        }
        Ok(())
    }

    /// Mirror an already-authorized origin write into the cache, one
    /// `MAX_CACHE_MUTATION_BYTES` frame at a time. The origin is already
    /// committed, so a mid-write failure revokes the cache and leaves reads
    /// falling back to origin — correctness never depends on the mirror.
    fn update_cache(&self, offset: u64, data: &[u8]) {
        // DT-9: no mirror into an unleased cache.
        if !self.cache_admitted() {
            return;
        }
        for (offset, slice) in mutation_frames(offset, data) {
            let failed = match self.cache.lock() {
                Ok(mut cache) => cache.update(offset, slice) == CacheMutation::Failed,
                Err(_) => true,
            };
            if failed {
                if let Ok(mut state) = self.state.lock() {
                    state.telemetry.cache_write_failures =
                        state.telemetry.cache_write_failures.saturating_add(1);
                }
                let _ = self.revoke_cache();
                return;
            }
        }
    }
}

impl<O: OriginStorage, C: BestEffortCache> AuthoritativeOriginBackend<O, C> {
    /// Serve a read. `&self`: the origin is positional, the cache socket and
    /// the counters take their own locks (SPEC DT-3).
    pub fn read_at(&self, offset: u64, destination: &mut [u8]) -> Result<(), IoError> {
        self.check_range(offset, destination.len())?;
        let _range = self.ranges.read(offset, destination.len())?;
        self.require_ready_origin()?;
        if destination.is_empty() {
            return Ok(());
        }
        // DT-9: the cache is consulted only under an admitted authority; an
        // unleased cache is revoked and the read falls through to origin.
        let cache_admitted = self.cache_admitted();
        let read = if cache_admitted {
            match self.cache.lock() {
                Ok(mut cache) => cache.read(offset, destination),
                Err(_) => CacheRead::Failed,
            }
        } else {
            CacheRead::Miss
        };
        match read {
            CacheRead::Hit => {
                if let Ok(mut state) = self.state.lock() {
                    state.telemetry.cache_read_bytes = state
                        .telemetry
                        .cache_read_bytes
                        .saturating_add(destination.len() as u64);
                    state.telemetry.cache_hits = state.telemetry.cache_hits.saturating_add(1);
                }
                return Ok(());
            }
            CacheRead::Miss => {}
            CacheRead::Failed => {
                if let Ok(mut state) = self.state.lock() {
                    state.telemetry.cache_read_failures =
                        state.telemetry.cache_read_failures.saturating_add(1);
                }
                let _ = self.revoke_cache();
            }
        }
        if let Err(error) = self.origin.read_exact_at(offset, destination) {
            self.mark_origin_failed();
            return Err(error);
        }
        if let Ok(mut state) = self.state.lock() {
            state.telemetry.fallback_reads = state.telemetry.fallback_reads.saturating_add(1);
        }
        if cache_admitted {
            for (offset, slice) in mutation_frames(offset, destination) {
                let failed = match self.cache.lock() {
                    Ok(mut cache) => cache.promote(offset, slice) == CacheMutation::Failed,
                    Err(_) => true,
                };
                if failed {
                    if let Ok(mut state) = self.state.lock() {
                        state.telemetry.promotion_refusals =
                            state.telemetry.promotion_refusals.saturating_add(1);
                    }
                    let _ = self.revoke_cache();
                    break;
                }
            }
        }
        Ok(())
    }

    /// Serve a write: origin write-through first, then the best-effort mirror.
    pub fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), IoError> {
        self.check_range(offset, data.len())?;
        let _sync = self
            .sync_gate
            .read()
            .map_err(|_| IoError("origin sync gate poisoned".into()))?;
        let _range = self.ranges.write(offset, data.len())?;
        self.require_ready_origin()?;
        if data.is_empty() {
            return Ok(());
        }
        self.write_origin(offset, data)?;
        if let Ok(mut state) = self.state.lock() {
            state.telemetry.batched_writes = state.telemetry.batched_writes.saturating_add(1);
        }
        self.update_cache(offset, data);
        Ok(())
    }

    pub fn write_at_with_options(
        &self,
        offset: u64,
        data: &[u8],
        options: WriteOptions,
    ) -> Result<(), IoError> {
        if !options.fua {
            return self.write_at(offset, data);
        }
        self.check_range(offset, data.len())?;
        let _sync = self
            .sync_gate
            .write()
            .map_err(|_| IoError("origin sync gate poisoned".into()))?;
        let _range = self.ranges.write(offset, data.len())?;
        self.require_ready_origin()?;
        if data.is_empty() {
            return Ok(());
        }
        self.write_origin(offset, data)?;
        self.sync_dirty_origin()?;
        self.update_cache(offset, data);
        Ok(())
    }

    pub fn flush(&self) -> Result<(), IoError> {
        let _sync = self
            .sync_gate
            .write()
            .map_err(|_| IoError("origin sync gate poisoned".into()))?;
        self.require_ready_origin()?;
        self.sync_dirty_origin()
    }
}

impl<O: OriginStorage, C: BestEffortCache> BlockBackend for AuthoritativeOriginBackend<O, C> {
    fn size_bytes(&self) -> u64 {
        self.size
    }

    fn block_size(&self) -> u32 {
        self.block
    }

    fn read_at(&mut self, offset: u64, destination: &mut [u8]) -> Result<(), IoError> {
        AuthoritativeOriginBackend::read_at(self, offset, destination)
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<(), IoError> {
        AuthoritativeOriginBackend::write_at(self, offset, data)
    }

    fn write_at_with_options(
        &mut self,
        offset: u64,
        data: &[u8],
        options: WriteOptions,
    ) -> Result<(), IoError> {
        AuthoritativeOriginBackend::write_at_with_options(self, offset, data, options)
    }

    fn flush(&mut self) -> Result<(), IoError> {
        AuthoritativeOriginBackend::flush(self)
    }
}

/// The shared serving path (SPEC `wsl2-nbd-serving-throughput` DT-3): the same
/// origin authority, callable from many threads.
impl<O: OriginStorage + Send + Sync, C: BestEffortCache + Send> crate::SharedBlockBackend
    for AuthoritativeOriginBackend<O, C>
{
    fn size_bytes(&self) -> u64 {
        self.size
    }

    fn block_size(&self) -> u32 {
        self.block
    }

    fn read_at(&self, offset: u64, destination: &mut [u8]) -> Result<(), IoError> {
        AuthoritativeOriginBackend::read_at(self, offset, destination)
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), IoError> {
        AuthoritativeOriginBackend::write_at(self, offset, data)
    }

    fn write_at_with_options(
        &self,
        offset: u64,
        data: &[u8],
        options: WriteOptions,
    ) -> Result<(), IoError> {
        AuthoritativeOriginBackend::write_at_with_options(self, offset, data, options)
    }

    fn flush(&self) -> Result<(), IoError> {
        AuthoritativeOriginBackend::flush(self)
    }
}

#[cfg(test)]
mod tests;
