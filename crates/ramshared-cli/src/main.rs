// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/main.rs
//! ramshared CLI — preflight (check/doctor) and cascade orchestration (up/down).
//! No `unsafe`: the CUDA probe uses the audited `ramshared-cuda` crate (Day-0).
#![deny(unsafe_code)]

use std::env;
use std::process::ExitCode;

// Shared with the agent: one bounded-process custody contract (AUD-11).
// Child modules reach it as `crate::bounded_process`.
use ramshared_exec::bounded_process;

mod artifact_file;
mod budget_observer;
mod cascade;
mod cli_dispatch;
mod command;
mod customer;
mod diagnose;
mod installed_image;
mod monitor;
mod operation_progress;
mod published_budget;
mod readiness;
mod release_coherence;
mod release_identity;
mod resource_config;
mod runtime;
mod service;
mod setup;
mod stress;
mod supervisor;
mod usage;
mod workload;
mod zram_compression;

use cli_dispatch::{SystemCliActions, run_from_args};

fn main() -> ExitCode {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args.as_slice() == ["__config-benchmark-worker"] {
        return resource_config::benchmark::run_worker_from_stdin();
    }
    let mut actions = SystemCliActions;
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut stdout = stdout.lock();
    let mut stderr = stderr.lock();
    run_from_args(&args, &mut actions, &mut stdout, &mut stderr)
}

#[cfg(test)]
mod dispatch_contract_tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::process::ExitCode;

    use super::tests::{RecordingCliActions, cli_args};
    use crate::cli_dispatch::run_from_args;
    use crate::command::CliCommand;

    #[test]
    fn dispatch_refuses_invalid_flags_before_action() {
        for args in [
            cli_args(&["check", "--verbose"]),
            cli_args(&["doctor", "--json", "--json"]),
            cli_args(&["status", "--unsafe"]),
            cli_args(&["status", "--json", "--json"]),
            cli_args(&["down", "--force"]),
            cli_args(&["migrate-cascade"]),
            cli_args(&["migrate-cascade", "--from-legacy", "--force"]),
        ] {
            let mut actions = RecordingCliActions::default();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let exit = run_from_args(&args, &mut actions, &mut stdout, &mut stderr);

            assert_eq!(exit, ExitCode::from(2));
            assert!(
                actions.calls.is_empty(),
                "{args:?} must not reach an action"
            );
            assert!(stdout.is_empty());
            assert!(
                String::from_utf8_lossy(&stderr).contains("invalid"),
                "{args:?} must explain the refusal"
            );
        }
    }

    #[test]
    fn dispatch_forwards_exact_status_and_diagnose_args() {
        let mut actions = RecordingCliActions::with_exit(1);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        assert_eq!(
            run_from_args(
                &cli_args(&["status", "--json"]),
                &mut actions,
                &mut stdout,
                &mut stderr,
            ),
            ExitCode::from(1)
        );
        assert_eq!(
            run_from_args(
                &cli_args(&["diagnose", "--events", "sample.jsonl", "--json"]),
                &mut actions,
                &mut stdout,
                &mut stderr,
            ),
            ExitCode::from(1)
        );
        assert_eq!(
            run_from_args(
                &cli_args(&["up", "--zram", "invalid"]),
                &mut actions,
                &mut stdout,
                &mut stderr,
            ),
            ExitCode::from(1)
        );

        assert_eq!(
            actions.calls,
            vec![
                CliCommand::Status { json: true },
                CliCommand::Diagnose {
                    args: cli_args(&["--events", "sample.jsonl", "--json"]),
                },
                CliCommand::Up {
                    args: cli_args(&["--zram", "invalid"]),
                },
            ]
        );
    }
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
