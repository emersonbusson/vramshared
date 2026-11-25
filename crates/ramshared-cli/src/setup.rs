// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/setup.rs
//! `ramshared setup` — the customer setup command surface (ITEM-5).
//!
//! SPEC: `ramshared setup plan --profile <name> --output <file> [--scope storage-only|gpu-cache] [--json]` is
//! read-only. It observes, plans, writes the plan document and stops.
//!
//! `setup apply` delegates native storage-only preparation to its supervised
//! owner. Other platforms and cache allocation retain their provider gates.
//! Planning and preparation never activate swap, cache or boot policy.
//!
//! Exit model: this is a **new** command surface, so every refusal — including
//! an unknown flag or a missing required option — reports through
//! [`crate::customer::CustomerError`] and the PRD mapping. An unknown flag is
//! `unsupported_surface` (exit 2), never the shared 22/EINVAL the existing
//! commands keep.

use std::io::Write;
use std::path::PathBuf;

use crate::cascade::boot::{BootPaths, observe_lower_tier_capacity, release_version};
use crate::customer::diagnostics::{DiagnosticLimits, ReadinessDiagnosticSource};
use crate::customer::plan_document::MAX_PROFILE_NAME_BYTES;
use crate::customer::setup_scope::SetupScope;
use crate::customer::{
    CustomerCommand, CustomerDependencies, CustomerError, CustomerReason, CustomerState,
    execute_customer,
};
use crate::readiness::run_check;
use crate::resource_config::default_configuration_root;

/// `ramshared setup` is a new command surface: a malformed request is a
/// refusal with a stable reason, not an errno.
fn refused(detail: impl Into<String>) -> CustomerError {
    CustomerError {
        reason: CustomerReason::UnsupportedSurface,
        detail: detail.into(),
        state: CustomerState::InstalledOff,
    }
}

/// Strict parser for `setup plan --profile <name> --output <file> [--json]`.
///
/// Option order is free, each named option may appear at most once, and every
/// unknown token is a refusal. The profile argument is a **name**: the parser
/// rejects a value that looks like a path before any resolution happens, so a
/// caller can never hand a device path or an executable in on argv.
fn parse_plan_args(args: &[String]) -> Result<(String, PathBuf, SetupScope, bool), CustomerError> {
    let mut profile_name: Option<String> = None;
    let mut output: Option<PathBuf> = None;
    let mut json = false;
    let mut scope = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--json" if !json => json = true,
            "--scope" if scope.is_none() && index + 1 < args.len() => {
                index += 1;
                scope = Some(SetupScope::parse(&args[index])?);
            }
            "--profile" if profile_name.is_none() && index + 1 < args.len() => {
                index += 1;
                let name = args[index].as_str();
                if name.is_empty() || name.starts_with("--") {
                    return Err(refused(
                        "setup plan --profile needs a profile name, not an option or an empty value",
                    ));
                }
                if name.len() > MAX_PROFILE_NAME_BYTES {
                    return Err(refused(format!(
                        "profile name is {} bytes; the bound is {MAX_PROFILE_NAME_BYTES}",
                        name.len()
                    )));
                }
                profile_name = Some(name.to_string());
            }
            "--output" if output.is_none() && index + 1 < args.len() => {
                index += 1;
                let path = args[index].as_str();
                if path.is_empty() || path.starts_with("--") {
                    return Err(refused(
                        "setup plan --output needs a file path, not an option or an empty value",
                    ));
                }
                output = Some(PathBuf::from(path));
            }
            other => {
                return Err(refused(format!("unsupported setup plan argument: {other}")));
            }
        }
        index += 1;
    }
    let Some(profile_name) = profile_name else {
        return Err(refused("setup plan requires --profile <name>"));
    };
    let Some(output) = output else {
        return Err(refused("setup plan requires --output <file>"));
    };
    Ok((profile_name, output, scope.unwrap_or_default(), json))
}

fn parse_wizard_args(args: &[String]) -> Result<bool, CustomerError> {
    match args {
        [] => Ok(false),
        [flag] if flag == "--json" => Ok(true),
        _ => Err(refused(
            "usage: ramshared setup wizard [--json] (unsupported option)",
        )),
    }
}

fn run_wizard_subcommand(args: &[String], stdout: &mut dyn Write, stderr: &mut dyn Write) -> u8 {
    let json = match parse_wizard_args(args) {
        Ok(json) => json,
        Err(error) => {
            let _ = writeln!(stderr, "{error}");
            return error.exit_code();
        }
    };
    match crate::resource_config::assistant_ui::run_wizard(json, stdout) {
        Ok(()) => 0,
        Err(error) => {
            let _ = writeln!(stderr, "setup wizard failed: {error}");
            1
        }
    }
}

/// `ramshared setup` dispatch. Returns the PRD-mapped exit code.
pub(crate) fn run(args: &[String], stdout: &mut dyn Write, stderr: &mut dyn Write) -> u8 {
    if let Some((subcommand, rest)) = args.split_first() {
        match subcommand.as_str() {
            "apply" => return crate::customer::setup_apply::run(rest, stdout, stderr),
            "reconcile" => {
                return crate::customer::setup_apply::run_reconcile(rest, stdout, stderr);
            }
            "recommend" => {
                return crate::resource_config::benchmark::windows_host::run_setup_recommend(
                    rest, stdout, stderr,
                );
            }
            "wizard" => return run_wizard_subcommand(rest, stdout, stderr),
            _ => {}
        }
    }
    // Planning remains read-only; native apply dispatches above.
    let plan_args = match args.split_first() {
        Some((subcommand, rest)) if *subcommand == "plan" => rest,
        Some((subcommand, _)) => {
            let error = refused(format!(
                "unsupported setup subcommand: {subcommand} (available: `plan`, `recommend`, native storage `apply` and `reconcile`)"
            ));
            let _ = writeln!(stderr, "{error}");
            return error.exit_code();
        }
        None => {
            let error = refused(
                "usage: ramshared setup plan --profile <name> --output <file> [--scope storage-only|gpu-cache] [--json]",
            );
            let _ = writeln!(stderr, "{error}");
            return error.exit_code();
        }
    };

    let (profile_name, output, scope, json) = match parse_plan_args(plan_args) {
        Ok(parsed) => parsed,
        Err(error) => {
            let _ = writeln!(stderr, "{error}");
            return error.exit_code();
        }
    };

    // Shared readiness and release observations remain read-only. Only cache
    // scope needs a lower-sink observation; storage scope carries none.
    let report = run_check();
    let product_root = BootPaths::production().product_root;
    let current = product_root.join("current");
    let lower_sink = match scope {
        SetupScope::StorageOnly => ramshared_tier::nbd_readiness::CapacitySample::Unknown,
        SetupScope::GpuCache => observe_lower_tier_capacity(&current),
    };

    let source = ReadinessDiagnosticSource;
    let outcome = execute_customer(
        CustomerCommand::SetupPlan {
            profile_name: profile_name.clone(),
            output: output.clone(),
            scope,
        },
        &CustomerDependencies {
            source: &source,
            limits: DiagnosticLimits::default(),
            setup: Some(crate::customer::setup_command::SetupObservation {
                report,
                release_version: release_version(),
                lower_sink,
                profile_root: default_configuration_root(),
            }),
        },
    );

    match outcome {
        Ok(outcome) => {
            // `--json` prints the exact document that was written, so a piped
            // consumer can never see a re-encoding that drifts from the file
            // `setup apply` reads.
            if json {
                if let Some(document) = outcome.document.as_deref() {
                    let _ = writeln!(stdout, "{document}");
                }
            } else {
                let _ = writeln!(stdout, "{}", outcome.result.detail);
            }
            0
        }
        Err(error) => {
            let _ = writeln!(stderr, "{error}");
            error.exit_code()
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn setup_scope_defaults_to_storage_and_requires_explicit_cache() {
        let base = ["--profile", "resource-profile", "--output", "plan.json"].map(str::to_string);
        let (_, _, scope, json) = parse_plan_args(&base).unwrap();
        assert_eq!(scope, SetupScope::StorageOnly);
        assert!(!json);
        for (value, expected) in [
            ("storage-only", SetupScope::StorageOnly),
            ("gpu-cache", SetupScope::GpuCache),
        ] {
            let args = base
                .iter()
                .cloned()
                .chain(["--scope".into(), value.into(), "--json".into()])
                .collect::<Vec<_>>();
            let (_, _, scope, json) = parse_plan_args(&args).unwrap();
            assert_eq!(scope, expected);
            assert!(json);
        }
        for extra in [
            vec!["--scope"],
            vec!["--scope", ""],
            vec!["--scope", "auto"],
            vec!["--scope", "--json"],
            vec!["--scope", "storage-only", "--scope", "gpu-cache"],
        ] {
            let args = base
                .iter()
                .cloned()
                .chain(extra.into_iter().map(str::to_string))
                .collect::<Vec<_>>();
            let error = parse_plan_args(&args).unwrap_err();
            assert_eq!(error.reason, CustomerReason::UnsupportedSurface);
            assert_eq!(error.exit_code(), 2);
        }
    }
    #[test]
    fn setup_plan_refuses_missing_and_option_shaped_values_before_observation() {
        for args in [
            vec!["--profile", "", "--output", "file"],
            vec!["--profile", "--json", "--output", "file"],
            vec!["--profile", "name", "--output", ""],
            vec!["--profile", "name", "--output", "--json"],
            vec!["--output", "file"],
            vec!["--profile", "name"],
        ] {
            assert!(
                parse_plan_args(&args.into_iter().map(str::to_string).collect::<Vec<_>>()).is_err()
            );
        }
        let args = vec![
            "--profile".into(),
            "x".repeat(MAX_PROFILE_NAME_BYTES + 1),
            "--output".into(),
            "file".into(),
        ];
        assert!(parse_plan_args(&args).is_err());
        let mut output = Vec::new();
        let mut errors = Vec::new();
        assert_eq!(run(&["apply".into()], &mut output, &mut errors), 2);
        assert!(output.is_empty());
    }

    #[test]
    fn setup_wizard_parses_json_and_refuses_unknown_flags() {
        assert_eq!(parse_wizard_args(&[]), Ok(false));
        assert_eq!(parse_wizard_args(&["--json".into()]), Ok(true));
        for bad in [
            vec!["--unknown".to_string()],
            vec!["--json".to_string(), "--json".to_string()],
            vec!["extra".to_string()],
        ] {
            let err = parse_wizard_args(&bad).unwrap_err();
            assert_eq!(err.reason, CustomerReason::UnsupportedSurface);
            assert_eq!(err.exit_code(), 2);
            let mut out = Vec::new();
            let mut err_buf = Vec::new();
            let mut full = vec!["wizard".to_string()];
            full.extend(bad);
            assert_eq!(run(&full, &mut out, &mut err_buf), 2);
            assert!(out.is_empty());
        }
    }
}
