// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main_tests/broker_setup_and_nbd_basic.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;

#[test]
fn daemon_broker_setup_stops_bounded_without_backend() {
    struct RecordingAcceptors {
        unix_started: bool,
        tcp_started: bool,
        exports: usize,
    }

    impl BrokerAcceptorStarter for RecordingAcceptors {
        fn start(
            &mut self,
            unix: UnixListener,
            tcp: Option<std::net::TcpListener>,
            exports: std::sync::Arc<Vec<ramshared_block::handshake::Export>>,
            _tx_flags: u16,
            _jobs_tx: std::sync::mpsc::SyncSender<WMsg>,
        ) -> Result<(), Box<dyn std::error::Error>> {
            self.unix_started = unix.local_addr().is_ok();
            self.tcp_started = tcp
                .as_ref()
                .and_then(|listener| listener.local_addr().ok())
                .is_some_and(|addr| addr.ip().is_loopback());
            self.exports = exports.len();
            Ok(())
        }
    }

    let path = std::env::temp_dir().join(format!(
        "ramshared-daemon-bounded-broker-{}.sock",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let shutdown = std::sync::Arc::new(AtomicBool::new(false));
    let mut acceptors = RecordingAcceptors {
        unix_started: false,
        tcp_started: false,
        exports: 0,
    };
    let runtime = broker_setup_with_acceptors(
        1,
        4096,
        path.to_str().expect("temporary path is UTF-8"),
        Some("127.0.0.1:0".parse().expect("loopback NBD listener")),
        Some(("127.0.0.1".into(), 10809)),
        "127.0.0.1:0".parse().expect("loopback arbiter listener"),
        None,
        std::sync::Arc::clone(&shutdown),
        Duration::from_millis(1),
        &mut acceptors,
    )
    .expect("safe temporary broker control plane must start");
    assert!(
        acceptors.unix_started,
        "temporary Unix listener was handed off"
    );
    assert!(
        acceptors.tcp_started,
        "loopback TCP listener was handed off"
    );
    assert_eq!(acceptors.exports, 1, "slice geometry becomes one export");

    assert!(matches!(
        runtime.shutdown.request(),
        BrokerShutdownWake::Queued | BrokerShutdownWake::QueueFull
    ));
    let (worker, broker, _shutdown, socket, _shutdown_bridge) = runtime.into_parts();
    let _backend = serve_broker_jobs_with_poll(
        RamBackend::new(4096),
        worker,
        |_| None,
        Duration::from_millis(1),
    );
    broker
        .join()
        .expect("broker exits after its injected shutdown");
    drop(socket);
    assert!(
        !path.exists(),
        "owned temporary socket is cleaned after stop"
    );
}

#[test]
fn daemon_broker_acceptor_failure_rolls_back_owned_socket() {
    struct FailingAcceptors;

    impl BrokerAcceptorStarter for FailingAcceptors {
        fn start(
            &mut self,
            _unix: UnixListener,
            _tcp: Option<std::net::TcpListener>,
            _exports: std::sync::Arc<Vec<ramshared_block::handshake::Export>>,
            _tx_flags: u16,
            _jobs_tx: std::sync::mpsc::SyncSender<WMsg>,
        ) -> Result<(), Box<dyn std::error::Error>> {
            Err(std::io::Error::other("injected acceptor failure").into())
        }
    }

    let path = std::env::temp_dir().join(format!(
        "ramshared-daemon-rollback-broker-{}.sock",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let shutdown = std::sync::Arc::new(AtomicBool::new(false));
    let result = broker_setup_with_acceptors(
        1,
        4096,
        path.to_str().expect("temporary path is UTF-8"),
        None,
        None,
        "127.0.0.1:0".parse().expect("loopback arbiter listener"),
        None,
        std::sync::Arc::clone(&shutdown),
        Duration::from_millis(1),
        &mut FailingAcceptors,
    );
    let error = match result {
        Ok(_) => panic!("injected acceptor failure must refuse startup"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("injected acceptor failure"));
    assert!(
        shutdown.load(Ordering::SeqCst),
        "failure signals broker shutdown"
    );
    assert!(
        !path.exists(),
        "failure removes only the temporary Unix socket created by this attempt"
    );
}

#[test]
fn daemon_nbd_serves_two_connection_generations_before_explicit_shutdown() {
    struct TestProvider;

    impl VramProvider for TestProvider {
        type Mem<'a>
            = TestMemory
        where
            Self: 'a;

        fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, ramshared_vram::VramError> {
            Ok(TestMemory::new(bytes))
        }

        fn mem_info(&self) -> Result<(u64, u64), ramshared_vram::VramError> {
            Ok((8 * 1024 * 1024 * 1024, 8 * 1024 * 1024 * 1024))
        }
    }

    struct SafeStarter {
        lock_calls: usize,
        saw_socket: bool,
        zero_done: Option<std::sync::mpsc::Receiver<bool>>,
        reply: Option<std::sync::mpsc::Receiver<Reply>>,
        status_updates: Vec<ramshared_wsl2d::DemoteStatusFile>,
    }

    impl NbdRuntimeStarter for SafeStarter {
        fn try_claim_shutdown(&mut self, _: &str) -> Option<ShutdownPermit> {
            Some(ShutdownPermit::injected(()))
        }

        fn lock_memory(
            &mut self,
            _force: bool,
            _lock_future: bool,
        ) -> Result<(), Box<dyn std::error::Error>> {
            self.lock_calls += 1;
            Ok(())
        }

        fn start_acceptor(
            &mut self,
            listener: UnixListener,
            exports: std::sync::Arc<Vec<ramshared_block::handshake::Export>>,
            _tx_flags: u16,
            jobs_tx: std::sync::mpsc::SyncSender<WMsg>,
        ) -> Result<(), Box<dyn std::error::Error>> {
            self.saw_socket = listener.local_addr().is_ok()
                && exports.len() == 1
                && exports[0].name == "default"
                && exports[0].size == 4096;
            let (zero_tx, zero_rx) = std::sync::mpsc::channel();
            let (reply_tx, reply_rx) = std::sync::mpsc::channel();
            jobs_tx.send(WMsg::Opened)?;
            jobs_tx.send(WMsg::ZeroExport {
                base: 0,
                len: 512,
                done: zero_tx,
            })?;
            jobs_tx.send(WMsg::Job(ramshared_wsl2d::conn::Job {
                export: 0,
                req: ramshared_block::Request {
                    flags: 0,
                    cmd: Command::Write,
                    handle: 99,
                    offset: 0,
                    len: 512,
                },
                payload: vec![0xC3; 512],
                credit: fixture_credit(),
                reply: reply_tx.clone(),
            }))?;
            jobs_tx.send(WMsg::Closed)?;
            jobs_tx.send(WMsg::Opened)?;
            jobs_tx.send(WMsg::Job(ramshared_wsl2d::conn::Job {
                export: 0,
                req: ramshared_block::Request {
                    flags: 0,
                    cmd: Command::Write,
                    handle: 100,
                    offset: 512,
                    len: 512,
                },
                payload: vec![0x5A; 512],
                credit: fixture_credit(),
                reply: reply_tx,
            }))?;
            jobs_tx.send(WMsg::Closed)?;
            jobs_tx.send(WMsg::Shutdown)?;
            self.zero_done = Some(zero_rx);
            self.reply = Some(reply_rx);
            Ok(())
        }

        fn nbd_used_kb(&mut self, _nbd_dev: &str) -> u64 {
            0
        }

        fn nbd_swap_is_explicitly_absent(&mut self, _nbd_dev: &str) -> bool {
            true
        }

        fn publish_demote(&mut self, status: &ramshared_wsl2d::DemoteStatusFile) {
            self.status_updates.push(status.clone());
        }

        fn elapsed_us(&mut self, _started: Instant) -> u64 {
            10
        }

        fn spawn_swapoff(&mut self, _nbd_dev: &str) -> std::sync::mpsc::Receiver<SwapoffOutcome> {
            panic!("the safe sparse fixture must not request swapoff")
        }

        fn spawn_recovery_activation(
            &mut self,
            _nbd_dev: &str,
            _priority: i16,
        ) -> Result<std::sync::mpsc::Receiver<bool>, Box<dyn std::error::Error>> {
            panic!("the safe sparse fixture must not activate swap")
        }
    }

    let path = std::env::temp_dir().join(format!(
        "ramshared-daemon-safe-nbd-{}.sock",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let mut starter = SafeStarter {
        lock_calls: 0,
        saw_socket: false,
        zero_done: None,
        reply: None,
        status_updates: Vec::new(),
    };
    run_nbd_with_startup(
        TestProvider,
        None,
        4096,
        path.to_string_lossy().into_owned(),
        false,
        "/dev/ramshared-test-nbd".into(),
        false,
        serve_pool::DEFAULT_SERVE_THREADS,
        &mut starter,
    )
    .expect("fake sparse worker must complete without a CUDA, swap, or NBD device");

    assert_eq!(starter.lock_calls, 1);
    assert!(
        starter.saw_socket,
        "temporary Unix listener reached injected starter"
    );
    assert_eq!(
        starter
            .zero_done
            .take()
            .expect("zero completion receiver")
            .recv_timeout(Duration::from_secs(1)),
        Ok(true)
    );
    let replies = starter
        .reply
        .take()
        .expect("worker reply receiver")
        .try_iter()
        .collect::<Vec<_>>();
    assert_eq!(replies.len(), 2, "both connection generations are served");
    assert!(replies.iter().all(|reply| !reply.disconnect));
    assert_eq!(
        starter.status_updates,
        vec![ramshared_wsl2d::DemoteStatusFile::new(0, None, false)]
    );
    assert!(
        !path.exists(),
        "temporary socket is removed after fake worker teardown"
    );
}

#[test]
fn daemon_nbd_explicit_shutdown_wakes_idle_runtime_without_timer_dependence() {
    static TEST_SHUTDOWN: AtomicBool = AtomicBool::new(false);
    TEST_SHUTDOWN.store(false, Ordering::SeqCst);
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let bridge = spawn_nbd_shutdown_bridge(&TEST_SHUTDOWN, tx, Duration::from_millis(5));
    assert!(
        rx.try_recv().is_err(),
        "shutdown is not emitted before approval"
    );

    TEST_SHUTDOWN.store(true, Ordering::SeqCst);
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(1)),
        Ok(WMsg::Shutdown)
    ));
    drop(bridge);
    TEST_SHUTDOWN.store(false, Ordering::SeqCst);
}

#[test]
fn daemon_nbd_shutdown_bridge_full_or_disconnected_queue_is_nonblocking() {
    static FULL_SHUTDOWN: AtomicBool = AtomicBool::new(false);
    FULL_SHUTDOWN.store(false, Ordering::SeqCst);
    let (full_tx, full_rx) = std::sync::mpsc::sync_channel(1);
    full_tx.send(WMsg::Opened).expect("fill bounded queue");
    let full_bridge = spawn_nbd_shutdown_bridge(&FULL_SHUTDOWN, full_tx, Duration::from_millis(5));
    FULL_SHUTDOWN.store(true, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(20));
    let started = Instant::now();
    drop(full_bridge);
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(matches!(full_rx.recv(), Ok(WMsg::Opened)));
    assert!(
        full_rx.try_recv().is_err(),
        "a full queue is never blocked on"
    );
    FULL_SHUTDOWN.store(false, Ordering::SeqCst);

    static DISCONNECTED_SHUTDOWN: AtomicBool = AtomicBool::new(false);
    DISCONNECTED_SHUTDOWN.store(false, Ordering::SeqCst);
    let (disconnected_tx, disconnected_rx) = std::sync::mpsc::sync_channel(1);
    drop(disconnected_rx);
    let disconnected_bridge = spawn_nbd_shutdown_bridge(
        &DISCONNECTED_SHUTDOWN,
        disconnected_tx,
        Duration::from_millis(5),
    );
    DISCONNECTED_SHUTDOWN.store(true, Ordering::SeqCst);
    drop(disconnected_bridge);
    DISCONNECTED_SHUTDOWN.store(false, Ordering::SeqCst);
}

#[test]
fn daemon_nbd_sparse_floor_refusal_reclaims_without_provider_allocation() {
    struct FloorProvider;

    impl VramProvider for FloorProvider {
        type Mem<'a>
            = TestMemory
        where
            Self: 'a;

        fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, ramshared_vram::VramError> {
            assert_eq!(
                bytes, CANARY_BYTES,
                "sparse write must refuse before chunk allocation"
            );
            Ok(TestMemory::new(bytes))
        }

        fn mem_info(&self) -> Result<(u64, u64), ramshared_vram::VramError> {
            Ok((0, 8 * 1024 * 1024 * 1024))
        }
    }

    struct SparseStarter {
        replies: Option<std::sync::mpsc::Receiver<Reply>>,
        used_kb_calls: usize,
    }

    impl NbdRuntimeStarter for SparseStarter {
        fn try_claim_shutdown(&mut self, _: &str) -> Option<ShutdownPermit> {
            Some(ShutdownPermit::injected(()))
        }

        fn lock_memory(
            &mut self,
            _force: bool,
            _lock_future: bool,
        ) -> Result<(), Box<dyn std::error::Error>> {
            Ok(())
        }

        fn start_acceptor(
            &mut self,
            _listener: UnixListener,
            _exports: std::sync::Arc<Vec<ramshared_block::handshake::Export>>,
            _tx_flags: u16,
            jobs_tx: std::sync::mpsc::SyncSender<WMsg>,
        ) -> Result<(), Box<dyn std::error::Error>> {
            let (reply_tx, reply_rx) = std::sync::mpsc::channel();
            jobs_tx.send(WMsg::Opened)?;
            jobs_tx.send(WMsg::Job(ramshared_wsl2d::conn::Job {
                export: 0,
                req: ramshared_block::Request {
                    flags: 0,
                    cmd: Command::Write,
                    handle: 1,
                    offset: 0,
                    len: 512,
                },
                payload: vec![0x9A; 512],
                credit: fixture_credit(),
                reply: reply_tx.clone(),
            }))?;
            jobs_tx.send(WMsg::Job(ramshared_wsl2d::conn::Job {
                export: 0,
                req: ramshared_block::Request {
                    flags: 0,
                    cmd: Command::Flush,
                    handle: 2,
                    offset: 0,
                    len: 0,
                },
                payload: Vec::new(),
                credit: fixture_credit(),
                reply: reply_tx,
            }))?;
            jobs_tx.send(WMsg::Closed)?;
            jobs_tx.send(WMsg::Shutdown)?;
            self.replies = Some(reply_rx);
            Ok(())
        }

        fn nbd_used_kb(&mut self, _nbd_dev: &str) -> u64 {
            self.used_kb_calls += 1;
            0
        }

        fn nbd_swap_is_explicitly_absent(&mut self, _nbd_dev: &str) -> bool {
            true
        }

        fn publish_demote(&mut self, _status: &ramshared_wsl2d::DemoteStatusFile) {}

        fn elapsed_us(&mut self, _started: Instant) -> u64 {
            10
        }

        fn spawn_swapoff(&mut self, _nbd_dev: &str) -> std::sync::mpsc::Receiver<SwapoffOutcome> {
            panic!("free-floor refusal must not request swapoff")
        }

        fn spawn_recovery_activation(
            &mut self,
            _nbd_dev: &str,
            _priority: i16,
        ) -> Result<std::sync::mpsc::Receiver<bool>, Box<dyn std::error::Error>> {
            panic!("free-floor refusal must not activate swap")
        }
    }

    let path = std::env::temp_dir().join(format!(
        "ramshared-daemon-sparse-floor-{}.sock",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let mut starter = SparseStarter {
        replies: None,
        used_kb_calls: 0,
    };
    run_nbd_with_startup(
        FloorProvider,
        None,
        4096,
        path.to_string_lossy().into_owned(),
        false,
        "/dev/ramshared-test-nbd".into(),
        false,
        serve_pool::DEFAULT_SERVE_THREADS,
        &mut starter,
    )
    .expect("sparse floor refusal remains a safe daemon outcome");
    assert!(
        starter.used_kb_calls >= 2,
        "worker and teardown use injected usage evidence"
    );
    assert_eq!(
        starter
            .replies
            .take()
            .expect("reply receiver")
            .try_iter()
            .count(),
        2
    );
    assert!(
        !path.exists(),
        "sparse safe teardown removes its temporary socket"
    );
}

#[test]
fn strict_proc_swaps_parser_accepts_valid_rows_and_rejects_ambiguous_state() {
    let header = "Filename Type Size Used Priority";
    let valid = format!("{header}\n/dev/nbd3 partition 8192 17 -2\n");
    let parsed = parse_strict_proc_swaps(&valid).expect("valid proc snapshot");
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].filename, "/dev/nbd3");
    assert_eq!(parsed[0].used_kb, 17);

    let malformed = [
        "",
        "Filename Type Size Used",
        "Filename Type Size Used Priority\n\n",
        "Filename Type Size Used Priority\n/dev/nbd3 partition 1 0",
        "Filename Type Size Used Priority\n/dev/nbd3 loop 1 0 1",
        "Filename Type Size Used Priority\n/dev/nbd3 partition bad 0 1",
        "Filename Type Size Used Priority\n/dev/nbd3 partition 1 bad 1",
        "Filename Type Size Used Priority\n/dev/nbd3 partition 1 0 bad",
        "Filename Type Size Used Priority\nrelative partition 1 0 1",
        "Filename Type Size Used Priority\n/dev/nbd\u{1}3 partition 1 0 1",
        "Filename Type Size Used Priority\n/dev/nbd3 partition 1 0 1\n/dev/nbd3 partition 1 0 1",
        "Filename Type Size Used Priority\n/dev/nbd3 partition 1 0 1\n/dev/nbd3\\040(deleted) partition 1 0 1",
    ];
    for snapshot in malformed {
        assert!(
            parse_strict_proc_swaps(snapshot).is_err(),
            "malformed snapshot unexpectedly accepted: {snapshot:?}"
        );
    }
}

#[test]
fn daemon_nbd_io_error_burst_demotes_once_with_exact_fake_swap() {
    struct FloorProvider;

    impl VramProvider for FloorProvider {
        type Mem<'a>
            = TestMemory
        where
            Self: 'a;

        fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, ramshared_vram::VramError> {
            assert_eq!(
                bytes, CANARY_BYTES,
                "free-floor refusals must not allocate a sparse chunk"
            );
            Ok(TestMemory::new(bytes))
        }

        fn mem_info(&self) -> Result<(u64, u64), ramshared_vram::VramError> {
            Ok((0, 8 * 1024 * 1024 * 1024))
        }
    }

    struct ErrorBurstStarter {
        replies: Option<std::sync::mpsc::Receiver<Reply>>,
        swapoff_calls: usize,
        statuses: Vec<ramshared_wsl2d::DemoteStatusFile>,
    }

    impl NbdRuntimeStarter for ErrorBurstStarter {
        fn try_claim_shutdown(&mut self, _: &str) -> Option<ShutdownPermit> {
            Some(ShutdownPermit::injected(()))
        }

        fn lock_memory(
            &mut self,
            _force: bool,
            _lock_future: bool,
        ) -> Result<(), Box<dyn std::error::Error>> {
            Ok(())
        }

        fn start_acceptor(
            &mut self,
            _listener: UnixListener,
            _exports: std::sync::Arc<Vec<ramshared_block::handshake::Export>>,
            _tx_flags: u16,
            jobs_tx: std::sync::mpsc::SyncSender<WMsg>,
        ) -> Result<(), Box<dyn std::error::Error>> {
            let (reply_tx, reply_rx) = std::sync::mpsc::channel();
            jobs_tx.send(WMsg::Opened)?;
            for handle in 0..3 {
                jobs_tx.send(WMsg::Job(ramshared_wsl2d::conn::Job {
                    export: 0,
                    req: ramshared_block::Request {
                        flags: 0,
                        cmd: Command::Write,
                        handle,
                        offset: handle * u64::from(BLOCK_SIZE),
                        len: BLOCK_SIZE,
                    },
                    payload: vec![0xA5; BLOCK_SIZE as usize],
                    credit: fixture_credit(),
                    reply: reply_tx.clone(),
                }))?;
            }
            jobs_tx.send(WMsg::Closed)?;
            jobs_tx.send(WMsg::Shutdown)?;
            self.replies = Some(reply_rx);
            Ok(())
        }

        fn nbd_used_kb(&mut self, _nbd_dev: &str) -> u64 {
            0
        }

        fn nbd_swap_is_explicitly_absent(&mut self, _nbd_dev: &str) -> bool {
            true
        }

        fn publish_demote(&mut self, status: &ramshared_wsl2d::DemoteStatusFile) {
            self.statuses.push(status.clone());
        }

        fn elapsed_us(&mut self, _started: Instant) -> u64 {
            10
        }

        fn spawn_swapoff(&mut self, _nbd_dev: &str) -> std::sync::mpsc::Receiver<SwapoffOutcome> {
            self.swapoff_calls += 1;
            let (tx, rx) = std::sync::mpsc::channel();
            tx.send(SwapoffOutcome::Drained)
                .expect("complete the fake swapoff");
            rx
        }

        fn spawn_recovery_activation(
            &mut self,
            _nbd_dev: &str,
            _priority: i16,
        ) -> Result<std::sync::mpsc::Receiver<bool>, Box<dyn std::error::Error>> {
            panic!("an error-burst demotion must not reactivate the fake swap")
        }
    }

    let path = std::env::temp_dir().join(format!(
        "ramshared-daemon-io-error-burst-{}.sock",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let mut starter = ErrorBurstStarter {
        replies: None,
        swapoff_calls: 0,
        statuses: Vec::new(),
    };
    run_nbd_with_startup(
        FloorProvider,
        None,
        4 * u64::from(BLOCK_SIZE),
        path.to_string_lossy().into_owned(),
        false,
        "/dev/ramshared-test-nbd".into(),
        false,
        serve_pool::DEFAULT_SERVE_THREADS,
        &mut starter,
    )
    .expect("three storage I/O errors safely demote the fake NBD swap");

    let replies = starter
        .replies
        .take()
        .expect("reply receiver")
        .try_iter()
        .collect::<Vec<_>>();
    assert_eq!(replies.len(), 3);
    assert!(replies.iter().all(|reply| {
        u32::from_be_bytes([
            reply.reply[4],
            reply.reply[5],
            reply.reply[6],
            reply.reply[7],
        ]) == ramshared_block::protocol::NBD_EIO
    }));
    assert_eq!(starter.swapoff_calls, 1);
    assert_eq!(
        starter.statuses,
        vec![
            ramshared_wsl2d::DemoteStatusFile::new(0, None, false),
            ramshared_wsl2d::DemoteStatusFile::new(0, Some("IoErrorBurst".into()), true),
            ramshared_wsl2d::DemoteStatusFile::new(1, Some("IoErrorBurst".into()), false),
        ]
    );
    assert!(!path.exists(), "the fixture removes its temporary socket");
}
