// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/cache_group/tests/ipc_journey.rs
//! Real Unix protocol peers backed by CPU maps, not GPU driver/budget fixtures.
use super::*;
use crate::gpu_cache_worker::*;
use crate::ipc_cache_client::PacingPolicy;
use crate::{AuthoritativeOriginBackend, FileOrigin, IpcCacheClient, WriteOptions};
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::sync::Mutex;

struct Peer {
    socket: UnixStream,
    thread: Option<std::thread::JoinHandle<()>>,
    trace: Arc<Mutex<Vec<u8>>>,
}
impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(Shutdown::Both);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

fn peer() -> (IpcCacheClient, Peer) {
    let (socket, mut server) = UnixStream::pair().unwrap();
    let control = server.try_clone().unwrap();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let traced = Arc::clone(&trace);
    let thread = std::thread::spawn(move || {
        let mut data = BTreeMap::<u64, u8>::new();
        loop {
            let mut bytes = [0; FRAME_HEADER_LEN];
            if server.read_exact(&mut bytes).is_err() {
                break;
            }
            let request = FrameHeader::decode(&bytes).unwrap();
            assert!(
                request.payload_len as usize
                    <= crate::ipc_cache_client::MAX_MUTATION_FRAME_DATA_BYTES
            );
            traced.lock().unwrap().push(request.msg_type);
            let mut payload = vec![0; request.payload_len as usize];
            if server.read_exact(&mut payload).is_err() {
                break;
            }
            if matches!(request.msg_type, MSG_PROMOTE | MSG_UPDATE) {
                if request.msg_type == MSG_PROMOTE
                    || (0..payload.len()).all(|i| data.contains_key(&(request.offset + i as u64)))
                {
                    for (i, byte) in payload.into_iter().enumerate() {
                        data.insert(request.offset + i as u64, byte);
                    }
                }
                continue;
            }
            let mut response = FrameHeader {
                msg_type: 0,
                status: STATUS_OK,
                correlation_id: request.correlation_id,
                offset: 4096,
                payload_len: 0,
                aux: (data.len() / 1024) as u32,
            };
            match request.msg_type {
                MSG_HANDSHAKE_REQ => response.msg_type = MSG_HANDSHAKE_RESP,
                MSG_READ_REQ => {
                    assert!(request.aux <= 8192);
                    response.msg_type = MSG_READ_RESP;
                    response.offset = request.offset;
                    if (0..request.aux).all(|i| data.contains_key(&(request.offset + i as u64))) {
                        payload = (0..request.aux)
                            .map(|i| data[&(request.offset + i as u64)])
                            .collect();
                        response.payload_len = payload.len() as u32;
                    } else {
                        response.status = STATUS_MISS;
                    }
                }
                MSG_HEARTBEAT_REQ => response.msg_type = MSG_HEARTBEAT_RESP,
                MSG_DISABLE_REQ => {
                    data.clear();
                    response.msg_type = MSG_DISABLE_RESP;
                    response.offset = 0;
                    response.aux = 0;
                }
                _ => panic!("unexpected reference-peer request"),
            }
            if server
                .write_all(&response.encode())
                .and_then(|()| server.write_all(&payload))
                .is_err()
            {
                break;
            }
            if request.msg_type == MSG_DISABLE_REQ {
                break;
            }
        }
    });
    let mut client = IpcCacheClient::with_pacing(
        socket,
        Duration::from_millis(50),
        4096,
        PacingPolicy::immediate(),
    );
    client.perform_handshake().unwrap();
    (
        client,
        Peer {
            socket: control,
            thread: Some(thread),
            trace,
        },
    )
}

struct OwnedFile(std::path::PathBuf);
impl Drop for OwnedFile {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).unwrap();
    }
}

#[test]
fn two_unix_peers_keep_authoritative_bytes_and_healthy_cache_through_recovery() {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "ramshared-group-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let owned = OwnedFile(path);
    let mut original = vec![0x11; 8192];
    original[4096..].fill(0x22);
    file.write_all_at(&original, 0).unwrap();
    file.sync_all().unwrap();
    let (a, first) = peer();
    let (b, second) = peer();
    let cache = CacheGroup::new(
        vec![
            CacheMember::new("b".into(), 4096, b),
            CacheMember::new("a".into(), 4096, a),
        ],
        4096,
        Duration::from_millis(50),
        Duration::from_millis(100),
    )
    .unwrap();
    let backend = AuthoritativeOriginBackend::new_with_admission(
        FileOrigin::from_file(file.try_clone().unwrap()),
        cache,
        8192,
        4096,
        crate::AllowCacheAdmission,
    )
    .unwrap();
    let mut actual = vec![0; 8192];
    backend.read_at(0, &mut actual).unwrap();
    backend.read_at(0, &mut actual).unwrap();
    assert_eq!(actual, original);
    let hits = backend.telemetry().cache_hits;
    assert!(hits > 0, "warm reads never used the two IPC peers");
    second.socket.shutdown(Shutdown::Both).unwrap();
    backend.read_at(0, &mut actual).unwrap();
    assert_eq!(
        actual, original,
        "a partial cache hit escaped origin fallback"
    );
    assert_eq!(
        backend.cache_state(),
        CacheState::Restricted,
        "one cache peer fault revoked its healthy peer"
    );
    let reads_before = first
        .trace
        .lock()
        .unwrap()
        .iter()
        .filter(|kind| **kind == MSG_READ_REQ)
        .count();
    backend.read_at(0, &mut actual[..4096]).unwrap();
    assert_eq!(&actual[..4096], &original[..4096]);
    assert_eq!(
        first
            .trace
            .lock()
            .unwrap()
            .iter()
            .filter(|kind| **kind == MSG_READ_REQ)
            .count(),
        reads_before + 1
    );
    assert!(!first.trace.lock().unwrap().contains(&MSG_DISABLE_REQ));

    let (replacement, third) = peer();
    backend
        .try_control_cache(|group| group.control_member(1).unwrap().replace(replacement))
        .unwrap();
    backend.read_at(0, &mut actual).unwrap();
    backend.read_at(0, &mut actual).unwrap();
    assert_eq!(actual, original);
    assert_eq!(backend.cache_state(), CacheState::Active);
    let updated = vec![0x67; 8192];
    backend
        .write_at_with_options(0, &updated, WriteOptions { fua: true })
        .unwrap();
    backend.flush().unwrap();
    backend.read_at(0, &mut actual).unwrap();
    assert_eq!(actual, updated, "post-write cache returned pre-write bytes");
    file.read_exact_at(&mut actual, 0).unwrap();
    assert_eq!(
        actual, updated,
        "SSD origin differed after recovery and FUA"
    );
    backend.release_cache().unwrap();
    assert_eq!(backend.cache_state(), CacheState::Off);
    drop(backend);
    drop(first);
    drop(second);
    drop(third);
    drop(file);
    drop(owned);
}
