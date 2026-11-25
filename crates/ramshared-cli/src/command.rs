// SPDX-License-Identifier: GPL-2.0
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/command.rs
//
// Copyright (c) 2026 RamShared contributors.
//
// Top-level operator command parsing.
//
// This module owns the pure decision from argv to [`CliCommand`]. It performs
// no effects, opens nothing, and holds no state: it validates the command word
// and its option shape and returns either a typed command for dispatch or a
// typed refusal. `main.rs` composes the action runner and maps the result to an
// exit code; the flags this parser reads are the surface `usage::SURFACES`
// advertises, and `usage::tests` holds both directions of that contract.
//
// Nothing here is an effect adapter. If a verb needs deeper validation, it owns
// that inside its own parser (see `MonitorOptions::parse`, `stress::parse_stress_args`
// and `cascade`'s `up` parser); this module only splits off the command name
// and the options that shape it.

use std::fmt;

use crate::monitor::MonitorOptions;
use crate::resource_config::{BenchmarkAction, BenchmarkRequest, BenchmarkUseCase, ConfigMode};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum CliCommand {
    Version,
    BuildInfo,
    Run {
        args: Vec<String>,
    },
    Session {
        args: Vec<String>,
    },
    Supervise {
        args: Vec<String>,
    },
    Recover {
        resume: bool,
    },
    RecoverNbdPeer,
    Check {
        json: bool,
    },
    Doctor {
        json: bool,
    },
    Up {
        args: Vec<String>,
    },
    MigrateLegacyCascade,
    EnsureOrigin,
    EnsureGuardian,
    Boot,
    Down,
    /// The bounded recovery for a lifecycle whose peer died and whose origin
    /// tier the kernel already released.
    DownRecoverReleasedPeer,
    DownPlan {
        json: bool,
    },
    /// Attended fallback rebalance: a read-only plan or one exact
    /// `swapoff`/`swapon` pair for a single fallback block device.
    SwapRebalance {
        mode: SwapRebalanceMode,
        target: String,
    },
    ReleaseSync,
    Status {
        json: bool,
    },
    Monitor {
        options: MonitorOptions,
    },
    Config {
        mode: ConfigMode,
    },
    Diagnose {
        args: Vec<String>,
    },
    Setup {
        args: Vec<String>,
    },
    Service {
        args: Vec<String>,
    },
    Install {
        args: Vec<String>,
    },
    Package {
        args: Vec<String>,
    },
    Stress {
        args: Vec<String>,
    },
    Help,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum CliParseError {
    UnsupportedCommand(String),
    InvalidOption {
        command: &'static str,
        options: Vec<String>,
    },
}

/// The two attended modes of `swap-rebalance`. `--json` belongs to the
/// read-only plan; an apply prints its own outcome and has no JSON contract.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SwapRebalanceMode {
    Plan { json: bool },
    Apply,
}

/// A target is an absolute `/dev/...` path without traversal, or a canonical
/// `UUID=<8-4-4-4-12 hex>` selector. The deep identity checks live in
/// `cascade_io::rebalance`; this is the parser's shape gate (exit 2).
fn valid_swap_rebalance_target(value: &str) -> bool {
    if let Some(uuid) = value.strip_prefix("UUID=") {
        return uuid.len() == 36
            && uuid.bytes().enumerate().all(|(index, byte)| {
                if matches!(index, 8 | 13 | 18 | 23) {
                    byte == b'-'
                } else {
                    byte.is_ascii_hexdigit()
                }
            });
    }
    let Some(rest) = value.strip_prefix("/dev/") else {
        return false;
    };
    if rest.is_empty() || value.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return false;
    }
    !rest
        .split('/')
        .any(|component| component.is_empty() || component == "." || component == "..")
}

fn parse_swap_rebalance(options: &[String]) -> Result<CliCommand, CliParseError> {
    let refusal = || CliParseError::InvalidOption {
        command: "swap-rebalance",
        options: options.to_vec(),
    };
    let mut plan = false;
    let mut apply = false;
    let mut json = false;
    let mut target = None;
    let mut index = 0;
    while index < options.len() {
        match options[index].as_str() {
            "--plan" if !plan && !apply => plan = true,
            "--apply" if !apply && !plan => apply = true,
            "--json" if !json => json = true,
            "--target" if target.is_none() => {
                let Some(value) = options.get(index + 1) else {
                    return Err(refusal());
                };
                if !valid_swap_rebalance_target(value) {
                    return Err(refusal());
                }
                target = Some(value.clone());
                index += 1;
            }
            _ => return Err(refusal()),
        }
        index += 1;
    }
    if plan == apply {
        return Err(refusal());
    }
    let Some(target) = target else {
        return Err(refusal());
    };
    if json && !plan {
        return Err(refusal());
    }
    let mode = if plan {
        SwapRebalanceMode::Plan { json }
    } else {
        SwapRebalanceMode::Apply
    };
    Ok(CliCommand::SwapRebalance { mode, target })
}

impl fmt::Display for CliParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliParseError::UnsupportedCommand(command) => {
                write!(f, "unsupported command: {command}")
            }
            CliParseError::InvalidOption { command, options } => {
                write!(f, "invalid {command} option: {}", options.join(" "))
            }
        }
    }
}

fn parse_json_option(command: &'static str, options: &[String]) -> Result<bool, CliParseError> {
    match options {
        [] => Ok(false),
        [option] if option == "--json" => Ok(true),
        _ => Err(CliParseError::InvalidOption {
            command,
            options: options.to_vec(),
        }),
    }
}

fn parse_config_mode(options: &[String]) -> Result<ConfigMode, CliParseError> {
    match options {
        [] => Ok(ConfigMode::Interactive),
        [flag, path]
            if flag == "--profile" && !path.trim().is_empty() && !path.starts_with("--") =>
        {
            Ok(ConfigMode::Edit {
                profile_path: path.clone(),
            })
        }
        [command] if command == "show" => Ok(ConfigMode::Show { json: false }),
        [command, format] if command == "show" && format == "--json" => {
            Ok(ConfigMode::Show { json: true })
        }
        [command] if command == "wizard" => Ok(ConfigMode::Wizard { json: false }),
        [command, format] if command == "wizard" && format == "--json" => {
            Ok(ConfigMode::Wizard { json: true })
        }
        [command, ..] if command == "plan" => parse_config_plan(&options[1..]),
        [command, ..] if command == "draft" => parse_config_draft(&options[1..]),
        [command, ..] if command == "benchmark" => parse_config_benchmark(&options[1..]),
        _ => Err(CliParseError::InvalidOption {
            command: "config",
            options: options.to_vec(),
        }),
    }
}

fn parse_config_draft(options: &[String]) -> Result<ConfigMode, CliParseError> {
    match options {
        [flag, path] if flag == "--output" && !path.is_empty() && !path.starts_with("--") => {
            Ok(ConfigMode::Draft {
                output_path: path.clone(),
            })
        }
        _ => Err(CliParseError::InvalidOption {
            command: "config",
            options: options.to_vec(),
        }),
    }
}

fn parse_config_plan(options: &[String]) -> Result<ConfigMode, CliParseError> {
    let mut json = false;
    let mut profile_path = None;
    let mut index = 0;
    while index < options.len() {
        match options[index].as_str() {
            "--json" if !json => json = true,
            "--profile" if profile_path.is_none() && index + 1 < options.len() => {
                index += 1;
                let path = options[index].as_str();
                if path.is_empty() || path.starts_with("--") {
                    return Err(CliParseError::InvalidOption {
                        command: "config",
                        options: options.to_vec(),
                    });
                }
                profile_path = Some(path.to_string());
            }
            _ => {
                return Err(CliParseError::InvalidOption {
                    command: "config",
                    options: options.to_vec(),
                });
            }
        }
        index += 1;
    }
    Ok(ConfigMode::Plan { json, profile_path })
}

fn invalid_config_benchmark(options: &[String]) -> CliParseError {
    CliParseError::InvalidOption {
        command: "config",
        options: options.to_vec(),
    }
}

fn parse_config_benchmark(options: &[String]) -> Result<ConfigMode, CliParseError> {
    let mut action: Option<BenchmarkAction> = None;
    let mut json = false;
    let mut profile_path: Option<String> = None;
    let mut batch: Vec<String> = Vec::new();
    let mut batch_seen = false;
    let mut use_case: Option<BenchmarkUseCase> = None;
    let mut index = 0;

    let set_action = |current: &mut Option<BenchmarkAction>, next: BenchmarkAction| -> bool {
        if current.is_some() {
            false
        } else {
            *current = Some(next);
            true
        }
    };

    while index < options.len() {
        match options[index].as_str() {
            "--plan" if set_action(&mut action, BenchmarkAction::Plan) => {}
            "--confirm" if set_action(&mut action, BenchmarkAction::Confirmed) => {}
            "--recover" if set_action(&mut action, BenchmarkAction::Recover) => {}
            "--json" if !json => json = true,
            "--profile" if profile_path.is_none() && index + 1 < options.len() => {
                index += 1;
                let path = options[index].as_str();
                if path.trim().is_empty() || path.starts_with("--") {
                    return Err(invalid_config_benchmark(options));
                }
                profile_path = Some(path.to_string());
            }
            "--batch" if !batch_seen && index + 1 < options.len() => {
                index += 1;
                let raw = options[index].as_str();
                if raw.starts_with("--") {
                    return Err(invalid_config_benchmark(options));
                }
                let parts: Vec<String> = raw
                    .split(',')
                    .map(|segment| segment.trim().to_string())
                    .collect();
                if parts.is_empty() || parts.iter().any(String::is_empty) {
                    return Err(invalid_config_benchmark(options));
                }
                batch = parts;
                batch_seen = true;
            }
            "--use-case" if use_case.is_none() && index + 1 < options.len() => {
                index += 1;
                use_case = Some(match options[index].as_str() {
                    "swap" => BenchmarkUseCase::Swap,
                    "origin" => BenchmarkUseCase::Origin,
                    _ => return Err(invalid_config_benchmark(options)),
                });
            }
            _ => return Err(invalid_config_benchmark(options)),
        }
        index += 1;
    }

    let action = action.unwrap_or(BenchmarkAction::InteractiveOrConfirm);
    if profile_path.is_some() && batch_seen {
        return Err(invalid_config_benchmark(options));
    }
    if action == BenchmarkAction::Recover
        && (profile_path.is_some() || batch_seen || use_case.is_some())
    {
        return Err(invalid_config_benchmark(options));
    }

    Ok(ConfigMode::Benchmark(BenchmarkRequest {
        action,
        use_case: use_case.unwrap_or(BenchmarkUseCase::Swap),
        json,
        profile_path,
        batch,
    }))
}

pub(super) fn parse_cli_command(args: &[String]) -> Result<CliCommand, CliParseError> {
    let Some((command, options)) = args.split_first() else {
        return Ok(CliCommand::Help);
    };

    match command.as_str() {
        "--build-info" => {
            if options.is_empty() {
                Ok(CliCommand::BuildInfo)
            } else {
                Err(CliParseError::InvalidOption {
                    command: "--build-info",
                    options: options.to_vec(),
                })
            }
        }
        "version" | "-V" | "--version" => {
            if options.is_empty() {
                Ok(CliCommand::Version)
            } else {
                Err(CliParseError::InvalidOption {
                    command: "version",
                    options: options.to_vec(),
                })
            }
        }
        "check" => Ok(CliCommand::Check {
            json: parse_json_option("check", options)?,
        }),
        "run" => Ok(CliCommand::Run {
            args: options.to_vec(),
        }),
        "session" => Ok(CliCommand::Session {
            args: options.to_vec(),
        }),
        "supervise" => {
            if options.is_empty() || matches!(options, [option] if option == "--once") {
                Ok(CliCommand::Supervise {
                    args: options.to_vec(),
                })
            } else {
                Err(CliParseError::InvalidOption {
                    command: "supervise",
                    options: options.to_vec(),
                })
            }
        }
        "recover" => match options {
            [option] if option == "--status" => Ok(CliCommand::Recover { resume: false }),
            [option] if option == "--resume" => Ok(CliCommand::Recover { resume: true }),
            [option] if option == "--nbd-peer" => Ok(CliCommand::RecoverNbdPeer),
            _ => Err(CliParseError::InvalidOption {
                command: "recover",
                options: options.to_vec(),
            }),
        },
        "doctor" => Ok(CliCommand::Doctor {
            json: parse_json_option("doctor", options)?,
        }),
        "config" => Ok(CliCommand::Config {
            mode: parse_config_mode(options)?,
        }),
        "up" => Ok(CliCommand::Up {
            args: options.to_vec(),
        }),
        "migrate-cascade" => {
            if matches!(options, [option] if option == "--from-legacy") {
                Ok(CliCommand::MigrateLegacyCascade)
            } else {
                Err(CliParseError::InvalidOption {
                    command: "migrate-cascade",
                    options: options.to_vec(),
                })
            }
        }
        "ensure-origin" => {
            // Idempotent host preparation: no options, no sizing, no approval
            // token. The host-gate unit runs this before its identity check so
            // a cold WSL2 start has the sealed origin device present.
            if options.is_empty() {
                Ok(CliCommand::EnsureOrigin)
            } else {
                Err(CliParseError::InvalidOption {
                    command: "ensure-origin",
                    options: options.to_vec(),
                })
            }
        }
        "ensure-guardian" => {
            // DT-19, the exact twin of `ensure-origin`: no options, no sizing,
            // no approval token. The host-gate unit runs this before its proof
            // check so a guest-only restart has the sealed producer running.
            if options.is_empty() {
                Ok(CliCommand::EnsureGuardian)
            } else {
                Err(CliParseError::InvalidOption {
                    command: "ensure-guardian",
                    options: options.to_vec(),
                })
            }
        }
        "boot" => {
            // DT-1 / DT-7: `boot` is the single native bootstrap verb and takes
            // no options. Unknown flags (including deploy-shaped ones) are
            // refused by the parser before any gate runs; `parse_boot_invocation`
            // is the in-depth backstop inside `boot` itself.
            if options.is_empty() {
                Ok(CliCommand::Boot)
            } else {
                Err(CliParseError::InvalidOption {
                    command: "boot",
                    options: options.to_vec(),
                })
            }
        }
        "down" => match options {
            [] => Ok(CliCommand::Down),
            // The recovery is its own verb-like flag, never a silent
            // behaviour of `down`: it acts on a lifecycle the ordered path
            // refused, and an operator must ask for it by name.
            [one] if one == "--recover-released-peer" => Ok(CliCommand::DownRecoverReleasedPeer),
            [one] if one == "--plan" => Ok(CliCommand::DownPlan { json: false }),
            [a, b] if (a == "--plan" && b == "--json") || (a == "--json" && b == "--plan") => {
                Ok(CliCommand::DownPlan { json: true })
            }
            _ => Err(CliParseError::InvalidOption {
                command: "down",
                options: options.to_vec(),
            }),
        },
        // RF-1/RF-6: one exact fallback target, attended. The parser refuses
        // `--force`, `--all`, `--yes` and `-a` by not implementing them.
        "swap-rebalance" => parse_swap_rebalance(options),
        // DT-3: `release-sync` is the joint-move verb and takes no options. It
        // moves already-active product units onto the selected release; it is
        // not a start/stop surface and must not grow flags that would make it
        // one.
        "release-sync" => {
            if options.is_empty() {
                Ok(CliCommand::ReleaseSync)
            } else {
                Err(CliParseError::InvalidOption {
                    command: "release-sync",
                    options: options.to_vec(),
                })
            }
        }
        "status" => Ok(CliCommand::Status {
            json: parse_json_option("status", options)?,
        }),
        cmd @ ("monitor" | "top") => Ok(CliCommand::Monitor {
            options: MonitorOptions::parse(options).map_err(|_| CliParseError::InvalidOption {
                command: match cmd {
                    "top" => "top",
                    _ => "monitor",
                },
                options: options.to_vec(),
            })?,
        }),
        "diagnose" => Ok(CliCommand::Diagnose {
            args: options.to_vec(),
        }),
        // `setup` is a new command surface. It owns its own strict parser and
        // reports every refusal through the customer result/exit model, so the
        // top-level parser only splits off the command name here.
        "setup" => Ok(CliCommand::Setup {
            args: options.to_vec(),
        }),
        // `service` is a new command surface. It owns its own strict parser and
        // reports every refusal through the customer result/exit model, so the
        // top-level parser only splits off the command name here.
        "service" => Ok(CliCommand::Service {
            args: options.to_vec(),
        }),
        "install" => Ok(CliCommand::Install {
            args: options.to_vec(),
        }),
        "package" => Ok(CliCommand::Package {
            args: options.to_vec(),
        }),
        "stress" | "bench" => Ok(CliCommand::Stress {
            args: options.to_vec(),
        }),
        "-h" | "--help" => Ok(CliCommand::Help),
        other => Err(CliParseError::UnsupportedCommand(other.to_string())),
    }
}

#[cfg(test)]
mod tests;
