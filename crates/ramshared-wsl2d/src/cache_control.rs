// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/cache_control.rs
//! A recovery transaction controls one cache member under the backend lock.
use super::{GpuAdapterIdentity, OriginCache, OriginCacheState};
use ramshared_block::{BestEffortCache, cache_group::CacheMemberControl};

pub(super) enum RecoveryCache<'a> {
    Direct(&'a mut OriginCache),
    Member(CacheMemberControl<'a, OriginCache>),
}
impl<'a> RecoveryCache<'a> {
    pub(super) fn select(cache: &'a mut OriginCache, index: usize) -> Option<Self> {
        match cache {
            OriginCache::Group(group) => group.control_member(index).map(Self::Member),
            other if index == 0 => Some(Self::Direct(other)),
            _ => None,
        }
    }
    fn client(&self) -> Option<&OriginCache> {
        match self {
            Self::Direct(cache) => Some(cache),
            Self::Member(member) => member.cache(),
        }
    }
    pub(super) fn cache_state(&self) -> OriginCacheState {
        self.client()
            .map_or(OriginCacheState::Unavailable, BestEffortCache::state)
    }
    pub(super) fn bound_adapter_identity(&self) -> Option<GpuAdapterIdentity> {
        self.client()?.bound_adapter_identity()
    }
    pub(super) fn replace_cache(&mut self, cache: OriginCache) {
        match self {
            Self::Direct(current) => **current = cache,
            Self::Member(member) => member.replace(cache),
        }
    }
}
