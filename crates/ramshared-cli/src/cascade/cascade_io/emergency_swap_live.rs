// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/emergency_swap_live.rs
//! Real emergency swap lifecycle in the disposable kernel fixture.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;

/// Runs only in the disposable kernel fixture, while its controller owns
/// the NBD workload and holds this process responsible for the swap file.
#[test]
#[ignore = "requires the isolated s18 kernel fixture"]
fn isolated_kernel_emergency_lifecycle() {
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap();
    assert!(
        cmdline
            .split_whitespace()
            .any(|v| v == "ktest.scenario=s18")
    );
    let runner = super::super::command_runner::SystemCommandRunner;
    let emergency = create_in(
        &runner,
        448 * MIB,
        &[PathBuf::from("/mnt/emergency")],
        &linux_free_bytes,
    )
    .expect("create real emergency swap");
    assert!(
        emergency
            .free_bytes(&crate::cascade::read_swaps().unwrap())
            .unwrap()
            > 0
    );
    std::fs::write("/tmp/emergency-ready", emergency.path.to_str().unwrap()).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(240);
    while !Path::new("/tmp/emergency-release").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "fixture release timed out"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(emergency.remove(&runner), "remove real emergency swap");
    assert!(!emergency.path.exists());
    std::fs::write("/tmp/emergency-removed", b"ok").unwrap();
}
