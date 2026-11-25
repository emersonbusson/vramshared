// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/budget_observer.rs
//! Persistent metadata custody in the existing pressure supervisor; no tier effects.
mod transport;
use ramshared_exec::retained_process::{NativeProcessControl, ProcessControl, ServingCommandOwner};
use ramshared_ipc::budget_observation::{LEASE_PATH, MAX_BYTES, Packet, Report, SOCKET_NAME};
use ramshared_vram::GPU_BUDGET_MAX_AGE_MS;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
const PERIOD: Duration = Duration::from_millis(GPU_BUDGET_MAX_AGE_MS / 2);
const POLL: Duration = Duration::from_millis(10);
struct State<C: ProcessControl = NativeProcessControl> {
    owner: ServingCommandOwner<C>,
    sample: Mutex<Option<(Report, Instant)>>,
    stop: AtomicBool,
    failure: Mutex<Option<String>>,
}
impl<C: ProcessControl> State<C> {
    fn new(control: C) -> Self {
        Self {
            owner: ServingCommandOwner::new(control),
            sample: Mutex::new(None),
            stop: AtomicBool::new(false),
            failure: Mutex::new(None),
        }
    }
    fn failed(&self, error: &str) {
        self.clear();
        let message: String = error
            .chars()
            .filter(|c| !c.is_control())
            .take(512)
            .collect();
        let mut last = self.failure.lock().unwrap_or_else(|e| e.into_inner());
        if last.as_deref() != Some(message.as_str()) {
            eprintln!("[ramshared] Off budget sample unavailable: {message}");
            *last = Some(message);
        }
    }
    fn clear(&self) {
        *self.sample.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
    fn packet(&self) -> Packet {
        if self.stop.load(Ordering::Acquire) {
            return Packet {
                report: None,
                age_ms: 0,
            };
        }
        let sample = self.sample.lock().unwrap_or_else(|e| e.into_inner());
        sample
            .as_ref()
            .and_then(|(report, start)| {
                let age_ms = u64::try_from(start.elapsed().as_nanos().div_ceil(1_000_000)).ok()?;
                (age_ms < GPU_BUDGET_MAX_AGE_MS).then(|| Packet {
                    report: Some(report.clone()),
                    age_ms,
                })
            })
            .unwrap_or(Packet {
                report: None,
                age_ms: 0,
            })
    }
    fn settle(&self) {
        self.clear();
        while self.owner.try_shutdown().is_none() {
            std::thread::sleep(POLL);
        }
    }
}
pub(crate) struct Observer {
    state: &'static State,
    query: Option<JoinHandle<()>>,
    server: Option<JoinHandle<()>>,
}
impl Drop for Observer {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Release);
        self.state.clear();
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
        if let Some(query) = self.query.take() {
            let _ = query.join();
        }
        self.state.settle();
    }
}
fn unix_ms() -> Result<u64, String> {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_millis(),
    )
    .map_err(|e| e.to_string())
}
fn query_once<C: ProcessControl>(
    state: &'static State<C>,
    command: &mut Command,
    start: Instant,
    stamp: u64,
) -> Result<Report, String> {
    let output = state
        .owner
        .run_recorded(command, PERIOD, MAX_BYTES)
        .map_err(|e| e.to_string())?;
    if !output.output.stderr.is_empty() {
        return Err("native query wrote unexpected diagnostics".into());
    }
    let now = unix_ms()?;
    let report = Report::decode(&output.output.stdout, now)?;
    if report.pid != output.pid
        || report.started_unix_ms < stamp
        || u128::from(report.elapsed_ms) > start.elapsed().as_millis()
    {
        return Err("probe changed its actual child identity or query window".into());
    }
    Ok(report)
}
fn run_queries_with<C: ProcessControl>(
    state: &'static State<C>,
    lease: PathBuf,
    uid: u32,
    mut make: impl FnMut(File) -> Command,
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut next = Instant::now();
        while !state.stop.load(Ordering::Acquire) {
            if !state.owner.reconcile() {
                state.clear();
                std::thread::sleep(POLL);
                continue;
            }
            if Instant::now() < next {
                std::thread::sleep(POLL);
                continue;
            }
            let start = Instant::now();
            next = start + PERIOD;
            let result = (|| {
                let stamp = unix_ms()?;
                let held = ramshared_exec::inherited_lease::acquire(&lease, uid)
                    .map_err(|e| e.to_string())?;
                let mut command = make(held);
                query_once(state, &mut command, start, stamp)
            })();
            match result {
                Ok(report) => {
                    *state.failure.lock().unwrap_or_else(|e| e.into_inner()) = None;
                    let mut sample = state.sample.lock().unwrap_or_else(|e| e.into_inner());
                    if !state.stop.load(Ordering::Acquire) {
                        *sample = Some((report, start));
                    }
                }
                Err(error) => state.failed(&error),
            }
        }
    }));
    if result.is_err() {
        state.stop.store(true, Ordering::Release);
    }
    state.settle();
}
pub(crate) fn start() -> Result<Observer, String> {
    static STATE: OnceLock<State> = OnceLock::new();
    let image = transport::probe_image()?;
    let listener = transport::bind(SOCKET_NAME)?;
    let state = STATE.get_or_init(|| State::new(NativeProcessControl));
    start_with(state, listener, move || {
        run_queries_with(state, PathBuf::from(LEASE_PATH), 0, |held| {
            let mut command = Command::new(format!("/proc/self/fd/{}", image.as_raw_fd()));
            command.arg("__gpu_budget_probe").stdin(Stdio::from(held));
            command
        })
    })
}
fn start_with(
    state: &'static State,
    listener: std::os::unix::net::UnixListener,
    query: impl FnOnce() + Send + 'static,
) -> Result<Observer, String> {
    if state.stop.load(Ordering::Acquire) {
        return Err("stopped observer cannot restart".into());
    }
    let mut observer = Observer {
        state,
        query: None,
        server: None,
    };
    observer.server = Some(
        std::thread::Builder::new()
            .name("ramshared-budget-response".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(|| {
                    while !state.stop.load(Ordering::Acquire) {
                        match listener.accept() {
                            Ok((stream, _)) => {
                                let _ = transport::send(stream, &state.packet());
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::sleep(POLL)
                            }
                            Err(_) => break,
                        }
                    }
                });
                let _ = result;
                state.stop.store(true, Ordering::Release);
                state.clear();
            })
            .map_err(|e| e.to_string())?,
    );
    observer.query = Some(
        std::thread::Builder::new()
            .name("ramshared-budget-query".into())
            .spawn(query)
            .map_err(|e| e.to_string())?,
    );
    Ok(observer)
}

pub(crate) fn off_budgets(now: u64) -> Option<Vec<ramshared_vram::GpuBudgetTelemetry>> {
    off_budgets_with(
        now,
        || crate::cascade::preparation_is_off().ok() == Some(true),
        transport::read,
    )
}

fn off_budgets_with(
    now: u64,
    prep_is_off: impl Fn() -> bool,
    read_packet: impl FnOnce(u64) -> Option<Packet>,
) -> Option<Vec<ramshared_vram::GpuBudgetTelemetry>> {
    if !prep_is_off() {
        return None;
    }
    let packet = read_packet(now)?;
    let report = packet.report.filter(|r| r.unavailable_reason.is_none())?;
    prep_is_off().then_some(report.budgets)
}

#[cfg(test)]
mod tests;
