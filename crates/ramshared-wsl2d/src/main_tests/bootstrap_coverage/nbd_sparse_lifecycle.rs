// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main_tests/bootstrap_coverage/nbd_sparse_lifecycle.rs
use super::*;

#[test]
fn nbd_sparse_lifecycle_covers_wddm_global_floor_gpu_lost_and_recovery_failures() {
    use std::sync::atomic::{AtomicU64, AtomicUsize};

    struct ConfigurableProvider {
        fail_mem_info: Arc<AtomicBool>,
        free_bytes: Arc<AtomicU64>,
        alloc_count: Arc<AtomicUsize>,
    }
    impl VramProvider for ConfigurableProvider {
        type Mem<'a>
            = TestMemory
        where
            Self: 'a;
        fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, ramshared_vram::VramError> {
            self.alloc_count.fetch_add(1, Ordering::SeqCst);
            Ok(TestMemory::new(bytes))
        }
        fn mem_info(&self) -> Result<(u64, u64), ramshared_vram::VramError> {
            if self.fail_mem_info.load(Ordering::SeqCst) {
                Err(ramshared_vram::VramError::Provider(
                    "injected gpu loss".into(),
                ))
            } else {
                Ok((
                    self.free_bytes.load(Ordering::SeqCst),
                    8 * 1024 * 1024 * 1024,
                ))
            }
        }
    }

    struct SeqBudget {
        mode: Arc<AtomicUsize>, // 0 = unhealthy, 1 = healthy, 2 = err
    }
    impl NbdBudgetProvider for SeqBudget {
        fn snapshot(&self) -> Result<NbdBudgetSnapshot, String> {
            match self.mode.load(Ordering::SeqCst) {
                0 => Ok(NbdBudgetSnapshot {
                    budget: 128 * 1024 * 1024,
                    current_usage: 128 * 1024 * 1024,
                    sampled_at: Instant::now(),
                }),
                1 => Ok(NbdBudgetSnapshot {
                    budget: 8 * 1024 * 1024 * 1024,
                    current_usage: 0,
                    sampled_at: Instant::now(),
                }),
                _ => Err("wddm snapshot error".into()),
            }
        }
    }

    struct BranchStarter {
        budget_mode: Arc<AtomicUsize>,
        global_free: Arc<AtomicUsize>, // 0 = None, 1 = 16 GiB, 2 = 0 bytes
        free_bytes: Arc<AtomicU64>,
        alloc_count: Arc<AtomicUsize>,
        recovery_step: Arc<AtomicUsize>,
        swapoff_count: Arc<AtomicUsize>,
        scenario: u8, // 0 = Drained/recovery, 1 = Rejected/floor, 2 = Disconnected/GpuLost, 3 = Residency/reclaim/WddmBudget
        swapoff_mode: u8, // 0 = Drained, 1 = Rejected, 2 = Disconnected
        fail_mem_info: Arc<AtomicBool>,
        spike_elapsed: bool,
        elapsed_calls: Arc<AtomicUsize>,
        fail_notify: bool,
        first_claim_none: bool,
        claim_attempts: Arc<AtomicUsize>,
        producer: Option<std::thread::JoinHandle<()>>,
        demote_reasons: Vec<Option<String>>,
    }

    impl NbdRuntimeStarter for BranchStarter {
        fn take_notify_socket(
            &mut self,
        ) -> Result<
            Option<ramshared_wsl2d::systemd_notify::NotifySocket>,
            ramshared_wsl2d::systemd_notify::NotifyError,
        > {
            if self.fail_notify {
                return Err(ramshared_wsl2d::systemd_notify::NotifyError::NotASocket);
            }
            Ok(None)
        }
        fn try_claim_shutdown(&mut self, _: &str) -> Option<ShutdownPermit> {
            let n = self.claim_attempts.fetch_add(1, Ordering::SeqCst);
            if self.first_claim_none && n == 0 {
                None
            } else {
                Some(ShutdownPermit::injected(()))
            }
        }
        fn teardown_retry_delay(&mut self) -> Duration {
            Duration::ZERO
        }
        fn startup_budget(
            &mut self,
            requested: bool,
        ) -> Result<Option<Box<dyn NbdBudgetProvider>>, Box<dyn std::error::Error>> {
            if !requested {
                return Ok(None);
            }
            Ok(Some(Box::new(SeqBudget {
                mode: Arc::clone(&self.budget_mode),
            })))
        }
        fn global_free_bytes(&mut self, _: Duration) -> Option<u64> {
            match self.global_free.load(Ordering::SeqCst) {
                0 => None,
                1 => Some(16 * 1024 * 1024 * 1024),
                _ => Some(0),
            }
        }
        fn lock_memory(&mut self, _: bool, _: bool) -> Result<(), Box<dyn std::error::Error>> {
            Ok(())
        }
        fn start_acceptor(
            &mut self,
            _listener: UnixListener,
            _exports: Arc<Vec<ramshared_block::handshake::Export>>,
            _tx_flags: u16,
            jobs_tx: std::sync::mpsc::SyncSender<WMsg>,
        ) -> Result<(), Box<dyn std::error::Error>> {
            let (reply_tx, reply_rx) = std::sync::mpsc::channel();
            let mode = self.scenario;
            let budget = Arc::clone(&self.budget_mode);
            let global = Arc::clone(&self.global_free);
            let free_bytes = Arc::clone(&self.free_bytes);
            let _alloc_count = Arc::clone(&self.alloc_count);
            let recovery = Arc::clone(&self.recovery_step);
            let swapoffs = Arc::clone(&self.swapoff_count);
            let fail_mem = Arc::clone(&self.fail_mem_info);
            self.producer = Some(std::thread::spawn(move || {
                let send_read = |handle: u64| {
                    let _ = jobs_tx.send(WMsg::Job(ramshared_wsl2d::conn::Job {
                        export: 0,
                        req: ramshared_block::Request {
                            flags: 0,
                            cmd: Command::Read,
                            handle,
                            offset: 0,
                            len: 512,
                        },
                        payload: Vec::new(),
                        credit: fixture_credit(),
                        reply: reply_tx.clone(),
                    }));
                };
                let send_write = |handle: u64| {
                    let _ = jobs_tx.send(WMsg::Job(ramshared_wsl2d::conn::Job {
                        export: 0,
                        req: ramshared_block::Request {
                            flags: 0,
                            cmd: Command::Write,
                            handle,
                            offset: 0,
                            len: 4096,
                        },
                        payload: vec![0x7E; 4096],
                        credit: fixture_credit(),
                        reply: reply_tx.clone(),
                    }));
                };
                let drain_reply = || {
                    let _ = reply_rx.recv_timeout(Duration::from_secs(5));
                };
                let _ = jobs_tx.send(WMsg::Opened);
                if mode == 0 {
                    // Wait deterministically for initial WddmBudgetPoll swapoff before flipping healthy.
                    for h in 1..=200 {
                        if swapoffs.load(Ordering::SeqCst) >= 1 {
                            break;
                        }
                        send_read(h);
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    budget.store(1, Ordering::SeqCst);
                    global.store(1, Ordering::SeqCst);
                    for h in 201..=400 {
                        if recovery.load(Ordering::SeqCst) >= 1 {
                            break;
                        }
                        send_read(h);
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    // Transition to Err budget to clear failed_epoch, then healthy again to trigger Err spawn.
                    budget.store(2, Ordering::SeqCst);
                    send_read(401);
                    std::thread::sleep(Duration::from_millis(20));
                    budget.store(1, Ordering::SeqCst);
                    for h in 402..=600 {
                        if recovery.load(Ordering::SeqCst) >= 2 {
                            break;
                        }
                        send_read(h);
                        std::thread::sleep(Duration::from_millis(5));
                    }
                } else if mode == 1 {
                    // Write 1 chunk so committed > 0 -> GlobalGpuFreeFloor -> Rejected.
                    send_write(20);
                    for h in 21..=220 {
                        if swapoffs.load(Ordering::SeqCst) >= 1 {
                            send_read(h);
                            std::thread::sleep(Duration::from_millis(10));
                            break;
                        }
                        send_read(h);
                        std::thread::sleep(Duration::from_millis(5));
                    }
                } else if mode == 2 {
                    // Idle tick with mem_info failing => GpuLost -> Disconnected swapoff.
                    fail_mem.store(true, Ordering::SeqCst);
                    for _ in 0..200 {
                        if swapoffs.load(Ordering::SeqCst) >= 1 {
                            std::thread::sleep(Duration::from_millis(20));
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(15));
                    }
                } else {
                    // Scenario 3: extra Open/Close (live.on_close() == false), reply-synchronized
                    // sparse reclaim, WddmBudget inline swapoff + Disconnected bottom poll, and
                    // residency_check (Some, Some) / (None, None) combos.
                    let _ = jobs_tx.send(WMsg::Opened);
                    let _ = jobs_tx.send(WMsg::Closed);

                    // 1. Commit chunk 0 cleanly.
                    send_write(500);
                    drain_reply();

                    // 2. Drop free_bytes below floor; send Read 501 + Read 502 so try_reclaim on
                    // 501's loop tail is guaranteed finished before 502 completes.
                    free_bytes.store(512 * 1024 * 1024, Ordering::SeqCst);
                    send_read(501);
                    drain_reply();
                    send_read(502);
                    drain_reply();

                    // 3. Restore free_bytes, set budget unhealthy, write freed chunk 0 ->
                    // sparse.budget_refuses increments -> WddmBudget swapoff -> Disconnected poll.
                    free_bytes.store(8 * 1024 * 1024 * 1024, Ordering::SeqCst);
                    budget.store(0, Ordering::SeqCst);
                    send_write(503);
                    drain_reply();
                    send_read(504);
                    drain_reply();

                    // 4. Restore healthy budget and drive cadence ticks for (Some, Some) and (None, None).
                    budget.store(1, Ordering::SeqCst);
                    for h in 600..670 {
                        send_read(h);
                        drain_reply();
                    }
                    fail_mem.store(true, Ordering::SeqCst);
                    global.store(0, Ordering::SeqCst);
                    for h in 670..740 {
                        send_read(h);
                        drain_reply();
                    }
                }
                let _ = jobs_tx.send(WMsg::Closed);
                let _ = jobs_tx.send(WMsg::Shutdown);
            }));
            Ok(())
        }
        fn start_shutdown_bridge(
            &mut self,
            _: std::sync::mpsc::SyncSender<WMsg>,
        ) -> Result<Option<NbdShutdownBridge>, Box<dyn std::error::Error>> {
            Ok(None)
        }
        fn nbd_used_kb(&mut self, _: &str) -> u64 {
            0
        }
        fn nbd_swap_is_explicitly_absent(&mut self, _: &str) -> bool {
            true
        }
        fn publish_demote(&mut self, status: &ramshared_wsl2d::DemoteStatusFile) {
            self.demote_reasons.push(status.last_reason.clone());
        }
        fn elapsed_us(&mut self, _: Instant) -> u64 {
            let n = self.elapsed_calls.fetch_add(1, Ordering::SeqCst);
            if self.spike_elapsed && n >= 16 {
                50_000
            } else {
                10
            }
        }
        fn spawn_swapoff(&mut self, _: &str) -> std::sync::mpsc::Receiver<SwapoffOutcome> {
            self.swapoff_count.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) = std::sync::mpsc::channel();
            match self.swapoff_mode {
                0 => {
                    let _ = tx.send(SwapoffOutcome::Drained);
                }
                1 => {
                    let _ = tx.send(SwapoffOutcome::Rejected {
                        exit_code: Some(16),
                    });
                }
                _ => {
                    drop(tx);
                }
            }
            rx
        }
        fn spawn_recovery_activation(
            &mut self,
            _: &str,
            _: i16,
        ) -> Result<std::sync::mpsc::Receiver<bool>, Box<dyn std::error::Error>> {
            let step = self.recovery_step.fetch_add(1, Ordering::SeqCst);
            if step == 0 {
                let (tx, rx) = std::sync::mpsc::channel();
                let _ = tx.send(false);
                Ok(rx)
            } else {
                Err("recovery spawn refused".into())
            }
        }
    }

    let root = temp_test_dir("nbd-branches");

    // 1. WddmBudgetPoll -> Drained -> Recovery Failed -> Recovery Err.
    let sock1 = root.join("s1.sock");
    let mut s1 = BranchStarter {
        budget_mode: Arc::new(AtomicUsize::new(0)),
        global_free: Arc::new(AtomicUsize::new(1)),
        free_bytes: Arc::new(AtomicU64::new(8 * 1024 * 1024 * 1024)),
        alloc_count: Arc::new(AtomicUsize::new(0)),
        recovery_step: Arc::new(AtomicUsize::new(0)),
        swapoff_count: Arc::new(AtomicUsize::new(0)),
        scenario: 0,
        swapoff_mode: 0,
        fail_mem_info: Arc::new(AtomicBool::new(false)),
        spike_elapsed: false,
        elapsed_calls: Arc::new(AtomicUsize::new(0)),
        fail_notify: false,
        first_claim_none: false,
        claim_attempts: Arc::new(AtomicUsize::new(0)),
        producer: None,
        demote_reasons: Vec::new(),
    };
    run_nbd_with_startup(
        ConfigurableProvider {
            fail_mem_info: Arc::clone(&s1.fail_mem_info),
            free_bytes: Arc::clone(&s1.free_bytes),
            alloc_count: Arc::clone(&s1.alloc_count),
        },
        None,
        65536,
        sock1.to_string_lossy().into_owned(),
        false,
        "/dev/ramshared-test-nbd".into(),
        true,
        serve_pool::DEFAULT_SERVE_THREADS,
        &mut s1,
    )
    .expect("wddm poll + recovery branches complete");
    s1.producer.take().expect("p1").join().expect("p1 join");
    assert!(
        s1.demote_reasons
            .iter()
            .any(|r| r.as_deref() == Some("WddmBudgetPoll"))
    );
    assert!(s1.recovery_step.load(Ordering::SeqCst) >= 2);

    // 2. GlobalGpuFreeFloor + Rejected swapoff.
    let sock2 = root.join("s2.sock");
    let mut s2 = BranchStarter {
        budget_mode: Arc::new(AtomicUsize::new(1)),
        global_free: Arc::new(AtomicUsize::new(2)),
        free_bytes: Arc::new(AtomicU64::new(8 * 1024 * 1024 * 1024)),
        alloc_count: Arc::new(AtomicUsize::new(0)),
        recovery_step: Arc::new(AtomicUsize::new(0)),
        swapoff_count: Arc::new(AtomicUsize::new(0)),
        scenario: 1,
        swapoff_mode: 1,
        fail_mem_info: Arc::new(AtomicBool::new(false)),
        spike_elapsed: false,
        elapsed_calls: Arc::new(AtomicUsize::new(0)),
        fail_notify: false,
        first_claim_none: false,
        claim_attempts: Arc::new(AtomicUsize::new(0)),
        producer: None,
        demote_reasons: Vec::new(),
    };
    run_nbd_with_startup(
        ConfigurableProvider {
            fail_mem_info: Arc::clone(&s2.fail_mem_info),
            free_bytes: Arc::clone(&s2.free_bytes),
            alloc_count: Arc::clone(&s2.alloc_count),
        },
        None,
        65536,
        sock2.to_string_lossy().into_owned(),
        false,
        "/dev/ramshared-test-nbd".into(),
        true,
        serve_pool::DEFAULT_SERVE_THREADS,
        &mut s2,
    )
    .expect("global free floor + rejected swapoff complete");
    s2.producer.take().expect("p2").join().expect("p2 join");
    assert!(
        s2.demote_reasons
            .iter()
            .any(|r| r.as_deref() == Some("GlobalGpuFreeFloor"))
    );

    // 3. GpuLost on idle tick + Disconnected swapoff + mem_info error during reclaim.
    let sock3 = root.join("s3.sock");
    let mut s3 = BranchStarter {
        budget_mode: Arc::new(AtomicUsize::new(1)),
        global_free: Arc::new(AtomicUsize::new(1)),
        free_bytes: Arc::new(AtomicU64::new(8 * 1024 * 1024 * 1024)),
        alloc_count: Arc::new(AtomicUsize::new(0)),
        recovery_step: Arc::new(AtomicUsize::new(0)),
        swapoff_count: Arc::new(AtomicUsize::new(0)),
        scenario: 2,
        swapoff_mode: 2,
        fail_mem_info: Arc::new(AtomicBool::new(false)),
        spike_elapsed: false,
        elapsed_calls: Arc::new(AtomicUsize::new(0)),
        fail_notify: false,
        first_claim_none: false,
        claim_attempts: Arc::new(AtomicUsize::new(0)),
        producer: None,
        demote_reasons: Vec::new(),
    };
    run_nbd_with_startup(
        ConfigurableProvider {
            fail_mem_info: Arc::clone(&s3.fail_mem_info),
            free_bytes: Arc::clone(&s3.free_bytes),
            alloc_count: Arc::clone(&s3.alloc_count),
        },
        None,
        65536,
        sock3.to_string_lossy().into_owned(),
        false,
        "/dev/ramshared-test-nbd".into(),
        false,
        serve_pool::DEFAULT_SERVE_THREADS,
        &mut s3,
    )
    .expect("gpu lost + disconnected swapoff complete");
    s3.producer.take().expect("p3").join().expect("p3 join");
    assert!(
        s3.demote_reasons
            .iter()
            .any(|r| r.as_deref() == Some("GpuLost"))
    );

    // 4. Sparse reclaim + WddmBudget Disconnected poll + Latency/probe combos + notify error + claim retry.
    let sock4 = root.join("s4.sock");
    let mut s4 = BranchStarter {
        budget_mode: Arc::new(AtomicUsize::new(1)),
        global_free: Arc::new(AtomicUsize::new(1)),
        free_bytes: Arc::new(AtomicU64::new(8 * 1024 * 1024 * 1024)),
        alloc_count: Arc::new(AtomicUsize::new(0)),
        recovery_step: Arc::new(AtomicUsize::new(0)),
        swapoff_count: Arc::new(AtomicUsize::new(0)),
        scenario: 3,
        swapoff_mode: 2,
        fail_mem_info: Arc::new(AtomicBool::new(false)),
        spike_elapsed: true,
        elapsed_calls: Arc::new(AtomicUsize::new(0)),
        fail_notify: true,
        first_claim_none: true,
        claim_attempts: Arc::new(AtomicUsize::new(0)),
        producer: None,
        demote_reasons: Vec::new(),
    };
    run_nbd_with_startup(
        ConfigurableProvider {
            fail_mem_info: Arc::clone(&s4.fail_mem_info),
            free_bytes: Arc::clone(&s4.free_bytes),
            alloc_count: Arc::clone(&s4.alloc_count),
        },
        None,
        65536,
        sock4.to_string_lossy().into_owned(),
        false,
        "/dev/ramshared-test-nbd".into(),
        true,
        serve_pool::DEFAULT_SERVE_THREADS,
        &mut s4,
    )
    .expect("reclaim + residency combos + WddmBudget disconnected complete");
    s4.producer.take().expect("p4").join().expect("p4 join");
    assert!(s4.swapoff_count.load(Ordering::SeqCst) >= 1);
    assert!(s4.claim_attempts.load(Ordering::SeqCst) >= 2);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn stop_worker_process_covers_kill_error_and_reaper_unavailable_paths() {
    use std::collections::VecDeque;

    struct FakeWorker {
        waits: VecDeque<std::io::Result<bool>>,
        kill_res: Option<std::io::Result<()>>,
        reap_res: Option<std::io::Result<()>>,
    }

    impl WorkerProcessControl for FakeWorker {
        fn id(&self) -> u32 {
            4242
        }
        fn try_wait(&mut self) -> std::io::Result<bool> {
            self.waits.pop_front().unwrap_or(Ok(false))
        }
        fn kill(&mut self) -> std::io::Result<()> {
            self.kill_res.take().unwrap_or(Ok(()))
        }
        fn reap_in_background(&mut self) -> std::io::Result<()> {
            self.reap_res.take().unwrap_or(Ok(()))
        }
    }

    let mut would_block = FakeWorker {
        waits: VecDeque::from([Ok(false)]),
        kill_res: Some(Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "held lock",
        ))),
        reap_res: None,
    };
    assert_eq!(
        stop_worker_process(
            &mut would_block,
            Duration::ZERO,
            Duration::ZERO,
            Duration::from_millis(1)
        ),
        WorkerStopOutcome::ReaperUnavailable
    );

    let mut kill_err_exited = FakeWorker {
        waits: VecDeque::from([Ok(false), Ok(true)]),
        kill_res: Some(Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "eperm",
        ))),
        reap_res: None,
    };
    assert_eq!(
        stop_worker_process(
            &mut kill_err_exited,
            Duration::ZERO,
            Duration::ZERO,
            Duration::from_millis(1)
        ),
        WorkerStopOutcome::Exited
    );

    let mut reap_err = FakeWorker {
        waits: VecDeque::from([Ok(false), Ok(false)]),
        kill_res: Some(Ok(())),
        reap_res: Some(Err(std::io::Error::other("reaper spawn failed"))),
    };
    assert_eq!(
        stop_worker_process(
            &mut reap_err,
            Duration::ZERO,
            Duration::ZERO,
            Duration::from_millis(1)
        ),
        WorkerStopOutcome::ReaperUnavailable
    );
}

#[test]
fn recovery_activation_covers_duplicate_and_parked_starts() {
    let mut active = RecoveryActivation::default();
    let (_tx1, rx1) = std::sync::mpsc::channel();
    active.start(rx1).expect("initial activation start");
    let (_tx2, rx2) = std::sync::mpsc::channel();
    assert!(active.start(rx2).is_err());

    let mut parked = RecoveryActivation::default();
    parked.mark_dispatch_failure();
    let (_tx3, rx3) = std::sync::mpsc::channel();
    assert!(parked.start(rx3).is_err());
}
