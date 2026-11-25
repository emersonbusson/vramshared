// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-exec/src/retained_process.rs
//! Bounded helper admission that retains uncertain work without exiting its owner.

pub(super) mod native;
pub use native::{NativeProcessControl, ProcessControl};

use crate::bounded_process::{BoundedOutput, ProcessSpawnError};
use crate::capture::{CaptureWorker, CapturedBytes};
use std::fmt;
use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(10);
const CLEANUP_GRACE: Duration = Duration::from_millis(500);

#[derive(Debug)]
pub enum ServingCommandError {
    Busy,
    Closing,
    InvalidDeadline,
    DeadlineExpired,
    Pending { pid: u32, detail: String },
    Failed(ProcessSpawnError),
}
impl fmt::Display for ServingCommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => write!(f, "helper custody remains occupied"),
            Self::Closing => write!(f, "helper admission is frozen for shutdown"),
            Self::InvalidDeadline => write!(f, "helper deadline cannot be represented"),
            Self::DeadlineExpired => write!(f, "helper deadline expired before spawn"),
            Self::Pending { pid, detail } => write!(f, "helper {pid} remains owned: {detail}"),
            Self::Failed(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for ServingCommandError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HelperState {
    Idle,
    Running,
    Pending { pid: u32 },
    Closing,
}

/// Identity comes from the retained native child, never from its output.
pub struct RecordedOutput {
    pub pid: u32,
    pub output: BoundedOutput,
}

enum Phase {
    Idle,
    Running,
    Retained(Box<Flight>),
}
struct Slot {
    phase: Phase,
    closing: bool,
}

/// One owner per serving process. No effect runs under the ownership mutex.
/// Running requires a process-lifetime borrow: a scoped owner cannot start
/// work and then drop its retained child or capture. Shutdown requires a lease.
///
/// ```compile_fail
/// use ramshared_exec::retained_process::ServingCommandOwner;
/// use std::{process::Command, time::Duration};
/// let scoped = ServingCommandOwner::default();
/// // Pending work must never outlive a scoped owner.
/// scoped.run(&mut Command::new("/bin/true"), Duration::from_secs(1), 1024);
/// ```
pub struct ServingCommandOwner<C: ProcessControl = NativeProcessControl> {
    control: C,
    slot: Mutex<Slot>,
}
impl Default for ServingCommandOwner {
    fn default() -> Self {
        Self::new(NativeProcessControl)
    }
}
impl<C: ProcessControl> ServingCommandOwner<C> {
    pub fn new(control: C) -> Self {
        Self {
            control,
            slot: Mutex::new(Slot {
                phase: Phase::Idle,
                closing: false,
            }),
        }
    }
    pub fn state(&self) -> HelperState {
        let slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        match &slot.phase {
            Phase::Idle if slot.closing => HelperState::Closing,
            Phase::Idle => HelperState::Idle,
            Phase::Running => HelperState::Running,
            Phase::Retained(flight) => HelperState::Pending {
                pid: flight.child.id(),
            },
        }
    }
    fn reserve(&self, retained: bool) -> Result<Reservation<'_, C>, ServingCommandError> {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if slot.closing {
            return Err(ServingCommandError::Closing);
        }
        let allowed = matches!(slot.phase, Phase::Idle)
            || retained && matches!(slot.phase, Phase::Retained(_));
        if !allowed {
            return Err(ServingCommandError::Busy);
        }
        let flight = match std::mem::replace(&mut slot.phase, Phase::Running) {
            Phase::Retained(flight) => Some(flight),
            _ => None,
        };
        Ok(Reservation {
            owner: self,
            flight,
        })
    }
    /// Poll only the already-owned operation. No path restart and no wait/join
    /// on an unfinished capture are permitted during reconciliation.
    pub fn reconcile(&self) -> bool {
        let Ok(mut reservation) = self.reserve(true) else {
            return false;
        };
        let Some(flight) = reservation.flight.as_mut() else {
            return true;
        };
        if flight.poll(&self.control, Instant::now()).is_some() {
            reservation.flight.take();
            true
        } else {
            false
        }
    }
    pub fn try_shutdown(&self) -> Option<HelperShutdownLease<'_, C>> {
        self.reconcile();
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if slot.closing || !matches!(slot.phase, Phase::Idle) {
            return None;
        }
        slot.closing = true;
        Some(HelperShutdownLease { owner: self })
    }
    pub fn run(
        &'static self,
        command: &mut Command,
        timeout: Duration,
        limit: usize,
    ) -> Result<BoundedOutput, ServingCommandError> {
        self.run_recorded(command, timeout, limit)
            .map(|result| result.output)
    }

    pub fn run_recorded(
        &'static self,
        command: &mut Command,
        timeout: Duration,
        limit: usize,
    ) -> Result<RecordedOutput, ServingCommandError> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(ServingCommandError::InvalidDeadline)?;
        let final_deadline = deadline
            .checked_add(CLEANUP_GRACE)
            .ok_or(ServingCommandError::InvalidDeadline)?;
        self.reconcile();
        let mut reservation = self.reserve(false)?;
        if Instant::now() >= deadline {
            return Err(ServingCommandError::DeadlineExpired);
        }
        let label = command.get_program().to_string_lossy().into_owned();
        let child = command
            .process_group(0)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                ServingCommandError::Failed(ProcessSpawnError::SpawnFailed {
                    command: label.clone(),
                    kind: error.kind(),
                    detail: error.to_string(),
                })
            })?;
        // Install custody before capture setup can fail or unwind.
        reservation.flight = Some(Box::new(Flight::new(child, label, deadline, timeout)));
        let flight = reservation.flight.as_mut().ok_or_else(|| {
            ServingCommandError::Failed(ProcessSpawnError::GenericError {
                detail: "helper custody was not installed".into(),
            })
        })?;
        flight.start_captures(limit);
        loop {
            let now = Instant::now();
            if let Some(status) = flight.poll(&self.control, now) {
                let flight = reservation.flight.take().ok_or_else(|| {
                    ServingCommandError::Failed(ProcessSpawnError::GenericError {
                        detail: "terminal helper custody disappeared".into(),
                    })
                })?;
                let pid = flight.child.id();
                return (*flight)
                    .finish(status)
                    .map(|output| RecordedOutput { pid, output })
                    .map_err(ServingCommandError::Failed);
            }
            if now >= final_deadline {
                return Err(ServingCommandError::Pending {
                    pid: flight.child.id(),
                    detail: flight.detail(),
                });
            }
            std::thread::sleep(POLL.min(final_deadline.saturating_duration_since(now)));
        }
    }
}

/// Freeze admission until the last backend-stop holder drops this lease.
pub struct HelperShutdownLease<'a, C: ProcessControl = NativeProcessControl> {
    owner: &'a ServingCommandOwner<C>,
}
impl<C: ProcessControl> Drop for HelperShutdownLease<'_, C> {
    fn drop(&mut self) {
        self.owner
            .slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .closing = false;
    }
}

struct Reservation<'a, C: ProcessControl> {
    owner: &'a ServingCommandOwner<C>,
    flight: Option<Box<Flight>>,
}
impl<C: ProcessControl> Drop for Reservation<'_, C> {
    fn drop(&mut self) {
        let phase = self.flight.take().map_or(Phase::Idle, Phase::Retained);
        self.owner
            .slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .phase = phase;
    }
}

enum Completion {
    Status(ExitStatus),
    LostIdentity,
}

struct Flight {
    child: Child,
    label: String,
    deadline: Instant,
    timeout: Duration,
    observed: bool,
    identity_lost: bool,
    capture_deadline: Option<Instant>,
    stop_requested: bool,
    status: Option<ExitStatus>,
    stdout: Option<CaptureWorker>,
    stderr: Option<CaptureWorker>,
    out: Option<CapturedBytes>,
    err: Option<CapturedBytes>,
    failure: Option<ProcessSpawnError>,
}
impl Flight {
    fn new(child: Child, label: String, deadline: Instant, timeout: Duration) -> Self {
        Self {
            child,
            label,
            deadline,
            timeout,
            observed: false,
            identity_lost: false,
            capture_deadline: None,
            stop_requested: false,
            status: None,
            stdout: None,
            stderr: None,
            out: None,
            err: None,
            failure: None,
        }
    }
    fn fail(&mut self, detail: impl Into<String>) {
        if self.failure.is_none() {
            self.failure = Some(ProcessSpawnError::GenericError {
                detail: detail.into(),
            });
        }
    }
    fn start_captures(&mut self, limit: usize) {
        (self.stdout, self.out) = start_capture("stdout", self.child.stdout.take(), limit);
        (self.stderr, self.err) = start_capture("stderr", self.child.stderr.take(), limit);
    }
    fn poll(&mut self, control: &impl ProcessControl, now: Instant) -> Option<Completion> {
        let out = account_capture(&mut self.stdout, &mut self.out);
        let err = account_capture(&mut self.stderr, &mut self.err);
        if let Err(detail) = &out {
            self.fail(detail.clone());
        }
        if let Err(detail) = &err {
            self.fail(detail.clone());
        }
        let captures_done = out.unwrap_or(true) && err.unwrap_or(true);
        if self.identity_lost {
            return (captures_done && self.observe_group_absence(control))
                .then_some(Completion::LostIdentity);
        }
        if let Some(status) = self.status {
            return self
                .observe_group_absence(control)
                .then_some(Completion::Status(status));
        }
        let relationship_observed = self.observe_relationship(control, now);
        if self.identity_lost {
            return (captures_done && self.observe_group_absence(control))
                .then_some(Completion::LostIdentity);
        }
        if now >= self.deadline && !self.observed && self.failure.is_none() {
            self.fail(format!(
                "{} helper observation deadline expired after {} ms",
                self.label,
                self.timeout.as_millis()
            ));
        }
        let results_ready = self.out.is_some() && self.err.is_some();
        if self.observed
            && !results_ready
            && self
                .capture_deadline
                .is_some_and(|deadline| now >= deadline)
        {
            self.fail("helper output remained open after direct-child exit");
        }
        if !self.stop_requested
            && relationship_observed
            && (self.failure.is_some() || self.observed && results_ready)
        {
            self.stop_requested = true;
            if let Err(error) = control.stop_group(&mut self.child) {
                self.fail(format!("owned helper group signal failed: {error}"));
            }
        }
        if relationship_observed && self.observed && self.stop_requested && captures_done {
            match control.reap(&mut self.child) {
                Ok(Some(status)) => self.status = Some(status),
                Ok(None) => {}
                Err(error) => self.fail(format!("owned helper reap unavailable: {error}")),
            }
        }
        self.status
            .filter(|_| self.observe_group_absence(control))
            .map(Completion::Status)
    }
    fn observe_relationship(&mut self, control: &impl ProcessControl, now: Instant) -> bool {
        match control.observe_exit(&mut self.child) {
            Ok(true) => {
                if !self.observed {
                    self.capture_deadline = Some(now + CLEANUP_GRACE);
                }
                self.observed = true;
                true
            }
            Ok(false) => true,
            Err(error) => {
                // ECHILD invalidates numeric child/group authority permanently.
                // Reconciliation may only observe group absence from this point.
                if error.raw_os_error() == Some(rustix::io::Errno::CHILD.raw_os_error()) {
                    self.identity_lost = true;
                }
                self.fail(format!("helper exit observation unavailable: {error}"));
                false
            }
        }
    }
    fn observe_group_absence(&mut self, control: &impl ProcessControl) -> bool {
        // Only a read-only group check follows reaping or a lost relationship.
        match control.group_absent(self.child.id()) {
            Ok(absent) => absent,
            Err(error) => {
                self.fail(format!("helper group absence unavailable: {error}"));
                false
            }
        }
    }
    fn detail(&self) -> String {
        self.failure
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "helper exit, capture or group completion remains unproven".into())
    }
    fn finish(mut self, completion: Completion) -> Result<BoundedOutput, ProcessSpawnError> {
        let status = match completion {
            Completion::Status(status) => status,
            Completion::LostIdentity => return Err(ProcessSpawnError::GenericError {
                detail: "helper reaping identity was lost; group absence and capture completion proved without adopting the PID".into(),
            }),
        };
        if let Some(error) = self.failure.take() {
            return Err(error);
        }
        let stdout = self
            .out
            .take()
            .unwrap_or_else(|| Err("missing stdout".into()))
            .map_err(|detail| ProcessSpawnError::PipeError { detail })?;
        let stderr = self
            .err
            .take()
            .unwrap_or_else(|| Err("missing stderr".into()))
            .map_err(|detail| ProcessSpawnError::PipeError { detail })?;
        if !status.success() {
            return Err(ProcessSpawnError::NonZeroExit {
                command: self.label,
                exit_code: status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&stderr).into_owned(),
            });
        }
        Ok(BoundedOutput {
            status,
            stdout,
            stderr,
        })
    }
}

fn start_capture<R: io::Read + Send + 'static>(
    stream: &'static str,
    reader: Option<R>,
    limit: usize,
) -> (Option<CaptureWorker>, Option<CapturedBytes>) {
    match reader {
        Some(reader) => match CaptureWorker::spawn(stream, reader, limit) {
            Ok(worker) => (Some(worker), None),
            Err(error) => (
                None,
                Some(Err(format!("{stream} capture setup failed: {error}"))),
            ),
        },
        None => (None, Some(Err(format!("missing {stream} pipe")))),
    }
}
fn account_capture(
    worker: &mut Option<CaptureWorker>,
    result: &mut Option<CapturedBytes>,
) -> Result<bool, String> {
    let Some(worker) = worker else {
        return result
            .as_ref()
            .and_then(|r| r.as_ref().err())
            .map_or(Ok(true), |e| Err(e.clone()));
    };
    if result.is_none() {
        *result = worker.try_receive();
    }
    let Some(accounted) = worker.finish_if_ready() else {
        return Ok(false);
    };
    accounted.map_err(|error| error.to_string())?;
    match result.as_ref() {
        Some(Ok(_)) => Ok(true),
        Some(Err(error)) => Err(error.clone()),
        None => Ok(false),
    }
}

#[cfg(test)]
mod tests;
