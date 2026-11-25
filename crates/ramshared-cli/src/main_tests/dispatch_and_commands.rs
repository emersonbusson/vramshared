// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/main_tests/dispatch_and_commands.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;

#[test]
fn parser_and_state_variants_cover_fail_closed_edges() {
    assert_eq!(Decision::Blocked.as_str(), "blocked");
    assert_eq!(parse_cli_command(&[]).unwrap(), CliCommand::Help);
    assert!(parse_cli_command(&cli_args(&["version", "extra"])).is_err());
    assert!(parse_cli_command(&cli_args(&["supervise", "--bad"])).is_err());
    assert_eq!(
        parse_cli_command(&cli_args(&["down"])).unwrap(),
        CliCommand::Down
    );
}

#[test]
fn swap_rebalance_dispatches_exact_plan_and_apply() {
    let mut actions = RecordingCliActions::default();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let exit = run_from_args(
        &cli_args(&["swap-rebalance", "--plan", "--json", "--target", "/dev/sdf"]),
        &mut actions,
        &mut stdout,
        &mut stderr,
    );
    assert_eq!(exit, ExitCode::SUCCESS);
    assert_eq!(
        actions.calls,
        vec![CliCommand::SwapRebalance {
            mode: SwapRebalanceMode::Plan { json: true },
            target: "/dev/sdf".into(),
        }]
    );

    let mut actions = RecordingCliActions::default();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let target = "UUID=11111111-2222-4333-8444-555555555555";
    let exit = run_from_args(
        &cli_args(&["swap-rebalance", "--apply", "--target", target]),
        &mut actions,
        &mut stdout,
        &mut stderr,
    );
    assert_eq!(exit, ExitCode::SUCCESS);
    assert_eq!(
        actions.calls,
        vec![CliCommand::SwapRebalance {
            mode: SwapRebalanceMode::Apply,
            target: target.into(),
        }]
    );
}

#[test]
fn swap_rebalance_parse_error_does_not_dispatch() {
    for args in [
        cli_args(&["swap-rebalance", "--plan", "--target", "sdf"]),
        cli_args(&[
            "swap-rebalance",
            "--apply",
            "--target",
            "/dev/sdf",
            "--json",
        ]),
        cli_args(&[
            "swap-rebalance",
            "--plan",
            "--target",
            "/dev/sdf",
            "--force",
        ]),
        cli_args(&["swap-rebalance", "--all"]),
    ] {
        let mut actions = RecordingCliActions::default();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = run_from_args(&args, &mut actions, &mut stdout, &mut stderr);

        assert_eq!(exit, ExitCode::from(2));
        assert!(actions.calls.is_empty(), "{args:?} must not dispatch");
        assert!(stdout.is_empty());
        assert!(String::from_utf8_lossy(&stderr).contains("invalid"));
    }
}

#[test]
fn version_is_public_and_side_effect_free() {
    let mut actions = RecordingCliActions::default();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let exit = run_from_args(
        &cli_args(&["--version"]),
        &mut actions,
        &mut stdout,
        &mut stderr,
    );
    assert_eq!(exit, ExitCode::SUCCESS);
    assert!(actions.calls.is_empty());
    let output = String::from_utf8(stdout).expect("version output is UTF-8");
    let mut lines = output.lines();
    let build_line = lines.next().expect("version/build identity line");
    assert!(build_line.starts_with(&format!("RamShared CLI v{} · ", env!("CARGO_PKG_VERSION"))));
    assert!(!build_line.contains("git "));
    let build_info = installed_image::build_info_lines();
    if let Some(commit) = build_info
        .lines()
        .find_map(|line| line.strip_prefix("source_commit="))
        .filter(|commit| commit.len() == 40)
    {
        assert!(build_line.contains(&commit[..8]));
        assert!(!build_line.contains(commit));
    }
    assert!(
        lines
            .next()
            .is_some_and(|line| line.starts_with("Running: "))
    );
    // The install block collapses to one line when the named installs are
    // the same image, and expands only when they differ. Either shape must
    // carry the install identity under the "Installed" heading, and the
    // author line closes the output.
    let author = "(Author: Emerson Busson - https://www.linkedin.com/in/emersonbusson)";
    let rest: Vec<&str> = lines.collect();
    assert_eq!(
        rest.last().copied(),
        Some(author),
        "author line closes: {rest:?}"
    );
    let install_block = &rest[..rest.len().saturating_sub(1)];
    assert!(
        install_block
            .iter()
            .any(|line| line.starts_with("Installed: ") || line.starts_with("Installed ")),
        "version output must report the install identity: {install_block:?}"
    );
    assert!(
        install_block.iter().all(|line| {
            line.starts_with("Installed: ")
                || line.starts_with("Installed ")
                || line.starts_with("Missing: ")
        }),
        "only install lines belong in the install block: {install_block:?}"
    );
    assert!(stderr.is_empty());
}

#[test]
fn build_info_keeps_full_commit_for_audit_tools() {
    let mut actions = RecordingCliActions::default();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let exit = run_from_args(
        &cli_args(&["--build-info"]),
        &mut actions,
        &mut stdout,
        &mut stderr,
    );
    assert_eq!(exit, ExitCode::SUCCESS);
    assert!(actions.calls.is_empty());
    let output = String::from_utf8(stdout).expect("build info is UTF-8");
    let fields = output
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(
        fields.get("version").copied(),
        Some(env!("CARGO_PKG_VERSION"))
    );
    assert!(fields.get("source_commit").copied().is_some_and(|commit| {
        commit == "unavailable"
            || (commit.len() == 40 && commit.bytes().all(|byte| byte.is_ascii_hexdigit()))
    }));
    assert!(matches!(
        fields.get("source_tree_state").copied(),
        Some("clean" | "dirty" | "unavailable")
    ));
    assert!(stderr.is_empty());
}

#[test]
fn monitor_defaults_are_read_only_and_bounded() {
    let command = parse_cli_command(&cli_args(&["monitor"])).expect("monitor parses");
    assert_eq!(
        command,
        CliCommand::Monitor {
            options: MonitorOptions {
                jsonl: false,
                compact: false,
                interval_ms: 1_000,
                history_seconds: 300,
                output: None,
                heartbeat: None,
                benchmark_evidence: None,
                once: false,
                language: Language::En,
            }
        }
    );
}

#[test]
fn config_show_dispatches_to_read_only_action() {
    let mut actions = RecordingCliActions::default();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    let exit = run_from_args(
        &cli_args(&["config", "show", "--json"]),
        &mut actions,
        &mut stdout,
        &mut stderr,
    );

    assert_eq!(exit, ExitCode::SUCCESS);
    assert_eq!(
        actions.calls,
        vec![CliCommand::Config {
            mode: ConfigMode::Show { json: true },
        }]
    );
}

#[test]
fn config_plan_dispatches_to_read_only_action() {
    let mut actions = RecordingCliActions::default();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    let exit = run_from_args(
        &cli_args(&["config", "plan", "--json"]),
        &mut actions,
        &mut stdout,
        &mut stderr,
    );

    assert_eq!(exit, ExitCode::SUCCESS);
    assert_eq!(
        actions.calls,
        vec![CliCommand::Config {
            mode: ConfigMode::Plan {
                json: true,
                profile_path: None,
            },
        }]
    );
}

#[test]
fn monitor_parses_machine_stream_outputs_without_mutation_flags() {
    let command = parse_cli_command(&cli_args(&[
        "monitor",
        "--jsonl",
        "--interval-ms",
        "2500",
        "--history-seconds",
        "600",
        "--output",
        "/tmp/health.jsonl",
        "--heartbeat",
        "/tmp/heartbeat.json",
        "--benchmark-evidence",
        "/tmp/evidence.json",
        "--once",
    ]))
    .expect("monitor stream parses");
    assert_eq!(
        command,
        CliCommand::Monitor {
            options: MonitorOptions {
                jsonl: true,
                compact: false,
                interval_ms: 2_500,
                history_seconds: 600,
                output: Some(PathBuf::from("/tmp/health.jsonl")),
                heartbeat: Some(PathBuf::from("/tmp/heartbeat.json")),
                benchmark_evidence: Some(PathBuf::from("/tmp/evidence.json")),
                once: true,
                language: Language::En,
            }
        }
    );

    let error = parse_cli_command(&cli_args(&["monitor", "--activate"]))
        .expect_err("monitor has no mutation controls");
    assert!(matches!(error, CliParseError::InvalidOption { .. }));
}

/// `setup` is a new command surface: the top-level parser only splits off
/// the command name and hands the rest to the surface's own strict parser,
/// which reports every refusal through the PRD mapping.
#[test]
fn setup_command_is_a_new_surface_with_one_admitted_subcommand() {
    assert_eq!(
        parse_cli_command(&cli_args(&[
            "setup",
            "plan",
            "--profile",
            "resource-profile",
            "--output",
            "plan.json",
            "--json"
        ]))
        .unwrap(),
        CliCommand::Setup {
            args: cli_args(&[
                "plan",
                "--profile",
                "resource-profile",
                "--output",
                "plan.json",
                "--json"
            ]),
        }
    );
    // `apply` reaches the surface parser, which refuses it: the top-level
    // parser does not decide what the surface admits.
    assert_eq!(
        parse_cli_command(&cli_args(&["setup", "apply"])).unwrap(),
        CliCommand::Setup {
            args: cli_args(&["apply"]),
        }
    );
    assert_eq!(
        parse_cli_command(&cli_args(&["setup"])).unwrap(),
        CliCommand::Setup { args: Vec::new() }
    );
}

#[test]
fn boot_command_parses_and_dispatches() {
    assert_eq!(
        parse_cli_command(&cli_args(&["boot"])).unwrap(),
        CliCommand::Boot
    );
    let mut actions = RecordingCliActions::default();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let exit = run_from_args(&cli_args(&["boot"]), &mut actions, &mut stdout, &mut stderr);
    assert_eq!(exit, ExitCode::SUCCESS);
    assert_eq!(actions.calls, vec![CliCommand::Boot]);
}

#[test]
fn release_sync_command_parses_and_dispatches() {
    // DT-3: the joint-move verb is not a start/stop surface. It takes no
    // options, so a flag can never widen it into one.
    assert_eq!(
        parse_cli_command(&cli_args(&["release-sync"])).unwrap(),
        CliCommand::ReleaseSync
    );
    let mut actions = RecordingCliActions::default();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let exit = run_from_args(
        &cli_args(&["release-sync"]),
        &mut actions,
        &mut stdout,
        &mut stderr,
    );
    assert_eq!(exit, ExitCode::SUCCESS);
    assert_eq!(actions.calls, vec![CliCommand::ReleaseSync]);
    for args in [
        cli_args(&["release-sync", "--force"]),
        cli_args(&["release-sync", "--json"]),
        cli_args(&["release-sync", "start"]),
    ] {
        let mut refused = RecordingCliActions::default();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = run_from_args(&args, &mut refused, &mut stdout, &mut stderr);
        assert_eq!(exit, ExitCode::from(2), "{args:?}");
        assert!(
            refused.calls.is_empty(),
            "{args:?} must not reach an action"
        );
    }
}

#[test]
fn ensure_origin_command_parses_and_dispatches() {
    // verified test case: this verb is the boot-gate prerequisite. It takes no
    // options — a flag here would be a sizing or approval surface that
    // does not belong on an idempotent host preparation step.
    assert_eq!(
        parse_cli_command(&cli_args(&["ensure-origin"])).unwrap(),
        CliCommand::EnsureOrigin
    );
    let mut actions = RecordingCliActions::default();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let exit = run_from_args(
        &cli_args(&["ensure-origin"]),
        &mut actions,
        &mut stdout,
        &mut stderr,
    );
    assert_eq!(exit, ExitCode::SUCCESS);
    assert_eq!(actions.calls, vec![CliCommand::EnsureOrigin]);
    for args in [
        cli_args(&["ensure-origin", "--vram", "1024"]),
        cli_args(&["ensure-origin", "--force"]),
        cli_args(&["ensure-origin", "--json"]),
    ] {
        let mut refused = RecordingCliActions::default();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = run_from_args(&args, &mut refused, &mut stdout, &mut stderr);
        assert_eq!(exit, ExitCode::from(2), "{args:?} must refuse");
        assert!(
            refused.calls.is_empty(),
            "{args:?} must not reach the action"
        );
    }
}

#[test]
fn ensure_guardian_command_parses_and_dispatches() {
    // DT-19: the guest-restart twin of `ensure-origin`. It takes no
    // options — a flag here would be a task-name, sizing or approval
    // surface that does not belong on an idempotent producer start. The
    // task name comes from the sealed document, never from argv.
    assert_eq!(
        parse_cli_command(&cli_args(&["ensure-guardian"])).unwrap(),
        CliCommand::EnsureGuardian
    );
    let mut actions = RecordingCliActions::default();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let exit = run_from_args(
        &cli_args(&["ensure-guardian"]),
        &mut actions,
        &mut stdout,
        &mut stderr,
    );
    assert_eq!(exit, ExitCode::SUCCESS);
    assert_eq!(actions.calls, vec![CliCommand::EnsureGuardian]);
    for args in [
        cli_args(&["ensure-guardian", "--vram", "1024"]),
        cli_args(&["ensure-guardian", "--force"]),
        cli_args(&["ensure-guardian", "--json"]),
        cli_args(&["ensure-guardian", "--task-name", "RamSharedWslGuardian.v1"]),
    ] {
        let mut refused = RecordingCliActions::default();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = run_from_args(&args, &mut refused, &mut stdout, &mut stderr);
        assert_eq!(exit, ExitCode::from(2), "{args:?} must refuse");
        assert!(
            refused.calls.is_empty(),
            "{args:?} must not reach the action"
        );
    }
}

#[test]
fn boot_command_rejects_unknown_options() {
    // DT-7: `boot` has no deploy API. Every deploy-shaped flag and every
    // unknown flag is refused by the parser before any gate runs.
    for args in [
        cli_args(&["boot", "--install"]),
        cli_args(&["boot", "--upgrade"]),
        cli_args(&["boot", "--deploy"]),
        cli_args(&["boot", "--replace"]),
        cli_args(&["boot", "--release"]),
        cli_args(&["boot", "--release=0.14.0"]),
        cli_args(&["boot", "--force"]),
        cli_args(&["boot", "--uninstall"]),
        cli_args(&["boot", "--json"]),
        cli_args(&["boot", "--check"]),
    ] {
        let mut actions = RecordingCliActions::default();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = run_from_args(&args, &mut actions, &mut stdout, &mut stderr);

        assert_eq!(exit, ExitCode::from(2), "{args:?} must refuse");
        assert!(
            actions.calls.is_empty(),
            "{args:?} must not reach the boot action"
        );
        assert!(
            String::from_utf8_lossy(&stderr).contains("invalid"),
            "{args:?} must explain the refusal"
        );
    }
}

#[test]
fn up_auto_envelops_in_systemd_scope_when_invocation_id_missing() {
    let env_empty = |_key: &str| Err(std::env::VarError::NotPresent);
    assert!(should_auto_wrap_systemd_scope(&env_empty, true));

    // When systemd is not running, do not attempt systemd-run
    assert!(!should_auto_wrap_systemd_scope(&env_empty, false));

    // When RAMSHARED_NO_AUTO_SCOPE is set, do not auto-wrap
    let env_no_scope = |key: &str| {
        if key == "RAMSHARED_NO_AUTO_SCOPE" {
            Ok("1".to_string())
        } else {
            Err(std::env::VarError::NotPresent)
        }
    };
    assert!(!should_auto_wrap_systemd_scope(&env_no_scope, true));

    // When recursion guard _RAMSHARED_SCOPED is set, do not re-wrap
    let env_scoped = |key: &str| {
        if key == "_RAMSHARED_SCOPED" {
            Ok("1".to_string())
        } else {
            Err(std::env::VarError::NotPresent)
        }
    };
    assert!(!should_auto_wrap_systemd_scope(&env_scoped, true));
}

#[test]
fn up_executes_inline_when_invocation_id_present() {
    let env_with_invocation = |key: &str| {
        if key == "INVOCATION_ID" {
            Ok("0123456789abcdef0123456789abcdef".to_string())
        } else {
            Err(std::env::VarError::NotPresent)
        }
    };
    assert!(!should_auto_wrap_systemd_scope(&env_with_invocation, true));
    assert!(!should_auto_wrap_systemd_scope(&env_with_invocation, false));
}

#[test]
fn exit_helper_and_usage_cover_all_branches() {
    let mut err_buf = Vec::new();
    let ok_exit = to_exit::<&str>(Ok(()), &mut err_buf);
    assert_eq!(ok_exit, ExitCode::SUCCESS);
    assert!(err_buf.is_empty());

    let err_exit = to_exit(Err("forced failure"), &mut err_buf);
    assert_ne!(err_exit, ExitCode::SUCCESS);
    assert!(String::from_utf8_lossy(&err_buf).contains("forced failure"));

    let mut usage_buf = Vec::new();
    print_usage(&mut usage_buf);
    assert!(String::from_utf8_lossy(&usage_buf).contains("usage:"));
}

#[test]
fn decision_exit_maps_readiness_onto_process_exit() {
    assert_eq!(decision_exit(Decision::Ready), ExitCode::SUCCESS);
    assert_eq!(decision_exit(Decision::Blocked), ExitCode::from(1));
}

#[test]
fn finish_report_prefers_a_failed_write_over_the_decision() {
    let mut err = Vec::new();
    assert_eq!(
        finish_report(
            Ok::<(), &str>(()),
            Decision::Ready,
            "check report",
            &mut err
        ),
        ExitCode::SUCCESS
    );
    assert!(err.is_empty());

    assert_eq!(
        finish_report(
            Ok::<(), &str>(()),
            Decision::Blocked,
            "check report",
            &mut err
        ),
        ExitCode::from(1)
    );
    assert!(err.is_empty());

    assert_eq!(
        finish_report(Err("disk full"), Decision::Ready, "doctor report", &mut err),
        ExitCode::from(1)
    );
    let text = String::from_utf8_lossy(&err);
    assert!(text.contains("failed to write doctor report"));
    assert!(text.contains("disk full"));

    assert_eq!(
        finish_report(
            Err("disk full"),
            Decision::Blocked,
            "check report",
            &mut FailingWriter
        ),
        ExitCode::from(1)
    );
}

#[test]
fn announce_prepared_prints_success_or_falls_through_to_to_exit() {
    let mut out = Vec::new();
    let mut err = Vec::new();
    assert_eq!(
        announce_prepared(
            Ok::<(), &str>(()),
            "ensure-origin",
            "sealed origin device is attached",
            &mut out,
            &mut err
        ),
        ExitCode::SUCCESS
    );
    assert_eq!(
        String::from_utf8_lossy(&out),
        "[ensure-origin] sealed origin device is attached\n"
    );
    assert!(err.is_empty());

    out.clear();
    assert_eq!(
        announce_prepared(
            Err("lease stale"),
            "ensure-guardian",
            "sealed guardian producer is running",
            &mut out,
            &mut err
        ),
        ExitCode::from(1)
    );
    assert!(out.is_empty());
    assert!(String::from_utf8_lossy(&err).contains("lease stale"));
}

#[test]
fn boot_announcement_names_the_active_budgets_or_the_refusal() {
    let ok = cascade::BootConfig {
        vram_mib: 1024,
        zram_mib: 2048,
        min_vram_headroom_mib: 640,
    };
    assert_eq!(
        boot_announcement(&Ok(ok)),
        "[boot] cascade active (vram=1024MiB zram=2048MiB headroom=640MiB)"
    );

    let refused = cascade::BootError::HostPrerequisite("sealed origin missing");
    assert_eq!(
        boot_announcement(&Err(refused)),
        "[boot] REFUSED: host prerequisite: sealed origin missing"
    );
}

#[test]
fn recover_nbd_peer_wraps_into_systemd_scope_and_propagates_errors() {
    let cmd =
        systemd_scope_command_for(Path::new("/bin/ramshared"), &["recover", "--nbd-peer"], &[]);
    let args: Vec<_> = cmd
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        args,
        vec![
            "--scope",
            "-q",
            "--",
            "/bin/ramshared",
            "recover",
            "--nbd-peer"
        ]
    );
    let mut err = Vec::new();
    let wrapped = finish_recover_nbd_peer_with(
        true,
        |_| ExitCode::from(42),
        || -> Result<(), &'static str> { unreachable!("wrapped path should delegate") },
        &mut err,
    );
    assert_eq!(wrapped, ExitCode::from(42));

    let ok =
        finish_recover_nbd_peer_with(false, |_| unreachable!(), || Ok::<(), &str>(()), &mut err);
    assert_eq!(ok, ExitCode::SUCCESS);

    let failed =
        finish_recover_nbd_peer_with(false, |_| unreachable!(), || Err("peer missing"), &mut err);
    assert_eq!(failed, ExitCode::from(1));
    assert!(String::from_utf8_lossy(&err).contains("peer missing"));
}
