// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/gpu_identity/supervisor.rs
//! Single-flight native producer policy. Observations confer no allocation authority.
use super::{HostGpuIdentityReport, MAX_IDENTITY_REPORT_BYTES, ProducerProcess, decode_report};
use ramshared_vram::GPU_BUDGET_MAX_AGE_MS;
use std::time::{Duration, Instant};

const CHUNK_BYTES: usize = 4096;
const STDERR_BYTES: usize = 4096;

pub enum PipeRead {
    Bytes(usize),
    Pending,
    Closed,
}
pub struct ExitObservation {
    pub code: Option<u32>,
    pub job_empty: bool,
}
/// Native implementations own the actual process/job/pipes. Errors never
/// establish exit; reads are nonblocking and bounded by the supplied buffer.
pub trait Producer {
    fn identity(&self) -> Result<ProducerProcess, String>;
    fn observe_exit(&mut self) -> Result<ExitObservation, String>;
    fn read_stdout(&mut self, buffer: &mut [u8]) -> Result<PipeRead, String>;
    fn read_stderr(&mut self, buffer: &mut [u8]) -> Result<PipeRead, String>;
    fn terminate_job(&mut self) -> Result<(), String>;
}
/// Concrete report codecs expose the process/window checked by native custody.
pub trait ProbeReport: Clone {
    fn decode(bytes: &[u8]) -> Result<Self, String>;
    fn producer(&self) -> &ProducerProcess;
    fn window(&self) -> (u64, u64);
}
impl ProbeReport for HostGpuIdentityReport {
    fn decode(bytes: &[u8]) -> Result<Self, String> {
        decode_report(bytes)
    }
    fn producer(&self) -> &ProducerProcess {
        &self.producer
    }
    fn window(&self) -> (u64, u64) {
        (self.started_at_unix_ms, self.elapsed_ms)
    }
}
pub type Tick = ProbeTick<HostGpuIdentityReport>;
pub type Supervisor<P> = ProbeSupervisor<P, HostGpuIdentityReport>;
#[derive(Debug)]
pub enum ProbeTick<R> {
    Idle,
    Running,
    Pending,
    Completed(Result<R, String>),
}
struct Capture {
    bytes: Vec<u8>,
    closed: bool,
    limit: usize,
}
impl Capture {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            closed: false,
            limit,
        }
    }
    fn poll(
        &mut self,
        read: impl FnOnce(&mut [u8]) -> Result<PipeRead, String>,
    ) -> Result<(), String> {
        if self.closed {
            return Ok(());
        }
        let mut buffer = [0; CHUNK_BYTES];
        match read(&mut buffer) {
            Ok(PipeRead::Pending) => Ok(()),
            Ok(PipeRead::Closed) => {
                self.closed = true;
                Ok(())
            }
            Ok(PipeRead::Bytes(count))
                if count > 0
                    && count <= buffer.len()
                    && self
                        .bytes
                        .len()
                        .checked_add(count)
                        .is_some_and(|size| size <= self.limit) =>
            {
                self.bytes.extend_from_slice(&buffer[..count]);
                Ok(())
            }
            Ok(PipeRead::Bytes(_)) => {
                self.closed = true;
                Err("native producer output exceeds its byte bound or read contract".into())
            }
            Err(error) => {
                self.closed = true;
                Err(error)
            }
        }
    }
}
struct Flight<P> {
    process: P,
    identity: Option<ProducerProcess>,
    started: Instant,
    started_unix_ms: u64,
    deadline: Instant,
    stdout: Capture,
    stderr: Capture,
    failure: Option<String>,
    stop_requested: bool,
}
impl<P: Producer> Flight<P> {
    fn fail(&mut self, detail: String) {
        if self.failure.is_none() {
            self.failure = Some(detail);
        }
    }
    fn poll<R: ProbeReport>(&mut self, now: Instant, now_unix_ms: u64) -> ProbeTick<R> {
        if let Err(error) = self.stdout.poll(|buffer| self.process.read_stdout(buffer)) {
            self.fail(error);
        }
        if let Err(error) = self.stderr.poll(|buffer| self.process.read_stderr(buffer)) {
            self.fail(error);
        }
        if !self.stderr.bytes.is_empty() {
            self.fail("native producer wrote unexpected diagnostics".into());
        }
        let exit = match self.process.observe_exit() {
            Ok(exit) => Some(exit),
            Err(error) => {
                self.fail(error);
                None
            }
        };
        if now < self.started || now >= self.deadline {
            self.fail("native producer parent deadline expired or clock is invalid".into());
        }
        let settled = exit
            .as_ref()
            .is_some_and(|exit| exit.code.is_some() && exit.job_empty);
        if exit
            .as_ref()
            .is_some_and(|exit| exit.code.is_some() && !exit.job_empty)
        {
            self.fail("native producer left live job members after direct-child exit".into());
        }
        if self.failure.is_some() && !self.stop_requested && !settled {
            self.stop_requested = true;
            if let Err(error) = self.process.terminate_job() {
                self.fail(error);
            }
        }
        if settled && self.stdout.closed && self.stderr.closed {
            let code = exit.and_then(|exit| exit.code);
            return ProbeTick::Completed(self.finish(code, now, now_unix_ms));
        }
        if self.failure.is_some() {
            ProbeTick::Pending
        } else {
            ProbeTick::Running
        }
    }
    fn finish<R: ProbeReport>(
        &self,
        code: Option<u32>,
        now: Instant,
        now_unix_ms: u64,
    ) -> Result<R, String> {
        if let Some(error) = &self.failure {
            let diagnostic = std::str::from_utf8(&self.stderr.bytes)
                .unwrap_or("non-UTF8 native diagnostics")
                .chars()
                .filter(|character| !character.is_control())
                .collect::<String>();
            return Err(if diagnostic.is_empty() {
                error.clone()
            } else {
                format!("{error}: {diagnostic}")
            });
        }
        if code != Some(0) || !self.stderr.bytes.is_empty() {
            return Err("native producer did not exit successfully with clean diagnostics".into());
        }
        let bytes = self
            .stdout
            .bytes
            .strip_suffix(b"\n")
            .unwrap_or(&self.stdout.bytes);
        let report = R::decode(bytes)?;
        let (started, elapsed) = report.window();
        if self.identity.as_ref() != Some(report.producer())
            || started < self.started_unix_ms
            || started > now_unix_ms
            || started
                .checked_add(elapsed)
                .is_none_or(|end| end > now_unix_ms)
            || u128::from(elapsed) > now.duration_since(self.started).as_millis()
        {
            return Err(
                "native producer report changed process identity or observation window".into(),
            );
        }
        Ok(report)
    }
}
/// Native service instances keep this in a process-lifetime owner so unwinding
/// cannot relinquish pending handles. `start` checks admission before effects.
pub struct ProbeSupervisor<P, R> {
    flight: Option<Flight<P>>,
    snapshot: Option<(R, Instant, Instant)>,
}
impl<P, R> Default for ProbeSupervisor<P, R> {
    fn default() -> Self {
        Self {
            flight: None,
            snapshot: None,
        }
    }
}
impl<P: Producer, R: ProbeReport> ProbeSupervisor<P, R> {
    pub fn busy(&self) -> bool {
        self.flight.is_some()
    }
    pub fn start(
        &mut self,
        spawn: impl FnOnce() -> Result<P, String>,
        started: Instant,
        started_unix_ms: u64,
        timeout: Duration,
    ) -> Result<(), String> {
        if self.busy() {
            return Err("native producer custody remains occupied".into());
        }
        if started_unix_ms == 0
            || timeout.is_zero()
            || timeout >= Duration::from_millis(GPU_BUDGET_MAX_AGE_MS)
        {
            self.snapshot = None;
            return Err("native producer start clock or deadline is invalid".into());
        }
        let Some(deadline) = started.checked_add(timeout) else {
            self.snapshot = None;
            return Err("native producer deadline overflow".into());
        };
        let process = match spawn() {
            Ok(process) => process,
            Err(error) => {
                self.snapshot = None;
                return Err(error);
            }
        };
        self.flight = Some(Flight {
            process,
            identity: None,
            started,
            started_unix_ms,
            deadline,
            stdout: Capture::new(MAX_IDENTITY_REPORT_BYTES + 1),
            stderr: Capture::new(STDERR_BYTES),
            failure: None,
            stop_requested: false,
        });
        // Store custody before invoking another effect: unwind retains it.
        if let Some(flight) = self.flight.as_mut() {
            match flight.process.identity() {
                Ok(identity) if identity.pid != 0 && identity.creation_time_100ns != 0 => {
                    flight.identity = Some(identity)
                }
                _ => {
                    self.snapshot = None;
                    flight.fail("native producer process identity is unavailable".into());
                }
            }
        }
        Ok(())
    }
    pub fn tick(&mut self, now: Instant, now_unix_ms: u64) -> ProbeTick<R> {
        let Some(flight) = self.flight.as_mut() else {
            return ProbeTick::Idle;
        };
        let tick = flight.poll::<R>(now, now_unix_ms);
        if flight.failure.is_some() {
            self.snapshot = None;
        }
        if let ProbeTick::Completed(result) = &tick {
            self.snapshot = None;
            if let Ok(report) = result
                && let Some(expires) = flight
                    .started
                    .checked_add(Duration::from_millis(GPU_BUDGET_MAX_AGE_MS))
            {
                self.snapshot = Some((report.clone(), flight.started, expires));
            }
            self.flight.take();
        }
        tick
    }
    pub fn cancel(&mut self) {
        self.snapshot = None;
        if let Some(flight) = self.flight.as_mut() {
            flight.fail("native producer sampling stopped".into());
        }
    }
    pub fn snapshot_entry(&self, now: Instant) -> Option<(&R, Instant, Instant)> {
        self.snapshot
            .as_ref()
            .filter(|(_, start, expires)| now >= *start && now < *expires)
            .map(|(report, start, expires)| (report, *start, *expires))
    }
    pub fn snapshot_with_age(&self, now: Instant) -> Option<(&R, u64)> {
        let (report, start, _) = self.snapshot_entry(now)?;
        let age = elapsed_age_ms(start, now)?;
        Some((report, age))
    }
    pub fn snapshot(&self, now: Instant) -> Option<&R> {
        self.snapshot_entry(now).map(|(report, _, _)| report)
    }
}

pub(crate) fn elapsed_age_ms(start: Instant, now: Instant) -> Option<u64> {
    let age = u64::try_from(
        now.checked_duration_since(start)?
            .as_nanos()
            .div_ceil(1_000_000),
    )
    .ok()?;
    (age < GPU_BUDGET_MAX_AGE_MS).then_some(age)
}

impl<P: Producer> ProbeSupervisor<P, HostGpuIdentityReport> {
    pub fn aged_snapshot(&self, now: Instant) -> Option<super::HostGpuObservation> {
        let (report, age_ms) = self.snapshot_with_age(now)?;
        let sample = super::HostGpuObservation {
            report: report.clone(),
            age_ms,
        };
        sample.validate().ok()?;
        Some(sample)
    }
}
