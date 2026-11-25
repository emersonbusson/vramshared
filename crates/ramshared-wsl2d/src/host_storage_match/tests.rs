// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/host_storage_match/tests.rs
use super::*;
use ramshared_ipc::storage_proof::{StorageCandidate, StorageSession};
use ramshared_ipc::{HandshakeTranscript, HostChallenge};

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

#[test]
fn directory_match_requires_fresh_matching_authenticated_evidence() {
    let candidate = StorageCandidate {
        disk_id: [3; 16],
        volume_id: "volume".into(),
        file_serial: 1,
        file_index: 2,
        total_bytes: 100,
        available_bytes: 50,
        age_ms: 10,
    };
    let mut sender = session();
    let mut receiver = session();
    let now = Instant::now();
    let request = receiver.request(b"secret", [3; 16]).unwrap();
    let verified = sender.verify_request(b"secret", &request).unwrap();
    let frame = sender
        .seal_requested(b"secret", verified, true, vec![candidate])
        .unwrap();
    let sample = receiver
        .open(b"secret", [3; 16], &frame, now, now, 1000)
        .unwrap();
    let mut directory = RootVhdObservation {
        major: 8,
        minor: 1,
        identifier: [3; 16],
        observed_at_unix_ms: 10_000,
        observed_at: now,
    };
    assert_eq!(
        match_directory_observation(&directory, &sample, true, now, |id| id == 7)
            .unwrap()
            .available_bytes,
        50
    );
    assert!(match_directory_observation(&directory, &sample, false, now, |_| true).is_none());
    directory.observed_at = now - std::time::Duration::from_secs(5);
    assert!(match_directory_observation(&directory, &sample, true, now, |_| true).is_none());
    directory.observed_at = now + std::time::Duration::from_millis(1);
    assert!(match_directory_observation(&directory, &sample, true, now, |_| true).is_none());
    directory.observed_at = now;
    assert!(match_directory_observation(&directory, &sample, true, now, |_| false).is_none());
    directory.identifier = [4; 16];
    assert!(match_directory_observation(&directory, &sample, true, now, |_| true).is_none());
    directory.identifier = [3; 16];
    drop(receiver);
    assert!(match_directory_observation(&directory, &sample, true, now, |_| true).is_none());
}
