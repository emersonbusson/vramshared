// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/ipc_cache_client/tests/deferred_invalidation.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use std::sync::mpsc;

#[test]
fn saturated_invalidations_defer_reads_and_recover() {
    let (client_sock, mut worker_sock) = UnixStream::pair().unwrap();
    let mut client = IpcCacheClient::new(client_sock, Duration::from_secs(1), 1024 * 1024);
    saturate_with_updates(&mut client);
    fill_pending_to(&mut client, MAX_PENDING_MUTATION_BYTES);
    assert_eq!(client.update(8192, &[1; 4096]), CacheMutation::Skipped);
    assert_eq!(client.state(), CacheState::Active);
    assert_eq!(client.read(8192, &mut [0; 16]), CacheRead::Miss);
    assert_eq!(client.update(12288, &[2; 4096]), CacheMutation::Skipped);
    assert_eq!(client.promote(8192, &[3; 16]), CacheMutation::Skipped);
    client.observe_cached_kib(1);
    assert!(client.refresh_cached_bytes().is_ok());
    assert!(client.slab_telemetry().is_err());
    assert_eq!(client.state(), CacheState::Active);

    let (sent, received) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut invalidated = false;
        loop {
            let mut raw = [0u8; FRAME_HEADER_LEN];
            if worker_sock.read_exact(&mut raw).is_err() {
                break;
            }
            let frame = FrameHeader::decode(&raw).unwrap();
            let mut payload = vec![0; frame.payload_len as usize];
            worker_sock.read_exact(&mut payload).unwrap();
            match frame.msg_type {
                MSG_UPDATE | MSG_PROMOTE => {}
                MSG_INVALIDATE_REQ => {
                    invalidated = frame.offset <= 8192
                        && frame.offset.saturating_add(u64::from(frame.aux)) >= 16384;
                    sent.send(("invalidate", invalidated)).unwrap();
                }
                MSG_READ_REQ => {
                    let mut response =
                        read_hit_response(frame.correlation_id, frame.offset, &[0x99; 16]);
                    if invalidated {
                        response.status = STATUS_MISS;
                        response.payload_len = 0;
                    }
                    worker_sock.write_all(&response.encode()).unwrap();
                    if !invalidated {
                        worker_sock.write_all(&[0x99; 16]).unwrap();
                    }
                    sent.send(("read", invalidated)).unwrap();
                }
                _ => panic!("unexpected frame"),
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            Instant::now() < deadline,
            "invalidation never reached worker"
        );
        assert_ne!(client.read(8192, &mut [0; 16]), CacheRead::Hit);
        if let Ok(event) = received.try_recv() {
            assert_eq!(event, ("invalidate", true));
            break;
        }
        std::thread::yield_now();
    }
    assert_eq!(client.read(8192, &mut [0; 16]), CacheRead::Miss);
    assert_eq!(
        received.recv_timeout(Duration::from_secs(2)).unwrap(),
        ("read", true)
    );
    assert_eq!(client.state(), CacheState::Active);
    drop(client);
    worker.join().unwrap();
}

#[test]
fn deferred_invalidation_peer_loss_revokes_cache() {
    let (client_sock, worker_sock) = UnixStream::pair().unwrap();
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(50), 1024 * 1024);
    saturate_with_updates(&mut client);
    fill_pending_to(&mut client, MAX_PENDING_MUTATION_BYTES);
    assert_eq!(client.update(0, &[1; 4096]), CacheMutation::Skipped);
    drop(worker_sock);
    assert_eq!(client.read(0, &mut [0; 16]), CacheRead::Failed);
    assert_eq!(client.state(), CacheState::Unavailable);
}

#[test]
fn deferred_range_capacity_fails_closed() {
    let (client_sock, _worker_sock) = UnixStream::pair().unwrap();
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(50), 1024 * 1024);
    saturate_with_updates(&mut client);
    fill_pending_to(&mut client, MAX_PENDING_MUTATION_BYTES);
    let max_ranges = super::super::deferred_invalidations::MAX_RANGES as u64;
    for index in 0..max_ranges {
        assert_eq!(
            client.update(index * 8192, &[1; 4096]),
            CacheMutation::Skipped
        );
    }
    assert_eq!(
        client.update(max_ranges * 8192, &[1; 4096]),
        CacheMutation::Failed
    );
    assert_eq!(client.state(), CacheState::Unavailable);
    assert_eq!(client.read(0, &mut [0; 16]), CacheRead::Miss);
}

#[test]
fn overflowing_deferred_range_revokes_cache() {
    let (client_sock, _worker_sock) = UnixStream::pair().unwrap();
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(50), 1024 * 1024);
    saturate_with_updates(&mut client);
    fill_pending_to(&mut client, MAX_PENDING_MUTATION_BYTES);
    assert_eq!(
        client.update(u64::MAX - 1, &[1; 4096]),
        CacheMutation::Failed
    );
    assert_eq!(client.state(), CacheState::Unavailable);
}

#[test]
fn disjoint_large_offsets_drain_in_bounded_batches_before_reads() {
    let (client_sock, mut worker_sock) = UnixStream::pair().unwrap();
    let mut client = IpcCacheClient::new(client_sock, Duration::from_secs(1), 16 * 1024 * 1024);
    saturate_with_updates(&mut client);
    fill_pending_to(&mut client, MAX_PENDING_MUTATION_BYTES);
    let base = 8u64 << 30;
    for index in 0..16 {
        assert_eq!(
            client.update(base + index * 8192, &[1; 4096]),
            CacheMutation::Skipped
        );
    }
    let (sent, received) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut seen = 0u32;
        loop {
            let mut raw = [0u8; FRAME_HEADER_LEN];
            if worker_sock.read_exact(&mut raw).is_err() {
                break;
            }
            let frame = FrameHeader::decode(&raw).unwrap();
            let mut payload = vec![0; frame.payload_len as usize];
            worker_sock.read_exact(&mut payload).unwrap();
            match frame.msg_type {
                MSG_UPDATE | MSG_PROMOTE => {}
                MSG_INVALIDATE_REQ => {
                    assert_eq!(frame.aux, 4096);
                    let index = (frame.offset - base) / 8192;
                    assert!(index < 16);
                    seen |= 1 << index;
                    if seen == 0xffff {
                        sent.send(true).unwrap();
                    }
                }
                MSG_READ_REQ => {
                    let all_invalidated = seen == 0xffff;
                    let mut response =
                        read_hit_response(frame.correlation_id, frame.offset, &[0x99; 16]);
                    if all_invalidated {
                        response.status = STATUS_MISS;
                        response.payload_len = 0;
                    }
                    worker_sock.write_all(&response.encode()).unwrap();
                    if !all_invalidated {
                        worker_sock.write_all(&[0x99; 16]).unwrap();
                    }
                }
                _ => panic!("unexpected frame"),
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(Instant::now() < deadline, "ranges did not drain");
        assert_ne!(client.read(base, &mut [0; 16]), CacheRead::Hit);
        if received.try_recv().is_ok() {
            break;
        }
        std::thread::yield_now();
    }
    assert_eq!(client.read(base, &mut [0; 16]), CacheRead::Miss);
    assert_eq!(client.state(), CacheState::Active);
    drop(client);
    worker.join().unwrap();
}
