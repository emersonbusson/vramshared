// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/ipc_cache_client/tests/support.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::super::*;

/// Pushes real `MSG_UPDATE` frames until the socket cannot accept a whole
/// one, so the client lands in the DT-10 backpressured state. Saturation is
/// the frames themselves — never filler — so every byte the peer can read
/// is frame data and the wire is never contaminated.
pub(super) fn saturate_with_updates(client: &mut IpcCacheClient) -> usize {
    // These fixtures exercise socket backpressure itself; disable adaptive
    // write-mirror pacing so the test can actually fill the mutation queue.
    client.pacing.read_drain_grace = Duration::ZERO;
    let payload = vec![0x5A; MAX_MUTATION_FRAME_DATA_BYTES];
    let mut sent = 0;
    while client.pending_frame.is_empty() {
        assert_eq!(
            client.update(sent as u64, &payload),
            CacheMutation::Accepted
        );
        sent += 1;
        assert!(sent < 512, "socket never backpressured after {sent} frames");
    }
    sent
}

/// Queues further updates while the backlog has room for one more frame.
/// Leaves the client exactly one frame below [`MAX_PENDING_MUTATION_BYTES`].
pub(super) fn queue_updates_until_one_frame_from_full(client: &mut IpcCacheClient) -> usize {
    let payload = vec![0x5A; MAX_MUTATION_FRAME_DATA_BYTES];
    let frame_len = FRAME_HEADER_LEN + MAX_MUTATION_FRAME_DATA_BYTES;
    let mut queued = 0;
    while client.pending_frame.len() + frame_len <= MAX_PENDING_MUTATION_BYTES {
        assert_eq!(
            client.update(queued as u64, &payload),
            CacheMutation::Accepted
        );
        queued += 1;
        assert!(queued < 512, "backlog never approached the cap");
    }
    queued
}

/// Tops the pending backlog up to exactly `target` bytes using real
/// `MSG_UPDATE` frames — never filler — so a test can choose how much
/// headroom is left for a DT-11 invalidate.
pub(super) fn fill_pending_to(client: &mut IpcCacheClient, target: usize) {
    assert!(target <= MAX_PENDING_MUTATION_BYTES);
    while client.pending_frame.len() < target {
        let room = target - client.pending_frame.len();
        assert!(
            room >= FRAME_HEADER_LEN,
            "cannot land on target with {room} bytes left"
        );
        let frame_len = room.min(FRAME_HEADER_LEN + MAX_MUTATION_FRAME_DATA_BYTES);
        let payload = vec![0x5A; frame_len - FRAME_HEADER_LEN];
        assert_eq!(
            client.update(0, &payload),
            CacheMutation::Accepted,
            "a fill step must queue while under the cap"
        );
    }
    assert_eq!(client.pending_frame.len(), target);
}

/// Serves one request with a caller-supplied response frame and optional
/// payload. The fixtures for the AUD-34 contracts all need a peer that
/// answers with an exact, sometimes wrong, frame.
///
/// A malformed frame is expected to make the client revoke the session
/// and shut the socket down (`fail`), which can tear the connection down
/// before the payload write drains. Delivery is therefore completed on a
/// best-effort basis: what these fixtures prove is the client's refusal,
/// and a legitimate counterpart is already asserted on the client side by
/// the bytes it accepted.
pub(super) fn serve_one_response(
    mut worker_sock: UnixStream,
    resp: FrameHeader,
    payload: Vec<u8>,
) -> std::io::Result<()> {
    let mut request = [0u8; FRAME_HEADER_LEN];
    worker_sock.read_exact(&mut request)?;
    worker_sock.write_all(&resp.encode())?;
    if !payload.is_empty() {
        let _ = worker_sock.write_all(&payload);
    }
    Ok(())
}

/// A well-formed READ Hit response for `offset` carrying `bytes`.
pub(super) fn read_hit_response(correlation_id: u64, offset: u64, bytes: &[u8]) -> FrameHeader {
    FrameHeader {
        msg_type: MSG_READ_RESP,
        status: STATUS_OK,
        correlation_id,
        offset,
        payload_len: bytes.len() as u32,
        aux: 0,
    }
}

/// A DISABLE response is the empty/zeroed acknowledgement of exactly one
/// operation (AUD-34). Anything else is not a release of this resource.
pub(super) fn disable_response(
    correlation_id: u64,
    offset: u64,
    payload_len: u32,
    aux: u32,
) -> FrameHeader {
    FrameHeader {
        msg_type: MSG_DISABLE_RESP,
        status: STATUS_OK,
        correlation_id,
        offset,
        payload_len,
        aux,
    }
}

/// Serves one handshake response after reading the request.
pub(super) fn serve_handshake_response(
    mut worker_sock: UnixStream,
    resp: FrameHeader,
) -> std::io::Result<()> {
    let mut request = [0u8; FRAME_HEADER_LEN];
    worker_sock.read_exact(&mut request)?;
    worker_sock.write_all(&resp.encode())
}

/// A handshake response reporting `target` as the **admitted** capacity.
pub(super) fn handshake_response(correlation_id: u64, status: u8, target: u64) -> FrameHeader {
    FrameHeader {
        msg_type: MSG_HANDSHAKE_RESP,
        status,
        correlation_id,
        offset: target,
        payload_len: 0,
        aux: 0,
    }
}

/// Runs one handshake against a worker thread answering `resp`.
pub(super) fn handshake_with(
    requested: u64,
    resp: FrameHeader,
) -> (IpcCacheClient, Result<(), String>) {
    let (client_sock, worker_sock) = UnixStream::pair().expect("socketpair failed");
    let worker = std::thread::spawn(move || {
        serve_handshake_response(worker_sock, resp).unwrap();
    });
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(200), requested);
    let outcome = client.perform_handshake();
    worker.join().unwrap();
    (client, outcome)
}

/// Reads whatever a nonblocking peer socket has already buffered.
pub(super) fn drain_available(socket: &mut UnixStream) -> Vec<u8> {
    let mut collected = Vec::new();
    let mut buf = [0u8; 256];
    loop {
        match socket.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => collected.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    collected
}

/// Serves one heartbeat response carrying `payload` after reading the
/// request, so the client sees a well-formed frame with a caller-chosen body.
pub(super) fn serve_heartbeat_payload(mut worker_sock: UnixStream, payload: Vec<u8>, aux: u32) {
    let mut request = [0u8; FRAME_HEADER_LEN];
    worker_sock
        .read_exact(&mut request)
        .expect("heartbeat request");
    let request = FrameHeader::decode(&request).expect("valid request header");
    let response = FrameHeader {
        msg_type: MSG_HEARTBEAT_RESP,
        status: STATUS_OK,
        correlation_id: request.correlation_id,
        offset: 1024 * 1024,
        payload_len: payload.len() as u32,
        aux,
    };
    worker_sock
        .write_all(&response.encode())
        .expect("heartbeat header");
    worker_sock.write_all(&payload).expect("heartbeat body");
}

pub(super) fn sample_cache_telemetry(
    logical_cached_bytes: u64,
    sampled_at_unix_ms: u64,
) -> WorkerCacheTelemetry {
    use ramshared_vram::{CodecCapability, CodecState, CodecTelemetry};

    WorkerCacheTelemetry {
        schema_version: 1,
        sampled_at_unix_ms,
        codec: CodecTelemetry::new(CodecCapability::Available, CodecState::Ready, None),
        logical_cached_bytes,
        physical_cache_slab_bytes: 2 * 1024 * 1024,
        codec_workspace_bytes: 0,
        compressed_payload_bytes: 128,
        raw_payload_bytes: 64,
        metadata_bytes: Some(32),
        device_metadata_lower_bound_bytes: None,
        raw_bypass_bytes: 2048,
        codec_integrity_errors: 0,
        codec_decode_errors: 0,
        codec_timeouts: 0,
    }
}

/// A bound adapter-budget sample, as the worker produces it at heartbeat time.
pub(super) fn sample_budget_telemetry(sampled_at_unix_ms: u64) -> GpuBudgetTelemetry {
    use ramshared_vram::{GpuAdapterIdentity, GpuBudgetSource};

    GpuBudgetTelemetry {
        schema_version: 1,
        adapter: Some(GpuAdapterIdentity {
            node_mask: None,
            backend: "cuda".to_string(),
            key: "test-adapter".to_string(),
            luid: Some("00000000:00011f8a".to_string()),
        }),
        total_bytes: Some(6 << 30),
        budget_bytes: 4 << 30,
        used_bytes: 1 << 30,
        available_bytes: 3 << 30,
        source: GpuBudgetSource::DriverReported,
        sampled_at_unix_ms,
    }
}

/// A measured adapter busy-time sample, as the worker produces it beside
/// the budget at the same heartbeat.
pub(super) fn sample_utilization_telemetry(sampled_at_unix_ms: u64) -> GpuUtilizationTelemetry {
    GpuUtilizationTelemetry {
        schema_version: 1,
        gpu_pct: 37,
        memory_pct: 82,
        encoder_pct: None,
        decoder_pct: None,
        sampled_at_unix_ms,
    }
}

/// Answers a heartbeat with `payload`, then answers the next request with
/// `disable_status`, so one worker thread can drive both exchanges.
pub(super) fn serve_heartbeat_then(
    worker_sock: UnixStream,
    payload: Vec<u8>,
    disable_status: u8,
    respond_to_disable: bool,
) {
    let mut worker_sock = worker_sock;
    serve_heartbeat_payload(
        worker_sock.try_clone().expect("clone worker socket"),
        payload,
        64,
    );
    let mut request = [0u8; FRAME_HEADER_LEN];
    let _ = worker_sock.read_exact(&mut request);
    if !respond_to_disable {
        drop(worker_sock);
        return;
    }
    let request = FrameHeader::decode(&request).expect("valid request header");
    let response = FrameHeader {
        msg_type: MSG_DISABLE_RESP,
        status: disable_status,
        correlation_id: request.correlation_id,
        offset: 0,
        payload_len: 0,
        aux: 0,
    };
    worker_sock.write_all(&response.encode()).unwrap();
}
