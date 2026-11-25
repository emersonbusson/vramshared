// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/control_plane/tests/heartbeat_and_vsock.rs
use super::*;

// --- HeartbeatLoop with a fake plane ---------------------------------

struct FakePlane {
    beats: Vec<u64>,
    fail_on: Vec<usize>,
    state: ControlPlaneState,
}

impl FakePlane {
    fn new() -> Self {
        Self {
            beats: Vec::new(),
            fail_on: Vec::new(),
            state: ControlPlaneState::VsockLeased,
        }
    }

    fn failing_on(mut self, indices: &[usize]) -> Self {
        self.fail_on = indices.to_vec();
        self
    }
}

impl ControlPlane for FakePlane {
    fn state(&self) -> ControlPlaneState {
        self.state
    }
    fn admit_cache(&self, _now_ms: u64) -> Result<(), GateError> {
        Ok(())
    }
    fn admit_origin_io(&self) -> Result<(), GateError> {
        Ok(())
    }
    fn heartbeat(&mut self, now_ms: u64) -> Result<(), ControlPlaneError> {
        let index = self.beats.len();
        self.beats.push(now_ms);
        if self.fail_on.contains(&index) {
            self.state = ControlPlaneState::SafeMode;
            return Err(ControlPlaneError::Disconnected);
        }
        Ok(())
    }
}

#[test]
fn heartbeat_loop_rejects_a_zero_interval_or_zero_failure_budget() {
    assert!(matches!(
        HeartbeatLoop::new(0, 3),
        Err(ControlPlaneError::Protocol(_))
    ));
    assert!(matches!(
        HeartbeatLoop::new(100, 0),
        Err(ControlPlaneError::Protocol(_))
    ));
}

#[test]
fn heartbeat_loop_runs_every_requested_tick_when_all_beats_succeed() {
    let loop_ctl = HeartbeatLoop::new(50, 3).expect("valid loop");
    let mut plane = FakePlane::new();
    let mut slept = Vec::new();
    let stop = loop_ctl.run(&mut plane, 4, || 1_000, |ms| slept.push(ms));
    assert_eq!(stop, HeartbeatLoopStop::TicksExhausted { ticks: 4 });
    assert_eq!(plane.beats, vec![1_000, 1_000, 1_000, 1_000]);
    assert_eq!(slept, vec![50, 50, 50], "no sleep after the final beat");
}

#[test]
fn heartbeat_loop_stops_at_the_consecutive_failure_budget() {
    let loop_ctl = HeartbeatLoop::new(50, 2).expect("valid loop");
    // Beats 0 and 1 fail; the budget is 2, so the loop must stop at tick 2
    // and never reach the remaining two.
    let mut plane = FakePlane::new().failing_on(&[0, 1]);
    let stop = loop_ctl.run(&mut plane, 4, || 0, |_| {});
    assert_eq!(
        stop,
        HeartbeatLoopStop::Failure {
            ticks: 2,
            consecutive_failures: 2,
            error: ControlPlaneError::Disconnected,
        }
    );
    assert_eq!(plane.beats.len(), 2);
}

#[test]
fn heartbeat_loop_treats_an_isolated_failure_as_transient_and_continues() {
    let loop_ctl = HeartbeatLoop::new(50, 3).expect("valid loop");
    // Only beat 1 fails; the budget is 3, so the loop recovers and finishes.
    let mut plane = FakePlane::new().failing_on(&[1]);
    let stop = loop_ctl.run(&mut plane, 4, || 0, |_| {});
    assert_eq!(stop, HeartbeatLoopStop::TicksExhausted { ticks: 4 });
    assert_eq!(plane.beats.len(), 4);
}

#[test]
fn heartbeat_loop_of_zero_ticks_is_a_no_op() {
    let loop_ctl = HeartbeatLoop::new(50, 3).expect("valid loop");
    let mut plane = FakePlane::new();
    let stop = loop_ctl.run(&mut plane, 0, || 0, |_| panic!("must not sleep"));
    assert_eq!(stop, HeartbeatLoopStop::TicksExhausted { ticks: 0 });
    assert!(plane.beats.is_empty());
}

// --- End-to-end over a real socket pair ------------------------------

/// Drive the host side of DT-3 over `host`, then service `beats`
/// heartbeats. `close_after_beats` drops the socket instead of acking the
/// next beat, which is the disconnect the guest must fail closed on.
fn spawn_host(
    host: UnixStream,
    heartbeat_secs: u64,
    lease_timeout_secs: u64,
    beats: Vec<HeartbeatAck>,
    close_instead_of_ack: bool,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut s = host;

        let (msg_type, payload, _) =
            read_frame(&mut s, ipc::MAX_CONTROL_PAYLOAD).expect("read handshake");
        assert_eq!(msg_type, MSG_HANDSHAKE);
        let opening: Handshake = ipc::decode_control(&payload).expect("decode handshake");
        let transcript = HandshakeTranscript {
            min_version: opening.min_version,
            max_version: opening.max_version,
            boot_id: opening.boot_id.clone(),
            distro_id: opening.distro_id.clone(),
            guest_nonce: opening.guest_nonce,
        };
        assert!(
            ipc::verify_guest_proof(SECRET, &transcript, &opening.guest_proof),
            "host must be able to verify the guest opening proof"
        );

        let host_nonce = [0x2Cu8; 32];
        let challenge = HostChallenge {
            accepted_version: ipc::IPC_VERSION_3,
            heartbeat_secs,
            lease_timeout_secs,
            host_nonce,
        };
        let ack = HandshakeAck {
            accepted_version: ipc::IPC_VERSION_3,
            heartbeat_secs,
            lease_timeout_secs,
            host_nonce,
            host_proof: ipc::host_proof(SECRET, &transcript, &challenge).expect("host proof"),
        };
        let payload = ipc::encode_control(&ack).expect("encode ack");
        write_frame(&mut s, MSG_HANDSHAKE_ACK, &payload, 1).expect("write ack");

        let (msg_type, payload, _) =
            read_frame(&mut s, ipc::MAX_CONTROL_PAYLOAD).expect("read finish");
        assert_eq!(msg_type, MSG_HANDSHAKE_FINISH);
        let finish: HandshakeFinish = ipc::decode_control(&payload).expect("decode finish");
        assert!(
            ipc::verify_guest_finish_proof(
                SECRET,
                &transcript,
                &challenge,
                &finish.guest_finish_proof
            ),
            "host must be able to verify the guest finish proof"
        );

        let (msg_type, payload, _) =
            read_frame(&mut s, ipc::MAX_CONTROL_PAYLOAD).expect("read lease request");
        assert_eq!(msg_type, MSG_LEASE_REQUEST);
        let request: LeaseRequest = ipc::decode_control(&payload).expect("decode lease request");
        assert_eq!(request.nonce, opening.guest_nonce.to_vec());

        let granted = LeaseGranted {
            lease_id: 4242,
            deadline_ms: 60_000,
        };
        let payload = ipc::encode_control(&granted).expect("encode granted");
        write_frame(&mut s, MSG_LEASE_GRANTED, &payload, 3).expect("write granted");

        for (index, beat_ack) in beats.iter().enumerate() {
            if close_instead_of_ack && index + 1 == beats.len() {
                drop(s);
                return;
            }
            let (msg_type, payload, _) =
                read_frame(&mut s, ipc::MAX_CONTROL_PAYLOAD).expect("read beat");
            assert_eq!(msg_type, MSG_HEARTBEAT, "beat {index}");
            let beat: Heartbeat = ipc::decode_control(&payload).expect("decode beat");
            assert!(beat.timestamp_ms > 0);
            let payload = ipc::encode_control(beat_ack).expect("encode beat ack");
            write_frame(&mut s, MSG_HEARTBEAT_ACK, &payload, 4).expect("write beat ack");
        }
    })
}

#[test]
fn a_full_handshake_grants_a_lease_that_admits_cache() {
    let (guest, host) = UnixStream::pair().expect("pair");
    let host_thread = spawn_host(
        host,
        5,
        30,
        vec![HeartbeatAck {
            timestamp_ms: 0,
            lease_remaining_ms: 25_000,
            host_memory: None,
        }],
        false,
    );

    let mut plane = plane_over(MockVsockStream(guest));
    plane.perform_handshake(1_000).expect("handshake");
    assert_eq!(plane.state(), ControlPlaneState::VsockLeased);
    assert_eq!(
        plane.cadence(),
        Some(NegotiatedCadence {
            heartbeat_secs: 5,
            lease_timeout_secs: 30
        })
    );
    assert_eq!(plane.lease_deadline_ms(), 60_000);
    assert_eq!(
        plane
            .liveness_handle()
            .load(std::sync::atomic::Ordering::Relaxed),
        1_000
    );
    assert!(plane.admit_cache(1_000).is_ok());
    assert!(plane.admit_origin_io().is_ok());

    plane.heartbeat(2_000).expect("beat");
    assert_eq!(
        plane
            .liveness_handle()
            .load(std::sync::atomic::Ordering::Relaxed),
        2_000
    );
    assert_eq!(
        plane.lease_deadline_ms(),
        25_000 + 2_000,
        "a beat renews the deadline from the ack"
    );

    host_thread.join().expect("host thread");
}

#[test]
fn unsigned_legacy_heartbeat_cannot_publish_authoritative_host_memory() {
    let (guest, host) = UnixStream::pair().unwrap();
    let host = spawn_host(
        host,
        5,
        30,
        vec![HeartbeatAck {
            timestamp_ms: 1000,
            lease_remaining_ms: 25_000,
            host_memory: Some(ipc::HostMemoryTelemetry {
                sampled_at_unix_ms: 1000,
                total_bytes: 1024,
                available_bytes: 512,
                commit_total_bytes: 128,
                commit_limit_bytes: 2048,
                page_size_bytes: 4096,
                vmmem_wsl_working_set_bytes: None,
            }),
        }],
        false,
    );
    let mut plane = plane_over(MockVsockStream(guest));
    plane.perform_handshake(1000).unwrap();
    plane.heartbeat(1001).unwrap();
    assert!(plane.host_memory_handle().lock().unwrap().is_none());
    assert!(plane.admit_origin_io().is_ok());
    host.join().unwrap();
}

#[test]
fn the_authority_mirror_carries_the_plane_authority() {
    // The daemon consults the authority from its own backend, and the
    // plane owns the live one. The mirror is what makes the two the same
    // view; without it the backend saw a snapshot taken at handshake time
    // and refused the cache one lease window later.
    let (guest, host) = UnixStream::pair().expect("pair");
    let host_thread = spawn_host(
        host,
        5,
        30,
        vec![HeartbeatAck {
            timestamp_ms: 0,
            lease_remaining_ms: 45_000,
            host_memory: None,
        }],
        false,
    );
    let shared = crate::host_gate::SharedAuthority::new();
    let mut plane =
        plane_over(MockVsockStream(guest)).with_authority_mirror(shared.begin_session());
    plane.perform_handshake(1_000).expect("handshake");
    assert_eq!(
        shared.slot().lock().expect("mirror").state(),
        ControlPlaneState::VsockLeased,
        "the mirror must carry the granted lease"
    );
    plane.heartbeat(2_000).expect("beat");
    let mirrored_deadline = shared
        .slot()
        .lock()
        .expect("mirror")
        .lease()
        .expect("lease")
        .deadline_ms;
    assert_eq!(
        mirrored_deadline,
        plane.lease_deadline_ms(),
        "the mirror must follow the beat's deadline"
    );
    host_thread.join().expect("host thread");
}

#[test]
fn a_beat_slides_the_authorities_own_lease_deadline() {
    // The cache gate reads the authority's lease token, not the plane's
    // private deadline. A beat that only slid the private one left the
    // token at the handshake's value, so the cache was refused again one
    // lease window later while the heartbeats were alive — the daemon
    // copied that stale authority and published "lease expired" while the
    // host was still beating.
    let (guest, host) = UnixStream::pair().expect("pair");
    let host_thread = spawn_host(
        host,
        5,
        30,
        vec![HeartbeatAck {
            timestamp_ms: 0,
            lease_remaining_ms: 45_000,
            host_memory: None,
        }],
        false,
    );

    let mut plane = plane_over(MockVsockStream(guest));
    plane.perform_handshake(1_000).expect("handshake");
    let granted_deadline = plane
        .authority()
        .lease()
        .expect("the handshake must mint a lease")
        .deadline_ms;
    plane.heartbeat(2_000).expect("beat");
    let authority_deadline = plane
        .authority()
        .lease()
        .expect("the lease must survive the beat")
        .deadline_ms;
    assert_eq!(
        authority_deadline,
        plane.lease_deadline_ms(),
        "the authority's lease token must follow the beat"
    );
    assert!(
        authority_deadline != granted_deadline,
        "the beat must move the deadline, not leave it at the handshake's"
    );
    assert!(
        plane.admit_cache(authority_deadline - 1).is_ok(),
        "the cache must stay admitted up to the slid deadline"
    );

    host_thread.join().expect("host thread");
}

#[test]
fn a_beat_that_reports_no_remaining_lease_moves_the_plane_to_origin_only() {
    let (guest, host) = UnixStream::pair().expect("pair");
    let host_thread = spawn_host(
        host,
        5,
        30,
        vec![HeartbeatAck {
            timestamp_ms: 0,
            lease_remaining_ms: 0,
            host_memory: None,
        }],
        false,
    );

    let mut plane = plane_over(MockVsockStream(guest));
    plane.perform_handshake(1_000).expect("handshake");
    let err = plane.heartbeat(2_000).expect_err("lease must expire");
    assert_eq!(err, ControlPlaneError::LeaseExpired);
    assert_eq!(plane.state(), ControlPlaneState::OriginOnly);
    assert!(matches!(
        plane.admit_cache(2_000),
        Err(GateError::LeaseDenied(_))
    ));
    assert!(plane.admit_origin_io().is_ok());

    host_thread.join().expect("host thread");
}

#[test]
fn a_transport_failure_on_a_beat_fails_closed_to_safe_mode() {
    let (guest, host) = UnixStream::pair().expect("pair");
    // One beat is expected; the host closes instead of acking it.
    let host_thread = spawn_host(
        host,
        5,
        30,
        vec![HeartbeatAck {
            timestamp_ms: 0,
            lease_remaining_ms: 25_000,
            host_memory: None,
        }],
        true,
    );

    let mut plane = plane_over(MockVsockStream(guest));
    plane.perform_handshake(1_000).expect("handshake");
    assert_eq!(plane.state(), ControlPlaneState::VsockLeased);

    let err = plane.heartbeat(2_000).expect_err("must fail closed");
    assert!(
        matches!(
            err,
            ControlPlaneError::Frame(FrameError::TruncatedFrame { .. })
                | ControlPlaneError::Frame(FrameError::StreamIo(_))
        ),
        "unexpected error: {err:?}"
    );
    assert_eq!(plane.state(), ControlPlaneState::SafeMode);
    assert!(matches!(
        plane.admit_cache(2_000),
        Err(GateError::LeaseDenied(_))
    ));
    assert!(
        plane.admit_origin_io().is_ok(),
        "RF-3: origin I/O survives lease loss"
    );

    host_thread.join().expect("host thread");
}

#[test]
fn a_host_thread_can_reject_the_lease_and_the_guest_keeps_no_authority() {
    let (guest, host) = UnixStream::pair().expect("pair");
    let host_thread = std::thread::spawn(move || {
        let mut s = host;
        let (_msg_type, payload, _) =
            read_frame(&mut s, ipc::MAX_CONTROL_PAYLOAD).expect("read handshake");
        let opening: Handshake = ipc::decode_control(&payload).expect("decode handshake");
        let transcript = HandshakeTranscript {
            min_version: opening.min_version,
            max_version: opening.max_version,
            boot_id: opening.boot_id.clone(),
            distro_id: opening.distro_id.clone(),
            guest_nonce: opening.guest_nonce,
        };
        let host_nonce = [0x2Cu8; 32];
        let challenge = HostChallenge {
            accepted_version: ipc::IPC_VERSION_3,
            heartbeat_secs: 5,
            lease_timeout_secs: 30,
            host_nonce,
        };
        let ack = HandshakeAck {
            accepted_version: ipc::IPC_VERSION_3,
            heartbeat_secs: 5,
            lease_timeout_secs: 30,
            host_nonce,
            host_proof: ipc::host_proof(SECRET, &transcript, &challenge).expect("host proof"),
        };
        let payload = ipc::encode_control(&ack).expect("encode ack");
        write_frame(&mut s, MSG_HANDSHAKE_ACK, &payload, 1).expect("write ack");

        // Drain the finish + request, then deny.
        let _ = read_frame(&mut s, ipc::MAX_CONTROL_PAYLOAD).expect("finish");
        let _ = read_frame(&mut s, ipc::MAX_CONTROL_PAYLOAD).expect("request");
        let denied = LeaseDenied {
            reason: "guardian proof stale".into(),
        };
        let payload = ipc::encode_control(&denied).expect("encode denied");
        write_frame(&mut s, MSG_LEASE_DENIED, &payload, 3).expect("write denied");
    });

    let mut plane = plane_over(MockVsockStream(guest));
    let err = plane.perform_handshake(1_000).expect_err("must be denied");
    assert!(
        matches!(err, ControlPlaneError::LeaseDenied(ref r) if r.contains("guardian")),
        "unexpected error: {err:?}"
    );
    assert_eq!(plane.state(), ControlPlaneState::SafeMode);
    assert!(matches!(
        plane.admit_cache(1_000),
        Err(GateError::LeaseDenied(_))
    ));

    host_thread.join().expect("host thread");
}

#[test]
fn v5_transient_unavailable_storage_revokes_storage_without_dropping_lease() {
    use ramshared_block::vhd_identity::RootVhdObservation;
    use ramshared_winsvc::host_storage::{HostFileIdentity, HostStorageReport, HostVolumeSample};
    use ramshared_winsvc::vsock_lease::{
        HostSamples, LeasePolicy, StorageSample, serve_connection,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    const DISK_ID: [u8; 16] = [0x5Au8; 16];
    fn observe_stub() -> Option<RootVhdObservation> {
        Some(RootVhdObservation {
            major: 8,
            minor: 48,
            identifier: DISK_ID,
            observed_at_unix_ms: 1_000,
            observed_at: Instant::now(),
        })
    }

    let (guest, mut host) = UnixStream::pair().expect("pair");
    host.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    host.set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_srv = Arc::clone(&calls);
    let report = HostStorageReport {
        schema_version: 1,
        producer: ramshared_winsvc::gpu_identity::ProducerProcess {
            pid: 10,
            creation_time_100ns: 20,
        },
        started_at_unix_ms: 100,
        elapsed_ms: 2,
        sample: HostVolumeSample {
            sampled_at_unix_ms: 101,
            volume_id: r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\".into(),
            file_identity: HostFileIdentity {
                volume_serial: 1,
                file_index: 42,
            },
            total_bytes: 100 << 30,
            free_bytes: 60 << 30,
            available_bytes: 50 << 30,
        },
        virtual_disk_id: Some(DISK_ID),
    };
    let policy = LeasePolicy::new(1, 20).expect("policy");
    let host_thread = std::thread::spawn(move || {
        serve_connection(
            &mut host,
            SECRET,
            policy,
            || 1_000,
            move || {
                let base_report = report.clone();
                let calls = Arc::clone(&calls_srv);
                HostSamples {
                    storage: Some(Arc::new(move |requested| {
                        let attempt = calls.fetch_add(1, Ordering::Relaxed);
                        if requested != DISK_ID || attempt == 1 {
                            return None;
                        }
                        let mut sample_report = base_report.clone();
                        if attempt >= 2 {
                            sample_report.sample.available_bytes = 49 << 30;
                        }
                        Some(StorageSample {
                            report: sample_report,
                            age_ms: 5,
                            captured_since: Instant::now(),
                            complete: true,
                        })
                    })),
                    ..HostSamples::default()
                }
            },
        )
    });

    let authority = crate::host_gate::SharedAuthority::new();
    let mirror = authority.begin_session();
    let shared_storage = crate::host_storage_state::SharedHostStorage::new();
    let mut plane = plane_over(MockVsockStream(guest)).with_authority_mirror(mirror);
    plane.attach_host_storage(shared_storage.clone());
    plane.set_root_vhd_observer(observe_stub);
    plane.perform_handshake(1_000).expect("handshake");

    plane.heartbeat(1_500).expect("beat 1");
    assert_eq!(plane.state(), ControlPlaneState::VsockLeased);
    assert_eq!(
        shared_storage.telemetry(false).unwrap().available_bytes,
        50 << 30
    );

    plane
        .heartbeat(2_000)
        .expect("beat 2 must survive transient None");
    assert_eq!(plane.state(), ControlPlaneState::VsockLeased);
    assert!(
        shared_storage.telemetry(false).is_none(),
        "transient unavailable sample must revoke host_storage fail-closed"
    );

    plane.heartbeat(2_500).expect("beat 3 must recover storage");
    assert_eq!(plane.state(), ControlPlaneState::VsockLeased);
    assert_eq!(
        shared_storage.telemetry(false).unwrap().available_bytes,
        49 << 30
    );

    drop(plane);
    let _ = host_thread.join().expect("host thread");
}
