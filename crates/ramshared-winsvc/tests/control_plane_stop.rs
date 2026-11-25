// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/tests/control_plane_stop.rs
//! Owned thread completion, cancellation and panic are distinct lifecycle facts.
#![allow(clippy::unwrap_used)]
use ramshared_winsvc::vsock_lease::{ControlPlaneTask, StopState};
use std::sync::mpsc;
use std::time::Duration;
#[test]
fn lab_host_stop_requires_control_plane_thread_exit() {
    let (ready, running) = mpsc::channel();
    let (release, blocked) = mpsc::channel();
    let (stopping, stop_observed) = mpsc::channel();
    let mut task = ControlPlaneTask::spawn("blocked-test", move |stop| {
        ready.send(()).unwrap();
        assert!(stop.wait(Duration::from_secs(5)));
        stopping.send(()).unwrap();
        blocked.recv().unwrap();
        assert!(stop.is_requested());
        Ok(())
    })
    .unwrap();
    struct Release(Option<mpsc::Sender<()>>);
    impl Drop for Release {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }
    let release = Release(Some(release));
    running.recv().unwrap();
    task.request_stop();
    // Observe the explicit request before any wait_for_stop can request it.
    stop_observed.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(
        task.wait_for_stop(Duration::from_millis(20)).unwrap(),
        StopState::Pending
    );
    assert_eq!(
        task.wait_for_stop(Duration::ZERO).unwrap(),
        StopState::Pending
    );
    release.0.as_ref().unwrap().send(()).unwrap();
    assert_eq!(
        task.wait_for_stop(Duration::from_secs(1)).unwrap(),
        StopState::Stopped
    );
    assert_eq!(
        task.wait_for_stop(Duration::ZERO).unwrap(),
        StopState::Stopped
    );
}
#[test]
fn owned_control_drop_joins_and_reports_panics() {
    let (exited, proof) = mpsc::channel();
    let task = ControlPlaneTask::spawn("drop-test", move |stop| {
        assert!(stop.wait(Duration::from_secs(5)));
        exited.send(()).unwrap();
        Ok(())
    })
    .unwrap();
    drop(task);
    proof.try_recv().unwrap();
    let mut panicked = ControlPlaneTask::spawn("panic-test", |_| -> std::io::Result<()> {
        panic!("test-only thread panic")
    })
    .unwrap();
    assert!(
        panicked
            .wait_for_stop(Duration::from_secs(1))
            .unwrap_err()
            .to_string()
            .contains("panicked")
    );
    assert!(panicked.wait_for_stop(Duration::ZERO).is_err());
}

#[test]
fn owned_control_refuses_invalid_admission_and_retains_terminal_failure() {
    assert!(ControlPlaneTask::spawn("invalid\0name", |_| Ok(())).is_err());
    let mut task = ControlPlaneTask::spawn("failed-test", |_| {
        Err(std::io::ErrorKind::PermissionDenied.into())
    })
    .unwrap();
    assert_eq!(
        task.wait_for_stop(Duration::from_secs(1))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        task.wait_for_stop(Duration::ZERO).unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    let mut waiting = ControlPlaneTask::spawn("deadline-test", |stop| {
        assert!(stop.wait(Duration::from_secs(5)));
        Ok(())
    })
    .unwrap();
    assert!(waiting.wait_for_stop(Duration::MAX).is_err());
    assert_eq!(
        waiting.wait_for_stop(Duration::from_secs(1)).unwrap(),
        StopState::Stopped
    );
}

#[cfg(windows)]
#[test]
fn native_thread_local_cleanup_remains_pending_until_kernel_exit() {
    use std::cell::RefCell;
    struct Cleanup(mpsc::Sender<()>, mpsc::Receiver<()>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.0.send(());
            let _ = self.1.recv_timeout(Duration::from_secs(3));
        }
    }
    thread_local! { static CLEANUP: RefCell<Option<Cleanup>> = const { RefCell::new(None) }; }
    let (entered, ready) = mpsc::channel();
    let (release, blocked) = mpsc::channel();
    let mut task = ControlPlaneTask::spawn("tls-stop-test", move |_| {
        CLEANUP.with(|slot| *slot.borrow_mut() = Some(Cleanup(entered, blocked)));
        Ok(())
    })
    .unwrap();
    struct Release(mpsc::Sender<()>);
    impl Drop for Release {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }
    let release = Release(release);
    ready.recv_timeout(Duration::from_secs(2)).unwrap();
    let before = std::time::Instant::now();
    let result = task.wait_for_stop(Duration::from_millis(20));
    let elapsed = before.elapsed();
    let _ = release.0.send(());
    assert_eq!(result.unwrap(), StopState::Pending);
    assert!(elapsed < Duration::from_secs(1));
    assert_eq!(
        task.wait_for_stop(Duration::from_secs(1)).unwrap(),
        StopState::Stopped
    );
}
