// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/cache_group/tests.rs
//! Explicit CPU reference caches; they do not simulate GPU admission authority.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

struct ReferenceCache {
    data: BTreeMap<u64, u8>,
    failed: Arc<AtomicBool>,
    state: CacheState,
    reported: Option<u64>,
}
impl ReferenceCache {
    fn new() -> Self {
        Self {
            data: BTreeMap::new(),
            failed: Arc::new(AtomicBool::new(false)),
            state: CacheState::Active,
            reported: None,
        }
    }
}
impl BestEffortCache for ReferenceCache {
    fn read(&mut self, offset: u64, destination: &mut [u8]) -> CacheRead {
        if self.failed.load(Ordering::Acquire) {
            return CacheRead::Failed;
        }
        if self.state != CacheState::Active {
            return CacheRead::Miss;
        }
        if !(0..destination.len()).all(|i| self.data.contains_key(&(offset + i as u64))) {
            return CacheRead::Miss;
        }
        for (i, byte) in destination.iter_mut().enumerate() {
            *byte = self.data[&(offset + i as u64)];
        }
        CacheRead::Hit
    }
    fn update(&mut self, offset: u64, data: &[u8]) -> CacheMutation {
        if self.failed.load(Ordering::Acquire) {
            return CacheMutation::Failed;
        }
        if self.state != CacheState::Active {
            return CacheMutation::Skipped;
        }
        for (i, byte) in data.iter().enumerate() {
            self.data.insert(offset + i as u64, *byte);
        }
        CacheMutation::Accepted
    }
    fn promote(&mut self, offset: u64, data: &[u8]) -> CacheMutation {
        self.update(offset, data)
    }
    fn disable(&mut self) -> CacheMutation {
        if self.failed.load(Ordering::Acquire) {
            self.state = CacheState::Stuck;
            return CacheMutation::Failed;
        }
        self.state = CacheState::Off;
        self.data.clear();
        CacheMutation::Accepted
    }
    fn state(&self) -> CacheState {
        self.state
    }
    fn cached_bytes(&self) -> u64 {
        self.reported.unwrap_or(self.data.len() as u64)
    }
    fn target_bytes(&self) -> u64 {
        4096
    }
}
impl BoundedCache for ReferenceCache {
    fn read_until(&mut self, offset: u64, destination: &mut [u8], deadline: Instant) -> CacheRead {
        if Instant::now() >= deadline {
            CacheRead::Miss
        } else {
            self.read(offset, destination)
        }
    }
    fn disable_until(&mut self, deadline: Instant) -> CacheMutation {
        if Instant::now() >= deadline {
            self.state = CacheState::Stuck;
            CacheMutation::Failed
        } else {
            self.disable()
        }
    }
    fn refresh_until(&mut self, deadline: Instant) -> Result<u64, &'static str> {
        if Instant::now() >= deadline {
            return Err("reference budget expired");
        }
        if self.failed.load(Ordering::Acquire) {
            self.state = CacheState::Unavailable;
            return Err("reference failure");
        }
        Ok(self.cached_bytes())
    }
}

fn group() -> CacheGroup<ReferenceCache> {
    CacheGroup::new(
        vec![
            CacheMember::new("b".into(), 4, ReferenceCache::new()),
            CacheMember::new("a".into(), 12, ReferenceCache::new()),
        ],
        4,
        Duration::from_millis(30),
        Duration::from_millis(30),
    )
    .unwrap()
}

#[test]
fn stable_weighted_routing_survives_member_failure_and_replacement() {
    let mut cache = group();
    let bytes: Vec<_> = (0..32).collect();
    assert_eq!(cache.promote(0, &bytes), CacheMutation::Accepted);
    assert_eq!(
        cache.control_member(0).unwrap().cache().unwrap().data.len(),
        24
    );
    assert_eq!(
        cache.control_member(1).unwrap().cache().unwrap().data.len(),
        8
    );
    let failed = Arc::clone(&cache.control_member(0).unwrap().cache().unwrap().failed);
    failed.store(true, Ordering::Release);
    assert_eq!(cache.read(0, &mut [0; 4]), CacheRead::Miss);
    assert_eq!(cache.state(), CacheState::Restricted);
    let mut healthy = [0; 4];
    assert_eq!(cache.read(12, &mut healthy), CacheRead::Hit);
    assert_eq!(healthy, [12, 13, 14, 15]);
    let status = cache.member_statuses();
    assert_eq!(status[0].key, "a");
    assert_eq!(status[0].failures, 1);
    assert_eq!(status[0].cached_bytes, None);
    assert_eq!(cache.read(0, &mut [0; 4]), CacheRead::Miss);
    cache
        .control_member(0)
        .unwrap()
        .replace(ReferenceCache::new());
    assert_eq!(cache.read(0, &mut [0; 4]), CacheRead::Miss);
    assert_eq!(cache.promote(0, &bytes), CacheMutation::Accepted);
    let mut actual = [0; 32];
    assert_eq!(cache.read(0, &mut actual), CacheRead::Hit);
    assert_eq!(actual.as_slice(), bytes);
    assert!(cache.gpu_budget_telemetry().is_none());
    assert!(cache.bound_adapter_identity().is_none());
}

#[test]
fn mutation_failure_does_not_revoke_or_remap_the_healthy_member() {
    let mut cache = group();
    cache
        .control_member(0)
        .unwrap()
        .cache()
        .unwrap()
        .failed
        .store(true, Ordering::Release);
    assert_eq!(cache.update(0, &[81; 32]), CacheMutation::Accepted);
    let mut actual = [0; 4];
    assert_eq!(cache.read(12, &mut actual), CacheRead::Hit);
    assert_eq!(actual, [81; 4]);
    assert_eq!(cache.read(0, &mut actual), CacheRead::Miss);
    assert_eq!(cache.state(), CacheState::Restricted);
}

#[test]
fn group_constructor_refuses_invalid_or_overflowed_topology() {
    for (keys, targets, chunk) in [
        (vec![], vec![], 4),
        (vec![""], vec![4], 4),
        (vec!["a", "a"], vec![4, 4], 4),
        (vec!["a"], vec![0], 4),
        (vec!["a"], vec![4], 0),
        (vec!["a", "b"], vec![u64::MAX, 1], 1),
    ] {
        let members = keys
            .into_iter()
            .zip(targets)
            .map(|(k, t)| CacheMember::new(k.into(), t, ReferenceCache::new()))
            .collect();
        assert!(
            CacheGroup::new(
                members,
                chunk,
                Duration::from_millis(30),
                Duration::from_millis(30)
            )
            .is_err()
        );
    }
    for (io, release) in [
        (Duration::ZERO, Duration::from_secs(1)),
        (Duration::MAX, Duration::from_secs(1)),
        (Duration::from_secs(1), Duration::ZERO),
        (Duration::from_secs(1), Duration::MAX),
    ] {
        assert!(
            CacheGroup::new(
                vec![CacheMember::new("a".into(), 4, ReferenceCache::new())],
                4,
                io,
                release
            )
            .is_err()
        );
    }
}

#[test]
fn single_member_keeps_full_range_and_owned_telemetry_behavior() {
    let mut cache = CacheGroup::new(
        vec![CacheMember::new("only".into(), 1, ReferenceCache::new())],
        u64::MAX,
        Duration::from_millis(30),
        Duration::from_millis(30),
    )
    .unwrap();
    assert_eq!(cache.update(0, &[7; 32]), CacheMutation::Accepted);
    assert_eq!(cache.read(0, &mut [0; 32]), CacheRead::Hit);
    assert_eq!(cache.refresh_cached_bytes(), Ok(32));
    assert_eq!(cache.cached_bytes(), 32);
    assert_eq!(cache.target_bytes(), 4096);
    assert!(cache.gpu_budget_telemetry().is_none());
    assert!(cache.gpu_utilization_telemetry().is_none());
    assert!(cache.cache_telemetry().is_none());
    assert!(cache.bound_adapter_identity().is_none());
    assert_eq!(cache.disable(), CacheMutation::Accepted);
    assert_eq!(cache.state(), CacheState::Off);
    assert_eq!(cache.member_statuses()[0].cached_bytes, Some(0));
}

#[test]
fn expired_budget_preserves_uncontacted_data_but_refuses_release() {
    let mut cache = group();
    cache.promote(0, &[5; 32]);
    let expired = Instant::now();
    assert_eq!(cache.read_until(0, &mut [0; 4], expired), CacheRead::Miss);
    assert_eq!(cache.state(), CacheState::Active);
    assert!(cache.refresh_until(expired).is_err());
    assert_eq!(cache.state(), CacheState::Active);
    assert_eq!(cache.disable_until(expired), CacheMutation::Failed);
    assert_eq!(cache.state(), CacheState::Stuck);
}

#[test]
fn malformed_ranges_never_reach_a_member() {
    let mut cache = group();
    assert_eq!(cache.read(u64::MAX, &mut [0; 2]), CacheRead::Miss);
    assert_eq!(cache.update(u64::MAX, &[0; 2]), CacheMutation::Failed);
    assert_eq!(cache.read(0, &mut []), CacheRead::Hit);
    assert_eq!(cache.update(0, &[]), CacheMutation::Skipped);
    assert!(cache.control_member(100).is_none());
}

#[test]
fn refresh_failure_retains_the_healthy_members_measurement() {
    let mut cache = group();
    cache.promote(0, &[6; 32]);
    cache
        .control_member(0)
        .unwrap()
        .cache()
        .unwrap()
        .failed
        .store(true, Ordering::Release);
    assert!(cache.refresh_cached_bytes().is_err());
    let statuses = cache.member_statuses();
    assert_eq!(statuses[0].state, CacheState::Unavailable);
    assert_eq!(statuses[0].cached_bytes, None);
    assert_eq!(statuses[1].cached_bytes, Some(8));
    assert_eq!(cache.state(), CacheState::Restricted);
    assert!(cache.refresh_cached_bytes().is_err());
    assert_eq!(cache.disable(), CacheMutation::Accepted);
    assert_eq!(
        cache.state(),
        CacheState::Unavailable,
        "an unknown member became confirmed Off"
    );
}

#[test]
fn one_failed_release_does_not_report_the_whole_group_as_released() {
    let mut cache = group();
    cache
        .control_member(0)
        .unwrap()
        .cache()
        .unwrap()
        .failed
        .store(true, Ordering::Release);
    assert_eq!(cache.disable(), CacheMutation::Failed);
    let statuses = cache.member_statuses();
    assert_eq!(statuses[0].state, CacheState::Stuck);
    assert_eq!(statuses[0].cached_bytes, None);
    assert_eq!(statuses[1].state, CacheState::Off);
    assert_eq!(statuses[1].cached_bytes, Some(0));
    assert_eq!(cache.state(), CacheState::Stuck);
}

#[test]
fn aggregate_overflow_is_unavailable_instead_of_a_measured_zero() {
    let mut cache = CacheGroup::new(
        vec![
            CacheMember::new(
                "a".into(),
                4,
                ReferenceCache {
                    reported: Some(u64::MAX),
                    ..ReferenceCache::new()
                },
            ),
            CacheMember::new(
                "b".into(),
                4,
                ReferenceCache {
                    reported: Some(1),
                    ..ReferenceCache::new()
                },
            ),
        ],
        4,
        Duration::from_millis(30),
        Duration::from_millis(30),
    )
    .unwrap();
    assert_eq!(cache.cached_bytes(), u64::MAX);
    assert!(cache.refresh_cached_bytes().is_err());
    assert!(
        cache
            .member_statuses()
            .iter()
            .all(|member| member.cached_bytes.is_some())
    );
    assert_eq!(cache.state(), CacheState::Active);
}

#[test]
fn single_member_failure_preserves_origin_fault_accounting() {
    for mutation in [true, false] {
        let leaf = ReferenceCache::new();
        leaf.failed.store(true, Ordering::Release);
        let mut cache = CacheGroup::new(
            vec![CacheMember::new("one".into(), 4, leaf)],
            4,
            Duration::from_millis(30),
            Duration::from_millis(30),
        )
        .unwrap();
        if mutation {
            assert_eq!(cache.update(0, &[1; 4]), CacheMutation::Failed);
        } else {
            assert_eq!(cache.read(0, &mut [0; 4]), CacheRead::Failed);
        }
        assert_eq!(cache.state(), CacheState::Unavailable);
        assert_eq!(cache.member_statuses()[0].failures, 1);
    }
}

#[cfg(unix)]
mod ipc_journey;

#[test]
fn lifecycle_comparison_ignores_samples_but_detects_every_member() {
    let mut cache = group();
    let original = cache.member_statuses();
    assert!(cache.member_lifecycle_matches(&original));
    assert!(!cache.member_lifecycle_matches(&original[..1]));
    let mut altered = original.clone();
    altered[1].key = "other".into();
    assert!(!cache.member_lifecycle_matches(&altered));
    let mut altered = original.clone();
    altered[1].requested_target_bytes += 1;
    assert!(!cache.member_lifecycle_matches(&altered));
    let mut altered = original.clone();
    altered[1].failures += 1;
    assert!(!cache.member_lifecycle_matches(&altered));
    let mut altered = original.clone();
    altered[1].cached_bytes = Some(123);
    assert!(cache.member_lifecycle_matches(&altered));
    cache.members[1].cache.as_mut().unwrap().state = CacheState::Off;
    assert!(!cache.member_lifecycle_matches(&original));
    let latest = cache.member_statuses();
    assert!(cache.member_lifecycle_matches(&latest));
    cache.fail_member(1);
    assert!(!cache.member_lifecycle_matches(&latest));
}
