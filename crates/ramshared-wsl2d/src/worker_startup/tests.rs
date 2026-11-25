// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/worker_startup/tests.rs
//! Actual child lifetime and deterministic startup admission/refusal tests.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::time::Instant;

fn failed_task() -> StartTask {
    Box::new(|| StartOutcome {
        result: Err("fixture start refusal".into()),
        creator_child: None,
    })
}
fn wait_for(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "fixture condition did not complete"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn wait_result(startup: &mut Startup) -> Result<StartedWorker, String> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match startup.poll() {
            Ok(Some(result)) => return Ok(result),
            Err(error) => return Err(error),
            Ok(None) => assert!(Instant::now() < deadline, "startup result did not arrive"),
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn startup_slot_refuses_overlap_and_releases_only_finished_tasks() {
    let owner = Arc::new(StartupCustody::default());
    let (entered_tx, entered_rx) = sync_channel(1);
    let (release_tx, release_rx) = sync_channel(1);
    let mut startup = owner
        .begin(Box::new(move || {
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            failed_task()()
        }))
        .unwrap();
    entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(startup.poll().unwrap().is_none());
    assert!(
        matches!(owner.begin(failed_task()), Err(error) if error.kind()==io::ErrorKind::WouldBlock)
    );
    assert!(!startup.is_quiescent());
    release_tx.send(()).unwrap();
    assert_eq!(
        wait_result(&mut startup).err().unwrap(),
        "fixture start refusal"
    );
    wait_for(|| startup.is_quiescent());
    let mut next = owner.begin(failed_task()).unwrap();
    assert_eq!(
        wait_result(&mut next).err().unwrap(),
        "fixture start refusal"
    );
    wait_for(|| next.is_quiescent());
}

#[test]
fn startup_creation_failure_releases_empty_slot_and_busy_lock_never_waits() {
    let owner = Arc::new(StartupCustody::default());
    let guard = owner.0.lock().unwrap();
    assert!(
        matches!(owner.begin(failed_task()), Err(error) if error.kind()==io::ErrorKind::WouldBlock)
    );
    drop(guard);
    assert!(
        owner
            .begin_with(failed_task(), |_| Err(io::Error::other(
                "thread creation refused"
            )))
            .is_err()
    );
    let mut startup = owner.begin(failed_task()).unwrap();
    assert!(wait_result(&mut startup).is_err());
    wait_for(|| startup.is_quiescent());
}

#[test]
fn startup_thread_panic_latches_unknown_outcome_without_replacement() {
    let owner = Arc::new(StartupCustody::default());
    let mut startup = owner
        .begin(Box::new(|| panic!("injected startup panic")))
        .unwrap();
    assert!(
        wait_result(&mut startup)
            .err()
            .unwrap()
            .contains("disconnected")
    );
    wait_for(
        || matches!(&*owner.0.lock().unwrap(), State::Running(thread) if thread.is_finished()),
    );
    assert!(owner.begin(failed_task()).is_err());
    assert!(!startup.is_quiescent());
    assert!(owner.begin(failed_task()).is_err());
}

#[test]
fn cancelled_unstarted_task_does_not_launch_and_poisoned_custody_refuses() {
    let owner = Arc::new(StartupCustody::default());
    let called = Arc::new(AtomicBool::new(false));
    let spy = Arc::clone(&called);
    let (release_tx, release_rx) = sync_channel(1);
    let mut startup = owner
        .begin_with(
            Box::new(move || {
                spy.store(true, Ordering::Release);
                failed_task()()
            }),
            |task| {
                std::thread::Builder::new().spawn(move || {
                    release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
                    task();
                })
            },
        )
        .unwrap();
    startup.cancel();
    assert!(startup.is_cancelled());
    assert!(startup.poll().is_err());
    release_tx.send(()).unwrap();
    wait_for(|| startup.is_quiescent());
    assert!(!called.load(Ordering::Acquire));
    let poisoned = Arc::clone(&owner);
    assert!(
        std::thread::spawn(move || {
            let _guard = poisoned.0.lock().unwrap();
            panic!("inject custody poison");
        })
        .join()
        .is_err()
    );
    assert!(owner.begin(failed_task()).is_err());
    assert!(!startup.is_quiescent());
}

struct ChildControl {
    input: Option<std::process::ChildStdin>,
    process: ChildWorkerProcess,
}

fn child_is_running(mut observe: impl FnMut() -> io::Result<bool>) -> io::Result<bool> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match observe() {
            Ok(exited) => return Ok(!exited),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) => return Err(error),
        }
    }
}
impl Drop for ChildControl {
    fn drop(&mut self) {
        // This fixture can always release its own blocked shell without a
        // signal or abandoning an unconfirmed process on a failed assertion.
        drop(self.input.take());
        wait_for(|| matches!(self.process.try_wait(), Ok(true)));
    }
}

fn child_task(control: SyncSender<ChildControl>, fail_handshake: bool) -> StartTask {
    Box::new(move || {
        let owner = Arc::new(super::super::worker_custody::WorkerCustody::default());
        let reservation = owner.reserve().unwrap();
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "read line"]).stdin(Stdio::piped());
        // SAFETY: the post-fork callback makes one pointer-free prctl call,
        // uses only scalar arguments and errno on failure, and touches no
        // inherited Rust locks or allocation. The kernel binds the signal to
        // this exact creating thread. No callback capture outlives its owner.
        unsafe {
            command.pre_exec(|| {
                if super::super::prctl(1, 15, 0, 0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let input = child.stdin.take().unwrap();
        let process = ChildWorkerProcess::from_resources(reservation.bind(child));
        control
            .send(ChildControl {
                input: Some(input),
                process: process.clone(),
            })
            .unwrap();
        if fail_handshake {
            return StartOutcome {
                result: Err("fixture handshake refused after spawn".into()),
                creator_child: Some(process),
            };
        }
        let (socket, peer) = UnixStream::pair().unwrap();
        drop(peer);
        StartOutcome {
            result: Ok((
                IpcCacheClient::new(socket, Duration::from_millis(50), 4096),
                IsolatedWorkerSupervisor {
                    child: Some(super::super::WorkerChildHandle::Process(process.clone())),
                },
            )),
            creator_child: Some(process),
        }
    })
}

#[test]
fn creator_thread_survives_handoff_until_actual_child_exit() {
    let owner = Arc::new(StartupCustody::default());
    let (control_tx, control_rx) = sync_channel(1);
    let mut startup = owner.begin(child_task(control_tx, false)).unwrap();
    let mut control = control_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let (client, mut supervisor) = wait_result(&mut startup).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        child_is_running(|| control.process.try_wait())
            .expect("native child state could not be confirmed"),
        "creator exited and killed the handed-off child"
    );
    assert!(!startup.is_quiescent());
    assert!(
        matches!(owner.begin(failed_task()), Err(error) if error.kind()==io::ErrorKind::WouldBlock)
    );
    drop(control.input.take());
    wait_for(|| startup.is_quiescent());
    assert!(supervisor.shutdown());
    drop(client);
    let mut next = owner.begin(failed_task()).unwrap();
    assert!(wait_result(&mut next).is_err());
    wait_for(|| next.is_quiescent());
}

#[test]
fn busy_native_observation_is_not_creator_death() {
    let owner = Arc::new(StartupCustody::default());
    let (control_tx, control_rx) = sync_channel(1);
    let mut startup = owner.begin(child_task(control_tx, false)).unwrap();
    let mut control = control_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let (client, mut supervisor) = wait_result(&mut startup).unwrap();
    let native = Arc::clone(&control.process.child);
    let mut held = Some(native.lock().unwrap());
    let mut calls = 0;
    let running = child_is_running(|| {
        calls += 1;
        if calls == 2 {
            drop(held.take());
        }
        control.process.try_wait()
    })
    .unwrap();
    drop(held);
    assert!(
        running,
        "busy native observation was mistaken for creator death"
    );
    assert!(calls >= 2, "busy observation was not retried");
    drop(control.input.take());
    wait_for(|| startup.is_quiescent());
    assert!(supervisor.shutdown());
    drop(client);
}

#[test]
fn failed_handshake_retains_creator_and_stop_custody_until_exit() {
    let owner = Arc::new(StartupCustody::default());
    let (control_tx, control_rx) = sync_channel(1);
    let mut startup = owner.begin(child_task(control_tx, true)).unwrap();
    let mut control = control_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(
        wait_result(&mut startup)
            .err()
            .unwrap()
            .contains("handshake refused")
    );
    assert!(!startup.is_quiescent());
    assert!(owner.begin(failed_task()).is_err());
    drop(control.input.take());
    wait_for(|| startup.is_quiescent());
}

#[test]
fn dropped_result_receiver_retains_creator_until_owned_child_exits() {
    let owner = Arc::new(StartupCustody::default());
    let (control_tx, control_rx) = sync_channel(1);
    let startup = owner.begin(child_task(control_tx, false)).unwrap();
    let mut control = control_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    drop(startup);
    assert!(owner.begin(failed_task()).is_err());
    drop(control.input.take());
    wait_for(|| {
        let mut state = owner.0.try_lock().unwrap();
        reap_finished(&mut state).is_ok()
    });
}

#[test]
fn failed_production_child_reports_eof_and_retains_exact_process() {
    // The current executable is a libtest binary, so it refuses the real
    // internal worker argv before drivers. Exercise the production launcher,
    // inherited stdin, handshake EOF and failure retention without a GPU.
    for expected_adapter in [
        None,
        Some(super::super::GpuAdapterIdentity {
            node_mask: None,
            backend: "cuda".into(),
            key: "00112233445566778899aabbccddeeff".into(),
            luid: None,
        }),
    ] {
        let owner = Arc::new(StartupCustody::default());
        let (child_tx, child_rx) = sync_channel(1);
        let mut startup = owner
            .begin(Box::new(move || {
                let outcome =
                    super::super::worker_launcher::launch(&super::super::WorkerSpawnParams {
                        generation: None,
                        target_bytes: 4096,
                        chunk_bytes: 4096,
                        reserve_floor: 0,
                        expected_adapter,
                        host_catalog: None,
                    });
                child_tx.send(outcome.creator_child.clone()).unwrap();
                outcome
            }))
            .unwrap();
        let error = wait_result(&mut startup).err().unwrap();
        assert!(
            error.contains("worker handshake failed"),
            "unexpected launcher refusal: {error}"
        );
        let mut child = child_rx
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .expect("real child must have been created");
        wait_for(|| startup.is_quiescent());
        assert!(
            matches!(child.try_wait(), Ok(true)),
            "failure discarded an unconfirmed child"
        );
    }
}

#[test]
fn revoked_supervisor_refuses_start_and_discards_late_worker() {
    let owner = Arc::new(StartupCustody::default());
    let allowed = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let gate = Arc::clone(&allowed);
    let mut refused = owner
        .begin_guarded(
            Box::new(move || {
                observed.fetch_add(1, Ordering::AcqRel);
                failed_task()()
            }),
            Arc::new(move || gate.load(Ordering::Acquire)),
        )
        .unwrap();
    assert!(wait_result(&mut refused).is_err());
    wait_for(|| refused.is_quiescent());
    assert_eq!(calls.load(Ordering::Acquire), 0);

    allowed.store(true, Ordering::Release);
    let gate = Arc::clone(&allowed);
    let (control_tx, control_rx) = sync_channel(1);
    let (ready_tx, ready_rx) = sync_channel(1);
    let (release_tx, release_rx) = sync_channel(1);
    let task = child_task(control_tx, false);
    let mut startup = owner
        .begin_guarded(
            Box::new(move || {
                let outcome = task();
                ready_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
                outcome
            }),
            Arc::new(move || gate.load(Ordering::Acquire)),
        )
        .unwrap();
    let mut control = control_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    allowed.store(false, Ordering::Release);
    release_tx.send(()).unwrap();
    assert!(
        wait_result(&mut startup).is_err(),
        "revoked supervisor cannot deliver a worker"
    );
    assert!(
        !startup.is_quiescent(),
        "custody must persist while the child is alive"
    );
    drop(control.input.take());
    wait_for(|| startup.is_quiescent());
    drop(control);
}

#[test]
fn authority_lost_after_worker_creation_refuses_queued_handoff() {
    let owner = Arc::new(StartupCustody::default());
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let (control_tx, control_rx) = sync_channel(1);
    let mut startup = owner
        .begin_guarded(
            child_task(control_tx, false),
            Arc::new(move || observed.fetch_add(1, Ordering::AcqRel) < 2),
        )
        .unwrap();
    let mut control = control_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    wait_for(|| calls.load(Ordering::Acquire) >= 2);
    assert!(
        startup.poll().is_err(),
        "revoked authority delivered the queued worker"
    );
    assert!(!startup.is_quiescent());
    drop(control.input.take());
    wait_for(|| startup.is_quiescent());
}
