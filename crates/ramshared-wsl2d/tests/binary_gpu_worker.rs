// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/tests/binary_gpu_worker.rs
//! Candidate executable check with a test-authenticated catalog from a real host probe.
#![allow(clippy::expect_used, clippy::unwrap_used)]
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use ramshared_block::ipc_cache_client::PacingPolicy;
use ramshared_block::{BestEffortCache, CacheMutation, CacheRead, IpcCacheClient};
use ramshared_ipc::gpu_catalog::{CatalogSession, CatalogTiming};
use ramshared_ipc::{HandshakeTranscript, HeartbeatAck, HostChallenge, IPC_VERSION_4};
use ramshared_vram::host_identity::{HostGpuIdentityReport, HostGpuObservation};
use ramshared_wsl2d::host_catalog::SharedHostCatalog;

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if !matches!(self.0.try_wait(), Ok(None)) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

#[test]
#[ignore = "requires candidate executables, WSL2, Windows host and CUDA GPU"]
fn candidate_worker_serves_compressed_io_with_real_host_identity() {
    exercise_candidate(Scenario::Clean);
}

#[test]
#[ignore = "requires candidate executables, WSL2, Windows host and CUDA GPU"]
fn candidate_worker_loss_preserves_origin_and_recovers_cache() {
    exercise_candidate(Scenario::WorkerDeath);
}

#[test]
#[ignore = "requires candidate executables, WSL2, Windows host and CUDA GPU"]
fn candidate_host_catalog_loss_falls_back_and_recovers_cache() {
    exercise_candidate(Scenario::CatalogRevoked);
}

#[test]
#[ignore = "requires candidate executables, WSL2, Windows host and CUDA GPU"]
fn candidate_worker_recovers_from_saturated_mutation_stream() {
    exercise_candidate(Scenario::Backpressure);
}

#[test]
#[ignore = "requires candidate executables, WSL2, Windows host and CUDA GPU"]
fn candidate_worker_preserves_compressed_rewrites_across_pages() {
    exercise_candidate(Scenario::WorkingSet);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scenario {
    Clean,
    WorkerDeath,
    CatalogRevoked,
    Backpressure,
    WorkingSet,
}

fn exercise_candidate(scenario: Scenario) {
    let daemon = std::fs::canonicalize(
        std::env::var_os("RAMSHARED_CANDIDATE_DAEMON").expect("candidate daemon path"),
    )
    .unwrap();
    let windows = std::env::var_os("RAMSHARED_CANDIDATE_WINDOWS").expect("candidate Windows path");
    let probe_started = Instant::now();
    let output = Command::new(windows)
        .args(["inspect-gpus", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: HostGpuIdentityReport = serde_json::from_slice(&output.stdout).unwrap();
    let adapter = report
        .adapters
        .iter()
        .find(|adapter| adapter.allocator.backend == "cuda")
        .expect("real CUDA host identity")
        .allocator
        .clone();
    let secret = b"fixture-catalog-secret-no-production-authority";
    let transcript = HandshakeTranscript {
        min_version: IPC_VERSION_4,
        max_version: IPC_VERSION_4,
        boot_id: "candidate-test".into(),
        distro_id: "candidate-test".into(),
        guest_nonce: [1; 32],
    };
    let challenge = HostChallenge {
        accepted_version: IPC_VERSION_4,
        heartbeat_secs: 1,
        lease_timeout_secs: 15,
        host_nonce: [2; 32],
    };
    let ack = HeartbeatAck {
        timestamp_ms: 6000,
        lease_remaining_ms: 15000,
        host_memory: None,
    };
    let mut signer = CatalogSession::new(secret, &transcript, &challenge, 7).unwrap();
    let mut verifier = CatalogSession::new(secret, &transcript, &challenge, 7).unwrap();
    let age_ms = u64::try_from(probe_started.elapsed().as_millis())
        .unwrap()
        .max(report.elapsed_ms);
    let encoded = signer
        .seal(secret, &ack, Some(HostGpuObservation { report, age_ms }))
        .unwrap();
    let now = Instant::now();
    let authenticated = verifier
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
        .unwrap()
        .unwrap();
    let catalog = SharedHostCatalog::new();
    catalog.publish(Some(&authenticated));
    let (mut child, mut client, _catalog_stream) = spawn_candidate(&daemon, &catalog, &adapter);
    assert!(
        client.target_bytes() > 0,
        "candidate did not admit a GPU cache"
    );
    let expected = vec![0x46; 8192];
    assert_eq!(client.update(0, &expected), CacheMutation::Accepted);
    let mut read = vec![0; expected.len()];
    assert_eq!(client.read(0, &mut read), CacheRead::Hit);
    assert_eq!(read, expected);
    client.refresh_cached_bytes().unwrap();
    let telemetry = client.cache_telemetry().expect("worker codec telemetry");
    assert!(
        telemetry.compressed_payload_bytes > 0,
        "candidate did not use GPU compression: {telemetry:?}"
    );
    if scenario == Scenario::Backpressure {
        exercise_saturated_stream(&mut child, &mut client);
        assert_eq!(client.disable(), CacheMutation::Accepted);
        assert!(wait_child(&mut child).success());
        return;
    }
    if scenario == Scenario::WorkingSet {
        exercise_compressed_rewrites(&mut client);
        assert_eq!(client.disable(), CacheMutation::Accepted);
        assert!(wait_child(&mut child).success());
        return;
    }
    if scenario == Scenario::Clean {
        assert_eq!(client.disable(), CacheMutation::Accepted);
        assert!(wait_child(&mut child).success());
        return;
    }

    use ramshared_block::{AllowCacheAdmission, AuthoritativeOriginBackend, FileOrigin};
    let path = std::env::temp_dir().join(format!(
        "ramshared-candidate-failure-{}",
        std::process::id()
    ));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    file.set_len(16384).unwrap();
    let backend = AuthoritativeOriginBackend::new_with_admission(
        FileOrigin::from_file(file),
        client,
        16384,
        4096,
        AllowCacheAdmission,
    )
    .unwrap();
    let payload = vec![0x72; 4096];
    backend.write_at(0, &payload).unwrap();
    let mut read = vec![0; payload.len()];
    backend.read_at(0, &mut read).unwrap();
    assert_eq!(read, payload);
    assert!(backend.telemetry().cache_hits > 0);
    match scenario {
        Scenario::WorkerDeath => {
            child.0.kill().unwrap();
            assert!(!wait_child(&mut child).success());
        }
        Scenario::CatalogRevoked => {
            catalog.revoke();
            assert!(
                backend.refresh_cached_bytes().is_err(),
                "revoked host catalog must refuse the worker heartbeat"
            );
            wait_child(&mut child);
            catalog.publish(Some(&authenticated));
        }
        Scenario::Clean | Scenario::Backpressure | Scenario::WorkingSet => unreachable!(),
    }
    read.fill(0);
    backend.read_at(0, &mut read).unwrap();
    assert_eq!(read, payload, "origin must survive worker loss");
    assert!(backend.telemetry().fallback_reads > 0);
    assert_eq!(&std::fs::read(&path).unwrap()[..payload.len()], payload);
    let (mut recovered_child, recovered_client, _recovered_catalog) =
        spawn_candidate(&daemon, &catalog, &adapter);
    backend.replace_cache(recovered_client);
    let hits_before = backend.telemetry().cache_hits;
    read.fill(0);
    backend.read_at(0, &mut read).unwrap();
    assert_eq!(read, payload);
    backend.read_at(0, &mut read).unwrap();
    assert_eq!(read, payload);
    assert!(
        backend.telemetry().cache_hits > hits_before,
        "replacement worker did not warm cache"
    );
    assert_eq!(
        backend.release_cache().unwrap(),
        ramshared_block::CacheReleaseOutcome::Revoked
    );
    assert!(wait_child(&mut recovered_child).success());
    drop(backend);
    std::fs::remove_file(path).unwrap();
}

fn exercise_compressed_rewrites(client: &mut IpcCacheClient) {
    const PAGES: u64 = 16;
    const PAGE_SIZE: usize = 4096;
    for round in 0..3u8 {
        for page in 0..PAGES {
            let value = round.wrapping_mul(61).wrapping_add(page as u8);
            assert_eq!(
                client.update(page * PAGE_SIZE as u64, &vec![value; PAGE_SIZE]),
                CacheMutation::Accepted
            );
        }
        for page in 0..PAGES {
            let value = round.wrapping_mul(61).wrapping_add(page as u8);
            let mut actual = vec![0; PAGE_SIZE];
            assert_eq!(
                client.read(page * PAGE_SIZE as u64, &mut actual),
                CacheRead::Hit
            );
            assert_eq!(
                actual,
                vec![value; PAGE_SIZE],
                "stale data on page {page} round {round}"
            );
        }
    }
    client.refresh_cached_bytes().unwrap();
    assert!(client.cache_telemetry().unwrap().compressed_payload_bytes > 0);
}

fn exercise_saturated_stream(child: &mut OwnedChild, client: &mut IpcCacheClient) {
    // SAFETY: this is the still-owned fixture child, not the installed daemon.
    assert_eq!(unsafe { libc::kill(child.0.id() as i32, libc::SIGSTOP) }, 0);
    let old = vec![0x11; 64 * 1024];
    let mut skipped = 0;
    for _ in 0..5000 {
        match client.update(0, &old) {
            CacheMutation::Accepted => {}
            CacheMutation::Skipped => skipped += 1,
            CacheMutation::Failed => panic!("cache revoked under bounded backpressure"),
        }
    }
    assert!(
        skipped > 4096,
        "worker stream did not sustain the required backpressure: {skipped}"
    );
    eprintln!("fixture_backpressure_updates_skipped={skipped}");
    let latest = vec![0x22; 8192];
    assert_eq!(client.update(0, &latest), CacheMutation::Skipped);
    assert_eq!(client.read(0, &mut vec![0; latest.len()]), CacheRead::Miss);
    // SAFETY: same owned fixture child; all earlier writes remain ordered.
    assert_eq!(unsafe { libc::kill(child.0.id() as i32, libc::SIGCONT) }, 0);
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        let mut actual = vec![0; latest.len()];
        match client.read(0, &mut actual) {
            CacheRead::Hit => assert_eq!(actual, latest, "stale cache hit"),
            CacheRead::Miss => {}
            CacheRead::Failed => panic!("cache failed while draining"),
        }
        if client.update(0, &latest) == CacheMutation::Accepted {
            let mut warmed = vec![0; latest.len()];
            if client.read(0, &mut warmed) == CacheRead::Hit {
                assert_eq!(warmed, latest);
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "cache did not recover after backpressure"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn wait_child(child: &mut OwnedChild) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "candidate worker did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn spawn_candidate(
    daemon: &std::path::Path,
    catalog: &SharedHostCatalog,
    adapter: &ramshared_vram::GpuAdapterIdentity,
) -> (OwnedChild, IpcCacheClient, UnixStream) {
    let catalog_stream = catalog.subscribe().unwrap();
    let catalog_fd = catalog_stream.as_raw_fd();
    let (client_sock, child_sock) = UnixStream::pair().unwrap();
    let mut command = Command::new(daemon);
    command
        .args([
            "__gpu_worker",
            "--fd",
            "0",
            "--target-bytes",
            "16777216",
            "--chunk-bytes",
            "134217728",
            "--reserve-floor",
            "2147483648",
            "--compression",
            "--adapter-identity",
            &serde_json::to_string(adapter).unwrap(),
            "--catalog-fd",
            &catalog_fd.to_string(),
        ])
        .stdin(Stdio::from(std::os::fd::OwnedFd::from(child_sock)))
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    // SAFETY: only fcntl is called after fork; the descriptor is held through spawn.
    unsafe {
        command.pre_exec(move || {
            let flags = libc::fcntl(catalog_fd, libc::F_GETFD);
            if flags < 0 || libc::fcntl(catalog_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = OwnedChild(command.spawn().unwrap());
    assert_eq!(
        std::fs::read_link(format!("/proc/{}/exe", child.0.id())).unwrap(),
        daemon
    );
    let mut client = IpcCacheClient::with_pacing(
        client_sock,
        Duration::from_secs(5),
        16 * 1024 * 1024,
        PacingPolicy::immediate(),
    );
    client
        .perform_handshake()
        .expect("candidate worker handshake");
    assert!(
        client.target_bytes() > 0,
        "candidate did not admit a GPU cache"
    );
    (child, client, catalog_stream)
}
