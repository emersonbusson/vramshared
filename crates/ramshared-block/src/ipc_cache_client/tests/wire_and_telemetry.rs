// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/ipc_cache_client/tests/wire_and_telemetry.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::super::*;
use super::support::*;

/// SPEC §12 · ITEM-1 · AUD-34 · invariant check.
///
/// A response is an answer only when it answers *this* request. The
/// matching frame is accepted; the same frame with a foreign offset is
/// refused and revokes the session instead of being interpreted as this
/// range's bytes.
#[test]
fn ipc_rejects_wrong_identity_and_accepts_matching_response() {
    let payload = b"WRNG".to_vec();

    // Matching response: accepted as a Hit with the right bytes.
    let (client_sock, worker_sock) = UnixStream::pair().expect("socketpair failed");
    let good = payload.clone();
    let worker = std::thread::spawn(move || {
        serve_one_response(worker_sock, read_hit_response(1, 0, &good), good).unwrap();
    });
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(200), 1024 * 1024);
    let mut buf = [0u8; 4];
    assert_eq!(client.read(0, &mut buf), CacheRead::Hit);
    assert_eq!(&buf, b"WRNG");
    worker.join().unwrap();

    // Same shape, foreign offset: the audit's `WRONG_OFFSET_READ=Hit`.
    let (client_sock, worker_sock) = UnixStream::pair().expect("socketpair failed");
    let wrong = payload.clone();
    let worker = std::thread::spawn(move || {
        serve_one_response(worker_sock, read_hit_response(1, 4096, &wrong), wrong).unwrap();
    });
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(200), 1024 * 1024);
    assert_eq!(
        client.read(0, &mut buf),
        CacheRead::Failed,
        "a response for another offset must not answer this read"
    );
    assert_eq!(client.state(), CacheState::Unavailable);
    worker.join().unwrap();
}

/// AUD-34 · invariant check. The worker echoes the requested offset.
#[test]
fn ipc_read_rejects_wrong_offset() {
    let (client_sock, worker_sock) = UnixStream::pair().expect("socketpair failed");
    let bytes = b"WRNG".to_vec();
    let payload = bytes.clone();
    let worker = std::thread::spawn(move || {
        serve_one_response(worker_sock, read_hit_response(1, 4096, &payload), payload).unwrap();
    });
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(200), 1024 * 1024);
    let mut buf = [0u8; 4];
    assert_eq!(client.read(0, &mut buf), CacheRead::Failed);
    assert_ne!(
        &buf, b"WRNG",
        "foreign bytes must never be exposed as this range"
    );
    worker.join().unwrap();
}

/// AUD-34 · invariant check. A MISS is empty; a payload would stay on the
/// stream and corrupt the next frame.
#[test]
fn ipc_miss_rejects_nonempty_payload() {
    let (client_sock, worker_sock) = UnixStream::pair().expect("socketpair failed");
    let stray = b"junk".to_vec();
    let payload = stray.clone();
    let worker = std::thread::spawn(move || {
        let resp = FrameHeader {
            msg_type: MSG_READ_RESP,
            status: STATUS_MISS,
            correlation_id: 1,
            offset: 0,
            payload_len: payload.len() as u32,
            aux: 0,
        };
        serve_one_response(worker_sock, resp, payload).unwrap();
    });
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(200), 1024 * 1024);
    let mut buf = [0u8; 4];
    assert_eq!(
        client.read(0, &mut buf),
        CacheRead::Failed,
        "a MISS carrying bytes is malformed, not an answer"
    );
    worker.join().unwrap();

    // The legitimate counterpart: an empty MISS is a Miss.
    let (client_sock, worker_sock) = UnixStream::pair().expect("socketpair failed");
    let worker = std::thread::spawn(move || {
        let resp = FrameHeader {
            msg_type: MSG_READ_RESP,
            status: STATUS_MISS,
            correlation_id: 1,
            offset: 0,
            payload_len: 0,
            aux: 0,
        };
        serve_one_response(worker_sock, resp, Vec::new()).unwrap();
    });
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(200), 1024 * 1024);
    assert_eq!(client.read(0, &mut buf), CacheRead::Miss);
    worker.join().unwrap();
}

#[test]
fn ipc_disable_rejects_wrong_correlation() {
    let (client_sock, worker_sock) = UnixStream::pair().expect("socketpair failed");
    let worker = std::thread::spawn(move || {
        serve_one_response(worker_sock, disable_response(99, 0, 0, 0), Vec::new()).unwrap();
    });
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(200), 1024 * 1024);
    assert_eq!(client.disable(), CacheMutation::Failed);
    assert_eq!(client.state(), CacheState::Stuck);
    worker.join().unwrap();
}

#[test]
fn ipc_disable_rejects_nonempty_payload() {
    // The audit's fixture: wrong correlation, offset 123 and a nonempty
    // payload were accepted as `Accepted` / `Off`.
    let (client_sock, worker_sock) = UnixStream::pair().expect("socketpair failed");
    let stray = b"junk".to_vec();
    let payload = stray.clone();
    let worker = std::thread::spawn(move || {
        serve_one_response(worker_sock, disable_response(99, 123, 4, 7), payload).unwrap();
    });
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(200), 1024 * 1024);
    assert_eq!(client.disable(), CacheMutation::Failed);
    assert_ne!(client.state(), CacheState::Off);
    worker.join().unwrap();

    // Legitimate counterpart: the exact empty/zeroed contract is accepted.
    let (client_sock, worker_sock) = UnixStream::pair().expect("socketpair failed");
    let worker = std::thread::spawn(move || {
        serve_one_response(worker_sock, disable_response(1, 0, 0, 0), Vec::new()).unwrap();
    });
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(200), 1024 * 1024);
    assert_eq!(client.disable(), CacheMutation::Accepted);
    assert_eq!(client.state(), CacheState::Off);
    worker.join().unwrap();
}

/// AUD-34: a timed-out partially written disable is terminal. Retrying
/// must not replay the already transmitted `pending_frame` from byte zero.
#[test]
fn disable_is_terminal_after_a_failed_write() {
    let (client_sock, worker_sock) = UnixStream::pair().expect("socketpair failed");
    // Peer gone: the disable write fails with the request frame still
    // sitting in `pending_frame`.
    drop(worker_sock);
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(30), 1024 * 1024);
    assert_eq!(client.disable(), CacheMutation::Failed);
    assert_eq!(client.state(), CacheState::Stuck);

    // A second disable must not touch the socket again: re-sending the
    // buffer would replay bytes from byte zero.
    assert_eq!(client.disable(), CacheMutation::Failed);
    assert_eq!(client.state(), CacheState::Stuck);
}

/// SPEC §12 · ITEM-1 · AUD-35 · invariant check.
///
/// A handshake response reporting zero admitted capacity must never leave
/// the client Active with the figure it asked for. The audit's
/// reproduction was `ZERO_TARGET_HANDSHAKE=Ok(()) STATE=Active TARGET=8192`
/// against a request of 8192.
#[test]
fn zero_handshake_target_never_enters_active() {
    let (client, outcome) = handshake_with(8192, handshake_response(1, STATUS_OK, 0));
    assert_eq!(
        outcome,
        Ok(()),
        "a zero admission is a refusal, not a fault"
    );
    assert_eq!(
        client.state(),
        CacheState::Unavailable,
        "zero admitted capacity must not report Active"
    );
    assert_eq!(
        BestEffortCache::target_bytes(&client),
        0,
        "the requested figure must not survive a zero admission"
    );
}

/// AUD-35 · invariant check. Requested and admitted capacity are distinct at
/// the reporting surface.
#[test]
fn zero_worker_target_never_reports_active_capacity() {
    // Positive admission: the worker's figure wins over the request.
    let (client, outcome) = handshake_with(8192, handshake_response(1, STATUS_OK, 4096));
    assert_eq!(outcome, Ok(()));
    assert_eq!(client.state(), CacheState::Active);
    assert_eq!(BestEffortCache::target_bytes(&client), 4096);

    // Zero admission against a positive request.
    let (client, outcome) = handshake_with(8192, handshake_response(1, STATUS_OK, 0));
    assert_eq!(outcome, Ok(()));
    assert_eq!(BestEffortCache::target_bytes(&client), 0);
    assert_eq!(client.state(), CacheState::Unavailable);

    // Zero request and zero admission agree.
    let (client, outcome) = handshake_with(0, handshake_response(1, STATUS_OK, 0));
    assert_eq!(outcome, Ok(()));
    assert_eq!(client.state(), CacheState::Unavailable);
    assert_eq!(BestEffortCache::target_bytes(&client), 0);

    // An error status admits nothing and needs no destructive repair.
    let (client, outcome) = handshake_with(8192, handshake_response(1, STATUS_ERROR, 0));
    assert_eq!(outcome, Err("handshake refused".to_string()));
    assert_eq!(client.state(), CacheState::Unavailable);
    assert_eq!(BestEffortCache::target_bytes(&client), 0);
}

#[test]
fn oversized_cache_read_is_miss_before_frame_send() {
    let (client_sock, mut worker_sock) = UnixStream::pair().expect("socketpair failed");
    worker_sock.set_nonblocking(true).unwrap();
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(50), 1024 * 1024);
    let mut destination = vec![0u8; 16 * 1024 * 1024 + 1];

    let start = Instant::now();
    assert_eq!(client.read(0, &mut destination), CacheRead::Miss);
    assert!(start.elapsed() < Duration::from_millis(10));
    assert_eq!(client.state(), CacheState::Active);
    let mut byte = [0u8; 1];
    assert_eq!(
        worker_sock.read(&mut byte).unwrap_err().kind(),
        ErrorKind::WouldBlock,
        "an oversize cache read must never reach the worker"
    );
}

#[test]
fn oversized_heartbeat_telemetry_fails_closed() {
    let (client_sock, mut worker_sock) = UnixStream::pair().expect("socketpair failed");
    let worker = std::thread::spawn(move || {
        let mut request = [0u8; FRAME_HEADER_LEN];
        worker_sock.read_exact(&mut request).unwrap();
        let request = FrameHeader::decode(&request).expect("valid request header");
        let response = FrameHeader {
            msg_type: MSG_HEARTBEAT_RESP,
            status: STATUS_OK,
            correlation_id: request.correlation_id,
            offset: 1024 * 1024,
            payload_len: MAX_GPU_BUDGET_PAYLOAD_BYTES + 1,
            aux: 64,
        };
        worker_sock.write_all(&response.encode()).unwrap();
    });
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(50), 1024 * 1024);

    assert_eq!(
        client.refresh_cached_bytes(),
        Err("GPU cache worker heartbeat telemetry exceeded its limit")
    );
    assert_eq!(client.state(), CacheState::Unavailable);
    assert_eq!(client.cached_bytes(), 0);
    worker.join().unwrap();
}

#[test]
fn telemetry_envelope_rejects_unknown_version_or_oversize() {
    // Unknown envelope version: the sample is omitted, never accepted and
    // never turned into a client failure. Physical header fields survive.
    let mut unknown =
        WorkerTelemetryEnvelope::new(1_000, None, None, Some(sample_cache_telemetry(4096, 1_000)));
    unknown.schema_version = 99;
    let unknown_payload = serde_json::to_vec(&unknown).unwrap();

    let (client_sock, worker_sock) = UnixStream::pair().expect("socketpair failed");
    let worker = std::thread::spawn(move || {
        serve_heartbeat_payload(worker_sock, unknown_payload, 64);
    });
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(50), 1024 * 1024);
    assert_eq!(client.refresh_cached_bytes(), Ok(64 << 10));
    assert_eq!(
        client.cache_telemetry(),
        None,
        "an unknown envelope version must not be accepted as cache telemetry"
    );
    assert_eq!(client.gpu_budget_telemetry(), None);
    assert_eq!(
        client.state(),
        CacheState::Active,
        "an unknown envelope version is omitted, not a transport failure"
    );
    worker.join().unwrap();

    // Oversize: rejected whole. The client fails closed and never truncates
    // an oversize payload into a plausible-looking sample.
    let (client_sock, mut worker_sock) = UnixStream::pair().expect("socketpair failed");
    let worker = std::thread::spawn(move || {
        let mut request = [0u8; FRAME_HEADER_LEN];
        worker_sock.read_exact(&mut request).unwrap();
        let request = FrameHeader::decode(&request).expect("valid request header");
        let response = FrameHeader {
            msg_type: MSG_HEARTBEAT_RESP,
            status: STATUS_OK,
            correlation_id: request.correlation_id,
            offset: 1024 * 1024,
            payload_len: MAX_GPU_BUDGET_PAYLOAD_BYTES + 1,
            aux: 64,
        };
        worker_sock.write_all(&response.encode()).unwrap();
    });
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(50), 1024 * 1024);
    assert!(client.refresh_cached_bytes().is_err());
    assert_eq!(client.state(), CacheState::Unavailable);
    assert_eq!(client.cache_telemetry(), None);
    worker.join().unwrap();
}

#[test]
fn logical_bytes_never_replace_physical_cached_bytes() {
    // The envelope reports 1 GiB of logical cache occupancy. The frame
    // header reports 64 KiB of physical cached bytes. They must never be
    // swapped: `cached_bytes()` is physical and comes from `aux` alone.
    use crate::isolated_origin::BestEffortCache;

    const LOGICAL: u64 = 1 << 30;
    let envelope = WorkerTelemetryEnvelope::new(
        1_000,
        None,
        None,
        Some(sample_cache_telemetry(LOGICAL, 1_000)),
    );
    let payload = envelope.to_bounded_payload().expect("envelope fits");

    let (client_sock, worker_sock) = UnixStream::pair().expect("socketpair failed");
    let worker = std::thread::spawn(move || {
        serve_heartbeat_payload(worker_sock, payload, 64);
    });
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(50), 1024 * 1024);
    // Refresh through the trait surface the daemon uses, so the forwards
    // stay exercised and cannot silently re-derive either figure.
    assert_eq!(
        BestEffortCache::refresh_cached_bytes(&mut client),
        Ok(64 << 10)
    );

    assert_eq!(
        client.cached_bytes(),
        64 << 10,
        "physical cached bytes must stay the frame-header value"
    );
    let cache = client.cache_telemetry().expect("cache telemetry present");
    assert_eq!(cache.logical_cached_bytes, LOGICAL);
    assert_ne!(
        cache.logical_cached_bytes,
        client.cached_bytes(),
        "logical cache occupancy must never be reported as physical cached bytes"
    );

    // The same separation holds on every read-only trait forward.
    assert_eq!(BestEffortCache::cached_bytes(&client), 64 << 10);
    assert_eq!(BestEffortCache::target_bytes(&client), 1024 * 1024);
    let trait_cache = BestEffortCache::cache_telemetry(&client).expect("trait cache telemetry");
    assert_eq!(trait_cache.logical_cached_bytes, LOGICAL);
    assert_ne!(
        trait_cache.logical_cached_bytes,
        BestEffortCache::cached_bytes(&client)
    );
    assert!(BestEffortCache::gpu_budget_telemetry(&client).is_none());

    worker.join().unwrap();
}

#[test]
fn unknown_physical_occupancy_is_not_reported_as_zero() {
    let (client_sock, worker_sock) = UnixStream::pair().unwrap();
    let worker =
        std::thread::spawn(move || serve_heartbeat_payload(worker_sock, Vec::new(), u32::MAX));
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(50), 1024 * 1024);
    assert_eq!(
        client.refresh_cached_bytes(),
        Err("GPU cache physical occupancy is unknown")
    );
    assert_eq!(client.state(), CacheState::Active);
    worker.join().unwrap();
}

#[test]
fn disable_clears_held_budget_and_cache_telemetry() {
    // A released cache stops being sampled. `refresh_cached_bytes` never
    // heartbeats again once the cache is not `Active`, so any sample left
    // behind freezes at its last live value and is then republished on
    // every status write under a fresh `written_at_unix_ms`. The publisher
    // would advertise a bound budget that nothing has consulted since the
    // release, and only a downstream freshness predicate could catch it.
    const SAMPLED_AT: u64 = 1_000;
    let envelope = WorkerTelemetryEnvelope::new(
        SAMPLED_AT,
        Some(sample_budget_telemetry(SAMPLED_AT)),
        Some(sample_utilization_telemetry(SAMPLED_AT)),
        Some(sample_cache_telemetry(4096, SAMPLED_AT)),
    );
    let payload = envelope.to_bounded_payload().expect("envelope fits");

    let (client_sock, worker_sock) = UnixStream::pair().expect("socketpair failed");
    let worker =
        std::thread::spawn(move || serve_heartbeat_then(worker_sock, payload, STATUS_OK, true));
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(200), 1024 * 1024);

    assert_eq!(client.refresh_cached_bytes(), Ok(64 << 10));
    assert!(
        client.gpu_budget_telemetry().is_some(),
        "the heartbeat must install a budget sample for this test to mean anything"
    );
    assert!(
        client.gpu_utilization_telemetry().is_some(),
        "the heartbeat must install a utilization sample for this test to mean anything"
    );
    assert!(
        client.cache_telemetry().is_some(),
        "the heartbeat must install a cache sample for this test to mean anything"
    );

    assert_eq!(client.disable(), CacheMutation::Accepted);
    assert_eq!(client.state(), CacheState::Off);
    assert_eq!(client.cached_bytes(), 0);
    assert_eq!(
        client.gpu_budget_telemetry(),
        None,
        "a released cache holds no budget sample"
    );
    assert_eq!(
        client.gpu_utilization_telemetry(),
        None,
        "a released cache holds no utilization sample"
    );
    assert_eq!(
        client.cache_telemetry(),
        None,
        "a released cache holds no cache sample"
    );

    // Nothing heartbeats a non-Active cache, so the only way these could
    // reappear is if the transition had left them behind.
    assert_eq!(
        client.refresh_cached_bytes(),
        Err("GPU cache worker is unavailable")
    );
    assert_eq!(client.gpu_budget_telemetry(), None);
    assert_eq!(client.gpu_utilization_telemetry(), None);
    assert_eq!(client.cache_telemetry(), None);
    worker.join().unwrap();
}

#[test]
fn failed_disable_also_clears_held_telemetry() {
    // A peer that disappears mid-disable lands on `Stuck`, not `Off`. The
    // held samples are no more live there than on a clean release.
    const SAMPLED_AT: u64 = 1_000;
    let envelope = WorkerTelemetryEnvelope::new(
        SAMPLED_AT,
        Some(sample_budget_telemetry(SAMPLED_AT)),
        Some(sample_utilization_telemetry(SAMPLED_AT)),
        Some(sample_cache_telemetry(4096, SAMPLED_AT)),
    );
    let payload = envelope.to_bounded_payload().expect("envelope fits");

    let (client_sock, worker_sock) = UnixStream::pair().expect("socketpair failed");
    let worker =
        std::thread::spawn(move || serve_heartbeat_then(worker_sock, payload, STATUS_OK, false));
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(200), 1024 * 1024);

    assert_eq!(client.refresh_cached_bytes(), Ok(64 << 10));
    assert!(client.gpu_budget_telemetry().is_some());
    assert!(client.gpu_utilization_telemetry().is_some());
    assert!(client.cache_telemetry().is_some());

    assert_eq!(client.disable(), CacheMutation::Failed);
    assert_eq!(client.state(), CacheState::Stuck);
    assert_eq!(
        client.gpu_budget_telemetry(),
        None,
        "a failed release is still a release"
    );
    assert_eq!(
        client.gpu_utilization_telemetry(),
        None,
        "a failed release is still a release"
    );
    assert_eq!(client.cache_telemetry(), None);
    worker.join().unwrap();
}

#[test]
fn heartbeat_telemetry_truncation_fails_closed() {
    // The worker advertises a payload and then never sends it. The client
    // must fail closed rather than accept a partial sample.
    let (client_sock, mut worker_sock) = UnixStream::pair().expect("socketpair failed");
    let worker = std::thread::spawn(move || {
        let mut request = [0u8; FRAME_HEADER_LEN];
        worker_sock.read_exact(&mut request).unwrap();
        let request = FrameHeader::decode(&request).expect("valid request header");
        let response = FrameHeader {
            msg_type: MSG_HEARTBEAT_RESP,
            status: STATUS_OK,
            correlation_id: request.correlation_id,
            offset: 1024 * 1024,
            payload_len: 128,
            aux: 64,
        };
        worker_sock.write_all(&response.encode()).unwrap();
        // Body deliberately omitted.
        drop(worker_sock);
    });
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(50), 1024 * 1024);

    assert_eq!(
        client.refresh_cached_bytes(),
        Err("GPU cache worker heartbeat telemetry was truncated")
    );
    assert_eq!(client.state(), CacheState::Unavailable);
    assert_eq!(client.cache_telemetry(), None);
    worker.join().unwrap();
}

#[test]
fn heartbeat_mismatched_correlation_fails_closed() {
    let (client_sock, mut worker_sock) = UnixStream::pair().expect("socketpair failed");
    let worker = std::thread::spawn(move || {
        let mut request = [0u8; FRAME_HEADER_LEN];
        worker_sock.read_exact(&mut request).unwrap();
        let request = FrameHeader::decode(&request).expect("valid request header");
        let response = FrameHeader {
            msg_type: MSG_HEARTBEAT_RESP,
            status: STATUS_OK,
            correlation_id: request.correlation_id.wrapping_add(1),
            offset: 1024 * 1024,
            payload_len: 0,
            aux: 64,
        };
        worker_sock.write_all(&response.encode()).unwrap();
    });
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(50), 1024 * 1024);

    assert_eq!(
        client.refresh_cached_bytes(),
        Err("GPU cache worker heartbeat mismatched")
    );
    assert_eq!(client.state(), CacheState::Unavailable);
    assert_eq!(client.cache_telemetry(), None);
    worker.join().unwrap();
}

#[test]
fn heartbeat_worker_refusal_is_distinguished_from_wire_mismatch() {
    let (client_sock, mut worker_sock) = UnixStream::pair().expect("socketpair failed");
    let worker = std::thread::spawn(move || {
        let mut request = [0u8; FRAME_HEADER_LEN];
        worker_sock.read_exact(&mut request).unwrap();
        let request = FrameHeader::decode(&request).unwrap();
        let response = FrameHeader {
            msg_type: MSG_HEARTBEAT_RESP,
            status: STATUS_ERROR,
            correlation_id: request.correlation_id,
            offset: 1024 * 1024,
            payload_len: 0,
            aux: 0,
        };
        worker_sock.write_all(&response.encode()).unwrap();
    });
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(50), 1024 * 1024);
    assert_eq!(
        client.refresh_cached_bytes(),
        Err("GPU cache worker heartbeat refused")
    );
    assert_eq!(client.state(), CacheState::Unavailable);
    worker.join().unwrap();
}

#[test]
fn mutations_and_deferred_invalidations_preserve_held_telemetry() {
    const SAMPLED_AT: u64 = 1_000;
    let envelope = WorkerTelemetryEnvelope::new(
        SAMPLED_AT,
        Some(sample_budget_telemetry(SAMPLED_AT)),
        Some(sample_utilization_telemetry(SAMPLED_AT)),
        Some(sample_cache_telemetry(4096, SAMPLED_AT)),
    );
    let payload = envelope.to_bounded_payload().expect("envelope fits");

    let (client_sock, worker_sock) = UnixStream::pair().expect("socketpair failed");
    let worker = std::thread::spawn(move || {
        serve_heartbeat_payload(worker_sock.try_clone().unwrap(), payload, 64);
        worker_sock
    });
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(200), 1024 * 1024);

    assert_eq!(client.refresh_cached_bytes(), Ok(64 << 10));
    let _worker_sock = worker.join().unwrap();
    assert!(client.gpu_budget_telemetry().is_some());
    assert!(client.gpu_utilization_telemetry().is_some());
    assert!(client.cache_telemetry().is_some());

    // Queue update, promote, and dropped-update invalidate without blanking held samples.
    assert_eq!(client.update(0, &[1u8; 4096]), CacheMutation::Accepted);
    assert!(client.gpu_budget_telemetry().is_some());
    assert!(client.gpu_utilization_telemetry().is_some());
    assert!(client.cache_telemetry().is_some());

    assert_eq!(client.promote(4096, &[2u8; 4096]), CacheMutation::Accepted);
    assert!(client.gpu_budget_telemetry().is_some());
    assert!(client.gpu_utilization_telemetry().is_some());
    assert!(client.cache_telemetry().is_some());

    assert_eq!(
        client.defer_invalidation(8192, 4096),
        CacheMutation::Skipped
    );
    assert!(client.gpu_budget_telemetry().is_some());
    assert!(client.gpu_utilization_telemetry().is_some());
    assert!(client.cache_telemetry().is_some());
}

#[test]
fn promote_burst_caps_unacked_backlog_and_recovers_after_read() {
    let (client_sock, mut worker_sock) = UnixStream::pair().expect("socketpair failed");
    let worker = std::thread::spawn(move || {
        let mut drained_promotes = 0usize;
        let mut req_buf = [0u8; FRAME_HEADER_LEN];
        while worker_sock.read_exact(&mut req_buf).is_ok() {
            let frame = FrameHeader::decode(&req_buf).unwrap();
            let mut body = vec![0u8; frame.payload_len as usize];
            if !body.is_empty() {
                worker_sock.read_exact(&mut body).unwrap();
            }
            if frame.msg_type == MSG_PROMOTE {
                drained_promotes += 1;
            } else if frame.msg_type == MSG_READ_REQ {
                let mut resp = read_hit_response(frame.correlation_id, frame.offset, &[0xAB; 4096]);
                resp.aux = 64;
                worker_sock.write_all(&resp.encode()).unwrap();
                worker_sock.write_all(&[0xAB; 4096]).unwrap();
            }
        }
        drained_promotes
    });
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(200), 1024 * 1024);
    for i in 0..4u64 {
        assert_eq!(
            client.promote(i * 4096, &[0xAB; 4096]),
            CacheMutation::Accepted
        );
    }
    assert_eq!(
        client.promote(4 * 4096, &[0xAB; 4096]),
        CacheMutation::Skipped
    );

    let mut out = [0u8; 4096];
    assert_eq!(client.read(0, &mut out), CacheRead::Hit);
    assert_eq!(
        client.promote(5 * 4096, &[0xAB; 4096]),
        CacheMutation::Accepted
    );
    drop(client);
    assert_eq!(worker.join().unwrap(), 5);
}

#[test]
fn overdue_heartbeat_runs_once_write_mirrors_settle() {
    let (client_sock, mut worker_sock) = UnixStream::pair().expect("socketpair failed");
    let mut client = IpcCacheClient::with_pacing(
        client_sock,
        Duration::from_millis(200),
        1024 * 1024,
        PacingPolicy {
            read_drain_grace: Duration::from_millis(5),
            heartbeat_drain_grace: Duration::from_millis(200),
            min_heartbeat_interval: Duration::from_millis(5),
        },
    );
    let worker = std::thread::spawn(move || {
        for expected in 1..=2 {
            let mut req_buf = [0u8; FRAME_HEADER_LEN];
            loop {
                worker_sock.read_exact(&mut req_buf).unwrap();
                let frame = FrameHeader::decode(&req_buf).unwrap();
                let mut body = vec![0u8; frame.payload_len as usize];
                if !body.is_empty() {
                    worker_sock.read_exact(&mut body).unwrap();
                }
                if frame.msg_type == MSG_HEARTBEAT_REQ {
                    let resp = FrameHeader {
                        msg_type: MSG_HEARTBEAT_RESP,
                        status: STATUS_OK,
                        correlation_id: frame.correlation_id,
                        offset: 1024 * 1024,
                        payload_len: 0,
                        aux: expected * 64,
                    };
                    worker_sock.write_all(&resp.encode()).unwrap();
                    break;
                }
            }
        }
    });
    assert_eq!(client.refresh_cached_bytes(), Ok(64 << 10));
    std::thread::sleep(Duration::from_millis(20));
    // Trigger an invalidate-only mutation inside heartbeat_drain_grace (200ms)
    // after min_heartbeat_interval * 2 (10ms) has passed and write_mirrors_are_draining is false.
    assert_eq!(client.defer_invalidation(0, 4096), CacheMutation::Skipped);
    assert_eq!(client.refresh_cached_bytes(), Ok(128 << 10));
    worker.join().unwrap();
}
