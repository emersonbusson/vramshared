// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-exec/src/bounded_process/tests/group_containment.rs
#![allow(clippy::expect_used)]
use super::super::*;
use super::support::*;
use super::*;

#[test]
fn wait_grouped_child_reaps_a_short_child_after_exit() {
    let mut command = Command::new("/bin/true");
    configure_process_group(&mut command)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = command.spawn().expect("short fixture child starts");
    let status = wait_grouped_child(&mut child, "short grouped child", Duration::from_secs(5))
        .expect("a child that already exited must be reaped and returned");
    assert!(status.success());
}

#[test]
fn wait_grouped_child_terminates_its_group_on_timeout() {
    let mut command = Command::new("/bin/sleep");
    command.args(["30"]);
    configure_process_group(&mut command)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = command.spawn().expect("hung fixture child starts");
    let pid = child.id();
    let error = wait_grouped_child(&mut child, "hung grouped child", Duration::from_millis(50))
        .expect_err("a child past its deadline must be terminated, not awaited forever");
    assert!(
        matches!(error, ProcessSpawnError::ExecutionTimeout { .. }),
        "{error}"
    );
    assert!(
        wait_for_fixture_process_exit(pid),
        "the timed-out fixture must not outlive its process group"
    );
}

#[test]
// TestName: wait_grouped_child_unbounded_waits_out_the_child_lifetime
fn wait_grouped_child_unbounded_waits_out_the_child_lifetime() {
    let mut command = Command::new("/bin/sleep");
    command.args(["0.4"]);
    configure_process_group(&mut command)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = command.spawn().expect("bounded fixture child starts");
    let pid = child.id();
    let started = std::time::Instant::now();
    let status = wait_grouped_child_unbounded(&mut child, "managed scope runner")
        .expect("a managed workload is never given a product deadline");
    let elapsed = started.elapsed();
    assert!(status.success());
    assert!(
        elapsed >= Duration::from_millis(300),
        "the unbounded wait returned after {elapsed:?}; it must wait out the child, not kill it"
    );
    assert!(
        wait_for_fixture_process_exit(pid),
        "the waited-out fixture must leave no process group behind"
    );
}

#[test]
// TestName: wait_grouped_child_unbounded_is_the_workload_exit_status_authority
fn wait_grouped_child_unbounded_is_the_workload_exit_status_authority() {
    // A `.scope` reports `Result=success` for a command that exited 7, so
    // the runner child's own status is the only workload exit status.
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "sleep 0.2 & wait; exit 7"]);
    configure_process_group(&mut command)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = command.spawn().expect("nonzero fixture child starts");
    let pid = child.id();
    let status = wait_grouped_child_unbounded(&mut child, "managed scope runner")
        .expect("a nonzero exit is a result, not a spawn failure");
    assert_eq!(
        status.code(),
        Some(7),
        "the runner child must carry the workload's exit status"
    );
    assert!(
        wait_for_fixture_process_exit(pid),
        "the nonzero fixture must leave no process group behind"
    );
}

#[test]
// TestName: child_has_exited_observes_without_reaping_the_owned_child
fn child_has_exited_observes_without_reaping_the_owned_child() {
    let mut command = Command::new("/bin/sleep");
    command.args(["0.5"]);
    configure_process_group(&mut command)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = command.spawn().expect("observation fixture child starts");
    assert!(
        !child_has_exited(&mut child).expect("observation of a live child must succeed"),
        "a live child was reported as exited"
    );
    // The child is still ours after the observation: nothing was reaped,
    // so the unbounded wait still owns the exact group and its status.
    let status = wait_grouped_child_unbounded(&mut child, "observed fixture")
        .expect("an observed-then-waited child must still be reaped exactly once");
    assert!(status.success());
    // Once the group has been reaped the child is no longer ours, so the
    // wait state is gone. That is reported as an error and never as
    // `Ok(false)`: a reaped child must not be told apart as "still
    // running" by an observation that can no longer see it.
    assert!(
        child_has_exited(&mut child).is_err(),
        "an already-reaped child must not be observed as still running"
    );
}

#[test]
fn account_single_capture_resolves_after_owned_group_kill() {
    let fixture = OwnedGroupFixture::spawn();
    let mut capture = CaptureWorker::spawn(
        "stdout",
        DelayedEofReader {
            delay: Duration::from_millis(800),
        },
        1024,
    )
    .expect("worker");
    account_single_capture_before_reap(
        fixture.group_id(),
        "delayed capture",
        &mut capture,
        &ExitController,
    )
    .expect("a worker that answers within the second close-grace is accounted, not fatal");
}

#[test]
fn account_single_capture_fails_closed_when_worker_stays_blocked() {
    let fixture = OwnedGroupFixture::spawn();
    let mut capture = CaptureWorker::spawn(
        "stdout",
        DelayedEofReader {
            delay: Duration::from_secs(2),
        },
        1024,
    )
    .expect("worker");
    let fatal = RecordingFatal::default();
    let error = account_single_capture_before_reap(
        fixture.group_id(),
        "stuck capture",
        &mut capture,
        &fatal,
    )
    .expect_err("a worker still blocked after exact group SIGKILL must remain fatal");
    assert!(
        error
            .to_string()
            .contains("remained blocked after exact process-group SIGKILL"),
        "{error}"
    );
    assert_eq!(fatal.0.borrow().len(), 1);
}

#[test]
fn collect_captures_accounts_a_natural_close_without_group_kill() {
    let mut stdout =
        CaptureWorker::spawn("stdout", Cursor::new(b"out".to_vec()), 1024).expect("stdout");
    let mut stderr =
        CaptureWorker::spawn("stderr", Cursor::new(b"err".to_vec()), 1024).expect("stderr");
    let (out, err) = collect_captures_before_reap(
        0,
        "natural close",
        &mut stdout,
        &mut stderr,
        &ExitController,
        None,
    )
    .expect("pipes that closed before the leader was reaped yield trusted output");
    assert_eq!(out, b"out");
    assert_eq!(err, b"err");
}

#[test]
fn collect_captures_kills_the_owned_group_when_a_pipe_stays_open() {
    let fixture = OwnedGroupFixture::spawn();
    let mut stdout = CaptureWorker::spawn(
        "stdout",
        DelayedEofReader {
            delay: Duration::from_millis(800),
        },
        1024,
    )
    .expect("stdout");
    let mut stderr = CaptureWorker::spawn(
        "stderr",
        DelayedEofReader {
            delay: Duration::from_millis(800),
        },
        1024,
    )
    .expect("stderr");
    let error = collect_captures_before_reap(
        fixture.group_id(),
        "open pipe fixture",
        &mut stdout,
        &mut stderr,
        &ExitController,
        None,
    )
    .expect_err("output captured after an inherited pipe stayed open is not trusted");
    assert!(
        error
            .to_string()
            .contains("output pipe remained open after direct-child exit"),
        "{error}"
    );
}

#[test]
fn collect_captures_remains_fatal_when_both_workers_stay_blocked() {
    let fixture = OwnedGroupFixture::spawn();
    let mut stdout = CaptureWorker::spawn(
        "stdout",
        DelayedEofReader {
            delay: Duration::from_secs(2),
        },
        1024,
    )
    .expect("stdout");
    let mut stderr = CaptureWorker::spawn(
        "stderr",
        DelayedEofReader {
            delay: Duration::from_secs(2),
        },
        1024,
    )
    .expect("stderr");
    let fatal = RecordingFatal::default();
    let error = collect_captures_before_reap(
        fixture.group_id(),
        "both stuck",
        &mut stdout,
        &mut stderr,
        &fatal,
        None,
    )
    .expect_err("workers still blocked after exact group SIGKILL must remain fatal");
    assert!(error.is_fatal(), "{error}");
    assert!(error.to_string().contains("remained blocked"), "{error}");
}

#[test]
fn collect_captures_propagates_a_capture_failure_as_a_typed_error() {
    let mut stdout = CaptureWorker::spawn("stdout", FailingReader, 1024).expect("stdout");
    let mut stderr =
        CaptureWorker::spawn("stderr", Cursor::new(b"err".to_vec()), 1024).expect("stderr");
    let error = collect_captures_before_reap(
        0,
        "capture failure",
        &mut stdout,
        &mut stderr,
        &ExitController,
        None,
    )
    .expect_err("a capture worker read failure is reported, never swallowed");
    assert!(
        error.to_string().contains("stdout capture failed"),
        "{error}"
    );
}

#[test]
fn signal_owned_process_group_rejects_unusable_group_ids() {
    let fatal = RecordingFatal::default();
    let overflow = signal_owned_process_group(
        u32::try_from(i32::MAX).expect("i32::MAX") + 1,
        "overflow",
        &fatal,
    )
    .expect_err("a group ID beyond pid_t must be rejected before any syscall");
    assert!(
        overflow.to_string().contains("process-group ID overflow"),
        "{overflow}"
    );
    let zero = signal_owned_process_group(0, "zero", &fatal)
        .expect_err("group 0 is not an owned process group");
    assert!(zero.to_string().contains("zero process-group ID"), "{zero}");
    assert_eq!(fatal.0.borrow().len(), 2);
}

#[test]
fn capture_runner_times_out_and_reports_typed_execution_timeout() {
    let mut command = Command::new("/bin/sleep");
    command.args(["30"]);
    let error = run_capture_command(
        &mut command,
        "timeout fixture",
        Duration::from_millis(50),
        DEFAULT_OUTPUT_LIMIT,
        |_| {},
    )
    .expect_err("a helper that never exits must be terminated, not awaited forever");
    assert!(
        matches!(error, ProcessSpawnError::ExecutionTimeout { .. }),
        "{error}"
    );
}

#[test]
fn capture_runner_reports_signal_termination_without_an_exit_code() {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "kill -KILL $$"]);
    let error = run_capture_command(
        &mut command,
        "signal fixture",
        Duration::from_secs(2),
        DEFAULT_OUTPUT_LIMIT,
        |_| {},
    )
    .expect_err("a helper killed by a signal has no exit code to report");
    assert!(
        error.to_string().contains("terminated by signal"),
        "{error}"
    );
    assert!(
        !matches!(error, ProcessSpawnError::NonZeroExit { .. }),
        "a signal death must not be mis-typed as a nonzero exit"
    );
}

#[test]
fn reap_observed_target_names_the_exact_live_child_it_cannot_reap() {
    // `ReapTarget::id` for `Child` is only reached through a `dyn`
    // dispatch; the inherent `Child::id` used inside the impl shadows it
    // everywhere else. Calling the trait method through the object is
    // what proves the unreaped-error path names the exact process.
    let mut command = Command::new("/bin/sleep");
    command.args(["30"]);
    let mut child = command.spawn().expect("live fixture child starts");
    let pid = child.id();
    let fatal = RecordingFatal::default();
    let error = reap_observed_target(&mut child, "live unreaped child", Duration::ZERO, &fatal)
        .expect_err("a still-running child has no reap proof within a zero grace");
    assert!(error.to_string().contains("was not reaped"), "{error}");
    assert!(
        error.to_string().contains(&pid.to_string()),
        "the fatal path must name the exact child: {error}"
    );
    assert_eq!(fatal.0.borrow().len(), 1);
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn signal_owned_process_group_refuses_the_posix_kill_broadcast_group() {
    // POSIX `kill(-1, sig)` signals every process the caller may signal.
    // Group ID 1 is that broadcast, not an owned process group, so
    // production must refuse it before the syscall rather than negating it
    // into `kill_process_group(Pid::INIT, ..)`.
    let fatal = RecordingFatal::default();
    let error = signal_owned_process_group(1, "broadcast guard", &fatal)
        .expect_err("group ID 1 is not an owned process group");
    assert!(
        error.to_string().contains("POSIX kill broadcast"),
        "{error}"
    );
    assert_eq!(fatal.0.borrow().len(), 1);
}

#[test]
fn signal_owned_process_group_reports_a_foreign_group_as_fatal() {
    // `ESRCH` means the group is already gone and nothing leaked, so it is
    // not a containment failure. `EPERM` on a foreign group is: production
    // cannot resume if a group it does not own cannot be killed.
    let Some(group) = foreign_unsignable_process_group() else {
        // No foreign group is observable on this host (private PID
        // namespace, or we are root). The broadcast refusal above still
        // covers the dangerous identifier on every host.
        return;
    };
    let fatal = RecordingFatal::default();
    let error = signal_owned_process_group(
        group.as_raw_nonzero().get() as u32,
        "foreign group fixture",
        &fatal,
    )
    .expect_err("a process group outside our custody must stay fatal, not be ignored");
    assert!(
        error
            .to_string()
            .contains("exact process-group SIGKILL failed"),
        "{error}"
    );
    assert_eq!(fatal.0.borrow().len(), 1);
}

#[test]
fn capture_runner_timeout_keeps_a_fatal_capture_containment() {
    // The leader is group-killed at its deadline, but a `setsid` grandchild
    // outside that group keeps the capture pipes open. The capture then
    // fails closed as fatal, and that containment must win over the
    // ordinary timeout error: resuming on the timeout alone would leave a
    // leaked grandchild holding the output pipes.
    //
    // The grandchild writes its own PID only after it is executing, so the
    // pid file is a ready signal rather than a parent-side `&` snapshot.
    // `on_spawn` then waits until that exact process is alive, holds a
    // capture pipe, and sits outside the leader group — and only after
    // `on_spawn` returns does `run_capture_command_with_fatal` establish
    // the capture deadline. The timeout therefore cannot fire before the
    // leaked holder is an observed fact, and the holder outlives the whole
    // timeout-plus-containment window so it cannot drop the pipes mid
    // observation.
    let pid_path = std::env::temp_dir().join(format!(
        "ramshared-bounded-process-leaked-pipe-{}.pid",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&pid_path);
    let pid_arg = pid_path.to_string_lossy().into_owned();
    let mut command = Command::new("/bin/sh");
    command.args([
        "-c",
        // `setsid` moves the grandchild into its own session, so the group
        // SIGKILL cannot close the capture pipes it inherited. It publishes
        // its own PID as the ready signal, then holds the pipes for the
        // entire containment observation.
        "setsid /bin/sh -c 'printf \"%s\\n\" \"$$\" > \"$1\"; exec sleep 30' sh \"$1\" & exec sleep 30",
        "sh",
        pid_arg.as_str(),
    ]);
    let fatal = RecordingFatal::default();
    let error = run_capture_command_with_fatal(
        &mut command,
        "leaked pipe fixture",
        Duration::from_millis(500),
        DEFAULT_OUTPUT_LIMIT,
        |group_id| {
            wait_for_escaped_pipe_holder(&pid_path, group_id);
        },
        &fatal,
    )
    .expect_err("a timeout with a fatally blocked capture must stay fatal");
    assert!(error.is_fatal(), "capture containment must win: {error}");
    assert!(
        !matches!(error, ProcessSpawnError::ExecutionTimeout { .. }),
        "the ordinary timeout must not win while the leaked holder pins the pipes: {error}"
    );
    assert!(
        error
            .to_string()
            .contains("remained blocked after exact process-group SIGKILL"),
        "{error}"
    );

    let leaked = std::fs::read_to_string(&pid_path)
        .expect("fixture records its exact setsid grandchild PID")
        .trim()
        .parse::<u32>()
        .expect("fixture prints a decimal PID");
    let _ = std::fs::remove_file(&pid_path);
    kill_exact_fixture_process(leaked);
    let _ = wait_for_fixture_process_exit(leaked);
}
