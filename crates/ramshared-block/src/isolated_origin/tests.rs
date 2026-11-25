// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/isolated_origin/tests.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use std::time::Duration;

use super::*;

/// The cache-behaviour tests opt into admission explicitly: the backend's
/// fail-closed default refuses the cache (DT-9), and a test that exercises
/// cache hits/promotions/mirrors must say so.
fn backend_with_cache<O: OriginStorage, C: BestEffortCache>(
    origin: O,
    cache: C,
    size: u64,
    block: u32,
) -> Result<AuthoritativeOriginBackend<O, C>, IoError> {
    AuthoritativeOriginBackend::new_with_admission(origin, cache, size, block, AllowCacheAdmission)
}

#[derive(Clone)]
struct MemoryOrigin(Rc<RefCell<Vec<u8>>>);

impl OriginStorage for MemoryOrigin {
    fn read_at(&self, offset: u64, destination: &mut [u8]) -> Result<usize, IoError> {
        let start = offset as usize;
        destination.copy_from_slice(&self.0.borrow()[start..start + destination.len()]);
        Ok(destination.len())
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> Result<usize, IoError> {
        let start = offset as usize;
        self.0.borrow_mut()[start..start + data.len()].copy_from_slice(data);
        Ok(data.len())
    }

    fn sync_data(&self) -> Result<(), IoError> {
        Ok(())
    }
}

#[test]
fn cache_timeout_falls_back_to_origin() {
    let bytes = Rc::new(RefCell::new(b"origin!!".to_vec()));
    let (cache, _hung_worker) = isolated_cache_channel(1, Duration::ZERO);
    let backend = backend_with_cache(MemoryOrigin(bytes), cache, 8, 4).unwrap();

    let mut read_back = [0; 8];
    backend.read_at(0, &mut read_back).unwrap();

    assert_eq!(&read_back, b"origin!!");
    assert_eq!(backend.cache_state(), CacheState::Stuck);
    assert_eq!(backend.telemetry().cache_read_failures, 1);
    assert_eq!(backend.telemetry().fallback_reads, 1);
}

#[test]
fn cache_disconnect_falls_back_to_origin() {
    let bytes = Rc::new(RefCell::new(b"durable!".to_vec()));
    let (cache, worker) = isolated_cache_channel(1, Duration::from_millis(1));
    drop(worker);
    let backend = backend_with_cache(MemoryOrigin(bytes), cache, 8, 4).unwrap();

    let mut read_back = [0; 8];
    backend.read_at(0, &mut read_back).unwrap();

    assert_eq!(&read_back, b"durable!");
    assert_eq!(backend.cache_state(), CacheState::Stuck);
    assert_eq!(backend.telemetry().cache_read_failures, 1);
    assert_eq!(backend.telemetry().fallback_reads, 1);
}

/// A codec/decode fault inside the compressed cache must serve the SSD
/// origin bytes, never the refused payload and never a fabricated one
/// (RF-1, DT-1, DT-7, DT-11).
///
/// Unlike a transport timeout or a disconnect, a codec integrity failure
/// surfaces as a cache **miss** with the corrupt payload left unread. The
/// origin stays authoritative and the cache client is not revoked (DT-11).
#[test]
fn compressed_cache_fault_falls_back_to_origin() {
    let counters = Rc::new(CacheCounters::default());
    let mut codec_fault_cache = ScriptedCache::active(Rc::clone(&counters));
    // The worker refused this entry before decode (stored-checksum
    // mismatch), so the read is a miss. The payload the slab still holds
    // is deliberately wrong: it must never reach the reader.
    codec_fault_cache.read = CacheRead::Miss;
    codec_fault_cache.hit = b"corrupt!".to_vec();

    let bytes = Rc::new(RefCell::new(b"origin!!".to_vec()));
    let backend =
        backend_with_cache(MemoryOrigin(Rc::clone(&bytes)), codec_fault_cache, 8, 4).unwrap();

    let mut destination = [0; 8];
    backend.read_at(0, &mut destination).unwrap();

    // The origin is authoritative: the durable bytes are what the reader
    // gets, not the refused compressed payload.
    assert_eq!(&destination, b"origin!!");
    assert_ne!(&destination, b"corrupt!");
    assert_eq!(backend.telemetry().fallback_reads, 1);
    assert_eq!(
        backend.telemetry().cache_read_failures,
        0,
        "a codec integrity miss is not a transport failure"
    );
    assert_eq!(
        backend.cache_state(),
        CacheState::Active,
        "a codec fault must not revoke the cache client (DT-11)"
    );

    // A later real hit is still served from the cache, proving the client
    // survived the codec fault and the raw path keeps working.
    let mut hit_cache = ScriptedCache::active(counters);
    hit_cache.read = CacheRead::Hit;
    hit_cache.hit = b"cache!!!".to_vec();
    let after = backend_with_cache(MemoryOrigin(bytes), hit_cache, 8, 4).unwrap();
    after.read_at(0, &mut destination).unwrap();
    assert_eq!(&destination, b"cache!!!");
    assert_eq!(after.telemetry().fallback_reads, 0);
}

#[test]
fn disabled_cache_release_reports_already_released() {
    // SPEC swap-drain-safety RF-9: a permanently disabled cache has
    // nothing to revoke. `Accepted` here made every control-pressure pass
    // report a fresh revocation and count one in `telemetry.releases`.
    let bytes = Rc::new(RefCell::new(vec![0; 8]));
    let backend =
        AuthoritativeOriginBackend::new(MemoryOrigin(Rc::clone(&bytes)), DisabledCache, 8, 4)
            .unwrap();
    assert_eq!(
        backend.release_cache().unwrap(),
        CacheReleaseOutcome::AlreadyReleased
    );
    assert_eq!(backend.telemetry().releases, 0);
}

#[test]
fn disabled_cache_never_changes_origin_durability_order() {
    let bytes = Rc::new(RefCell::new(vec![0; 8]));
    let backend =
        AuthoritativeOriginBackend::new(MemoryOrigin(Rc::clone(&bytes)), DisabledCache, 8, 4)
            .unwrap();

    backend
        .write_at_with_options(0, b"safe", WriteOptions { fua: true })
        .unwrap();

    assert_eq!(&bytes.borrow()[..4], b"safe");
    assert_eq!(backend.telemetry().origin_syncs, 1);
    assert_eq!(backend.cache_state(), CacheState::Unavailable);
}

#[test]
fn replace_cache_swaps_the_mirror_without_touching_origin_telemetry() {
    let bytes = Rc::new(RefCell::new(vec![b'o'; 8]));
    let counters = Rc::new(CacheCounters::default());
    let mut dead = ScriptedCache::active(Rc::clone(&counters));
    dead.state = CacheState::Unavailable;
    dead.read = CacheRead::Failed;
    let backend = backend_with_cache(MemoryOrigin(Rc::clone(&bytes)), dead, 8, 4).unwrap();

    // A failed mirror already forced a fall back. That counter documents
    // origin service, so it must survive the swap — a respawn is a mirror
    // replacement, not a new origin lifetime.
    let mut scratch = [0; 8];
    backend.read_at(0, &mut scratch).unwrap();
    assert_eq!(&scratch, b"oooooooo");
    let fallback_reads_before = backend.telemetry().fallback_reads;
    assert_eq!(fallback_reads_before, 1);

    let mut fresh = ScriptedCache::active(Rc::clone(&counters));
    fresh.read = CacheRead::Hit;
    fresh.hit = b"cache!!!".to_vec();
    backend.replace_cache(fresh);
    assert_eq!(backend.cache_state(), CacheState::Active);

    let mut hit = [0; 8];
    backend.read_at(0, &mut hit).unwrap();
    assert_eq!(&hit, b"cache!!!");
    assert_eq!(
        backend.telemetry().fallback_reads,
        fallback_reads_before,
        "replace_cache must not reset origin fall-back accounting"
    );
    assert_eq!(bytes.borrow().as_slice(), b"oooooooo");
}

#[derive(Default)]
struct CacheCounters {
    disables: Cell<u32>,
}

struct ScriptedCache {
    read: CacheRead,
    hit: Vec<u8>,
    update: CacheMutation,
    promote: CacheMutation,
    state: CacheState,
    counters: Rc<CacheCounters>,
    cached_bytes: u64,
    target_bytes: u64,
}

impl ScriptedCache {
    fn active(counters: Rc<CacheCounters>) -> Self {
        Self {
            read: CacheRead::Miss,
            hit: Vec::new(),
            update: CacheMutation::Accepted,
            promote: CacheMutation::Accepted,
            state: CacheState::Active,
            counters,
            cached_bytes: 4,
            target_bytes: 8,
        }
    }
}

impl BestEffortCache for ScriptedCache {
    fn read(&mut self, _offset: u64, destination: &mut [u8]) -> CacheRead {
        if self.read == CacheRead::Hit {
            destination.copy_from_slice(&self.hit);
        }
        self.read
    }

    fn update(&mut self, _offset: u64, _data: &[u8]) -> CacheMutation {
        self.update
    }

    fn promote(&mut self, _offset: u64, _data: &[u8]) -> CacheMutation {
        self.promote
    }

    fn disable(&mut self) -> CacheMutation {
        self.counters
            .disables
            .set(self.counters.disables.get().saturating_add(1));
        self.state = CacheState::Unavailable;
        CacheMutation::Accepted
    }

    fn state(&self) -> CacheState {
        self.state
    }

    fn cached_bytes(&self) -> u64 {
        self.cached_bytes
    }

    fn target_bytes(&self) -> u64 {
        self.target_bytes
    }
}

struct FaultOriginState {
    bytes: RefCell<Vec<u8>>,
    fail_read: Cell<bool>,
    fail_write: Cell<bool>,
    fail_sync: Cell<bool>,
    syncs: Cell<u32>,
}

#[derive(Clone)]
struct FaultOrigin(Rc<FaultOriginState>);

impl OriginStorage for FaultOrigin {
    fn read_at(&self, offset: u64, destination: &mut [u8]) -> Result<usize, IoError> {
        if self.0.fail_read.get() {
            return Err(IoError("fixture origin read failure".into()));
        }
        let start = offset as usize;
        destination.copy_from_slice(&self.0.bytes.borrow()[start..start + destination.len()]);
        Ok(destination.len())
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> Result<usize, IoError> {
        if self.0.fail_write.get() {
            return Err(IoError("fixture origin write failure".into()));
        }
        let start = offset as usize;
        self.0.bytes.borrow_mut()[start..start + data.len()].copy_from_slice(data);
        Ok(data.len())
    }

    fn sync_data(&self) -> Result<(), IoError> {
        if self.0.fail_sync.get() {
            return Err(IoError("fixture origin sync failure".into()));
        }
        self.0.syncs.set(self.0.syncs.get().saturating_add(1));
        Ok(())
    }
}

fn fault_origin(bytes: &[u8]) -> (FaultOrigin, Rc<FaultOriginState>) {
    let state = Rc::new(FaultOriginState {
        bytes: RefCell::new(bytes.to_vec()),
        fail_read: Cell::new(false),
        fail_write: Cell::new(false),
        fail_sync: Cell::new(false),
        syncs: Cell::new(0),
    });
    (FaultOrigin(Rc::clone(&state)), state)
}

#[test]
fn bounded_cache_client_covers_hit_miss_mutation_and_disable_protocol() {
    let (mut cache, worker) = isolated_cache_channel(2, Duration::from_millis(100));
    let worker = std::thread::spawn(move || {
        if let Ok(IsolatedCacheRequest::Read { offset, len, reply }) = worker.requests.recv() {
            assert_eq!((offset, len), (4, 4));
            reply.send(Ok(Some(b"hit!".to_vec()))).unwrap();
        }
        worker
    });
    let mut destination = [0; 4];
    assert_eq!(cache.read(4, &mut destination), CacheRead::Hit);
    assert_eq!(&destination, b"hit!");
    let worker = worker.join().unwrap();

    assert_eq!(cache.update(8, b"new!"), CacheMutation::Accepted);
    if let Ok(IsolatedCacheRequest::Update { offset, data }) = worker.requests.recv() {
        assert_eq!(offset, 8);
        assert_eq!(data, b"new!");
    }
    assert_eq!(cache.promote(12, b"warm"), CacheMutation::Accepted);
    if let Ok(IsolatedCacheRequest::Promote { offset, data }) = worker.requests.recv() {
        assert_eq!(offset, 12);
        assert_eq!(data, b"warm");
    }
    let control = std::thread::spawn(move || {
        let IsolatedCacheControl::Disable { reply } = worker.control.recv().unwrap();
        reply.send(Ok(())).unwrap();
        worker
    });
    assert_eq!(cache.disable(), CacheMutation::Accepted);
    let worker = control.join().unwrap();
    assert_eq!(cache.state(), CacheState::Off);
    assert_eq!(cache.update(0, b"skip"), CacheMutation::Skipped);
    assert_eq!(cache.promote(0, b"skip"), CacheMutation::Skipped);
    assert_eq!(cache.read(0, &mut destination), CacheRead::Miss);
    assert_eq!(cache.disable(), CacheMutation::Skipped);
    drop(worker);
}

fn cache_read_with_reply(reply: Result<Option<Vec<u8>>, String>) -> (CacheRead, CacheState) {
    let (mut cache, worker) = isolated_cache_channel(1, Duration::from_millis(100));
    let worker = std::thread::spawn(move || {
        if let Ok(IsolatedCacheRequest::Read { reply: sender, .. }) = worker.requests.recv() {
            let _ = sender.send(reply);
        }
    });
    let result = cache.read(0, &mut [0; 4]);
    let _ = worker.join();
    (result, cache.state())
}

#[test]
fn bounded_cache_client_revokes_on_protocol_queue_and_transport_faults() {
    assert_eq!(
        cache_read_with_reply(Ok(None)),
        (CacheRead::Miss, CacheState::Active)
    );
    for reply in [Ok(Some(vec![1; 3])), Err("fixture refusal".into())] {
        assert_eq!(
            cache_read_with_reply(reply),
            (CacheRead::Failed, CacheState::Unavailable)
        );
    }

    let (mut blocked, worker) = isolated_cache_channel(1, Duration::ZERO);
    assert_eq!(blocked.update(0, b"first"), CacheMutation::Accepted);
    assert_eq!(blocked.update(4, b"second"), CacheMutation::Failed);
    assert_eq!(blocked.state(), CacheState::Unavailable);
    assert_eq!(blocked.update(4, b"third"), CacheMutation::Skipped);
    drop(worker);

    let (mut disconnected, worker) = isolated_cache_channel(1, Duration::ZERO);
    drop(worker);
    assert_eq!(disconnected.update(0, b"data"), CacheMutation::Failed);

    let (mut no_queue, _requests) = isolated_cache_channel(0, Duration::ZERO);
    assert_eq!(no_queue.read(0, &mut [0; 4]), CacheRead::Failed);
    assert_eq!(no_queue.state(), CacheState::Unavailable);
}

#[test]
// TestName: cache_disable_remains_deliverable_after_unavailable_full_data_queue
fn cache_disable_remains_deliverable_after_unavailable_full_data_queue() {
    let (mut cache, worker) = isolated_cache_channel(1, Duration::from_millis(100));
    assert_eq!(cache.update(0, b"first"), CacheMutation::Accepted);
    assert_eq!(cache.update(4, b"full"), CacheMutation::Failed);
    assert_eq!(cache.state(), CacheState::Unavailable);

    let control = std::thread::spawn(move || {
        let IsolatedCacheControl::Disable { reply } = worker.control.recv().unwrap();
        reply.send(Ok(())).unwrap();
        worker
    });
    assert_eq!(cache.disable(), CacheMutation::Accepted);
    let worker = control.join().unwrap();
    assert!(matches!(
        worker
            .requests
            .recv_timeout(Duration::from_millis(100))
            .unwrap(),
        IsolatedCacheRequest::Update { offset: 0, .. }
    ));
    assert_eq!(cache.state(), CacheState::Off);
}

#[test]
// TestName: release_cache_succeeds_only_after_dedicated_control_acknowledgement
fn release_cache_succeeds_only_after_dedicated_control_acknowledgement() {
    let bytes = Rc::new(RefCell::new(b"durable!".to_vec()));
    let (cache, worker) = isolated_cache_channel(1, Duration::from_millis(100));
    let acknowledged = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker_acknowledged = std::sync::Arc::clone(&acknowledged);
    let control = std::thread::spawn(move || {
        let IsolatedCacheControl::Disable { reply } = worker.control.recv().unwrap();
        worker_acknowledged.store(true, std::sync::atomic::Ordering::SeqCst);
        reply.send(Ok(())).unwrap();
    });
    let backend = AuthoritativeOriginBackend::new(MemoryOrigin(bytes), cache, 8, 4).unwrap();

    assert_eq!(
        backend.release_cache_if_observed(|_| None::<()>).unwrap(),
        None
    );
    assert_eq!(backend.cache_state(), CacheState::Active);
    assert!(!acknowledged.load(std::sync::atomic::Ordering::SeqCst));

    // The outcome is the control fact; there is no amount beside it. The
    // isolated client never reports how much the cache held, so a byte
    // figure here could only be a placeholder — which is what the old
    // `Ok(0)` was, and what daemon log faces printed as "0 MiB".
    assert_eq!(
        backend
            .release_cache_observed(BestEffortCache::state)
            .unwrap(),
        (CacheReleaseOutcome::Revoked, CacheState::Active)
    );
    assert!(acknowledged.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(backend.cache_state(), CacheState::Off);
    control.join().unwrap();

    let bytes = Rc::new(RefCell::new(b"durable!".to_vec()));
    let (cache, worker) = isolated_cache_channel(1, Duration::ZERO);
    drop(worker);
    let unacknowledged = AuthoritativeOriginBackend::new(MemoryOrigin(bytes), cache, 8, 4).unwrap();
    assert!(
        unacknowledged
            .release_cache_observed(BestEffortCache::state)
            .is_err()
    );
    assert_eq!(unacknowledged.cache_state(), CacheState::Stuck);
}

/// A release that finds nothing to release is a successful control outcome
/// and is not a cache release.
///
/// Counting `Skipped` made `cache_releases` grow with the serving-loop rate
/// under a live `control_pressure` request while the cache was already `Off`
/// and `vram_cached_kib` was `0`. The sibling `invalidations` counter never
/// had that problem: `revoke_cache` gates it on there being something to
/// invalidate. Both counters must describe cache work, not request traffic.
#[test]
// TestName: a_release_that_finds_nothing_to_release_is_not_counted
fn a_release_that_finds_nothing_to_release_is_not_counted() {
    let bytes = Rc::new(RefCell::new(b"durable!".to_vec()));
    let (cache, worker) = isolated_cache_channel(1, Duration::from_millis(100));
    let control = std::thread::spawn(move || {
        let IsolatedCacheControl::Disable { reply } = worker.control.recv().unwrap();
        reply.send(Ok(())).unwrap();
    });
    let backend = AuthoritativeOriginBackend::new(MemoryOrigin(bytes), cache, 8, 4).unwrap();
    assert_eq!(backend.cache_state(), CacheState::Active);

    // First release: a real revoke, acknowledged by the worker.
    assert_eq!(
        backend.release_cache().unwrap(),
        CacheReleaseOutcome::Revoked
    );
    control.join().unwrap();
    assert_eq!(backend.cache_state(), CacheState::Off);
    assert_eq!(backend.telemetry().releases, 1);

    // Second release: the cache is already `Off`, so `disable` reports
    // `Skipped` and no control frame is sent. Still a successful outcome —
    // a distinguishable one, and still not a second cache release.
    for _ in 0..8 {
        assert_eq!(
            backend.release_cache().unwrap(),
            CacheReleaseOutcome::AlreadyReleased
        );
    }
    assert_eq!(backend.cache_state(), CacheState::Off);
    assert_eq!(
        backend.telemetry().releases,
        1,
        "a no-op release must not publish a cache release"
    );
    // `revoke_cache` gates `invalidations` on there being something to
    // invalidate, so it cannot absorb the no-ops either.
    assert_eq!(backend.telemetry().invalidations, 1);
}

#[test]
fn backend_geometry_range_and_empty_io_are_bounded() {
    let bytes = Rc::new(RefCell::new(vec![0; 8]));
    assert!(
        AuthoritativeOriginBackend::new(MemoryOrigin(Rc::clone(&bytes)), DisabledCache, 0, 4)
            .is_err()
    );
    assert!(
        AuthoritativeOriginBackend::new(MemoryOrigin(Rc::clone(&bytes)), DisabledCache, 8, 0)
            .is_err()
    );
    assert!(
        AuthoritativeOriginBackend::new(MemoryOrigin(Rc::clone(&bytes)), DisabledCache, 7, 4)
            .is_err()
    );

    let backend =
        AuthoritativeOriginBackend::new(MemoryOrigin(bytes), DisabledCache, 8, 4).unwrap();
    assert_eq!(backend.size_bytes(), 8);
    assert_eq!(backend.block_size(), 4);
    assert_eq!(backend.origin_state(), OriginState::Ready);
    assert_eq!((backend.cached_bytes(), backend.target_bytes()), (0, 0));
    assert!(backend.read_at(8, &mut []).is_ok());
    assert!(backend.write_at(8, &[]).is_ok());
    assert!(
        backend
            .write_at_with_options(8, &[], WriteOptions { fua: true })
            .is_ok()
    );
    assert!(backend.flush().is_ok());
    assert!(backend.read_at(8, &mut [0]).is_err());
    assert!(backend.write_at(u64::MAX, b"x").is_err());
}

#[test]
fn backend_cache_paths_preserve_origin_authority_and_telemetry() {
    let counters = Rc::new(CacheCounters::default());
    let mut hit_cache = ScriptedCache::active(Rc::clone(&counters));
    hit_cache.read = CacheRead::Hit;
    hit_cache.hit = b"cache!!!".to_vec();
    let bytes = Rc::new(RefCell::new(b"origin!!".to_vec()));
    let hit = backend_with_cache(MemoryOrigin(Rc::clone(&bytes)), hit_cache, 8, 4).unwrap();
    let mut destination = [0; 8];
    hit.read_at(0, &mut destination).unwrap();
    assert_eq!(&destination, b"cache!!!");
    assert_eq!(hit.telemetry().cache_read_bytes, 8);
    assert_eq!(hit.telemetry().fallback_reads, 0);
    assert_eq!((hit.cached_bytes(), hit.target_bytes()), (4, 8));
    assert_eq!(hit.release_cache().unwrap(), CacheReleaseOutcome::Revoked);
    assert_eq!(hit.cache_state(), CacheState::Unavailable);
    assert_eq!(hit.telemetry().invalidations, 1);

    let mut failed_cache = ScriptedCache::active(Rc::clone(&counters));
    failed_cache.read = CacheRead::Failed;
    let fallback = backend_with_cache(MemoryOrigin(Rc::clone(&bytes)), failed_cache, 8, 4).unwrap();
    fallback.read_at(0, &mut destination).unwrap();
    assert_eq!(&destination, b"origin!!");
    assert_eq!(fallback.telemetry().cache_read_failures, 1);
    assert_eq!(fallback.telemetry().fallback_reads, 1);

    let mut promotion_cache = ScriptedCache::active(Rc::clone(&counters));
    promotion_cache.promote = CacheMutation::Failed;
    let promotion =
        backend_with_cache(MemoryOrigin(Rc::clone(&bytes)), promotion_cache, 8, 4).unwrap();
    promotion.read_at(0, &mut destination).unwrap();
    assert_eq!(promotion.telemetry().promotion_refusals, 1);
    assert_eq!(promotion.cache_state(), CacheState::Unavailable);

    let mut update_cache = ScriptedCache::active(counters);
    update_cache.update = CacheMutation::Failed;
    let update = backend_with_cache(MemoryOrigin(bytes), update_cache, 8, 4).unwrap();
    update.write_at(0, b"safe").unwrap();
    assert_eq!(update.telemetry().origin_written_bytes, 4);
    assert_eq!(update.telemetry().batched_writes, 1);
    assert_eq!(update.telemetry().cache_write_failures, 1);
    assert_eq!(update.cache_state(), CacheState::Unavailable);
}

/// A refusal that only means "the peer has not arrived yet" must not
/// revoke the cache.
///
/// The daemon's authority refuses the cache before the first lease, and
/// the peer connects a few seconds later. Revoking on that refusal moved
/// the cache to `Off` — a state the product never recovers from — so a
/// host whose peer was a moment late kept an empty cache forever.
#[test]
fn a_pre_lease_refusal_does_not_revoke_the_cache() {
    struct PreLeaseAdmission;
    impl CacheAdmission for PreLeaseAdmission {
        fn admit_cache(&self) -> Result<(), String> {
            Err("cache admission requires a live vsock lease".into())
        }

        fn cache_revocation_pending(&self) -> bool {
            true
        }
    }

    let counters = Rc::new(CacheCounters::default());
    let mut cache = ScriptedCache::active(Rc::clone(&counters));
    cache.read = CacheRead::Hit;
    cache.hit = b"cache!!!".to_vec();
    let bytes = Rc::new(RefCell::new(b"origin!!".to_vec()));
    let backend = AuthoritativeOriginBackend::new_with_admission(
        MemoryOrigin(Rc::clone(&bytes)),
        cache,
        8,
        4,
        PreLeaseAdmission,
    )
    .expect("backend");
    let mut destination = [0; 8];
    backend.read_at(0, &mut destination).unwrap();
    assert_eq!(
        &destination, b"origin!!",
        "a refused cache must not serve, even with a hit staged"
    );
    assert_eq!(
        backend.cache_state(),
        CacheState::Active,
        "a pre-lease refusal must not move the cache to a state it never leaves"
    );
    assert_eq!(
        backend.admission_refusal_reason().as_deref(),
        Some("cache admission requires a live vsock lease"),
        "the refusal is still reported honestly"
    );
}

/// SPEC native-vsock ITEM-3 / DT-9: the backend consults the admission
/// authority before every cache interaction. A refused authority serves
/// the origin, revokes the cache, and never promotes or mirrors; an
/// admitted one keeps the cache behaviour the sibling tests exercise.
#[test]
fn cache_admission_refusal_serves_origin_and_revokes_the_cache() {
    let counters = Rc::new(CacheCounters::default());
    let mut cache = ScriptedCache::active(Rc::clone(&counters));
    cache.read = CacheRead::Hit;
    cache.hit = b"cache!!!".to_vec();
    let bytes = Rc::new(RefCell::new(b"origin!!".to_vec()));
    // The fail-closed default: no authority was supplied.
    let refused = AuthoritativeOriginBackend::new(MemoryOrigin(Rc::clone(&bytes)), cache, 8, 4)
        .expect("fail-closed default backend");
    let mut destination = [0; 8];
    refused.read_at(0, &mut destination).unwrap();
    assert_eq!(
        &destination, b"origin!!",
        "a refused cache must not serve, even with a hit staged"
    );
    assert_eq!(
        refused.cache_state(),
        CacheState::Unavailable,
        "the refused cache is revoked rather than left advertising"
    );
    assert_eq!(refused.telemetry().cache_read_bytes, 0);
    assert_eq!(refused.telemetry().fallback_reads, 1);
    assert_eq!(
        refused.admission_refusal_reason().as_deref(),
        Some("cache admission requires a supplied authority (DT-9)"),
        "the refusal itself is the published cause"
    );
    // A mirror write under a refused authority never reaches the cache.
    refused.write_at(0, b"safe").unwrap();
    assert_eq!(refused.telemetry().origin_written_bytes, 4);
    assert_eq!(refused.telemetry().cache_write_failures, 0);

    // The admitted path: a staged hit serves, and the cache stays active.
    let mut hit = ScriptedCache::active(Rc::clone(&counters));
    hit.read = CacheRead::Hit;
    hit.hit = b"cache!!!".to_vec();
    let admitted = backend_with_cache(MemoryOrigin(bytes), hit, 8, 4).unwrap();
    admitted.read_at(0, &mut destination).unwrap();
    assert_eq!(&destination, b"cache!!!");
    assert_eq!(admitted.cache_state(), CacheState::Active);
    assert_eq!(admitted.telemetry().cache_read_bytes, 8);
}

/// The recorded refusal is a live verdict: it is set by a refusal and
/// cleared by the next admission, so a recovered authority does not leave
/// a stale "no lease" cause beside a serving cache.
#[test]
fn cache_admission_reason_is_the_authority_verdict() {
    struct Switchable(Arc<AtomicBool>);

    impl CacheAdmission for Switchable {
        fn admit_cache(&self) -> Result<(), String> {
            if self.0.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err("cache admission requires a live vsock lease".into())
            }
        }
    }

    let admits = Arc::new(AtomicBool::new(false));
    let backend = AuthoritativeOriginBackend::new_with_admission(
        MemoryOrigin(Rc::new(RefCell::new(b"origin!!".to_vec()))),
        DisabledCache,
        8,
        4,
        Switchable(Arc::clone(&admits)),
    )
    .unwrap();
    let mut destination = [0; 8];
    backend.read_at(0, &mut destination).unwrap();
    assert_eq!(
        backend.admission_refusal_reason().as_deref(),
        Some("cache admission requires a live vsock lease")
    );

    admits.store(true, Ordering::SeqCst);
    backend.read_at(0, &mut destination).unwrap();
    assert_eq!(backend.admission_refusal_reason(), None);
}

/// Records every cache mutation frame so tests can assert the framing.
struct RecordingCache {
    updates: RefCell<Vec<(u64, usize)>>,
    promotes: RefCell<Vec<(u64, usize)>>,
}

impl RecordingCache {
    fn new() -> Self {
        Self {
            updates: RefCell::new(Vec::new()),
            promotes: RefCell::new(Vec::new()),
        }
    }
}

impl BestEffortCache for RecordingCache {
    fn read(&mut self, _offset: u64, destination: &mut [u8]) -> CacheRead {
        destination.fill(0);
        CacheRead::Miss
    }

    fn update(&mut self, offset: u64, data: &[u8]) -> CacheMutation {
        self.updates.borrow_mut().push((offset, data.len()));
        CacheMutation::Accepted
    }

    fn promote(&mut self, offset: u64, data: &[u8]) -> CacheMutation {
        self.promotes.borrow_mut().push((offset, data.len()));
        CacheMutation::Accepted
    }

    fn disable(&mut self) -> CacheMutation {
        CacheMutation::Accepted
    }

    fn state(&self) -> CacheState {
        CacheState::Active
    }
}

/// A block-layer write can be far larger than one mutation frame (the NBD
/// queue on this surface admits up to 4 MiB). Each frame must stay within
/// `MAX_CACHE_MUTATION_BYTES` or the transport fails closed and revokes the
/// cache, which is what used to leave `cache_state: UNAVAILABLE`.
#[test]
fn large_write_is_framed_to_the_cache_mutation_limit() {
    let capacity = 4 * 1024 * 1024;
    let bytes = Rc::new(RefCell::new(vec![0u8; capacity]));
    let cache = RecordingCache::new();
    let backend = backend_with_cache(MemoryOrigin(bytes), cache, capacity as u64, 512).unwrap();

    let payload = vec![0xA5u8; capacity];
    backend.write_at(0, &payload).unwrap();

    {
        // The cache guard is scoped: the backend's own accessors take the
        // same lock, and holding it here would deadlock the test.
        let cache = backend.cache.lock().expect("cache");
        let frames = cache.updates.borrow();
        assert_eq!(
            frames.len(),
            capacity / MAX_CACHE_MUTATION_BYTES,
            "one frame per MAX_CACHE_MUTATION_BYTES slice"
        );
        let mut expected_offset = 0u64;
        for (offset, len) in frames.iter() {
            assert_eq!(*offset, expected_offset, "frames must be consecutive");
            assert!(
                *len <= MAX_CACHE_MUTATION_BYTES,
                "frame of {len} bytes exceeds the mutation limit"
            );
            expected_offset += *len as u64;
        }
        assert_eq!(
            expected_offset, capacity as u64,
            "frames must cover the write"
        );
    }
    assert_eq!(backend.cache_state(), CacheState::Active);
    assert_eq!(backend.telemetry().cache_write_failures, 0);
}

/// The read-miss fallback promotes origin data back into the cache with the
/// same framing constraint as `update`.
#[test]
fn large_promote_is_framed_to_the_cache_mutation_limit() {
    let capacity = MAX_CACHE_MUTATION_BYTES * 3 + 512;
    let bytes = Rc::new(RefCell::new(vec![0x5Au8; capacity]));
    let cache = RecordingCache::new();
    let backend = backend_with_cache(MemoryOrigin(bytes), cache, capacity as u64, 512).unwrap();

    let mut destination = vec![0u8; capacity];
    backend.read_at(0, &mut destination).unwrap();

    {
        let cache = backend.cache.lock().expect("cache");
        let frames = cache.promotes.borrow();
        assert_eq!(frames.len(), 4, "three full frames plus a 512-byte tail");
        assert_eq!(frames[0], (0, MAX_CACHE_MUTATION_BYTES));
        assert_eq!(
            frames[1],
            (MAX_CACHE_MUTATION_BYTES as u64, MAX_CACHE_MUTATION_BYTES)
        );
        assert_eq!(
            frames[2],
            (
                2 * MAX_CACHE_MUTATION_BYTES as u64,
                MAX_CACHE_MUTATION_BYTES
            )
        );
        assert_eq!(frames[3], (3 * MAX_CACHE_MUTATION_BYTES as u64, 512));
    }
    assert_eq!(backend.telemetry().promotion_refusals, 0);
}

/// ITEM-1's matrix row: the interior-mutable backend serves concurrent
/// readers and writers from `&self`. Four threads write disjoint regions
/// and read them back; the
/// counters and the origin bytes must agree afterwards.
#[test]
fn shared_origin_backend_serves_reads_and_writes_concurrently() {
    struct SharedOrigin(Arc<std::sync::Mutex<Vec<u8>>>);

    impl OriginStorage for SharedOrigin {
        fn read_at(&self, offset: u64, destination: &mut [u8]) -> Result<usize, IoError> {
            let bytes = self.0.lock().expect("origin");
            let start = offset as usize;
            destination.copy_from_slice(&bytes[start..start + destination.len()]);
            Ok(destination.len())
        }

        fn write_at(&self, offset: u64, data: &[u8]) -> Result<usize, IoError> {
            let mut bytes = self.0.lock().expect("origin");
            let start = offset as usize;
            bytes[start..start + data.len()].copy_from_slice(data);
            Ok(data.len())
        }

        fn sync_data(&self) -> Result<(), IoError> {
            Ok(())
        }
    }

    let capacity = 4096usize;
    let origin_bytes = Arc::new(std::sync::Mutex::new(vec![0u8; capacity]));
    let backend = Arc::new(
        backend_with_cache(
            SharedOrigin(Arc::clone(&origin_bytes)),
            RecordingCache::new(),
            capacity as u64,
            8,
        )
        .unwrap(),
    );

    let mut workers = Vec::new();
    for worker in 0..4u64 {
        let backend = Arc::clone(&backend);
        workers.push(std::thread::spawn(move || {
            let region = worker * 512;
            for step in 0..16u64 {
                let offset = region + step * 16;
                let byte = (worker * 16 + step) as u8;
                backend.write_at(offset, &[byte; 16]).unwrap();
                let mut read_back = [0u8; 16];
                backend.read_at(offset, &mut read_back).unwrap();
                assert_eq!(read_back, [byte; 16]);
            }
        }));
    }
    for worker in workers {
        worker
            .join()
            .expect("a concurrent serving thread must not panic");
    }

    let telemetry = backend.telemetry();
    assert_eq!(telemetry.batched_writes, 64, "one counter per write");
    assert_eq!(
        telemetry.fallback_reads, 64,
        "the recording cache is a stub: every read falls through to the origin"
    );

    let bytes = origin_bytes.lock().expect("origin");
    for worker in 0..4u64 {
        let region = (worker * 512) as usize;
        for step in 0..16u64 {
            let offset = region + (step * 16) as usize;
            let byte = (worker * 16 + step) as u8;
            assert_eq!(
                &bytes[offset..offset + 16],
                &[byte; 16],
                "worker {worker} step {step} must survive on the origin"
            );
        }
    }
}

#[test]
fn origin_failure_requires_three_successful_read_sync_probes() {
    let (origin, state) = fault_origin(b"durable!");
    state.fail_read.set(true);
    let backend = AuthoritativeOriginBackend::new(origin, DisabledCache, 8, 4).unwrap();
    assert!(backend.read_at(0, &mut [0; 4]).is_err());
    assert_eq!(backend.origin_state(), OriginState::Failed);
    assert!(backend.write_at(0, b"nope").is_err());
    assert!(backend.flush().is_err());

    state.fail_read.set(false);
    assert_eq!(backend.probe_origin().unwrap(), OriginState::Degraded);
    assert_eq!(backend.probe_origin().unwrap(), OriginState::Degraded);
    assert_eq!(backend.probe_origin().unwrap(), OriginState::Ready);
    assert_eq!(state.syncs.get(), 3);

    backend.write_at(0, b"safe").unwrap();
    backend.flush().unwrap();
    assert_eq!(state.syncs.get(), 4);
    state.fail_sync.set(true);
    backend.write_at(4, b"data").unwrap();
    assert!(backend.flush().is_err());
    assert_eq!(backend.origin_state(), OriginState::Failed);
    assert!(backend.probe_origin().is_err());

    let (origin, state) = fault_origin(b"durable!");
    state.fail_write.set(true);
    let write_failure = AuthoritativeOriginBackend::new(origin, DisabledCache, 8, 4).unwrap();
    assert!(
        write_failure
            .write_at_with_options(0, b"fail", WriteOptions { fua: true })
            .is_err()
    );
    assert_eq!(write_failure.origin_state(), OriginState::Failed);
}

#[path = "tests/concurrency.rs"]
mod concurrency;
