// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main_tests/nbd_budget_and_recovery.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;

#[test]
fn daemon_nbd_failed_swapoff_keeps_backend_until_teardown_confirmation() {
    struct Provider;

    impl VramProvider for Provider {
        type Mem<'a> = TestMemory;

        fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, ramshared_vram::VramError> {
            assert_eq!(bytes, CANARY_BYTES);
            Ok(TestMemory::new(bytes))
        }

        fn mem_info(&self) -> Result<(u64, u64), ramshared_vram::VramError> {
            Ok((8 * 1024 * 1024 * 1024, 8 * 1024 * 1024 * 1024))
        }
    }

    struct ConstrainedThenHealthyBudget(std::cell::Cell<usize>);

    impl NbdBudgetProvider for ConstrainedThenHealthyBudget {
        fn snapshot(&self) -> Result<NbdBudgetSnapshot, String> {
            let sample = self.0.get();
            self.0.set(sample + 1);
            Ok(NbdBudgetSnapshot {
                budget: if sample == 0 {
                    0
                } else {
                    4 * 1024 * 1024 * 1024
                },
                current_usage: 0,
                sampled_at: Instant::now(),
            })
        }
    }

    struct Starter {
        budget: Option<Box<dyn NbdBudgetProvider>>,
        swapoff_results: std::collections::VecDeque<SwapoffOutcome>,
        swap_is_absent: bool,
        statuses: Vec<ramshared_wsl2d::DemoteStatusFile>,
        replies: std::sync::mpsc::Receiver<Reply>,
    }

    impl NbdRuntimeStarter for Starter {
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
            jobs_tx.send(WMsg::Job(ramshared_wsl2d::conn::Job {
                export: 0,
                req: ramshared_block::Request {
                    flags: 0,
                    cmd: Command::Write,
                    handle: 17,
                    offset: 0,
                    len: BLOCK_SIZE,
                },
                payload: vec![0x7B; BLOCK_SIZE as usize],
                credit: fixture_credit(),
                reply: reply_tx,
            }))?;
            jobs_tx.send(WMsg::Shutdown)?;
            self.replies = reply_rx;
            Ok(())
        }

        fn nbd_used_kb(&mut self, _nbd_dev: &str) -> u64 {
            0
        }

        fn nbd_swap_is_explicitly_absent(&mut self, _nbd_dev: &str) -> bool {
            self.swap_is_absent
        }

        fn publish_demote(&mut self, status: &ramshared_wsl2d::DemoteStatusFile) {
            self.statuses.push(status.clone());
        }

        fn elapsed_us(&mut self, _started: Instant) -> u64 {
            10
        }

        fn spawn_swapoff(&mut self, _nbd_dev: &str) -> std::sync::mpsc::Receiver<SwapoffOutcome> {
            let result = self
                .swapoff_results
                .pop_front()
                .expect("each requested swapoff has one injected result");
            self.swap_is_absent = result == SwapoffOutcome::Drained;
            let (tx, rx) = std::sync::mpsc::channel();
            tx.send(result).expect("in-memory swapoff receiver is live");
            rx
        }

        fn spawn_recovery_activation(
            &mut self,
            _nbd_dev: &str,
            _priority: i16,
        ) -> Result<std::sync::mpsc::Receiver<bool>, Box<dyn std::error::Error>> {
            panic!("a failed swapoff must not start recovery activation")
        }

        fn startup_budget(
            &mut self,
            requested: bool,
        ) -> Result<Option<Box<dyn NbdBudgetProvider>>, Box<dyn std::error::Error>> {
            assert!(requested);
            Ok(self.budget.take())
        }

        fn global_free_bytes(&mut self, _timeout: Duration) -> Option<u64> {
            Some(8 * 1024 * 1024 * 1024)
        }

        fn teardown_retry_delay(&mut self) -> Duration {
            Duration::ZERO
        }
    }

    let path = std::env::temp_dir().join(format!(
        "ramshared-daemon-swapoff-refusal-{}.sock",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let (_reply_tx, reply_rx) = std::sync::mpsc::channel();
    let mut starter = Starter {
        budget: Some(Box::new(ConstrainedThenHealthyBudget(
            std::cell::Cell::new(0),
        ))),
        swapoff_results: [
            SwapoffOutcome::Rejected { exit_code: Some(1) },
            SwapoffOutcome::Drained,
        ]
        .into(),
        swap_is_absent: false,
        statuses: Vec::new(),
        replies: reply_rx,
    };

    run_nbd_with_startup(
        Provider,
        None,
        4096,
        path.to_string_lossy().into_owned(),
        false,
        "/dev/ramshared-test-nbd".into(),
        true,
        serve_pool::DEFAULT_SERVE_THREADS,
        &mut starter,
    )
    .expect("teardown releases the backend only after a later swapoff confirmation");

    let reply = starter
        .replies
        .recv_timeout(Duration::from_secs(1))
        .expect("the harmless flush job receives a bounded reply");
    assert_eq!(
        u32::from_be_bytes(reply.reply[4..8].try_into().expect("NBD errno width")),
        ramshared_block::protocol::NBD_EIO,
        "the exhausted injected budget refuses the sparse write"
    );
    assert_eq!(
        starter.statuses,
        vec![
            ramshared_wsl2d::DemoteStatusFile::new(0, None, false),
            ramshared_wsl2d::DemoteStatusFile::new(0, Some("WddmBudget".into()), true),
            ramshared_wsl2d::DemoteStatusFile::new(0, Some("WddmBudget".into()), false),
            ramshared_wsl2d::DemoteStatusFile::new(0, Some("Shutdown".into()), true),
            ramshared_wsl2d::DemoteStatusFile::new(1, Some("Shutdown".into()), false),
        ]
    );
    assert!(starter.swapoff_results.is_empty());
    assert!(starter.swap_is_absent);
    assert!(!path.exists(), "the fixture removes its temporary socket");
}

#[test]
fn daemon_nbd_budget_poll_uses_injected_wddm_snapshot_and_global_probe() {
    struct BudgetProvider;

    impl VramProvider for BudgetProvider {
        type Mem<'a>
            = TestMemory
        where
            Self: 'a;

        fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, ramshared_vram::VramError> {
            assert_eq!(
                bytes, CANARY_BYTES,
                "poll fixture must not allocate sparse chunks"
            );
            Ok(TestMemory::new(bytes))
        }

        fn mem_info(&self) -> Result<(u64, u64), ramshared_vram::VramError> {
            Ok((8 * 1024 * 1024 * 1024, 8 * 1024 * 1024 * 1024))
        }
    }

    struct FreshBudget(std::rc::Rc<std::cell::Cell<usize>>);

    impl NbdBudgetProvider for FreshBudget {
        fn snapshot(&self) -> Result<NbdBudgetSnapshot, String> {
            self.0.set(self.0.get() + 1);
            Ok(NbdBudgetSnapshot {
                budget: 4 * 1024 * 1024 * 1024,
                current_usage: 0,
                sampled_at: Instant::now(),
            })
        }
    }

    struct BudgetStarter {
        budget: Option<Box<dyn NbdBudgetProvider>>,
        budget_calls: std::rc::Rc<std::cell::Cell<usize>>,
        global_calls: usize,
    }

    impl NbdRuntimeStarter for BudgetStarter {
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
            let (reply_tx, _reply_rx) = std::sync::mpsc::channel();
            jobs_tx.send(WMsg::Opened)?;
            jobs_tx.send(WMsg::Job(ramshared_wsl2d::conn::Job {
                export: 0,
                req: ramshared_block::Request {
                    flags: 0,
                    cmd: Command::Flush,
                    handle: 7,
                    offset: 0,
                    len: 0,
                },
                payload: Vec::new(),
                credit: fixture_credit(),
                reply: reply_tx,
            }))?;
            jobs_tx.send(WMsg::Closed)?;
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

        fn elapsed_us(&mut self, _started: Instant) -> u64 {
            10
        }

        fn spawn_swapoff(&mut self, _nbd_dev: &str) -> std::sync::mpsc::Receiver<SwapoffOutcome> {
            panic!("healthy fresh budget must not request swapoff")
        }

        fn spawn_recovery_activation(
            &mut self,
            _nbd_dev: &str,
            _priority: i16,
        ) -> Result<std::sync::mpsc::Receiver<bool>, Box<dyn std::error::Error>> {
            panic!("healthy fresh budget must not activate swap")
        }

        fn startup_budget(
            &mut self,
            requested: bool,
        ) -> Result<Option<Box<dyn NbdBudgetProvider>>, Box<dyn std::error::Error>> {
            assert!(requested, "test requests the WDDM budget path");
            Ok(self.budget.take())
        }

        fn global_free_bytes(&mut self, _timeout: Duration) -> Option<u64> {
            self.global_calls += 1;
            Some(8 * 1024 * 1024 * 1024)
        }
    }

    let path = std::env::temp_dir().join(format!(
        "ramshared-daemon-budget-poll-{}.sock",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let calls = std::rc::Rc::new(std::cell::Cell::new(0));
    let mut starter = BudgetStarter {
        budget: Some(Box::new(FreshBudget(std::rc::Rc::clone(&calls)))),
        budget_calls: std::rc::Rc::clone(&calls),
        global_calls: 0,
    };
    run_nbd_with_startup(
        BudgetProvider,
        None,
        4096,
        path.to_string_lossy().into_owned(),
        false,
        "/dev/ramshared-test-nbd".into(),
        true,
        serve_pool::DEFAULT_SERVE_THREADS,
        &mut starter,
    )
    .expect("fresh injected budget keeps sparse daemon available");
    assert!(
        starter.budget_calls.get() >= 1,
        "budget snapshot was polled"
    );
    assert!(
        starter.global_calls >= 1,
        "global free probe was bounded and polled"
    );
    assert!(
        !path.exists(),
        "budget poll fixture cleans its temporary socket"
    );
}

#[test]
fn daemon_nbd_budget_constraint_demotes_then_recovers_with_fake_swap() {
    struct BudgetProvider;

    impl VramProvider for BudgetProvider {
        type Mem<'a>
            = TestMemory
        where
            Self: 'a;

        fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, ramshared_vram::VramError> {
            assert_eq!(
                bytes, CANARY_BYTES,
                "recovery fixture has no sparse allocation"
            );
            Ok(TestMemory::new(bytes))
        }

        fn mem_info(&self) -> Result<(u64, u64), ramshared_vram::VramError> {
            Ok((8 * 1024 * 1024 * 1024, 8 * 1024 * 1024 * 1024))
        }
    }

    struct SequenceBudget(
        std::cell::RefCell<std::collections::VecDeque<Result<NbdBudgetSnapshot, String>>>,
    );

    impl NbdBudgetProvider for SequenceBudget {
        fn snapshot(&self) -> Result<NbdBudgetSnapshot, String> {
            self.0
                .borrow_mut()
                .pop_front()
                .expect("one budget sample per injected worker tick")
        }
    }

    struct RecoveryStarter {
        budget: Option<Box<dyn NbdBudgetProvider>>,
        swapoff_calls: usize,
        peer_closed_calls: usize,
        activate_calls: usize,
        statuses: Vec<ramshared_wsl2d::DemoteStatusFile>,
    }

    impl NbdRuntimeStarter for RecoveryStarter {
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
            let (reply_tx, _reply_rx) = std::sync::mpsc::channel();
            jobs_tx.send(WMsg::Opened)?;
            for handle in 0..3 {
                jobs_tx.send(WMsg::Job(ramshared_wsl2d::conn::Job {
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
                    reply: reply_tx.clone(),
                }))?;
            }
            jobs_tx.send(WMsg::Closed)?;
            jobs_tx.send(WMsg::Shutdown)?;
            Ok(())
        }

        fn nbd_used_kb(&mut self, _nbd_dev: &str) -> u64 {
            0
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
                .expect("in-memory swapoff completion");
            rx
        }

        fn spawn_peer_closed_swapoff(
            &mut self,
            nbd_dev: &str,
        ) -> std::sync::mpsc::Receiver<SwapoffOutcome> {
            self.peer_closed_calls += 1;
            self.spawn_swapoff(nbd_dev)
        }

        fn spawn_recovery_activation(
            &mut self,
            _nbd_dev: &str,
            priority: i16,
        ) -> Result<std::sync::mpsc::Receiver<bool>, Box<dyn std::error::Error>> {
            assert_eq!(priority, 100);
            self.activate_calls += 1;
            let (tx, rx) = std::sync::mpsc::channel();
            tx.send(true).expect("in-memory recovery completion");
            Ok(rx)
        }

        fn startup_budget(
            &mut self,
            requested: bool,
        ) -> Result<Option<Box<dyn NbdBudgetProvider>>, Box<dyn std::error::Error>> {
            assert!(requested);
            Ok(self.budget.take())
        }

        fn global_free_bytes(&mut self, _timeout: Duration) -> Option<u64> {
            Some(8 * 1024 * 1024 * 1024)
        }
    }

    let healthy = || NbdBudgetSnapshot {
        budget: 4 * 1024 * 1024 * 1024,
        current_usage: 0,
        sampled_at: Instant::now(),
    };
    let path = std::env::temp_dir().join(format!(
        "ramshared-daemon-budget-recovery-{}.sock",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let mut starter = RecoveryStarter {
        budget: Some(Box::new(SequenceBudget(std::cell::RefCell::new(
            [
                Err("injected stale WDDM sample".into()),
                Ok(healthy()),
                Ok(healthy()),
                Ok(healthy()),
            ]
            .into(),
        )))),
        swapoff_calls: 0,
        peer_closed_calls: 0,
        activate_calls: 0,
        statuses: Vec::new(),
    };
    run_nbd_with_startup(
        BudgetProvider,
        None,
        4096,
        path.to_string_lossy().into_owned(),
        false,
        "/dev/ramshared-test-nbd".into(),
        true,
        serve_pool::DEFAULT_SERVE_THREADS,
        &mut starter,
    )
    .expect("healthy hysteresis recovers the fake sparse daemon");
    assert_eq!(
        starter.swapoff_calls, 2,
        "the recovered fake swap must receive a second, confirmed teardown swapoff"
    );
    assert_eq!(starter.activate_calls, 1);
    assert_eq!(starter.peer_closed_calls, 1);
    assert_eq!(
        starter.statuses,
        vec![
            ramshared_wsl2d::DemoteStatusFile::new(0, None, false),
            ramshared_wsl2d::DemoteStatusFile::new(0, Some("WddmBudgetPoll".into()), true),
            ramshared_wsl2d::DemoteStatusFile::new(1, Some("WddmBudgetPoll".into()), false),
            ramshared_wsl2d::DemoteStatusFile::new(1, Some("PeerClosed".into()), true),
            ramshared_wsl2d::DemoteStatusFile::new(2, Some("PeerClosed".into()), false),
        ]
    );
    assert!(
        !path.exists(),
        "recovery fixture cleans its temporary socket"
    );
}
