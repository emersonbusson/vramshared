// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-ipc/tests/cache_wire.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{self, Cursor, Read, Write};

use ramshared_ipc::cache_wire::{
    CACHE_HEADER_LEN, CACHE_MAX_FRAME_BYTES, CACHE_PAGE_BYTES, CACHE_PAYLOAD_BYTES,
    CACHE_WIRE_MAGIC, CACHE_WIRE_VERSION, CacheFrameBuffer, CacheFrameHeader, CacheGuestRole,
    CacheOpcode, CacheScope, CacheWireError, DEFAULT_POOLED_CACHE_PORT, crc32c, read_into,
    validate_cache_data_port, write_frame_with,
};
use ramshared_ipc::{FrameError, MSG_HEARTBEAT, read_frame, write_frame};

fn sample_scope(role: CacheGuestRole, epoch: u64) -> CacheScope {
    CacheScope {
        role,
        runtime_id: [0x11; 16],
        service_guid: validate_cache_data_port(DEFAULT_POOLED_CACHE_PORT, 2761).unwrap(),
        origin_fingerprint: [0x22; 32],
        epoch,
        extent_bytes: 16 * CACHE_PAGE_BYTES as u64,
        guest_nonce: [0x33; 32],
        host_nonce: [0x44; 32],
    }
}

/// Chunked reader simulating partial stream reads and EINTR retries.
struct PartialReader<'a> {
    data: &'a [u8],
    pos: usize,
    chunk: usize,
    inject_eintr: bool,
}

impl<'a> Read for PartialReader<'a> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.inject_eintr {
            self.inject_eintr = false;
            return Err(io::Error::new(io::ErrorKind::Interrupted, "eintr"));
        }
        if self.pos >= self.data.len() {
            return Ok(0);
        }
        let n = self.chunk.min(buf.len()).min(self.data.len() - self.pos);
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

struct FailingWriter;
impl Write for FailingWriter {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "broken"))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn cache_wire_accepts_bound_offer_and_refuses_replay_and_cross_role() {
    // Standard Castagnoli CRC32C check vector (RFC 3720).
    assert_eq!(crc32c(b"123456789"), 0xE306_9283);

    // Data-lane port validation and control-port collision refusal.
    assert_eq!(
        validate_cache_data_port(0, 2761),
        Err(CacheWireError::InvalidPort)
    );
    assert_eq!(
        validate_cache_data_port(0x8000_0000, 2761),
        Err(CacheWireError::InvalidPort)
    );
    assert_eq!(
        validate_cache_data_port(2761, 2761),
        Err(CacheWireError::PortCollision)
    );
    let guid = validate_cache_data_port(DEFAULT_POOLED_CACHE_PORT, 2761).unwrap();
    assert_eq!(&guid[0..4], &DEFAULT_POOLED_CACHE_PORT.to_be_bytes());

    // Mutual proof binds role, route, origin fingerprint, epoch, and nonces.
    let wsl_key = [0xA5u8; 32];
    let secondary_key = [0x5Au8; 32];
    let wsl_scope = sample_scope(CacheGuestRole::Wsl2, 7);
    let secondary_scope = sample_scope(CacheGuestRole::SecondaryGuest, 7);

    let g_proof = wsl_scope.guest_proof(&wsl_key).unwrap();
    let h_proof = wsl_scope.host_proof(&wsl_key).unwrap();
    let f_proof = wsl_scope.finish_proof(&wsl_key).unwrap();
    assert!(wsl_scope.verify_guest_proof(&wsl_key, &g_proof));
    assert!(wsl_scope.verify_host_proof(&wsl_key, &h_proof));
    assert!(wsl_scope.verify_finish_proof(&wsl_key, &f_proof));

    // Domain separation & cross-role refusal (#13).
    assert!(!wsl_scope.verify_guest_proof(&wsl_key, &h_proof));
    assert!(!wsl_scope.verify_host_proof(&wsl_key, &f_proof));
    assert!(!secondary_scope.verify_guest_proof(&wsl_key, &g_proof));
    assert!(!wsl_scope.verify_guest_proof(&secondary_key, &g_proof));
    assert!(!sample_scope(CacheGuestRole::Wsl2, 8).verify_guest_proof(&wsl_key, &g_proof));

    // Invalid scopes and short keys are refused.
    assert!(matches!(
        wsl_scope.guest_proof(&[0x11; 31]),
        Err(CacheWireError::InvalidScope(_))
    ));
    let mut bad = sample_scope(CacheGuestRole::Wsl2, 1);
    bad.runtime_id = [0; 16];
    assert!(bad.validate().is_err());
    let mut bad = sample_scope(CacheGuestRole::Wsl2, 1);
    bad.origin_fingerprint = [0; 32];
    assert!(bad.validate().is_err());
    let mut bad = sample_scope(CacheGuestRole::Wsl2, 0);
    assert_eq!(bad.validate(), Err(CacheWireError::InvalidEpoch));
    bad.epoch = 1;
    bad.extent_bytes = 4097;
    assert!(bad.validate().is_err());
    bad.extent_bytes = 4096;
    bad.guest_nonce = [0; 32];
    assert!(bad.validate().is_err());

    // Roundtrip a bound Offer and Accepted reply through fixed CacheFrameBuffer.
    let mut page = [0u8; CACHE_PAGE_BYTES];
    for (idx, byte) in page.iter_mut().enumerate() {
        *byte = ((idx * 31) ^ 0x5C) as u8;
    }
    let offer_hdr = CacheFrameHeader::new(CacheOpcode::Offer, 1, 7, 4096, 3).unwrap();
    let mut wire = Vec::new();
    let mut wbuf = CacheFrameBuffer::default();
    write_frame_with(&mut wire, &mut wbuf, offer_hdr, Some(&page)).unwrap();
    assert_eq!(wire.len(), CACHE_MAX_FRAME_BYTES);

    let mut rbuf = CacheFrameBuffer::new();
    let mut reader = PartialReader {
        data: &wire,
        pos: 0,
        chunk: 73,
        inject_eintr: true,
    };
    let decoded = read_into(&mut reader, &mut rbuf).unwrap();
    assert_eq!(decoded.header, offer_hdr);
    assert_eq!(decoded.page, Some(&page));
    assert_eq!(decoded.crc, Some(crc32c(&page)));

    let mut last_corr = 0u64;
    assert!(
        wsl_scope
            .admit_request(&decoded.header, &mut last_corr)
            .is_ok()
    );
    assert_eq!(last_corr, 1);

    // Replay of the same correlation is refused (#17 / #13) and leaves cursor unchanged.
    assert_eq!(
        wsl_scope.admit_request(&decoded.header, &mut last_corr),
        Err(CacheWireError::InvalidCorrelation)
    );
    assert_eq!(last_corr, 1);

    // Stale epoch is refused.
    let stale_epoch = CacheFrameHeader::new(CacheOpcode::Lookup, 2, 6, 4096, 3).unwrap();
    assert_eq!(
        wsl_scope.admit_request(&stale_epoch, &mut last_corr),
        Err(CacheWireError::InvalidEpoch)
    );

    // Out-of-extent offset is refused.
    let out_of_extent =
        CacheFrameHeader::new(CacheOpcode::Lookup, 2, 7, wsl_scope.extent_bytes, 3).unwrap();
    assert_eq!(
        wsl_scope.admit_request(&out_of_extent, &mut last_corr),
        Err(CacheWireError::OutOfExtent(wsl_scope.extent_bytes))
    );

    // Reply opcodes on the request side are refused as unsolicited.
    let unsolicited_req = CacheFrameHeader::new(CacheOpcode::Hit, 2, 7, 0, 3).unwrap();
    assert_eq!(
        wsl_scope.admit_request(&unsolicited_req, &mut last_corr),
        Err(CacheWireError::UnsolicitedOrMismatchedReply)
    );

    // Legal and illegal reply shapes for Lookup and Offer.
    let accepted = CacheFrameHeader::new(CacheOpcode::Accepted, 1, 7, 4096, 3).unwrap();
    assert!(wsl_scope.admit_reply(&offer_hdr, &accepted).is_ok());
    let wrong_reply = CacheFrameHeader::new(CacheOpcode::Hit, 1, 7, 4096, 3).unwrap();
    assert_eq!(
        wsl_scope.admit_reply(&offer_hdr, &wrong_reply),
        Err(CacheWireError::UnsolicitedOrMismatchedReply)
    );
    let lookup = CacheFrameHeader::new(CacheOpcode::Lookup, 2, 7, 4096, 3).unwrap();
    for ok_op in [CacheOpcode::Hit, CacheOpcode::Miss, CacheOpcode::Busy] {
        let reply = CacheFrameHeader::new(ok_op, 2, 7, 4096, 3).unwrap();
        assert!(wsl_scope.admit_reply(&lookup, &reply).is_ok());
    }
    let bad_gen = CacheFrameHeader::new(CacheOpcode::Miss, 2, 7, 4096, 4).unwrap();
    assert_eq!(
        wsl_scope.admit_reply(&lookup, &bad_gen),
        Err(CacheWireError::UnsolicitedOrMismatchedReply)
    );
    assert!(!CacheOpcode::Hit.accepts_reply(CacheOpcode::Miss));

    // Write empty-payload frame and failing writer error propagation.
    let mut empty_wire = Vec::new();
    write_frame_with(&mut empty_wire, &mut wbuf, lookup, None).unwrap();
    assert_eq!(empty_wire.len(), CACHE_HEADER_LEN);
    let decoded_empty = read_into(&mut Cursor::new(&empty_wire), &mut rbuf).unwrap();
    assert_eq!(decoded_empty.page, None);
    assert_eq!(decoded_empty.crc, None);
    assert!(matches!(
        write_frame_with(&mut wbuf_sink(), &mut wbuf, lookup, Some(&page)),
        Err(CacheWireError::InvalidPayloadLength { .. })
    ));
    assert!(matches!(
        write_frame_with(&mut FailingWriter, &mut wbuf, lookup, None),
        Err(CacheWireError::Frame(FrameError::StreamIo(_)))
    ));
    let _ = format!("{}", CacheWireError::PortCollision);
}

fn wbuf_sink() -> Vec<u8> {
    Vec::new()
}

/// Deterministic XorShift64* PRNG for reproducible property testing.
/// Recorded seed: `0x5253_4350_2026_1127`.
struct SeededRng(u64);
impl SeededRng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn fill(&mut self, out: &mut [u8]) {
        for chunk in out.chunks_mut(8) {
            let v = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }
}

#[test]
fn cache_wire_parses_bounded_arbitrary_streams() {
    const SEED: u64 = 0x5253_4350_2026_1127;
    let mut rng = SeededRng(SEED);
    let mut rbuf = CacheFrameBuffer::new();
    let mut wbuf = CacheFrameBuffer::new();
    let mut raw = vec![0u8; CACHE_MAX_FRAME_BYTES + 64];

    // 1. Arbitrary byte streams must never panic and must only succeed on valid frames.
    for case in 0..1024usize {
        let len = (rng.next_u64() as usize) % raw.len();
        rng.fill(&mut raw[..len]);
        // Occasionally inject valid magic/version to penetrate deeper into header checks.
        if case % 3 == 0 && len >= 6 {
            raw[0..4].copy_from_slice(&CACHE_WIRE_MAGIC);
            raw[4] = CACHE_WIRE_VERSION;
        }
        let mut cursor = Cursor::new(&raw[..len]);
        let res = read_into(&mut cursor, &mut rbuf);
        if let Ok(frame) = res {
            assert_eq!(frame.header.version, CACHE_WIRE_VERSION);
            assert!(frame.header.validate().is_ok());
            assert_eq!(
                cursor.position() as usize,
                CACHE_HEADER_LEN + frame.header.payload_len as usize
            );
        }
    }

    // 2. Roundtrip random valid frames and assert single-bit corruption is always caught.
    for case in 0..128u64 {
        let mut page = [0u8; CACHE_PAGE_BYTES];
        rng.fill(&mut page);
        let opcode = if case % 2 == 0 {
            CacheOpcode::Offer
        } else {
            CacheOpcode::Hit
        };
        let hdr = CacheFrameHeader::new(
            opcode,
            case + 1,
            (case % 9) + 1,
            (case % 64) * CACHE_PAGE_BYTES as u64,
            case * 7,
        )
        .unwrap();
        let mut wire = Vec::with_capacity(CACHE_MAX_FRAME_BYTES);
        write_frame_with(&mut wire, &mut wbuf, hdr, Some(&page)).unwrap();
        let ok = read_into(&mut Cursor::new(&wire), &mut rbuf).unwrap();
        assert_eq!(ok.header, hdr);
        assert_eq!(ok.page, Some(&page));

        // Flip one bit in either page or CRC and assert CrcMismatch.
        let flip_idx = CACHE_HEADER_LEN + ((rng.next_u64() as usize) % CACHE_PAYLOAD_BYTES);
        wire[flip_idx] ^= 1 << ((rng.next_u64() as u8) & 7);
        assert!(matches!(
            read_into(&mut Cursor::new(&wire), &mut rbuf),
            Err(CacheWireError::CrcMismatch { .. })
        ));
    }
}

#[test]
fn old_frame_read_preserves_truncation_and_new_lane_rejects_lengths() {
    // 1. Legacy frame reader keeps exact truncation & roundtrip parity.
    let mut legacy_wire = Vec::new();
    let body = br#"{"timestamp_ms":42}"#;
    write_frame(&mut legacy_wire, MSG_HEARTBEAT, body, 99).unwrap();
    let (msg_type, decoded_body, corr) = read_frame(&mut Cursor::new(&legacy_wire), 4096).unwrap();
    assert_eq!(msg_type, MSG_HEARTBEAT);
    assert_eq!(decoded_body, body);
    assert_eq!(corr, 99);

    let truncated_legacy = &legacy_wire[..legacy_wire.len() - 3];
    assert_eq!(
        read_frame(&mut Cursor::new(truncated_legacy), 4096),
        Err(FrameError::TruncatedFrame {
            expected: body.len(),
            got: body.len() - 3,
        })
    );

    // Failing header write, failing body write, and mid-stream read IO errors map to StreamIo.
    struct FailAfter {
        remaining: usize,
    }
    impl Write for FailAfter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "pipe closed"));
            }
            let n = self.remaining.min(buf.len());
            self.remaining -= n;
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    struct FailingReader;
    impl Read for FailingReader {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "read error"))
        }
    }
    assert!(matches!(
        write_frame(&mut FailingWriter, MSG_HEARTBEAT, body, 1),
        Err(FrameError::StreamIo(_))
    ));
    assert!(matches!(
        write_frame(&mut FailAfter { remaining: 24 }, MSG_HEARTBEAT, body, 1),
        Err(FrameError::StreamIo(_))
    ));
    assert!(matches!(
        read_frame(&mut FailingReader, 4096),
        Err(FrameError::StreamIo(_))
    ));

    // 2. New pooled cache lane rejects hostile lengths, reserved bits, bad magic/version/opcode,
    // alignment/overflow, and truncated bodies before accepting any frame.
    let mut rbuf = CacheFrameBuffer::new();
    let valid_hdr = CacheFrameHeader::new(CacheOpcode::Lookup, 1, 1, 0, 1).unwrap();
    let base = valid_hdr.encode();

    // Bad magic.
    let mut bad_magic = base;
    bad_magic[0] = b'X';
    assert_eq!(
        CacheFrameHeader::decode(&bad_magic),
        Err(CacheWireError::InvalidMagic(*b"XSCP"))
    );

    // Bad version.
    let mut bad_ver = base;
    bad_ver[4] = 2;
    assert_eq!(
        CacheFrameHeader::decode(&bad_ver),
        Err(CacheWireError::UnsupportedVersion(2))
    );

    // Unknown opcode.
    let mut bad_op = base;
    bad_op[5] = 99;
    assert_eq!(
        CacheFrameHeader::decode(&bad_op),
        Err(CacheWireError::UnknownOpcode(99))
    );

    // Nonzero reserved flags or reserved u32.
    let mut bad_res = base;
    bad_res[6] = 1;
    assert_eq!(
        CacheFrameHeader::decode(&bad_res),
        Err(CacheWireError::NonZeroReserved)
    );
    let mut bad_res2 = base;
    bad_res2[12] = 1;
    assert_eq!(
        CacheFrameHeader::decode(&bad_res2),
        Err(CacheWireError::NonZeroReserved)
    );

    // Hostile payload length on Lookup (expected 0, got 4100) and Offer (expected 4100, got 1 MiB).
    let mut bad_len = base;
    bad_len[8..12].copy_from_slice(&(CACHE_PAYLOAD_BYTES as u32).to_le_bytes());
    assert_eq!(
        read_into(&mut Cursor::new(&bad_len), &mut rbuf),
        Err(CacheWireError::InvalidPayloadLength {
            expected: 0,
            got: CACHE_PAYLOAD_BYTES as u32,
        })
    );
    let mut hostile_offer = base;
    hostile_offer[5] = CacheOpcode::Offer as u8;
    hostile_offer[8..12].copy_from_slice(&(1024 * 1024u32).to_le_bytes());
    assert_eq!(
        read_into(&mut Cursor::new(&hostile_offer), &mut rbuf),
        Err(CacheWireError::InvalidPayloadLength {
            expected: CACHE_PAYLOAD_BYTES as u32,
            got: 1024 * 1024,
        })
    );

    // Zero/wrapping correlation, zero epoch, unaligned/overflow offset.
    assert_eq!(
        CacheFrameHeader::new(CacheOpcode::Lookup, 0, 1, 0, 1),
        Err(CacheWireError::InvalidCorrelation)
    );
    assert_eq!(
        CacheFrameHeader::new(CacheOpcode::Lookup, u64::MAX, 1, 0, 1),
        Err(CacheWireError::InvalidCorrelation)
    );
    assert_eq!(
        CacheFrameHeader::new(CacheOpcode::Lookup, 1, 0, 0, 1),
        Err(CacheWireError::InvalidEpoch)
    );
    assert_eq!(
        CacheFrameHeader::new(CacheOpcode::Lookup, 1, 1, 1, 1),
        Err(CacheWireError::UnalignedOrOverflowOffset(1))
    );
    assert_eq!(
        CacheFrameHeader::new(CacheOpcode::Lookup, 1, 1, u64::MAX - 4095, 1),
        Err(CacheWireError::UnalignedOrOverflowOffset(u64::MAX - 4095))
    );

    // Truncated body on Offer.
    let mut truncated_offer = CacheFrameHeader::new(CacheOpcode::Offer, 1, 1, 0, 1)
        .unwrap()
        .encode()
        .to_vec();
    truncated_offer.extend_from_slice(&[0u8; 100]);
    assert_eq!(
        read_into(&mut Cursor::new(&truncated_offer), &mut rbuf),
        Err(CacheWireError::Frame(FrameError::TruncatedFrame {
            expected: CACHE_PAYLOAD_BYTES,
            got: 100,
        }))
    );
}

#[test]
fn cache_wire_handshake_frames_roundtrip_and_refuse_malformed_inputs() {
    use ramshared_ipc::cache_wire::{
        CACHE_CHALLENGE_LEN, CACHE_FINISH_LEN, CACHE_HELLO_LEN, CacheClientFinish,
        CacheClientHello, CacheHostChallenge, origin_fingerprint_from_binding, read_client_finish,
        read_client_hello, read_host_challenge, write_client_finish, write_client_hello,
        write_host_challenge,
    };

    // Origin binding fingerprint is deterministic, case-insensitive, and refuses empty strings.
    assert!(origin_fingerprint_from_binding("   ").is_err());
    let fp_a = origin_fingerprint_from_binding("5039DCA1-61A0-41FF-AA08-221F49326A1B").unwrap();
    let fp_b = origin_fingerprint_from_binding(" 5039dca1-61a0-41ff-aa08-221f49326a1b ").unwrap();
    assert_eq!(fp_a, fp_b);
    assert_ne!(fp_a, [0u8; 32]);

    // 1. ClientHello roundtrip for both roles + validation/wire refusals.
    for role in [CacheGuestRole::Wsl2, CacheGuestRole::SecondaryGuest] {
        let hello = CacheClientHello {
            role,
            requested_epoch: 3,
            extent_bytes: 16 * CACHE_PAGE_BYTES as u64,
            origin_fingerprint: fp_a,
            guest_nonce: [0x11; 32],
        };
        let mut wire = Vec::new();
        write_client_hello(&mut wire, &hello).unwrap();
        assert_eq!(wire.len(), CACHE_HELLO_LEN);
        assert_eq!(read_client_hello(&mut Cursor::new(&wire)).unwrap(), hello);
    }
    let bad_extent = CacheClientHello {
        role: CacheGuestRole::Wsl2,
        requested_epoch: 1,
        extent_bytes: 4097,
        origin_fingerprint: fp_a,
        guest_nonce: [0x11; 32],
    };
    assert!(bad_extent.encode().is_err());
    let zero_nonce = CacheClientHello {
        extent_bytes: 4096,
        guest_nonce: [0; 32],
        ..bad_extent
    };
    assert!(zero_nonce.encode().is_err());
    let valid_hello = CacheClientHello {
        extent_bytes: 4096,
        guest_nonce: [0x11; 32],
        ..bad_extent
    };
    let h_raw = valid_hello.encode().unwrap();
    let mut bad = h_raw;
    bad[0] = b'X';
    assert!(matches!(
        CacheClientHello::decode(&bad),
        Err(CacheWireError::InvalidMagic(_))
    ));
    let mut bad = h_raw;
    bad[4] = 9;
    assert_eq!(
        CacheClientHello::decode(&bad),
        Err(CacheWireError::UnsupportedVersion(9))
    );
    let mut bad = h_raw;
    bad[6] = 1;
    assert_eq!(
        CacheClientHello::decode(&bad),
        Err(CacheWireError::NonZeroReserved)
    );
    let mut bad = h_raw;
    bad[5] = 7;
    assert_eq!(
        CacheClientHello::decode(&bad),
        Err(CacheWireError::UnknownOpcode(7))
    );
    assert!(matches!(
        write_client_hello(&mut FailingWriter, &valid_hello),
        Err(CacheWireError::Frame(FrameError::StreamIo(_)))
    ));

    // 2. HostChallenge accepted + rejected roundtrip and wire refusals.
    for ch in [
        CacheHostChallenge {
            accepted: true,
            epoch: 5,
            runtime_id: [0xA1; 16],
            host_nonce: [0x22; 32],
            host_proof: [0x33; 32],
        },
        CacheHostChallenge::rejected(),
    ] {
        let mut wire = Vec::new();
        write_host_challenge(&mut wire, &ch).unwrap();
        assert_eq!(wire.len(), CACHE_CHALLENGE_LEN);
        assert_eq!(read_host_challenge(&mut Cursor::new(&wire)).unwrap(), ch);
    }
    let c_raw = CacheHostChallenge::rejected().encode();
    let mut bad = c_raw;
    bad[0] = b'Z';
    assert!(matches!(
        CacheHostChallenge::decode(&bad),
        Err(CacheWireError::InvalidMagic(_))
    ));
    let mut bad = c_raw;
    bad[4] = 2;
    assert_eq!(
        CacheHostChallenge::decode(&bad),
        Err(CacheWireError::UnsupportedVersion(2))
    );
    let mut bad = c_raw;
    bad[7] = 1;
    assert_eq!(
        CacheHostChallenge::decode(&bad),
        Err(CacheWireError::NonZeroReserved)
    );
    let mut bad = c_raw;
    bad[5] = 2;
    assert_eq!(
        CacheHostChallenge::decode(&bad),
        Err(CacheWireError::UnknownOpcode(2))
    );
    assert!(matches!(
        write_host_challenge(&mut FailingWriter, &CacheHostChallenge::rejected()),
        Err(CacheWireError::Frame(FrameError::StreamIo(_)))
    ));

    // 3. ClientFinish roundtrip and wire refusals.
    let finish = CacheClientFinish {
        guest_proof: [0x44; 32],
        finish_proof: [0x55; 32],
    };
    let mut wire = Vec::new();
    write_client_finish(&mut wire, &finish).unwrap();
    assert_eq!(wire.len(), CACHE_FINISH_LEN);
    assert_eq!(read_client_finish(&mut Cursor::new(&wire)).unwrap(), finish);
    let f_raw = finish.encode();
    let mut bad = f_raw;
    bad[0] = b'Q';
    assert!(matches!(
        CacheClientFinish::decode(&bad),
        Err(CacheWireError::InvalidMagic(_))
    ));
    let mut bad = f_raw;
    bad[4] = 3;
    assert_eq!(
        CacheClientFinish::decode(&bad),
        Err(CacheWireError::UnsupportedVersion(3))
    );
    let mut bad = f_raw;
    bad[5] = 1;
    assert_eq!(
        CacheClientFinish::decode(&bad),
        Err(CacheWireError::NonZeroReserved)
    );
    assert!(matches!(
        write_client_finish(&mut FailingWriter, &finish),
        Err(CacheWireError::Frame(FrameError::StreamIo(_)))
    ));
}
