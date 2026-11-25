// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/gpu_cache_worker/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "tests/codec_and_slab.rs"]
mod codec_and_slab;
#[path = "tests/codec_fixtures.rs"]
mod codec_fixtures;
#[path = "tests/device_eviction.rs"]
mod device_eviction;
#[path = "tests/device_integrity.rs"]
mod device_integrity;
#[path = "tests/device_timeout.rs"]
mod device_timeout;
#[path = "tests/raw_and_pressure.rs"]
mod raw_and_pressure;
#[path = "tests/raw_fixtures.rs"]
mod raw_fixtures;

use super::*;

pub(super) use crate::compressed_cache::MAX_EXTENT_LEN;
pub(super) use crate::ipc_cache_client::{IpcCacheClient, PacingPolicy};
pub(super) use crate::isolated_origin::{
    AuthoritativeOriginBackend, BestEffortCache, CacheMutation, CacheRead, mutation_frames,
};
pub(super) use crate::origin_cache::{CacheState, OriginStorage};
pub(super) use crate::request::IoError;
use codec_fixtures::*;
pub(super) use ramshared_vram::device_codec::{PAGE_BYTES, PageDecodeStatus};
pub(super) use ramshared_vram::{CodecId, CodecStatus, GpuAdapterIdentity, GpuBudgetSource, crc32};
pub(super) use ramshared_vram::{FakeCodec, GpuCacheCodec};
use raw_fixtures::*;

#[test]
fn sustained_writes_leave_read_windows_without_serving_stale_pages() {
    let (client_sock, worker_sock) = UnixStream::pair().unwrap();
    let provider = FakeProvider::new(8 * 1024 * 1024 * 1024, 6 * 1024 * 1024 * 1024);
    let config = GpuWorkerConfig {
        target_bytes: 64 * 1024 * 1024,
        chunk_bytes: 2 * 1024 * 1024,
        reserve_floor_bytes: 1536 * 1024 * 1024,
        compression_enabled: false,
    };
    let worker =
        std::thread::spawn(move || run_gpu_worker_loop(worker_sock, provider, config).unwrap());
    let mut client = IpcCacheClient::new(client_sock, Duration::from_millis(100), 64 * 1024 * 1024);
    client.perform_handshake().unwrap();
    assert_eq!(client.update(0, &[1; 4096]), CacheMutation::Accepted);
    // This write replaces data that may already be cached. Its ordered
    // invalidation must prevent a stale hit during the read window.
    assert_eq!(client.update(0, &[2; 4096]), CacheMutation::Skipped);
    std::thread::sleep(crate::ipc_cache_client::READ_DRAIN_GRACE + Duration::from_millis(15));
    let mut page = [0u8; 4096];
    assert_eq!(client.read(0, &mut page), CacheRead::Miss);
    assert_eq!(client.promote(0, &[2; 4096]), CacheMutation::Accepted);
    let mut hits = 0;
    let mut skipped_mirrors = 0;
    for index in 0..200 {
        match client.update(8192 + index * 4096, &[3; 4096]) {
            CacheMutation::Skipped => skipped_mirrors += 1,
            CacheMutation::Accepted => {}
            CacheMutation::Failed => panic!("worker failed during sustained writes"),
        }
        match client.read(0, &mut page) {
            CacheRead::Hit => {
                hits += 1;
                assert_eq!(page, [2; 4096]);
            }
            CacheRead::Miss => {}
            CacheRead::Failed => panic!("cache read failed during sustained writes"),
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(hits > 30, "read windows did not make progress: {hits}");
    assert!(
        skipped_mirrors > 30,
        "write pacing did not engage: {skipped_mirrors}"
    );
    assert_eq!(client.state(), CacheState::Active);
    assert_eq!(client.disable(), CacheMutation::Accepted);
    worker.join().unwrap();
}
pub(super) use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
pub(super) use std::sync::{Arc, Mutex};
pub(super) use std::time::{Duration, Instant};

#[test]
fn worker_handshake_and_read_hit_cycle() {
    let (client_sock, worker_sock) = UnixStream::pair().expect("socketpair failed");
    let provider = FakeProvider::new(8 * 1024 * 1024 * 1024, 6 * 1024 * 1024 * 1024);
    let config = GpuWorkerConfig {
        target_bytes: 64 * 1024 * 1024,
        chunk_bytes: 2 * 1024 * 1024,
        reserve_floor_bytes: 1536 * 1024 * 1024,
        compression_enabled: false,
    };

    let worker_thread = std::thread::spawn(move || {
        run_gpu_worker_loop(worker_sock, provider, config).expect("worker loop failed");
    });

    let mut client = IpcCacheClient::with_pacing(
        client_sock,
        Duration::from_millis(100),
        64 * 1024 * 1024,
        PacingPolicy::immediate(),
    );
    client.perform_handshake().expect("handshake failed");
    assert_eq!(client.target_bytes(), 64 * 1024 * 1024);

    let test_payload = vec![0x42; 4096];
    let outcome = client.update(0, &test_payload);
    assert_eq!(outcome, CacheMutation::Accepted);

    // Give worker brief moment to process non-blocking update
    std::thread::sleep(Duration::from_millis(10));

    let mut read_buf = vec![0u8; 4096];
    let read_outcome = client.read(0, &mut read_buf);
    assert_eq!(read_outcome, CacheRead::Hit);
    assert_eq!(read_buf, test_payload);

    // Read unwritten offset in another chunk
    let mut unwritten = vec![0u8; 4096];
    let miss_outcome = client.read(4 * 1024 * 1024, &mut unwritten);
    assert_eq!(miss_outcome, CacheRead::Miss);

    let disable_outcome = client.disable();
    assert_eq!(disable_outcome, CacheMutation::Accepted);
    worker_thread.join().expect("join worker thread");
}

/// `invalidate_drops_overlapping_coverage` — DT-11.
///
/// An `MSG_INVALIDATE` for `[offset, offset+len)` must drop every cached
/// entry overlapping that range — compressed extents and raw chunk ranges
/// — while leaving unrelated coverage readable. A punched hole is a miss,
/// never a stale Hit.
#[test]
fn invalidate_drops_overlapping_coverage() {
    const GIB: u64 = 1024 * 1024 * 1024;

    // Compressed extents are dropped whole: they are the unit of coverage.
    let provider = CodecProvider::new(8 * GIB, 8 * GIB);
    let mut worker = GpuCacheWorker::new(
        &provider,
        GpuWorkerConfig {
            target_bytes: 64 * 1024 * 1024,
            chunk_bytes: 2 * 1024 * 1024,
            reserve_floor_bytes: GIB,
            compression_enabled: true,
        },
    );
    let first = vec![0xA1u8; 4096];
    let second = vec![0xB2u8; 4096];
    worker.handle_update(0, &first);
    worker.handle_update(8192, &second);
    assert!(
        worker.compressed_entries_count() >= 2,
        "the codec must have published both extents"
    );
    assert_eq!(worker.handle_read(0, 4096).as_deref(), Some(&first[..]));
    assert_eq!(worker.handle_read(8192, 4096).as_deref(), Some(&second[..]));

    // A hole overlapping only the first extent drops that extent alone.
    worker.handle_invalidate(1024, 1024);
    assert_eq!(
        worker.compressed_entries_count(),
        1,
        "the overlapping compressed extent must be dropped whole"
    );
    assert!(
        worker.handle_read(0, 4096).is_none(),
        "the overlapping extent must miss, never serve pre-write bytes"
    );
    assert_eq!(
        worker.handle_read(8192, 4096).as_deref(),
        Some(&second[..]),
        "unrelated coverage survives an invalidate"
    );
    worker.handle_invalidate(8192, 4096);
    assert_eq!(worker.compressed_entries_count(), 0);

    // Raw chunks punch `valid_ranges` precisely, so remnants stay readable.
    let provider = FakeProvider::new(4 * GIB, 3 * GIB);
    let mut worker = GpuCacheWorker::new(
        &provider,
        GpuWorkerConfig {
            target_bytes: 2 * 1024 * 1024,
            chunk_bytes: 2 * 1024 * 1024,
            reserve_floor_bytes: 1536 * 1024 * 1024,
            compression_enabled: false,
        },
    );
    let first = [0xC3u8; 8];
    let second = [0xD4u8; 8];
    worker.handle_update(0, &first);
    worker.handle_update(64, &second);
    assert_eq!(worker.handle_read(0, 8), Some(first.to_vec()));
    assert_eq!(worker.handle_read(64, 8), Some(second.to_vec()));

    // Punch `[2, 5)`: only those three bytes drop.
    worker.handle_invalidate(2, 3);
    assert!(
        worker.handle_read(0, 8).is_none(),
        "the split extent must no longer cover eight bytes"
    );
    assert_eq!(
        worker.handle_read(0, 2),
        Some(first[..2].to_vec()),
        "the left remnant stays readable"
    );
    assert_eq!(
        worker.handle_read(5, 3),
        Some(first[5..8].to_vec()),
        "the right remnant stays readable"
    );
    assert_eq!(
        worker.handle_read(64, 8),
        Some(second.to_vec()),
        "an unrelated range in the same chunk survives"
    );

    // Covering the whole chunk reclaims it: nothing readable remains.
    worker.handle_invalidate(0, 128);
    assert_eq!(
        worker.active_chunks_count(),
        0,
        "an empty chunk is given back to the provider"
    );
    assert!(worker.handle_read(0, 2).is_none());
    assert!(worker.handle_read(64, 8).is_none());
}

#[test]
fn worker_respects_headroom_floor() {
    let total_vram = 8 * 1024 * 1024 * 1024u64; // 8 GiB
    let free_vram = 7 * 1024 * 1024 * 1024u64; // 7 GiB
    let provider = FakeProvider::new(total_vram, free_vram);

    // Deduct existing usage, then retain both the 2 GiB display reserve
    // and the 640 MiB runtime headroom from currently free bytes.
    let config = GpuWorkerConfig {
        target_bytes: 10 * 1024 * 1024 * 1024, // Request 10 GiB (more than available)
        chunk_bytes: 2 * 1024 * 1024,
        reserve_floor_bytes: 2 * 1024 * 1024 * 1024,
        compression_enabled: false,
    };

    let worker = GpuCacheWorker::new(&provider, config);
    assert_eq!(
        worker.target_bytes(),
        free_vram - config.reserve_floor_bytes - RUNTIME_FREE_BUFFER_BYTES
    );
}

#[test]
fn worker_disable_frees_allocations() {
    let provider = FakeProvider::new(4 * 1024 * 1024 * 1024, 3 * 1024 * 1024 * 1024);
    let live_allocs = Arc::clone(&provider.live_allocations);
    let config = GpuWorkerConfig {
        target_bytes: 16 * 1024 * 1024,
        chunk_bytes: 2 * 1024 * 1024,
        reserve_floor_bytes: 1536 * 1024 * 1024,
        compression_enabled: false,
    };

    let mut worker = GpuCacheWorker::new(&provider, config);
    worker.handle_update(0, &[1, 2, 3, 4]);
    worker.handle_update(2 * 1024 * 1024, &[5, 6, 7, 8]);

    assert_eq!(worker.active_chunks_count(), 2);
    assert_eq!(worker.cached_bytes(), 4 * 1024 * 1024);
    assert_eq!(live_allocs.load(Ordering::SeqCst), 2);

    worker.handle_disable();

    assert_eq!(worker.active_chunks_count(), 0);
    assert_eq!(worker.cached_bytes(), 0);
    assert!(worker.is_disabled());
    assert_eq!(live_allocs.load(Ordering::SeqCst), 0);
}

#[test]
fn worker_evicts_coldest_chunk_on_pressure() {
    let provider = FakeProvider::new(4 * 1024 * 1024 * 1024, 3 * 1024 * 1024 * 1024);
    let config = GpuWorkerConfig {
        target_bytes: 4 * 1024 * 1024, // Space for only 2 chunks of 2 MiB
        chunk_bytes: 2 * 1024 * 1024,
        reserve_floor_bytes: 1536 * 1024 * 1024,
        compression_enabled: false,
    };

    let mut worker = GpuCacheWorker::new(&provider, config);

    // Fill 2 chunks
    worker.handle_update(0, &[10, 20]);
    std::thread::sleep(Duration::from_millis(5));
    worker.handle_promote(2 * 1024 * 1024, &[30, 40]);
    assert_eq!(worker.active_chunks_count(), 2);

    // Add 3rd chunk - chunk 0 should be evicted as coldest
    std::thread::sleep(Duration::from_millis(5));
    worker.handle_update(4 * 1024 * 1024, &[50, 60]);
    assert_eq!(worker.active_chunks_count(), 2);

    // Chunk 0 is evicted -> miss
    assert_eq!(worker.handle_read(0, 2), None);
    // Chunk 1 and 2 remain -> hit
    assert_eq!(worker.handle_read(2 * 1024 * 1024, 2), Some(vec![30, 40]));
    assert_eq!(worker.handle_read(4 * 1024 * 1024, 2), Some(vec![50, 60]));

    // Boundary crossing read returns None
    assert_eq!(worker.handle_read(2 * 1024 * 1024 - 1, 4), None);
}

#[test]
fn worker_handles_promote_and_heartbeat_loop() {
    let (client_sock, worker_sock) = UnixStream::pair().expect("socketpair failed");
    let provider = FakeProvider::new(8 * 1024 * 1024 * 1024, 6 * 1024 * 1024 * 1024);
    let config = GpuWorkerConfig {
        target_bytes: 64 * 1024 * 1024,
        chunk_bytes: 2 * 1024 * 1024,
        reserve_floor_bytes: 1536 * 1024 * 1024,
        compression_enabled: false,
    };

    let worker_thread = std::thread::spawn(move || {
        run_gpu_worker_loop(worker_sock, provider, config).expect("worker loop failed");
    });

    let mut client = IpcCacheClient::with_pacing(
        client_sock,
        Duration::from_millis(100),
        64 * 1024 * 1024,
        PacingPolicy::immediate(),
    );
    client.perform_handshake().expect("handshake failed");

    // Promote frame
    let promote_data = vec![0xEE; 1024];
    let promote_outcome = client.promote(0, &promote_data);
    assert_eq!(promote_outcome, CacheMutation::Accepted);

    std::thread::sleep(Duration::from_millis(10));

    let mut read_buf = vec![0u8; 1024];
    let read_outcome = client.read(0, &mut read_buf);
    assert_eq!(read_outcome, CacheRead::Hit);
    assert_eq!(read_buf, promote_data);

    client
        .refresh_cached_bytes()
        .expect("heartbeat reports cached bytes");
    let budget = client
        .gpu_budget_telemetry()
        .expect("heartbeat publishes adapter budget");
    assert_eq!(
        budget.adapter.as_ref().map(|id| id.backend.as_str()),
        Some("test")
    );
    assert_eq!(budget.source, GpuBudgetSource::DriverReported);
    assert_eq!(
        budget.trusted_available_at(
            unix_time_ms(),
            crate::origin_cache::SAMPLE_MAX_AGE.as_millis() as u64
        ),
        Some(6 * 1024 * 1024 * 1024)
    );

    let disable_outcome = client.disable();
    assert_eq!(disable_outcome, CacheMutation::Accepted);
    worker_thread.join().expect("join worker thread");
}

/// invariant check— teardown must be idempotent and bounded.
/// SPEC: `worker_teardown_is_idempotent_and_bounded`
#[test]
fn worker_teardown_is_idempotent_and_bounded() {
    let provider = FakeProvider::new(4 * 1024 * 1024 * 1024, 3 * 1024 * 1024 * 1024);
    let live_allocs = Arc::clone(&provider.live_allocations);
    let config = GpuWorkerConfig {
        target_bytes: 16 * 1024 * 1024,
        chunk_bytes: 2 * 1024 * 1024,
        reserve_floor_bytes: 1536 * 1024 * 1024,
        compression_enabled: false,
    };

    let mut worker = GpuCacheWorker::new(&provider, config);
    worker.handle_update(0, &[1, 2, 3]);
    worker.handle_update(2 * 1024 * 1024, &[4, 5, 6]);
    assert_eq!(live_allocs.load(Ordering::SeqCst), 2);

    // First teardown: frees all allocations
    let start = Instant::now();
    worker.handle_disable();
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "teardown must be bounded"
    );
    assert_eq!(live_allocs.load(Ordering::SeqCst), 0);

    // Second teardown: idempotent — no panic, no double-free
    worker.handle_disable();
    assert_eq!(live_allocs.load(Ordering::SeqCst), 0);
    assert!(worker.is_disabled());

    // Third teardown on empty state: still idempotent
    worker.handle_disable();
    assert_eq!(worker.active_chunks_count(), 0);
}

/// `read_over_16_mib_refuses_before_allocation` — DT-4.
///
/// A cache read above the private-response ceiling must be refused
/// **before** any response buffer is allocated. A request at exactly the
/// ceiling is still reachable (the ceiling is an upper bound, not a
/// lower one) and a normal hit still serves.
#[test]
fn read_over_16_mib_refuses_before_allocation() {
    const GIB: u64 = 1024 * 1024 * 1024;
    let provider = FakeProvider::new(8 * GIB, 8 * GIB);
    let config = GpuWorkerConfig {
        target_bytes: 2 * GIB,
        chunk_bytes: 2 * 1024 * 1024,
        reserve_floor_bytes: GIB,
        compression_enabled: false,
    };
    let mut worker = GpuCacheWorker::new(&provider, config);
    assert_eq!(worker.target_bytes(), 2 * GIB);

    // One populated chunk so a legitimate read is possible.
    let payload = vec![0xABu8; 4096];
    worker.handle_update(0, &payload);
    assert_eq!(worker.handle_read(0, 4096).as_deref(), Some(&payload[..]));

    // Over the ceiling: refused, and the refusal is not a "miss because
    // nothing is cached" — the same range serves at a legal length.
    assert!(worker.handle_read(0, MAX_READ_LEN + 1).is_none());
    assert!(worker.handle_read(0, MAX_READ_LEN).is_none());
    assert_eq!(worker.handle_read(0, 4096).as_deref(), Some(&payload[..]));

    // Zero and disabled still refuse early.
    assert!(worker.handle_read(0, 0).is_none());
    worker.handle_disable();
    assert!(worker.handle_read(0, 4096).is_none());
}

/// `worker_compression_refuses_from_zero_or_stale_budget` — DT-4.
#[test]
fn worker_compression_refuses_from_zero_or_stale_budget() {
    // Zero / unmeasured budget: target is 0 and nothing may be allocated.
    let none = NoBudgetProvider {
        codec: FakeCodec::new(),
    };
    let config = GpuWorkerConfig {
        target_bytes: 1024 * 1024 * 1024,
        chunk_bytes: 2 * 1024 * 1024,
        reserve_floor_bytes: 128 * 1024 * 1024,
        compression_enabled: true,
    };
    let mut zero_worker = GpuCacheWorker::new(&none, config);
    assert_eq!(zero_worker.target_bytes(), 0);
    zero_worker.handle_update(0, &vec![0u8; 8192]);
    assert_eq!(zero_worker.compressed_entries_count(), 0);
    assert_eq!(zero_worker.compressed_slab_count(), 0);

    // Stale budget: `can_admit` refuses, so no slab and no entry.
    let stale = StaleBudgetProvider {
        total: 8 * 1024 * 1024 * 1024,
        free: 8 * 1024 * 1024 * 1024,
        codec: FakeCodec::new(),
    };
    let mut stale_worker = GpuCacheWorker::new(&stale, config);
    stale_worker.handle_update(0, &vec![0u8; 8192]);
    assert_eq!(stale_worker.compressed_entries_count(), 0);
    assert_eq!(stale_worker.compressed_slab_count(), 0);
}

#[test]
fn compression_admits_the_slab_bridge_and_round_trips_a_page() {
    const GIB: u64 = 1024 * 1024 * 1024;
    let provider = FakeProvider::new(8 * GIB, 8 * GIB);
    let config = GpuWorkerConfig {
        target_bytes: 8 * GIB,
        chunk_bytes: 2 * 1024 * 1024,
        reserve_floor_bytes: 0,
        compression_enabled: true,
    };
    let mut worker = GpuCacheWorker::new(&provider, config);
    assert!(worker.slab_bridge().is_some());
    let mut page = [0u8; PAGE_BYTES];
    for (index, byte) in page.iter_mut().enumerate() {
        *byte = if index % 64 < 32 {
            7
        } else {
            (index / 64) as u8
        };
    }
    let mut tickets = [None; 1];
    worker
        .compress_pages_into_slabs(&[page], &mut tickets)
        .unwrap();
    let ticket = tickets[0].unwrap();
    let mut decoded = [[0u8; PAGE_BYTES]; 1];
    let mut status = [PageDecodeStatus::CorruptLz4Stream; 1];
    worker
        .recover_pages_from_slabs(&[ticket], &mut decoded, &mut status, |_| Ok(page))
        .unwrap();
    assert_eq!(status, [PageDecodeStatus::Ok]);
    assert_eq!(decoded[0], page);

    let mut plain = GpuCacheWorker::new(
        &provider,
        GpuWorkerConfig {
            compression_enabled: false,
            ..config
        },
    );
    assert!(plain.slab_bridge().is_none());
    assert!(
        plain
            .compress_pages_into_slabs(&[page], &mut [None])
            .is_err()
    );
}
