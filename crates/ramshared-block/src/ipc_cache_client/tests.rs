// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/ipc_cache_client/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "tests/deferred_invalidation.rs"]
mod deferred_invalidation;
#[path = "tests/response_failures.rs"]
mod response_failures;
#[path = "tests/support.rs"]
mod support;
#[path = "tests/wire_and_telemetry.rs"]
mod wire_and_telemetry;

use super::*;

pub(super) use std::time::Instant;
use support::*;

#[test]
fn read_timeout_falls_back_cleanly() {
    let (client_sock, _hung_worker) = UnixStream::pair().expect("socketpair failed");
    // Use a short read timeout for the test to avoid slowing down CI
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(20), 1024 * 1024);
    assert_eq!(client.state(), CacheState::Active);

    let mut buf = [0u8; 128];
    let outcome = client.read(0, &mut buf);

    // Hung worker causes timeout -> marks unavailable and returns Failed
    assert_eq!(outcome, CacheRead::Failed);
    assert_eq!(client.state(), CacheState::Unavailable);

    // Subsequent reads immediately return Miss without waiting
    let start = Instant::now();
    let second_outcome = client.read(0, &mut buf);
    assert_eq!(second_outcome, CacheRead::Miss);
    assert!(start.elapsed() < Duration::from_millis(5));
}

#[test]
fn trickled_response_cannot_extend_the_absolute_read_deadline() {
    let (client_sock, mut worker_sock) = UnixStream::pair().expect("socketpair failed");
    let timeout = Duration::from_millis(30);
    let worker = std::thread::spawn(move || {
        let mut request = [0u8; FRAME_HEADER_LEN];
        worker_sock.read_exact(&mut request).unwrap();
        let mut written = 0;
        for _ in 0..FRAME_HEADER_LEN {
            if worker_sock.write_all(&[0]).is_err() {
                break;
            }
            written += 1;
            std::thread::sleep(Duration::from_millis(8));
        }
        written
    });
    let mut client = IpcCacheClient::new(client_sock, timeout, 1024 * 1024);

    let start = Instant::now();
    let outcome = client.read(0, &mut [0u8; 16]);
    let elapsed = start.elapsed();

    assert_eq!(outcome, CacheRead::Failed);
    assert_eq!(client.state(), CacheState::Unavailable);
    assert!(
        elapsed < Duration::from_millis(180),
        "30ms cache read exceeded absolute deadline by too much: {elapsed:?}"
    );
    assert!(worker.join().unwrap() < FRAME_HEADER_LEN);
}

#[test]
fn socket_disconnect_marks_unavailable() {
    let (client_sock, worker_sock) = UnixStream::pair().expect("socketpair failed");
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(50), 1024 * 1024);
    assert_eq!(client.state(), CacheState::Active);

    // Abrupt worker crash closes socket
    drop(worker_sock);

    let mut buf = [0u8; 128];
    let outcome = client.read(0, &mut buf);
    assert_eq!(outcome, CacheRead::Failed);
    assert_eq!(client.state(), CacheState::Unavailable);
    assert_eq!(client.cached_bytes(), 0);

    // Mutations on disconnected client return Skipped
    let mut_outcome = client.update(0, &[1, 2, 3]);
    assert_eq!(mut_outcome, CacheMutation::Skipped);
}

#[test]
fn small_update_and_promote_complete_within_the_deadline() {
    let (client_sock, _worker_sock) = UnixStream::pair().expect("socketpair failed");
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(50), 1024 * 1024);

    let start = Instant::now();
    let payload = vec![0xAB; 4096];
    let update_outcome = client.update(0, &payload);
    let promote_outcome = client.promote(4096, &payload);

    assert!(start.elapsed() < Duration::from_millis(50));
    assert_eq!(update_outcome, CacheMutation::Accepted);
    assert_eq!(promote_outcome, CacheMutation::Accepted);
    assert_eq!(
        client.cached_bytes(),
        0,
        "queued bytes are not physical allocations"
    );
}

/// A saturated socket must not consume the origin thread and must not
/// revoke the cache: backpressure queues frames (DT-10), it is not a
/// broken peer.
#[test]
fn saturated_mutation_socket_does_not_block_origin_thread() {
    let (client_sock, _worker_sock) = UnixStream::pair().expect("socketpair failed");
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(250), 1024 * 1024);
    saturate_with_updates(&mut client);
    let payload = vec![0xCD; MAX_MUTATION_FRAME_DATA_BYTES];

    let start = Instant::now();
    for round in 0..8 {
        assert_eq!(
            client.update(round as u64, &payload),
            CacheMutation::Accepted
        );
    }
    let elapsed = start.elapsed();

    assert_eq!(
        client.state(),
        CacheState::Active,
        "backpressure must not revoke the cache"
    );
    assert!(
        elapsed < Duration::from_millis(100),
        "a blocked mutation consumed the origin thread for {elapsed:?}"
    );
}

/// A short write is not a fatal error. The unwritten tail waits in the
/// pending queue and is completed by a later drain, so the byte stream
/// stays framed and the cache stays alive (DT-10).
#[test]
fn partial_mutation_write_completes_the_frame_and_keeps_the_cache_active() {
    let (client_sock, mut worker_sock) = UnixStream::pair().expect("socketpair failed");
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(50), 1024 * 1024);
    let whole_frames = saturate_with_updates(&mut client);
    assert!(
        !client.pending_frame.is_empty(),
        "saturation must leave an unwritten tail, not an abandoned frame"
    );
    assert!(
        client.pending_frame.len() < FRAME_HEADER_LEN + MAX_MUTATION_FRAME_DATA_BYTES,
        "the tail must be a fragment of one frame"
    );
    assert_eq!(
        client.state(),
        CacheState::Active,
        "a partial write must not revoke the cache"
    );

    // Interleave peer reads with client drains until the tail completes.
    worker_sock.set_nonblocking(true).unwrap();
    let frame_len = FRAME_HEADER_LEN + MAX_MUTATION_FRAME_DATA_BYTES;
    // Every accepted update is one whole frame: the last is split across
    // the wire and the pending tail, so the drained stream is exactly
    // `whole_frames` frames long.
    let expected = whole_frames * frame_len;
    let mut collected = Vec::with_capacity(expected);
    let mut buffer = [0u8; 8192];
    while collected.len() < expected {
        match worker_sock.read(&mut buffer) {
            Ok(0) => panic!("peer closed before the mutation frame completed"),
            Ok(read) => collected.extend_from_slice(&buffer[..read]),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                try_drain_pending(&mut client.socket, &mut client.pending_frame)
                    .expect("drain must not hit a hard error");
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => panic!("peer read failed: {error}"),
        }
    }

    assert_eq!(
        client.pending_frame.len(),
        0,
        "the pending tail must drain to nothing"
    );
    assert_eq!(client.state(), CacheState::Active);
    // Saturation used only `MSG_UPDATE` frames of one fixed payload length,
    // so the stream must be a clean run of whole frames with no holes.
    assert_eq!(collected.len() % frame_len, 0);
    for (index, frame) in collected.chunks_exact(frame_len).enumerate() {
        let mut header_bytes = [0u8; FRAME_HEADER_LEN];
        header_bytes.copy_from_slice(&frame[..FRAME_HEADER_LEN]);
        let header = FrameHeader::decode(&header_bytes).expect("valid header");
        assert_eq!(header.msg_type, MSG_UPDATE);
        assert_eq!(header.correlation_id as usize, index + 1);
        assert_eq!(header.payload_len as usize, MAX_MUTATION_FRAME_DATA_BYTES);
        assert_eq!(
            &frame[FRAME_HEADER_LEN..],
            &[0x5A; MAX_MUTATION_FRAME_DATA_BYTES]
        );
    }
}

/// A write-mirror that cannot reach the worker must never be silently
/// dropped: the worker would keep pre-write bytes and a later Hit would
/// serve them. While the queue has room the update is queued (DT-10).
#[test]
fn backpressured_update_is_queued_never_dropped() {
    let (client_sock, _worker_sock) = UnixStream::pair().expect("socketpair failed");
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(50), 1024 * 1024);
    saturate_with_updates(&mut client);
    let payload = vec![0x11; MAX_MUTATION_FRAME_DATA_BYTES];
    let frame_len = FRAME_HEADER_LEN + MAX_MUTATION_FRAME_DATA_BYTES;
    let before = client.pending_frame.len();

    for round in 0..3 {
        assert_eq!(
            client.update(round as u64, &payload),
            CacheMutation::Accepted,
            "update {round} must be queued while the backlog has room"
        );
    }
    assert_eq!(client.state(), CacheState::Active);
    assert_eq!(
        client.pending_frame.len(),
        before + 3 * frame_len,
        "every accepted update must sit whole in the pending queue"
    );
}

/// A dropped `Promote` is not stale data: the range is simply not cached
/// and a later read is a miss. It must not revoke the cache (DT-10).
#[test]
fn backpressured_promote_degrades_to_not_cached() {
    let (client_sock, _worker_sock) = UnixStream::pair().expect("socketpair failed");
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(50), 1024 * 1024);
    saturate_with_updates(&mut client);
    let payload = vec![0x22; MAX_MUTATION_FRAME_DATA_BYTES];
    queue_updates_until_one_frame_from_full(&mut client);

    assert_eq!(
        client.promote(0, &payload),
        CacheMutation::Skipped,
        "a promote that cannot be queued is simply not cached"
    );
    assert_eq!(
        client.state(),
        CacheState::Active,
        "a dropped promote must not revoke the cache"
    );
}

/// A request written while a mutation frame is mid-stream would corrupt
/// both frames. The read degrades to a miss — origin is authoritative, so
/// a miss is the correct answer — and the cache stays alive (DT-10).
#[test]
fn cache_read_is_a_miss_while_a_mutation_frame_drains() {
    let (client_sock, _worker_sock) = UnixStream::pair().expect("socketpair failed");
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(50), 1024 * 1024);
    saturate_with_updates(&mut client);
    let pending_before = client.pending_frame.len();
    assert!(pending_before > 0);

    let start = Instant::now();
    assert_eq!(client.read(0, &mut [0u8; 16]), CacheRead::Miss);
    assert!(
        start.elapsed() < Duration::from_millis(10),
        "a draining read must not wait on IPC"
    );
    assert_eq!(
        client.state(),
        CacheState::Active,
        "degrading to a miss must keep the cache alive"
    );
    assert_eq!(
        client.pending_frame.len(),
        pending_before,
        "a deferred read must leave the mutation backlog untouched"
    );
}

/// The pending backlog is bounded. An `Update` that cannot enter it is
/// converted to `MSG_INVALIDATE` (DT-11) and the queue never exceeds the
/// cap — the conversion is not a licence to grow past it.
#[test]
fn pending_mutation_backlog_is_bounded() {
    let (client_sock, _worker_sock) = UnixStream::pair().expect("socketpair failed");
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(50), 1024 * 1024);
    saturate_with_updates(&mut client);
    fill_pending_to(&mut client, MAX_PENDING_MUTATION_BYTES - FRAME_HEADER_LEN);
    let payload = vec![0x44; MAX_MUTATION_FRAME_DATA_BYTES];

    assert_eq!(
        client.update(0, &payload),
        CacheMutation::Skipped,
        "a write-mirror that cannot be queued is not silently accepted"
    );
    assert_eq!(
        client.pending_frame.len(),
        MAX_PENDING_MUTATION_BYTES,
        "the invalidate fits exactly in the remaining headroom and never grows the cap"
    );
    assert!(client.pending_frame.len() <= MAX_PENDING_MUTATION_BYTES);
    assert_eq!(
        client.state(),
        CacheState::Active,
        "the converted invalidate keeps the session alive (DT-11)"
    );
}

/// A backpressured `Update` is converted into `MSG_INVALIDATE` for exactly
/// its range and the cache session stays `Active` (DT-11). The worker
/// therefore drops pre-write coverage there and a later read is a miss,
/// without throwing away every unrelated hot range.
#[test]
fn backpressured_update_invalidates_its_range_and_stays_active() {
    let (client_sock, _worker_sock) = UnixStream::pair().expect("socketpair failed");
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(50), 1024 * 1024);
    saturate_with_updates(&mut client);
    // Leave exactly one header of headroom: the dropped 64 KiB write-mirror
    // cannot fit, but its 32-byte invalidate can.
    fill_pending_to(&mut client, MAX_PENDING_MUTATION_BYTES - FRAME_HEADER_LEN);
    let payload = vec![0x44; MAX_MUTATION_FRAME_DATA_BYTES];
    let before = client.pending_frame.len();

    assert_eq!(
        client.update(0x2000, &payload),
        CacheMutation::Skipped,
        "a write-mirror that cannot be queued is not cached"
    );
    assert_eq!(
        client.state(),
        CacheState::Active,
        "the invalidate keeps the cache session alive (DT-11)"
    );
    assert_eq!(
        client.pending_frame.len(),
        before + FRAME_HEADER_LEN,
        "only the 32-byte invalidate may join the queue"
    );

    let mut header_bytes = [0u8; FRAME_HEADER_LEN];
    header_bytes.copy_from_slice(&client.pending_frame[before..before + FRAME_HEADER_LEN]);
    let header = FrameHeader::decode(&header_bytes).expect("valid invalidate header");
    assert_eq!(header.msg_type, MSG_INVALIDATE_REQ);
    assert_eq!(
        header.offset, 0x2000,
        "the invalidate covers the dropped range start"
    );
    assert_eq!(
        header.aux as usize, MAX_MUTATION_FRAME_DATA_BYTES,
        "the invalidate covers the dropped range length"
    );
    assert_eq!(header.payload_len, 0, "an invalidate carries no payload");
}

/// Saturation retains the exact invalidation without admitting stale reads.
#[test]
fn unqueueable_invalidation_is_deferred() {
    let (client_sock, _worker_sock) = UnixStream::pair().unwrap();
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(50), 1024 * 1024);
    saturate_with_updates(&mut client);
    fill_pending_to(&mut client, MAX_PENDING_MUTATION_BYTES);
    assert_eq!(client.update(0x2000, &[0x44; 4096]), CacheMutation::Skipped);
    assert_eq!(client.state(), CacheState::Active);
    assert_eq!(client.read(0x2000, &mut [0; 16]), CacheRead::Miss);
}

#[test]
fn paced_write_defers_its_unqueueable_invalidation() {
    let (client_sock, _worker_sock) = UnixStream::pair().unwrap();
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(50), 1024 * 1024);
    saturate_with_updates(&mut client);
    fill_pending_to(&mut client, MAX_PENDING_MUTATION_BYTES);
    client.pacing = PacingPolicy::default();
    client.mirror_pause_until = Instant::now().checked_add(WRITE_MIRROR_PAUSE);
    assert_eq!(client.update(0x2000, &[0x44; 4096]), CacheMutation::Skipped);
    assert_eq!(client.state(), CacheState::Active);
}

#[test]
fn oversize_mutation_disables_cache_without_touching_ipc() {
    let (client_sock, mut worker_sock) = UnixStream::pair().expect("socketpair failed");
    worker_sock.set_nonblocking(true).unwrap();
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(50), 1024 * 1024);
    let payload = vec![0xEE; MAX_MUTATION_FRAME_DATA_BYTES + 1];

    assert_eq!(client.update(0, &payload), CacheMutation::Failed);
    assert_eq!(client.state(), CacheState::Unavailable);
    let mut byte = [0u8; 1];
    match worker_sock.read(&mut byte) {
        Ok(0) => {}
        Err(error) if error.kind() == ErrorKind::WouldBlock => {}
        other => panic!("oversize mutation unexpectedly reached IPC: {other:?}"),
    }
}

#[test]
fn invalid_timeout_configuration_disables_cache_before_io() {
    let (client_sock, _worker_sock) = UnixStream::pair().expect("socketpair failed");
    let mut client = IpcCacheClient::new(client_sock, Duration::ZERO, 1024 * 1024);

    assert_eq!(client.state(), CacheState::Unavailable);
    assert!(client.perform_handshake().is_err());
    assert_eq!(client.read(0, &mut [0u8; 16]), CacheRead::Miss);
}

/// A mutation must not zero the last confirmed occupancy sample.
#[test]
fn mutation_preserves_last_confirmed_occupancy() {
    let (client_sock, _worker_sock) = UnixStream::pair().expect("socketpair failed");
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(50), 1024 * 1024);
    // Handshake response reports 64 KiB already cached.
    client.observe_cached_kib(64);

    assert_eq!(client.update(0, &[1, 2, 3]), CacheMutation::Accepted);
    assert_eq!(
        client.cached_bytes(),
        64 << 10,
        "a queued mutation is not evidence of allocation and must not clear the sample"
    );
}

/// A heartbeat issued into a write-mirror backlog would queue behind the
/// frames and miss NFR-1's 50 ms bound, which fail-closes the cache. The
/// client skips the round-trip and reports the last confirmed sample.
#[test]
fn heartbeat_is_deferred_while_write_mirrors_drain() {
    let (client_sock, mut worker_sock) = UnixStream::pair().expect("socketpair failed");
    worker_sock.set_nonblocking(true).unwrap();
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(50), 1024 * 1024);
    client.observe_cached_kib(64);

    assert_eq!(client.update(0, &[1, 2, 3]), CacheMutation::Accepted);
    let start = Instant::now();
    assert_eq!(client.refresh_cached_bytes(), Ok(64 << 10));
    assert!(start.elapsed() < Duration::from_millis(10));
    assert_eq!(client.state(), CacheState::Active);
    // Only the mutation frame reached the worker; no heartbeat followed.
    let drained = drain_available(&mut worker_sock);
    assert!(!drained.is_empty(), "the mutation frame must be queued");
    assert_eq!(
        worker_sock.read(&mut [0u8; 1]).unwrap_err().kind(),
        ErrorKind::WouldBlock,
        "a heartbeat must not be queued into a write-mirror backlog"
    );
}

/// Telemetry round-trips are spaced so the serve loop cannot issue one
/// heartbeat per NBD request.
#[test]
fn heartbeat_is_spaced_to_one_per_interval() {
    let (client_sock, mut worker_sock) = UnixStream::pair().expect("socketpair failed");
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(50), 1024 * 1024);
    // Prequeue the matching response so this spacing-policy test does not
    // depend on scheduling a stub thread inside the production I/O deadline.
    let response = FrameHeader {
        msg_type: MSG_HEARTBEAT_RESP,
        status: STATUS_OK,
        correlation_id: 1,
        offset: 1024 * 1024,
        payload_len: 0,
        aux: 64,
    };
    worker_sock.write_all(&response.encode()).unwrap();

    assert_eq!(client.refresh_cached_bytes(), Ok(64 << 10));
    // Second call inside the interval returns the last confirmed sample.
    assert_eq!(client.refresh_cached_bytes(), Ok(64 << 10));
    assert_eq!(client.state(), CacheState::Active);
    drop(client);
    let mut request = [0u8; FRAME_HEADER_LEN];
    worker_sock.read_exact(&mut request).unwrap();
    let request = FrameHeader::decode(&request).unwrap();
    assert_eq!(request.msg_type, MSG_HEARTBEAT_REQ);
    assert_eq!(request.correlation_id, response.correlation_id);
    assert_eq!(
        worker_sock.read(&mut [0u8; 1]).unwrap(),
        0,
        "a second heartbeat must not reach the worker inside the interval"
    );
}

/// A cache read queued behind a write-mirror burst would miss NFR-1's
/// bound and revoke the cache. It is deferred as a miss instead.
#[test]
fn cache_read_is_deferred_while_write_mirrors_drain() {
    let (client_sock, mut worker_sock) = UnixStream::pair().expect("socketpair failed");
    worker_sock.set_nonblocking(true).unwrap();
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(50), 1024 * 1024);

    assert_eq!(client.update(0, &[1, 2, 3]), CacheMutation::Accepted);
    let mut destination = [0u8; 16];
    let start = Instant::now();
    assert_eq!(client.read(0, &mut destination), CacheRead::Miss);
    assert!(start.elapsed() < Duration::from_millis(10));
    assert_eq!(client.state(), CacheState::Active);
    // Drain the mutation frame; no read request may follow it.
    let drained = drain_available(&mut worker_sock);
    assert!(!drained.is_empty());
    assert_eq!(
        worker_sock.read(&mut [0u8; 1]).unwrap_err().kind(),
        ErrorKind::WouldBlock,
        "a cache read must not be queued into a write-mirror backlog"
    );
}

/// Promotes come from the read path itself and must not suppress the very
/// reads that produce them, or a cold read stream would never hit.
#[test]
fn promote_does_not_defer_subsequent_reads() {
    let (client_sock, mut worker_sock) = UnixStream::pair().expect("socketpair failed");
    worker_sock.set_nonblocking(true).unwrap();
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(50), 1024 * 1024);

    assert_eq!(client.promote(0, &[1, 2, 3]), CacheMutation::Accepted);
    // The read is still issued: promotes never gate the read path. The
    // stub worker never answers, so the read hits the 50 ms deadline and
    // fail-closes — proving the request left the client (a deferred read
    // returns Miss without any I/O).
    let mut destination = [0u8; 16];
    assert_eq!(client.read(0, &mut destination), CacheRead::Failed);
    assert_eq!(client.state(), CacheState::Unavailable);
    let drained = drain_available(&mut worker_sock);
    assert!(
        drained.len() > FRAME_HEADER_LEN,
        "both the promote frame and the read request must reach the worker"
    );
}

#[test]
fn codec_timeout_falls_back_to_origin() {
    use crate::IoError;
    use crate::isolated_origin::AuthoritativeOriginBackend;
    use crate::origin_cache::OriginStorage;

    struct MemoryOrigin(std::cell::RefCell<Vec<u8>>);
    impl OriginStorage for MemoryOrigin {
        fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<usize, IoError> {
            let start = off as usize;
            let end = (start + buf.len()).min(self.0.borrow().len());
            if start >= self.0.borrow().len() {
                return Ok(0);
            }
            buf[..end - start].copy_from_slice(&self.0.borrow()[start..end]);
            Ok(end - start)
        }
        fn write_at(&self, off: u64, data: &[u8]) -> Result<usize, IoError> {
            let start = off as usize;
            let end = start + data.len();
            if end > self.0.borrow().len() {
                self.0.borrow_mut().resize(end, 0);
            }
            self.0.borrow_mut()[start..end].copy_from_slice(data);
            Ok(data.len())
        }
        fn sync_data(&self) -> Result<(), IoError> {
            Ok(())
        }
    }

    // A worker that accepts the read request and never answers: the decoder
    // is stuck in a driver call that DT-3 explicitly cannot preempt.
    let (client_sock, mut worker_sock) = UnixStream::pair().expect("socketpair failed");
    let stalled = std::thread::spawn(move || {
        let mut request = [0u8; FRAME_HEADER_LEN];
        if worker_sock.read_exact(&mut request).is_ok() {
            // Hold the stream open and never respond.
            std::thread::sleep(Duration::from_millis(200));
        }
    });

    let client = IpcCacheClient::new(client_sock, Duration::from_millis(30), 1024 * 1024);
    let backend = AuthoritativeOriginBackend::new_with_admission(
        MemoryOrigin(std::cell::RefCell::new(b"origin!!".to_vec())),
        client,
        8,
        4,
        crate::isolated_origin::AllowCacheAdmission,
    )
    .expect("origin backend");

    let start = Instant::now();
    let mut destination = [0u8; 8];
    backend.read_at(0, &mut destination).expect("origin read");
    let elapsed = start.elapsed();

    // The origin is authoritative: the reader gets the durable bytes, and
    // the stalled cache path did not extend the cache read deadline.
    assert_eq!(&destination, b"origin!!");
    assert!(
        elapsed < Duration::from_millis(150),
        "a stalled decoder must fall back to the origin within the cache deadline, not hang ({elapsed:?})"
    );
    assert_eq!(backend.telemetry().fallback_reads, 1);
    stalled.join().unwrap();
}

#[test]
fn default_read_timeout_covers_codec_latency_and_caps_promotes() {
    assert!(DEFAULT_READ_TIMEOUT >= Duration::from_millis(200));
    assert_eq!(MAX_UNACKED_PROMOTE_FRAMES, 4);

    let (client_sock, mut worker_sock) = UnixStream::pair().expect("socketpair failed");
    let worker = std::thread::spawn(move || {
        let mut hdr = [0u8; FRAME_HEADER_LEN];
        worker_sock.read_exact(&mut hdr).unwrap();
        let frame = FrameHeader::decode(&hdr).unwrap();
        assert_eq!(frame.msg_type, MSG_READ_REQ);
        std::thread::sleep(Duration::from_millis(75));
        let resp = FrameHeader {
            msg_type: MSG_READ_RESP,
            status: STATUS_OK,
            correlation_id: frame.correlation_id,
            offset: frame.offset,
            payload_len: 4,
            aux: 64,
        };
        worker_sock.write_all(&resp.encode()).unwrap();
        worker_sock.write_all(b"pong").unwrap();
    });

    let mut client = IpcCacheClient::new(client_sock, DEFAULT_READ_TIMEOUT, 1024 * 1024);
    let mut out = [0u8; 4];
    assert_eq!(client.read(0, &mut out), CacheRead::Hit);
    assert_eq!(&out, b"pong");
    assert_eq!(client.state(), CacheState::Active);
    worker.join().unwrap();
}
