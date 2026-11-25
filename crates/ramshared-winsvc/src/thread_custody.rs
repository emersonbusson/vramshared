// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/thread_custody.rs
//! Actual thread custody. A stop request or observation deadline never proves exit.
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const JOIN_POLL: Duration = Duration::from_millis(5);
#[cfg(windows)]
mod native;
#[derive(Default, Debug)]
pub struct StopSignal {
    flag: AtomicBool,
    wait_lock: Mutex<()>,
    wake: Condvar,
}
impl StopSignal {
    pub fn is_requested(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }
    pub(super) fn flag(&self) -> &AtomicBool {
        &self.flag
    }
    fn request(&self) {
        let _guard = self
            .wait_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.flag.store(true, Ordering::Release);
        self.wake.notify_all();
    }
    /// Interrupt a retry wait without exposing a resettable stop intent.
    pub fn wait(&self, duration: Duration) -> bool {
        let guard = self
            .wait_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (_guard, _) = self
            .wake
            .wait_timeout_while(guard, duration, |_| !self.is_requested())
            .unwrap_or_else(|error| error.into_inner());
        self.is_requested()
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThreadState {
    Pending,
    Stopped,
}
#[derive(Debug)]
pub struct OwnedThread {
    stop: Arc<StopSignal>,
    worker: Option<JoinHandle<io::Result<()>>>,
    outcome: Option<io::Result<()>>,
}
impl OwnedThread {
    pub fn spawn<F>(name: &str, run: F) -> io::Result<Self>
    where
        F: FnOnce(Arc<StopSignal>) -> io::Result<()> + Send + 'static,
    {
        if name.is_empty() || name.chars().any(char::is_control) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid owned thread name",
            ));
        }
        let stop = Arc::new(StopSignal::default());
        let signal = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name(name.into())
            .spawn(move || run(signal))?;
        Ok(Self {
            stop,
            worker: Some(worker),
            outcome: None,
        })
    }
    pub fn has_joined(&self) -> bool {
        self.worker.is_none() && self.outcome.is_some()
    }
    pub fn request_stop(&self) {
        self.stop.request();
    }
    pub(crate) fn observe(&mut self) -> io::Result<ThreadState> {
        if let Some(worker) = self.worker.as_ref() {
            #[cfg(windows)]
            let finished = native::has_exited(worker)?;
            #[cfg(not(windows))]
            let finished = worker.is_finished();
            if !finished {
                return Ok(ThreadState::Pending);
            }
        }
        if let Some(worker) = self.worker.take() {
            self.outcome = Some(
                worker
                    .join()
                    .unwrap_or_else(|_| Err(io::Error::other("owned thread panicked"))),
            );
        }
        match self.outcome.as_ref() {
            Some(Ok(())) => Ok(ThreadState::Stopped),
            Some(Err(error)) => Err(io::Error::new(error.kind(), error.to_string())),
            None => Err(io::Error::other("owned thread custody is missing")),
        }
    }
    /// Pending retains this exact join handle; later observations never spawn.
    pub fn wait_for_stop(&mut self, budget: Duration) -> io::Result<ThreadState> {
        let deadline = Instant::now().checked_add(budget).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid stop observation deadline",
            )
        })?;
        self.request_stop();
        loop {
            let state = self.observe()?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if state == ThreadState::Stopped || remaining.is_zero() {
                return Ok(state);
            }
            thread::sleep(remaining.min(JOIN_POLL));
        }
    }
}
impl Drop for OwnedThread {
    fn drop(&mut self) {
        self.request_stop();
        // Ownership may not silently detach on an early return or unwind.
        while self.worker.is_some() {
            let _ = self.wait_for_stop(Duration::from_secs(1));
        }
    }
}
