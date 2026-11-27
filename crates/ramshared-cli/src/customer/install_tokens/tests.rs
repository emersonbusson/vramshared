// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/install_tokens/tests.rs
#![allow(clippy::unwrap_used)]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt, symlink};

use super::*;

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct FakeBootHost {
    state: RefCell<BTreeMap<String, bool>>,
    fail_enable: Option<String>,
    fail_disable: Option<String>,
    fail_observe: Option<String>,
}

impl FakeBootHost {
    fn all_disabled() -> Self {
        let mut state = BTreeMap::new();
        for unit in [CASCADE_UNIT, SUPERVISOR_UNIT, HOST_GATE_UNIT, HEALTH_UNIT] {
            state.insert(unit.to_owned(), false);
        }
        Self {
            state: RefCell::new(state),
            ..Self::default()
        }
    }
}

impl BootPolicyHost for FakeBootHost {
    fn enable_unit(&self, unit: &str) -> Result<(), String> {
        if self.fail_enable.as_deref() == Some(unit) {
            return Err(format!("synthetic enable failure for {unit}"));
        }
        self.state.borrow_mut().insert(unit.to_owned(), true);
        Ok(())
    }

    fn disable_unit(&self, unit: &str) -> Result<(), String> {
        if self.fail_disable.as_deref() == Some(unit) {
            return Err(format!("synthetic disable failure for {unit}"));
        }
        self.state.borrow_mut().insert(unit.to_owned(), false);
        Ok(())
    }

    fn unit_is_enabled(&self, unit: &str) -> Result<bool, String> {
        if self.fail_observe.as_deref() == Some(unit) {
            return Err(format!("synthetic observe failure for {unit}"));
        }
        Ok(*self.state.borrow().get(unit).unwrap_or(&false))
    }
}

#[test]
fn recovery_reuses_only_exact_approval_and_enrollment_tokens() {
    use std::os::unix::fs::MetadataExt;

    let root =
        std::env::temp_dir().join(format!("ramshared-token-recovery-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let _fixture = Fixture(root.clone());
    let release = "v1.2.3";
    let activation = root.join("activate-v1.2.3.token");
    let ack = root.join("wsl2-nbd-ack-v1.2.3.token");
    let host = FakeBootHost::all_disabled();

    assert!(recover_activation_approval(&root, "../bad", Posture::Swap).is_err());
    assert!(recover_enrollment(&host, &root, "../bad").is_err());
    recover_activation_approval(&root, release, Posture::Writeback).unwrap();
    let identity = std::fs::metadata(&activation).unwrap().ino();
    recover_activation_approval(&root, release, Posture::Writeback).unwrap();
    assert_eq!(std::fs::metadata(&activation).unwrap().ino(), identity);
    assert!(recover_activation_approval(&root, release, Posture::Swap).is_err());
    assert_eq!(std::fs::metadata(&activation).unwrap().ino(), identity);

    recover_enrollment(&host, &root, release).unwrap();
    let identity = std::fs::metadata(&ack).unwrap().ino();
    recover_enrollment(&host, &root, release).unwrap();
    assert_eq!(std::fs::metadata(&ack).unwrap().ino(), identity);
    assert!(host.unit_is_enabled(HEALTH_UNIT).unwrap());
    std::fs::remove_file(&ack).unwrap();
    std::os::unix::fs::symlink(&activation, &ack).unwrap();
    assert!(recover_enrollment(&host, &root, release).is_err());
    assert!(std::fs::symlink_metadata(&ack).unwrap().is_symlink());

    let failed = FakeBootHost {
        fail_enable: Some(CASCADE_UNIT.to_owned()),
        ..FakeBootHost::all_disabled()
    };
    assert!(recover_enrollment(&failed, &root, "v2.0.0").is_err());
    assert!(!root.join("wsl2-nbd-ack-v2.0.0.token").exists());
}

#[test]
fn recovered_boot_enable_failure_preserves_existing_acknowledgment() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let root = std::env::temp_dir().join(format!(
        "ramshared-enrollment-custody-{}",
        std::process::id()
    ));
    std::fs::create_dir(&root).unwrap();
    let _fixture = Fixture(root.clone());
    let release = "v1.2.3";
    let token = root.join("wsl2-nbd-ack-v1.2.3.token");
    let body = acknowledgment_body(release);
    std::fs::write(&token, &body).unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o400)).unwrap();
    let before = std::fs::metadata(&token).unwrap();

    let mut host = FakeBootHost {
        fail_enable: Some(HEALTH_UNIT.to_owned()),
        ..FakeBootHost::all_disabled()
    };
    assert!(recover_enrollment(&host, &root, release).is_err());
    assert!(host.state.borrow().values().all(|&enabled| !enabled));
    assert_eq!(std::fs::metadata(&token).unwrap().ino(), before.ino());
    assert_eq!(std::fs::read_to_string(&token).unwrap(), body);

    host.fail_enable = None;
    host.fail_observe = Some(HEALTH_UNIT.to_owned());
    let error = recover_enrollment(&host, &root, release).unwrap_err();
    assert!(error.detail.contains("synthetic observe failure"));
    assert!(host.state.borrow().values().all(|&enabled| !enabled));
    assert_eq!(std::fs::metadata(&token).unwrap().ino(), before.ino());

    host.fail_observe = None;
    recover_enrollment(&host, &root, release).unwrap();
    assert!(host.state.borrow().values().all(|&enabled| enabled));
    let after = std::fs::metadata(&token).unwrap();
    assert_eq!((after.dev(), after.ino()), (before.dev(), before.ino()));
    assert_eq!(after.mode() & 0o7777, 0o400);
    assert_eq!(std::fs::read_to_string(&token).unwrap(), body);
}

#[test]
fn incomplete_boot_rollback_keeps_new_acknowledgment_for_recovery() {
    let root = std::env::temp_dir().join(format!(
        "ramshared-enrollment-incomplete-{}",
        std::process::id()
    ));
    std::fs::create_dir(&root).unwrap();
    let _fixture = Fixture(root.clone());
    let release = "v1.2.3";
    let token = root.join("wsl2-nbd-ack-v1.2.3.token");
    let mut host = FakeBootHost {
        fail_enable: Some(CASCADE_UNIT.to_owned()),
        fail_disable: Some(HEALTH_UNIT.to_owned()),
        ..FakeBootHost::all_disabled()
    };

    let error = recover_enrollment(&host, &root, release).unwrap_err();
    assert!(error.detail.contains("ENROLLMENT_RESTORE_FAILED"));
    assert_eq!(
        std::fs::read_to_string(&token).unwrap(),
        acknowledgment_body(release)
    );

    host.fail_enable = None;
    host.fail_disable = None;
    recover_enrollment(&host, &root, release).unwrap();
    assert!(host.state.borrow().values().all(|&enabled| enabled));
}

#[test]
fn approval_token_publishes_mode_0400_refuses_existing_and_rolls_back_safely() {
    let base = std::env::temp_dir().join(format!("ramshared-tokens-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let _fix = Fixture(base.clone());
    let approvals = base.join("approvals");

    assert!(publish_activation_approval(&approvals, "../bad", Posture::Writeback).is_err());

    let mut custody =
        publish_activation_approval(&approvals, "v1.2.3", Posture::Writeback).unwrap();
    let token = approvals.join("activate-v1.2.3.token");
    assert_eq!(
        std::fs::read_to_string(&token).unwrap(),
        "activate:v1.2.3:posture=writeback\n"
    );
    assert_eq!(std::fs::metadata(&token).unwrap().mode() & 0o777, 0o400);

    // Existing token is never overwritten.
    assert!(publish_activation_approval(&approvals, "v1.2.3", Posture::Swap).is_err());

    // Foreign replacement blocks rollback rather than removing foreign evidence.
    let tmp = base.join("foreign");
    std::fs::write(&tmp, b"foreign\n").unwrap();
    std::fs::rename(&tmp, &token).unwrap();
    assert!(custody.rollback().is_err());
    assert_eq!(std::fs::read_to_string(&token).unwrap(), "foreign\n");

    std::fs::remove_file(&token).unwrap();
    let mut custody2 = publish_activation_approval(&approvals, "v1.2.4", Posture::Swap).unwrap();
    assert!(custody2.rollback().is_ok());
    assert!(!approvals.join("activate-v1.2.4.token").exists());
}

#[test]
fn cleanup_removes_only_owned_prior_tokens_never_foreign_or_links() {
    let base = std::env::temp_dir().join(format!("ramshared-token-clean-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let approvals = base.join("approvals");
    std::fs::create_dir_all(&approvals).unwrap();
    let _fix = Fixture(base.clone());

    let _cur = publish_activation_approval(&approvals, "v2.0.0", Posture::Writeback).unwrap();
    let prior = approvals.join("activate-v1.0.0.token");
    let foreign = approvals.join("activate-foreign.token");
    let link = approvals.join("activate-link.token");
    std::fs::write(&prior, b"activate:v1.0.0:posture=writeback\n").unwrap();
    std::fs::write(&foreign, b"foreign user data\n").unwrap();
    symlink(&foreign, &link).unwrap();

    cleanup_owned_prior_approvals(&approvals, "v2.0.0").unwrap();
    assert!(!prior.exists());
    assert!(foreign.is_file());
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(approvals.join("activate-v2.0.0.token").is_file());
}

#[test]
fn enrollment_writes_ack_enables_units_and_rolls_back_on_refusal() {
    let base = std::env::temp_dir().join(format!("ramshared-enroll-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let approvals = base.join("approvals");
    std::fs::create_dir_all(&approvals).unwrap();
    let _fix = Fixture(base.clone());

    let prior_ack = approvals.join("wsl2-nbd-ack-v0.9.0.token");
    std::fs::write(&prior_ack, b"accept-wsl2-nbd:v0.9.0\n").unwrap();
    let foreign_ack = approvals.join("wsl2-nbd-ack-foreign.token");
    std::fs::write(&foreign_ack, b"keep me\n").unwrap();

    // Conflict with an existing ack for the target release warns non-fatally without overwriting.
    let target_ack = approvals.join("wsl2-nbd-ack-v1.2.3.token");
    std::fs::write(&target_ack, b"foreign ack\n").unwrap();
    let host_ok = FakeBootHost::all_disabled();
    let conflict = apply_default_enrollment(&host_ok, &approvals, "v1.2.3").unwrap();
    assert_eq!(
        conflict,
        EnrollmentOutcome::Warned {
            warning: "enrollment-acknowledgment"
        }
    );
    assert_eq!(
        std::fs::read_to_string(&target_ack).unwrap(),
        "foreign ack\n"
    );
    std::fs::remove_file(&target_ack).unwrap();

    // Boot-policy refusal rolls back units & target ack while preserving prior ack.
    let mut host_fail = FakeBootHost::all_disabled();
    host_fail.fail_enable = Some(HEALTH_UNIT.to_owned());
    let warned = apply_default_enrollment(&host_fail, &approvals, "v1.2.3").unwrap();
    assert_eq!(
        warned,
        EnrollmentOutcome::Warned {
            warning: "enrollment-boot-policy"
        }
    );
    assert!(!target_ack.exists());
    assert!(prior_ack.is_file());
    assert!(!*host_fail.state.borrow().get(CASCADE_UNIT).unwrap());

    // Success writes mode 0400 ack, enables all 4 units, and cleans only owned prior acks.
    let ok = apply_default_enrollment(&host_ok, &approvals, "v1.2.3").unwrap();
    assert_eq!(ok, EnrollmentOutcome::Enrolled { warnings: vec![] });
    assert_eq!(
        std::fs::metadata(&target_ack).unwrap().mode() & 0o777,
        0o400
    );
    assert_eq!(
        std::fs::read_to_string(&target_ack).unwrap(),
        "accept-wsl2-nbd:v1.2.3\n"
    );
    assert!(!prior_ack.exists());
    assert!(foreign_ack.is_file());
    for unit in [CASCADE_UNIT, SUPERVISOR_UNIT, HOST_GATE_UNIT, HEALTH_UNIT] {
        assert!(
            *host_ok.state.borrow().get(unit).unwrap(),
            "{unit} not enabled"
        );
    }

    // Invalid release segment & prior_health=true preservation on rollback.
    assert!(publish_activation_approval(&approvals, "../bad", Posture::Writeback).is_err());
    assert!(apply_default_enrollment(&host_ok, &approvals, "../bad").is_err());
    assert!(cleanup_owned_prior_approvals(&base.join("absent-approvals"), "v1.2.3").is_ok());

    let mut custody = publish_activation_approval(&approvals, "v9.9.9", Posture::Swap).unwrap();
    let foreign_replacement = approvals.join("activate-v9.9.9.token");
    let foreign_tmp = approvals.join("foreign-tmp.token");
    std::fs::write(&foreign_tmp, b"foreign replacement").unwrap();
    std::fs::rename(&foreign_tmp, &foreign_replacement).unwrap();
    assert!(custody.rollback().is_err());

    let host_health_on = FakeBootHost::all_disabled();
    host_health_on
        .state
        .borrow_mut()
        .insert(HEALTH_UNIT.to_owned(), true);
    let mut host_rb = FakeBootHost::all_disabled();
    host_rb
        .state
        .borrow_mut()
        .insert(HEALTH_UNIT.to_owned(), true);
    host_rb.fail_enable = Some(CASCADE_UNIT.to_owned());
    let warned2 = apply_default_enrollment(&host_rb, &approvals, "v8.0.0").unwrap();
    assert_eq!(
        warned2,
        EnrollmentOutcome::Warned {
            warning: "enrollment-boot-policy"
        }
    );
    assert!(*host_rb.state.borrow().get(HEALTH_UNIT).unwrap());

    // Symlink approval dir refuses cleanly.
    let link_approvals = base.join("link-approvals");
    symlink(&approvals, &link_approvals).unwrap();
    assert!(cleanup_owned_prior_approvals(&link_approvals, "v1.2.3").is_err());

    // fail_observe & fail_disable rollback & missing dir on token custody rollback.
    let mut obs_fail = FakeBootHost::all_disabled();
    obs_fail.fail_observe = Some(HEALTH_UNIT.into());
    assert!(apply_default_enrollment(&obs_fail, &approvals, "v8.1.0").is_err());

    let mut rb_fail = FakeBootHost::all_disabled();
    rb_fail.fail_enable = Some(SUPERVISOR_UNIT.into());
    rb_fail.fail_disable = Some(HOST_GATE_UNIT.into());
    assert!(apply_default_enrollment(&rb_fail, &approvals, "v8.2.0").is_err());

    let gone = base.join("gone-approvals");
    let mut custody_gone =
        publish_activation_approval(&gone, "v8.3.0", Posture::Writeback).unwrap();
    std::fs::remove_dir_all(&gone).unwrap();
    assert!(custody_gone.rollback().is_err());
}
