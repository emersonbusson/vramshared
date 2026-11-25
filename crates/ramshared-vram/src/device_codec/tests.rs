// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-vram/src/device_codec/tests.rs
#![allow(clippy::unwrap_used)]
use super::*;

fn compressible_page(seed: u8) -> [u8; PAGE_BYTES] {
    let mut page = [0u8; PAGE_BYTES];
    for (index, byte) in page.iter_mut().enumerate() {
        *byte = if index % 64 < 32 {
            seed
        } else {
            (index / 64) as u8
        };
    }
    page
}

fn high_entropy_page() -> [u8; PAGE_BYTES] {
    let mut page = [0u8; PAGE_BYTES];
    let mut state = 0x1234_5678u32;
    for byte in &mut page {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        *byte = state as u8;
    }
    page
}

#[test]
fn reference_codec_roundtrips_all_classes_and_raw_fallback() {
    let mut codec = ReferenceLz4Codec::new();
    let mut pool = VramSlabPool::new(4).unwrap();
    let pages = [compressible_page(7), high_entropy_page()];
    let mut tickets = [None; 2];
    codec
        .compress_and_pack_batch(&pages, &mut pool, &mut tickets)
        .unwrap();
    let mut decoded = [[0u8; PAGE_BYTES]; 2];
    let mut status = [PageDecodeStatus::SkippedOrIncompressible; 2];
    codec
        .verify_and_unpack_batch(
            &[tickets[0].unwrap(), tickets[1].unwrap()],
            &mut decoded,
            &mut status,
        )
        .unwrap();
    assert_eq!(status, [PageDecodeStatus::Ok, PageDecodeStatus::Ok]);
    assert_eq!(decoded[0], pages[0]);
    assert_eq!(decoded[1], pages[1]);
    assert_eq!(tickets[1].unwrap().class, SlabClass::Raw4096);
    assert_eq!(tickets[1].unwrap().encoding, SlotEncoding::Raw);
}

#[test]
fn reference_codec_catches_bitflips_before_decode() {
    let mut codec = ReferenceLz4Codec::new();
    let mut pool = VramSlabPool::new(2).unwrap();
    let pages = [compressible_page(3), compressible_page(4)];
    let mut tickets = [None; 2];
    codec
        .compress_and_pack_batch(&pages, &mut pool, &mut tickets)
        .unwrap();
    let first = tickets[0].unwrap();
    codec.payload_mut(&first).unwrap()[0] ^= 1;
    let mut decoded = [[0u8; PAGE_BYTES]; 2];
    let mut status = [PageDecodeStatus::Ok; 2];
    codec
        .verify_and_unpack_batch(&[first, tickets[1].unwrap()], &mut decoded, &mut status)
        .unwrap();
    assert_eq!(status[0], PageDecodeStatus::PreDecodeCrcMismatch);
    assert_eq!(status[1], PageDecodeStatus::Ok);
    assert_eq!(decoded[1], pages[1]);
}

#[test]
fn reference_codec_rejects_malformed_lz4_after_valid_crc() {
    let mut codec = ReferenceLz4Codec::new();
    let mut pool = VramSlabPool::new(1).unwrap();
    let page = compressible_page(9);
    let mut tickets = [None; 1];
    codec
        .compress_and_pack_batch(&[page], &mut pool, &mut tickets)
        .unwrap();
    let mut ticket = tickets[0].unwrap();
    let crc = {
        let payload = codec.payload_mut(&ticket).unwrap();
        payload[..6].copy_from_slice(&[0x1F, 0, 0, 1, 2, 3]);
        crc32(payload, 0)
    };
    ticket.payload_crc32 = crc;
    let mut decoded = [[0u8; PAGE_BYTES]; 1];
    let mut status = [PageDecodeStatus::Ok; 1];
    codec
        .verify_and_unpack_batch(&[ticket], &mut decoded, &mut status)
        .unwrap();
    assert_eq!(status[0], PageDecodeStatus::CorruptLz4Stream);
}

#[test]
fn reference_codec_rejects_a_stale_epoch_after_slot_reuse() {
    let mut codec = ReferenceLz4Codec::new();
    let mut pool = VramSlabPool::new(1).unwrap();
    let mut tickets = [None; 1];
    codec
        .compress_and_pack_batch(&[compressible_page(1)], &mut pool, &mut tickets)
        .unwrap();
    let stale = tickets[0].unwrap();
    assert!(pool.invalidate(&stale));
    codec.invalidate(&stale);
    assert!(codec.payload_mut(&stale).is_none());
    let mut newer = [None; 1];
    codec
        .compress_and_pack_batch(&[compressible_page(2)], &mut pool, &mut newer)
        .unwrap();
    let mut decoded = [[0u8; PAGE_BYTES]; 2];
    let mut status = [PageDecodeStatus::Ok; 2];
    codec
        .verify_and_unpack_batch(&[stale, newer[0].unwrap()], &mut decoded, &mut status)
        .unwrap();
    assert_eq!(status[0], PageDecodeStatus::InvalidTicket);
    assert_eq!(status[1], PageDecodeStatus::Ok);
}

#[test]
fn reference_codec_deadline_rolls_back_tentative_tickets() {
    let mut codec = ReferenceLz4Codec::new();
    codec.set_force_deadline(true);
    let mut pool = VramSlabPool::new(1).unwrap();
    let mut tickets = [None; 1];
    let error = codec
        .compress_and_pack_batch(&[compressible_page(1)], &mut pool, &mut tickets)
        .unwrap_err();
    assert_eq!(error, CodecError::DeadlineExceeded);
    assert!(tickets[0].is_none());
    assert_eq!(pool.logical_pages_cached(), 0);
}

#[test]
fn lz4_decoder_rejects_out_of_bounds_tokens() {
    let mut dst = [0u8; PAGE_BYTES];
    assert!(lz4_decompress_block(&[0x00], &mut dst).is_err());
    assert!(lz4_decompress_block(&[0xF0, 0xFF], &mut dst).is_err());
    assert!(lz4_decompress_block(&[0x40, 1, 2, 3, 4, 0, 0], &mut dst).is_err());
}

#[test]
fn lz4_encoder_refuses_short_and_high_entropy_input() {
    assert!(lz4_compress_block(&[0u8; 4]).is_none());
    let mut page = [0u8; PAGE_BYTES];
    for (index, byte) in page.iter_mut().enumerate() {
        *byte = (index.wrapping_mul(37) & 0xFF) as u8;
    }
    let _ = lz4_compress_block(&page);
}

#[test]
fn reference_codec_covers_trait_defaults_bounds_rollback_and_frame_cap() {
    struct DefaultInvalidateCodec;
    impl GpuDeviceCodec for DefaultInvalidateCodec {
        fn backend_name(&self) -> &'static str {
            "noop"
        }
        fn compress_and_pack_batch(
            &mut self,
            _pages: &[[u8; PAGE_BYTES]],
            _slab_alloc: &mut VramSlabPool,
            _out_tickets: &mut [Option<VramSlabTicket>],
        ) -> Result<(), CodecError> {
            Ok(())
        }
        fn verify_and_unpack_batch(
            &mut self,
            _tickets: &[VramSlabTicket],
            _out_pages: &mut [[u8; PAGE_BYTES]],
            _out_status: &mut [PageDecodeStatus],
        ) -> Result<(), CodecError> {
            Ok(())
        }
    }

    let mut codec = ReferenceLz4Codec::default();
    assert_eq!(codec.backend_name(), "reference-lz4");
    codec.set_deadline(Duration::from_millis(5));
    assert_eq!(
        CodecError::from(SlabError::InvalidTicket),
        CodecError::Slab(SlabError::InvalidTicket)
    );

    let mut pool = VramSlabPool::new(1).unwrap();
    let too_many_pages = vec![compressible_page(1); MAX_BATCH_PAGES + 1];
    let mut big_tickets = vec![None; MAX_BATCH_PAGES + 1];
    assert_eq!(
        codec.compress_and_pack_batch(&too_many_pages, &mut pool, &mut big_tickets),
        Err(CodecError::InvalidBatchLength)
    );
    assert_eq!(
        codec.compress_and_pack_batch(&[compressible_page(1)], &mut pool, &mut []),
        Err(CodecError::InvalidBatchLength)
    );

    let mut one_ticket = [None; 1];
    codec
        .compress_and_pack_batch(&[compressible_page(5)], &mut pool, &mut one_ticket)
        .unwrap();
    let ticket = one_ticket[0].unwrap();
    assert_eq!(
        codec.payload_len(&ticket),
        Some(usize::from(ticket.payload_len))
    );
    let mut wrong_epoch = ticket;
    wrong_epoch.epoch ^= 1;
    assert!(codec.payload_len(&wrong_epoch).is_none());
    let mut wrong_len = ticket;
    wrong_len.payload_len ^= 1;
    assert!(codec.payload_mut(&wrong_len).is_none());

    let mut noop = DefaultInvalidateCodec;
    noop.invalidate(&ticket);

    let mut mismatched_raw = ticket;
    mismatched_raw.encoding = SlotEncoding::Raw;
    let mut unpacked = [[0u8; PAGE_BYTES]; 1];
    let mut statuses = [PageDecodeStatus::Ok; 1];
    codec
        .verify_and_unpack_batch(&[mismatched_raw], &mut unpacked, &mut statuses)
        .unwrap();
    assert_eq!(statuses, [PageDecodeStatus::CorruptLz4Stream]);

    assert_eq!(
        codec.verify_and_unpack_batch(&[ticket], &mut [], &mut statuses),
        Err(CodecError::InvalidBatchLength)
    );

    codec.invalidate(&ticket);
    codec
        .verify_and_unpack_batch(&[ticket], &mut unpacked, &mut statuses)
        .unwrap();
    assert_eq!(statuses, [PageDecodeStatus::InvalidTicket]);

    codec.set_force_deadline(true);
    assert_eq!(
        codec.verify_and_unpack_batch(&[ticket], &mut unpacked, &mut statuses),
        Err(CodecError::DeadlineExceeded)
    );
    assert_eq!(statuses, [PageDecodeStatus::SkippedOrIncompressible]);

    // Unbounded reference codec scales cleanly past the legacy 1024-frame cap
    // even when low-indexed frames are already occupied and allocation reaches frame 1024.
    let mut unbounded_pool = VramSlabPool::new(1025).unwrap();
    for _ in 0..(1024 * 16) {
        let _ = unbounded_pool.alloc_sub_slot(SlabClass::Raw4096).unwrap();
    }
    let mut unbounded_codec = ReferenceLz4Codec::new();
    unbounded_codec.set_deadline(Duration::from_secs(5));
    let mut scaled_tickets = [None; 2];
    unbounded_codec
        .compress_and_pack_batch(
            &[compressible_page(1), high_entropy_page()],
            &mut unbounded_pool,
            &mut scaled_tickets,
        )
        .unwrap();
    assert!(scaled_tickets.iter().flatten().any(|t| t.frame_idx == 1024));

    // Occupy frames 0..=1022 so a 2-page batch commits page 0 into frame 1023
    // (within max_frames=1024) and hits frame 1024 on page 1, rolling back cleanly.
    let mut max_pool = VramSlabPool::new(1025).unwrap();
    for _ in 0..(1023 * 16) {
        let _ = max_pool.alloc_sub_slot(SlabClass::Raw4096).unwrap();
    }
    let mut max_codec = ReferenceLz4Codec::with_max_frames(Some(1024));
    max_codec.set_deadline(Duration::from_secs(5));
    let mut rollback_tickets = [None; 2];
    let overflow_err = max_codec
        .compress_and_pack_batch(
            &[compressible_page(1), high_entropy_page()],
            &mut max_pool,
            &mut rollback_tickets,
        )
        .unwrap_err();
    assert_eq!(
        overflow_err,
        CodecError::BackendFailure("reference codec frame budget exceeded".to_string())
    );
    assert_eq!(rollback_tickets, [None, None]);
    assert_eq!(max_pool.logical_pages_cached(), 0);
}
