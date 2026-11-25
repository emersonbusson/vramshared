// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main_tests/worker_fixture.rs
//! Owned worker launcher and authoritative-file fixtures shared by recovery tests.
use super::*;

/// Hold a real IPC handshake until the serving test explicitly releases it.
pub(super) struct StartupDelay {
    pub(super) entered: std::sync::mpsc::SyncSender<()>,
    pub(super) release: Mutex<std::sync::mpsc::Receiver<()>>,
}

/// Owned launcher fixture; unrelated memory/swap/acceptor operations are tripwires.
pub(super) struct RespawnTestStarter {
    pub(super) startup_custody: Arc<worker_startup::StartupCustody>,
    pub(super) handshake_gate: Option<Arc<StartupDelay>>,
    pub(super) requests: Arc<Mutex<Vec<WorkerSpawnParams>>>,
    pub(super) fixture_root: Option<PathBuf>,
    pub(super) attempts: std::sync::Arc<std::sync::atomic::AtomicU32>,
    pub(super) succeed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub(super) predecessor_was_stopped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub(super) native_driver:
        Arc<Mutex<Option<crate::native_driver_probe::NativeDriverObservation>>>,
}

impl Drop for RespawnTestStarter {
    fn drop(&mut self) {
        if let Some(root) = self.fixture_root.take() {
            std::fs::remove_dir_all(root).expect("remove only this fixture's newly created root");
        }
    }
}

impl NbdRuntimeStarter for RespawnTestStarter {
    fn try_claim_shutdown(&mut self, _: &str) -> Option<ShutdownPermit> {
        Some(ShutdownPermit::injected(()))
    }

    fn lock_memory(
        &mut self,
        _force: bool,
        _lock_future: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        panic!("worker recovery must not touch memory locking")
    }
    fn start_acceptor(
        &mut self,
        _listener: std::os::unix::net::UnixListener,
        _exports: std::sync::Arc<Vec<ramshared_block::handshake::Export>>,
        _tx_flags: u16,
        _jobs_tx: std::sync::mpsc::SyncSender<super::WMsg>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        panic!("worker recovery must not restart the acceptor")
    }
    fn nbd_used_kb(&mut self, _nbd_dev: &str) -> u64 {
        panic!("worker recovery must not probe swap")
    }
    fn publish_demote(&mut self, _status: &ramshared_wsl2d::DemoteStatusFile) {
        panic!("worker recovery must not publish demotes")
    }
    fn elapsed_us(&mut self, _started: Instant) -> u64 {
        0
    }
    fn spawn_swapoff(&mut self, _nbd_dev: &str) -> std::sync::mpsc::Receiver<SwapoffOutcome> {
        panic!("worker recovery must not swapoff")
    }
    fn spawn_recovery_activation(
        &mut self,
        _nbd_dev: &str,
        _priority: i16,
    ) -> Result<std::sync::mpsc::Receiver<bool>, Box<dyn std::error::Error>> {
        panic!("worker recovery must not run recovery activation")
    }
    fn startup_budget(
        &mut self,
        _requested: bool,
    ) -> Result<Option<Box<dyn NbdBudgetProvider>>, Box<dyn std::error::Error>> {
        panic!("worker recovery must not initialize DXG")
    }

    fn begin_isolated_worker_start(
        &mut self,
        params: &WorkerSpawnParams,
    ) -> std::io::Result<worker_startup::Startup> {
        let mut owned = RespawnTestStarter {
            startup_custody: Arc::clone(&self.startup_custody),
            handshake_gate: self.handshake_gate.clone(),
            requests: Arc::clone(&self.requests),
            fixture_root: None,
            attempts: Arc::clone(&self.attempts),
            succeed: Arc::clone(&self.succeed),
            predecessor_was_stopped: Arc::clone(&self.predecessor_was_stopped),
            native_driver: Arc::clone(&self.native_driver),
        };
        let params = params.clone();
        let custody = Arc::clone(&self.startup_custody);
        custody.begin(Box::new(move || {
            worker_startup::StartOutcome::fixture(owned.spawn_isolated_gpu_worker(&params))
        }))
    }

    fn begin_isolated_worker_start_guarded(
        &mut self,
        params: &WorkerSpawnParams,
        authority: worker_startup::StartupAuthority,
    ) -> std::io::Result<worker_startup::Startup> {
        let mut owned = RespawnTestStarter {
            startup_custody: Arc::clone(&self.startup_custody),
            handshake_gate: self.handshake_gate.clone(),
            requests: Arc::clone(&self.requests),
            fixture_root: None,
            attempts: Arc::clone(&self.attempts),
            succeed: Arc::clone(&self.succeed),
            predecessor_was_stopped: Arc::clone(&self.predecessor_was_stopped),
            native_driver: Arc::clone(&self.native_driver),
        };
        let params = params.clone();
        self.startup_custody.begin_guarded(
            Box::new(move || {
                worker_startup::StartOutcome::fixture(owned.spawn_isolated_gpu_worker(&params))
            }),
            authority,
        )
    }

    fn spawn_isolated_gpu_worker(
        &mut self,
        params: &WorkerSpawnParams,
    ) -> worker_startup::InitialStartResult {
        let (target_bytes, _chunk_bytes, _reserve_floor) = (
            params.target_bytes,
            params.chunk_bytes,
            params.reserve_floor,
        );
        self.requests.lock().unwrap().push(params.clone());
        self.attempts.fetch_add(1, Ordering::SeqCst);
        assert_eq!(target_bytes, 4 * 1024 * 1024);
        assert!(
            self.predecessor_was_stopped.load(Ordering::SeqCst),
            "the previous worker must be shut down before a replacement is spawned"
        );
        if !self.succeed.load(Ordering::SeqCst) {
            return Err("manufactured spawn failure".into());
        }
        use ramshared_block::gpu_cache_worker::{
            FRAME_HEADER_LEN, FrameHeader, MSG_DISABLE_REQ, MSG_DISABLE_RESP, MSG_HANDSHAKE_REQ,
            MSG_HANDSHAKE_RESP, MSG_HEARTBEAT_REQ, MSG_HEARTBEAT_RESP, STATUS_OK,
        };
        use std::io::{Read, Write};

        let (client_socket, mut worker_socket) = std::os::unix::net::UnixStream::pair()?;
        worker_socket
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let worker_stop = std::sync::Arc::clone(&stop);
        let handshake_gate = self.handshake_gate.clone();
        let adapter = params.expected_adapter.clone();
        let handle = std::thread::spawn(move || {
            while !worker_stop.load(Ordering::SeqCst) {
                let mut bytes = [0u8; FRAME_HEADER_LEN];
                match worker_socket.read_exact(&mut bytes) {
                    Ok(()) => {
                        let Some(request) = FrameHeader::decode(&bytes) else {
                            break;
                        };
                        if request.msg_type == MSG_HANDSHAKE_REQ
                            && let Some(gate) = &handshake_gate
                        {
                            gate.entered.send(()).unwrap();
                            gate.release
                                .lock()
                                .unwrap()
                                .recv_timeout(Duration::from_secs(5))
                                .unwrap();
                        }
                        let response_type = match request.msg_type {
                            MSG_HANDSHAKE_REQ => MSG_HANDSHAKE_RESP,
                            MSG_HEARTBEAT_REQ => MSG_HEARTBEAT_RESP,
                            MSG_DISABLE_REQ => MSG_DISABLE_RESP,
                            _ => break,
                        };
                        // AUD-34: DISABLE is acknowledged with the
                        // empty/zeroed contract, never with a carried
                        // occupancy. Handshake and heartbeat report the
                        // admitted target in `offset`.
                        let payload = if request.msg_type == MSG_HEARTBEAT_REQ {
                            fixture_worker_budget_payload(adapter.clone())
                        } else {
                            Vec::new()
                        };
                        let response = FrameHeader {
                            msg_type: response_type,
                            status: STATUS_OK,
                            correlation_id: request.correlation_id,
                            offset: if request.msg_type == MSG_DISABLE_REQ {
                                0
                            } else {
                                4 * 1024 * 1024
                            },
                            payload_len: payload.len() as u32,
                            aux: 0,
                        };
                        if worker_socket
                            .write_all(&response.encode())
                            .and_then(|()| worker_socket.write_all(&payload))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => break,
                }
            }
        });
        let mut client =
            IpcCacheClient::new(client_socket, Duration::from_millis(50), 4 * 1024 * 1024);
        if let Some(expected) = &params.expected_adapter {
            client
                .expect_adapter(expected.clone())
                .map_err(std::io::Error::other)?;
        }
        client.perform_handshake().map_err(std::io::Error::other)?;
        gpu_worker_request::complete_startup_binding(&mut client).map_err(std::io::Error::other)?;
        Ok((
            client,
            IsolatedWorkerSupervisor {
                child: Some(WorkerChildHandle::Thread {
                    stop,
                    handle: Some(handle),
                }),
            },
        ))
    }

    fn native_gpu_driver_observation(
        &self,
    ) -> Option<crate::native_driver_probe::NativeDriverObservation> {
        self.native_driver.lock().unwrap().clone()
    }
}

pub(super) fn respawn_fixture(
    succeed: bool,
    predecessor_was_stopped: bool,
) -> (
    AuthoritativeOriginBackend<FileOrigin, OriginCache>,
    IsolatedWorkerRuntime,
    RespawnTestStarter,
) {
    respawn_fixture_with_admission(
        succeed,
        predecessor_was_stopped,
        ramshared_block::RefuseCacheAdmission,
    )
}

pub(super) fn respawn_fixture_with_admission(
    succeed: bool,
    predecessor_was_stopped: bool,
    admission: impl ramshared_block::CacheAdmission + 'static,
) -> (
    AuthoritativeOriginBackend<FileOrigin, OriginCache>,
    IsolatedWorkerRuntime,
    RespawnTestStarter,
) {
    let root = std::env::temp_dir().join(format!(
        "ramshared-worker-respawn-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    let origin_file = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(root.join("origin.bin"))
        .unwrap();
    origin_file.set_len(16 * 1024 * 1024).unwrap();
    let backend = AuthoritativeOriginBackend::new_with_admission(
        FileOrigin::from_file(origin_file),
        OriginCache::Disabled(DisabledCache),
        16 * 1024 * 1024,
        BLOCK_SIZE,
        admission,
    )
    .unwrap();
    let runtime = IsolatedWorkerRuntime {
        cache_member: 0,
        startup: None,
        supervisor: None,
        params: WorkerSpawnParams {
            generation: None,
            target_bytes: 4 * 1024 * 1024,
            chunk_bytes: 1024 * 1024,
            reserve_floor: 2 * 1024 * 1024,
            expected_adapter: None,
            host_catalog: None,
        },
        budget: WorkerRespawnBudget::new(),
        unavailable_reason: None,
        lease_lost_release: false,
        permanent_fallback_logged: false,
    };
    let starter = RespawnTestStarter {
        startup_custody: Arc::new(worker_startup::StartupCustody::default()),
        handshake_gate: None,
        requests: Arc::new(Mutex::new(Vec::new())),
        fixture_root: Some(root),
        attempts: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
        succeed: std::sync::Arc::new(AtomicBool::new(succeed)),
        predecessor_was_stopped: std::sync::Arc::new(AtomicBool::new(predecessor_was_stopped)),
        native_driver: Arc::new(Mutex::new(None)),
    };
    (backend, runtime, starter)
}

pub(super) fn recover_and_settle(
    starter: &mut RespawnTestStarter,
    backend: &AuthoritativeOriginBackend<FileOrigin, OriginCache>,
    runtime: &mut IsolatedWorkerRuntime,
    lease_live: bool,
) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        recover_isolated_gpu_worker_if_failed(starter, backend, runtime, lease_live);
        if runtime.startup.is_none() {
            return;
        }
        assert!(Instant::now() < deadline, "fixture startup did not finish");
        std::thread::sleep(Duration::from_millis(1));
    }
}

pub(super) fn fixture_worker_budget_payload(adapter: Option<GpuAdapterIdentity>) -> Vec<u8> {
    let now = unix_time_ms().expect("fixture wall clock");
    let budget = ramshared_vram::GpuBudgetTelemetry::from_snapshot(
        &GpuBudgetSnapshot {
            adapter: Some(adapter.unwrap_or_else(|| GpuAdapterIdentity {
                node_mask: None,
                backend: "cuda".into(),
                key: "00112233445566778899aabbccddeeff".into(),
                luid: None,
            })),
            total_bytes: Some(8 * GIB),
            budget_bytes: 8 * GIB,
            used_bytes: 0,
            source: ramshared_vram::GpuBudgetSource::DriverReported,
            sampled_at: Instant::now(),
        },
        now,
    );
    ramshared_vram::WorkerTelemetryEnvelope::new(now, Some(budget), None, None)
        .to_bounded_payload()
        .unwrap()
}
