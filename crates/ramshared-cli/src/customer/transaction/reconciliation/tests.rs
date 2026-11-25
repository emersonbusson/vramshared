// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/transaction/reconciliation/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::customer::setup_scope::SetupOperation;
use crate::customer::transition::acquire_transition;
use crate::readiness::Decision;
use std::cell::Cell;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    root: PathBuf,
    paths: TransitionPaths,
    plan: SetupPlan,
    approval: ApprovalBinding,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "ramshared-reconcile-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let paths = TransitionPaths::under(&root);
        fs::create_dir_all(&paths.lock_root).unwrap();
        fs::create_dir_all(paths.transactions_root()).unwrap();
        let plan = SetupPlan {
            operation: SetupOperation::StorageOnly {},
            release_version: "v0.15.0".into(),
            profile_schema_version: 1,
            profile_fingerprint: "fixture-profile".into(),
            report_decision: Decision::Blocked,
        };
        let approval = ApprovalBinding {
            approval_id: "original-approval".into(),
            plan_fingerprint: plan.fingerprint(),
            issued_unix: 100,
            expires_unix: 200,
        };
        Self {
            root,
            paths,
            plan,
            approval,
        }
    }
    fn guard(&self) -> CustomerTransitionGuard {
        acquire_transition(OperationOwner::Setup, &self.paths).unwrap()
    }
    fn pending(&self, guard: &CustomerTransitionGuard) {
        write_record(
            guard,
            &TransactionRecord {
                plan_fingerprint: self.plan.fingerprint(),
                approval_id: self.approval.approval_id.clone(),
                release_version: self.plan.release_version.clone(),
                phase: TransactionPhase::Applying,
                issued_unix: 150,
                applied_unix: None,
            },
        )
        .unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}
struct ReadBack {
    calls: Cell<u32>,
    count: u32,
}
impl SetupEffects for ReadBack {
    fn create_owned_origin(&mut self, _: &SetupPlan) -> Result<(), CustomerError> {
        panic!("reconciliation cannot create resources")
    }
    fn owned_origin_count(&self, _: &SetupPlan) -> Result<u32, CustomerError> {
        self.calls.set(self.calls.get() + 1);
        Ok(self.count)
    }
}

#[test]
fn setup_reconciliation_commits_verified_pending_without_creation() {
    let fixture = Fixture::new();
    let mut guard = fixture.guard();
    fixture.pending(&guard);
    let observation = ReadBack {
        calls: Cell::new(0),
        count: 1,
    };
    // Expiry prevents a new allocation, never read-back of this exact original attempt.
    let receipt = reconcile_setup(
        fixture.plan.clone(),
        fixture.approval.clone(),
        &mut guard,
        &observation,
        250,
    )
    .unwrap();
    assert_eq!(receipt.applied_unix, 250);
    assert_eq!(observation.calls.get(), 1);
    let record = read_record(&fixture.paths).unwrap().unwrap();
    assert_eq!(record.phase, TransactionPhase::Committed);
    assert_eq!(record.issued_unix, 150);
    let before = read_journal_body(&fixture.paths, record_file_name(), "test receipt").unwrap();
    assert_eq!(
        reconcile_setup(
            fixture.plan.clone(),
            fixture.approval.clone(),
            &mut guard,
            &observation,
            500
        )
        .unwrap(),
        receipt
    );
    assert_eq!(
        before,
        read_journal_body(&fixture.paths, record_file_name(), "test receipt").unwrap()
    );
    assert_eq!(observation.calls.get(), 2);
    drop(guard);
    let allowed = acquire_transition(OperationOwner::Up, &fixture.paths).unwrap();
    drop(allowed);
}

#[test]
fn setup_reconciliation_refuses_identity_clock_and_unproven_state() {
    let fixture = Fixture::new();
    let mut guard = fixture.guard();
    let proof = ReadBack {
        calls: Cell::new(0),
        count: 1,
    };
    assert_eq!(
        reconcile_setup(
            fixture.plan.clone(),
            fixture.approval.clone(),
            &mut guard,
            &proof,
            250
        )
        .unwrap_err()
        .reason,
        CustomerReason::RecoveryRequired
    );
    fixture.pending(&guard);
    let before = read_journal_body(&fixture.paths, record_file_name(), "test receipt").unwrap();
    for field in ["plan", "approval", "grant_plan", "release", "window"] {
        let mut plan = fixture.plan.clone();
        let mut approval = fixture.approval.clone();
        match field {
            "plan" => plan.profile_fingerprint.push_str("-different"),
            "approval" => approval.approval_id.push_str("-different"),
            "grant_plan" => approval.plan_fingerprint.push_str("-different"),
            "release" => plan.release_version.push_str("-different"),
            "window" => approval.issued_unix = 151,
            _ => unreachable!(),
        }
        let error = reconcile_setup(plan, approval, &mut guard, &proof, 250).unwrap_err();
        assert_eq!(
            error.reason,
            if field == "window" {
                CustomerReason::AuthorizationRequired
            } else {
                CustomerReason::IdentityChanged
            }
        );
        assert_eq!(
            before,
            read_journal_body(&fixture.paths, record_file_name(), "test receipt").unwrap()
        );
    }
    for now in [0, 149] {
        assert_eq!(
            reconcile_setup(
                fixture.plan.clone(),
                fixture.approval.clone(),
                &mut guard,
                &proof,
                now
            )
            .unwrap_err()
            .reason,
            CustomerReason::ObservationUnavailable
        );
    }
    assert_eq!(
        proof.calls.get(),
        0,
        "invalid authority never observes the resource"
    );
    for count in [0, 2] {
        let unproven = ReadBack {
            calls: Cell::new(0),
            count,
        };
        assert_eq!(
            reconcile_setup(
                fixture.plan.clone(),
                fixture.approval.clone(),
                &mut guard,
                &unproven,
                250
            )
            .unwrap_err()
            .reason,
            CustomerReason::RecoveryRequired
        );
        assert_eq!(
            before,
            read_journal_body(&fixture.paths, record_file_name(), "test receipt").unwrap()
        );
    }
}

#[test]
fn setup_reconciliation_rejects_malformed_journals_guard_and_readback_failures() {
    let fixture = Fixture::new();
    let mut guard = fixture.guard();
    fixture.pending(&guard);
    let mut record = read_record(&fixture.paths).unwrap().unwrap();
    let proof = ReadBack {
        calls: Cell::new(0),
        count: 1,
    };
    record.release_version.push_str("-foreign");
    write_record(&guard, &record).unwrap();
    assert_eq!(
        reconcile_setup(
            fixture.plan.clone(),
            fixture.approval.clone(),
            &mut guard,
            &proof,
            250
        )
        .unwrap_err()
        .reason,
        CustomerReason::IdentityChanged
    );
    record.release_version = fixture.plan.release_version.clone();
    record.issued_unix = 0;
    write_record(&guard, &record).unwrap();
    assert_eq!(
        reconcile_setup(
            fixture.plan.clone(),
            fixture.approval.clone(),
            &mut guard,
            &proof,
            250
        )
        .unwrap_err()
        .reason,
        CustomerReason::RecoveryRequired
    );
    record.issued_unix = 150;
    record.phase = TransactionPhase::Committed;
    record.applied_unix = Some(149);
    write_record(&guard, &record).unwrap();
    assert_eq!(
        reconcile_setup(
            fixture.plan.clone(),
            fixture.approval.clone(),
            &mut guard,
            &proof,
            250
        )
        .unwrap_err()
        .reason,
        CustomerReason::RecoveryRequired
    );
    write_journal_record(
        &guard,
        record_file_name(),
        "test malformed receipt",
        "not JSON",
    )
    .unwrap();
    assert_eq!(
        reconcile_setup(
            fixture.plan.clone(),
            fixture.approval.clone(),
            &mut guard,
            &proof,
            250
        )
        .unwrap_err()
        .reason,
        CustomerReason::RecoveryRequired
    );
    assert_eq!(
        read_journal_body(&fixture.paths, record_file_name(), "test receipt")
            .unwrap()
            .as_deref(),
        Some("not JSON")
    );
    drop(guard);
    let other = Fixture::new();
    let mut foreign = acquire_transition(OperationOwner::Up, &other.paths).unwrap();
    assert_eq!(
        reconcile_setup(
            other.plan.clone(),
            other.approval.clone(),
            &mut foreign,
            &proof,
            250
        )
        .unwrap_err()
        .reason,
        CustomerReason::UnsupportedSurface
    );
    assert_eq!(proof.calls.get(), 0);
}

#[test]
fn setup_reconciliation_retains_pending_when_observation_is_unavailable() {
    struct Unavailable;
    impl SetupEffects for Unavailable {
        fn create_owned_origin(&mut self, _: &SetupPlan) -> Result<(), CustomerError> {
            panic!("no allocation in reconciliation")
        }
        fn owned_origin_count(&self, _: &SetupPlan) -> Result<u32, CustomerError> {
            Err(recovery_refusal("descriptor observation failed"))
        }
    }
    let fixture = Fixture::new();
    let mut guard = fixture.guard();
    fixture.pending(&guard);
    let before = read_journal_body(&fixture.paths, record_file_name(), "test receipt").unwrap();
    assert_eq!(
        reconcile_setup(
            fixture.plan.clone(),
            fixture.approval.clone(),
            &mut guard,
            &Unavailable,
            250
        )
        .unwrap_err()
        .reason,
        CustomerReason::RecoveryRequired
    );
    assert_eq!(
        before,
        read_journal_body(&fixture.paths, record_file_name(), "test receipt").unwrap()
    );
}

#[test]
fn setup_apply_replay_refuses_corrupted_release_metadata() {
    let fixture = Fixture::new();
    let mut guard = fixture.guard();
    fixture.pending(&guard);
    let mut record = read_record(&fixture.paths).unwrap().unwrap();
    record.phase = TransactionPhase::Committed;
    record.applied_unix = Some(175);
    record.release_version = "foreign-release".into();
    write_record(&guard, &record).unwrap();
    let before = read_journal_body(&fixture.paths, record_file_name(), "test receipt").unwrap();
    let mut proof = ReadBack {
        calls: Cell::new(0),
        count: 1,
    };
    assert_eq!(
        apply_setup(
            fixture.plan.clone(),
            fixture.approval.clone(),
            &mut guard,
            &mut proof,
            250
        )
        .unwrap_err()
        .reason,
        CustomerReason::IdentityChanged
    );
    assert_eq!(proof.calls.get(), 0);
    assert_eq!(
        before,
        read_journal_body(&fixture.paths, record_file_name(), "test receipt").unwrap()
    );
}

#[test]
fn setup_reconciliation_validates_before_publication() {
    struct PublicationProof {
        published: Cell<bool>,
        refuse: bool,
    }
    impl SetupEffects for PublicationProof {
        fn create_owned_origin(&mut self, _: &SetupPlan) -> Result<(), CustomerError> {
            panic!("reconciliation cannot allocate")
        }
        fn publish_existing_origin(&self, _: &SetupPlan) -> Result<(), CustomerError> {
            self.published.set(true);
            if self.refuse {
                Err(recovery_refusal("fixture publication failed"))
            } else {
                Ok(())
            }
        }
        fn owned_origin_count(&self, _: &SetupPlan) -> Result<u32, CustomerError> {
            assert!(
                self.published.get(),
                "verify only after explicit metadata completion"
            );
            Ok(1)
        }
    }
    let fixture = Fixture::new();
    let mut guard = fixture.guard();
    fixture.pending(&guard);
    let proof = PublicationProof {
        published: Cell::new(false),
        refuse: false,
    };
    let mut foreign = fixture.approval.clone();
    foreign.approval_id.push_str("-foreign");
    assert!(reconcile_setup(fixture.plan.clone(), foreign, &mut guard, &proof, 250).is_err());
    assert!(!proof.published.get());
    assert!(
        reconcile_setup(
            fixture.plan.clone(),
            fixture.approval.clone(),
            &mut guard,
            &proof,
            0
        )
        .is_err()
    );
    assert!(!proof.published.get());
    let refused = PublicationProof {
        published: Cell::new(false),
        refuse: true,
    };
    assert!(
        reconcile_setup(
            fixture.plan.clone(),
            fixture.approval.clone(),
            &mut guard,
            &refused,
            250
        )
        .is_err()
    );
    assert!(refused.published.get());
    assert_eq!(
        read_record(&fixture.paths).unwrap().unwrap().phase,
        TransactionPhase::Applying
    );
    reconcile_setup(
        fixture.plan.clone(),
        fixture.approval.clone(),
        &mut guard,
        &proof,
        250,
    )
    .unwrap();
    assert!(proof.published.get());
    assert_eq!(
        read_record(&fixture.paths).unwrap().unwrap().phase,
        TransactionPhase::Committed
    );
}
