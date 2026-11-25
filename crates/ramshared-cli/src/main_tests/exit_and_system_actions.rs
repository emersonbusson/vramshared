// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/main_tests/exit_and_system_actions.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;

#[test]
fn release_sync_after_move_exits_on_residual_failing_units() {
    let clean = release_coherence::JointMoveReport(vec![
        ("ramsharedd.service".to_string(), MoveOutcome::Restarted),
        (
            "ramshared-cascade.service".to_string(),
            MoveOutcome::AlreadyCurrent,
        ),
    ]);
    let mut out = Vec::new();
    let mut err = Vec::new();
    assert_eq!(
        release_sync_after_move(&clean, &mut out, &mut err),
        ExitCode::SUCCESS
    );
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains("[release-sync] ramsharedd.service: restarted"));
    assert!(text.contains("[release-sync] ramshared-cascade.service: already_current"));
    assert!(err.is_empty());

    let residual = release_coherence::JointMoveReport(vec![
        ("ramsharedd.service".to_string(), MoveOutcome::Restarted),
        (
            "ramshared-supervisor.service".to_string(),
            MoveOutcome::Failed {
                observed: "failed".to_string(),
            },
        ),
    ]);
    out.clear();
    assert_eq!(
        release_sync_after_move(&residual, &mut out, &mut err),
        ExitCode::from(1)
    );
    assert!(
        String::from_utf8_lossy(&out)
            .contains("[release-sync] ramshared-supervisor.service: failed: failed")
    );
    assert!(String::from_utf8_lossy(&err).contains("1 unit(s) did not reach"));
}

#[test]
fn release_sync_without_selector_is_an_empty_fleet_not_a_partial_one() {
    let mut out = Vec::new();
    assert_eq!(release_sync_without_selector(&mut out), ExitCode::SUCCESS);
    assert_eq!(
        String::from_utf8_lossy(&out),
        "[release-sync] no selected release; nothing to move\n"
    );
}

#[test]
fn recover_status_exit_reports_a_failed_write_as_failure() {
    let mut out = Vec::new();
    let mut err = Vec::new();
    assert_eq!(
        recover_status_exit("resume pending".to_string(), &mut out, &mut err),
        ExitCode::SUCCESS
    );
    assert_eq!(String::from_utf8_lossy(&out), "resume pending\n");
    assert!(err.is_empty());

    assert_eq!(
        recover_status_exit("resume pending".to_string(), &mut FailingWriter, &mut err),
        ExitCode::from(1)
    );
    assert!(String::from_utf8_lossy(&err).contains("failed to write recovery status"));
}

#[test]
fn parse_refusal_exit_is_an_operator_mistake_not_a_runtime_failure() {
    let mut err = Vec::new();
    assert_eq!(
        parse_refusal_exit("unknown option --plan", &mut err),
        ExitCode::from(2)
    );
    assert_eq!(
        String::from_utf8_lossy(&err),
        "error: unknown option --plan\n"
    );
}

#[test]
fn write_joint_move_report_counts_residual_failures() {
    let report = release_coherence::JointMoveReport(vec![
        ("a.service".to_string(), MoveOutcome::LeftInactive),
        (
            "b.service".to_string(),
            MoveOutcome::Failed {
                observed: "inactive".to_string(),
            },
        ),
        (
            "c.service".to_string(),
            MoveOutcome::Failed {
                observed: "failed".to_string(),
            },
        ),
    ]);
    let mut out = Vec::new();
    assert_eq!(write_joint_move_report(&report, &mut out), 2);
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains("[release-sync] a.service: left_inactive"));
    assert!(text.contains("[release-sync] b.service: failed: inactive"));
    assert!(text.contains("[release-sync] c.service: failed: failed"));
}

/// `workload::run` parses before it opens `/proc/meminfo` or takes any
/// scope, so an unknown option is a refusal, not an effect. `session`
/// only appends `-- $SHELL` and then takes the same gate. Calling the
/// action-runner methods here is therefore a unit test of the exit
/// mapping, not a live workload on this host.
#[test]
fn system_actions_run_and_session_refuse_before_any_host_effect() {
    let mut actions = SystemCliActions;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    let exit = actions.run_workload(&["--unknown-option".to_string()], &mut stdout, &mut stderr);
    assert_eq!(exit, ExitCode::from(1));
    assert!(String::from_utf8_lossy(&stderr).contains("usage: ramshared run"));

    stdout.clear();
    stderr.clear();
    let exit = actions.session(&[], &mut stdout, &mut stderr);
    assert_eq!(exit, ExitCode::from(1));
    assert!(String::from_utf8_lossy(&stderr).contains("--class is required"));
}

/// `supervisor::run` matches the argv before it constructs a supervisor
/// or opens any state path, so an unknown option reaches the loop only
/// as a refusal.
#[test]
fn system_actions_supervise_refuses_unknown_options_before_the_loop() {
    let mut actions = SystemCliActions;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    let exit = actions.supervise(&["--unknown-option".to_string()], &mut stdout, &mut stderr);
    assert_eq!(exit, ExitCode::from(1));
    assert!(String::from_utf8_lossy(&stderr).contains("usage: ramshared supervise [--once]"));
}

/// `workload::recover_status` only tests two paths and renders JSON; it
/// never resumes. The status arm of `recover` is therefore reachable
/// without touching the recovery effect (`--resume`).
#[test]
fn system_actions_recover_status_is_a_read_only_observation() {
    let mut actions = SystemCliActions;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    let exit = actions.recover(false, &mut stdout, &mut stderr);
    assert_eq!(exit, ExitCode::SUCCESS);
    let status = String::from_utf8_lossy(&stdout);
    // The recover document has its own schema version (4); it is not
    // the cascade status document's.
    assert!(status.contains("\"schema_version\":4"));
    assert!(status.contains("\"safe_mode\":"));
    assert!(status.contains("\"resume_lease\":"));
}

#[test]
fn run_from_args_dispatches_all_command_variants() {
    let mut actions = RecordingCliActions::default();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    for args in [
        &["run", "--", "echo", "test"][..],
        &["session", "--class", "build"][..],
        &["supervise", "--once"][..],
        &["recover", "--status"][..],
        &["recover", "--resume"][..],
        &["check"][..],
        &["check", "--json"][..],
        &["doctor"][..],
        &["doctor", "--json"][..],
        &["config"][..],
        &["config", "show"][..],
        &["config", "show", "--json"][..],
        &["up", "--vram", "1024"][..],
        &["migrate-cascade", "--from-legacy"][..],
        &["down"][..],
        &["status"][..],
        &["status", "--json"][..],
        &["monitor", "--once"][..],
        &["diagnose", "--events", "/dev/null"][..],
        &["--help"][..],
    ] {
        stdout.clear();
        stderr.clear();
        let exit = run_from_args(&cli_args(args), &mut actions, &mut stdout, &mut stderr);
        assert_eq!(exit, ExitCode::SUCCESS);
    }

    // Invalid command
    stdout.clear();
    stderr.clear();
    let exit = run_from_args(
        &cli_args(&["--unknown-flag"]),
        &mut actions,
        &mut stdout,
        &mut stderr,
    );
    assert_eq!(exit, ExitCode::from(2));
    assert!(!stderr.is_empty());
}

#[test]
fn system_actions_check_and_doctor_execute_safely() {
    let mut actions = SystemCliActions;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    let _ = actions.check(true, &mut stdout, &mut stderr);
    stdout.clear();
    stderr.clear();
    let _ = actions.check(false, &mut stdout, &mut stderr);
    stdout.clear();
    stderr.clear();
    let _ = actions.doctor(true, &mut stdout, &mut stderr);
    stdout.clear();
    stderr.clear();
    let _ = actions.doctor(false, &mut stdout, &mut stderr);
}

#[test]
fn run_from_args_covers_down_variants_and_version_surfaces() {
    let mut actions = RecordingCliActions::default();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    for args in [
        &["version"][..],
        &["--build-info"][..],
        &["down", "--recover-released-peer"][..],
        &["down", "--plan"][..],
        &["down", "--plan", "--json"][..],
        &["ensure-origin"][..],
        &["ensure-guardian"][..],
        &["boot"][..],
        &[
            "swap-rebalance",
            "--plan",
            "--json",
            "--target",
            "/dev/nvme0n1",
        ][..],
        &["release-sync"][..],
    ] {
        stdout.clear();
        stderr.clear();
        assert_eq!(
            run_from_args(&cli_args(args), &mut actions, &mut stdout, &mut stderr),
            ExitCode::SUCCESS
        );
    }
    assert!(stderr.is_empty());
}

#[test]
fn finish_up_and_scope_helpers_cover_plan_wrap_sync_and_errors() {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let exit = finish_up_with(
        &cli_args(&["--plan", "--json"]),
        true,
        |_, _| unreachable!("wrap skipped for --plan"),
        |_| -> Result<(), &str> { unreachable!("up skipped for --plan") },
        || None,
        &mut stdout,
        &mut stderr,
    );
    assert_eq!(exit, ExitCode::SUCCESS);
    assert!(String::from_utf8_lossy(&stdout).contains("\"plan\"") || !stdout.is_empty());

    stdout.clear();
    stderr.clear();
    let exit = finish_up_with(
        &cli_args(&["--vram", "512"]),
        true,
        |args, _| {
            assert_eq!(args, &cli_args(&["--vram", "512"]));
            ExitCode::from(7)
        },
        |_| -> Result<(), &str> { unreachable!("up skipped when wrapped") },
        || None,
        &mut stdout,
        &mut stderr,
    );
    assert_eq!(exit, ExitCode::from(7));

    stdout.clear();
    stderr.clear();
    let exit = finish_up_with(
        &[],
        false,
        |_, _| unreachable!(),
        |_| Ok::<(), &str>(()),
        || {
            Some(release_coherence::JointMoveReport(vec![(
                "ramsharedd.service".to_string(),
                MoveOutcome::Restarted,
            )]))
        },
        &mut stdout,
        &mut stderr,
    );
    assert_eq!(exit, ExitCode::SUCCESS);
    assert!(
        String::from_utf8_lossy(&stdout).contains("[release-sync] ramsharedd.service: restarted")
    );

    stdout.clear();
    stderr.clear();
    let exit = finish_up_with(
        &[],
        false,
        |_, _| unreachable!(),
        |_| Err("up failed"),
        || unreachable!("sync skipped on up failure"),
        &mut stdout,
        &mut stderr,
    );
    assert_eq!(exit, ExitCode::from(1));
    assert!(String::from_utf8_lossy(&stderr).contains("up failed"));
}

#[test]
fn finish_release_sync_boot_rebalance_recover_and_systemd_exit_cover_all_paths() {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    assert_eq!(
        finish_release_sync(None, &mut stdout, &mut stderr),
        ExitCode::SUCCESS
    );
    assert!(String::from_utf8_lossy(&stdout).contains("nothing to move"));

    stdout.clear();
    stderr.clear();
    let report = release_coherence::JointMoveReport(vec![(
        "ramsharedd.service".to_string(),
        MoveOutcome::Failed {
            observed: "boom".to_string(),
        },
    )]);
    assert_eq!(
        finish_release_sync(Some(report), &mut stdout, &mut stderr),
        ExitCode::from(1)
    );
    assert!(String::from_utf8_lossy(&stderr).contains("1 unit(s)"));

    stdout.clear();
    stderr.clear();
    let ok = cascade::BootConfig {
        vram_mib: 512,
        zram_mib: 1024,
        min_vram_headroom_mib: 256,
    };
    assert_eq!(
        finish_boot_result(&Ok(ok), &mut stdout, &mut stderr),
        ExitCode::SUCCESS
    );
    let refused = cascade::BootError::DeployRefused;
    assert_eq!(
        finish_boot_result(&Err(refused), &mut stdout, &mut stderr),
        ExitCode::from(1)
    );

    stderr.clear();
    assert_eq!(
        finish_swap_rebalance_with(
            &SwapRebalanceMode::Plan { json: true },
            "zram0",
            |target, json| {
                assert_eq!(target, "zram0");
                assert!(json);
                Ok::<(), &str>(())
            },
            |_| unreachable!(),
            &mut stderr,
        ),
        ExitCode::SUCCESS
    );
    assert_eq!(
        finish_swap_rebalance_with(
            &SwapRebalanceMode::Apply,
            "zram0",
            |_, _| unreachable!(),
            |_| Err("apply failed"),
            &mut stderr,
        ),
        ExitCode::from(1)
    );

    stdout.clear();
    stderr.clear();
    assert_eq!(
        finish_recover_with(true, || Ok(()), || unreachable!(), &mut stdout, &mut stderr),
        ExitCode::SUCCESS
    );
    assert_eq!(
        finish_recover_with(
            false,
            || unreachable!(),
            || Ok("ok".to_string()),
            &mut stdout,
            &mut stderr,
        ),
        ExitCode::SUCCESS
    );
    assert_eq!(
        finish_recover_with(
            false,
            || unreachable!(),
            || Err("read failed".to_string()),
            &mut stdout,
            &mut stderr,
        ),
        ExitCode::from(1)
    );

    let cmd = systemd_scope_command(
        Path::new("/usr/bin/ramshared"),
        &cli_args(&["--vram", "512"]),
    );
    let cmd_args: Vec<_> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        cmd_args,
        vec![
            "--scope",
            "-q",
            "--",
            "/usr/bin/ramshared",
            "up",
            "--vram",
            "512"
        ]
    );

    stderr.clear();
    let exit_status = Command::new("/bin/sh").args(["-c", "exit 3"]).status();
    assert_eq!(
        systemd_scope_exit(exit_status, &mut stderr),
        ExitCode::from(3)
    );

    stderr.clear();
    let missing = Command::new("/nonexistent/ramshared-missing-bin").status();
    assert_eq!(systemd_scope_exit(missing, &mut stderr), ExitCode::from(1));
    assert!(String::from_utf8_lossy(&stderr).contains("failed to spawn systemd-run --scope"));

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        stderr.clear();
        assert_eq!(
            systemd_scope_exit(Ok(std::process::ExitStatus::from_raw(9)), &mut stderr),
            ExitCode::from(1)
        );
    }
}
