// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/origin_cache_client.rs
//! Delegation and revocable startup intent for the authoritative origin cache.
use super::{
    BestEffortCache, CacheMutation, CacheRead, DisabledCache, GpuAdapterIdentity, IpcCacheClient,
    OriginCacheState,
};
use ramshared_block::{BoundedCache, cache_group::CacheGroup};
use ramshared_vram::{GpuBudgetTelemetry, GpuUtilizationTelemetry, WorkerCacheTelemetry};
use std::time::Instant;

pub(super) enum OriginCache {
    // Keep transport and telemetry storage on the heap. The disabled and
    // pending variants carry no allocated cache client.
    Ipc(Box<IpcCacheClient>),
    Group(Box<CacheGroup<OriginCache>>),
    Disabled(DisabledCache),
    /// A startup intent can be revoked before any worker is handed off.
    Starting {
        released: bool,
    },
}

impl OriginCache {
    /// Capture only serving or not-yet-released starting members. The caller
    /// holds the backend cache lock across observation and revocation.
    pub(super) fn pressure_candidates(&self) -> Vec<(usize, bool)> {
        fn eligible(cache: &OriginCache) -> Option<bool> {
            match cache {
                OriginCache::Starting { released: false } => Some(true),
                _ if matches!(
                    cache.state(),
                    OriginCacheState::Active | OriginCacheState::Restricted
                ) =>
                {
                    Some(false)
                }
                _ => None,
            }
        }
        match self {
            Self::Group(group) => group
                .member_caches()
                .enumerate()
                .filter_map(|(index, cache)| eligible(cache?).map(|starting| (index, starting)))
                .collect(),
            cache => eligible(cache)
                .map(|starting| vec![(0, starting)])
                .unwrap_or_default(),
        }
    }
    pub(super) fn pressure_off_members(&self) -> Vec<usize> {
        match self {
            Self::Group(group) => group
                .member_caches()
                .enumerate()
                .filter_map(|(index, cache)| {
                    (cache?.state() == OriginCacheState::Off).then_some(index)
                })
                .collect(),
            cache if cache.state() == OriginCacheState::Off => vec![0],
            _ => Vec::new(),
        }
    }
    pub(super) fn member_lifecycle_matches(
        &self,
        observed: Option<&[ramshared_block::cache_group::CacheMemberStatus]>,
    ) -> bool {
        match (self, observed) {
            (Self::Group(group), Some(prior)) => group.member_lifecycle_matches(prior),
            (Self::Group(_), None) | (_, Some(_)) => false,
            (_, None) => true,
        }
    }
    pub(super) fn member_statuses(
        &self,
    ) -> Option<Vec<ramshared_block::cache_group::CacheMemberStatus>> {
        match self {
            Self::Group(group) => Some(group.member_statuses()),
            _ => None,
        }
    }
}

impl BestEffortCache for OriginCache {
    fn read(&mut self, offset: u64, destination: &mut [u8]) -> CacheRead {
        match self {
            Self::Group(group) => group.read(offset, destination),
            Self::Ipc(c) => c.read(offset, destination),
            Self::Disabled(c) => c.read(offset, destination),
            Self::Starting { .. } => CacheRead::Miss,
        }
    }

    fn update(&mut self, offset: u64, data: &[u8]) -> CacheMutation {
        match self {
            Self::Group(group) => group.update(offset, data),
            Self::Ipc(c) => c.update(offset, data),
            Self::Disabled(c) => c.update(offset, data),
            Self::Starting { .. } => CacheMutation::Skipped,
        }
    }

    fn promote(&mut self, offset: u64, data: &[u8]) -> CacheMutation {
        match self {
            Self::Group(group) => group.promote(offset, data),
            Self::Ipc(c) => c.promote(offset, data),
            Self::Disabled(c) => c.promote(offset, data),
            Self::Starting { .. } => CacheMutation::Skipped,
        }
    }

    fn disable(&mut self) -> CacheMutation {
        match self {
            Self::Group(group) => group.disable(),
            Self::Ipc(c) => c.disable(),
            Self::Disabled(c) => c.disable(),
            Self::Starting { released } => {
                *released = true;
                CacheMutation::Skipped
            }
        }
    }

    fn state(&self) -> OriginCacheState {
        match self {
            Self::Group(group) => group.state(),
            Self::Ipc(c) => c.state(),
            Self::Disabled(c) => c.state(),
            Self::Starting { released } => {
                if *released {
                    OriginCacheState::Off
                } else {
                    OriginCacheState::Unavailable
                }
            }
        }
    }

    fn cached_bytes(&self) -> u64 {
        match self {
            Self::Group(group) => group.cached_bytes(),
            Self::Ipc(c) => c.cached_bytes(),
            Self::Disabled(c) => c.cached_bytes(),
            Self::Starting { .. } => 0,
        }
    }

    fn refresh_cached_bytes(&mut self) -> Result<u64, &'static str> {
        match self {
            Self::Group(group) => group.refresh_cached_bytes(),
            Self::Ipc(c) => c.refresh_cached_bytes(),
            Self::Disabled(c) => c.refresh_cached_bytes(),
            Self::Starting { .. } => Ok(0),
        }
    }

    fn target_bytes(&self) -> u64 {
        match self {
            Self::Group(group) => group.target_bytes(),
            Self::Ipc(c) => c.target_bytes(),
            Self::Disabled(c) => c.target_bytes(),
            Self::Starting { .. } => 0,
        }
    }

    fn bound_adapter_identity(&self) -> Option<GpuAdapterIdentity> {
        match self {
            Self::Group(group) => group.bound_adapter_identity(),
            Self::Ipc(cache) => cache.bound_adapter_identity().cloned(),
            Self::Disabled(cache) => cache.bound_adapter_identity(),
            Self::Starting { .. } => None,
        }
    }

    fn gpu_budget_telemetry(&self) -> Option<GpuBudgetTelemetry> {
        match self {
            Self::Group(group) => group.gpu_budget_telemetry(),
            Self::Ipc(c) => c.gpu_budget_telemetry().cloned(),
            Self::Disabled(c) => c.gpu_budget_telemetry(),
            Self::Starting { .. } => None,
        }
    }

    fn gpu_utilization_telemetry(&self) -> Option<GpuUtilizationTelemetry> {
        match self {
            Self::Group(group) => group.gpu_utilization_telemetry(),
            Self::Ipc(c) => c.gpu_utilization_telemetry().cloned(),
            Self::Disabled(c) => c.gpu_utilization_telemetry(),
            Self::Starting { .. } => None,
        }
    }

    fn cache_telemetry(&self) -> Option<WorkerCacheTelemetry> {
        match self {
            Self::Group(group) => group.cache_telemetry(),
            Self::Ipc(c) => c.cache_telemetry().cloned(),
            Self::Disabled(c) => c.cache_telemetry(),
            Self::Starting { .. } => None,
        }
    }
}

impl BoundedCache for OriginCache {
    fn read_until(&mut self, offset: u64, destination: &mut [u8], deadline: Instant) -> CacheRead {
        match self {
            Self::Ipc(client) => client.read_until(offset, destination, deadline),
            Self::Group(group) => group.read_until(offset, destination, deadline),
            _ => self.read(offset, destination),
        }
    }
    fn disable_until(&mut self, deadline: Instant) -> CacheMutation {
        match self {
            Self::Ipc(client) => client.disable_until(deadline),
            Self::Group(group) => group.disable_until(deadline),
            _ => self.disable(),
        }
    }
    fn refresh_until(&mut self, deadline: Instant) -> Result<u64, &'static str> {
        match self {
            Self::Ipc(client) => client.refresh_until(deadline),
            Self::Group(group) => group.refresh_until(deadline),
            _ => self.refresh_cached_bytes(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::unwrap_used)]
    fn publication_membership_shape_must_match_the_cache_owner() {
        let direct = OriginCache::Disabled(DisabledCache);
        assert!(direct.member_lifecycle_matches(None));
        assert!(!direct.member_lifecycle_matches(Some(&[])));
        let group = ramshared_block::cache_group::CacheGroup::new(
            vec![ramshared_block::cache_group::CacheMember::new(
                "member".into(),
                4096,
                direct,
            )],
            4096,
            std::time::Duration::from_millis(50),
            std::time::Duration::from_millis(50),
        )
        .unwrap();
        let cache = OriginCache::Group(Box::new(group));
        let prior = cache.member_statuses().unwrap();
        assert!(!cache.member_lifecycle_matches(None));
        assert!(cache.member_lifecycle_matches(Some(&prior)));
    }

    #[test]
    fn startup_intent_is_empty_and_explicit_release_remains_off() {
        let mut client = OriginCache::Starting { released: false };
        assert_eq!(client.state(), OriginCacheState::Unavailable);
        assert_eq!(client.read(0, &mut [0; 4]), CacheRead::Miss);
        assert_eq!(client.update(0, &[1; 4]), CacheMutation::Skipped);
        assert_eq!(client.promote(0, &[1; 4]), CacheMutation::Skipped);
        assert_eq!(client.cached_bytes(), 0);
        assert_eq!(client.refresh_cached_bytes(), Ok(0));
        assert_eq!(client.target_bytes(), 0);
        assert!(client.bound_adapter_identity().is_none());
        assert!(client.gpu_budget_telemetry().is_none());
        assert!(client.gpu_utilization_telemetry().is_none());
        assert!(client.cache_telemetry().is_none());
        assert_eq!(client.disable(), CacheMutation::Skipped);
        assert_eq!(client.state(), OriginCacheState::Off);
        assert_eq!(client.disable(), CacheMutation::Skipped);
        assert_eq!(client.state(), OriginCacheState::Off);
    }
}
