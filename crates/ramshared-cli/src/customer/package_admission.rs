// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_admission.rs
//! DT-6 package admission: one pure decision, no effect (ITEM-3 / ITEM-7).
//!
//! [`admit_package`] decides whether one package action may proceed against an
//! observed owned lifecycle and a named release. It sits beside the
//! release-manifest validator in [`super::package`] — the two are one surface
//! split by responsibility, not by product concept: that file owns *what the
//! payload is*, this one owns *whether the lifecycle may be touched*.
//!
//! Effects stay with the existing release selection owner; nothing here opens a
//! file or moves a byte. The durable exclusion that keeps a second action out
//! while the first runs is [`super::transition::PackagePending`], enrolled by
//! the effect owner that consumes the returned [`PackagePlan`].
use super::package::{identity_changed, observation_unavailable, unsupported};
use super::plan::ReleaseIdentity;
use super::transition::PackageAction;
use super::{CustomerError, CustomerReason, CustomerState};

/// Who owns the observed lifecycle.
///
/// DT-4: unknown owner, process or device observations fail closed. A package
/// action is admitted only against a lifecycle this product owns and can name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LifecycleOwnership {
    /// This product's owned lifecycle, identified and understood.
    Owned,
    /// A foreign or ambiguous process or device holds it.
    Foreign,
    /// The owner could not be observed at all.
    Unobserved,
}

/// The owned lifecycle observation a package action admits against.
///
/// `state` reuses [`CustomerState`] so a package path cannot invent a state
/// name. `guarded_hooks` is observed from the journal surface, never inferred
/// from a version number: "releases before X are legacy" is exactly the
/// hardcoded predicate DT-6 must not rely on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LifecycleSnapshot {
    pub(crate) state: CustomerState,
    pub(crate) owner: LifecycleOwnership,
    pub(crate) guarded_hooks: bool,
}

/// The admitted intent of one package action.
///
/// Pure: it names what the caller may do and what identity it is bound to. A
/// later effect owner either matches this plan exactly or refuses as changed —
/// the same shape as [`super::plan::SetupPlan`]. Effects are delegated to the
/// existing release selection owner.
///
/// Every admitted action retains payload, daemon and origins (PRD row for an
/// active package upgrade/remove). Nothing here authorizes replacement of live
/// backing, and nothing here is an effect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PackagePlan {
    pub(crate) action: PackageAction,
    /// The release identity this action is bound to: the version being
    /// replaced on upgrade, the version being removed on remove, and — because
    /// a fresh install has no predecessor — the version being installed. A
    /// later effect refuses if it changed.
    pub(crate) old_version: String,
    /// The state admission observed, so an effect re-checks the same fact.
    pub(crate) observed_state: CustomerState,
}

/// Admit one package action against an observed owned lifecycle.
///
/// DT-6: ordinary upgrade/removal requires an already quiescent owned
/// lifecycle and a compatible, guarded old package. Unsafe legacy hooks get a
/// separately attended, exact-version migration; automatic upgrade from those
/// versions is unsupported, and this function refuses them rather than
/// guessing.
///
/// This is a pure decision. It takes no lock, opens no file and performs no
/// effect. The same admission against the same observation yields the same
/// plan, so the once-guard belongs to the effect, where it can survive hook
/// exit.
pub(crate) fn admit_package(
    action: PackageAction,
    snapshot: &LifecycleSnapshot,
    old: &ReleaseIdentity,
) -> Result<PackagePlan, CustomerError> {
    // A lifecycle whose owner is not this product is not a surface a package
    // may mutate. DT-4: unknown owner, process or device observations fail
    // closed before anything else is considered.
    match snapshot.owner {
        LifecycleOwnership::Owned => {}
        LifecycleOwnership::Foreign => {
            return Err(identity_changed(
                "the observed lifecycle is held by a foreign or ambiguous owner; package admission fails closed",
            ));
        }
        LifecycleOwnership::Unobserved => {
            return Err(observation_unavailable(
                "the lifecycle owner could not be observed; package admission never treats unknown as clean",
            ));
        }
    }

    // An uncertain effect is a recovery outcome, not a clean slate: a package
    // must not replace payload under a state the product cannot account for.
    if snapshot.state == CustomerState::RecoveryRequired {
        return Err(CustomerError {
            reason: CustomerReason::RecoveryRequired,
            detail:
                "the lifecycle requires attended reconciliation; package admission does not clear its own record"
                    .to_string(),
            state: CustomerState::RecoveryRequired,
        });
    }
    if snapshot.state == CustomerState::UnknownBlocked {
        return Err(observation_unavailable(
            "the lifecycle state could not be observed; package admission never treats unknown as clean",
        ));
    }

    // A serving product refuses. The PRD row for an active package
    // upgrade/remove is exit 4 / active_session and the existing down/recovery
    // action; payload, daemon and origins are retained.
    if snapshot.state.is_serving() {
        return Err(CustomerError {
            reason: CustomerReason::ActiveSession,
            detail: format!(
                "the lifecycle is serving ({}); an active session refuses package admission",
                snapshot.state.as_str()
            ),
            state: CustomerState::InstalledOff,
        });
    }

    match (action, snapshot.state) {
        (PackageAction::Install, CustomerState::Absent) => {}
        (PackageAction::Install, _) => {
            return Err(identity_changed(
                "fresh installation requires an observed absent installation; reclassify the package action",
            ));
        }
        (_, CustomerState::Absent) => {
            return Err(identity_changed(
                "upgrade or removal requires an observed installed release; reclassify the package action",
            ));
        }
        _ => {}
    }

    // DT-6: upgrade and removal act on an old package whose hooks may run
    // before the new guard exists. A package without guarded hooks needs the
    // attended exact-version migration; automatic upgrade from it is
    // unsupported. A fresh install runs no old hooks, so the observation does
    // not apply to it.
    if action != PackageAction::Install && !snapshot.guarded_hooks {
        return Err(unsupported(format!(
            "the installed package's hooks carry no transition guard; automatic {} from it is unsupported and needs attended exact-version migration",
            action.as_str()
        )));
    }

    Ok(PackagePlan {
        action,
        old_version: old.version.clone(),
        observed_state: snapshot.state,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn package_action_must_match_observed_installation_presence() {
        let release = ReleaseIdentity::new("0.15.0");
        let installed = LifecycleSnapshot {
            state: CustomerState::InstalledOff,
            owner: LifecycleOwnership::Owned,
            guarded_hooks: false,
        };
        for state in [
            CustomerState::InstalledOff,
            CustomerState::PreparedOff,
            CustomerState::Ready,
        ] {
            let present = LifecycleSnapshot { state, ..installed };
            assert!(admit_package(PackageAction::Install, &present, &release).is_err());
        }
        let absent = LifecycleSnapshot {
            state: CustomerState::Absent,
            ..installed
        };
        assert!(admit_package(PackageAction::Install, &absent, &release).is_ok());
        let guarded_absent = LifecycleSnapshot {
            guarded_hooks: true,
            ..absent
        };
        for action in [PackageAction::Upgrade, PackageAction::Remove] {
            assert!(admit_package(action, &guarded_absent, &release).is_err());
        }
    }

    /// Clean vs active/unknown package: an active or unobservable lifecycle
    /// refuses, and a clean owned lifecycle is admitted exactly once — the
    /// same observation yields the same plan, so a second call cannot mint a
    /// second effect.
    #[test]
    fn package_refuses_active_and_accepts_clean_owner() {
        let old = ReleaseIdentity::new("0.14.0");
        let clean = LifecycleSnapshot {
            state: CustomerState::InstalledOff,
            owner: LifecycleOwnership::Owned,
            guarded_hooks: true,
        };

        // The legitimate path: one clean owned upgrade is admitted and names
        // the identity it is bound to.
        let plan = admit_package(PackageAction::Upgrade, &clean, &old)
            .expect("a clean owned lifecycle admits one upgrade");
        assert_eq!(plan.action, PackageAction::Upgrade);
        assert_eq!(plan.old_version, "0.14.0");
        assert_eq!(plan.observed_state, CustomerState::InstalledOff);

        // Exactly once at this layer: the decision is pure, so the same
        // observation cannot produce a different plan on a second call.
        let again = admit_package(PackageAction::Upgrade, &clean, &old)
            .expect("a repeated identical admission is the same decision");
        assert_eq!(
            plan, again,
            "admission is not deterministic for one observation"
        );

        // Active refuses as active_session (exit 4), the PRD row for an active
        // package upgrade/remove.
        for serving in [CustomerState::Active, CustomerState::Draining] {
            let snapshot = LifecycleSnapshot {
                state: serving,
                owner: LifecycleOwnership::Owned,
                guarded_hooks: true,
            };
            let error = admit_package(PackageAction::Remove, &snapshot, &old)
                .expect_err("a serving lifecycle must refuse package admission");
            assert_eq!(error.reason, CustomerReason::ActiveSession);
            assert_eq!(error.exit_code(), 4);
            assert_eq!(error.state, CustomerState::InstalledOff);
        }

        // Unknown refuses: a missing observation and an uncertain effect are
        // different outcomes and neither is clean.
        let unobserved = LifecycleSnapshot {
            state: CustomerState::UnknownBlocked,
            owner: LifecycleOwnership::Unobserved,
            guarded_hooks: true,
        };
        let error = admit_package(PackageAction::Upgrade, &unobserved, &old)
            .expect_err("an unobserved owner must refuse");
        assert_eq!(error.reason, CustomerReason::ObservationUnavailable);
        assert_eq!(error.exit_code(), 6);
        assert_eq!(error.state, CustomerState::UnknownBlocked);

        let foreign = LifecycleSnapshot {
            state: CustomerState::InstalledOff,
            owner: LifecycleOwnership::Foreign,
            guarded_hooks: true,
        };
        let error = admit_package(PackageAction::Upgrade, &foreign, &old)
            .expect_err("a foreign owner must refuse");
        assert_eq!(error.reason, CustomerReason::IdentityChanged);
        assert_eq!(error.exit_code(), 4);

        let recovering = LifecycleSnapshot {
            state: CustomerState::RecoveryRequired,
            owner: LifecycleOwnership::Owned,
            guarded_hooks: true,
        };
        let error = admit_package(PackageAction::Upgrade, &recovering, &old)
            .expect_err("a lifecycle needing reconciliation must refuse");
        assert_eq!(error.reason, CustomerReason::RecoveryRequired);
        assert_eq!(error.exit_code(), 5);
        assert_eq!(error.state, CustomerState::RecoveryRequired);

        let unknown_state = LifecycleSnapshot {
            state: CustomerState::UnknownBlocked,
            owner: LifecycleOwnership::Owned,
            guarded_hooks: true,
        };
        let error = admit_package(PackageAction::Upgrade, &unknown_state, &old)
            .expect_err("an unknown state must refuse");
        assert_eq!(error.reason, CustomerReason::ObservationUnavailable);
        assert_eq!(error.exit_code(), 6);
    }

    /// DT-6: an old package whose hooks carry no transition guard is not a
    /// compatible upgrade target. Automatic upgrade from it is unsupported,
    /// and a fresh install — which runs no old hooks — is unaffected.
    #[test]
    fn unguarded_old_package_refuses_automatic_upgrade_and_remove() {
        let old = ReleaseIdentity::new("0.9.0");
        let unguarded = LifecycleSnapshot {
            state: CustomerState::InstalledOff,
            owner: LifecycleOwnership::Owned,
            guarded_hooks: false,
        };
        for action in [PackageAction::Upgrade, PackageAction::Remove] {
            let error = admit_package(action, &unguarded, &old).expect_err(
                "an unguarded old package must not be upgraded or removed automatically",
            );
            assert_eq!(error.reason, CustomerReason::UnsupportedSurface);
            assert_eq!(error.exit_code(), 2);
            assert_eq!(error.state, CustomerState::InstalledOff);
        }
        // A fresh install runs no old hooks.
        let absent = LifecycleSnapshot {
            state: CustomerState::Absent,
            ..unguarded
        };
        let plan = admit_package(PackageAction::Install, &absent, &old)
            .expect("a fresh install admits against a clean owned lifecycle");
        assert_eq!(plan.action, PackageAction::Install);
    }
}
