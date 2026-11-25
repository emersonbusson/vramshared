// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/worker_custody.rs
//! Process-lifetime ownership and admission for one isolated GPU worker.

use std::io;
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

mod host_memory;
#[cfg(test)]
mod memory_tests;
use host_memory::HostMemoryCustody;
use ramshared_wsl2d::host_memory_budget::MemoryReservation;

const STOP_RETRY_DELAY_MS: u64 = 250;

fn stop_clock_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    u64::try_from(START.get_or_init(Instant::now).elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[derive(Clone)]
pub(super) struct WorkerResources {
    pub(super) pid: u32,
    pub(super) child: Arc<Mutex<Child>>,
    pub(super) identity_lost: Arc<AtomicBool>,
    pub(super) exited: Arc<AtomicBool>,
    pub(super) reaper_active: Arc<AtomicBool>,
    pub(super) stop_active: Arc<AtomicBool>,
    stop_retry_after: Arc<AtomicU64>,
    kill_requested: Arc<AtomicBool>,
    stop_start_error_reported: Arc<AtomicBool>,
    host_memory: HostMemoryCustody,
}

enum State {
    Empty,
    Reserved,
    Retained(WorkerResources),
}

pub(super) struct WorkerCustody(Mutex<State>);
impl Default for WorkerCustody {
    fn default() -> Self {
        Self(Mutex::new(State::Empty))
    }
}

impl WorkerCustody {
    pub(super) fn is_quiescent(&self) -> bool {
        self.0.try_lock().is_ok_and(|state| match &*state {
            State::Empty => true,
            State::Reserved => false,
            State::Retained(worker) => worker.reconcile().unwrap_or(false),
        })
    }

    pub(super) fn reserve(self: &Arc<Self>) -> io::Result<Reservation> {
        let previous = {
            let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
            if matches!(*state, State::Reserved) {
                return Err(io::Error::other("GPU worker custody is reserved"));
            }
            std::mem::replace(&mut *state, State::Reserved)
        };
        let mut reservation = Reservation {
            owner: Arc::clone(self),
            host_memory: None,
            retained: match previous {
                State::Retained(worker) => Some(worker),
                _ => None,
            },
        };
        if let Some(worker) = &reservation.retained {
            if !worker.reconcile()? {
                return Err(io::Error::other("GPU worker custody remains occupied"));
            }
            drop(reservation.retained.take());
        }
        Ok(reservation)
    }
}

impl WorkerResources {
    fn reconcile(&self) -> io::Result<bool> {
        if self.identity_lost.load(Ordering::Acquire) {
            return Err(rustix::io::Errno::CHILD.into());
        }
        if self.stop_active.load(Ordering::Acquire) {
            return Ok(false);
        }
        if self.exited.load(Ordering::Acquire) {
            return self.release_host_memory_if_settled();
        }
        let mut child = self.child.try_lock().map_err(|error| match error {
            std::sync::TryLockError::WouldBlock => io::Error::from(io::ErrorKind::WouldBlock),
            std::sync::TryLockError::Poisoned(_) => {
                io::Error::other("GPU worker child lock poisoned")
            }
        })?;
        if self.identity_lost.load(Ordering::Acquire) {
            return Err(rustix::io::Errno::CHILD.into());
        }
        match child.try_wait() {
            Ok(Some(_)) => {
                self.exited.store(true, Ordering::Release);
                self.release_host_memory_if_settled()
            }
            Ok(None) => Ok(false),
            Err(error) => {
                if error.raw_os_error() == Some(rustix::io::Errno::CHILD.raw_os_error()) {
                    self.identity_lost.store(true, Ordering::Release);
                }
                Err(error)
            }
        }
    }

    fn release_host_memory_if_settled(&self) -> io::Result<bool> {
        self.host_memory.release_if_settled(
            self.identity_lost.load(Ordering::Acquire),
            self.exited.load(Ordering::Acquire),
            self.stop_active.load(Ordering::Acquire),
            self.reaper_active.load(Ordering::Acquire),
        )
    }
}

pub(super) struct Reservation {
    owner: Arc<WorkerCustody>,
    retained: Option<WorkerResources>,
    host_memory: Option<MemoryReservation>,
}
impl Reservation {
    /// Attach the grant before spawning; ownership then follows the child.
    pub(super) fn with_host_memory(mut self, reservation: MemoryReservation) -> io::Result<Self> {
        if self.host_memory.is_some() {
            return Err(io::Error::other("worker memory is already reserved"));
        }
        self.host_memory = Some(reservation);
        Ok(self)
    }

    pub(super) fn bind(mut self, child: Child) -> WorkerResources {
        let pid = child.id();
        let worker = WorkerResources {
            pid,
            child: Arc::new(Mutex::new(child)),
            identity_lost: Arc::new(AtomicBool::new(false)),
            exited: Arc::new(AtomicBool::new(false)),
            reaper_active: Arc::new(AtomicBool::new(false)),
            stop_active: Arc::new(AtomicBool::new(false)),
            stop_retry_after: Arc::new(AtomicU64::new(0)),
            kill_requested: Arc::new(AtomicBool::new(false)),
            stop_start_error_reported: Arc::new(AtomicBool::new(false)),
            host_memory: HostMemoryCustody::new(self.host_memory.take()),
        };
        self.retained = Some(worker.clone());
        worker
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        let next = self.retained.take().map_or(State::Empty, State::Retained);
        *self
            .owner
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = next;
    }
}

pub(super) trait WorkerProcessControl: Send {
    fn id(&self) -> u32;
    fn try_wait(&mut self) -> std::io::Result<bool>;
    fn kill(&mut self) -> std::io::Result<()>;
    fn reap_in_background(&mut self) -> std::io::Result<()>;
}

#[derive(Clone)]
pub(super) struct ChildWorkerProcess {
    pid: u32,
    pub(super) child: std::sync::Arc<std::sync::Mutex<Child>>,
    identity_lost: Arc<AtomicBool>,
    exited: Arc<AtomicBool>,
    reaper_active: Arc<AtomicBool>,
    stop_active: Arc<AtomicBool>,
    stop_retry_after: Arc<AtomicU64>,
    kill_requested: Arc<AtomicBool>,
    stop_start_error_reported: Arc<AtomicBool>,
}

impl ChildWorkerProcess {
    #[cfg(test)]
    pub(super) fn new(child: Child) -> Self {
        let owner = Arc::new(WorkerCustody::default());
        let Ok(reservation) = owner.reserve() else {
            panic!("empty test worker slot")
        };
        Self::from_resources(reservation.bind(child))
    }

    pub(super) fn from_resources(worker: WorkerResources) -> Self {
        let pid = worker.pid;
        Self {
            pid,
            child: worker.child,
            identity_lost: worker.identity_lost,
            exited: worker.exited,
            reaper_active: worker.reaper_active,
            stop_active: worker.stop_active,
            stop_retry_after: worker.stop_retry_after,
            kill_requested: worker.kill_requested,
            stop_start_error_reported: worker.stop_start_error_reported,
        }
    }

    pub(super) fn stop_is_quiescent(&self) -> bool {
        !self.stop_active.load(Ordering::Acquire) && !self.reaper_active.load(Ordering::Acquire)
    }

    pub(super) fn start_stop(&self) -> io::Result<()> {
        let result = self.start_stop_with(|task| {
            std::thread::Builder::new()
                .name("ramshared-gpu-worker-stop".into())
                .spawn(task)
                .map(drop)
        });
        match &result {
            Ok(()) => self
                .stop_start_error_reported
                .store(false, Ordering::Release),
            Err(error)
                if error.kind() != io::ErrorKind::WouldBlock
                    && !self.stop_start_error_reported.swap(true, Ordering::AcqRel) =>
            {
                eprintln!("[ramsharedd] GPU worker stop admission failed: {error}");
            }
            Err(_) => {}
        }
        result
    }

    fn start_stop_with(
        &self,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> io::Result<()>,
    ) -> io::Result<()> {
        if self.identity_lost.load(Ordering::Acquire) || self.exited.load(Ordering::Acquire) {
            return Err(io::Error::other(
                "GPU worker is no longer eligible for stop",
            ));
        }
        if stop_clock_ms() < self.stop_retry_after.load(Ordering::Acquire) {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        self.stop_active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| io::Error::from(io::ErrorKind::WouldBlock))?;
        let mut admission = StopAdmission {
            active: Arc::clone(&self.stop_active),
            retry_after: Arc::clone(&self.stop_retry_after),
            retry: false,
        };
        if stop_clock_ms() < self.stop_retry_after.load(Ordering::Acquire) {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        if self.identity_lost.load(Ordering::Acquire) || self.exited.load(Ordering::Acquire) {
            return Err(io::Error::other(
                "GPU worker is no longer eligible for stop",
            ));
        }
        let mut process = self.clone();
        admission.retry = true;
        spawn(Box::new(move || {
            let mut admission = admission;
            let outcome = super::stop_worker_process(
                &mut process,
                std::time::Duration::from_secs(5),
                std::time::Duration::from_millis(500),
                std::time::Duration::from_millis(10),
            );
            if outcome != super::WorkerStopOutcome::ReaperUnavailable {
                admission.retry = false;
            }
        }))
    }

    fn try_child(&self) -> std::io::Result<std::sync::MutexGuard<'_, Child>> {
        if self.identity_lost.load(Ordering::Acquire) {
            return Err(rustix::io::Errno::CHILD.into());
        }
        let child = self.child.try_lock().map_err(|error| match error {
            std::sync::TryLockError::WouldBlock => {
                std::io::Error::from(std::io::ErrorKind::WouldBlock)
            }
            std::sync::TryLockError::Poisoned(_) => {
                std::io::Error::other("GPU worker child lock poisoned")
            }
        })?;
        if self.identity_lost.load(Ordering::Acquire) {
            return Err(rustix::io::Errno::CHILD.into());
        }
        Ok(child)
    }
}

struct StopAdmission {
    active: Arc<AtomicBool>,
    retry_after: Arc<AtomicU64>,
    retry: bool,
}
impl Drop for StopAdmission {
    fn drop(&mut self) {
        if self.retry {
            self.retry_after.store(
                stop_clock_ms().saturating_add(STOP_RETRY_DELAY_MS),
                Ordering::Release,
            );
        }
        self.active.store(false, Ordering::Release);
    }
}

impl WorkerProcessControl for ChildWorkerProcess {
    fn id(&self) -> u32 {
        self.pid
    }

    fn try_wait(&mut self) -> std::io::Result<bool> {
        if self.identity_lost.load(Ordering::Acquire) {
            return Err(rustix::io::Errno::CHILD.into());
        }
        if self.exited.load(Ordering::Acquire) {
            return Ok(true);
        }
        let mut child = self.try_child()?;
        let result = child.try_wait();
        if result.as_ref().is_err_and(|error| {
            error.raw_os_error() == Some(rustix::io::Errno::CHILD.raw_os_error())
        }) {
            self.identity_lost.store(true, Ordering::Release);
        }
        if result.as_ref().is_ok_and(Option::is_some) {
            self.exited.store(true, Ordering::Release);
        }
        result.map(|status| status.is_some())
    }

    fn kill(&mut self) -> std::io::Result<()> {
        let mut child = self.try_child()?;
        if self.exited.load(Ordering::Acquire) {
            return Err(std::io::Error::other("GPU worker has exited"));
        }
        if self.kill_requested.load(Ordering::Acquire) {
            return Ok(());
        }
        child.kill()?;
        self.kill_requested.store(true, Ordering::Release);
        Ok(())
    }

    fn reap_in_background(&mut self) -> std::io::Result<()> {
        if self.identity_lost.load(Ordering::Acquire) {
            return Err(rustix::io::Errno::CHILD.into());
        }
        if self.exited.load(Ordering::Acquire) {
            return Err(std::io::Error::other("GPU worker has exited"));
        }
        if self
            .reaper_active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        if self.identity_lost.load(Ordering::Acquire) || self.exited.load(Ordering::Acquire) {
            self.reaper_active.store(false, Ordering::Release);
            return Err(std::io::Error::other(
                "GPU worker is no longer eligible for a reaper",
            ));
        }
        let child = std::sync::Arc::clone(&self.child);
        let identity = Arc::clone(&self.identity_lost);
        let exited = Arc::clone(&self.exited);
        let active = Arc::clone(&self.reaper_active);
        let result = std::thread::Builder::new()
            .name("ramshared-gpu-worker-reaper".into())
            .spawn(move || {
                if let Ok(mut child) = child.lock()
                    && !identity.load(Ordering::Acquire)
                    && !exited.load(Ordering::Acquire)
                {
                    match child.wait() {
                        Ok(_) => exited.store(true, Ordering::Release),
                        Err(error)
                            if error.raw_os_error()
                                == Some(rustix::io::Errno::CHILD.raw_os_error()) =>
                        {
                            identity.store(true, Ordering::Release)
                        }
                        Err(_) => {}
                    }
                }
                active.store(false, Ordering::Release);
            });
        if result.is_err() {
            self.reaper_active.store(false, Ordering::Release);
        }
        result.map(drop)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    fn held_child() -> (Child, std::process::ChildStdin) {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "read line"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        (child, input)
    }

    #[test]
    fn admission_retains_live_child_and_recovers_after_exit() {
        let owner = Arc::new(WorkerCustody::default());
        let reservation = owner.reserve().unwrap();
        assert!(owner.reserve().is_err());
        let (child, input) = held_child();
        let worker = reservation.bind(child);
        assert!(worker.child.lock().unwrap().try_wait().unwrap().is_none());
        let weak = Arc::downgrade(&worker.child);
        drop(worker);
        assert!(owner.reserve().is_err());
        assert!(weak.upgrade().is_some());
        drop(input);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match owner.reserve() {
                Ok(reservation) => {
                    drop(reservation);
                    break;
                }
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(error) => panic!("child custody did not settle: {error}"),
            }
        }
        assert!(weak.upgrade().is_none());
        assert!(owner.reserve().is_ok());
    }

    #[test]
    fn reservation_and_unwind_release_only_empty_capacity() {
        let owner = Arc::new(WorkerCustody::default());
        let result = std::panic::catch_unwind(|| {
            let _reservation = owner.reserve().unwrap();
            assert!(owner.reserve().is_err());
            panic!("injected unwind");
        });
        assert!(result.is_err());
        assert!(owner.reserve().is_ok());
    }

    #[test]
    fn stop_admission_is_single_flight_and_fences_exit_until_controller_drops() {
        let owner = Arc::new(WorkerCustody::default());
        let (child, input) = held_child();
        let worker = owner.reserve().unwrap().bind(child);
        let mut process = ChildWorkerProcess::from_resources(worker.clone());
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        process
            .start_stop_with(|task| {
                tx.send(task).unwrap();
                Ok(())
            })
            .unwrap();
        let held_stop = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(
            process.start_stop().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(input);
        let deadline = Instant::now() + Duration::from_secs(3);
        while !process.try_wait().unwrap() {
            assert!(Instant::now() < deadline, "child did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!process.stop_is_quiescent());
        assert!(
            owner.reserve().is_err(),
            "exit alone must not release the stop controller"
        );
        drop(held_stop);
        assert!(process.stop_is_quiescent());
        assert!(owner.reserve().is_ok());
    }

    #[test]
    fn native_stop_thread_creation_failure_retains_child_and_allows_retry() {
        let owner = Arc::new(WorkerCustody::default());
        let (child, input) = held_child();
        let worker = owner.reserve().unwrap().bind(child);
        let mut process = ChildWorkerProcess::from_resources(worker.clone());
        let failure = process.start_stop_with(|task| {
            std::thread::Builder::new()
                .stack_size(usize::MAX / 2)
                .spawn(task)
                .map(drop)
        });
        assert!(
            failure.is_err(),
            "an impossible native stack must refuse thread creation"
        );
        assert!(process.stop_is_quiescent());
        assert!(
            owner.reserve().is_err(),
            "failed thread creation must retain the live child"
        );
        assert_eq!(
            process.start_stop().unwrap_err().kind(),
            io::ErrorKind::WouldBlock,
            "a failed native spawn must not be retried on every serving request"
        );
        std::thread::sleep(Duration::from_millis(300));
        process.start_stop().unwrap();
        assert!(worker.stop_active.load(Ordering::Acquire));
        drop(input);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match owner.reserve() {
                Ok(reservation) => {
                    drop(reservation);
                    break;
                }
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(error) => panic!("stop controller did not settle: {error}"),
            }
        }
        assert!(process.try_wait().unwrap());
    }

    #[test]
    fn stop_task_unwind_clears_only_its_own_admission() {
        let owner = Arc::new(WorkerCustody::default());
        let (child, input) = held_child();
        let worker = owner.reserve().unwrap().bind(child);
        let process = ChildWorkerProcess::from_resources(worker);
        process
            .start_stop_with(|task| {
                assert!(
                    std::thread::spawn(move || {
                        let _task = task;
                        panic!("injected stop task unwind");
                    })
                    .join()
                    .is_err()
                );
                Ok(())
            })
            .unwrap();
        assert!(process.stop_is_quiescent());
        assert!(owner.reserve().is_err());
        drop(input);
        let deadline = Instant::now() + Duration::from_secs(3);
        while owner.reserve().is_err() {
            assert!(Instant::now() < deadline, "child custody did not settle");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn accepted_kill_is_not_replayed_on_retry() {
        let owner = Arc::new(WorkerCustody::default());
        let (child, input) = held_child();
        let worker = owner.reserve().unwrap().bind(child);
        let mut process = ChildWorkerProcess::from_resources(worker.clone());
        // Model a prior accepted signal while the child is still observable.
        worker.kill_requested.store(true, Ordering::Release);
        process.kill().unwrap();
        assert!(!process.try_wait().unwrap(), "retry sent another signal");
        drop(input);
        let deadline = Instant::now() + Duration::from_secs(3);
        while !process.try_wait().unwrap() {
            assert!(Instant::now() < deadline, "fixture child did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }

        let (child, input) = held_child();
        let worker = owner.reserve().unwrap().bind(child);
        let mut process = ChildWorkerProcess::from_resources(worker.clone());
        process.kill().unwrap();
        assert!(
            worker.kill_requested.load(Ordering::Acquire),
            "accepted kill was not recorded"
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        while !process.try_wait().unwrap() {
            assert!(Instant::now() < deadline, "killed child did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(input);
    }

    #[test]
    fn lost_identity_and_active_reaper_refuse_reuse() {
        let owner = Arc::new(WorkerCustody::default());
        let (child, input) = held_child();
        let worker = owner.reserve().unwrap().bind(child);
        worker.reaper_active.store(true, Ordering::Release);
        worker.exited.store(true, Ordering::Release);
        assert!(owner.reserve().is_err());
        worker.exited.store(false, Ordering::Release);
        worker.reaper_active.store(false, Ordering::Release);
        worker.identity_lost.store(true, Ordering::Release);
        assert_eq!(
            owner.reserve().err().unwrap().raw_os_error(),
            Some(rustix::io::Errno::CHILD.raw_os_error())
        );
        drop(input);
        worker.child.lock().unwrap().wait().unwrap();
    }

    #[test]
    fn concurrent_reservation_is_refused_without_waiting_for_owner() {
        let owner = Arc::new(WorkerCustody::default());
        let held = owner.reserve().unwrap();
        let other = Arc::clone(&owner);
        assert!(
            std::thread::spawn(move || other.reserve().is_err())
                .join()
                .unwrap()
        );
        drop(held);
        assert!(owner.reserve().is_ok());
    }

    #[test]
    fn externally_reaped_child_latches_identity_loss_in_slot() {
        let owner = Arc::new(WorkerCustody::default());
        let child = Command::new("/bin/true").spawn().unwrap();
        let pid = rustix::process::Pid::from_raw(child.id() as i32).unwrap();
        let worker = owner.reserve().unwrap().bind(child);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match rustix::process::waitid(
                rustix::process::WaitId::Pid(pid),
                rustix::process::WaitIdOptions::EXITED | rustix::process::WaitIdOptions::NOHANG,
            )
            .unwrap()
            {
                Some(_) => break,
                None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
                None => panic!("fixture child did not exit"),
            }
        }
        for _ in 0..2 {
            assert_eq!(
                owner.reserve().err().unwrap().raw_os_error(),
                Some(rustix::io::Errno::CHILD.raw_os_error())
            );
            assert!(worker.identity_lost.load(Ordering::Acquire));
        }
    }

    #[test]
    fn repeated_reaper_admission_keeps_one_owned_child_until_thread_finishes() {
        let owner = Arc::new(WorkerCustody::default());
        let (child, input) = held_child();
        let worker = owner.reserve().unwrap().bind(child);
        let mut process = ChildWorkerProcess::from_resources(worker.clone());
        process.reap_in_background().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while worker.child.try_lock().is_ok() {
            assert!(Instant::now() < deadline, "reaper did not claim the child");
            std::thread::yield_now();
        }
        assert_eq!(
            process.reap_in_background().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(worker.reaper_active.load(Ordering::Acquire));
        assert!(owner.reserve().is_err());
        drop(input);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match owner.reserve() {
                Ok(reservation) => {
                    drop(reservation);
                    break;
                }
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(error) => panic!("reaper did not finish: {error}"),
            }
        }
        assert!(!worker.reaper_active.load(Ordering::Acquire));
    }

    #[test]
    fn concurrent_reaper_requests_create_only_one_owned_thread() {
        let owner = Arc::new(WorkerCustody::default());
        let (child, input) = held_child();
        let worker = owner.reserve().unwrap().bind(child);
        let guard = worker.child.lock().unwrap();
        let baseline = Arc::strong_count(&worker.child);
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let worker = worker.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let mut process = ChildWorkerProcess::from_resources(worker);
                    barrier.wait();
                    match process.reap_in_background() {
                        Ok(()) => true,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => false,
                        Err(error) => panic!("unexpected reaper admission error: {error}"),
                    }
                })
            })
            .collect();
        let admitted = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .filter(|admitted| *admitted)
            .count();
        assert_eq!(
            admitted, 1,
            "only the creating request may report admission"
        );
        assert_eq!(
            Arc::strong_count(&worker.child),
            baseline + 1,
            "only one reaper may hold the child"
        );
        assert!(worker.reaper_active.load(Ordering::Acquire));
        drop(guard);
        drop(input);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match owner.reserve() {
                Ok(reservation) => {
                    drop(reservation);
                    break;
                }
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(error) => panic!("worker custody did not settle: {error}"),
            }
        }
    }
}
