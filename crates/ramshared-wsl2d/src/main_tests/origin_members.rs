// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main_tests/origin_members.rs
//! Constructor components: real IPC/file I/O, synthetic GPU and seal authority.
use super::*;
use ramshared_block::gpu_cache_worker::*;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

#[derive(Clone, Copy)]
enum Reply {
    Good,
    Unavailable,
    Failed,
    WrongIdentity,
    OverCeiling,
    Stale,
    Panic,
}

struct Starter {
    plan: Option<origin_plan::WorkerPlan>,
    replies: Vec<Reply>,
    requests: Vec<WorkerSpawnParams>,
    stopped: Vec<Arc<AtomicBool>>,
}
fn request(key: u8) -> WorkerSpawnParams {
    WorkerSpawnParams {
        generation: None,
        target_bytes: 4096,
        chunk_bytes: 4096,
        reserve_floor: 1024,
        expected_adapter: Some(GpuAdapterIdentity {
            backend: "cuda".into(),
            key: format!("{key:032x}"),
            luid: None,
            node_mask: None,
        }),
        host_catalog: None,
    }
}
fn starter(replies: Vec<Reply>) -> Starter {
    Starter {
        plan: Some(origin_plan::WorkerPlan {
            chunk_bytes: 4096,
            members: vec![request(2), request(1)],
        }),
        replies,
        requests: vec![],
        stopped: vec![],
    }
}

impl NbdRuntimeStarter for Starter {
    fn origin_worker_plan(&mut self, _: u64, _: u64) -> origin_plan::PlanResult {
        Ok(self.plan.take().unwrap())
    }
    fn origin_cache_target_bytes(&mut self, _: u64) -> Result<u64, Box<dyn std::error::Error>> {
        Ok(4096)
    }
    fn spawn_isolated_gpu_worker(
        &mut self,
        params: &WorkerSpawnParams,
    ) -> worker_startup::InitialStartResult {
        let reply = self.replies[self.requests.len()];
        self.requests.push(params.clone());
        match reply {
            Reply::Failed => return Err("synthetic refusal before child creation".into()),
            Reply::Panic => panic!("synthetic constructor interruption"),
            _ => {}
        }
        let (client, supervisor, stopped) = peer(params, reply);
        self.stopped.push(stopped);
        Ok((client, supervisor))
    }
    fn lock_memory(&mut self, _: bool, _: bool) -> Result<(), Box<dyn std::error::Error>> {
        panic!("no memory locking")
    }
    fn start_acceptor(
        &mut self,
        _: UnixListener,
        _: Arc<Vec<ramshared_block::handshake::Export>>,
        _: u16,
        _: std::sync::mpsc::SyncSender<WMsg>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        panic!("no acceptor")
    }
    fn nbd_used_kb(&mut self, _: &str) -> u64 {
        panic!("no swap probe")
    }
    fn try_claim_shutdown(&mut self, _: &str) -> Option<ShutdownPermit> {
        panic!("no shutdown claim")
    }
    fn publish_demote(&mut self, _: &ramshared_wsl2d::DemoteStatusFile) {
        panic!("no demote")
    }
    fn elapsed_us(&mut self, _: Instant) -> u64 {
        panic!("no demote timing")
    }
    fn spawn_swapoff(&mut self, _: &str) -> std::sync::mpsc::Receiver<SwapoffOutcome> {
        panic!("no swapoff")
    }
    fn spawn_recovery_activation(
        &mut self,
        _: &str,
        _: i16,
    ) -> Result<std::sync::mpsc::Receiver<bool>, Box<dyn std::error::Error>> {
        panic!("no activation")
    }
}

fn peer(
    params: &WorkerSpawnParams,
    reply: Reply,
) -> (IpcCacheClient, IsolatedWorkerSupervisor, Arc<AtomicBool>) {
    let (socket, mut server) = UnixStream::pair().unwrap();
    server
        .set_read_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    let mut adapter = params
        .expected_adapter
        .clone()
        .unwrap_or_else(|| request(1).expected_adapter.unwrap());
    if matches!(reply, Reply::WrongIdentity) {
        adapter = request(9).expected_adapter.unwrap();
    }
    let marker = u8::from_str_radix(&adapter.key[30..], 16).unwrap();
    let target = match reply {
        Reply::Unavailable => 0,
        Reply::OverCeiling => params.target_bytes * 2,
        _ => params.target_bytes,
    };
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = Arc::new(AtomicBool::new(false));
    let (thread_stop, thread_stopped, thread_adapter) =
        (stop.clone(), stopped.clone(), adapter.clone());
    let handle = std::thread::spawn(move || {
        while !thread_stop.load(Ordering::SeqCst) {
            let mut bytes = [0; FRAME_HEADER_LEN];
            match server.read_exact(&mut bytes) {
                Ok(()) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    continue;
                }
                Err(_) => break,
            }
            let header = FrameHeader::decode(&bytes).unwrap();
            assert!(header.payload_len <= 4096);
            if header.msg_type == MSG_READ_REQ {
                assert!(header.aux <= 4096);
            }
            if header.payload_len > 0 {
                let mut input = vec![0; header.payload_len as usize];
                if server.read_exact(&mut input).is_err() {
                    break;
                }
            }
            let (kind, offset, payload) = match header.msg_type {
                MSG_HANDSHAKE_REQ => (MSG_HANDSHAKE_RESP, target, vec![]),
                MSG_HEARTBEAT_REQ => (
                    MSG_HEARTBEAT_RESP,
                    target,
                    fixture_worker_budget_payload(Some(thread_adapter.clone())),
                ),
                MSG_DISABLE_REQ => (MSG_DISABLE_RESP, 0, vec![]),
                MSG_READ_REQ => (
                    MSG_READ_RESP,
                    header.offset,
                    vec![marker; header.aux as usize],
                ),
                _ => break,
            };
            let response = FrameHeader {
                msg_type: kind,
                status: STATUS_OK,
                correlation_id: header.correlation_id,
                offset,
                payload_len: payload.len() as u32,
                aux: 0,
            };
            if server
                .write_all(&response.encode())
                .and_then(|()| server.write_all(&payload))
                .is_err()
            {
                break;
            }
        }
        thread_stopped.store(true, Ordering::SeqCst);
    });
    // Own the thread before fallible handshakes, just like the native launcher.
    let supervisor = IsolatedWorkerSupervisor {
        child: Some(WorkerChildHandle::Thread {
            stop,
            handle: Some(handle),
        }),
    };
    let mut client = IpcCacheClient::new(socket, Duration::from_millis(200), params.target_bytes);
    client.expect_adapter(adapter).unwrap();
    client.perform_handshake().unwrap();
    gpu_worker_request::complete_startup_binding(&mut client).unwrap();
    if matches!(reply, Reply::Stale) {
        std::thread::sleep(Duration::from_millis(
            ramshared_vram::GPU_BUDGET_MAX_AGE_MS + 50,
        ));
    }
    (client, supervisor, stopped)
}

struct OriginFile {
    path: PathBuf,
}
impl OriginFile {
    fn new() -> Self {
        Self {
            path: std::env::temp_dir().join(format!(
                "ramshared-members-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )),
        }
    }
    fn build(
        &self,
        starter: &mut Starter,
    ) -> Result<origin_runtime::OriginRuntime, Box<dyn std::error::Error>> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&self.path)
            .unwrap();
        file.set_len(8192).unwrap();
        origin_runtime::build(
            origin_runtime::OpenedOrigin::Wsl(
                FileOrigin::from_file(file),
                ramshared_wsl2d::host_gate::SealedOrigin {
                    logical_capacity_mib: 1,
                    partuuid: "synthetic-member-seal".into(),
                    origin_vhdx: "synthetic-member-origin".into(),
                },
            ),
            8192,
            1024,
            starter,
        )
    }
}
impl Drop for OriginFile {
    fn drop(&mut self) {
        if self.path.exists() {
            std::fs::remove_file(&self.path).unwrap();
        }
    }
}

fn assert_origin(runtime: &origin_runtime::OriginRuntime) {
    runtime
        .backend
        .write_at_with_options(0, &[73; 4096], ramshared_block::WriteOptions { fua: true })
        .unwrap();
    runtime.backend.flush().unwrap();
    let mut data = [0; 4096];
    runtime.backend.read_at(0, &mut data).unwrap();
    assert_eq!(
        data, [73; 4096],
        "synthetic seal never grants actual cache admission"
    );
}
fn assert_slots(runtime: &mut origin_runtime::OriginRuntime, starter: &Starter) {
    assert_eq!(
        starter.requests.len(),
        2,
        "all configured members must be constructed"
    );
    let members: Vec<_> = runtime.workers.iter_mut().collect();
    assert_eq!(members.len(), 2);
    for (i, member) in members.iter().enumerate() {
        assert_eq!(member.cache_member, i);
        assert_eq!(
            member.params.expected_adapter,
            request((i + 1) as u8).expected_adapter
        );
        assert_eq!(member.params.target_bytes, 4096);
        assert_eq!(member.params.reserve_floor, 1024);
    }
}

#[test]
fn configured_members_construct_canonical_ipc_routes() {
    let file = OriginFile::new();
    let mut starter = starter(vec![Reply::Good, Reply::Good]);
    let mut runtime = file.build(&mut starter).unwrap();
    assert_slots(&mut runtime, &starter);
    assert_origin(&runtime);
    // Exercise the group's actual IPC routing without granting serving authority.
    for (offset, marker) in [(0, 1), (4096, 2), (8192, 1)] {
        let mut data = [0; 4096];
        let result = runtime
            .backend
            .try_control_cache(|cache| cache.read(offset, &mut data))
            .unwrap();
        assert_eq!(result, CacheRead::Hit);
        assert_eq!(data, [marker; 4096]);
    }
    drop(runtime);
    assert!(starter.stopped.iter().all(|s| s.load(Ordering::SeqCst)));
}

#[test]
fn configured_members_quarantine_invalid_replies_and_retain_planned_identity() {
    for reply in [
        Reply::Unavailable,
        Reply::Failed,
        Reply::WrongIdentity,
        Reply::OverCeiling,
        Reply::Stale,
    ] {
        let file = OriginFile::new();
        let mut starter = starter(vec![Reply::Good, reply]);
        let mut runtime = file.build(&mut starter).unwrap();
        assert_slots(&mut runtime, &starter);
        assert_origin(&runtime);
        runtime
            .backend
            .try_control_cache(|cache| {
                let statuses = cache.member_statuses().unwrap();
                assert_eq!(statuses.len(), 2);
                assert_eq!(statuses[0].state, OriginCacheState::Active);
                assert_eq!(statuses[1].state, OriginCacheState::Unavailable);
                assert!(statuses.iter().all(|s| s.requested_target_bytes == 4096));
                let mut bytes = [0; 4096];
                assert_eq!(cache.read(4096, &mut bytes), CacheRead::Miss);
                assert_eq!(cache.read(0, &mut bytes), CacheRead::Hit);
                assert_eq!(bytes, [1; 4096]);
            })
            .unwrap();
        let member = runtime.workers.iter_mut().nth(1).unwrap();
        assert_eq!(member.supervisor.is_some(), !matches!(reply, Reply::Failed));
        if !matches!(reply, Reply::Unavailable) {
            assert!(member.unavailable_reason.is_some());
        }
        drop(runtime);
        assert!(starter.stopped.iter().all(|s| s.load(Ordering::SeqCst)));
    }
}

#[test]
fn configured_members_validate_whole_plan_before_spawn_and_collect_on_unwind() {
    let file = OriginFile::new();
    let mut invalid = starter(vec![Reply::Good]);
    invalid.plan.as_mut().unwrap().members[1].target_bytes = 0;
    assert!(file.build(&mut invalid).is_err());
    assert!(invalid.requests.is_empty());
    let file = OriginFile::new();
    let mut interrupted = starter(vec![Reply::Good, Reply::Panic]);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        file.build(&mut interrupted)
    }));
    assert!(result.is_err());
    assert_eq!(interrupted.requests.len(), 2);
    assert_eq!(interrupted.stopped.len(), 1);
    assert!(
        interrupted.stopped[0].load(Ordering::SeqCst),
        "returned worker must settle during origin unwind"
    );
}

#[test]
fn native_configured_origin_consumes_approved_members_and_preserves_origin_on_failure() {
    use crate::native_cache::tests::{policy, synthetic_session};
    use crate::native_fixture::Fixture;
    for reply in [Reply::Good, Reply::Failed, Reply::WrongIdentity] {
        let fixture = Fixture::new();
        let session = synthetic_session(policy(&fixture.manifest));
        let origin = crate::native_origin::AdmittedOrigin {
            file: FileOrigin::open(fixture.origin_path()).unwrap(),
            logical_bytes: 8192,
            manifest: fixture.manifest.clone(),
            cache_session: Some(session.clone()),
        };
        let mut starter = starter(vec![Reply::Good, reply]);
        let mut runtime =
            origin_runtime::build(OpenedOrigin::Native(origin), 8192, 0, &mut starter).unwrap();
        assert_eq!(starter.requests.len(), 2);
        assert_eq!(runtime.workers.iter_mut().count(), 2);
        assert_eq!(
            runtime.native_session.as_ref().unwrap().generation(),
            Some(7)
        );
        assert!(
            runtime.admission.is_none(),
            "native authority never fabricates a Windows lease"
        );
        for (index, request) in starter.requests.iter().enumerate() {
            assert_eq!(request.target_bytes, 4096);
            assert_eq!(
                request.expected_adapter.as_ref().unwrap().key,
                format!("{:032x}", index + 1)
            );
            assert!(Arc::ptr_eq(request.generation.as_ref().unwrap(), &session));
            assert_eq!(
                request.reserve_floor,
                ramshared_vram::ReserveFloorPolicy::sealed().min_floor_bytes
            );
        }
        runtime.backend.release_cache().unwrap();
        assert_origin(&runtime);
        drop(runtime);
        assert!(!session.is_active());
        assert!(
            starter
                .stopped
                .iter()
                .all(|stopped| stopped.load(Ordering::SeqCst))
        );
        assert_eq!(
            &std::fs::read(fixture.origin_path()).unwrap()[..4096],
            &[73; 4096]
        );
    }
}

#[test]
fn configured_two_adapter_data_path_survives_independent_loss_recovery_and_shutdown() {
    use crate::native_cache::tests::{policy, synthetic_session};
    use crate::native_fixture::Fixture;

    struct RecoveryStarter {
        custody: Arc<worker_startup::StartupCustody>,
        requests: Vec<WorkerSpawnParams>,
        kill_switches: Vec<Arc<AtomicBool>>,
        stopped: Vec<Arc<AtomicBool>>,
        respawn_marker: u8,
    }

    fn spawn_controllable_peer(
        params: &WorkerSpawnParams,
        override_marker: Option<u8>,
    ) -> (
        IpcCacheClient,
        IsolatedWorkerSupervisor,
        Arc<AtomicBool>,
        Arc<AtomicBool>,
    ) {
        let (socket, mut server) = UnixStream::pair().unwrap();
        server
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let adapter = params.expected_adapter.clone().unwrap();
        let marker =
            override_marker.unwrap_or_else(|| u8::from_str_radix(&adapter.key[30..], 16).unwrap());
        let target = params.target_bytes;
        let stop = Arc::new(AtomicBool::new(false));
        let kill_switch = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let (t_stop, t_kill, t_stopped, t_adapter) = (
            stop.clone(),
            kill_switch.clone(),
            stopped.clone(),
            adapter.clone(),
        );
        let handle = std::thread::spawn(move || {
            while !t_stop.load(Ordering::SeqCst) && !t_kill.load(Ordering::SeqCst) {
                let mut bytes = [0; FRAME_HEADER_LEN];
                match server.read_exact(&mut bytes) {
                    Ok(()) => {}
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        continue;
                    }
                    Err(_) => break,
                }
                if t_kill.load(Ordering::SeqCst) {
                    break;
                }
                let header = FrameHeader::decode(&bytes).unwrap();
                if header.payload_len > 0 {
                    let mut input = vec![0; header.payload_len as usize];
                    if server.read_exact(&mut input).is_err() {
                        break;
                    }
                }
                let (kind, offset, payload) = match header.msg_type {
                    MSG_HANDSHAKE_REQ => (MSG_HANDSHAKE_RESP, target, vec![]),
                    MSG_HEARTBEAT_REQ => (
                        MSG_HEARTBEAT_RESP,
                        target,
                        fixture_worker_budget_payload(Some(t_adapter.clone())),
                    ),
                    MSG_DISABLE_REQ => (MSG_DISABLE_RESP, 0, vec![]),
                    MSG_READ_REQ => (
                        MSG_READ_RESP,
                        header.offset,
                        vec![marker; header.aux as usize],
                    ),
                    MSG_UPDATE | MSG_PROMOTE | MSG_INVALIDATE_REQ => continue,
                    _ => break,
                };
                let response = FrameHeader {
                    msg_type: kind,
                    status: STATUS_OK,
                    correlation_id: header.correlation_id,
                    offset,
                    payload_len: payload.len() as u32,
                    aux: 0,
                };
                if server
                    .write_all(&response.encode())
                    .and_then(|()| server.write_all(&payload))
                    .is_err()
                {
                    break;
                }
            }
            let _ = server.shutdown(std::net::Shutdown::Both);
            t_stopped.store(true, Ordering::SeqCst);
        });
        let supervisor = IsolatedWorkerSupervisor {
            child: Some(WorkerChildHandle::Thread {
                stop,
                handle: Some(handle),
            }),
        };
        let mut client = IpcCacheClient::with_pacing(
            socket,
            Duration::from_millis(200),
            params.target_bytes,
            ramshared_block::ipc_cache_client::PacingPolicy::immediate(),
        );
        client.expect_adapter(adapter).unwrap();
        client.perform_handshake().unwrap();
        gpu_worker_request::complete_startup_binding(&mut client).unwrap();
        (client, supervisor, kill_switch, stopped)
    }

    impl NbdRuntimeStarter for RecoveryStarter {
        fn spawn_isolated_gpu_worker(
            &mut self,
            params: &WorkerSpawnParams,
        ) -> worker_startup::InitialStartResult {
            self.requests.push(params.clone());
            let (client, supervisor, kill, stopped) = spawn_controllable_peer(params, None);
            self.kill_switches.push(kill);
            self.stopped.push(stopped);
            Ok((client, supervisor))
        }
        fn begin_isolated_worker_start(
            &mut self,
            params: &WorkerSpawnParams,
        ) -> std::io::Result<worker_startup::Startup> {
            let p = params.clone();
            let marker = self.respawn_marker;
            let (client, supervisor, kill, stopped) = spawn_controllable_peer(&p, Some(marker));
            self.kill_switches.push(kill);
            self.stopped.push(stopped);
            self.custody
                .begin(Box::new(move || worker_startup::StartOutcome {
                    result: Ok((client, supervisor)),
                    creator_child: None,
                }))
        }
        fn lock_memory(&mut self, _: bool, _: bool) -> Result<(), Box<dyn std::error::Error>> {
            panic!("no memory locking")
        }
        fn start_acceptor(
            &mut self,
            _: UnixListener,
            _: Arc<Vec<ramshared_block::handshake::Export>>,
            _: u16,
            _: std::sync::mpsc::SyncSender<WMsg>,
        ) -> Result<(), Box<dyn std::error::Error>> {
            panic!("no acceptor")
        }
        fn nbd_used_kb(&mut self, _: &str) -> u64 {
            panic!("no swap probe")
        }
        fn try_claim_shutdown(&mut self, _: &str) -> Option<ShutdownPermit> {
            panic!("no shutdown claim")
        }
        fn publish_demote(&mut self, _: &ramshared_wsl2d::DemoteStatusFile) {
            panic!("no demote")
        }
        fn elapsed_us(&mut self, _: Instant) -> u64 {
            panic!("no demote timing")
        }
        fn spawn_swapoff(&mut self, _: &str) -> std::sync::mpsc::Receiver<SwapoffOutcome> {
            panic!("no swapoff")
        }
        fn spawn_recovery_activation(
            &mut self,
            _: &str,
            _: i16,
        ) -> Result<std::sync::mpsc::Receiver<bool>, Box<dyn std::error::Error>> {
            panic!("no activation")
        }
    }

    let chunk = ramshared_block::chunk_bytes_from_env();
    let logical_bytes = chunk * 2;
    let fixture = Fixture::new();
    std::fs::OpenOptions::new()
        .write(true)
        .open(fixture.origin_path())
        .unwrap()
        .set_len(logical_bytes)
        .unwrap();
    let mut cfg = policy(&fixture.manifest);
    for member in &mut cfg.members {
        member.target_bytes = chunk;
    }
    let session = synthetic_session(cfg);
    let origin = crate::native_origin::AdmittedOrigin {
        file: FileOrigin::open(fixture.origin_path()).unwrap(),
        logical_bytes,
        manifest: fixture.manifest.clone(),
        cache_session: Some(session.clone()),
    };
    let mut starter = RecoveryStarter {
        custody: Arc::new(worker_startup::StartupCustody::default()),
        requests: vec![],
        kill_switches: vec![],
        stopped: vec![],
        respawn_marker: 111,
    };
    let mut runtime =
        origin_runtime::build(OpenedOrigin::Native(origin), logical_bytes, 0, &mut starter)
            .unwrap();
    assert_eq!(starter.requests.len(), 2);

    // Phase 1: Both adapters active; writes persist to origin and reads hit each adapter.
    runtime.backend.write_at(0, &[0xA1; 4096]).unwrap();
    runtime.backend.write_at(chunk, &[0xB2; 4096]).unwrap();
    let mut buf0 = [0u8; 4096];
    let mut buf1 = [0u8; 4096];
    runtime.backend.read_at(0, &mut buf0).unwrap();
    runtime.backend.read_at(chunk, &mut buf1).unwrap();
    assert_eq!(buf0, [1; 4096]);
    assert_eq!(buf1, [2; 4096]);

    // Phase 2: Independent loss of adapter 0 while adapter 1 continues serving.
    starter.kill_switches[0].store(true, Ordering::SeqCst);
    while !starter.stopped[0].load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(5));
    }
    runtime.backend.read_at(0, &mut buf0).unwrap();
    runtime.backend.read_at(chunk, &mut buf1).unwrap();
    assert_eq!(
        buf0, [0xA1; 4096],
        "dead adapter 0 must fall back to authoritative origin"
    );
    assert_eq!(
        buf1, [2; 4096],
        "healthy adapter 1 must continue serving cache hits"
    );
    runtime.backend.write_at(0, &[0xC3; 4096]).unwrap();

    // Phase 3: Independent recovery of adapter 0; adapter 1 untouched.
    {
        let mut members: Vec<_> = runtime.workers.iter_mut().collect();
        recover_isolated_gpu_worker_if_failed(&mut starter, &runtime.backend, members[0], true);
        recover_isolated_gpu_worker_if_failed(&mut starter, &runtime.backend, members[1], true);
        let deadline = Instant::now() + Duration::from_secs(2);
        while members[0].startup.is_some() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
            recover_isolated_gpu_worker_if_failed(&mut starter, &runtime.backend, members[0], true);
        }
        assert!(members[0].startup.is_none());
        assert_eq!(members[0].budget.respawns_since_healthy, 1);
        assert_eq!(members[1].budget.respawns_since_healthy, 0);
    }

    runtime.backend.read_at(0, &mut buf0).unwrap();
    runtime.backend.read_at(chunk, &mut buf1).unwrap();
    assert_eq!(
        buf0, [111; 4096],
        "recovered adapter 0 must serve hits from replacement worker"
    );
    assert_eq!(buf1, [2; 4096], "adapter 1 remains active");

    // Phase 4: Shutdown settles all workers and preserves authoritative origin bytes.
    drop(runtime);
    assert!(!session.is_active());
    assert_eq!(starter.stopped.len(), 3);
    assert!(starter.stopped.iter().all(|s| s.load(Ordering::SeqCst)));
    let mut disk0 = [0u8; 4096];
    let mut disk1 = [0u8; 4096];
    let origin_verify = FileOrigin::open(fixture.origin_path()).unwrap();
    use ramshared_block::OriginStorage;
    origin_verify.read_exact_at(0, &mut disk0).unwrap();
    origin_verify.read_exact_at(chunk, &mut disk1).unwrap();
    assert_eq!(disk0, [0xC3; 4096]);
    assert_eq!(disk1, [0xB2; 4096]);
}
