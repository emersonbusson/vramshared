// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main_tests/nbd_residency_and_version.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;

#[test]
fn daemon_nbd_shutdown_keeps_serving_one_pending_drain() {
    struct Memory {
        inner: TestMemory,
        held: Arc<std::sync::atomic::AtomicBool>,
        drops: Arc<std::sync::Mutex<Vec<bool>>>,
    }
    impl VramMemory for Memory {
        fn len(&self) -> usize {
            self.inner.len()
        }
        fn zero(&mut self) -> Result<(), ramshared_vram::VramError> {
            self.inner.zero()
        }
        fn read_at(&self, off: u64, dst: &mut [u8]) -> Result<(), ramshared_vram::VramError> {
            self.inner.read_at(off, dst)
        }
        fn write_at(&mut self, off: u64, src: &[u8]) -> Result<(), ramshared_vram::VramError> {
            self.inner.write_at(off, src)
        }
    }
    impl Drop for Memory {
        fn drop(&mut self) {
            self.drops
                .lock()
                .unwrap()
                .push(self.held.load(Ordering::SeqCst));
        }
    }
    struct Guard(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.store(false, Ordering::SeqCst);
        }
    }
    struct Provider {
        held: Arc<std::sync::atomic::AtomicBool>,
        drops: Arc<std::sync::Mutex<Vec<bool>>>,
    }
    impl VramProvider for Provider {
        type Mem<'a> = Memory;
        fn alloc(&self, bytes: usize) -> Result<Memory, ramshared_vram::VramError> {
            Ok(Memory {
                inner: TestMemory::new(bytes),
                held: Arc::clone(&self.held),
                drops: Arc::clone(&self.drops),
            })
        }
        fn mem_info(&self) -> Result<(u64, u64), ramshared_vram::VramError> {
            Ok((8 * GIB, 8 * GIB))
        }
    }
    struct Starter {
        jobs: std::sync::mpsc::SyncSender<std::sync::mpsc::SyncSender<WMsg>>,
        started: std::sync::mpsc::SyncSender<()>,
        drain: Option<std::sync::mpsc::Receiver<SwapoffOutcome>>,
        calls: Arc<AtomicUsize>,
        held: Arc<std::sync::atomic::AtomicBool>,
        drained: bool,
    }
    impl NbdRuntimeStarter for Starter {
        fn try_claim_shutdown(&mut self, _: &str) -> Option<ShutdownPermit> {
            assert!(!self.held.swap(true, Ordering::SeqCst));
            Some(ShutdownPermit::injected(Guard(Arc::clone(&self.held))))
        }

        fn lock_memory(&mut self, _: bool, _: bool) -> Result<(), Box<dyn std::error::Error>> {
            Ok(())
        }
        fn start_acceptor(
            &mut self,
            _: UnixListener,
            _: Arc<Vec<ramshared_block::handshake::Export>>,
            _: u16,
            tx: std::sync::mpsc::SyncSender<WMsg>,
        ) -> Result<(), Box<dyn std::error::Error>> {
            self.jobs.send(tx)?;
            Ok(())
        }
        fn nbd_used_kb(&mut self, _: &str) -> u64 {
            if self.drained { 0 } else { 32 }
        }
        fn publish_demote(&mut self, status: &DemoteStatusFile) {
            self.drained = status.total > 0 && !status.in_progress;
        }
        fn elapsed_us(&mut self, _: Instant) -> u64 {
            0
        }
        fn spawn_swapoff(&mut self, _: &str) -> std::sync::mpsc::Receiver<SwapoffOutcome> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.send(()).unwrap();
            self.drain.take().expect("only one drain may be in flight")
        }
        fn spawn_recovery_activation(
            &mut self,
            _: &str,
            _: i16,
        ) -> Result<std::sync::mpsc::Receiver<bool>, Box<dyn std::error::Error>> {
            panic!("shutdown must never re-arm swap")
        }
        fn teardown_retry_delay(&mut self) -> Duration {
            Duration::ZERO
        }
    }
    let path = std::env::temp_dir().join(format!("nsd-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let (jobs_tx, jobs_rx) = std::sync::mpsc::sync_channel(1);
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
    let (drain_tx, drain_rx) = std::sync::mpsc::sync_channel(1);
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
    let calls = Arc::new(AtomicUsize::new(0));
    let held = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let drops = Arc::new(std::sync::Mutex::new(Vec::new()));
    let thread_held = Arc::clone(&held);
    let thread_drops = Arc::clone(&drops);
    let thread_calls = calls.clone();
    let thread_path = path.clone();
    let thread = std::thread::spawn(move || {
        let mut starter = Starter {
            jobs: jobs_tx,
            started: started_tx,
            drain: Some(drain_rx),
            calls: thread_calls,
            held: Arc::clone(&thread_held),
            drained: false,
        };
        let result = run_nbd_with_startup(
            Provider {
                held: thread_held,
                drops: thread_drops,
            },
            None,
            4096,
            thread_path.to_string_lossy().into_owned(),
            false,
            "/dev/ramshared-test-nbd".into(),
            false,
            serve_pool::DEFAULT_SERVE_THREADS,
            &mut starter,
        )
        .map_err(|error| error.to_string());
        done_tx.send(result).unwrap();
    });
    let jobs = jobs_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let (write_tx, write_rx) = std::sync::mpsc::channel();
    jobs.send(WMsg::Job(ramshared_wsl2d::conn::Job {
        export: 0,
        req: ramshared_block::Request {
            flags: 0,
            cmd: Command::Write,
            handle: 700,
            offset: 0,
            len: BLOCK_SIZE,
        },
        payload: vec![0x5a; BLOCK_SIZE as usize],
        credit: fixture_credit(),
        reply: write_tx,
    }))
    .unwrap();
    let write = write_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("fixture commits real backend memory");
    assert_eq!(&write.reply[4..8], &[0, 0, 0, 0]);
    jobs.send(WMsg::Shutdown).unwrap();
    started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    jobs.send(WMsg::Job(ramshared_wsl2d::conn::Job {
        export: 0,
        req: ramshared_block::Request {
            flags: 0,
            cmd: Command::Flush,
            handle: 701,
            offset: 0,
            len: 0,
        },
        payload: Vec::new(),
        credit: fixture_credit(),
        reply: reply_tx,
    }))
    .unwrap();
    let reply = reply_rx.recv_timeout(Duration::from_secs(10));
    let retained = path.exists() && done_rx.try_recv().is_err();
    drain_tx.send(SwapoffOutcome::Drained).unwrap();
    assert_eq!(
        done_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        Ok(())
    );
    thread.join().unwrap();
    assert!(
        retained,
        "pending release must retain the backend and socket"
    );
    let reply = reply.expect("pending shutdown drain must keep dispatching NBD I/O");
    assert_eq!(
        &reply.reply[4..8],
        &[0, 0, 0, 0],
        "pending shutdown drain must serve successfully"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "pending drain must never restart"
    );
    assert!(!path.exists());
    assert!(!held.load(Ordering::SeqCst));
    let observed = drops.lock().unwrap();
    assert!(
        observed.len() >= 2,
        "the fixture must destroy both backend and canary memory"
    );
    assert!(
        observed.iter().all(|held| *held),
        "every allocation drops under the exclusive permit"
    );
}

#[test]
fn daemon_nbd_residency_demote_uses_injected_clock_and_swapoff() {
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
            // Three cadence samples below the configured floor produce a
            // FreeFloor demotion, which remains a valid sparse swapoff
            // reason. Latency alone is intentionally non-destructive.
            Ok((1, 8 * 1024 * 1024 * 1024))
        }
    }

    struct DemoteStarter {
        latencies: std::collections::VecDeque<u64>,
        swapoff_calls: usize,
        replies: Option<std::sync::mpsc::Receiver<Reply>>,
        producer: Option<std::thread::JoinHandle<()>>,
        status_updates: Vec<ramshared_wsl2d::DemoteStatusFile>,
    }

    impl NbdRuntimeStarter for DemoteStarter {
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
            self.replies = Some(reply_rx);
            self.producer = Some(std::thread::spawn(move || {
                jobs_tx.send(WMsg::Opened).expect("open fake NBD client");
                for handle in 0..192 {
                    jobs_tx
                        .send(WMsg::Job(ramshared_wsl2d::conn::Job {
                            export: 0,
                            req: ramshared_block::Request {
                                flags: 0,
                                cmd: Command::Write,
                                handle,
                                offset: 0,
                                len: 512,
                            },
                            payload: vec![0x3C; 512],
                            credit: fixture_credit(),
                            reply: reply_tx.clone(),
                        }))
                        .expect("queue fake NBD write");
                }
                jobs_tx.send(WMsg::Closed).expect("close fake NBD client");
                jobs_tx
                    .send(WMsg::Shutdown)
                    .expect("request fake NBD shutdown");
            }));
            Ok(())
        }

        fn nbd_used_kb(&mut self, _nbd_dev: &str) -> u64 {
            0
        }

        fn publish_demote(&mut self, status: &ramshared_wsl2d::DemoteStatusFile) {
            self.status_updates.push(status.clone());
        }

        fn elapsed_us(&mut self, _started: Instant) -> u64 {
            self.latencies
                .pop_front()
                .expect("one injected latency per worker job")
        }

        fn spawn_swapoff(&mut self, _nbd_dev: &str) -> std::sync::mpsc::Receiver<SwapoffOutcome> {
            self.swapoff_calls += 1;
            let (tx, rx) = std::sync::mpsc::channel();
            tx.send(SwapoffOutcome::Drained)
                .expect("in-memory swapoff completion");
            rx
        }

        fn spawn_recovery_activation(
            &mut self,
            _nbd_dev: &str,
            _priority: i16,
        ) -> Result<std::sync::mpsc::Receiver<bool>, Box<dyn std::error::Error>> {
            panic!("a successful terminal DEMOTE must not attempt recovery")
        }
    }

    let path = std::env::temp_dir().join(format!(
        "ramshared-daemon-fake-demote-{}.sock",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let mut starter = DemoteStarter {
        latencies: std::iter::repeat_n(10, 192).collect(),
        swapoff_calls: 0,
        replies: None,
        producer: None,
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
    .expect("injected terminal DEMOTE must tear down the fake sparse backend");
    starter
        .producer
        .take()
        .expect("injected producer exists")
        .join()
        .expect("injected producer completes");

    assert_eq!(
        starter.swapoff_calls, 1,
        "one DEMOTE starts one bounded swapoff"
    );
    assert_eq!(
        starter.status_updates,
        vec![
            ramshared_wsl2d::DemoteStatusFile::new(0, None, false),
            ramshared_wsl2d::DemoteStatusFile::new(0, Some("FreeFloor".into()), true),
            ramshared_wsl2d::DemoteStatusFile::new(1, Some("FreeFloor".into()), false),
        ]
    );
    let replies = starter
        .replies
        .take()
        .expect("injected reply receiver")
        .try_iter()
        .count();
    assert_eq!(
        replies, 192,
        "every queued write gets a reply before shutdown"
    );
    assert!(
        !path.exists(),
        "fake DEMOTE cleanup removes the temporary socket"
    );
}

/// SPEC §9.2: a drain that outlives the detection window is reported stuck
/// with its age, without aborting the `swapoff` and without clearing the
/// pending flight. The window is `Duration::ZERO` here so the report is due
/// on the first poll — waiting 30 s is not what this test is about.
#[test]
fn daemon_nbd_stuck_swapoff_publishes_pending_age_once() {
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
            // Below the sealed floor on every cadence sample, so the
            // FreeFloor demotion is the one that opens the stuck flight.
            Ok((1, 8 * 1024 * 1024 * 1024))
        }
    }

    struct StuckStarter {
        latencies: std::collections::VecDeque<u64>,
        swapoff_calls: usize,
        pending_swapoff: Option<std::sync::mpsc::Sender<SwapoffOutcome>>,
        replies: Option<std::sync::mpsc::Receiver<Reply>>,
        producer: Option<std::thread::JoinHandle<()>>,
        status_updates: Vec<ramshared_wsl2d::DemoteStatusFile>,
    }

    impl NbdRuntimeStarter for StuckStarter {
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
            self.replies = Some(reply_rx);
            self.producer = Some(std::thread::spawn(move || {
                jobs_tx.send(WMsg::Opened).expect("open fake NBD client");
                // Enough writes for the §9.4 cadence to open the flight
                // well before the tail, so later ticks observe it.
                for handle in 0..256 {
                    jobs_tx
                        .send(WMsg::Job(ramshared_wsl2d::conn::Job {
                            export: 0,
                            req: ramshared_block::Request {
                                flags: 0,
                                cmd: Command::Write,
                                handle,
                                offset: 0,
                                len: 512,
                            },
                            payload: vec![0x3C; 512],
                            credit: fixture_credit(),
                            reply: reply_tx.clone(),
                        }))
                        .expect("queue fake NBD write");
                }
                jobs_tx.send(WMsg::Closed).expect("close fake NBD client");
                jobs_tx
                    .send(WMsg::Shutdown)
                    .expect("request fake NBD shutdown");
            }));
            Ok(())
        }

        fn nbd_used_kb(&mut self, _nbd_dev: &str) -> u64 {
            // The reclaim tick runs after the stuck report in the same
            // iteration, so completing here means the report already
            // happened and the next tick can observe the terminal outcome.
            if let Some(tx) = self.pending_swapoff.take() {
                let _ = tx.send(SwapoffOutcome::Drained);
            }
            0
        }

        fn publish_demote(&mut self, status: &ramshared_wsl2d::DemoteStatusFile) {
            self.status_updates.push(status.clone());
        }

        fn demote_stuck_window(&self) -> Duration {
            Duration::ZERO
        }

        fn elapsed_us(&mut self, _started: Instant) -> u64 {
            self.latencies
                .pop_front()
                .expect("one injected latency per worker job")
        }

        fn spawn_swapoff(&mut self, _nbd_dev: &str) -> std::sync::mpsc::Receiver<SwapoffOutcome> {
            self.swapoff_calls += 1;
            let (tx, rx) = std::sync::mpsc::channel();
            // Park the sender: the receiver stays `Empty` until the
            // reclaim tick completes it, which is a genuinely in-flight
            // drain rather than a rejected or vanished one.
            self.pending_swapoff = Some(tx);
            rx
        }

        fn spawn_recovery_activation(
            &mut self,
            _nbd_dev: &str,
            _priority: i16,
        ) -> Result<std::sync::mpsc::Receiver<bool>, Box<dyn std::error::Error>> {
            panic!("a successful terminal DEMOTE must not attempt recovery")
        }
    }

    let path = std::env::temp_dir().join(format!(
        "ramshared-daemon-fake-stuck-demote-{}.sock",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let mut starter = StuckStarter {
        latencies: std::iter::repeat_n(10, 256).collect(),
        swapoff_calls: 0,
        pending_swapoff: None,
        replies: None,
        producer: None,
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
    .expect("the stuck drain is reported and then completed before teardown");
    starter
        .producer
        .take()
        .expect("injected producer exists")
        .join()
        .expect("injected producer completes");

    assert_eq!(
        starter.swapoff_calls, 1,
        "one DEMOTE starts one swapoff and the pending flight holds the spawn gate"
    );
    assert_eq!(
        starter.status_updates.len(),
        4,
        "initial, spawn, stuck report, terminal — got {:?}",
        starter.status_updates
    );
    assert_eq!(
        starter.status_updates[0],
        ramshared_wsl2d::DemoteStatusFile::new(0, None, false)
    );
    assert_eq!(
        starter.status_updates[1],
        ramshared_wsl2d::DemoteStatusFile::new(0, Some("FreeFloor".into()), true)
    );
    let stuck = &starter.status_updates[2];
    assert_eq!(stuck.total, 0, "a stuck drain has not drained anything yet");
    assert_eq!(stuck.last_reason, Some("FreeFloor".into()));
    assert!(stuck.in_progress);
    assert!(
        stuck.pending_age_secs.is_some(),
        "the stuck report carries the age of the pending drain (SPEC §9.2): {stuck:?}"
    );
    assert_eq!(
        starter.status_updates[3],
        ramshared_wsl2d::DemoteStatusFile::new(1, Some("FreeFloor".into()), false),
        "the terminal record clears the pending age"
    );
    assert!(
        !path.exists(),
        "fake stuck-DEMOTE cleanup removes the temporary socket"
    );
}

#[test]
fn daemon_broker_vram_and_ram_lifecycles_use_injected_runtime() {
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

    struct NoopAcceptors;

    impl BrokerAcceptorStarter for NoopAcceptors {
        fn start(
            &mut self,
            _unix: UnixListener,
            _tcp: Option<std::net::TcpListener>,
            _exports: std::sync::Arc<Vec<ramshared_block::handshake::Export>>,
            _tx_flags: u16,
            _jobs_tx: std::sync::mpsc::SyncSender<WMsg>,
        ) -> Result<(), Box<dyn std::error::Error>> {
            Ok(())
        }
    }

    fn runtime(path: &Path, shutdown: std::sync::Arc<AtomicBool>) -> BrokerRuntime {
        broker_setup_with_acceptors(
            1,
            4096,
            path.to_str().expect("temporary path is UTF-8"),
            None,
            None,
            "127.0.0.1:0".parse().expect("loopback arbiter listener"),
            None,
            shutdown,
            Duration::from_millis(1),
            &mut NoopAcceptors,
        )
        .expect("temporary injected broker runtime")
    }

    let vram_path = std::env::temp_dir().join(format!(
        "ramshared-daemon-vram-lifecycle-{}.sock",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&vram_path);
    let vram_shutdown = std::sync::Arc::new(AtomicBool::new(false));
    let vram_runtime = runtime(&vram_path, std::sync::Arc::clone(&vram_shutdown));
    let _ = vram_runtime.shutdown.request();
    run_broker_with_setup(
        TestProvider,
        4096,
        1,
        vram_path.to_string_lossy().into_owned(),
        false,
        |_force, _future| Ok(()),
        || Ok(vram_runtime),
        Duration::from_millis(1),
    )
    .expect("fake VRAM broker lifecycle");
    assert!(
        !vram_path.exists(),
        "VRAM lifecycle cleans its owned socket"
    );

    let ram_path = std::env::temp_dir().join(format!(
        "ramshared-daemon-ram-lifecycle-{}.sock",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&ram_path);
    let ram_shutdown = std::sync::Arc::new(AtomicBool::new(false));
    let ram_runtime = runtime(&ram_path, std::sync::Arc::clone(&ram_shutdown));
    let _ = ram_runtime.shutdown.request();
    run_broker_ram_with_setup(
        4096,
        1,
        ram_path.to_string_lossy().into_owned(),
        || Ok(ram_runtime),
        Duration::from_millis(1),
    )
    .expect("heap RAM broker lifecycle");
    assert!(!ram_path.exists(), "RAM lifecycle cleans its owned socket");
}

#[test]
// TestName: daemon_broker_panic_propagates_after_bounded_worker_cleanup
fn daemon_broker_panic_propagates_after_bounded_worker_cleanup() {
    let (jobs_tx, jobs_rx) = std::sync::mpsc::sync_channel(CHAN_CAP);
    let shutdown = std::sync::Arc::new(AtomicBool::new(false));
    let stop = BrokerShutdown::new(std::sync::Arc::clone(&shutdown), jobs_tx);
    let (demote_tx, _demote_rx) = std::sync::mpsc::channel();
    let broker = std::thread::spawn(|| panic!("injected broker panic"));
    let runtime = BrokerRuntime {
        shutdown_bridge: None,
        worker: BrokerWorkerRuntime {
            geom: vec![(0, 4096)],
            jobs_rx,
            demote_tx,
            shutdown,
            shutdown_wake_pending: std::sync::Arc::clone(&stop.wake_pending),
            shutdown_wake_tx: stop.wake_tx.clone(),
            slice_io: std::sync::Arc::new(vec![SliceIoCounters::default()]),
            vram: std::sync::Arc::new(VramGauge::default()),
        },
        broker,
        shutdown: stop.clone(),
        socket: None,
    };
    let socket = std::env::temp_dir().join(format!(
        "ramshared-broker-panic-{}.sock",
        std::process::id()
    ));
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
    let controller = std::thread::spawn(move || {
        let result = run_broker_ram_with_setup(
            4096,
            1,
            socket.to_string_lossy().into_owned(),
            || Ok(runtime),
            Duration::from_secs(30),
        )
        .map_err(|error| error.to_string());
        let _ = result_tx.send(result);
    });

    let (needed_rescue, result) = match result_rx.recv_timeout(Duration::from_secs(1)) {
        Ok(result) => (false, result),
        Err(_) => {
            let _ = stop.request();
            (
                true,
                result_rx
                    .recv_timeout(Duration::from_secs(1))
                    .expect("fixture rescue must bound cleanup of the broken implementation"),
            )
        }
    };
    controller
        .join()
        .expect("broker lifecycle controller joins");

    assert!(
        !needed_rescue,
        "broker panic did not independently stop its worker"
    );
    assert!(
        result
            .expect_err("broker panic must not become clean daemon success")
            .contains("broker"),
        "broker panic error lost its typed context"
    );
}
