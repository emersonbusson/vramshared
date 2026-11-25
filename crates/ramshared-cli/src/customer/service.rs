// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/service.rs
//! Boot-policy consent and the decision it authorises (ITEM-6).
//!
//! PRD RF-6: boot policy is **optional and separate**. Installing a package or
//! saving a setup never enables boot. A boot opt-in retains fresh readiness —
//! it records a policy, not a capability snapshot — and it never keeps WSL
//! alive by assertion. PRD line: "User enables boot separately: policy is
//! recorded; later startup still checks current capabilities and safety."
//!
//! DT-5 / DT-9: this module owns the **decision** only. The canonical sealed
//! unit and its lifecycle stay with [`crate::cascade::boot`] and the existing
//! activation owner; there is no second activation surface here and no
//! `systemctl` call. Enable changes what a future boot may do. Disable changes
//! future activation only and never tears down a live tier.
//!
//! The grant is its own type on purpose. A setup [`super::plan::ApprovalBinding`]
//! authorises one setup plan. The cascade lifecycle token
//! (`activate|deactivate:<version>`) authorises one activation. Neither may
//! enable boot policy: a distinct document with its own identity is required,
//! so one consent cannot be replayed as another.

use serde::Deserialize;

use super::{CustomerError, CustomerReason, CustomerState};
use crate::customer::plan::ReleaseIdentity;
use crate::readiness::{CheckReport, Decision};

/// Longest identity field a boot-policy grant may carry. Same bound as the
/// setup transaction uses, so one malformed-document rule covers both.
const MAX_IDENTITY_FIELD_BYTES: usize = 128;

/// Consent for boot policy. Distinct from every other grant in the product.
///
/// A grant names the release it is for. The caller supplies the release it
/// currently observes; a mismatch is an identity change and re-plans, exactly
/// like a changed setup plan (PRD: "Changed plan/device/distro/hash | 4").
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BootPolicyGrant {
    /// Non-empty identifier of this consent, ≤ [`MAX_IDENTITY_FIELD_BYTES`].
    pub(crate) grant_id: String,
    /// Release version the consent was issued for.
    pub(crate) release_version: String,
}

impl BootPolicyGrant {
    /// A grant is shaped correctly when its identity fields are present and
    /// bounded. Shape is not authority: the caller still binds the window.
    pub(crate) fn is_well_formed(&self) -> bool {
        for value in [self.grant_id.as_str(), self.release_version.as_str()] {
            if value.is_empty() || value.len() > MAX_IDENTITY_FIELD_BYTES {
                return false;
            }
        }
        true
    }
}

/// Strict boot-grant document fields. Unknown fields are refused, not ignored:
/// an ignored field is how a caller smuggles a second meaning into a consent.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct BootPolicyGrantDocument {
    grant_id: String,
    release_version: String,
}

/// Parse a boot-policy grant document.
///
/// The document is its own identity and is not a setup approval or a cascade
/// lifecycle token under another name. A malformed document is a refusal on a
/// new command surface, so it reports `unsupported_surface` (exit 2) like every
/// other malformed request there.
pub(crate) fn parse_boot_grant(document: &str) -> Result<BootPolicyGrant, CustomerError> {
    let parsed: BootPolicyGrantDocument =
        serde_json::from_str(document).map_err(|error| CustomerError {
            reason: CustomerReason::UnsupportedSurface,
            detail: format!("boot grant document is not a strict grant object: {error}"),
            state: CustomerState::InstalledOff,
        })?;
    let grant = BootPolicyGrant {
        grant_id: parsed.grant_id,
        release_version: parsed.release_version,
    };
    if !grant.is_well_formed() {
        return Err(CustomerError {
            reason: CustomerReason::UnsupportedSurface,
            detail: format!(
                "boot grant identity fields must be present and at most {MAX_IDENTITY_FIELD_BYTES} bytes"
            ),
            state: CustomerState::InstalledOff,
        });
    }
    Ok(grant)
}

/// What the operator asked the boot policy to become.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BootPolicyAction {
    /// Record that a future boot may activate the cascade, under a distinct
    /// consent. Never activates anything now.
    Enable,
    /// Record that a future boot must not activate the cascade. Never stops
    /// anything now.
    Disable,
}

/// One boot-policy request.
///
/// `grant` is required for [`BootPolicyAction::Enable`] and ignored for
/// [`BootPolicyAction::Disable`]: opting out of a future convenience needs no
/// consent, and the decision still refuses to touch a live tier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BootPolicyRequest {
    pub(crate) action: BootPolicyAction,
    /// Release the caller currently observes. The grant must name the same.
    pub(crate) observed_release: ReleaseIdentity,
    /// The distinct consent. `None` for a disable, `Some` for an enable.
    pub(crate) grant: Option<BootPolicyGrant>,
}

/// The recorded policy after the decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BootPolicy {
    /// A future boot must not activate the cascade.
    Off,
    /// A future boot may activate the cascade. Startup still re-checks current
    /// capabilities and safety (RF-6); this is not a pinned readiness.
    On,
}

impl BootPolicy {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
        }
    }
}

/// The pure decision. Performing it is a later, separate owner's job.
///
/// `live_tier_untouched` is asserted by construction: this module has no
/// effect surface at all, so a decision cannot stop, drain or detach anything.
/// The field is on the plan so a renderer can state it without inferring it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BootPolicyPlan {
    /// What the policy will be after the effect.
    pub(crate) policy: BootPolicy,
    /// Release the decision is bound to.
    pub(crate) release_version: String,
    /// Consent consumed, when the action needed one.
    pub(crate) grant_id: Option<String>,
    /// Readiness the decision was computed under. Recorded so a later
    /// observation can show the opt-in did not pin it.
    pub(crate) readiness: Decision,
    /// Always `true`: a boot-policy decision never touches a live tier.
    pub(crate) live_tier_untouched: bool,
}

/// Decide one boot-policy request against the current readiness report.
///
/// Refuses through the PRD reason table only. A malformed new-surface request
/// is `unsupported_surface` (exit 2), never a shared 22/EINVAL.
pub(crate) fn plan_boot_policy(
    request: BootPolicyRequest,
    report: &CheckReport,
) -> Result<BootPolicyPlan, CustomerError> {
    let BootPolicyRequest {
        action,
        observed_release,
        grant,
    } = request;

    match action {
        BootPolicyAction::Disable => Ok(BootPolicyPlan {
            policy: BootPolicy::Off,
            release_version: observed_release.version,
            grant_id: None,
            readiness: report.decision(),
            live_tier_untouched: true,
        }),
        BootPolicyAction::Enable => {
            // Enabling is a promise that a future boot may start a root
            // service. It needs its own consent: a setup approval or a
            // cascade lifecycle token is a different authority and is not
            // present here as a `BootPolicyGrant` at all.
            let Some(grant) = grant else {
                return Err(CustomerError {
                    reason: CustomerReason::AuthorizationRequired,
                    detail: "boot policy opt-in requires a distinct boot grant; setup approval and cascade lifecycle consent do not authorize it".to_string(),
                    state: CustomerState::InstalledOff,
                });
            };

            if !grant.is_well_formed() {
                return Err(CustomerError {
                    reason: CustomerReason::UnsupportedSurface,
                    detail: "boot grant identity fields must be present and at most 128 bytes"
                        .to_string(),
                    state: CustomerState::InstalledOff,
                });
            }

            if grant.release_version != observed_release.version {
                return Err(CustomerError {
                    reason: CustomerReason::IdentityChanged,
                    detail: format!(
                        "boot grant names release {} and the observed release is {}; re-plan required, no new effects",
                        grant.release_version, observed_release.version
                    ),
                    state: CustomerState::InstalledOff,
                });
            }

            // RF-6 keeps readiness fresh rather than pinned, but an opt-in
            // recorded while the surface is already known blocked would be a
            // boot that every startup refuses. Refuse now instead of
            // installing a known-bad policy.
            if report.decision() != Decision::Ready {
                return Err(CustomerError {
                    reason: CustomerReason::UnsupportedSurface,
                    detail: format!(
                        "boot policy opt-in requires a ready surface and readiness reports {}: {}",
                        report.decision().as_str(),
                        if report.blockers.is_empty() {
                            "no blocker detail".to_string()
                        } else {
                            report.blockers.join("; ")
                        }
                    ),
                    state: CustomerState::InstalledOff,
                });
            }

            Ok(BootPolicyPlan {
                policy: BootPolicy::On,
                release_version: observed_release.version,
                grant_id: Some(grant.grant_id),
                readiness: report.decision(),
                live_tier_untouched: true,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::readiness::capacity::{CapacityObservation, CapacitySource};
    use crate::readiness::probes::{
        BackendProbe, GpuProviderKind, GpuProviderProbe, IoUringRuntime, KernelConfig,
        KernelFeatures, Observation, PlatformKind, PlatformProbe, Status, SysctlTuning,
    };
    use crate::readiness::{SurfaceSelection, TransportKind};

    /// The one typed report [`crate::readiness::CheckReport`], built
    /// field-by-field so the decision reads the real type rather than a
    /// lookalike.
    fn report_with(blockers: Vec<String>) -> CheckReport {
        CheckReport {
            platform: PlatformProbe {
                platform: Observation::Measured(PlatformKind::NativeLinux),
                release: "6.18.0".to_string(),
                version: "linux version test".to_string(),
            },
            swaps: Observation::Measured(Vec::new()),
            kernel: KernelFeatures {
                config_source: Some("boot".to_string()),
                swap: Some(KernelConfig::BuiltIn),
                io_uring: Some(KernelConfig::BuiltIn),
                io_uring_runtime: Some(IoUringRuntime::Enabled),
                nbd: Some(KernelConfig::Module),
                ublk: Some(KernelConfig::Module),
                zram: Some(KernelConfig::Module),
            },
            gpu: GpuProviderProbe {
                kind: Observation::Measured(GpuProviderKind::Vulkan),
                status: Status::Ok,
                libcuda_path: None,
                dxg_present: false,
                nvidia_smi_path: None,
                nvidia_smi_status: None,
                nvidia_smi_output: None,
                gpu: None,
                detail: String::new(),
            },
            backends: BackendProbe {
                nbd_status: Status::Ok,
                nbd_detail: String::new(),
                ublk_status: Status::Ok,
                ublk_detail: String::new(),
            },
            sysctl: SysctlTuning::default(),
            surface: SurfaceSelection {
                platform: Observation::Measured(PlatformKind::NativeLinux),
                transport: TransportKind::Nbd,
                gpu_backend: Observation::Measured(GpuProviderKind::Vulkan),
            },
            gpu_capacities: crate::readiness::published_members::PublishedGpuCapacities::Legacy,
            capacity: CapacityObservation {
                source: CapacitySource::DriverReported,
                age_ms: Some(1),
                effective_bytes: Some(6 * 1024 * 1024 * 1024),
                driver_budget: None,
                provider: Some(GpuProviderKind::Vulkan),
            },
            blockers,
            warnings: Vec::new(),
        }
    }

    fn grant(id: &str, release: &str) -> BootPolicyGrant {
        BootPolicyGrant {
            grant_id: id.to_string(),
            release_version: release.to_string(),
        }
    }

    /// §12 Boot consent. Enable requires a distinct grant, and disable
    /// changes future activation only while an active owner keeps serving.
    ///
    /// The name is the SPEC's. Both halves are proved here: no other consent
    /// is accepted as a boot grant, and disabling never becomes a teardown.
    #[test]
    fn boot_enable_requires_distinct_grant_and_disable_preserves_active_owner() {
        let report = report_with(Vec::new());
        let release = ReleaseIdentity::new("0.15.0");

        // Enable with no grant at all: refused as missing authorization, and
        // the refusal leaves the product Installed/Off rather than half-on.
        let bare = plan_boot_policy(
            BootPolicyRequest {
                action: BootPolicyAction::Enable,
                observed_release: release.clone(),
                grant: None,
            },
            &report,
        )
        .expect_err("enable without a grant must refuse");
        assert_eq!(bare.reason, CustomerReason::AuthorizationRequired);
        assert_eq!(bare.exit_code(), 3);
        assert_eq!(bare.state, CustomerState::InstalledOff);

        // A grant is required to be *this* kind of consent. The cascade
        // lifecycle token shape (`activate:<version>`) is not a boot grant;
        // it is a different authority and is refused the same way as absence
        // when it is not presented as one. A setup approval binding cannot be
        // converted into a `BootPolicyGrant` without a caller inventing one,
        // and an invented identity field is what the next cases cover.
        let empty_id = plan_boot_policy(
            BootPolicyRequest {
                action: BootPolicyAction::Enable,
                observed_release: release.clone(),
                grant: Some(grant("", "0.15.0")),
            },
            &report,
        )
        .expect_err("an empty grant id is not a consent");
        assert_eq!(empty_id.reason, CustomerReason::UnsupportedSurface);
        assert_eq!(empty_id.exit_code(), 2);

        // A grant for a different release is an identity change, not an
        // authorization success.
        let wrong_release = plan_boot_policy(
            BootPolicyRequest {
                action: BootPolicyAction::Enable,
                observed_release: release.clone(),
                grant: Some(grant("boot-grant-1", "0.14.0")),
            },
            &report,
        )
        .expect_err("a grant for another release must refuse");
        assert_eq!(wrong_release.reason, CustomerReason::IdentityChanged);
        assert_eq!(wrong_release.exit_code(), 4);
        assert_eq!(wrong_release.state, CustomerState::InstalledOff);

        // The legitimate case: a well-formed grant naming the observed
        // release, on a ready surface. The plan records On and is idempotent
        // (#17) — a second identical request produces the identical plan.
        let enable = BootPolicyRequest {
            action: BootPolicyAction::Enable,
            observed_release: release.clone(),
            grant: Some(grant("boot-grant-1", "0.15.0")),
        };
        let first = plan_boot_policy(enable.clone(), &report).expect("valid enable must plan");
        let second = plan_boot_policy(enable, &report).expect("replay must plan the same");
        assert_eq!(first, second, "boot enable is not idempotent");
        assert_eq!(first.policy, BootPolicy::On);
        assert_eq!(first.policy.as_str(), "on");
        assert_eq!(first.release_version, "0.15.0");
        assert_eq!(first.grant_id.as_deref(), Some("boot-grant-1"));
        assert_eq!(first.readiness, Decision::Ready);
        assert!(
            first.live_tier_untouched,
            "an enable must never present itself as an activation"
        );

        // Disable needs no grant and is always available. It records Off and
        // explicitly preserves the live owner: a serving cascade keeps
        // serving, and nothing here is allowed to imply otherwise.
        let off = plan_boot_policy(
            BootPolicyRequest {
                action: BootPolicyAction::Disable,
                observed_release: release.clone(),
                // Even a stale or absent grant is irrelevant to disable.
                grant: None,
            },
            &report,
        )
        .expect("disable must plan");
        assert_eq!(off.policy, BootPolicy::Off);
        assert_eq!(off.policy.as_str(), "off");
        assert_eq!(off.release_version, "0.15.0");
        assert_eq!(off.grant_id, None);
        assert!(
            off.live_tier_untouched,
            "disable changes future activation only; it must not tear down a live tier"
        );

        // Disable is idempotent too, and identical whether or not a caller
        // passed a grant it will not use.
        let off_with_stray_grant = plan_boot_policy(
            BootPolicyRequest {
                action: BootPolicyAction::Disable,
                observed_release: release,
                grant: Some(grant("boot-grant-1", "0.15.0")),
            },
            &report,
        )
        .expect("disable ignores a stray grant");
        assert_eq!(off, off_with_stray_grant);
    }

    /// RF-6 keeps readiness fresh. An opt-in recorded while the surface is
    /// already blocked would be a boot every startup refuses, so enable is
    /// refused rather than installing a known-bad policy. Disable is still
    /// available and still touches nothing.
    #[test]
    fn boot_enable_refuses_on_a_blocked_surface_and_disable_still_plans() {
        let blocked = report_with(vec!["gpu capacity sample is stale".to_string()]);
        assert_eq!(blocked.decision(), Decision::Blocked);
        let release = ReleaseIdentity::new("0.15.0");

        let error = plan_boot_policy(
            BootPolicyRequest {
                action: BootPolicyAction::Enable,
                observed_release: release.clone(),
                grant: Some(grant("boot-grant-1", "0.15.0")),
            },
            &blocked,
        )
        .expect_err("enable on a blocked surface must refuse");
        assert_eq!(error.reason, CustomerReason::UnsupportedSurface);
        assert_eq!(error.exit_code(), 2);
        assert_eq!(error.state, CustomerState::InstalledOff);
        assert!(
            error.detail.contains("gpu capacity sample is stale"),
            "the refusal must carry the blocker detail, got {}",
            error.detail
        );

        let off = plan_boot_policy(
            BootPolicyRequest {
                action: BootPolicyAction::Disable,
                observed_release: release,
                grant: None,
            },
            &blocked,
        )
        .expect("disable plans even when readiness is blocked");
        assert_eq!(off.policy, BootPolicy::Off);
        assert_eq!(off.readiness, Decision::Blocked);
        assert!(off.live_tier_untouched);
    }

    /// A grant's identity fields are bounded. This is a shape rule, not
    /// authority: it keeps a malformed document from reaching an effect owner.
    #[test]
    fn boot_grant_identity_fields_are_present_and_bounded() {
        assert!(grant("boot-grant-1", "0.15.0").is_well_formed());
        assert!(!grant("", "0.15.0").is_well_formed());
        assert!(!grant("boot-grant-1", "").is_well_formed());
        let long_id = "x".repeat(MAX_IDENTITY_FIELD_BYTES + 1);
        assert!(!grant(&long_id, "0.15.0").is_well_formed());
        let long_release = "y".repeat(MAX_IDENTITY_FIELD_BYTES + 1);
        assert!(!grant("boot-grant-1", &long_release).is_well_formed());
        let exact = "z".repeat(MAX_IDENTITY_FIELD_BYTES);
        assert!(grant(&exact, "0.15.0").is_well_formed());
    }

    /// §12 boot-grant document. Unknown fields are refused, not ignored: an
    /// ignored field is how a caller smuggles a second meaning into a consent.
    #[test]
    fn boot_grant_document_is_strict_and_refuses_every_malformed_shape() {
        let parsed = parse_boot_grant(r#"{"grant_id":"boot-1","release_version":"0.15.0"}"#)
            .expect("a well-formed grant document is admitted");
        assert_eq!(
            parsed,
            grant("boot-1", "0.15.0"),
            "the document and the typed grant carry the same identity"
        );

        for document in [
            "not-json",
            "{}",
            r#"{"grant_id":"boot-1"}"#,
            r#"{"release_version":"0.15.0"}"#,
            r#"{"grant_id":"boot-1","release_version":"0.15.0","extra":true}"#,
            r#"{"grant_id":"","release_version":"0.15.0"}"#,
            r#"{"grant_id":"boot-1","release_version":""}"#,
        ] {
            let error =
                parse_boot_grant(document).expect_err("a malformed boot-grant document is refused");
            assert!(
                matches!(error.reason, CustomerReason::UnsupportedSurface),
                "a malformed new-surface document is unsupported_surface (exit 2): {error}"
            );
        }
    }
}
