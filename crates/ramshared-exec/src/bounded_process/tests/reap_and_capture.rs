// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-exec/src/bounded_process/tests/reap_and_capture.rs
#![allow(clippy::expect_used)]
use super::super::*;
use super::support::*;
use super::*;

// TestName: unreaped_group_selects_fatal_controller_containment
#[test]
fn unreaped_group_selects_fatal_controller_containment() {
    let mut target = NeverReaped {
        id: 42,
        group_kills: Cell::new(0),
        direct_kills: Cell::new(0),
    };
    let fatal = RecordingFatal::default();
    let error = terminate_target_with(&mut target, "fixture", Duration::ZERO, &fatal)
        .expect_err("unreaped SIGKILL must select fatal containment");

    assert_eq!(target.group_kills.get(), 1);
    assert_eq!(target.direct_kills.get(), 0);
    assert_eq!(fatal.0.borrow().len(), 1);
    assert!(fatal.0.borrow()[0].contains("observable direct-child exit"));
    assert!(error.to_string().contains("fatal containment selected"));
}

#[test]
fn reap_observation_retries_then_fails_closed_if_still_unreaped() {
    struct DelayedReapTarget {
        reaps: usize,
    }

    impl ReapTarget for DelayedReapTarget {
        fn id(&self) -> u32 {
            43
        }

        fn signal_group_kill(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn signal_direct_kill(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn observe_exit(&mut self) -> io::Result<bool> {
            Ok(true)
        }

        fn reap_observed(&mut self) -> io::Result<Option<ExitStatus>> {
            self.reaps += 1;
            Ok((self.reaps > 1).then(|| ExitStatus::from_raw(0)))
        }
    }

    let fatal = RecordingFatal::default();
    let mut delayed = DelayedReapTarget { reaps: 0 };
    let status = reap_observed_target(&mut delayed, "delayed reap", Duration::from_secs(1), &fatal)
        .expect("a transiently unavailable wait status should be retried");
    assert!(status.success());
    assert_eq!(delayed.reaps, 2);

    let mut never_reaped = NeverReaped {
        id: 44,
        group_kills: Cell::new(0),
        direct_kills: Cell::new(0),
    };
    let error = reap_observed_target(
        &mut never_reaped,
        "unreaped fixture",
        Duration::ZERO,
        &fatal,
    )
    .expect_err("a child without a reap proof must remain fatal");
    assert!(error.to_string().contains("was not reaped"));
    assert_eq!(fatal.0.borrow().len(), 1);
}

#[test]
fn fatal_process_errors_are_classified_separately() {
    let fatal = ProcessSpawnError::fatal("containment proof failed");
    assert!(fatal.is_fatal());
    assert!(fatal.to_string().contains("containment proof failed"));

    let generic = ProcessSpawnError::new("ordinary fixture error");
    assert!(!generic.is_fatal());
    let pipe = ProcessSpawnError::new("capture pipe fixture error");
    assert!(!pipe.is_fatal());
}

#[test]
fn reap_policy_covers_preinspection_races_and_signal_failures() {
    let fatal = RecordingFatal::default();
    let mut already_observed = ScriptedTarget::new(vec![Ok(true)], None);
    terminate_target_with(
        &mut already_observed,
        "already observed",
        Duration::ZERO,
        &fatal,
    )
    .expect("an observed zombie pins the exact group until the residual kill");
    assert_eq!(already_observed.group_kills, 2);

    let mut observation_failed = ScriptedTarget::new(
        vec![Err(io::Error::other("fixture observation failure"))],
        None,
    );
    let error = terminate_target_with(
        &mut observation_failed,
        "observation failure",
        Duration::ZERO,
        &fatal,
    )
    .expect_err("an unprovable exit observation must select containment");
    assert!(error.to_string().contains("exit observation failed"));

    let mut esrch_race = ScriptedTarget::new(vec![Ok(true)], Some(Errno::SRCH.raw_os_error()));
    terminate_target_with(&mut esrch_race, "ESRCH race", Duration::ZERO, &fatal)
        .expect("ESRCH is safe only when the direct child is then proven reaped");
    assert_eq!(esrch_race.group_kills, 2);
    assert_eq!(esrch_race.direct_kills, 1);

    let mut group_failed = ScriptedTarget::new(vec![Ok(true)], Some(5));
    let error = terminate_target_with(&mut group_failed, "group failure", Duration::ZERO, &fatal)
        .expect_err("a group signal failure remains fatal after direct-child reap");
    assert!(error.to_string().contains("exact process-group SIGKILL"));
    assert_eq!(group_failed.direct_kills, 1);

    let mut reap_failed = ScriptedTarget::new(vec![Ok(true)], None)
        .with_reap_error(io::Error::other("fixture reap failure"));
    let error = terminate_target_with(&mut reap_failed, "reap failure", Duration::ZERO, &fatal)
        .expect_err("a failed post-signal reap proof must select containment");
    assert!(error.to_string().contains("final direct-child reap failed"));
    assert_eq!(fatal.0.borrow().len(), 3);
}

#[test]
fn capture_workers_surface_reader_panics_errors_and_double_join() {
    let mut failed =
        CaptureWorker::spawn("fixture", FailingReader, 16).expect("the capture worker must start");
    let result = failed
        .receive_until(Instant::now() + Duration::from_secs(1))
        .expect("the reader failure must be delivered");
    assert!(
        result
            .expect_err("the reader must fail")
            .contains("capture failed")
    );
    failed.join().expect("the failed reader thread still joins");
    let error = failed.join().expect_err("a capture worker joins only once");
    assert!(error.to_string().contains("joined twice"));

    let mut panicked = CaptureWorker::spawn("fixture", PanickingReader, 16)
        .expect("the panic fixture worker must start");
    assert!(
        panicked
            .receive_until(Instant::now() + Duration::from_secs(1))
            .is_err(),
        "a panicked capture worker disconnects its result channel"
    );
    let error = panicked
        .join()
        .expect_err("the capture panic must remain visible");
    assert!(error.to_string().contains("capture worker panicked"));
}

#[test]
fn capture_cleanup_and_group_identifier_failures_are_typed() {
    let fatal = RecordingFatal::default();
    let mut closed = CaptureWorker::spawn("fixture", Cursor::new(b"closed".to_vec()), 16)
        .expect("the closed capture worker must start");
    account_single_capture_before_reap(u32::MAX, "closed", &mut closed, &fatal)
        .expect("a closed capture pipe needs no process-group signal");

    let error = signal_owned_process_group(u32::MAX, "overflow", &fatal)
        .expect_err("an overflowing process-group ID must select containment");
    assert!(error.to_string().contains("ID overflow"));
    let error = signal_owned_process_group(0, "zero", &fatal)
        .expect_err("a zero process-group ID must select containment");
    assert!(error.to_string().contains("zero process-group ID"));
}

#[test]
fn direct_child_kill_fallback_targets_only_the_owned_fixture() {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "exec sleep 10"]);
    configure_process_group(&mut command);
    let mut child = command
        .spawn()
        .expect("the direct fixture child must start");
    ReapTarget::signal_direct_kill(&mut child)
        .expect("the fallback must signal the exact direct child");
    let status = child.wait().expect("the exact child must be reaped");
    assert!(!status.success());
}

#[test]
fn capture_runner_keeps_legitimate_success_and_nonzero_status_typed() {
    let mut success = Command::new("/bin/sh");
    success.args(["-c", "printf 'ok'; printf 'note' >&2"]);
    let output = run_capture_command(
        &mut success,
        "success fixture",
        Duration::from_secs(1),
        DEFAULT_OUTPUT_LIMIT,
        |_| {},
    )
    .expect("legitimate child must succeed");
    assert!(output.status.success());
    assert_eq!(output.stdout, b"ok");
    assert_eq!(output.stderr, b"note");

    let mut failure = Command::new("/bin/sh");
    failure.args(["-c", "printf 'failed' >&2; exit 7"]);
    let error = run_capture_command(
        &mut failure,
        "failure fixture",
        Duration::from_secs(1),
        DEFAULT_OUTPUT_LIMIT,
        |_| {},
    )
    .expect_err("nonzero status must return NonZeroExit");

    match error {
        ProcessSpawnError::NonZeroExit {
            command: _,
            exit_code,
            stderr,
        } => {
            assert_eq!(exit_code, 7);
            assert_eq!(stderr, "failed");
        }
        _ => panic!("Expected NonZeroExit"),
    }
}

#[test]
fn capture_runner_rejects_bounded_output_overflow() {
    let mut command = Command::new("head");
    command.args(["-c", "1025", "/dev/zero"]);
    let error = run_capture_command(
        &mut command,
        "overflow fixture",
        Duration::from_secs(1),
        1024,
        |_| {},
    )
    .expect_err("capture storage must remain bounded");
    assert!(error.to_string().contains("output exceeded"), "{error}");
}

#[test]
fn capture_runner_rejects_empty_program() {
    let mut command = Command::new("");
    let error = run_capture_command(
        &mut command,
        "empty program fixture",
        Duration::from_secs(1),
        1024,
        |_| {},
    )
    .expect_err("should reject empty program");
    assert!(error.to_string().contains("empty program"));
}

#[test]
fn capture_runner_rejects_non_existent_binary() {
    let mut command = Command::new("/path/to/nowhere/does/not/exist");
    let error = run_capture_command(
        &mut command,
        "non existent fixture",
        Duration::from_secs(1),
        1024,
        |_| {},
    )
    .expect_err("should reject non existent binary");
    assert!(
        error.to_string().contains("binary not found")
            || error.to_string().contains("binary path does not exist")
    );
}

#[test]
fn capture_runner_rejects_null_bytes_in_args() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let mut command = Command::new("/bin/sh");
    let arg = OsStr::from_bytes(b"hello\0world");
    command.arg(arg);
    let error = run_capture_command(
        &mut command,
        "null bytes fixture",
        Duration::from_secs(1),
        1024,
        |_| {},
    )
    .expect_err("should reject null bytes in arguments");
    assert!(error.to_string().contains("nul byte"));
}

#[test]
fn capture_runner_rejects_empty_argument() {
    let mut command = Command::new("/bin/sh");
    command.arg("");
    let error = run_capture_command(
        &mut command,
        "empty argument fixture",
        Duration::from_secs(1),
        1024,
        |_| {},
    )
    .expect_err("should reject empty arguments");
    assert!(error.to_string().contains("empty argument"));
}

#[test]
// TestName: capture_runner_reaps_successful_leader_and_all_stdio_redirected_descendant
fn capture_runner_reaps_successful_leader_and_all_stdio_redirected_descendant() {
    // `cmd &` forks the descendant before its redirections apply, so that
    // descendant briefly holds the capture pipes; the close-grace can expire
    // inside that window under load and the run then fails closed as a stuck
    // pipe. `exec` redirects the leader's own stdio first (releasing the pipe
    // write end), so the descendant is forked already detached from the
    // capture and the close is deterministic. The PID cannot ride the capture
    // stdout after that redirect, so it lands in a pid file.
    let pid_path = std::env::temp_dir().join(format!(
        "ramshared-bounded-process-descendant-{}.pid",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&pid_path);
    let pid_arg = pid_path.to_string_lossy().into_owned();
    let mut command = Command::new("/bin/sh");
    command.args([
        "-c",
        "printf '%s\\n' 'leader-output'; exec </dev/null >/dev/null 2>/dev/null; sleep 10 & printf '%s\\n' \"$!\" > \"$1\"; exit 0",
        "sh",
        pid_arg.as_str(),
    ]);
    let output = run_capture_command(
        &mut command,
        "redirected descendant fixture",
        Duration::from_secs(1),
        DEFAULT_OUTPUT_LIMIT,
        |_| {},
    )
    .expect("a successful leader remains a legitimate success");
    let pid_text = std::fs::read_to_string(&pid_path)
        .expect("fixture writes its exact descendant PID to the pid file");
    let _ = std::fs::remove_file(&pid_path);
    let descendant = pid_text
        .trim()
        .parse::<u32>()
        .expect("fixture prints its exact descendant PID");
    let gone = wait_for_fixture_process_exit(descendant);
    if !gone {
        kill_exact_fixture_process(descendant);
        let _ = wait_for_fixture_process_exit(descendant);
    }

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "leader-output",
        "the leader must still be captured before it releases the pipes"
    );
    assert!(
        gone,
        "a descendant with every stdio stream redirected survived its successful leader"
    );
}

#[test]
// TestName: capture_runner_on_spawn_panic_cannot_strand_owned_child
fn capture_runner_on_spawn_panic_cannot_strand_owned_child() {
    let spawned = Cell::new(None);
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "exec sleep 10"]);
    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = run_capture_command(
            &mut command,
            "on-spawn panic fixture",
            Duration::from_secs(1),
            DEFAULT_OUTPUT_LIMIT,
            |pid| {
                spawned.set(Some(pid));
                panic!("injected on_spawn panic");
            },
        );
    }));
    let pid = spawned
        .get()
        .expect("fixture child was spawned before panic");
    let stranded = fixture_process_exists(pid);
    if stranded {
        kill_and_reap_exact_fixture_child(pid);
    }

    assert!(
        unwind.is_err(),
        "the injected panic must resume after cleanup"
    );
    assert!(!stranded, "on_spawn panic stranded its exact fixture child");
}

#[test]
fn capture_runner_never_executes_a_nul_bearing_program_name() {
    use std::os::unix::ffi::OsStrExt;
    let program = std::ffi::OsStr::from_bytes(b"bad\0program");
    let mut command = Command::new(program);
    // `std::process::Command` replaces an interior NUL in its accessors
    // (`get_program`/`get_args`) with the placeholder `<string-with-nul>`,
    // so the explicit guard inside `run_capture_command` cannot observe the
    // NUL through this API. The safety property that still holds is the one
    // that matters: the NUL never reaches `exec`, and the failure is typed
    // rather than a successful run of a renamed binary.
    assert!(
        !command.get_program().as_bytes().contains(&0),
        "std must not hand a raw NUL back through get_program"
    );
    let error = run_capture_command(
        &mut command,
        "nul program fixture",
        Duration::from_secs(1),
        1024,
        |_| {},
    )
    .expect_err("a program name with an interior NUL must be refused");
    assert!(
        !matches!(error, ProcessSpawnError::GenericError { .. }) && !error.is_fatal(),
        "a NUL program name must stay a typed spawn failure: {error}"
    );
}
