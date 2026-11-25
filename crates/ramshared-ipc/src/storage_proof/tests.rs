// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-ipc/src/storage_proof/tests.rs
use super::*;

fn session() -> StorageSession {
    let transcript = HandshakeTranscript {
        min_version: 2,
        max_version: 5,
        boot_id: "boot".into(),
        distro_id: "linux".into(),
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
fn candidate() -> StorageCandidate {
    StorageCandidate {
        disk_id: [3; 16],
        volume_id: "volume".into(),
        file_serial: 1,
        file_index: 2,
        total_bytes: 100,
        available_bytes: 50,
        age_ms: 100,
    }
}

#[test]
#[cfg(target_os = "linux")]
fn boot_deadline_rejects_a_different_time_namespace() {
    let boot = "11111111-2222-3333-4444-555555555555";
    let deadline = BootBoundStorageDeadline::new(boot, 1_000_000, 0, 5_000).unwrap();
    let mut serialized = serde_json::to_value(&deadline).unwrap();
    serialized["time_namespace_inode"] = serde_json::json!(0);
    let foreign: BootBoundStorageDeadline = serde_json::from_value(serialized).unwrap();
    assert!(!foreign.is_current(boot, 1_000_000));
}

#[test]
fn boot_bound_storage_deadline_never_extends_request_authority() {
    let boot = "11111111-2222-3333-4444-555555555555";
    let start = 1_000_000_u64;
    let deadline = BootBoundStorageDeadline::new(boot, start, 100, 5_000).unwrap();
    assert!(deadline.is_current(boot, start));
    assert_eq!(
        deadline.elapsed(boot, start + 2_000_000_000),
        Some(Duration::from_secs(2))
    );
    assert!(deadline.is_current(boot, start + 4_900_000_000 - 1));
    assert!(!deadline.is_current(boot, start + 4_900_000_000));
    assert!(deadline.elapsed(boot, start + 4_900_000_000).is_none());
    assert!(!deadline.is_current(boot, start - 1));
    assert!(!deadline.is_current("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", start));
    assert!(BootBoundStorageDeadline::new("invalid", start, 100, 5_000).is_none());
    assert!(BootBoundStorageDeadline::new(boot, start, MAX_STORAGE_AGE_MS, 5_000).is_none());
    assert!(BootBoundStorageDeadline::new(boot, start, 0, 0).is_none());
    assert!(BootBoundStorageDeadline::new(boot, u64::MAX, 0, 1).is_none());
    let short_lease = BootBoundStorageDeadline::new(boot, start, 0, 7).unwrap();
    assert!(!short_lease.is_current(boot, start + 7_000_000));
    #[cfg(target_os = "linux")]
    {
        let restricted = deadline.with_sample_age(4_900).unwrap();
        assert!(restricted.is_current(boot, start + 99_999_999));
        assert!(!restricted.is_current(boot, start + 100_000_000));
        assert!(deadline.with_sample_age(MAX_STORAGE_AGE_MS).is_none());
    }
    let mut serialized = serde_json::to_value(&deadline).unwrap();
    serialized["expires_ns"] = serde_json::json!(u64::MAX);
    let malformed: BootBoundStorageDeadline = serde_json::from_value(serialized).unwrap();
    assert!(!malformed.is_current(boot, start));
}

#[test]
fn storage_telemetry_rejects_legacy_record_without_boot_deadline() {
    let legacy = serde_json::json!({
        "disk_id": "11111111111111111111111111111111",
        "volume_id": "volume",
        "total_bytes": 100,
        "available_bytes": 50,
        "age_ms": 10
    });
    assert!(serde_json::from_value::<HostStorageTelemetry>(legacy).is_err());
}

#[test]
fn storage_remaining_lifetime_never_outlives_lease_or_rounds_up() {
    let mut host = session();
    let mut guest = session();
    let now = Instant::now();
    guest.request(b"secret", [3; 16]).unwrap();
    let frame = host
        .seal(b"secret", [3; 16], true, vec![candidate()])
        .unwrap();
    let proof = guest
        .open(
            b"secret",
            [3; 16],
            &frame,
            now,
            now + Duration::from_millis(20),
            1_000,
        )
        .unwrap();
    assert_eq!(
        proof.remaining_lifetime_ms(now + Duration::from_millis(21), |id| id == 7),
        Some(979)
    );
    assert_eq!(
        proof.remaining_lifetime_ms(now + Duration::from_micros(999_500), |_| true),
        None
    );
    assert_eq!(
        proof.remaining_lifetime_ms(now + Duration::from_millis(1_000), |_| true),
        None
    );
    assert_eq!(
        proof.remaining_lifetime_ms(now + Duration::from_millis(21), |_| false),
        None
    );
    assert_eq!(proof.remaining_lifetime_ms(now, |_| true), None);
    drop(guest);
    assert_eq!(
        proof.remaining_lifetime_ms(now + Duration::from_millis(21), |_| true),
        None
    );
}

#[test]
fn response_requires_the_exact_outstanding_request() {
    let now = Instant::now();
    let mut host = session();
    let frame = host
        .seal(b"secret", [3; 16], true, vec![candidate()])
        .unwrap();

    let mut unsolicited = session();
    assert!(
        unsolicited
            .open(b"secret", [3; 16], &frame, now, now, 1000)
            .is_err()
    );

    let mut wrong_disk = session();
    wrong_disk.request(b"secret", [4; 16]).unwrap();
    assert!(
        wrong_disk
            .open(b"secret", [3; 16], &frame, now, now, 1000)
            .is_err()
    );

    let mut duplicate = session();
    duplicate.request(b"secret", [3; 16]).unwrap();
    assert!(duplicate.request(b"secret", [4; 16]).is_err());
    assert!(
        duplicate
            .open(b"secret", [3; 16], &frame, now, now, 1000)
            .is_ok()
    );
}
#[test]
fn storage_proof_binds_session_round_root_and_expiry() {
    let mut host = session();
    let mut guest = session();
    guest.request(b"secret", [3; 16]).unwrap();
    let now = Instant::now();
    let bytes = host
        .seal(b"secret", [3; 16], true, vec![candidate()])
        .unwrap();
    let verified = guest
        .open(
            b"secret",
            [3; 16],
            &bytes,
            now,
            now + Duration::from_millis(20),
            1000,
        )
        .unwrap();
    assert!(
        verified
            .candidates(now + Duration::from_millis(21), |id| id == 7)
            .is_some()
    );
    assert!(
        verified
            .candidates(now + Duration::from_millis(1020), |_| true)
            .is_none()
    );
    assert!(
        verified
            .candidates(now + Duration::from_millis(22), |_| false)
            .is_none()
    );
    assert!(
        verified
            .candidates(now + Duration::from_millis(22), |id| id == 8)
            .is_none()
    );
    assert!(
        guest
            .open(b"secret", [3; 16], &bytes, now, now, 1000)
            .is_err()
    );
    let mut other = session();
    assert!(
        other
            .open(b"secret", [4; 16], &bytes, now, now, 1000)
            .is_err()
    );
    let mut tampered: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    tampered["body"]["candidates"][0]["available_bytes"] = 49.into();
    let mut verifier = session();
    verifier.request(b"secret", [3; 16]).unwrap();
    assert!(
        verifier
            .open(
                b"secret",
                [3; 16],
                &serde_json::to_vec(&tampered).unwrap(),
                now,
                now,
                1000
            )
            .is_err()
    );
    let mut verifier = session();
    verifier.request(b"secret", [3; 16]).unwrap();
    assert!(
        verifier
            .open(
                b"secret",
                [3; 16],
                &bytes,
                now,
                now + Duration::from_millis(1000),
                1000
            )
            .is_err()
    );
    let mut verifier = session();
    verifier.request(b"secret", [3; 16]).unwrap();
    assert!(
        verifier
            .open(
                b"secret",
                [3; 16],
                &bytes,
                now + Duration::from_millis(1),
                now,
                1000
            )
            .is_err()
    );
    let mut wrong_key = session();
    wrong_key.request(b"secret", [3; 16]).unwrap();
    assert!(
        wrong_key
            .open(b"wrong", [3; 16], &bytes, now, now, 1000)
            .is_err()
    );
}
#[test]
fn storage_proof_refuses_unbounded_missing_or_ambiguous_candidates() {
    let mut s = session();
    assert!(
        s.seal(b"secret", [3; 16], false, vec![candidate()])
            .is_err()
    );
    assert!(
        s.seal(b"secret", [3; 16], true, vec![candidate(), candidate()])
            .is_err()
    );
    let mut stale = candidate();
    stale.age_ms = 5000;
    assert!(s.seal(b"secret", [3; 16], true, vec![stale]).is_err());
    let mut bad = candidate();
    bad.available_bytes = 101;
    assert!(s.seal(b"secret", [3; 16], true, vec![bad]).is_err());
}

#[test]
fn invalid_storage_frame_poisons_its_session() {
    let mut host = session();
    let mut guest = session();
    guest.request(b"secret", [3; 16]).unwrap();
    let now = Instant::now();
    let valid = host
        .seal(b"secret", [3; 16], true, vec![candidate()])
        .unwrap();
    let mut altered: serde_json::Value = serde_json::from_slice(&valid).unwrap();
    altered["body"]["candidates"][0]["available_bytes"] = 49.into();
    assert!(
        guest
            .open(
                b"secret",
                [3; 16],
                &serde_json::to_vec(&altered).unwrap(),
                now,
                now,
                1000
            )
            .is_err()
    );
    assert!(
        guest
            .open(b"secret", [3; 16], &valid, now, now, 1000)
            .is_err()
    );
}

#[test]
fn later_invalid_frame_or_session_drop_revokes_existing_observation() {
    let now = Instant::now();
    let mut host = session();
    let mut guest = session();
    guest.request(b"secret", [3; 16]).unwrap();
    let first = host
        .seal(b"secret", [3; 16], true, vec![candidate()])
        .unwrap();
    let verified = guest
        .open(b"secret", [3; 16], &first, now, now, 1000)
        .unwrap();
    assert!(verified.candidates(now, |_| true).is_some());
    assert!(
        guest
            .open(b"secret", [3; 16], &first, now, now, 1000)
            .is_err()
    );
    assert!(verified.candidates(now, |_| true).is_none());
    let mut fresh = session();
    fresh.request(b"secret", [3; 16]).unwrap();
    let verified = fresh
        .open(b"secret", [3; 16], &first, now, now, 1000)
        .unwrap();
    drop(fresh);
    assert!(verified.candidates(now, |_| true).is_none());
}

#[test]
fn later_valid_observation_invalidates_prior_capacity() {
    let now = Instant::now();
    let mut host = session();
    let mut guest = session();
    guest.request(b"secret", [3; 16]).unwrap();
    let first = host
        .seal(b"secret", [3; 16], true, vec![candidate()])
        .unwrap();
    let old = guest
        .open(b"secret", [3; 16], &first, now, now, 1000)
        .unwrap();
    assert!(old.candidates(now, |_| true).is_some());
    let mut newer = candidate();
    newer.available_bytes = 5;
    let second = host.seal(b"secret", [3; 16], true, vec![newer]).unwrap();
    guest.request(b"secret", [3; 16]).unwrap();
    let fresh = guest
        .open(b"secret", [3; 16], &second, now, now, 1000)
        .unwrap();
    assert!(old.candidates(now, |_| true).is_none());
    assert_eq!(
        fresh.candidates(now, |_| true).unwrap()[0].available_bytes,
        5
    );
}

#[test]
fn observation_only_selects_the_disk_bound_to_its_request() {
    let now = Instant::now();
    let mut host = session();
    let mut guest = session();
    guest.request(b"secret", [3; 16]).unwrap();
    let mut other = candidate();
    other.disk_id = [4; 16];
    other.file_index = 3;
    let bytes = host
        .seal(b"secret", [3; 16], true, vec![candidate(), other])
        .unwrap();
    let observation = guest
        .open(b"secret", [3; 16], &bytes, now, now, 1000)
        .unwrap();
    assert_eq!(
        observation
            .candidate_for([3; 16], now, |_| true)
            .unwrap()
            .disk_id,
        [3; 16]
    );
    assert!(observation.candidate_for([4; 16], now, |_| true).is_none());
    assert!(observation.candidate_for([3; 16], now, |_| false).is_none());
    assert!(
        observation
            .candidate_for([3; 16], now + Duration::from_millis(1000), |_| true)
            .is_none()
    );
    drop(guest);
    assert!(observation.candidate_for([3; 16], now, |_| true).is_none());
}

#[test]
fn authenticated_request_binds_disk_round_and_session() {
    let mut guest = session();
    let mut host = session();
    let request = guest.request(b"secret", [3; 16]).unwrap();
    let verified = host.verify_request(b"secret", &request).unwrap();
    assert_eq!(verified.disk_id(), [3; 16]);
    let frame = host
        .seal_requested(b"secret", verified, true, vec![candidate()])
        .unwrap();
    let now = Instant::now();
    assert!(
        guest
            .open(b"secret", [3; 16], &frame, now, now, 1000)
            .is_ok()
    );
    assert!(host.verify_request(b"secret", &request).is_err());

    let mut forged = session();
    let mut changed: serde_json::Value = serde_json::from_slice(&request).unwrap();
    changed["body"]["disk_id"][0] = 4.into();
    assert!(
        forged
            .verify_request(b"secret", &serde_json::to_vec(&changed).unwrap())
            .is_err()
    );
    let mut wrong_key = session();
    assert!(wrong_key.verify_request(b"wrong", &request).is_err());
    assert!(session().request(b"secret", [0; 16]).is_err());

    let transcript = HandshakeTranscript {
        min_version: 2,
        max_version: 5,
        boot_id: "boot".into(),
        distro_id: "linux".into(),
        guest_nonce: [1; 32],
    };
    let challenge = HostChallenge {
        accepted_version: 5,
        heartbeat_secs: 5,
        lease_timeout_secs: 20,
        host_nonce: [9; 32],
    };
    let mut verifier = session();
    let capability = verifier.verify_request(b"secret", &request).unwrap();
    let mut other = StorageSession::new(b"secret", &transcript, &challenge, 7).unwrap();
    assert!(
        other
            .seal_requested(b"secret", capability, true, vec![candidate()])
            .is_err()
    );
    assert!(other.verify_request(b"secret", &request).is_err());
}

#[test]
fn duplicate_request_before_response_retires_the_host_session() {
    let mut guest = session();
    let mut host = session();
    let request = guest.request(b"secret", [3; 16]).unwrap();
    let first = host.verify_request(b"secret", &request).unwrap();
    assert!(host.verify_request(b"secret", &request).is_err());
    assert!(
        host.seal_requested(b"secret", first, true, vec![candidate()])
            .is_err()
    );
}

#[test]
fn verified_request_cannot_replace_another_pending_disk() {
    let mut guest_a = session();
    let mut guest_b = session();
    let mut host_a = session();
    let mut host_b = session();
    let request_a = guest_a.request(b"secret", [3; 16]).unwrap();
    let request_b = guest_b.request(b"secret", [4; 16]).unwrap();
    let verified_a = host_a.verify_request(b"secret", &request_a).unwrap();
    let verified_b = host_b.verify_request(b"secret", &request_b).unwrap();
    assert!(
        host_b
            .seal_requested(b"secret", verified_a, true, vec![candidate()])
            .is_err()
    );
    assert!(
        host_b
            .seal_requested(b"secret", verified_b, true, vec![candidate()])
            .is_err()
    );
}

#[test]
fn verified_request_cannot_move_between_identical_sessions() {
    let mut guest_a = session();
    let mut guest_b = session();
    let mut host_a = session();
    let mut host_b = session();
    let request_a = guest_a.request(b"secret", [3; 16]).unwrap();
    let request_b = guest_b.request(b"secret", [3; 16]).unwrap();
    let verified_a = host_a.verify_request(b"secret", &request_a).unwrap();
    let verified_b = host_b.verify_request(b"secret", &request_b).unwrap();
    assert!(
        host_b
            .seal_requested(b"secret", verified_a, true, vec![candidate()])
            .is_err()
    );
    assert!(
        host_b
            .seal_requested(b"secret", verified_b, true, vec![candidate()])
            .is_err()
    );
}

#[test]
fn host_accepts_next_round_only_after_answering_the_prior_request() {
    let mut guest = session();
    let mut host = session();
    let now = Instant::now();
    for _ in 0..2 {
        let request = guest.request(b"secret", [3; 16]).unwrap();
        let verified = host.verify_request(b"secret", &request).unwrap();
        let frame = host
            .seal_requested(b"secret", verified, true, vec![candidate()])
            .unwrap();
        assert!(
            guest
                .open(b"secret", [3; 16], &frame, now, now, 1000)
                .is_ok()
        );
    }
}

#[test]
fn rejected_response_retires_host_session() {
    let mut guest = session();
    let mut host = session();
    let request = guest.request(b"secret", [3; 16]).unwrap();
    let verified = host.verify_request(b"secret", &request).unwrap();
    assert!(
        host.seal_requested(b"secret", verified, false, vec![candidate()])
            .is_err()
    );
    assert!(host.verify_request(b"secret", &request).is_err());
}

#[test]
fn unavailable_response_keeps_session_alive_and_revokes_prior_sample() {
    let now = Instant::now();
    let mut guest = session();
    let mut host = session();

    guest.request(b"secret", [3; 16]).unwrap();
    let frame1 = host
        .seal(b"secret", [3; 16], true, vec![candidate()])
        .unwrap();
    let prior = guest
        .open_optional(b"secret", [3; 16], &frame1, now, now, 1_000)
        .unwrap()
        .unwrap();
    assert!(prior.candidates(now, |id| id == 7).is_some());

    let req2 = guest.request(b"secret", [3; 16]).unwrap();
    let verified2 = host.verify_request(b"secret", &req2).unwrap();
    let frame2 = host.seal_unavailable(b"secret", verified2).unwrap();
    assert!(
        guest
            .open_optional(b"secret", [3; 16], &frame2, now, now, 1_000)
            .unwrap()
            .is_none()
    );
    assert!(
        prior.candidates(now, |id| id == 7).is_none(),
        "an unavailable response must supersede prior observations"
    );

    let req3 = guest.request(b"secret", [3; 16]).unwrap();
    let verified3 = host.verify_request(b"secret", &req3).unwrap();
    let frame3 = host
        .seal_requested(b"secret", verified3, true, vec![candidate()])
        .unwrap();
    let recovered = guest
        .open_optional(b"secret", [3; 16], &frame3, now, now, 1_000)
        .unwrap()
        .unwrap();
    assert_eq!(
        recovered
            .candidate_for([3; 16], now, |id| id == 7)
            .unwrap()
            .available_bytes,
        50
    );

    let mut strict_guest = session();
    let mut strict_host = session();
    let req4 = strict_guest.request(b"secret", [3; 16]).unwrap();
    let verified4 = strict_host.verify_request(b"secret", &req4).unwrap();
    let frame4 = strict_host.seal_unavailable(b"secret", verified4).unwrap();
    assert!(
        strict_guest
            .open(b"secret", [3; 16], &frame4, now, now, 1_000)
            .is_err()
    );
    assert!(strict_guest.request(b"secret", [3; 16]).is_err());
}
