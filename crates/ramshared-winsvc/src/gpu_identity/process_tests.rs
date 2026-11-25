// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/gpu_identity/process_tests.rs
//! Real Windows job/pipe controls. Fixtures never load a GPU driver.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::gpu_identity::supervisor::{Supervisor, Tick};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[test]
#[ignore = "child-only fixture; explicitly selected by native custody tests"]
fn child_waits_for_termination() {
    println!("CUSTODY_FIXTURE_WAIT");
    std::thread::sleep(Duration::from_secs(10));
}

#[test]
#[ignore = "child-only fixture; explicitly selected by native custody tests"]
fn child_exceeds_output_limit() {
    println!("CUSTODY_FIXTURE_OUTPUT");
    std::io::stdout()
        .lock()
        .write_all(&vec![b'x'; 70 * 1024])
        .unwrap();
    std::thread::sleep(Duration::from_secs(10));
}

#[test]
#[ignore = "child-only fixture; explicitly selected by native custody tests"]
fn child_leaves_inherited_pipe_writer() {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "gpu_identity::process::tests::child_waits_for_termination",
            "--ignored",
        ])
        .spawn()
        .unwrap();
    // The child intentionally outlives this fixture. The enclosing native job
    // must retain and terminate it; no detached PID/name search is used.
    let _ = child.try_wait().unwrap();
    println!("CUSTODY_FIXTURE_INHERITED");
}

#[derive(Default)]
struct Trace {
    output: Vec<u8>,
    stops: usize,
    exited_with_live_members: bool,
    exit: Option<u32>,
    empty: bool,
}
struct TracedProcess {
    process: NativeProcess,
    trace: Arc<Mutex<Trace>>,
}
impl Producer for TracedProcess {
    fn identity(&self) -> Result<ProducerProcess, String> {
        self.process.identity()
    }
    fn observe_exit(&mut self) -> Result<ExitObservation, String> {
        let result = self.process.observe_exit()?;
        let mut trace = self.trace.lock().unwrap();
        trace.exited_with_live_members |= result.code.is_some() && !result.job_empty;
        trace.exit = result.code;
        trace.empty = result.job_empty;
        Ok(result)
    }
    fn read_stdout(&mut self, buffer: &mut [u8]) -> Result<PipeRead, String> {
        let result = self.process.read_stdout(buffer)?;
        if let PipeRead::Bytes(count) = result {
            let mut trace = self.trace.lock().unwrap();
            let count = count.min(2048 - trace.output.len());
            trace.output.extend_from_slice(&buffer[..count]);
        }
        Ok(result)
    }
    fn read_stderr(&mut self, buffer: &mut [u8]) -> Result<PipeRead, String> {
        self.process.read_stderr(buffer)
    }
    fn terminate_job(&mut self) -> Result<(), String> {
        self.trace.lock().unwrap().stops += 1;
        self.process.terminate_job()
    }
}

fn run_fixture(name: &str) {
    let exe = std::env::current_exe().unwrap();
    let args = format!("--exact gpu_identity::process::tests::{name} --ignored --nocapture");
    let mut owner = Supervisor::default();
    let trace = Arc::new(Mutex::new(Trace::default()));
    let started = Instant::now();
    owner
        .start(
            || {
                Ok(TracedProcess {
                    process: NativeProcess::spawn_fixture(&exe, &args)?,
                    trace: trace.clone(),
                })
            },
            started,
            1000,
            if name == "child_exceeds_output_limit" {
                Duration::from_secs(2)
            } else {
                Duration::from_millis(500)
            },
        )
        .unwrap();
    let observation_deadline = started + Duration::from_secs(5);
    loop {
        match owner.tick(Instant::now(), 1000) {
            Tick::Completed(result) => {
                let error = result.unwrap_err();
                let expected = match name {
                    "child_waits_for_termination" => "parent deadline",
                    "child_exceeds_output_limit" => "byte bound",
                    _ => "live job members",
                };
                assert!(error.contains(expected), "unexpected refusal: {error}");
                break;
            }
            Tick::Running | Tick::Pending => {}
            Tick::Idle => panic!("native custody disappeared before completion"),
        }
        // Failure intentionally fails the test; job RAII contains the exact
        // fixture tree if the runner exits unexpectedly. No production claim.
        assert!(
            Instant::now() < observation_deadline,
            "native fixture remains pending"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!owner.busy());
    assert!(owner.snapshot(Instant::now()).is_none());
    let trace = trace.lock().unwrap();
    let output = String::from_utf8_lossy(&trace.output);
    let marker = match name {
        "child_waits_for_termination" => "CUSTODY_FIXTURE_WAIT",
        "child_exceeds_output_limit" => "CUSTODY_FIXTURE_OUTPUT",
        _ => "CUSTODY_FIXTURE_INHERITED",
    };
    assert!(
        output.contains(marker),
        "native fixture never reached scenario: {output}"
    );
    assert_eq!(trace.stops, 1);
    assert!(trace.empty && trace.exit.is_some());
    if name == "child_leaves_inherited_pipe_writer" {
        assert!(trace.exited_with_live_members);
    }
    eprintln!(
        "NATIVE_CUSTODY_OK scenario={name} stops={} exit={:?} job_empty={}",
        trace.stops, trace.exit, trace.empty
    );
}
#[test]
fn native_job_timeout_retains_until_verified_exit() {
    run_fixture("child_waits_for_termination");
}
#[test]
fn native_job_excess_output_refuses_and_reaps() {
    run_fixture("child_exceeds_output_limit");
}
#[test]
fn native_job_inherited_writer_cannot_outlive_custody() {
    run_fixture("child_leaves_inherited_pipe_writer");
}

#[test]
fn storage_producer_command_quotes_one_absolute_file_and_refuses_injection() {
    assert_eq!(
        storage_arguments(Path::new(r"C:\path with spaces\disk.vhdx")).unwrap(),
        r#"inspect-storage --file "C:\path with spaces\disk.vhdx""#
    );
    for bad in [
        r"relative\disk.vhdx",
        "C:\\path\\bad\" --start",
        "C:\\path\\bad\nname",
        "C:\\dir\\",
    ] {
        assert!(
            storage_arguments(Path::new(bad)).is_err(),
            "accepted {bad:?}"
        );
    }
}
