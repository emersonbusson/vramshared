// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-exec/src/bounded_process.rs
//! Exact, bounded custody for short-lived production subprocesses.
//!
//! Every trusted helper starts as the leader of a new process group. Timeout
//! and inherited-pipe cleanup therefore target only that group, never a name
//! or a broad PID search. Exit is observed with `waitid(..., WNOWAIT)` so the
//! zombie leader pins its PID/PGID until residual group members and capture
//! workers are closed. Production cannot resume if group SIGKILL fails or the
//! direct child cannot be reaped within the fixed grace window.
//!
//! This is a trusted-helper boundary: RamShared helpers do not intentionally
//! call `setsid`/`setpgid` or daemonize. A malicious helper that deliberately
//! escapes its private group is outside this custody contract.

use crate::capture::{CaptureWorker, CapturedBytes};
use crate::retained_process::ProcessControl;
use rustix::io::Errno;
use rustix::process::{Pid, Signal, kill_process_group};
#[cfg(test)]
use rustix::process::{getpgid, test_kill_process_group};
use std::fmt;
use std::io;
#[cfg(test)]
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(10);
pub(crate) const REAP_GRACE: Duration = Duration::from_millis(500);
const CAPTURE_CLOSE_GRACE: Duration = Duration::from_millis(500);
pub const DEFAULT_OUTPUT_LIMIT: usize = 64 * 1024;
const FATAL_EXIT_CODE: i32 = 125;

#[derive(Debug)]
pub enum ProcessSpawnError {
    BinaryNotFound {
        command: String,
    },
    ExecutionTimeout {
        command: String,
        timeout: Duration,
    },
    NonZeroExit {
        command: String,
        exit_code: i32,
        stderr: String,
    },
    SpawnFailed {
        command: String,
        kind: io::ErrorKind,
        detail: String,
    },
    FatalContainment {
        detail: String,
    },
    PipeError {
        detail: String,
    },
    GenericError {
        detail: String,
    },
}

impl ProcessSpawnError {
    pub(super) fn new(detail: impl Into<String>) -> Self {
        let detail_str = detail.into();
        if detail_str.contains("pipe") || detail_str.contains("capture") {
            ProcessSpawnError::PipeError { detail: detail_str }
        } else {
            ProcessSpawnError::GenericError { detail: detail_str }
        }
    }

    fn spawn(label: &str, error: io::Error) -> Self {
        if error.kind() == io::ErrorKind::NotFound {
            ProcessSpawnError::BinaryNotFound {
                command: label.to_string(),
            }
        } else {
            ProcessSpawnError::SpawnFailed {
                command: label.to_string(),
                kind: error.kind(),
                detail: error.to_string(),
            }
        }
    }

    fn fatal(detail: impl Into<String>) -> Self {
        ProcessSpawnError::FatalContainment {
            detail: detail.into(),
        }
    }

    fn is_fatal(&self) -> bool {
        matches!(self, ProcessSpawnError::FatalContainment { .. })
    }
}

impl fmt::Display for ProcessSpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProcessSpawnError::BinaryNotFound { command } => {
                write!(f, "spawn {command}: binary not found")
            }
            ProcessSpawnError::ExecutionTimeout { command, timeout } => write!(
                f,
                "{command} timed out after {} ms; process group killed and direct child reaped",
                timeout.as_millis()
            ),
            ProcessSpawnError::NonZeroExit {
                command,
                exit_code,
                stderr,
            } => {
                if stderr.is_empty() {
                    write!(f, "{command} exited with {exit_code}")
                } else {
                    write!(f, "{command} exited with {exit_code}: {stderr}")
                }
            }
            ProcessSpawnError::SpawnFailed {
                command,
                kind,
                detail,
            } => write!(f, "spawn {command} ({kind}): {detail}"),
            ProcessSpawnError::FatalContainment { detail } => {
                write!(f, "fatal containment selected: {detail}")
            }
            ProcessSpawnError::PipeError { detail } => write!(f, "{detail}"),
            ProcessSpawnError::GenericError { detail } => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for ProcessSpawnError {}

#[derive(Debug)]
pub struct BoundedOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub(crate) trait FatalContainment {
    fn contain(&self, detail: &str);
}

struct ExitController;

impl FatalContainment for ExitController {
    fn contain(&self, detail: &str) {
        eprintln!("ramshared fatal subprocess containment: {detail}");
        std::process::exit(FATAL_EXIT_CODE);
    }
}

trait ReapTarget {
    fn id(&self) -> u32;
    fn signal_group_kill(&mut self) -> io::Result<()>;
    fn signal_direct_kill(&mut self) -> io::Result<()>;
    fn observe_exit(&mut self) -> io::Result<bool>;
    fn reap_observed(&mut self) -> io::Result<Option<ExitStatus>>;
}

impl ReapTarget for Child {
    fn id(&self) -> u32 {
        Child::id(self)
    }

    fn signal_group_kill(&mut self) -> io::Result<()> {
        crate::retained_process::native::signal_owned_group(self)
    }

    fn signal_direct_kill(&mut self) -> io::Result<()> {
        self.kill()
    }

    fn observe_exit(&mut self) -> io::Result<bool> {
        crate::retained_process::NativeProcessControl.observe_exit(self)
    }

    fn reap_observed(&mut self) -> io::Result<Option<ExitStatus>> {
        crate::retained_process::NativeProcessControl.reap(self)
    }
}

fn fatal_error(fatal: &dyn FatalContainment, detail: impl Into<String>) -> ProcessSpawnError {
    let detail = detail.into();
    fatal.contain(&detail);
    ProcessSpawnError::fatal(format!("fatal containment selected: {detail}"))
}

#[derive(Default)]
struct GroupTerminationProof {
    errors: Vec<String>,
}

impl GroupTerminationProof {
    fn record(&mut self, label: &str, phase: &str, error: io::Error) {
        if error.raw_os_error() != Some(Errno::SRCH.raw_os_error()) {
            self.errors.push(format!(
                "{label}: exact process-group SIGKILL {phase} failed: {error}"
            ));
        }
    }
}

fn wait_for_exit_observation(
    target: &mut dyn ReapTarget,
    label: &str,
    grace: Duration,
    fatal: &dyn FatalContainment,
) -> Result<(), ProcessSpawnError> {
    let deadline = Instant::now() + grace;
    loop {
        match target.observe_exit() {
            Ok(true) => return Ok(()),
            Ok(false) if Instant::now() < deadline => std::thread::sleep(POLL_INTERVAL),
            Ok(false) => {
                return Err(fatal_error(
                    fatal,
                    format!(
                        "{label}: process-group SIGKILL did not produce an observable direct-child exit {} within {} ms",
                        target.id(),
                        grace.as_millis()
                    ),
                ));
            }
            Err(error) => {
                return Err(fatal_error(
                    fatal,
                    format!("{label}: direct-child exit observation failed after SIGKILL: {error}"),
                ));
            }
        }
    }
}

fn force_exit_observed(
    target: &mut dyn ReapTarget,
    label: &str,
    grace: Duration,
    fatal: &dyn FatalContainment,
) -> Result<GroupTerminationProof, ProcessSpawnError> {
    let mut proof = GroupTerminationProof::default();
    if let Err(error) = target.signal_group_kill() {
        proof.record(label, "before exit observation", error);
        let _ = target.signal_direct_kill();
    }
    wait_for_exit_observation(target, label, grace, fatal)?;
    if let Err(error) = target.signal_group_kill() {
        proof.record(label, "while the zombie leader pinned the group", error);
    }
    Ok(proof)
}

fn contain_group_errors(
    proof: GroupTerminationProof,
    fatal: &dyn FatalContainment,
) -> Result<(), ProcessSpawnError> {
    if proof.errors.is_empty() {
        Ok(())
    } else {
        Err(fatal_error(fatal, proof.errors.join("; ")))
    }
}

fn reap_observed_target(
    target: &mut dyn ReapTarget,
    label: &str,
    grace: Duration,
    fatal: &dyn FatalContainment,
) -> Result<ExitStatus, ProcessSpawnError> {
    let deadline = Instant::now() + grace;
    loop {
        match target.reap_observed() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL_INTERVAL),
            Ok(None) => {
                return Err(fatal_error(
                    fatal,
                    format!(
                        "{label}: observed direct child {} was not reaped within {} ms",
                        target.id(),
                        grace.as_millis()
                    ),
                ));
            }
            Err(error) => {
                return Err(fatal_error(
                    fatal,
                    format!("{label}: final direct-child reap failed: {error}"),
                ));
            }
        }
    }
}

fn terminate_target_with(
    target: &mut dyn ReapTarget,
    label: &str,
    grace: Duration,
    fatal: &dyn FatalContainment,
) -> Result<(), ProcessSpawnError> {
    let proof = force_exit_observed(target, label, grace, fatal)?;
    let _ = reap_observed_target(target, label, grace, fatal)?;
    contain_group_errors(proof, fatal)
}

pub fn configure_process_group(command: &mut Command) -> &mut Command {
    command.process_group(0)
}

pub fn terminate_group_and_reap(child: &mut Child, label: &str) -> Result<(), ProcessSpawnError> {
    terminate_target_with(child, label, REAP_GRACE, &ExitController)
}

pub fn wait_grouped_child(
    child: &mut Child,
    label: &str,
    timeout: Duration,
) -> Result<ExitStatus, ProcessSpawnError> {
    let deadline = Instant::now() + timeout;
    loop {
        match ReapTarget::observe_exit(child) {
            Ok(true) => {
                let mut proof = GroupTerminationProof::default();
                if let Err(error) = ReapTarget::signal_group_kill(child) {
                    proof.record(label, "after normal exit observation", error);
                }
                let status = reap_observed_target(child, label, REAP_GRACE, &ExitController)?;
                contain_group_errors(proof, &ExitController)?;
                return Ok(status);
            }
            Ok(false) if Instant::now() < deadline => std::thread::sleep(POLL_INTERVAL),
            Ok(false) => {
                terminate_group_and_reap(child, label)?;
                return Err(ProcessSpawnError::ExecutionTimeout {
                    command: label.to_string(),
                    timeout,
                });
            }
            Err(error) => {
                return Err(fatal_error(
                    &ExitController,
                    format!("observe {label} without reaping: {error}"),
                ));
            }
        }
    }
}

/// Wait for the child's **own** lifetime and reap its group on exit.
///
/// A managed workload is not given a product deadline: the operator's command
/// runs as long as it needs to, and the runner only accounts for it. This is
/// the completion half of a scope runner — the start half never waits on the
/// child at all. There is deliberately no timeout branch here: a deadline that
/// kills the target would be the timeout shim the SPEC forbids, and a deadline
/// that abandons it would leave an unreaped group.
pub fn wait_grouped_child_unbounded(
    child: &mut Child,
    label: &str,
) -> Result<ExitStatus, ProcessSpawnError> {
    loop {
        match ReapTarget::observe_exit(child) {
            Ok(true) => {
                let mut proof = GroupTerminationProof::default();
                if let Err(error) = ReapTarget::signal_group_kill(child) {
                    proof.record(label, "after normal exit observation", error);
                }
                let status = reap_observed_target(child, label, REAP_GRACE, &ExitController)?;
                contain_group_errors(proof, &ExitController)?;
                return Ok(status);
            }
            Ok(false) => std::thread::sleep(POLL_INTERVAL),
            Err(error) => {
                return Err(fatal_error(
                    &ExitController,
                    format!("observe {label} without reaping: {error}"),
                ));
            }
        }
    }
}

/// Observe whether an owned child has already exited, **without** reaping it.
///
/// This is the non-destructive half of exit observation: the zombie leader
/// stays un-reaped and its PID/PGID stay pinned, so a later
/// [`wait_grouped_child_unbounded`] still owns the exact group. Callers use it
/// to tell "still ours" from "already over" without spending either the
/// child's lifetime or its exit status.
///
/// The observation only speaks for a child this process still owns. Once the
/// child has been reaped the wait state is gone, and the call reports that as
/// an error rather than as either outcome.
///
/// An observation failure is reported as an error and never as an exit: an
/// unknown wait state is not evidence that the child is done.
pub fn child_has_exited(child: &mut Child) -> Result<bool, ProcessSpawnError> {
    ReapTarget::observe_exit(child).map_err(|error| ProcessSpawnError::GenericError {
        detail: format!("observe owned child exit without reaping: {error}"),
    })
}

fn account_single_capture_before_reap(
    group_id: u32,
    label: &str,
    capture: &mut CaptureWorker,
    fatal: &dyn FatalContainment,
) -> Result<(), ProcessSpawnError> {
    let deadline = Instant::now() + CAPTURE_CLOSE_GRACE;
    if capture.receive_until(deadline).is_err() {
        signal_owned_process_group(group_id, label, fatal)?;
        if capture
            .receive_until(Instant::now() + CAPTURE_CLOSE_GRACE)
            .is_err()
        {
            return Err(fatal_error(
                fatal,
                format!(
                    "{label}: capture worker remained blocked after exact process-group SIGKILL"
                ),
            ));
        }
    }
    capture.join()
}

fn signal_owned_process_group(
    group_id: u32,
    label: &str,
    fatal: &dyn FatalContainment,
) -> Result<(), ProcessSpawnError> {
    let raw = i32::try_from(group_id).map_err(|_| {
        fatal_error(
            fatal,
            format!("{label}: process-group ID overflow before direct-child reap"),
        )
    })?;
    // POSIX `kill(-1, sig)` signals every process the caller may signal. That
    // is not process group 1, so group ID 1 must never reach the negation in
    // `kill_process_group`. Refuse it before any syscall.
    if raw == 1 {
        return Err(fatal_error(
            fatal,
            format!(
                "{label}: process-group ID 1 is the POSIX kill broadcast, not an owned child group"
            ),
        ));
    }
    let group = Pid::from_raw(raw).ok_or_else(|| {
        fatal_error(
            fatal,
            format!("{label}: zero process-group ID before direct-child reap"),
        )
    })?;
    match kill_process_group(group, Signal::KILL) {
        Ok(()) | Err(Errno::SRCH) => Ok(()),
        Err(error) => Err(fatal_error(
            fatal,
            format!(
                "{label}: output remained open but exact process-group SIGKILL failed: {error}"
            ),
        )),
    }
}

fn collect_captures_before_reap(
    group_id: u32,
    label: &str,
    stdout: &mut CaptureWorker,
    stderr: &mut CaptureWorker,
    fatal: &dyn FatalContainment,
    initial: Option<(Option<CapturedBytes>, Option<CapturedBytes>)>,
) -> Result<(Vec<u8>, Vec<u8>), ProcessSpawnError> {
    let (mut stdout_result, mut stderr_result) = initial.unwrap_or_else(|| {
        let first_deadline = Instant::now() + CAPTURE_CLOSE_GRACE;
        (
            stdout.receive_until(first_deadline).ok(),
            stderr.receive_until(first_deadline).ok(),
        )
    });
    if stdout_result.is_none() || stderr_result.is_none() {
        signal_owned_process_group(group_id, label, fatal)?;
        let final_deadline = Instant::now() + CAPTURE_CLOSE_GRACE;
        if stdout_result.is_none() {
            stdout_result = stdout.receive_until(final_deadline).ok();
        }
        if stderr_result.is_none() {
            stderr_result = stderr.receive_until(final_deadline).ok();
        }
        if stdout_result.is_none() || stderr_result.is_none() {
            return Err(fatal_error(
                fatal,
                format!(
                    "{label}: capture worker remained blocked after exact process-group SIGKILL"
                ),
            ));
        }
        stdout.join()?;
        stderr.join()?;
        return Err(ProcessSpawnError::new(format!(
            "{label}: output pipe remained open after direct-child exit; owned process group was killed"
        )));
    }

    stdout.join()?;
    stderr.join()?;
    let Some(stdout) = stdout_result else {
        return Err(fatal_error(
            fatal,
            format!("{label}: stdout capture result disappeared after worker join"),
        ));
    };
    let Some(stderr) = stderr_result else {
        return Err(fatal_error(
            fatal,
            format!("{label}: stderr capture result disappeared after worker join"),
        ));
    };
    let stdout = stdout.map_err(ProcessSpawnError::new)?;
    let stderr = stderr.map_err(ProcessSpawnError::new)?;
    Ok((stdout, stderr))
}

struct SpawnedChildGuard {
    child: Child,
    label: String,
    armed: bool,
}

impl SpawnedChildGuard {
    fn new(child: Child, label: &str) -> Self {
        Self {
            child,
            label: label.to_string(),
            armed: true,
        }
    }

    fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    fn id(&self) -> u32 {
        self.child.id()
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for SpawnedChildGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ =
                terminate_target_with(&mut self.child, &self.label, REAP_GRACE, &ExitController);
        }
    }
}

pub fn run_capture_command<F>(
    command: &mut Command,
    label: &str,
    timeout: Duration,
    output_limit: usize,
    on_spawn: F,
) -> Result<BoundedOutput, ProcessSpawnError>
where
    F: FnOnce(u32),
{
    run_capture_command_with_fatal(
        command,
        label,
        timeout,
        output_limit,
        on_spawn,
        &ExitController,
    )
}

fn run_capture_command_with_fatal<F>(
    command: &mut Command,
    label: &str,
    timeout: Duration,
    output_limit: usize,
    on_spawn: F,
    fatal: &dyn FatalContainment,
) -> Result<BoundedOutput, ProcessSpawnError>
where
    F: FnOnce(u32),
{
    let program = command.get_program();
    if program.is_empty() {
        return Err(ProcessSpawnError::spawn(
            label,
            io::Error::new(io::ErrorKind::InvalidInput, "empty program"),
        ));
    }
    if program.as_bytes().contains(&0) {
        return Err(ProcessSpawnError::spawn(
            label,
            io::Error::new(io::ErrorKind::InvalidInput, "null byte in program"),
        ));
    }
    let program_path = std::path::Path::new(program);
    if program_path.is_absolute() && !program_path.exists() {
        return Err(ProcessSpawnError::spawn(
            label,
            io::Error::new(io::ErrorKind::NotFound, "binary path does not exist"),
        ));
    }
    for arg in command.get_args() {
        if arg.as_bytes().contains(&0) {
            return Err(ProcessSpawnError::spawn(
                label,
                io::Error::new(io::ErrorKind::InvalidInput, "nul byte"),
            ));
        }
    }

    configure_process_group(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command
        .spawn()
        .map_err(|error| ProcessSpawnError::spawn(label, error))?;
    let mut child = SpawnedChildGuard::new(child, label);
    let group_id = child.id();
    on_spawn(group_id);
    let stdout = match child.child_mut().stdout.take() {
        Some(stdout) => stdout,
        None => {
            terminate_group_and_reap(child.child_mut(), label)?;
            child.disarm();
            return Err(ProcessSpawnError::new(format!(
                "{label}: stdout pipe was unavailable"
            )));
        }
    };
    let stderr = match child.child_mut().stderr.take() {
        Some(stderr) => stderr,
        None => {
            drop(stdout);
            terminate_group_and_reap(child.child_mut(), label)?;
            child.disarm();
            return Err(ProcessSpawnError::new(format!(
                "{label}: stderr pipe was unavailable"
            )));
        }
    };
    let mut stdout = match CaptureWorker::spawn("stdout", stdout, output_limit) {
        Ok(worker) => worker,
        Err(error) => {
            drop(stderr);
            terminate_group_and_reap(child.child_mut(), label)?;
            child.disarm();
            return Err(ProcessSpawnError::new(format!(
                "{label}: start stdout capture worker: {error}"
            )));
        }
    };
    let mut stderr = match CaptureWorker::spawn("stderr", stderr, output_limit) {
        Ok(worker) => worker,
        Err(error) => {
            let proof = force_exit_observed(child.child_mut(), label, REAP_GRACE, fatal)?;
            account_single_capture_before_reap(group_id, label, &mut stdout, fatal)?;
            let _ = reap_observed_target(child.child_mut(), label, REAP_GRACE, fatal)?;
            child.disarm();
            contain_group_errors(proof, fatal)?;
            return Err(ProcessSpawnError::new(format!(
                "{label}: start stderr capture worker: {error}"
            )));
        }
    };
    let deadline = Instant::now() + timeout;
    let (completion_error, proof, initial_capture) = loop {
        match ReapTarget::observe_exit(child.child_mut()) {
            Ok(true) => {
                // Before stopping residual descendants, distinguish a pipe
                // that closed naturally from one still inherited after the
                // leader exited. Only the former can produce trusted output.
                let capture_deadline = Instant::now() + CAPTURE_CLOSE_GRACE;
                let initial_capture = (
                    stdout.receive_until(capture_deadline).ok(),
                    stderr.receive_until(capture_deadline).ok(),
                );
                let mut proof = GroupTerminationProof::default();
                if let Err(error) = ReapTarget::signal_group_kill(child.child_mut()) {
                    proof.record(label, "after normal exit observation", error);
                }
                break (None, proof, Some(initial_capture));
            }
            Ok(false) if Instant::now() < deadline => std::thread::sleep(POLL_INTERVAL),
            Ok(false) => {
                let proof = force_exit_observed(child.child_mut(), label, REAP_GRACE, fatal)?;
                break (
                    Some(ProcessSpawnError::ExecutionTimeout {
                        command: label.to_string(),
                        timeout,
                    }),
                    proof,
                    None,
                );
            }
            Err(error) => {
                return Err(fatal_error(
                    fatal,
                    format!("observe {label} without reaping: {error}"),
                ));
            }
        }
    };
    let capture = collect_captures_before_reap(
        group_id,
        label,
        &mut stdout,
        &mut stderr,
        fatal,
        initial_capture,
    );
    let status = reap_observed_target(child.child_mut(), label, REAP_GRACE, fatal)?;
    child.disarm();
    contain_group_errors(proof, fatal)?;
    if let Some(error) = completion_error {
        return match capture {
            Err(capture_error) if capture_error.is_fatal() => Err(capture_error),
            Ok(_) | Err(_) => Err(error),
        };
    }
    let (stdout, stderr) = capture?;
    if !status.success() {
        if let Some(exit_code) = status.code() {
            let stderr_str = String::from_utf8_lossy(&stderr).trim().to_string();
            return Err(ProcessSpawnError::NonZeroExit {
                command: label.to_string(),
                exit_code,
                stderr: stderr_str,
            });
        } else {
            return Err(ProcessSpawnError::new(format!(
                "{label} terminated by signal"
            )));
        }
    }
    Ok(BoundedOutput {
        status,
        stdout,
        stderr,
    })
}

#[cfg(test)]
#[path = "bounded_process/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "bounded_process/discovery_tests.rs"]
mod discovery_tests;
