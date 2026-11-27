// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/mod.rs
//! Customer command composition and the result/exit model (ITEM-4).
//!
//! DT-10: stable reasons, explicit Unknown/Blocked/RecoveryRequired outcomes,
//! no upload, no arbitrary elevated shell, no UAC bypass and no automatic
//! whole-WSL restart.
//!
//! ITEM-4 lands the vocabulary **before** mutating setup (ITEM-5) so every
//! later command refuses through one model instead of inventing an exit code
//! per call site. Exit mapping is for **new** command surfaces; existing
//! `up/down/status/top/recover/config/check/doctor` exits stay stable.

pub(crate) mod cache_configuration;
pub(crate) mod diagnostics;
pub(crate) mod gpu_plan;
pub(crate) mod gpu_set;
mod install_apply;
mod install_args;
mod install_drain;
pub(crate) use install_drain::observe_upgrade_drain;
mod install_fs;
mod install_lifecycle;
mod install_tokens;
pub(crate) use install_tokens::valid_release_token_segment;
pub(crate) mod install_transaction;
mod install_units;
pub(crate) mod package;
pub(crate) mod package_admission;
pub(crate) mod package_hook;
pub(crate) mod package_member;
mod package_prepare;
mod package_publish;
mod package_selector;
mod package_source;
mod package_stage;
mod package_stage_journal;
pub(crate) mod package_transition;
mod package_tree;
mod package_trust;
pub(crate) mod plan;
pub(crate) mod plan_document;
mod published_update;
mod release_ordering;
pub(crate) mod service;
pub(crate) mod setup_apply;
pub(crate) mod setup_command;
pub(crate) mod setup_scope;
#[cfg(test)]
mod test_space;
pub(crate) mod transaction;
pub(crate) mod transition;
pub(crate) mod transition_custody;

use std::path::PathBuf;

use diagnostics::{DiagnosticBundle, DiagnosticLimits, DiagnosticSource, collect_diagnostics};
use setup_command::{SetupObservation, SetupPlanRequest, run_setup_plan};

/// Installed/activation state. Observed fact, never requested intent.
///
/// The vocabulary is complete before the first mutator exists (ITEM-5) on
/// purpose: a later command must not be free to invent a state name. Only
/// `InstalledOff`, `RecoveryRequired` and `UnknownBlocked` are constructed
/// until setup lands; the rest are named now so they cannot drift.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(
    dead_code,
    reason = "state vocabulary is complete before ITEM-5 mutators"
)]
pub(crate) enum CustomerState {
    Absent,
    InstalledOff,
    PreparedOff,
    Ready,
    Active,
    Draining,
    RecoveryRequired,
    UnknownBlocked,
}

impl CustomerState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::InstalledOff => "installed_off",
            Self::PreparedOff => "prepared_off",
            Self::Ready => "ready",
            Self::Active => "active",
            Self::Draining => "draining",
            Self::RecoveryRequired => "recovery_required",
            Self::UnknownBlocked => "unknown_blocked",
        }
    }

    /// True when the product is serving tiers. A refusal never reaches these.
    pub(crate) fn is_serving(self) -> bool {
        matches!(self, Self::Active | Self::Draining)
    }
}

/// Stable refusal reason. The exit code is a property of the reason, not of
/// the call site, so two commands refuse the same defect the same way.
///
/// The vocabulary is the PRD error table, named in full before the first
/// mutator exists (ITEM-5) so a later command cannot invent a reason.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(
    dead_code,
    reason = "reason vocabulary is complete before ITEM-5 mutators"
)]
pub(crate) enum CustomerReason {
    /// Exit 2 — target or provider is not a supported surface.
    UnsupportedSurface,
    /// Exit 2 — numeric capacity/headroom is insufficient.
    CapacityRefused,
    /// Exit 3 — UAC/sudo denied or cancelled.
    AuthorizationRequired,
    /// Exit 4 — plan, device, distro or hash changed under the request.
    IdentityChanged,
    /// Exit 4 — a package action against an active session.
    ActiveSession,
    /// Exit 5 — an effect failed with uncertain device state.
    RecoveryRequired,
    /// Exit 6 — a bounded observation is unavailable; never a fabricated zero.
    ObservationUnavailable,
}

impl CustomerReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::UnsupportedSurface => "unsupported_surface",
            Self::CapacityRefused => "capacity_refused",
            Self::AuthorizationRequired => "authorization_required",
            Self::IdentityChanged => "identity_changed",
            Self::ActiveSession => "active_session",
            Self::RecoveryRequired => "recovery_required",
            Self::ObservationUnavailable => "observation_unavailable",
        }
    }

    /// PRD exit mapping for new command surfaces.
    pub(crate) fn exit_code(self) -> u8 {
        match self {
            Self::UnsupportedSurface | Self::CapacityRefused => 2,
            Self::AuthorizationRequired => 3,
            Self::IdentityChanged | Self::ActiveSession => 4,
            Self::RecoveryRequired => 5,
            Self::ObservationUnavailable => 6,
        }
    }
}

/// A refusal of a new command surface.
///
/// `state` is what remains true after the refusal. A refusal never advances
/// the state: the PRD rows that say "Installed/Off" are recorded here as
/// [`CustomerState::InstalledOff`], and the two that change the picture say
/// so explicitly ([`CustomerState::RecoveryRequired`] for an uncertain effect,
/// [`CustomerState::UnknownBlocked`] when the observation itself is missing).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CustomerError {
    pub(crate) reason: CustomerReason,
    pub(crate) detail: String,
    pub(crate) state: CustomerState,
}

impl CustomerError {
    pub(crate) fn exit_code(&self) -> u8 {
        self.reason.exit_code()
    }
}

impl std::fmt::Display for CustomerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.reason.as_str(), self.detail)
    }
}

/// Success shape of a customer command. Carries the observed state so a
/// caller never has to infer it from output text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CustomerResult {
    pub(crate) state: CustomerState,
    pub(crate) detail: String,
}

impl CustomerResult {
    /// Result of a command that performed no lifecycle mutation. It may not
    /// claim a serving state: that would be a lie about what the command did.
    fn read_only(state: CustomerState, detail: String) -> Result<Self, CustomerError> {
        if state.is_serving() {
            return Err(CustomerError {
                reason: CustomerReason::UnsupportedSurface,
                detail: format!(
                    "read-only command reported serving state {}",
                    state.as_str()
                ),
                state: CustomerState::InstalledOff,
            });
        }
        Ok(Self { state, detail })
    }
}

/// New command surface. Mutating setup commands are added under ITEM-5; this
/// enum only grows, so the result/exit model they will report through is
/// already fixed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CustomerCommand {
    /// `ramshared diagnose export --output <file>`
    DiagnoseExport { output: PathBuf },
    /// `ramshared setup plan --profile <name> --output <file> [--json]`
    ///
    /// The `--json` flag is a rendering choice on the command's stdout and is
    /// held by the CLI dispatcher, not by this command: planning is the same
    /// either way.
    SetupPlan {
        profile_name: String,
        output: PathBuf,
        scope: setup_scope::SetupScope,
    },
}

/// Everything a command needs, injected. No global device or provider state.
///
/// `setup` is the live observation a plan is computed from. It is `None` when
/// the caller did not assemble one; a setup command then refuses
/// `observation_unavailable` (exit 6) rather than inventing numbers, and the
/// diagnose surface never needs it.
pub(crate) struct CustomerDependencies<'a> {
    pub(crate) source: &'a dyn DiagnosticSource,
    pub(crate) limits: DiagnosticLimits,
    pub(crate) setup: Option<SetupObservation>,
}

/// What a customer command produced: the result every caller reports through,
/// plus the encoded artifact when the command wrote one.
///
/// `document` is the **exact** bytes written to the output file, so
/// `setup plan --json` cannot drift from what `setup apply` reads.
/// `diagnose export` writes a bundle and does not echo it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CustomerOutcome {
    pub(crate) result: CustomerResult,
    pub(crate) document: Option<String>,
}

/// Run one customer command. Every failure is a [`CustomerError`] with a
/// stable reason and the state it leaves behind.
pub(crate) fn execute_customer(
    command: CustomerCommand,
    dependencies: &CustomerDependencies<'_>,
) -> Result<CustomerOutcome, CustomerError> {
    match command {
        CustomerCommand::DiagnoseExport { output } => Ok(CustomerOutcome {
            result: export_diagnostics(&output, dependencies)?,
            document: None,
        }),
        CustomerCommand::SetupPlan {
            profile_name,
            output,
            scope,
        } => {
            let Some(observation) = dependencies.setup.as_ref() else {
                return Err(CustomerError {
                    reason: CustomerReason::ObservationUnavailable,
                    detail: "setup plan needs a readiness report, a release identity and a lower-tier sample; none was supplied"
                        .to_string(),
                    state: CustomerState::UnknownBlocked,
                });
            };
            let request = SetupPlanRequest {
                profile_name,
                output,
                scope,
            };
            let (result, document) = run_setup_plan(&request, observation)?;
            Ok(CustomerOutcome {
                result,
                document: Some(document),
            })
        }
    }
}

fn export_diagnostics(
    output: &std::path::Path,
    dependencies: &CustomerDependencies<'_>,
) -> Result<CustomerResult, CustomerError> {
    let bundle = collect_diagnostics(dependencies.source, dependencies.limits);
    write_bundle(output, &bundle)?;
    CustomerResult::read_only(
        CustomerState::InstalledOff,
        format!(
            "diagnostic bundle written ({} bytes, truncation={}, state={})",
            bundle.total_bytes,
            bundle.truncation.as_str(),
            CustomerState::InstalledOff.as_str()
        ),
    )
}

fn write_bundle(output: &std::path::Path, bundle: &DiagnosticBundle) -> Result<(), CustomerError> {
    std::fs::write(output, bundle.to_json()).map_err(|error| CustomerError {
        // The customer asked for observations and did not get them. The
        // export never mutates, so the installed state is unchanged.
        reason: CustomerReason::ObservationUnavailable,
        detail: format!(
            "cannot write diagnostic bundle to {}: {error}",
            output.display()
        ),
        state: CustomerState::InstalledOff,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::diagnostics::{DiagnosticEntry, DiagnosticSource};
    use super::*;
    use std::time::Instant;

    struct EmptySource;

    impl DiagnosticSource for EmptySource {
        fn observe(&self, _deadline: Instant) -> Vec<DiagnosticEntry> {
            Vec::new()
        }
    }

    /// A refusal of a new command surface leaves the product Installed/Off,
    /// exits with the PRD-mapped code and never exits zero. Uncertain effects
    /// and missing observations are the two explicit non-Off outcomes, and
    /// neither of them is a serving state.
    #[test]
    fn customer_refusal_exits_preserve_installed_off_state() {
        let cases = [
            (CustomerReason::UnsupportedSurface, 2u8),
            (CustomerReason::CapacityRefused, 2u8),
            (CustomerReason::AuthorizationRequired, 3u8),
            (CustomerReason::IdentityChanged, 4u8),
            (CustomerReason::ActiveSession, 4u8),
            (CustomerReason::RecoveryRequired, 5u8),
            (CustomerReason::ObservationUnavailable, 6u8),
        ];
        for (reason, expected_exit) in cases {
            let preserved = match reason {
                CustomerReason::RecoveryRequired => CustomerState::RecoveryRequired,
                CustomerReason::ObservationUnavailable => CustomerState::UnknownBlocked,
                _ => CustomerState::InstalledOff,
            };
            let error = CustomerError {
                reason,
                detail: format!("probe refusal for {}", reason.as_str()),
                state: preserved,
            };
            assert_eq!(
                error.exit_code(),
                expected_exit,
                "exit code drifted for {}",
                reason.as_str()
            );
            assert_ne!(error.exit_code(), 0, "{} exited zero", reason.as_str());
            assert!(
                !error.state.is_serving(),
                "{} advanced the product into a serving state",
                reason.as_str()
            );
            if reason != CustomerReason::RecoveryRequired
                && reason != CustomerReason::ObservationUnavailable
            {
                assert_eq!(
                    error.state,
                    CustomerState::InstalledOff,
                    "{} did not preserve installed Off",
                    reason.as_str()
                );
            }
        }

        // The two explicit non-Off outcomes are named states, not silence.
        assert_eq!(
            CustomerState::RecoveryRequired.as_str(),
            "recovery_required"
        );
        assert_eq!(CustomerState::UnknownBlocked.as_str(), "unknown_blocked");

        // The full vocabulary is named now so a later command cannot invent a
        // state name. Each name is unique and no variant is a serving state
        // except Active/Draining.
        let vocabulary = [
            (CustomerState::Absent, "absent", false),
            (CustomerState::InstalledOff, "installed_off", false),
            (CustomerState::PreparedOff, "prepared_off", false),
            (CustomerState::Ready, "ready", false),
            (CustomerState::Active, "active", true),
            (CustomerState::Draining, "draining", true),
            (CustomerState::RecoveryRequired, "recovery_required", false),
            (CustomerState::UnknownBlocked, "unknown_blocked", false),
        ];
        for (state, name, serving) in vocabulary {
            assert_eq!(state.as_str(), name);
            assert_eq!(state.is_serving(), serving, "{name} serving flag drifted");
        }
    }

    #[test]
    fn diagnose_export_reports_the_bundle_it_wrote() {
        let dir =
            std::env::temp_dir().join(format!("ramshared-customer-export-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let output = dir.join("bundle.json");
        let outcome = execute_customer(
            CustomerCommand::DiagnoseExport {
                output: output.clone(),
            },
            &CustomerDependencies {
                source: &EmptySource,
                limits: DiagnosticLimits::default(),
                setup: None,
            },
        )
        .unwrap();
        assert_eq!(outcome.result.state, CustomerState::InstalledOff);
        assert!(output.is_file());
        let body = std::fs::read_to_string(&output).unwrap();
        assert!(body.contains("ramshared-diagnostics/v1"));
        let _ = std::fs::remove_file(&output);
    }

    #[test]
    fn unwritable_export_is_an_observation_refusal_that_changes_nothing() {
        let error = execute_customer(
            CustomerCommand::DiagnoseExport {
                output: PathBuf::from("/proc/ramshared-diagnostics-never-writable"),
            },
            &CustomerDependencies {
                source: &EmptySource,
                limits: DiagnosticLimits::default(),
                setup: None,
            },
        )
        .expect_err("a non-writable path must refuse");
        assert_eq!(error.reason, CustomerReason::ObservationUnavailable);
        assert_eq!(error.exit_code(), 6);
        assert_eq!(error.state, CustomerState::InstalledOff);
    }

    #[test]
    fn reasons_map_to_their_documented_strings() {
        assert_eq!(
            CustomerReason::UnsupportedSurface.as_str(),
            "unsupported_surface"
        );
        assert_eq!(CustomerReason::CapacityRefused.as_str(), "capacity_refused");
        assert_eq!(
            CustomerReason::AuthorizationRequired.as_str(),
            "authorization_required"
        );
        assert_eq!(CustomerReason::IdentityChanged.as_str(), "identity_changed");
        assert_eq!(CustomerReason::ActiveSession.as_str(), "active_session");
        assert_eq!(
            CustomerReason::RecoveryRequired.as_str(),
            "recovery_required"
        );
        assert_eq!(
            CustomerReason::ObservationUnavailable.as_str(),
            "observation_unavailable"
        );
    }
}
