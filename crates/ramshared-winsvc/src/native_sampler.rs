// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/native_sampler.rs
//! Independent persistent native metadata owners sharing one custody engine.
use crate::gpu_identity::process::NativeProcess;
use crate::gpu_identity::supervisor::{ProbeReport, ProbeSupervisor, ProbeTick, elapsed_age_ms};
use crate::thread_custody::{OwnedThread, StopSignal, ThreadState};
use ramshared_vram::GPU_BUDGET_MAX_AGE_MS;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
const POLL: Duration = Duration::from_millis(10);
const QUERY_BUDGET: Duration = Duration::from_millis((GPU_BUDGET_MAX_AGE_MS * 2) / 5);
const REFRESH_CADENCE: Duration = Duration::from_millis(GPU_BUDGET_MAX_AGE_MS / 5);
const IDLE_REFRESH_CADENCE: Duration = Duration::from_millis(GPU_BUDGET_MAX_AGE_MS / 2);
#[derive(Clone)]
pub enum ProbeKind {
    Gpu,
    Vm,
    Storage(PathBuf),
    StorageDisk([u8; 16]),
}
pub struct NativeSampler<R> {
    owner: OnceLock<Mutex<ProbeSupervisor<NativeProcess, R>>>,
    published: Mutex<Option<(R, Instant, Instant)>>,
    task: Mutex<Option<OwnedThread>>,
    stopped: AtomicBool,
    stop_requested: AtomicBool,
    paused: AtomicBool,
    idle: AtomicBool,
    last_failure: Mutex<Option<String>>,
}
impl<R: ProbeReport + Send + 'static> NativeSampler<R> {
    pub const fn new() -> Self {
        Self {
            owner: OnceLock::new(),
            published: Mutex::new(None),
            task: Mutex::new(None),
            stopped: AtomicBool::new(false),
            stop_requested: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            idle: AtomicBool::new(false),
            last_failure: Mutex::new(None),
        }
    }
    /// Pause new probe spawns while the consumer holds an active session and does not
    /// need periodic re-enumeration (e.g. VM discovery during an established vsock lease).
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Release);
    }
    /// Hint whether the consumer is idle (disconnected) so the sampler can use a
    /// gentler refresh interval while staying strictly inside `GPU_BUDGET_MAX_AGE_MS`.
    pub fn set_idle(&self, idle: bool) {
        self.idle.store(idle, Ordering::Release);
    }
    fn refresh_cadence(&self) -> Duration {
        if self.idle.load(Ordering::Acquire) {
            IDLE_REFRESH_CADENCE
        } else {
            REFRESH_CADENCE
        }
    }
    fn owner(&self) -> &Mutex<ProbeSupervisor<NativeProcess, R>> {
        self.owner
            .get_or_init(|| Mutex::new(ProbeSupervisor::default()))
    }
    pub fn start(&'static self, executable: PathBuf, kind: ProbeKind) -> Result<(), String> {
        if !executable.is_absolute() || !executable.is_file() {
            return Err("native sampler requires the verified absolute product executable".into());
        }
        if let ProbeKind::Storage(path) = &kind {
            crate::gpu_identity::process::storage_arguments(path)?;
        }
        self.launch(|| {
            OwnedThread::spawn(
                match &kind {
                    ProbeKind::Gpu => "ramshared-native-gpu-sampler",
                    ProbeKind::Vm => "ramshared-native-vm-sampler",
                    ProbeKind::Storage(_) | ProbeKind::StorageDisk(_) => {
                        "ramshared-native-storage-sampler"
                    }
                },
                move |stop| self.run_scoped(|| self.run(executable, kind, &stop)),
            )
        })
    }
    fn launch<F>(&'static self, spawn: F) -> Result<(), String>
    where
        F: FnOnce() -> std::io::Result<OwnedThread>,
    {
        let mut slot = self.task.lock().unwrap_or_else(|error| error.into_inner());
        if self.stop_requested.load(Ordering::Acquire) {
            return Err("native sampler already stopped; replacement is refused".into());
        }
        if slot.is_some() {
            return Err("native sampler already started; replacement is refused".into());
        }
        *slot = Some(spawn().map_err(|error| error.to_string())?);
        Ok(())
    }
    fn publish_snapshot(&self, entry: Option<(R, Instant, Instant)>) {
        *self
            .published
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = entry;
    }
    fn run(&self, executable: PathBuf, kind: ProbeKind, stop: &StopSignal) {
        let mut next = Instant::now();
        let mut was_paused = false;
        let mut was_idle = false;
        loop {
            let (snapshot, exit_loop) = {
                let mut custody = self
                    .owner()
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                let stopping = stop.is_requested();
                if stopping {
                    custody.cancel();
                }
                let paused = self.paused.load(Ordering::Acquire);
                let idle = self.idle.load(Ordering::Acquire);
                if (was_paused && !paused) || (was_idle && !idle) {
                    next = Instant::now();
                }
                was_paused = paused;
                was_idle = idle;
                let mut exit_loop = false;
                if !custody.busy() {
                    if stopping {
                        exit_loop = true;
                    } else if !paused {
                        let now = Instant::now();
                        if now >= next {
                            let admission = custody.start(
                                || match &kind {
                                    ProbeKind::Gpu => NativeProcess::spawn_identity(&executable),
                                    ProbeKind::Vm => NativeProcess::spawn_vm_identity(&executable),
                                    ProbeKind::Storage(path) => {
                                        NativeProcess::spawn_storage(&executable, path)
                                    }
                                    ProbeKind::StorageDisk(disk_id) => {
                                        NativeProcess::spawn_storage_disk(&executable, *disk_id)
                                    }
                                },
                                now,
                                unix_ms(),
                                QUERY_BUDGET,
                            );
                            if let Err(error) = admission {
                                self.record_failure(Some(error));
                            }
                            next = now + self.refresh_cadence();
                        }
                    }
                }
                if !exit_loop
                    && let ProbeTick::Completed(result) = custody.tick(Instant::now(), unix_ms())
                {
                    let failed = result.is_err();
                    self.record_failure(result.err());
                    let now = Instant::now();
                    next = if failed && custody.snapshot_entry(now).is_none() {
                        now + POLL
                    } else {
                        next.max(now)
                    };
                }
                let snapshot = if exit_loop || self.stop_requested.load(Ordering::Acquire) {
                    None
                } else {
                    custody
                        .snapshot_entry(Instant::now())
                        .map(|(report, start, expires)| (report.clone(), start, expires))
                };
                (snapshot, exit_loop)
            };
            // Publish outside `owner()` so child-process creation and pipe
            // polling never stall or drop concurrent heartbeat readers.
            self.publish_snapshot(snapshot);
            if exit_loop {
                break;
            }
            std::thread::sleep(POLL);
        }
    }
    fn run_scoped<F: FnOnce()>(&self, run: F) -> std::io::Result<()> {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).is_err() {
            self.stop_requested.store(true, Ordering::Release);
            self.publish_snapshot(None);
            self.record_failure(Some("native sampler actor panicked".into()));
            // Never resume partially mutated actor state: cancel its same owned
            // process supervisor and keep the failed thread owned through cleanup.
            while !self.settle_processes() {
                std::thread::sleep(POLL);
            }
            return Err(std::io::Error::other("native sampler actor panicked"));
        }
        Ok(())
    }
    fn settle_processes(&self) -> bool {
        let settled = {
            let mut custody = self
                .owner()
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            custody.cancel();
            if custody.busy() {
                let _ = custody.tick(Instant::now(), unix_ms());
            }
            !custody.busy()
        };
        self.publish_snapshot(None);
        settled
    }
    pub fn observation(&self) -> Option<R> {
        self.observation_with_age().map(|(report, _)| report)
    }
    pub fn observation_with_age(&self) -> Option<(R, u64)> {
        let (report, start) = self.observation_with_start()?;
        let age = elapsed_age_ms(start, Instant::now())?;
        Some((report, age))
    }
    pub fn observation_with_start(&self) -> Option<(R, Instant)> {
        if self.stop_requested.load(Ordering::Acquire) {
            return None;
        }
        let guard = self.published.lock().ok()?;
        let (report, start, expires) = guard.as_ref()?;
        let now = Instant::now();
        if now < *start || now >= *expires {
            return None;
        }
        Some((report.clone(), *start))
    }
    pub fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
    fn record_failure(&self, error: Option<String>) {
        *self
            .last_failure
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = error;
    }
    /// Last observed admission/query failure, never a source of authority.
    pub fn diagnostic(&self) -> Option<String> {
        self.last_failure.try_lock().ok()?.clone()
    }
    /// Thread and producer-process completion are independent stop prerequisites.
    pub fn stop(&self, budget: Duration) -> Result<ThreadState, String> {
        if Instant::now().checked_add(budget).is_none() {
            return Err("invalid sampler stop observation deadline".into());
        }
        self.stop_requested.store(true, Ordering::Release);
        self.publish_snapshot(None);
        let mut slot = self.task.lock().unwrap_or_else(|error| error.into_inner());
        let (result, joined) = match slot.as_mut() {
            Some(task) => {
                let result = task.wait_for_stop(budget);
                (result, task.has_joined())
            }
            None => (Ok(ThreadState::Stopped), true),
        };
        if let Err(error) = &result {
            self.record_failure(Some(error.to_string()));
        }
        if !joined {
            return Ok(ThreadState::Pending);
        }
        // An actor panic leaves the static process supervisor owned here. Drive
        // its same cancellation/settlement algorithm, never abandon the child.
        if !self.settle_processes() {
            return Ok(ThreadState::Pending);
        }
        self.stopped.store(true, Ordering::Release);
        result
            .map(|_| ThreadState::Stopped)
            .map_err(|error| error.to_string())
    }
    pub fn stop_and_wait(&self) -> Result<(), String> {
        loop {
            match self.stop(Duration::from_secs(1))? {
                ThreadState::Stopped => return Ok(()),
                ThreadState::Pending => std::thread::sleep(POLL),
            }
        }
    }
}
fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
