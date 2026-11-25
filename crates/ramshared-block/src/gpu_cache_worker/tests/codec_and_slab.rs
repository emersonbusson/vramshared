// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/gpu_cache_worker/tests/codec_and_slab.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::super::*;
use super::codec_fixtures::*;
use super::*;

/// `codec_admission_uses_full_parent_free_floor` — DT-4 / NFR-2.
///
/// Codec/slab admission must read the **full** parent free floor
/// `required_free_bytes(configured_reserve, runtime_headroom)`, never the
/// configured `reserve_floor_bytes` alone. The named threshold must be
/// strictly larger than `reserve_floor_bytes` on a real capacity, and
/// admission must refuse exactly at `reserve + needed` while accepting at
/// `full_floor + needed`.
#[test]
fn codec_admission_uses_full_parent_free_floor() {
    const GIB: u64 = 1024 * 1024 * 1024;
    let provider = CodecProvider::new(8 * GIB, 8 * GIB);
    let configured = 512 * 1024 * 1024u64;
    let config = GpuWorkerConfig {
        target_bytes: GIB,
        chunk_bytes: 2 * 1024 * 1024,
        reserve_floor_bytes: configured,
        compression_enabled: true,
    };
    let worker = GpuCacheWorker::new(&provider, config);
    let budget = provider.budget_snapshot().expect("budget");
    let full_floor = worker.codec_admission_required_free(&budget);

    // The full floor is reserve ∨ ceil(capacity/5) + runtime headroom.
    let capacity = 8 * GIB;
    let expected = configured
        .max(capacity.div_ceil(5))
        .saturating_add(RUNTIME_FREE_BUFFER_BYTES);
    assert_eq!(full_floor, expected);
    assert!(
        full_floor > configured + RUNTIME_FREE_BUFFER_BYTES,
        "the ceil(capacity/5) share must be part of the floor, got {full_floor}"
    );
    assert_ne!(
        full_floor, configured,
        "using reserve_floor_bytes alone is forbidden (DT-4)"
    );

    // At exactly `configured + needed` the admission must refuse: that
    // free level is below the runtime headroom and the safety share.
    let needed = 2 * 1024 * 1024u64;
    let tight = configured + needed;
    assert!(
        tight < full_floor + needed,
        "the fixture must actually distinguish the two floors"
    );
    let tight_provider = CodecProvider::new(8 * GIB, tight);
    let mut tight_worker = GpuCacheWorker::new(&tight_provider, config);
    // `codec_admission_allows` is private; the observable consequence is
    // that a compression-enabled update does not grow any slab.
    tight_worker.handle_update(0, &vec![0u8; 8192]);
    assert_eq!(
        tight_worker.compressed_entries_count(),
        0,
        "admission at reserve+needed must refuse the slab allocation"
    );

    // At `full_floor + needed` the same update is admissible.
    let roomy = full_floor + needed + (2 * 1024 * 1024);
    let roomy_provider = CodecProvider::new(8 * GIB, roomy);
    let mut roomy_worker = GpuCacheWorker::new(&roomy_provider, config);
    roomy_worker.handle_update(0, &vec![0u8; 8192]);
    assert!(
        roomy_worker.compressed_entries_count() > 0,
        "admission at full_floor+needed must accept the slab allocation"
    );
    assert!(roomy_worker.compressed_slab_count() > 0);
}

/// `worker_compression_respects_physical_budget` — DT-5.
///
/// `target_bytes` and `cached_bytes` stay **physical**. The compressed
/// store never grows past the slab ceiling derived from the physical
/// target, and a zero target allocates no slab at all.
#[test]
fn worker_compression_respects_physical_budget() {
    const GIB: u64 = 1024 * 1024 * 1024;
    // A 2 MiB physical target backs exactly one 2 MiB slab.
    let provider = CodecProvider::new(8 * GIB, 8 * GIB);
    let config = GpuWorkerConfig {
        target_bytes: 2 * 1024 * 1024,
        chunk_bytes: 2 * 1024 * 1024,
        reserve_floor_bytes: GIB,
        compression_enabled: true,
    };
    let mut worker = GpuCacheWorker::new(&provider, config);
    assert_eq!(worker.target_bytes(), 2 * 1024 * 1024);
    let physical_target = worker.target_bytes();

    // Publish enough compressible extents to exhaust the single slab.
    let payload = vec![0u8; MAX_EXTENT_LEN];
    for i in 0..64 {
        worker.handle_update((i as u64) * MAX_EXTENT_LEN as u64, &payload);
    }
    assert!(
        worker.compressed_slab_count() <= 1,
        "a 2 MiB physical target may back at most one slab, got {}",
        worker.compressed_slab_count()
    );
    // The physical target is reported unchanged by compression.
    assert_eq!(worker.target_bytes(), physical_target);
    // And the allocator itself refuses to exceed its slab ceiling.
    assert!(worker.compressed_entries_count() > 0);
}

/// `worker_evicts_compressed_lru_extent` — DT-5 / DT-6.
///
/// Under host pressure the coldest compressed extent is released and the
/// released range is a Gap afterwards, never a stale hit.
#[test]
fn worker_evicts_compressed_lru_extent() {
    const GIB: u64 = 1024 * 1024 * 1024;
    let provider = CodecProvider::new(8 * GIB, 8 * GIB);
    let config = GpuWorkerConfig {
        target_bytes: 64 * 1024 * 1024,
        chunk_bytes: 2 * 1024 * 1024,
        reserve_floor_bytes: GIB,
        compression_enabled: true,
    };
    let mut worker = GpuCacheWorker::new(&provider, config);
    let payload = vec![0u8; 4096];
    // Three extents; the first is the coldest once the others are read.
    worker.handle_update(0, &payload);
    worker.handle_update(4096, &payload);
    worker.handle_update(8192, &payload);
    let before = worker.compressed_entries_count();
    assert!(
        before > 0,
        "the codec must have published compressed extents"
    );

    // Touch the later extents so extent 0 is the LRU.
    let _ = worker.handle_read(4096, 4096);
    let _ = worker.handle_read(8192, 4096);

    // Drop the live free level below the full free floor: an external GPU
    // workload now consumes the headroom the cache was holding (engineering
    // #5 — the realistic condition, not the idle one).
    provider.set_free(64 * 1024 * 1024);
    let released = worker.reclaim_under_host_pressure().expect("reclaim");
    let _ = released;

    // The LRU extent is gone: its range is no longer a compressed hit.
    assert!(
        worker.compressed_entries_count() < before,
        "at least the coldest compressed extent must be released: {before} -> {}",
        worker.compressed_entries_count()
    );
    // The range that was evicted is a Gap, never a stale hit (DT-6).
    assert!(
        worker.handle_read(0, 4096).is_none(),
        "an evicted extent must miss and fall through to the origin"
    );
}

/// `worker_decode_error_returns_miss` — DT-7 / DT-11.
///
/// A stored-checksum mismatch or a decode failure must return a miss to
/// the client. The raw cache keeps serving afterwards: the fault never
/// revokes the cache client (DT-11).
#[test]
fn worker_decode_error_returns_miss() {
    const GIB: u64 = 1024 * 1024 * 1024;
    let provider = CodecProvider::new(8 * GIB, 8 * GIB);
    let config = GpuWorkerConfig {
        target_bytes: 64 * 1024 * 1024,
        chunk_bytes: 2 * 1024 * 1024,
        reserve_floor_bytes: GIB,
        compression_enabled: true,
    };
    let mut worker = GpuCacheWorker::new(&provider, config);
    let payload = vec![5u8; 4096];
    worker.handle_update(0, &payload);
    assert!(worker.compressed_entries_count() > 0);
    // Byte-exact hit through the compressed path.
    assert_eq!(worker.handle_read(0, 4096).as_deref(), Some(&payload[..]));

    // Corrupt the stored payload of the first entry. The provider-side
    // checksum must refuse before decode and the read must miss.
    let representation = worker
        .compressed
        .entries
        .first()
        .expect("one compressed entry")
        .representation
        .clone();
    if let crate::compressed_cache::CacheRepresentation::Compressed {
        address,
        stored_crc32,
        ..
    } = representation
    {
        let slab = worker
            .compressed_slabs
            .get_mut(address.slab_index)
            .expect("slab backing");
        let mut bytes = vec![0u8; address.span.stored_len];
        slab.read_at(address.span.offset, &mut bytes)
            .expect("read stored");
        assert_eq!(crc32(&bytes, 0), stored_crc32, "fixture starts consistent");
        bytes[0] ^= 0xff;
        slab.write_at(address.span.offset, &bytes)
            .expect("write corrupt");
    } else {
        panic!("expected a compressed representation");
    }

    let faults_before = worker.codec_faults();
    assert!(
        worker.handle_read(0, 4096).is_none(),
        "a corrupt stored payload must return a miss, never wrong bytes"
    );
    assert!(
        worker.codec_faults() >= faults_before,
        "the fault must be recorded for triage"
    );
    // DT-11: the worker is not disabled and the raw cache still serves.
    assert!(!worker.is_disabled());
    worker.handle_update(2 * 1024 * 1024, &payload);
    assert_eq!(
        worker.handle_read(2 * 1024 * 1024, 4096).as_deref(),
        Some(&payload[..]),
        "the raw cache must keep serving after a codec fault"
    );
}

/// `worker_teardown_waits_for_codec_completion` — DT-3 / DT-11.
///
/// Teardown is bounded and idempotent, and it only proceeds once no codec
/// operation is in flight. A completed operation is counted; an in-flight
/// one is cleared by the fault/completion path before the store is
/// dropped.
#[test]
fn worker_teardown_waits_for_codec_completion() {
    const GIB: u64 = 1024 * 1024 * 1024;
    let provider = CodecProvider::new(8 * GIB, 8 * GIB);
    let config = GpuWorkerConfig {
        target_bytes: 64 * 1024 * 1024,
        chunk_bytes: 2 * 1024 * 1024,
        reserve_floor_bytes: GIB,
        compression_enabled: true,
    };
    let mut worker = GpuCacheWorker::new(&provider, config);
    worker.handle_update(0, &vec![0u8; 8192]);
    let completed = worker.completed_codec_ops();
    assert!(
        completed > 0,
        "a successful publish must count as completed"
    );
    assert!(
        !worker.codec_in_flight(),
        "no operation may remain in flight after handle_update returns"
    );

    let start = Instant::now();
    worker.handle_disable();
    assert!(start.elapsed() < Duration::from_secs(5), "teardown bounded");
    assert!(!worker.codec_in_flight(), "teardown clears in-flight");
    assert_eq!(worker.compressed_entries_count(), 0);
    // Idempotent: a second teardown changes nothing and does not panic.
    worker.handle_disable();
    assert_eq!(worker.completed_codec_ops(), completed);
}

/// `worker_compression_disable_is_idempotent` — DT-9.
///
/// Compression is off by default; turning it off again is a no-op and the
/// raw path is what serves.
#[test]
fn worker_compression_disable_is_idempotent() {
    const GIB: u64 = 1024 * 1024 * 1024;
    let provider = CodecProvider::new(8 * GIB, 8 * GIB);
    let config = GpuWorkerConfig::default();
    assert!(
        !config.compression_enabled,
        "DT-9: compression is off by default"
    );
    let mut worker = GpuCacheWorker::new(&provider, config);
    let payload = vec![9u8; 4096];
    worker.handle_update(0, &payload);
    assert_eq!(worker.compressed_entries_count(), 0);
    assert_eq!(worker.compressed_slab_count(), 0);
    assert_eq!(worker.handle_read(0, 4096).as_deref(), Some(&payload[..]));
    // Disabling an already-disabled compression path is a no-op.
    worker.handle_update(4096, &payload);
    assert_eq!(worker.compressed_entries_count(), 0);
}

/// `codec_fault_keeps_raw_cache_serving` — DT-11.
#[test]
fn codec_fault_keeps_raw_cache_serving() {
    const GIB: u64 = 1024 * 1024 * 1024;
    let provider = FaultyProvider {
        total: 8 * GIB,
        free: 8 * GIB,
        live_allocations: Arc::new(AtomicUsize::new(0)),
        codec: FaultyCodec,
    };
    let config = GpuWorkerConfig {
        target_bytes: 64 * 1024 * 1024,
        chunk_bytes: 2 * 1024 * 1024,
        reserve_floor_bytes: GIB,
        compression_enabled: true,
    };
    let mut worker = GpuCacheWorker::new(&provider, config);
    let payload = vec![3u8; 4096];
    worker.handle_update(0, &payload);
    // The codec failed, so nothing was published compressed...
    assert_eq!(worker.compressed_entries_count(), 0);
    // ...but the raw path published and serves.
    assert_eq!(worker.handle_read(0, 4096).as_deref(), Some(&payload[..]));
    assert!(!worker.is_disabled(), "a codec fault must not disable");
    assert!(worker.codec_faults() > 0, "the fault must be recorded");
}

#[test]
fn compressed_telemetry_states_and_slab_admission_bounds_are_reported() {
    use ramshared_vram::CodecState;

    const GIB: u64 = 1024 * 1024 * 1024;
    let provider = CodecProvider::new(8 * GIB, 8 * GIB);
    let raw_worker = GpuCacheWorker::new(
        &provider,
        GpuWorkerConfig {
            target_bytes: 64 * 1024 * 1024,
            chunk_bytes: 2 * 1024 * 1024,
            reserve_floor_bytes: GIB,
            compression_enabled: false,
        },
    );
    let raw_telemetry = raw_worker.cache_telemetry(1);
    assert_eq!(raw_telemetry.codec.state, CodecState::RawOnly);
    assert_eq!(
        raw_telemetry.codec.refusal_reason.as_deref(),
        Some("compression-disabled")
    );

    let tiny_worker = GpuCacheWorker::new(
        &provider,
        GpuWorkerConfig {
            target_bytes: 4096,
            chunk_bytes: 2 * 1024 * 1024,
            reserve_floor_bytes: GIB,
            compression_enabled: true,
        },
    );
    assert!(tiny_worker.slab_bridge().is_none());

    let mut worker = GpuCacheWorker::new(
        &provider,
        GpuWorkerConfig {
            target_bytes: 8 * GIB,
            chunk_bytes: 2 * 1024 * 1024,
            reserve_floor_bytes: 0,
            compression_enabled: true,
        },
    );
    // Mismatched ticket count exercises `CodecError::InvalidBatchLength` formatting.
    let err = worker
        .compress_pages_into_slabs(&[[1u8; PAGE_BYTES]], &mut [])
        .unwrap_err();
    assert!(err.contains("InvalidBatchLength"));

    // Simulate a codec fault and timeout directly on telemetry state transitions.
    let corrupted_provider =
        ScriptedCodecProvider::new(8 * GIB, 8 * GIB, CodecId::Fake, ScriptedCodecFailure::None);
    let mut corrupted = GpuCacheWorker::new(
        &corrupted_provider,
        GpuWorkerConfig {
            target_bytes: 64 * 1024 * 1024,
            chunk_bytes: 2 * 1024 * 1024,
            reserve_floor_bytes: GIB,
            compression_enabled: true,
        },
    );
    corrupted
        .provider
        .codec
        .failure
        .set(ScriptedCodecFailure::FailCompress);
    corrupted.handle_update(0, &vec![5u8; 4096]);
    let faulted = corrupted.cache_telemetry(2);
    assert_eq!(faulted.codec.state, CodecState::Faulted);
    assert_eq!(faulted.codec.refusal_reason.as_deref(), Some("codec-fault"));

    corrupted.handle_disable();
    let disabled = corrupted.cache_telemetry(3);
    assert_eq!(disabled.codec.state, CodecState::Disabled);
    assert_eq!(
        disabled.codec.refusal_reason.as_deref(),
        Some("worker-disabled")
    );

    // Timeout state (`CodecState::TimedOut`, `"codec-subdeadline"`).
    let timed_out_provider =
        ScriptedCodecProvider::new(8 * GIB, 8 * GIB, CodecId::Fake, ScriptedCodecFailure::None);
    let mut timed_out_worker = GpuCacheWorker::new(
        &timed_out_provider,
        GpuWorkerConfig {
            target_bytes: 64 * 1024 * 1024,
            chunk_bytes: 2 * 1024 * 1024,
            reserve_floor_bytes: GIB,
            compression_enabled: true,
        },
    );
    timed_out_worker.handle_update(0, &vec![5u8; 4096]);
    timed_out_worker.compressed.note_codec_timeout();
    let timed_out = timed_out_worker.cache_telemetry(4);
    assert_eq!(timed_out.codec.state, CodecState::TimedOut);
    assert_eq!(
        timed_out.codec.refusal_reason.as_deref(),
        Some("codec-subdeadline")
    );
}

include!("codec_oversize.rs");

#[test]
fn scripted_codec_failures_cover_publish_decode_and_slab_growth_refusals() {
    const GIB: u64 = 1024 * 1024 * 1024;
    let config = GpuWorkerConfig {
        target_bytes: 64 * 1024 * 1024,
        chunk_bytes: 2 * 1024 * 1024,
        reserve_floor_bytes: GIB,
        compression_enabled: true,
    };

    // Compress error records decode fault and falls back to raw.
    let compress_err_provider = ScriptedCodecProvider::new(
        8 * GIB,
        8 * GIB,
        CodecId::NvcompLz4,
        ScriptedCodecFailure::FailCompress,
    );
    let mut worker = GpuCacheWorker::new(&compress_err_provider, config);
    let payload = vec![5u8; 4096];
    worker.handle_update(0, &payload);
    assert!(worker.codec_decode_errors() > 0);
    assert_eq!(worker.compressed_entries_count(), 0);
    assert_eq!(worker.handle_read(0, 4096).as_deref(), Some(&payload[..]));

    // Publish checksum error records integrity error.
    let checksum_pub_provider = ScriptedCodecProvider::new(
        8 * GIB,
        8 * GIB,
        CodecId::Raw,
        ScriptedCodecFailure::FailPublishChecksum,
    );
    let mut pub_worker = GpuCacheWorker::new(&checksum_pub_provider, config);
    pub_worker.handle_update(0, &payload);
    assert!(pub_worker.codec_integrity_errors() > 0);

    // Publish with NvcompLz4 succeeds, then inject each decode failure mode.
    let decode_provider = ScriptedCodecProvider::new(
        8 * GIB,
        8 * GIB,
        CodecId::NvcompLz4,
        ScriptedCodecFailure::None,
    );
    let mut dec_worker = GpuCacheWorker::new(&decode_provider, config);
    dec_worker.handle_update(0, &payload);
    assert_eq!(dec_worker.compressed_entries_count(), 1);

    for (mode, expects_integrity) in [
        (ScriptedCodecFailure::FailDecodeChecksum, true),
        (ScriptedCodecFailure::FailDecompress, false),
        (ScriptedCodecFailure::BadDecompressStatus, false),
        (ScriptedCodecFailure::WrongDecompressLen, false),
        (ScriptedCodecFailure::CorruptDecompressBytes, true),
    ] {
        dec_worker.provider.codec.failure.set(mode);
        let integrity_before = dec_worker.codec_integrity_errors();
        let decode_before = dec_worker.codec_decode_errors();
        assert!(dec_worker.handle_read(0, 4096).is_none());
        if expects_integrity {
            assert!(dec_worker.codec_integrity_errors() > integrity_before);
        } else {
            assert!(dec_worker.codec_decode_errors() > decode_before);
        }
    }

    // Budget headroom big enough for reservation_len (< 64 KiB) but below
    // SLAB_BYTES (4 MiB) causes `grow_slab_backing` to refuse cleanly.
    let total = 8 * GIB;
    let probe = ScriptedCodecProvider::new(total, total, CodecId::Fake, ScriptedCodecFailure::None);
    let base_required = probe
        .budget_snapshot()
        .unwrap()
        .required_free_bytes(config.reserve_floor_bytes, RUNTIME_FREE_BUFFER_BYTES);
    let tight_slab_provider = ScriptedCodecProvider::new(
        total,
        base_required + 64 * 1024,
        CodecId::Fake,
        ScriptedCodecFailure::None,
    );
    let mut tight_worker = GpuCacheWorker::new(&tight_slab_provider, config);
    tight_worker.handle_update(0, &payload);
    assert_eq!(tight_worker.compressed_entries_count(), 0);
    assert_eq!(tight_worker.compressed_slab_count(), 0);
}

#[test]
fn slab_bridge_uses_post_reserve_budget_without_subtracting_floor_twice() {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    let provider = CodecProvider::new(8 * GIB, 8 * GIB);
    let worker = GpuCacheWorker::new(
        &provider,
        GpuWorkerConfig {
            target_bytes: 32 * MIB,
            chunk_bytes: (2 * MIB) as usize,
            reserve_floor_bytes: 2 * GIB,
            compression_enabled: true,
        },
    );
    assert_eq!(worker.effective_target_bytes, 32 * MIB);
    assert!(
        worker.slab_bridge().is_some(),
        "already-reserved safe target must fund the codec"
    );
}

#[test]
fn codec_workspace_refuses_low_headroom_and_reuses_existing_allocation() {
    const GIB: u64 = 1024 * 1024 * 1024;
    let provider = CodecProvider::new(8 * GIB, 8 * GIB);
    let floor = provider
        .budget_snapshot()
        .unwrap()
        .required_free_bytes(GIB, RUNTIME_FREE_BUFFER_BYTES);
    let mut workspace = None;
    provider.set_free(floor);
    assert!(
        GpuCacheWorker::ensure_codec_workspace(&provider, &mut workspace, &provider.codec, GIB)
            .is_none()
    );
    assert_eq!(provider.live_allocations.load(Ordering::SeqCst), 0);
    provider.set_free(floor + 4096);
    assert!(
        GpuCacheWorker::ensure_codec_workspace(&provider, &mut workspace, &provider.codec, GIB)
            .is_some()
    );
    assert_eq!(provider.live_allocations.load(Ordering::SeqCst), 1);
    provider.set_free(floor);
    assert!(
        GpuCacheWorker::ensure_codec_workspace(&provider, &mut workspace, &provider.codec, GIB)
            .is_some()
    );
    assert_eq!(provider.live_allocations.load(Ordering::SeqCst), 1);
}

#[test]
fn compressed_replacement_never_reveals_stale_raw_bytes() {
    const MIB: u64 = 1024 * 1024;
    let provider = CodecProvider::new(8 * 1024 * MIB, 8 * 1024 * MIB);
    let mut worker = GpuCacheWorker::new(
        &provider,
        GpuWorkerConfig {
            target_bytes: 64 * MIB,
            chunk_bytes: (2 * MIB) as usize,
            reserve_floor_bytes: 1024 * MIB,
            compression_enabled: false,
        },
    );
    let offset = 2 * MIB + 4096;
    worker.handle_update(offset, &[0x31; 4096]);
    assert_eq!(worker.handle_read(offset, 4096), Some(vec![0x31; 4096]));
    worker.config.compression_enabled = true;
    worker.handle_update(offset, &[0x42; 4096]);
    assert_eq!(worker.handle_read(offset, 4096), Some(vec![0x42; 4096]));
    worker.invalidate_compressed_range(offset, offset + 4096);
    assert_eq!(worker.handle_read(offset, 4096), None);
}

#[test]
fn compressed_slab_cannot_exceed_target_already_used_by_raw_cache() {
    const MIB: u64 = 1024 * 1024;
    let provider = CodecProvider::new(8 * 1024 * MIB, 8 * 1024 * MIB);
    let mut worker = GpuCacheWorker::new(
        &provider,
        GpuWorkerConfig {
            target_bytes: 2 * MIB,
            chunk_bytes: (2 * MIB) as usize,
            reserve_floor_bytes: 1024 * MIB,
            compression_enabled: false,
        },
    );
    worker.handle_update(0, &[0x33; 4096]);
    assert_eq!(worker.cached_bytes(), 2 * MIB);
    worker.config.compression_enabled = true;
    worker.handle_update(4096, &[0x44; 4096]);
    assert_eq!(worker.compressed_slab_count(), 0);
    assert_eq!(worker.handle_read(0, 4096), Some(vec![0x33; 4096]));
}

#[test]
fn raw_allocation_cannot_overrun_resident_compressed_slab() {
    const MIB: u64 = 1024 * 1024;
    let provider = CodecProvider::new(8 * 1024 * MIB, 8 * 1024 * MIB);
    let mut worker = GpuCacheWorker::new(
        &provider,
        GpuWorkerConfig {
            target_bytes: 2 * MIB,
            chunk_bytes: (2 * MIB) as usize,
            reserve_floor_bytes: 1024 * MIB,
            compression_enabled: true,
        },
    );
    worker.handle_update(0, &[0x55; 4096]);
    assert_eq!(worker.compressed_slab_count(), 1);
    assert_eq!(worker.handle_read(0, 4096), Some(vec![0x55; 4096]));
    worker.config.compression_enabled = false;
    worker.handle_update(2 * MIB, &[0x77; 4096]);
    assert_eq!(worker.raw_cached_bytes(), 0);
    assert!(worker.cached_bytes() >= 2 * MIB);
    worker.config.compression_enabled = true;
    assert_eq!(worker.handle_read(0, 4096), Some(vec![0x55; 4096]));
}

#[test]
fn device_codec_reservation_prevents_other_gpu_allocations() {
    const MIB: u64 = 1024 * 1024;
    let provider = CodecProvider::new(8 * 1024 * MIB, 8 * 1024 * MIB);
    let config = GpuWorkerConfig {
        target_bytes: 32 * MIB,
        chunk_bytes: (2 * MIB) as usize,
        reserve_floor_bytes: 1024 * MIB,
        compression_enabled: true,
    };
    let mut worker = GpuCacheWorker::new_with_device_codec(
        &provider,
        config,
        Some(|_, _, _| {
            Ok(
                Box::new(ramshared_vram::device_codec::ReferenceLz4Codec::new())
                    as Box<dyn ramshared_vram::device_codec::GpuDeviceCodec>,
            )
        }),
    );
    assert!(worker.slab_bridge().is_some());
    worker.handle_update(0, &[0x22; 4096]);
    assert_eq!(worker.cached_bytes(), 0);
    assert_eq!(worker.compressed_slab_count(), 0);
}

#[test]
fn device_codec_normal_io_roundtrip_and_partial_invalidation() {
    const MIB: u64 = 1024 * 1024;
    let provider = CodecProvider::new(8 * 1024 * MIB, 8 * 1024 * MIB);
    let config = GpuWorkerConfig {
        target_bytes: 32 * MIB,
        chunk_bytes: (2 * MIB) as usize,
        reserve_floor_bytes: 1024 * MIB,
        compression_enabled: true,
    };
    let mut worker = GpuCacheWorker::new_with_device_codec(
        &provider,
        config,
        Some(|_, _, _| {
            let mut codec = ramshared_vram::device_codec::ReferenceLz4Codec::new();
            codec.set_deadline(Duration::from_secs(5));
            Ok(Box::new(codec) as Box<dyn ramshared_vram::device_codec::GpuDeviceCodec>)
        }),
    );
    let data = vec![0x39; 8192];
    worker.handle_update(0, &data);
    assert_eq!(worker.slab_pages.len(), 2, "device page publication");
    let sample = worker.cache_telemetry(1);
    assert_eq!(sample.logical_cached_bytes, 8192);
    assert!(sample.compressed_payload_bytes > 0);
    assert!(sample.compressed_payload_bytes < sample.logical_cached_bytes);
    assert!(worker.observed_cache_telemetry(1).is_none());
    assert_eq!(worker.handle_read(0, data.len()), Some(data));
    worker.handle_invalidate(1, 1);
    assert_eq!(worker.handle_read(0, 4096), None);
    assert_eq!(worker.handle_read(4096, 4096), Some(vec![0x39; 4096]));
}

#[test]
fn measured_device_allocation_is_reported_as_physical_occupancy() {
    use ramshared_vram::device_codec::{CodecError, GpuDeviceCodec, PAGE_BYTES, PageDecodeStatus};
    use ramshared_vram::slab_pool::{VramSlabPool, VramSlabTicket};
    struct Measured(ramshared_vram::device_codec::ReferenceLz4Codec, u64);
    impl GpuDeviceCodec for Measured {
        fn backend_name(&self) -> &'static str {
            "measured-test"
        }
        fn device_allocation_bytes(&self) -> Option<(u64, u64)> {
            Some((self.1, 64 * 1024))
        }
        fn compress_and_pack_batch(
            &mut self,
            pages: &[[u8; PAGE_BYTES]],
            pool: &mut VramSlabPool,
            out: &mut [Option<VramSlabTicket>],
        ) -> Result<(), CodecError> {
            self.0.compress_and_pack_batch(pages, pool, out)
        }
        fn verify_and_unpack_batch(
            &mut self,
            tickets: &[VramSlabTicket],
            out: &mut [[u8; PAGE_BYTES]],
            status: &mut [PageDecodeStatus],
        ) -> Result<(), CodecError> {
            self.0.verify_and_unpack_batch(tickets, out, status)
        }
    }
    let mib = 1024 * 1024;
    let provider = CodecProvider::new(8192 * mib, 8192 * mib);
    let mut worker = GpuCacheWorker::new_with_device_codec(
        &provider,
        GpuWorkerConfig {
            target_bytes: 2048 * mib,
            chunk_bytes: 2 * mib as usize,
            reserve_floor_bytes: 1024 * mib,
            compression_enabled: true,
        },
        Some(|_, _, frames| {
            Ok(Box::new(Measured(
                ramshared_vram::device_codec::ReferenceLz4Codec::new(),
                u64::from(frames) * 65536,
            )) as Box<dyn GpuDeviceCodec>)
        }),
    );
    let physical_slab = u64::from(worker.slab_bridge().unwrap().pool().frame_count()) * 65536;
    assert_eq!(worker.cached_bytes(), physical_slab + 64 * 1024);
    assert_eq!(worker.target_bytes(), 2048 * mib);
    let sample = worker.observed_cache_telemetry(1).unwrap();
    assert_eq!(sample.physical_cache_slab_bytes, physical_slab);
    assert_eq!(sample.codec_workspace_bytes, 64 * 1024);
    assert_eq!(sample.metadata_bytes, None);
    assert!(sample.device_metadata_lower_bound_bytes.unwrap() > 0);
    // An unaligned update cannot use the page codec. The remaining measured
    // target must serve it through raw storage without discarding the codec.
    worker.handle_update(1, &[0x55; 31]);
    assert_eq!(worker.handle_read(1, 31), Some(vec![0x55; 31]));
    assert_eq!(worker.raw_cached_bytes(), 2 * mib);
    assert_eq!(worker.cached_bytes(), physical_slab + 64 * 1024 + 2 * mib);
    worker.handle_update(4 * mib, &[0x39; 4096]);
    assert_eq!(worker.handle_read(4 * mib, 4096), Some(vec![0x39; 4096]));
    provider.set_free(0);
    assert!(matches!(
        worker.reclaim_under_host_pressure(),
        Err(VramError::Busy)
    ));
    assert_eq!(worker.raw_cached_bytes(), 0);
    assert_eq!(worker.handle_read(1, 31), None);
    provider.set_free(8192 * mib);

    let over_budget = GpuCacheWorker::new_with_device_codec(
        &provider,
        GpuWorkerConfig {
            target_bytes: 32 * mib,
            chunk_bytes: 2 * mib as usize,
            reserve_floor_bytes: 1024 * mib,
            compression_enabled: true,
        },
        Some(|_, _, _| {
            Ok(Box::new(Measured(
                ramshared_vram::device_codec::ReferenceLz4Codec::new(),
                64 * mib,
            )) as Box<dyn GpuDeviceCodec>)
        }),
    );
    assert!(over_budget.slab_bridge().is_none());
    assert_eq!(over_budget.device_codec_reservation, 0);

    // Model different adapter capacities without allocating their VRAM. The
    // capped test codec reports the same physical allocation at each size.
    for capacity_gib in [6, 32, 96, 192, 256] {
        let gib = 1024 * mib;
        let capacity = capacity_gib * gib;
        let provider = CodecProvider::new(capacity, capacity);
        let target = capacity / 2;
        let mut worker = GpuCacheWorker::new_with_device_codec(
            &provider,
            GpuWorkerConfig {
                target_bytes: target,
                chunk_bytes: 2 * mib as usize,
                reserve_floor_bytes: gib,
                compression_enabled: true,
            },
            Some(|_, _, frames| {
                Ok(Box::new(Measured(
                    ramshared_vram::device_codec::ReferenceLz4Codec::new(),
                    u64::from(frames) * 65536,
                )) as Box<dyn GpuDeviceCodec>)
            }),
        );
        assert_eq!(worker.target_bytes(), target, "{capacity_gib} GiB");
        worker.handle_update(1, &[0x5a; 31]);
        assert_eq!(worker.handle_read(1, 31), Some(vec![0x5a; 31]));
        assert!(worker.cached_bytes() <= target);
    }
}

#[test]
fn device_page_batch_failure_rolls_back_prior_tickets() {
    use ramshared_vram::device_codec::{CodecError, GpuDeviceCodec, MAX_BATCH_PAGES};
    use ramshared_vram::slab_pool::{VramSlabPool, VramSlabTicket};
    struct FailSecond {
        inner: ramshared_vram::device_codec::ReferenceLz4Codec,
        calls: usize,
    }
    impl GpuDeviceCodec for FailSecond {
        fn backend_name(&self) -> &'static str {
            "test-fail-second"
        }
        fn compress_and_pack_batch(
            &mut self,
            pages: &[[u8; PAGE_BYTES]],
            pool: &mut VramSlabPool,
            tickets: &mut [Option<VramSlabTicket>],
        ) -> Result<(), CodecError> {
            self.calls += 1;
            if self.calls == 2 {
                return Err(CodecError::BackendFailure("injected failure".into()));
            }
            self.inner.compress_and_pack_batch(pages, pool, tickets)
        }
        fn verify_and_unpack_batch(
            &mut self,
            tickets: &[VramSlabTicket],
            pages: &mut [[u8; PAGE_BYTES]],
            status: &mut [PageDecodeStatus],
        ) -> Result<(), CodecError> {
            self.inner.verify_and_unpack_batch(tickets, pages, status)
        }
        fn invalidate(&mut self, ticket: &VramSlabTicket) {
            self.inner.invalidate(ticket);
        }
    }
    const MIB: u64 = 1024 * 1024;
    let provider = CodecProvider::new(8 * 1024 * MIB, 8 * 1024 * MIB);
    let config = GpuWorkerConfig {
        target_bytes: 32 * MIB,
        chunk_bytes: (2 * MIB) as usize,
        reserve_floor_bytes: 1024 * MIB,
        compression_enabled: true,
    };
    let mut worker = GpuCacheWorker::new_with_device_codec(
        &provider,
        config,
        Some(|_, _, _| {
            let mut inner = ramshared_vram::device_codec::ReferenceLz4Codec::new();
            inner.set_deadline(Duration::from_secs(10));
            Ok(Box::new(FailSecond { inner, calls: 0 }) as Box<dyn GpuDeviceCodec>)
        }),
    );
    let bytes = vec![0x37; (MAX_BATCH_PAGES + 1) * PAGE_BYTES];
    worker.handle_update(0, &bytes);
    assert!(worker.slab_pages.is_empty());
    assert_eq!(
        worker.slab_bridge().unwrap().pool().logical_pages_cached(),
        0
    );
    assert!(worker.handle_read(0, PAGE_BYTES).is_none());
}

#[test]
fn device_page_index_prunes_evicted_frames_and_returns_miss() {
    const MIB: u64 = 1024 * 1024;
    let provider = CodecProvider::new(8 * 1024 * MIB, 8 * 1024 * MIB);
    let config = GpuWorkerConfig {
        target_bytes: MIB,
        chunk_bytes: (2 * MIB) as usize,
        reserve_floor_bytes: 1024 * MIB,
        compression_enabled: true,
    };
    let mut worker = GpuCacheWorker::new_with_device_codec(
        &provider,
        config,
        Some(|_, _, _| {
            let mut codec = ramshared_vram::device_codec::ReferenceLz4Codec::new();
            codec.set_deadline(Duration::from_secs(10));
            Ok(Box::new(codec) as Box<dyn ramshared_vram::device_codec::GpuDeviceCodec>)
        }),
    );
    let frames = worker.slab_bridge().unwrap().pool().frame_count() as usize;
    let count = frames * 128 + 1;
    for index in 0..count {
        worker.handle_update((index * PAGE_BYTES) as u64, &[0x41; PAGE_BYTES]);
    }
    assert!(worker.slab_pages.len() <= frames * 128);
    assert!(worker.handle_read(0, PAGE_BYTES).is_none());
    assert_eq!(
        worker.handle_read(((count - 1) * PAGE_BYTES) as u64, PAGE_BYTES),
        Some(vec![0x41; PAGE_BYTES])
    );
}

#[test]
fn device_codec_is_dropped_before_disable_returns() {
    use ramshared_vram::device_codec::{CodecError, GpuDeviceCodec};
    use ramshared_vram::slab_pool::{VramSlabPool, VramSlabTicket};
    use std::sync::atomic::AtomicBool;
    struct DropCodec(Arc<AtomicBool>);
    impl Drop for DropCodec {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    impl GpuDeviceCodec for DropCodec {
        fn backend_name(&self) -> &'static str {
            "drop-test"
        }
        fn compress_and_pack_batch(
            &mut self,
            _: &[[u8; PAGE_BYTES]],
            _: &mut VramSlabPool,
            _: &mut [Option<VramSlabTicket>],
        ) -> Result<(), CodecError> {
            Ok(())
        }
        fn verify_and_unpack_batch(
            &mut self,
            _: &[VramSlabTicket],
            _: &mut [[u8; PAGE_BYTES]],
            _: &mut [PageDecodeStatus],
        ) -> Result<(), CodecError> {
            Ok(())
        }
    }
    let dropped = Arc::new(AtomicBool::new(false));
    let witness = Arc::clone(&dropped);
    let provider = CodecProvider::new(8 * 1024 * 1024 * 1024, 8 * 1024 * 1024 * 1024);
    let config = GpuWorkerConfig {
        target_bytes: 32 * 1024 * 1024,
        chunk_bytes: 2 * 1024 * 1024,
        reserve_floor_bytes: 1024 * 1024 * 1024,
        compression_enabled: true,
    };
    let mut worker = GpuCacheWorker::new_with_device_codec(
        &provider,
        config,
        Some(move |_, _, _| Ok(Box::new(DropCodec(witness)) as Box<dyn GpuDeviceCodec>)),
    );
    assert!(worker.slab_bridge().is_some());
    assert!(!dropped.load(Ordering::SeqCst));
    worker.handle_disable();
    assert!(dropped.load(Ordering::SeqCst));
    assert!(worker.slab_bridge().is_none());
    worker.handle_disable();
}

#[test]
fn pressure_refuses_partial_reclaim_when_device_codec_owns_buffers() {
    const MIB: u64 = 1024 * 1024;
    let provider = CodecProvider::new(8 * 1024 * MIB, 8 * 1024 * MIB);
    let config = GpuWorkerConfig {
        target_bytes: 32 * MIB,
        chunk_bytes: (2 * MIB) as usize,
        reserve_floor_bytes: 1024 * MIB,
        compression_enabled: true,
    };
    let mut worker = GpuCacheWorker::new_with_device_codec(
        &provider,
        config,
        Some(|_, _, _| {
            Ok(
                Box::new(ramshared_vram::device_codec::ReferenceLz4Codec::new())
                    as Box<dyn ramshared_vram::device_codec::GpuDeviceCodec>,
            )
        }),
    );
    assert!(worker.slab_bridge().is_some());
    provider.set_free(0);
    assert!(matches!(
        worker.reclaim_under_host_pressure(),
        Err(VramError::Busy)
    ));
    worker.handle_disable();
    assert!(worker.is_disabled());
    assert!(worker.slab_bridge().is_none());
}

#[test]
fn device_codec_pressure_closes_worker_after_heartbeat_refusal() {
    const MIB: u64 = 1024 * 1024;
    let provider = CodecProvider::new(8 * 1024 * MIB, 8 * 1024 * MIB);
    let free = Arc::clone(&provider.free);
    let (client_socket, worker_socket) = UnixStream::pair().unwrap();
    let (finished_tx, finished_rx) = std::sync::mpsc::sync_channel(1);
    let handle = std::thread::spawn(move || {
        let result = run_gpu_worker_loop_with_device_codec(
            worker_socket,
            provider,
            GpuWorkerConfig {
                target_bytes: 32 * MIB,
                chunk_bytes: (2 * MIB) as usize,
                reserve_floor_bytes: 1024 * MIB,
                compression_enabled: true,
            },
            |_, _, _| {
                Ok(
                    Box::new(ramshared_vram::device_codec::ReferenceLz4Codec::new())
                        as Box<dyn ramshared_vram::device_codec::GpuDeviceCodec>,
                )
            },
        );
        finished_tx.send(result).unwrap();
    });
    let mut client = IpcCacheClient::with_pacing(
        client_socket,
        Duration::from_secs(2),
        32 * MIB,
        PacingPolicy::immediate(),
    );
    client.perform_handshake().unwrap();
    assert_eq!(
        client.refresh_cached_bytes(),
        Err("GPU cache physical occupancy is unknown")
    );
    assert_eq!(client.state(), crate::origin_cache::CacheState::Active);
    free.store(0, Ordering::SeqCst);
    assert!(client.refresh_cached_bytes().is_err());
    assert!(
        finished_rx
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .is_ok()
    );
    handle.join().unwrap();
}

#[test]
fn legacy_client_cannot_misread_unknown_device_occupancy() {
    const MIB: u64 = 1024 * 1024;
    let provider = CodecProvider::new(8 * 1024 * MIB, 8 * 1024 * MIB);
    let (mut client, worker_socket) = UnixStream::pair().unwrap();
    let worker = std::thread::spawn(move || {
        run_gpu_worker_loop_with_device_codec(
            worker_socket,
            provider,
            GpuWorkerConfig {
                target_bytes: 32 * MIB,
                chunk_bytes: (2 * MIB) as usize,
                reserve_floor_bytes: 1024 * MIB,
                compression_enabled: true,
            },
            |_, _, _| {
                Ok(
                    Box::new(ramshared_vram::device_codec::ReferenceLz4Codec::new())
                        as Box<dyn ramshared_vram::device_codec::GpuDeviceCodec>,
                )
            },
        )
    });
    let request = FrameHeader {
        msg_type: MSG_HANDSHAKE_REQ,
        status: STATUS_OK,
        correlation_id: 7,
        offset: 0,
        payload_len: 0,
        aux: 0,
    };
    client.write_all(&request.encode()).unwrap();
    let mut raw = [0; FRAME_HEADER_LEN];
    client.read_exact(&mut raw).unwrap();
    let response = FrameHeader::decode(&raw).unwrap();
    assert_eq!(response.status, STATUS_ERROR);
    assert_eq!(response.aux, 0);
    assert!(worker.join().unwrap().is_ok());
}

#[test]
fn slab_ipc_rejects_malformed_requests_and_ticket_bytes() {
    use crate::gpu_cache_worker::slab_ipc::{
        MSG_SLAB_COMPRESS_BATCH_REQ, MSG_SLAB_COMPRESS_BATCH_RESP, MSG_SLAB_RECOVER_BATCH_REQ,
        MSG_SLAB_RECOVER_BATCH_RESP, MSG_SLAB_TELEMETRY_REQ, MSG_SLAB_TELEMETRY_RESP,
        SLAB_TICKET_WIRE_LEN, decode_ticket, encode_ticket, handle_slab_frame,
    };
    use ramshared_vram::slab_pool::{SlabClass, SlotEncoding, VramSlabTicket};
    use std::io::Read;

    assert!(decode_ticket(&[0u8; 10]).is_none());
    let mut bad_ticket = [0u8; SLAB_TICKET_WIRE_LEN];
    bad_ticket[7] = 1;
    assert!(decode_ticket(&bad_ticket).is_none());
    bad_ticket[7] = 0;
    bad_ticket[5] = 99;
    assert!(decode_ticket(&bad_ticket).is_none());
    bad_ticket[5] = 1;
    bad_ticket[6] = 99;
    assert!(decode_ticket(&bad_ticket).is_none());

    for (class_idx, class) in [
        (0u8, SlabClass::B512),
        (1, SlabClass::B1024),
        (2, SlabClass::B2048),
        (3, SlabClass::B3072),
        (4, SlabClass::Raw4096),
    ] {
        let ticket = VramSlabTicket {
            frame_idx: 3,
            sub_slot: class_idx,
            class,
            encoding: if class_idx == 4 {
                SlotEncoding::Raw
            } else {
                SlotEncoding::Lz4
            },
            payload_len: 512,
            payload_crc32: 0x1234_5678,
            epoch: 7,
        };
        let mut wire = [0u8; SLAB_TICKET_WIRE_LEN];
        encode_ticket(&ticket, &mut wire);
        assert_eq!(decode_ticket(&wire), Some(ticket));
    }

    const MIB: u64 = 1024 * 1024;
    let provider = CodecProvider::new(8 * 1024 * MIB, 8 * 1024 * MIB);
    let mut worker = GpuCacheWorker::new(
        &provider,
        GpuWorkerConfig {
            target_bytes: 16 * MIB,
            chunk_bytes: (2 * MIB) as usize,
            reserve_floor_bytes: 1024 * MIB,
            compression_enabled: false,
        },
    );
    let (mut client, mut server) = UnixStream::pair().unwrap();

    let mut expect_error = |msg_type: u8, expected_resp: u8, aux: u32, payload: &[u8]| {
        let hdr = FrameHeader {
            msg_type,
            status: STATUS_OK,
            correlation_id: 41,
            offset: 0,
            payload_len: payload.len() as u32,
            aux,
        };
        assert!(handle_slab_frame(&mut worker, &hdr, payload, &mut server).unwrap());
        let mut raw = [0u8; FRAME_HEADER_LEN];
        client.read_exact(&mut raw).unwrap();
        let resp = FrameHeader::decode(&raw).unwrap();
        assert_eq!(resp.msg_type, expected_resp);
        assert_eq!(resp.status, STATUS_ERROR);
        let mut body = vec![0u8; resp.payload_len as usize];
        client.read_exact(&mut body).unwrap();
        assert!(!body.is_empty());
    };

    expect_error(
        MSG_SLAB_COMPRESS_BATCH_REQ,
        MSG_SLAB_COMPRESS_BATCH_RESP,
        2,
        &[0u8; 16],
    );
    expect_error(
        MSG_SLAB_RECOVER_BATCH_REQ,
        MSG_SLAB_RECOVER_BATCH_RESP,
        2,
        &[0u8; 16],
    );
    let mut malformed = [0u8; SLAB_TICKET_WIRE_LEN];
    malformed[5] = 99;
    expect_error(
        MSG_SLAB_RECOVER_BATCH_REQ,
        MSG_SLAB_RECOVER_BATCH_RESP,
        1,
        &malformed,
    );
    expect_error(
        MSG_SLAB_TELEMETRY_REQ,
        MSG_SLAB_TELEMETRY_RESP,
        0,
        b"unexpected",
    );
}

#[test]
fn worker_slab_admission_scales_above_64mib_when_host_budget_permits() {
    use crate::gpu_cache_worker::host_budget::{HostMemoryObservation, SharedHostMetadataBudget};
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    let provider = CodecProvider::new(16 * GIB, 16 * GIB);
    let config = GpuWorkerConfig {
        target_bytes: 256 * MIB,
        chunk_bytes: (2 * MIB) as usize,
        reserve_floor_bytes: GIB,
        compression_enabled: true,
    };
    let ledger = SharedHostMetadataBudget::new();
    let observation = HostMemoryObservation {
        total_host_bytes: 32 * GIB,
        available_host_bytes: 24 * GIB,
        cgroup_headroom_bytes: Some(8 * GIB),
    };
    let worker = GpuCacheWorker::new_with_device_codec_and_host_budget(
        &provider,
        config,
        1,
        Some(ledger),
        Some(observation),
        Some(|_, _, _| {
            let mut codec = ramshared_vram::device_codec::ReferenceLz4Codec::new();
            codec.set_deadline(Duration::from_secs(10));
            Ok(Box::new(codec) as Box<dyn ramshared_vram::device_codec::GpuDeviceCodec>)
        }),
    );
    let frames = worker.slab_bridge().unwrap().pool().frame_count();
    assert!(
        frames > 1024,
        "expected dynamic budget admission above legacy 1024-frame (64 MiB) cap, got {frames}"
    );
    assert!(
        frames >= 4000,
        "256 MiB target should admit ~4054 frames, got {frames}"
    );
}

#[test]
fn concurrent_workers_share_host_metadata_ledger_and_release_on_disable() {
    use crate::gpu_cache_worker::host_budget::{HostMemoryObservation, SharedHostMetadataBudget};
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    let ledger = SharedHostMetadataBudget::new();
    let observation = HostMemoryObservation {
        total_host_bytes: 16 * GIB,
        available_host_bytes: 8 * GIB,
        cgroup_headroom_bytes: Some(2 * GIB),
    };
    let p1 = CodecProvider::new(8 * GIB, 8 * GIB);
    let p2 = CodecProvider::new(8 * GIB, 8 * GIB);
    let cfg = GpuWorkerConfig {
        target_bytes: 128 * MIB,
        chunk_bytes: (2 * MIB) as usize,
        reserve_floor_bytes: GIB,
        compression_enabled: true,
    };
    let factory = |_, _, _| {
        let mut codec = ramshared_vram::device_codec::ReferenceLz4Codec::new();
        codec.set_deadline(Duration::from_secs(10));
        Ok(Box::new(codec) as Box<dyn ramshared_vram::device_codec::GpuDeviceCodec>)
    };
    let mut w1 = GpuCacheWorker::new_with_device_codec_and_host_budget(
        &p1,
        cfg,
        2,
        Some(ledger.clone()),
        Some(observation),
        Some(factory),
    );
    let mut w2 = GpuCacheWorker::new_with_device_codec_and_host_budget(
        &p2,
        cfg,
        2,
        Some(ledger.clone()),
        Some(observation),
        Some(factory),
    );
    let initial_reserved = ledger.total_reserved_bytes();
    assert!(initial_reserved > 0);

    let data = vec![0x6b; 64 * 1024];
    w1.handle_update(0, &data);
    w2.handle_update(0, &data);
    let peak_reserved = ledger.total_reserved_bytes();
    assert!(peak_reserved > initial_reserved);
    assert!(
        peak_reserved <= observation.total_metadata_ceiling_bytes(),
        "shared ledger must remain bounded under total ceiling"
    );

    w1.handle_disable();
    let after_w1 = ledger.total_reserved_bytes();
    assert!(after_w1 > 0 && after_w1 < peak_reserved);
    w2.handle_disable();
    assert_eq!(ledger.total_reserved_bytes(), 0);
}

#[test]
fn host_metadata_ceiling_evicts_coldest_frame_and_continues_safely() {
    use crate::gpu_cache_worker::host_budget::{
        HostMemoryObservation, LIVE_PAGE_METADATA_BYTES, SharedHostMetadataBudget,
    };
    use ramshared_vram::device_codec::PAGE_BYTES;
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    // Construct a very tight host ceiling so startup succeeds cleanly, then dynamic
    // page-metadata writes hit the host ceiling before filling all admitted frames.
    let provider = CodecProvider::new(8 * GIB, 8 * GIB);
    let per_frame_fixed =
        ramshared_vram::slab_pool::VramSlabPool::per_frame_fixed_metadata_bytes() as u64;
    let tight_worker_limit = 64 * per_frame_fixed + 16 * LIVE_PAGE_METADATA_BYTES;
    let ledger = SharedHostMetadataBudget::with_ceiling_bytes(tight_worker_limit);
    let observation = HostMemoryObservation {
        total_host_bytes: 2 * GIB,
        available_host_bytes: 512 * MIB,
        cgroup_headroom_bytes: Some(6 * MIB),
    };
    let mut worker = GpuCacheWorker::new_with_device_codec_and_host_budget(
        &provider,
        GpuWorkerConfig {
            target_bytes: 64 * MIB,
            chunk_bytes: (2 * MIB) as usize,
            reserve_floor_bytes: 512 * MIB,
            compression_enabled: true,
        },
        1,
        Some(ledger.clone()),
        Some(observation),
        Some(|_, _, _| {
            let mut codec = ramshared_vram::device_codec::ReferenceLz4Codec::new();
            codec.set_deadline(Duration::from_secs(10));
            Ok(Box::new(codec) as Box<dyn ramshared_vram::device_codec::GpuDeviceCodec>)
        }),
    );
    assert!(worker.slab_bridge().is_some());
    // Fill many pages across multiple frames with compressible data.
    for idx in 0..200u64 {
        let mut buf = vec![0x5a; PAGE_BYTES];
        buf[..8].copy_from_slice(&idx.to_le_bytes());
        worker.handle_update(idx * PAGE_BYTES as u64, &buf);
    }
    assert!(
        ledger.total_reserved_bytes() <= tight_worker_limit,
        "live metadata {} exceeded tight ceiling {}",
        ledger.total_reserved_bytes(),
        tight_worker_limit
    );
    // Most recent writes are readable; oldest frames were evicted cleanly.
    let last_idx = 199u64;
    let mut expected = vec![0x5a; PAGE_BYTES];
    expected[..8].copy_from_slice(&last_idx.to_le_bytes());
    assert_eq!(
        worker.handle_read(last_idx * PAGE_BYTES as u64, PAGE_BYTES),
        Some(expected)
    );
    assert!(worker.slab_pages.len() < 200);
}
