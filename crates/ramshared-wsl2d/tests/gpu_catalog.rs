// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/tests/gpu_catalog.rs
//! Real connected product host/guest exchange with synthetic metadata, no GPU effects.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use ramshared_vram::host_identity::{HostGpuIdentityReport, HostGpuObservation, ProducerProcess};
use ramshared_winsvc::vsock_lease::{HostSamples, LeasePolicy, serve_connection};
use ramshared_wsl2d::control_plane::{ControlPlane, VsockControlPlane};
use ramshared_wsl2d::host_gate::{SealedOrigin, SharedAuthority};
use std::os::unix::net::UnixStream;
use std::time::Duration;
const SECRET: &[u8] = b"actual-connected-catalog-test-secret";
fn sample() -> HostGpuObservation {
    HostGpuObservation {
        age_ms: 100,
        report: HostGpuIdentityReport {
            schema_version: 1,
            producer: ProducerProcess {
                pid: 42,
                creation_time_100ns: 77,
            },
            started_at_unix_ms: 100,
            elapsed_ms: 1,
            adapters: vec![],
        },
    }
}
fn plane(stream: UnixStream) -> VsockControlPlane<UnixStream> {
    VsockControlPlane::new(
        stream,
        SECRET.to_vec(),
        "boot".into(),
        "distro".into(),
        SealedOrigin {
            logical_capacity_mib: 16,
            partuuid: "test-verified-origin".into(),
            origin_vhdx: "test-origin".into(),
        },
        Duration::from_secs(2),
    )
}
#[test]
fn authenticated_catalog_tracks_availability_and_current_generation() {
    let (guest, mut host) = UnixStream::pair().unwrap();
    let server = std::thread::spawn(move || {
        let mut count = 0;
        serve_connection(
            &mut host,
            SECRET,
            LeasePolicy::new(5, 15).unwrap(),
            || 1000,
            move || {
                count += 1;
                HostSamples {
                    memory: None,
                    gpu: if count == 2 { None } else { Some(sample()) },
                    ..HostSamples::default()
                }
            },
        )
    });
    let shared = SharedAuthority::new();
    let mut guest = plane(guest).with_authority_mirror(shared.begin_session());
    guest.perform_handshake(1000).unwrap();
    assert!(guest.gpu_catalog().is_none());
    guest.heartbeat(1001).unwrap();
    assert_eq!(guest.gpu_catalog().unwrap(), &sample().report);
    guest.heartbeat(1002).unwrap();
    assert!(guest.gpu_catalog().is_none());
    guest.heartbeat(1003).unwrap();
    assert!(guest.gpu_catalog().is_some());
    shared.slot().lock().unwrap().on_lease_expired();
    assert!(
        guest.gpu_catalog().is_none(),
        "shared lease revocation removes metadata access"
    );
    guest.heartbeat(1004).unwrap();
    assert!(guest.gpu_catalog().is_some());
    let _new_session = shared.begin_session();
    assert!(
        guest.gpu_catalog().is_none(),
        "superseded sessions cannot expose observations"
    );
    assert!(guest.admit_origin_io().is_ok());
    drop(guest);
    assert!(server.join().unwrap().is_err());
}
#[test]
fn invalid_catalog_never_renews_cache_authority_or_revokes_origin() {
    use ramshared_ipc::{MAX_GPU_CATALOG_PAYLOAD, MSG_GPU_CATALOG, read_frame, write_frame};
    for fault in ["proof", "missing", "correlation"] {
        let (guest, mut relay_guest) = UnixStream::pair().unwrap();
        let (mut relay_host, mut host) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            serve_connection(
                &mut host,
                SECRET,
                LeasePolicy::new(5, 15).unwrap(),
                || 1000,
                || HostSamples {
                    memory: None,
                    gpu: Some(sample()),
                    ..HostSamples::default()
                },
            )
        });
        let relay = std::thread::spawn(move || {
            fn forward(from: &mut UnixStream, to: &mut UnixStream) {
                let (kind, bytes, round) = read_frame(from, MAX_GPU_CATALOG_PAYLOAD).unwrap();
                write_frame(to, kind, &bytes, round).unwrap();
            }
            forward(&mut relay_guest, &mut relay_host); // Opening
            forward(&mut relay_host, &mut relay_guest); // Challenge
            forward(&mut relay_guest, &mut relay_host); // Finish
            forward(&mut relay_guest, &mut relay_host); // Lease request
            forward(&mut relay_host, &mut relay_guest); // Grant
            forward(&mut relay_guest, &mut relay_host); // Beat
            forward(&mut relay_host, &mut relay_guest); // Ack
            let (kind, bytes, round) =
                read_frame(&mut relay_host, MAX_GPU_CATALOG_PAYLOAD).unwrap();
            assert_eq!(kind, MSG_GPU_CATALOG);
            if fault == "missing" {
                return;
            }
            let mut envelope: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            if fault == "proof" {
                envelope["proof"][0] = (envelope["proof"][0].as_u64().unwrap() ^ 1).into();
            }
            write_frame(
                &mut relay_guest,
                kind,
                &serde_json::to_vec(&envelope).unwrap(),
                round + u64::from(fault == "correlation"),
            )
            .unwrap();
        });
        let mut guest = plane(guest);
        guest.perform_handshake(1000).unwrap();
        assert!(guest.heartbeat(1001).is_err(), "{fault}");
        assert!(guest.gpu_catalog().is_none());
        assert!(guest.admit_cache(1001).is_err());
        assert!(guest.admit_origin_io().is_ok());
        assert_eq!(
            guest
                .liveness_handle()
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        drop(guest);
        relay.join().unwrap();
        assert!(server.join().unwrap().is_err());
    }
}
