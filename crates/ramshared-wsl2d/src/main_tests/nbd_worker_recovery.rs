// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main_tests/nbd_worker_recovery.rs
//! NBD origin I/O while a live cache worker is stopping.

use super::*;
use ramshared_block::gpu_cache_worker::{
    FRAME_HEADER_LEN, FrameHeader, MSG_DISABLE_REQ, MSG_DISABLE_RESP, MSG_HANDSHAKE_REQ,
    MSG_HANDSHAKE_RESP, MSG_HEARTBEAT_REQ, MSG_HEARTBEAT_RESP, STATUS_OK,
};
use ramshared_block::handshake::{Export, NBD_OPT_EXPORT_NAME};
use ramshared_block::protocol::{IHAVEOPT, NBD_REQUEST_MAGIC, NBD_SIMPLE_REPLY_MAGIC, NBDMAGIC};
use std::io::{Read, Write};
use std::os::unix::fs::FileExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::{Sender, SyncSender};
use std::thread::JoinHandle;

const CHILD_MARKER: &str = "RAMSHARED_TEST_NBD_WORKER_MARKER";

struct ChildCleanup(std::process::Child);

impl Drop for ChildCleanup {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) && self.0.kill().is_ok() {
            let deadline = Instant::now() + Duration::from_secs(2);
            while matches!(self.0.try_wait(), Ok(None)) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn run_isolated_test() {
    // The daemon's control-plane supervisor is process-lifetime. Run
    // this journey in a child so that it cannot outlive the test.
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "ramshared-nbd-journey-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir(&root).unwrap();
    let marker = root.join("completed");
    let log_path = root.join("child.log");
    let output = File::options()
        .create_new(true)
        .write(true)
        .open(&log_path)
        .unwrap();
    let mut child = ChildCleanup(ProcessCommand::new(std::env::current_exe().unwrap())
        .arg("--exact").arg("tests::worker_observation::nbd_worker_recovery::unix_nbd_keeps_origin_io_during_live_worker_recovery")
        .arg("--nocapture").env(CHILD_MARKER, &marker)
        .stdout(output.try_clone().unwrap()).stderr(output).spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        match child.0.try_wait().unwrap() {
            Some(status) => break status,
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            None => {
                panic!(
                    "isolated NBD journey timed out; log: {}",
                    log_path.display()
                );
            }
        }
    };
    let evidence = std::fs::read(&marker).unwrap_or_default();
    assert!(
        status.success() && evidence == b"completed",
        "isolated NBD journey failed or selected no test; log: {}",
        log_path.display()
    );
    std::fs::remove_dir_all(root).unwrap();
}

type AcceptorReady = (SyncSender<WMsg>, UnixListener, JoinHandle<()>);
struct ChildReady {
    input: std::process::ChildStdin,
    resources: worker_custody::WorkerResources,
    peer: UnixStream,
    peer_thread: JoinHandle<()>,
}

struct DelayedHandshake {
    gate: StartupDelay,
    blocked: Arc<AtomicBool>,
}
struct HeldHandshake(Arc<AtomicBool>);
impl Drop for HeldHandshake {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[derive(Clone)]
struct Starter {
    replacement_delay: Option<Arc<DelayedHandshake>>,
    owner: Arc<worker_registry::WorkerRegistry>,
    attempts: Arc<std::sync::atomic::AtomicU32>,
    published_active: Arc<AtomicBool>,
    recovered_active: Arc<AtomicBool>,
    replacement_after_exit: Arc<AtomicBool>,
    first_worker: Option<worker_custody::WorkerResources>,
    child: Sender<ChildReady>,
    acceptor: Option<Sender<AcceptorReady>>,
}
impl NbdRuntimeStarter for Starter {
    fn test_recovery_admission(&self) -> Option<bool> {
        Some(true)
    }

    fn seed_test_catalog(&self, catalog: &ramshared_wsl2d::host_catalog::SharedHostCatalog) {
        use ramshared_ipc::gpu_catalog::{CatalogSession, CatalogTiming};
        use ramshared_ipc::{HandshakeTranscript, HeartbeatAck, HostChallenge, IPC_VERSION_4};
        use ramshared_vram::GpuAdapterIdentity;
        use ramshared_vram::host_identity::{
            HostGpuIdentity, HostGpuIdentityReport, HostGpuObservation, ProducerProcess,
        };
        let secret = b"test-vsock-shared-secret-32-bytes";
        let transcript = HandshakeTranscript {
            min_version: IPC_VERSION_4,
            max_version: IPC_VERSION_4,
            boot_id: "boot-1".into(),
            distro_id: "ubuntu".into(),
            guest_nonce: [0x11; 32],
        };
        let challenge = HostChallenge {
            accepted_version: IPC_VERSION_4,
            heartbeat_secs: 2,
            lease_timeout_secs: 6,
            host_nonce: [0x22; 32],
        };
        let mut host = CatalogSession::new(secret, &transcript, &challenge, 7).unwrap();
        let mut guest = CatalogSession::new(secret, &transcript, &challenge, 7).unwrap();
        let ack = HeartbeatAck {
            timestamp_ms: 6000,
            lease_remaining_ms: 6000,
            host_memory: None,
        };
        let report = HostGpuIdentityReport {
            schema_version: 1,
            producer: ProducerProcess {
                pid: 99,
                creation_time_100ns: 1234,
            },
            started_at_unix_ms: 5000,
            elapsed_ms: 10,
            adapters: vec![HostGpuIdentity {
                ordinal: 0,
                allocator: GpuAdapterIdentity {
                    backend: "cuda".into(),
                    key: format!("{:032x}", 1),
                    luid: Some("00000000:00011287".into()),
                    node_mask: Some(1),
                },
                interface_id: r"\\?\PCI#VEN_10DE&DEV_1F08#first".into(),
            }],
        };
        let encoded = host
            .seal(
                secret,
                &ack,
                Some(HostGpuObservation { report, age_ms: 50 }),
            )
            .unwrap();
        let now = Instant::now();
        let verified = guest
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
        catalog.publish(Some(&verified));
    }

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
        let listener_copy = listener.try_clone()?;
        let thread = spawn_acceptor(listener, exports, flags, jobs.clone());
        self.acceptor
            .take()
            .ok_or("duplicate acceptor")?
            .send((jobs, listener_copy, thread))
            .map_err(|_| "acceptor receiver closed")?;
        Ok(())
    }
    fn nbd_used_kb(&mut self, _: &str) -> u64 {
        0
    }
    fn try_claim_shutdown(&mut self, _: &str) -> Option<ShutdownPermit> {
        Some(ShutdownPermit::injected(()))
    }
    fn nbd_swap_is_explicitly_absent(&mut self, _: &str) -> bool {
        true
    }
    fn publish_demote(&mut self, _: &ramshared_wsl2d::DemoteStatusFile) {}
    fn publish_origin_cache(&mut self, status: &OriginCacheStatus) {
        if status.cache_state == "ACTIVE" {
            self.published_active.store(true, Ordering::Release);
            if self.attempts.load(Ordering::Acquire) >= 2 {
                self.recovered_active.store(true, Ordering::Release);
            }
        }
    }
    fn elapsed_us(&mut self, started: Instant) -> u64 {
        started.elapsed().as_micros() as u64
    }
    fn spawn_swapoff(&mut self, _: &str) -> std::sync::mpsc::Receiver<SwapoffOutcome> {
        panic!("unarmed test must not start swapoff")
    }
    fn spawn_recovery_activation(
        &mut self,
        _: &str,
        _: i16,
    ) -> Result<std::sync::mpsc::Receiver<bool>, Box<dyn std::error::Error>> {
        panic!("unarmed test must not activate swap")
    }
    fn origin_cache_target_bytes(&mut self, _: u64) -> Result<u64, Box<dyn std::error::Error>> {
        Ok(4096)
    }
    fn begin_isolated_worker_start(
        &mut self,
        params: &WorkerSpawnParams,
    ) -> std::io::Result<worker_startup::Startup> {
        let mut owned = self.clone();
        let params = params.clone();
        let registry = Arc::clone(&self.owner);
        let adapter = params.expected_adapter.clone();
        registry.begin(
            adapter.as_ref(),
            Box::new(move |reservation| {
                worker_startup::StartOutcome::fixture(owned.spawn_reserved(&params, reservation))
            }),
        )
    }

    fn spawn_isolated_gpu_worker(
        &mut self,
        params: &WorkerSpawnParams,
    ) -> worker_startup::InitialStartResult {
        let reservation = self.owner.reserve(params.expected_adapter.as_ref())?;
        self.spawn_reserved(params, reservation)
    }
}

impl Starter {
    fn spawn_reserved(
        &mut self,
        params: &WorkerSpawnParams,
        reservation: worker_custody::Reservation,
    ) -> worker_startup::InitialStartResult {
        let (target, _, _) = (
            params.target_bytes,
            params.chunk_bytes,
            params.reserve_floor,
        );
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
        if attempt > 1
            && let Some(previous) = self.first_worker.as_ref()
        {
            self.replacement_after_exit.store(
                previous.exit_status.get().is_some()
                    && !previous.stop_active.load(Ordering::Acquire)
                    && !previous.reaper_active.load(Ordering::Acquire),
                Ordering::Release,
            );
        }
        let mut child = ProcessCommand::new("/bin/sh")
            .args(["-c", "read line"])
            .stdin(Stdio::piped())
            .spawn()?;
        let input = child.stdin.take().ok_or("child stdin missing")?;
        let resources = reservation.bind(child);
        if attempt == 1 {
            self.first_worker = Some(resources.clone());
        }
        let process = ChildWorkerProcess::from_resources(resources.clone());
        let (socket, mut peer) = UnixStream::pair()?;
        let control = peer.try_clone()?;
        let adapter = params.expected_adapter.clone();
        let delay = (attempt > 1)
            .then(|| self.replacement_delay.clone())
            .flatten();
        let peer_thread = std::thread::spawn(move || {
            loop {
                let mut bytes = [0; FRAME_HEADER_LEN];
                if peer.read_exact(&mut bytes).is_err() {
                    break;
                }
                let Some(request) = FrameHeader::decode(&bytes) else {
                    break;
                };
                if request.msg_type == MSG_HANDSHAKE_REQ
                    && let Some(delay) = &delay
                {
                    delay.blocked.store(true, Ordering::Release);
                    let _held = HeldHandshake(Arc::clone(&delay.blocked));
                    delay.gate.entered.send(()).unwrap();
                    delay
                        .gate
                        .release
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap();
                }
                let msg_type = match request.msg_type {
                    MSG_HANDSHAKE_REQ => MSG_HANDSHAKE_RESP,
                    MSG_HEARTBEAT_REQ => MSG_HEARTBEAT_RESP,
                    MSG_DISABLE_REQ => MSG_DISABLE_RESP,
                    _ => break,
                };
                let payload = if request.msg_type == MSG_HEARTBEAT_REQ {
                    fixture_worker_budget_payload(adapter.clone())
                } else {
                    Vec::new()
                };
                let response = FrameHeader {
                    msg_type,
                    status: STATUS_OK,
                    correlation_id: request.correlation_id,
                    offset: if request.msg_type == MSG_DISABLE_REQ {
                        0
                    } else {
                        target
                    },
                    payload_len: payload.len() as u32,
                    aux: 0,
                };
                if peer
                    .write_all(&response.encode())
                    .and_then(|()| peer.write_all(&payload))
                    .is_err()
                {
                    break;
                }
            }
        });
        let mut client = IpcCacheClient::new(socket, Duration::from_millis(50), target);
        client.perform_handshake().map_err(std::io::Error::other)?;
        gpu_worker_request::complete_startup_binding(&mut client).map_err(std::io::Error::other)?;
        self.child
            .send(ChildReady {
                input,
                resources,
                peer: control,
                peer_thread,
            })
            .map_err(|_| "child receiver closed")?;
        Ok((
            client,
            IsolatedWorkerSupervisor {
                child: Some(WorkerChildHandle::Process(process)),
            },
        ))
    }
}

fn request(stream: &mut UnixStream, command: u16, handle: u64, offset: u64, data: &mut [u8]) {
    request_with_flags(stream, 0, command, handle, offset, data)
}
fn request_with_flags(
    stream: &mut UnixStream,
    flags: u16,
    command: u16,
    handle: u64,
    offset: u64,
    data: &mut [u8],
) {
    let mut header = [0u8; 28];
    header[..4].copy_from_slice(&NBD_REQUEST_MAGIC.to_be_bytes());
    header[4..6].copy_from_slice(&flags.to_be_bytes());
    header[6..8].copy_from_slice(&command.to_be_bytes());
    header[8..16].copy_from_slice(&handle.to_be_bytes());
    header[16..24].copy_from_slice(&offset.to_be_bytes());
    header[24..28].copy_from_slice(&(data.len() as u32).to_be_bytes());
    stream.write_all(&header).unwrap();
    if command == 1 {
        stream.write_all(data).unwrap();
    }
    let mut reply = [0u8; 16];
    stream.read_exact(&mut reply).unwrap();
    assert_eq!(&reply[..4], &NBD_SIMPLE_REPLY_MAGIC.to_be_bytes());
    assert_eq!(&reply[4..8], &0u32.to_be_bytes());
    assert_eq!(&reply[8..16], &handle.to_be_bytes());
    if command == 0 {
        stream.read_exact(data).unwrap();
    }
}

struct Cleanup {
    jobs: Option<SyncSender<WMsg>>,
    listener: Option<UnixListener>,
    input: Option<std::process::ChildStdin>,
    peer: Option<UnixStream>,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(jobs) = self.jobs.take() {
            let _ = jobs.try_send(WMsg::Shutdown);
        }
        drop(self.input.take());
        if let Some(peer) = self.peer.take() {
            let _ = peer.shutdown(std::net::Shutdown::Both);
        }
        if let Some(listener) = self.listener.take() {
            let _ = rustix::net::shutdown(&listener, rustix::net::Shutdown::Both);
        }
    }
}

fn connect_client(socket: &std::path::Path) -> UnixStream {
    let mut client = UnixStream::connect(socket).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    client
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut greeting = [0u8; 18];
    client.read_exact(&mut greeting).unwrap();
    assert_eq!(&greeting[..8], &NBDMAGIC.to_be_bytes());
    client.write_all(&(2u32).to_be_bytes()).unwrap();
    client.write_all(&IHAVEOPT.to_be_bytes()).unwrap();
    client
        .write_all(&NBD_OPT_EXPORT_NAME.to_be_bytes())
        .unwrap();
    client.write_all(&(7u32).to_be_bytes()).unwrap();
    client.write_all(b"default").unwrap();
    let mut export = [0u8; 10];
    client.read_exact(&mut export).unwrap();
    assert_eq!(&export[..8], &GIB.to_be_bytes());
    client
}

fn assert_worker_is_live(resources: &worker_custody::WorkerResources) {
    let pid = rustix::process::Pid::from_raw(i32::try_from(resources.pid).unwrap()).unwrap();
    let status = rustix::process::waitid(
        rustix::process::WaitId::Pid(pid),
        rustix::process::WaitIdOptions::EXITED
            | rustix::process::WaitIdOptions::NOHANG
            | rustix::process::WaitIdOptions::NOWAIT,
    )
    .unwrap();
    assert!(status.is_none(), "the owned worker already exited");
}

fn exercise_origin_io(
    client: &mut UnixStream,
    resources: &worker_custody::WorkerResources,
    owner: &Arc<worker_registry::WorkerRegistry>,
    attempts: &std::sync::atomic::AtomicU32,
    origin_path: &std::path::Path,
) {
    assert!(resources.stop_active.load(Ordering::Acquire));
    assert_worker_is_live(resources);
    assert!(!resources.exit_status.get().is_some());
    assert!(owner.reserve(None).is_err());
    let started = Instant::now();
    let mut first = vec![0; 4096];
    request(client, 0, 11, 0, &mut first);
    assert_eq!(first, vec![0x41; 4096]);
    let mut second = vec![0x72; 4096];
    request(client, 1, 12, 4096, &mut second);
    request(client, 3, 14, 0, &mut []);
    second.fill(0);
    request(client, 0, 13, 4096, &mut second);
    assert_eq!(second, vec![0x72; 4096]);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "NBD I/O waited for worker stop"
    );
    assert!(
        resources.stop_active.load(Ordering::Acquire),
        "the worker stopped before I/O completed"
    );
    assert!(!resources.exit_status.get().is_some());
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "a replacement worker was attempted"
    );
    assert_worker_is_live(resources);
    let mut disk = vec![0; 4096];
    File::open(origin_path)
        .unwrap()
        .read_exact_at(&mut disk, 4096)
        .unwrap();
    assert_eq!(disk, second, "NBD reply did not match authoritative origin");
}

fn await_worker_recovery(client: &mut UnixStream, resources: &worker_custody::WorkerResources) {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut handle = 100;
    while !resources.stop_active.load(Ordering::Acquire) {
        assert!(
            Instant::now() < deadline,
            "worker recovery was not admitted"
        );
        let mut data = vec![0; 4096];
        request(client, 0, handle, 0, &mut data);
        assert_eq!(data, vec![0x41; 4096]);
        handle += 1;
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn fixture_origin() -> (std::path::PathBuf, std::path::PathBuf, File) {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "ramshared-nbd-worker-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir(&root).unwrap();
    let origin_path = root.join("origin.img");
    let file = File::options()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&origin_path)
        .unwrap();
    file.set_len(GIB).unwrap();
    file.write_all_at(&vec![0x41; 4096], 0).unwrap();
    (root, origin_path, file)
}

fn disconnect_initial_peer(
    client: &mut UnixStream,
    resources: &worker_custody::WorkerResources,
    peer_thread: JoinHandle<()>,
    cleanup: &mut Cleanup,
) {
    let mut before = vec![0; 4096];
    request(client, 0, 1, 0, &mut before);
    assert_eq!(before, vec![0x41; 4096]);
    assert!(!resources.stop_active.load(Ordering::Acquire));
    assert!(
        !peer_thread.is_finished(),
        "IPC peer exited before fault injection"
    );
    assert!(!resources.exit_status.get().is_some());
    assert_worker_is_live(resources);
    cleanup
        .peer
        .take()
        .unwrap()
        .shutdown(std::net::Shutdown::Both)
        .unwrap();
    peer_thread.join().unwrap();
}

fn assert_replacement(
    resources: &worker_custody::WorkerResources,
    replacement: &ChildReady,
    attempts: &std::sync::atomic::AtomicU32,
    replacement_after_exit: &AtomicBool,
) {
    assert!(
        resources.exit_status.get().is_some(),
        "the old worker was not observed exiting"
    );
    assert!(
        !resources.stop_active.load(Ordering::Acquire),
        "the old stop controller still owns the worker"
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert!(
        replacement_after_exit.load(Ordering::Acquire),
        "replacement began before the old worker and its controllers finished"
    );
    assert!(
        !Arc::ptr_eq(&resources.child, &replacement.resources.child),
        "replacement retained the old child handle"
    );
    assert_worker_is_live(&replacement.resources);
}

fn exercise_recovered_io(
    client: &mut UnixStream,
    replacement: &ChildReady,
    owner: &Arc<worker_registry::WorkerRegistry>,
    origin_path: &std::path::Path,
) {
    assert!(
        !replacement.peer_thread.is_finished(),
        "replacement IPC peer already exited"
    );
    assert!(
        owner.reserve(None).is_err(),
        "live replacement did not retain custody"
    );
    let mut recovered = vec![0; 4096];
    request(client, 0, 20, 4096, &mut recovered);
    assert_eq!(
        recovered,
        vec![0x72; 4096],
        "origin bytes changed across recovery"
    );
    let mut after = vec![0x35; 4096];
    request(client, 1, 21, 8192, &mut after);
    request(client, 3, 22, 0, &mut []);
    after.fill(0);
    request(client, 0, 23, 8192, &mut after);
    assert_eq!(after, vec![0x35; 4096]);
    let mut disk_after = vec![0; 4096];
    File::open(origin_path)
        .unwrap()
        .read_exact_at(&mut disk_after, 8192)
        .unwrap();
    assert_eq!(disk_after, after);
    assert_worker_is_live(&replacement.resources);
}

fn spawn_daemon(
    file: File,
    socket_name: String,
    mut starter: Starter,
) -> (JoinHandle<()>, mpsc::Receiver<Result<(), String>>) {
    let (finished_tx, finished_rx) = mpsc::channel();
    let daemon = std::thread::spawn(move || {
        let result = run_nbd_with_startup(
            UnavailableVramProvider,
            Some(OpenedOrigin::Wsl(
                FileOrigin::from_file(file),
                SealedOrigin {
                    logical_capacity_mib: 1024,
                    partuuid: "11111111-2222-4333-8444-555555555555".into(),
                    origin_vhdx: "fixture-origin".into(),
                },
            )),
            GIB,
            socket_name,
            false,
            "/dev/ramshared-test-nbd".into(),
            true,
            4,
            &mut starter,
        );
        let _ = finished_tx.send(result.map_err(|error| error.to_string()));
    });
    (daemon, finished_rx)
}

fn wait_for_publication(flag: &AtomicBool, message: &str) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !flag.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline, "{message}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn run_socket_journey(marker: std::path::PathBuf) {
    let (root, origin_path, file) = fixture_origin();
    let socket = root.join("daemon.sock");
    let (child_tx, child_rx) = mpsc::channel();
    let (acceptor_tx, acceptor_rx) = mpsc::channel();
    let owner = Arc::new(worker_registry::WorkerRegistry::default());
    let attempts = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let published_active = Arc::new(AtomicBool::new(false));
    let recovered_active = Arc::new(AtomicBool::new(false));
    let replacement_after_exit = Arc::new(AtomicBool::new(false));
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let blocked = Arc::new(AtomicBool::new(false));
    let starter = Starter {
        replacement_delay: Some(Arc::new(DelayedHandshake {
            gate: StartupDelay {
                entered: entered_tx,
                release: Mutex::new(release_rx),
            },
            blocked: Arc::clone(&blocked),
        })),
        owner: Arc::clone(&owner),
        attempts: Arc::clone(&attempts),
        published_active: Arc::clone(&published_active),
        recovered_active: Arc::clone(&recovered_active),
        replacement_after_exit: Arc::clone(&replacement_after_exit),
        first_worker: None,
        child: child_tx,
        acceptor: Some(acceptor_tx),
    };
    let socket_name = socket.to_string_lossy().into_owned();
    let (daemon, finished_rx) = spawn_daemon(file, socket_name, starter);
    let ChildReady {
        input,
        resources,
        peer,
        peer_thread,
    } = child_rx.recv_timeout(Duration::from_secs(4)).unwrap();
    let (jobs, listener, acceptor) = acceptor_rx.recv_timeout(Duration::from_secs(4)).unwrap();
    let mut cleanup = Cleanup {
        jobs: Some(jobs),
        listener: Some(listener),
        input: Some(input),
        peer: Some(peer),
    };
    let mut client = connect_client(&socket);
    wait_for_publication(&published_active, "active cache was not published");
    disconnect_initial_peer(&mut client, &resources, peer_thread, &mut cleanup);
    await_worker_recovery(&mut client, &resources);
    exercise_origin_io(&mut client, &resources, &owner, &attempts, &origin_path);
    drop(cleanup.input.take());
    entered_rx.recv_timeout(Duration::from_secs(4)).unwrap();
    assert!(
        blocked.load(Ordering::Acquire),
        "replacement handshake is not held"
    );
    assert!(
        owner.reserve(None).is_err(),
        "replacement child lost custody during handshake"
    );
    let mut during = vec![0; 4096];
    request(&mut client, 0, 31, 0, &mut during);
    assert_eq!(during, vec![0x41; 4096]);
    during.fill(0x26);
    request_with_flags(
        &mut client,
        ramshared_block::protocol::NBD_CMD_FLAG_FUA,
        1,
        32,
        12288,
        &mut during,
    );
    request(&mut client, 3, 33, 0, &mut []);
    during.fill(0);
    request(&mut client, 0, 34, 12288, &mut during);
    assert_eq!(during, vec![0x26; 4096]);
    let mut disk = vec![0; 4096];
    File::open(&origin_path)
        .unwrap()
        .read_exact_at(&mut disk, 12288)
        .unwrap();
    assert_eq!(disk, during);
    assert!(
        blocked.load(Ordering::Acquire),
        "NBD I/O waited until handshake completion"
    );
    assert!(!recovered_active.load(Ordering::Acquire));
    release_tx.send(()).unwrap();
    let replacement = child_rx.recv_timeout(Duration::from_secs(4)).unwrap();
    assert_replacement(&resources, &replacement, &attempts, &replacement_after_exit);
    wait_for_publication(&recovered_active, "replacement did not become active");
    exercise_recovered_io(&mut client, &replacement, &owner, &origin_path);
    cleanup.input = Some(replacement.input);
    cleanup.peer = Some(replacement.peer);
    client.shutdown(std::net::Shutdown::Both).unwrap();
    drop(client);
    cleanup
        .jobs
        .as_ref()
        .unwrap()
        .try_send(WMsg::Shutdown)
        .unwrap();
    drop(cleanup.input.take());
    let result = finished_rx.recv_timeout(Duration::from_secs(8)).unwrap();
    result.expect("unarmed daemon shutdown");
    daemon.join().unwrap();
    assert!(
        replacement.resources.exit_status.get().is_some(),
        "replacement exit was not confirmed"
    );
    replacement.peer_thread.join().unwrap();
    cleanup.peer.take();
    rustix::net::shutdown(
        cleanup.listener.as_ref().unwrap(),
        rustix::net::Shutdown::Both,
    )
    .unwrap();
    acceptor.join().unwrap();
    cleanup.jobs.take();
    cleanup.listener.take();
    assert!(
        resources.exit_status.get().is_some(),
        "worker exit was not observed"
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match owner.reserve(None) {
            Ok(reservation) => {
                drop(reservation);
                break;
            }
            Err(error) => {
                assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
                assert!(
                    Instant::now() < deadline,
                    "worker registry did not become quiescent after shutdown"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
    std::fs::remove_dir_all(root).unwrap();
    std::fs::write(marker, b"completed").unwrap();
}

#[test]
fn unix_nbd_keeps_origin_io_during_live_worker_recovery() {
    if let Some(marker) = std::env::var_os(CHILD_MARKER) {
        run_socket_journey(marker.into());
    } else {
        run_isolated_test();
    }
}
