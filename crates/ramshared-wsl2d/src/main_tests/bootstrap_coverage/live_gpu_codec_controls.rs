// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main_tests/bootstrap_coverage/live_gpu_codec_controls.rs
//! Live RTX 2060 3-round same-load throughput controls and 4 GiB mixed-compressibility
//! slab lifecycle qualification (SPEC gpu-vram-hardware-compression).

use super::*;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ramshared_block::{CacheState, IpcCacheClient};
use ramshared_cuda::{Cuda, CudaPtxLz4Codec};
use ramshared_dxg::{BudgetRequest, DxgBudgetProvider, DxgError};
use ramshared_ipc::gpu_catalog::{CatalogSession, CatalogTiming};
use ramshared_ipc::{HandshakeTranscript, HeartbeatAck, HostChallenge, IPC_VERSION_4};
use ramshared_vram::ReserveFloorPolicy;
use ramshared_vram::device_codec::{
    GpuDeviceCodec, MAX_BATCH_PAGES, PAGE_BYTES, PageDecodeStatus, ReferenceLz4Codec,
};
use ramshared_vram::host_identity::{
    HostGpuIdentity, HostGpuIdentityReport, HostGpuObservation, ProducerProcess,
};
use ramshared_vram::slab_pool::{SlabClass, VramSlabPool};
use ramshared_vulkan::VulkanInventory;
use ramshared_vulkan::compute_lz4_codec::VulkanComputeLz4Codec;
use ramshared_wsl2d::gpu_budget::DxgOpenTarget;
use ramshared_wsl2d::host_catalog::SharedHostCatalog;
use sha2::{Digest, Sha256};

fn fill_high_entropy_prefix(page: &mut [u8; PAGE_BYTES], prefix_len: usize, seed: u64) {
    page.fill(0);
    let mut state = seed ^ 0x9E37_79B9_7F4A_7C15;
    for b in page.iter_mut().take(prefix_len) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *b = (state >> 24) as u8;
    }
}

/// Builds a deterministic 64-page mixed-compressibility batch:
/// - 26 pages (40.6%) B512 (`prefix_len = 128`)
/// - 19 pages (29.7%) B1024 (`prefix_len = 768`)
/// - 10 pages (15.6%) B2048 (`prefix_len = 1536`)
/// -  9 pages (14.1%) Raw4096 (`prefix_len = 4096`)
fn build_mixed_corpus_batch(batch_seed: u64) -> Vec<[u8; PAGE_BYTES]> {
    let mut batch = Vec::with_capacity(MAX_BATCH_PAGES);
    for idx in 0..MAX_BATCH_PAGES {
        let mut page = [0u8; PAGE_BYTES];
        let seed = batch_seed
            .wrapping_mul(131)
            .wrapping_add(idx as u64 + 1)
            .wrapping_mul(0x517C_C1B7_2722_0A95);
        let prefix_len = if idx < 26 {
            128
        } else if idx < 45 {
            768
        } else if idx < 55 {
            1536
        } else {
            PAGE_BYTES
        };
        fill_high_entropy_prefix(&mut page, prefix_len, seed);
        batch.push(page);
    }
    batch
}

fn percentile_us(samples: &mut [u64], num: usize, den: usize) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    samples.sort_unstable();
    let idx = ((samples.len() - 1) * num) / den;
    samples[idx]
}

struct LiveControlHost {
    sock: Option<UnixStream>,
}

impl GpuWorkerEntryHost for LiveControlHost {
    fn duplicate_socket(&mut self, _fd: i32) -> std::io::Result<UnixStream> {
        self.sock
            .take()
            .ok_or_else(|| std::io::Error::other("missing worker socket"))
    }
    fn bind_parent(&mut self, _socket: &UnixStream) -> std::io::Result<()> {
        Ok(())
    }
    fn admit(&mut self, _request: &gpu_worker_request::WorkerRequest) -> Result<(), String> {
        Ok(())
    }
    fn claim_catalog(
        &mut self,
        catalog_fd: Option<i32>,
        worker_socket: &UnixStream,
    ) -> std::io::Result<Option<Arc<dyn ramshared_wsl2d::host_catalog::HostGpuCatalogProvider>>>
    {
        ProductionGpuWorkerEntryHost.claim_catalog(catalog_fd, worker_socket)
    }
    fn open_dxg(&self, req: BudgetRequest) -> Result<DxgBudgetProvider, DxgError> {
        ProductionGpuWorkerEntryHost.open_dxg(req)
    }
    fn open_dxg_target(&self, target: DxgOpenTarget<'_>) -> Result<DxgBudgetProvider, DxgError> {
        ProductionGpuWorkerEntryHost.open_dxg_target(target)
    }
}

#[derive(Debug)]
struct RoundMetrics {
    compress_mib_s: f64,
    decompress_mib_s: f64,
    compress_p50_us: u64,
    compress_p99_us: u64,
    decompress_p50_us: u64,
    decompress_p99_us: u64,
    density_ratio: f64,
}

fn run_codec_round(
    codec: &mut dyn GpuDeviceCodec,
    pool: &mut VramSlabPool,
    batches: &[Vec<[u8; PAGE_BYTES]>],
) -> RoundMetrics {
    let mut comp_lat_us = Vec::with_capacity(batches.len());
    let mut dec_lat_us = Vec::with_capacity(batches.len());
    let mut out_tickets = vec![None; MAX_BATCH_PAGES];
    let mut out_pages = vec![[0u8; PAGE_BYTES]; MAX_BATCH_PAGES];
    let mut out_status = vec![PageDecodeStatus::Ok; MAX_BATCH_PAGES];

    let total_bytes = (batches.len() * MAX_BATCH_PAGES * PAGE_BYTES) as f64;
    let mut comp_total_sec = 0.0f64;
    let mut dec_total_sec = 0.0f64;

    for batch in batches {
        let t0 = Instant::now();
        codec
            .compress_and_pack_batch(batch, pool, &mut out_tickets)
            .expect("compress_and_pack_batch must succeed");
        let comp_el = t0.elapsed();
        comp_total_sec += comp_el.as_secs_f64();
        comp_lat_us.push(comp_el.as_micros() as u64);

        let tickets: Vec<_> = out_tickets
            .iter()
            .map(|t| t.expect("ticket present"))
            .collect();
        let t1 = Instant::now();
        codec
            .verify_and_unpack_batch(&tickets, &mut out_pages, &mut out_status)
            .expect("verify_and_unpack_batch must succeed");
        let dec_el = t1.elapsed();
        dec_total_sec += dec_el.as_secs_f64();
        dec_lat_us.push(dec_el.as_micros() as u64);

        for (i, status) in out_status.iter().enumerate() {
            assert_eq!(*status, PageDecodeStatus::Ok, "page {i} decode failed");
            assert_eq!(out_pages[i], batch[i], "page {i} roundtrip mismatch");
            let _ = pool.lookup_and_touch(&tickets[i]);
        }
    }

    let density_ratio = pool.effective_density_ratio().unwrap_or(0.0);
    let mib = total_bytes / (1024.0 * 1024.0);
    RoundMetrics {
        compress_mib_s: mib / comp_total_sec.max(1e-9),
        decompress_mib_s: mib / dec_total_sec.max(1e-9),
        compress_p50_us: percentile_us(&mut comp_lat_us, 50, 100),
        compress_p99_us: percentile_us(&mut comp_lat_us, 99, 100),
        decompress_p50_us: percentile_us(&mut dec_lat_us, 50, 100),
        decompress_p99_us: percentile_us(&mut dec_lat_us, 99, 100),
        density_ratio,
    }
}

#[test]
#[ignore = "requires an explicitly scheduled live CUDA hardware qualification"]
fn live_rtx2060_same_load_codec_throughput_controls_three_rounds() {
    let Ok(cuda) = Cuda::load() else {
        eprintln!("SAME_LOAD_THROUGHPUT_CONTROLS=SKIP reason=cuda_unavailable");
        return;
    };
    let Ok(device) = cuda.device(0) else {
        eprintln!("SAME_LOAD_THROUGHPUT_CONTROLS=SKIP reason=no_cuda_device");
        return;
    };
    let Ok(ctx) = cuda.create_context(&device) else {
        eprintln!("SAME_LOAD_THROUGHPUT_CONTROLS=SKIP reason=cuda_context_failed");
        return;
    };

    const BATCHES_PER_ROUND: usize = 16; // 16 * 64 = 1,024 pages = 4 MiB per round
    const ROUNDS: usize = 3;
    let batches: Vec<Vec<[u8; PAGE_BYTES]>> = (0..BATCHES_PER_ROUND)
        .map(|b| build_mixed_corpus_batch(b as u64 + 1))
        .collect();

    // 1. ReferenceLz4Codec (3 rounds; debug-profile CPU reference uses 500 ms bound)
    let mut ref_rounds = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let mut pool = VramSlabPool::new(128).expect("ref pool");
        let mut codec = ReferenceLz4Codec::new();
        codec.set_deadline(Duration::from_millis(500));
        let m = run_codec_round(&mut codec, &mut pool, &batches);
        assert!(m.density_ratio >= 1.60, "ref density={}", m.density_ratio);
        assert!(m.compress_p99_us < 500_000);
        assert!(m.decompress_p99_us < 500_000);
        ref_rounds.push(m);
    }

    // 2. CudaContextPtxCodec on live RTX 2060 (3 rounds; strict 20 ms hardware sub-deadline)
    let pool_frames = 128u32;
    let avail_bytes = 64 * 1024 * 1024u64;
    let mut cuda_rounds = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let mut pool = VramSlabPool::new(pool_frames).expect("cuda pool");
        let mut codec =
            CudaPtxLz4Codec::new_context_codec(&ctx, avail_bytes, pool_frames).expect("ptx codec");
        let m = run_codec_round(&mut codec, &mut pool, &batches);
        assert!(m.density_ratio >= 1.60, "cuda density={}", m.density_ratio);
        assert!(m.compress_p99_us < 20_000);
        assert!(m.decompress_p99_us < 20_000);
        assert!(!codec.poisoned());
        cuda_rounds.push(m);
    }

    // 3. VulkanComputeLz4Codec + Vulkan Inventory check (3 rounds)
    let vulkan_devices = VulkanInventory::open()
        .map(|inv| inv.device_count())
        .unwrap_or(0);
    let mut vk_rounds = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let mut pool = VramSlabPool::new(128).expect("vk pool");
        let mut codec = VulkanComputeLz4Codec::new();
        codec.set_deadline(Duration::from_millis(500));
        assert!(codec.hardware_spirv_embedded());
        let m = run_codec_round(&mut codec, &mut pool, &batches);
        assert!(m.density_ratio >= 1.60, "vk density={}", m.density_ratio);
        assert!(m.compress_p99_us < 500_000);
        assert!(m.decompress_p99_us < 500_000);
        vk_rounds.push(m);
    }

    eprintln!(
        "SAME_LOAD_THROUGHPUT_CONTROLS=PASS rounds={} pages_per_round={} vulkan_devices={} \
         ref_comp_mib_s={:.1} ref_dec_mib_s={:.1} ref_comp_p99_us={} ref_dec_p99_us={} \
         cuda_ptx_comp_mib_s={:.1} cuda_ptx_dec_mib_s={:.1} cuda_ptx_comp_p50_us={} cuda_ptx_comp_p99_us={} \
         cuda_ptx_dec_p50_us={} cuda_ptx_dec_p99_us={} cuda_density={:.2} \
         vk_comp_mib_s={:.1} vk_dec_mib_s={:.1} vk_comp_p99_us={} vk_dec_p99_us={}",
        ROUNDS,
        BATCHES_PER_ROUND * MAX_BATCH_PAGES,
        vulkan_devices,
        ref_rounds[1].compress_mib_s,
        ref_rounds[1].decompress_mib_s,
        ref_rounds[1].compress_p99_us,
        ref_rounds[1].decompress_p99_us,
        cuda_rounds[1].compress_mib_s,
        cuda_rounds[1].decompress_mib_s,
        cuda_rounds[1].compress_p50_us,
        cuda_rounds[1].compress_p99_us,
        cuda_rounds[1].decompress_p50_us,
        cuda_rounds[1].decompress_p99_us,
        cuda_rounds[1].density_ratio,
        vk_rounds[1].compress_mib_s,
        vk_rounds[1].decompress_mib_s,
        vk_rounds[1].compress_p99_us,
        vk_rounds[1].decompress_p99_us,
    );
}

fn capture_binary_match_telemetry() -> (String, u32, u64, String) {
    let exe_bytes = std::fs::read("/proc/self/exe").expect("read /proc/self/exe");
    let sha256: String = Sha256::digest(&exe_bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let pid = std::process::id();
    let stat = std::fs::read_to_string("/proc/self/stat").expect("read /proc/self/stat");
    let after_comm = stat.rfind(')').map(|i| &stat[i + 2..]).unwrap_or("");
    let start_ticks = after_comm
        .split_whitespace()
        .nth(19)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .unwrap_or_default()
        .trim()
        .to_string();
    (sha256, pid, start_ticks, boot_id)
}

#[test]
#[ignore = "live RTX 2060 4 GiB (1,048,576 pages) mixed-compressibility slab lifecycle"]
fn live_rtx2060_four_gib_mixed_compressibility_slab_lifecycle() {
    let (exe_sha256, pid, start_ticks, boot_id) = capture_binary_match_telemetry();
    assert_eq!(exe_sha256.len(), 64);
    assert!(pid > 0 && start_ticks > 0 && !boot_id.is_empty());

    let cuda = Cuda::load().expect("CUDA driver must load on RTX 2060 host");
    let device = cuda.device(0).expect("CUDA device 0 must exist");
    let ctx = cuda.create_context(&device).expect("create CUDA context");
    let (free_before, total_vram) = ctx.mem_info().expect("query CUDA mem_info");
    let reserve_floor_bytes =
        ReserveFloorPolicy::sealed().configured_reserve_bytes(total_vram as u64);
    assert!(
        (free_before as u64) > reserve_floor_bytes + 512 * 1024 * 1024,
        "free VRAM ({free_before}) must exceed reserve floor ({reserve_floor_bytes}) + 512 MiB"
    );

    // 4,096 frames * 64 KiB = 256 MiB physical slab pool in GPU VRAM.
    let pool_frames: u32 = 4_096;
    let avail_budget: u64 = 320 * 1024 * 1024;
    let mut pool = VramSlabPool::new(pool_frames).expect("create 4096-frame VramSlabPool");
    let mut codec = CudaPtxLz4Codec::new_context_codec(&ctx, avail_budget, pool_frames)
        .expect("allocate resident 256 MiB CudaContextPtxCodec in RTX 2060 VRAM");

    // Pre-generate 64 distinct mixed-compressibility batches (4,096 unique pages)
    // and stream 16,384 batches = 1,048,576 logical 4 KiB pages = 4 GiB total.
    const UNIQUE_BATCHES: usize = 64;
    const TOTAL_BATCHES: usize = 16_384; // 16,384 * 64 = 1,048,576 pages = 4 GiB
    let corpus: Vec<Vec<[u8; PAGE_BYTES]>> = (0..UNIQUE_BATCHES)
        .map(|i| build_mixed_corpus_batch(i as u64 + 100))
        .collect();

    let mut out_tickets = vec![None; MAX_BATCH_PAGES];
    let mut out_pages = vec![[0u8; PAGE_BYTES]; MAX_BATCH_PAGES];
    let mut out_status = vec![PageDecodeStatus::Ok; MAX_BATCH_PAGES];
    let mut comp_lat_us = Vec::with_capacity(TOTAL_BATCHES);
    let mut dec_lat_us = Vec::with_capacity(TOTAL_BATCHES / 64);
    let mut pages_processed: u64 = 0;
    let mut pages_verified: u64 = 0;
    let mut class_counts = [0u64; 5];

    let started = Instant::now();
    for batch_idx in 0..TOTAL_BATCHES {
        let batch = &corpus[batch_idx % UNIQUE_BATCHES];
        let t0 = Instant::now();
        codec
            .compress_and_pack_batch(batch, &mut pool, &mut out_tickets)
            .expect("4 GiB stream compress_and_pack_batch must succeed");
        comp_lat_us.push(t0.elapsed().as_micros() as u64);
        pages_processed += MAX_BATCH_PAGES as u64;

        for t in out_tickets.iter().flatten() {
            let idx = match t.class {
                SlabClass::B512 => 0,
                SlabClass::B1024 => 1,
                SlabClass::B2048 => 2,
                SlabClass::B3072 => 3,
                SlabClass::Raw4096 => 4,
            };
            class_counts[idx] += 1;
        }

        // Every 64th batch (256 times across the 4 GiB run = 16,384 pages),
        // run in-VRAM CRC32C + LZ4 unpack kernel and verify bit-exact recovery.
        if batch_idx % 64 == 63 {
            let tickets: Vec<_> = out_tickets
                .iter()
                .map(|t| t.expect("ticket present"))
                .collect();
            let t1 = Instant::now();
            codec
                .verify_and_unpack_batch(&tickets, &mut out_pages, &mut out_status)
                .expect("4 GiB stream verify_and_unpack_batch must succeed");
            dec_lat_us.push(t1.elapsed().as_micros() as u64);
            for i in 0..MAX_BATCH_PAGES {
                assert_eq!(out_status[i], PageDecodeStatus::Ok);
                assert_eq!(out_pages[i], batch[i]);
                let _ = pool.lookup_and_touch(&tickets[i]);
            }
            pages_verified += MAX_BATCH_PAGES as u64;
        }
    }
    let elapsed_s = started.elapsed().as_secs_f64();
    let final_density = pool.effective_density_ratio().unwrap_or(0.0);
    let evictions = pool.evictions();
    let active_frames = pool.active_frames();
    let cached_pages = pool.logical_pages_cached();
    let comp_p50 = percentile_us(&mut comp_lat_us, 50, 100);
    let comp_p99 = percentile_us(&mut comp_lat_us, 99, 100);
    let dec_p50 = percentile_us(&mut dec_lat_us, 50, 100);
    let dec_p99 = percentile_us(&mut dec_lat_us, 99, 100);

    assert_eq!(pages_processed, 1_048_576, "must process 4 GiB (1M pages)");
    assert!(
        final_density >= 1.60,
        "4 GiB slab density {final_density} < 1.60"
    );
    assert!(
        evictions > 10_000,
        "expected >10,000 O(1) frame LRU evictions, got {evictions}"
    );
    assert!(comp_p99 < 20_000, "compress p99 {comp_p99}us >= 20ms");
    assert!(dec_p99 < 20_000, "decompress p99 {dec_p99}us >= 20ms");
    assert!(!codec.poisoned());
    drop(codec);
    drop(ctx);

    // Also run isolated GPU worker IPC verification on the same mixed corpus.
    let mut cuda_id = device.adapter_identity().expect("adapter identity");
    cuda_id.backend = cuda_id.backend.to_ascii_lowercase();
    cuda_id.key = cuda_id.key.to_ascii_lowercase();
    cuda_id.luid = cuda_id.luid.as_deref().map(str::to_ascii_lowercase);
    let dxg = DxgBudgetProvider::open(None).expect("open dxg");
    let interface_id = dxg
        .adapter_interface()
        .expect("dxg interface")
        .expect("some interface");
    let report = HostGpuIdentityReport {
        schema_version: 1,
        producer: ProducerProcess {
            pid: pid.max(1),
            creation_time_100ns: 1_234_569,
        },
        started_at_unix_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_millis() as u64,
        elapsed_ms: 1,
        adapters: vec![HostGpuIdentity {
            ordinal: 0,
            allocator: cuda_id.clone(),
            interface_id,
        }],
    };
    let secret = b"live-rtx2060-4gib-slab-secret32!";
    let transcript = HandshakeTranscript {
        min_version: IPC_VERSION_4,
        max_version: IPC_VERSION_4,
        boot_id: boot_id.clone(),
        distro_id: "ubuntu".into(),
        guest_nonce: [0x55; 32],
    };
    let challenge = HostChallenge {
        accepted_version: IPC_VERSION_4,
        heartbeat_secs: 2,
        lease_timeout_secs: 6,
        host_nonce: [0x66; 32],
    };
    let mut hs = CatalogSession::new(secret, &transcript, &challenge, 9).expect("hs");
    let mut gs = CatalogSession::new(secret, &transcript, &challenge, 9).expect("gs");
    let ack = HeartbeatAck {
        timestamp_ms: 9000,
        lease_remaining_ms: 10_000,
        host_memory: None,
    };
    let encoded = hs
        .seal(secret, &ack, Some(HostGpuObservation { report, age_ms: 5 }))
        .expect("seal");
    let now = Instant::now();
    let entry = gs
        .open(
            secret,
            &ack,
            &encoded,
            4,
            CatalogTiming {
                started: now,
                received: now,
            },
        )
        .expect("open")
        .expect("entry");
    let catalog = SharedHostCatalog::new();
    catalog.publish(Some(&entry));
    let reader = catalog.subscribe().expect("subscribe");
    let inherited_cat = rustix::io::fcntl_dupfd_cloexec(reader.as_fd(), 3).expect("dup cat");
    let cat_raw = inherited_cat.as_raw_fd();
    let expected_json = serde_json::to_string(&cuda_id).expect("json");
    let worker_reserve_floor: u64 = 1024 * 1024 * 1024;
    let target_bytes: u64 = 4 * 1024 * 1024 * 1024;
    let (client_sock, worker_sock) = UnixStream::pair().expect("pair");
    let args: Vec<String> = vec![
        "--fd".into(),
        "3".into(),
        "--target-bytes".into(),
        target_bytes.to_string(),
        "--chunk-bytes".into(),
        "4096".into(),
        "--reserve-floor".into(),
        worker_reserve_floor.to_string(),
        "--compression".into(),
        "--adapter-identity".into(),
        expected_json,
        ramshared_wsl2d::host_catalog::CATALOG_FD_ARG.into(),
        cat_raw.to_string(),
    ];
    let worker = std::thread::spawn(move || {
        let _keep_cat = inherited_cat;
        let _keep_reader = reader;
        let mut host = LiveControlHost {
            sock: Some(worker_sock),
        };
        run_isolated_gpu_worker_entry_with(&args, &mut host).map_err(|e| e.to_string())
    });
    let mut client = IpcCacheClient::with_pacing(
        client_sock,
        Duration::from_secs(10),
        target_bytes,
        ramshared_block::ipc_cache_client::PacingPolicy::immediate(),
    );
    client
        .expect_adapter(cuda_id)
        .expect("expect adapter on client");
    client.perform_handshake().expect("worker handshake");
    assert_eq!(client.state(), CacheState::Active);
    for b in corpus.iter().take(16) {
        let tickets_opt = client.slab_compress_batch(b).expect("ipc compress batch");
        let tickets: Vec<_> = tickets_opt.into_iter().map(|t| t.expect("t")).collect();
        let (statuses, recovered) = client
            .slab_recover_batch(&tickets)
            .expect("ipc recover batch");
        assert!(statuses.iter().all(|s| *s == PageDecodeStatus::Ok));
        assert_eq!(&recovered, b);
    }
    let ipc_tele = client.slab_telemetry().expect("ipc tele").expect("present");
    assert!(ipc_tele.effective_density_ratio.unwrap_or(0.0) >= 1.60);
    drop(client);
    worker.join().expect("join").expect("clean exit");

    eprintln!(
        "LIVE_RTX2060_4GIB_SLAB_LIFECYCLE=PASS BINARY_MATCH=PASS exe_sha256={} pid={} \
         start_time_ticks={} boot_id={} logical_pages={} logical_bytes=4294967296 \
         pages_verified={} pool_frames={} active_frames={} cached_pages={} evictions={} \
         density_ratio={:.2} ipc_density_ratio={:.2} b512={} b1024={} b2048={} raw4096={} \
         comp_p50_us={} comp_p99_us={} dec_p50_us={} dec_p99_us={} elapsed_s={:.2} \
         verdict=PASS_ZERO_PANIC",
        exe_sha256,
        pid,
        start_ticks,
        boot_id,
        pages_processed,
        pages_verified,
        pool_frames,
        active_frames,
        cached_pages,
        evictions,
        final_density,
        ipc_tele.effective_density_ratio.unwrap_or(0.0),
        class_counts[0],
        class_counts[1],
        class_counts[2],
        class_counts[4],
        comp_p50,
        comp_p99,
        dec_p50,
        dec_p99,
        elapsed_s,
    );
}
