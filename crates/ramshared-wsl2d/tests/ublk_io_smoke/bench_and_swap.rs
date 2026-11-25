// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/tests/ublk_io_smoke/bench_and_swap.rs
use super::*;
use ramshared_wsl2d::ublk_control;
use std::fs;
use std::thread;
use std::time::{Duration, Instant};

#[test]
#[ignore = "requires root + CUDA GPU; bounded mkswap/swapon/swapoff on VRAM-ublk (no memory pressure)"]
fn vram_ublk_round_trips_as_swap_device() {
    let before = ublk_nodes();
    let report = ublk_control::add_device(UBLK_CONTROL, ublk_control::DeviceSpec::smoke_auto())
        .expect("ublk ADD_DEV");
    let block_path = format!("/dev/ublkb{}", report.dev_id);
    let char_path = format!("/dev/ublkc{}", report.dev_id);
    // Guard: ensures swapoff BEFORE stop/del even if the test fails.
    let mut guard = SwapGuard::new(report.dev_id, block_path.clone());

    let block_size = 4096u32;
    let dev_sectors = 128 * 1024 * 1024 / SECTOR; // 128 MiB of swap on VRAM
    ublk_control::set_params(
        UBLK_CONTROL,
        report.dev_id,
        ublk::Params::basic_disk(dev_sectors, 12, 12),
    )
    .expect("ublk SET_PARAMS");

    let vram_bytes = (dev_sectors * SECTOR) as usize;
    let server = ublk_server::spawn_server_dt3_vram(
        &char_path,
        report.queue_depth,
        2 * 1024 * 1024, // buffer per tag covers swap clusters
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

    // mkswap writes swap header -> ublk WRITE -> cuMemcpyHtoD in VRAM.
    run_ok("mkswap", &[&block_path]);
    // swapon (without -p: auto low priority) -> kernel reads the header (ublk READ) and
    // registers the VRAM-ublk as swap area.
    run_ok("swapon", &[&block_path]);

    let swaps = fs::read_to_string("/proc/swaps").expect("/proc/swaps");
    assert!(
        swaps.contains(&block_path),
        "VRAM-ublk was not registered as swap:\n{swaps}"
    );

    // immediate swapoff (without generating pressure) -> deactivates and drains.
    run_ok("swapoff", &[&block_path]);
    let swaps = fs::read_to_string("/proc/swaps").expect("/proc/swaps");
    assert!(!swaps.contains(&block_path), "swap should be deactivated");

    ublk_control::stop_dev(UBLK_CONTROL, report.dev_id).expect("ublk STOP_DEV");
    server.join().expect("DT-3 VRAM server terminated ok");
    ublk_control::delete_device(UBLK_CONTROL, report.dev_id).expect("ublk DEL_DEV");
    guard.disarm();
    wait_until_missing(&char_path);
    assert_eq!(ublk_nodes(), before);
}

#[test]
#[ignore = "requires root + CUDA GPU; measures 4KB read latency of ublk-VRAM, no swap"]
fn bench_vram_ublk_read_latency() {
    let report = ublk_control::add_device(UBLK_CONTROL, ublk_control::DeviceSpec::smoke_auto())
        .expect("ublk ADD_DEV");
    let mut guard = DeviceGuard::new(report.dev_id);

    let block_size = 4096u32;
    let dev_sectors = 16 * 1024 * 1024 / SECTOR; // 16 MiB
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
        64 * 1024,
        vram_bytes,
        block_size,
    )
    .expect("spawn DT-3 VRAM server");
    ublk_control::start_dev(UBLK_CONTROL, report.dev_id, std::process::id())
        .expect("ublk START_DEV");

    let n_blocks = vram_bytes / block_size as usize;
    let (p50, p90, p99, p999, max) = bench_read_latency(&block_path, block_size, n_blocks);
    println!(
        "ublk-VRAM 4KB READ O_DIRECT (n=4000): p50={p50:?} p90={p90:?} p99={p99:?} p99.9={p999:?} max={max:?}"
    );
    // Sanity: plausible latency (microseconds to a few ms), not hung.
    assert!(p50 < Duration::from_millis(50), "p50 implausible: {p50:?}");

    ublk_control::stop_dev(UBLK_CONTROL, report.dev_id).expect("ublk STOP_DEV");
    server.join().expect("DT-3 VRAM server terminated ok");
    ublk_control::delete_device(UBLK_CONTROL, report.dev_id).expect("ublk DEL_DEV");
    guard.disarm();
    wait_until_missing(&char_path);
}

/// Measures the latency of 4KB `O_DIRECT` reads (each hits the device, no cache)
/// at pseudo-random offsets. Returns (p50, p90, p99, p99.9, max).
fn bench_read_latency(
    path: &str,
    block_size: u32,
    n_blocks: usize,
) -> (Duration, Duration, Duration, Duration, Duration) {
    const O_DIRECT: i32 = 0o40000; // x86_64 Linux
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_DIRECT)
        .open(path)
        .expect("open O_DIRECT");

    let bs = block_size as usize;
    // Buffer aligned to block size (O_DIRECT requirement).
    let mut raw = vec![0u8; bs * 2];
    let pad = (bs - (raw.as_ptr() as usize % bs)) % bs;
    let n = n_blocks as u64;

    // xorshift64 for aligned pseudo-random offsets.
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let mut next_off = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        (x % n) * bs as u64
    };

    for _ in 0..128 {
        let off = next_off();
        file.read_exact_at(&mut raw[pad..pad + bs], off)
            .expect("warmup");
    }

    let iters = 4000usize;
    let mut lat = Vec::with_capacity(iters);
    for _ in 0..iters {
        let off = next_off();
        let t = Instant::now();
        file.read_exact_at(&mut raw[pad..pad + bs], off)
            .expect("read");
        lat.push(t.elapsed());
    }
    lat.sort_unstable();
    let last = lat.len() - 1;
    let pct = |q: usize| lat[last * q / 100];
    (
        pct(50),
        pct(90),
        pct(99),
        lat[lat.len() * 999 / 1000],
        lat[last],
    )
}

#[test]
#[ignore = "requires root + CUDA GPU + fio; fio latency of ublk-VRAM (compares with NBD), no swap"]
fn fio_bench_vram_ublk() {
    let report = ublk_control::add_device(UBLK_CONTROL, ublk_control::DeviceSpec::smoke_auto())
        .expect("ublk ADD_DEV");
    let mut guard = DeviceGuard::new(report.dev_id);

    let block_size = 4096u32;
    let dev_sectors = 64 * 1024 * 1024 / SECTOR; // 64 MiB
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
        64 * 1024,
        vram_bytes,
        block_size,
    )
    .expect("spawn DT-3 VRAM server");
    ublk_control::start_dev(UBLK_CONTROL, report.dev_id, std::process::id())
        .expect("ublk START_DEV");

    let out = fio_randread(&block_path, "ublk-vram");
    print!("{out}");

    ublk_control::stop_dev(UBLK_CONTROL, report.dev_id).expect("ublk STOP_DEV");
    server.join().expect("DT-3 VRAM server terminated ok");
    ublk_control::delete_device(UBLK_CONTROL, report.dev_id).expect("ublk DEL_DEV");
    guard.disarm();
    wait_until_missing(&char_path);
}

/// Runs `fio` randread 4KB O_DIRECT iodepth=1 on block device and returns stdout.
fn fio_randread(dev: &str, name: &str) -> String {
    let out = std::process::Command::new("fio")
        .args([
            &format!("--name={name}"),
            &format!("--filename={dev}"),
            "--rw=randread",
            "--bs=4k",
            "--direct=1",
            "--ioengine=psync",
            "--iodepth=1",
            "--runtime=4",
            "--time_based",
            "--norandommap",
        ])
        .output()
        .unwrap_or_else(|e| panic!("failed to execute fio: {e}"));
    assert!(
        out.status.success(),
        "fio failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn run_ok(cmd: &str, args: &[&str]) {
    let out = std::process::Command::new(cmd)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to execute {cmd}: {e}"));
    assert!(
        out.status.success(),
        "{cmd} {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Teardown guard for the swap test: `swapoff` (best-effort) before
/// `stop_dev`/`delete_device`, since a device with active swap cannot be deleted.
struct SwapGuard {
    dev_id: Option<u32>,
    block_path: String,
}

impl SwapGuard {
    fn new(dev_id: u32, block_path: String) -> Self {
        Self {
            dev_id: Some(dev_id),
            block_path,
        }
    }

    fn disarm(&mut self) {
        self.dev_id = None;
    }
}

impl Drop for SwapGuard {
    fn drop(&mut self) {
        if let Some(dev_id) = self.dev_id.take() {
            let _ = std::process::Command::new("swapoff")
                .arg(&self.block_path)
                .status();
            let _ = ublk_control::stop_dev(UBLK_CONTROL, dev_id);
            let _ = ublk_control::delete_device(UBLK_CONTROL, dev_id);
        }
    }
}

#[test]
#[ignore = "requires an isolated non-WSL Linux host and RAMSHARED_DANGEROUS_DAEMON_SMOKE=1; prefer VM/QEMU"]
fn daemon_ublk_serves_and_terminates_on_signal() {
    // SAFETY GUARD: this smoke starts the ublk daemon as a separate process and
    // depends on SIGTERM for teardown. If teardown fails, `/dev/ublkbN` is left without
    // server -> I/O in D-state -> WSL2 FREEZES (already happened, 2026-06-09). Therefore
    // it does NOT run even with `--ignored`, unless explicit opt-in is set.
    if std::env::var("RAMSHARED_DANGEROUS_DAEMON_SMOKE").as_deref() != Ok("1") {
        eprintln!(
            "[skip] daemon_ublk_serves_and_terminates_on_signal: gated. \
             Set RAMSHARED_DANGEROUS_DAEMON_SMOKE=1 to run (CAN FREEZE WSL2)."
        );
        return;
    }

    let osrelease = fs::read_to_string("/proc/sys/kernel/osrelease")
        .unwrap_or_default()
        .to_ascii_lowercase();
    if osrelease.contains("microsoft") || osrelease.contains("wsl") {
        eprintln!(
            "[skip] daemon_ublk_serves_and_terminates_on_signal: standalone ublk smoke is permanently disabled on WSL2"
        );
        return;
    }

    let before = ublk_nodes();
    // Starts the daemon in ublk mode only after proving this is not WSL. The daemon repeats
    // that fail-closed platform check, so this ignored smoke cannot override the WSL lockout.
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_ramsharedd"))
        .args(["--transport", "ublk", "--size", "8", "--queue-depth", "1"])
        .spawn()
        .expect("spawn daemon ublk");

    // Waits for the device to appear (CUDA initialization takes ~1s).
    let block_path = match wait_new_ublkb(&before, Duration::from_secs(30)) {
        Some(p) => p,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("daemon did not create /dev/ublkbN in 30s");
        }
    };

    // Serves: WRITE + READ of a 4KB block via block device (closes the fds in
    // helpers). Does NOT use drop_page_cache(): it amplifies the stall if the device hangs
    // (was one of the triggers of the 2026-06-09 freeze). WRITE+fsync already proves the serve.
    let off = 8192u64;
    let pattern: Vec<u8> = (0..4096u32).map(|i| ((i * 7 + 13) % 251) as u8).collect();
    write_block(&block_path, off, &pattern);
    let got = read_block(&block_path, off, 4096);
    assert_eq!(got, pattern, "ublk daemon must serve the block written");

    // SIGTERM -> orderly teardown in daemon (STOP_DEV -> join -> DEL_DEV).
    run_ok("kill", &["-TERM", &child.id().to_string()]);

    let status =
        wait_child(&mut child, Duration::from_secs(15)).expect("daemon did not exit in 15s");
    assert!(
        status.success(),
        "ublk daemon exited with error: {status:?}"
    );

    // /dev returns to initial state (device removed by teardown).
    let deadline = Instant::now() + Duration::from_secs(5);
    while ublk_nodes() != before && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        ublk_nodes(),
        before,
        "ublk daemon must remove the device on teardown"
    );
}

/// Waits for a `/dev/ublkbN` to appear that was not in `before`; returns the path.
fn wait_new_ublkb(before: &[String], timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        for name in ublk_nodes() {
            if name.starts_with("ublkb") && !before.iter().any(|b| b == &name) {
                return Some(format!("/dev/{name}"));
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    None
}

/// Waits (bounded) for the process to exit; returns the status or `None` on timeout.
fn wait_child(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => thread::sleep(Duration::from_millis(100)),
            Err(_) => return None,
        }
    }
    None
}
