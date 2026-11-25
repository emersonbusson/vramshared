// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/service.rs
//! `ramshared service` — the boot-policy command surface (ITEM-6).
//!
//! SPEC: `ramshared service enable-boot --approval <file> [--json]` records
//! that a future boot may activate the cascade, under a distinct consent.
//! `ramshared service disable-boot [--json]` records that it must not.
//! `ramshared service status [--json]` reads the recorded policy and changes
//! nothing.
//!
//! None of the three ever touches a live tier. Enable and disable move only the
//! enablement of the sealed boot set — the prerequisite, the observer and the
//! cascade; the cascade that is running now keeps running. A future boot still
//! re-checks current capabilities and safety (RF-6), so this is a recorded
//! policy and not a pinned readiness.
//!
//! Exit model: this is a **new** command surface, so every refusal — including
//! an unknown flag or a missing required option — reports through
//! [`crate::customer::CustomerError`] and the PRD mapping. An unknown flag is
//! `unsupported_surface` (exit 2), never the shared 22/EINVAL the existing
//! commands keep.

use std::io::Read;
use std::io::Write;
use std::path::{Path, PathBuf};

use ramshared_config::resource_profile::MAX_RESOURCE_PROFILE_BYTES;

use crate::cascade::boot::release_version;
use crate::cascade::boot_policy_effect::{
    BootPolicyHost, BootPolicyObservation, apply_boot_policy, observe_boot_policy,
};
use crate::cascade::boot_policy_host::SystemBootPolicyHost;
use crate::customer::plan::ReleaseIdentity;
use crate::customer::service::{
    BootPolicy, BootPolicyAction, BootPolicyGrant, BootPolicyRequest, parse_boot_grant,
    plan_boot_policy,
};
use crate::customer::transition::{OperationOwner, TransitionPaths, acquire_transition};
use crate::customer::{CustomerError, CustomerReason, CustomerState};
use crate::readiness::{CheckReport, run_check};

/// `ramshared service` is a new command surface: a malformed request is a
/// refusal with a stable reason, not an errno.
fn refused(detail: impl Into<String>) -> CustomerError {
    CustomerError {
        reason: CustomerReason::UnsupportedSurface,
        detail: detail.into(),
        state: CustomerState::InstalledOff,
    }
}

/// What the parser accepted. One shape per subcommand, no shared bag of flags.
#[derive(Clone, Debug, Eq, PartialEq)]
enum ServiceAction {
    EnableBoot { approval: PathBuf, json: bool },
    DisableBoot { json: bool },
    Status { json: bool },
}

/// Strict parser for `enable-boot --approval <file> [--json]`.
///
/// Option order is free, each named option may appear at most once, and every
/// unknown token is a refusal.
fn parse_enable_boot(args: &[String]) -> Result<ServiceAction, CustomerError> {
    let mut approval: Option<PathBuf> = None;
    let mut json = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--json" if !json => json = true,
            "--approval" if approval.is_none() && index + 1 < args.len() => {
                index += 1;
                let path = args[index].as_str();
                if path.is_empty() || path.starts_with("--") {
                    return Err(refused(
                        "service enable-boot --approval needs a file path, not an option or an empty value",
                    ));
                }
                approval = Some(PathBuf::from(path));
            }
            other => {
                return Err(refused(format!(
                    "unsupported service enable-boot argument: {other}"
                )));
            }
        }
        index += 1;
    }
    let Some(approval) = approval else {
        return Err(refused("service enable-boot requires --approval <file>"));
    };
    Ok(ServiceAction::EnableBoot { approval, json })
}

/// Strict parser for `disable-boot [--json]` and `status [--json]`.
fn parse_no_option(subcommand: &str, args: &[String]) -> Result<ServiceAction, CustomerError> {
    let mut json = false;
    for arg in args {
        match arg.as_str() {
            "--json" if !json => json = true,
            other => {
                return Err(refused(format!(
                    "unsupported service {subcommand} argument: {other}"
                )));
            }
        }
    }
    Ok(match subcommand {
        "disable-boot" => ServiceAction::DisableBoot { json },
        _ => ServiceAction::Status { json },
    })
}

/// Strict parser for the whole `service` surface.
fn parse_service_args(args: &[String]) -> Result<ServiceAction, CustomerError> {
    let Some((subcommand, rest)) = args.split_first() else {
        return Err(refused(
            "usage: ramshared service enable-boot --approval <file> [--json] | disable-boot [--json] | status [--json]",
        ));
    };
    match subcommand.as_str() {
        "enable-boot" => parse_enable_boot(rest),
        "disable-boot" => parse_no_option("disable-boot", rest),
        "status" => parse_no_option("status", rest),
        other => Err(refused(format!(
            "unsupported service subcommand: {other} (expected `enable-boot`, `disable-boot` or `status`)"
        ))),
    }
}

/// Read one bounded boot-grant document.
///
/// The bound is the same one every other customer document uses, so one
/// malformed-document rule covers the whole surface.
fn read_boot_grant(path: &Path) -> Result<String, CustomerError> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        refused(format!(
            "cannot stat boot grant {}: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(refused(format!(
            "boot grant {} is not a regular file",
            path.display()
        )));
    }
    if metadata.len() > MAX_RESOURCE_PROFILE_BYTES as u64 {
        return Err(refused(format!(
            "boot grant document is {} bytes; the bound is {MAX_RESOURCE_PROFILE_BYTES}",
            metadata.len()
        )));
    }
    let file = std::fs::File::open(path).map_err(|error| {
        refused(format!(
            "cannot open boot grant {}: {error}",
            path.display()
        ))
    })?;
    let mut text = String::new();
    file.take((MAX_RESOURCE_PROFILE_BYTES + 1) as u64)
        .read_to_string(&mut text)
        .map_err(|error| {
            refused(format!(
                "cannot read boot grant {}: {error}",
                path.display()
            ))
        })?;
    if text.len() > MAX_RESOURCE_PROFILE_BYTES {
        return Err(refused(format!(
            "boot grant document is larger than {MAX_RESOURCE_PROFILE_BYTES} bytes"
        )));
    }
    Ok(text)
}

/// What a mutating subcommand decides against. Injected so a test never runs
/// the live readiness probes.
struct ServiceObservation {
    report: CheckReport,
    release: ReleaseIdentity,
}

/// Supplier of the observation a mutating subcommand decides against.
///
/// Production reads the live readiness report and the sealed release identity.
/// A test supplies the two numbers it wants the decision to see, so an enable
/// is never refused as `identity_changed` because the host's version string
/// moved under a fixture.
trait ServiceObserver {
    fn observe(&self) -> ServiceObservation;
}

/// Production observer: the live surface, read once per mutating subcommand.
struct LiveObservation;

impl ServiceObserver for LiveObservation {
    fn observe(&self) -> ServiceObservation {
        ServiceObservation {
            report: run_check(),
            release: ReleaseIdentity::new(release_version()),
        }
    }
}

/// Acquire boot-surface custody, decide, and perform the decided policy.
///
/// Custody is [`OperationOwner::Boot`]: both mutating subcommands touch the
/// boot surface, so they exclude each other and a concurrent `boot`, and they
/// honor a durable [`crate::customer::transition::PackagePending`] record.
fn run_boot_policy(
    action: BootPolicyAction,
    grant: Option<BootPolicyGrant>,
    observation: ServiceObservation,
    host: &impl BootPolicyHost,
    paths: &TransitionPaths,
) -> Result<BootPolicy, CustomerError> {
    let _guard = acquire_transition(OperationOwner::Boot, paths)?;
    let request = BootPolicyRequest {
        action,
        observed_release: observation.release,
        grant,
    };
    let plan = plan_boot_policy(request, &observation.report)?;
    apply_boot_policy(&plan, host)?;
    Ok(plan.policy)
}

/// Render one decided policy for stdout.
fn render_policy(policy: BootPolicy, json: bool) -> String {
    if json {
        format!("{{\"policy\":\"{}\"}}", policy.as_str())
    } else {
        format!("boot policy recorded as {}", policy.as_str())
    }
}

/// Render one observation for stdout.
///
/// `policy` is `null` exactly when the set disagrees: a half-state is not a
/// recorded policy and must not be printed as one. The JSON key is `boot_set`
/// because the observation covers every member of the set, not a pair.
fn render_status(observation: BootPolicyObservation, json: bool) -> String {
    let (cascade_enabled, supervisor_enabled, host_gate_enabled) =
        observation.enablement().as_triple();
    if json {
        let policy = match observation.policy() {
            Some(policy) => format!("\"{}\"", policy.as_str()),
            None => "null".to_string(),
        };
        format!(
            "{{\"policy\":{policy},\"boot_set\":\"{}\",\"cascade_enabled\":{cascade_enabled},\"supervisor_enabled\":{supervisor_enabled},\"host_gate_enabled\":{host_gate_enabled},\"live_tier_untouched\":true}}",
            observation.as_str()
        )
    } else {
        format!(
            "boot policy: {} (cascade enabled={cascade_enabled}, supervisor enabled={supervisor_enabled}, host-gate enabled={host_gate_enabled}); a live tier is never touched by a policy change",
            observation.as_str()
        )
    }
}

/// `ramshared service` dispatch. Returns the PRD-mapped exit code.
pub(crate) fn run(args: &[String], stdout: &mut dyn Write, stderr: &mut dyn Write) -> u8 {
    run_from(
        args,
        stdout,
        stderr,
        &SystemBootPolicyHost,
        &TransitionPaths::production(),
        &LiveObservation,
    )
}

/// Dispatch with an injected host, layout and observation. The production
/// wrapper above names the real ones; a test never reaches systemd, the product
/// layout or the live readiness probes.
fn run_from(
    args: &[String],
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    host: &impl BootPolicyHost,
    paths: &TransitionPaths,
    observer: &impl ServiceObserver,
) -> u8 {
    let action = match parse_service_args(args) {
        Ok(action) => action,
        Err(error) => {
            let _ = writeln!(stderr, "{error}");
            return error.exit_code();
        }
    };
    match action {
        ServiceAction::Status { json } => {
            // Read-only: no custody, no readiness probe, no release read. A
            // status must be cheap enough to run while an operator watches.
            match observe_boot_policy(host) {
                Ok(observation) => {
                    let _ = writeln!(stdout, "{}", render_status(observation, json));
                    0
                }
                Err(error) => {
                    let _ = writeln!(stderr, "{error}");
                    error.exit_code()
                }
            }
        }
        ServiceAction::DisableBoot { json } => {
            match run_boot_policy(
                BootPolicyAction::Disable,
                None,
                observer.observe(),
                host,
                paths,
            ) {
                Ok(policy) => {
                    let _ = writeln!(stdout, "{}", render_policy(policy, json));
                    0
                }
                Err(error) => {
                    let _ = writeln!(stderr, "{error}");
                    error.exit_code()
                }
            }
        }
        ServiceAction::EnableBoot { approval, json } => {
            let document = match read_boot_grant(&approval) {
                Ok(document) => document,
                Err(error) => {
                    let _ = writeln!(stderr, "{error}");
                    return error.exit_code();
                }
            };
            let grant = match parse_boot_grant(&document) {
                Ok(grant) => grant,
                Err(error) => {
                    let _ = writeln!(stderr, "{error}");
                    return error.exit_code();
                }
            };
            match run_boot_policy(
                BootPolicyAction::Enable,
                Some(grant),
                observer.observe(),
                host,
                paths,
            ) {
                Ok(policy) => {
                    let _ = writeln!(stdout, "{}", render_policy(policy, json));
                    0
                }
                Err(error) => {
                    let _ = writeln!(stderr, "{error}");
                    error.exit_code()
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
