// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/tests/gpu_worker_protocol.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::time::Duration;

use ramshared_block::{
    FRAME_HEADER_LEN, FrameHeader, GpuWorkerConfig, IpcCacheClient,
    gpu_cache_worker::{MSG_HANDSHAKE_REQ, MSG_UPDATE, STATUS_OK},
    run_gpu_worker_loop, run_gpu_worker_loop_with_frame_read_timeout,
};
use ramshared_vram::{GpuBudgetSnapshot, VramError, VramMemory, VramProvider};

struct NoopMemory;

impl VramMemory for NoopMemory {
    fn len(&self) -> usize {
        0
    }

    fn zero(&mut self) -> Result<(), VramError> {
        Ok(())
    }

    fn read_at(&self, _off: u64, _dst: &mut [u8]) -> Result<(), VramError> {
        Err(VramError::OutOfMemory)
    }

    fn write_at(&mut self, _off: u64, _src: &[u8]) -> Result<(), VramError> {
        Err(VramError::OutOfMemory)
    }
}

struct NoopProvider;

impl VramProvider for NoopProvider {
    type Mem<'p> = NoopMemory;

    fn alloc(&self, _bytes: usize) -> Result<Self::Mem<'_>, VramError> {
        Err(VramError::OutOfMemory)
    }

    fn mem_info(&self) -> Result<(u64, u64), VramError> {
        Err(VramError::Provider("unused in invalid-frame test".into()))
    }

    fn budget_snapshot(&self) -> Result<GpuBudgetSnapshot, VramError> {
        Err(VramError::Provider("unused in invalid-frame test".into()))
    }
}

fn worker_config() -> GpuWorkerConfig {
    GpuWorkerConfig {
        target_bytes: 0,
        chunk_bytes: 4096,
        reserve_floor_bytes: 0,
        compression_enabled: false,
    }
}

#[test]
fn worker_rejects_unknown_message_types_instead_of_silently_dropping_them() {
    let (mut client, worker_socket) = UnixStream::pair().expect("socketpair");
    let worker = std::thread::spawn(move || {
        run_gpu_worker_loop(worker_socket, NoopProvider, worker_config())
    });

    let invalid = FrameHeader {
        msg_type: u8::MAX,
        status: STATUS_OK,
        correlation_id: 1,
        offset: 0,
        payload_len: 0,
        aux: 0,
    };
    client
        .write_all(&invalid.encode())
        .expect("write invalid frame");
    client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("set timeout");

    let mut response = [0; FRAME_HEADER_LEN];
    let response_len = client.read(&mut response);
    drop(client);
    let worker_result = worker.join().expect("worker thread");

    assert!(
        matches!(response_len, Ok(0)),
        "invalid frame must close the worker stream: {response_len:?}"
    );
    assert!(
        worker_result.is_err(),
        "invalid frame must fail the worker loop"
    );
}

#[test]
fn worker_rejects_nonzero_reserved_header_bytes() {
    let (mut client, worker_socket) = UnixStream::pair().expect("socketpair");
    let worker = std::thread::spawn(move || {
        run_gpu_worker_loop(worker_socket, NoopProvider, worker_config())
    });

    let handshake = FrameHeader {
        msg_type: MSG_HANDSHAKE_REQ,
        status: STATUS_OK,
        correlation_id: 2,
        offset: 0,
        payload_len: 0,
        aux: 0,
    };
    let mut malformed = handshake.encode();
    malformed[2] = 1;
    client.write_all(&malformed).expect("write malformed frame");
    client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("set timeout");

    let mut response = [0; FRAME_HEADER_LEN];
    let response_len = client.read(&mut response);
    drop(client);
    let worker_result = worker.join().expect("worker thread");

    assert!(
        matches!(response_len, Ok(0)),
        "malformed header must close the worker stream: {response_len:?}"
    );
    assert!(
        worker_result.is_err(),
        "malformed header must fail the worker loop"
    );
}

#[test]
fn client_refuses_nonzero_reserved_response_header_bytes() {
    let (client_socket, mut worker_socket) = UnixStream::pair().expect("socketpair");
    let fake_worker = std::thread::spawn(move || {
        let mut request = [0; FRAME_HEADER_LEN];
        worker_socket
            .read_exact(&mut request)
            .expect("read handshake request");
        let request = FrameHeader::decode(&request).expect("valid handshake request");
        let response = FrameHeader {
            msg_type: ramshared_block::gpu_cache_worker::MSG_HANDSHAKE_RESP,
            status: STATUS_OK,
            correlation_id: request.correlation_id,
            offset: 4096,
            payload_len: 0,
            aux: 0,
        };
        let mut malformed = response.encode();
        malformed[7] = 1;
        worker_socket
            .write_all(&malformed)
            .expect("write malformed response");
    });
    let mut client = IpcCacheClient::new(client_socket, Duration::from_secs(1), 4096);

    assert!(client.perform_handshake().is_err());
    fake_worker.join().expect("fake worker thread");
}

const STALL_FRAME_READ_TIMEOUT: Duration = Duration::from_millis(100);
const STALL_WATCHDOG: Duration = Duration::from_secs(5);

fn spawn_worker_with_stall_budget(worker_socket: UnixStream) -> mpsc::Receiver<Result<(), String>> {
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = run_gpu_worker_loop_with_frame_read_timeout(
            worker_socket,
            NoopProvider,
            worker_config(),
            STALL_FRAME_READ_TIMEOUT,
        );
        let _ = done_tx.send(result);
    });
    done_rx
}

fn expect_stalled_worker_fails_closed(
    done_rx: mpsc::Receiver<Result<(), String>>,
    case: &str,
    client: UnixStream,
) {
    let result = done_rx
        .recv_timeout(STALL_WATCHDOG)
        .unwrap_or_else(|_| panic!("worker must not block forever on {case}"));
    drop(client);
    let error = result.expect_err("stalled peer must fail closed");
    assert!(
        error.to_ascii_lowercase().contains("deadline"),
        "stall failure must report the read deadline for {case}: {error}"
    );
}

#[test]
fn worker_frame_read_deadline_fails_closed_on_a_stalled_partial_header() {
    let (mut client, worker_socket) = UnixStream::pair().expect("socketpair");
    let done_rx = spawn_worker_with_stall_budget(worker_socket);

    let handshake = FrameHeader {
        msg_type: MSG_HANDSHAKE_REQ,
        status: STATUS_OK,
        correlation_id: 1,
        offset: 0,
        payload_len: 0,
        aux: 0,
    };
    let encoded = handshake.encode();
    client
        .write_all(&encoded[..FRAME_HEADER_LEN / 2])
        .expect("write stalled partial header");

    expect_stalled_worker_fails_closed(done_rx, "a stalled partial header", client);
}

#[test]
fn worker_frame_read_deadline_fails_closed_on_a_stalled_partial_payload() {
    let (mut client, worker_socket) = UnixStream::pair().expect("socketpair");
    let done_rx = spawn_worker_with_stall_budget(worker_socket);

    let update = FrameHeader {
        msg_type: MSG_UPDATE,
        status: STATUS_OK,
        correlation_id: 1,
        offset: 0,
        payload_len: 4,
        aux: 0,
    };
    client
        .write_all(&update.encode())
        .expect("write frame header");
    client.write_all(&[1, 2]).expect("write half the payload");

    expect_stalled_worker_fails_closed(done_rx, "a stalled partial payload", client);
}

#[test]
fn worker_frame_read_deadline_fails_closed_on_a_silent_peer() {
    let (client, worker_socket) = UnixStream::pair().expect("socketpair");
    let done_rx = spawn_worker_with_stall_budget(worker_socket);

    expect_stalled_worker_fails_closed(done_rx, "a silent peer", client);
}

struct BudgetedFakeProvider;

fn fake_budget() -> GpuBudgetSnapshot {
    const GIB: u64 = 1024 * 1024 * 1024;
    GpuBudgetSnapshot {
        adapter: Some(ramshared_vram::GpuAdapterIdentity {
            backend: "fake".to_string(),
            key: "fake-0".to_string(),
            luid: None,
            node_mask: None,
        }),
        total_bytes: Some(8 * GIB),
        budget_bytes: 8 * GIB,
        used_bytes: 0,
        source: ramshared_vram::GpuBudgetSource::DriverReported,
        sampled_at: std::time::Instant::now(),
    }
}

impl VramProvider for BudgetedFakeProvider {
    type Mem<'p> = NoopMemory;

    fn alloc(&self, _bytes: usize) -> Result<Self::Mem<'_>, VramError> {
        Ok(NoopMemory)
    }

    fn mem_info(&self) -> Result<(u64, u64), VramError> {
        let b = fake_budget();
        Ok((b.available_bytes(), b.budget_bytes))
    }

    fn budget_snapshot(&self) -> Result<GpuBudgetSnapshot, VramError> {
        Ok(fake_budget())
    }
}

#[test]
fn worker_slab_ipc_batch_compress_recover_and_telemetry_roundtrip() {
    use ramshared_vram::device_codec::{
        CodecError, GpuDeviceCodec, PAGE_BYTES, PageDecodeStatus, ReferenceLz4Codec,
    };
    use ramshared_vram::slab_pool::SlotEncoding;

    const GIB: u64 = 1024 * 1024 * 1024;
    let provider = BudgetedFakeProvider;
    let config = GpuWorkerConfig {
        target_bytes: 8 * GIB,
        chunk_bytes: 2 * 1024 * 1024,
        reserve_floor_bytes: 0,
        compression_enabled: true,
    };
    let (client_sock, worker_sock) = UnixStream::pair().expect("socketpair");
    let worker = std::thread::spawn(move || {
        ramshared_block::run_gpu_worker_loop_with_device_codec(
            worker_sock,
            provider,
            config,
            |_p, _avail, _frames| {
                let mut codec = ReferenceLz4Codec::new();
                codec.set_deadline(Duration::from_secs(30));
                Ok(Box::new(codec) as Box<dyn GpuDeviceCodec + '_>)
            },
        )
    });

    let mut client = IpcCacheClient::with_pacing(
        client_sock,
        Duration::from_secs(2),
        8 * GIB,
        ramshared_block::ipc_cache_client::PacingPolicy::immediate(),
    );
    client.perform_handshake().expect("handshake");

    let initial_tele = client
        .slab_telemetry()
        .expect("telemetry query")
        .expect("slab bridge admitted");
    assert_eq!(initial_tele.backend.as_deref(), Some("reference-lz4"));
    assert_eq!(initial_tele.logical_pages_cached, Some(0));

    let comp_page = [0x5Au8; PAGE_BYTES];
    let mut raw_page = [0u8; PAGE_BYTES];
    let mut state = 0x1234_5678_9abc_def0_u64;
    for b in &mut raw_page {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *b = (state >> 24) as u8;
    }

    let mut batch = vec![comp_page; 63];
    batch.push(raw_page);
    let tickets_opt = client
        .slab_compress_batch(&batch)
        .expect("slab compress batch");
    assert_eq!(tickets_opt.len(), 64);
    let t0 = tickets_opt[0].expect("compressible page 0 ticket");
    let t62 = tickets_opt[62].expect("compressible page 62 ticket");
    let t_raw = tickets_opt[63].expect("raw page ticket");
    assert_eq!(t0.encoding, SlotEncoding::Lz4);
    assert_eq!(t62.encoding, SlotEncoding::Lz4);
    assert_eq!(t_raw.encoding, SlotEncoding::Raw);

    let after_tele = client
        .slab_telemetry()
        .expect("telemetry query")
        .expect("slab bridge telemetry");
    assert_eq!(after_tele.logical_pages_cached, Some(64));
    assert!(
        after_tele.effective_density_ratio.unwrap_or(0.0) >= 2.0,
        "63 compressed pages in 1 B512 frame + 1 raw frame = 2.0x density: {:?}",
        after_tele
    );

    let (statuses, recovered) = client
        .slab_recover_batch(&[t0, t62, t_raw])
        .expect("recover");
    assert_eq!(statuses, vec![PageDecodeStatus::Ok; 3]);
    assert_eq!(recovered[0], comp_page);
    assert_eq!(recovered[1], comp_page);
    assert_eq!(recovered[2], raw_page);

    // Forged epoch fails cleanly per-lane while healthy sibling remains Ok.
    let mut bad_t_raw = t_raw;
    bad_t_raw.epoch = bad_t_raw.epoch.wrapping_add(99);
    let (mixed_statuses, mixed_pages) = client
        .slab_recover_batch(&[t0, bad_t_raw])
        .expect("mixed recover");
    assert_eq!(
        mixed_statuses,
        vec![PageDecodeStatus::Ok, PageDecodeStatus::InvalidTicket]
    );
    assert_eq!(mixed_pages[0], comp_page);

    drop(client);
    worker.join().unwrap().expect("clean worker EOF");

    // Disabled compression omits slab bridge (telemetry Miss, compress error).
    let (client_sock2, worker_sock2) = UnixStream::pair().expect("socketpair");
    let raw_only_config = GpuWorkerConfig {
        target_bytes: 8 * GIB,
        chunk_bytes: 2 * 1024 * 1024,
        reserve_floor_bytes: 0,
        compression_enabled: false,
    };
    let worker2 = std::thread::spawn(move || {
        run_gpu_worker_loop(worker_sock2, BudgetedFakeProvider, raw_only_config)
    });
    let mut client2 = IpcCacheClient::new(client_sock2, Duration::from_secs(2), 8 * GIB);
    client2.perform_handshake().expect("handshake2");
    assert_eq!(client2.slab_telemetry().expect("telemetry2"), None);
    let err = client2
        .slab_compress_batch(&[comp_page])
        .expect_err("disabled compression refuses compress batch");
    assert!(matches!(err, CodecError::BackendFailure(_)));
    drop(client2);
    worker2.join().unwrap().expect("clean worker2 EOF");
}

#[test]
fn client_slab_rpc_handles_error_and_malformed_server_replies() {
    use ramshared_block::gpu_cache_worker::{
        MSG_HANDSHAKE_RESP, MSG_SLAB_COMPRESS_BATCH_RESP, MSG_SLAB_RECOVER_BATCH_RESP,
        MSG_SLAB_TELEMETRY_RESP, STATUS_ERROR,
    };
    use ramshared_vram::device_codec::{CodecError, PAGE_BYTES};
    use ramshared_vram::slab_pool::{SlabClass, SlotEncoding, VramSlabTicket};

    const GIB: u64 = 1024 * 1024 * 1024;
    let ticket = VramSlabTicket {
        frame_idx: 0,
        sub_slot: 0,
        class: SlabClass::Raw4096,
        encoding: SlotEncoding::Raw,
        payload_len: PAGE_BYTES as u16,
        payload_crc32: 0,
        epoch: 1,
    };

    let answer_handshake = |sock: &mut UnixStream| {
        let mut buf = [0u8; FRAME_HEADER_LEN];
        sock.read_exact(&mut buf).unwrap();
        let req = FrameHeader::decode(&buf).unwrap();
        let resp = FrameHeader {
            msg_type: MSG_HANDSHAKE_RESP,
            status: STATUS_OK,
            correlation_id: req.correlation_id,
            offset: 4 * GIB,
            payload_len: 0,
            aux: 0,
        };
        sock.write_all(&resp.encode()).unwrap();
    };
    let read_req = |sock: &mut UnixStream| -> FrameHeader {
        let mut buf = [0u8; FRAME_HEADER_LEN];
        sock.read_exact(&mut buf).unwrap();
        let req = FrameHeader::decode(&buf).unwrap();
        if req.payload_len > 0 {
            let mut drop_buf = vec![0u8; req.payload_len as usize];
            sock.read_exact(&mut drop_buf).unwrap();
        }
        req
    };

    // 1. Worker STATUS_ERROR error mappings (DeadlineExceeded, InvalidBatchLength, BackendFailure, telemetry error)
    let (c_sock, mut s_sock) = UnixStream::pair().unwrap();
    let srv = std::thread::spawn(move || {
        answer_handshake(&mut s_sock);
        let errors: [&[u8]; 4] = [
            b"DeadlineExceeded",
            b"InvalidBatchLength",
            b"bad telemetry",
            b"other",
        ];
        let types = [
            MSG_SLAB_COMPRESS_BATCH_RESP,
            MSG_SLAB_RECOVER_BATCH_RESP,
            MSG_SLAB_TELEMETRY_RESP,
            MSG_SLAB_COMPRESS_BATCH_RESP,
        ];
        for (idx, msg_type) in types.iter().enumerate() {
            let req = read_req(&mut s_sock);
            let payload = errors[idx];
            let resp = FrameHeader {
                msg_type: *msg_type,
                status: STATUS_ERROR,
                correlation_id: req.correlation_id,
                offset: 0,
                payload_len: payload.len() as u32,
                aux: 0,
            };
            s_sock.write_all(&resp.encode()).unwrap();
            s_sock.write_all(payload).unwrap();
        }
    });
    let mut client = IpcCacheClient::new(c_sock, Duration::from_secs(2), 4 * GIB);
    client.perform_handshake().unwrap();
    assert_eq!(
        client.slab_compress_batch(&[[1u8; PAGE_BYTES]]),
        Err(CodecError::DeadlineExceeded)
    );
    assert_eq!(
        client.slab_recover_batch(&[ticket]),
        Err(CodecError::InvalidBatchLength)
    );
    assert!(client.slab_telemetry().is_err());
    assert!(matches!(
        client.slab_compress_batch(&[[2u8; PAGE_BYTES]]),
        Err(CodecError::BackendFailure(_))
    ));
    srv.join().unwrap();

    // 2. Malformed responses & inactive refusal paths fail cleanly.
    for case in 0..5 {
        let (c_sock2, mut s_sock2) = UnixStream::pair().unwrap();
        let srv2 = std::thread::spawn(move || {
            answer_handshake(&mut s_sock2);
            let req = read_req(&mut s_sock2);
            match case {
                0 => {
                    // Bad JSON in telemetry
                    let resp = FrameHeader {
                        msg_type: MSG_SLAB_TELEMETRY_RESP,
                        status: STATUS_OK,
                        correlation_id: req.correlation_id,
                        offset: 0,
                        payload_len: 3,
                        aux: 0,
                    };
                    s_sock2.write_all(&resp.encode()).unwrap();
                    s_sock2.write_all(b"{{{").unwrap();
                }
                1 => {
                    // Invalid telemetry status code
                    let resp = FrameHeader {
                        msg_type: MSG_SLAB_TELEMETRY_RESP,
                        status: 99,
                        correlation_id: req.correlation_id,
                        offset: 0,
                        payload_len: 0,
                        aux: 0,
                    };
                    s_sock2.write_all(&resp.encode()).unwrap();
                }
                2 => {
                    // Wrong aux count on compress response
                    let resp = FrameHeader {
                        msg_type: MSG_SLAB_COMPRESS_BATCH_RESP,
                        status: STATUS_OK,
                        correlation_id: req.correlation_id,
                        offset: 0,
                        payload_len: 0,
                        aux: 9,
                    };
                    s_sock2.write_all(&resp.encode()).unwrap();
                }
                3 => {
                    // Truncated payload on recover response
                    let resp = FrameHeader {
                        msg_type: MSG_SLAB_RECOVER_BATCH_RESP,
                        status: STATUS_OK,
                        correlation_id: req.correlation_id,
                        offset: 0,
                        payload_len: 4,
                        aux: 1,
                    };
                    s_sock2.write_all(&resp.encode()).unwrap();
                    s_sock2.write_all(&[0, 1, 2, 3]).unwrap();
                }
                _ => {
                    // Wrong correlation ID -> identity mismatch + fail client
                    let resp = FrameHeader {
                        msg_type: MSG_SLAB_TELEMETRY_RESP,
                        status: STATUS_OK,
                        correlation_id: req.correlation_id + 100,
                        offset: 0,
                        payload_len: 0,
                        aux: 0,
                    };
                    s_sock2.write_all(&resp.encode()).unwrap();
                }
            }
        });
        let mut c2 = IpcCacheClient::new(c_sock2, Duration::from_secs(2), 4 * GIB);
        c2.perform_handshake().unwrap();
        match case {
            0 | 1 | 4 => assert!(c2.slab_telemetry().is_err()),
            2 => assert!(c2.slab_compress_batch(&[[0u8; PAGE_BYTES]]).is_err()),
            _ => assert!(c2.slab_recover_batch(&[ticket]).is_err()),
        }
        // Once failed (inactive), subsequent call refuses without I/O.
        assert!(c2.slab_telemetry().is_err());
        srv2.join().unwrap();
    }
}
