// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main_tests/worker_observation.rs
//! Native child observation while another owner holds the child lock.

use super::*;
use std::sync::mpsc;

#[test]
fn gpu_worker_initial_stop_keeps_origin_io_serving() {
    let mut child = ProcessCommand::new("/bin/sh")
        .args(["-c", "read line"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let input = child.stdin.take().unwrap();
    let owner = Arc::new(worker_custody::WorkerCustody::default());
    let resources = owner.reserve().unwrap().bind(child);
    let process = ChildWorkerProcess::from_resources(resources.clone());
    let supervisor = IsolatedWorkerSupervisor {
        child: Some(WorkerChildHandle::Process(process)),
    };
    let runtime = IsolatedWorkerRuntime {
        cache_member: 0,
        startup: None,
        supervisor: Some(supervisor),
        params: WorkerSpawnParams {
            generation: None,
            target_bytes: 4096,
            chunk_bytes: 4096,
            reserve_floor: 0,
            expected_adapter: None,
            host_catalog: None,
        },
        budget: WorkerRespawnBudget::new(),
        unavailable_reason: None,
        lease_lost_release: false,
        permanent_fallback_logged: false,
    };
    let attempts = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let starter = RespawnTestStarter {
        startup_custody: Arc::new(worker_startup::StartupCustody::default()),
        handshake_gate: None,
        requests: Arc::new(Mutex::new(Vec::new())),
        fixture_root: None,
        attempts: Arc::clone(&attempts),
        succeed: Arc::new(AtomicBool::new(false)),
        predecessor_was_stopped: Arc::new(AtomicBool::new(false)),
        native_driver: Arc::new(Mutex::new(None)),
    };
    let path = std::env::temp_dir().join(format!("ramshared-stop-origin-{}", std::process::id()));
    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    file.set_len(8192).unwrap();
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let serving = std::thread::spawn(move || {
        let mut runtime = runtime;
        let mut starter = starter;
        let backend = AuthoritativeOriginBackend::new_with_admission(
            FileOrigin::from_file(file),
            OriginCache::Disabled(DisabledCache),
            8192,
            BLOCK_SIZE,
            ramshared_block::AllowCacheAdmission,
        )
        .unwrap();
        started_tx.send(()).unwrap();
        recover_isolated_gpu_worker_if_failed(&mut starter, &backend, &mut runtime, true);
        let pending = runtime.supervisor.is_some();
        let payload = vec![0x6a; 4096];
        backend.write_at(0, &payload).unwrap();
        let mut actual = vec![0; 4096];
        backend.read_at(0, &mut actual).unwrap();
        done_tx.send((runtime, pending, actual == payload)).unwrap();
    });
    started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let timely = done_rx.recv_timeout(Duration::from_millis(250));
    let completed_before_release = timely.is_ok();
    let stop_admitted = resources.stop_active.load(Ordering::Acquire);
    let slot_refused = owner.reserve().is_err();
    drop(input);
    let (mut runtime, pending, correct) = match timely {
        Ok(result) => (result.0, result.1, result.2),
        Err(_) => done_rx.recv_timeout(Duration::from_secs(3)).unwrap(),
    };
    serving.join().unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !runtime.supervisor.as_mut().unwrap().shutdown() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(runtime);
    std::fs::remove_file(path).unwrap();
    assert!(
        slot_refused,
        "a live worker must retain the sole custody slot"
    );
    assert!(
        completed_before_release,
        "origin I/O waited for the worker to exit"
    );
    assert!(pending, "an unobserved live worker must remain pending");
    assert!(stop_admitted, "the owned stop was not started");
    assert_eq!(
        attempts.load(Ordering::Acquire),
        0,
        "the live worker was replaced"
    );
    assert!(correct, "origin data must remain intact");
}

#[test]
fn final_shutdown_reports_pending_until_the_exact_worker_exits() {
    let mut child = ProcessCommand::new("/bin/sh")
        .args(["-c", "read line"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let input = child.stdin.take().unwrap();
    let owner = Arc::new(worker_custody::WorkerCustody::default());
    let process = ChildWorkerProcess::from_resources(owner.reserve().unwrap().bind(child));
    let mut supervisor = IsolatedWorkerSupervisor {
        child: Some(WorkerChildHandle::Process(process)),
    };
    assert!(!supervisor.shutdown_at_exit(Duration::ZERO, Duration::from_millis(1)));
    assert!(owner.reserve().is_err());
    drop(input);
    assert!(supervisor.shutdown_at_exit(Duration::from_secs(3), Duration::from_millis(10)));
    assert!(owner.reserve().is_ok());
}

#[test]
fn repeated_supervisor_polling_does_not_abort_an_active_stop() {
    let mut child = ProcessCommand::new("/bin/sh")
        .args(["-c", "read line"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let input = child.stdin.take().unwrap();
    let owner = Arc::new(worker_custody::WorkerCustody::default());
    let resources = owner.reserve().unwrap().bind(child);
    let process = ChildWorkerProcess::from_resources(resources.clone());
    let mut runtime = IsolatedWorkerRuntime {
        cache_member: 0,
        startup: None,
        supervisor: Some(IsolatedWorkerSupervisor {
            child: Some(WorkerChildHandle::Process(process)),
        }),
        params: WorkerSpawnParams {
            generation: None,
            target_bytes: 4096,
            chunk_bytes: 4096,
            reserve_floor: 0,
            expected_adapter: None,
            host_catalog: None,
        },
        budget: WorkerRespawnBudget::new(),
        unavailable_reason: None,
        lease_lost_release: false,
        permanent_fallback_logged: false,
    };
    assert!(!runtime.supervisor.as_mut().unwrap().shutdown());
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut lost = false;
    while Instant::now() < deadline {
        assert!(!gpu_worker_has_exited(&mut runtime));
        assert!(!runtime.supervisor.as_mut().unwrap().shutdown());
        if !resources.stop_active.load(Ordering::Acquire) {
            lost = true;
            break;
        }
        std::thread::yield_now();
    }
    drop(input);
    assert!(
        runtime
            .supervisor
            .as_mut()
            .unwrap()
            .shutdown_at_exit(Duration::from_secs(3), Duration::from_millis(10))
    );
    assert!(!lost, "polling interrupted the active stop controller");
}

#[test]
fn gpu_worker_drop_preserves_unresolved_custody() {
    let mut child = ProcessCommand::new("/bin/sh")
        .args(["-c", "read line"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let input = child.stdin.take().unwrap();
    let pid = rustix::process::Pid::from_raw(child.id() as i32).unwrap();
    let owner = Arc::new(worker_custody::WorkerCustody::default());
    let process = ChildWorkerProcess::from_resources(owner.reserve().unwrap().bind(child));
    let weak = Arc::downgrade(&process.child);
    let poison = Arc::clone(&process.child);
    assert!(
        std::thread::spawn(move || {
            let _guard = poison.lock().unwrap();
            panic!("injected child observation failure");
        })
        .join()
        .is_err()
    );
    let supervisor = IsolatedWorkerSupervisor {
        child: Some(WorkerChildHandle::Process(process)),
    };
    drop(supervisor);
    let retained = weak.upgrade().is_some();
    assert!(
        owner.reserve().is_err(),
        "poisoned custody must refuse a new worker"
    );
    drop(input);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match rustix::process::waitid(
            rustix::process::WaitId::Pid(pid),
            rustix::process::WaitIdOptions::EXITED | rustix::process::WaitIdOptions::NOHANG,
        )
        .unwrap()
        {
            Some(_) => break,
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            None => panic!("fixture child did not exit"),
        }
    }
    assert!(
        retained,
        "unresolved child ownership disappeared when supervisor dropped"
    );
}

struct UnobservableChild {
    error: std::io::ErrorKind,
    kills: usize,
    reapers: usize,
}

impl WorkerProcessControl for UnobservableChild {
    fn id(&self) -> u32 {
        42
    }
    fn try_wait(&mut self) -> std::io::Result<bool> {
        Err(self.error.into())
    }
    fn kill(&mut self) -> std::io::Result<()> {
        self.kills += 1;
        Ok(())
    }
    fn reap_in_background(&mut self) -> std::io::Result<()> {
        self.reapers += 1;
        Ok(())
    }
}

#[test]
fn gpu_worker_unknown_exit_never_authorizes_kill() {
    for error in [
        std::io::ErrorKind::Other,
        std::io::ErrorKind::PermissionDenied,
        std::io::ErrorKind::Interrupted,
        std::io::ErrorKind::WouldBlock,
    ] {
        let mut child = UnobservableChild {
            error,
            kills: 0,
            reapers: 0,
        };
        let outcome =
            stop_worker_process(&mut child, Duration::ZERO, Duration::ZERO, Duration::ZERO);
        assert_eq!(outcome, WorkerStopOutcome::ReaperUnavailable, "{error:?}");
        assert_eq!((child.kills, child.reapers), (0, 0), "{error:?}");
    }
}

#[test]
fn gpu_worker_external_reap_permanently_revokes_child_effects() {
    let child = ProcessCommand::new("/bin/true").spawn().unwrap();
    let pid = rustix::process::Pid::from_raw(child.id() as i32).unwrap();
    let mut process = ChildWorkerProcess::new(child);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match rustix::process::waitid(
            rustix::process::WaitId::Pid(pid),
            rustix::process::WaitIdOptions::EXITED | rustix::process::WaitIdOptions::NOHANG,
        )
        .unwrap()
        {
            Some(_) => break,
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            None => panic!("external reaper did not observe child exit"),
        }
    }
    let no_child = rustix::io::Errno::CHILD.raw_os_error();
    assert_eq!(
        process.try_wait().unwrap_err().raw_os_error(),
        Some(no_child)
    );
    assert_eq!(process.kill().unwrap_err().raw_os_error(), Some(no_child));
    assert_eq!(
        process.reap_in_background().unwrap_err().raw_os_error(),
        Some(no_child)
    );
}

#[test]
fn gpu_worker_unknown_after_kill_does_not_start_reaper() {
    struct LateError {
        observed: bool,
        kills: usize,
        reapers: usize,
    }
    impl WorkerProcessControl for LateError {
        fn id(&self) -> u32 {
            42
        }
        fn try_wait(&mut self) -> std::io::Result<bool> {
            if std::mem::replace(&mut self.observed, true) {
                Err(rustix::io::Errno::CHILD.into())
            } else {
                Ok(false)
            }
        }
        fn kill(&mut self) -> std::io::Result<()> {
            self.kills += 1;
            Ok(())
        }
        fn reap_in_background(&mut self) -> std::io::Result<()> {
            self.reapers += 1;
            Ok(())
        }
    }
    let mut child = LateError {
        observed: false,
        kills: 0,
        reapers: 0,
    };
    assert_eq!(
        stop_worker_process(&mut child, Duration::ZERO, Duration::ZERO, Duration::ZERO),
        WorkerStopOutcome::ReaperUnavailable
    );
    assert_eq!((child.kills, child.reapers), (1, 0));
}

fn assert_refuses_held_lock(operation: fn(&mut ChildWorkerProcess) -> std::io::Result<bool>) {
    let child = ProcessCommand::new("/bin/sleep").arg("10").spawn().unwrap();
    let process = ChildWorkerProcess::new(child);
    let shared = Arc::clone(&process.child);
    let guard = shared.lock().unwrap();
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let observer = std::thread::spawn(move || {
        let mut process = process;
        started_tx.send(()).unwrap();
        let result = operation(&mut process);
        done_tx.send((process, result)).unwrap();
    });
    started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    let timely = done_rx.recv_timeout(Duration::from_millis(200));
    drop(guard);
    let (mut process, result) = match timely {
        Ok(result) => result,
        Err(_) => done_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
    };
    observer.join().unwrap();
    if !process.try_wait().unwrap() {
        process.kill().unwrap();
    }
    assert!(
        wait_for_worker_exit(
            &mut process,
            Duration::from_secs(2),
            Duration::from_millis(10)
        )
        .unwrap()
    );
    assert!(
        result.is_err_and(|error| error.kind() == std::io::ErrorKind::WouldBlock),
        "the operation must refuse promptly while the reaper owns the lock"
    );
}

#[test]
fn gpu_worker_exit_observation_does_not_wait_for_reaper_lock() {
    assert_refuses_held_lock(|process| process.try_wait());
}

#[test]
fn gpu_worker_kill_does_not_wait_for_reaper_lock() {
    assert_refuses_held_lock(|process| process.kill().map(|()| true));
}

#[test]
fn gpu_worker_observation_recovers_after_real_reaper_finishes() {
    let mut child = ProcessCommand::new("/bin/sh")
        .args(["-c", "read line"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let input = child.stdin.take().unwrap();
    let mut process = ChildWorkerProcess::new(child);
    process.reap_in_background().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match process.try_wait() {
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Ok(false) if Instant::now() < deadline => std::thread::yield_now(),
            result => panic!("reaper did not retain the live child: {result:?}"),
        }
    }
    assert_eq!(
        process.kill().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    drop(input);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match process.try_wait() {
            Ok(true) => break,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Ok(false) => {}
            result => panic!("worker completion observation failed: {result:?}"),
        }
        assert!(Instant::now() < deadline, "reaper did not finish");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn gpu_worker_recovery_refuses_replacement_until_previous_exit() {
    let (backend, mut runtime, mut starter) = respawn_fixture(true, true);
    let page = [0xa5; 4096];
    backend.write_at(0, &page).unwrap();
    let mut child = ProcessCommand::new("/bin/sh")
        .args(["-c", "read line"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let input = child.stdin.take().unwrap();
    let mut process = ChildWorkerProcess::new(child);
    process.reap_in_background().unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    while process.try_wait().is_ok() {
        assert!(Instant::now() < deadline, "reaper did not claim child");
        std::thread::yield_now();
    }
    runtime.supervisor = Some(IsolatedWorkerSupervisor {
        child: Some(WorkerChildHandle::Process(process)),
    });
    recover_isolated_gpu_worker_if_failed(&mut starter, &backend, &mut runtime, true);
    assert_eq!(starter.attempts.load(Ordering::SeqCst), 0);
    assert!(
        runtime.supervisor.is_some(),
        "previous worker custody must remain"
    );
    let mut output = [0; 4096];
    backend.read_at(0, &mut output).unwrap();
    assert_eq!(
        output, page,
        "origin must keep serving while recovery is pending"
    );
    drop(input);
    let deadline = Instant::now() + Duration::from_secs(4);
    while starter.attempts.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "worker recovery did not resume");
        std::thread::sleep(Duration::from_millis(10));
        recover_isolated_gpu_worker_if_failed(&mut starter, &backend, &mut runtime, true);
    }
    assert_eq!(starter.attempts.load(Ordering::SeqCst), 1);
    backend.read_at(0, &mut output).unwrap();
    assert_eq!(output, page);
}

#[path = "nbd_worker_recovery.rs"]
mod nbd_worker_recovery;
