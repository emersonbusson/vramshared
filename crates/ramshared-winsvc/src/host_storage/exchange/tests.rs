// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/host_storage/exchange/tests.rs
use crate::host_storage::{HostFileIdentity, HostStorageReport, validate_sample};

#[test]
fn supervised_exchange_preserves_age_and_refuses_wrong_disk() {
    use crate::host_storage::exchange::{SupervisedSample, respond_with_supervised_sample};
    use ramshared_ipc::storage_proof::StorageSession;
    use ramshared_ipc::storage_transport::{receive_response, send_request};
    use ramshared_ipc::{HandshakeTranscript, HostChallenge};
    use std::io::{Cursor, Read, Write};
    use std::time::{Duration, Instant};

    struct Stream {
        input: Cursor<Vec<u8>>,
        output: Vec<u8>,
    }
    impl Read for Stream {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            self.input.read(bytes)
        }
    }
    impl Write for Stream {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.output.write(bytes)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let transcript = HandshakeTranscript {
        min_version: 2,
        max_version: 5,
        boot_id: "boot".into(),
        distro_id: "guest".into(),
        guest_nonce: [1; 32],
    };
    let challenge = HostChallenge {
        accepted_version: 5,
        heartbeat_secs: 1,
        lease_timeout_secs: 20,
        host_nonce: [2; 32],
    };
    let make_session = || StorageSession::new(b"secret", &transcript, &challenge, 7).unwrap();
    let report = HostStorageReport {
        schema_version: 1,
        producer: crate::gpu_identity::ProducerProcess {
            pid: 10,
            creation_time_100ns: 20,
        },
        started_at_unix_ms: 100,
        elapsed_ms: 2,
        sample: validate_sample(
            101,
            r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\",
            HostFileIdentity {
                volume_serial: 1,
                file_index: 42,
            },
            100,
            50,
            40,
        )
        .unwrap(),
        virtual_disk_id: Some([7; 16]),
    };
    let mut guest = make_session();
    let mut host = make_session();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut request = Vec::new();
    let pending = send_request(&mut request, &mut guest, b"secret", [7; 16], deadline).unwrap();
    let mut stream = Stream {
        input: Cursor::new(request),
        output: Vec::new(),
    };
    let captured = Instant::now() - Duration::from_millis(25);
    respond_with_supervised_sample(
        &mut stream,
        &mut host,
        b"secret",
        deadline,
        SupervisedSample {
            report: &report,
            age_ms: 125,
            captured_since: captured,
            complete: true,
        },
    )
    .unwrap();
    let sample = receive_response(
        &mut Cursor::new(stream.output),
        &mut guest,
        b"secret",
        pending,
    )
    .unwrap();
    let candidate = sample
        .candidate_for([7; 16], Instant::now(), |id| id == 7)
        .unwrap();
    assert!(candidate.age_ms >= 150);
    assert_eq!(candidate.available_bytes, 40);

    let mut guest = make_session();
    let mut host = make_session();
    let mut request = Vec::new();
    send_request(&mut request, &mut guest, b"secret", [8; 16], deadline).unwrap();
    let mut stream = Stream {
        input: Cursor::new(request),
        output: Vec::new(),
    };
    assert!(
        respond_with_supervised_sample(
            &mut stream,
            &mut host,
            b"secret",
            deadline,
            SupervisedSample {
                report: &report,
                age_ms: 125,
                captured_since: captured,
                complete: true
            }
        )
        .is_err()
    );
    assert!(stream.output.is_empty());

    for (age_ms, captured_since, complete) in [
        (5_000, Instant::now(), true),
        (u64::MAX, Instant::now() - Duration::from_millis(10), true),
        (125, Instant::now() + Duration::from_secs(30), true),
        (125, Instant::now(), false),
    ] {
        let mut guest = make_session();
        let mut host = make_session();
        let mut request = Vec::new();
        send_request(&mut request, &mut guest, b"secret", [7; 16], deadline).unwrap();
        let mut stream = Stream {
            input: Cursor::new(request),
            output: Vec::new(),
        };
        assert!(
            respond_with_supervised_sample(
                &mut stream,
                &mut host,
                b"secret",
                deadline,
                SupervisedSample {
                    report: &report,
                    age_ms,
                    captured_since,
                    complete,
                }
            )
            .is_err()
        );
        assert!(stream.output.is_empty());
    }

    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;
        let (mut guest_io, mut host_io) = UnixStream::pair().unwrap();
        for stream in [&guest_io, &host_io] {
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
        }
        let mut host = make_session();
        let mut guest = make_session();
        let thread = std::thread::spawn(move || {
            respond_with_supervised_sample(
                &mut host_io,
                &mut host,
                b"secret",
                deadline,
                SupervisedSample {
                    report: &report,
                    age_ms: 125,
                    captured_since: Instant::now(),
                    complete: true,
                },
            )
        });
        let pending =
            send_request(&mut guest_io, &mut guest, b"secret", [7; 16], deadline).unwrap();
        let sample = receive_response(&mut guest_io, &mut guest, b"secret", pending).unwrap();
        assert_eq!(
            sample
                .candidate_for([7; 16], Instant::now(), |id| id == 7)
                .unwrap()
                .available_bytes,
            40
        );
        thread.join().unwrap().unwrap();
    }
}

#[test]
fn multiplexed_unavailable_sample_answers_cleanly_without_retiring_session() {
    use crate::host_storage::exchange::respond_to_received_with_supervised_sample;
    use ramshared_ipc::storage_proof::StorageSession;
    use ramshared_ipc::storage_transport::{receive_response_optional, send_request};
    use ramshared_ipc::{HandshakeTranscript, HostChallenge, MSG_STORAGE_REQUEST, read_frame};
    use std::io::Cursor;
    use std::time::{Duration, Instant};

    let transcript = HandshakeTranscript {
        min_version: 2,
        max_version: 5,
        boot_id: "boot".into(),
        distro_id: "guest".into(),
        guest_nonce: [1; 32],
    };
    let challenge = HostChallenge {
        accepted_version: 5,
        heartbeat_secs: 1,
        lease_timeout_secs: 20,
        host_nonce: [2; 32],
    };
    let make_session = || StorageSession::new(b"secret", &transcript, &challenge, 7).unwrap();
    let report = HostStorageReport {
        schema_version: 1,
        producer: crate::gpu_identity::ProducerProcess {
            pid: 10,
            creation_time_100ns: 20,
        },
        started_at_unix_ms: 100,
        elapsed_ms: 2,
        sample: validate_sample(
            101,
            r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\",
            HostFileIdentity {
                volume_serial: 1,
                file_index: 42,
            },
            100,
            50,
            40,
        )
        .unwrap(),
        virtual_disk_id: Some([7; 16]),
    };

    let mut guest = make_session();
    let mut host = make_session();
    let deadline = Instant::now() + Duration::from_secs(10);

    let mut req1 = Vec::new();
    let pending1 = send_request(&mut req1, &mut guest, b"secret", [7; 16], deadline).unwrap();
    let (msg1, payload1, round1) = read_frame(&mut Cursor::new(req1), 1024).unwrap();
    assert_eq!(msg1, MSG_STORAGE_REQUEST);
    let mut out1 = Vec::new();
    respond_to_received_with_supervised_sample(
        &mut out1,
        &mut host,
        b"secret",
        round1,
        &payload1,
        deadline,
        |_| Err("busy"),
    )
    .unwrap();
    assert!(
        receive_response_optional(&mut Cursor::new(out1), &mut guest, b"secret", pending1)
            .unwrap()
            .is_none()
    );

    let mut req2 = Vec::new();
    let pending2 = send_request(&mut req2, &mut guest, b"secret", [7; 16], deadline).unwrap();
    let (msg2, payload2, round2) = read_frame(&mut Cursor::new(req2), 1024).unwrap();
    assert_eq!(msg2, MSG_STORAGE_REQUEST);
    let mut out2 = Vec::new();
    respond_to_received_with_supervised_sample(
        &mut out2,
        &mut host,
        b"secret",
        round2,
        &payload2,
        deadline,
        |_| Ok::<_, &str>((report.clone(), 10, Instant::now(), true)),
    )
    .unwrap();
    let recovered =
        receive_response_optional(&mut Cursor::new(out2), &mut guest, b"secret", pending2)
            .unwrap()
            .unwrap();
    assert_eq!(
        recovered
            .candidate_for([7; 16], Instant::now(), |id| id == 7)
            .unwrap()
            .available_bytes,
        40
    );
}
