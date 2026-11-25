// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/gpu_cache_worker/tests/raw_and_pressure.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::super::*;
use super::raw_fixtures::*;
use super::*;

#[test]
fn cached_kib_aux_saturates_instead_of_wrapping_at_wire_limit() {
    let max_kib = u64::from(u32::MAX);
    let max_bytes = max_kib * 1024;

    assert_eq!(cached_kib_aux(max_bytes - 1024), u32::MAX - 1);
    assert_eq!(cached_kib_aux(max_bytes), u32::MAX - 1);
    assert_eq!(cached_kib_aux(max_bytes + 1024), u32::MAX - 1);
    assert_eq!(cached_kib_aux(4 * 1024 * 1024 * 1024 * 1024), u32::MAX - 1);
    assert_eq!(cached_kib_observation(None), CACHED_KIB_UNKNOWN);
}

/// Cached bytes are a hold, so the IPC face ceils.
///
/// A sub-KiB occupancy holds something and must not publish `0 KiB`; a
/// measured zero is the only zero, so nothing is invented from nothing.
/// `>> 10` used to serve this face and printed every ramp of `1..=1023`
/// cached bytes as an empty cache.
#[test]
fn cached_kib_aux_ceils_a_sub_kib_hold_instead_of_flooring_it_to_zero() {
    // Non-zero holds must never reach the wire as zero.
    assert_eq!(cached_kib_aux(1), 1);
    assert_eq!(cached_kib_aux(512), 1);
    assert_eq!(cached_kib_aux(1023), 1);
    assert_eq!(cached_kib_aux(1024), 1);
    assert_eq!(cached_kib_aux(1025), 2);
    // Ceil must not invent a KiB from nothing: a measured empty cache
    // stays empty.
    assert_eq!(cached_kib_aux(0), 0);
}

/// ITEM-4: every worker admission threshold is the shared helper's value
/// over an identical snapshot, with the headroom from the one named
/// constant. A second source of the 640 MiB buffer, or any caller that
/// admits below the helper threshold, trips this.
#[test]
fn worker_admission_uses_resolved_policy() {
    const GIB: u64 = 1024 * 1024 * 1024;
    let policy = ramshared_vram::ReserveFloorPolicy::from_manifest(1, 20).expect("valid");
    let configured = policy.configured_reserve_bytes(8 * GIB);
    let config = GpuWorkerConfig {
        target_bytes: 16 * GIB,
        chunk_bytes: 64 * 1024 * 1024,
        reserve_floor_bytes: configured,
        compression_enabled: false,
    };
    // Sit exactly one byte below reserve + runtime + chunk.
    let budget = trusted_test_budget(
        configured + RUNTIME_FREE_BUFFER_BYTES + (config.chunk_bytes as u64) - 1,
        8 * GIB,
    );

    // The single helper threshold over this snapshot.
    let required_free = budget.required_free_bytes(configured, RUNTIME_FREE_BUFFER_BYTES);
    assert_eq!(
        required_free,
        policy.enforced_free_floor_bytes(8 * GIB, RUNTIME_FREE_BUFFER_BYTES),
        "worker and policy must compute one floor"
    );

    // Admission refuses exactly at the boundary: one byte below.
    let needed = config.chunk_bytes as u64;
    let below = budget.available_bytes() < needed + required_free;
    assert!(below, "fixture must sit below the threshold");
    assert_eq!(
        effective_target_from_budget(&budget, config),
        budget.safe_target_bytes(config.target_bytes, configured, RUNTIME_FREE_BUFFER_BYTES)
    );

    // Above the threshold the same snapshot yields the shared target.
    let roomy = trusted_test_budget(8 * GIB - 1024, 8 * GIB);
    assert!(roomy.available_bytes() >= needed + required_free);
    assert_eq!(
        effective_target_from_budget(&roomy, config),
        roomy.safe_target_bytes(config.target_bytes, configured, RUNTIME_FREE_BUFFER_BYTES)
    );
}

/// ITEM-4: a stale budget is refused, never admitted with a reduced
/// target. `can_admit` is the freshness gate; a stale snapshot must fail
/// it rather than reach `safe_target_bytes`.
#[test]
fn worker_admission_refuses_on_stale_budget() {
    const GIB: u64 = 1024 * 1024 * 1024;
    let config = GpuWorkerConfig {
        target_bytes: 4 * GIB,
        chunk_bytes: 512 * 1024 * 1024,
        reserve_floor_bytes: GIB,
        compression_enabled: false,
    };
    let mut stale = trusted_test_budget(6 * GIB, 8 * GIB);
    stale.sampled_at = Instant::now() - Duration::from_secs(60);
    assert!(
        !stale.can_admit(0),
        "a 60s-old snapshot must not be admitted"
    );
    assert_eq!(effective_target_from_budget(&stale, config), 0);

    // Fresh equivalent is admitted.
    let fresh = trusted_test_budget(6 * GIB, 8 * GIB);
    assert!(fresh.can_admit(0));
    assert!(effective_target_from_budget(&fresh, config) > 0);
}

#[test]
fn worker_budget_target_requires_external_adapter_bound_snapshot() {
    const GIB: u64 = 1024 * 1024 * 1024;
    let config = GpuWorkerConfig {
        target_bytes: 4 * GIB,
        chunk_bytes: 512 * 1024 * 1024,
        reserve_floor_bytes: GIB,
        compression_enabled: false,
    };
    let trusted = GpuBudgetSnapshot {
        adapter: Some(GpuAdapterIdentity {
            node_mask: None,
            backend: "test".into(),
            key: "stable-id".into(),
            luid: None,
        }),
        total_bytes: Some(8 * GIB),
        budget_bytes: 5 * GIB,
        used_bytes: 0,
        source: GpuBudgetSource::DriverReported,
        sampled_at: Instant::now(),
    };
    // Both reserve and runtime headroom remain free after allocation.
    assert_eq!(
        effective_target_from_budget(&trusted, config),
        4 * GIB - RUNTIME_FREE_BUFFER_BYTES
    );

    let unknown = GpuBudgetSnapshot {
        adapter: None,
        source: GpuBudgetSource::ProviderLocalEstimate,
        ..trusted
    };
    assert_eq!(effective_target_from_budget(&unknown, config), 0);
}

#[test]
fn worker_budget_target_never_exceeds_current_available_headroom() {
    const GIB: u64 = 1024 * 1024 * 1024;
    let config = GpuWorkerConfig {
        target_bytes: 4 * GIB,
        chunk_bytes: 512 * 1024 * 1024,
        reserve_floor_bytes: GIB,
        compression_enabled: false,
    };
    let low_headroom = GpuBudgetSnapshot {
        adapter: Some(GpuAdapterIdentity {
            node_mask: None,
            backend: "test".into(),
            key: "stable-id".into(),
            luid: None,
        }),
        total_bytes: Some(8 * GIB),
        budget_bytes: 5 * GIB,
        used_bytes: 3 * GIB,
        source: GpuBudgetSource::DriverReported,
        sampled_at: Instant::now(),
    };

    // Available 2 GiB must still retain the 1 GiB reserve and runtime buffer.
    assert_eq!(
        effective_target_from_budget(&low_headroom, config),
        GIB - RUNTIME_FREE_BUFFER_BYTES
    );
}

#[test]
fn worker_budget_target_is_zero_until_runtime_buffer_is_available() {
    const GIB: u64 = 1024 * 1024 * 1024;
    let config = GpuWorkerConfig {
        target_bytes: GIB,
        chunk_bytes: 64 * 1024 * 1024,
        reserve_floor_bytes: 0,
        compression_enabled: false,
    };
    let insufficient = GpuBudgetSnapshot {
        adapter: Some(GpuAdapterIdentity {
            node_mask: None,
            backend: "test".into(),
            key: "stable-id".into(),
            luid: None,
        }),
        total_bytes: Some(2 * GIB),
        budget_bytes: 2 * GIB,
        used_bytes: 2 * GIB - (RUNTIME_FREE_BUFFER_BYTES - 1),
        source: GpuBudgetSource::DriverReported,
        sampled_at: Instant::now(),
    };

    assert_eq!(effective_target_from_budget(&insufficient, config), 0);
}

#[test]
fn heartbeat_pressure_reclaims_cold_cache_and_keeps_origin_fallback() {
    let chunk_bytes = 2 * 1024 * 1024;
    let total = 2 * 1024 * 1024 * 1024;
    let external = Arc::new(AtomicU64::new(0));
    let live_allocations = Arc::new(AtomicUsize::new(0));
    let provider = PressureProvider {
        total,
        external: Arc::clone(&external),
        live_allocations: Arc::clone(&live_allocations),
        chunk_bytes,
    };
    let mut worker = GpuCacheWorker::new(
        &provider,
        GpuWorkerConfig {
            target_bytes: 4 * chunk_bytes,
            chunk_bytes: chunk_bytes as usize,
            reserve_floor_bytes: 128 * 1024 * 1024,
            compression_enabled: false,
        },
    );
    worker.handle_update(0, &[1]);
    std::thread::sleep(Duration::from_millis(1));
    worker.handle_update(chunk_bytes, &[2]);
    assert_eq!(worker.cached_bytes(), 2 * chunk_bytes);

    external.store(
        total - 2 * chunk_bytes - (RUNTIME_FREE_BUFFER_BYTES - chunk_bytes),
        Ordering::SeqCst,
    );
    assert_eq!(
        worker.reclaim_under_host_pressure().unwrap(),
        2 * chunk_bytes
    );
    assert_eq!(worker.cached_bytes(), 0);
    assert_eq!(worker.handle_read(0, 1), None);
    assert_eq!(worker.handle_read(chunk_bytes, 1), None);
    assert!(provider.mem_info().unwrap().0 >= RUNTIME_FREE_BUFFER_BYTES);
    worker.handle_update(2 * chunk_bytes, &[3]);
    assert_eq!(worker.cached_bytes(), 0);
    external.store(0, Ordering::SeqCst);
    assert_eq!(worker.reclaim_under_host_pressure().unwrap(), 0);
    worker.handle_update(2 * chunk_bytes, &[3]);
    assert_eq!(worker.cached_bytes(), chunk_bytes);
}

#[test]
fn heartbeat_reports_physical_release_after_external_gpu_pressure() {
    let chunk_bytes = 2 * 1024 * 1024;
    let total = 2 * 1024 * 1024 * 1024;
    let external = Arc::new(AtomicU64::new(0));
    let live_allocations = Arc::new(AtomicUsize::new(0));
    let provider = PressureProvider {
        total,
        external: Arc::clone(&external),
        live_allocations: Arc::clone(&live_allocations),
        chunk_bytes,
    };
    let (client_socket, worker_socket) = UnixStream::pair().expect("socketpair failed");
    let worker_thread = std::thread::spawn(move || {
        run_gpu_worker_loop(
            worker_socket,
            provider,
            GpuWorkerConfig {
                target_bytes: 4 * chunk_bytes,
                chunk_bytes: chunk_bytes as usize,
                reserve_floor_bytes: 128 * 1024 * 1024,
                compression_enabled: false,
            },
        )
        .expect("worker loop failed");
    });
    let mut client = IpcCacheClient::with_pacing(
        client_socket,
        Duration::from_secs(1),
        4 * chunk_bytes,
        PacingPolicy::immediate(),
    );
    client.perform_handshake().expect("handshake failed");
    assert_eq!(client.update(0, &[1]), CacheMutation::Accepted);
    assert_eq!(client.update(chunk_bytes, &[2]), CacheMutation::Accepted);
    assert_eq!(
        client.refresh_cached_bytes().expect("first heartbeat"),
        2 * chunk_bytes
    );

    external.store(
        total - 2 * chunk_bytes - (RUNTIME_FREE_BUFFER_BYTES - chunk_bytes),
        Ordering::SeqCst,
    );
    assert_eq!(
        client.refresh_cached_bytes().expect("pressure heartbeat"),
        0
    );
    assert_eq!(client.read(0, &mut [0]), CacheRead::Miss);
    assert_eq!(
        client.update(2 * chunk_bytes, &[3]),
        CacheMutation::Accepted
    );
    assert_eq!(client.refresh_cached_bytes().expect("parked heartbeat"), 0);
    external.store(0, Ordering::SeqCst);
    assert_eq!(
        client.refresh_cached_bytes().expect("recovery heartbeat"),
        0
    );
    assert_eq!(
        client.update(2 * chunk_bytes, &[3]),
        CacheMutation::Accepted
    );
    assert_eq!(
        client.refresh_cached_bytes().expect("refill heartbeat"),
        chunk_bytes
    );
    drop(client);
    worker_thread.join().expect("worker thread joined");
    assert_eq!(live_allocations.load(Ordering::SeqCst), 0);
}

#[test]
fn worker_never_serves_unwritten_bytes_from_an_allocated_chunk() {
    let provider = FakeProvider::new(4 * 1024 * 1024 * 1024, 3 * 1024 * 1024 * 1024);
    let mut worker = GpuCacheWorker::new(
        &provider,
        GpuWorkerConfig {
            target_bytes: 2 * 1024 * 1024,
            chunk_bytes: 2 * 1024 * 1024,
            reserve_floor_bytes: 1536 * 1024 * 1024,
            compression_enabled: false,
        },
    );
    worker.handle_update(0, &[1, 2, 3, 4]);
    assert_eq!(worker.handle_read(0, 4), Some(vec![1, 2, 3, 4]));
    assert_eq!(worker.handle_read(4096, 4), None);
    assert_eq!(worker.handle_read(2, 4), None);
    worker.handle_update(4, &[5, 6, 7, 8]);
    assert_eq!(worker.handle_read(0, 8), Some(vec![1, 2, 3, 4, 5, 6, 7, 8]));
}

/// SPEC §12 · ITEM-1 · AUD-33 · invariant check.
///
/// A mutation that crosses a chunk boundary must leave **neither** old raw
/// entry readable as its pre-write bytes. The audit geometry: two 2 MiB
/// chunks filled with `A`, 8 KiB of `B` written 4 KiB before the boundary.
/// Before the fix `handle_update` returned early on the crossing and both
/// sides kept serving `A` as Hits.
#[test]
fn cross_chunk_write_invalidates_both_raw_entries() {
    let chunk_bytes = 2 * 1024 * 1024;
    let provider = roomy_provider();
    let mut worker = worker_covering_two_chunks(&provider, chunk_bytes, false, b'A');

    let boundary = chunk_bytes;
    let offset = boundary - 4096;
    let payload = vec![b'B'; 8192];
    worker.handle_update(offset, &payload);

    // Neither side may serve the pre-write bytes.
    let left = worker
        .handle_read(offset, 4096)
        .expect("left half must be readable after a successful write");
    let right = worker
        .handle_read(boundary, 4096)
        .expect("right half must be readable after a successful write");
    assert!(
        left.iter().all(|&b| b == b'B'),
        "left raw entry still serves pre-write bytes: {:?}",
        &left[..8]
    );
    assert!(
        right.iter().all(|&b| b == b'B'),
        "right raw entry still serves pre-write bytes: {:?}",
        &right[..8]
    );
    // And the write is a real write, not just an invalidation: the bytes
    // immediately outside the mutation are untouched.
    assert_eq!(worker.handle_read(0, 1), Some(vec![b'A']));
    assert_eq!(worker.handle_read(boundary + 4096, 1), Some(vec![b'A']));
}

/// AUD-33 · invariant check.
///
/// Byte equality over the shapes the audit named: offsets immediately
/// before/on/after a boundary, mutations spanning more than two chunks,
/// both compression modes, and more than one chunk size. A cross-chunk
/// update must never return the old bytes from any confining subrange.
#[test]
fn raw_cache_cross_chunk_update_never_returns_old_bytes() {
    for &(chunk_bytes, compression_enabled) in &[
        (64 * 1024_u64, false),
        (64 * 1024_u64, true),
        (256 * 1024_u64, false),
        (256 * 1024_u64, true),
    ] {
        // 4-chunk fixture so a mutation can span more than two chunks.
        let provider = roomy_provider();
        let mut worker = GpuCacheWorker::new(
            &provider,
            GpuWorkerConfig {
                target_bytes: 16 * chunk_bytes,
                chunk_bytes: chunk_bytes as usize,
                reserve_floor_bytes: 128 * 1024 * 1024,
                compression_enabled,
            },
        );
        let fill = vec![b'A'; chunk_bytes as usize];
        for i in 0..4u64 {
            worker.handle_update(i * chunk_bytes, &fill);
        }

        // Start 1 byte before the first boundary and span three chunks.
        let offset = chunk_bytes - 1;
        let len = (2 * chunk_bytes + 2) as usize;
        let payload: Vec<u8> = (0..len).map(|i| b'B' + (i % 7) as u8).collect();
        worker.handle_update(offset, &payload);

        assert_eq!(
            worker.handle_read(offset, len),
            Some(payload.clone()),
            "cross-chunk read must return exactly what was written \
             (chunk={chunk_bytes}, compression={compression_enabled})"
        );
        // Confining subranges on either side of each crossed boundary.
        for probe in [
            offset,
            chunk_bytes - 1,
            chunk_bytes,
            2 * chunk_bytes - 1,
            2 * chunk_bytes,
            offset + len as u64 - 1,
        ] {
            let want = payload[(probe - offset) as usize];
            assert_eq!(
                worker.handle_read(probe, 1),
                Some(vec![want]),
                "probe {probe} serves stale bytes (chunk={chunk_bytes})"
            );
        }
        // Bytes outside the mutation are untouched.
        assert_eq!(
            worker.handle_read(0, 1),
            Some(vec![b'A']),
            "bytes before the mutation must stay readable and unchanged"
        );
    }
}

/// AUD-33 · invariant check.
///
/// The authoritative origin path splits every mutation into consecutive
/// 64 KiB frames without aligning them to worker chunk boundaries
/// (`mutation_frames`). After an origin write that crosses a chunk edge,
/// the cache-backed read path must return the new bytes — not the
/// pre-write bytes with a non-zero `cache_read_bytes`.
#[test]
fn authoritative_origin_cross_chunk_write_matches_cache_reads() {
    let chunk_bytes = 2 * 1024 * 1024;
    let size = 4 * chunk_bytes;
    let provider = roomy_provider();
    let mut worker = worker_covering_two_chunks(&provider, chunk_bytes, false, b'A');

    let boundary = chunk_bytes;
    let offset = boundary - 4096;
    let payload = vec![b'B'; 8192];
    let mut origin = vec![b'A'; size as usize];
    origin[offset as usize..offset as usize + 8192].copy_from_slice(&payload);

    let backend = AuthoritativeOriginBackend::new_with_admission(
        SliceOrigin(std::cell::RefCell::new(origin)),
        WorkerCache {
            worker: &mut worker,
        },
        size,
        4096,
        crate::isolated_origin::AllowCacheAdmission,
    )
    .expect("backend over an in-range origin");
    backend.write_at(offset, &payload).expect("origin write");

    let mut left = vec![0u8; 4096];
    let mut right = vec![0u8; 4096];
    backend.read_at(offset, &mut left).expect("left read");
    backend.read_at(boundary, &mut right).expect("right read");
    assert!(
        left.iter().all(|&b| b == b'B'),
        "authoritative write left the left chunk serving pre-write bytes"
    );
    assert!(
        right.iter().all(|&b| b == b'B'),
        "authoritative write left the right chunk serving pre-write bytes"
    );
    // Both reads were cache hits, so a stale answer here would have been
    // served with a non-zero cache_read_bytes — the audit's signature.
    assert_eq!(backend.telemetry().cache_read_bytes, 8192);
}

#[test]
fn worker_reports_allocated_vram_after_bounded_heartbeat() {
    let (client_sock, worker_sock) = UnixStream::pair().expect("socketpair failed");
    let provider = FakeProvider::new(4 * 1024 * 1024 * 1024, 3 * 1024 * 1024 * 1024);
    let worker_thread = std::thread::spawn(move || {
        run_gpu_worker_loop(
            worker_sock,
            provider,
            GpuWorkerConfig {
                target_bytes: 2 * 1024 * 1024,
                chunk_bytes: 2 * 1024 * 1024,
                reserve_floor_bytes: 1536 * 1024 * 1024,
                compression_enabled: false,
            },
        )
        .expect("worker loop failed");
    });
    let mut client = IpcCacheClient::with_pacing(
        client_sock,
        Duration::from_millis(100),
        2 * 1024 * 1024,
        PacingPolicy::immediate(),
    );
    client.perform_handshake().expect("handshake failed");
    assert_eq!(client.update(0, &[1, 2, 3, 4]), CacheMutation::Accepted);
    assert_eq!(
        client.refresh_cached_bytes().expect("heartbeat failed"),
        2 * 1024 * 1024
    );
    assert_eq!(client.cached_bytes(), 2 * 1024 * 1024);
    assert_eq!(client.disable(), CacheMutation::Accepted);
    worker_thread.join().expect("join worker thread");
}

#[test]
fn oversized_mutation_disables_cache_before_worker_frame_is_sent() {
    let (client_sock, worker_sock) = UnixStream::pair().expect("socketpair failed");
    let provider = FakeProvider::new(4 * 1024 * 1024 * 1024, 3 * 1024 * 1024 * 1024);
    let worker_thread = std::thread::spawn(move || {
        run_gpu_worker_loop(
            worker_sock,
            provider,
            GpuWorkerConfig {
                target_bytes: 2 * 1024 * 1024,
                chunk_bytes: 2 * 1024 * 1024,
                reserve_floor_bytes: 1536 * 1024 * 1024,
                compression_enabled: false,
            },
        )
        .expect("worker loop failed");
    });
    let mut client = IpcCacheClient::with_pacing(
        client_sock,
        Duration::from_millis(100),
        2 * 1024 * 1024,
        PacingPolicy::immediate(),
    );
    client.perform_handshake().expect("handshake failed");
    let payload = vec![0x5a; 512 * 1024];
    assert_eq!(client.update(0, &payload), CacheMutation::Failed);
    assert_eq!(client.state(), crate::origin_cache::CacheState::Unavailable);
    worker_thread.join().expect("join worker thread");
}

#[test]
fn worker_refuses_allocation_when_live_gpu_free_buffer_is_low() {
    let provider = FakeProvider::new(6 * 1024 * 1024 * 1024, 256 * 1024 * 1024);
    let allocations = Arc::clone(&provider.live_allocations);
    let mut worker = GpuCacheWorker::new(
        &provider,
        GpuWorkerConfig {
            target_bytes: 2 * 1024 * 1024,
            chunk_bytes: 2 * 1024 * 1024,
            reserve_floor_bytes: 1536 * 1024 * 1024,
            compression_enabled: false,
        },
    );
    worker.handle_update(0, &[1, 2, 3, 4]);
    assert_eq!(allocations.load(Ordering::SeqCst), 0);
    assert_eq!(worker.cached_bytes(), 0);
}

#[test]
fn small_admitted_headroom_still_caches_raw_data() {
    const MIB: u64 = 1024 * 1024;
    let provider = FakeProvider::new(
        8 * 1024 * MIB,
        2 * 1024 * MIB + RUNTIME_FREE_BUFFER_BYTES + 33 * MIB,
    );
    let mut worker = GpuCacheWorker::new(
        &provider,
        GpuWorkerConfig {
            target_bytes: 1024 * MIB,
            chunk_bytes: (128 * MIB) as usize,
            reserve_floor_bytes: 2 * 1024 * MIB,
            compression_enabled: false,
        },
    );
    assert_eq!(worker.target_bytes(), 33 * MIB);
    let data = vec![0x5a; 4096];
    worker.handle_update(0, &data);
    assert_eq!(
        worker.handle_read(0, data.len()).as_deref(),
        Some(data.as_slice())
    );
    assert!(worker.cached_bytes() <= worker.target_bytes());
    worker.handle_invalidate(0, data.len() as u64);
    assert!(worker.handle_read(0, data.len()).is_none());
}

#[test]
fn small_headroom_worker_socket_round_trips_cache_data() {
    const MIB: u64 = 1024 * 1024;
    let provider = FakeProvider::new(
        8 * 1024 * MIB,
        2 * 1024 * MIB + RUNTIME_FREE_BUFFER_BYTES + 33 * MIB,
    );
    let config = GpuWorkerConfig {
        target_bytes: 1024 * MIB,
        chunk_bytes: (128 * MIB) as usize,
        reserve_floor_bytes: 2 * 1024 * MIB,
        compression_enabled: false,
    };
    let (client_socket, worker_socket) = UnixStream::pair().unwrap();
    let thread =
        std::thread::spawn(move || run_gpu_worker_loop(worker_socket, provider, config).unwrap());
    let mut client = IpcCacheClient::with_pacing(
        client_socket,
        Duration::from_secs(1),
        config.target_bytes,
        PacingPolicy::immediate(),
    );
    client.perform_handshake().unwrap();
    assert_eq!(client.target_bytes(), 33 * MIB);
    let data = vec![0x3b; 4096];
    assert_eq!(client.update(4096, &data), CacheMutation::Accepted);
    let mut read = vec![0; data.len()];
    assert_eq!(client.read(4096, &mut read), CacheRead::Hit);
    assert_eq!(read, data);
    assert_eq!(client.disable(), CacheMutation::Accepted);
    thread.join().unwrap();
}

#[test]
fn device_codec_reclaim_keeps_slab_inside_runtime_hysteresis_and_aborts_below_hard_reserve() {
    use super::codec_fixtures::CodecProvider;
    use ramshared_vram::device_codec::{GpuDeviceCodec, ReferenceLz4Codec};
    const MIB: u64 = 1024 * 1024;
    let reserve = 2048 * MIB;
    let total = 6144 * MIB;
    let provider = CodecProvider::new(total, reserve + RUNTIME_FREE_BUFFER_BYTES + 64 * MIB);
    let config = GpuWorkerConfig {
        target_bytes: 64 * MIB,
        chunk_bytes: (16 * MIB) as usize,
        reserve_floor_bytes: reserve,
        compression_enabled: true,
    };
    let mut worker = GpuCacheWorker::new_with_device_codec(
        &provider,
        config,
        Some(|_, _, _| {
            let mut codec = ReferenceLz4Codec::new();
            codec.set_deadline(std::time::Duration::from_secs(10));
            Ok(Box::new(codec) as Box<dyn GpuDeviceCodec>)
        }),
    );
    assert!(worker.device_codec_reservation > 0 && worker.slab_bridge.is_some());

    // Positive (invariant checkhysteresis): a dip inside the runtime cushion above
    // the sealed hard reserve constrains new admissions without tearing down the slab.
    provider.set_free(reserve + 64 * MIB);
    assert!(matches!(worker.reclaim_under_host_pressure(), Ok(0)));
    assert!(worker.pressure_constrained);
    assert!(!worker.is_disabled());

    // Refusal: breaching the hard sealed reserve still fails
    // closed with Busy so the worker exits and yields VRAM back to the host.
    provider.set_free(reserve - MIB);
    assert!(matches!(
        worker.reclaim_under_host_pressure(),
        Err(VramError::Busy)
    ));
}
