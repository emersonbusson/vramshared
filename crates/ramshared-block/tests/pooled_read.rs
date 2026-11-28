// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/tests/pooled_read.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use ramshared_block::isolated_origin::{
    AuthoritativeOriginBackend, BestEffortCache, DisabledCache, POOLED_PAGE_BYTES,
    PooledCacheClient, PooledLookupReply, PooledReadState, pooled_crc32c,
};
use ramshared_block::{BlockBackend, IoError, OriginStorage, SharedBlockBackend, WriteOptions};

struct MemOrigin {
    data: Mutex<Vec<u8>>,
    syncs: AtomicU64,
}

impl MemOrigin {
    fn new(size: usize) -> Self {
        Self {
            data: Mutex::new(vec![0u8; size]),
            syncs: AtomicU64::new(0),
        }
    }
}

#[derive(Clone)]
struct SharedOrigin(Arc<MemOrigin>);

impl OriginStorage for SharedOrigin {
    fn read_at(&self, offset: u64, destination: &mut [u8]) -> Result<usize, IoError> {
        let guard = self.0.data.lock().unwrap();
        let start = offset as usize;
        let len = destination.len().min(guard.len().saturating_sub(start));
        destination[..len].copy_from_slice(&guard[start..start + len]);
        Ok(len)
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> Result<usize, IoError> {
        let mut guard = self.0.data.lock().unwrap();
        let start = offset as usize;
        let len = data.len().min(guard.len().saturating_sub(start));
        guard[start..start + len].copy_from_slice(&data[..len]);
        Ok(len)
    }

    fn sync_data(&self) -> Result<(), IoError> {
        self.0.syncs.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum FaultMode {
    Normal,
    BadCrc,
    WrongEpoch,
    WrongSlotGen,
    TimedOut,
    Busy,
    TransportError,
    ExceedDeadline,
}

struct MockPooledClient {
    pages: Mutex<HashMap<(u64, u64, u64), [u8; POOLED_PAGE_BYTES]>>,
    fault: Mutex<FaultMode>,
    accept_offers: AtomicBool,
    poisoned_epochs: Mutex<Vec<u64>>,
    lookup_hook: Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl MockPooledClient {
    fn new() -> Self {
        Self {
            pages: Mutex::new(HashMap::new()),
            fault: Mutex::new(FaultMode::Normal),
            accept_offers: AtomicBool::new(true),
            poisoned_epochs: Mutex::new(Vec::new()),
            lookup_hook: Mutex::new(None),
        }
    }
}

#[derive(Clone)]
struct SharedClient(Arc<MockPooledClient>);

impl PooledCacheClient for SharedClient {
    fn lookup_into(
        &self,
        epoch: u64,
        offset: u64,
        content_gen: u64,
        slot_gen: u64,
        deadline: Instant,
        dst: &mut [u8; POOLED_PAGE_BYTES],
    ) -> PooledLookupReply {
        if let Some(hook) = self.0.lookup_hook.lock().unwrap().take() {
            hook();
        }
        match *self.0.fault.lock().unwrap() {
            FaultMode::Busy => return PooledLookupReply::Busy,
            FaultMode::TimedOut => return PooledLookupReply::TimedOut,
            FaultMode::TransportError => return PooledLookupReply::TransportError,
            FaultMode::ExceedDeadline => {
                while Instant::now() <= deadline {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            _ => {}
        }
        let guard = self.0.pages.lock().unwrap();
        let Some(page) = guard.get(&(epoch, offset, content_gen)) else {
            return PooledLookupReply::Miss;
        };
        dst.copy_from_slice(page);
        let crc = pooled_crc32c(page);
        match *self.0.fault.lock().unwrap() {
            FaultMode::BadCrc => PooledLookupReply::Hit {
                epoch,
                slot_gen,
                content_gen,
                crc: crc ^ 1,
            },
            FaultMode::WrongEpoch => PooledLookupReply::Hit {
                epoch: epoch + 99,
                slot_gen,
                content_gen,
                crc,
            },
            FaultMode::WrongSlotGen => PooledLookupReply::Hit {
                epoch,
                slot_gen: slot_gen + 7,
                content_gen,
                crc,
            },
            _ => PooledLookupReply::Hit {
                epoch,
                slot_gen,
                content_gen,
                crc,
            },
        }
    }

    fn offer_copy(
        &self,
        epoch: u64,
        offset: u64,
        content_gen: u64,
        page: &[u8; POOLED_PAGE_BYTES],
        crc: u32,
    ) -> bool {
        assert_eq!(crc, pooled_crc32c(page));
        if !self.0.accept_offers.load(Ordering::Acquire) {
            return false;
        }
        self.0
            .pages
            .lock()
            .unwrap()
            .insert((epoch, offset, content_gen), *page);
        true
    }

    fn close_poisoned(&self, epoch: u64) {
        self.0.poisoned_epochs.lock().unwrap().push(epoch);
    }
}

#[test]
fn pooled_read_returns_ssd_after_write_races_hit_and_after_partial_timeout() {
    let origin = SharedOrigin(Arc::new(MemOrigin::new(256 * 1024)));
    let client = SharedClient(Arc::new(MockPooledClient::new()));
    let pooled = PooledReadState::with_budget(11, Duration::from_millis(50), client.clone());
    let backend = Arc::new(
        AuthoritativeOriginBackend::new(origin.clone(), DisabledCache, 256 * 1024, 4096)
            .unwrap()
            .with_pooled_read(pooled),
    );

    // 1. Write v1, read once (miss -> SSD -> offer), then read again (cache hit).
    let v1 = [0x11u8; POOLED_PAGE_BYTES];
    let v2 = [0x22u8; POOLED_PAGE_BYTES];
    backend.write_at(4096, &v1).unwrap();
    let mut out = [0u8; POOLED_PAGE_BYTES];
    backend.read_at(4096, &mut out).unwrap();
    assert_eq!(out, v1);
    assert_eq!(
        backend.pooled_read().unwrap().telemetry().offers_submitted,
        1
    );
    assert_eq!(backend.pooled_read().unwrap().telemetry().hits, 0);

    // Corrupt SSD directly underneath only to prove the second read came from the cache hit.
    origin
        .write_all_at(4096, &[0xEEu8; POOLED_PAGE_BYTES])
        .unwrap();
    backend.read_at(4096, &mut out).unwrap();
    assert_eq!(out, v1);
    assert_eq!(backend.pooled_read().unwrap().telemetry().hits, 1);
    origin.write_all_at(4096, &v1).unwrap();

    // 2. Concurrent write races a remote hit: lookup pauses after range lock drop while
    // another thread commits v2; revalidation must reject the stale v1 hit and return v2 from SSD.
    let gate = Arc::new((Mutex::new((false, false)), Condvar::new()));
    let hook_gate = Arc::clone(&gate);
    *client.0.lookup_hook.lock().unwrap() = Some(Box::new(move || {
        let (lock, cvar) = &*hook_gate;
        let mut state = lock.lock().unwrap();
        state.0 = true;
        cvar.notify_all();
        while !state.1 {
            state = cvar.wait(state).unwrap();
        }
    }));

    let reader_backend = Arc::clone(&backend);
    let reader = std::thread::spawn(move || {
        let mut buf = [0u8; POOLED_PAGE_BYTES];
        reader_backend.read_at(4096, &mut buf).unwrap();
        buf
    });
    {
        let (lock, cvar) = &*gate;
        let mut state = lock.lock().unwrap();
        while !state.0 {
            state = cvar.wait(state).unwrap();
        }
        backend.write_at(4096, &v2).unwrap();
        state.1 = true;
        cvar.notify_all();
    }
    assert_eq!(reader.join().unwrap(), v2);
    assert_eq!(
        backend
            .pooled_read()
            .unwrap()
            .telemetry()
            .stale_hit_refusals,
        1
    );

    // 3. Partial timeout quarantines lookup slots instead of reusing them until settled.
    *client.0.fault.lock().unwrap() = FaultMode::TimedOut;
    backend.read_at(4096, &mut out).unwrap();
    assert_eq!(out, v2);
    backend.read_at(4096, &mut out).unwrap();
    assert_eq!(out, v2);
    assert_eq!(
        backend.pooled_read().unwrap().telemetry().quarantined_slots,
        2
    );
    backend.read_at(4096, &mut out).unwrap();
    assert_eq!(out, v2);
    assert_eq!(backend.pooled_read().unwrap().telemetry().busy_skips, 1);

    assert!(backend.pooled_read().unwrap().settle_quarantined_slot(0));
    assert!(!backend.pooled_read().unwrap().settle_quarantined_slot(0));
    assert!(!backend.pooled_read().unwrap().settle_quarantined_slot(9));
    assert_eq!(
        backend.pooled_read().unwrap().telemetry().quarantined_slots,
        1
    );

    let expired = Instant::now()
        .checked_sub(Duration::from_millis(1))
        .unwrap();
    backend
        .read_at_with_deadline(4096, &mut out, Some(expired))
        .unwrap();
    assert_eq!(out, v2);

    *client.0.fault.lock().unwrap() = FaultMode::ExceedDeadline;
    let short_deadline = Instant::now() + Duration::from_millis(2);
    backend
        .read_at_with_deadline(4096, &mut out, Some(short_deadline))
        .unwrap();
    assert_eq!(out, v2);

    *client.0.fault.lock().unwrap() = FaultMode::Busy;
    client.0.accept_offers.store(false, Ordering::Release);
    backend.read_at(4096, &mut out).unwrap();
    assert_eq!(out, v2);
    assert!(backend.pooled_read().unwrap().telemetry().offers_dropped >= 1);

    let mut small = [0u8; 512];
    backend.read_at(4096, &mut small).unwrap();
    assert_eq!(&small[..], &v2[..512]);
    let mut empty = [];
    backend.read_at(4096, &mut empty).unwrap();
}

#[test]
fn pooled_write_and_fua_never_wait_on_host_and_invalidate_touched_stripes() {
    let origin = SharedOrigin(Arc::new(MemOrigin::new(256 * 1024)));
    let client = SharedClient(Arc::new(MockPooledClient::new()));
    *client.0.lookup_hook.lock().unwrap() =
        Some(Box::new(|| panic!("write/FUA must not touch host")));
    let pooled = PooledReadState::new(21, client.clone());
    let backend = AuthoritativeOriginBackend::new(origin.clone(), DisabledCache, 256 * 1024, 4096)
        .unwrap()
        .with_pooled_read(pooled);

    let before_s0 = backend.pooled_read().unwrap().snapshot(0, 4096).unwrap().1;
    let before_s1 = backend
        .pooled_read()
        .unwrap()
        .snapshot(64 * 1024, 4096)
        .unwrap()
        .1;

    let cross = vec![0x77u8; 8192];
    backend
        .write_at_with_options(64 * 1024 - 4096, &cross, WriteOptions { fua: true })
        .unwrap();
    assert_eq!(origin.0.syncs.load(Ordering::Acquire), 1);

    let after_s0 = backend.pooled_read().unwrap().snapshot(0, 4096).unwrap().1;
    let after_s1 = backend
        .pooled_read()
        .unwrap()
        .snapshot(64 * 1024, 4096)
        .unwrap()
        .1;
    let untouched_s2 = backend
        .pooled_read()
        .unwrap()
        .snapshot(128 * 1024, 4096)
        .unwrap()
        .1;
    assert_eq!(after_s0, before_s0 + 1);
    assert_eq!(after_s1, before_s1 + 1);
    assert_eq!(untouched_s2, 1);

    backend
        .write_at_with_options(128 * 1024, &[0x55u8; 4096], WriteOptions { fua: false })
        .unwrap();
    assert_eq!(origin.0.syncs.load(Ordering::Acquire), 1);
    assert_eq!(
        backend
            .pooled_read()
            .unwrap()
            .snapshot(128 * 1024, 4096)
            .unwrap()
            .1,
        2
    );
}

#[test]
fn pooled_crc_or_epoch_failure_retires_hit_and_reads_origin() {
    for (fault, label) in [
        (FaultMode::BadCrc, "bad_crc"),
        (FaultMode::WrongEpoch, "wrong_epoch"),
        (FaultMode::WrongSlotGen, "wrong_slot_gen"),
        (FaultMode::TransportError, "transport_error"),
    ] {
        let origin = SharedOrigin(Arc::new(MemOrigin::new(64 * 1024)));
        let client = SharedClient(Arc::new(MockPooledClient::new()));
        let pooled = PooledReadState::new(31, client.clone());
        let backend =
            AuthoritativeOriginBackend::new(origin.clone(), DisabledCache, 64 * 1024, 4096)
                .unwrap()
                .with_pooled_read(pooled);

        let good = [0x9Au8; POOLED_PAGE_BYTES];
        backend.write_at(0, &good).unwrap();
        let mut out = [0u8; POOLED_PAGE_BYTES];
        backend.read_at(0, &mut out).unwrap();
        assert_eq!(out, good);
        assert_eq!(backend.pooled_read().unwrap().active_epoch(), 31);

        *client.0.fault.lock().unwrap() = fault;
        out.fill(0);
        backend.read_at(0, &mut out).unwrap();
        assert_eq!(
            out, good,
            "origin fallback must return exact bytes for {label}"
        );
        assert_eq!(
            backend.pooled_read().unwrap().active_epoch(),
            0,
            "epoch must retire on {label}"
        );
        assert_eq!(*client.0.poisoned_epochs.lock().unwrap(), vec![31]);
        assert_eq!(
            backend
                .pooled_read()
                .unwrap()
                .telemetry()
                .poison_retirements,
            1
        );

        backend.read_at(0, &mut out).unwrap();
        assert_eq!(out, good);
        assert_eq!(*client.0.poisoned_epochs.lock().unwrap(), vec![31]);
    }
}

#[test]
fn authoritative_origin_trait_delegation_telemetry_and_offer_slot_saturation() {
    // 1. Exercise BlockBackend (&mut self), SharedBlockBackend (&self), and cache telemetry accessors.
    let origin = SharedOrigin(Arc::new(MemOrigin::new(64 * 1024)));
    let mut backend =
        AuthoritativeOriginBackend::new(origin.clone(), DisabledCache, 64 * 1024, 4096).unwrap();
    assert_eq!(backend.refresh_cached_bytes(), Ok(0));
    assert!(backend.bound_adapter_identity().is_none());
    assert!(backend.gpu_budget_telemetry().is_none());
    assert!(backend.gpu_utilization_telemetry().is_none());
    assert!(backend.cache_telemetry().is_none());

    let page_a = [0xA1u8; 4096];
    let page_b = [0xB2u8; 4096];
    BlockBackend::write_at(&mut backend, 0, &page_a).unwrap();
    BlockBackend::write_at_with_options(&mut backend, 4096, &page_b, WriteOptions { fua: true })
        .unwrap();
    BlockBackend::flush(&mut backend).unwrap();
    let mut read_buf = [0u8; 4096];
    BlockBackend::read_at(&mut backend, 0, &mut read_buf).unwrap();
    assert_eq!(read_buf, page_a);
    assert_eq!(BlockBackend::size_bytes(&backend), 64 * 1024);
    assert_eq!(BlockBackend::block_size(&backend), 4096);

    assert_eq!(SharedBlockBackend::size_bytes(&backend), 64 * 1024);
    assert_eq!(SharedBlockBackend::block_size(&backend), 4096);
    SharedBlockBackend::write_at(&backend, 8192, &page_a).unwrap();
    SharedBlockBackend::write_at_with_options(
        &backend,
        12288,
        &page_b,
        WriteOptions { fua: false },
    )
    .unwrap();
    SharedBlockBackend::flush(&backend).unwrap();
    SharedBlockBackend::read_at(&backend, 12288, &mut read_buf).unwrap();
    assert_eq!(read_buf, page_b);

    // 2. Origin read/write IO error marks origin failed and propagates cleanly.
    struct FailingOrigin;
    impl OriginStorage for FailingOrigin {
        fn read_at(&self, _offset: u64, _destination: &mut [u8]) -> Result<usize, IoError> {
            Err(IoError("disk read failed".into()))
        }
        fn write_at(&self, _offset: u64, _data: &[u8]) -> Result<usize, IoError> {
            Err(IoError("disk write failed".into()))
        }
        fn sync_data(&self) -> Result<(), IoError> {
            Err(IoError("disk sync failed".into()))
        }
    }
    let failing =
        AuthoritativeOriginBackend::new(FailingOrigin, DisabledCache, 64 * 1024, 4096).unwrap();
    assert!(failing.read_at(0, &mut read_buf).is_err());
    assert!(failing.write_at(0, &page_a).is_err());
    assert!(failing.flush().is_err());

    // 3. Failing BestEffortCache triggers clean origin fallback and cache unavailability.
    struct FailingCache;
    impl BestEffortCache for FailingCache {
        fn read(
            &mut self,
            _offset: u64,
            _destination: &mut [u8],
        ) -> ramshared_block::isolated_origin::CacheRead {
            ramshared_block::isolated_origin::CacheRead::Failed
        }
        fn update(
            &mut self,
            _offset: u64,
            _data: &[u8],
        ) -> ramshared_block::isolated_origin::CacheMutation {
            ramshared_block::isolated_origin::CacheMutation::Failed
        }
        fn promote(
            &mut self,
            _offset: u64,
            _data: &[u8],
        ) -> ramshared_block::isolated_origin::CacheMutation {
            ramshared_block::isolated_origin::CacheMutation::Failed
        }
        fn disable(&mut self) -> ramshared_block::isolated_origin::CacheMutation {
            ramshared_block::isolated_origin::CacheMutation::Failed
        }
        fn state(&self) -> ramshared_block::origin_cache::CacheState {
            ramshared_block::origin_cache::CacheState::Active
        }
    }
    let with_fail_cache =
        AuthoritativeOriginBackend::new(origin.clone(), FailingCache, 64 * 1024, 4096).unwrap();
    with_fail_cache.read_at(0, &mut read_buf).unwrap();
    assert_eq!(read_buf, page_a);
    let with_fail_cache2 =
        AuthoritativeOriginBackend::new(origin.clone(), FailingCache, 64 * 1024, 4096).unwrap();
    with_fail_cache2.write_at(0, &page_a).unwrap();
    assert!(with_fail_cache2.release_cache().is_err());

    // 4. Offer slot saturation across 3 concurrent origin misses (OFFER_SLOTS == 2) drops excess offer.
    struct BlockingOfferClient {
        gate: Arc<(Mutex<(usize, bool)>, Condvar)>,
    }
    impl PooledCacheClient for BlockingOfferClient {
        fn lookup_into(
            &self,
            _epoch: u64,
            _offset: u64,
            _content_gen: u64,
            _slot_gen: u64,
            _deadline: Instant,
            _dst: &mut [u8; POOLED_PAGE_BYTES],
        ) -> PooledLookupReply {
            PooledLookupReply::Miss
        }
        fn offer_copy(
            &self,
            _epoch: u64,
            _offset: u64,
            _content_gen: u64,
            _page: &[u8; POOLED_PAGE_BYTES],
            _crc: u32,
        ) -> bool {
            let (lock, cvar) = &*self.gate;
            let mut st = lock.lock().unwrap();
            st.0 += 1;
            cvar.notify_all();
            while !st.1 {
                st = cvar.wait(st).unwrap();
            }
            true
        }
        fn close_poisoned(&self, _epoch: u64) {}
    }
    let gate = Arc::new((Mutex::new((0usize, false)), Condvar::new()));
    let sat_pooled = PooledReadState::new(
        41,
        BlockingOfferClient {
            gate: Arc::clone(&gate),
        },
    );
    let sat_backend = Arc::new(
        AuthoritativeOriginBackend::new(origin, DisabledCache, 64 * 1024, 4096)
            .unwrap()
            .with_pooled_read(sat_pooled),
    );
    let mut handles = Vec::new();
    for i in 0..2u64 {
        let b = Arc::clone(&sat_backend);
        handles.push(std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            b.read_at(i * 4096, &mut buf).unwrap();
        }));
    }
    {
        let (lock, cvar) = &*gate;
        let mut st = lock.lock().unwrap();
        while st.0 < 2 {
            st = cvar.wait(st).unwrap();
        }
    }
    // Both offer slots are occupied; a 3rd miss drops its offer without waiting.
    let mut third_buf = [0u8; 4096];
    sat_backend.read_at(8192, &mut third_buf).unwrap();
    assert_eq!(
        sat_backend
            .pooled_read()
            .unwrap()
            .telemetry()
            .offers_dropped,
        1
    );
    {
        let (lock, cvar) = &*gate;
        let mut st = lock.lock().unwrap();
        st.1 = true;
        cvar.notify_all();
    }
    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn pooled_offer_transport_close_retires_epoch_and_rebind_restores_hits_cleanly() {
    struct ClosingOfferClient {
        closed: AtomicBool,
        fail_offer: AtomicBool,
        pages: Mutex<HashMap<(u64, u64, u64), [u8; POOLED_PAGE_BYTES]>>,
        poisoned: Mutex<Vec<u64>>,
    }
    impl PooledCacheClient for ClosingOfferClient {
        fn lookup_into(
            &self,
            epoch: u64,
            offset: u64,
            content_gen: u64,
            slot_gen: u64,
            _deadline: Instant,
            dst: &mut [u8; POOLED_PAGE_BYTES],
        ) -> PooledLookupReply {
            if self.closed.load(Ordering::Acquire) {
                return PooledLookupReply::TransportError;
            }
            let guard = self.pages.lock().unwrap();
            let Some(page) = guard.get(&(epoch, offset, content_gen)) else {
                return PooledLookupReply::Miss;
            };
            dst.copy_from_slice(page);
            PooledLookupReply::Hit {
                epoch,
                slot_gen,
                content_gen,
                crc: pooled_crc32c(page),
            }
        }
        fn offer_copy(
            &self,
            epoch: u64,
            offset: u64,
            content_gen: u64,
            page: &[u8; POOLED_PAGE_BYTES],
            _crc: u32,
        ) -> bool {
            if self.fail_offer.load(Ordering::Acquire) {
                self.closed.store(true, Ordering::Release);
                return false;
            }
            self.pages
                .lock()
                .unwrap()
                .insert((epoch, offset, content_gen), *page);
            true
        }
        fn close_poisoned(&self, epoch: u64) {
            self.closed.store(true, Ordering::Release);
            self.poisoned.lock().unwrap().push(epoch);
        }
        fn is_transport_closed(&self) -> bool {
            self.closed.load(Ordering::Acquire)
        }
    }

    let origin = SharedOrigin(Arc::new(MemOrigin::new(64 * 1024)));
    // Boot in retired state (e.g. host initially offline): origin reads work without panics.
    let backend = Arc::new(
        AuthoritativeOriginBackend::new(origin.clone(), DisabledCache, 64 * 1024, 4096)
            .unwrap()
            .with_pooled_read(PooledReadState::new_retired()),
    );
    let pooled = backend.pooled_read_arc().unwrap();
    assert_eq!(pooled.active_epoch(), 0);
    assert!(
        !pooled.rebind_client(0, SharedClient(Arc::new(MockPooledClient::new()))),
        "epoch 0 must be refused on rebind"
    );

    let v1 = [0x42u8; POOLED_PAGE_BYTES];
    backend.write_at(0, &v1).unwrap();
    let mut out = [0u8; POOLED_PAGE_BYTES];
    backend.read_at(0, &mut out).unwrap();
    assert_eq!(out, v1);

    // Bind first transport whose offer closes the stream; epoch must retire immediately
    // with zero quarantined lookup slots.
    let c1 = Arc::new(ClosingOfferClient {
        closed: AtomicBool::new(false),
        fail_offer: AtomicBool::new(true),
        pages: Mutex::new(HashMap::new()),
        poisoned: Mutex::new(Vec::new()),
    });
    struct ArcClosing(Arc<ClosingOfferClient>);
    impl PooledCacheClient for ArcClosing {
        fn lookup_into(
            &self,
            epoch: u64,
            offset: u64,
            content_gen: u64,
            slot_gen: u64,
            deadline: Instant,
            dst: &mut [u8; POOLED_PAGE_BYTES],
        ) -> PooledLookupReply {
            self.0
                .lookup_into(epoch, offset, content_gen, slot_gen, deadline, dst)
        }
        fn offer_copy(
            &self,
            epoch: u64,
            offset: u64,
            content_gen: u64,
            page: &[u8; POOLED_PAGE_BYTES],
            crc: u32,
        ) -> bool {
            self.0.offer_copy(epoch, offset, content_gen, page, crc)
        }
        fn close_poisoned(&self, epoch: u64) {
            self.0.close_poisoned(epoch);
        }
        fn is_transport_closed(&self) -> bool {
            self.0.is_transport_closed()
        }
    }
    assert!(pooled.rebind_client(5, ArcClosing(Arc::clone(&c1))));
    assert_eq!(pooled.active_epoch(), 5);
    backend.read_at(0, &mut out).unwrap();
    assert_eq!(out, v1);
    assert_eq!(
        pooled.active_epoch(),
        0,
        "offer transport close must retire epoch immediately"
    );
    assert_eq!(pooled.telemetry().quarantined_slots, 0);
    assert_eq!(*c1.poisoned.lock().unwrap(), vec![5]);

    // Rebind onto a fresh client (even if a restarted host reuses epoch=5) and verify hits work.
    let c2 = Arc::new(ClosingOfferClient {
        closed: AtomicBool::new(false),
        fail_offer: AtomicBool::new(false),
        pages: Mutex::new(HashMap::new()),
        poisoned: Mutex::new(Vec::new()),
    });
    assert!(pooled.rebind_client(5, ArcClosing(Arc::clone(&c2))));
    assert_eq!(pooled.active_epoch(), 5);
    // Miss -> SSD -> offer accepted -> next read hits VRAM!
    backend.read_at(0, &mut out).unwrap();
    assert_eq!(out, v1);
    origin
        .write_all_at(0, &[0xCCu8; POOLED_PAGE_BYTES])
        .unwrap();
    backend.read_at(0, &mut out).unwrap();
    assert_eq!(out, v1);
    assert_eq!(pooled.telemetry().hits, 1);
    origin.write_all_at(0, &v1).unwrap();

    // Quarantine both slots via TimedOut, then rebind onto a fresh epoch: idle quarantined slots
    // from the dead transport are cleared cleanly and lookups serve hits again.
    let bad = SharedClient(Arc::new(MockPooledClient::new()));
    *bad.0.fault.lock().unwrap() = FaultMode::TimedOut;
    assert!(pooled.rebind_client(6, bad));
    backend.read_at(0, &mut out).unwrap();
    backend.read_at(0, &mut out).unwrap();
    assert_eq!(pooled.telemetry().quarantined_slots, 2);

    let healthy = SharedClient(Arc::new(MockPooledClient::new()));
    assert!(pooled.rebind_client(7, healthy));
    assert_eq!(pooled.telemetry().quarantined_slots, 0);
    backend.read_at(0, &mut out).unwrap();
    backend.read_at(0, &mut out).unwrap();
    assert_eq!(pooled.telemetry().hits, 2);
}
