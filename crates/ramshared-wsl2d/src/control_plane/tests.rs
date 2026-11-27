// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/control_plane/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
pub(super) use ipc::{HeartbeatAck, MSG_HEARTBEAT, MSG_HEARTBEAT_ACK};
pub(super) use ramshared_ipc::vsock::{MockVsockStream, VsockStreamTrait};
pub(super) use std::io::{Read, Write};
pub(super) use std::os::unix::net::UnixStream;
pub(super) use std::time::Duration;

/// Test-only shared secret. Not a product key and never a default.
pub(super) const SECRET: &[u8] = b"unit-test-shared-secret-not-a-product-key";
pub(super) const IO_TIMEOUT: Duration = Duration::from_secs(5);

#[test]
pub(super) fn retired_mirror_rejects_an_actual_late_authenticated_ack() {
    use crate::host_gate::SharedAuthority;
    use ramshared_winsvc::vsock_lease::{
        HostSamples, LeasePolicy, LeaseServerError, serve_handshake, serve_heartbeats,
    };
    use std::sync::atomic::AtomicBool;
    struct Cleanup {
        release: Arc<AtomicBool>,
        host: Option<std::thread::JoinHandle<Result<usize, LeaseServerError>>>,
        guest: Option<std::thread::JoinHandle<Result<(), ControlPlaneError>>>,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            self.release.store(true, Ordering::Release);
            if let Some(h) = self.guest.take() {
                let _ = h.join();
            }
            if let Some(h) = self.host.take() {
                let _ = h.join();
            }
        }
    }
    let (guest, host) = UnixStream::pair().unwrap();
    host.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    host.set_write_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let release = Arc::new(AtomicBool::new(false));
    let gate = release.clone();
    let arrived = Arc::new(AtomicBool::new(false));
    let waiting = arrived.clone();
    let host = std::thread::spawn(move || {
        let mut host = MockVsockStream(host);
        let policy = LeasePolicy::new(1, 3).unwrap();
        let mut session = serve_handshake(&mut host, SECRET, policy, 1000).unwrap();
        serve_heartbeats(
            &mut host,
            &mut session,
            policy,
            || 1001,
            Some(1),
            || {
                waiting.store(true, Ordering::Release);
                while !gate.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                HostSamples::default()
            },
            SECRET,
        )
    });
    let mut cleanup = Cleanup {
        release: release.clone(),
        host: Some(host),
        guest: None,
    };
    let shared = SharedAuthority::new();
    let session = shared.begin_session();
    let mut plane = plane_over(MockVsockStream(guest)).with_authority_mirror(session.clone());
    plane.perform_handshake(1000).unwrap();
    cleanup.guest = Some(std::thread::spawn(move || plane.heartbeat(1001)));
    let until = std::time::Instant::now() + Duration::from_secs(2);
    while !arrived.load(Ordering::Acquire) && std::time::Instant::now() < until {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(arrived.load(Ordering::Acquire));
    assert!(shared.slot().lock().unwrap().admit_cache(1001).is_ok());
    assert!(session.retire(|slot| slot.on_lease_expired()));
    release.store(true, Ordering::Release);
    cleanup.guest.take().unwrap().join().unwrap().unwrap();
    assert_eq!(cleanup.host.take().unwrap().join().unwrap().unwrap(), 1);
    assert!(shared.slot().lock().unwrap().admit_cache(1001).is_err());
    assert!(shared.slot().lock().unwrap().admit_origin_io().is_ok());
    assert!(!session.is_current());
}

pub(super) fn sealed_origin() -> SealedOrigin {
    SealedOrigin {
        logical_capacity_mib: 4096,
        partuuid: "11111111-2222-3333-4444-555555555555".into(),
        origin_vhdx: "C:\\ramshared\\origin.vhdx".into(),
    }
}

pub(super) fn plane_over<S: VsockStreamTrait>(stream: S) -> VsockControlPlane<S> {
    VsockControlPlane::new(
        stream,
        SECRET.to_vec(),
        "boot-test".into(),
        "distro-test".into(),
        sealed_origin(),
        IO_TIMEOUT,
    )
}

// --- Scripted stream (no peer thread) ---------------------------------

/// Reads come from a prepared buffer; writes are captured. Used for the
/// paths that must fail closed without needing a real host proof.
pub(super) struct ScriptedStream {
    to_guest: std::io::Cursor<Vec<u8>>,
    from_guest: Vec<u8>,
    fail_write: bool,
    fail_read: bool,
    fail_timeout: bool,
    fail_write_timeout: bool,
    fail_shutdown: bool,
}

impl ScriptedStream {
    fn new(to_guest: Vec<u8>) -> Self {
        Self {
            to_guest: std::io::Cursor::new(to_guest),
            from_guest: Vec::new(),
            fail_write: false,
            fail_read: false,
            fail_timeout: false,
            fail_write_timeout: false,
            fail_shutdown: false,
        }
    }
}

impl Read for ScriptedStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.fail_read {
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "scripted read failure",
            ));
        }
        self.to_guest.read(buf)
    }
}

impl Write for ScriptedStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.fail_write {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "scripted write failure",
            ));
        }
        self.from_guest.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl VsockStreamTrait for ScriptedStream {
    fn set_read_timeout(&self, _timeout: Option<Duration>) -> std::io::Result<()> {
        if self.fail_timeout {
            return Err(std::io::Error::other("socket timeout installation failed"));
        }
        Ok(())
    }
    fn set_write_timeout(&self, _timeout: Option<Duration>) -> std::io::Result<()> {
        if self.fail_write_timeout {
            return Err(std::io::Error::other("write timeout installation failed"));
        }
        Ok(())
    }
    fn shutdown_both(&self) -> std::io::Result<()> {
        if self.fail_shutdown {
            return Err(std::io::Error::other("shutdown request failed"));
        }
        Ok(())
    }
}

pub(super) fn frame_bytes(msg_type: u8, payload: &[u8], correlation_id: u64) -> Vec<u8> {
    let mut buf = Vec::new();
    write_frame(&mut buf, msg_type, payload, correlation_id).expect("frame");
    buf
}

#[test]
pub(super) fn timeout_setup_failure_revokes_cache_and_preserves_origin() {
    for write_timeout in [false, true] {
        let ack = HeartbeatAck {
            timestamp_ms: 1000,
            lease_remaining_ms: 15000,
            host_memory: None,
        };
        let mut stream = ScriptedStream::new(frame_bytes(
            MSG_HEARTBEAT_ACK,
            &ipc::encode_control(&ack).unwrap(),
            4,
        ));
        stream.fail_timeout = !write_timeout;
        stream.fail_write_timeout = write_timeout;
        let mut plane = plane_over(stream);
        plane
            .authority
            .accept_lease(
                LeaseToken {
                    lease_id: 1,
                    deadline_ms: 20000,
                },
                sealed_origin(),
                1000,
            )
            .unwrap();
        assert!(plane.admit_cache(1000).is_ok());
        assert!(
            plane.heartbeat(1000).is_err(),
            "timeout failure must not be treated as a successful beat"
        );
        assert!(plane.admit_cache(1000).is_err());
        assert!(plane.admit_origin_io().is_ok());
        assert!(
            plane.stream.from_guest.is_empty(),
            "failed timeout setup must precede all product IO"
        );
    }
}

#[test]
pub(super) fn failed_stream_cannot_restart_handshake_even_when_shutdown_fails() {
    let mut stream = ScriptedStream::new(vec![]);
    stream.fail_read = true;
    stream.fail_shutdown = true;
    let mut plane = plane_over(stream);
    assert!(plane.perform_handshake(1000).is_err());
    let sent = plane.stream.from_guest.len();
    assert!(sent > 0, "fixture must reach partial handshake IO");
    plane.stream.fail_read = false;
    assert_eq!(
        plane.perform_handshake(1001),
        Err(ControlPlaneError::Disconnected)
    );
    assert_eq!(
        plane.stream.from_guest.len(),
        sent,
        "a failed stream cannot replay its opening"
    );
    assert!(plane.admit_cache(1001).is_err());
    assert!(plane.admit_origin_io().is_ok());
    assert_eq!(plane.liveness_handle().load(Ordering::Relaxed), 0);
}

#[test]
pub(super) fn partial_handshake_over_real_socket_cannot_extend_absolute_deadline() {
    use std::time::Instant;
    let (guest, mut host) = UnixStream::pair().unwrap();
    host.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    host.set_write_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let peer = std::thread::spawn(move || {
        let (kind, payload, _) = read_frame(&mut host, ipc::MAX_CONTROL_PAYLOAD).unwrap();
        assert_eq!(kind, MSG_HANDSHAKE);
        let opening: Handshake = ipc::decode_control(&payload).unwrap();
        let transcript = HandshakeTranscript {
            min_version: opening.min_version,
            max_version: opening.max_version,
            boot_id: opening.boot_id,
            distro_id: opening.distro_id,
            guest_nonce: opening.guest_nonce,
        };
        let challenge = HostChallenge {
            accepted_version: ipc::IPC_VERSION_3,
            heartbeat_secs: 5,
            lease_timeout_secs: 15,
            host_nonce: [9; 32],
        };
        let ack = HandshakeAck {
            accepted_version: challenge.accepted_version,
            heartbeat_secs: 5,
            lease_timeout_secs: 15,
            host_nonce: challenge.host_nonce,
            host_proof: ipc::host_proof(SECRET, &transcript, &challenge).unwrap(),
        };
        let frame = frame_bytes(MSG_HANDSHAKE_ACK, &ipc::encode_control(&ack).unwrap(), 1);
        let mut sent = 0;
        for byte in &frame {
            if host.write_all(&[*byte]).is_err() {
                break;
            }
            sent += 1;
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            sent > 0 && sent < frame.len(),
            "peer must exercise a genuinely partial response"
        );
    });
    let mut plane = VsockControlPlane::new(
        MockVsockStream(guest),
        SECRET.to_vec(),
        "boot".into(),
        "distro".into(),
        sealed_origin(),
        Duration::from_millis(120),
    );
    let start = Instant::now();
    assert!(plane.perform_handshake(1000).is_err());
    assert!(start.elapsed() < Duration::from_secs(1));
    assert!(plane.admit_cache(1000).is_err());
    assert!(plane.admit_origin_io().is_ok());
    peer.join().unwrap();
}

/// A structurally valid `HandshakeAck` whose proof is all-zero. Valid
/// enough to reach proof verification; never a valid proof (the guest
/// nonce is 32 fresh random bytes).
pub(super) fn bogus_handshake_ack(heartbeat_secs: u64, lease_timeout_secs: u64) -> Vec<u8> {
    let ack = HandshakeAck {
        accepted_version: ipc::IPC_VERSION_3,
        heartbeat_secs,
        lease_timeout_secs,
        host_nonce: [7u8; 32],
        host_proof: [0u8; 32],
    };
    let payload = ipc::encode_control(&ack).expect("encode ack");
    frame_bytes(MSG_HANDSHAKE_ACK, &payload, 1)
}

// --- Handshake: fail-closed paths ------------------------------------

#[test]
pub(super) fn accepted_version_outside_the_supported_range_is_refused() {
    assert_eq!(
        validate_accepted_version(0),
        Err(ControlPlaneError::Protocol(
            "peer accepted version 0, outside the supported range 2..=7".into()
        ))
    );
    assert_eq!(
        validate_accepted_version(1),
        Err(ControlPlaneError::Protocol(
            "peer accepted version 1, outside the supported range 2..=7".into()
        ))
    );
    assert_eq!(
        validate_accepted_version(8),
        Err(ControlPlaneError::Protocol(
            "peer accepted version 8, outside the supported range 2..=7".into()
        ))
    );
    assert_eq!(validate_accepted_version(2), Ok(2));
    assert_eq!(validate_accepted_version(3), Ok(3));
    assert_eq!(validate_accepted_version(4), Ok(4));
    assert_eq!(validate_accepted_version(5), Ok(5));
    assert_eq!(validate_accepted_version(6), Ok(6));
    assert_eq!(validate_accepted_version(7), Ok(7));
}

#[test]
pub(super) fn a_zero_heartbeat_interval_from_the_peer_is_refused_before_any_authority() {
    let stream = ScriptedStream::new(bogus_handshake_ack(0, 30));
    let mut plane = plane_over(stream);
    let err = plane.perform_handshake(1_000).expect_err("must refuse");
    assert!(
        matches!(err, ControlPlaneError::Protocol(ref m) if m.contains("zero heartbeat")),
        "unexpected error: {err:?}"
    );
    assert_eq!(plane.state(), ControlPlaneState::SafeMode);
    assert!(matches!(
        plane.admit_cache(1_000),
        Err(GateError::LeaseDenied(_))
    ));
}

#[test]
pub(super) fn a_zero_lease_timeout_from_the_peer_is_refused() {
    let stream = ScriptedStream::new(bogus_handshake_ack(5, 0));
    let mut plane = plane_over(stream);
    let err = plane.perform_handshake(1_000).expect_err("must refuse");
    assert!(
        matches!(err, ControlPlaneError::Protocol(ref m) if m.contains("zero lease")),
        "unexpected error: {err:?}"
    );
    assert_eq!(plane.state(), ControlPlaneState::SafeMode);
}

#[test]
pub(super) fn a_lease_timeout_that_does_not_outlive_the_heartbeat_is_refused() {
    let stream = ScriptedStream::new(bogus_handshake_ack(10, 10));
    let mut plane = plane_over(stream);
    let err = plane.perform_handshake(1_000).expect_err("must refuse");
    assert!(
        matches!(err, ControlPlaneError::Protocol(ref m) if m.contains("does not outlive")),
        "unexpected error: {err:?}"
    );
    assert_eq!(plane.state(), ControlPlaneState::SafeMode);
}

#[test]
pub(super) fn an_invalid_host_proof_never_grants_a_lease() {
    // Cadence is acceptable; only the proof is wrong.
    let stream = ScriptedStream::new(bogus_handshake_ack(5, 30));
    let mut plane = plane_over(stream);
    let err = plane.perform_handshake(1_000).expect_err("must refuse");
    assert!(
        matches!(
            err,
            ControlPlaneError::Handshake(HandshakeError::ProofMismatch)
        ),
        "unexpected error: {err:?}"
    );
    assert_eq!(plane.state(), ControlPlaneState::SafeMode);
    assert!(matches!(
        plane.admit_cache(1_000),
        Err(GateError::LeaseDenied(_))
    ));
    // Origin I/O is preserved: losing the lease must not take the
    // authoritative path down (RF-3).
    assert!(plane.admit_origin_io().is_ok());
}

#[test]
pub(super) fn an_unexpected_message_type_mid_handshake_is_refused() {
    let heartbeat = ipc::encode_control(&Heartbeat { timestamp_ms: 1 }).expect("encode");
    let stream = ScriptedStream::new(frame_bytes(MSG_HEARTBEAT, &heartbeat, 1));
    let mut plane = plane_over(stream);
    let err = plane.perform_handshake(1_000).expect_err("must refuse");
    assert!(
        matches!(err, ControlPlaneError::Protocol(ref m) if m.contains("expected handshake ack")),
        "unexpected error: {err:?}"
    );
    assert_eq!(plane.state(), ControlPlaneState::SafeMode);
}

#[test]
pub(super) fn a_truncated_stream_during_handshake_fails_closed() {
    let stream = ScriptedStream::new(vec![0x52, 0x41, 0x4D]);
    let mut plane = plane_over(stream);
    let err = plane.perform_handshake(1_000).expect_err("must refuse");
    assert!(
        matches!(
            err,
            ControlPlaneError::Frame(FrameError::TruncatedFrame { .. })
        ),
        "unexpected error: {err:?}"
    );
    assert_eq!(plane.state(), ControlPlaneState::SafeMode);
}

#[test]
pub(super) fn a_write_failure_during_handshake_fails_closed() {
    let mut stream = ScriptedStream::new(Vec::new());
    stream.fail_write = true;
    let mut plane = plane_over(stream);
    let err = plane.perform_handshake(1_000).expect_err("must refuse");
    assert!(
        matches!(err, ControlPlaneError::Frame(FrameError::StreamIo(_))),
        "unexpected error: {err:?}"
    );
    assert_eq!(plane.state(), ControlPlaneState::SafeMode);
}

#[test]
pub(super) fn heartbeat_before_a_lease_is_refused_without_touching_the_stream() {
    let stream = ScriptedStream::new(Vec::new());
    let mut plane = plane_over(stream);
    assert_eq!(plane.heartbeat(1_000), Err(ControlPlaneError::Disconnected));
    assert!(plane.stream.from_guest.is_empty());
}

#[path = "tests/heartbeat_and_vsock.rs"]
mod heartbeat_and_vsock;
