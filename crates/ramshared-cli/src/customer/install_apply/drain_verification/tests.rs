// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/install_apply/drain_verification/tests.rs
#![allow(clippy::unwrap_used)]

use std::os::unix::fs::symlink;

use super::*;

struct Fixture(PathBuf);

impl Fixture {
    fn create(scenario: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "ramshared-drain-verification-{scenario}-{}",
            std::process::id()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn absent_install() -> PriorInstallState {
    PriorInstallState {
        prior_root: None,
        prior_identity: None,
        action: crate::customer::transition::PackageAction::Install,
        old_release: "absent".into(),
        old_hash: "0".repeat(64),
        prev_evidence_bundle: None,
    }
}

#[test]
fn apply_requires_proved_absence_of_the_managed_lifecycle() {
    let fixture = Fixture::create("lifecycle");
    let selector = fixture.0.join("current");
    let prior = absent_install();

    let error = verify_observed_quiescence(
        &selector,
        &prior,
        || Err(recovery_refusal("units not quiesced")),
        || panic!("lifecycle observer must follow the unit gate"),
    )
    .unwrap_err();
    assert!(error.detail.contains("units not quiesced"));

    for safe_to_drain in [true, false] {
        let error = verify_observed_quiescence(
            &selector,
            &prior,
            || Ok(()),
            || Ok(Some((safe_to_drain, "fixture lifecycle".into()))),
        )
        .unwrap_err();
        assert_eq!(error.reason, CustomerReason::ActiveSession);
        assert_eq!(error.state, CustomerState::Active);
        assert!(error.detail.contains("MANAGED_LIFECYCLE_STILL_PRESENT"));
    }

    let error = verify_observed_quiescence(
        &selector,
        &prior,
        || Ok(()),
        || Err(CascadeError::Io("cannot read lifecycle witness".into())),
    )
    .unwrap_err();
    assert_eq!(error.reason, CustomerReason::ObservationUnavailable);
    assert_eq!(error.state, CustomerState::UnknownBlocked);
    assert!(error.detail.contains("cannot read lifecycle witness"));

    verify_observed_quiescence(&selector, &prior, || Ok(()), || Ok(None)).unwrap();
}

#[test]
fn apply_rechecks_selector_identity_and_seal_before_observing_lifecycle() {
    let fixture = Fixture::create("selector");
    let root = fixture.0.join("release");
    let selector = fixture.0.join("current");
    std::fs::create_dir(&root).unwrap();
    symlink(&root, &selector).unwrap();

    let mut prior = absent_install();
    for expected_identity in [
        None,
        resolve_prior_identity(&selector)
            .unwrap()
            .map(|location| location.identity),
    ] {
        prior.prior_identity = expected_identity;
        let error = verify_observed_quiescence(
            &selector,
            &prior,
            || Ok(()),
            || panic!("the changed selector must be rejected first"),
        )
        .unwrap_err();
        assert_eq!(error.reason, CustomerReason::IdentityChanged);
        assert!(error.detail.contains("PRIOR_SELECTOR_CHANGED_DURING_DRAIN"));
    }

    std::fs::remove_dir(&root).unwrap();
    let error = resolve_prior_identity(&selector).err().unwrap();
    assert!(error.detail.contains("SELECTOR_TARGET_INVALID"));
    std::fs::write(&root, b"not a containing directory").unwrap();
    let error = resolve_prior_identity(&selector).err().unwrap();
    assert!(error.detail.contains("PRIOR_RELEASE_INVALID"));
    let error = resolve_prior_identity(&root.join("current")).err().unwrap();
    assert!(error.detail.contains("SELECTOR_UNAVAILABLE"));
}

#[test]
#[ignore = "requires root and a current lifecycle witness; read-only native observation"]
fn native_quiescence_gate_respects_the_managed_lifecycle() {
    let fixture = Fixture::create("native");
    let mut env = InstallEnvironment::production();
    env.product_root = fixture.0.clone();
    let prior = absent_install();

    let before = crate::cascade::evaluate_live_down_plan();
    let result = verify_quiesced_install(&env, &prior, || Ok(()));
    let after = crate::cascade::evaluate_live_down_plan();
    match (before, after) {
        (Ok(None), Ok(None)) => result.unwrap(),
        (Ok(Some(_)), Ok(Some(_))) => {
            assert_eq!(result.unwrap_err().reason, CustomerReason::ActiveSession);
        }
        (Err(_), Err(_)) => {
            assert_eq!(
                result.unwrap_err().reason,
                CustomerReason::ObservationUnavailable
            );
        }
        _ => panic!("managed lifecycle changed during the read-only observation"),
    }
}
