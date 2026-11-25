// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/shutdown.rs
//! Shutdown custody while a managed tier may still need backend I/O.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Instant;

use ramshared_wsl2d::swap::{DemoteRearm, SwapoffOutcome};
use ramshared_wsl2d::tier_release::{self, BackendShutdownLease};

use super::{
    DaemonAction, DaemonActionRunner, ExactSwapState, UblkDevice, UblkRuntime, UblkServer,
    prove_ublk_swap_absent,
};

/// Fatal release may address only the selected NBD action after the process
/// identity is acquired. Other transports have no bound fatal-release target.
pub(super) fn run_claimed_action<G>(
    action: DaemonAction,
    runner: &mut impl DaemonActionRunner,
    claim: impl FnOnce() -> Result<G, Box<dyn std::error::Error>>,
    arm: impl FnOnce(&str),
) -> Result<(), Box<dyn std::error::Error>> {
    let _identity = claim()?;
    if let DaemonAction::Nbd(args) = &action {
        arm(&args.nbd_dev);
    }
    let result = runner.execute(action);
    let _helpers = super::command_runtime::wait_for_helpers();
    result
}

/// Production can construct this only from the sole release owner's lease.
/// Test adapters must supply their isolated custody explicitly.
pub(super) struct ShutdownPermit {
    lease: Option<Arc<BackendShutdownLease<'static>>>,
    #[cfg(test)]
    _injected: Option<Box<dyn std::any::Any + Send>>,
}

impl ShutdownPermit {
    pub(super) fn claim(device: &str) -> Option<Self> {
        let helpers = super::command_runtime::try_claim_shutdown()?;
        tier_release::reconcile_release_custody();
        let lease =
            tier_release::try_claim_backend_shutdown_for(device)?.retain_shutdown_guard(helpers);
        Some(Self {
            lease: Some(Arc::new(lease)),
            #[cfg(test)]
            _injected: None,
        })
    }

    pub(super) fn shared_lease(&self) -> Option<Arc<BackendShutdownLease<'static>>> {
        self.lease.as_ref().map(Arc::clone)
    }

    #[cfg(test)]
    pub(super) fn injected(guard: impl std::any::Any + Send) -> Self {
        Self {
            lease: None,
            _injected: Some(Box::new(guard)),
        }
    }
}

/// Wait on one owned release while the separate ublk server keeps serving.
/// A refusal or unreadable table never unwinds the process owning that server.
pub(super) fn wait_for_ublk_release(runtime: &mut dyn UblkRuntime, device: &str) -> ShutdownPermit {
    let mut pending: Option<Receiver<SwapoffOutcome>> = None;
    let mut retry = DemoteRearm::new();
    let mut last_state = None;
    loop {
        if let Some(receiver) = pending.take() {
            match receiver.try_recv() {
                Ok(outcome) => {
                    retry.record(outcome, Instant::now());
                }
                Err(TryRecvError::Empty) => pending = Some(receiver),
                Err(TryRecvError::Disconnected) => {
                    retry.record(SwapoffOutcome::Rejected { exit_code: None }, Instant::now());
                }
            }
        }
        let state = match runtime.swap_state(device) {
            Ok(ExactSwapState::Absent) if pending.is_none() => {
                if let Some(permit) = runtime.try_claim_shutdown(device) {
                    return permit;
                }
                "release custody remains unresolved"
            }
            Ok(ExactSwapState::Active { .. }) if pending.is_none() => {
                if retry.may_attempt_at(Instant::now()) {
                    pending = Some(runtime.spawn_swapoff(device));
                }
                "managed swap remains active"
            }
            Ok(_) => "release remains pending",
            Err(_) => "managed swap observation is unavailable",
        };
        if last_state != Some(state) {
            eprintln!("[ramsharedd] ublk shutdown deferred: {state}; backend remains serving");
            last_state = Some(state);
        }
        runtime.wait_for_release_progress();
    }
}

/// Own the server until every destructive frontier is safe. Handoff and
/// STOP_DEV are idempotent; a refusal retries with a delay and the same lease.
/// No failed observation or stop may unwind the still-serving process.
pub(super) fn stop_ublk(
    runtime: &mut dyn UblkRuntime,
    device: UblkDevice,
    block_path: &str,
    mut server: Box<dyn UblkServer>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut failure = None;
    let permit = loop {
        let permit = wait_for_ublk_release(runtime, block_path);
        match prove_ublk_swap_absent(runtime, block_path) {
            Ok(()) => break permit,
            Err(error) => {
                eprintln!(
                    "[ramsharedd] ublk pre-stop observation refused: {error}; backend remains serving"
                );
                failure.get_or_insert(error);
                drop(permit);
                runtime.wait_for_release_progress();
            }
        }
    };
    while let Err(error) = server.authorize_shutdown(&permit) {
        eprintln!(
            "[ramsharedd] ublk shutdown handoff refused: {error}; retaining server and permit"
        );
        failure.get_or_insert(error.into());
        runtime.wait_for_release_progress();
    }
    while let Err(error) = runtime.stop_device(device) {
        eprintln!("[ramsharedd] ublk stop refused: {error}; retaining server and permit");
        failure.get_or_insert(error);
        runtime.wait_for_release_progress();
    }
    // join consumes the server and waits for both ring and worker destruction,
    // even on an error. The worker owns a clone of this same production lease.
    let joined = server.join();
    prove_ublk_swap_absent(runtime, block_path)?;
    runtime.delete_device(device)?;
    drop(permit);
    if let Err(error) = joined {
        failure.get_or_insert(error.into());
    }
    failure.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::super::{BackendKind, UblkDevice, UblkServer, run_ublk_with_runtime};
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{SyncSender, sync_channel};

    struct Guard {
        held: Arc<AtomicBool>,
        events: Arc<Mutex<Vec<&'static str>>>,
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            self.events
                .lock()
                .expect("fixture event lock")
                .push("permit-drop");
            self.held.store(false, Ordering::SeqCst);
        }
    }

    struct Server {
        held: Arc<AtomicBool>,
        events: Arc<Mutex<Vec<&'static str>>>,
        handoff_failures: usize,
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.events
                .lock()
                .expect("planned fixture operation succeeds")
                .push(if self.held.load(Ordering::SeqCst) {
                    "backend-drop"
                } else {
                    "backend-drop-without-permit"
                });
        }
    }

    impl UblkServer for Server {
        fn authorize_shutdown(&mut self, _: &ShutdownPermit) -> std::io::Result<()> {
            assert!(self.held.load(Ordering::SeqCst));
            self.events
                .lock()
                .expect("fixture event lock")
                .push("authorize");
            if self.handoff_failures > 0 {
                self.handoff_failures -= 1;
                Err(std::io::Error::other("injected handoff refusal"))
            } else {
                Ok(())
            }
        }

        fn join(self: Box<Self>) -> std::io::Result<()> {
            assert!(self.held.load(Ordering::SeqCst));
            self.events.lock().expect("fixture event lock").push("join");
            Ok(())
        }
    }

    struct Runtime {
        states: VecDeque<Result<ExactSwapState, &'static str>>,
        release: Option<Receiver<SwapoffOutcome>>,
        terminal: Option<SyncSender<SwapoffOutcome>>,
        terminal_at: usize,
        release_calls: usize,
        claim_calls: usize,
        denied_claims: usize,
        waits: usize,
        held: Arc<AtomicBool>,
        events: Arc<Mutex<Vec<&'static str>>>,
        handoff_failures: usize,
        stop_failures: usize,
    }

    impl Runtime {
        fn new(states: impl IntoIterator<Item = Result<ExactSwapState, &'static str>>) -> Self {
            Self {
                states: states.into_iter().collect(),
                release: None,
                terminal: None,
                terminal_at: 0,
                release_calls: 0,
                claim_calls: 0,
                denied_claims: 0,
                waits: 0,
                held: Arc::new(AtomicBool::new(false)),
                events: Arc::new(Mutex::new(Vec::new())),
                handoff_failures: 0,
                stop_failures: 0,
            }
        }
    }

    impl UblkRuntime for Runtime {
        fn guard_not_wsl2(&mut self) -> Result<(), Box<dyn std::error::Error>> {
            Ok(())
        }
        fn lock_memory(&mut self, _: bool, _: bool) -> Result<(), Box<dyn std::error::Error>> {
            Ok(())
        }
        fn install_shutdown_handler(&mut self) -> Result<(), Box<dyn std::error::Error>> {
            Ok(())
        }
        fn add_device(
            &mut self,
            queue_depth: u16,
        ) -> Result<UblkDevice, Box<dyn std::error::Error>> {
            Ok(UblkDevice {
                id: 42,
                queue_depth,
            })
        }
        fn set_params(&mut self, _: UblkDevice, _: u64) -> Result<(), Box<dyn std::error::Error>> {
            Ok(())
        }
        fn start_server(
            &mut self,
            _: BackendKind,
            _: &str,
            _: &str,
            _: u16,
            _: u64,
        ) -> Result<Box<dyn UblkServer>, Box<dyn std::error::Error>> {
            Ok(Box::new(Server {
                held: Arc::clone(&self.held),
                events: Arc::clone(&self.events),
                handoff_failures: self.handoff_failures,
            }))
        }
        fn start_device(&mut self, _: UblkDevice) -> Result<(), Box<dyn std::error::Error>> {
            Ok(())
        }
        fn wait_for_shutdown(&mut self) -> Result<(), Box<dyn std::error::Error>> {
            Ok(())
        }
        fn swap_state(&mut self, _: &str) -> Result<ExactSwapState, Box<dyn std::error::Error>> {
            self.states
                .pop_front()
                .expect("planned observation")
                .map_err(Into::into)
        }
        fn spawn_swapoff(&mut self, _: &str) -> Receiver<SwapoffOutcome> {
            self.release_calls += 1;
            self.release.take().expect("only one owned release")
        }
        fn try_claim_shutdown(&mut self, _: &str) -> Option<ShutdownPermit> {
            self.claim_calls += 1;
            if self.denied_claims > 0 {
                self.denied_claims -= 1;
                return None;
            }
            assert!(!self.held.swap(true, Ordering::SeqCst));
            self.events
                .lock()
                .expect("fixture event lock")
                .push("claim");
            Some(ShutdownPermit::injected(Guard {
                held: Arc::clone(&self.held),
                events: Arc::clone(&self.events),
            }))
        }
        fn wait_for_release_progress(&mut self) {
            self.waits += 1;
            assert!(self.waits < 16, "coordinator must make planned progress");
            if self.waits == self.terminal_at {
                self.terminal
                    .take()
                    .expect("planned fixture operation succeeds")
                    .send(SwapoffOutcome::Drained)
                    .expect("planned fixture operation succeeds");
            }
        }
        fn stop_device(&mut self, _: UblkDevice) -> Result<(), Box<dyn std::error::Error>> {
            assert!(self.held.load(Ordering::SeqCst));
            self.events.lock().expect("fixture event lock").push("stop");
            if self.stop_failures > 0 {
                self.stop_failures -= 1;
                Err("injected stop refusal".into())
            } else {
                Ok(())
            }
        }
        fn delete_device(&mut self, _: UblkDevice) -> Result<(), Box<dyn std::error::Error>> {
            assert!(self.held.load(Ordering::SeqCst));
            self.events
                .lock()
                .expect("fixture event lock")
                .push("delete");
            Ok(())
        }
    }

    #[test]
    fn shutdown_wait_keeps_one_flight_until_terminal() {
        let mut runtime = Runtime::new([
            Ok(ExactSwapState::Active { used_kb: 8 }),
            Ok(ExactSwapState::Absent),
            Ok(ExactSwapState::Absent),
            Ok(ExactSwapState::Absent),
        ]);
        let (tx, rx) = sync_channel(1);
        runtime.release = Some(rx);
        runtime.terminal = Some(tx);
        runtime.terminal_at = 2;
        runtime.denied_claims = 1;
        let permit = wait_for_ublk_release(&mut runtime, "/dev/ublkb42");
        assert_eq!(
            (runtime.release_calls, runtime.claim_calls, runtime.waits),
            (1, 2, 3)
        );
        assert!(
            permit.shared_lease().is_none(),
            "fixture custody is explicit"
        );
        assert!(runtime.held.load(Ordering::SeqCst));
        drop(permit);
        assert!(!runtime.held.load(Ordering::SeqCst));
    }

    #[test]
    fn shutdown_wait_survives_disconnection_and_unknown_observation() {
        let mut runtime = Runtime::new([
            Ok(ExactSwapState::Active { used_kb: 0 }),
            Err("unreadable table"),
            Ok(ExactSwapState::Active { used_kb: 0 }),
            Ok(ExactSwapState::Absent),
        ]);
        let (tx, rx) = sync_channel(1);
        drop(tx);
        runtime.release = Some(rx);
        let permit = wait_for_ublk_release(&mut runtime, "/dev/ublkb42");
        assert_eq!(
            (runtime.release_calls, runtime.claim_calls, runtime.waits),
            (1, 1, 3)
        );
        drop(permit);
    }

    #[test]
    fn shutdown_permit_outlives_backend_drop() {
        let mut runtime = Runtime::new([Ok(ExactSwapState::Absent); 3]);
        run_ublk_with_runtime(4096, false, 1, BackendKind::Ram, &mut runtime)
            .expect("planned fixture operation succeeds");
        assert_eq!(
            *runtime.events.lock().expect("fixture event lock"),
            [
                "claim",
                "authorize",
                "stop",
                "join",
                "backend-drop",
                "delete",
                "permit-drop",
            ]
        );
        assert!(!runtime.held.load(Ordering::SeqCst));
    }

    #[test]
    fn ublk_shutdown_preserves_server_across_final_frontier_failures() {
        let mut runtime = Runtime::new([
            Ok(ExactSwapState::Absent),
            Err("pre-stop observation unavailable"),
            Ok(ExactSwapState::Absent),
            Ok(ExactSwapState::Absent),
            Ok(ExactSwapState::Absent),
        ]);
        runtime.handoff_failures = 1;
        runtime.stop_failures = 1;
        let result = run_ublk_with_runtime(4096, false, 1, BackendKind::Ram, &mut runtime);
        assert!(
            result
                .expect_err("original refusal is reported after cleanup")
                .to_string()
                .contains("pre-stop observation unavailable"),
            "the original frontier error is reported only after safe cleanup"
        );
        assert_eq!(
            *runtime.events.lock().expect("fixture event lock"),
            [
                "claim",
                "permit-drop",
                "claim",
                "authorize",
                "authorize",
                "stop",
                "stop",
                "join",
                "backend-drop",
                "delete",
                "permit-drop",
            ]
        );
    }

    #[test]
    fn shutdown_production_permit_holds_the_sole_owner_until_last_arc() {
        assert!(ShutdownPermit::claim("relative-target").is_none());
        let permit = ShutdownPermit::claim("/dev/ramshared-absent-shutdown-fixture")
            .expect("read-only exact absence permits isolated shutdown custody");
        let worker = permit.shared_lease().expect("production has a real lease");
        assert!(tier_release::try_begin_release("/dev/nbd98765").is_none());
        drop(permit);
        assert!(tier_release::try_begin_release("/dev/nbd98765").is_none());
        drop(worker);
        let flight = tier_release::try_begin_release("/dev/nbd98765")
            .expect("last lease drop reopens owner");
        drop(flight);
    }

    #[test]
    fn watchdog_arms_only_claimed_nbd_action() {
        use super::super::{AppArgs, DaemonAction, DaemonActionRunner};
        struct Runner(Arc<Mutex<Vec<String>>>);
        impl DaemonActionRunner for Runner {
            fn execute(&mut self, _: DaemonAction) -> Result<(), Box<dyn std::error::Error>> {
                self.0
                    .lock()
                    .expect("fixture event lock")
                    .push("execute".into());
                Ok(())
            }
        }
        struct Claimed(Arc<Mutex<Vec<String>>>);
        impl Drop for Claimed {
            fn drop(&mut self) {
                self.0
                    .lock()
                    .expect("fixture event lock")
                    .push("unclaim".into());
            }
        }
        for (kind, permitted) in [("nbd", true), ("ublk", false), ("broker", false)] {
            let mut args = AppArgs::parse_from(&["ramsharedd".into()])
                .expect("planned fixture operation succeeds");
            args.nbd_dev = "/dev/nbd17".into();
            let action = match kind {
                "nbd" => DaemonAction::Nbd(args),
                "ublk" => DaemonAction::Ublk(args),
                _ => DaemonAction::Broker(args),
            };
            let events = Arc::new(Mutex::new(Vec::<String>::new()));
            let mut runner = Runner(Arc::clone(&events));
            run_claimed_action(
                action,
                &mut runner,
                || {
                    events
                        .lock()
                        .expect("fixture event lock")
                        .push("claim".into());
                    Ok(Claimed(Arc::clone(&events)))
                },
                |target| {
                    events
                        .lock()
                        .expect("fixture event lock")
                        .push(format!("arm:{target}"));
                },
            )
            .expect("planned fixture operation succeeds");
            let expected = if permitted {
                vec!["claim", "arm:/dev/nbd17", "execute", "unclaim"]
            } else {
                vec!["claim", "execute", "unclaim"]
            };
            assert_eq!(
                *events.lock().expect("fixture event lock"),
                expected,
                "transport {kind}"
            );
        }
        let args = AppArgs::parse_from(&["ramsharedd".into()])
            .expect("planned fixture operation succeeds");
        let events = Arc::new(Mutex::new(Vec::<String>::new()));
        let mut runner = Runner(Arc::clone(&events));
        let result = run_claimed_action(
            DaemonAction::Nbd(args),
            &mut runner,
            || Err::<(), Box<dyn std::error::Error>>("identity already owned".into()),
            |_| panic!("failed PID ownership must never arm a release watchdog"),
        );
        assert!(result.is_err());
        assert!(
            events.lock().expect("fixture event lock").is_empty(),
            "failed claim must not execute"
        );
    }
}
