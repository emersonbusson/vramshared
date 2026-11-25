// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/boot_policy_effect.rs
//! The boot-policy effect owner (ITEM-6).
//!
//! [`crate::customer::service`] owns the **decision** and never calls
//! `systemctl`. DT-5 / DT-9 leave the canonical sealed unit and its lifecycle
//! with [`crate::cascade::boot`] and the existing activation owner, so this
//! module is the narrow enablement effect that consumes a decided
//! [`BootPolicyPlan`] and changes only what a future boot may do.
//!
//! Enablement **is** the recorded policy. A future boot consults the unit's
//! enablement state; [`crate::cascade::boot`] still re-checks current
//! capabilities and safety on every startup (RF-6), so this never pins a
//! readiness snapshot and never creates a second record that could drift from
//! the unit. Activity is deliberately not policy: a running cascade under a
//! disabled unit is an operator's live tier, not an opt-in.
//!
//! Contracts this owner holds:
//!
//! - **Never touch a live tier.** [`BootPolicy::Off`] calls `disable`, never
//!   `stop`. A running cascade keeps serving until something else drains it.
//! - **The boot set moves together.** `ramshared-cascade.service` has
//!   `Requires=ramshared-host-gate.service`, so a half-enable is a state that
//!   every startup refuses. The effect rolls an earlier verb back when a later
//!   one fails.
//! - **The observer is in the same set.** The cascade envelope reports the
//!   product as blocked while the product is active and the supervisor unit is
//!   not publishing current control state. A future boot that arms the cascade
//!   without it arms a product that will not report itself healthy, so the
//!   consent that arms the product also arms its observer.
//! - **Idempotent.** Enabling an enabled set and disabling a disabled set are
//!   no-ops that report success.
//! - **A partial effect is never reported as success.** An uncertain half-state
//!   is [`CustomerReason::RecoveryRequired`] with the state retained for an
//!   attended reconciliation.

use crate::customer::service::{BootPolicy, BootPolicyPlan};
use crate::customer::{CustomerError, CustomerReason, CustomerState};

/// The cascade unit. Its enablement is what a future boot acts on.
pub(crate) const CASCADE_UNIT: &str = "ramshared-cascade.service";
/// Health monitoring must also be inactive before storage preparation.
pub(crate) const HEALTH_UNIT: &str = "ramshared-cascade-health.service";

/// The prerequisite the cascade unit names with `Requires=`.
pub(crate) const HOST_GATE_UNIT: &str = "ramshared-host-gate.service";

/// The control-plane observer. Not an optional extra: without a current sample
/// from it the product cannot honestly report itself healthy while active.
pub(crate) const SUPERVISOR_UNIT: &str = "ramshared-supervisor.service";

/// The owned boot set, in enable order: the cascade's prerequisite first, the
/// cascade itself last, and the observer in between so it is never the member
/// whose failure leaves the cascade enabled without its prerequisite.
const SET_ENABLE_ORDER: [&str; 3] = [HOST_GATE_UNIT, SUPERVISOR_UNIT, CASCADE_UNIT];

/// The same set in reverse: effect first, prerequisite last.
const SET_DISABLE_ORDER: [&str; 3] = [CASCADE_UNIT, SUPERVISOR_UNIT, HOST_GATE_UNIT];

/// Host operations the enablement effect needs.
///
/// Injected so a test never reaches the live systemd. The trait names exactly
/// the three observations the effect makes and nothing else — this is not a
/// general command runner. There is no `stop` verb because there is no
/// contract under which this owner may tear down a live tier.
pub(crate) trait BootPolicyHost {
    /// Make one unit start on a future boot. Idempotent.
    fn enable_unit(&self, unit: &str) -> Result<(), String>;
    /// Stop one unit starting on a future boot. Idempotent. Never stops a
    /// running unit.
    fn disable_unit(&self, unit: &str) -> Result<(), String>;
    /// Whether a future boot would start this unit. This is the policy
    /// observation; activity is a different fact and is not read here.
    fn unit_is_enabled(&self, unit: &str) -> Result<bool, String>;
}

/// What `systemctl is-enabled --quiet` said, read as policy.
///
/// The platform's own vocabulary is the contract here, not an assumption:
/// `0` is `enabled` (or `linked`), `1` is `disabled`, `2` is `static` and `3` is
/// `indirect`. Those four answer the question, and 1–3 all mean "a future boot
/// will not start this unit on its own", which is what the policy reads as off.
///
/// `4` is **not** an off answer. Observed on the sealed units and on a unit
/// name that does not exist, `systemctl is-enabled` prints `not-found` and
/// exits 4. A missing unit is a broken installation, not a recorded policy, so
/// it is `None` here and the caller refuses as `observation_unavailable`. The
/// same holds for a signal, a negative wait status and any code systemd has not
/// published: an unreadable policy is never guessed off.
///
/// This vocabulary lives here and not in the process boundary so the mapping
/// from a platform status to a policy is one covered decision, not two copies.
pub(crate) fn enabled_from_exit_code(code: Option<i32>) -> Option<bool> {
    match code {
        Some(0) => Some(true),
        Some(1) | Some(2) | Some(3) => Some(false),
        _ => None,
    }
}

/// Perform one decided boot policy.
///
/// `plan` comes from [`crate::customer::service::plan_boot_policy`]; this owner
/// does not re-decide. A refusal keeps the stable PRD reason so an operator or
/// a script can match it without parsing prose.
pub(crate) fn apply_boot_policy(
    plan: &BootPolicyPlan,
    host: &impl BootPolicyHost,
) -> Result<(), CustomerError> {
    match plan.policy {
        BootPolicy::On => enable_set(host),
        BootPolicy::Off => disable_set(host),
    }
}

/// Enable the boot set prerequisite-first, rolling back if a later verb fails.
fn enable_set(host: &impl BootPolicyHost) -> Result<(), CustomerError> {
    let mut applied: [&str; 3] = [""; 3];
    let mut applied_count = 0;
    for unit in SET_ENABLE_ORDER {
        match host.enable_unit(unit) {
            Ok(()) => {
                applied[applied_count] = unit;
                applied_count += 1;
            }
            Err(detail) => {
                rollback_enable(host, &applied[..applied_count]);
                return Err(effect_refusal(unit, "enable", &detail, applied_count > 0));
            }
        }
    }
    Ok(())
}

/// Undo a partial enable so a failed set never leaves half a control plane
/// armed. Each rollback verb is itself idempotent.
fn rollback_enable(host: &impl BootPolicyHost, applied: &[&str]) {
    for unit in applied.iter().rev() {
        let _ = host.disable_unit(unit);
    }
}

/// Disable the boot set effect-first. Never stops anything that is running.
fn disable_set(host: &impl BootPolicyHost) -> Result<(), CustomerError> {
    let mut applied: [&str; 3] = [""; 3];
    let mut applied_count = 0;
    for unit in SET_DISABLE_ORDER {
        match host.disable_unit(unit) {
            Ok(()) => {
                applied[applied_count] = unit;
                applied_count += 1;
            }
            Err(detail) => {
                rollback_disable(host, &applied[..applied_count]);
                return Err(effect_refusal(unit, "disable", &detail, applied_count > 0));
            }
        }
    }
    Ok(())
}

/// Undo a partial disable so a failed set never leaves the prerequisite
/// disabled under an enabled dependant.
fn rollback_disable(host: &impl BootPolicyHost, applied: &[&str]) {
    for unit in applied.iter().rev() {
        let _ = host.enable_unit(unit);
    }
}

/// Build the refusal for a failed verb.
///
/// A rollback was attempted whenever something had already been applied. The
/// outcome of that rollback is not invented: if a verb failed after an earlier
/// member had already changed, the set is in a state this owner did not intend,
/// and the caller is told so as [`CustomerReason::RecoveryRequired`] rather than
/// being handed a false "off" or "on".
fn effect_refusal(unit: &str, verb: &str, detail: &str, rollback_attempted: bool) -> CustomerError {
    if rollback_attempted {
        CustomerError {
            reason: CustomerReason::RecoveryRequired,
            detail: format!(
                "systemctl {verb} {unit} failed after an earlier unit had already changed; a rollback was attempted and its outcome is not assumed: {detail}"
            ),
            state: CustomerState::RecoveryRequired,
        }
    } else {
        CustomerError {
            reason: CustomerReason::UnsupportedSurface,
            detail: format!("systemctl {verb} {unit} failed: {detail}"),
            state: CustomerState::InstalledOff,
        }
    }
}

/// The three enablement bits the boot set records, always observed and never
/// inferred from the derived policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BootSetEnablement {
    /// Whether a future boot would start the cascade unit.
    pub(crate) cascade_enabled: bool,
    /// Whether a future boot would start the observer.
    pub(crate) supervisor_enabled: bool,
    /// Whether a future boot would start the prerequisite.
    pub(crate) host_gate_enabled: bool,
}

impl BootSetEnablement {
    /// The observed bits as `(cascade, supervisor, host_gate)`, for rendering.
    pub(crate) fn as_triple(self) -> (bool, bool, bool) {
        (
            self.cascade_enabled,
            self.supervisor_enabled,
            self.host_gate_enabled,
        )
    }

    /// Whether every member agrees on the same answer.
    fn agrees(self) -> bool {
        self.cascade_enabled == self.supervisor_enabled
            && self.supervisor_enabled == self.host_gate_enabled
    }
}

/// What the boot set's enablement currently records.
///
/// The set is read as one policy. When its members disagree the observation is
/// a **half-state**, which is reported as its own outcome rather than being
/// guessed into `On` or `Off`: a half-enable is exactly the state every
/// startup refuses, and an operator has to see it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BootPolicyObservation {
    /// Every member disabled: a future boot must not activate the cascade.
    Off,
    /// Every member enabled: a future boot may activate the cascade complete
    /// with its observer, and startup still re-checks current capabilities and
    /// safety (RF-6).
    On,
    /// The members disagree. The cascade's enablement is what a future boot
    /// acts on, so its value is carried; the set is not a recorded policy.
    Half(BootSetEnablement),
}

impl BootPolicyObservation {
    /// The recorded policy, or `None` when the set disagrees.
    pub(crate) fn policy(self) -> Option<BootPolicy> {
        match self {
            Self::Off => Some(BootPolicy::Off),
            Self::On => Some(BootPolicy::On),
            Self::Half(_) => None,
        }
    }

    /// Stable rendering for a status line. A disagreeing set is named `half`,
    /// never collapsed into `on` or `off`.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
            Self::Half(_) => "half",
        }
    }

    /// The enablement bits, always observed, never inferred from the derived
    /// policy.
    pub(crate) fn enablement(self) -> BootSetEnablement {
        match self {
            Self::Off => BootSetEnablement {
                cascade_enabled: false,
                supervisor_enabled: false,
                host_gate_enabled: false,
            },
            Self::On => BootSetEnablement {
                cascade_enabled: true,
                supervisor_enabled: true,
                host_gate_enabled: true,
            },
            Self::Half(bits) => bits,
        }
    }
}

/// Observe the recorded boot policy without changing anything.
///
/// A missing or unreadable enablement observation is
/// [`CustomerReason::ObservationUnavailable`] (exit 6). It is never read as
/// `off`: an unobserved policy is not a disabled one.
pub(crate) fn observe_boot_policy(
    host: &(impl BootPolicyHost + ?Sized),
) -> Result<BootPolicyObservation, CustomerError> {
    let bits = BootSetEnablement {
        cascade_enabled: host
            .unit_is_enabled(CASCADE_UNIT)
            .map_err(observation_refusal)?,
        supervisor_enabled: host
            .unit_is_enabled(SUPERVISOR_UNIT)
            .map_err(observation_refusal)?,
        host_gate_enabled: host
            .unit_is_enabled(HOST_GATE_UNIT)
            .map_err(observation_refusal)?,
    };
    if !bits.agrees() {
        return Ok(BootPolicyObservation::Half(bits));
    }
    Ok(if bits.cascade_enabled {
        BootPolicyObservation::On
    } else {
        BootPolicyObservation::Off
    })
}

/// A failed observation is exit 6 and leaves the product state unknown.
fn observation_refusal(detail: String) -> CustomerError {
    CustomerError {
        reason: CustomerReason::ObservationUnavailable,
        detail: format!("cannot observe the sealed boot units: {detail}"),
        state: CustomerState::UnknownBlocked,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::customer::service::BootPolicyPlan;
    use crate::readiness::Decision;
    use std::cell::RefCell;

    #[derive(Debug, Default)]
    struct FakeHost {
        enabled: RefCell<Vec<String>>,
        disabled: RefCell<Vec<String>>,
        enablement: RefCell<Vec<(String, bool)>>,
        fail_enable: RefCell<Option<String>>,
        fail_disable: RefCell<Option<String>>,
        fail_observe: RefCell<Option<String>>,
    }

    impl FakeHost {
        fn failing_enable(unit: &str) -> Self {
            let host = Self::default();
            *host.fail_enable.borrow_mut() = Some(unit.to_string());
            host
        }

        fn failing_disable(unit: &str) -> Self {
            let host = Self::default();
            *host.fail_disable.borrow_mut() = Some(unit.to_string());
            host
        }

        fn with_enablement(cascade: bool, supervisor: bool, host_gate: bool) -> Self {
            let host = Self::default();
            host.enablement
                .borrow_mut()
                .push((CASCADE_UNIT.to_string(), cascade));
            host.enablement
                .borrow_mut()
                .push((SUPERVISOR_UNIT.to_string(), supervisor));
            host.enablement
                .borrow_mut()
                .push((HOST_GATE_UNIT.to_string(), host_gate));
            host
        }

        fn failing_observe(unit: &str) -> Self {
            let host = Self::default();
            *host.fail_observe.borrow_mut() = Some(unit.to_string());
            host
        }

        fn calls(&self) -> (Vec<String>, Vec<String>) {
            (
                self.enabled.borrow().clone(),
                self.disabled.borrow().clone(),
            )
        }
    }

    impl BootPolicyHost for FakeHost {
        fn enable_unit(&self, unit: &str) -> Result<(), String> {
            if self.fail_enable.borrow().as_deref() == Some(unit) {
                return Err(format!("synthetic enable failure on {unit}"));
            }
            self.enabled.borrow_mut().push(unit.to_string());
            Ok(())
        }

        fn disable_unit(&self, unit: &str) -> Result<(), String> {
            if self.fail_disable.borrow().as_deref() == Some(unit) {
                return Err(format!("synthetic disable failure on {unit}"));
            }
            self.disabled.borrow_mut().push(unit.to_string());
            Ok(())
        }

        fn unit_is_enabled(&self, unit: &str) -> Result<bool, String> {
            if self.fail_observe.borrow().as_deref() == Some(unit) {
                return Err(format!("synthetic observation failure on {unit}"));
            }
            Ok(self
                .enablement
                .borrow()
                .iter()
                .find(|(name, _)| name == unit)
                .map(|(_, value)| *value)
                .unwrap_or(false))
        }
    }

    fn plan(policy: BootPolicy) -> BootPolicyPlan {
        BootPolicyPlan {
            policy,
            release_version: "v1".into(),
            grant_id: None,
            readiness: Decision::Ready,
            live_tier_untouched: true,
        }
    }

    #[test]
    fn enable_moves_the_set_prerequisite_first_and_disable_moves_it_back() {
        let host = FakeHost::default();
        apply_boot_policy(&plan(BootPolicy::On), &host).expect("enable the boot set");
        let (enabled, disabled) = host.calls();
        assert_eq!(enabled, vec![HOST_GATE_UNIT, SUPERVISOR_UNIT, CASCADE_UNIT]);
        assert!(
            disabled.is_empty(),
            "an enable must not disable anything: {disabled:?}"
        );

        let host = FakeHost::default();
        apply_boot_policy(&plan(BootPolicy::Off), &host).expect("disable the boot set");
        let (enabled, disabled) = host.calls();
        assert_eq!(
            disabled,
            vec![CASCADE_UNIT, SUPERVISOR_UNIT, HOST_GATE_UNIT]
        );
        assert!(
            enabled.is_empty(),
            "a disable must not enable anything: {enabled:?}"
        );
    }

    #[test]
    fn the_effect_has_no_verb_that_can_tear_down_a_live_tier() {
        // DT-5: "Disable changes future activation only and never tears down a
        // live tier." The proof is structural as well as behavioural: the host
        // trait this effect is written against has no stop verb at all, so a
        // running cascade cannot be reached from a policy change.
        let host = FakeHost::default();
        apply_boot_policy(&plan(BootPolicy::Off), &host).expect("disable the policy");
        let (enabled, disabled) = host.calls();
        assert_eq!(
            disabled,
            vec![CASCADE_UNIT, SUPERVISOR_UNIT, HOST_GATE_UNIT]
        );
        assert!(
            enabled.is_empty(),
            "a disable under a live tier must not enable anything: {enabled:?}"
        );
    }

    #[test]
    fn a_failed_last_enable_rolls_the_earlier_back_and_reports_recovery() {
        let host = FakeHost::failing_enable(CASCADE_UNIT);
        let error = apply_boot_policy(&plan(BootPolicy::On), &host)
            .expect_err("a half-enabled set must not report success");
        assert!(
            matches!(error.reason, CustomerReason::RecoveryRequired),
            "a rollback after a partial effect is uncertain, not a clean refusal: {error}"
        );
        assert!(
            matches!(error.state, CustomerState::RecoveryRequired),
            "the state after an uncertain effect is recovery: {error}"
        );
        assert!(
            error.to_string().contains(CASCADE_UNIT),
            "the refusal must name the unit that failed: {error}"
        );
        let (enabled, disabled) = host.calls();
        assert_eq!(enabled, vec![HOST_GATE_UNIT, SUPERVISOR_UNIT]);
        assert_eq!(
            disabled,
            vec![SUPERVISOR_UNIT, HOST_GATE_UNIT],
            "the already-enabled members must be rolled back, effect first"
        );
    }

    #[test]
    fn a_failed_middle_enable_rolls_the_prerequisite_back_and_reports_recovery() {
        let host = FakeHost::failing_enable(SUPERVISOR_UNIT);
        let error = apply_boot_policy(&plan(BootPolicy::On), &host)
            .expect_err("a set missing its observer must not report success");
        assert!(
            matches!(error.reason, CustomerReason::RecoveryRequired),
            "the prerequisite already changed, so the outcome is uncertain: {error}"
        );
        let (enabled, disabled) = host.calls();
        assert_eq!(enabled, vec![HOST_GATE_UNIT]);
        assert_eq!(
            disabled,
            vec![HOST_GATE_UNIT],
            "the prerequisite must not stay enabled under a set that never completed"
        );
    }

    #[test]
    fn a_failed_first_enable_is_a_clean_refusal_with_no_rollback() {
        let host = FakeHost::failing_enable(HOST_GATE_UNIT);
        let error = apply_boot_policy(&plan(BootPolicy::On), &host)
            .expect_err("a refused prerequisite must not enable anything");
        assert!(
            matches!(error.reason, CustomerReason::UnsupportedSurface),
            "nothing was applied, so the refusal is clean: {error}"
        );
        assert!(
            matches!(error.state, CustomerState::InstalledOff),
            "a clean refusal leaves the policy off: {error}"
        );
        let (enabled, disabled) = host.calls();
        assert!(enabled.is_empty(), "nothing may be enabled: {enabled:?}");
        assert!(
            disabled.is_empty(),
            "with nothing applied there is nothing to roll back: {disabled:?}"
        );
    }

    #[test]
    fn a_failed_last_disable_rolls_the_earlier_back_and_reports_recovery() {
        let host = FakeHost::failing_disable(HOST_GATE_UNIT);
        let error = apply_boot_policy(&plan(BootPolicy::Off), &host)
            .expect_err("a half-disabled set must not report success");
        assert!(
            matches!(error.reason, CustomerReason::RecoveryRequired),
            "a rollback after a partial effect is uncertain: {error}"
        );
        let (enabled, disabled) = host.calls();
        assert_eq!(disabled, vec![CASCADE_UNIT, SUPERVISOR_UNIT]);
        assert_eq!(
            enabled,
            vec![SUPERVISOR_UNIT, CASCADE_UNIT],
            "the already-disabled members must be rolled back to enabled"
        );
    }

    #[test]
    fn the_effect_is_idempotent_in_both_directions() {
        // Enabling twice and disabling twice are the same verbs a single call
        // makes: the host treats them as idempotent, so the effect must not
        // accumulate state or refuse a repeat.
        let host = FakeHost::default();
        apply_boot_policy(&plan(BootPolicy::On), &host).expect("first enable");
        apply_boot_policy(&plan(BootPolicy::On), &host).expect("second enable");
        let (enabled, _) = host.calls();
        assert_eq!(
            enabled,
            vec![
                HOST_GATE_UNIT,
                SUPERVISOR_UNIT,
                CASCADE_UNIT,
                HOST_GATE_UNIT,
                SUPERVISOR_UNIT,
                CASCADE_UNIT
            ],
            "a repeat enable is the same set again, not a refusal"
        );

        let host = FakeHost::default();
        apply_boot_policy(&plan(BootPolicy::Off), &host).expect("first disable");
        apply_boot_policy(&plan(BootPolicy::Off), &host).expect("second disable");
        let (_, disabled) = host.calls();
        assert_eq!(
            disabled,
            vec![
                CASCADE_UNIT,
                SUPERVISOR_UNIT,
                HOST_GATE_UNIT,
                CASCADE_UNIT,
                SUPERVISOR_UNIT,
                HOST_GATE_UNIT
            ],
            "a repeat disable is the same set again, not a refusal"
        );
    }

    #[test]
    fn the_set_is_observed_as_one_policy_and_a_disagreement_is_half() {
        let host = FakeHost::with_enablement(true, true, true);
        assert_eq!(
            observe_boot_policy(&host).expect("observe"),
            BootPolicyObservation::On
        );
        assert_eq!(
            observe_boot_policy(&host).unwrap().policy(),
            Some(BootPolicy::On)
        );

        let host = FakeHost::with_enablement(false, false, false);
        assert_eq!(
            observe_boot_policy(&host).expect("observe"),
            BootPolicyObservation::Off
        );
        assert_eq!(
            observe_boot_policy(&host).unwrap().policy(),
            Some(BootPolicy::Off)
        );

        // Cascade and prerequisite on, observer off: the regression this set
        // exists to prevent. A future boot would arm a product that cannot
        // report itself healthy, so the observation is `half`, not `on`.
        let host = FakeHost::with_enablement(true, false, true);
        let half = observe_boot_policy(&host).expect("observe");
        assert_eq!(
            half,
            BootPolicyObservation::Half(BootSetEnablement {
                cascade_enabled: true,
                supervisor_enabled: false,
                host_gate_enabled: true,
            })
        );
        assert_eq!(
            half.policy(),
            None,
            "a disagreeing set is not a recorded policy"
        );
        assert_eq!(half.as_str(), "half");

        // Observer on alone is also half: the observer without the effect it
        // observes is not an `on`, and the prerequisite without the effect is
        // not an `off`.
        let host = FakeHost::with_enablement(false, true, false);
        let half = observe_boot_policy(&host).expect("observe");
        assert_eq!(
            half,
            BootPolicyObservation::Half(BootSetEnablement {
                cascade_enabled: false,
                supervisor_enabled: true,
                host_gate_enabled: false,
            })
        );
    }

    #[test]
    fn an_unobservable_policy_is_never_read_as_off() {
        let host = FakeHost::failing_observe(CASCADE_UNIT);
        let error =
            observe_boot_policy(&host).expect_err("a missing observation is not a disabled policy");
        assert!(
            matches!(error.reason, CustomerReason::ObservationUnavailable),
            "the refusal must be exit 6, not a guessed off: {error}"
        );
        assert!(
            matches!(error.state, CustomerState::UnknownBlocked),
            "an unobserved policy leaves the state unknown, not off: {error}"
        );
    }

    /// The `is-enabled` vocabulary is decided here, not in the spawn wrapper.
    /// Every code systemd publishes as an answer maps to a policy; a code that
    /// is not an answer is no observation at all.
    #[test]
    fn an_is_enabled_exit_code_is_read_as_policy_only_when_it_is_known() {
        // 0 = enabled / linked: a future boot starts this unit.
        assert_eq!(enabled_from_exit_code(Some(0)), Some(true));
        // 1 = disabled, 2 = static, 3 = indirect: each answers "not enabled",
        // which is exactly what this policy reads as off.
        for off_code in [1, 2, 3] {
            assert_eq!(
                enabled_from_exit_code(Some(off_code)),
                Some(false),
                "code {off_code} means a future boot will not start this unit"
            );
        }
        // 4 = not-found. A unit that is missing is a broken installation, not
        // a recorded off policy. Empirically `systemctl is-enabled --quiet`
        // prints `not-found` and exits 4 for a name with no unit file.
        assert_eq!(
            enabled_from_exit_code(Some(4)),
            None,
            "code 4 is `not-found`: an unreadable policy is never guessed off"
        );
        for unknown in [None, Some(5), Some(127), Some(-1)] {
            assert_eq!(
                enabled_from_exit_code(unknown),
                None,
                "{unknown:?} is not an observation and must never become a policy"
            );
        }
    }

    /// Every observation shape renders the same way it was observed. A half
    /// set is named `half`, never collapsed into `on` or `off`.
    #[test]
    fn every_observation_shape_renders_what_it_actually_observed() {
        let cases = [
            (
                BootPolicyObservation::Off,
                "off",
                Some(BootPolicy::Off),
                (false, false, false),
            ),
            (
                BootPolicyObservation::On,
                "on",
                Some(BootPolicy::On),
                (true, true, true),
            ),
            (
                BootPolicyObservation::Half(BootSetEnablement {
                    cascade_enabled: true,
                    supervisor_enabled: false,
                    host_gate_enabled: true,
                }),
                "half",
                None,
                (true, false, true),
            ),
        ];
        for (observation, text, policy, enablement) in cases {
            assert_eq!(observation.as_str(), text);
            assert_eq!(observation.policy(), policy);
            assert_eq!(observation.enablement().as_triple(), enablement);
        }
    }
}
