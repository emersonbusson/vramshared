// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-ipc/tests/storage_transport.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use ramshared_ipc::storage_proof::{StorageCandidate, StorageSession};
use ramshared_ipc::storage_transport::{
    receive_response, respond_to_received_request, respond_to_request, send_request,
};
use ramshared_ipc::{
    HandshakeTranscript, HostChallenge, MAX_CONTROL_PAYLOAD, MSG_STORAGE_REQUEST, read_frame,
};
use std::io::Cursor;
use std::time::Instant;

#[cfg(target_os = "linux")]
#[test]
fn storage_deadline_namespace_child() {
    use ramshared_ipc::storage_proof::{BootBoundStorageDeadline, storage_boot_clock};

    let Ok(encoded) = std::env::var("RAMSHARED_TEST_DEADLINE") else {
        return;
    };
    let value: serde_json::Value = serde_json::from_str(&encoded).unwrap();
    let started = value["started_ns"].as_u64().unwrap();
    let deadline: BootBoundStorageDeadline = serde_json::from_value(value).unwrap();
    let (boot, _) = storage_boot_clock().unwrap();
    let expected = std::env::var("RAMSHARED_TEST_NAMESPACE_MATCH").unwrap() == "yes";
    assert_eq!(deadline.is_current(&boot, started), expected);
}

#[cfg(target_os = "linux")]
#[test]
fn storage_deadline_crosses_only_matching_time_namespaces() {
    use ramshared_ipc::storage_proof::{BootBoundStorageDeadline, storage_boot_clock};
    use std::process::Command;

    let (boot, started) = storage_boot_clock().unwrap();
    let deadline = BootBoundStorageDeadline::new(&boot, started, 0, 5_000).unwrap();
    let encoded = serde_json::to_string(&deadline).unwrap();
    let executable = std::env::current_exe().unwrap();
    let run = |command: &mut Command, expected: &str| {
        command
            .arg(&executable)
            .args(["--exact", "storage_deadline_namespace_child"])
            .env("RAMSHARED_TEST_DEADLINE", &encoded)
            .env("RAMSHARED_TEST_NAMESPACE_MATCH", expected)
            .output()
            .unwrap()
    };
    let mut same = Command::new("env");
    let output = run(&mut same, "yes");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));

    // Some CI environments deny unprivileged namespaces. Exercise the real
    // boundary when available and report an explicit skip otherwise.
    let available = Command::new("unshare")
        .args(["--user", "--map-root-user", "--time", "--fork", "true"])
        .output();
    if !available.is_ok_and(|output| output.status.success()) {
        eprintln!("time namespace isolation unavailable; foreign-namespace case skipped");
        return;
    }
    let mut foreign = Command::new("unshare");
    foreign.args(["--user", "--map-root-user", "--time", "--fork"]);
    let output = run(&mut foreign, "no");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
}

struct FixtureStream {
    input: Cursor<Vec<u8>>,
    output: Vec<u8>,
}
impl FixtureStream {
    fn new(input: Vec<u8>) -> Self {
        Self {
            input: Cursor::new(input),
            output: Vec::new(),
        }
    }
}
impl std::io::Read for FixtureStream {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        std::io::Read::read(&mut self.input, buffer)
    }
}
impl std::io::Write for FixtureStream {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        std::io::Write::write(&mut self.output, bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn session() -> StorageSession {
    let transcript = HandshakeTranscript {
        min_version: 2,
        max_version: 5,
        boot_id: "boot".into(),
        distro_id: "guest".into(),
        guest_nonce: [1; 32],
    };
    let challenge = HostChallenge {
        accepted_version: 5,
        heartbeat_secs: 5,
        lease_timeout_secs: 20,
        host_nonce: [2; 32],
    };
    StorageSession::new(b"secret", &transcript, &challenge, 7).unwrap()
}

fn lease() -> Instant {
    Instant::now() + std::time::Duration::from_secs(1)
}

fn candidate() -> StorageCandidate {
    StorageCandidate {
        disk_id: [3; 16],
        volume_id: "volume".into(),
        file_serial: 1,
        file_index: 2,
        total_bytes: 100,
        available_bytes: 50,
        age_ms: 10,
    }
}

#[test]
fn authenticated_storage_frames_round_trip_and_expire_with_the_lease() {
    let mut guest = session();
    let mut host = session();
    let mut request = Vec::new();
    let pending = send_request(&mut request, &mut guest, b"secret", [3; 16], lease()).unwrap();
    let mut stream = FixtureStream::new(request);
    respond_to_request(&mut stream, &mut host, b"secret", lease(), |id| {
        assert_eq!(id.disk_id(), [3; 16]);
        Ok((true, vec![candidate()]))
    })
    .unwrap();
    let sample = receive_response(
        &mut Cursor::new(stream.output),
        &mut guest,
        b"secret",
        pending,
    )
    .unwrap();
    #[cfg(target_os = "linux")]
    {
        let deadline = sample
            .current_boot_deadline(Instant::now(), |id| id == 7)
            .expect("received sample retains its boot-bound deadline");
        let (boot, boot_ns) = ramshared_ipc::storage_proof::storage_boot_clock().unwrap();
        assert!(deadline.is_current(&boot, boot_ns));
        assert!(!deadline.is_current("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", boot_ns));
        assert!(
            sample
                .current_boot_deadline(Instant::now(), |_| false)
                .is_none()
        );
    }
    assert_eq!(
        sample
            .candidate_for([3; 16], Instant::now(), |id| id == 7)
            .unwrap()
            .available_bytes,
        50
    );
    assert!(
        sample
            .candidate_for(
                [3; 16],
                Instant::now() + std::time::Duration::from_millis(1000),
                |_| true
            )
            .is_none()
    );
}

#[test]
fn wrong_frame_version_refuses_before_sampling_and_retires_the_session() {
    let mut guest = session();
    let mut host = session();
    let mut request = Vec::new();
    send_request(&mut request, &mut guest, b"secret", [3; 16], lease()).unwrap();
    request[4..8].copy_from_slice(&4u32.to_le_bytes());
    let mut sampled = false;
    assert!(
        respond_to_request(
            &mut FixtureStream::new(request.clone()),
            &mut host,
            b"secret",
            lease(),
            |_| {
                sampled = true;
                Ok((true, vec![candidate()]))
            }
        )
        .is_err()
    );
    assert!(!sampled);
    request[4..8].copy_from_slice(&5u32.to_le_bytes());
    assert!(
        respond_to_request(
            &mut FixtureStream::new(request),
            &mut host,
            b"secret",
            lease(),
            |_| Ok((true, vec![candidate()]))
        )
        .is_err()
    );
}

#[test]
fn malformed_headers_and_truncated_bodies_never_trigger_observation() {
    let mut guest = session();
    let mut request = Vec::new();
    send_request(&mut request, &mut guest, b"secret", [3; 16], lease()).unwrap();
    for variant in 0..5 {
        let mut frame = request.clone();
        match variant {
            0 => frame[0] ^= 1,
            1 => frame[8..12].copy_from_slice(&u32::MAX.to_le_bytes()),
            2 => frame[12..16].copy_from_slice(&0x118u32.to_le_bytes()),
            3 => frame[16..24].copy_from_slice(&2u64.to_le_bytes()),
            _ => {
                frame.pop();
            }
        }
        let mut host = session();
        let mut called = false;
        assert!(
            respond_to_request(
                &mut FixtureStream::new(frame),
                &mut host,
                b"secret",
                lease(),
                |_| {
                    called = true;
                    Ok((true, vec![candidate()]))
                }
            )
            .is_err()
        );
        assert!(!called);
    }
}

#[test]
fn altered_response_and_wrong_session_retire_the_recipient() {
    let mut guest = session();
    let mut host = session();
    let mut request = Vec::new();
    let pending = send_request(&mut request, &mut guest, b"secret", [3; 16], lease()).unwrap();
    let mut stream = FixtureStream::new(request);
    respond_to_request(&mut stream, &mut host, b"secret", lease(), |_| {
        Ok((true, vec![candidate()]))
    })
    .unwrap();
    *stream.output.last_mut().unwrap() ^= 1;
    assert!(
        receive_response(
            &mut Cursor::new(stream.output),
            &mut guest,
            b"secret",
            pending
        )
        .is_err()
    );
    assert!(send_request(&mut Vec::new(), &mut guest, b"secret", [3; 16], lease()).is_err());

    let mut a = session();
    let mut b = session();
    let pending_a = send_request(&mut Vec::new(), &mut a, b"secret", [3; 16], lease()).unwrap();
    send_request(&mut Vec::new(), &mut b, b"secret", [3; 16], lease()).unwrap();
    assert!(receive_response(&mut Cursor::new(Vec::new()), &mut b, b"secret", pending_a).is_err());
    assert!(send_request(&mut Vec::new(), &mut b, b"secret", [3; 16], lease()).is_err());
}

#[test]
fn write_failure_and_expired_lease_retire_the_session() {
    struct Broken;
    impl std::io::Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut guest = session();
    assert!(send_request(&mut Broken, &mut guest, b"secret", [3; 16], lease()).is_err());
    assert!(send_request(&mut Vec::new(), &mut guest, b"secret", [3; 16], lease()).is_err());
    let mut expired = session();
    assert!(
        send_request(
            &mut Vec::new(),
            &mut expired,
            b"secret",
            [3; 16],
            Instant::now()
        )
        .is_err()
    );
}

#[test]
fn response_arriving_after_absolute_lease_deadline_is_refused() {
    struct Slow(Cursor<Vec<u8>>);
    impl std::io::Read for Slow {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            std::thread::sleep(std::time::Duration::from_millis(150));
            std::io::Read::read(&mut self.0, out)
        }
    }
    let mut guest = session();
    let mut host = session();
    let mut request = Vec::new();
    let deadline = Instant::now() + std::time::Duration::from_millis(100);
    let pending = send_request(&mut request, &mut guest, b"secret", [3; 16], deadline).unwrap();
    let mut stream = FixtureStream::new(request);
    respond_to_request(&mut stream, &mut host, b"secret", lease(), |_| {
        Ok((true, vec![candidate()]))
    })
    .unwrap();
    assert!(
        receive_response(
            &mut Slow(Cursor::new(stream.output)),
            &mut guest,
            b"secret",
            pending
        )
        .is_err()
    );
    assert!(send_request(&mut Vec::new(), &mut guest, b"secret", [3; 16], lease()).is_err());
}

#[test]
fn host_never_sends_observation_after_lease_expiry() {
    let mut guest = session();
    let mut host = session();
    let mut request = Vec::new();
    send_request(&mut request, &mut guest, b"secret", [3; 16], lease()).unwrap();
    let mut stream = FixtureStream::new(request);
    let deadline = Instant::now() + std::time::Duration::from_millis(100);
    assert!(
        respond_to_request(&mut stream, &mut host, b"secret", deadline, |_| {
            std::thread::sleep(std::time::Duration::from_millis(150));
            Ok((true, vec![candidate()]))
        })
        .is_err()
    );
    assert!(stream.output.is_empty());
}

#[cfg(unix)]
#[test]
fn real_socket_exchange_authenticates_the_requested_disk() {
    use std::os::unix::net::UnixStream;
    use std::time::Duration;
    let (mut guest_io, mut host_io) = UnixStream::pair().unwrap();
    for stream in [&guest_io, &host_io] {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
    }
    let thread = std::thread::spawn(move || {
        let mut host = session();
        respond_to_request(&mut host_io, &mut host, b"secret", lease(), |_| {
            Ok((true, vec![candidate()]))
        })
        .unwrap();
    });
    let mut guest = session();
    let pending = send_request(&mut guest_io, &mut guest, b"secret", [3; 16], lease()).unwrap();
    let result = receive_response(&mut guest_io, &mut guest, b"secret", pending).unwrap();
    assert!(
        result
            .candidate_for([3; 16], Instant::now(), |id| id == 7)
            .is_some()
    );
    thread.join().unwrap();
}

#[test]
fn respond_to_received_request_multiplexes_with_read_frame() {
    let mut guest = session();
    let mut host = session();
    let mut request = Vec::new();
    let pending = send_request(&mut request, &mut guest, b"secret", [3; 16], lease()).unwrap();
    let mut cursor = Cursor::new(request);
    let (msg_type, payload, correlation) = read_frame(&mut cursor, MAX_CONTROL_PAYLOAD).unwrap();
    assert_eq!(msg_type, MSG_STORAGE_REQUEST);
    assert!(correlation > 0);
    let mut reply = Vec::new();
    respond_to_received_request(
        &mut reply,
        &mut host,
        b"secret",
        correlation,
        &payload,
        lease(),
        |id| {
            assert_eq!(id.disk_id(), [3; 16]);
            Ok((true, vec![candidate()]))
        },
    )
    .unwrap();
    let sample = receive_response(&mut Cursor::new(reply), &mut guest, b"secret", pending).unwrap();
    assert!(
        sample
            .candidate_for([3; 16], Instant::now(), |id| id == 7)
            .is_some()
    );
}
