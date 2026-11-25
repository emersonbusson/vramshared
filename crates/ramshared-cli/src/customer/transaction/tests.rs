// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/transaction/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use super::super::transition::{TransitionPaths, acquire_transition};
use super::*;
use crate::readiness::Decision;

static TEMP: AtomicU64 = AtomicU64::new(0);

fn fixture() -> PathBuf {
    let parent = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    for _ in 0..1024 {
        let number = TEMP.fetch_add(1, AtomicOrdering::SeqCst);
        let path = parent.join(format!(
            "ramshared-transaction-{}-{number}",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => return path,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!("create fixture directory: {error}"),
        }
    }
    panic!("exhausted fixture directory names");
}

fn plan(tag: &str) -> SetupPlan {
    let gpu = crate::customer::gpu_plan::tests::fixture_plan();
    let policy = ramshared_vram::ReserveFloorPolicy::sealed();
    SetupPlan {
        operation: super::super::setup_scope::SetupOperation::GpuCache {
            cache: Box::new(super::super::setup_scope::CachePreparationPlan {
                schema_version: super::super::setup_scope::CACHE_PLAN_SCHEMA_VERSION,
                gpus: vec![super::super::gpu_set::CacheAdapterPlan {
                    configured_reserve_bytes: policy.configured_reserve_bytes(gpu.capacity_bytes()),
                    runtime_free_buffer_bytes: ramshared_vram::RUNTIME_FREE_BUFFER_BYTES,
                    gpu: gpu.clone(),
                }],
                required_lower_sink_bytes: ramshared_tier::nbd_readiness::minimum_lower_tier_bytes(
                    gpu.target_bytes,
                )
                .unwrap(),
                lower_sink_identity: format!("lab-lower-sink-{tag}"),
            }),
        },
        release_version: "0.15.0".into(),
        profile_schema_version: 1,
        profile_fingerprint: format!("{tag:0<64}"),
        report_decision: Decision::Ready,
    }
}

fn approval(plan: &SetupPlan, id: &str, issued_unix: u64, expires_unix: u64) -> ApprovalBinding {
    ApprovalBinding {
        approval_id: id.to_string(),
        plan_fingerprint: plan.fingerprint(),
        issued_unix,
        expires_unix,
    }
}

/// A typed effect owner that keeps its own honest count.
///
/// `fail_create` is the crash stand-in: it is the effect that did not
/// complete, so the device state is uncertain and the coordinator must
/// retain the applying record.
#[derive(Default)]
struct FakeEffects {
    origins: u32,
    create_calls: u32,
    fail_create: bool,
    fail_count: bool,
}

impl SetupEffects for FakeEffects {
    fn create_owned_origin(&mut self, _plan: &SetupPlan) -> Result<(), CustomerError> {
        self.create_calls += 1;
        if self.fail_create {
            return Err(recovery_refusal("lab effect owner failed mid-create"));
        }
        self.origins += 1;
        Ok(())
    }

    fn owned_origin_count(&self, _plan: &SetupPlan) -> Result<u32, CustomerError> {
        if self.fail_count {
            return Err(recovery_refusal("origin observation unavailable"));
        }
        Ok(self.origins)
    }
}

/// An effect owner that already holds `origins` origins and refuses to
/// create another one — the shape of a foreign or duplicate artifact.
struct FixedCount {
    origins: u32,
    create_calls: u32,
}

impl SetupEffects for FixedCount {
    fn create_owned_origin(&mut self, _plan: &SetupPlan) -> Result<(), CustomerError> {
        self.create_calls += 1;
        Ok(())
    }

    fn owned_origin_count(&self, _plan: &SetupPlan) -> Result<u32, CustomerError> {
        Ok(self.origins)
    }
}

fn setup_guard(root: &Path) -> CustomerTransitionGuard {
    let paths = TransitionPaths::under(root);
    fs::create_dir_all(root.join("run")).expect("lock root");
    fs::create_dir_all(paths.transactions_root()).expect("transactions root");
    acquire_transition(OperationOwner::Setup, &paths).expect("the setup guard")
}

#[test]
fn apply_refuses_changed_identity_and_accepts_pinned_plan() {
    let root = fixture();
    let mut guard = setup_guard(&root);
    let pinned = plan("pinned");
    let other = plan("other");

    // An approval that binds a different plan is a changed identity.
    let foreign = approval(&other, "approval-foreign", 1_000, 2_000);
    let refused = apply_setup(
        pinned.clone(),
        foreign,
        &mut guard,
        &mut FakeEffects::default(),
        1_500,
    )
    .expect_err("an approval for another plan is not this plan");
    assert_eq!(refused.reason, CustomerReason::IdentityChanged);
    assert_eq!(refused.exit_code(), 4);
    assert_eq!(refused.state, CustomerState::InstalledOff);

    // The same plan with a grant that has not started yet is not authorized.
    let early = approval(&pinned, "approval-early", 2_000, 3_000);
    let refused = apply_setup(
        pinned.clone(),
        early,
        &mut guard,
        &mut FakeEffects::default(),
        1_500,
    )
    .expect_err("a grant that has not started does not authorize");
    assert_eq!(refused.reason, CustomerReason::AuthorizationRequired);
    assert_eq!(refused.exit_code(), 3);
    assert_eq!(refused.state, CustomerState::InstalledOff);

    // The same plan with an expired grant is not authorized.
    let expired = approval(&pinned, "approval-expired", 1_000, 1_500);
    let refused = apply_setup(
        pinned.clone(),
        expired,
        &mut guard,
        &mut FakeEffects::default(),
        1_500,
    )
    .expect_err("an expired grant does not authorize");
    assert_eq!(refused.reason, CustomerReason::AuthorizationRequired);
    assert_eq!(refused.exit_code(), 3);
    assert_eq!(refused.state, CustomerState::InstalledOff);

    // A wrong owner cannot apply setup effects at all.
    drop(guard);
    let paths = TransitionPaths::under(&root);
    let mut up_guard = acquire_transition(OperationOwner::Up, &paths).expect("the up guard");
    let pinned_again = plan("pinned");
    let valid = approval(&pinned_again, "approval-valid", 1_000, 2_000);
    let refused = apply_setup(
        pinned_again.clone(),
        valid,
        &mut up_guard,
        &mut FakeEffects::default(),
        1_500,
    )
    .expect_err("only the setup owner applies setup effects");
    assert_eq!(refused.reason, CustomerReason::UnsupportedSurface);
    assert_eq!(refused.exit_code(), 2);
    drop(up_guard);

    // And the pinned plan under a live grant is accepted.
    let mut guard = setup_guard(&root);
    let accepted_plan = plan("pinned");
    let valid = approval(&accepted_plan, "approval-valid", 1_000, 2_000);
    let mut effects = FakeEffects::default();
    let receipt = apply_setup(
        accepted_plan.clone(),
        valid,
        &mut guard,
        &mut effects,
        1_500,
    )
    .expect("a pinned plan under a live grant is applied");
    assert_eq!(receipt.plan_fingerprint, accepted_plan.fingerprint());
    assert_eq!(receipt.approval_id, "approval-valid");
    assert_eq!(receipt.release_version, "0.15.0");
    assert_eq!(receipt.applied_unix, 1_500);
    assert_eq!(effects.create_calls, 1);
    assert_eq!(effects.origins, 1);
}

#[test]
fn apply_twice_preserves_single_owned_origin() {
    let root = fixture();
    let mut guard = setup_guard(&root);
    let pinned = plan("idempotent");
    let valid = approval(&pinned, "approval-once", 1_000, 2_000);
    let mut effects = FakeEffects::default();

    let first = apply_setup(
        pinned.clone(),
        valid.clone(),
        &mut guard,
        &mut effects,
        1_500,
    )
    .expect("the first apply");
    assert_eq!(effects.create_calls, 1);
    assert_eq!(
        effects
            .owned_origin_count(&pinned)
            .expect("the count is observable"),
        1
    );

    // The same approved setup applied again is one origin and one receipt.
    let second = apply_setup(pinned.clone(), valid, &mut guard, &mut effects, 1_600)
        .expect("the second apply of the same identity");
    assert_eq!(first, second);
    assert_eq!(effects.create_calls, 1, "no new effect is issued");
    assert_eq!(
        effects
            .owned_origin_count(&pinned)
            .expect("the count is observable"),
        1,
        "exactly one owned origin survives a repeat apply"
    );

    // A committed origin is never adopted by a different identity.
    let other = plan("other-owner");
    let foreign = approval(&other, "approval-other", 1_000, 2_000);
    let refused = apply_setup(other.clone(), foreign, &mut guard, &mut effects, 1_700)
        .expect_err("a committed origin has a prior owner");
    assert_eq!(refused.reason, CustomerReason::IdentityChanged);
    assert_eq!(refused.exit_code(), 4);
    assert_eq!(refused.state, CustomerState::InstalledOff);
    assert_eq!(effects.create_calls, 1, "the prior owner keeps the origin");
}

#[test]
fn interrupted_setup_retains_receipt_and_requires_recovery() {
    let root = fixture();
    let mut guard = setup_guard(&root);
    let pinned = plan("interrupted");
    let valid = approval(&pinned, "approval-crash", 1_000, 2_000);

    // The effect owner dies mid-create. The device state is uncertain.
    let mut crashing = FakeEffects {
        fail_create: true,
        ..FakeEffects::default()
    };
    let refused = apply_setup(
        pinned.clone(),
        valid.clone(),
        &mut guard,
        &mut crashing,
        1_500,
    )
    .expect_err("an interrupted effect is not a clean setup");
    assert_eq!(refused.reason, CustomerReason::RecoveryRequired);
    assert_eq!(refused.exit_code(), 5);
    assert_eq!(refused.state, CustomerState::RecoveryRequired);

    // The durable record is retained at Applying and names the attempt.
    let record = read_record(guard.paths())
        .expect("the journal is readable")
        .expect("the interrupted attempt is retained");
    assert_eq!(record.phase, TransactionPhase::Applying);
    assert_eq!(record.plan_fingerprint, pinned.fingerprint());
    assert_eq!(record.approval_id, "approval-crash");
    assert_eq!(record.release_version, "0.15.0");
    assert_eq!(record.applied_unix, None);

    // A fresh effect owner cannot silently finish someone else's attempt.
    let mut fresh = FakeEffects::default();
    let refused = apply_setup(pinned.clone(), valid.clone(), &mut guard, &mut fresh, 1_600)
        .expect_err("an applying record requires attended reconciliation");
    assert_eq!(refused.reason, CustomerReason::RecoveryRequired);
    assert_eq!(refused.exit_code(), 5);
    assert_eq!(refused.state, CustomerState::RecoveryRequired);
    assert_eq!(fresh.create_calls, 0, "no new effect is issued");

    // The record outlives the guard process that wrote it.
    drop(guard);
    let paths = TransitionPaths::under(&root);
    let observed = read_record(&paths)
        .expect("the journal is readable without a lock")
        .expect("the interrupted attempt outlives its process");
    assert_eq!(observed.phase, TransactionPhase::Applying);
    assert_eq!(observed.approval_id, "approval-crash");

    // A count that is not exactly one is also recovery, and also retains
    // the applying record. Clearing the interrupted record is what an
    // attended reconciliation does; this coordinator never does it itself.
    let mut guard = setup_guard(&root);
    clear_record(&root);
    let other = plan("duplicate");
    let other_approval = approval(&other, "approval-duplicate", 1_000, 2_000);
    let mut duplicate = FixedCount {
        origins: 2,
        create_calls: 0,
    };
    let refused = apply_setup(
        other.clone(),
        other_approval,
        &mut guard,
        &mut duplicate,
        1_800,
    )
    .expect_err("two owned origins is not one owned origin");
    assert_eq!(refused.reason, CustomerReason::RecoveryRequired);
    assert_eq!(refused.state, CustomerState::RecoveryRequired);
    assert_eq!(duplicate.create_calls, 1);
    let record = read_record(guard.paths())
        .expect("the journal is readable")
        .expect("the failed count check is retained");
    assert_eq!(record.phase, TransactionPhase::Applying);
    assert_eq!(record.plan_fingerprint, other.fingerprint());
}

/// Attended reconciliation is out of this module's scope; the test clears
/// the record the way an operator would so the next journey can start.
fn clear_record(root: &Path) {
    let paths = TransitionPaths::under(root);
    let _ = fs::remove_file(paths.transactions_root().join(record_file_name()));
}

#[test]
fn setup_transaction_record_refuses_malformed_foreign_and_incomplete_shapes() {
    let root = fixture();
    let guard = setup_guard(&root);
    let pinned = plan("strict");
    let valid = approval(&pinned, "approval-strict", 1_000, 2_000);

    // A committed record without a commit timestamp is not a record.
    let incomplete = TransactionRecord {
        plan_fingerprint: pinned.fingerprint(),
        approval_id: "approval-strict".to_string(),
        release_version: "0.15.0".to_string(),
        phase: TransactionPhase::Committed,
        issued_unix: 1_500,
        applied_unix: None,
    };
    write_record(&guard, &incomplete)
        .expect_err("a committed record must carry its commit timestamp");

    // An applying record with a commit timestamp is the lie the phase
    // exists to prevent.
    let lying = TransactionRecord {
        phase: TransactionPhase::Applying,
        applied_unix: Some(1_500),
        ..incomplete.clone()
    };
    write_record(&guard, &lying).expect_err("an applying record has no commit timestamp");

    // An empty identity field cannot be written.
    let empty = TransactionRecord {
        approval_id: String::new(),
        ..incomplete.clone()
    };
    write_record(&guard, &empty).expect_err("identity fields are never empty");

    // An oversized identity field cannot be written.
    let oversized = TransactionRecord {
        approval_id: "a".repeat(MAX_IDENTITY_FIELD_BYTES + 1),
        ..incomplete.clone()
    };
    write_record(&guard, &oversized)
        .expect_err("identity fields are bounded before they reach the journal");

    // A body that is not a strict record is a refusal, not an empty journal.
    write_journal_record(
        &guard,
        record_file_name(),
        "setup transaction record",
        "{not json",
    )
    .expect("the hostile body is writable");
    read_record(guard.paths()).expect_err("a malformed journal body is not an empty journal");

    // A symlink at the record name is never opened, and never replaced.
    let record_path = guard.paths().transactions_root().join(record_file_name());
    let _ = fs::remove_file(&record_path);
    std::os::unix::fs::symlink("/etc/passwd", &record_path).expect("the hostile name");
    read_record(guard.paths()).expect_err("a symlink at the record name is never opened");
    assert!(
        record_path
            .symlink_metadata()
            .expect("name still exists")
            .file_type()
            .is_symlink(),
        "the refusal leaves the hostile name in place"
    );

    // A non-setup owner cannot pass the unreadable setup record.
    drop(guard);
    let paths = TransitionPaths::under(&root);
    assert_eq!(
        acquire_transition(OperationOwner::Package, &paths)
            .expect_err("symlink blocks package custody")
            .reason,
        CustomerReason::RecoveryRequired
    );
    fs::remove_file(&record_path).expect("remove fixture-owned symlink without following it");
    // With a clean journal the wrong owner still cannot issue setup effects.
    let paths = TransitionPaths::under(&root);
    let mut package =
        acquire_transition(OperationOwner::Package, &paths).expect("the package guard");
    let refused = apply_setup(
        pinned.clone(),
        valid,
        &mut package,
        &mut FakeEffects::default(),
        1_500,
    )
    .expect_err("the package owner does not apply setup effects");
    assert_eq!(refused.reason, CustomerReason::UnsupportedSurface);
    drop(package);
    fs::remove_dir_all(root).expect("remove isolated transaction fixture");
}
#[test]
fn committed_retry_rechecks_origin_without_recreating_it() {
    let root = fixture();
    let mut guard = setup_guard(&root);
    let pinned = plan("recheck");
    let grant = approval(&pinned, "approval-recheck", 1_000, 2_000);
    let mut effects = FakeEffects::default();
    apply_setup(
        pinned.clone(),
        grant.clone(),
        &mut guard,
        &mut effects,
        1_500,
    )
    .expect("initial setup");
    let record_path = guard
        .paths()
        .transactions_root()
        .join("setup-transaction.json");
    let before = fs::read(&record_path).expect("committed record");
    for (count, fail) in [(0, false), (2, false), (1, true)] {
        effects.origins = count;
        effects.fail_count = fail;
        let error = apply_setup(
            pinned.clone(),
            grant.clone(),
            &mut guard,
            &mut effects,
            1_600,
        )
        .expect_err("current origin ownership is required");
        assert_eq!(error.reason, CustomerReason::RecoveryRequired);
        assert_eq!(effects.create_calls, 1);
        assert_eq!(fs::read(&record_path).expect("record retained"), before);
    }
    effects.origins = 1;
    effects.fail_count = false;
    apply_setup(pinned, grant, &mut guard, &mut effects, 2_100)
        .expect("existing valid receipt after grant expiry issues no create");
    assert_eq!(effects.create_calls, 1);
    drop(guard);
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn blocked_plan_never_writes_intent_or_runs_effects() {
    let root = fixture();
    let mut guard = setup_guard(&root);
    let mut blocked = plan("blocked");
    blocked.report_decision = Decision::Blocked;
    let grant = approval(&blocked, "approval-blocked", 1_000, 2_000);
    let mut effects = FakeEffects::default();
    assert!(apply_setup(blocked, grant, &mut guard, &mut effects, 1_500).is_err());
    assert_eq!(effects.create_calls, 0);
    assert!(
        !guard
            .paths()
            .transactions_root()
            .join("setup-transaction.json")
            .exists()
    );
    drop(guard);
    fs::remove_dir_all(root).expect("remove fixture");
}
#[test]
fn filesystem_origin_loss_or_replacement_refuses_receipt_replay() {
    use std::fs::{File, OpenOptions};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    struct FileEffects {
        path: PathBuf,
        held: Option<File>,
        creates: u32,
    }
    impl SetupEffects for FileEffects {
        fn create_owned_origin(&mut self, _: &SetupPlan) -> Result<(), CustomerError> {
            self.creates += 1;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&self.path)
                .map_err(|e| recovery_refusal(e.to_string()))?;
            file.set_len(4096)
                .and_then(|_| file.sync_all())
                .map_err(|e| recovery_refusal(e.to_string()))?;
            self.held = Some(file);
            Ok(())
        }
        fn owned_origin_count(&self, _: &SetupPlan) -> Result<u32, CustomerError> {
            let Some(held) = &self.held else {
                return Ok(0);
            };
            let pinned = held
                .metadata()
                .map_err(|e| recovery_refusal(e.to_string()))?;
            let observed = match fs::symlink_metadata(&self.path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
                Err(error) => return Err(recovery_refusal(error.to_string())),
            };
            Ok(u32::from(
                observed.is_file()
                    && observed.dev() == pinned.dev()
                    && observed.ino() == pinned.ino()
                    && observed.nlink() == 1
                    && observed.len() == 4096,
            ))
        }
    }
    let root = fixture();
    let mut guard = setup_guard(&root);
    let pinned = plan("real-file");
    let grant = approval(&pinned, "approval-real-file", 1_000, 2_000);
    let path = root.join("origin.img");
    let mut effects = FileEffects {
        path: path.clone(),
        held: None,
        creates: 0,
    };
    let receipt = apply_setup(
        pinned.clone(),
        grant.clone(),
        &mut guard,
        &mut effects,
        1_500,
    )
    .expect("real file and journal");
    let record_path = guard
        .paths()
        .transactions_root()
        .join("setup-transaction.json");
    let journal = fs::read(&record_path).expect("committed journal");
    assert_eq!(
        apply_setup(
            pinned.clone(),
            grant.clone(),
            &mut guard,
            &mut effects,
            1_600
        )
        .expect("valid retry"),
        receipt
    );
    fs::remove_file(&path).expect("simulate lost path while old inode remains held");
    assert_eq!(
        apply_setup(
            pinned.clone(),
            grant.clone(),
            &mut guard,
            &mut effects,
            1_600
        )
        .expect_err("missing origin")
        .reason,
        CustomerReason::RecoveryRequired
    );
    let mut replacement = vec![0u8; 4096];
    replacement[..7].copy_from_slice(b"foreign");
    fs::write(&path, &replacement).expect("same-size foreign replacement");
    assert_eq!(
        apply_setup(pinned, grant, &mut guard, &mut effects, 1_600)
            .expect_err("foreign replacement")
            .reason,
        CustomerReason::RecoveryRequired
    );
    assert_eq!(effects.creates, 1);
    assert_eq!(fs::read(&path).expect("foreign data retained"), replacement);
    assert_eq!(fs::read(&record_path).expect("receipt retained"), journal);
    drop(effects);
    drop(guard);
    fs::remove_dir_all(root).expect("remove isolated fixture");
}
#[test]
fn incomplete_setup_blocks_other_transition_owners() {
    let root = fixture();
    let guard = setup_guard(&root);
    let paths = guard.paths().clone();
    let record = TransactionRecord {
        plan_fingerprint: "f".repeat(64),
        approval_id: "setup-test".into(),
        release_version: "0.15.0".into(),
        phase: TransactionPhase::Applying,
        issued_unix: 1000,
        applied_unix: None,
    };
    write_record(&guard, &record).expect("durable applying record");
    let path = paths.transactions_root().join("setup-transaction.json");
    let before = fs::read(&path).expect("record bytes");
    drop(guard);
    for owner in [
        OperationOwner::Up,
        OperationOwner::Boot,
        OperationOwner::Selection,
        OperationOwner::Package,
    ] {
        let error = acquire_transition(owner, &paths)
            .expect_err("incomplete setup blocks every other owner");
        assert_eq!(error.reason, CustomerReason::RecoveryRequired);
        assert_eq!(fs::read(&path).expect("record retained"), before);
    }
    let guard = acquire_transition(OperationOwner::Setup, &paths)
        .expect("setup may inspect its interrupted record");
    let committed = TransactionRecord {
        phase: TransactionPhase::Committed,
        applied_unix: Some(1010),
        ..record
    };
    write_record(&guard, &committed).expect("manufactured reconciliation");
    drop(guard);
    drop(acquire_transition(OperationOwner::Up, &paths).expect("committed setup allows lifecycle"));
    for corrupt in ["{truncated", &"x".repeat(65537)] {
        fs::write(&path, corrupt).expect("manufactured interrupted journal write");
        let error = acquire_transition(OperationOwner::Up, &paths)
            .expect_err("unknown record blocks mutation");
        assert_eq!(error.reason, CustomerReason::RecoveryRequired);
        assert_eq!(
            fs::read_to_string(&path).expect("corrupt record retained"),
            corrupt
        );
    }
    fs::remove_dir_all(root).expect("remove fixture");
}
#[test]
fn inconsistent_gpu_plan_never_writes_intent_or_runs_effects() {
    let root = fixture();
    let mut guard = setup_guard(&root);
    let mut invalid = plan("invalid-gpu");
    let super::super::setup_scope::SetupOperation::GpuCache { cache } = &mut invalid.operation
    else {
        panic!("cache fixture")
    };
    cache.gpus[0].gpu.target_bytes += 1;
    let grant = approval(&invalid, "approval-invalid-gpu", 1000, 2000);
    let mut effects = FakeEffects::default();
    assert_eq!(
        apply_setup(invalid, grant, &mut guard, &mut effects, 1500)
            .expect_err("inconsistent captured target")
            .reason,
        CustomerReason::CapacityRefused
    );
    assert_eq!(effects.create_calls, 0);
    assert!(
        !guard
            .paths()
            .transactions_root()
            .join(record_file_name())
            .exists()
    );
    drop(guard);
    fs::remove_dir_all(root).expect("remove fixture");
}
#[test]
fn setup_storage_transaction_preserves_scope_and_idempotency() {
    use super::super::setup_scope::SetupOperation;
    struct StorageEffects {
        calls: u32,
    }
    impl SetupEffects for StorageEffects {
        fn create_owned_origin(&mut self, plan: &SetupPlan) -> Result<(), CustomerError> {
            assert!(matches!(plan.operation, SetupOperation::StorageOnly {}));
            self.calls += 1;
            Ok(())
        }
        fn owned_origin_count(&self, plan: &SetupPlan) -> Result<u32, CustomerError> {
            assert!(matches!(plan.operation, SetupOperation::StorageOnly {}));
            Ok(self.calls)
        }
    }
    let root = fixture();
    let mut guard = setup_guard(&root);
    let cache = plan("storage-scope");
    let mut storage = cache.clone();
    storage.operation = SetupOperation::StorageOnly {};
    storage.report_decision = Decision::Blocked;
    assert_ne!(storage.fingerprint(), cache.fingerprint());
    let mut effects = StorageEffects { calls: 0 };
    let cache_approval = approval(&cache, "cache-grant", 1000, 2000);
    assert_eq!(
        apply_setup(
            storage.clone(),
            cache_approval,
            &mut guard,
            &mut effects,
            1500
        )
        .unwrap_err()
        .reason,
        CustomerReason::IdentityChanged
    );
    assert_eq!(effects.calls, 0);
    assert!(
        !guard
            .paths()
            .transactions_root()
            .join(record_file_name())
            .exists()
    );
    let grant = approval(&storage, "storage-grant", 1000, 2000);
    let first = apply_setup(
        storage.clone(),
        grant.clone(),
        &mut guard,
        &mut effects,
        1500,
    )
    .unwrap();
    let second = apply_setup(storage, grant, &mut guard, &mut effects, 1501).unwrap();
    assert_eq!(first, second);
    assert_eq!(effects.calls, 1);
    drop(guard);
    fs::remove_dir_all(root).unwrap();
}
