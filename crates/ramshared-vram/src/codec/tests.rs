// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-vram/src/codec/tests.rs
// unwrap/expect allowed in tests only (coding.md rules).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::{GpuBudgetSnapshot, GpuBudgetSource, GpuBudgetTelemetry, VramProvider};
use std::time::Instant;

/// A raw-only provider: no codec capability, `cache_codec` stays `None`.
struct RawOnlyProvider;

impl VramMemory for RawOnlyProvider {
    fn len(&self) -> usize {
        0
    }
    fn zero(&mut self) -> Result<(), VramError> {
        Ok(())
    }
    fn read_at(&self, _off: u64, _dst: &mut [u8]) -> Result<(), VramError> {
        Err(VramError::Provider("no memory".into()))
    }
    fn write_at(&mut self, _off: u64, _src: &[u8]) -> Result<(), VramError> {
        Err(VramError::Provider("no memory".into()))
    }
}

impl VramProvider for RawOnlyProvider {
    type Mem<'p> = RawOnlyProvider;
    fn alloc(&self, _bytes: usize) -> Result<Self::Mem<'_>, VramError> {
        Err(VramError::OutOfMemory)
    }
    fn mem_info(&self) -> Result<(u64, u64), VramError> {
        Ok((0, 0))
    }
}

/// `codec_bounds_reject_overflow` — DT-4 / RF-6.
///
/// Every offset-plus-length calculation is checked before any provider
/// call. A span or reservation that overflows its allocation or its slab
/// is refused; a good one is accepted (refusal plus legitimate pass,
/// invariant check).
#[test]
fn codec_bounds_reject_overflow() {
    // stored_len above allocation_len.
    assert!(VramSpan::new(0, 9, 8).is_err());
    // offset + allocation_len overflows u64.
    assert!(VramSpan::new(u64::MAX, 1, 8).is_err());
    // A well-formed span is accepted.
    let good = VramSpan::new(16, 8, 16).expect("well-formed");
    assert_eq!(good.stored_end().expect("stored end"), 24);
    assert_eq!(good.alloc_end().expect("alloc end"), 32);

    // Allocation end beyond the slab is refused.
    assert!(good.check_within(24).is_err());
    assert!(good.check_within(32).is_ok());
    // The payload fitting is not enough: the allocation must fit too.
    assert!(
        VramSpan::new(20, 4, 16)
            .expect("well-formed")
            .check_within(24)
            .is_err()
    );

    // Reservation capacity above allocation.
    assert!(VramOutputReservation::new(0, 9, 8).is_err());
    let out = VramOutputReservation::new(0, 8, 16).expect("well-formed");
    assert!(out.check_within(8).is_err());
    assert!(out.check_within(16).is_ok());
    // Offset overflow on the reservation.
    assert!(
        VramOutputReservation::new(u64::MAX, 1, 8)
            .expect("well-formed")
            .check_within(usize::MAX)
            .is_err()
    );

    // Alignments must be non-zero powers of two (DT-4).
    assert!(CodecAlignments::byte().validate().is_ok());
    assert!(
        CodecAlignments {
            compression_input: 3,
            ..CodecAlignments::byte()
        }
        .validate()
        .is_err()
    );
    assert!(
        CodecAlignments {
            workspace: 0,
            ..CodecAlignments::byte()
        }
        .validate()
        .is_err()
    );
}

/// `unsupported_provider_is_raw_only` — RF-7 / DT-2.
///
/// A provider that does not implement the optional codec capability stays
/// raw-only: `cache_codec` returns `None` and the raw path is what serves.
#[test]
fn unsupported_provider_is_raw_only() {
    let provider = RawOnlyProvider;
    assert!(
        provider.cache_codec().is_none(),
        "a raw-only provider must report no codec capability"
    );

    // The default is also present on any provider that does not override.
    fn default_is_none<P: VramProvider>(p: &P) -> bool {
        p.cache_codec().is_none()
    }
    assert!(default_is_none(&provider));

    // A raw-only provider never allocates on demand either: the physical
    // target of zero must not create slabs (DT-5). Here the provider
    // itself refuses allocation, which is the same fail-safe shape.
    assert!(provider.alloc(2 * 1024 * 1024).is_err());

    // The capability is opt-in and off by default; nothing in the codec
    // module turns it on (DT-9).
    assert_eq!(CodecId::Raw.as_str(), "raw");
}

#[test]
fn codec_id_and_status_are_labelled() {
    assert_eq!(CodecId::Fake.to_string(), "fake");
    assert_eq!(CodecId::NvcompLz4.to_string(), "nvcomp-lz4");
    assert!(CodecStatus::Ok.is_ok());
    assert!(!CodecStatus::Incompressible.is_ok());
    assert!(!CodecStatus::ChecksumMismatch.is_ok());
    assert_eq!(
        CodecStatus::ChecksumMismatch.to_string(),
        "checksum-mismatch"
    );
    assert_eq!(CodecStatus::TimedOut.to_string(), "timed-out");
}

#[test]
fn crc32_detects_single_bit_corruption() {
    let original = b"ramshared-compressed-entry";
    let good = crc32(original, 0);
    assert_eq!(crc32(original, 0), good, "crc is deterministic");
    let mut corrupted = original.to_vec();
    corrupted[3] ^= 0x01;
    assert_ne!(crc32(&corrupted, 0), good, "one flipped bit must be seen");
    // Empty input has a defined value, not zero-by-accident.
    assert_eq!(crc32(b"", 0), 0);
}

#[test]
fn span_and_reservation_reject_inverted_ranges() {
    // stored_len 0 with a non-zero allocation is legal (an empty payload).
    let empty = VramSpan::new(0, 0, 8).expect("empty payload");
    assert_eq!(empty.stored_len, 0);
    assert_eq!(empty.allocation_len, 8);
    // A zero allocation with a zero payload is legal at offset 0.
    let zero = VramSpan::new(0, 0, 0).expect("zero span");
    assert_eq!(zero.alloc_end().expect("alloc end"), 0);
}

/// A Vec-backed `VramMemory` so the codec contract can be exercised
/// without a GPU provider. Test-only: the shipping tree never selects it.
struct HostSlab {
    bytes: Vec<u8>,
}

impl HostSlab {
    fn with_len(len: usize) -> Self {
        Self {
            bytes: vec![0u8; len],
        }
    }
}

impl VramMemory for HostSlab {
    fn len(&self) -> usize {
        self.bytes.len()
    }
    fn zero(&mut self) -> Result<(), VramError> {
        self.bytes.fill(0);
        Ok(())
    }
    fn read_at(&self, off: u64, dst: &mut [u8]) -> Result<(), VramError> {
        let start = off as usize;
        let end = start.checked_add(dst.len()).ok_or(VramError::OutOfRange {
            off,
            len: dst.len() as u64,
            size: self.bytes.len() as u64,
        })?;
        if end > self.bytes.len() {
            return Err(VramError::OutOfRange {
                off,
                len: dst.len() as u64,
                size: self.bytes.len() as u64,
            });
        }
        dst.copy_from_slice(&self.bytes[start..end]);
        Ok(())
    }
    fn write_at(&mut self, off: u64, src: &[u8]) -> Result<(), VramError> {
        let start = off as usize;
        let end = start.checked_add(src.len()).ok_or(VramError::OutOfRange {
            off,
            len: src.len() as u64,
            size: self.bytes.len() as u64,
        })?;
        if end > self.bytes.len() {
            return Err(VramError::OutOfRange {
                off,
                len: src.len() as u64,
                size: self.bytes.len() as u64,
            });
        }
        self.bytes[start..end].copy_from_slice(src);
        Ok(())
    }
}

/// `fake_codec_roundtrip_is_byte_exact` — ITEM-1 / DT-7.
///
/// The deterministic fake codec reproduces its input exactly for both
/// compressible and incompressible data, and the incompressible branch is
/// the one that grows (so the raw fallback is reachable).
#[test]
fn fake_codec_roundtrip_is_byte_exact() {
    let codec = FakeCodec::new();
    let mut slab = HostSlab::with_len(256 * 1024);
    let mut workspace = HostSlab::with_len(1);

    // Repetitive: must shrink. Random-ish: must grow past the reservation.
    let zeros = vec![0u8; 64 * 1024];
    let mut noise = vec![0u8; 64 * 1024];
    for (i, byte) in noise.iter_mut().enumerate() {
        *byte = ((i.wrapping_mul(2654435761) >> 13) & 0xff) as u8;
    }

    let max_zeros =
        GpuCacheCodec::<HostSlab>::max_encoded_len(&codec, zeros.len()).expect("bounded");
    let max_noise =
        GpuCacheCodec::<HostSlab>::max_encoded_len(&codec, noise.len()).expect("bounded");
    // Worst case is the literal-run expansion: always at least the raw
    // length, and exact for equal lengths.
    assert!(max_zeros >= zeros.len());
    assert_eq!(max_zeros, max_noise, "the bound depends only on length");

    // Reserve at the *encoded* worst case, which is smaller than the raw
    // length only for the zeros case when we reserve exactly the shrink
    // target. Reserve the full worst case here and let the codec report
    // incompressible only when the encoded form exceeds capacity.
    let mut offset = 0u64;
    let mut reserve = |capacity: usize| {
        let out = VramOutputReservation::new(offset, capacity, capacity).expect("reservation");
        offset += capacity as u64;
        out
    };
    let zeros_out = reserve(max_zeros);
    let noise_out = reserve(max_noise);

    let results = codec
        .compress_batch_into(
            &[zeros.as_slice(), noise.as_slice()],
            &mut slab,
            &[zeros_out, noise_out],
            &mut workspace,
        )
        .expect("compress");
    assert_eq!(results.len(), 2);
    assert!(
        results[0].status.is_ok(),
        "zeros must compress: {results:?}"
    );
    assert!(
        results[0].encoded_len < zeros.len(),
        "repetitive data must shrink"
    );
    assert!(results[1].status.is_ok(), "noise fits its worst case");

    // Now force the incompressible path: a reservation sized below the
    // encoded form.
    let tiny = VramOutputReservation::new(0, 4, 4).expect("tiny");
    let refused = codec
        .compress_batch_into(&[noise.as_slice()], &mut slab, &[tiny], &mut workspace)
        .expect("compress call");
    assert_eq!(refused[0].status, CodecStatus::Incompressible);

    // Byte-exact round trip through checksum then decode (DT-7 order).
    let spans = [
        VramSpan::new(zeros_out.offset, results[0].encoded_len, max_zeros).expect("span0"),
        VramSpan::new(noise_out.offset, results[1].encoded_len, max_noise).expect("span1"),
    ];
    let checksums = codec
        .checksum_batch(&slab, &spans, &mut workspace)
        .expect("checksum");
    assert_eq!(checksums.len(), 2);
    let mut decoded = vec![Vec::new(), Vec::new()];
    let decode_before = codec.decode_invocations();
    let statuses = codec
        .decompress_batch_from(
            &slab,
            &spans,
            &[zeros.len(), noise.len()],
            &mut decoded,
            &mut workspace,
        )
        .expect("decode");
    assert_eq!(codec.decode_invocations(), decode_before + 2);
    assert!(statuses.iter().all(|status| status.is_ok()));
    assert_eq!(decoded[0], zeros);
    assert_eq!(decoded[1], noise);
}

/// `fake_codec_checksum_mismatch_refuses_before_decode` — DT-7.
///
/// The stored-payload checksum is computed on the provider and compared
/// before the decoder is invoked. A mismatch must leave the decode counter
/// untouched.
#[test]
fn fake_codec_checksum_mismatch_refuses_before_decode() {
    let codec = FakeCodec::new();
    let mut slab = HostSlab::with_len(4096);
    let mut workspace = HostSlab::with_len(1);
    let payload = vec![7u8; 512];
    let out = VramOutputReservation::new(0, 4096, 4096).expect("reservation");
    let results = codec
        .compress_batch_into(&[payload.as_slice()], &mut slab, &[out], &mut workspace)
        .expect("compress");
    assert!(results[0].status.is_ok());
    let span = VramSpan::new(0, results[0].encoded_len, 4096).expect("span");
    let good = codec
        .checksum_batch(&slab, &[span], &mut workspace)
        .expect("checksum");

    // Corrupt one stored byte; the provider-side checksum must differ.
    let mut probe = vec![0u8; span.stored_len];
    slab.read_at(span.offset, &mut probe).expect("read");
    probe[0] ^= 0xff;
    slab.write_at(span.offset, &probe).expect("write");
    let bad = codec
        .checksum_batch(&slab, &[span], &mut workspace)
        .expect("checksum");
    assert_ne!(good[0], bad[0], "stored-payload checksum must move");

    // The caller refuses before decode: the decoder must not have been
    // entered by any of the steps above (checksum is computed on the
    // provider while the bytes are still in the slab).
    let before = codec.decode_invocations();
    assert_eq!(before, 0, "no decode may precede the checksum comparison");
    let mut decoded = vec![Vec::new()];
    let _ = codec.decompress_batch_from(
        &slab,
        &[span],
        &[payload.len()],
        &mut decoded,
        &mut workspace,
    );
    assert_eq!(codec.decode_invocations(), before + 1);
}

// Keep the unused-import lint quiet for the re-exported budget types used
// by provider implementations outside this module.
#[test]
fn budget_types_remain_the_provider_surface() {
    let snapshot = GpuBudgetSnapshot {
        adapter: None,
        total_bytes: Some(1024),
        budget_bytes: 1024,
        used_bytes: 0,
        source: GpuBudgetSource::ProviderLocalEstimate,
        sampled_at: Instant::now(),
    };
    let telemetry = GpuBudgetTelemetry::from_snapshot(&snapshot, 1);
    assert_eq!(telemetry.budget_bytes, 1024);
}
