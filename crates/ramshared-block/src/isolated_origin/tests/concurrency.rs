// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/isolated_origin/tests/concurrency.rs
//! Concurrent origin and cache ordering, with controlled real thread interleavings.
use super::*;
use std::sync::{Mutex, mpsc};

struct ControlledOrigin {
    bytes: Mutex<Vec<u8>>,
    first_written: mpsc::Sender<()>,
    release_first: Mutex<mpsc::Receiver<()>>,
}
impl OriginStorage for ControlledOrigin {
    fn read_at(&self, offset: u64, dst: &mut [u8]) -> Result<usize, IoError> {
        let bytes = self.bytes.lock().unwrap();
        dst.copy_from_slice(&bytes[offset as usize..offset as usize + dst.len()]);
        Ok(dst.len())
    }
    fn write_at(&self, offset: u64, src: &[u8]) -> Result<usize, IoError> {
        self.bytes.lock().unwrap()[offset as usize..offset as usize + src.len()]
            .copy_from_slice(src);
        if src.first() == Some(&1) {
            self.first_written.send(()).unwrap();
            self.release_first
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap();
        }
        Ok(src.len())
    }
    fn sync_data(&self) -> Result<(), IoError> {
        Ok(())
    }
}

struct ByteCache {
    bytes: Vec<u8>,
    valid: Vec<bool>,
}
impl BestEffortCache for ByteCache {
    fn read(&mut self, offset: u64, dst: &mut [u8]) -> CacheRead {
        let start = offset as usize;
        if !self.valid[start..start + dst.len()]
            .iter()
            .all(|valid| *valid)
        {
            return CacheRead::Miss;
        }
        dst.copy_from_slice(&self.bytes[start..start + dst.len()]);
        CacheRead::Hit
    }
    fn update(&mut self, offset: u64, data: &[u8]) -> CacheMutation {
        let start = offset as usize;
        self.bytes[start..start + data.len()].copy_from_slice(data);
        self.valid[start..start + data.len()].fill(true);
        CacheMutation::Accepted
    }
    fn promote(&mut self, offset: u64, data: &[u8]) -> CacheMutation {
        self.update(offset, data)
    }
    fn disable(&mut self) -> CacheMutation {
        self.valid.fill(false);
        CacheMutation::Accepted
    }
    fn state(&self) -> CacheState {
        CacheState::Active
    }
}

type Backend = AuthoritativeOriginBackend<ControlledOrigin, ByteCache>;
fn fixture() -> (Arc<Backend>, mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let size = 128 * 1024;
    let origin = ControlledOrigin {
        bytes: Mutex::new(vec![0; size]),
        first_written: started_tx,
        release_first: Mutex::new(release_rx),
    };
    let cache = ByteCache {
        bytes: vec![0; size],
        valid: vec![false; size],
    };
    (
        Arc::new(backend_with_cache(origin, cache, size as u64, 4096).unwrap()),
        started_rx,
        release_tx,
    )
}

#[test]
fn overlapping_writes_cannot_publish_an_older_mirror_after_a_newer_origin_write() {
    let (backend, started, release) = fixture();
    let first = Arc::clone(&backend);
    let a = std::thread::spawn(move || first.write_at(0, &[1; 4]).unwrap());
    started.recv_timeout(Duration::from_secs(2)).unwrap();
    let second = Arc::clone(&backend);
    let (done_tx, done_rx) = mpsc::channel();
    let b = std::thread::spawn(move || {
        second.write_at(2, &[2; 4]).unwrap();
        done_tx.send(()).unwrap();
    });
    let premature = done_rx.recv_timeout(Duration::from_millis(100)).is_ok();
    release.send(()).unwrap();
    a.join().unwrap();
    b.join().unwrap();
    assert!(
        !premature,
        "overlapping writer entered before the first mirror completed"
    );
    let mut actual = [0; 6];
    backend.read_at(0, &mut actual).unwrap();
    assert_eq!(actual, [1, 1, 2, 2, 2, 2]);
    assert_eq!(&backend.origin.bytes.lock().unwrap()[..6], &actual);
}

#[test]
fn independent_stripes_remain_concurrent() {
    let (backend, started, release) = fixture();
    let first = Arc::clone(&backend);
    let a = std::thread::spawn(move || first.write_at(0, &[1; 4]).unwrap());
    started.recv_timeout(Duration::from_secs(2)).unwrap();
    let second = Arc::clone(&backend);
    let (done_tx, done_rx) = mpsc::channel();
    let b = std::thread::spawn(move || {
        second.write_at(65536, &[2; 4]).unwrap();
        done_tx.send(()).unwrap();
    });
    let independent_completed = done_rx.recv_timeout(Duration::from_secs(2)).is_ok();
    release.send(()).unwrap();
    a.join().unwrap();
    b.join().unwrap();
    assert!(
        independent_completed,
        "unrelated stripe was blocked by origin I/O"
    );
}

struct PausedReadOrigin {
    bytes: Mutex<Vec<u8>>,
    read_captured: mpsc::Sender<()>,
    release_read: Mutex<mpsc::Receiver<()>>,
    pause_once: AtomicBool,
}
impl OriginStorage for PausedReadOrigin {
    fn read_at(&self, offset: u64, dst: &mut [u8]) -> Result<usize, IoError> {
        let start = offset as usize;
        dst.copy_from_slice(&self.bytes.lock().unwrap()[start..start + dst.len()]);
        if self.pause_once.swap(false, Ordering::SeqCst) {
            self.read_captured.send(()).unwrap();
            self.release_read
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap();
        }
        Ok(dst.len())
    }
    fn write_at(&self, offset: u64, src: &[u8]) -> Result<usize, IoError> {
        let start = offset as usize;
        self.bytes.lock().unwrap()[start..start + src.len()].copy_from_slice(src);
        Ok(src.len())
    }
    fn sync_data(&self) -> Result<(), IoError> {
        Ok(())
    }
}

#[test]
fn delayed_cache_miss_cannot_promote_old_bytes_over_a_new_write() {
    let (captured_tx, captured_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let origin = PausedReadOrigin {
        bytes: Mutex::new(vec![0; 4096]),
        read_captured: captured_tx,
        release_read: Mutex::new(release_rx),
        pause_once: AtomicBool::new(true),
    };
    let cache = ByteCache {
        bytes: vec![0; 4096],
        valid: vec![false; 4096],
    };
    let backend = Arc::new(backend_with_cache(origin, cache, 4096, 4096).unwrap());
    let reader = Arc::clone(&backend);
    let a = std::thread::spawn(move || {
        let mut bytes = [0; 4];
        reader.read_at(0, &mut bytes).unwrap();
        bytes
    });
    captured_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let writer = Arc::clone(&backend);
    let (done_tx, done_rx) = mpsc::channel();
    let b = std::thread::spawn(move || {
        writer.write_at(0, &[2; 4]).unwrap();
        done_tx.send(()).unwrap();
    });
    let premature = done_rx.recv_timeout(Duration::from_millis(100)).is_ok();
    release_tx.send(()).unwrap();
    assert_eq!(a.join().unwrap(), [0; 4]);
    b.join().unwrap();
    assert!(!premature, "writer overtook a cache-miss promotion");
    let mut bytes = [0; 4];
    backend.read_at(0, &mut bytes).unwrap();
    assert_eq!(bytes, [2; 4]);
}

#[test]
fn flush_waits_for_inflight_origin_write() {
    let (backend, started, release) = fixture();
    let writer = Arc::clone(&backend);
    let a = std::thread::spawn(move || writer.write_at(0, &[1; 4]).unwrap());
    started.recv_timeout(Duration::from_secs(2)).unwrap();
    let flusher = Arc::clone(&backend);
    let (done_tx, done_rx) = mpsc::channel();
    let b = std::thread::spawn(move || {
        flusher.flush().unwrap();
        done_tx.send(()).unwrap();
    });
    let premature = done_rx.recv_timeout(Duration::from_millis(100)).is_ok();
    release.send(()).unwrap();
    a.join().unwrap();
    b.join().unwrap();
    assert!(
        !premature,
        "flush returned before an in-flight write completed"
    );
    assert_eq!(backend.telemetry().origin_syncs, 1);
}

struct PausedSyncOrigin {
    sync_started: mpsc::Sender<()>,
    release_sync: Mutex<mpsc::Receiver<()>>,
    first: AtomicBool,
}
impl OriginStorage for PausedSyncOrigin {
    fn read_at(&self, _: u64, dst: &mut [u8]) -> Result<usize, IoError> {
        dst.fill(0);
        Ok(dst.len())
    }
    fn write_at(&self, _: u64, src: &[u8]) -> Result<usize, IoError> {
        Ok(src.len())
    }
    fn sync_data(&self) -> Result<(), IoError> {
        if self.first.swap(false, Ordering::SeqCst) {
            self.sync_started.send(()).unwrap();
            self.release_sync
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(3))
                .unwrap();
        }
        Ok(())
    }
}

#[test]
fn write_during_flush_remains_dirty_for_the_next_barrier() {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let origin = PausedSyncOrigin {
        sync_started: started_tx,
        release_sync: Mutex::new(release_rx),
        first: AtomicBool::new(true),
    };
    let backend = Arc::new(backend_with_cache(origin, DisabledCache, 131072, 4096).unwrap());
    backend.write_at(0, &[3; 4]).unwrap();
    let flusher = Arc::clone(&backend);
    let a = std::thread::spawn(move || flusher.flush().unwrap());
    started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let writer = Arc::clone(&backend);
    let (done_tx, done_rx) = mpsc::channel();
    let b = std::thread::spawn(move || {
        writer.write_at(65536, &[4; 4]).unwrap();
        done_tx.send(()).unwrap();
    });
    let premature = done_rx.recv_timeout(Duration::from_millis(100)).is_ok();
    release_tx.send(()).unwrap();
    a.join().unwrap();
    b.join().unwrap();
    assert!(!premature, "write overtook an active durability barrier");
    backend.flush().unwrap();
    assert_eq!(backend.telemetry().origin_syncs, 2);
}

#[test]
fn write_during_health_probe_remains_dirty_for_the_next_barrier() {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let origin = PausedSyncOrigin {
        sync_started: started_tx,
        release_sync: Mutex::new(release_rx),
        first: AtomicBool::new(true),
    };
    let backend = Arc::new(backend_with_cache(origin, DisabledCache, 131072, 4096).unwrap());
    let probe = Arc::clone(&backend);
    let a = std::thread::spawn(move || probe.probe_origin().unwrap());
    started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let writer = Arc::clone(&backend);
    let (done_tx, done_rx) = mpsc::channel();
    let b = std::thread::spawn(move || {
        writer.write_at(65536, &[4; 4]).unwrap();
        done_tx.send(()).unwrap();
    });
    let premature = done_rx.recv_timeout(Duration::from_millis(100)).is_ok();
    release_tx.send(()).unwrap();
    a.join().unwrap();
    b.join().unwrap();
    assert!(!premature, "write overtook a health-probe sync");
    backend.flush().unwrap();
    assert_eq!(backend.telemetry().origin_syncs, 2);
}

struct PartiallyFailingOrigin {
    bytes: Mutex<Vec<u8>>,
    first_write: AtomicBool,
}
impl OriginStorage for PartiallyFailingOrigin {
    fn read_at(&self, offset: u64, dst: &mut [u8]) -> Result<usize, IoError> {
        let start = offset as usize;
        dst.copy_from_slice(&self.bytes.lock().unwrap()[start..start + dst.len()]);
        Ok(dst.len())
    }
    fn write_at(&self, offset: u64, src: &[u8]) -> Result<usize, IoError> {
        if !self.first_write.swap(false, Ordering::SeqCst) {
            return Err(IoError(
                "injected failure after partial origin write".into(),
            ));
        }
        let count = src.len() / 2;
        self.bytes.lock().unwrap()[offset as usize..offset as usize + count]
            .copy_from_slice(&src[..count]);
        Ok(count)
    }
    fn sync_data(&self) -> Result<(), IoError> {
        Ok(())
    }
}

#[test]
fn partial_origin_write_failure_revokes_stale_cache_before_recovery() {
    let origin = PartiallyFailingOrigin {
        bytes: Mutex::new(vec![0; 4096]),
        first_write: AtomicBool::new(true),
    };
    let cache = ByteCache {
        bytes: vec![0; 4096],
        valid: vec![false; 4096],
    };
    let backend = backend_with_cache(origin, cache, 4096, 4096).unwrap();
    let mut bytes = [0; 4];
    backend.read_at(0, &mut bytes).unwrap();
    assert!(backend.write_at(0, &[2; 4]).is_err());
    assert!(backend.read_at(0, &mut bytes).is_err());
    assert!(!backend.cache.lock().unwrap().valid.iter().any(|x| *x));
    for _ in 0..3 {
        backend.probe_origin().unwrap();
    }
    backend.read_at(0, &mut bytes).unwrap();
    assert_eq!(bytes, [2, 2, 0, 0]);
}
