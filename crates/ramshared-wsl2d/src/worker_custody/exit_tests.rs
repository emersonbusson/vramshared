// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/worker_custody/exit_tests.rs
#![allow(clippy::unwrap_used)]

use std::os::unix::process::ExitStatusExt;
use std::process::Command;
use std::time::Duration;

use super::*;

#[test]
fn child_wait_preserves_the_exit_code_across_observers() {
    let owner = Arc::new(WorkerCustody::default());
    let child = Command::new("/bin/sh")
        .args(["-c", "exit 42"])
        .spawn()
        .unwrap();
    let resources = owner.reserve().unwrap().bind(child);
    let mut process = ChildWorkerProcess::from_resources(resources.clone());

    assert!(
        super::super::wait_for_worker_exit(
            &mut process,
            Duration::from_secs(3),
            Duration::from_millis(10)
        )
        .unwrap()
    );
    assert_eq!(
        process.exit_status().and_then(|status| status.code()),
        Some(42)
    );
    assert_eq!(process.exit_status().as_ref(), resources.exit_status.get());
    assert!(owner.is_quiescent());
}

#[test]
fn custody_reconciliation_preserves_the_exit_code() {
    let owner = Arc::new(WorkerCustody::default());
    let child = Command::new("/bin/sh")
        .args(["-c", "exit 23"])
        .spawn()
        .unwrap();
    let resources = owner.reserve().unwrap().bind(child);
    resources.child.lock().unwrap().wait().unwrap();
    assert!(resources.exit_status.get().is_none());

    assert!(owner.is_quiescent());
    let process = ChildWorkerProcess::from_resources(resources);
    assert_eq!(
        process.exit_status().and_then(|status| status.code()),
        Some(23)
    );
}

#[test]
fn background_reaper_preserves_the_terminating_signal() {
    let child = Command::new("/bin/sh")
        .args(["-c", "kill -TERM $$"])
        .spawn()
        .unwrap();
    let mut process = ChildWorkerProcess::new(child);
    process.reap_in_background().unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !process.stop_is_quiescent() {
        assert!(Instant::now() < deadline, "reaper did not finish");
        std::thread::sleep(Duration::from_millis(10));
    }
    let status = process.exit_status().unwrap();
    assert_eq!(
        status.signal(),
        Some(rustix::process::Signal::TERM.as_raw())
    );
    assert_eq!(status.code(), None);
}
