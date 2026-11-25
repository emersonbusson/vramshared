// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main_tests/cache_publication.rs
//! Real unarmed Unix NBD dispatch and atomic status publication counts.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::native_fixture::Fixture;
use ramshared_block::handshake::{Export, NBD_OPT_EXPORT_NAME};
use ramshared_block::protocol::{IHAVEOPT, NBD_REQUEST_MAGIC, NBD_SIMPLE_REPLY_MAGIC, NBDMAGIC};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, SyncSender};

struct Starter {
    path: PathBuf,
    output: PathBuf,
    client: Option<std::thread::JoinHandle<u64>>,
    load_duration: Duration,
    statuses: Vec<(Instant, OriginCacheStatus)>,
    updates: mpsc::Sender<u64>,
    samples: Option<mpsc::Receiver<u64>>,
}
impl NbdRuntimeStarter for Starter {
    fn lock_memory(&mut self, _: bool, _: bool) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }
    fn start_acceptor(
        &mut self,
        listener: UnixListener,
        exports: Arc<Vec<Export>>,
        flags: u16,
        jobs: SyncSender<WMsg>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let close = listener.try_clone()?;
        let acceptor = spawn_acceptor(listener, exports, flags, jobs.clone());
        let path = self.path.clone();
        let samples = self.samples.take().unwrap();
        let load_duration = self.load_duration;
        self.client = Some(std::thread::spawn(move || {
            let result = std::panic::catch_unwind(|| exercise(&path, samples, load_duration));
            let _ = jobs.send(WMsg::Shutdown);
            rustix::net::shutdown(&close, rustix::net::Shutdown::Both).unwrap();
            acceptor.join().unwrap();
            match result {
                Ok(requests) => requests,
                Err(panic) => std::panic::resume_unwind(panic),
            }
        }));
        Ok(())
    }
    fn nbd_used_kb(&mut self, _: &str) -> u64 {
        0
    }
    fn nbd_swap_is_explicitly_absent(&mut self, _: &str) -> bool {
        true
    }
    fn try_claim_shutdown(&mut self, _: &str) -> Option<ShutdownPermit> {
        Some(ShutdownPermit::injected(()))
    }
    fn publish_demote(&mut self, _: &DemoteStatusFile) {}
    fn publish_origin_cache(&mut self, status: &OriginCacheStatus) {
        write_origin_cache_status(&self.output, status).unwrap();
        self.statuses.push((Instant::now(), status.clone()));
        let _ = self.updates.send(status.serve_requests_total);
    }
    fn elapsed_us(&mut self, started: Instant) -> u64 {
        started.elapsed().as_micros() as u64
    }
    fn spawn_swapoff(&mut self, _: &str) -> mpsc::Receiver<SwapoffOutcome> {
        panic!("unarmed fixture")
    }
    fn spawn_recovery_activation(
        &mut self,
        _: &str,
        _: i16,
    ) -> Result<mpsc::Receiver<bool>, Box<dyn std::error::Error>> {
        panic!("no activation")
    }
}

fn connect(path: &Path) -> UnixStream {
    let mut client = UnixStream::connect(path).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    client
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut greeting = [0; 18];
    client.read_exact(&mut greeting).unwrap();
    assert_eq!(&greeting[..8], &NBDMAGIC.to_be_bytes());
    client.write_all(&2u32.to_be_bytes()).unwrap();
    client.write_all(&IHAVEOPT.to_be_bytes()).unwrap();
    client
        .write_all(&NBD_OPT_EXPORT_NAME.to_be_bytes())
        .unwrap();
    client.write_all(&7u32.to_be_bytes()).unwrap();
    client.write_all(b"default").unwrap();
    let mut export = [0; 10];
    client.read_exact(&mut export).unwrap();
    assert_eq!(&export[..8], &8192u64.to_be_bytes());
    client
}
fn request(client: &mut UnixStream, command: u16, handle: u64, data: &mut [u8]) {
    let mut header = [0; 28];
    header[..4].copy_from_slice(&NBD_REQUEST_MAGIC.to_be_bytes());
    header[6..8].copy_from_slice(&command.to_be_bytes());
    header[8..16].copy_from_slice(&handle.to_be_bytes());
    header[24..].copy_from_slice(&(data.len() as u32).to_be_bytes());
    client.write_all(&header).unwrap();
    if command == 1 {
        client.write_all(data).unwrap();
    }
    let mut reply = [0; 16];
    client.read_exact(&mut reply).unwrap();
    assert_eq!(&reply[..4], &NBD_SIMPLE_REPLY_MAGIC.to_be_bytes());
    assert_eq!(&reply[4..8], &0u32.to_be_bytes());
    assert_eq!(&reply[8..], &handle.to_be_bytes());
    if command == 0 {
        client.read_exact(data).unwrap();
    }
}
fn exercise(path: &Path, samples: mpsc::Receiver<u64>, load_duration: Duration) -> u64 {
    let mut client = connect(path);
    let started = Instant::now();
    let mut reads = 0;
    while reads < 96 || started.elapsed() < load_duration {
        reads += 1;
        let mut page = [0; 4096];
        request(&mut client, 0, reads, &mut page);
        assert_eq!(page, [17; 4096]);
    }
    request(&mut client, 1, reads + 1, &mut [73; 4096]);
    request(&mut client, 3, reads + 2, &mut []);
    let mut actual = [0; 4096];
    request(&mut client, 0, reads + 3, &mut actual);
    assert_eq!(actual, [73; 4096]);
    let requests = reads + 3;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let count = samples
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
        if count >= requests {
            assert_eq!(count, requests);
            break;
        }
    }
    requests
}

#[test]
fn metadata_publication_is_bounded_under_sustained_dispatch() {
    run_journey(Duration::ZERO);
}

#[test]
fn publication_remains_bounded_across_multiple_busy_intervals() {
    run_journey(SERVE_IDLE_TICK * 2);
}

fn run_journey(load_duration: Duration) {
    let fixture = Fixture::new();
    let file = File::options()
        .read(true)
        .write(true)
        .open(fixture.origin_path())
        .unwrap();
    let (updates, samples) = mpsc::channel();
    let mut starter = Starter {
        path: fixture.root.join("cadence.sock"),
        output: fixture.root.join("status.json"),
        client: None,
        load_duration,
        statuses: vec![],
        updates,
        samples: Some(samples),
    };
    let started = Instant::now();
    let result = run_nbd_with_startup(
        UnavailableVramProvider,
        Some(OpenedOrigin::Native(crate::native_origin::AdmittedOrigin {
            file: FileOrigin::from_file(file),
            logical_bytes: 8192,
            manifest: fixture.manifest.clone(),
            cache_session: None,
        })),
        8192,
        starter.path.to_str().unwrap().into(),
        false,
        "/dev/unarmed-publication-fixture".into(),
        false,
        2,
        &mut starter,
    );
    let requests = starter.client.take().unwrap().join().unwrap();
    result.unwrap();
    assert_eq!(
        &std::fs::read(fixture.origin_path()).unwrap()[..4096],
        &[73; 4096]
    );
    let allowance = 2 + started.elapsed().as_millis() / SERVE_IDLE_TICK.as_millis();
    eprintln!(
        "publication-count: requests={requests} writes={} elapsed_ms={} allowance={allowance}",
        starter.statuses.len(),
        started.elapsed().as_millis()
    );
    assert!(
        starter.statuses.len() as u128 <= allowance,
        "metadata publication followed job rate: {} writes for {requests} requests, allowance {allowance}",
        starter.statuses.len()
    );
    assert!(
        starter.statuses.len() >= 2,
        "idle dispatcher never refreshed the completed request count"
    );
    let wire: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&starter.output).unwrap()).unwrap();
    assert_eq!(wire["serve_requests_total"], requests);
    assert!(
        starter
            .statuses
            .iter()
            .all(|(_, s)| s.origin_state == "READY" && s.cache_members.is_none())
    );
    assert!(!starter.path.exists());
}

fn exercise_measured(
    path: &Path,
    samples: mpsc::Receiver<u64>,
    min_reads: u64,
    load_duration: Duration,
    write_byte: u8,
) -> (u64, u64, u64) {
    let mut client = connect(path);
    let started = Instant::now();
    let mut reads = 0u64;
    let mut lat_us = Vec::with_capacity(min_reads as usize + 4);
    while reads < min_reads || started.elapsed() < load_duration {
        reads += 1;
        let mut page = [0; 4096];
        let t0 = Instant::now();
        request(&mut client, 0, reads, &mut page);
        lat_us.push(t0.elapsed().as_micros() as u64);
        assert_eq!(page, [17; 4096]);
    }
    let t_w = Instant::now();
    request(&mut client, 1, reads + 1, &mut [write_byte; 4096]);
    lat_us.push(t_w.elapsed().as_micros() as u64);
    let t_f = Instant::now();
    request(&mut client, 3, reads + 2, &mut []);
    lat_us.push(t_f.elapsed().as_micros() as u64);
    let mut actual = [0; 4096];
    let t_r = Instant::now();
    request(&mut client, 0, reads + 3, &mut actual);
    lat_us.push(t_r.elapsed().as_micros() as u64);
    assert_eq!(actual, [write_byte; 4096]);
    let requests = reads + 3;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let count = samples
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
        if count >= requests {
            assert_eq!(count, requests);
            break;
        }
    }
    lat_us.sort_unstable();
    let p50 = lat_us[(lat_us.len() - 1) * 50 / 100];
    let p99 = lat_us[(lat_us.len() - 1) * 99 / 100];
    (requests, p50, p99)
}

#[test]
#[ignore = "requires an explicitly scheduled sustained-load qualification"]
fn physical_same_load_cache_status_publication_controls_three_rounds() {
    use sha2::{Digest, Sha256};

    let exe_bytes = std::fs::read("/proc/self/exe").unwrap();
    let exe_sha256: String = Sha256::digest(&exe_bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let pid = std::process::id();
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    let start_ticks = stat
        .rfind(')')
        .map(|i| &stat[i + 2..])
        .unwrap_or("")
        .split_whitespace()
        .nth(19)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    assert_eq!(exe_sha256.len(), 64);
    assert!(pid > 0 && start_ticks > 0);

    struct MeasuredStarter {
        path: PathBuf,
        output: PathBuf,
        client: Option<std::thread::JoinHandle<(u64, u64, u64)>>,
        load_duration: Duration,
        write_byte: u8,
        statuses: Vec<OriginCacheStatus>,
        status_bytes_written: u64,
        updates: mpsc::Sender<u64>,
        samples: Option<mpsc::Receiver<u64>>,
    }
    impl NbdRuntimeStarter for MeasuredStarter {
        fn lock_memory(&mut self, _: bool, _: bool) -> Result<(), Box<dyn std::error::Error>> {
            Ok(())
        }
        fn start_acceptor(
            &mut self,
            listener: UnixListener,
            exports: Arc<Vec<Export>>,
            flags: u16,
            jobs: SyncSender<WMsg>,
        ) -> Result<(), Box<dyn std::error::Error>> {
            let close = listener.try_clone()?;
            let acceptor = spawn_acceptor(listener, exports, flags, jobs.clone());
            let path = self.path.clone();
            let samples = self.samples.take().unwrap();
            let load_duration = self.load_duration;
            let write_byte = self.write_byte;
            self.client = Some(std::thread::spawn(move || {
                let res = std::panic::catch_unwind(|| {
                    exercise_measured(&path, samples, 512, load_duration, write_byte)
                });
                let _ = jobs.send(WMsg::Shutdown);
                rustix::net::shutdown(&close, rustix::net::Shutdown::Both).unwrap();
                acceptor.join().unwrap();
                match res {
                    Ok(m) => m,
                    Err(p) => std::panic::resume_unwind(p),
                }
            }));
            Ok(())
        }
        fn nbd_used_kb(&mut self, _: &str) -> u64 {
            0
        }
        fn nbd_swap_is_explicitly_absent(&mut self, _: &str) -> bool {
            true
        }
        fn try_claim_shutdown(&mut self, _: &str) -> Option<ShutdownPermit> {
            Some(ShutdownPermit::injected(()))
        }
        fn publish_demote(&mut self, _: &DemoteStatusFile) {}
        fn publish_origin_cache(&mut self, status: &OriginCacheStatus) {
            write_origin_cache_status(&self.output, status).unwrap();
            if let Ok(meta) = std::fs::metadata(&self.output) {
                self.status_bytes_written = self.status_bytes_written.saturating_add(meta.len());
            }
            self.statuses.push(status.clone());
            let _ = self.updates.send(status.serve_requests_total);
        }
        fn elapsed_us(&mut self, started: Instant) -> u64 {
            started.elapsed().as_micros() as u64
        }
        fn spawn_swapoff(&mut self, _: &str) -> mpsc::Receiver<SwapoffOutcome> {
            panic!("unarmed fixture")
        }
        fn spawn_recovery_activation(
            &mut self,
            _: &str,
            _: i16,
        ) -> Result<mpsc::Receiver<bool>, Box<dyn std::error::Error>> {
            panic!("no activation")
        }
    }

    for round in 0..3u8 {
        let fixture = Fixture::new();
        let file = File::options()
            .read(true)
            .write(true)
            .open(fixture.origin_path())
            .unwrap();
        let write_byte = 80 + round;
        let (updates, samples) = mpsc::channel();
        let mut starter = MeasuredStarter {
            path: fixture.root.join(format!("cadence-r{round}.sock")),
            output: fixture.root.join(format!("status-r{round}.json")),
            client: None,
            load_duration: SERVE_IDLE_TICK * 2,
            write_byte,
            statuses: vec![],
            status_bytes_written: 0,
            updates,
            samples: Some(samples),
        };
        let started = Instant::now();
        run_nbd_with_startup(
            UnavailableVramProvider,
            Some(OpenedOrigin::Native(crate::native_origin::AdmittedOrigin {
                file: FileOrigin::from_file(file),
                logical_bytes: 8192,
                manifest: fixture.manifest.clone(),
                cache_session: None,
            })),
            8192,
            starter.path.to_str().unwrap().into(),
            false,
            "/dev/unarmed-publication-fixture".into(),
            false,
            2,
            &mut starter,
        )
        .unwrap();
        let (requests, p50_us, p99_us) = starter.client.take().unwrap().join().unwrap();
        let elapsed_ms = started.elapsed().as_millis();
        let allowance = 2 + elapsed_ms / SERVE_IDLE_TICK.as_millis();
        let pubs = starter.statuses.len();
        assert!(requests >= 515);
        assert!(pubs >= 2 && (pubs as u128) <= allowance);
        assert!(pubs * 20 <= requests as usize);
        let origin_bytes = std::fs::read(fixture.origin_path()).unwrap();
        assert_eq!(&origin_bytes[..4096], &[write_byte; 4096]);
        assert_eq!(&origin_bytes[4096..8192], &[17; 4096]);
        let wire: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&starter.output).unwrap()).unwrap();
        assert_eq!(wire["serve_requests_total"], requests);
        eprintln!(
            "SAME_LOAD_PUBLICATION_CONTROLS=PASS round={} exe_sha256={} pid={} start_ticks={} \
             requests={} publications={} allowance={} status_bytes={} origin_bytes={} \
             req_p50_us={} req_p99_us={} elapsed_ms={}",
            round + 1,
            exe_sha256,
            pid,
            start_ticks,
            requests,
            pubs,
            allowance,
            starter.status_bytes_written,
            origin_bytes.len(),
            p50_us,
            p99_us,
            elapsed_ms,
        );
    }
}
