// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/origin_cache/tests.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]
#[path = "tests/recovery_and_eviction.rs"]
mod recovery_and_eviction;

use super::*;

use std::cell::{Cell, RefCell};

use std::rc::Rc;

use std::time::Duration;

use ramshared_vram::{
    GpuAdapterIdentity, GpuBudgetSnapshot, GpuBudgetSource, VramError, VramMemory, VramProvider,
};

#[derive(Clone)]
pub(super) struct ScriptedOrigin {
    bytes: Rc<RefCell<Vec<u8>>>,
    events: Rc<RefCell<Vec<&'static str>>>,
    max_write: usize,
    fail_read: Rc<Cell<bool>>,
    fail_write: Rc<Cell<bool>>,
    fail_sync: Rc<Cell<bool>>,
    zero_write: Rc<Cell<bool>>,
    writes_before_failure: Rc<Cell<usize>>,
}

impl ScriptedOrigin {
    fn new(size: usize, events: Rc<RefCell<Vec<&'static str>>>) -> Self {
        Self {
            bytes: Rc::new(RefCell::new(vec![0; size])),
            events,
            max_write: usize::MAX,
            fail_read: Rc::new(Cell::new(false)),
            fail_write: Rc::new(Cell::new(false)),
            fail_sync: Rc::new(Cell::new(false)),
            zero_write: Rc::new(Cell::new(false)),
            writes_before_failure: Rc::new(Cell::new(usize::MAX)),
        }
    }
}

impl OriginStorage for ScriptedOrigin {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<usize, IoError> {
        self.events.borrow_mut().push("origin_read");
        if self.fail_read.get() {
            return Err(IoError("injected origin read failure".into()));
        }
        let bytes = self.bytes.borrow();
        let start = off as usize;
        let count = buf.len().min(bytes.len().saturating_sub(start));
        buf[..count].copy_from_slice(&bytes[start..start + count]);
        Ok(count)
    }

    fn write_at(&self, off: u64, data: &[u8]) -> Result<usize, IoError> {
        self.events.borrow_mut().push("origin_write");
        if self.fail_write.get() {
            return Err(IoError("injected origin write failure".into()));
        }
        let writes_before_failure = self.writes_before_failure.get();
        if writes_before_failure == 0 {
            return Err(IoError("injected partial origin write failure".into()));
        }
        if self.zero_write.get() {
            return Ok(0);
        }
        let count = data.len().min(self.max_write);
        let start = off as usize;
        self.bytes.borrow_mut()[start..start + count].copy_from_slice(&data[..count]);
        self.writes_before_failure
            .set(writes_before_failure.saturating_sub(1));
        Ok(count)
    }

    fn sync_data(&self) -> Result<(), IoError> {
        self.events.borrow_mut().push("origin_sync");
        if self.fail_sync.get() {
            Err(IoError("injected origin sync failure".into()))
        } else {
            Ok(())
        }
    }
}

#[derive(Clone)]
pub(super) struct FakeMem {
    bytes: Rc<RefCell<Vec<u8>>>,
    events: Rc<RefCell<Vec<&'static str>>>,
    fail_read: Rc<Cell<bool>>,
    fail_write: Rc<Cell<bool>>,
}

impl VramMemory for FakeMem {
    fn len(&self) -> usize {
        self.bytes.borrow().len()
    }

    fn zero(&mut self) -> Result<(), VramError> {
        self.bytes.borrow_mut().fill(0);
        Ok(())
    }

    fn read_at(&self, off: u64, dst: &mut [u8]) -> Result<(), VramError> {
        self.events.borrow_mut().push("cache_read");
        if self.fail_read.get() {
            return Err(VramError::Provider("injected cache read failure".into()));
        }
        let start = off as usize;
        dst.copy_from_slice(&self.bytes.borrow()[start..start + dst.len()]);
        Ok(())
    }

    fn write_at(&mut self, off: u64, src: &[u8]) -> Result<(), VramError> {
        self.events.borrow_mut().push("cache_write");
        if self.fail_write.get() {
            return Err(VramError::Provider("injected cache write failure".into()));
        }
        let start = off as usize;
        self.bytes.borrow_mut()[start..start + src.len()].copy_from_slice(src);
        Ok(())
    }
}

pub(super) struct FakeProvider {
    events: Rc<RefCell<Vec<&'static str>>>,
    fail_alloc: Cell<bool>,
    fail_read: Rc<Cell<bool>>,
    fail_write: Rc<Cell<bool>>,
}

impl FakeProvider {
    fn new(events: Rc<RefCell<Vec<&'static str>>>) -> Self {
        Self {
            events,
            fail_alloc: Cell::new(false),
            fail_read: Rc::new(Cell::new(false)),
            fail_write: Rc::new(Cell::new(false)),
        }
    }
}

impl VramProvider for FakeProvider {
    type Mem<'a> = FakeMem;

    fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, VramError> {
        self.events.borrow_mut().push("cache_alloc");
        if self.fail_alloc.get() {
            return Err(VramError::Provider("injected allocation failure".into()));
        }
        Ok(FakeMem {
            bytes: Rc::new(RefCell::new(vec![0; bytes])),
            events: Rc::clone(&self.events),
            fail_read: Rc::clone(&self.fail_read),
            fail_write: Rc::clone(&self.fail_write),
        })
    }

    fn mem_info(&self) -> Result<(u64, u64), VramError> {
        Ok((u64::MAX, u64::MAX))
    }
}

/// A fresh, driver-reported, adapter-bound sample at `t0`.
pub(super) fn sample_at(
    t0: Instant,
    budget_bytes: u64,
    external_usage_bytes: u64,
    total_vram_bytes: u64,
) -> GpuSample {
    GpuSample {
        budget_bytes,
        external_usage_bytes,
        total_vram_bytes,
        sampled_at: t0,
        source: GpuBudgetSource::DriverReported,
        adapter: Some(GpuAdapterIdentity {
            node_mask: None,
            backend: "test".into(),
            key: "test-adapter".into(),
            luid: None,
        }),
    }
}

pub(super) fn healthy_sample() -> GpuSample {
    sample_at(Instant::now(), 4 * GIB, 0, 8 * GIB)
}

/// The runtime headroom this crate's default backend applies (DT-4).
pub(super) fn runtime() -> u64 {
    RUNTIME_FREE_BUFFER_BYTES
}

pub(super) fn backend<'a>(
    provider: &'a FakeProvider,
    origin: ScriptedOrigin,
) -> WriteThroughCacheBackend<'a, FakeProvider, ScriptedOrigin> {
    WriteThroughCacheBackend::with_chunk_bytes(provider, origin, 32, 4, 8)
        .expect("valid test backend geometry")
}

pub(super) fn grow_one<O: OriginStorage>(
    backend: &mut WriteThroughCacheBackend<'_, FakeProvider, O>,
) {
    backend.observe_gpu(
        Some(&healthy_sample()),
        Duration::from_secs(0),
        Instant::now(),
    );
    backend.observe_gpu(
        Some(&healthy_sample()),
        Duration::from_secs(1),
        Instant::now(),
    );
    let outcome = backend.observe_gpu(
        Some(&healthy_sample()),
        Duration::from_secs(2),
        Instant::now(),
    );
    assert_eq!(outcome.allocated_bytes, 8);
}

pub(super) fn assert_write_release_vram_read_origin_hash_matches() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let provider = FakeProvider::new(Rc::clone(&events));
    let origin = ScriptedOrigin::new(32, Rc::clone(&events));
    let mut backend = backend(&provider, origin);
    grow_one(&mut backend);
    events.borrow_mut().clear();

    let payload = *b"cache-origin-round-trip-proof-32";
    backend.write_at(0, &payload).unwrap();
    assert_eq!(backend.release_cache(), 8);

    let mut read_back = [0; 32];
    backend.read_at(0, &mut read_back).unwrap();
    assert_eq!(read_back, payload);
    assert_eq!(backend.telemetry().fallback_reads, 1);
    assert_eq!(backend.telemetry().cache_hits, 0);
    assert_eq!(
        events.borrow().as_slice(),
        ["origin_write", "cache_write", "origin_read"]
    );
}

#[test]
fn origin_write_precedes_cache_update() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let provider = FakeProvider::new(Rc::clone(&events));
    let origin = ScriptedOrigin::new(32, Rc::clone(&events));
    let mut backend = backend(&provider, origin);
    grow_one(&mut backend);
    events.borrow_mut().clear();

    backend.write_at(0, &[1, 2, 3, 4]).unwrap();

    assert_eq!(events.borrow().as_slice(), ["origin_write", "cache_write"]);
}

#[test]
fn write_release_vram_read_origin_hash_matches() {
    assert_write_release_vram_read_origin_hash_matches();
}

#[test]
// TestName: write_release_vram_read_origin_hash_parallel_fixtures_are_isolated
fn write_release_vram_read_origin_hash_parallel_fixtures_are_isolated() {
    std::thread::scope(|scope| {
        let mut workers = Vec::with_capacity(4);
        for _ in 0..4 {
            workers.push(scope.spawn(assert_write_release_vram_read_origin_hash_matches));
        }
        for worker in workers.drain(..) {
            worker.join().unwrap();
        }
    });
}

#[test]
fn gpu_allocation_failure_continues_on_origin() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let provider = FakeProvider::new(Rc::clone(&events));
    provider.fail_alloc.set(true);
    let origin = ScriptedOrigin::new(32, Rc::clone(&events));
    let mut backend = backend(&provider, origin);

    backend.observe_gpu(
        Some(&healthy_sample()),
        Duration::from_secs(0),
        Instant::now(),
    );
    backend.observe_gpu(
        Some(&healthy_sample()),
        Duration::from_secs(1),
        Instant::now(),
    );
    let outcome = backend.observe_gpu(
        Some(&healthy_sample()),
        Duration::from_secs(2),
        Instant::now(),
    );
    assert_eq!(outcome.allocated_bytes, 0);
    backend.write_at(0, b"safe").unwrap();
    let mut read_back = [0; 4];
    backend.read_at(0, &mut read_back).unwrap();
    assert_eq!(&read_back, b"safe");
    assert_eq!(backend.telemetry().allocation_failures, 1);
}

#[test]
fn cache_growth_and_reclaim_hysteresis_is_exact() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let provider = FakeProvider::new(Rc::clone(&events));
    let origin = ScriptedOrigin::new(32, Rc::clone(&events));
    let mut backend = backend(&provider, origin);

    assert_eq!(
        backend
            .observe_gpu(Some(&healthy_sample()), Duration::ZERO, Instant::now())
            .allocated_bytes,
        0
    );
    assert_eq!(
        backend
            .observe_gpu(
                Some(&healthy_sample()),
                Duration::from_secs(1),
                Instant::now()
            )
            .allocated_bytes,
        0
    );
    assert_eq!(
        backend
            .observe_gpu(
                Some(&healthy_sample()),
                Duration::from_secs(2),
                Instant::now()
            )
            .allocated_bytes,
        8
    );
    assert_eq!(
        backend
            .observe_gpu(
                Some(&healthy_sample()),
                Duration::from_secs(3),
                Instant::now()
            )
            .allocated_bytes,
        0
    );
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
    assert_eq!(backend.cached_bytes(), 16);

    let restricted = sample_at(Instant::now(), 0, 0, 8 * GIB);
    assert_eq!(
        backend
            .observe_gpu(Some(&restricted), Duration::from_secs(5), Instant::now())
            .released_bytes,
        0
    );
    assert_eq!(
        backend
            .observe_gpu(Some(&restricted), Duration::from_secs(6), Instant::now())
            .released_bytes,
        0
    );
    assert_eq!(
        backend
            .observe_gpu(Some(&restricted), Duration::from_secs(7), Instant::now())
            .released_bytes,
        16
    );
    assert_eq!(backend.cached_bytes(), 0);
    assert_eq!(backend.cache_state(), CacheState::Restricted);
}

#[test]
fn configured_physical_cap_bounds_an_ample_gpu_budget() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let provider = FakeProvider::new(Rc::clone(&events));
    let origin = ScriptedOrigin::new(32, Rc::clone(&events));
    let mut backend = backend(&provider, origin);
    backend.set_physical_cap_bytes(8);

    let outcome = backend.observe_gpu(Some(&healthy_sample()), Duration::ZERO, Instant::now());
    assert_eq!(outcome.target_bytes, 8);
}

#[test]
fn origin_failure_returns_io_error() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let provider = FakeProvider::new(Rc::clone(&events));
    let origin = ScriptedOrigin::new(32, events);
    origin.fail_write.set(true);
    let mut backend = backend(&provider, origin);

    assert!(backend.write_at(0, b"fail").is_err());
    assert_eq!(backend.origin_state(), OriginState::Failed);
    assert_eq!(backend.telemetry().origin_written_bytes, 0);
    assert!(backend.read_at(0, &mut [0; 4]).is_err());
}

#[test]
fn partial_origin_write_is_completed_before_ack() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let provider = FakeProvider::new(Rc::clone(&events));
    let mut origin = ScriptedOrigin::new(32, Rc::clone(&events));
    origin.max_write = 2;
    let bytes = Rc::clone(&origin.bytes);
    let mut backend = backend(&provider, origin);

    backend.write_at(4, b"partial!").unwrap();

    assert_eq!(&bytes.borrow()[4..12], b"partial!");
    assert_eq!(
        events
            .borrow()
            .iter()
            .filter(|event| **event == "origin_write")
            .count(),
        4
    );
    assert_eq!(events.borrow().last(), Some(&"origin_write"));
    assert!(!events.borrow().contains(&"origin_sync"));
}

#[test]
fn zero_progress_origin_write_is_never_acknowledged() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let provider = FakeProvider::new(Rc::clone(&events));
    let origin = ScriptedOrigin::new(32, events);
    origin.zero_write.set(true);
    let mut backend = backend(&provider, origin);

    let error = backend.write_at(0, b"stop").unwrap_err();

    assert!(error.0.contains("no progress"));
    assert_eq!(backend.telemetry().origin_written_bytes, 0);
    assert_eq!(backend.telemetry().valid_blocks, 0);
}

#[test]
fn origin_flush_failure_does_not_ack_or_validate_cache() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let provider = FakeProvider::new(Rc::clone(&events));
    let origin = ScriptedOrigin::new(32, Rc::clone(&events));
    let fail_sync = Rc::clone(&origin.fail_sync);
    let mut backend = backend(&provider, origin);
    grow_one(&mut backend);
    events.borrow_mut().clear();

    backend.write_at(0, b"nope").unwrap();
    fail_sync.set(true);
    assert!(backend.flush().is_err());
    assert!(events.borrow().contains(&"cache_write"));
    assert_eq!(backend.telemetry().valid_blocks, 0);
    assert_eq!(backend.origin_state(), OriginState::Failed);
}

#[test]
fn partial_origin_failure_invalidates_cached_data_before_recovery_read() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let provider = FakeProvider::new(Rc::clone(&events));
    let mut origin = ScriptedOrigin::new(32, Rc::clone(&events));
    origin.max_write = 4;
    let bytes = Rc::clone(&origin.bytes);
    let writes_before_failure = Rc::clone(&origin.writes_before_failure);
    let mut backend = backend(&provider, origin);
    grow_one(&mut backend);
    backend.write_at(0, b"ABCDEFGH").unwrap();

    writes_before_failure.set(1);
    assert!(backend.write_at(0, b"ijklmnop").is_err());
    assert_eq!(&bytes.borrow()[..8], b"ijklEFGH");
    writes_before_failure.set(usize::MAX);
    assert_eq!(backend.probe_origin().unwrap(), OriginState::Degraded);
    assert_eq!(backend.probe_origin().unwrap(), OriginState::Degraded);
    assert_eq!(backend.probe_origin().unwrap(), OriginState::Ready);

    let mut recovered = [0; 8];
    backend.read_at(0, &mut recovered).unwrap();
    assert_eq!(&recovered, b"ijklEFGH");
}

#[test]
fn sync_origin_failure_invalidates_cached_data_before_recovery_read() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let provider = FakeProvider::new(Rc::clone(&events));
    let origin = ScriptedOrigin::new(32, Rc::clone(&events));
    let fail_sync = Rc::clone(&origin.fail_sync);
    let mut backend = backend(&provider, origin);
    grow_one(&mut backend);
    backend.write_at(0, b"old!").unwrap();
    backend.flush().unwrap();

    backend.write_at(0, b"new!").unwrap();
    fail_sync.set(true);
    assert!(backend.flush().is_err());
    fail_sync.set(false);
    assert_eq!(backend.probe_origin().unwrap(), OriginState::Degraded);
    assert_eq!(backend.probe_origin().unwrap(), OriginState::Degraded);
    assert_eq!(backend.probe_origin().unwrap(), OriginState::Ready);

    let mut recovered = [0; 4];
    backend.read_at(0, &mut recovered).unwrap();
    assert_eq!(&recovered, b"new!");
}

#[test]
fn durable_origin_write_legitimate_path_passes() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let provider = FakeProvider::new(Rc::clone(&events));
    let origin = ScriptedOrigin::new(32, Rc::clone(&events));
    let bytes = Rc::clone(&origin.bytes);
    let mut backend = backend(&provider, origin);

    backend.write_at(8, b"good").unwrap();
    backend.flush().unwrap();

    assert_eq!(&bytes.borrow()[8..12], b"good");
    assert_eq!(backend.telemetry().origin_written_bytes, 4);
    assert_eq!(backend.origin_state(), OriginState::Ready);
}

#[test]
fn normal_writes_batch_until_flush() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let provider = FakeProvider::new(Rc::clone(&events));
    let origin = ScriptedOrigin::new(32, Rc::clone(&events));
    let mut backend = backend(&provider, origin);

    backend.write_at(0, b"one!").unwrap();
    backend.write_at(4, b"two!").unwrap();
    assert!(!events.borrow().contains(&"origin_sync"));
    assert_eq!(backend.telemetry().batched_writes, 2);

    backend.flush().unwrap();
    assert_eq!(
        events
            .borrow()
            .iter()
            .filter(|event| **event == "origin_sync")
            .count(),
        1
    );
    assert_eq!(backend.telemetry().origin_syncs, 1);
}

#[test]
fn fua_write_syncs_before_ack() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let provider = FakeProvider::new(Rc::clone(&events));
    let origin = ScriptedOrigin::new(32, Rc::clone(&events));
    let mut backend = backend(&provider, origin);

    backend
        .write_at_with_options(0, b"fua!", WriteOptions { fua: true })
        .unwrap();

    assert_eq!(events.borrow().as_slice(), ["origin_write", "origin_sync"]);
    assert_eq!(backend.telemetry().origin_syncs, 1);
    assert_eq!(backend.telemetry().batched_writes, 0);
}

#[test]
fn flush_failure_invalidates_dirty_epoch() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let provider = FakeProvider::new(Rc::clone(&events));
    let origin = ScriptedOrigin::new(32, Rc::clone(&events));
    let fail_sync = Rc::clone(&origin.fail_sync);
    let mut backend = backend(&provider, origin);
    grow_one(&mut backend);
    backend.write_at(0, b"data").unwrap();
    assert_eq!(backend.telemetry().valid_blocks, 1);

    fail_sync.set(true);
    assert!(backend.flush().is_err());

    assert_eq!(backend.telemetry().valid_blocks, 0);
    assert_eq!(backend.origin_state(), OriginState::Failed);
}

/// DT-4/DT-6: the origin target uses the shared reserve floor, not an
/// independent reserve.
#[test]
fn origin_physical_target_uses_shared_reserve_floor() {
    let t0 = Instant::now();
    let sample = sample_at(t0, 10 * GIB, 2 * GIB, 20 * GIB);
    let snapshot = GpuBudgetSnapshot {
        adapter: sample.adapter.clone(),
        total_bytes: Some(sample.total_vram_bytes),
        budget_bytes: sample.budget_bytes,
        used_bytes: sample.external_usage_bytes,
        source: sample.source,
        sampled_at: sample.sampled_at,
    };
    let configured = 0;
    assert_eq!(
        physical_target_bytes(24 * GIB, Some(&sample), configured, runtime(), t0),
        snapshot.safe_target_bytes(24 * GIB, configured, runtime())
    );
}

/// DT-6: the `.max(2 * GIB)` term is deleted. At a capacity whose 20%
/// share is well under 2 GiB the old formula reserved 2 GiB; the shared
/// formula must produce a strictly larger target.
#[test]
fn origin_physical_target_has_no_hardcoded_two_gib_term() {
    let t0 = Instant::now();
    // budget 4 GiB, total 8 GiB → capacity = min(8, 4) = 4 GiB.
    // Shared reserve = ceil(4 GiB / 5) = 0.8 GiB.
    // Old reserve   = max(0.8 GiB, 2 GiB) = 2 GiB.
    let sample = sample_at(t0, 4 * GIB, 0, 8 * GIB);
    let target = physical_target_bytes(4 * GIB, Some(&sample), 0, runtime(), t0);
    let snapshot = GpuBudgetSnapshot {
        adapter: sample.adapter.clone(),
        total_bytes: Some(8 * GIB),
        budget_bytes: 4 * GIB,
        used_bytes: 0,
        source: GpuBudgetSource::DriverReported,
        sampled_at: t0,
    };
    let old_style = 4 * GIB.saturating_sub(0).saturating_sub(2 * GIB);
    assert_eq!(target, snapshot.safe_target_bytes(4 * GIB, 0, runtime()));
    assert!(target < 4 * GIB, "reserve and runtime still bind");
    // The shared floor is the 20% share, not 2 GiB.
    assert!(
        snapshot.required_free_bytes(0, runtime()) < 2 * GIB + runtime(),
        "no 2 GiB constant may remain in the reserve"
    );
    let _ = old_style;
}
