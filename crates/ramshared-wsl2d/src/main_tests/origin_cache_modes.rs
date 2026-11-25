// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main_tests/origin_cache_modes.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;

#[test]
fn product_origin_mode_does_not_preallocate_logical_capacity() {
    struct CountingProvider(std::cell::Cell<u64>);
    impl VramProvider for CountingProvider {
        type Mem<'a> = TestMemory;

        fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, ramshared_vram::VramError> {
            self.0.set(self.0.get().saturating_add(bytes as u64));
            Ok(TestMemory::new(bytes))
        }

        fn mem_info(&self) -> Result<(u64, u64), ramshared_vram::VramError> {
            panic!("origin composition must not query the GPU provider")
        }
    }

    let provider = CountingProvider(std::cell::Cell::new(0));
    let fail = std::rc::Rc::new(std::cell::Cell::new(false));
    let origin = TestOrigin {
        bytes: std::cell::RefCell::new(vec![0; BLOCK_SIZE as usize]),
        fail,
    };
    let cache = AuthoritativeOriginBackend::new(origin, DisabledCache, GIB, BLOCK_SIZE)
        .expect("valid 1 GiB authoritative origin");

    assert_eq!(cache.size_bytes(), GIB);
    assert_eq!(cache.cached_bytes(), 0);
    assert_eq!(provider.0.get(), 0, "constructor must allocate no VRAM");
    drop(cache);

    struct OriginStarter {
        published_origin_status: Option<(bool, &'static str, &'static str)>,
        replies: Option<std::sync::mpsc::Receiver<Reply>>,
        client: Option<std::thread::JoinHandle<Result<(), String>>>,
    }
    impl NbdRuntimeStarter for OriginStarter {
        fn try_claim_shutdown(&mut self, _: &str) -> Option<ShutdownPermit> {
            Some(ShutdownPermit::injected(()))
        }

        fn lock_memory(
            &mut self,
            _force: bool,
            lock_future: bool,
        ) -> Result<(), Box<dyn std::error::Error>> {
            assert!(
                !lock_future,
                "origin mode must never arm MCL_FUTURE before cache mappings"
            );
            Ok(())
        }

        fn start_acceptor(
            &mut self,
            _listener: UnixListener,
            exports: std::sync::Arc<Vec<ramshared_block::handshake::Export>>,
            _tx_flags: u16,
            jobs_tx: std::sync::mpsc::SyncSender<WMsg>,
        ) -> Result<(), Box<dyn std::error::Error>> {
            assert_eq!(exports[0].size, GIB);
            let (observed_tx, observed_rx) = std::sync::mpsc::channel();
            self.client = Some(std::thread::spawn(move || {
                let result = (|| {
                    jobs_tx
                        .send(WMsg::Opened)
                        .map_err(|error| error.to_string())?;
                    // Dependent requests follow acknowledgements. The pool
                    // deliberately permits concurrent requests to complete out of order.
                    for (command, handle, length, payload) in [
                        (
                            Command::Write,
                            101,
                            BLOCK_SIZE,
                            vec![0xA5; BLOCK_SIZE as usize],
                        ),
                        (Command::Read, 102, BLOCK_SIZE, Vec::new()),
                        (Command::Flush, 103, 0, Vec::new()),
                    ] {
                        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
                        jobs_tx
                            .send(WMsg::Job(ramshared_wsl2d::conn::Job {
                                export: 0,
                                req: ramshared_block::Request {
                                    flags: 0,
                                    cmd: command,
                                    handle,
                                    offset: 0,
                                    len: length,
                                },
                                payload,
                                credit: fixture_credit(),
                                reply: reply_tx,
                            }))
                            .map_err(|error| error.to_string())?;
                        let reply = reply_rx
                            .recv_timeout(Duration::from_secs(5))
                            .map_err(|error| format!("origin fixture reply {handle}: {error}"))?;
                        observed_tx.send(reply).map_err(|error| error.to_string())?;
                    }
                    Ok(())
                })();
                // Always request shutdown, so a fixture assertion cannot leave
                // the runtime waiting forever for a producer that has exited.
                let _ = jobs_tx.send(WMsg::Shutdown);
                result
            }));
            self.replies = Some(observed_rx);
            Ok(())
        }

        fn nbd_used_kb(&mut self, _nbd_dev: &str) -> u64 {
            0
        }

        fn nbd_swap_is_explicitly_absent(&mut self, _nbd_dev: &str) -> bool {
            true
        }

        fn publish_demote(&mut self, _status: &ramshared_wsl2d::DemoteStatusFile) {}

        fn publish_origin_cache(&mut self, status: &OriginCacheStatus) {
            self.published_origin_status =
                Some((status.ok, status.origin_state, status.cache_state));
        }

        fn elapsed_us(&mut self, _started: Instant) -> u64 {
            0
        }

        fn spawn_swapoff(&mut self, _nbd_dev: &str) -> std::sync::mpsc::Receiver<SwapoffOutcome> {
            panic!("origin-cache clean shutdown must not swapoff an inactive fixture")
        }

        fn spawn_recovery_activation(
            &mut self,
            _nbd_dev: &str,
            _priority: i16,
        ) -> Result<std::sync::mpsc::Receiver<bool>, Box<dyn std::error::Error>> {
            panic!("origin-cache fixture must not start recovery activation")
        }

        fn startup_budget(
            &mut self,
            _requested: bool,
        ) -> Result<Option<Box<dyn NbdBudgetProvider>>, Box<dyn std::error::Error>> {
            panic!("origin composition must not initialize DXG")
        }

        fn origin_cache_target_bytes(
            &mut self,
            logical_size: u64,
        ) -> Result<u64, Box<dyn std::error::Error>> {
            assert_eq!(logical_size, GIB);
            Ok(256 * 1024 * 1024)
        }

        fn begin_isolated_worker_start(
            &mut self,
            _: &WorkerSpawnParams,
        ) -> std::io::Result<worker_startup::Startup> {
            Arc::new(worker_startup::StartupCustody::default()).begin(Box::new(|| {
                worker_startup::StartOutcome {
                    result: Err("origin test fixture uses disabled cache fallback".into()),
                    creator_child: None,
                }
            }))
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
            assert_eq!(target_bytes, 256 * 1024 * 1024);
            Err("origin test fixture uses disabled cache fallback".into())
        }
    }

    let root = std::env::temp_dir().join(format!("ramshared-origin-mode-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir(&root).unwrap();
    let origin_path = root.join("origin.img");
    let origin_file = File::options()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&origin_path)
        .unwrap();
    origin_file.set_len(GIB).unwrap();
    let socket = root.join("daemon.sock");
    let mut starter = OriginStarter {
        published_origin_status: None,
        replies: None,
        client: None,
    };
    run_nbd_with_startup(
        provider,
        Some(OpenedOrigin::Wsl(
            FileOrigin::from_file(origin_file),
            SealedOrigin {
                logical_capacity_mib: 1024,
                partuuid: "11111111-2222-4333-8444-555555555555".into(),
                origin_vhdx: "fixture-origin".into(),
            },
        )),
        GIB,
        socket.to_string_lossy().into_owned(),
        false,
        "/dev/ramshared-test-nbd".into(),
        true,
        serve_pool::DEFAULT_SERVE_THREADS,
        &mut starter,
    )
    .expect("tempfile origin mode must compose without a GPU or NBD device");
    starter
        .client
        .take()
        .expect("origin client fixture")
        .join()
        .expect("origin client thread")
        .expect("dependent origin requests complete");
    assert_eq!(
        starter.published_origin_status,
        Some((true, "READY", "UNAVAILABLE")),
        "origin readiness must be published before the serving loop can idle or shut down"
    );
    let replies = starter
        .replies
        .take()
        .expect("origin request replies")
        .try_iter()
        .collect::<Vec<_>>();
    assert_eq!(replies.len(), 3, "origin READ, WRITE, and FLUSH all reply");
    assert!(replies.iter().all(|reply| {
        u32::from_be_bytes([
            reply.reply[4],
            reply.reply[5],
            reply.reply[6],
            reply.reply[7],
        ]) == ramshared_block::protocol::NBD_OK
    }));
    // Handle-addressed, never positional: the serve pool completes jobs in
    // worker order (DT-2 of wsl2-nbd-serving-throughput — no ordering
    // guarantee), so the READ's payload is matched by its handle (102).
    let reply_for = |handle: u64| {
        replies
            .iter()
            .find(|reply| u64::from_be_bytes(reply.reply[8..16].try_into().unwrap()) == handle)
            .unwrap_or_else(|| panic!("no reply for handle {handle}"))
    };
    assert_eq!(reply_for(102).data, vec![0xA5; BLOCK_SIZE as usize]);
    assert!(
        reply_for(101).data.is_empty(),
        "WRITE replies carry no data"
    );
    assert!(
        reply_for(103).data.is_empty(),
        "FLUSH replies carry no data"
    );
    assert!(!socket.exists());
    std::fs::remove_dir_all(root).unwrap();

    let args = AppArgs::parse_from(&daemon_argv(&[
        "ramsharedd",
        "--origin-manifest",
        ORIGIN_MANIFEST_PATH,
    ]))
    .expect("sealed origin manifest args");
    let wsl2_error = match select_daemon_action_for(args, Some(true)) {
        Err(error) => error,
        Ok(_) => panic!("the product NBD swap path needs the operator's acknowledgment"),
    };
    assert!(wsl2_error.to_string().contains("NBD"));
    assert!(
        wsl2_error
            .to_string()
            .contains("keep the configured WSL fallback swap active")
    );
    assert!(
        wsl2_error.to_string().contains("--accept-wsl2-nbd"),
        "the refusal names the acknowledgment: {wsl2_error}"
    );

    // The explicit acknowledgment admits a measured WSL2 host, and never a platform the probe did not measure.
    let accepted = || {
        AppArgs::parse_from(&daemon_argv(&[
            "ramsharedd",
            "--origin-manifest",
            ORIGIN_MANIFEST_PATH,
            "--accept-wsl2-nbd",
        ]))
        .expect("sealed origin manifest args with the acknowledgment")
    };
    assert!(matches!(
        select_daemon_action_for(accepted(), Some(true)),
        Ok(DaemonAction::Nbd(_))
    ));
    assert!(
        select_daemon_action_for(accepted(), None).is_err(),
        "an unprobed platform refuses even with the acknowledgment"
    );

    let native_args = AppArgs::parse_from(&daemon_argv(&[
        "ramsharedd",
        "--origin-manifest",
        ORIGIN_MANIFEST_PATH,
    ]))
    .expect("sealed origin manifest args for native Linux");
    assert!(matches!(
        select_daemon_action_for(native_args, Some(false)),
        Ok(DaemonAction::Nbd(_))
    ));

    let missing_origin = AppArgs::parse_from(&daemon_argv(&["ramsharedd", "--size", "1024"]))
        .expect("syntactically valid originless size args");
    assert!(select_daemon_action(missing_origin).is_err());

    let unavailable = UnavailableVramProvider;
    assert!(
        unavailable
            .alloc(ramshared_block::ORIGIN_CACHE_CHUNK_BYTES as usize)
            .is_err()
    );
    assert!(unavailable.mem_info().is_err());
    let mut memory = UnavailableVramMemory;
    assert_eq!(memory.len(), 0);
    assert!(memory.zero().is_err());
    assert!(memory.read_at(0, &mut [0; 1]).is_err());
    assert!(memory.write_at(0, &[0; 1]).is_err());
}

#[test]
fn product_origin_mode_keeps_a_live_injected_cache_worker_until_shutdown() {
    struct Provider;

    impl VramProvider for Provider {
        type Mem<'a> = TestMemory;

        fn alloc(&self, _bytes: usize) -> Result<Self::Mem<'_>, ramshared_vram::VramError> {
            panic!("origin composition must not allocate through the daemon provider")
        }

        fn mem_info(&self) -> Result<(u64, u64), ramshared_vram::VramError> {
            panic!("origin composition must not query the daemon GPU provider")
        }
    }

    struct ActiveOriginStarter {
        published: Option<(bool, &'static str, &'static str, u64, Option<u64>)>,
    }

    impl NbdRuntimeStarter for ActiveOriginStarter {
        fn try_claim_shutdown(&mut self, _: &str) -> Option<ShutdownPermit> {
            Some(ShutdownPermit::injected(()))
        }

        fn lock_memory(
            &mut self,
            _force: bool,
            lock_future: bool,
        ) -> Result<(), Box<dyn std::error::Error>> {
            assert!(!lock_future);
            Ok(())
        }

        fn start_acceptor(
            &mut self,
            _listener: UnixListener,
            exports: std::sync::Arc<Vec<ramshared_block::handshake::Export>>,
            _tx_flags: u16,
            jobs_tx: std::sync::mpsc::SyncSender<WMsg>,
        ) -> Result<(), Box<dyn std::error::Error>> {
            assert_eq!(exports[0].size, GIB);
            jobs_tx.send(WMsg::Shutdown)?;
            Ok(())
        }

        fn nbd_used_kb(&mut self, _nbd_dev: &str) -> u64 {
            0
        }

        fn nbd_swap_is_explicitly_absent(&mut self, _nbd_dev: &str) -> bool {
            true
        }

        fn publish_demote(&mut self, _status: &ramshared_wsl2d::DemoteStatusFile) {}

        fn publish_origin_cache(&mut self, status: &OriginCacheStatus) {
            self.published = Some((
                status.ok,
                status.origin_state,
                status.cache_state,
                status.cache_target_kib,
                status.vram_cached_kib,
            ));
        }

        fn elapsed_us(&mut self, _started: Instant) -> u64 {
            0
        }

        fn spawn_swapoff(&mut self, _nbd_dev: &str) -> std::sync::mpsc::Receiver<SwapoffOutcome> {
            panic!("origin worker shutdown does not call swapoff")
        }

        fn spawn_recovery_activation(
            &mut self,
            _nbd_dev: &str,
            _priority: i16,
        ) -> Result<std::sync::mpsc::Receiver<bool>, Box<dyn std::error::Error>> {
            panic!("origin worker shutdown does not activate swap")
        }

        fn startup_budget(
            &mut self,
            _requested: bool,
        ) -> Result<Option<Box<dyn NbdBudgetProvider>>, Box<dyn std::error::Error>> {
            panic!("origin mode must not initialize a DXG budget provider")
        }

        fn origin_cache_target_bytes(
            &mut self,
            logical_size: u64,
        ) -> Result<u64, Box<dyn std::error::Error>> {
            assert_eq!(logical_size, GIB);
            Ok(256 * 1024 * 1024)
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
            use ramshared_block::gpu_cache_worker::{
                FRAME_HEADER_LEN, FrameHeader, MSG_DISABLE_REQ, MSG_DISABLE_RESP,
                MSG_HANDSHAKE_REQ, MSG_HANDSHAKE_RESP, MSG_HEARTBEAT_REQ, MSG_HEARTBEAT_RESP,
                STATUS_OK,
            };
            use std::io::{Read, Write};

            assert_eq!(target_bytes, 256 * 1024 * 1024);
            let (client_socket, mut worker_socket) = std::os::unix::net::UnixStream::pair()?;
            worker_socket.set_read_timeout(Some(Duration::from_millis(20)))?;
            let stop = std::sync::Arc::new(AtomicBool::new(false));
            let worker_stop = std::sync::Arc::clone(&stop);
            let worker = std::thread::spawn(move || {
                while !worker_stop.load(Ordering::SeqCst) {
                    let mut bytes = [0u8; FRAME_HEADER_LEN];
                    match worker_socket.read_exact(&mut bytes) {
                        Ok(()) => {
                            let Some(request) = FrameHeader::decode(&bytes) else {
                                break;
                            };
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
                            let response = FrameHeader {
                                msg_type: response_type,
                                status: STATUS_OK,
                                correlation_id: request.correlation_id,
                                offset: if request.msg_type == MSG_DISABLE_REQ {
                                    0
                                } else {
                                    target_bytes
                                },
                                payload_len: 0,
                                aux: 0,
                            };
                            if worker_socket.write_all(&response.encode()).is_err() {
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
                IpcCacheClient::new(client_socket, Duration::from_millis(50), target_bytes);
            client.perform_handshake().map_err(std::io::Error::other)?;
            let supervisor = IsolatedWorkerSupervisor {
                child: Some(WorkerChildHandle::Thread {
                    stop,
                    handle: Some(worker),
                }),
            };
            Ok((client, supervisor))
        }
    }

    let root = std::env::temp_dir().join(format!(
        "ramshared-origin-active-worker-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir(&root).expect("create isolated origin fixture");
    let origin_path = root.join("origin.img");
    let origin_file = File::options()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&origin_path)
        .expect("create sparse origin fixture");
    origin_file
        .set_len(GIB)
        .expect("size sparse origin fixture");
    let socket = root.join("daemon.sock");
    let mut starter = ActiveOriginStarter { published: None };

    run_nbd_with_startup(
        Provider,
        Some(OpenedOrigin::Wsl(
            FileOrigin::from_file(origin_file),
            SealedOrigin {
                logical_capacity_mib: 1024,
                partuuid: "11111111-2222-4333-8444-555555555555".into(),
                origin_vhdx: "fixture-origin".into(),
            },
        )),
        GIB,
        socket.to_string_lossy().into_owned(),
        false,
        "/dev/ramshared-test-nbd".into(),
        true,
        serve_pool::DEFAULT_SERVE_THREADS,
        &mut starter,
    )
    .expect("injected live cache worker serves the bounded origin lifecycle");

    assert_eq!(
        starter.published,
        Some((true, "READY", "ACTIVE", 256 * 1024, Some(0))),
        "the daemon publishes the handshake-confirmed worker and target"
    );
    assert!(!socket.exists(), "shutdown removes the owned Unix socket");
    std::fs::remove_dir_all(root).expect("remove isolated origin fixture");
}

#[test]
fn missing_gpu_measurement_sets_zero_cache_target() {
    assert_eq!(
        ramshared_block::physical_target_bytes(
            4 * GIB,
            None,
            0,
            ramshared_block::gpu_cache_worker::RUNTIME_FREE_BUFFER_BYTES,
            std::time::Instant::now()
        ),
        0
    );
}

#[test]
fn critical_supervisor_request_is_consumed_and_reclaims_daemon_cache_to_zero() {
    struct AllocatingProvider;
    impl VramProvider for AllocatingProvider {
        type Mem<'a> = TestMemory;

        fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, ramshared_vram::VramError> {
            Ok(TestMemory::new(bytes))
        }

        fn mem_info(&self) -> Result<(u64, u64), ramshared_vram::VramError> {
            Ok((16 * GIB, 16 * GIB))
        }
    }

    let provider = AllocatingProvider;
    let origin = TestOrigin {
        bytes: std::cell::RefCell::new(vec![0; 32]),
        fail: std::rc::Rc::new(std::cell::Cell::new(false)),
    };
    let mut cache =
        WriteThroughCacheBackend::with_chunk_bytes(&provider, origin, 32, 4, 8).unwrap();
    cache.set_physical_cap_bytes(8);
    let now = std::time::Instant::now();
    let sample = GpuSample {
        budget_bytes: 16 * GIB,
        external_usage_bytes: 0,
        total_vram_bytes: 16 * GIB,
        sampled_at: now,
        source: ramshared_vram::GpuBudgetSource::DriverReported,
        adapter: Some(ramshared_vram::GpuAdapterIdentity {
            node_mask: None,
            backend: "test".into(),
            key: "test-adapter".into(),
            luid: None,
        }),
    };
    for seconds in 0..3 {
        cache.observe_gpu(Some(&sample), Duration::from_secs(seconds), now);
    }
    assert_eq!(cache.cached_bytes(), 8);

    let request = serde_json::json!({
        "schema_version": 1,
        "target_bytes": 0,
        "reason": "control_pressure",
        "daemon_instance_id": "fixture-daemon",
        "issued_at_unix_ms": 1_000,
    });

    let root = std::env::temp_dir().join(format!(
        "ramshared-critical-cache-request-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let cache_target = root.join("cache-target.json");
    let reclaim_request = root.join("reclaim-request.json");
    std::fs::write(&cache_target, request.to_string()).unwrap();
    assert!(critical_cache_reclaim_requested_at(
        &cache_target,
        &reclaim_request,
        "fixture-daemon",
        1_001,
    ));
    assert_eq!(
        consume_critical_cache_request(&request, "fixture-daemon", 1_001),
        Some(0)
    );
    assert_eq!(cache.release_cache(), 8);
    assert_eq!(cache.cached_bytes(), 0);
    assert_eq!(
        consume_critical_cache_request(&request, "foreign-daemon", 1_001),
        None
    );
    assert_eq!(
        consume_critical_cache_request(&request, "fixture-daemon", 16_001),
        None
    );
    assert_eq!(
        consume_critical_cache_request(
            &serde_json::json!({"target_bytes": 0}),
            "fixture-daemon",
            1_001
        ),
        None
    );
    std::fs::remove_dir_all(root).unwrap();
}
