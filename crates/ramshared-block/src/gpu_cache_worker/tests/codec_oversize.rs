// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/gpu_cache_worker/tests/codec_oversize.rs
// The parent test module supplies the worker fixtures and imports.
#[test]
fn compressed_oversize_update_splits_and_raw_compressed_entry_roundtrips_with_crc() {
    use crate::compressed_cache::{CacheEntry, CacheRepresentation, SlabSpan};
    use ramshared_vram::VramSpan;

    const GIB: u64 = 1024 * 1024 * 1024;
    let provider = CodecProvider::new(8 * GIB, 8 * GIB);
    let mut worker = GpuCacheWorker::new(
        &provider,
        GpuWorkerConfig {
            target_bytes: 128 * 1024 * 1024,
            chunk_bytes: 2 * 1024 * 1024,
            reserve_floor_bytes: GIB,
            compression_enabled: true,
        },
    );

    // Oversized update splits cleanly at MAX_EXTENT_LEN and round-trips.
    let oversized = vec![7u8; MAX_EXTENT_LEN + 4096];
    worker.handle_update(0, &oversized);
    assert!(worker.compressed_entries_count() >= 2);
    // A real scheduling pause can exhaust the production codec deadline even
    // with this in-memory fixture. A timeout is an allowed cache miss, not a
    // decoding error; retry only that classified outcome within a firm bound.
    let started = Instant::now();
    let timeouts_before = worker.codec_timeouts();
    let deadline = started + std::time::Duration::from_secs(2);
    let restored = loop {
        let before = worker.codec_timeouts();
        if let Some(bytes) = worker.handle_read(0, oversized.len()) {
            break bytes;
        }
        assert!(worker.codec_timeouts() > before, "unclassified cache miss");
        assert_eq!(worker.codec_integrity_errors(), 0);
        assert_eq!(worker.codec_decode_errors(), 0);
        assert!(
            Instant::now() < deadline,
            "codec never completed before retry deadline"
        );
        std::thread::yield_now();
    };
    assert!(
        restored == oversized,
        "decoded bytes differ from the original"
    );
    eprintln!(
        "COMPRESSED_EXTENT_READ elapsed_us={} timeouts={} bytes={}",
        started.elapsed().as_micros(),
        worker.codec_timeouts().saturating_sub(timeouts_before),
        restored.len()
    );

    // Exercise a Raw-represented extent inside `CompressedStore` for read,
    // telemetry, CRC refusal, and invalidate release.
    let raw_bytes = vec![0x42u8; 4096];
    let raw_addr = worker.compressed.allocator.alloc(raw_bytes.len()).unwrap();
    while worker.compressed_slabs.len() <= raw_addr.slab_index {
        let slab = provider.alloc(crate::compressed_cache::SLAB_BYTES).unwrap();
        worker.compressed_slabs.push(slab);
    }
    let valid_span = VramSpan::new(
        raw_addr.span.offset,
        raw_bytes.len(),
        raw_addr.span.allocation_len,
    )
    .unwrap();
    worker.compressed_slabs[raw_addr.slab_index]
        .write_at(valid_span.offset, &raw_bytes)
        .unwrap();
    let base_offset = 64 * 1024 * 1024u64;
    worker.compressed.entries.push(CacheEntry {
        logical_start: base_offset,
        logical_len: raw_bytes.len() as u64,
        original_crc32: crc32(&raw_bytes, 0),
        generation: 99,
        last_accessed: Instant::now(),
        representation: CacheRepresentation::Raw {
            address: SlabSpan {
                slab_index: raw_addr.slab_index,
                span: valid_span,
            },
        },
    });
    assert!(worker.cache_telemetry(10).raw_payload_bytes >= raw_bytes.len() as u64);
    assert_eq!(
        worker.handle_read(base_offset, raw_bytes.len()).as_deref(),
        Some(&raw_bytes[..])
    );

    // Corrupting the backing slab triggers CRC integrity refusal on Raw read.
    worker.compressed_slabs[raw_addr.slab_index]
        .write_at(valid_span.offset, &[0x99u8; 4096])
        .unwrap();
    let integrity_before = worker.codec_integrity_errors();
    assert!(worker.handle_read(base_offset, raw_bytes.len()).is_none());
    assert!(worker.codec_integrity_errors() > integrity_before);

    // Missing allocator slab for Raw entry triggers decode error.
    worker.compressed.entries.push(CacheEntry {
        logical_start: base_offset + 4096,
        logical_len: 4096,
        original_crc32: 0,
        generation: 100,
        last_accessed: Instant::now(),
        representation: CacheRepresentation::Raw {
            address: SlabSpan {
                slab_index: 999,
                span: valid_span,
            },
        },
    });
    let decode_before = worker.codec_decode_errors();
    assert!(worker.handle_read(base_offset + 4096, 4096).is_none());
    assert!(worker.codec_decode_errors() > decode_before);

    worker.handle_invalidate(base_offset, 8192);
}

#[test]
fn compressed_read_refuses_result_completed_after_codec_deadline() {
    const GIB: u64 = 1024 * 1024 * 1024;
    let provider =
        ScriptedCodecProvider::new(8 * GIB, 8 * GIB, CodecId::Fake, ScriptedCodecFailure::None);
    let mut worker = GpuCacheWorker::new(
        &provider,
        GpuWorkerConfig {
            target_bytes: 64 * 1024 * 1024,
            chunk_bytes: 2 * 1024 * 1024,
            reserve_floor_bytes: GIB,
            compression_enabled: true,
        },
    );
    let payload = vec![5; 4096];
    worker.handle_update(0, &payload);
    assert!(worker.compressed_entries_count() > 0);
    provider
        .codec
        .failure
        .set(ScriptedCodecFailure::DelayDecompress);
    assert!(worker.handle_read(0, payload.len()).is_none());
    assert_eq!(worker.codec_timeouts(), 1);
    assert_eq!(worker.codec_integrity_errors(), 0);
    assert_eq!(worker.codec_decode_errors(), 0);
    let telemetry = worker.cache_telemetry(1);
    assert_eq!(telemetry.codec.state, ramshared_vram::CodecState::TimedOut);
    assert_eq!(
        telemetry.codec.refusal_reason.as_deref(),
        Some("codec-subdeadline")
    );
}
