// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/main_tests.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

pub(super) use std::io::Write;

pub(super) use std::path::{Path, PathBuf};

pub(super) use std::process::{Command, ExitCode};

pub(super) use super::cli_dispatch::*;

pub(super) use crate::cascade;

pub(super) use crate::command::{CliCommand, CliParseError, SwapRebalanceMode, parse_cli_command};

pub(super) use crate::installed_image;

pub(super) use crate::monitor::{Language, MonitorOptions};

pub(super) use crate::readiness::Decision;

pub(super) use crate::release_coherence::{self, MoveOutcome};

pub(super) use crate::resource_config::ConfigMode;

#[derive(Default)]
pub(super) struct RecordingCliActions {
    pub(super) calls: Vec<CliCommand>,
    pub(super) exit: u8,
}

impl RecordingCliActions {
    pub(super) fn with_exit(exit: u8) -> Self {
        Self {
            calls: Vec::new(),
            exit,
        }
    }

    fn result(&self) -> ExitCode {
        ExitCode::from(self.exit)
    }
}

impl CliActionRunner for RecordingCliActions {
    fn check(
        &mut self,
        json: bool,
        _stdout: &mut dyn std::io::Write,
        _stderr: &mut dyn std::io::Write,
    ) -> ExitCode {
        self.calls.push(CliCommand::Check { json });
        self.result()
    }

    fn doctor(
        &mut self,
        json: bool,
        _stdout: &mut dyn std::io::Write,
        _stderr: &mut dyn std::io::Write,
    ) -> ExitCode {
        self.calls.push(CliCommand::Doctor { json });
        self.result()
    }

    fn up(
        &mut self,
        args: &[String],
        _stdout: &mut dyn std::io::Write,
        _stderr: &mut dyn std::io::Write,
    ) -> ExitCode {
        self.calls.push(CliCommand::Up {
            args: args.to_vec(),
        });
        self.result()
    }

    fn migrate_legacy_cascade(
        &mut self,
        _stdout: &mut dyn std::io::Write,
        _stderr: &mut dyn std::io::Write,
    ) -> ExitCode {
        self.calls.push(CliCommand::MigrateLegacyCascade);
        self.result()
    }

    fn ensure_origin(
        &mut self,
        _stdout: &mut dyn std::io::Write,
        _stderr: &mut dyn std::io::Write,
    ) -> ExitCode {
        self.calls.push(CliCommand::EnsureOrigin);
        self.result()
    }

    fn ensure_guardian(
        &mut self,
        _stdout: &mut dyn std::io::Write,
        _stderr: &mut dyn std::io::Write,
    ) -> ExitCode {
        self.calls.push(CliCommand::EnsureGuardian);
        self.result()
    }

    fn boot(
        &mut self,
        _stdout: &mut dyn std::io::Write,
        _stderr: &mut dyn std::io::Write,
    ) -> ExitCode {
        self.calls.push(CliCommand::Boot);
        self.result()
    }

    fn down(
        &mut self,
        _stdout: &mut dyn std::io::Write,
        _stderr: &mut dyn std::io::Write,
    ) -> ExitCode {
        self.calls.push(CliCommand::Down);
        self.result()
    }

    fn down_recover_released_peer(
        &mut self,
        _stdout: &mut dyn std::io::Write,
        _stderr: &mut dyn std::io::Write,
    ) -> ExitCode {
        self.calls.push(CliCommand::DownRecoverReleasedPeer);
        self.result()
    }

    fn down_plan(
        &mut self,
        json: bool,
        _stdout: &mut dyn std::io::Write,
        _stderr: &mut dyn std::io::Write,
    ) -> ExitCode {
        self.calls.push(CliCommand::DownPlan { json });
        self.result()
    }

    fn swap_rebalance(
        &mut self,
        mode: &SwapRebalanceMode,
        target: &str,
        _stdout: &mut dyn std::io::Write,
        _stderr: &mut dyn std::io::Write,
    ) -> ExitCode {
        self.calls.push(CliCommand::SwapRebalance {
            mode: mode.clone(),
            target: target.to_string(),
        });
        self.result()
    }

    fn release_sync(
        &mut self,
        _stdout: &mut dyn std::io::Write,
        _stderr: &mut dyn std::io::Write,
    ) -> ExitCode {
        self.calls.push(CliCommand::ReleaseSync);
        self.result()
    }

    fn status(
        &mut self,
        json: bool,
        _stdout: &mut dyn std::io::Write,
        _stderr: &mut dyn std::io::Write,
    ) -> ExitCode {
        self.calls.push(CliCommand::Status { json });
        self.result()
    }

    fn config(
        &mut self,
        mode: ConfigMode,
        _stdout: &mut dyn std::io::Write,
        _stderr: &mut dyn std::io::Write,
    ) -> ExitCode {
        self.calls.push(CliCommand::Config { mode });
        self.result()
    }

    fn monitor(
        &mut self,
        options: &MonitorOptions,
        _stdout: &mut dyn std::io::Write,
        _stderr: &mut dyn std::io::Write,
    ) -> ExitCode {
        self.calls.push(CliCommand::Monitor {
            options: options.clone(),
        });
        self.result()
    }

    fn diagnose(
        &mut self,
        args: &[String],
        _stdout: &mut dyn std::io::Write,
        _stderr: &mut dyn std::io::Write,
    ) -> ExitCode {
        self.calls.push(CliCommand::Diagnose {
            args: args.to_vec(),
        });
        self.result()
    }

    fn setup(
        &mut self,
        args: &[String],
        _stdout: &mut dyn std::io::Write,
        _stderr: &mut dyn std::io::Write,
    ) -> ExitCode {
        self.calls.push(CliCommand::Setup {
            args: args.to_vec(),
        });
        self.result()
    }

    fn service(
        &mut self,
        args: &[String],
        _stdout: &mut dyn std::io::Write,
        _stderr: &mut dyn std::io::Write,
    ) -> ExitCode {
        self.calls.push(CliCommand::Service {
            args: args.to_vec(),
        });
        self.result()
    }

    fn install(
        &mut self,
        args: &[String],
        _stdout: &mut dyn std::io::Write,
        _stderr: &mut dyn std::io::Write,
    ) -> ExitCode {
        self.calls.push(CliCommand::Install {
            args: args.to_vec(),
        });
        self.result()
    }

    fn package(
        &mut self,
        args: &[String],
        _stdout: &mut dyn std::io::Write,
        _stderr: &mut dyn std::io::Write,
    ) -> ExitCode {
        self.calls.push(CliCommand::Package {
            args: args.to_vec(),
        });
        self.result()
    }

    fn stress(
        &mut self,
        args: &[String],
        _stdout: &mut dyn std::io::Write,
        _stderr: &mut dyn std::io::Write,
    ) -> ExitCode {
        self.calls.push(CliCommand::Stress {
            args: args.to_vec(),
        });
        self.result()
    }

    fn run_workload(
        &mut self,
        args: &[String],
        _stdout: &mut dyn std::io::Write,
        _stderr: &mut dyn std::io::Write,
    ) -> ExitCode {
        self.calls.push(CliCommand::Run {
            args: args.to_vec(),
        });
        self.result()
    }

    fn session(
        &mut self,
        args: &[String],
        _stdout: &mut dyn std::io::Write,
        _stderr: &mut dyn std::io::Write,
    ) -> ExitCode {
        self.calls.push(CliCommand::Session {
            args: args.to_vec(),
        });
        self.result()
    }

    fn supervise(
        &mut self,
        args: &[String],
        _stdout: &mut dyn std::io::Write,
        _stderr: &mut dyn std::io::Write,
    ) -> ExitCode {
        self.calls.push(CliCommand::Supervise {
            args: args.to_vec(),
        });
        self.result()
    }

    fn recover(
        &mut self,
        resume: bool,
        _stdout: &mut dyn std::io::Write,
        _stderr: &mut dyn std::io::Write,
    ) -> ExitCode {
        self.calls.push(CliCommand::Recover { resume });
        self.result()
    }

    fn recover_nbd_peer(
        &mut self,
        _stdout: &mut dyn std::io::Write,
        _stderr: &mut dyn std::io::Write,
    ) -> ExitCode {
        self.calls.push(CliCommand::RecoverNbdPeer);
        self.result()
    }
}

pub(super) fn cli_args(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_string()).collect()
}

/// A `Write` that always fails, so the write-error arms of the pure
/// rendering helpers are reachable without a real broken pipe.
pub(super) struct FailingWriter;

impl Write for FailingWriter {
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("simulated write failure"))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[path = "main_tests/dispatch_and_commands.rs"]
mod dispatch_and_commands;
#[path = "main_tests/exit_and_system_actions.rs"]
mod exit_and_system_actions;
