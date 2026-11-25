// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/tests/build5_three_tier_bench.rs
//! Controlled 3-round Build #5 same-load qualification benchmark comparing:
//! 1. Tier 3 SSD direct/uncached 4 KiB I/O baseline + live Tier 1 ZRAM & fallback swap telemetry
//! 2. Production `AuthoritativeOriginBackend` NBD over Unix socket (origin-only, cache disabled)
//! 3. Production `AuthoritativeOriginBackend` NBD over Unix socket backed by the live RTX 2060
//!    CUDA PTX LZ4 isolated GPU cache worker (write-through to origin + compressed VRAM hits)
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::num::NonZeroU64;
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ramshared_block::ipc_cache_client::PacingPolicy;
use ramshared_block::{
    AllowCacheAdmission, AuthoritativeOriginBackend, BestEffortCache, CacheReleaseOutcome, Command,
    DisabledCache, FileOrigin, GpuWorkerConfig, IpcCacheClient, RefuseCacheAdmission,
    parse_request, serve, server_handshake,
};
use ramshared_cuda::{Cuda, CudaPtxLz4Codec};
use ramshared_vram::ReserveFloorPolicy;
use ramshared_vram::device_codec::{GpuDeviceCodec, PAGE_BYTES, PageDecodeStatus};
use ramshared_vram::slab_pool::VramSlabPool;
use rustix::fs::{Advice, fadvise};
use serde::Serialize;
use sha2::{Digest, Sha256};

const MIB: u64 = 1024 * 1024;
const ROUNDS: usize = 3;
const WORKING_SET_PAGES: usize = 1024; // 4 MiB per arm per round
const CLUSTER_PAGES: usize = 16; // 64 KiB write cluster (Linux swap cluster & MAX_CACHE_MUTATION_BYTES)
const CLUSTER_BYTES: usize = CLUSTER_PAGES * PAGE_BYTES;
const READ_OPS_PER_ROUND: usize = WORKING_SET_PAGES; // 1,024 unique shuffled 4 KiB reads (zero page-cache repeat aliasing)
const WORKING_SET_BYTES: u64 = (WORKING_SET_PAGES * PAGE_BYTES) as u64;

#[derive(Serialize)]
struct ArmIoStats {
    write_mib_s: f64,
    read_mib_s: f64,
    read_p50_us: f64,
    read_p99_us: f64,
    origin_written_bytes: u64,
    origin_syncs: u64,
    cache_hits: u64,
    cache_fallback_reads: u64,
    cache_read_bytes: u64,
    logical_cached_bytes: u64,
    compressed_payload_bytes: u64,
    physical_cache_slab_bytes: u64,
    codec_workspace_bytes: u64,
    vram_density_ratio: f64,
    reclaim_teardown_ms: f64,
    mismatches: u64,
}

#[derive(Serialize)]
struct ZramSnapshot {
    device: String,
    orig_data_bytes: u64,
    compr_data_bytes: u64,
    mem_used_bytes: u64,
    ratio: f64,
}

#[derive(Serialize)]
struct RoundReport {
    round: usize,
    ssd_direct: ArmIoStats,
    origin_only_nbd: ArmIoStats,
    vram_cache_raw_rtx2060_nbd: ArmIoStats,
    vram_cache_rtx2060_nbd: ArmIoStats,
    tier1_zram: Option<ZramSnapshot>,
    tier3_fallback_swap_kib_before: u64,
    tier3_fallback_swap_kib_after: u64,
    tier3_fallback_swap_delta_kib: i64,
    mem_available_kib_before: u64,
    mem_available_kib_after: u64,
    pgmajfault_delta: u64,
    psi_some_avg10: f64,
    psi_full_avg10: f64,
}

#[derive(Serialize)]
struct BenchOutput {
    exe_sha256: String,
    gpu_name: String,
    total_vram_bytes: u64,
    free_vram_before_bytes: u64,
    free_vram_after_bytes: u64,
    sealed_reserve_floor_bytes: u64,
    working_set_pages: usize,
    read_ops_per_round: usize,
    rounds: Vec<RoundReport>,
    refusal_budget_below_reserve: bool,
    refusal_gpu_crc32_tamper: bool,
    residue_files: usize,
}

fn make_corpus(pages: usize, seed_base: u64) -> Vec<[u8; PAGE_BYTES]> {
    let mut out = Vec::with_capacity(pages);
    for idx in 0..pages {
        let mut page = [0u8; PAGE_BYTES];
        let bucket = idx % 64;
        let prefix = if bucket < 26 {
            128
        } else if bucket < 45 {
            768
        } else if bucket < 55 {
            1536
        } else {
            PAGE_BYTES
        };
        let mut state = seed_base
            .wrapping_mul(131)
            .wrapping_add(idx as u64 + 1)
            .wrapping_mul(0x517C_C1B7_2722_0A95);
        for b in page.iter_mut().take(prefix) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *b = (state >> 24) as u8;
        }
        out.push(page);
    }
    out
}

fn make_read_order(pages: usize, seed: u64) -> Vec<usize> {
    let mut state = seed ^ 0x9E37_79B9_7F4A_7C15;
    let mut order: Vec<usize> = (0..pages).collect();
    // Deterministic Fisher-Yates permutation so each 4 KiB page is read once
    // without repeat page-cache aliasing on the uncached SSD/origin arms.
    for i in (1..pages).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let j = (state as usize) % (i + 1);
        order.swap(i, j);
    }
    order
}

fn percentile_us(samples: &mut [f64], pct: f64) -> f64 {
    samples.sort_by(f64::total_cmp);
    let rank = ((pct / 100.0) * samples.len() as f64).ceil() as usize;
    samples[rank.saturating_sub(1).min(samples.len() - 1)]
}

fn median_us(samples: &mut [f64]) -> f64 {
    samples.sort_by(f64::total_cmp);
    let n = samples.len();
    if n.is_multiple_of(2) {
        (samples[n / 2 - 1] + samples[n / 2]) / 2.0
    } else {
        samples[n / 2]
    }
}

fn read_fallback_swap_kib() -> u64 {
    let Ok(text) = std::fs::read_to_string("/proc/swaps") else {
        return 0;
    };
    text.lines()
        .skip(1)
        .filter(|line| !line.contains("zram") && !line.contains("nbd"))
        .filter_map(|line| line.split_whitespace().nth(3)?.parse::<u64>().ok())
        .sum()
}

fn read_zram_snapshot() -> Option<ZramSnapshot> {
    for entry in std::fs::read_dir("/sys/block").ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("zram") {
            continue;
        }
        let stat = std::fs::read_to_string(entry.path().join("mm_stat")).ok()?;
        let cols: Vec<u64> = stat
            .split_whitespace()
            .filter_map(|v| v.parse().ok())
            .collect();
        if cols.len() >= 3 && cols[0] > 0 && cols[1] > 0 {
            return Some(ZramSnapshot {
                device: name,
                orig_data_bytes: cols[0],
                compr_data_bytes: cols[1],
                mem_used_bytes: cols[2],
                ratio: cols[0] as f64 / cols[1] as f64,
            });
        }
    }
    None
}

fn read_mem_available_kib() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("MemAvailable:"))?
                .split_whitespace()
                .nth(1)?
                .parse()
                .ok()
        })
        .unwrap_or(0)
}

fn read_pgmajfault() -> u64 {
    std::fs::read_to_string("/proc/vmstat")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("pgmajfault "))?
                .split_whitespace()
                .nth(1)?
                .parse()
                .ok()
        })
        .unwrap_or(0)
}

fn read_psi_memory() -> (f64, f64) {
    let Ok(text) = std::fs::read_to_string("/proc/pressure/memory") else {
        return (0.0, 0.0);
    };
    let parse_avg10 = |prefix: &str| -> f64 {
        text.lines()
            .find(|l| l.starts_with(prefix))
            .and_then(|l| l.split("avg10=").nth(1))
            .and_then(|s| s.split_whitespace().next())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.0)
    };
    (parse_avg10("some "), parse_avg10("full "))
}

fn create_temp_backing(path: &Path, size: u64) -> File {
    let _ = std::fs::remove_file(path);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
        .expect("create backing file");
    file.set_len(size).expect("set backing len");
    file
}

fn run_ssd_direct_arm(path: &Path, corpus: &[[u8; PAGE_BYTES]], order: &[usize]) -> ArmIoStats {
    let file = create_temp_backing(path, WORKING_SET_BYTES);
    let mut cluster_buf = vec![0u8; CLUSTER_BYTES];
    let w_start = Instant::now();
    for (cluster_idx, pages) in corpus.chunks(CLUSTER_PAGES).enumerate() {
        for (i, page) in pages.iter().enumerate() {
            cluster_buf[i * PAGE_BYTES..(i + 1) * PAGE_BYTES].copy_from_slice(page);
        }
        file.write_all_at(&cluster_buf, (cluster_idx * CLUSTER_BYTES) as u64)
            .expect("ssd write");
    }
    file.sync_data().expect("ssd sync");
    let w_sec = w_start.elapsed().as_secs_f64();

    // Disable readahead and evict clean page cache so every 4 KiB read hits ext4-VHDX.
    fadvise(&file, 0, NonZeroU64::new(WORKING_SET_BYTES), Advice::Random).expect("fadvise random");
    fadvise(
        &file,
        0,
        NonZeroU64::new(WORKING_SET_BYTES),
        Advice::DontNeed,
    )
    .expect("fadvise dontneed");

    let mut lats = Vec::with_capacity(order.len());
    let mut buf = [0u8; PAGE_BYTES];
    let mut mismatches = 0u64;
    let r_start = Instant::now();
    for &idx in order {
        let t0 = Instant::now();
        file.read_exact_at(&mut buf, (idx * PAGE_BYTES) as u64)
            .expect("ssd read");
        lats.push(t0.elapsed().as_secs_f64() * 1e6);
        if buf != corpus[idx] {
            mismatches += 1;
        }
    }
    let r_sec = r_start.elapsed().as_secs_f64();
    drop(file);
    std::fs::remove_file(path).expect("unlink ssd temp");

    let write_mib = WORKING_SET_BYTES as f64 / MIB as f64;
    let read_mib = (order.len() * PAGE_BYTES) as f64 / MIB as f64;
    ArmIoStats {
        write_mib_s: write_mib / w_sec.max(1e-9),
        read_mib_s: read_mib / r_sec.max(1e-9),
        read_p50_us: median_us(&mut lats),
        read_p99_us: percentile_us(&mut lats, 99.0),
        origin_written_bytes: WORKING_SET_BYTES,
        origin_syncs: 1,
        cache_hits: 0,
        cache_fallback_reads: order.len() as u64,
        cache_read_bytes: 0,
        logical_cached_bytes: 0,
        compressed_payload_bytes: 0,
        physical_cache_slab_bytes: 0,
        codec_workspace_bytes: 0,
        vram_density_ratio: 0.0,
        reclaim_teardown_ms: 0.0,
        mismatches,
    }
}

fn nbd_client_handshake(sock: &mut UnixStream) {
    let mut greeting = [0u8; 18];
    sock.read_exact(&mut greeting).expect("greeting");
    sock.write_all(&2u32.to_be_bytes()).expect("flags");
    sock.write_all(&ramshared_block::protocol::IHAVEOPT.to_be_bytes())
        .expect("ihaveopt");
    sock.write_all(&ramshared_block::handshake::NBD_OPT_EXPORT_NAME.to_be_bytes())
        .expect("opt");
    sock.write_all(&7u32.to_be_bytes()).expect("name len");
    sock.write_all(b"default").expect("name");
    let mut export = [0u8; 10];
    sock.read_exact(&mut export).expect("export");
}

fn nbd_send_req(sock: &mut UnixStream, cmd: u16, handle: u64, offset: u64, len: u32, data: &[u8]) {
    let mut hdr = [0u8; 28];
    hdr[..4].copy_from_slice(&ramshared_block::protocol::NBD_REQUEST_MAGIC.to_be_bytes());
    hdr[6..8].copy_from_slice(&cmd.to_be_bytes());
    hdr[8..16].copy_from_slice(&handle.to_be_bytes());
    hdr[16..24].copy_from_slice(&offset.to_be_bytes());
    hdr[24..28].copy_from_slice(&len.to_be_bytes());
    sock.write_all(&hdr).expect("nbd req hdr");
    if !data.is_empty() {
        sock.write_all(data).expect("nbd req data");
    }
}

fn nbd_recv_reply(sock: &mut UnixStream, expected_handle: u64, read_buf: Option<&mut [u8]>) {
    let mut reply = [0u8; 16];
    sock.read_exact(&mut reply).expect("nbd reply");
    assert_eq!(u32::from_be_bytes(reply[4..8].try_into().unwrap()), 0);
    assert_eq!(&reply[8..], &expected_handle.to_be_bytes());
    if let Some(buf) = read_buf {
        sock.read_exact(buf).expect("nbd read payload");
    }
}

fn run_nbd_arm<C, A>(
    origin_path: &Path,
    cache: C,
    admission: A,
    corpus: &[[u8; PAGE_BYTES]],
    order: &[usize],
    evict_origin_before_reads: bool,
) -> (ArmIoStats, AuthoritativeOriginBackend<FileOrigin, C>)
where
    C: ramshared_block::BestEffortCache + Send + 'static,
    A: ramshared_block::CacheAdmission + Send + 'static,
{
    let origin_file = create_temp_backing(origin_path, WORKING_SET_BYTES);
    let evict_fd = origin_file.try_clone().expect("clone origin fd");
    let backend = AuthoritativeOriginBackend::new_with_admission(
        FileOrigin::from_file(origin_file),
        cache,
        WORKING_SET_BYTES,
        PAGE_BYTES as u32,
        admission,
    )
    .expect("origin backend");

    let (mut client_sock, mut server_sock) = UnixStream::pair().expect("nbd sock pair");
    client_sock
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    server_sock
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();

    let write_clusters = corpus.len() / CLUSTER_PAGES;
    let total_ops = write_clusters + 1 + order.len();
    let server = std::thread::spawn(move || {
        let mut reader = server_sock.try_clone().unwrap();
        let exports = [ramshared_block::handshake::Export::production(
            WORKING_SET_BYTES,
        )];
        server_handshake(&mut reader, &mut server_sock, &exports, 1).expect("nbd handshake");
        let mut backend = backend;
        let mut writes_seen = 0usize;
        for _ in 0..total_ops {
            let mut header = [0u8; 28];
            reader.read_exact(&mut header).expect("req hdr");
            let req = parse_request(&header).expect("parse req");
            let mut data = if req.cmd == Command::Write {
                vec![0u8; req.len as usize]
            } else {
                Vec::new()
            };
            if !data.is_empty() {
                reader.read_exact(&mut data).expect("req data");
            }
            let out = serve(&req, &data, &mut backend);
            if req.cmd == Command::Write {
                writes_seen += 1;
                // Keep queued IPC mutations below the Unix socket buffer ceiling so
                // every 64 KiB write cluster commits to VRAM without drops.
                if writes_seen.is_multiple_of(2) {
                    let _ = backend.refresh_cached_bytes();
                }
            } else if req.cmd == Command::Flush {
                let _ = backend.refresh_cached_bytes();
            }
            server_sock.write_all(&out.reply).expect("write reply");
            if !out.read_data.is_empty() {
                server_sock
                    .write_all(&out.read_data)
                    .expect("write read_data");
            }
        }
        let _ = backend.refresh_cached_bytes();
        backend
    });

    nbd_client_handshake(&mut client_sock);
    let mut handle = 1u64;
    let mut cluster_buf = vec![0u8; CLUSTER_BYTES];
    let w_start = Instant::now();
    for (cluster_idx, pages) in corpus.chunks(CLUSTER_PAGES).enumerate() {
        for (i, page) in pages.iter().enumerate() {
            cluster_buf[i * PAGE_BYTES..(i + 1) * PAGE_BYTES].copy_from_slice(page);
        }
        nbd_send_req(
            &mut client_sock,
            1,
            handle,
            (cluster_idx * CLUSTER_BYTES) as u64,
            CLUSTER_BYTES as u32,
            &cluster_buf,
        );
        nbd_recv_reply(&mut client_sock, handle, None);
        handle += 1;
    }
    // NBD_CMD_FLUSH (cmd=3) guarantees durable write-through to authoritative SSD origin.
    nbd_send_req(&mut client_sock, 3, handle, 0, 0, &[]);
    nbd_recv_reply(&mut client_sock, handle, None);
    handle += 1;
    let w_sec = w_start.elapsed().as_secs_f64();

    if evict_origin_before_reads {
        fadvise(
            &evict_fd,
            0,
            NonZeroU64::new(WORKING_SET_BYTES),
            Advice::Random,
        )
        .expect("fadvise origin random");
        fadvise(
            &evict_fd,
            0,
            NonZeroU64::new(WORKING_SET_BYTES),
            Advice::DontNeed,
        )
        .expect("evict origin cache");
    }
    drop(evict_fd);

    let mut lats = Vec::with_capacity(order.len());
    let mut buf = [0u8; PAGE_BYTES];
    let mut mismatches = 0u64;
    let r_start = Instant::now();
    for &idx in order {
        let t0 = Instant::now();
        nbd_send_req(
            &mut client_sock,
            0,
            handle,
            (idx * PAGE_BYTES) as u64,
            PAGE_BYTES as u32,
            &[],
        );
        nbd_recv_reply(&mut client_sock, handle, Some(&mut buf));
        lats.push(t0.elapsed().as_secs_f64() * 1e6);
        handle += 1;
        if buf != corpus[idx] {
            mismatches += 1;
        }
    }
    let r_sec = r_start.elapsed().as_secs_f64();
    let backend = server.join().expect("nbd server join");
    let tele = backend.telemetry();
    let write_mib = WORKING_SET_BYTES as f64 / MIB as f64;
    let read_mib = (order.len() * PAGE_BYTES) as f64 / MIB as f64;
    let stats = ArmIoStats {
        write_mib_s: write_mib / w_sec.max(1e-9),
        read_mib_s: read_mib / r_sec.max(1e-9),
        read_p50_us: median_us(&mut lats),
        read_p99_us: percentile_us(&mut lats, 99.0),
        origin_written_bytes: tele.origin_written_bytes,
        origin_syncs: tele.origin_syncs,
        cache_hits: tele.cache_hits,
        cache_fallback_reads: tele.fallback_reads,
        cache_read_bytes: tele.cache_read_bytes,
        logical_cached_bytes: 0,
        compressed_payload_bytes: 0,
        physical_cache_slab_bytes: 0,
        codec_workspace_bytes: 0,
        vram_density_ratio: 0.0,
        reclaim_teardown_ms: 0.0,
        mismatches,
    };
    (stats, backend)
}

#[test]
#[ignore = "controlled 3-round Build #5 same-load qualification (ZRAM/SSD, origin-only, RTX 2060 VRAM-cache)"]
fn build5_three_tier_same_load_qualification_three_rounds() {
    let exe_bytes = std::fs::read("/proc/self/exe").expect("read /proc/self/exe");
    let exe_sha256: String = Sha256::digest(&exe_bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    let cuda = Cuda::load().expect("CUDA driver on RTX 2060 host");
    let device = cuda.device(0).expect("CUDA device 0");
    let gpu_name = device.name().to_owned();
    let ctx = cuda.create_context(&device).expect("CUDA context");
    let (free_before, total_vram) = ctx.mem_info().expect("CUDA mem_info");
    let sealed_reserve = ReserveFloorPolicy::sealed().configured_reserve_bytes(total_vram as u64);

    // Refusal 1: budget below codec reserve must fail closed.
    let refusal_budget_below_reserve =
        CudaPtxLz4Codec::new_context_codec(&ctx, 1024 * 1024, 8).is_err();
    assert!(refusal_budget_below_reserve);

    // Refusal 2: tampered CRC32 ticket must be rejected by GPU PTX CRC32 kernel.
    let mut probe_pool = VramSlabPool::new(8).expect("probe pool");
    let mut probe_codec =
        CudaPtxLz4Codec::new_context_codec(&ctx, 32 * MIB, 8).expect("probe codec");
    let sample_page = [0x42u8; PAGE_BYTES];
    let mut tickets = [None; 1];
    probe_codec
        .compress_and_pack_batch(&[sample_page], &mut probe_pool, &mut tickets)
        .expect("probe pack");
    let mut tampered = tickets[0].expect("ticket");
    tampered.payload_crc32 ^= 0x0000_0001;
    let mut out_pages = [[0u8; PAGE_BYTES]; 1];
    let mut out_status = [PageDecodeStatus::Ok; 1];
    probe_codec
        .verify_and_unpack_batch(&[tampered], &mut out_pages, &mut out_status)
        .expect("gpu verify");
    let refusal_gpu_crc32_tamper = out_status[0] == PageDecodeStatus::PreDecodeCrcMismatch;
    assert!(refusal_gpu_crc32_tamper);
    drop(probe_codec);
    drop(ctx);

    let pid = std::process::id();
    let mut temp_paths: Vec<PathBuf> = Vec::new();
    let mut rounds = Vec::with_capacity(ROUNDS);

    for round in 0..ROUNDS {
        let corpus = make_corpus(WORKING_SET_PAGES, 1000 + round as u64);
        let order = make_read_order(WORKING_SET_PAGES, 9000 + round as u64);
        let swap_before = read_fallback_swap_kib();
        let mem_before = read_mem_available_kib();
        let maj_before = read_pgmajfault();

        // Arm 1: Tier 3 SSD direct/uncached 4 KiB baseline
        let ssd_path = std::env::temp_dir().join(format!("ramshared-b5-ssd-{pid}-r{round}.bin"));
        temp_paths.push(ssd_path.clone());
        let ssd_direct = run_ssd_direct_arm(&ssd_path, &corpus, &order);
        assert_eq!(ssd_direct.mismatches, 0);

        // Arm 2: Production AuthoritativeOriginBackend NBD (origin-only, cache disabled)
        let orig_path = std::env::temp_dir().join(format!("ramshared-b5-orig-{pid}-r{round}.bin"));
        temp_paths.push(orig_path.clone());
        let (origin_only_nbd, orig_backend) = run_nbd_arm(
            &orig_path,
            DisabledCache,
            RefuseCacheAdmission,
            &corpus,
            &order,
            true,
        );
        drop(orig_backend);
        std::fs::remove_file(&orig_path).expect("unlink orig");
        assert_eq!(origin_only_nbd.mismatches, 0);
        assert_eq!(origin_only_nbd.cache_hits, 0);

        // Arm 3a: Production AuthoritativeOriginBackend NBD + live RTX 2060 RAW VRAM cache
        let vram_raw_path =
            std::env::temp_dir().join(format!("ramshared-b5-vram-raw-{pid}-r{round}.bin"));
        temp_paths.push(vram_raw_path.clone());
        let (raw_client_sock, raw_peer_sock) = UnixStream::pair().expect("raw worker pair");
        let raw_worker = std::thread::spawn(move || {
            let c = Cuda::load().expect("raw worker cuda");
            let dev = c.device(0).expect("raw worker dev");
            let wctx = c.create_context(&dev).expect("raw worker ctx");
            ramshared_block::run_gpu_worker_loop(
                raw_peer_sock,
                wctx,
                GpuWorkerConfig {
                    target_bytes: 128 * MIB,
                    chunk_bytes: (2 * MIB) as usize,
                    reserve_floor_bytes: sealed_reserve,
                    compression_enabled: false,
                },
            )
        });
        let mut raw_client = IpcCacheClient::with_pacing(
            raw_client_sock,
            Duration::from_secs(5),
            128 * MIB,
            PacingPolicy::immediate(),
        );
        raw_client
            .perform_handshake()
            .expect("raw worker handshake");
        assert!(
            raw_client.target_bytes() > 0,
            "RTX 2060 raw budget must admit cache"
        );
        let (mut vram_raw_stats, vram_raw_backend) = run_nbd_arm(
            &vram_raw_path,
            raw_client,
            AllowCacheAdmission,
            &corpus,
            &order,
            true,
        );
        assert_eq!(vram_raw_stats.mismatches, 0);
        assert_eq!(vram_raw_stats.cache_hits, order.len() as u64);
        vram_raw_stats.logical_cached_bytes = WORKING_SET_BYTES;
        vram_raw_stats.physical_cache_slab_bytes = vram_raw_backend.cached_bytes();
        vram_raw_stats.vram_density_ratio = 1.0;
        let raw_rel_t0 = Instant::now();
        assert_eq!(
            vram_raw_backend.release_cache().expect("raw release_cache"),
            CacheReleaseOutcome::Revoked
        );
        vram_raw_stats.reclaim_teardown_ms = raw_rel_t0.elapsed().as_secs_f64() * 1e3;
        drop(vram_raw_backend);
        raw_worker
            .join()
            .expect("raw worker join")
            .expect("raw worker exit");
        std::fs::remove_file(&vram_raw_path).expect("unlink vram raw orig");

        // Arm 3b: Production AuthoritativeOriginBackend NBD + live RTX 2060 CUDA PTX LZ4 worker
        let vram_path = std::env::temp_dir().join(format!("ramshared-b5-vram-{pid}-r{round}.bin"));
        temp_paths.push(vram_path.clone());
        let (worker_client_sock, worker_peer_sock) = UnixStream::pair().expect("worker pair");
        let worker = std::thread::spawn(move || {
            let c = Cuda::load().expect("worker cuda");
            let dev = c.device(0).expect("worker dev");
            let wctx = c.create_context(&dev).expect("worker ctx");
            ramshared_block::run_gpu_worker_loop_with_device_codec(
                worker_peer_sock,
                wctx,
                GpuWorkerConfig {
                    target_bytes: 128 * MIB,
                    chunk_bytes: (2 * MIB) as usize,
                    reserve_floor_bytes: sealed_reserve,
                    compression_enabled: true,
                },
                |wctx, available, frames| {
                    CudaPtxLz4Codec::new_context_codec(wctx, available, frames)
                        .map(|codec| Box::new(codec) as Box<dyn GpuDeviceCodec + '_>)
                        .map_err(|e| format!("{e:?}"))
                },
            )
        });
        let mut client = IpcCacheClient::with_pacing(
            worker_client_sock,
            Duration::from_secs(5),
            128 * MIB,
            PacingPolicy::immediate(),
        );
        client.perform_handshake().expect("worker handshake");
        assert!(
            client.target_bytes() > 0,
            "RTX 2060 budget must admit cache"
        );

        let (mut vram_stats, vram_backend) = run_nbd_arm(
            &vram_path,
            client,
            AllowCacheAdmission,
            &corpus,
            &order,
            true,
        );
        assert_eq!(vram_stats.mismatches, 0);
        let raw_tele = vram_backend.telemetry();
        assert!(
            vram_stats.cache_hits > 0,
            "VRAM cache must serve hits (tele={raw_tele:?}, cache_tele={:?})",
            vram_backend.cache_telemetry()
        );

        let cache_tele = vram_backend.cache_telemetry().expect("codec telemetry");
        vram_stats.logical_cached_bytes = cache_tele.logical_cached_bytes;
        vram_stats.compressed_payload_bytes = cache_tele.compressed_payload_bytes;
        vram_stats.physical_cache_slab_bytes = cache_tele.physical_cache_slab_bytes;
        vram_stats.codec_workspace_bytes = cache_tele.codec_workspace_bytes;
        vram_stats.vram_density_ratio = if cache_tele.compressed_payload_bytes > 0 {
            cache_tele.logical_cached_bytes as f64 / cache_tele.compressed_payload_bytes as f64
        } else {
            0.0
        };

        let rel_t0 = Instant::now();
        assert_eq!(
            vram_backend.release_cache().expect("release_cache"),
            CacheReleaseOutcome::Revoked
        );
        vram_stats.reclaim_teardown_ms = rel_t0.elapsed().as_secs_f64() * 1e3;
        drop(vram_backend);
        worker.join().expect("worker join").expect("worker exit");
        std::fs::remove_file(&vram_path).expect("unlink vram orig");

        let swap_after = read_fallback_swap_kib();
        let mem_after = read_mem_available_kib();
        let maj_after = read_pgmajfault();
        let (psi_some, psi_full) = read_psi_memory();

        rounds.push(RoundReport {
            round: round + 1,
            ssd_direct,
            origin_only_nbd,
            vram_cache_raw_rtx2060_nbd: vram_raw_stats,
            vram_cache_rtx2060_nbd: vram_stats,
            tier1_zram: read_zram_snapshot(),
            tier3_fallback_swap_kib_before: swap_before,
            tier3_fallback_swap_kib_after: swap_after,
            tier3_fallback_swap_delta_kib: swap_after as i64 - swap_before as i64,
            mem_available_kib_before: mem_before,
            mem_available_kib_after: mem_after,
            pgmajfault_delta: maj_after.saturating_sub(maj_before),
            psi_some_avg10: psi_some,
            psi_full_avg10: psi_full,
        });
    }

    let ctx_after = cuda.create_context(&device).expect("CUDA ctx after");
    let (free_after, _) = ctx_after.mem_info().expect("CUDA mem_info after");
    drop(ctx_after);

    let residue_files = temp_paths.iter().filter(|p| p.exists()).count();
    assert_eq!(residue_files, 0, "all benchmark temp files must be removed");

    let output = BenchOutput {
        exe_sha256,
        gpu_name,
        total_vram_bytes: total_vram as u64,
        free_vram_before_bytes: free_before as u64,
        free_vram_after_bytes: free_after as u64,
        sealed_reserve_floor_bytes: sealed_reserve,
        working_set_pages: WORKING_SET_PAGES,
        read_ops_per_round: READ_OPS_PER_ROUND,
        rounds,
        refusal_budget_below_reserve,
        refusal_gpu_crc32_tamper,
        residue_files,
    };
    println!("BUILD5_BENCH_JSON_BEGIN");
    println!(
        "{}",
        serde_json::to_string_pretty(&output).expect("serialize output")
    );
    println!("BUILD5_BENCH_JSON_END");
}
