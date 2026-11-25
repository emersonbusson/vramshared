// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/ipc_cache_client/identity_tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use ramshared_vram::{GpuAdapterIdentity, GpuBudgetSnapshot, GpuBudgetSource};

fn identity(byte: char) -> GpuAdapterIdentity {
    GpuAdapterIdentity {
        node_mask: None,
        backend: "cuda".into(),
        key: byte.to_string().repeat(32),
        luid: None,
    }
}

fn envelope(adapter: GpuAdapterIdentity) -> Vec<u8> {
    let sample = GpuBudgetSnapshot {
        adapter: Some(adapter),
        total_bytes: Some(8 << 30),
        budget_bytes: 8 << 30,
        used_bytes: 1 << 30,
        source: GpuBudgetSource::DriverReported,
        sampled_at: Instant::now(),
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    WorkerTelemetryEnvelope::new(
        now,
        Some(GpuBudgetTelemetry::from_snapshot(&sample, now)),
        None,
        None,
    )
    .to_bounded_payload()
    .unwrap()
}

#[test]
fn ipc_worker_identity_drift_revokes_without_origin_loss() {
    use crate::{AuthoritativeOriginBackend, FileOrigin};
    let (parent, mut peer) = UnixStream::pair().unwrap();
    let worker = std::thread::spawn(move || {
        for adapter in [identity('a'), identity('b')] {
            let mut request = [0; FRAME_HEADER_LEN];
            peer.read_exact(&mut request).unwrap();
            let request = FrameHeader::decode(&request).unwrap();
            assert_eq!(request.msg_type, MSG_HEARTBEAT_REQ);
            let payload = envelope(adapter);
            let response = FrameHeader {
                msg_type: MSG_HEARTBEAT_RESP,
                status: STATUS_OK,
                correlation_id: request.correlation_id,
                offset: 1024 * 1024,
                payload_len: payload.len() as u32,
                aux: 64,
            };
            peer.write_all(&response.encode()).unwrap();
            peer.write_all(&payload).unwrap();
        }
    });
    let mut client = IpcCacheClient::with_pacing(
        parent,
        Duration::from_millis(200),
        1024 * 1024,
        PacingPolicy::immediate(),
    );
    assert_eq!(client.refresh_cached_bytes(), Ok(64 << 10));
    assert_eq!(
        client.gpu_budget_telemetry().unwrap().adapter,
        Some(identity('a'))
    );
    assert!(
        client.refresh_cached_bytes().is_err(),
        "another adapter must not replace the worker's identity"
    );
    assert_eq!(client.state(), CacheState::Unavailable);
    assert!(client.gpu_budget_telemetry().is_none());
    worker.join().unwrap();
    let directory = std::env::temp_dir().join(format!(
        "ramshared-identity-origin-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("origin");
    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    file.set_len(8192).unwrap();
    let backend = AuthoritativeOriginBackend::new_with_admission(
        FileOrigin::from_file(file),
        client,
        8192,
        4096,
        crate::isolated_origin::AllowCacheAdmission,
    )
    .unwrap();
    backend.write_at(0, &[71; 4096]).unwrap();
    backend.flush().unwrap();
    let mut read = [0; 4096];
    backend.read_at(0, &mut read).unwrap();
    assert_eq!(read, [71; 4096]);
    drop(backend);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn ipc_worker_pin_cannot_be_rebound_after_io() {
    let (socket, peer) = UnixStream::pair().unwrap();
    let mut client = IpcCacheClient::new(socket, Duration::from_millis(50), 1 << 20);
    let mut invalid = identity('a');
    invalid.key = "0".into();
    assert!(client.expect_adapter(invalid).is_err());
    assert!(client.bound_adapter_identity().is_none());
    client.expect_adapter(identity('a')).unwrap();
    assert!(client.expect_adapter(identity('b')).is_err());
    drop(peer);
    assert!(client.refresh_cached_bytes().is_err());
    assert!(client.expect_adapter(identity('b')).is_err());
    assert_eq!(client.bound_adapter_identity(), Some(&identity('a')));
    let (socket, peer) = UnixStream::pair().unwrap();
    drop(peer);
    let mut client = IpcCacheClient::new(socket, Duration::from_millis(50), 1 << 20);
    assert!(client.refresh_cached_bytes().is_err());
    assert!(client.expect_adapter(identity('a')).is_err());
    assert!(client.bound_adapter_identity().is_none());
}
