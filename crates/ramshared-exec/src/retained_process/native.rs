// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-exec/src/retained_process/native.rs
//! Process effects for one owned short-helper group.

use rustix::process::{
    Pid, Signal, WaitId, WaitIdOptions, kill_process_group, test_kill_process_group, waitid,
};
use std::io;
use std::process::{Child, ExitStatus};

/// A narrow effect boundary. Overrides are Rust dependencies, never CLI/env input.
/// Implementations must preserve the supplied child's identity and never reap
/// during observation. Only the owner decides when a signal/reap is admitted.
pub trait ProcessControl: Send + Sync + 'static {
    fn observe_exit(&self, child: &mut Child) -> io::Result<bool>;
    fn stop_group(&self, child: &mut Child) -> io::Result<()>;
    fn reap(&self, child: &mut Child) -> io::Result<Option<ExitStatus>>;
    fn group_absent(&self, group: u32) -> io::Result<bool>;
}

#[derive(Default)]
pub struct NativeProcessControl;

pub(super) fn owned_group(raw: u32) -> io::Result<Pid> {
    let raw = i32::try_from(raw)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "child PID overflow"))?;
    if raw <= 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "group is not an owned child group",
        ));
    }
    Pid::from_raw(raw).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "zero child PID"))
}

impl ProcessControl for NativeProcessControl {
    fn observe_exit(&self, child: &mut Child) -> io::Result<bool> {
        waitid(
            WaitId::Pid(owned_group(child.id())?),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        )
        .map(|status| status.is_some())
        .map_err(io::Error::from)
    }
    fn stop_group(&self, child: &mut Child) -> io::Result<()> {
        match signal_owned_group(child) {
            Ok(()) => Ok(()),
            Err(error) if error.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error()) => {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
    fn reap(&self, child: &mut Child) -> io::Result<Option<ExitStatus>> {
        child.try_wait()
    }
    fn group_absent(&self, group: u32) -> io::Result<bool> {
        match test_kill_process_group(owned_group(group)?) {
            Err(rustix::io::Errno::SRCH) => Ok(true),
            Ok(()) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
}

/// Raw errno is retained for the controller policy's existing classification.
pub(crate) fn signal_owned_group(child: &mut Child) -> io::Result<()> {
    kill_process_group(owned_group(child.id())?, Signal::KILL).map_err(io::Error::from)
}
