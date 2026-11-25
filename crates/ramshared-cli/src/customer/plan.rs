// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/plan.rs
//! Pure checked setup plan (ITEM-5).
//!
//! Planning is arithmetic and identity pinning only. It performs no I/O, holds
//! no locks and applies no effect: a plan is the approved intent a later
//! [`super::transaction`] step either matches exactly or refuses as changed.
//!
//! Every number here is **read from an existing authority**, never re-derived:
//!
//! - the canonical reserve and the enforced free floor come from
//!   [`ramshared_vram::reserve_policy::ReserveFloorPolicy`];
//! - the NBD lower-sink requirement comes from
//!   [`ramshared_tier::nbd_readiness::minimum_lower_tier_bytes`];
//! - the profile fingerprint is the one digest in
//!   [`crate::resource_config`], not a second hash of the same bytes.
//!
//! The PRD/SPEC proposed name for the readiness input is `CapabilityReport`.
//! That type is the report ITEM-2 already extracted as
//! [`crate::readiness::CheckReport`]; a second report type answering the same
//! question is not created here.

use std::collections::BTreeSet;

use ramshared_config::resource_profile::{MAX_RESOURCE_PROFILE_BYTES, ResourceProfile};
use ramshared_tier::nbd_readiness::CapacitySample;
use serde::{Deserialize, Serialize};

use super::setup_scope::{SetupOperation, SetupScope};
use super::{CustomerError, CustomerReason, CustomerState};
use crate::readiness::{CheckReport, Decision};
#[cfg(test)]
use ramshared_config::resource_profile::ResourceTarget;

/// The release identity a plan pins.
///
/// A **value object** produced by the caller from
/// [`crate::release_identity`], which stays the single reader of the sealed
/// bundle. This type only carries the identity forward so a later apply can
/// refuse a changed one; it never opens a file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReleaseIdentity {
    pub(crate) version: String,
}

impl ReleaseIdentity {
    pub(crate) fn new(version: impl Into<String>) -> Self {
        Self {
            version: version.into(),
        }
    }
}

/// The approved, pinned intent of one setup.
///
/// Every field is a number or an identity that a later apply compares. A plan
/// carries no capability the requester did not already have and no effect.
///
/// This is also the **wire body** of [`super::plan_document::SetupPlanDocument`]:
/// one schema, so the document cannot describe a plan the arithmetic never
/// produced. Unknown fields are refused for the same reason the approval
/// document refuses them — an ignored field is a second meaning smuggled in.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetupPlan {
    pub(crate) operation: SetupOperation,
    /// Pinned release identity.
    pub(crate) release_version: String,
    /// Pinned resource-profile schema version.
    pub(crate) profile_schema_version: u32,
    /// Pinned resource-profile content fingerprint.
    pub(crate) profile_fingerprint: String,
    /// The readiness decision this plan was computed under.
    pub(crate) report_decision: Decision,
}

impl SetupPlan {
    pub(crate) fn validate_operation(&self) -> Result<(), CustomerError> {
        self.operation.validate(self.report_decision)
    }

    /// The binding this plan accepts. A binding for any other plan is a
    /// replay and is refused at apply time.
    pub(crate) fn fingerprint(&self) -> String {
        // Length framing prevents field separators in identities from changing
        // the meaning of an approved plan. Every wire-body field participates.
        let Self {
            operation,
            release_version,
            profile_schema_version,
            profile_fingerprint,
            report_decision,
        } = self;
        let mut text = String::from("ramshared-setup-plan-v4:");
        for value in [
            release_version.clone(),
            profile_schema_version.to_string(),
            profile_fingerprint.clone(),
            report_decision.as_str().to_string(),
        ]
        .into_iter()
        .chain(operation.identity_values())
        {
            text.push_str(&value.len().to_string());
            text.push(':');
            text.push_str(&value);
        }
        crate::resource_config::profile_digest(&text)
    }
}

/// Strict approval document fields. Unknown fields are refused, not ignored:
/// an ignored field is how a caller smuggles a second meaning into a grant.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct ApprovalDocument {
    approval_id: String,
    plan_fingerprint: String,
    issued_unix: u64,
    expires_unix: u64,
}

/// A single-use authorization for one exact plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ApprovalBinding {
    pub(crate) approval_id: String,
    pub(crate) plan_fingerprint: String,
    pub(crate) issued_unix: u64,
    pub(crate) expires_unix: u64,
}

/// Maximum length of an approval identifier, in bytes.
const MAX_APPROVAL_ID_BYTES: usize = 128;

fn refused(reason: CustomerReason, detail: impl Into<String>) -> CustomerError {
    CustomerError {
        reason,
        detail: detail.into(),
        state: CustomerState::InstalledOff,
    }
}

/// Parse and admit an approval binding.
///
/// `consumed` is the set of approval identifiers that have already authorized
/// an effect. A binding naming one of them is a **replay**: the PRD row for a
/// changed plan/hash is `identity_changed` (exit 4, "re-plan required; no new
/// effects"), which is exactly what a replay must produce. A malformed
/// document is a refusal on a new command surface, so it reports
/// `unsupported_surface` (exit 2) like every other malformed request there.
pub(crate) fn parse_approval(
    document: &str,
    consumed: &BTreeSet<String>,
) -> Result<ApprovalBinding, CustomerError> {
    if document.len() > MAX_RESOURCE_PROFILE_BYTES {
        return Err(refused(
            CustomerReason::UnsupportedSurface,
            format!(
                "approval document is {} bytes; the bound is {MAX_RESOURCE_PROFILE_BYTES}",
                document.len()
            ),
        ));
    }

    let parsed: ApprovalDocument = serde_json::from_str(document).map_err(|error| {
        refused(
            CustomerReason::UnsupportedSurface,
            format!("approval document is not a strict approval object: {error}"),
        )
    })?;

    if parsed.approval_id.is_empty() || parsed.approval_id.len() > MAX_APPROVAL_ID_BYTES {
        return Err(refused(
            CustomerReason::UnsupportedSurface,
            format!(
                "approval_id length {} is outside 1..={MAX_APPROVAL_ID_BYTES}",
                parsed.approval_id.len()
            ),
        ));
    }
    if parsed.plan_fingerprint.is_empty() || parsed.plan_fingerprint.len() > 128 {
        return Err(refused(
            CustomerReason::UnsupportedSurface,
            "plan_fingerprint is empty or oversized",
        ));
    }
    if parsed.expires_unix <= parsed.issued_unix {
        return Err(refused(
            CustomerReason::UnsupportedSurface,
            "approval expires_unix must be after issued_unix",
        ));
    }

    if consumed.contains(&parsed.approval_id) {
        return Err(refused(
            CustomerReason::IdentityChanged,
            format!(
                "approval {} has already authorized an effect; a replay produces no new effects",
                parsed.approval_id
            ),
        ));
    }

    Ok(ApprovalBinding {
        approval_id: parsed.approval_id,
        plan_fingerprint: parsed.plan_fingerprint,
        issued_unix: parsed.issued_unix,
        expires_unix: parsed.expires_unix,
    })
}

/// True when `now_unix` is inside the grant's half-open validity window.
///
/// One predicate for "this authorization is currently valid", shared by the
/// planner's match check and the transaction's apply gate, so the two can
/// never disagree about what `expires_unix` means.
pub(crate) fn approval_window_open(binding: &ApprovalBinding, now_unix: u64) -> bool {
    now_unix >= binding.issued_unix && now_unix < binding.expires_unix
}

/// True when the binding authorizes exactly this plan and has not expired.
pub(crate) fn binding_matches(binding: &ApprovalBinding, plan: &SetupPlan, now_unix: u64) -> bool {
    binding.plan_fingerprint == plan.fingerprint() && approval_window_open(binding, now_unix)
}

fn validate_profile_for_report(
    profile: &ResourceProfile,
    report: &CheckReport,
) -> Result<(), CustomerError> {
    let platform = match &report.platform.platform {
        crate::readiness::probes::Observation::Measured(
            crate::readiness::probes::PlatformKind::NativeLinux,
        ) => ramshared_config::resource_profile::ResourcePlatform::NativeLinux,
        crate::readiness::probes::Observation::Measured(
            crate::readiness::probes::PlatformKind::Wsl2,
        ) => ramshared_config::resource_profile::ResourcePlatform::Wsl2,
        _ => {
            return Err(CustomerError {
                reason: CustomerReason::ObservationUnavailable,
                detail: "setup platform is not measured; no profile authority can be inferred"
                    .into(),
                state: CustomerState::UnknownBlocked,
            });
        }
    };
    profile.validate_for(platform).map_err(|error| {
        refused(
            CustomerReason::UnsupportedSurface,
            format!("setup profile is not valid on the observed platform: {error}"),
        )
    })?;
    profile
        .required_free_bytes_by_volume()
        .map(|_| ())
        .map_err(|error| {
            refused(
                CustomerReason::CapacityRefused,
                format!("setup storage requirements cannot be represented: {error}"),
            )
        })
}

/// Read-only authorities used to bind one setup intent.
pub(crate) struct SetupInputs<'a> {
    pub(crate) report: &'a CheckReport,
    pub(crate) profile: &'a ResourceProfile,
    pub(crate) profile_fingerprint: &'a str,
    pub(crate) release: &'a ReleaseIdentity,
    pub(crate) lower_sink: &'a CapacitySample,
}

/// Compute explicit intent, without granting allocation or activation.
pub(crate) fn plan_setup(
    scope: SetupScope,
    input: SetupInputs<'_>,
) -> Result<SetupPlan, CustomerError> {
    validate_profile_for_report(input.profile, input.report)?;
    let operation = SetupOperation::capture(scope, input.report, input.profile, input.lower_sink)?;
    Ok(SetupPlan {
        operation,
        release_version: input.release.version.clone(),
        profile_schema_version: input.profile.schema_version,
        profile_fingerprint: input.profile_fingerprint.to_string(),
        report_decision: input.report.decision(),
    })
}

#[cfg(test)]
mod tests;
