// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-exec/src/bounded_process/tests.rs
#![allow(clippy::expect_used)]

#[path = "tests/group_containment.rs"]
mod group_containment;
#[path = "tests/reap_and_capture.rs"]
mod reap_and_capture;
#[path = "tests/support.rs"]
mod support;

use super::*;

pub(super) use std::cell::{Cell, RefCell};
pub(super) use std::collections::VecDeque;
pub(super) use std::io::Cursor;
pub(super) use std::os::unix::process::ExitStatusExt;
pub(super) use std::path::Path;

#[test]
fn exit_controller_terminates_only_its_test_child() {
    const CHILD_MARKER: &str = "RAMSHARED_TEST_EXIT_CONTROLLER_CHILD";
    if std::env::var_os(CHILD_MARKER).is_some() {
        ExitController.contain("test-only containment fixture");
        unreachable!("the exit controller must terminate its isolated child");
    }

    let output = std::process::Command::new(
        std::env::current_exe().expect("test executable path must be available"),
    )
    .args([
        "--exact",
        "bounded_process::tests::exit_controller_terminates_only_its_test_child",
        "--nocapture",
    ])
    .env(CHILD_MARKER, "1")
    .output()
    .expect("the isolated test child must start");

    assert_eq!(output.status.code(), Some(FATAL_EXIT_CODE));
    assert!(String::from_utf8_lossy(&output.stderr).contains("test-only containment fixture"));
}

#[test]
// TestName: source_only_true_before_action_after_is_ordered
// design Step 3 before -> action -> after, source-only, with /bin/true as the
// action. Zero host pressure: the action is a no-op, so this proves the
// evidence-protocol shape without swap, ublk, or GPU load.
fn source_only_true_before_action_after_is_ordered() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-bounded-before-after-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock is after the Unix epoch")
            .as_nanos()
    ));
    std::fs::create_dir(&root).expect("isolated before/action/after root");
    let state = root.join("source-state");

    // BEFORE: a source-only observable exists and is recorded first.
    std::fs::write(&state, "before").expect("write before state");
    let before = std::fs::read_to_string(&state).expect("read before state");
    assert_eq!(before, "before");

    // ACTION: the real bounded spawn path runs /bin/true, a no-op.
    let mut action = Command::new("/bin/true");
    let output = run_capture_command(
        &mut action,
        "source-only /bin/true action",
        Duration::from_secs(2),
        DEFAULT_OUTPUT_LIMIT,
        |_| {},
    )
    .expect("/bin/true is a legitimate zero-pressure action");
    assert!(output.status.success(), "/bin/true must exit 0");
    assert!(output.stdout.is_empty(), "/bin/true must not emit stdout");
    assert!(output.stderr.is_empty(), "/bin/true must not emit stderr");

    // AFTER: the action is recorded and the protocol ordering holds.
    std::fs::write(&state, "after").expect("write after state");
    let after = std::fs::read_to_string(&state).expect("read after state");
    assert_eq!(after, "after");
    assert_ne!(before, after, "before and after must be distinct phases");

    let _ = std::fs::remove_dir_all(&root);
}
