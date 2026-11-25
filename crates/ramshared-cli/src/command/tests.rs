// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/command/tests.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]
use super::*;

fn cli_args(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_string()).collect()
}

#[test]
fn config_command_accepts_interactive_show_and_read_only_plan_modes() {
    assert_eq!(
        parse_cli_command(&cli_args(&["config"])).expect("interactive config parses"),
        CliCommand::Config {
            mode: ConfigMode::Interactive,
        }
    );
    assert_eq!(
        parse_cli_command(&cli_args(&["config", "show"])).expect("show parses"),
        CliCommand::Config {
            mode: ConfigMode::Show { json: false },
        }
    );
    assert_eq!(
        parse_cli_command(&cli_args(&["config", "show", "--json"])).expect("json show parses"),
        CliCommand::Config {
            mode: ConfigMode::Show { json: true },
        }
    );
    assert_eq!(
        parse_cli_command(&cli_args(&["config", "plan"])).expect("plan parses"),
        CliCommand::Config {
            mode: ConfigMode::Plan {
                json: false,
                profile_path: None,
            },
        }
    );
    assert_eq!(
        parse_cli_command(&cli_args(&[
            "config",
            "plan",
            "--json",
            "--profile",
            "/tmp/draft.toml",
        ]))
        .expect("profile-backed json plan parses"),
        CliCommand::Config {
            mode: ConfigMode::Plan {
                json: true,
                profile_path: Some("/tmp/draft.toml".into()),
            },
        }
    );
    assert!(parse_cli_command(&cli_args(&["config", "apply"])).is_err());
    assert!(parse_cli_command(&cli_args(&["config", "show", "--write"])).is_err());
    assert!(parse_cli_command(&cli_args(&["config", "plan", "--profile"])).is_err());
    assert!(parse_cli_command(&cli_args(&["config", "plan", "--profile", "--json"])).is_err());
    assert_eq!(
        parse_cli_command(&cli_args(&["config", "plan", "--profile", " draft.toml "]))
            .expect("profile path whitespace is preserved"),
        CliCommand::Config {
            mode: ConfigMode::Plan {
                json: false,
                profile_path: Some(" draft.toml ".into()),
            },
        }
    );
    assert!(
        parse_cli_command(&cli_args(&[
            "config",
            "plan",
            "--profile",
            "a",
            "--profile",
            "b"
        ]))
        .is_err()
    );
}

#[test]
fn config_benchmark_options_validate_combinations() {
    assert_eq!(
        parse_cli_command(&cli_args(&["config", "benchmark"]))
            .expect("bare benchmark defaults to interactive-or-confirm"),
        CliCommand::Config {
            mode: ConfigMode::Benchmark(BenchmarkRequest {
                action: BenchmarkAction::InteractiveOrConfirm,
                use_case: BenchmarkUseCase::Swap,
                json: false,
                profile_path: None,
                batch: Vec::new(),
            }),
        }
    );
    assert_eq!(
        parse_cli_command(&cli_args(&[
            "config",
            "benchmark",
            "--plan",
            "--json",
            "--batch",
            "vol-a, vol-b",
            "--use-case",
            "origin",
        ]))
        .expect("plan + batch + use-case parses"),
        CliCommand::Config {
            mode: ConfigMode::Benchmark(BenchmarkRequest {
                action: BenchmarkAction::Plan,
                use_case: BenchmarkUseCase::Origin,
                json: true,
                profile_path: None,
                batch: vec!["vol-a".into(), "vol-b".into()],
            }),
        }
    );
    assert_eq!(
        parse_cli_command(&cli_args(&["config", "benchmark", "--recover"]))
            .expect("recover parses"),
        CliCommand::Config {
            mode: ConfigMode::Benchmark(BenchmarkRequest {
                action: BenchmarkAction::Recover,
                use_case: BenchmarkUseCase::Swap,
                json: false,
                profile_path: None,
                batch: Vec::new(),
            }),
        }
    );

    for invalid in [
        vec!["config", "benchmark", "--plan", "--confirm"],
        vec!["config", "benchmark", "--plan", "--recover"],
        vec![
            "config",
            "benchmark",
            "--profile",
            "/tmp/a.toml",
            "--batch",
            "v1",
        ],
        vec!["config", "benchmark", "--recover", "--batch", "v1"],
        vec![
            "config",
            "benchmark",
            "--recover",
            "--profile",
            "/tmp/a.toml",
        ],
        vec!["config", "benchmark", "--recover", "--use-case", "swap"],
        vec!["config", "benchmark", "--batch", "v1,,v2"],
        vec!["config", "benchmark", "--batch", "--json"],
        vec!["config", "benchmark", "--use-case", "gpu"],
        vec!["config", "benchmark", "--profile", ""],
        vec!["config", "benchmark", "--bogus"],
    ] {
        assert!(
            parse_cli_command(&cli_args(&invalid)).is_err(),
            "{invalid:?} must be rejected"
        );
    }
}

#[test]
fn config_profile_option_reopens_an_explicit_desired_profile() {
    assert_eq!(
        parse_cli_command(&cli_args(&["config", "--profile", "draft.toml"]))
            .expect("profile editor"),
        CliCommand::Config {
            mode: ConfigMode::Edit {
                profile_path: "draft.toml".into()
            }
        }
    );
    for args in [
        vec!["config", "--profile"],
        vec!["config", "--profile", ""],
        vec!["config", "--profile", "--json"],
        vec!["config", "--profile", "a", "--profile", "b"],
    ] {
        assert!(parse_cli_command(&cli_args(&args)).is_err());
    }
}

#[test]
fn config_command_accepts_draft_mode_and_requires_output_path() {
    assert_eq!(
        parse_cli_command(&cli_args(&[
            "config",
            "draft",
            "--output",
            "/tmp/ramshared-draft.toml",
        ]))
        .expect("draft mode parses"),
        CliCommand::Config {
            mode: ConfigMode::Draft {
                output_path: "/tmp/ramshared-draft.toml".into(),
            },
        }
    );
    assert!(parse_cli_command(&cli_args(&["config", "draft"])).is_err());
    assert!(parse_cli_command(&cli_args(&["config", "draft", "--output"])).is_err());
    assert!(parse_cli_command(&cli_args(&["config", "draft", "--output", "--json"])).is_err());
    assert!(
        parse_cli_command(&cli_args(&[
            "config",
            "draft",
            "--output",
            "/tmp/a.toml",
            "--output",
            "/tmp/b.toml",
        ]))
        .is_err()
    );
}

#[test]
fn down_plan_parses_as_a_read_only_command() {
    assert_eq!(
        parse_cli_command(&cli_args(&["down"])).unwrap(),
        CliCommand::Down
    );
    assert_eq!(
        parse_cli_command(&cli_args(&["down", "--plan"])).unwrap(),
        CliCommand::DownPlan { json: false }
    );
    assert_eq!(
        parse_cli_command(&cli_args(&["down", "--plan", "--json"])).unwrap(),
        CliCommand::DownPlan { json: true }
    );
    assert_eq!(
        parse_cli_command(&cli_args(&["down", "--json", "--plan"])).unwrap(),
        CliCommand::DownPlan { json: true }
    );
    assert!(parse_cli_command(&cli_args(&["down", "--json"])).is_err());
    assert!(parse_cli_command(&cli_args(&["down", "--force"])).is_err());
}

#[test]
fn public_control_commands_parse_exactly() {
    assert_eq!(
        parse_cli_command(&cli_args(&["migrate-cascade", "--from-legacy"])).unwrap(),
        CliCommand::MigrateLegacyCascade
    );
    assert!(parse_cli_command(&cli_args(&["migrate-cascade"])).is_err());
    assert_eq!(
        parse_cli_command(&cli_args(&["ensure-origin"])).unwrap(),
        CliCommand::EnsureOrigin
    );
    assert!(parse_cli_command(&cli_args(&["ensure-origin", "--force"])).is_err());
    assert!(parse_cli_command(&cli_args(&["ensure-origin", "--json"])).is_err());
    assert_eq!(
        parse_cli_command(&cli_args(&["ensure-guardian"])).unwrap(),
        CliCommand::EnsureGuardian
    );
    assert!(parse_cli_command(&cli_args(&["ensure-guardian", "--force"])).is_err());
    assert!(parse_cli_command(&cli_args(&["ensure-guardian", "--json"])).is_err());
    assert_eq!(
        parse_cli_command(&cli_args(&[
            "run",
            "--class",
            "build",
            "--memory-max",
            "4096",
            "--",
            "make"
        ]))
        .unwrap(),
        CliCommand::Run {
            args: cli_args(&["--class", "build", "--memory-max", "4096", "--", "make"])
        }
    );
    assert_eq!(
        parse_cli_command(&cli_args(&["session", "--class", "interactive"])).unwrap(),
        CliCommand::Session {
            args: cli_args(&["--class", "interactive"])
        }
    );
    assert_eq!(
        parse_cli_command(&cli_args(&["supervise", "--once"])).unwrap(),
        CliCommand::Supervise {
            args: cli_args(&["--once"])
        }
    );
    assert_eq!(
        parse_cli_command(&cli_args(&["recover", "--status"])).unwrap(),
        CliCommand::Recover { resume: false }
    );
    assert_eq!(
        parse_cli_command(&cli_args(&["recover", "--resume"])).unwrap(),
        CliCommand::Recover { resume: true }
    );
    assert_eq!(
        parse_cli_command(&cli_args(&["recover", "--nbd-peer"])).unwrap(),
        CliCommand::RecoverNbdPeer
    );
    assert!(parse_cli_command(&cli_args(&["recover", "--force"])).is_err());
}

#[test]
fn swap_rebalance_plan_and_apply_parse_exactly() {
    assert_eq!(
        parse_cli_command(&cli_args(&[
            "swap-rebalance",
            "--plan",
            "--target",
            "/dev/sdf"
        ]))
        .unwrap(),
        CliCommand::SwapRebalance {
            mode: SwapRebalanceMode::Plan { json: false },
            target: "/dev/sdf".into(),
        }
    );
    assert_eq!(
        parse_cli_command(&cli_args(&[
            "swap-rebalance",
            "--json",
            "--target",
            "UUID=ABCdef01-2345-4678-8ABC-def012345678",
            "--plan"
        ]))
        .unwrap(),
        CliCommand::SwapRebalance {
            mode: SwapRebalanceMode::Plan { json: true },
            target: "UUID=ABCdef01-2345-4678-8ABC-def012345678".into(),
        }
    );
    assert_eq!(
        parse_cli_command(&cli_args(&[
            "swap-rebalance",
            "--apply",
            "--target",
            "/dev/sdf"
        ]))
        .unwrap(),
        CliCommand::SwapRebalance {
            mode: SwapRebalanceMode::Apply,
            target: "/dev/sdf".into(),
        }
    );
}

#[test]
fn swap_rebalance_refuses_force_all_and_apply_json() {
    for refused in ["--force", "--all", "--yes", "-a"] {
        assert!(
            parse_cli_command(&cli_args(&[
                "swap-rebalance",
                "--plan",
                "--target",
                "/dev/sdf",
                refused
            ]))
            .is_err(),
            "{refused} must be refused"
        );
    }
    assert!(
        parse_cli_command(&cli_args(&[
            "swap-rebalance",
            "--apply",
            "--target",
            "/dev/sdf",
            "--json"
        ]))
        .is_err(),
        "apply has no JSON contract"
    );
}

#[test]
fn swap_rebalance_requires_one_mode_and_exact_target() {
    for refused in [
        vec!["swap-rebalance", "--target", "/dev/sdf"],
        vec![
            "swap-rebalance",
            "--plan",
            "--apply",
            "--target",
            "/dev/sdf",
        ],
        vec!["swap-rebalance", "--plan"],
        vec!["swap-rebalance", "--plan", "--target"],
        vec![
            "swap-rebalance",
            "--plan",
            "--target",
            "/dev/sdf",
            "--target",
            "/dev/sdg",
        ],
        vec!["swap-rebalance", "--plan", "--target", "/dev/sdf", "--plan"],
        vec![
            "swap-rebalance",
            "--plan",
            "--target",
            "/dev/sdf",
            "--verbose",
        ],
    ] {
        assert!(
            parse_cli_command(&cli_args(&refused)).is_err(),
            "{refused:?} must be refused"
        );
    }
}

#[test]
fn swap_rebalance_refuses_malformed_target_shape() {
    for refused in [
        "sdf",
        "/dev/",
        "/dev/../sdf",
        "/dev/disk/../sdf",
        "/dev/sd f",
        "uuid=abcdef01-2345-4678-8abc-def012345678",
        "UUID=abcdef01-2345-4678-8abc",
        "UUID=zzzzzzzz-2345-4678-8abc-def012345678",
        "UUID=abcdef01-2345-4678-8abc-def012345678-extra",
    ] {
        assert!(
            parse_cli_command(&cli_args(&[
                "swap-rebalance",
                "--plan",
                "--target",
                refused
            ]))
            .is_err(),
            "{refused} must be refused"
        );
    }
}

#[test]
fn parses_config_wizard_and_refuses_unknown_flags() {
    assert_eq!(
        parse_cli_command(&cli_args(&["config", "wizard"])).unwrap(),
        CliCommand::Config {
            mode: ConfigMode::Wizard { json: false },
        }
    );
    assert_eq!(
        parse_cli_command(&cli_args(&["config", "wizard", "--json"])).unwrap(),
        CliCommand::Config {
            mode: ConfigMode::Wizard { json: true },
        }
    );
    for bad in [
        vec!["config", "wizard", "--unknown"],
        vec!["config", "wizard", "--json", "--json"],
        vec!["config", "wizard", "extra"],
    ] {
        assert!(
            parse_cli_command(&cli_args(&bad)).is_err(),
            "{bad:?} must be refused"
        );
    }
}
