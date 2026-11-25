// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/cache_group.rs
//! Stable cache-chunk routing with one deadline and independent peer failure.
use crate::isolated_origin::MAX_CACHE_MUTATION_BYTES;
use crate::{BestEffortCache, BoundedCache, CacheMutation, CacheRead, CacheState};
use ramshared_vram::{
    GpuAdapterIdentity, GpuBudgetTelemetry, GpuUtilizationTelemetry, WorkerCacheTelemetry,
};
use std::time::{Duration, Instant};
mod routing;
use routing::Routing;

/// Maximum encoded public cache-status document, including member records.
pub const MAX_CACHE_STATUS_BYTES: usize = 64 * 1024;

pub struct CacheMember<C> {
    key: String,
    target_bytes: u64,
    cache: Option<C>,
    last_cached: Option<u64>,
    failures: u64,
}
impl<C> CacheMember<C> {
    /// Key and target define routing intent, never allocation authority.
    pub fn new(key: String, target_bytes: u64, cache: C) -> Self {
        Self {
            key,
            target_bytes,
            cache: Some(cache),
            last_cached: None,
            failures: 0,
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheMemberStatus {
    pub key: String,
    pub state: CacheState,
    pub requested_target_bytes: u64,
    pub cached_bytes: Option<u64>,
    pub failures: u64,
    pub adapter: Option<GpuAdapterIdentity>,
    pub budget: Option<GpuBudgetTelemetry>,
    pub utilization: Option<GpuUtilizationTelemetry>,
    pub cache: Option<WorkerCacheTelemetry>,
}

pub struct CacheMemberControl<'a, C> {
    member: &'a mut CacheMember<C>,
}
impl<C> CacheMemberControl<'_, C> {
    pub fn cache(&self) -> Option<&C> {
        self.member.cache.as_ref()
    }
    pub fn replace(&mut self, cache: C) {
        self.member.last_cached = None;
        self.member.cache = Some(cache);
    }
}

pub struct CacheGroup<C> {
    members: Vec<CacheMember<C>>,
    routing: Routing,
    io_timeout: Duration,
    release_timeout: Duration,
    next_refresh: usize,
}
impl<C: BoundedCache> CacheGroup<C> {
    pub fn new(
        mut members: Vec<CacheMember<C>>,
        chunk_bytes: u64,
        io_timeout: Duration,
        release_timeout: Duration,
    ) -> Result<Self, &'static str> {
        if io_timeout.is_zero()
            || release_timeout.is_zero()
            || Instant::now().checked_add(io_timeout).is_none()
            || Instant::now().checked_add(release_timeout).is_none()
        {
            return Err("invalid cache group operation budget");
        }
        members.sort_by(|a, b| a.key.cmp(&b.key));
        if members.iter().any(|member| member.key.is_empty())
            || members.windows(2).any(|pair| pair[0].key == pair[1].key)
        {
            return Err("cache group requires unique nonempty member keys");
        }
        let targets: Vec<_> = members.iter().map(|member| member.target_bytes).collect();
        let routing = Routing::new(&targets, chunk_bytes)?;
        Ok(Self {
            members,
            routing,
            io_timeout,
            release_timeout,
            next_refresh: 0,
        })
    }
    /// Replacing a client preserves its immutable routing assignment.
    pub fn control_member(&mut self, index: usize) -> Option<CacheMemberControl<'_, C>> {
        self.members
            .get_mut(index)
            .map(|member| CacheMemberControl { member })
    }
    /// Observe members in their stable routing order without transferring control.
    pub fn member_caches(&self) -> impl Iterator<Item = Option<&C>> {
        self.members.iter().map(|member| member.cache.as_ref())
    }
    pub fn member_statuses(&self) -> Vec<CacheMemberStatus> {
        self.members
            .iter()
            .map(|member| CacheMemberStatus {
                key: member.key.clone(),
                state: member
                    .cache
                    .as_ref()
                    .map_or(CacheState::Unavailable, BestEffortCache::state),
                requested_target_bytes: member.target_bytes,
                cached_bytes: member.last_cached,
                failures: member.failures,
                adapter: member
                    .cache
                    .as_ref()
                    .and_then(BestEffortCache::bound_adapter_identity),
                budget: member
                    .cache
                    .as_ref()
                    .and_then(BestEffortCache::gpu_budget_telemetry),
                utilization: member
                    .cache
                    .as_ref()
                    .and_then(BestEffortCache::gpu_utilization_telemetry),
                cache: member
                    .cache
                    .as_ref()
                    .and_then(BestEffortCache::cache_telemetry),
            })
            .collect()
    }
    /// Compare lifecycle fields only; do not clone or refresh telemetry on a job.
    pub fn member_lifecycle_matches(&self, observed: &[CacheMemberStatus]) -> bool {
        self.members.len() == observed.len()
            && self.members.iter().zip(observed).all(|(member, prior)| {
                member.key == prior.key
                    && member.target_bytes == prior.requested_target_bytes
                    && member.failures == prior.failures
                    && member
                        .cache
                        .as_ref()
                        .map_or(CacheState::Unavailable, BestEffortCache::state)
                        == prior.state
            })
    }
    fn one(&self) -> Option<&C> {
        (self.members.len() == 1)
            .then(|| self.members[0].cache.as_ref())
            .flatten()
    }
    fn fail_member(&mut self, index: usize) {
        let member = &mut self.members[index];
        member.failures = member.failures.saturating_add(1);
        member.last_cached = None;
        // Native process custody stays with the caller. Closing this stream
        // revokes data access; it is not proof of physical memory release.
        drop(member.cache.take());
    }
    fn mutate(&mut self, offset: u64, data: &[u8], promote: bool) -> CacheMutation {
        if offset.checked_add(data.len() as u64).is_none() {
            return CacheMutation::Failed;
        }
        let (mut done, mut accepted, mut failed) = (0, false, false);
        while done < data.len() {
            let at = offset + done as u64;
            let (index, span) = self.routing.locate(at, data.len() - done);
            let span = span.min(MAX_CACHE_MUTATION_BYTES);
            if let Some(cache) = self.members[index].cache.as_mut() {
                let result = if promote {
                    cache.promote(at, &data[done..done + span])
                } else {
                    cache.update(at, &data[done..done + span])
                };
                match result {
                    CacheMutation::Accepted => accepted = true,
                    CacheMutation::Failed => {
                        failed = true;
                        self.fail_member(index);
                    }
                    CacheMutation::Skipped => {}
                }
            }
            done += span;
        }
        // The one-member product path preserves the origin's existing fault
        // counters. A local fault in a set must not revoke healthy peers.
        if failed && self.members.len() == 1 {
            CacheMutation::Failed
        } else if accepted {
            CacheMutation::Accepted
        } else {
            CacheMutation::Skipped
        }
    }
}

impl<C: BoundedCache> BoundedCache for CacheGroup<C> {
    fn read_until(&mut self, offset: u64, destination: &mut [u8], deadline: Instant) -> CacheRead {
        if offset.checked_add(destination.len() as u64).is_none() {
            return CacheRead::Miss;
        }
        let Some(local) = Instant::now().checked_add(self.io_timeout) else {
            return CacheRead::Miss;
        };
        let deadline = deadline.min(local);
        let mut done = 0;
        while done < destination.len() {
            if Instant::now() >= deadline {
                return CacheRead::Miss;
            }
            let at = offset + done as u64;
            let (index, span) = self.routing.locate(at, destination.len() - done);
            let Some(cache) = self.members[index].cache.as_mut() else {
                return CacheRead::Miss;
            };
            match cache.read_until(at, &mut destination[done..done + span], deadline) {
                CacheRead::Hit => done += span,
                CacheRead::Miss => return CacheRead::Miss,
                CacheRead::Failed => {
                    self.fail_member(index);
                    return if self.members.len() == 1 {
                        CacheRead::Failed
                    } else {
                        CacheRead::Miss
                    };
                }
            }
        }
        CacheRead::Hit
    }
    fn disable_until(&mut self, deadline: Instant) -> CacheMutation {
        let Some(local) = Instant::now().checked_add(self.release_timeout) else {
            return CacheMutation::Failed;
        };
        let deadline = deadline.min(local);
        let mut outcome = CacheMutation::Skipped;
        for member in &mut self.members {
            if let Some(cache) = member.cache.as_mut() {
                match cache.disable_until(deadline) {
                    CacheMutation::Failed => outcome = CacheMutation::Failed,
                    CacheMutation::Accepted if outcome != CacheMutation::Failed => {
                        outcome = CacheMutation::Accepted
                    }
                    _ => {}
                }
                member.last_cached = (cache.state() == CacheState::Off).then_some(0);
            }
        }
        outcome
    }
    fn refresh_until(&mut self, deadline: Instant) -> Result<u64, &'static str> {
        let local = Instant::now()
            .checked_add(self.io_timeout)
            .ok_or("cache group deadline overflow")?;
        let deadline = deadline.min(local);
        let start = self.next_refresh;
        self.next_refresh = (start + 1) % self.members.len();
        let (mut total, mut error) = (0u64, None);
        for step in 0..self.members.len() {
            let index = (start + step) % self.members.len();
            let Some(cache) = self.members[index].cache.as_mut() else {
                error = Some("cache member is unavailable");
                continue;
            };
            match cache.refresh_until(deadline) {
                Ok(bytes) => {
                    self.members[index].last_cached = Some(bytes);
                    match total.checked_add(bytes) {
                        Some(sum) => total = sum,
                        None => error = Some("cache group occupancy overflow"),
                    }
                }
                Err(reason) => {
                    let failed =
                        matches!(cache.state(), CacheState::Unavailable | CacheState::Stuck);
                    self.members[index].last_cached = None;
                    if failed {
                        self.fail_member(index);
                    }
                    error = Some(reason);
                }
            }
        }
        error.map_or(Ok(total), Err)
    }
}

impl<C: BoundedCache> BestEffortCache for CacheGroup<C> {
    fn read(&mut self, offset: u64, destination: &mut [u8]) -> CacheRead {
        let Some(deadline) = Instant::now().checked_add(self.io_timeout) else {
            return CacheRead::Miss;
        };
        self.read_until(offset, destination, deadline)
    }
    fn update(&mut self, offset: u64, data: &[u8]) -> CacheMutation {
        self.mutate(offset, data, false)
    }
    fn promote(&mut self, offset: u64, data: &[u8]) -> CacheMutation {
        self.mutate(offset, data, true)
    }
    fn disable(&mut self) -> CacheMutation {
        let Some(deadline) = Instant::now().checked_add(self.release_timeout) else {
            return CacheMutation::Failed;
        };
        self.disable_until(deadline)
    }
    fn state(&self) -> CacheState {
        let states = || {
            self.members.iter().map(|member| {
                member
                    .cache
                    .as_ref()
                    .map_or(CacheState::Unavailable, BestEffortCache::state)
            })
        };
        if states().all(|state| state == CacheState::Active) {
            CacheState::Active
        } else if states().any(|state| matches!(state, CacheState::Active | CacheState::Restricted))
        {
            CacheState::Restricted
        } else if states().any(|state| state == CacheState::Stuck) {
            CacheState::Stuck
        } else if states().all(|state| state == CacheState::Off) {
            CacheState::Off
        } else {
            CacheState::Unavailable
        }
    }
    fn cached_bytes(&self) -> u64 {
        self.members
            .iter()
            .filter_map(|member| member.cache.as_ref())
            .fold(0u64, |sum, cache| sum.saturating_add(cache.cached_bytes()))
    }
    fn target_bytes(&self) -> u64 {
        self.members
            .iter()
            .filter_map(|member| member.cache.as_ref())
            .fold(0u64, |sum, cache| sum.saturating_add(cache.target_bytes()))
    }
    fn refresh_cached_bytes(&mut self) -> Result<u64, &'static str> {
        let deadline = Instant::now()
            .checked_add(self.io_timeout)
            .ok_or("cache group deadline overflow")?;
        self.refresh_until(deadline)
    }
    fn bound_adapter_identity(&self) -> Option<GpuAdapterIdentity> {
        self.one()?.bound_adapter_identity()
    }
    fn gpu_budget_telemetry(&self) -> Option<GpuBudgetTelemetry> {
        self.one()?.gpu_budget_telemetry()
    }
    fn gpu_utilization_telemetry(&self) -> Option<GpuUtilizationTelemetry> {
        self.one()?.gpu_utilization_telemetry()
    }
    fn cache_telemetry(&self) -> Option<WorkerCacheTelemetry> {
        self.one()?.cache_telemetry()
    }
}

#[cfg(test)]
mod tests;
