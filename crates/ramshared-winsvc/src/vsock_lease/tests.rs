// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/vsock_lease/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use ramshared_ipc::{
    Heartbeat, HeartbeatAck, HostMemoryTelemetry, IPC_VERSION_3, MSG_HEARTBEAT, MSG_HEARTBEAT_ACK,
    MSG_LEASE_RELEASE, MSG_LEASE_REQUEST, encode_control, guest_finish_proof, guest_proof,
    read_frame, verify_host_proof, write_frame,
};
use std::error::Error;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";
const POLICY: LeasePolicy = LeasePolicy {
    heartbeat_secs: 5,
    lease_timeout_secs: 15,
};

fn opening(secret: &[u8]) -> (HandshakeTranscript, Handshake) {
    let guest_nonce = random_nonce().unwrap();
    let transcript = HandshakeTranscript {
        min_version: IPC_MIN_VERSION,
        max_version: IPC_VERSION_3,
        boot_id: "11111111-2222-3333-4444-555555555555".into(),
        distro_id: "ubuntu".into(),
        guest_nonce,
    };
    let handshake = Handshake {
        min_version: transcript.min_version,
        max_version: transcript.max_version,
        boot_id: transcript.boot_id.clone(),
        distro_id: transcript.distro_id.clone(),
        guest_nonce,
        guest_proof: guest_proof(secret, &transcript).unwrap(),
    };
    (transcript, handshake)
}

/// Guest side of the opening: send `Handshake`, verify the host proof, send
/// `HandshakeFinish`. Returns the nonce for the lease request.
fn guest_open(stream: &mut UnixStream, secret: &[u8]) -> (HandshakeTranscript, [u8; 32]) {
    let (transcript, handshake) = opening(secret);
    write_frame(
        stream,
        MSG_HANDSHAKE,
        &encode_control(&handshake).unwrap(),
        1,
    )
    .unwrap();
    let (msg_type, payload, _) = read_frame(stream, MAX_CONTROL_PAYLOAD).unwrap();
    assert_eq!(msg_type, MSG_HANDSHAKE_ACK);
    let ack: HandshakeAck = decode_control(&payload).unwrap();
    let challenge = HostChallenge {
        accepted_version: ack.accepted_version,
        heartbeat_secs: ack.heartbeat_secs,
        lease_timeout_secs: ack.lease_timeout_secs,
        host_nonce: ack.host_nonce,
    };
    assert!(verify_host_proof(
        secret,
        &transcript,
        &challenge,
        &ack.host_proof
    ));
    let finish = HandshakeFinish {
        guest_finish_proof: guest_finish_proof(secret, &transcript, &challenge).unwrap(),
    };
    write_frame(
        stream,
        MSG_HANDSHAKE_FINISH,
        &encode_control(&finish).unwrap(),
        2,
    )
    .unwrap();
    let nonce = transcript.guest_nonce;
    (transcript, nonce)
}

fn guest_request(stream: &mut UnixStream, nonce: [u8; 32]) -> LeaseGranted {
    let request = LeaseRequest {
        nonce: nonce.to_vec(),
    };
    write_frame(
        stream,
        MSG_LEASE_REQUEST,
        &encode_control(&request).unwrap(),
        3,
    )
    .unwrap();
    let (msg_type, payload, _) = read_frame(stream, MAX_CONTROL_PAYLOAD).unwrap();
    assert_eq!(msg_type, MSG_LEASE_GRANTED);
    decode_control(&payload).unwrap()
}

fn guest_beat(stream: &mut UnixStream, timestamp_ms: u64) -> HeartbeatAck {
    let beat = Heartbeat { timestamp_ms };
    write_frame(stream, MSG_HEARTBEAT, &encode_control(&beat).unwrap(), 4).unwrap();
    let (msg_type, payload, _) = read_frame(stream, MAX_CONTROL_PAYLOAD).unwrap();
    assert_eq!(msg_type, MSG_HEARTBEAT_ACK);
    decode_control(&payload).unwrap()
}

#[test]
fn config_is_optional_and_defaults_to_a_sane_cadence() {
    assert_eq!(
        VsockConfig::from_toml("[win_drive]\nsize_bytes = 1\n").unwrap(),
        None
    );
    let config =
        VsockConfig::from_toml("[vsock]\nport = 2761\nkey_file = \"/etc/ramshared/vsock.key\"\n")
            .unwrap()
            .expect("configured");
    assert_eq!(config.port, 2761);
    assert_eq!(config.heartbeat_secs, DEFAULT_HEARTBEAT_SECS);
    assert_eq!(config.lease_timeout_secs, DEFAULT_LEASE_TIMEOUT_SECS);
    assert_eq!(config.key_protection, KeyProtection::Plain);

    let protected = VsockConfig::from_toml(
        "[vsock]\nport = 2761\nkey_file = \"/k\"\nkey_protection = \"dpapi_machine\"\n",
    )
    .unwrap()
    .expect("configured");
    assert_eq!(protected.key_protection, KeyProtection::DpapiMachine);

    let connected = VsockConfig::from_toml(
        "[vsock]\nport = 2761\nkey_file = \"/k\"\nkey_protection = \"dpapi_machine\"\nmode = \"connect\"\nvm_id = \"d01a61a9-94e3-416a-89eb-f6eb128b179d\"\n",
    )
    .unwrap()
    .expect("configured");
    assert_eq!(connected.mode, VsockMode::Connect);
    assert_eq!(
        connected.vm_id.as_deref(),
        Some("d01a61a9-94e3-416a-89eb-f6eb128b179d")
    );
    assert_eq!(
        parse_guid("d01a61a9-94e3-416a-89eb-f6eb128b179d").unwrap(),
        [
            0xd0, 0x1a, 0x61, 0xa9, 0x94, 0xe3, 0x41, 0x6a, 0x89, 0xeb, 0xf6, 0xeb, 0x12, 0x8b,
            0x17, 0x9d
        ]
    );

    // A TOML basic string with a doubled backslash must recover the
    // literal path under the Unix rules used by these tests.
    let escaped = VsockConfig::from_toml(
        "[vsock]\nport = 2761\nkey_file = \"/tmp/vsock\\\\key\"\nkey_protection = \"dpapi_machine\"\n",
    )
    .unwrap()
    .expect("configured");
    assert_eq!(
        escaped.key_file,
        std::path::PathBuf::from(r"/tmp/vsock\key")
    );
}

#[test]
fn invalid_config_fails_closed() {
    assert!(
        VsockConfig::from_toml("[vsock]\nport = 0\nkey_file = \"/k\"\n").is_err(),
        "zero port must be refused"
    );
    assert!(
        VsockConfig::from_toml("[vsock]\nport = 2147483648\nkey_file = \"/k\"\n").is_err(),
        "port above the Linux vsock range must be refused"
    );
    assert!(
        VsockConfig::from_toml("[vsock]\nport = 1\nkey_file = \"relative\"\n").is_err(),
        "relative key path must be refused"
    );
    assert!(
        VsockConfig::from_toml(
            "[vsock]\nport = 1\nkey_file = \"/k\"\nheartbeat_secs = 15\nlease_timeout_secs = 15\n"
        )
        .is_err(),
        "lease must outlive the beat"
    );
    assert!(
        VsockConfig::from_toml(
            "[vsock]\nport = 1\nkey_file = \"/k\"\nkey_protection = \"rot13\"\n"
        )
        .is_err(),
        "unknown key protection must be refused"
    );
    assert!(
        VsockConfig::from_toml("[vsock]\nport = 1\nkey_file = \"/k\"\nmode = \"connect\"\n")
            .is_ok(),
        "connect mode without vm_id must auto-resolve, not be refused"
    );
    assert!(
        VsockConfig::from_toml(
            "[vsock]\nport = 1\nkey_file = \"/k\"\nmode = \"connect\"\nvm_id = \"not-a-guid\"\n"
        )
        .is_err(),
        "connect mode rejects an invalid VM ID"
    );
}

#[test]
fn the_heartbeat_ack_carries_the_host_memory_sample() {
    // The host has no PSI, so the operator's view of the host's pressure
    // comes from this sample. A host that cannot sample sends nothing;
    // a host that can must say so on every authenticated beat.
    let sample = HostMemoryTelemetry {
        sampled_at_unix_ms: 7,
        total_bytes: 1 << 30,
        available_bytes: 1 << 29,
        commit_total_bytes: 1 << 30,
        commit_limit_bytes: 2 << 30,
        page_size_bytes: 4096,
        vmmem_wsl_working_set_bytes: None,
    };
    let (mut guest, mut host) = UnixStream::pair().unwrap();
    let server = std::thread::spawn(move || {
        serve_connection(
            &mut host,
            SECRET,
            POLICY,
            || 0,
            || HostSamples {
                memory: Some(sample.clone()),
                gpu: None,
                ..HostSamples::default()
            },
        )
    });
    let (_, nonce) = guest_open(&mut guest, SECRET);
    let _granted = guest_request(&mut guest, nonce);
    write_frame(
        &mut guest,
        MSG_HEARTBEAT,
        &encode_control(&Heartbeat { timestamp_ms: 1 }).unwrap(),
        4,
    )
    .unwrap();
    let (msg_type, payload, _) = read_frame(&mut guest, MAX_CONTROL_PAYLOAD).unwrap();
    assert_eq!(msg_type, MSG_HEARTBEAT_ACK);
    let ack: HeartbeatAck = decode_control(&payload).unwrap();
    let published = ack.host_memory.expect("the host must publish its sample");
    assert_eq!(published.available_bytes, 1 << 29);
    assert_eq!(published.page_size_bytes, 4096);
    // The release ends the server's beat loop, so the join cannot hang.
    write_frame(
        &mut guest,
        MSG_LEASE_RELEASE,
        &encode_control(&LeaseRequest { nonce: Vec::new() }).unwrap(),
        4,
    )
    .unwrap();
    let _ = server.join();
}

#[test]
fn wrong_key_is_refused_before_any_grant() {
    let (mut guest, mut host) = UnixStream::pair().unwrap();
    let server = std::thread::spawn(move || {
        serve_connection(&mut host, SECRET, POLICY, || 0, HostSamples::default)
    });
    let (_, handshake) = opening(b"a-different-secret-value-000000000");
    write_frame(
        &mut guest,
        MSG_HANDSHAKE,
        &encode_control(&handshake).unwrap(),
        1,
    )
    .unwrap();
    let error = server.join().unwrap().unwrap_err();
    assert!(matches!(
        error,
        LeaseServerError::Refused("guest opening proof mismatch")
    ));
    assert!(read_frame(&mut guest, MAX_CONTROL_PAYLOAD).is_err());
}

#[test]
fn replayed_lease_request_nonce_is_refused() {
    let (mut guest, mut host) = UnixStream::pair().unwrap();
    let server = std::thread::spawn(move || {
        serve_connection(&mut host, SECRET, POLICY, || 0, HostSamples::default)
    });
    let _ = guest_open(&mut guest, SECRET);
    let request = LeaseRequest {
        nonce: vec![0xAA; 32],
    };
    write_frame(
        &mut guest,
        MSG_LEASE_REQUEST,
        &encode_control(&request).unwrap(),
        3,
    )
    .unwrap();
    let error = server.join().unwrap().unwrap_err();
    assert!(matches!(
        error,
        LeaseServerError::Refused("lease request nonce mismatch")
    ));
}

#[test]
fn handshake_mints_a_lease_and_beats_slide_the_deadline() {
    let (mut guest, mut host) = UnixStream::pair().unwrap();
    let clock = Arc::new(AtomicU64::new(1_000));
    let server_clock = Arc::clone(&clock);
    let server = std::thread::spawn(move || {
        serve_connection(
            &mut host,
            SECRET,
            POLICY,
            move || server_clock.load(Ordering::SeqCst),
            HostSamples::default,
        )
    });

    let (_, nonce) = guest_open(&mut guest, SECRET);
    let granted = guest_request(&mut guest, nonce);
    assert_eq!(granted.deadline_ms, 1_000 + 15_000);

    clock.store(10_000, Ordering::SeqCst);
    let ack = guest_beat(&mut guest, 10_000);
    assert_eq!(ack.lease_remaining_ms, 15_000);
    assert_eq!(ack.timestamp_ms, 10_000);

    clock.store(24_000, Ordering::SeqCst);
    let ack = guest_beat(&mut guest, 24_000);
    assert_eq!(ack.lease_remaining_ms, 15_000);

    write_frame(&mut guest, MSG_LEASE_RELEASE, b"", 4).unwrap();
    assert_eq!(server.join().unwrap().unwrap(), 2);
}

#[test]
fn a_late_beat_gets_zero_remaining_and_ends_the_session() {
    let (mut guest, mut host) = UnixStream::pair().unwrap();
    let clock = Arc::new(AtomicU64::new(0));
    let server_clock = Arc::clone(&clock);
    let server = std::thread::spawn(move || {
        serve_connection(
            &mut host,
            SECRET,
            POLICY,
            move || server_clock.load(Ordering::SeqCst),
            HostSamples::default,
        )
    });

    let (_, nonce) = guest_open(&mut guest, SECRET);
    let granted = guest_request(&mut guest, nonce);
    assert_eq!(granted.deadline_ms, 15_000);

    clock.store(15_000, Ordering::SeqCst);
    let ack = guest_beat(&mut guest, 15_000);
    assert_eq!(ack.lease_remaining_ms, 0);
    assert!(matches!(
        server.join().unwrap().unwrap_err(),
        LeaseServerError::LeaseExpired
    ));
}

#[test]
fn configured_listener_fails_closed_without_a_hyperv_transport() {
    let dir = std::env::temp_dir().join(format!(
        "ramshared-vsock-lease-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let key = dir.join("vsock.key");
    std::fs::write(&key, b"0123456789abcdef0123456789abcdef").unwrap();
    let config = dir.join("winsvc.toml");
    let text = format!("[vsock]\nport = 2761\nkey_file = \"{}\"\n", key.display());
    std::fs::write(&config, text).unwrap();
    let error =
        spawn_configured_listener(Some(&config), &std::env::current_exe().unwrap()).unwrap_err();
    assert!(matches!(
        error,
        LeaseServerError::Vsock(VsockError::Unsupported)
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ramshared-vsock-lease-{tag}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn server_refusal(action: impl FnOnce(&mut UnixStream)) -> LeaseServerError {
    let (mut guest, host) = UnixStream::pair().unwrap();
    let server = std::thread::spawn(move || {
        let mut host = host;
        serve_connection(&mut host, SECRET, POLICY, || 0, HostSamples::default)
    });
    action(&mut guest);
    server.join().unwrap().unwrap_err()
}

#[test]
fn error_display_and_source_name_every_variant() {
    let cases = [
        LeaseServerError::Config("config".into()),
        LeaseServerError::Key("key".into()),
        LeaseServerError::Frame(FrameError::UnknownMessageType(0)),
        LeaseServerError::Handshake(HandshakeError::ProofMismatch),
        LeaseServerError::Vsock(VsockError::Unsupported),
        LeaseServerError::Protocol("order"),
        LeaseServerError::Refused("proof"),
        LeaseServerError::LeaseExpired,
        LeaseServerError::Io(std::io::Error::other("io")),
    ];
    for case in &cases {
        assert!(!case.to_string().is_empty());
        let source = case.source();
        if matches!(
            case,
            LeaseServerError::Config(_)
                | LeaseServerError::Key(_)
                | LeaseServerError::Protocol(_)
                | LeaseServerError::Refused(_)
                | LeaseServerError::LeaseExpired
        ) {
            assert!(source.is_none());
        } else {
            assert!(source.is_some());
        }
    }
    assert!(matches!(
        LeaseServerError::from(FrameError::UnknownMessageType(0)),
        LeaseServerError::Frame(_)
    ));
    assert!(matches!(
        LeaseServerError::from(HandshakeError::ProofMismatch),
        LeaseServerError::Handshake(_)
    ));
}

#[test]
fn config_and_key_failures_fail_closed() {
    let dir = temp_dir("errors");

    let missing = dir.join("missing.toml");
    assert!(matches!(
        VsockConfig::load(&missing),
        Err(LeaseServerError::Config(_))
    ));

    let malformed = dir.join("malformed.toml");
    std::fs::write(&malformed, "not = [valid").unwrap();
    assert!(matches!(
        VsockConfig::load(&malformed),
        Err(LeaseServerError::Config(_))
    ));

    let oversized = dir.join("oversized.toml");
    std::fs::write(&oversized, " ".repeat(MAX_CONFIG_BYTES as usize + 1)).unwrap();
    assert!(matches!(
        VsockConfig::load(&oversized),
        Err(LeaseServerError::Config(_))
    ));

    let base = VsockConfig {
        port: 2761,
        key_file: dir.join("missing.key"),
        heartbeat_secs: 5,
        lease_timeout_secs: 15,
        key_protection: KeyProtection::Plain,
        mode: VsockMode::Listen,
        vm_id: None,
    };
    assert!(matches!(base.read_key(), Err(LeaseServerError::Key(_))));

    let empty = dir.join("empty.key");
    std::fs::write(&empty, b"").unwrap();
    assert!(matches!(
        VsockConfig {
            key_file: empty,
            ..base.clone()
        }
        .read_key(),
        Err(LeaseServerError::Key(_))
    ));

    let oversized_key = dir.join("large.key");
    std::fs::write(&oversized_key, vec![0x41u8; MAX_KEY_BYTES as usize + 1]).unwrap();
    assert!(matches!(
        VsockConfig {
            key_file: oversized_key,
            ..base.clone()
        }
        .read_key(),
        Err(LeaseServerError::Key(_))
    ));

    assert!(matches!(
        VsockConfig {
            key_file: dir.clone(),
            ..base.clone()
        }
        .read_key(),
        Err(LeaseServerError::Key(_))
    ));

    let good = dir.join("good.key");
    std::fs::write(&good, b"secret").unwrap();
    let good_config = VsockConfig {
        key_file: good,
        ..base
    };
    assert_eq!(good_config.read_key().unwrap(), b"secret");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cadence_requires_nonzero_and_outliving_timeout() {
    assert!(LeasePolicy::new(0, 15).is_err());
    assert!(LeasePolicy::new(5, 0).is_err());
    assert!(LeasePolicy::new(15, 15).is_err());
    assert_eq!(LeasePolicy::new(5, 15).unwrap(), POLICY);
}

#[test]
fn handshake_refuses_out_of_order_and_weak_openings() {
    let error = server_refusal(|guest| {
        let beat = Heartbeat { timestamp_ms: 1 };
        write_frame(guest, MSG_HEARTBEAT, &encode_control(&beat).unwrap(), 4).unwrap();
    });
    assert!(matches!(
        error,
        LeaseServerError::Protocol("expected handshake opening")
    ));

    let (_, mut inverted) = opening(SECRET);
    inverted.min_version = 99;
    inverted.max_version = 1;
    let error = server_refusal(move |guest| {
        write_frame(guest, MSG_HANDSHAKE, &encode_control(&inverted).unwrap(), 1).unwrap();
    });
    assert!(matches!(
        error,
        LeaseServerError::Refused("inverted version window")
    ));

    let (_, mut unnamed) = opening(SECRET);
    unnamed.boot_id.clear();
    let error = server_refusal(move |guest| {
        write_frame(guest, MSG_HANDSHAKE, &encode_control(&unnamed).unwrap(), 1).unwrap();
    });
    assert!(matches!(
        error,
        LeaseServerError::Refused("empty identity claim")
    ));

    let (_, mut old) = opening(SECRET);
    old.min_version = 0;
    old.max_version = 0;
    let error = server_refusal(move |guest| {
        write_frame(guest, MSG_HANDSHAKE, &encode_control(&old).unwrap(), 1).unwrap();
    });
    assert!(matches!(
        error,
        LeaseServerError::Refused("no mutually supported protocol version")
    ));
}

#[test]
fn handshake_refuses_a_bad_finish_and_a_skipped_request() {
    let error = server_refusal(|guest| {
        let (_, opening) = opening(SECRET);
        write_frame(guest, MSG_HANDSHAKE, &encode_control(&opening).unwrap(), 1).unwrap();
        let (msg_type, _, _) = read_frame(guest, MAX_CONTROL_PAYLOAD).unwrap();
        assert_eq!(msg_type, MSG_HANDSHAKE_ACK);
        let beat = Heartbeat { timestamp_ms: 1 };
        write_frame(guest, MSG_HEARTBEAT, &encode_control(&beat).unwrap(), 4).unwrap();
    });
    assert!(matches!(
        error,
        LeaseServerError::Protocol("expected handshake finish")
    ));

    let error = server_refusal(|guest| {
        let (_, opening) = opening(SECRET);
        write_frame(guest, MSG_HANDSHAKE, &encode_control(&opening).unwrap(), 1).unwrap();
        let (msg_type, _, _) = read_frame(guest, MAX_CONTROL_PAYLOAD).unwrap();
        assert_eq!(msg_type, MSG_HANDSHAKE_ACK);
        let bogus = HandshakeFinish {
            guest_finish_proof: [0u8; 32],
        };
        write_frame(
            guest,
            MSG_HANDSHAKE_FINISH,
            &encode_control(&bogus).unwrap(),
            2,
        )
        .unwrap();
    });
    assert!(matches!(
        error,
        LeaseServerError::Refused("guest finish proof mismatch")
    ));

    let error = server_refusal(|guest| {
        let (_, _) = guest_open(guest, SECRET);
        let beat = Heartbeat { timestamp_ms: 1 };
        write_frame(guest, MSG_HEARTBEAT, &encode_control(&beat).unwrap(), 4).unwrap();
    });
    assert!(matches!(
        error,
        LeaseServerError::Protocol("expected lease request")
    ));
}

#[test]
fn heartbeat_refuses_unknown_messages_and_honours_the_beat_bound() {
    let (mut guest, mut host) = UnixStream::pair().unwrap();
    let server = std::thread::spawn(move || {
        let mut session = serve_handshake(&mut host, SECRET, POLICY, 0).unwrap();
        serve_heartbeats(
            &mut host,
            &mut session,
            POLICY,
            || 0,
            Some(0),
            HostSamples::default,
            SECRET,
        )
    });
    let (_, nonce) = guest_open(&mut guest, SECRET);
    let _ = guest_request(&mut guest, nonce);
    assert_eq!(server.join().unwrap().unwrap(), 0);

    let (mut guest, mut host) = UnixStream::pair().unwrap();
    let server = std::thread::spawn(move || {
        let mut session = serve_handshake(&mut host, SECRET, POLICY, 0).unwrap();
        serve_heartbeats(
            &mut host,
            &mut session,
            POLICY,
            || 0,
            None,
            HostSamples::default,
            SECRET,
        )
    });
    let (_, nonce) = guest_open(&mut guest, SECRET);
    let _ = guest_request(&mut guest, nonce);
    let request = LeaseRequest {
        nonce: nonce.to_vec(),
    };
    write_frame(
        &mut guest,
        MSG_LEASE_REQUEST,
        &encode_control(&request).unwrap(),
        3,
    )
    .unwrap();
    assert!(matches!(
        server.join().unwrap().unwrap_err(),
        LeaseServerError::Protocol("expected heartbeat or lease release")
    ));
}

#[test]
fn config_without_a_vsock_table_does_not_start_a_listener() {
    let dir = temp_dir("no-vsock");
    let config = dir.join("winsvc.toml");
    std::fs::write(&config, "[win_drive]\nsize_bytes = 1\n").unwrap();
    spawn_configured_listener(Some(&config), &std::env::current_exe().unwrap()).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(not(windows))]
#[test]
fn dpapi_key_is_refused_off_windows() {
    let dir = temp_dir("dpapi");
    let key = dir.join("vsock.key");
    std::fs::write(&key, b"not-a-dpapi-blob").unwrap();
    let config = VsockConfig {
        port: 2761,
        key_file: key,
        heartbeat_secs: 5,
        lease_timeout_secs: 15,
        key_protection: KeyProtection::DpapiMachine,
        mode: VsockMode::Listen,
        vm_id: None,
    };
    assert!(matches!(config.read_key(), Err(LeaseServerError::Key(_))));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn absent_config_does_not_start_a_listener() {
    spawn_configured_listener(None, &std::env::current_exe().unwrap()).unwrap();
}
#[test]
fn owned_control_stop_interrupts_authenticated_idle_session() {
    let (mut guest, mut host) = UnixStream::pair().unwrap();
    let mut task = ControlPlaneTask::spawn("authenticated-stop-test", move |stop| {
        let error = serve_connection_with_stop(
            &mut host,
            SECRET,
            POLICY,
            || 1000,
            HostSamples::default,
            Some(&stop),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("control-plane stop requested"),
            "{error}"
        );
        Ok(())
    })
    .unwrap();
    let (_, nonce) = guest_open(&mut guest, SECRET);
    guest_request(&mut guest, nonce);
    write_frame(
        &mut guest,
        MSG_HEARTBEAT,
        &encode_control(&Heartbeat { timestamp_ms: 1001 }).unwrap(),
        4,
    )
    .unwrap();
    assert_eq!(
        read_frame(&mut guest, MAX_CONTROL_PAYLOAD).unwrap().0,
        MSG_HEARTBEAT_ACK
    );
    assert_eq!(
        task.wait_for_stop(Duration::from_secs(1)).unwrap(),
        StopState::Stopped
    );
    guest
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    assert_eq!(guest.read(&mut [0; 1]).unwrap(), 0);
}

#[test]
fn owned_control_stop_interrupts_partial_handshake() {
    let (mut guest, mut host) = UnixStream::pair().unwrap();
    let mut task = ControlPlaneTask::spawn("partial-stop-test", move |stop| {
        let error = serve_connection_with_stop(
            &mut host,
            SECRET,
            POLICY,
            || 1000,
            HostSamples::default,
            Some(&stop),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("control-plane stop requested"),
            "{error}"
        );
        Ok(())
    })
    .unwrap();
    guest.write_all(b"RA").unwrap();
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(
        task.wait_for_stop(Duration::from_secs(1)).unwrap(),
        StopState::Stopped
    );
}

#[test]
fn control_task_observation_does_not_request_stop() {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let mut task = ControlPlaneTask::spawn("observe-only-test", move |stop| {
        ready_tx.send(()).unwrap();
        while !stop.is_requested() {
            stop.wait(Duration::from_millis(10));
        }
        Ok(())
    })
    .unwrap();
    ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(task.poll().unwrap(), StopState::Pending);
    assert_eq!(
        task.wait_for_stop(Duration::from_secs(1)).unwrap(),
        StopState::Stopped
    );
}

#[test]
fn control_task_observation_reports_unexpected_worker_error() {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let mut task = ControlPlaneTask::spawn("observe-error-test", move |_| {
        ready_tx.send(()).unwrap();
        Err(std::io::Error::other("authority failed"))
    })
    .unwrap();
    ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    loop {
        match task.poll() {
            Err(error) => {
                assert!(error.to_string().contains("authority failed"));
                break;
            }
            Ok(StopState::Pending) if std::time::Instant::now() < deadline => {
                std::thread::yield_now();
            }
            other => panic!("unexpected observation: {other:?}"),
        }
    }
}

#[test]
fn serves_ipc_v5_storage_requests_alongside_heartbeats_and_gpu_catalog() {
    use ramshared_ipc::gpu_catalog::{CatalogSession, CatalogTiming};
    use ramshared_ipc::storage_proof::StorageSession;
    use ramshared_ipc::storage_transport::{receive_response, send_request};
    use ramshared_ipc::{IPC_VERSION_5, MSG_GPU_CATALOG};
    use std::time::{Duration, Instant};

    let (mut guest, mut host) = UnixStream::pair().unwrap();
    let disk_id = [0x42u8; 16];
    let report = crate::host_storage::HostStorageReport {
        schema_version: 1,
        producer: crate::gpu_identity::ProducerProcess {
            pid: 10,
            creation_time_100ns: 20,
        },
        started_at_unix_ms: 100,
        elapsed_ms: 2,
        sample: crate::host_storage::HostVolumeSample {
            sampled_at_unix_ms: 101,
            volume_id: r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\".into(),
            file_identity: crate::host_storage::HostFileIdentity {
                volume_serial: 1,
                file_index: 42,
            },
            total_bytes: 100 << 30,
            free_bytes: 60 << 30,
            available_bytes: 50 << 30,
        },
        virtual_disk_id: Some(disk_id),
    };
    let server = std::thread::spawn(move || {
        serve_connection(
            &mut host,
            SECRET,
            POLICY,
            || 0,
            move || {
                let report = report.clone();
                HostSamples {
                    storage: Some(Arc::new(move |requested| {
                        (requested == disk_id).then(|| StorageSample {
                            report: report.clone(),
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

    let guest_nonce = random_nonce().unwrap();
    let transcript = HandshakeTranscript {
        min_version: IPC_MIN_VERSION,
        max_version: IPC_VERSION_5,
        boot_id: "11111111-2222-3333-4444-555555555555".into(),
        distro_id: "ubuntu".into(),
        guest_nonce,
    };
    let handshake = Handshake {
        min_version: transcript.min_version,
        max_version: transcript.max_version,
        boot_id: transcript.boot_id.clone(),
        distro_id: transcript.distro_id.clone(),
        guest_nonce,
        guest_proof: guest_proof(SECRET, &transcript).unwrap(),
    };
    write_frame(
        &mut guest,
        MSG_HANDSHAKE,
        &encode_control(&handshake).unwrap(),
        1,
    )
    .unwrap();
    let (msg_type, payload, _) = read_frame(&mut guest, MAX_CONTROL_PAYLOAD).unwrap();
    assert_eq!(msg_type, MSG_HANDSHAKE_ACK);
    let ack: HandshakeAck = decode_control(&payload).unwrap();
    assert_eq!(ack.accepted_version, IPC_VERSION_5);
    let challenge = HostChallenge {
        accepted_version: ack.accepted_version,
        heartbeat_secs: ack.heartbeat_secs,
        lease_timeout_secs: ack.lease_timeout_secs,
        host_nonce: ack.host_nonce,
    };
    assert!(verify_host_proof(
        SECRET,
        &transcript,
        &challenge,
        &ack.host_proof
    ));
    let finish = HandshakeFinish {
        guest_finish_proof: guest_finish_proof(SECRET, &transcript, &challenge).unwrap(),
    };
    write_frame(
        &mut guest,
        MSG_HANDSHAKE_FINISH,
        &encode_control(&finish).unwrap(),
        2,
    )
    .unwrap();
    let granted = guest_request(&mut guest, guest_nonce);

    let mut catalog =
        CatalogSession::new(SECRET, &transcript, &challenge, granted.lease_id).unwrap();
    let mut storage =
        StorageSession::new(SECRET, &transcript, &challenge, granted.lease_id).unwrap();

    // Heartbeat round + GPU catalog frame on v5
    let beat_round = catalog.round();
    let started = Instant::now();
    write_frame(
        &mut guest,
        MSG_HEARTBEAT,
        &encode_control(&Heartbeat { timestamp_ms: 1 }).unwrap(),
        beat_round,
    )
    .unwrap();
    let (ack_type, ack_payload, ack_corr) = read_frame(&mut guest, MAX_CONTROL_PAYLOAD).unwrap();
    assert_eq!(ack_type, MSG_HEARTBEAT_ACK);
    assert_eq!(ack_corr, beat_round);
    let beat_ack: HeartbeatAck = decode_control(&ack_payload).unwrap();
    let (cat_type, cat_bytes, cat_corr) =
        read_frame(&mut guest, ramshared_ipc::MAX_GPU_CATALOG_PAYLOAD).unwrap();
    assert_eq!(cat_type, MSG_GPU_CATALOG);
    catalog
        .open_verified(
            SECRET,
            &beat_ack,
            &cat_bytes,
            cat_corr,
            CatalogTiming {
                started,
                received: Instant::now(),
            },
        )
        .unwrap();

    // Storage request/response multiplexed on the exact same v5 stream
    let deadline = Instant::now() + Duration::from_secs(5);
    let pending = send_request(&mut guest, &mut storage, SECRET, disk_id, deadline).unwrap();
    let sample = match receive_response(&mut guest, &mut storage, SECRET, pending) {
        Ok(sample) => sample,
        Err(err) => {
            let server_res = server.join();
            panic!("receive_response failed ({err}); server={server_res:?}");
        }
    };
    let candidate = sample
        .candidate_for(disk_id, Instant::now(), |id| id == granted.lease_id)
        .unwrap();
    assert_eq!(candidate.total_bytes, 100 << 30);
    assert_eq!(candidate.available_bytes, 50 << 30);

    write_frame(
        &mut guest,
        MSG_LEASE_RELEASE,
        &encode_control(&LeaseRequest { nonce: Vec::new() }).unwrap(),
        catalog.round(),
    )
    .unwrap();
    assert_eq!(server.join().unwrap().unwrap(), 1);
}

#[test]
fn transient_unavailable_storage_sample_keeps_v5_lease_alive() {
    use ramshared_ipc::IPC_VERSION_5;
    use ramshared_ipc::storage_proof::StorageSession;
    use ramshared_ipc::storage_transport::{receive_response_optional, send_request};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    let (mut host, mut guest) = std::os::unix::net::UnixStream::pair().unwrap();
    let disk_id = [0x66u8; 16];
    let report = crate::host_storage::HostStorageReport {
        schema_version: 1,
        producer: crate::gpu_identity::ProducerProcess {
            pid: 10,
            creation_time_100ns: 20,
        },
        started_at_unix_ms: 100,
        elapsed_ms: 2,
        sample: crate::host_storage::HostVolumeSample {
            sampled_at_unix_ms: 101,
            volume_id: r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\".into(),
            file_identity: crate::host_storage::HostFileIdentity {
                volume_serial: 1,
                file_index: 42,
            },
            total_bytes: 100 << 30,
            free_bytes: 60 << 30,
            available_bytes: 50 << 30,
        },
        virtual_disk_id: Some(disk_id),
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_srv = Arc::clone(&calls);
    let server = std::thread::spawn(move || {
        serve_connection(
            &mut host,
            SECRET,
            POLICY,
            || 0,
            move || {
                let report = report.clone();
                let calls = Arc::clone(&calls_srv);
                HostSamples {
                    storage: Some(Arc::new(move |requested| {
                        let attempt = calls.fetch_add(1, Ordering::Relaxed);
                        (requested == disk_id && attempt >= 1).then(|| StorageSample {
                            report: report.clone(),
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

    let guest_nonce = random_nonce().unwrap();
    let transcript = HandshakeTranscript {
        min_version: IPC_MIN_VERSION,
        max_version: IPC_VERSION_5,
        boot_id: "11111111-2222-3333-4444-555555555555".into(),
        distro_id: "ubuntu".into(),
        guest_nonce,
    };
    let handshake = Handshake {
        min_version: transcript.min_version,
        max_version: transcript.max_version,
        boot_id: transcript.boot_id.clone(),
        distro_id: transcript.distro_id.clone(),
        guest_nonce,
        guest_proof: guest_proof(SECRET, &transcript).unwrap(),
    };
    write_frame(
        &mut guest,
        MSG_HANDSHAKE,
        &encode_control(&handshake).unwrap(),
        1,
    )
    .unwrap();
    let (msg_type, payload, _) = read_frame(&mut guest, MAX_CONTROL_PAYLOAD).unwrap();
    assert_eq!(msg_type, MSG_HANDSHAKE_ACK);
    let ack: HandshakeAck = decode_control(&payload).unwrap();
    assert_eq!(ack.accepted_version, IPC_VERSION_5);
    let challenge = HostChallenge {
        accepted_version: ack.accepted_version,
        heartbeat_secs: ack.heartbeat_secs,
        lease_timeout_secs: ack.lease_timeout_secs,
        host_nonce: ack.host_nonce,
    };
    let finish = HandshakeFinish {
        guest_finish_proof: guest_finish_proof(SECRET, &transcript, &challenge).unwrap(),
    };
    write_frame(
        &mut guest,
        MSG_HANDSHAKE_FINISH,
        &encode_control(&finish).unwrap(),
        2,
    )
    .unwrap();
    let granted = guest_request(&mut guest, guest_nonce);

    let mut storage =
        StorageSession::new(SECRET, &transcript, &challenge, granted.lease_id).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);

    let pending1 = send_request(&mut guest, &mut storage, SECRET, disk_id, deadline).unwrap();
    assert!(
        receive_response_optional(&mut guest, &mut storage, SECRET, pending1)
            .unwrap()
            .is_none(),
        "transient unavailable sample must keep the live session intact"
    );

    let pending2 = send_request(&mut guest, &mut storage, SECRET, disk_id, deadline).unwrap();
    let sample2 = receive_response_optional(&mut guest, &mut storage, SECRET, pending2)
        .unwrap()
        .unwrap();
    assert_eq!(
        sample2
            .candidate_for(disk_id, Instant::now(), |id| id == granted.lease_id)
            .unwrap()
            .available_bytes,
        50 << 30
    );

    write_frame(
        &mut guest,
        MSG_LEASE_RELEASE,
        &encode_control(&LeaseRequest { nonce: Vec::new() }).unwrap(),
        4,
    )
    .unwrap();
    assert_eq!(server.join().unwrap().unwrap(), 0);
}
