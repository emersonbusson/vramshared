// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-exec/src/retained_process/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use std::sync::OnceLock;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

#[derive(Default)]
struct Faults {
    observe: AtomicBool,
    signal: AtomicBool,
    reap: AtomicBool,
    group: AtomicBool,
    signals: AtomicUsize,
    delay: AtomicBool,
    observed_ids: Mutex<Vec<u32>>,
}
impl ProcessControl for Arc<Faults> {
    fn observe_exit(&self, child: &mut Child) -> io::Result<bool> {
        self.observed_ids.lock().unwrap().push(child.id());
        if self.delay.swap(false, Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(30));
        }
        if self.observe.load(Ordering::SeqCst) {
            Err(io::Error::other("injected observation"))
        } else {
            NativeProcessControl.observe_exit(child)
        }
    }
    fn stop_group(&self, child: &mut Child) -> io::Result<()> {
        self.signals.fetch_add(1, Ordering::SeqCst);
        if self.signal.load(Ordering::SeqCst) {
            Err(io::Error::other("injected signal"))
        } else {
            NativeProcessControl.stop_group(child)
        }
    }
    fn reap(&self, child: &mut Child) -> io::Result<Option<ExitStatus>> {
        if self.reap.load(Ordering::SeqCst) {
            Err(io::Error::other("injected reap"))
        } else {
            NativeProcessControl.reap(child)
        }
    }
    fn group_absent(&self, group: u32) -> io::Result<bool> {
        if self.group.load(Ordering::SeqCst) {
            Err(io::Error::other("injected group observation"))
        } else {
            NativeProcessControl.group_absent(group)
        }
    }
}
fn settle<C: ProcessControl>(owner: &ServingCommandOwner<C>) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !owner.reconcile() && Instant::now() < deadline {
        std::thread::sleep(POLL);
    }
    assert_eq!(owner.state(), HelperState::Idle);
}
#[test]
fn retained_owner_preserves_legitimate_output_and_failure_results() {
    static OWNER: OnceLock<ServingCommandOwner> = OnceLock::new();
    let owner = OWNER.get_or_init(ServingCommandOwner::default);
    let output = owner
        .run(
            Command::new("/bin/sh").args(["-c", "printf payload; printf error >&2"]),
            Duration::from_secs(1),
            1024,
        )
        .unwrap();
    assert_eq!(output.stdout, b"payload");
    assert_eq!(output.stderr, b"error");
    assert!(matches!(
        owner.run(
            &mut Command::new("/bin/false"),
            Duration::from_secs(1),
            1024
        ),
        Err(ServingCommandError::Failed(
            ProcessSpawnError::NonZeroExit { .. }
        ))
    ));
    assert!(matches!(
        owner.run(
            &mut Command::new("/nonexistent/ramshared"),
            Duration::from_secs(1),
            1024
        ),
        Err(ServingCommandError::Failed(_))
    ));
    assert_eq!(owner.state(), HelperState::Idle);
}
#[test]
fn retained_owner_keeps_unobservable_child_and_rejects_new_spawn_and_shutdown() {
    static OWNER: OnceLock<ServingCommandOwner<Arc<Faults>>> = OnceLock::new();
    let owner = OWNER.get_or_init(|| ServingCommandOwner::new(Arc::new(Faults::default())));
    let faults = Arc::clone(&owner.control);
    faults.observe.store(true, Ordering::SeqCst);
    let error = owner
        .run(
            &mut Command::new("/bin/true"),
            Duration::from_millis(5),
            1024,
        )
        .unwrap_err();
    assert!(matches!(error, ServingCommandError::Pending { .. }));
    let pending = owner.state();
    assert!(matches!(pending, HelperState::Pending { .. }));
    assert!(matches!(
        owner.run(
            &mut Command::new("/bin/true"),
            Duration::from_millis(5),
            1024
        ),
        Err(ServingCommandError::Busy)
    ));
    assert_eq!(owner.state(), pending);
    assert!(owner.try_shutdown().is_none());
    faults.observe.store(false, Ordering::SeqCst);
    settle(owner);
    assert_eq!(faults.signals.load(Ordering::SeqCst), 1);
    let lease = owner.try_shutdown().unwrap();
    assert_eq!(owner.state(), HelperState::Closing);
    assert!(matches!(
        owner.run(&mut Command::new("/bin/true"), Duration::from_secs(1), 1024),
        Err(ServingCommandError::Closing)
    ));
    drop(lease);
    assert_eq!(owner.state(), HelperState::Idle);
}
#[test]
fn retained_reap_and_group_errors_never_resignal_after_reap() {
    static OWNER: OnceLock<ServingCommandOwner<Arc<Faults>>> = OnceLock::new();
    let owner = OWNER.get_or_init(|| ServingCommandOwner::new(Arc::new(Faults::default())));
    for stage in ["reap", "group"] {
        let faults = Arc::clone(&owner.control);
        faults.signals.store(0, Ordering::SeqCst);
        if stage == "reap" {
            faults.reap.store(true, Ordering::SeqCst);
        } else {
            faults.group.store(true, Ordering::SeqCst);
        }
        assert!(matches!(
            owner.run(
                &mut Command::new("/bin/true"),
                Duration::from_millis(5),
                1024
            ),
            Err(ServingCommandError::Pending { .. })
        ));
        assert_eq!(faults.signals.load(Ordering::SeqCst), 1);
        faults.reap.store(false, Ordering::SeqCst);
        faults.group.store(false, Ordering::SeqCst);
        settle(owner);
        assert_eq!(faults.signals.load(Ordering::SeqCst), 1);
    }
}
#[test]
fn retained_owner_bounds_capture_and_timeout_without_process_exit() {
    static OWNER: OnceLock<ServingCommandOwner> = OnceLock::new();
    let owner = OWNER.get_or_init(ServingCommandOwner::default);
    assert!(
        owner
            .run(
                Command::new("/bin/sh").args(["-c", "printf too-much-output"]),
                Duration::from_secs(1),
                3
            )
            .is_err()
    );
    assert!(
        owner
            .run(
                Command::new("/bin/sleep").arg("1"),
                Duration::from_millis(5),
                1024
            )
            .is_err()
    );
    settle(owner);
    assert!(
        owner
            .run(&mut Command::new("/bin/true"), Duration::MAX, 1024)
            .is_err()
    );
}
#[test]
fn command_group_signal_refuses_zero_and_init_aliases() {
    for raw in [0, 1, u32::MAX] {
        assert!(native::owned_group(raw).is_err());
    }
}

#[test]
fn retained_signal_failure_is_not_repeated_or_treated_as_completion() {
    static OWNER: OnceLock<ServingCommandOwner<Arc<Faults>>> = OnceLock::new();
    let owner = OWNER.get_or_init(|| ServingCommandOwner::new(Arc::new(Faults::default())));
    let faults = &owner.control;
    faults.signal.store(true, Ordering::SeqCst);
    // The child exits naturally; failure to signal is reported, never retried
    // after its exact leader has been reaped.
    assert!(matches!(
        owner.run(&mut Command::new("/bin/true"), Duration::from_secs(1), 1024),
        Err(ServingCommandError::Failed(_))
    ));
    assert_eq!(faults.signals.load(Ordering::SeqCst), 1);
    assert_eq!(owner.state(), HelperState::Idle);
}

#[test]
fn retained_worker_stays_owned_until_the_blocked_capture_finishes() {
    use std::sync::mpsc;
    struct BlockingReader(mpsc::Receiver<()>);
    impl io::Read for BlockingReader {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            self.0.recv().unwrap();
            Ok(0)
        }
    }
    let owner = ServingCommandOwner::default();
    let mut command = Command::new("/bin/true");
    command
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().unwrap();
    let mut reservation = owner.reserve(false).unwrap();
    let mut flight = Flight::new(
        child,
        "blocked capture".into(),
        Instant::now(),
        Duration::ZERO,
    );
    flight.start_captures(1024);
    // Account the original bounded pipe worker before replacing its reader.
    let deadline = Instant::now() + Duration::from_secs(1);
    while !matches!(
        account_capture(&mut flight.stdout, &mut flight.out),
        Ok(true)
    ) && Instant::now() < deadline
    {
        std::thread::sleep(POLL);
    }
    assert!(matches!(
        account_capture(&mut flight.stdout, &mut flight.out),
        Ok(true)
    ));
    let (sender, receiver) = mpsc::sync_channel(1);
    flight.stdout = Some(CaptureWorker::spawn("blocked", BlockingReader(receiver), 1024).unwrap());
    flight.out = None;
    assert!(flight.poll(&owner.control, Instant::now()).is_none());
    reservation.flight = Some(Box::new(flight));
    drop(reservation);
    assert!(matches!(owner.state(), HelperState::Pending { .. }));
    assert!(!owner.reconcile());
    assert!(owner.try_shutdown().is_none());
    sender.send(()).unwrap();
    settle(&owner);
}

#[test]
fn retained_capture_panic_is_accounted_without_terminating_owner() {
    struct PanicReader;
    impl io::Read for PanicReader {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            panic!("fixture capture panic")
        }
    }
    let mut worker = Some(CaptureWorker::spawn("panic", PanicReader, 1024).unwrap());
    let mut output = None;
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match account_capture(&mut worker, &mut output) {
            Ok(false) if Instant::now() < deadline => std::thread::sleep(POLL),
            Err(_) => break,
            other => panic!("panic capture incorrectly completed: {other:?}"),
        }
    }
}

#[test]
fn retained_reservation_survives_unwind_without_dropping_child() {
    let owner = ServingCommandOwner::default();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut reservation = owner.reserve(false).unwrap();
        let mut command = Command::new("/bin/true");
        command
            .process_group(0)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = command.spawn().unwrap();
        reservation.flight = Some(Box::new(Flight::new(
            child,
            "unwind fixture".into(),
            Instant::now(),
            Duration::ZERO,
        )));
        reservation.flight.as_mut().unwrap().start_captures(1024);
        panic!("fixture unwind after custody installed");
    }));
    assert!(result.is_err());
    assert!(matches!(owner.state(), HelperState::Pending { .. }));
    settle(&owner);
}

#[test]
fn retained_admission_is_exclusive_and_effects_run_outside_transfer_lock() {
    struct ObserveLock;
    static OWNER: OnceLock<ServingCommandOwner<ObserveLock>> = OnceLock::new();
    impl ProcessControl for ObserveLock {
        fn observe_exit(&self, child: &mut Child) -> io::Result<bool> {
            assert_eq!(OWNER.get().unwrap().state(), HelperState::Running);
            assert!(OWNER.get().unwrap().try_shutdown().is_none());
            NativeProcessControl.observe_exit(child)
        }
        fn stop_group(&self, child: &mut Child) -> io::Result<()> {
            NativeProcessControl.stop_group(child)
        }
        fn reap(&self, child: &mut Child) -> io::Result<Option<ExitStatus>> {
            NativeProcessControl.reap(child)
        }
        fn group_absent(&self, group: u32) -> io::Result<bool> {
            NativeProcessControl.group_absent(group)
        }
    }
    let owner = OWNER.get_or_init(|| ServingCommandOwner::new(ObserveLock));
    let reservation = owner.reserve(false).unwrap();
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                assert!(matches!(
                    owner.reserve(false),
                    Err(ServingCommandError::Busy)
                ))
            })
            .join()
            .unwrap();
    });
    drop(reservation);
    owner
        .run(&mut Command::new("/bin/true"), Duration::from_secs(1), 1024)
        .unwrap();
}

#[test]
fn command_group_signal_targets_the_owned_live_group_before_reap() {
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", "read never"])
        .stdin(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn().unwrap();
    let pid = rustix::process::Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
    assert_eq!(rustix::process::getpgid(Some(pid)).unwrap(), pid);
    assert_ne!(rustix::process::getpgid(None).unwrap(), pid);
    assert!(!NativeProcessControl.group_absent(child.id()).unwrap());
    NativeProcessControl.stop_group(&mut child).unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    while !NativeProcessControl.observe_exit(&mut child).unwrap() && Instant::now() < deadline {
        std::thread::sleep(POLL);
    }
    assert!(NativeProcessControl.observe_exit(&mut child).unwrap());
    let status = NativeProcessControl.reap(&mut child).unwrap().unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(status.signal(), Some(9));
    assert!(NativeProcessControl.group_absent(child.id()).unwrap());
}

#[test]
fn expired_helper_deadline_never_admits_a_new_child_after_reconciliation() {
    static OWNER: OnceLock<ServingCommandOwner<Arc<Faults>>> = OnceLock::new();
    let owner = OWNER.get_or_init(|| ServingCommandOwner::new(Arc::new(Faults::default())));
    let faults = &owner.control;
    faults.observe.store(true, Ordering::SeqCst);
    assert!(matches!(
        owner.run(
            &mut Command::new("/bin/true"),
            Duration::from_millis(5),
            1024
        ),
        Err(ServingCommandError::Pending { .. })
    ));
    let HelperState::Pending { pid: original } = owner.state() else {
        panic!("pending fixture");
    };
    faults.observe.store(false, Ordering::SeqCst);
    faults.observed_ids.lock().unwrap().clear();
    faults.delay.store(true, Ordering::SeqCst);
    let _ = owner.run(
        &mut Command::new("/bin/true"),
        Duration::from_millis(5),
        1024,
    );
    settle(owner);
    let ids = faults.observed_ids.lock().unwrap();
    assert!(
        ids.iter().all(|pid| *pid == original),
        "a new child was admitted after its deadline: {ids:?}"
    );
}

#[test]
fn unavailable_exit_observation_never_authorizes_a_group_signal() {
    static OWNER: OnceLock<ServingCommandOwner<Arc<Faults>>> = OnceLock::new();
    let owner = OWNER.get_or_init(|| ServingCommandOwner::new(Arc::new(Faults::default())));
    let faults = &owner.control;
    faults.observe.store(true, Ordering::SeqCst);
    assert!(matches!(
        owner.run(
            &mut Command::new("/bin/true"),
            Duration::from_millis(5),
            1024
        ),
        Err(ServingCommandError::Pending { .. })
    ));
    let signals_while_unknown = faults.signals.load(Ordering::SeqCst);
    faults.observe.store(false, Ordering::SeqCst);
    settle(owner);
    assert_eq!(
        signals_while_unknown, 0,
        "an unavailable child relationship cannot authorize signalling a numeric group"
    );
    assert_eq!(faults.signals.load(Ordering::SeqCst), 1);
}

struct ExternalReap {
    observed: AtomicUsize,
    signals: AtomicUsize,
    reaps: AtomicUsize,
    allow_group: AtomicBool,
}
impl ProcessControl for ExternalReap {
    fn observe_exit(&self, child: &mut Child) -> io::Result<bool> {
        assert_eq!(
            self.observed.fetch_add(1, Ordering::SeqCst),
            0,
            "must never adopt the numeric PID after ECHILD"
        );
        let deadline = Instant::now() + Duration::from_secs(1);
        while !NativeProcessControl.observe_exit(child)? && Instant::now() < deadline {
            std::thread::sleep(POLL);
        }
        let pid = native::owned_group(child.id())?;
        let status = rustix::process::waitid(
            rustix::process::WaitId::Pid(pid),
            rustix::process::WaitIdOptions::EXITED | rustix::process::WaitIdOptions::NOHANG,
        )
        .map_err(io::Error::from)?;
        assert!(
            status.is_some(),
            "the fixture must actually reap its owned child"
        );
        Err(rustix::io::Errno::CHILD.into())
    }
    fn stop_group(&self, _: &mut Child) -> io::Result<()> {
        self.signals.fetch_add(1, Ordering::SeqCst);
        panic!("lost child identity must never signal")
    }
    fn reap(&self, _: &mut Child) -> io::Result<Option<ExitStatus>> {
        self.reaps.fetch_add(1, Ordering::SeqCst);
        panic!("lost child identity must never reap")
    }
    fn group_absent(&self, group: u32) -> io::Result<bool> {
        if self.allow_group.load(Ordering::SeqCst) {
            NativeProcessControl.group_absent(group)
        } else {
            Ok(false)
        }
    }
}

#[test]
fn lost_reaping_identity_uses_only_readonly_group_completion() {
    static OWNER: OnceLock<ServingCommandOwner<ExternalReap>> = OnceLock::new();
    let owner = OWNER.get_or_init(|| {
        ServingCommandOwner::new(ExternalReap {
            observed: AtomicUsize::new(0),
            signals: AtomicUsize::new(0),
            reaps: AtomicUsize::new(0),
            allow_group: AtomicBool::new(false),
        })
    });
    assert!(matches!(
        owner.run(
            &mut Command::new("/bin/true"),
            Duration::from_millis(5),
            1024
        ),
        Err(ServingCommandError::Pending { .. })
    ));
    assert!(matches!(owner.state(), HelperState::Pending { .. }));
    assert!(!owner.reconcile());
    assert!(owner.try_shutdown().is_none());
    owner.control.allow_group.store(true, Ordering::SeqCst);
    settle(owner);
    assert_eq!(owner.control.observed.load(Ordering::SeqCst), 1);
    assert_eq!(owner.control.signals.load(Ordering::SeqCst), 0);
    assert_eq!(owner.control.reaps.load(Ordering::SeqCst), 0);
}

#[test]
fn lost_reaping_identity_never_returns_success_without_exit_status() {
    static OWNER: OnceLock<ServingCommandOwner<ExternalReap>> = OnceLock::new();
    let owner = OWNER.get_or_init(|| {
        ServingCommandOwner::new(ExternalReap {
            observed: AtomicUsize::new(0),
            signals: AtomicUsize::new(0),
            reaps: AtomicUsize::new(0),
            allow_group: AtomicBool::new(true),
        })
    });
    let error = owner
        .run(
            Command::new("/bin/echo").arg("must-not-be-accepted"),
            Duration::from_secs(1),
            1024,
        )
        .unwrap_err();
    assert!(matches!(error, ServingCommandError::Failed(_)));
    assert!(error.to_string().contains("reaping identity was lost"));
    assert_eq!(owner.state(), HelperState::Idle);
    assert_eq!(owner.control.signals.load(Ordering::SeqCst), 0);
    assert_eq!(owner.control.reaps.load(Ordering::SeqCst), 0);
}
