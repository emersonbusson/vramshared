// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/worker_startup.rs
//! Nonblocking startup results and custody of the thread that creates a worker.
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use super::{ChildWorkerProcess, IpcCacheClient, IsolatedWorkerSupervisor, WorkerProcessControl};

type StartedWorker = (IpcCacheClient, IsolatedWorkerSupervisor);
pub(super) type InitialStartResult = Result<StartedWorker, InitialStartFailure>;
pub(super) type StartTask = Box<dyn FnOnce() -> StartOutcome + Send>;
pub(super) type StartupAuthority = Arc<dyn Fn() -> bool + Send + Sync>;

/// Initial composition must retain a spawned child even when its IPC failed.
/// Pre-spawn errors have no child; neither case proves process exit.
pub(super) struct InitialStartFailure {
    pub(super) message: String,
    pub(super) retained_child: Option<Box<ChildWorkerProcess>>,
}
impl std::fmt::Display for InitialStartFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

#[cfg(test)]
impl From<&str> for InitialStartFailure {
    fn from(message: &str) -> Self {
        Self {
            message: message.into(),
            retained_child: None,
        }
    }
}
#[cfg(test)]
impl From<io::Error> for InitialStartFailure {
    fn from(error: io::Error) -> Self {
        Self {
            message: error.to_string(),
            retained_child: None,
        }
    }
}

pub(super) struct StartOutcome {
    pub(super) result: Result<StartedWorker, String>,
    // Retained even when the handshake failed after creating the child.
    pub(super) creator_child: Option<ChildWorkerProcess>,
}

impl StartOutcome {
    pub(super) fn into_initial_result(self) -> InitialStartResult {
        self.result.map_err(|message| InitialStartFailure {
            message,
            retained_child: self.creator_child.map(Box::new),
        })
    }

    #[cfg(test)]
    pub(super) fn fixture(result: InitialStartResult) -> Self {
        match result {
            Ok((client, supervisor)) => {
                let creator_child = match supervisor.child.as_ref() {
                    Some(super::WorkerChildHandle::Process(child)) => Some(child.clone()),
                    _ => None,
                };
                Self {
                    result: Ok((client, supervisor)),
                    creator_child,
                }
            }
            Err(failure) => Self {
                result: Err(failure.message),
                creator_child: failure.retained_child.map(|child| *child),
            },
        }
    }
}

enum State {
    Empty,
    Running(JoinHandle<()>),
    Unknown,
}

pub(super) struct StartupCustody(Mutex<State>);
impl Default for StartupCustody {
    fn default() -> Self {
        Self(Mutex::new(State::Empty))
    }
}

impl StartupCustody {
    pub(super) fn is_quiescent(&self) -> bool {
        self.0
            .try_lock()
            .is_ok_and(|mut state| reap_finished(&mut state).is_ok())
    }

    pub(super) fn begin(self: &Arc<Self>, task: StartTask) -> io::Result<Startup> {
        self.begin_with(task, |task| {
            std::thread::Builder::new()
                .name("ramshared-gpu-startup".into())
                .spawn(task)
        })
    }

    pub(super) fn begin_guarded(
        self: &Arc<Self>,
        task: StartTask,
        authority: StartupAuthority,
    ) -> io::Result<Startup> {
        let task_authority = Arc::clone(&authority);
        let mut startup = self.begin(Box::new(move || {
            if !task_authority() {
                return StartOutcome {
                    result: Err("startup authority revoked".into()),
                    creator_child: None,
                };
            }
            let outcome = task();
            if !task_authority() {
                drop(outcome.result);
                return StartOutcome {
                    result: Err("startup authority revoked".into()),
                    creator_child: outcome.creator_child,
                };
            }
            outcome
        }))?;
        startup.authority = Some(authority);
        Ok(startup)
    }

    fn begin_with(
        self: &Arc<Self>,
        task: StartTask,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> io::Result<JoinHandle<()>>,
    ) -> io::Result<Startup> {
        let mut state = self.0.try_lock().map_err(|error| match error {
            std::sync::TryLockError::WouldBlock => io::Error::from(io::ErrorKind::WouldBlock),
            std::sync::TryLockError::Poisoned(_) => io::Error::other("startup custody poisoned"),
        })?;
        reap_finished(&mut state)?;
        let (sender, receiver) = mpsc::sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation = Arc::clone(&cancelled);
        let custody = Arc::clone(self);
        let handle = spawn(Box::new(move || {
            // The creator retains the slot owner even if the serving runtime
            // drops its request. It must not disappear while the child is live.
            let _custody = custody;
            if cancellation.load(Ordering::Acquire) {
                return;
            }
            let outcome = task();
            let stop = outcome.result.is_err() || cancellation.load(Ordering::Acquire);
            // Exactly one response, capacity one: never wait for a consumer.
            if cancellation.load(Ordering::Acquire) {
                drop(outcome.result);
            } else if sender.try_send(outcome.result).is_err() {
                cancellation.store(true, Ordering::Release);
            }
            if let Some(mut child) = outcome.creator_child {
                retain_creator_until_exit(&mut child, &cancellation, stop);
            }
        }))?;
        *state = State::Running(handle);
        Ok(Startup {
            receiver: Some(receiver),
            cancelled,
            consumed: false,
            authority: None,
            custody: Arc::clone(self),
        })
    }
}

fn reap_finished(state: &mut State) -> io::Result<()> {
    match state {
        State::Running(handle) if !handle.is_finished() => {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        State::Unknown => return Err(io::Error::other("startup thread outcome is unknown")),
        _ => {}
    }
    if let State::Running(handle) = std::mem::replace(state, State::Empty)
        && handle.join().is_err()
    {
        *state = State::Unknown;
        return Err(io::Error::other(
            "startup thread panicked; custody retained",
        ));
    }
    Ok(())
}

fn retain_creator_until_exit(child: &mut ChildWorkerProcess, cancelled: &AtomicBool, failed: bool) {
    loop {
        if child.stop_is_quiescent() && matches!(child.try_wait(), Ok(true)) {
            return;
        }
        if failed || cancelled.load(Ordering::Acquire) {
            let _ = child.start_stop();
        }
        // Same observation cadence as the existing owned stop controller.
        // Unknown exit or a busy reaper never permits dropping the creator.
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub(super) struct Startup {
    receiver: Option<Receiver<Result<StartedWorker, String>>>,
    custody: Arc<StartupCustody>,
    cancelled: Arc<AtomicBool>,
    consumed: bool,
    authority: Option<StartupAuthority>,
}
impl Startup {
    #[cfg(test)]
    pub(super) fn restrict(mut self, authority: StartupAuthority) -> Self {
        self.authority = Some(authority);
        self
    }

    pub(super) fn cancel(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        drop(self.receiver.take());
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub(super) fn is_quiescent(&self) -> bool {
        self.custody.is_quiescent()
    }

    pub(super) fn poll(&mut self) -> Result<Option<StartedWorker>, String> {
        if self
            .authority
            .as_ref()
            .is_some_and(|authority| !authority())
        {
            self.cancel();
            return Err("startup authority revoked".into());
        }
        let receiver = self.receiver.as_ref().ok_or("worker startup cancelled")?;
        match receiver.try_recv() {
            Ok(result) => {
                if self
                    .authority
                    .as_ref()
                    .is_some_and(|authority| !authority())
                {
                    drop(result);
                    self.cancel();
                    return Err("startup authority revoked".into());
                }
                self.consumed = true;
                result.map(Some)
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err("worker startup controller disconnected".into()),
        }
    }
}
impl Drop for Startup {
    fn drop(&mut self) {
        if !self.consumed {
            self.cancelled.store(true, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests;
