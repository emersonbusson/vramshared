// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main_tests/nbd_teardown_and_drain.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;

#[test]
fn daemon_nbd_recovery_activation_does_not_block_nbd_jobs() {
    // Every wait below is a terminal-event deadline, not a performance
    // budget: the daemon path itself finishes in milliseconds. CI runs
    // `cargo test -- --test-threads=1`, but a local parallel run on a
    // loaded host can preempt the daemon thread long enough to expire a
    // 5s window and fail the test without any product defect. 30s keeps
    // the refusal fail-closed while surviving that contention.
    const TERMINAL_EVENT_DEADLINE: Duration = Duration::from_secs(30);

    fn assert_nbd_ok(reply: Reply, context: &str) {
        assert!(
            !reply.disconnect,
            "{context} must keep the NBD connection open"
        );
        assert_eq!(
            [
                reply.reply[4],
                reply.reply[5],
                reply.reply[6],
                reply.reply[7]
            ],
            [0; 4],
            "{context} must return NBD_OK"
        );
    }

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

    struct RecoveryBudget {
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl NbdBudgetProvider for RecoveryBudget {
        fn snapshot(&self) -> Result<NbdBudgetSnapshot, String> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err("injected initial constrained budget".into());
            }
            Ok(NbdBudgetSnapshot {
                budget: 4 * 1024 * 1024 * 1024,
                current_usage: 0,
                sampled_at: Instant::now(),
            })
        }
    }

    struct PendingRecoveryStarter {
        budget: Option<Box<dyn NbdBudgetProvider>>,
        jobs_tx: std::sync::mpsc::Sender<std::sync::mpsc::SyncSender<WMsg>>,
        activation_started: std::sync::mpsc::Sender<()>,
        activation_rx: Option<std::sync::mpsc::Receiver<bool>>,
        activation_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        swapoff_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    struct ActivationOutcomeGuard(Option<std::sync::mpsc::Sender<bool>>);

    impl ActivationOutcomeGuard {
        fn succeed(&mut self) {
            self.0
                .take()
                .expect("the test still owns the activation outcome")
                .send(true)
                .expect("observe terminal activation success");
        }
    }

    impl Drop for ActivationOutcomeGuard {
        fn drop(&mut self) {
            if let Some(tx) = self.0.take() {
                let _ = tx.send(false);
            }
        }
    }

    impl NbdRuntimeStarter for PendingRecoveryStarter {
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
            self.jobs_tx.send(jobs_tx)?;
            Ok(())
        }

        fn nbd_used_kb(&mut self, _nbd_dev: &str) -> u64 {
            0
        }

        fn publish_demote(&mut self, _status: &ramshared_wsl2d::DemoteStatusFile) {}

        fn elapsed_us(&mut self, _started: Instant) -> u64 {
            10
        }

        fn spawn_swapoff(&mut self, _nbd_dev: &str) -> std::sync::mpsc::Receiver<SwapoffOutcome> {
            self.swapoff_calls.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) = std::sync::mpsc::channel();
            tx.send(SwapoffOutcome::Drained)
                .expect("in-memory demote completion");
            rx
        }

        fn spawn_recovery_activation(
            &mut self,
            _nbd_dev: &str,
            priority: i16,
        ) -> Result<std::sync::mpsc::Receiver<bool>, Box<dyn std::error::Error>> {
            assert_eq!(priority, 100);
            self.activation_calls.fetch_add(1, Ordering::SeqCst);
            self.activation_started.send(())?;
            self.activation_rx
                .take()
                .ok_or_else(|| std::io::Error::other("duplicate recovery activation"))
                .map_err(Into::into)
        }

        fn startup_budget(
            &mut self,
            requested: bool,
        ) -> Result<Option<Box<dyn NbdBudgetProvider>>, Box<dyn std::error::Error>> {
            assert!(requested, "the test drives the bounded recovery path");
            Ok(self.budget.take())
        }

        fn global_free_bytes(&mut self, _timeout: Duration) -> Option<u64> {
            Some(8 * 1024 * 1024 * 1024)
        }
    }

    let path = std::env::temp_dir().join(format!(
        "ramshared-daemon-pending-recovery-{}.sock",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let (jobs_tx, jobs_rx) = std::sync::mpsc::channel();
    let (activation_started_tx, activation_started_rx) = std::sync::mpsc::channel();
    let (activation_result_tx, activation_result_rx) = std::sync::mpsc::channel();
    let mut activation_outcome = ActivationOutcomeGuard(Some(activation_result_tx));
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let activation_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let budget_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let thread_path = path.clone();
    let thread_activation_calls = std::sync::Arc::clone(&activation_calls);
    let swapoff_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let thread_swapoff_calls = std::sync::Arc::clone(&swapoff_calls);
    let runner = std::thread::spawn(move || {
        let mut starter = PendingRecoveryStarter {
            budget: Some(Box::new(RecoveryBudget {
                calls: budget_calls,
            })),
            jobs_tx,
            activation_started: activation_started_tx,
            activation_rx: Some(activation_result_rx),
            activation_calls: thread_activation_calls,
            swapoff_calls: thread_swapoff_calls,
        };
        let result = run_nbd_with_startup(
            TestProvider,
            None,
            4096,
            thread_path.to_string_lossy().into_owned(),
            false,
            "/dev/ramshared-test-nbd".into(),
            true,
            serve_pool::DEFAULT_SERVE_THREADS,
            &mut starter,
        )
        .map_err(|error| error.to_string());
        let _ = done_tx.send(result);
    });

    let worker_tx = jobs_rx
        .recv_timeout(TERMINAL_EVENT_DEADLINE)
        .expect("injected acceptor exposes the live NBD worker");
    worker_tx.send(WMsg::Opened).expect("open NBD generation");
    for handle in 0..3 {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        worker_tx
            .send(WMsg::Job(ramshared_wsl2d::conn::Job {
                export: 0,
                req: ramshared_block::Request {
                    flags: 0,
                    cmd: Command::Flush,
                    handle,
                    offset: 0,
                    len: 0,
                },
                payload: Vec::new(),
                credit: fixture_credit(),
                reply: reply_tx,
            }))
            .expect("drive one bounded recovery sample");
        assert_nbd_ok(
            reply_rx
                .recv_timeout(TERMINAL_EVENT_DEADLINE)
                .expect("pre-recovery NBD job reply"),
            "pre-recovery NBD job",
        );
    }
    activation_started_rx
        .recv_timeout(TERMINAL_EVENT_DEADLINE)
        .expect("the third healthy sample starts one pending activation");

    worker_tx
        .send(WMsg::Shutdown)
        .expect("request shutdown while activation is pending");
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    worker_tx
        .send(WMsg::Job(ramshared_wsl2d::conn::Job {
            export: 0,
            req: ramshared_block::Request {
                flags: 0,
                cmd: Command::Flush,
                handle: 99,
                offset: 0,
                len: 0,
            },
            payload: Vec::new(),
            credit: fixture_credit(),
            reply: reply_tx,
        }))
        .expect("queue NBD work after pending shutdown");
    assert_nbd_ok(
        reply_rx
            .recv_timeout(TERMINAL_EVENT_DEADLINE)
            .expect("pending recovery must not block the NBD serve loop"),
        "post-shutdown pending-recovery NBD job",
    );
    assert_eq!(
        activation_calls.load(Ordering::SeqCst),
        1,
        "a pending activation suppresses duplicate recovery launches"
    );
    assert!(
        path.exists(),
        "shutdown cannot clean the owned socket while swapon remains unobserved"
    );

    activation_outcome.succeed();
    assert_eq!(
        done_rx
            .recv_timeout(TERMINAL_EVENT_DEADLINE)
            .expect("daemon exits after the terminal activation outcome"),
        Ok(())
    );
    runner
        .join()
        .expect("pending-recovery test daemon thread joins");
    assert_eq!(
        swapoff_calls.load(Ordering::SeqCst),
        2,
        "recovery success during shutdown must start and observe final swapoff"
    );
    assert!(
        !path.exists(),
        "the owned socket is removed only after terminal recovery observation"
    );

    let (success_tx, success_rx) = std::sync::mpsc::channel();
    let mut success = RecoveryActivation::default();
    success
        .start(success_rx)
        .expect("the successful activation receiver is owned");
    success_tx
        .send(true)
        .expect("inject terminal swapon success");
    assert_eq!(success.poll(), RecoveryActivationPoll::Succeeded);
    assert!(
        success.launch_allowed(true, true),
        "a successful terminal result returns the tier to the available state"
    );
}

#[test]
fn daemon_nbd_recovery_failure_parks_without_relaunch() {
    let mut dispatch_failure = RecoveryActivation::default();
    dispatch_failure.mark_dispatch_failure();
    assert!(
        !dispatch_failure.launch_allowed(true, true),
        "a failed activation dispatch must park the current healthy epoch"
    );
    assert!(!dispatch_failure.launch_allowed(false, true));
    assert!(dispatch_failure.launch_allowed(true, true));

    let (activation_tx, activation_rx) = std::sync::mpsc::channel();
    let mut activation = RecoveryActivation::default();
    activation
        .start(activation_rx)
        .expect("the first recovery activation is owned");
    activation_tx
        .send(false)
        .expect("inject terminal swapon failure");

    assert_eq!(activation.poll(), RecoveryActivationPoll::Failed);
    assert!(
        !activation.launch_allowed(true, true),
        "one failed healthy epoch must not relaunch swapon"
    );
    assert!(!activation.launch_allowed(false, true));
    assert!(
        activation.launch_allowed(true, true),
        "an unhealthy observation starts a later recovery epoch"
    );

    let (disconnected_tx, disconnected_rx) = std::sync::mpsc::channel();
    drop(disconnected_tx);
    let mut disconnected = RecoveryActivation::default();
    disconnected
        .start(disconnected_rx)
        .expect("the disconnected activation receiver is owned");
    assert_eq!(disconnected.poll(), RecoveryActivationPoll::Failed);
    assert!(
        !disconnected.launch_allowed(true, true),
        "a disconnected activation receiver must also park the healthy epoch"
    );
}

#[test]
fn daemon_nbd_shutdown_with_pending_recovery_fails_closed() {
    let (activation_tx, activation_rx) = std::sync::mpsc::channel();
    let mut activation = RecoveryActivation::default();
    activation
        .start(activation_rx)
        .expect("the recovery activation is owned");
    activation.request_shutdown();

    assert_eq!(activation.poll(), RecoveryActivationPoll::Pending);
    assert!(
        !activation.backend_release_allowed(),
        "an unobserved swapon child must retain the backend"
    );
    activation_tx
        .send(false)
        .expect("inject terminal swapon failure");
    assert_eq!(activation.poll(), RecoveryActivationPoll::Failed);
    assert!(activation.backend_release_allowed());
}

#[test]
fn daemon_nbd_teardown_refuses_until_fake_usage_and_swapoff_confirm() {
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

    struct TeardownStarter {
        usage: std::collections::VecDeque<u64>,
        swapoff_calls: usize,
    }

    impl NbdRuntimeStarter for TeardownStarter {
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
            jobs_tx.send(WMsg::Opened)?;
            jobs_tx.send(WMsg::Closed)?;
            jobs_tx.send(WMsg::Shutdown)?;
            Ok(())
        }

        fn nbd_used_kb(&mut self, _nbd_dev: &str) -> u64 {
            // Once the planned active-to-zero transition is observed,
            // subsequent cadence and shutdown observations remain zero.
            self.usage.pop_front().unwrap_or(0)
        }

        fn publish_demote(&mut self, _status: &ramshared_wsl2d::DemoteStatusFile) {}

        fn elapsed_us(&mut self, _started: Instant) -> u64 {
            10
        }

        fn spawn_swapoff(&mut self, _nbd_dev: &str) -> std::sync::mpsc::Receiver<SwapoffOutcome> {
            self.swapoff_calls += 1;
            let (tx, rx) = std::sync::mpsc::channel();
            tx.send(SwapoffOutcome::Drained)
                .expect("fake swapoff completion");
            rx
        }

        fn spawn_recovery_activation(
            &mut self,
            _nbd_dev: &str,
            _priority: i16,
        ) -> Result<std::sync::mpsc::Receiver<bool>, Box<dyn std::error::Error>> {
            panic!("teardown refusal must not activate swap")
        }

        fn teardown_retry_delay(&mut self) -> Duration {
            Duration::ZERO
        }
    }

    let path = std::env::temp_dir().join(format!(
        "ramshared-daemon-teardown-refusal-{}.sock",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let mut starter = TeardownStarter {
        // Opened/reclaim observes 64 KiB. Peer close confirms the drain,
        // but shutdown still observes 64 KiB and must retain the backend
        // until the next fresh observation also proves zero usage.
        usage: [64, 64, 0].into(),
        swapoff_calls: 0,
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
    .expect("confirmed fake swapoff and zero usage release the fake backend");
    assert_eq!(starter.swapoff_calls, 1);
    assert!(
        starter.usage.is_empty(),
        "all teardown observations were consumed"
    );
    assert!(
        !path.exists(),
        "teardown fixture cleans its temporary socket"
    );
}

#[test]
// TestName: daemon_nbd_shutdown_attempts_swapoff_when_usage_is_zero_or_absent
fn daemon_nbd_shutdown_attempts_swapoff_when_usage_is_zero_or_absent() {
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
            Ok((8 * GIB, 8 * GIB))
        }
    }

    struct AbsentSwapStarter {
        swapoff_calls: usize,
    }

    impl NbdRuntimeStarter for AbsentSwapStarter {
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
            jobs_tx.send(WMsg::Shutdown)?;
            Ok(())
        }

        fn nbd_used_kb(&mut self, _nbd_dev: &str) -> u64 {
            0
        }

        fn nbd_swap_is_explicitly_absent(&mut self, _nbd_dev: &str) -> bool {
            false
        }

        fn publish_demote(&mut self, _status: &ramshared_wsl2d::DemoteStatusFile) {}

        fn elapsed_us(&mut self, _started: Instant) -> u64 {
            0
        }

        fn spawn_swapoff(&mut self, _nbd_dev: &str) -> std::sync::mpsc::Receiver<SwapoffOutcome> {
            self.swapoff_calls += 1;
            let (tx, rx) = std::sync::mpsc::channel();
            tx.send(SwapoffOutcome::Drained)
                .expect("fake absent swapoff confirmation");
            rx
        }

        fn spawn_recovery_activation(
            &mut self,
            _nbd_dev: &str,
            _priority: i16,
        ) -> Result<std::sync::mpsc::Receiver<bool>, Box<dyn std::error::Error>> {
            panic!("direct shutdown must not activate swap")
        }

        fn teardown_retry_delay(&mut self) -> Duration {
            Duration::ZERO
        }
    }

    let path = std::env::temp_dir().join(format!(
        "ramshared-daemon-absent-swap-{}.sock",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let mut starter = AbsentSwapStarter { swapoff_calls: 0 };
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
    .expect("explicit absent-state swapoff confirmation releases the backend");
    assert_eq!(starter.swapoff_calls, 1, "zero usage bypassed swapoff");
    assert!(!path.exists(), "terminal cleanup leaked the socket");
}
