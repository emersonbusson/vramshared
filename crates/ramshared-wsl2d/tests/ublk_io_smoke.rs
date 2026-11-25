// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/tests/ublk_io_smoke.rs
#![allow(clippy::unwrap_used, clippy::expect_used)] // test: unwrap/expect is idiomatic

use ramshared_block::BlockBackend;
use ramshared_wsl2d::{RamBackend, ResidencyConfig, ublk, ublk_control, ublk_server};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) const UBLK_CONTROL: &str = "/dev/ublk-control";
pub(crate) const SECTOR: u64 = 512;
const TEST_SECTOR: u64 = 100;

#[test]
#[ignore = "requires root; creates /dev/ublkbN and serves I/O from a RAM backend, no swap"]
fn serves_read_from_ram_backend_over_block_device() {
    let before = ublk_nodes();
    let report = ublk_control::add_device(UBLK_CONTROL, ublk_control::DeviceSpec::smoke_auto())
        .expect("ublk ADD_DEV");
    let mut guard = DeviceGuard::new(report.dev_id);

    let dev_sectors = 2048u64; // 1 MiB
    ublk_control::set_params(
        UBLK_CONTROL,
        report.dev_id,
        ublk::Params::basic_disk(dev_sectors, 9, 9),
    )
    .expect("ublk SET_PARAMS");

    // RAM backend with a known pattern in the test sector (outside the partition scan).
    let mut backend = RamBackend::new((dev_sectors * SECTOR) as usize);
    let pattern: Vec<u8> = (0..SECTOR).map(|i| (i % 251) as u8).collect();
    backend
        .write_at(TEST_SECTOR * SECTOR, &pattern)
        .expect("pre-load the backend");

    let char_path = format!("/dev/ublkc{}", report.dev_id);
    let block_path = format!("/dev/ublkb{}", report.dev_id);

    // Starts the server thread (submits FETCH + loop). It serves the partition scan that
    // START_DEV triggers, so it needs to be alive before/during START_DEV.
    let server = ublk_server::spawn_server(&char_path, report.queue_depth, 4096, backend)
        .expect("spawn server");

    ublk_control::start_dev(UBLK_CONTROL, report.dev_id, std::process::id())
        .expect("ublk START_DEV");
    assert!(
        fs::metadata(&block_path).is_ok(),
        "{block_path} should exist after START_DEV"
    );

    // READ of the test sector via block device -> loop serves from the backend -> pattern.
    let got = read_sector(&block_path, TEST_SECTOR);
    assert_eq!(
        got, pattern,
        "READ must return the pattern written to the backend"
    );

    // Teardown: STOP_DEV removes the gendisk and aborts the FETCHes -> the thread exits the loop.
    ublk_control::stop_dev(UBLK_CONTROL, report.dev_id).expect("ublk STOP_DEV");
    server.join().expect("server loop terminated ok");

    ublk_control::delete_device(UBLK_CONTROL, report.dev_id).expect("ublk DEL_DEV");
    guard.disarm();
    wait_until_missing(&char_path);
    assert_eq!(ublk_nodes(), before);
}

#[test]
#[ignore = "requires root; writes through /dev/ublkbN into the RAM backend, no swap"]
fn serves_write_into_ram_backend_over_block_device() {
    let before = ublk_nodes();
    let report = ublk_control::add_device(UBLK_CONTROL, ublk_control::DeviceSpec::smoke_auto())
        .expect("ublk ADD_DEV");
    let mut guard = DeviceGuard::new(report.dev_id);

    let dev_sectors = 2048u64;
    ublk_control::set_params(
        UBLK_CONTROL,
        report.dev_id,
        ublk::Params::basic_disk(dev_sectors, 9, 9),
    )
    .expect("ublk SET_PARAMS");

    let disk_size = (dev_sectors * SECTOR) as usize;
    let backend = RamBackend::new(disk_size);
    let char_path = format!("/dev/ublkc{}", report.dev_id);
    let block_path = format!("/dev/ublkb{}", report.dev_id);
    // Buffer per tag covers the entire disk: any writeback request fits.
    let server = ublk_server::spawn_server(&char_path, report.queue_depth, disk_size, backend)
        .expect("spawn server");

    ublk_control::start_dev(UBLK_CONTROL, report.dev_id, std::process::id())
        .expect("ublk START_DEV");
    assert!(
        fs::metadata(&block_path).is_ok(),
        "{block_path} should exist"
    );

    // WRITE of a pattern in the test sector via block device + fsync (forces writeback).
    let pattern: Vec<u8> = (0..SECTOR).map(|i| ((i * 7 + 1) % 251) as u8).collect();
    write_sector(&block_path, TEST_SECTOR, &pattern);

    // Teardown: the thread returns the backend for direct inspection (without page cache).
    ublk_control::stop_dev(UBLK_CONTROL, report.dev_id).expect("ublk STOP_DEV");
    let mut backend = server.join().expect("server loop terminated ok");

    let mut got = vec![0u8; SECTOR as usize];
    backend
        .read_at(TEST_SECTOR * SECTOR, &mut got)
        .expect("read the returned backend");
    assert_eq!(got, pattern, "the WRITE must have reached the backend");

    ublk_control::delete_device(UBLK_CONTROL, report.dev_id).expect("ublk DEL_DEV");
    guard.disarm();
    wait_until_missing(&char_path);
    assert_eq!(ublk_nodes(), before);
}

#[test]
#[ignore = "requires root; DT-3 ring owner + worker thread serve I/O, no swap"]
fn dt3_serves_read_from_ram_backend_over_block_device() {
    let before = ublk_nodes();
    let report = ublk_control::add_device(UBLK_CONTROL, ublk_control::DeviceSpec::smoke_auto())
        .expect("ublk ADD_DEV");
    let mut guard = DeviceGuard::new(report.dev_id);

    let dev_sectors = 2048u64;
    ublk_control::set_params(
        UBLK_CONTROL,
        report.dev_id,
        ublk::Params::basic_disk(dev_sectors, 9, 9),
    )
    .expect("ublk SET_PARAMS");

    let mut backend = RamBackend::new((dev_sectors * SECTOR) as usize);
    let pattern: Vec<u8> = (0..SECTOR).map(|i| (i % 251) as u8).collect();
    backend
        .write_at(TEST_SECTOR * SECTOR, &pattern)
        .expect("pre-load the backend");

    let char_path = format!("/dev/ublkc{}", report.dev_id);
    let block_path = format!("/dev/ublkb{}", report.dev_id);

    // DT-3 Architecture: ring owner thread + worker thread (owning the backend).
    let server = ublk_server::spawn_server_dt3(&char_path, report.queue_depth, 4096, backend)
        .expect("spawn DT-3 server");

    ublk_control::start_dev(UBLK_CONTROL, report.dev_id, std::process::id())
        .expect("ublk START_DEV");
    assert!(
        fs::metadata(&block_path).is_ok(),
        "{block_path} should exist"
    );

    let got = read_sector(&block_path, TEST_SECTOR);
    assert_eq!(got, pattern, "DT-3 READ must return the backend pattern");

    ublk_control::stop_dev(UBLK_CONTROL, report.dev_id).expect("ublk STOP_DEV");
    let _backend = server.join().expect("DT-3 server terminated ok");

    ublk_control::delete_device(UBLK_CONTROL, report.dev_id).expect("ublk DEL_DEV");
    guard.disarm();
    wait_until_missing(&char_path);
    assert_eq!(ublk_nodes(), before);
}

#[test]
#[ignore = "requires root + CUDA GPU; serves /dev/ublkbN from VRAM (cuMemcpy), no swap"]
fn dt3_serves_io_from_vram_over_block_device() {
    let before = ublk_nodes();
    let report = ublk_control::add_device(UBLK_CONTROL, ublk_control::DeviceSpec::smoke_auto())
        .expect("ublk ADD_DEV");
    let mut guard = DeviceGuard::new(report.dev_id);

    let block_size = 4096u32;
    let dev_sectors = 2048u64; // 1 MiB
    ublk_control::set_params(
        UBLK_CONTROL,
        report.dev_id,
        ublk::Params::basic_disk(dev_sectors, 12, 12),
    )
    .expect("ublk SET_PARAMS");

    let char_path = format!("/dev/ublkc{}", report.dev_id);
    let block_path = format!("/dev/ublkb{}", report.dev_id);
    let vram_bytes = (dev_sectors * SECTOR) as usize;

    // Worker DT-3 owner of VRAM (creates the CUDA stack on its own thread).
    let server = ublk_server::spawn_server_dt3_vram(
        &char_path,
        report.queue_depth,
        vram_bytes, // buffer per tag = whole disk
        vram_bytes,
        block_size,
    )
    .expect("spawn DT-3 VRAM server");

    ublk_control::start_dev(UBLK_CONTROL, report.dev_id, std::process::id())
        .expect("ublk START_DEV");
    assert!(
        fs::metadata(&block_path).is_ok(),
        "{block_path} should exist"
    );

    // WRITE aligned block -> fsync -> drop cache -> READ must come from VRAM.
    let off = 8192u64; // aligned to block size 4096
    let pattern: Vec<u8> = (0..block_size).map(|i| ((i * 7 + 3) % 251) as u8).collect();
    write_block(&block_path, off, &pattern);
    drop_page_cache();
    let got = read_block(&block_path, off, block_size as usize);
    assert_eq!(got, pattern, "READ must return from VRAM the block written");

    ublk_control::stop_dev(UBLK_CONTROL, report.dev_id).expect("ublk STOP_DEV");
    server.join().expect("DT-3 VRAM server terminated ok");

    ublk_control::delete_device(UBLK_CONTROL, report.dev_id).expect("ublk DEL_DEV");
    guard.disarm();
    wait_until_missing(&char_path);
    assert_eq!(ublk_nodes(), before);
}

#[test]
#[ignore = "requires root + CUDA GPU; serves /dev/ublkbN from VRAM with queue_depth>1 + concurrent O_DIRECT readers, no swap"]
fn dt3_vram_serves_concurrent_io_with_queue_depth_gt1() {
    let before = ublk_nodes();
    // Single queue (only serve queue 0), but queue_depth>1: up to N tags in flight.
    let mut spec = ublk_control::DeviceSpec::smoke_auto();
    spec.queue_depth = 4;
    let report = ublk_control::add_device(UBLK_CONTROL, spec).expect("ublk ADD_DEV");
    let mut guard = DeviceGuard::new(report.dev_id);
    assert!(
        report.queue_depth >= 2,
        "kernel gave queue_depth={}; concurrency test needs >=2",
        report.queue_depth
    );

    let block_size = 4096u32;
    let dev_sectors = 2048u64; // 1 MiB -> 256 blocks of 4 KiB
    ublk_control::set_params(
        UBLK_CONTROL,
        report.dev_id,
        ublk::Params::basic_disk(dev_sectors, 12, 12),
    )
    .expect("ublk SET_PARAMS");

    let char_path = format!("/dev/ublkc{}", report.dev_id);
    let block_path = format!("/dev/ublkb{}", report.dev_id);
    let vram_bytes = (dev_sectors * SECTOR) as usize;

    let server = ublk_server::spawn_server_dt3_vram(
        &char_path,
        report.queue_depth, // no-alloc pool pre-warms `queue_depth` buffers
        vram_bytes,         // buffer per tag (>= largest request possible)
        vram_bytes,
        block_size,
    )
    .expect("spawn DT-3 VRAM server");
    ublk_control::start_dev(UBLK_CONTROL, report.dev_id, std::process::id())
        .expect("ublk START_DEV");

    // Writes 16 blocks with distinct pattern per index and removes from page cache.
    let bs = block_size as usize;
    let n_blocks = 16usize;
    for i in 0..n_blocks {
        write_block(&block_path, (i * bs) as u64, &block_pattern(i, bs));
    }
    drop_page_cache();

    // 4 threads read in round-robin via O_DIRECT, in several rounds, checking each
    // block. With queue_depth>=2 this keeps multiple tags in flight simultaneously —
    // exercises the no-alloc buffer pool with in_flight>1 (aliasing/buffer swap
    // between tags would corrupt data or cause deadlock).
    const O_DIRECT: i32 = 0o40000;
    let workers: Vec<_> = (0..4u64)
        .map(|t| {
            let path = block_path.clone();
            thread::spawn(move || {
                let file = OpenOptions::new()
                    .read(true)
                    .custom_flags(O_DIRECT)
                    .open(&path)
                    .expect("open O_DIRECT");
                let mut raw = vec![0u8; bs * 2];
                let pad = (bs - (raw.as_ptr() as usize % bs)) % bs;
                for _round in 0..64 {
                    let mut i = t as usize;
                    while i < n_blocks {
                        file.read_exact_at(&mut raw[pad..pad + bs], (i * bs) as u64)
                            .expect("read O_DIRECT");
                        assert_eq!(
                            &raw[pad..pad + bs],
                            block_pattern(i, bs).as_slice(),
                            "block {i} corrupted under concurrency qd>1"
                        );
                        i += 4;
                    }
                }
            })
        })
        .collect();
    for w in workers {
        w.join()
            .expect("concurrent reader failed (corruption/deadlock qd>1)");
    }

    ublk_control::stop_dev(UBLK_CONTROL, report.dev_id).expect("ublk STOP_DEV");
    server.join().expect("DT-3 VRAM server terminated ok");
    ublk_control::delete_device(UBLK_CONTROL, report.dev_id).expect("ublk DEL_DEV");
    guard.disarm();
    wait_until_missing(&char_path);
    assert_eq!(ublk_nodes(), before);
}

/// Deterministic and distinct pattern per block (idx shifts the sequence ~+5/byte mod
/// 251, ensuring different blocks detect content swapping between tags).
pub(crate) fn block_pattern(idx: usize, bs: usize) -> Vec<u8> {
    keyed_pattern(idx as u64 * 1009 + 7, bs)
}

/// Deterministic pattern parameterized by a seed (allows varying per write round,
/// forcing the READ to see the latest write — not an old value).
pub(crate) fn keyed_pattern(seed: u64, bs: usize) -> Vec<u8> {
    (0..bs)
        .map(|j| ((j as u64 * 31 + seed) % 251) as u8)
        .collect()
}

#[test]
#[ignore = "requires root + CUDA GPU; concurrent O_DIRECT writes+reads with queue_depth>1, no swap"]
fn dt3_vram_serves_concurrent_writes_with_queue_depth_gt1() {
    let before = ublk_nodes();
    let mut spec = ublk_control::DeviceSpec::smoke_auto();
    spec.queue_depth = 4;
    let report = ublk_control::add_device(UBLK_CONTROL, spec).expect("ublk ADD_DEV");
    let mut guard = DeviceGuard::new(report.dev_id);
    assert!(
        report.queue_depth >= 2,
        "kernel gave queue_depth={}; concurrency test needs >=2",
        report.queue_depth
    );

    let block_size = 4096u32;
    let dev_sectors = 2048u64; // 1 MiB -> 256 blocks of 4 KiB
    ublk_control::set_params(
        UBLK_CONTROL,
        report.dev_id,
        ublk::Params::basic_disk(dev_sectors, 12, 12),
    )
    .expect("ublk SET_PARAMS");

    let char_path = format!("/dev/ublkc{}", report.dev_id);
    let block_path = format!("/dev/ublkb{}", report.dev_id);
    let vram_bytes = (dev_sectors * SECTOR) as usize;
    let server = ublk_server::spawn_server_dt3_vram(
        &char_path,
        report.queue_depth,
        vram_bytes,
        vram_bytes,
        block_size,
    )
    .expect("spawn DT-3 VRAM server");
    ublk_control::start_dev(UBLK_CONTROL, report.dev_id, std::process::id())
        .expect("ublk START_DEV");

    // 4 threads, each exclusive owner of 4 blocks (round-robin, no race between
    // threads on the same block). Each round: WRITE with new pattern via O_DIRECT, then
    // READ back checking. Concurrent writes keep multiple WRITE tags in
    // flight -> exercises the WRITE path of the no-alloc pool (dispatch copies tag_buf->pool
    // buffer; swap between tags would corrupt the block read back).
    const O_DIRECT: i32 = 0o40000;
    let bs = block_size as usize;
    let n_blocks = 16usize;
    let workers: Vec<_> = (0..4u64)
        .map(|t| {
            let path = block_path.clone();
            thread::spawn(move || {
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(O_DIRECT)
                    .open(&path)
                    .expect("open O_DIRECT rw");
                let mut raw = vec![0u8; bs * 2];
                let pad = (bs - (raw.as_ptr() as usize % bs)) % bs;
                for round in 0..32u64 {
                    let mut i = t as usize;
                    while i < n_blocks {
                        let seed = i as u64 * 1009 + round * 13 + 1;
                        let want = keyed_pattern(seed, bs);
                        raw[pad..pad + bs].copy_from_slice(&want);
                        file.write_all_at(&raw[pad..pad + bs], (i * bs) as u64)
                            .expect("write O_DIRECT");
                        // Re-reads from the device (O_DIRECT, no cache) -> must come from VRAM.
                        file.read_exact_at(&mut raw[pad..pad + bs], (i * bs) as u64)
                            .expect("read O_DIRECT");
                        assert_eq!(
                            &raw[pad..pad + bs],
                            want.as_slice(),
                            "block {i} round {round} corrupted under concurrent WRITE qd>1"
                        );
                        i += 4;
                    }
                }
            })
        })
        .collect();
    for w in workers {
        w.join()
            .expect("concurrent writer failed (corruption/deadlock qd>1)");
    }

    ublk_control::stop_dev(UBLK_CONTROL, report.dev_id).expect("ublk STOP_DEV");
    server.join().expect("DT-3 VRAM server terminated ok");
    ublk_control::delete_device(UBLK_CONTROL, report.dev_id).expect("ublk DEL_DEV");
    guard.disarm();
    wait_until_missing(&char_path);
    assert_eq!(ublk_nodes(), before);
}

#[test]
#[ignore = "requires root + CUDA GPU; serves a real >4KB (multi-page) O_DIRECT request from VRAM, no swap"]
fn dt3_vram_serves_multipage_request() {
    let before = ublk_nodes();
    let max_req = 128 * 1024usize; // maximum request of 128 KiB (32 pages)
    // ADD_DEV: the kernel's per-IO buffer limits the largest request (ublk_drv.c:307).
    let mut spec = ublk_control::DeviceSpec::smoke_auto();
    spec.max_io_buf_bytes = max_req as u32;
    let report = ublk_control::add_device(UBLK_CONTROL, spec).expect("ublk ADD_DEV");
    let mut guard = DeviceGuard::new(report.dev_id);

    let block_size = 4096u32;
    let dev_sectors = 2048u64; // 1 MiB
    // max_sectors couples with max_io_buf_bytes (kernel validates <= max_io_buf_bytes>>9)
    // and becomes the max_hw_sectors of the block device (ublk_drv.c:546).
    ublk_control::set_params(
        UBLK_CONTROL,
        report.dev_id,
        ublk::Params::basic_disk(dev_sectors, 12, 12)
            .with_max_sectors((max_req / SECTOR as usize) as u32),
    )
    .expect("ublk SET_PARAMS");

    let char_path = format!("/dev/ublkc{}", report.dev_id);
    let block_path = format!("/dev/ublkb{}", report.dev_id);
    let vram_bytes = (dev_sectors * SECTOR) as usize;
    let server = ublk_server::spawn_server_dt3_vram(
        &char_path,
        report.queue_depth,
        max_req, // buf_size per tag >= largest request possible (couples with the knobs)
        vram_bytes,
        block_size,
    )
    .expect("spawn DT-3 VRAM server");
    ublk_control::start_dev(UBLK_CONTROL, report.dev_id, std::process::id())
        .expect("ublk START_DEV");

    // The device must advertise multi-page capacity (no longer capped at 4KB).
    let hw_kb = read_queue_attr_u64(report.dev_id, "max_hw_sectors_kb");
    assert!(
        hw_kb >= 64,
        "max_hw_sectors_kb={hw_kb}; expected >=64 (multi-page request enabled)"
    );

    // WRITE + READ of a 64KB block (16 pages) via O_DIRECT: a single request
    // of len=65536 > 4096 goes through serve_request -> pool -> worker -> cuMemcpy. If
    // buf_size did not match, serve_request would return EINVAL.
    const O_DIRECT: i32 = 0o40000;
    let big = 64 * 1024usize;
    let bs = block_size as usize;
    let pattern = keyed_pattern(0x5151, big);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(O_DIRECT)
        .open(&block_path)
        .expect("open O_DIRECT rw");
    let mut raw = vec![0u8; big + bs];
    let pad = (bs - (raw.as_ptr() as usize % bs)) % bs;
    raw[pad..pad + big].copy_from_slice(&pattern);
    file.write_all_at(&raw[pad..pad + big], 0)
        .expect("write O_DIRECT 64KB");
    // Zeroes the buffer and re-reads from the device (no cache) -> must come from VRAM.
    raw[pad..pad + big].fill(0);
    file.read_exact_at(&mut raw[pad..pad + big], 0)
        .expect("read O_DIRECT 64KB");
    assert_eq!(
        &raw[pad..pad + big],
        pattern.as_slice(),
        "READ multi-page (64KB) must match WRITE"
    );

    // Closes the block device handle BEFORE STOP_DEV: `del_gendisk` blocks until
    // all openers close, so keeping the fd open would hang the teardown.
    drop(file);

    ublk_control::stop_dev(UBLK_CONTROL, report.dev_id).expect("ublk STOP_DEV");
    server.join().expect("DT-3 VRAM server terminated ok");
    ublk_control::delete_device(UBLK_CONTROL, report.dev_id).expect("ublk DEL_DEV");
    guard.disarm();
    wait_until_missing(&char_path);
    assert_eq!(ublk_nodes(), before);
}

#[test]
#[ignore = "requires root + CUDA GPU; DT-3 worker residency canary fires a synthetic DEMOTE (no real swap)"]
fn dt3_vram_residency_triggers_demote_synthetic() {
    let before = ublk_nodes();
    let report = ublk_control::add_device(UBLK_CONTROL, ublk_control::DeviceSpec::smoke_auto())
        .expect("ublk ADD_DEV");
    let mut guard = DeviceGuard::new(report.dev_id);

    let block_size = 4096u32;
    let dev_sectors = 2048u64;
    ublk_control::set_params(
        UBLK_CONTROL,
        report.dev_id,
        ublk::Params::basic_disk(dev_sectors, 12, 12),
    )
    .expect("ublk SET_PARAMS");

    let char_path = format!("/dev/ublkc{}", report.dev_id);
    let block_path = format!("/dev/ublkb{}", report.dev_id);
    let vram_bytes = (dev_sectors * SECTOR) as usize;

    // SYNTHETIC and deterministic config: latency_mult=0 -> threshold = baseline*0 = 0,
    // so any real serve (lat_us > 0) counts as above; consecutive=1 triggers
    // right after baseline arms (16 samples). free_floor=0 prevents free demote.
    let cfg = ResidencyConfig {
        latency_mult: 0,
        consecutive: 1,
        free_floor_bytes: 0,
    };
    // non-existent swap_dev: swapoff fails (no side effects), but the DEMOTE verdict
    // is counted by the handle -> observable without real swap.
    let server = ublk_server::spawn_server_dt3_vram_with_residency(
        &char_path,
        report.queue_depth,
        vram_bytes,
        vram_bytes,
        block_size,
        "/dev/ramshared-no-such-swap".to_string(),
        cfg,
    )
    .expect("spawn DT-3 VRAM residency server");
    ublk_control::start_dev(UBLK_CONTROL, report.dev_id, std::process::id())
        .expect("ublk START_DEV");

    // Directs O_DIRECT 4KB I/O until the canary triggers DEMOTE (or timeout).
    const O_DIRECT: i32 = 0o40000;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_DIRECT)
        .open(&block_path)
        .expect("open O_DIRECT");
    let bs = block_size as usize;
    let mut raw = vec![0u8; bs * 2];
    let pad = (bs - (raw.as_ptr() as usize % bs)) % bs;
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut off = 0u64;
    while server.demote_count() == 0 && Instant::now() < deadline {
        file.read_exact_at(&mut raw[pad..pad + bs], off)
            .expect("read O_DIRECT");
        off = (off + bs as u64) % vram_bytes as u64;
    }
    let demotes = server.demote_count();

    // Closes the fd before STOP_DEV (del_gendisk waits for openers).
    drop(file);
    ublk_control::stop_dev(UBLK_CONTROL, report.dev_id).expect("ublk STOP_DEV");
    server.join().expect("DT-3 residency server terminated ok");
    ublk_control::delete_device(UBLK_CONTROL, report.dev_id).expect("ublk DEL_DEV");
    guard.disarm();
    wait_until_missing(&char_path);
    assert_eq!(ublk_nodes(), before);

    assert!(
        demotes >= 1,
        "expected >=1 synthetic DEMOTE from canary, got {demotes}"
    );
}

/// Reads a numeric attribute from `/sys/block/ublkb<id>/queue/<attr>`.
fn read_queue_attr_u64(dev_id: u32, attr: &str) -> u64 {
    let path = format!("/sys/block/ublkb{dev_id}/queue/{attr}");
    let s = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    s.trim()
        .parse()
        .unwrap_or_else(|e| panic!("parse {path}={s:?}: {e}"))
}

fn read_sector(path: &str, sector: u64) -> Vec<u8> {
    read_block(path, sector * SECTOR, SECTOR as usize)
}

pub(crate) fn read_block(path: &str, off: u64, len: usize) -> Vec<u8> {
    let mut file = File::open(path).expect("open block device");
    file.seek(SeekFrom::Start(off)).expect("seek");
    let mut buf = vec![0u8; len];
    file.read_exact(&mut buf).expect("read_exact");
    buf
}

pub(crate) fn write_block(path: &str, off: u64, data: &[u8]) {
    let mut file = OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open block device for writing");
    file.seek(SeekFrom::Start(off)).expect("seek");
    file.write_all(data).expect("write_all");
    file.sync_all().expect("sync_all");
}

pub(crate) fn drop_page_cache() {
    let _ = std::process::Command::new("sync").status();
    if let Ok(mut f) = OpenOptions::new()
        .write(true)
        .open("/proc/sys/vm/drop_caches")
    {
        let _ = f.write_all(b"1\n");
    }
}

fn write_sector(path: &str, sector: u64, data: &[u8]) {
    let mut file = OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open block device for writing");
    file.seek(SeekFrom::Start(sector * SECTOR)).expect("seek");
    file.write_all(data).expect("write_all");
    file.sync_all().expect("sync_all");
}

pub(crate) struct DeviceGuard {
    dev_id: Option<u32>,
}

impl DeviceGuard {
    pub(crate) fn new(dev_id: u32) -> Self {
        Self {
            dev_id: Some(dev_id),
        }
    }

    pub(crate) fn disarm(&mut self) {
        self.dev_id = None;
    }
}

impl Drop for DeviceGuard {
    fn drop(&mut self) {
        if let Some(dev_id) = self.dev_id.take() {
            let _ = ublk_control::stop_dev(UBLK_CONTROL, dev_id);
            let _ = ublk_control::delete_device(UBLK_CONTROL, dev_id);
        }
    }
}

pub(crate) fn ublk_nodes() -> Vec<String> {
    let mut nodes = fs::read_dir("/dev")
        .expect("/dev read_dir")
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| {
            name == "ublk-control" || name.starts_with("ublkc") || name.starts_with("ublkb")
        })
        .collect::<Vec<_>>();
    nodes.sort();
    nodes
}

pub(crate) fn wait_until_missing(path: &str) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while fs::metadata(path).is_ok() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
}

#[path = "ublk_io_smoke/bench_and_swap.rs"]
mod bench_and_swap;
