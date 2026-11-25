// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/gpu_cache_worker/tests/raw_fixtures.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::super::*;
use super::*;

pub(super) fn trusted_test_budget(free: u64, total: u64) -> GpuBudgetSnapshot {
    GpuBudgetSnapshot {
        adapter: Some(GpuAdapterIdentity {
            node_mask: None,
            backend: "test".into(),
            key: "fake-adapter-0".into(),
            luid: None,
        }),
        total_bytes: Some(total),
        budget_bytes: total,
        used_bytes: total.saturating_sub(free),
        source: GpuBudgetSource::DriverReported,
        sampled_at: Instant::now(),
    }
}

pub(super) struct FakeMem {
    pub(super) data: Arc<Mutex<Vec<u8>>>,
    pub(super) len: usize,
    pub(super) live_allocations: Arc<AtomicUsize>,
}

impl Drop for FakeMem {
    fn drop(&mut self) {
        self.live_allocations.fetch_sub(1, Ordering::SeqCst);
    }
}

impl VramMemory for FakeMem {
    fn len(&self) -> usize {
        self.len
    }

    fn zero(&mut self) -> Result<(), VramError> {
        let mut guard = self.data.lock().map_err(|_| VramError::Busy)?;
        guard.fill(0);
        Ok(())
    }

    fn read_at(&self, off: u64, dst: &mut [u8]) -> Result<(), VramError> {
        let guard = self.data.lock().map_err(|_| VramError::Busy)?;
        let start = off as usize;
        let end = start + dst.len();
        if end > guard.len() {
            return Err(VramError::OutOfRange {
                off,
                len: dst.len() as u64,
                size: guard.len() as u64,
            });
        }
        dst.copy_from_slice(&guard[start..end]);
        Ok(())
    }

    fn write_at(&mut self, off: u64, src: &[u8]) -> Result<(), VramError> {
        let mut guard = self.data.lock().map_err(|_| VramError::Busy)?;
        let start = off as usize;
        let end = start + src.len();
        if end > guard.len() {
            return Err(VramError::OutOfRange {
                off,
                len: src.len() as u64,
                size: guard.len() as u64,
            });
        }
        guard[start..end].copy_from_slice(src);
        Ok(())
    }
}

pub(super) struct FakeProvider {
    pub(super) total: u64,
    pub(super) free: u64,
    pub(super) live_allocations: Arc<AtomicUsize>,
}

impl FakeProvider {
    pub(super) fn new(total: u64, free: u64) -> Self {
        Self {
            total,
            free,
            live_allocations: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl VramProvider for FakeProvider {
    type Mem<'p>
        = FakeMem
    where
        Self: 'p;

    fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, VramError> {
        self.live_allocations.fetch_add(1, Ordering::SeqCst);
        Ok(FakeMem {
            data: Arc::new(Mutex::new(vec![0u8; bytes])),
            len: bytes,
            live_allocations: Arc::clone(&self.live_allocations),
        })
    }

    fn mem_info(&self) -> Result<(u64, u64), VramError> {
        Ok((self.free, self.total))
    }

    fn budget_snapshot(&self) -> Result<GpuBudgetSnapshot, VramError> {
        Ok(trusted_test_budget(self.free, self.total))
    }
}

pub(super) struct PressureProvider {
    pub(super) total: u64,
    pub(super) external: Arc<AtomicU64>,
    pub(super) live_allocations: Arc<AtomicUsize>,
    pub(super) chunk_bytes: u64,
}

impl VramProvider for PressureProvider {
    type Mem<'p>
        = FakeMem
    where
        Self: 'p;

    fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, VramError> {
        self.live_allocations.fetch_add(1, Ordering::SeqCst);
        Ok(FakeMem {
            data: Arc::new(Mutex::new(vec![0u8; bytes])),
            len: bytes,
            live_allocations: Arc::clone(&self.live_allocations),
        })
    }

    fn mem_info(&self) -> Result<(u64, u64), VramError> {
        let cache_bytes =
            (self.live_allocations.load(Ordering::SeqCst) as u64).saturating_mul(self.chunk_bytes);
        let free = self
            .total
            .saturating_sub(self.external.load(Ordering::SeqCst))
            .saturating_sub(cache_bytes);
        Ok((free, self.total))
    }

    fn budget_snapshot(&self) -> Result<GpuBudgetSnapshot, VramError> {
        let (free, total) = self.mem_info()?;
        Ok(trusted_test_budget(free, total))
    }
}

/// In-memory origin for the authoritative-backend cases (AUD-33).
/// `RefCell` because positional origin I/O now takes `&self`.
pub(super) struct SliceOrigin(pub(super) std::cell::RefCell<Vec<u8>>);

impl OriginStorage for SliceOrigin {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<usize, IoError> {
        let start = off as usize;
        if start >= self.0.borrow().len() {
            return Ok(0);
        }
        let end = (start + buf.len()).min(self.0.borrow().len());
        buf[..end - start].copy_from_slice(&self.0.borrow()[start..end]);
        Ok(end - start)
    }

    fn write_at(&self, off: u64, data: &[u8]) -> Result<usize, IoError> {
        let start = off as usize;
        let end = start + data.len();
        if end > self.0.borrow().len() {
            return Err(IoError("origin write out of range".into()));
        }
        self.0.borrow_mut()[start..end].copy_from_slice(data);
        Ok(data.len())
    }

    fn sync_data(&self) -> Result<(), IoError> {
        Ok(())
    }
}

/// Forwards `BestEffortCache` onto a real `GpuCacheWorker`, splitting
/// mutations exactly the way the transport does: consecutive
/// `MAX_CACHE_MUTATION_BYTES` frames, **not** aligned to worker chunk
/// boundaries. Reproducing that split is the point (AUD-33) — an
/// already-aligned payload would never cross a chunk edge.
pub(super) struct WorkerCache<'a, 'p, P: VramProvider + 'p> {
    pub(super) worker: &'a mut GpuCacheWorker<'p, P>,
}

impl<P: VramProvider> BestEffortCache for WorkerCache<'_, '_, P> {
    fn read(&mut self, offset: u64, destination: &mut [u8]) -> CacheRead {
        match self.worker.handle_read(offset, destination.len()) {
            Some(bytes) => {
                destination.copy_from_slice(&bytes);
                CacheRead::Hit
            }
            None => CacheRead::Miss,
        }
    }

    fn update(&mut self, offset: u64, data: &[u8]) -> CacheMutation {
        for (frame_off, slice) in mutation_frames(offset, data) {
            self.worker.handle_update(frame_off, slice);
        }
        CacheMutation::Accepted
    }

    fn promote(&mut self, offset: u64, data: &[u8]) -> CacheMutation {
        self.update(offset, data)
    }

    fn disable(&mut self) -> CacheMutation {
        self.worker.handle_disable();
        CacheMutation::Accepted
    }

    fn state(&self) -> CacheState {
        CacheState::Active
    }
}

/// Provider with headroom well above the reserve floor + runtime buffer,
/// so admission never refuses a fixture allocation. A budget that sits
/// under the floor would silently skip every publish and the test would
/// measure refusal, not coherence.
pub(super) fn roomy_provider() -> FakeProvider {
    const GIB: u64 = 1024 * 1024 * 1024;
    FakeProvider::new(8 * GIB, 8 * GIB)
}

/// Worker with `chunk_bytes` and both compression modes, pre-filled with
/// `fill` across `[0, 2 * chunk_bytes)`.
pub(super) fn worker_covering_two_chunks(
    provider: &FakeProvider,
    chunk_bytes: u64,
    compression_enabled: bool,
    fill: u8,
) -> GpuCacheWorker<'_, FakeProvider> {
    let mut worker = GpuCacheWorker::new(
        provider,
        GpuWorkerConfig {
            target_bytes: 8 * chunk_bytes,
            chunk_bytes: chunk_bytes as usize,
            reserve_floor_bytes: 128 * 1024 * 1024,
            compression_enabled,
        },
    );
    let left = vec![fill; chunk_bytes as usize];
    worker.handle_update(0, &left);
    worker.handle_update(chunk_bytes, &left);
    assert_eq!(
        worker.handle_read(0, chunk_bytes as usize),
        Some(left.clone()),
        "fixture must hold the left chunk"
    );
    assert_eq!(
        worker.handle_read(chunk_bytes, chunk_bytes as usize),
        Some(left),
        "fixture must hold the right chunk"
    );
    worker
}
