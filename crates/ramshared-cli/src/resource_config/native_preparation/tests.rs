// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/native_preparation/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::customer::transaction::apply_setup;
use crate::customer::transition::{OperationOwner, TransitionPaths, acquire_transition};
use crate::readiness::Decision;
use std::fs;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixture {
    root: PathBuf,
    config: PathBuf,
    mount: super::super::MountInfo,
    profile: ResourceProfile,
    plan: SetupPlan,
    grant: ApprovalBinding,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "ramshared-native-preparation-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let config = root.join("configuration");
        fs::create_dir(&config).unwrap();
        let file = File::open(&root).unwrap();
        let stat = rustix::fs::statx(
            &file,
            "",
            rustix::fs::AtFlags::EMPTY_PATH,
            rustix::fs::StatxFlags::BASIC_STATS | rustix::fs::StatxFlags::MNT_ID,
        )
        .unwrap();
        let mut mount = super::super::tests::fixture_block_device().mounts.remove(0);
        mount.mountpoint = root.to_str().unwrap().into();
        mount.mount_id = stat.stx_mnt_id;
        mount.major_minor = format!("{}:{}", stat.stx_dev_major, stat.stx_dev_minor);
        let profile = ResourceProfile {
            schema_version: 1,
            planned_caps: Default::default(),
            targets: vec![ResourceTarget::LinuxFileOriginRequest {
                filesystem_uuid: "fs-uuid".into(),
                device_identity: "wwn:wwn-123".into(),
                managed_relative_path: "origin/new.img".into(),
                allocated_bytes: 8192,
            }],
        };
        let plan = SetupPlan {
            operation: SetupOperation::StorageOnly {},
            release_version: "v0.15.0".into(),
            profile_schema_version: 1,
            profile_fingerprint: super::super::profile_digest(&profile.to_toml().unwrap()),
            report_decision: Decision::Blocked,
        };
        let grant = ApprovalBinding {
            approval_id: "native-allocation-test".into(),
            plan_fingerprint: plan.fingerprint(),
            issued_unix: 1000,
            expires_unix: 2000,
        };
        Self {
            root,
            config,
            mount,
            profile,
            plan,
            grant,
        }
    }
    fn effects(&self) -> NativePreparation {
        // This fixture injects capacity for state-machine tests; it does
        // not qualify host/native capacity. Allocation/custody are real.
        NativePreparation {
            volume: open_native_origin(&self.mount, rustix::process::geteuid().as_raw()).unwrap(),
            request: self.profile.targets[0].clone(),
            binding: Binding {
                plan: self.plan.fingerprint(),
                approval: self.grant.approval_id.clone(),
                profile: self.plan.profile_fingerprint.clone(),
                release: self.plan.release_version.clone(),
            },
            manifest_path: self.config.join(NATIVE_ORIGIN_MANIFEST_NAME),
            allocation: None,
            manifest: None,
            recovery_publication: None,
            held_origin: None,
            observe_free: |_| Ok(u64::MAX),
        }
    }
    fn guard(&self) -> crate::customer::transition::CustomerTransitionGuard {
        let paths = TransitionPaths::under(&self.root);
        fs::create_dir_all(&paths.lock_root).unwrap();
        fs::create_dir_all(paths.transactions_root()).unwrap();
        acquire_transition(OperationOwner::Setup, &paths).unwrap()
    }
    fn origin(&self) -> PathBuf {
        self.root.join("var/lib/ramshared/origin/new.img")
    }
    fn reconciliation_snapshot(&self) -> ResourceSnapshot {
        let mut snapshot = super::super::tests::fixture_snapshot();
        snapshot.platform = RuntimePlatform::NativeLinux;
        snapshot.windows = None;
        snapshot.observed_unix_ms = super::super::unix_millis();
        let mut device = super::super::tests::fixture_block_device();
        device.mounts = vec![self.mount.clone()];
        device.major_minor = Some(self.mount.major_minor.clone());
        device.mounts[0].total_bytes = None;
        device.mounts[0].available_bytes = None;
        device.mounts[0].capacity_observed_unix_ms = None;
        snapshot.block_devices = vec![device];
        snapshot
    }
    fn readback(&self) -> Result<NativePreparation, CustomerError> {
        NativePreparation::from_observation(
            &self.plan,
            &self.grant,
            &self.profile,
            &self.reconciliation_snapshot(),
            &self.config,
            rustix::process::geteuid().as_raw(),
            PreparationMode::Reconcile,
        )
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

struct InterruptedReadback<'a>(&'a mut NativePreparation);
impl SetupEffects for InterruptedReadback<'_> {
    fn create_owned_origin(&mut self, plan: &SetupPlan) -> Result<(), CustomerError> {
        self.0.create_owned_origin(plan)
    }
    fn owned_origin_count(&self, _: &SetupPlan) -> Result<u32, CustomerError> {
        Err(recovery_refusal(
            "interrupted after publication, before receipt commit",
        ))
    }
}

#[test]
fn native_reconciliation_recovers_inode_proof_without_payload_changes() {
    use crate::customer::transaction::reconcile_setup;
    use std::os::unix::fs::FileExt;
    let fixture = Fixture::new();
    let mut effects = fixture.effects();
    effects.prepare_new(&fixture.profile).unwrap();
    // A competing publication makes the real creator fail after linking.
    fs::write(&effects.manifest_path, b"fixture publication conflict").unwrap();
    let mut guard = fixture.guard();
    assert!(
        apply_setup(
            fixture.plan.clone(),
            fixture.grant.clone(),
            &mut guard,
            &mut effects,
            1500
        )
        .is_err()
    );
    let record = guard
        .paths()
        .transactions_root()
        .join("setup-transaction.json");
    let pending = fs::read(&record).unwrap();
    assert!(String::from_utf8_lossy(&pending).contains("applying"));
    // Remove only this fixture's conflict, retaining the actual origin.
    fs::remove_file(&effects.manifest_path).unwrap();
    drop(effects);
    let file = fs::OpenOptions::new()
        .write(true)
        .open(fixture.origin())
        .unwrap();
    file.write_all_at(&[61; 8192], 0).unwrap();
    file.sync_all().unwrap();
    let inode = file.metadata().unwrap().ino();
    drop(file);
    let proof = fixture
        .readback()
        .expect("recover the creator proof from the actual inode");
    assert!(
        !proof.manifest_path.exists(),
        "observation cannot publish metadata"
    );
    assert_eq!(fs::read(&record).unwrap(), pending);
    let creator = proof.manifest.clone().unwrap();
    let receipt = reconcile_setup(
        fixture.plan.clone(),
        fixture.grant.clone(),
        &mut guard,
        &proof,
        2500,
    )
    .unwrap();
    assert_eq!(proof.read_manifest().unwrap(), creator);
    assert_eq!(fs::read(fixture.origin()).unwrap(), [61; 8192]);
    assert_eq!(fs::metadata(fixture.origin()).unwrap().ino(), inode);
    assert_eq!(
        reconcile_setup(
            fixture.plan.clone(),
            fixture.grant.clone(),
            &mut guard,
            &proof,
            2600
        )
        .unwrap(),
        receipt
    );
    assert_eq!(proof.read_manifest().unwrap(), creator);
}

#[test]
fn native_reconciliation_refuses_foreign_inode_proof() {
    use rustix::fs::{XattrFlags, fsetxattr};
    let fixture = Fixture::new();
    let mut effects = fixture.effects();
    effects.prepare_new(&fixture.profile).unwrap();
    effects.create_owned_origin(&fixture.plan).unwrap();
    let original = effects.manifest.clone().unwrap();
    fs::remove_file(&effects.manifest_path).unwrap();
    drop(effects);
    let file = fs::OpenOptions::new()
        .write(true)
        .open(fixture.origin())
        .unwrap();
    let mut foreign = original.clone();
    foreign.approval_id.push_str("-foreign");
    fsetxattr(
        &file,
        "user.ramshared.creation",
        foreign.encode().unwrap().as_bytes(),
        XattrFlags::REPLACE,
    )
    .unwrap();
    assert_eq!(
        fixture.readback().err().unwrap().reason,
        CustomerReason::IdentityChanged
    );
    fsetxattr(
        &file,
        "user.ramshared.creation",
        b"broken = [",
        XattrFlags::REPLACE,
    )
    .unwrap();
    assert_eq!(
        fixture.readback().err().unwrap().reason,
        CustomerReason::RecoveryRequired
    );
    fsetxattr(
        &file,
        "user.ramshared.creation",
        original.encode().unwrap().as_bytes(),
        XattrFlags::REPLACE,
    )
    .unwrap();
    let proof = fixture.readback().unwrap();
    assert!(
        fixture.readback().is_err(),
        "recovery retains the exclusive serving lease"
    );
    let inode = file.metadata().unwrap().ino();
    let bytes = fs::read(fixture.origin()).unwrap();
    // Observation is not permanent authority: reject proof drift before
    // the guarded coordinator publishes any canonical metadata.
    fsetxattr(
        &file,
        "user.ramshared.creation",
        foreign.encode().unwrap().as_bytes(),
        XattrFlags::REPLACE,
    )
    .unwrap();
    assert!(proof.publish_existing_origin(&fixture.plan).is_err());
    assert!(!proof.manifest_path.exists());
    fsetxattr(
        &file,
        "user.ramshared.creation",
        original.encode().unwrap().as_bytes(),
        XattrFlags::REPLACE,
    )
    .unwrap();
    // A rival canonical manifest is retained and blocks recovery publication.
    artifact_file::write_new(&proof.manifest_path, foreign.encode().unwrap().as_bytes()).unwrap();
    assert!(proof.publish_existing_origin(&fixture.plan).is_err());
    assert_eq!(
        fs::read_to_string(&proof.manifest_path).unwrap(),
        foreign.encode().unwrap()
    );
    assert_eq!(fs::metadata(fixture.origin()).unwrap().ino(), inode);
    assert_eq!(fs::read(fixture.origin()).unwrap(), bytes);
}

#[test]
fn native_reconciliation_preserves_published_data_and_creator_manifest() {
    use crate::customer::transaction::reconcile_setup;
    use std::os::unix::fs::FileExt;
    let fixture = Fixture::new();
    let mut effects = fixture.effects();
    effects.prepare_new(&fixture.profile).unwrap();
    let mut guard = fixture.guard();
    assert_eq!(
        apply_setup(
            fixture.plan.clone(),
            fixture.grant.clone(),
            &mut guard,
            &mut InterruptedReadback(&mut effects),
            1500
        )
        .unwrap_err()
        .reason,
        CustomerReason::RecoveryRequired
    );
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(fixture.origin())
        .unwrap();
    let bytes = vec![23; 8192];
    file.write_all_at(&bytes, 0).unwrap();
    file.sync_data().unwrap();
    drop(file);
    let inode = fs::metadata(fixture.origin()).unwrap().ino();
    let manifest = fs::read(&effects.manifest_path).unwrap();
    drop(effects);
    let mut proof = fixture.readback().unwrap();
    assert!(proof.allocation.is_none());
    proof.observe_free = |_| panic!("read-back must never allocate or require free space");
    let receipt = reconcile_setup(
        fixture.plan.clone(),
        fixture.grant.clone(),
        &mut guard,
        &proof,
        2500,
    )
    .unwrap();
    assert_eq!(receipt.applied_unix, 2500);
    assert_eq!(fs::read(fixture.origin()).unwrap(), bytes);
    assert_eq!(fs::metadata(fixture.origin()).unwrap().ino(), inode);
    assert_eq!(fs::read(&proof.manifest_path).unwrap(), manifest);
    // The reconciler retains the serving lease through durable completion.
    assert!(
        proof
            .volume
            .verify_for_io(&proof.manifest.as_ref().unwrap().target)
            .is_err()
    );
    assert_eq!(
        reconcile_setup(
            fixture.plan.clone(),
            fixture.grant.clone(),
            &mut guard,
            &proof,
            2600
        )
        .unwrap(),
        receipt
    );
    drop(proof);
    assert!(fixture.readback().is_ok());
    drop(guard);
    let allowed =
        acquire_transition(OperationOwner::Up, &TransitionPaths::under(&fixture.root)).unwrap();
    drop(allowed);
}

#[test]
fn native_reconciliation_refuses_unsealed_changed_and_serving_origins() {
    let fixture = Fixture::new();
    let mut effects = fixture.effects();
    effects.prepare_new(&fixture.profile).unwrap();
    let mut guard = fixture.guard();
    assert!(
        apply_setup(
            fixture.plan.clone(),
            fixture.grant.clone(),
            &mut guard,
            &mut InterruptedReadback(&mut effects),
            1500
        )
        .is_err()
    );
    // Model an older or foreign inode lacking independent creation proof.
    let file = File::open(fixture.origin()).unwrap();
    rustix::fs::fremovexattr(&file, "user.ramshared.creation").unwrap();
    drop(file);
    effects.held_origin.take();
    let manifest = fs::read(&effects.manifest_path).unwrap();
    let journal_path = guard
        .paths()
        .transactions_root()
        .join("setup-transaction.json");
    let journal = fs::read(&journal_path).unwrap();
    let bytes = fs::read(fixture.origin()).unwrap();
    fs::remove_file(&effects.manifest_path).unwrap();
    assert_eq!(
        fixture.readback().err().unwrap().reason,
        CustomerReason::RecoveryRequired
    );
    assert_eq!(fs::read(fixture.origin()).unwrap(), bytes);
    artifact_file::write_new(&effects.manifest_path, &manifest).unwrap();
    let serving = effects
        .volume
        .verify_for_io(&effects.manifest.as_ref().unwrap().target)
        .unwrap();
    assert_eq!(
        fixture.readback().err().unwrap().reason,
        CustomerReason::RecoveryRequired
    );
    drop(serving);
    let proof = fixture.readback().unwrap();
    drop(proof);
    let retained = fixture.root.join("retained.img");
    fs::rename(fixture.origin(), &retained).unwrap();
    fs::write(fixture.origin(), &bytes).unwrap();
    fs::set_permissions(fixture.origin(), fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        fixture.readback().err().unwrap().reason,
        CustomerReason::RecoveryRequired
    );
    assert_eq!(fs::read(retained).unwrap(), bytes);
    assert_eq!(fs::read(fixture.origin()).unwrap(), bytes);
    assert_eq!(fs::read(journal_path).unwrap(), journal);
    assert_eq!(fs::read(&effects.manifest_path).unwrap(), manifest);
}

#[test]
fn native_preparation_allocates_exact_private_origin_and_replays_one_receipt() {
    let fixture = Fixture::new();
    let mut effects = fixture.effects();
    effects.prepare_new(&fixture.profile).unwrap();
    let mut guard = fixture.guard();
    let receipt = apply_setup(
        fixture.plan.clone(),
        fixture.grant.clone(),
        &mut guard,
        &mut effects,
        1500,
    )
    .unwrap();
    let metadata = fs::metadata(fixture.origin()).unwrap();
    assert_eq!(metadata.len(), 8192);
    assert_eq!(metadata.mode() & 0o7777, 0o600);
    assert_eq!(metadata.nlink(), 1);
    assert!(metadata.blocks() * 512 >= 8192);
    let mut data = Vec::new();
    File::open(fixture.origin())
        .unwrap()
        .read_to_end(&mut data)
        .unwrap();
    assert_eq!(data, vec![0; 8192]);
    let manifest =
        NativeOriginManifest::parse(&fs::read_to_string(&effects.manifest_path).unwrap()).unwrap();
    assert_eq!(manifest.plan_fingerprint, fixture.plan.fingerprint());
    assert_eq!(
        fs::metadata(&effects.manifest_path).unwrap().mode() & 0o7777,
        0o600
    );
    assert_eq!(
        apply_setup(
            fixture.plan.clone(),
            fixture.grant.clone(),
            &mut guard,
            &mut effects,
            1501
        )
        .unwrap(),
        receipt
    );
    assert_eq!(
        fs::metadata(fixture.origin()).unwrap().ino(),
        metadata.ino()
    );
    // A new owner can observe the same durable receipt, without allocation.
    let mut reopened = fixture.effects();
    let known = reopened.read_manifest().unwrap();
    reopened.held_origin = Some(reopened.volume.verify(&known.target).unwrap());
    reopened.manifest = Some(known);
    assert_eq!(
        apply_setup(
            fixture.plan.clone(),
            fixture.grant.clone(),
            &mut guard,
            &mut reopened,
            1502
        )
        .unwrap(),
        receipt
    );
}

#[test]
fn native_preparation_capacity_existing_file_and_symlink_refuse_without_replacement() {
    let fixture = Fixture::new();
    let mut effects = fixture.effects();
    effects.observe_free = |_| Ok(0);
    assert!(effects.prepare_new(&fixture.profile).is_err());
    assert!(!fixture.origin().exists());
    fs::create_dir_all(fixture.origin().parent().unwrap()).unwrap();
    fs::write(fixture.origin(), b"foreign").unwrap();
    effects.observe_free = |_| Ok(u64::MAX);
    assert!(effects.prepare_new(&fixture.profile).is_err());
    assert_eq!(fs::read(fixture.origin()).unwrap(), b"foreign");
    fs::remove_file(fixture.origin()).unwrap();
    symlink(fixture.root.join("foreign"), fixture.origin()).unwrap();
    assert!(effects.prepare_new(&fixture.profile).is_err());
    assert!(!fixture.root.join("foreign").exists());
}

#[test]
fn native_preparation_manifest_failure_retains_origin_and_applying_record() {
    let fixture = Fixture::new();
    let mut effects = fixture.effects();
    effects.prepare_new(&fixture.profile).unwrap();
    let foreign = b"foreign manifest";
    fs::write(&effects.manifest_path, foreign).unwrap();
    let mut guard = fixture.guard();
    let error = apply_setup(
        fixture.plan.clone(),
        fixture.grant.clone(),
        &mut guard,
        &mut effects,
        1500,
    )
    .unwrap_err();
    assert_eq!(error.reason, CustomerReason::RecoveryRequired);
    assert!(fixture.origin().exists());
    assert_eq!(fs::read(&effects.manifest_path).unwrap(), foreign);
    let record = fs::read_to_string(
        guard
            .paths()
            .transactions_root()
            .join("setup-transaction.json"),
    )
    .unwrap();
    assert!(record.contains("applying"));
    assert!(
        apply_setup(
            fixture.plan.clone(),
            fixture.grant.clone(),
            &mut guard,
            &mut effects,
            1501
        )
        .is_err()
    );
}

#[test]
fn native_preparation_receipt_rejects_missing_replaced_or_foreign_data() {
    let fixture = Fixture::new();
    let mut effects = fixture.effects();
    effects.prepare_new(&fixture.profile).unwrap();
    effects.create_owned_origin(&fixture.plan).unwrap();
    let held = effects
        .held_origin
        .as_ref()
        .unwrap()
        .metadata()
        .unwrap()
        .ino();
    fs::remove_file(fixture.origin()).unwrap();
    assert!(effects.owned_origin_count(&fixture.plan).is_err());
    fs::write(fixture.origin(), vec![0; 8192]).unwrap();
    assert_ne!(held, fs::metadata(fixture.origin()).unwrap().ino());
    assert!(effects.owned_origin_count(&fixture.plan).is_err());
    let mut manifest = effects.manifest.clone().unwrap();
    manifest.approval_id = "foreign".into();
    fs::write(&effects.manifest_path, manifest.encode().unwrap()).unwrap();
    assert_eq!(
        effects.read_manifest().unwrap_err().reason,
        CustomerReason::IdentityChanged
    );
    fs::write(&effects.manifest_path, "broken = [").unwrap();
    assert_eq!(
        effects.read_manifest().unwrap_err().reason,
        CustomerReason::RecoveryRequired
    );
}

#[test]
fn native_preparation_rejects_platform_shape_and_mount_drift() {
    let fixture = Fixture::new();
    let mut snapshot = super::super::tests::fixture_snapshot();
    assert!(
        NativePreparation::from_observation(
            &fixture.plan,
            &fixture.grant,
            &fixture.profile,
            &snapshot,
            &fixture.config,
            rustix::process::geteuid().as_raw(),
            PreparationMode::Prepare,
        )
        .is_err()
    );
    snapshot.platform = RuntimePlatform::NativeLinux;
    snapshot.windows = None;
    let mut empty = fixture.profile.clone();
    empty.targets.clear();
    assert!(
        NativePreparation::from_observation(
            &fixture.plan,
            &fixture.grant,
            &empty,
            &snapshot,
            &fixture.config,
            rustix::process::geteuid().as_raw(),
            PreparationMode::Prepare,
        )
        .is_err()
    );
    let mut device = super::super::tests::fixture_block_device();
    device.mounts = vec![fixture.mount.clone()];
    device.major_minor = Some(fixture.mount.major_minor.clone());
    device.mounts[0].capacity_observed_unix_ms = Some(super::super::unix_millis());
    snapshot.block_devices = vec![device];
    snapshot.block_devices[0].mounts[0].mount_id += 1;
    assert!(
        NativePreparation::from_observation(
            &fixture.plan,
            &fixture.grant,
            &fixture.profile,
            &snapshot,
            &fixture.config,
            rustix::process::geteuid().as_raw(),
            PreparationMode::Prepare,
        )
        .is_err()
    );
}
#[test]
fn native_preparation_constructor_reopens_owned_manifest_without_double_charging() {
    let fixture = Fixture::new();
    let mut first = fixture.effects();
    first.prepare_new(&fixture.profile).unwrap();
    first.create_owned_origin(&fixture.plan).unwrap();
    let mut snapshot = super::super::tests::fixture_snapshot();
    snapshot.platform = RuntimePlatform::NativeLinux;
    snapshot.windows = None;
    let mut device = super::super::tests::fixture_block_device();
    device.mounts = vec![fixture.mount.clone()];
    device.major_minor = Some(fixture.mount.major_minor.clone());
    device.mounts[0].capacity_observed_unix_ms = Some(super::super::unix_millis());
    snapshot.block_devices = vec![device];
    let reopened = NativePreparation::from_observation(
        &fixture.plan,
        &fixture.grant,
        &fixture.profile,
        &snapshot,
        &fixture.config,
        rustix::process::geteuid().as_raw(),
        PreparationMode::Prepare,
    )
    .unwrap();
    assert_eq!(reopened.owned_origin_count(&fixture.plan).unwrap(), 1);
    assert!(reopened.allocation.is_none());
    let mut changed = fixture.plan.clone();
    changed.release_version = "different".into();
    assert!(reopened.owned_origin_count(&changed).is_err());
    fs::write(reopened.manifest_path, "corrupted = [").unwrap();
    assert!(
        NativePreparation::from_observation(
            &fixture.plan,
            &fixture.grant,
            &fixture.profile,
            &snapshot,
            &fixture.config,
            rustix::process::geteuid().as_raw(),
            PreparationMode::Prepare,
        )
        .is_err()
    );
}
#[test]
fn native_preparation_rechecks_capacity_and_request_authority_before_creation() {
    let fixture = Fixture::new();
    let mut effects = fixture.effects();
    assert!(effects.create_owned_origin(&fixture.plan).is_err());
    assert!(!fixture.origin().exists());
    effects.prepare_new(&fixture.profile).unwrap();
    effects.observe_free = |_| Ok(0);
    assert!(effects.create_owned_origin(&fixture.plan).is_err());
    assert!(!fixture.origin().exists());
    effects.observe_free = |_| Err("capacity observation lost".into());
    assert!(effects.prepare_new(&fixture.profile).is_err());
    let mut empty = fixture.profile.clone();
    empty.targets.clear();
    assert!(
        observe(
            &fixture.plan,
            &fixture.grant,
            &empty,
            PreparationMode::Prepare
        )
        .is_err()
    );
    assert!(!fixture.origin().exists());
    let mut changed = fixture.plan.clone();
    changed.release_version = "changed".into();
    assert_eq!(
        effects.create_owned_origin(&changed).unwrap_err().reason,
        CustomerReason::IdentityChanged
    );
    effects.request = ResourceTarget::LinuxSwapfile {
        filesystem_uuid: "fs".into(),
        device_identity: "dev".into(),
        managed_relative_path: "swap/file".into(),
        bytes: 8192,
        priority: 0,
    };
    assert!(effects.prepare_new(&fixture.profile).is_err());
}
