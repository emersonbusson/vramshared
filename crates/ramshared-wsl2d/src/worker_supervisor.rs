// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/worker_supervisor.rs
//! Isolated GPU worker child custody, bounded exit waiting, and stop escalation.

#[cfg(test)]
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
#[cfg(test)]
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::worker_custody::{ChildWorkerProcess, WorkerProcessControl};

pub(crate) enum WorkerChildHandle {
    Process(ChildWorkerProcess),
    #[cfg(test)]
    #[allow(dead_code)]
    Thread {
        stop: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    },
}

pub(crate) struct IsolatedWorkerSupervisor {
    pub(crate) child: Option<WorkerChildHandle>,
}

pub(crate) fn wait_for_worker_exit(
    child: &mut dyn WorkerProcessControl,
    timeout: Duration,
    poll_interval: Duration,
) -> std::io::Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(true) => return Ok(true),
            Ok(false) if Instant::now() < deadline => {
                std::thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(poll_interval),
                );
            }
            Ok(false) => return Ok(false),
            // A transient observation error is not an observation of "still
            // running". `try_wait` is one `waitpid(WNOHANG)`, and EINTR is
            // handed straight back to this caller; folding that single
            // failed look into `false` published "did not exit" for a child
            // that may already have. `stop_worker_process` then SIGKILLed a
            // corpse and started a reaper it did not need, and the
            // worker-kill test asserted a failure that never happened.
            // Retry inside the same deadline: only a confirmed exit is an
            // exit, and only a confirmed non-exit at the deadline is a
            // timeout. The budget is unchanged.
            Err(error)
                if error.kind() == std::io::ErrorKind::Interrupted && Instant::now() < deadline =>
            {
                std::thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(poll_interval),
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => return Err(error),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Err(error),
            Err(error) => {
                eprintln!(
                    "[ramsharedd] GPU worker {} exit observation failed: {error}",
                    child.id()
                );
                return Err(error);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkerStopOutcome {
    Exited,
    ReaperStarted,
    ReaperUnavailable,
}

pub(crate) fn stop_worker_process(
    child: &mut dyn WorkerProcessControl,
    graceful_timeout: Duration,
    kill_timeout: Duration,
    poll_interval: Duration,
) -> WorkerStopOutcome {
    match wait_for_worker_exit(child, graceful_timeout, poll_interval) {
        Ok(true) => return WorkerStopOutcome::Exited,
        Ok(false) => {}
        Err(_) => return WorkerStopOutcome::ReaperUnavailable,
    }
    if let Err(error) = child.kill() {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return WorkerStopOutcome::ReaperUnavailable;
        }
        eprintln!(
            "[ramsharedd] GPU worker {} kill request failed: {error}",
            child.id()
        );
    }
    match wait_for_worker_exit(child, kill_timeout, poll_interval) {
        Ok(true) => return WorkerStopOutcome::Exited,
        Ok(false) => {}
        Err(_) => return WorkerStopOutcome::ReaperUnavailable,
    }
    match child.reap_in_background() {
        Ok(()) => {
            eprintln!(
                "[ramsharedd] GPU worker {} did not exit after bounded SIGKILL observation; background reaper retained its child handle",
                child.id()
            );
            WorkerStopOutcome::ReaperStarted
        }
        Err(error) => {
            eprintln!(
                "[ramsharedd] GPU worker {} could not start background reaper: {error}; retaining child ownership",
                child.id()
            );
            WorkerStopOutcome::ReaperUnavailable
        }
    }
}

impl IsolatedWorkerSupervisor {
    pub(crate) fn pid(&self) -> u32 {
        match self.child.as_ref() {
            Some(WorkerChildHandle::Process(c)) => c.id(),
            #[cfg(test)]
            Some(WorkerChildHandle::Thread { .. }) => std::process::id(),
            None => 0,
        }
    }

    pub(crate) fn shutdown(&mut self) -> bool {
        let Some(mut child) = self.child.take() else {
            return true;
        };
        let retain_child = match &mut child {
            WorkerChildHandle::Process(process) => {
                if !process.stop_is_quiescent() {
                    true
                } else {
                    match process.try_wait() {
                        Ok(true) => !process.stop_is_quiescent(),
                        Ok(false) => {
                            let _ = process.start_stop();
                            true
                        }
                        Err(_) => true,
                    }
                }
            }
            #[cfg(test)]
            WorkerChildHandle::Thread { stop, handle } => {
                stop.store(true, Ordering::SeqCst);
                if let Some(h) = handle.take() {
                    let _ = h.join();
                }
                false
            }
        };
        if retain_child {
            self.child = Some(child);
        }
        !retain_child
    }

    /// Direct-supervisor fixture; production waits on the complete collection.
    #[cfg(test)]
    pub(crate) fn shutdown_at_exit(&mut self, timeout: Duration, poll_interval: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.shutdown() {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            std::thread::sleep(remaining.min(poll_interval));
        }
    }
}

impl Drop for IsolatedWorkerSupervisor {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}
