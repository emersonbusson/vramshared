// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main_tests/ublk_shutdown_and_worker.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;

#[test]
fn daemon_ublk_shutdown_is_swapoff_first_and_preserves_on_uncertainty() {
    struct Server(std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>);

    impl UblkServer for Server {
        fn authorize_shutdown(&mut self, _: &ShutdownPermit) -> std::io::Result<()> {
            Ok(())
        }

        fn join(self: Box<Self>) -> std::io::Result<()> {
            self.0.lock().expect("test call log").push("join");
            Ok(())
        }
    }

    struct Runtime {
        calls: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>,
        states: std::collections::VecDeque<Result<ExactSwapState, &'static str>>,
        swapoff_fails: bool,
    }

    impl Runtime {
        fn new(
            states: impl IntoIterator<Item = Result<ExactSwapState, &'static str>>,
            swapoff_fails: bool,
        ) -> Self {
            Self {
                calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                states: states.into_iter().collect(),
                swapoff_fails,
            }
        }

        fn mark(&self, call: &'static str) {
            self.calls.lock().expect("test call log").push(call);
        }

        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().expect("test call log").clone()
        }
    }

    impl UblkRuntime for Runtime {
        fn try_claim_shutdown(&mut self, _: &str) -> Option<ShutdownPermit> {
            Some(ShutdownPermit::injected(()))
        }

        fn wait_for_release_progress(&mut self) {}

        fn guard_not_wsl2(&mut self) -> Result<(), Box<dyn std::error::Error>> {
            self.mark("guard");
            Ok(())
        }

        fn lock_memory(
            &mut self,
            _force: bool,
            lock_future: bool,
        ) -> Result<(), Box<dyn std::error::Error>> {
            assert!(!lock_future);
            self.mark("lock");
            Ok(())
        }

        fn install_shutdown_handler(&mut self) -> Result<(), Box<dyn std::error::Error>> {
            self.mark("signal");
            Ok(())
        }

        fn add_device(
            &mut self,
            queue_depth: u16,
        ) -> Result<UblkDevice, Box<dyn std::error::Error>> {
            self.mark("add");
            Ok(UblkDevice { id: 7, queue_depth })
        }

        fn set_params(
            &mut self,
            _device: UblkDevice,
            _sectors: u64,
        ) -> Result<(), Box<dyn std::error::Error>> {
            self.mark("params");
            Ok(())
        }

        fn start_server(
            &mut self,
            _backend: BackendKind,
            _char_path: &str,
            _block_path: &str,
            _queue_depth: u16,
            _size: u64,
        ) -> Result<Box<dyn UblkServer>, Box<dyn std::error::Error>> {
            self.mark("server");
            Ok(Box::new(Server(std::sync::Arc::clone(&self.calls))))
        }

        fn start_device(&mut self, _device: UblkDevice) -> Result<(), Box<dyn std::error::Error>> {
            self.mark("start");
            Ok(())
        }

        fn wait_for_shutdown(&mut self) -> Result<(), Box<dyn std::error::Error>> {
            self.mark("wait");
            Ok(())
        }

        fn swap_state(
            &mut self,
            _block_path: &str,
        ) -> Result<ExactSwapState, Box<dyn std::error::Error>> {
            self.mark("swap-state");
            match self.states.pop_front().expect("planned strict snapshot") {
                Ok(state) => Ok(state),
                Err(error) => Err(std::io::Error::other(error).into()),
            }
        }

        fn spawn_swapoff(
            &mut self,
            _block_path: &str,
        ) -> std::sync::mpsc::Receiver<SwapoffOutcome> {
            self.mark("swapoff");
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            tx.send(if self.swapoff_fails {
                SwapoffOutcome::Rejected { exit_code: Some(1) }
            } else {
                SwapoffOutcome::Drained
            })
            .unwrap();
            rx
        }

        fn stop_device(&mut self, _device: UblkDevice) -> Result<(), Box<dyn std::error::Error>> {
            self.mark("stop");
            Ok(())
        }

        fn delete_device(&mut self, _device: UblkDevice) -> Result<(), Box<dyn std::error::Error>> {
            self.mark("delete");
            Ok(())
        }
    }

    let prefix = vec![
        "guard", "lock", "signal", "add", "params", "server", "start", "wait",
    ];

    let mut active_zero = Runtime::new(
        [
            Ok(ExactSwapState::Active { used_kb: 0 }),
            Ok(ExactSwapState::Active { used_kb: 0 }),
            Ok(ExactSwapState::Absent),
            Ok(ExactSwapState::Absent),
            Ok(ExactSwapState::Absent),
        ],
        true,
    );
    run_ublk_with_runtime(4096, false, 1, BackendKind::Ram, &mut active_zero)
        .expect("a refused drain must keep serving until a later strict absence permits shutdown");
    let mut expected = prefix.clone();
    expected.extend([
        "swap-state",
        "swapoff",
        "swap-state",
        "swap-state",
        "swap-state",
        "stop",
        "join",
        "swap-state",
        "delete",
    ]);
    assert_eq!(active_zero.calls(), expected);

    let mut active_used = Runtime::new(
        [
            Ok(ExactSwapState::Active { used_kb: 12 }),
            Ok(ExactSwapState::Absent),
            Ok(ExactSwapState::Absent),
            Ok(ExactSwapState::Absent),
        ],
        false,
    );
    run_ublk_with_runtime(4096, false, 1, BackendKind::Ram, &mut active_used)
        .expect("swapoff-first shutdown with fresh absence proofs");
    let mut expected = prefix.clone();
    expected.extend([
        "swap-state",
        "swapoff",
        "swap-state",
        "swap-state",
        "stop",
        "join",
        "swap-state",
        "delete",
    ]);
    assert_eq!(active_used.calls(), expected);

    let mut unreadable = Runtime::new(
        [
            Err("unreadable /proc/swaps"),
            Ok(ExactSwapState::Absent),
            Ok(ExactSwapState::Absent),
            Ok(ExactSwapState::Absent),
        ],
        false,
    );
    run_ublk_with_runtime(4096, false, 1, BackendKind::Ram, &mut unreadable)
        .expect("unavailable observation keeps the server alive until absence is proven");
    let mut expected = prefix;
    expected.extend([
        "swap-state",
        "swap-state",
        "swap-state",
        "stop",
        "join",
        "swap-state",
        "delete",
    ]);
    assert_eq!(unreadable.calls(), expected);
}

#[test]
fn daemon_ublk_wsl_guard_and_memory_lock_policy_are_pure_and_fail_closed() {
    assert!(ublk_platform_guard(Some(true)).is_err());
    assert!(ublk_platform_guard(Some(true)).is_err());
    assert!(ublk_platform_guard(Some(false)).is_ok());
    assert!(
        ublk_platform_guard(Some(true)).is_err(),
        "the dangerous WSL2 override must not exist"
    );

    assert!(matches!(
        memory_lock_status(true, true, false),
        Ok(MemoryLockStatus::Protected)
    ));
    assert!(memory_lock_status(false, true, false).is_err());
    assert!(memory_lock_status(true, false, false).is_err());
    assert!(matches!(
        memory_lock_status(false, false, true),
        Ok(MemoryLockStatus::ForcedDegraded {
            locked: false,
            oom_ok: false
        })
    ));
    assert!(matches!(
        memory_lock_status(true, false, true),
        Ok(MemoryLockStatus::ForcedDegraded {
            locked: true,
            oom_ok: false
        })
    ));
}

#[test]
fn origin_mode_never_arms_mcl_future_before_cache_mappings() {
    let error = lock_memory(false, true).unwrap_err();
    assert!(error.to_string().contains("MCL_FUTURE is forbidden"));
}

#[test]
fn daemon_worker_reply_is_io_accounting_barrier_and_shutdown_is_bounded() {
    const REPLIES: u64 = 512;
    let (jobs_tx, jobs_rx) = std::sync::mpsc::sync_channel(CHAN_CAP);
    let (demote_tx, _demote_rx) = std::sync::mpsc::channel();
    let shutdown = std::sync::Arc::new(AtomicBool::new(false));
    let stop = BrokerShutdown::new(std::sync::Arc::clone(&shutdown), jobs_tx.clone());
    let slice_io = std::sync::Arc::new(vec![SliceIoCounters::default()]);
    let worker_rt = BrokerWorkerRuntime {
        geom: vec![(0, 4096)],
        jobs_rx,
        demote_tx,
        shutdown: std::sync::Arc::clone(&shutdown),
        shutdown_wake_pending: std::sync::Arc::clone(&stop.wake_pending),
        shutdown_wake_tx: stop.wake_tx.clone(),
        slice_io: std::sync::Arc::clone(&slice_io),
        vram: std::sync::Arc::new(VramGauge::default()),
    };
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    let (published_tx, published_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let hook_timed_out = std::sync::Arc::new(AtomicBool::new(false));
    let hook_timeout_observed = std::sync::Arc::clone(&hook_timed_out);
    let hook_shutdown = std::sync::Arc::clone(&shutdown);

    let (observations, failure, wake, joined, elapsed) = std::thread::scope(|scope| {
        let worker = scope.spawn(move || {
            serve_broker_jobs_with_poll_and_reply_hook(
                RamBackend::new(4096),
                worker_rt,
                |_| None,
                Duration::from_millis(10),
                move || {
                    if published_tx.send(()).is_ok()
                        && release_rx.recv_timeout(Duration::from_secs(2)).is_err()
                    {
                        hook_timeout_observed.store(true, Ordering::SeqCst);
                        hook_shutdown.store(true, Ordering::SeqCst);
                    }
                },
            )
        });
        let started = Instant::now();
        let _shutdown_guard = BrokerWorkerShutdownGuard(&stop);
        let mut observations = Vec::with_capacity(REPLIES as usize);
        let mut failure = None;
        let mut deterministic_wake = None;

        for sequence in 1..=REPLIES {
            if jobs_tx
                .send(WMsg::Job(ramshared_wsl2d::conn::Job {
                    export: 0,
                    req: ramshared_block::Request {
                        flags: 0,
                        cmd: Command::Write,
                        handle: sequence,
                        offset: 0,
                        len: 512,
                    },
                    payload: vec![0x5A; 512],
                    credit: fixture_credit(),
                    reply: reply_tx.clone(),
                }))
                .is_err()
            {
                failure = Some(format!("worker queue disconnected at reply {sequence}"));
                break;
            }

            let reply = match reply_rx.recv_timeout(Duration::from_secs(2)) {
                Ok(reply) => reply,
                Err(error) => {
                    failure = Some(format!("reply {sequence} missed its deadline: {error}"));
                    break;
                }
            };
            if reply.disconnect {
                failure = Some(format!("reply {sequence} unexpectedly disconnected"));
                let _ = release_tx.send(());
                break;
            }
            if let Err(error) = published_rx.recv_timeout(Duration::from_secs(2)) {
                failure = Some(format!(
                    "reply {sequence} publication hook missed its deadline: {error}"
                ));
                let _ = release_tx.send(());
                break;
            }
            observations.push((
                sequence,
                slice_io[0].bytes_served.load(Ordering::Acquire),
                slice_io[0].io_count.load(Ordering::Acquire),
            ));
            if sequence == REPLIES {
                // Take the wake while the worker is still parked inside the
                // publication hook, before this iteration's release. `jobs_rx`
                // is alive and the bounded queue is empty, so the wake shape
                // is a property of the queue and not of who won a race to
                // reach quiescence.
                //
                // That matters because `BrokerShutdownWake::Disconnected` is
                // a documented-valid outcome — the enum says a disconnected
                // queue is already terminal — and it is exactly what a
                // too-late `request()` returns once the worker has drained and
                // dropped its receiver. Asserting `Queued` after letting the
                // worker reach quiescence first turns that race into a false
                // failure on correct behaviour. Requesting while the hook
                // still holds the worker removes the race instead of widening
                // the assertion to accept every arm.
                deterministic_wake = Some(stop.request());
            }
            if release_tx.send(()).is_err() {
                failure = Some(format!(
                    "worker publication hook disconnected at reply {sequence}"
                ));
                break;
            }
        }

        // An early `break` never reached the deterministic point above. The
        // request still has to happen here so the worker can leave its loop
        // and join; that path's wake shape is whatever the queue says, and
        // `failure` is already non-null so the assertions below own the report.
        let wake = deterministic_wake.unwrap_or_else(|| stop.request());
        let joined = worker.join();
        (observations, failure, wake, joined, started.elapsed())
    });

    assert_eq!(failure, None);
    assert!(!hook_timed_out.load(Ordering::SeqCst));
    assert_eq!(wake, BrokerShutdownWake::Queued);
    assert!(joined.is_ok(), "worker joins after shutdown");
    assert_eq!(observations.len(), REPLIES as usize);
    for (sequence, bytes, io_count) in observations {
        assert_eq!(
            bytes,
            sequence * 512,
            "reply {sequence} exposed stale byte accounting"
        );
        assert_eq!(
            io_count, sequence,
            "reply {sequence} exposed stale IO accounting"
        );
    }
    assert!(
        elapsed < Duration::from_secs(5),
        "worker completion and shutdown exceeded the bounded deadline"
    );
}

#[test]
fn daemon_worker_shutdown_wake_is_not_timer_dependent() {
    let (jobs_tx, jobs_rx) = std::sync::mpsc::sync_channel(CHAN_CAP);
    let (demote_tx, _demote_rx) = std::sync::mpsc::channel();
    let shutdown = std::sync::Arc::new(AtomicBool::new(false));
    let stop = BrokerShutdown::new(std::sync::Arc::clone(&shutdown), jobs_tx.clone());
    let worker_rt = BrokerWorkerRuntime {
        geom: vec![(0, 4096)],
        jobs_rx,
        demote_tx,
        shutdown: std::sync::Arc::clone(&shutdown),
        shutdown_wake_pending: std::sync::Arc::clone(&stop.wake_pending),
        shutdown_wake_tx: stop.wake_tx.clone(),
        slice_io: std::sync::Arc::new(vec![SliceIoCounters::default()]),
        vram: std::sync::Arc::new(VramGauge::default()),
    };

    std::thread::scope(|scope| {
        let worker = scope.spawn(move || {
            serve_broker_jobs_with_poll(
                RamBackend::new(4096),
                worker_rt,
                |_| None,
                Duration::from_secs(30),
            )
        });
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(stop.request(), BrokerShutdownWake::Queued);
        let started = Instant::now();
        let _backend = worker.join().expect("shutdown wake joins worker");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "explicit shutdown wake must not wait for the receive timer"
        );
    });
}

fn run_full_queue_shutdown_fixture() {
    let (jobs_tx, jobs_rx) = std::sync::mpsc::sync_channel(CHAN_CAP);
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    jobs_tx
        .try_send(WMsg::Job(ramshared_wsl2d::conn::Job {
            export: 0,
            req: ramshared_block::Request {
                flags: 0,
                cmd: Command::Write,
                handle: 17,
                offset: 0,
                len: 512,
            },
            payload: vec![0xA5; 512],
            credit: fixture_credit(),
            reply: reply_tx,
        }))
        .expect("manufactured queue accepts one admitted IO before shutdown");
    for _ in 1..CHAN_CAP {
        jobs_tx
            .try_send(WMsg::Opened)
            .expect("manufactured queue has exact capacity");
    }
    let shutdown = std::sync::Arc::new(AtomicBool::new(false));
    let stop = BrokerShutdown::new(std::sync::Arc::clone(&shutdown), jobs_tx.clone());
    let (demote_tx, _demote_rx) = std::sync::mpsc::channel();
    let slice_io = std::sync::Arc::new(vec![SliceIoCounters::default()]);
    let worker_rt = BrokerWorkerRuntime {
        geom: vec![(0, 4096)],
        jobs_rx,
        demote_tx,
        shutdown: std::sync::Arc::clone(&shutdown),
        shutdown_wake_pending: std::sync::Arc::clone(&stop.wake_pending),
        shutdown_wake_tx: stop.wake_tx.clone(),
        slice_io: std::sync::Arc::clone(&slice_io),
        vram: std::sync::Arc::new(VramGauge::default()),
    };

    assert_eq!(stop.request(), BrokerShutdownWake::QueueFull);
    assert!(shutdown.load(Ordering::SeqCst));
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let backend = serve_broker_jobs_with_poll(
            RamBackend::new(4096),
            worker_rt,
            |_| None,
            Duration::from_millis(10),
        );
        let _ = done_tx.send(backend);
    });

    let _backend = done_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("worker observes the terminal flag before a full queue");
    assert!(
        reply_rx.recv_timeout(Duration::from_millis(100)).is_err(),
        "worker executed queued IO after terminal state"
    );
    assert_eq!(slice_io[0].bytes_served.load(Ordering::Relaxed), 0);
    assert_eq!(slice_io[0].io_count.load(Ordering::Relaxed), 0);
    worker
        .join()
        .expect("full-queue worker thread must join after bounded shutdown");
    assert!(
        !stop.wake_pending.load(Ordering::SeqCst),
        "the worker retained a shutdown notifier after it stopped"
    );
}

#[test]
// TestName: daemon_worker_shutdown_full_queue_is_nonblocking
fn daemon_worker_shutdown_full_queue_is_nonblocking() {
    run_full_queue_shutdown_fixture();
}

#[test]
// TestName: daemon_worker_parallel_full_queue_shutdowns_reap_without_notifier_threads
fn daemon_worker_parallel_full_queue_shutdowns_reap_without_notifier_threads() {
    std::thread::scope(|scope| {
        let workers = (0..8)
            .map(|_| scope.spawn(run_full_queue_shutdown_fixture))
            .collect::<Vec<_>>();
        for worker in workers {
            worker
                .join()
                .expect("parallel full-queue shutdown fixture must join");
        }
    });
}

#[test]
// TestName: daemon_worker_shutdown_preempts_queued_io_at_iteration_boundary
fn daemon_worker_shutdown_preempts_queued_io_at_iteration_boundary() {
    let (jobs_tx, jobs_rx) = std::sync::mpsc::sync_channel(CHAN_CAP);
    let (demote_tx, _demote_rx) = std::sync::mpsc::channel();
    let shutdown = std::sync::Arc::new(AtomicBool::new(false));
    let stop = BrokerShutdown::new(std::sync::Arc::clone(&shutdown), jobs_tx.clone());
    let slice_io = std::sync::Arc::new(vec![SliceIoCounters::default()]);
    let worker_rt = BrokerWorkerRuntime {
        geom: vec![(0, 4096)],
        jobs_rx,
        demote_tx,
        shutdown,
        shutdown_wake_pending: std::sync::Arc::clone(&stop.wake_pending),
        shutdown_wake_tx: stop.wake_tx.clone(),
        slice_io: std::sync::Arc::clone(&slice_io),
        vram: std::sync::Arc::new(VramGauge::default()),
    };
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    jobs_tx
        .send(WMsg::Job(ramshared_wsl2d::conn::Job {
            export: 0,
            req: ramshared_block::Request {
                flags: 0,
                cmd: Command::Write,
                handle: 11,
                offset: 0,
                len: 512,
            },
            payload: vec![0xA5; 512],
            credit: fixture_credit(),
            reply: reply_tx,
        }))
        .expect("queue accepts write before shutdown");
    assert_eq!(stop.request(), BrokerShutdownWake::Queued);

    let _backend = serve_broker_jobs_with_poll(
        RamBackend::new(4096),
        worker_rt,
        |_| None,
        Duration::from_secs(30),
    );
    assert!(
        reply_rx.recv_timeout(Duration::from_millis(100)).is_err(),
        "terminal worker executed queued IO after observing shutdown"
    );
    assert_eq!(slice_io[0].bytes_served.load(Ordering::Relaxed), 0);
    assert_eq!(slice_io[0].io_count.load(Ordering::Relaxed), 0);
}

#[test]
// TestName: daemon_worker_terminal_flag_wins_over_512_continuous_queue_refills
fn daemon_worker_terminal_flag_wins_over_512_continuous_queue_refills() {
    const REFILLS: u64 = 512;
    let (jobs_tx, jobs_rx) = std::sync::mpsc::sync_channel(CHAN_CAP);
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    let make_job = |handle| {
        WMsg::Job(ramshared_wsl2d::conn::Job {
            export: 0,
            req: ramshared_block::Request {
                flags: 0,
                cmd: Command::Write,
                handle,
                offset: 0,
                len: 512,
            },
            payload: vec![0x5A; 512],
            credit: fixture_credit(),
            reply: reply_tx.clone(),
        })
    };
    for handle in 0..CHAN_CAP as u64 {
        jobs_tx
            .try_send(make_job(handle))
            .expect("fixture starts with an exactly full worker queue");
    }
    let shutdown = std::sync::Arc::new(AtomicBool::new(false));
    let stop = BrokerShutdown::new(std::sync::Arc::clone(&shutdown), jobs_tx.clone());
    let (demote_tx, _demote_rx) = std::sync::mpsc::channel();
    let worker_rt = BrokerWorkerRuntime {
        geom: vec![(0, 4096)],
        jobs_rx,
        demote_tx,
        shutdown,
        shutdown_wake_pending: std::sync::Arc::clone(&stop.wake_pending),
        shutdown_wake_tx: stop.wake_tx.clone(),
        slice_io: std::sync::Arc::new(vec![SliceIoCounters::default()]),
        vram: std::sync::Arc::new(VramGauge::default()),
    };
    let (refill_tx, refill_rx) = std::sync::mpsc::sync_channel::<u64>(0);
    let (refilled_tx, refilled_rx) = std::sync::mpsc::sync_channel::<()>(0);
    let processed = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let hook_processed = std::sync::Arc::clone(&processed);
    let hook_stop = stop.clone();

    std::thread::scope(|scope| {
        let producer = scope.spawn(move || {
            for sequence in 0..REFILLS {
                let handle = refill_rx
                    .recv_timeout(Duration::from_secs(1))
                    .expect("worker requests each deterministic refill");
                jobs_tx
                    .send(make_job(handle.saturating_add(CHAN_CAP as u64)))
                    .expect("one freed queue slot accepts one refill");
                refilled_tx
                    .send(())
                    .expect("worker observes queue-full restoration");
                assert_eq!(handle, sequence + 1);
            }
        });
        let worker = scope.spawn(move || {
            serve_broker_jobs_with_poll_and_reply_hook(
                RamBackend::new(4096),
                worker_rt,
                |_| None,
                Duration::from_millis(10),
                move || {
                    let sequence = hook_processed.fetch_add(1, Ordering::SeqCst) + 1;
                    if sequence <= REFILLS {
                        refill_tx
                            .send(sequence)
                            .expect("refill producer remains present");
                        refilled_rx
                            .recv_timeout(Duration::from_secs(1))
                            .expect("producer restores a full queue without sleeps");
                    }
                    if sequence == REFILLS {
                        assert_eq!(hook_stop.request(), BrokerShutdownWake::QueueFull);
                    }
                },
            )
        });
        producer.join().expect("bounded refill producer joins");
        worker.join().expect("terminal worker joins");
    });
    let reply_count = reply_rx.try_iter().count() as u64;

    assert_eq!(
        processed.load(Ordering::SeqCst),
        REFILLS,
        "a continuously full queue starved the terminal flag"
    );
    assert_eq!(reply_count, REFILLS);
}
