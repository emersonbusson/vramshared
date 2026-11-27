// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main_tests/candidate_recovery.rs
//! Live GPU worker recovery through production registry and supervisor policy.
use super::*;
use ramshared_ipc::gpu_catalog::{CatalogSession, CatalogTiming};
use ramshared_ipc::{HandshakeTranscript, HeartbeatAck, HostChallenge, IPC_VERSION_4};
use ramshared_vram::host_identity::{HostGpuIdentityReport, HostGpuObservation};
use ramshared_wsl2d::host_catalog::SharedHostCatalog;
use std::time::Duration;

struct CandidateStarter(PathBuf);
impl NbdRuntimeStarter for CandidateStarter {
    fn lock_memory(&mut self, _: bool, _: bool) -> Result<(), Box<dyn std::error::Error>> {
        panic!("not used")
    }
    fn start_acceptor(
        &mut self,
        _: UnixListener,
        _: Arc<Vec<ramshared_block::handshake::Export>>,
        _: u16,
        _: std::sync::mpsc::SyncSender<WMsg>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        panic!("not used")
    }
    fn nbd_used_kb(&mut self, _: &str) -> u64 {
        panic!("not used")
    }
    fn try_claim_shutdown(&mut self, _: &str) -> Option<ShutdownPermit> {
        panic!("not used")
    }
    fn publish_demote(&mut self, _: &DemoteStatusFile) {
        panic!("not used")
    }
    fn elapsed_us(&mut self, _: Instant) -> u64 {
        panic!("not used")
    }
    fn spawn_swapoff(&mut self, _: &str) -> std::sync::mpsc::Receiver<SwapoffOutcome> {
        panic!("not used")
    }
    fn spawn_recovery_activation(
        &mut self,
        _: &str,
        _: i16,
    ) -> Result<std::sync::mpsc::Receiver<bool>, Box<dyn std::error::Error>> {
        panic!("not used")
    }
    fn begin_isolated_worker_start(
        &mut self,
        params: &WorkerSpawnParams,
    ) -> std::io::Result<worker_startup::Startup> {
        worker_launcher::begin_candidate(params, self.0.clone())
    }
}

fn actual_catalog() -> (SharedHostCatalog, GpuAdapterIdentity) {
    let windows = std::env::var_os("RAMSHARED_CANDIDATE_WINDOWS").expect("Windows candidate");
    let started = Instant::now();
    let output = ProcessCommand::new(windows)
        .args(["inspect-gpus", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: HostGpuIdentityReport = serde_json::from_slice(&output.stdout).unwrap();
    let adapter = report
        .adapters
        .iter()
        .find(|entry| entry.allocator.backend == "cuda")
        .unwrap()
        .allocator
        .clone();
    let age_ms = u64::try_from(started.elapsed().as_millis())
        .unwrap()
        .max(report.elapsed_ms);
    let secret = b"fixture-catalog-secret-no-production-authority";
    let transcript = HandshakeTranscript {
        min_version: IPC_VERSION_4,
        max_version: IPC_VERSION_4,
        boot_id: "recovery-test".into(),
        distro_id: "recovery-test".into(),
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
    let wire = signer
        .seal(secret, &ack, Some(HostGpuObservation { report, age_ms }))
        .unwrap();
    let now = Instant::now();
    let verified = verifier
        .open(
            secret,
            &ack,
            &wire,
            4,
            CatalogTiming {
                started: now,
                received: now,
            },
        )
        .unwrap()
        .unwrap();
    let catalog = SharedHostCatalog::new();
    catalog.publish(Some(&verified));
    (catalog, adapter)
}

#[test]
#[ignore = "requires candidate executables, WSL2, Windows host and CUDA GPU"]
fn production_supervisor_recovers_real_candidate_worker_while_origin_serves() {
    let executable = std::fs::canonicalize(
        std::env::var_os("RAMSHARED_CANDIDATE_DAEMON").expect("daemon candidate"),
    )
    .unwrap();
    let (catalog, adapter) = actual_catalog();
    let (backend, mut runtime, _fixture_owner) = worker_fixture::respawn_fixture_with_admission(
        true,
        true,
        ramshared_block::AllowCacheAdmission,
    );
    let backend = Arc::new(backend);
    runtime.params.target_bytes = 16 * 1024 * 1024;
    runtime.params.chunk_bytes = 128 * 1024 * 1024;
    runtime.params.reserve_floor = 2 * 1024 * 1024 * 1024;
    runtime.params.expected_adapter = Some(adapter);
    runtime.params.host_catalog = Some(catalog);
    let mut workers = WorkerSet::new(vec![runtime]);
    let mut starter = CandidateStarter(executable.clone());
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        workers.recover(&mut starter, &backend, true);
        if backend.cache_state() == OriginCacheState::Active {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "initial candidate failed to start"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let codec = backend
        .try_control_cache(|cache| match cache {
            OriginCache::Ipc(client) => client.slab_telemetry(),
            _ => panic!("candidate must install an IPC cache"),
        })
        .expect("cache control")
        .expect("codec telemetry request")
        .expect("candidate GPU compression must be available");
    assert_eq!(codec.backend.as_deref(), Some("cuda-ptx-gpu"));
    let original_pid = workers
        .iter_mut()
        .next()
        .unwrap()
        .supervisor
        .as_ref()
        .unwrap()
        .pid();
    assert_eq!(
        std::fs::read_link(format!("/proc/{original_pid}/exe")).unwrap(),
        executable
    );
    let (mut nbd, nbd_thread) = nbd_socket(Arc::clone(&backend));
    let mut payload = [0x5b; 4096];
    nbd_request(&mut nbd, 1, &mut payload);
    let mut read = [0u8; 4096];
    nbd_request(&mut nbd, 0, &mut read);
    assert_eq!(read, payload);
    let supervisor = workers
        .iter_mut()
        .next()
        .unwrap()
        .supervisor
        .as_mut()
        .unwrap();
    match supervisor.child.as_mut().unwrap() {
        WorkerChildHandle::Process(child) => child.kill().unwrap(),
        _ => panic!("expected process worker"),
    }
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        workers.recover(&mut starter, &backend, true);
        read.fill(0);
        nbd_request(&mut nbd, 0, &mut read);
        assert_eq!(read, payload, "origin I/O must survive the worker loss");
        let current = workers
            .iter_mut()
            .next()
            .unwrap()
            .supervisor
            .as_ref()
            .map(IsolatedWorkerSupervisor::pid);
        if backend.cache_state() == OriginCacheState::Active
            && current.is_some_and(|pid| pid != original_pid)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "supervisor did not recover real GPU worker"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let replacement_pid = workers
        .iter_mut()
        .next()
        .unwrap()
        .supervisor
        .as_ref()
        .unwrap()
        .pid();
    assert_eq!(
        std::fs::read_link(format!("/proc/{replacement_pid}/exe")).unwrap(),
        executable
    );
    let recovered_codec = backend
        .try_control_cache(|cache| match cache {
            OriginCache::Ipc(client) => client.slab_telemetry(),
            _ => panic!("replacement must install an IPC cache"),
        })
        .expect("cache control")
        .expect("replacement codec telemetry request")
        .expect("replacement GPU compression must be available");
    assert_eq!(recovered_codec.backend.as_deref(), Some("cuda-ptx-gpu"));
    let hits = backend.telemetry().cache_hits;
    for _ in 0..30 {
        nbd_request(&mut nbd, 0, &mut read);
        if backend.telemetry().cache_hits > hits {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        backend.telemetry().cache_hits > hits,
        "recovered worker did not produce cache hits"
    );
    drop(nbd);
    nbd_thread.join().unwrap();
    assert_eq!(
        backend.release_cache().unwrap(),
        ramshared_block::CacheReleaseOutcome::Revoked
    );
    assert!(workers.shutdown(Duration::from_secs(8), Duration::from_millis(10)));
}

#[test]
#[ignore = "requires root, installed healthy supervisor, candidate executables, WSL2 and CUDA GPU"]
fn pressure_release_recovers_real_candidate_under_live_supervisor() {
    let bytes = crate::manifest_file::read_bounded_manifest_file(
        std::path::Path::new("/run/ramshared/supervisor-state.json"),
        true,
    )
    .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let daemon = value["daemon_instance_id"].as_str().unwrap().to_owned();
    let now = crate::control_requests::unix_time_ms().unwrap();
    assert!(
        crate::pressure_recovery::healthy_observation(&value, &daemon, now),
        "live supervisor must be healthy before the GPU fixture starts"
    );
    let executable = std::fs::canonicalize(
        std::env::var_os("RAMSHARED_CANDIDATE_DAEMON").expect("daemon candidate"),
    )
    .unwrap();
    let (catalog, adapter) = actual_catalog();
    let (backend, mut runtime, _fixture_owner) = worker_fixture::respawn_fixture_with_admission(
        true,
        true,
        ramshared_block::AllowCacheAdmission,
    );
    let backend = Arc::new(backend);
    runtime.params.target_bytes = 16 * 1024 * 1024;
    runtime.params.chunk_bytes = 128 * 1024 * 1024;
    runtime.params.reserve_floor = 2 * 1024 * 1024 * 1024;
    runtime.params.expected_adapter = Some(adapter);
    runtime.params.host_catalog = Some(catalog);
    let mut workers = WorkerSet::new(vec![runtime]);
    let mut starter = CandidateStarter(executable.clone());
    let deadline = Instant::now() + Duration::from_secs(12);
    while backend.cache_state() != OriginCacheState::Active {
        workers.recover(&mut starter, &backend, true);
        assert!(Instant::now() < deadline, "initial worker did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    let first_pid = workers
        .iter_mut()
        .next()
        .unwrap()
        .supervisor
        .as_ref()
        .unwrap()
        .pid();
    let (mut nbd, nbd_thread) = nbd_socket(Arc::clone(&backend));
    let mut payload = [0x7b; 4096];
    nbd_request(&mut nbd, 1, &mut payload);
    let mut read = [0; 4096];
    nbd_request(&mut nbd, 0, &mut read);
    assert_eq!(read, payload);

    let mut publisher = crate::cache_status::CacheStatusPublisher::default();
    let released = publisher.publish(
        &mut starter,
        &backend,
        crate::cache_status::PublicationContext {
            configuration_generation: None,
            daemon_instance_id: "isolated-pressure-test",
            serve_threads: 1,
            serve_requests_total: 0,
            worker: workers.observation(true, None),
            control_admission: None,
            pooled_session: None,
        },
    );
    assert_eq!(released, vec![(0, false)]);
    workers.record_pressure_release(released);
    assert_eq!(backend.cache_state(), OriginCacheState::Off);
    read.fill(0);
    nbd_request(&mut nbd, 0, &mut read);
    assert_eq!(read, payload, "origin must serve during pressure release");

    let now = crate::control_requests::unix_time_ms().unwrap();
    assert!(!workers.pressure_recovery_allowed("foreign-daemon", now));
    assert!(workers.pressure_recovery_allowed(&daemon, now));
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        let now = crate::control_requests::unix_time_ms().unwrap();
        let allowed = workers.pressure_recovery_allowed(&daemon, now);
        let authority = workers.startup_authority(&daemon, None);
        workers.recover_with_authority(&mut starter, &backend, allowed, authority);
        workers.finish_pressure_recovery_if_active(&backend);
        read.fill(0);
        nbd_request(&mut nbd, 0, &mut read);
        assert_eq!(read, payload, "origin must remain correct during recovery");
        if backend.cache_state() == OriginCacheState::Active && !workers.pressure_release_pending()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "pressure recovery did not finish"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let replacement = workers
        .iter_mut()
        .next()
        .unwrap()
        .supervisor
        .as_ref()
        .unwrap()
        .pid();
    assert_ne!(first_pid, replacement);
    assert_eq!(
        std::fs::read_link(format!("/proc/{replacement}/exe")).unwrap(),
        executable
    );
    let codec = backend
        .try_control_cache(|cache| match cache {
            OriginCache::Ipc(client) => client.slab_telemetry(),
            _ => panic!("recovered worker must use IPC"),
        })
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(codec.backend.as_deref(), Some("cuda-ptx-gpu"));
    let hits = backend.telemetry().cache_hits;
    let deadline = Instant::now() + Duration::from_secs(3);
    while backend.telemetry().cache_hits == hits {
        nbd_request(&mut nbd, 0, &mut read);
        assert_eq!(read, payload);
        assert!(
            Instant::now() < deadline,
            "recovered cache did not produce hits"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let compressed = backend
            .try_control_cache(|cache| match cache {
                OriginCache::Ipc(client) => {
                    client.refresh_cached_bytes().unwrap();
                    client.cache_telemetry().map(|t| t.compressed_payload_bytes)
                }
                _ => panic!("recovered worker must use IPC"),
            })
            .unwrap();
        if compressed.is_some_and(|bytes| bytes > 0) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "compressed payload was not observed"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    drop(nbd);
    nbd_thread.join().unwrap();
    backend.release_cache().unwrap();
    assert!(workers.shutdown(Duration::from_secs(8), Duration::from_millis(10)));
}

fn nbd_socket(
    backend: Arc<AuthoritativeOriginBackend<FileOrigin, OriginCache>>,
) -> (std::os::unix::net::UnixStream, std::thread::JoinHandle<()>) {
    use std::io::{Read, Write};
    let (mut client, mut server) = std::os::unix::net::UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    server
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let thread = std::thread::spawn(move || {
        let mut reader = server.try_clone().unwrap();
        let exports = [ramshared_block::handshake::Export::production(
            16 * 1024 * 1024,
        )];
        ramshared_block::server_handshake(&mut reader, &mut server, &exports, 1).unwrap();
        loop {
            let mut header = [0u8; 28];
            if reader.read_exact(&mut header).is_err() {
                break;
            }
            let request = ramshared_block::parse_request(&header).unwrap();
            let mut payload = if request.cmd == Command::Write {
                vec![0; request.len as usize]
            } else {
                Vec::new()
            };
            if !payload.is_empty() {
                reader.read_exact(&mut payload).unwrap();
            }
            let mut shared = &*backend;
            let result = ramshared_block::serve(&request, &payload, &mut shared);
            server.write_all(&result.reply).unwrap();
            server.write_all(&result.read_data).unwrap();
        }
    });
    let mut greeting = [0u8; 18];
    client.read_exact(&mut greeting).unwrap();
    assert_eq!(
        &greeting[..8],
        &ramshared_block::protocol::NBDMAGIC.to_be_bytes()
    );
    client.write_all(&2u32.to_be_bytes()).unwrap();
    client
        .write_all(&ramshared_block::protocol::IHAVEOPT.to_be_bytes())
        .unwrap();
    client
        .write_all(&ramshared_block::handshake::NBD_OPT_EXPORT_NAME.to_be_bytes())
        .unwrap();
    client.write_all(&7u32.to_be_bytes()).unwrap();
    client.write_all(b"default").unwrap();
    let mut export = [0u8; 10];
    client.read_exact(&mut export).unwrap();
    assert_eq!(&export[..8], &(16u64 * 1024 * 1024).to_be_bytes());
    (client, thread)
}

fn nbd_request(client: &mut std::os::unix::net::UnixStream, command: u16, data: &mut [u8]) {
    use std::io::{Read, Write};
    let mut header = [0u8; 28];
    header[..4].copy_from_slice(&ramshared_block::protocol::NBD_REQUEST_MAGIC.to_be_bytes());
    header[6..8].copy_from_slice(&command.to_be_bytes());
    header[8..16].copy_from_slice(&1u64.to_be_bytes());
    header[24..28].copy_from_slice(&(data.len() as u32).to_be_bytes());
    client.write_all(&header).unwrap();
    if command == 1 {
        client.write_all(data).unwrap();
    }
    let mut reply = [0u8; 16];
    client.read_exact(&mut reply).unwrap();
    assert_eq!(
        &reply[..4],
        &ramshared_block::protocol::NBD_SIMPLE_REPLY_MAGIC.to_be_bytes()
    );
    assert_eq!(u32::from_be_bytes(reply[4..8].try_into().unwrap()), 0);
    assert_eq!(&reply[8..], &1u64.to_be_bytes());
    if command == 0 {
        client.read_exact(data).unwrap();
    }
}
