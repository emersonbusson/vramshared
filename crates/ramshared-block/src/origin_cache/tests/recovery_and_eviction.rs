// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/origin_cache/tests/recovery_and_eviction.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]
use super::super::*;
use super::{FakeProvider, ScriptedOrigin, backend, grow_one, healthy_sample, runtime, sample_at};
use ramshared_vram::{GpuBudgetSnapshot, GpuBudgetSource};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

/// DT-6: `external_usage_bytes` maps to `used_bytes`.
#[test]
fn origin_sample_maps_external_usage_to_used_bytes() {
    let t0 = Instant::now();
    let sample = sample_at(t0, 10 * GIB, 3 * GIB, 20 * GIB);
    let snapshot = GpuBudgetSnapshot {
        adapter: sample.adapter.clone(),
        total_bytes: Some(sample.total_vram_bytes),
        budget_bytes: sample.budget_bytes,
        used_bytes: sample.external_usage_bytes,
        source: sample.source,
        sampled_at: sample.sampled_at,
    };
    assert_eq!(snapshot.used_bytes, 3 * GIB);
    assert_eq!(
        physical_target_bytes(24 * GIB, Some(&sample), 0, runtime(), t0),
        snapshot.safe_target_bytes(24 * GIB, 0, runtime())
    );
}

/// DT-6: an unnormalizable sample yields zero, not a partial target.
#[test]
fn origin_unnormalizable_sample_returns_zero() {
    let t0 = Instant::now();
    // External use above the budget cannot be normalized.
    let over = sample_at(t0, 4 * GIB, 8 * GIB, 16 * GIB);
    assert_eq!(
        physical_target_bytes(4 * GIB, Some(&over), 0, runtime(), t0),
        0
    );
    // A missing sample is the same refusal.
    assert_eq!(physical_target_bytes(4 * GIB, None, 0, runtime(), t0), 0);
}

/// DT-6: the path never stamps provenance. `physical_target_bytes` takes
/// the clock as an argument; it does not call `Instant::now()` and does
/// not write `GpuBudgetSource::DriverReported`.
#[test]
fn origin_path_never_stamps_provenance() {
    let t0 = Instant::now();
    // A sample stamped in the future is refused: the function trusts the
    // sample's own `sampled_at`, it does not replace it with a fresh one.
    let mut future = sample_at(t0, 4 * GIB, 0, 8 * GIB);
    future.sampled_at = t0 + Duration::from_secs(60);
    assert_eq!(
        physical_target_bytes(4 * GIB, Some(&future), 0, runtime(), t0),
        0
    );
    // A non-driver source is never normalized to DriverReported.
    let mut lab = sample_at(t0, 4 * GIB, 0, 8 * GIB);
    lab.source = GpuBudgetSource::ProviderLocalEstimate;
    assert_eq!(
        physical_target_bytes(4 * GIB, Some(&lab), 0, runtime(), t0),
        0
    );
}

#[test]
fn origin_stale_sample_refuses() {
    let t0 = Instant::now();
    let mut stale = sample_at(t0, 4 * GIB, 0, 8 * GIB);
    stale.sampled_at = t0 - (SAMPLE_MAX_AGE + Duration::from_secs(1));
    // A stale sample must yield no target at all, not a reduced one.
    assert_eq!(
        physical_target_bytes(4 * GIB, Some(&stale), 0, runtime(), t0),
        0
    );
    // The same sample inside the window is admitted. The value is the
    // shared formula, not the requested size: capacity = min(8G, 4G) =
    // 4G; reserve = ceil(4G/5); within_live_headroom = 4G - reserve -
    // runtime. Both terms bind below the requested 4G.
    let fresh = sample_at(t0, 4 * GIB, 0, 8 * GIB);
    let admitted = physical_target_bytes(4 * GIB, Some(&fresh), 0, runtime(), t0);
    assert!(admitted > 0, "fresh sample must be admitted");
    assert!(admitted < 4 * GIB, "reserve and runtime headroom must bind");
    let snapshot = GpuBudgetSnapshot {
        adapter: fresh.adapter.clone(),
        total_bytes: Some(fresh.total_vram_bytes),
        budget_bytes: fresh.budget_bytes,
        used_bytes: fresh.external_usage_bytes,
        source: fresh.source,
        sampled_at: fresh.sampled_at,
    };
    assert_eq!(admitted, snapshot.safe_target_bytes(4 * GIB, 0, runtime()));
}

#[test]
fn origin_untrusted_sample_refuses() {
    let t0 = Instant::now();
    // Source other than DriverReported.
    let mut lab = sample_at(t0, 4 * GIB, 0, 8 * GIB);
    lab.source = GpuBudgetSource::ProviderLocalEstimate;
    assert_eq!(
        physical_target_bytes(4 * GIB, Some(&lab), 0, runtime(), t0),
        0
    );
    // Missing adapter identity.
    let mut anonymous = sample_at(t0, 4 * GIB, 0, 8 * GIB);
    anonymous.adapter = None;
    assert_eq!(
        physical_target_bytes(4 * GIB, Some(&anonymous), 0, runtime(), t0),
        0
    );
}

#[test]
fn physical_target_bytes_refuses_inconsistent_sample() {
    let t0 = Instant::now();
    // External use above the budget.
    let over_used = sample_at(t0, 4 * GIB, 8 * GIB, 16 * GIB);
    assert_eq!(
        physical_target_bytes(4 * GIB, Some(&over_used), 0, runtime(), t0),
        0
    );
    // Budget above the physical total.
    let over_budget = sample_at(t0, 16 * GIB, 0, 8 * GIB);
    assert_eq!(
        physical_target_bytes(4 * GIB, Some(&over_budget), 0, runtime(), t0),
        0
    );
}

#[test]
fn exact_target_formula_and_missing_measurement_fail_safe() {
    let t0 = Instant::now();
    assert_eq!(physical_target_bytes(4 * GIB, None, 0, runtime(), t0), 0);
    // capacity = min(total 20G, budget 10G) = 10G.
    // reserve = max(0, ceil(10G/5)) = 2G   (no 2 GiB constant).
    // available = 8G. Retaining the 2G reserve and runtime gives 6G-runtime.
    let ten = sample_at(t0, 10 * GIB, 2 * GIB, 20 * GIB);
    let expected = 6 * GIB - runtime();
    assert_eq!(
        physical_target_bytes(24 * GIB, Some(&ten), 0, runtime(), t0),
        expected
    );
    // Reserve derives from the effective budget, not the uncapped total.
    assert!(expected > 4 * GIB);
    // Logical size still binds.
    let eight = sample_at(t0, 8 * GIB, 0, 8 * GIB);
    assert_eq!(
        physical_target_bytes(GIB, Some(&eight), 0, runtime(), t0),
        GIB
    );
}

#[test]
fn cache_io_failures_invalidate_and_fall_back_without_eio() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let provider = FakeProvider::new(Rc::clone(&events));
    let origin = ScriptedOrigin::new(32, Rc::clone(&events));
    let mut backend = backend(&provider, origin);
    grow_one(&mut backend);
    backend.write_at(0, b"data").unwrap();
    provider.fail_read.set(true);

    let mut read_back = [0; 4];
    backend.read_at(0, &mut read_back).unwrap();

    assert_eq!(&read_back, b"data");
    assert_eq!(backend.telemetry().cache_read_failures, 1);
    assert!(backend.telemetry().invalidations >= 1);

    provider.fail_read.set(false);
    provider.fail_write.set(true);
    backend.write_at(0, b"next").unwrap();
    assert_eq!(backend.telemetry().cache_write_failures, 1);
    let mut after_write_failure = [0; 4];
    backend.read_at(0, &mut after_write_failure).unwrap();
    assert_eq!(&after_write_failure, b"next");
}

#[test]
fn restricted_reclaim_releases_least_recent_clean_chunk() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let provider = FakeProvider::new(Rc::clone(&events));
    let origin = ScriptedOrigin::new(32, Rc::clone(&events));
    let mut backend = backend(&provider, origin);
    grow_one(&mut backend);
    assert_eq!(
        backend
            .observe_gpu(
                Some(&healthy_sample()),
                Duration::from_secs(4),
                Instant::now()
            )
            .allocated_bytes,
        8
    );
    backend.write_at(0, b"zero").unwrap();
    backend.write_at(8, b"one!").unwrap();
    let mut make_first_recent = [0; 4];
    backend.read_at(0, &mut make_first_recent).unwrap();

    // Isolate LRU reclaim from the reserve formula: bind the reserve so
    // `safe_target_bytes` returns exactly one 8-byte chunk. capacity =
    // min(total 8G, budget 2G+8) = 2G+8; reserve = 2G; within_capacity = 8;
    // within_live_headroom = (2G+8) - 2G - 0 = 8. The configured reserve is
    // doing the work here, not any hardcoded floor.
    backend.set_reserve_floor(2 * GIB, 0);
    let one_chunk_target = sample_at(Instant::now(), 2 * GIB + 8, 0, 8 * GIB);
    backend.observe_gpu(
        Some(&one_chunk_target),
        Duration::from_secs(5),
        Instant::now(),
    );
    backend.observe_gpu(
        Some(&one_chunk_target),
        Duration::from_secs(6),
        Instant::now(),
    );
    let outcome = backend.observe_gpu(
        Some(&one_chunk_target),
        Duration::from_secs(7),
        Instant::now(),
    );
    assert_eq!(outcome.released_bytes, 8);

    events.borrow_mut().clear();
    backend.read_at(0, &mut [0; 4]).unwrap();
    backend.read_at(8, &mut [0; 4]).unwrap();
    assert_eq!(events.borrow().as_slice(), ["cache_read", "origin_read"]);
}

#[test]
fn reallocated_chunk_requires_current_generation_validity() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let provider = FakeProvider::new(Rc::clone(&events));
    let origin = ScriptedOrigin::new(32, Rc::clone(&events));
    let mut backend = backend(&provider, origin);
    grow_one(&mut backend);
    backend.write_at(0, b"gen!").unwrap();
    backend.release_cache();
    assert_eq!(
        backend
            .observe_gpu(
                Some(&healthy_sample()),
                Duration::from_secs(4),
                Instant::now()
            )
            .allocated_bytes,
        8
    );

    events.borrow_mut().clear();
    let mut read_back = [0; 4];
    backend.read_at(0, &mut read_back).unwrap();

    assert_eq!(&read_back, b"gen!");
    assert_eq!(events.borrow().as_slice(), ["origin_read", "cache_write"]);
}

#[test]
fn origin_failure_is_sticky_until_three_read_sync_probes() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let provider = FakeProvider::new(Rc::clone(&events));
    let origin = ScriptedOrigin::new(32, events);
    let fail_write = Rc::clone(&origin.fail_write);
    let mut backend = backend(&provider, origin);
    fail_write.set(true);
    assert!(backend.write_at(0, b"fail").is_err());
    fail_write.set(false);

    assert_eq!(backend.probe_origin().unwrap(), OriginState::Degraded);
    assert_eq!(backend.probe_origin().unwrap(), OriginState::Degraded);
    assert_eq!(backend.probe_origin().unwrap(), OriginState::Ready);
}

#[test]
fn production_constructor_seals_chunk_size_at_128_mib() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let provider = FakeProvider::new(Rc::clone(&events));
    let origin = ScriptedOrigin::new(8, events);
    let backend = WriteThroughCacheBackend::new(&provider, origin, 8, 4).unwrap();
    assert_eq!(backend.chunk_bytes(), ORIGIN_CACHE_CHUNK_BYTES);
}

// TestName: origin_cache_io_carries_every_dropped_counter_from_the_measured_set
#[test]
fn origin_cache_io_carries_every_dropped_counter_from_the_measured_set() {
    let measured = CacheTelemetry {
        origin_written_bytes: 40_960,
        origin_syncs: 7,
        batched_writes: 11,
        cache_read_bytes: 8_192,
        cache_hits: 2,
        fallback_reads: 3,
        invalidations: 5,
        promotion_refusals: 13,
        releases: 2,
        allocation_failures: 17,
        cache_read_failures: 19,
        cache_write_failures: 23,
        valid_blocks: 29,
    };
    let published = OriginCacheIo::from(measured);
    assert_eq!(
        published,
        OriginCacheIo {
            origin_syncs: 7,
            batched_writes: 11,
            cache_read_bytes: 8_192,
            cache_read_failures: 19,
            cache_write_failures: 23,
            promotion_refusals: Some(13),
            allocation_failures: Some(17),
            valid_blocks: Some(29),
        }
    );
}

// TestName: origin_cache_io_publishes_the_byte_volume_without_renaming_its_unit
#[test]
fn origin_cache_io_publishes_the_byte_volume_without_renaming_its_unit() {
    let published = OriginCacheIo::from(CacheTelemetry {
        cache_read_bytes: 8_192,
        ..CacheTelemetry::default()
    });
    let wire = serde_json::to_value(published).expect("serializable");
    // The cache-hit metric keeps the source name and its byte unit, so it
    // matches the benchmark taxonomy. A KiB alias would be a second name
    // for one fact; `cache_write_bytes` does not exist and is never
    // invented — write-through traffic is `ssd_origin_written_kib`.
    assert_eq!(wire["cache_read_bytes"], 8_192);
    assert!(wire.get("cache_read_kib").is_none());
    assert!(wire.get("cache_write_bytes").is_none());
}

// TestName: origin_cache_io_is_a_faithful_round_trip_of_the_published_shape
#[test]
fn origin_cache_io_is_a_faithful_round_trip_of_the_published_shape() {
    let published = OriginCacheIo {
        origin_syncs: 1,
        batched_writes: 2,
        cache_read_bytes: 3,
        cache_read_failures: 4,
        cache_write_failures: 5,
        promotion_refusals: Some(6),
        allocation_failures: Some(7),
        valid_blocks: Some(8),
    };
    let wire = serde_json::to_value(published).expect("serializable");
    for key in [
        "origin_syncs",
        "batched_writes",
        "cache_read_bytes",
        "cache_read_failures",
        "cache_write_failures",
        "promotion_refusals",
        "allocation_failures",
        "valid_blocks",
    ] {
        assert!(wire.get(key).is_some(), "missing published key {key}");
    }
    let parsed: OriginCacheIo =
        serde_json::from_value(wire.clone()).expect("published shape parses");
    assert_eq!(parsed, published);
}

#[test]
fn origin_and_cache_state_labels_cover_all_variants() {
    let origin_pairs = [
        (OriginState::Off, "OFF"),
        (OriginState::Ready, "READY"),
        (OriginState::Degraded, "DEGRADED"),
        (OriginState::Failed, "FAILED"),
    ];
    for (state, expected) in origin_pairs {
        assert_eq!(state.as_str(), expected);
    }

    let cache_pairs = [
        (CacheState::Off, "OFF"),
        (CacheState::Active, "ACTIVE"),
        (CacheState::Restricted, "RESTRICTED"),
        (CacheState::Unavailable, "UNAVAILABLE"),
        (CacheState::Stuck, "STUCK"),
    ];
    for (state, expected) in cache_pairs {
        assert_eq!(state.as_str(), expected);
        let serialized = serde_json::to_string(&state).expect("serializable");
        let parsed: CacheState = serde_json::from_str(&serialized).expect("parses");
        assert_eq!(parsed, state);
    }
}
