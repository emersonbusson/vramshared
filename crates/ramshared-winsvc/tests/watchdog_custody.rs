// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/tests/watchdog_custody.rs
//! A blocked probe keeps its actual worker owned; storage intent is independent.
#![allow(clippy::unwrap_used)]
use ramshared_winsvc::vsock_lease::StopState;
use ramshared_winsvc::watchdog::wsl_scm_watchdog::{WatchdogTransport, spawn_watchdog};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

#[test]
fn scm_watchdog_stop_requires_joined_native_worker() {
    struct Probe(mpsc::Sender<()>, mpsc::Receiver<()>);
    impl WatchdogTransport for Probe {
        fn probe(&mut self) -> Result<(), String> {
            self.0.send(()).unwrap();
            self.1
                .recv_timeout(Duration::from_secs(3))
                .map_err(|e| e.to_string())
        }
    }
    let (started, ready) = mpsc::channel();
    let (release, blocked) = mpsc::channel();
    let mut task = spawn_watchdog(Probe(started, blocked), Duration::from_secs(5)).unwrap();
    struct Release(mpsc::Sender<()>);
    impl Drop for Release {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }
    let release = Release(release);
    ready.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(
        task.wait_for_stop(Duration::from_millis(20)).unwrap(),
        StopState::Pending
    );
    release.0.send(()).unwrap();
    assert_eq!(
        task.wait_for_stop(Duration::from_secs(1)).unwrap(),
        StopState::Stopped
    );
}

#[test]
fn watchdog_survives_refused_storage_stop_intent() {
    struct Probe(mpsc::Sender<()>);
    impl WatchdogTransport for Probe {
        fn probe(&mut self) -> Result<(), String> {
            self.0.send(()).map_err(|e| e.to_string())
        }
    }
    let (ticks, observed) = mpsc::channel();
    let storage_stop = Arc::new(AtomicBool::new(false));
    let mut task = spawn_watchdog(Probe(ticks), Duration::from_millis(10)).unwrap();
    observed.recv_timeout(Duration::from_secs(1)).unwrap();
    storage_stop.store(true, Ordering::Release);
    std::thread::sleep(Duration::from_millis(30));
    storage_stop.store(false, Ordering::Release);
    while observed.try_recv().is_ok() {}
    observed.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(
        task.wait_for_stop(Duration::from_secs(1)).unwrap(),
        StopState::Stopped
    );
}

#[test]
fn watchdog_zero_interval_refuses_before_probe() {
    struct Never;
    impl WatchdogTransport for Never {
        fn probe(&mut self) -> Result<(), String> {
            panic!("invalid admission performed a probe")
        }
    }
    assert!(spawn_watchdog(Never, Duration::ZERO).is_err());
}

#[cfg(windows)]
#[test]
#[ignore = "read-only: native SCM query permissions; no broker-health or installed qualification"]
fn native_scm_watchdog_observes_probe_and_joins() {
    use ramshared_winsvc::watchdog::wsl_scm_watchdog::native::ScmBrokerProbe;
    struct Observed(ScmBrokerProbe, mpsc::Sender<Result<(), String>>);
    impl WatchdogTransport for Observed {
        fn probe(&mut self) -> Result<(), String> {
            let result = self.0.probe();
            let _ = self.1.send(result.clone());
            result
        }
    }
    let (observed, result) = mpsc::channel();
    let mut task = spawn_watchdog(
        Observed(ScmBrokerProbe, observed),
        Duration::from_millis(20),
    )
    .unwrap();
    let outcome = result.recv_timeout(Duration::from_secs(5)).unwrap();
    eprintln!(
        "NATIVE_SCM_PROBE_OUTCOME broker_running={}",
        outcome.is_ok()
    );
    assert_eq!(
        task.wait_for_stop(Duration::from_secs(5)).unwrap(),
        StopState::Stopped
    );
    eprintln!("NATIVE_WATCHDOG_JOIN_OK native_probe_observed=true kernel_joined=true");
}
