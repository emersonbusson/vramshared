// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/command_runtime.rs
//! Compose the serving-process helper owner; never exit the storage peer.

#[cfg(test)]
use ramshared_exec::retained_process::HelperState;
use ramshared_exec::retained_process::{
    HelperShutdownLease, NativeProcessControl, ProcessControl, ServingCommandOwner,
};
use std::io;
use std::process::{Child, Command, ExitStatus};
use std::sync::OnceLock;
use std::time::Duration;

pub(super) struct DaemonProcessControl;
impl ProcessControl for DaemonProcessControl {
    fn observe_exit(&self, child: &mut Child) -> io::Result<bool> {
        #[cfg(test)]
        if super::tests::command_process::command_observation_fault() {
            return Err(io::Error::other("injected helper exit observation failure"));
        }
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
pub(super) type Owner = ServingCommandOwner<DaemonProcessControl>;
pub(super) type ShutdownLease = HelperShutdownLease<'static, DaemonProcessControl>;
static OWNER: OnceLock<Owner> = OnceLock::new();
fn owner() -> &'static Owner {
    #[cfg(test)]
    if let Some(owner) = fixtures::TEST_OWNER.with(|cell| cell.get()) {
        return owner;
    }
    OWNER.get_or_init(|| Owner::new(DaemonProcessControl))
}

pub(super) fn command_stdout_with_timeout(
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> Option<String> {
    command_stdout_with_limit(program, args, timeout, 256 * 1024)
}

pub(super) fn command_stdout_with_limit(
    program: &str,
    args: &[&str],
    timeout: Duration,
    output_limit: usize,
) -> Option<String> {
    let mut command = Command::new(program);
    command.args(args);
    match owner().run(&mut command, timeout, output_limit) {
        Ok(output) => String::from_utf8(output.stdout).ok(),
        Err(error) => {
            eprintln!("[ramsharedd] helper {program} unavailable: {error}");
            None
        }
    }
}
pub(super) fn try_claim_shutdown() -> Option<ShutdownLease> {
    owner().try_shutdown()
}
pub(super) fn wait_for_helpers() -> ShutdownLease {
    let mut reported = false;
    loop {
        if let Some(lease) = try_claim_shutdown() {
            return lease;
        }
        if !reported {
            eprintln!("[ramsharedd] shutdown deferred: helper custody remains owned");
            reported = true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
pub(super) use fixtures::{pending, reconcile, test_owner, test_scope};

#[cfg(test)]
mod fixtures {
    use super::*;
    thread_local! { pub(super) static TEST_OWNER:std::cell::Cell<Option<&'static Owner>>=const {std::cell::Cell::new(None)}; }
    pub(crate) fn test_owner() -> Owner {
        Owner::new(DaemonProcessControl)
    }
    pub(crate) struct TestOwnerScope(Option<&'static Owner>);
    pub(crate) fn test_scope(owner: &'static Owner) -> TestOwnerScope {
        TestOwnerScope(TEST_OWNER.with(|cell| cell.replace(Some(owner))))
    }
    impl Drop for TestOwnerScope {
        fn drop(&mut self) {
            let current = owner();
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while !current.reconcile() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            TEST_OWNER.with(|cell| cell.set(self.0));
            if !std::thread::panicking() {
                assert_eq!(
                    current.state(),
                    HelperState::Idle,
                    "test helper custody did not settle"
                );
            }
        }
    }
    pub(crate) fn pending() -> bool {
        matches!(owner().state(), HelperState::Pending { .. })
    }
    pub(crate) fn reconcile() -> bool {
        owner().reconcile()
    }
}
