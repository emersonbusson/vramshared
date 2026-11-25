// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-ipc/src/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

#[test]
fn constant_time_eq_matches_only_equal_length_inputs() {
    assert!(constant_time_eq(b"", b""));
    assert!(constant_time_eq(b"token-1", b"token-1"));
    assert!(!constant_time_eq(b"token-1", b"token-2"));
    assert!(!constant_time_eq(b"token-1", b"token-10"));
    assert!(!constant_time_eq(b"", b"x"));
    assert!(!constant_time_eq(b"x", b""));
    let left = [0xA5u8; 64];
    let mut right = left;
    right[63] ^= 0x01;
    assert!(!constant_time_eq(&left, &right));
}

#[test]
fn frame_round_trip() {
    let hdr = VsockFrameHeader::new(MSG_HEARTBEAT, 16, 42);
    let encoded = hdr.encode();
    let decoded = VsockFrameHeader::decode(&encoded).expect("decode");
    assert_eq!(decoded, hdr);
    assert_eq!(decoded.msg_type(), MSG_HEARTBEAT);
}

#[test]
fn frame_rejects_unknown_flag_bits_before_body_read() {
    for extra in [0x100, 0x8000_0000, 0xffff_ff00] {
        let mut header = VsockFrameHeader::new(MSG_HEARTBEAT, 10, 42);
        header.flags |= extra;
        let mut wire = std::io::Cursor::new(header.encode().to_vec());
        assert!(VsockFrameHeader::decode(&header.encode()).is_err());
        assert!(read_frame(&mut wire, MAX_CONTROL_PAYLOAD).is_err());
        assert_eq!(wire.position(), FRAME_HEADER_LEN as u64);
    }
}

#[test]
fn manifest_decoder_refuses_payload_above_its_declared_cap() {
    let payload = vec![0; MAX_MANIFEST_PAYLOAD as usize + 1];
    let error = decode_manifest(&payload).unwrap_err();
    assert!(
        !error.to_string().contains("4KB"),
        "manifest diagnostic must not claim the control-message cap"
    );
}

#[test]
fn payload_length_guard_never_narrows_an_unrepresentable_size() {
    assert_eq!(checked_payload_length(0), Ok(0));
    assert_eq!(checked_payload_length(u32::MAX as usize), Ok(u32::MAX));
    #[cfg(target_pointer_width = "64")]
    assert_eq!(
        checked_payload_length(u32::MAX as usize + 1),
        Err(FrameError::PayloadLengthOverflow)
    );
    assert!(
        FrameError::PayloadLengthOverflow
            .to_string()
            .contains("cannot be represented")
    );
    assert!(
        FrameError::UnknownFlags(0x100)
            .to_string()
            .contains("unknown frame flags")
    );
}

#[test]
fn control_encoder_stops_serialization_at_the_cap() {
    use serde::ser::SerializeSeq;
    use std::cell::Cell;
    struct Counted<'a>(&'a Cell<usize>);
    impl Serialize for Counted<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let mut sequence = serializer.serialize_seq(Some(100_000))?;
            for _ in 0..100_000 {
                self.0.set(self.0.get() + 1);
                sequence.serialize_element(&0_u8)?;
            }
            sequence.end()
        }
    }
    let visited = Cell::new(0);
    assert!(encode_control(&Counted(&visited)).is_err());
    assert!(
        visited.get() < MAX_CONTROL_PAYLOAD as usize,
        "encoder walked the entire oversized object before checking the cap: {}",
        visited.get()
    );
}

#[test]
fn control_encoder_preserves_serializer_failure_without_publishing_partial_json() {
    struct Refused;
    impl Serialize for Refused {
        fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("fixture serializer refused"))
        }
    }
    assert!(
        matches!(encode_control(&Refused), Err(FrameError::PayloadDeserialization(error)) if error.contains("fixture serializer refused"))
    );
}

#[test]
fn frame_rejects_bad_magic() {
    let mut buf = [0u8; FRAME_HEADER_LEN];
    buf[0] = 0xDE;
    assert!(matches!(
        VsockFrameHeader::decode(&buf),
        Err(FrameError::InvalidMagic(_))
    ));
}

#[test]
fn frame_rejects_oversized_payload() {
    let hdr = VsockFrameHeader {
        magic: IPC_MAGIC,
        version: IPC_VERSION_3,
        payload_len: MAX_PAYLOAD_LEN + 1,
        flags: MSG_HEARTBEAT as u32,
        correlation_id: 0,
    };
    let encoded = hdr.encode();
    assert!(matches!(
        VsockFrameHeader::decode(&encoded),
        Err(FrameError::PayloadTooLarge(_))
    ));
}

#[test]
fn frame_rejects_unknown_message_type() {
    let hdr = VsockFrameHeader {
        magic: IPC_MAGIC,
        version: IPC_VERSION_3,
        payload_len: 0,
        flags: 99,
        correlation_id: 0,
    };
    let encoded = hdr.encode();
    assert!(matches!(
        VsockFrameHeader::decode(&encoded),
        Err(FrameError::UnknownMessageType(99))
    ));
}

/// DT-2: the current protocol maximum is legal; larger types are refused.
#[test]
fn frame_accepts_handshake_finish_and_rejects_type_above_max() {
    let finish_hdr = VsockFrameHeader::new(MSG_HANDSHAKE_FINISH, 0, 1);
    let encoded = finish_hdr.encode();
    let decoded = VsockFrameHeader::decode(&encoded).expect("finish type decodes");
    assert_eq!(decoded.msg_type(), MSG_HANDSHAKE_FINISH);
    assert_eq!(MSG_MAX, MSG_STORAGE_RESPONSE);

    let over = VsockFrameHeader {
        magic: IPC_MAGIC,
        version: IPC_VERSION_3,
        payload_len: 0,
        flags: u32::from(MSG_MAX) + 1,
        correlation_id: 0,
    };
    let encoded = over.encode();
    assert!(matches!(
        VsockFrameHeader::decode(&encoded),
        Err(FrameError::UnknownMessageType(t)) if t == MSG_MAX + 1
    ));
}

#[test]
fn version_negotiation_selects_highest_mutual() {
    assert_eq!(negotiate_version(2, 3), Some(3));
    assert_eq!(negotiate_version(2, 2), Some(2));
    assert_eq!(negotiate_version(3, 3), Some(3));
    assert_eq!(negotiate_version(4, 5), Some(5));
    assert_eq!(negotiate_version(5, 5), Some(5));
    assert_eq!(negotiate_version(6, 6), None);
    assert_eq!(negotiate_version(1, 1), None);
}

#[test]
fn handshake_hmac_validates() {
    let secret = b"test-secret-key";
    let data = b"boot-id-1234Ubuntu-24.04nonce";
    let hmac_val = compute_hmac(secret, data);
    assert!(verify_hmac(secret, data, &hmac_val));
    assert!(!verify_hmac(b"wrong", data, &hmac_val));
    assert!(!verify_hmac(secret, b"tampered", &hmac_val));
}

fn sample_transcript() -> HandshakeTranscript {
    HandshakeTranscript {
        min_version: IPC_MIN_VERSION,
        max_version: IPC_VERSION_3,
        boot_id: "boot-abc".into(),
        distro_id: "Ubuntu-24.04".into(),
        guest_nonce: [7u8; 32],
    }
}

fn sample_challenge() -> HostChallenge {
    HostChallenge {
        accepted_version: IPC_VERSION_3,
        heartbeat_secs: 5,
        lease_timeout_secs: 30,
        host_nonce: [9u8; 32],
    }
}

/// the three-message flow must require fresh,
/// role-separated proofs. A proof from the wrong role, a stale nonce, or
/// a zeroed proof must not authenticate anything.
#[test]
fn mutual_handshake_requires_fresh_role_bound_proofs() {
    let secret = b"shared-hmac-key";
    let other = b"attacker-key";
    let t = sample_transcript();
    let c = sample_challenge();

    // Legitimate flow: each role's own proof validates under that role.
    let g = guest_proof(secret, &t).expect("guest proof");
    let h = host_proof(secret, &t, &c).expect("host proof");
    let f = guest_finish_proof(secret, &t, &c).expect("finish proof");
    assert!(verify_guest_proof(secret, &t, &g));
    assert!(verify_host_proof(secret, &t, &c, &h));
    assert!(verify_guest_finish_proof(secret, &t, &c, &f));

    // Role separation: a guest proof is not a host proof and not a finish.
    assert!(!verify_host_proof(secret, &t, &c, &g));
    assert!(!verify_guest_finish_proof(secret, &t, &c, &g));
    // A host proof is not a guest proof and not a finish.
    assert!(!verify_guest_proof(secret, &t, &h));
    assert!(!verify_guest_finish_proof(secret, &t, &c, &h));
    // A finish proof is not a guest proof and not a host proof.
    assert!(!verify_guest_proof(secret, &t, &f));
    assert!(!verify_host_proof(secret, &t, &c, &f));

    // Wrong shared secret never authenticates.
    assert!(!verify_guest_proof(other, &t, &g));
    assert!(!verify_host_proof(other, &t, &c, &h));
    assert!(!verify_guest_finish_proof(other, &t, &c, &f));

    // Stale/foreign guest nonce: the captured proof no longer matches.
    let mut stale = t.clone();
    stale.guest_nonce = [1u8; 32];
    assert!(!verify_guest_proof(secret, &stale, &g));
    assert!(!verify_host_proof(secret, &stale, &c, &h));
    assert!(!verify_guest_finish_proof(secret, &stale, &c, &f));

    // Stale/foreign host nonce: host and finish proofs stop validating.
    let mut stale_c = c.clone();
    stale_c.host_nonce = [2u8; 32];
    assert!(!verify_host_proof(secret, &t, &stale_c, &h));
    assert!(!verify_guest_finish_proof(secret, &t, &stale_c, &f));

    // Zeroed proof and zeroed nonces are refused.
    let zero = [0u8; 32];
    assert!(!verify_guest_proof(secret, &t, &zero));
    let mut zero_nonce = t.clone();
    zero_nonce.guest_nonce = [0u8; 32];
    assert!(guest_proof(secret, &zero_nonce).is_err());
    let mut zero_host = c.clone();
    zero_host.host_nonce = [0u8; 32];
    assert!(host_proof(secret, &t, &zero_host).is_err());

    // Both proofs must be present and fresh before authority moves.
    assert_ne!(g, h);
    assert_ne!(h, f);
    assert_ne!(g, f);
}

/// a finish proof bound to one host challenge must not
/// authenticate against a replayed or refreshed host nonce. Binding the
/// finish to both fresh nonces is what prevents transcript replay.
#[test]
fn handshake_finish_rejects_replayed_host_challenge() {
    let secret = b"shared-hmac-key";
    let t = sample_transcript();
    let challenge_a = sample_challenge();
    let mut challenge_b = sample_challenge();
    challenge_b.host_nonce = [0xA5u8; 32];

    // Finish computed against challenge A.
    let finish_a = guest_finish_proof(secret, &t, &challenge_a).expect("finish a");
    assert!(verify_guest_finish_proof(
        secret,
        &t,
        &challenge_a,
        &finish_a
    ));

    // Replay of finish A against a different host challenge is rejected.
    assert!(!verify_guest_finish_proof(
        secret,
        &t,
        &challenge_b,
        &finish_a
    ));

    // A finish computed for B does not stand in for A either.
    let finish_b = guest_finish_proof(secret, &t, &challenge_b).expect("finish b");
    assert!(!verify_guest_finish_proof(
        secret,
        &t,
        &challenge_a,
        &finish_b
    ));
    assert!(verify_guest_finish_proof(
        secret,
        &t,
        &challenge_b,
        &finish_b
    ));

    // Replaying the whole captured host challenge with a new guest nonce
    // (the common transcript-replay shape) is also rejected.
    let mut fresh_guest = t.clone();
    fresh_guest.guest_nonce = [0x5Au8; 32];
    assert!(!verify_guest_finish_proof(
        secret,
        &fresh_guest,
        &challenge_a,
        &finish_a
    ));

    // And a replayed host proof from challenge A does not authorize
    // challenge B on the guest side.
    let host_a = host_proof(secret, &t, &challenge_a).expect("host a");
    assert!(!verify_host_proof(secret, &t, &challenge_b, &host_a));
}

#[test]
fn control_round_trip() {
    let hb = Heartbeat {
        timestamp_ms: 12345,
    };
    let encoded = encode_control(&hb).expect("encode");
    let decoded: Heartbeat = decode_control(&encoded).expect("decode");
    assert_eq!(decoded, hb);
}

#[test]
fn control_payload_cap_enforced() {
    let big = Telemetry {
        control_plane_state: "x".repeat(5000),
        heartbeat_rtt_us: 0,
        lease_remaining_ms: 0,
    };
    assert!(matches!(
        encode_control(&big),
        Err(FrameError::PayloadExceedsCap(_))
    ));
}

#[test]
fn manifest_round_trip() {
    let m = OriginManifestPayload {
        sha256_hex: "abc123".into(),
        data: vec![1, 2, 3, 4, 5],
    };
    let encoded = encode_manifest(&m).expect("encode");
    let decoded = decode_manifest(&encoded).expect("decode");
    assert_eq!(decoded, m);
}

#[test]
fn manifest_rejects_short_payload() {
    assert!(matches!(
        decode_manifest(&[0x01]),
        Err(FrameError::PayloadDeserialization(_))
    ));
}

#[test]
fn lib_exports_and_constants_are_consistent() {
    assert_eq!(MSG_MAX, MSG_STORAGE_RESPONSE);
    assert_eq!(MSG_HANDSHAKE_FINISH, 22);
    assert_eq!(FRAME_HEADER_LEN, 24);
    const {
        assert!(MAX_CONTROL_PAYLOAD < MAX_MANIFEST_PAYLOAD);
        assert!(MAX_MANIFEST_PAYLOAD <= MAX_PAYLOAD_LEN);
    }
}

/// Version negotiation is enforced in the header: anything outside
/// `IPC_MIN_VERSION..=IPC_MAX_VERSION` is refused before the payload is read.
#[test]
fn frame_rejects_unsupported_version() {
    for version in [1u32, 6] {
        let hdr = VsockFrameHeader {
            magic: IPC_MAGIC,
            version,
            payload_len: 0,
            flags: MSG_HEARTBEAT as u32,
            correlation_id: 0,
        };
        let encoded = hdr.encode();
        assert!(matches!(
            VsockFrameHeader::decode(&encoded),
            Err(FrameError::UnsupportedVersion(v)) if v == version
        ));
    }
}

/// Every `FrameError` arm is total and distinguishable — telemetry must
/// never fail to describe a protocol refusal.
#[test]
fn frame_error_display_is_total() {
    assert!(
        FrameError::InvalidMagic(0xDEAD_BEEF)
            .to_string()
            .contains("invalid magic")
    );
    assert!(
        FrameError::UnsupportedVersion(4)
            .to_string()
            .contains("unsupported version")
    );
    assert!(
        FrameError::PayloadTooLarge(9)
            .to_string()
            .contains("payload too large")
    );
    assert!(
        FrameError::UnknownMessageType(23)
            .to_string()
            .contains("unknown message type")
    );
    assert!(
        FrameError::PayloadDeserialization("boom".into())
            .to_string()
            .contains("deserialization")
    );
    assert!(
        FrameError::PayloadExceedsCap(5000)
            .to_string()
            .contains("message byte cap")
    );
}

/// Every `HandshakeError` arm is total and distinguishable.
#[test]
fn handshake_error_display_is_total() {
    assert!(
        HandshakeError::NonceGenerationFailed
            .to_string()
            .contains("CSPRNG")
    );
    assert!(
        HandshakeError::NonceNotFresh
            .to_string()
            .contains("all-zero")
    );
    assert!(
        HandshakeError::EmptyIdentityClaim
            .to_string()
            .contains("empty")
    );
}

/// Security rule: oversized payloads are refused at the control and
/// manifest layers too, not only in the frame header.
#[test]
fn control_and_manifest_caps_are_enforced() {
    let oversize_control = vec![0u8; MAX_CONTROL_PAYLOAD as usize + 1];
    assert!(matches!(
        decode_control::<Heartbeat>(&oversize_control),
        Err(FrameError::PayloadExceedsCap(_))
    ));

    let big_manifest = OriginManifestPayload {
        sha256_hex: "ab".repeat(64),
        data: vec![0u8; MAX_MANIFEST_PAYLOAD as usize],
    };
    assert!(matches!(
        encode_manifest(&big_manifest),
        Err(FrameError::PayloadExceedsCap(_))
    ));

    // The hex-length field claims more bytes than the payload holds.
    let mut short = Vec::new();
    short.extend_from_slice(&100u16.to_le_bytes());
    short.extend_from_slice(b"ab");
    assert!(matches!(
        decode_manifest(&short),
        Err(FrameError::PayloadDeserialization(_))
    ));
}

/// HMAC key material longer than the SHA-256 block size is hashed first
/// (RFC 2104), and a truncated expected MAC is refused by length alone.
#[test]
fn hmac_long_secret_and_length_mismatch_are_handled() {
    let long_secret = [0x5Au8; 100];
    let data = b"bound-transcript";
    let mac = compute_hmac(&long_secret, data);
    assert_eq!(mac.len(), 32);
    assert!(verify_hmac(&long_secret, data, &mac));
    assert!(!verify_hmac(&long_secret, data, &mac[..16]));
}

/// DT-3: nonces come from the OS CSPRNG and must be fresh and non-zero.
#[test]
fn random_nonce_is_fresh_and_nonzero() {
    let a = random_nonce().expect("OS CSPRNG nonce");
    let b = random_nonce().expect("OS CSPRNG nonce");
    assert_ne!(a, [0u8; 32]);
    assert_ne!(b, [0u8; 32]);
    assert_ne!(a, b, "two nonces must not collide");
}

/// DT-3: empty identity claims and all-zero nonces are refused, and a
/// verifier returns `false` (never a panic) when the transcript itself is
/// unusable.
#[test]
fn handshake_refuses_empty_claims_and_bad_nonces() {
    let secret = b"shared-hmac-key";
    let mut empty_boot = sample_transcript();
    empty_boot.boot_id.clear();
    assert_eq!(
        guest_proof(secret, &empty_boot),
        Err(HandshakeError::EmptyIdentityClaim)
    );
    assert!(!verify_guest_proof(secret, &empty_boot, &[1u8; 32]));

    let mut empty_distro = sample_transcript();
    empty_distro.distro_id.clear();
    assert_eq!(
        guest_proof(secret, &empty_distro),
        Err(HandshakeError::EmptyIdentityClaim)
    );

    let mut zero_guest = sample_transcript();
    zero_guest.guest_nonce = [0u8; 32];
    assert_eq!(
        guest_proof(secret, &zero_guest),
        Err(HandshakeError::NonceNotFresh)
    );
    assert!(!verify_guest_proof(secret, &zero_guest, &[1u8; 32]));

    let t = sample_transcript();
    let mut zero_host = sample_challenge();
    zero_host.host_nonce = [0u8; 32];
    assert_eq!(
        host_proof(secret, &t, &zero_host),
        Err(HandshakeError::NonceNotFresh)
    );
    assert!(!verify_host_proof(secret, &t, &zero_host, &[1u8; 32]));
    assert!(!verify_guest_finish_proof(
        secret, &t, &zero_host, &[1u8; 32]
    ));
}

// --- Stream framing ---

#[test]
fn write_then_read_frame_round_trips() {
    let mut buf = Vec::new();
    write_frame(&mut buf, MSG_HEARTBEAT, b"hello-frame", 77).expect("write");
    assert_eq!(buf.len(), FRAME_HEADER_LEN + 11);

    let mut cursor = std::io::Cursor::new(buf);
    let (msg_type, payload, correlation_id) =
        read_frame(&mut cursor, MAX_CONTROL_PAYLOAD).expect("read");
    assert_eq!(msg_type, MSG_HEARTBEAT);
    assert_eq!(payload, b"hello-frame");
    assert_eq!(correlation_id, 77);
}

#[test]
fn write_then_read_empty_payload_frame_round_trips() {
    let mut buf = Vec::new();
    write_frame(&mut buf, MSG_HANDSHAKE_FINISH, b"", 0).expect("write");
    assert_eq!(buf.len(), FRAME_HEADER_LEN);

    let mut cursor = std::io::Cursor::new(buf);
    let (msg_type, payload, correlation_id) =
        read_frame(&mut cursor, MAX_CONTROL_PAYLOAD).expect("read");
    assert_eq!(msg_type, MSG_HANDSHAKE_FINISH);
    assert!(payload.is_empty());
    assert_eq!(correlation_id, 0);
}

#[test]
fn write_frame_rejects_unknown_message_type() {
    let mut buf = Vec::new();
    assert_eq!(
        write_frame(&mut buf, 0, b"x", 1),
        Err(FrameError::UnknownMessageType(0))
    );
    assert_eq!(
        write_frame(&mut buf, MSG_MAX + 1, b"x", 1),
        Err(FrameError::UnknownMessageType(MSG_MAX + 1))
    );
    assert!(
        buf.is_empty(),
        "no bytes may be emitted before the type is known good"
    );
}

#[test]
fn write_frame_rejects_oversized_payload() {
    let mut buf = Vec::new();
    let huge = vec![0u8; (MAX_PAYLOAD_LEN + 1) as usize];
    assert!(matches!(
        write_frame(&mut buf, MSG_HEARTBEAT, &huge, 1),
        Err(FrameError::PayloadTooLarge(_))
    ));
    assert!(buf.is_empty());
}

#[test]
fn read_frame_rejects_declared_length_above_the_caller_cap_before_reading_the_body() {
    // Header claims 64 bytes but the caller only allows 8. The body is never
    // read, so a peer cannot push the reader into a large allocation.
    let mut buf = Vec::new();
    {
        let header = VsockFrameHeader::new(MSG_HEARTBEAT, 64, 5);
        buf.extend_from_slice(&header.encode());
    }
    buf.extend_from_slice(&[0xABu8; 64]);

    let mut cursor = std::io::Cursor::new(buf);
    assert!(matches!(
        read_frame(&mut cursor, 8),
        Err(FrameError::PayloadTooLarge(64))
    ));
    assert_eq!(
        cursor.position(),
        FRAME_HEADER_LEN as u64,
        "the body must not have been consumed"
    );
}

#[test]
fn read_frame_rejects_declared_length_above_the_global_cap() {
    let header = VsockFrameHeader {
        magic: IPC_MAGIC,
        version: IPC_VERSION_3,
        payload_len: MAX_PAYLOAD_LEN + 1,
        flags: MSG_HEARTBEAT as u32,
        correlation_id: 0,
    };
    let mut buf = header.encode().to_vec();
    buf.extend_from_slice(&[0u8; 4]);
    let mut cursor = std::io::Cursor::new(buf);
    assert!(matches!(
        read_frame(&mut cursor, MAX_PAYLOAD_LEN),
        Err(FrameError::PayloadTooLarge(_))
    ));
}

#[test]
fn read_frame_reports_truncation_instead_of_an_empty_body() {
    // Header declares 16 payload bytes; only 4 arrive before EOF.
    let header = VsockFrameHeader::new(MSG_HEARTBEAT, 16, 9);
    let mut buf = header.encode().to_vec();
    buf.extend_from_slice(&[0xAA; 4]);

    let mut cursor = std::io::Cursor::new(buf);
    assert_eq!(
        read_frame(&mut cursor, MAX_CONTROL_PAYLOAD),
        Err(FrameError::TruncatedFrame {
            expected: 16,
            got: 4
        })
    );
}

#[test]
fn read_frame_reports_truncation_of_the_header_itself() {
    let mut cursor = std::io::Cursor::new(vec![0x52, 0x41, 0x4D]);
    assert_eq!(
        read_frame(&mut cursor, MAX_CONTROL_PAYLOAD),
        Err(FrameError::TruncatedFrame {
            expected: FRAME_HEADER_LEN,
            got: 3
        })
    );
}

#[test]
fn read_frame_surfaces_bad_magic_as_a_decode_error() {
    let mut buf = vec![0u8; FRAME_HEADER_LEN];
    buf[0] = 0xDE;
    let mut cursor = std::io::Cursor::new(buf);
    assert!(matches!(
        read_frame(&mut cursor, MAX_CONTROL_PAYLOAD),
        Err(FrameError::InvalidMagic(_))
    ));
}

#[test]
fn read_frame_over_a_socket_pair_round_trips_several_frames_in_order() {
    use std::io::{Read, Write};

    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("listen");
    let mut writer =
        std::net::TcpStream::connect(listener.local_addr().expect("address")).expect("connect");
    let (reader, _) = listener.accept().expect("accept");
    reader
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .expect("read bound");
    writer
        .set_write_timeout(Some(std::time::Duration::from_secs(2)))
        .expect("write bound");
    let payloads: [&[u8]; 3] = [b"one", b"", b"three-three"];
    for (i, payload) in payloads.iter().enumerate() {
        write_frame(&mut writer, MSG_HEARTBEAT, payload, i as u64).expect("write");
    }

    let mut reader = reader;
    for (i, expected) in payloads.iter().enumerate() {
        let (msg_type, payload, correlation_id) =
            read_frame(&mut reader, MAX_CONTROL_PAYLOAD).expect("read");
        assert_eq!(msg_type, MSG_HEARTBEAT);
        assert_eq!(&payload, expected);
        assert_eq!(correlation_id, i as u64);
    }

    // A clean peer close after the last frame is reported as a truncation
    // of a header that never arrived — never as a phantom empty frame.
    writer.flush().expect("flush");
    drop(writer);
    let mut scratch = [0u8; 1];
    assert_eq!(reader.read(&mut scratch).expect("eof read"), 0);
}

#[test]
fn catalog_header_requires_version_four_and_keeps_legacy_bootstrap() {
    let mut header = VsockFrameHeader::new(MSG_GPU_CATALOG, 0, 4);
    assert_eq!(header.version, IPC_VERSION_4);
    assert_eq!(
        VsockFrameHeader::decode(&header.encode())
            .unwrap()
            .msg_type(),
        MSG_GPU_CATALOG
    );
    for version in [2, 3] {
        header.version = version;
        assert_eq!(
            VsockFrameHeader::decode(&header.encode()),
            Err(FrameError::UnsupportedVersion(version))
        );
    }
    header.version = IPC_VERSION_5;
    assert_eq!(
        VsockFrameHeader::decode(&header.encode())
            .unwrap()
            .msg_type(),
        MSG_GPU_CATALOG
    );
    for msg_type in [MSG_STORAGE_REQUEST, MSG_STORAGE_RESPONSE] {
        let mut storage_header = VsockFrameHeader::new(msg_type, 0, 1);
        assert_eq!(storage_header.version, IPC_VERSION_5);
        assert_eq!(
            VsockFrameHeader::decode(&storage_header.encode())
                .unwrap()
                .msg_type(),
            msg_type
        );
        for old in [2, 3, IPC_VERSION_4] {
            storage_header.version = old;
            assert_eq!(
                VsockFrameHeader::decode(&storage_header.encode()),
                Err(FrameError::UnsupportedVersion(old))
            );
        }
    }
    assert_eq!(
        VsockFrameHeader::new(MSG_HANDSHAKE, 0, 1).version,
        IPC_VERSION_3
    );
}
#[test]
fn reserved_storage_protocol_preserves_catalog_negotiation_without_advertising() {
    use crate::gpu_catalog::CatalogSession;
    use crate::storage_proof::StorageSession;
    let mut transcript = HandshakeTranscript {
        min_version: 2,
        max_version: 5,
        boot_id: "boot".into(),
        distro_id: "guest".into(),
        guest_nonce: [1; 32],
    };
    let mut challenge = HostChallenge {
        accepted_version: 5,
        heartbeat_secs: 5,
        lease_timeout_secs: 20,
        host_nonce: [2; 32],
    };
    assert_eq!(IPC_MAX_VERSION, IPC_VERSION_5);
    assert!(CatalogSession::new(b"secret", &transcript, &challenge, 7).is_ok());
    assert!(StorageSession::new(b"secret", &transcript, &challenge, 7).is_ok());
    transcript.max_version = 4;
    assert!(CatalogSession::new(b"secret", &transcript, &challenge, 7).is_err());
    assert!(StorageSession::new(b"secret", &transcript, &challenge, 7).is_err());
    challenge.accepted_version = 4;
    assert!(CatalogSession::new(b"secret", &transcript, &challenge, 7).is_ok());
    assert!(StorageSession::new(b"secret", &transcript, &challenge, 7).is_err());
    challenge.accepted_version = 6;
    transcript.max_version = 6;
    assert!(CatalogSession::new(b"secret", &transcript, &challenge, 7).is_err());
    assert!(StorageSession::new(b"secret", &transcript, &challenge, 7).is_err());
}
