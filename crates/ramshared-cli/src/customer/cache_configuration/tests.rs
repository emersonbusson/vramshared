// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/cache_configuration/tests.rs
//! Issuer contracts with synthetic native/GPU observations and real journal I/O.
#![allow(clippy::unwrap_used)]
use super::*;
use crate::customer::setup_scope::CachePreparationPlan;
use crate::customer::{
    gpu_plan::{GpuPreparationPlan, tests::fixture_budget},
    gpu_set::CacheAdapterPlan,
};
use crate::readiness::{Decision, published_members::PublishedGpuCapacities};
use crate::readiness::{SurfaceSelection, TransportKind, capacity::effective_capacity, probes::*};
use ramshared_config::native_origin::identity_hash;
use ramshared_tier::nbd_readiness::LowerTierSink;
use ramshared_tier::nbd_readiness::minimum_lower_tier_bytes;
use std::os::unix::fs::PermissionsExt;
const GIB: u64 = 1 << 30;
struct Fixture {
    plan: SetupPlan,
    approval: ApprovalBinding,
    origin: ResourceTarget,
    image: (PathBuf, String),
    report: CheckReport,
    lower: CapacitySample,
}
impl Fixture {
    fn new() -> Self {
        let budgets = [fixture_budget(6 * GIB, 0), {
            let mut b = fixture_budget(96 * GIB, 0);
            b.adapter.as_mut().unwrap().key = "f".repeat(32);
            b
        }];
        let mut gpus = Vec::new();
        for budget in &budgets {
            let mut gpu = GpuPreparationPlan::capture(
                &effective_capacity(Some(budget), 1001, Some(GpuProviderKind::Cuda)),
                &Default::default(),
            )
            .unwrap();
            gpu.requested_ceiling_bytes = Some(GIB);
            gpu.target_bytes = GIB;
            gpus.push(CacheAdapterPlan::new(gpu));
        }
        let cache = CachePreparationPlan {
            schema_version: super::super::setup_scope::CACHE_PLAN_SCHEMA_VERSION,
            gpus,
            required_lower_sink_bytes: minimum_lower_tier_bytes(2 * GIB).unwrap(),
            lower_sink_identity: "fixture-sink".into(),
        };
        let plan = SetupPlan {
            operation: SetupOperation::GpuCache {
                cache: Box::new(cache),
            },
            release_version: "fixture-release".into(),
            profile_schema_version: 1,
            profile_fingerprint: "b".repeat(64),
            report_decision: Decision::Ready,
        };
        let approval = ApprovalBinding {
            approval_id: "approval-1".into(),
            plan_fingerprint: plan.fingerprint(),
            issued_unix: 1,
            expires_unix: 10,
        };
        let mut origin = ResourceTarget::LinuxFileOrigin {
            filesystem_uuid: "fixture-fs".into(),
            device_identity: "fixture-device".into(),
            managed_relative_path: "origin/cache.img".into(),
            inode: 42,
            allocated_bytes: 16 * GIB,
            identity_field_hash: String::new(),
        };
        let hash = identity_hash(&origin).unwrap();
        if let ResourceTarget::LinuxFileOrigin {
            identity_field_hash,
            ..
        } = &mut origin
        {
            *identity_field_hash = hash;
        }
        let report = CheckReport {
            platform: PlatformProbe {
                platform: Observation::Measured(PlatformKind::NativeLinux),
                release: "fixture-linux".into(),
                version: "fixture".into(),
            },
            swaps: Observation::Measured(vec![]),
            kernel: KernelFeatures {
                config_source: None,
                swap: Some(KernelConfig::BuiltIn),
                io_uring: Some(KernelConfig::BuiltIn),
                io_uring_runtime: Some(IoUringRuntime::Enabled),
                nbd: Some(KernelConfig::Module),
                ublk: Some(KernelConfig::Module),
                zram: Some(KernelConfig::Module),
            },
            gpu: GpuProviderProbe {
                kind: Observation::Measured(GpuProviderKind::Cuda),
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
                gpu_backend: Observation::Measured(GpuProviderKind::Cuda),
            },
            capacity: effective_capacity(None, 1001, None),
            gpu_capacities: crate::readiness::published_members::from_observations(&budgets, 1001),
            blockers: vec![],
            warnings: vec![],
        };
        Self {
            plan,
            approval,
            origin,
            image: (
                "/opt/ramshared/releases/fixture/bin/ramsharedd".into(),
                "c".repeat(64),
            ),
            report,
            lower: CapacitySample::Observed {
                sink: LowerTierSink::Known("fixture-sink".into()),
                free_absorbable_bytes: 64 * GIB,
                alignment_bytes: 4096,
            },
        }
    }
    fn request(&self) -> Issuance<'_> {
        Issuance {
            plan: &self.plan,
            approval: &self.approval,
            origin: &self.origin,
            image: &self.image,
        }
    }
    fn prepare(
        &self,
        old: Option<&CacheConfiguration>,
    ) -> Result<CacheConfiguration, CustomerError> {
        prepare(&self.request(), old, &self.report, &self.lower, 1001)
    }
}

#[test]
fn cache_apply_revalidates_every_member_and_lower_sink() {
    let f = Fixture::new();
    let policy = f.prepare(None).unwrap();
    assert_eq!(policy.members.len(), 2);
    assert_eq!(policy.generation, 1);
    for change in 0..7 {
        let mut f = Fixture::new();
        match change {
            0 => f.report.blockers.push("blocked".into()),
            1 => f.report.platform.platform = Observation::Measured(PlatformKind::Wsl2),
            2 => f.lower = CapacitySample::Unknown,
            3 => {
                if let CapacitySample::Observed { sink, .. } = &mut f.lower {
                    *sink = LowerTierSink::Known("foreign".into());
                }
            }
            4 => {
                if let PublishedGpuCapacities::OffObserved(rows) = &mut f.report.gpu_capacities {
                    rows.pop();
                }
            }
            5 => f.report.gpu_capacities = PublishedGpuCapacities::Unavailable("unavailable"),
            _ => f.approval.expires_unix = 1,
        }
        assert!(f.prepare(None).is_err(), "refused mutation {change}");
    }
    let mut f = Fixture::new();
    f.approval.plan_fingerprint = "e".repeat(64);
    assert!(f.prepare(None).is_err());
}

#[test]
fn cache_apply_is_idempotent_and_refuses_stale_approval_or_held_generation() {
    use super::super::transition::{TransitionPaths, acquire_transition};
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .join("local/tests/cache_issuer")
        .join(format!(
            "{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let f = Fixture::new();
    let paths = TransitionPaths::under(&root);
    let guard = acquire_transition(OperationOwner::Setup, &paths).unwrap();
    let path = root.join("generation.lock");
    let owner = rustix::process::geteuid().as_raw();
    let held = ramshared_exec::inherited_lease::acquire(&path, owner).unwrap();
    assert!(ramshared_exec::inherited_lease::acquire(&path, owner).is_err());
    let result = issue(
        &f.request(),
        None,
        false,
        || f.prepare(None),
        |policy| commit_with(policy, &guard, &held, &path, owner),
    )
    .unwrap();
    assert_eq!(result["configuration_generation"], 1);
    assert_eq!(result["activation"], "not_requested");
    let old = previous(&guard).unwrap().unwrap();
    let before = std::fs::read(paths.transactions_root().join(CONFIGURATION_NAME)).unwrap();
    for reconcile in [false, true] {
        let replay = issue(
            &f.request(),
            Some(&old),
            reconcile,
            || panic!("idempotent replay cannot re-observe or allocate"),
            |_| panic!("idempotent replay cannot write"),
        )
        .unwrap();
        assert_eq!(replay, result);
    }
    assert_eq!(
        std::fs::read(paths.transactions_root().join(CONFIGURATION_NAME)).unwrap(),
        before
    );
    assert!(f.prepare(Some(&old)).is_err());
    let mut foreign = old.clone();
    foreign.daemon_sha256 = "d".repeat(64);
    assert!(
        issue(
            &f.request(),
            Some(&foreign),
            false,
            || panic!("image mismatch"),
            |_| panic!("image mismatch")
        )
        .is_err()
    );
    assert!(
        issue(
            &f.request(),
            None,
            true,
            || panic!("no committed receipt"),
            |_| panic!("no committed receipt")
        )
        .is_err()
    );
    std::fs::write(paths.transactions_root().join(CONFIGURATION_NAME), "broken").unwrap();
    assert!(previous(&guard).is_err());
    drop(guard);
    drop(held);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn cache_apply_with_lease_bundle_and_owner_refusals() {
    use super::super::transition::{TransitionPaths, acquire_transition};
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .join("local/tests/cache_issuer_host")
        .join(format!(
            "{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();

    // Sealed release bundle lookup via installed_daemon_at
    assert!(installed_daemon_at(Path::new("ramshared")).is_err());
    let release = root.join("release");
    let bin = release.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    assert!(installed_daemon_at(&bin.join("ramshared")).is_err());
    std::fs::write(bin.join("ramshared"), b"cli").unwrap();
    std::fs::write(bin.join("ramsharedd"), b"daemon").unwrap();
    for file in crate::release_identity::SEALED_IDENTITY_FILES {
        std::fs::write(release.join(file), format!("{file}\n")).unwrap();
    }
    let mut covered = vec!["bin/ramshared", "bin/ramsharedd"];
    covered.extend_from_slice(crate::release_identity::SEALED_IDENTITY_FILES);
    let mut manifest = String::new();
    for rel in covered {
        let hash = crate::installed_image::sha256_file(&release.join(rel)).unwrap();
        manifest.push_str(&format!("{hash}  ./{rel}\n"));
    }
    std::fs::write(release.join("SHA256SUMS"), &manifest).unwrap();
    let manifest_hash = crate::installed_image::sha256_file(&release.join("SHA256SUMS")).unwrap();
    std::fs::write(release.join("INSTALLED_MANIFEST_SHA256"), manifest_hash).unwrap();
    let (daemon_path, daemon_hash) = installed_daemon_at(&bin.join("ramshared")).unwrap();
    assert_eq!(
        daemon_path,
        std::fs::canonicalize(bin.join("ramsharedd")).unwrap()
    );
    assert_eq!(
        daemon_hash,
        crate::installed_image::sha256_file(&bin.join("ramsharedd")).unwrap()
    );

    // Lease acquisition and apply_with lifecycle
    let owner = rustix::process::geteuid().as_raw();
    let lease_file = root.join("generation.lock");
    let lease = acquire_generation_at(&lease_file, owner).unwrap();
    assert_eq!(
        acquire_generation_at(&lease_file, owner)
            .unwrap_err()
            .reason,
        CustomerReason::ActiveSession
    );
    assert!(acquire_generation_at(&root.join("missing/lease"), owner).is_err());

    struct MockHost {
        origin: Option<ResourceTarget>,
        image: (PathBuf, String),
        lease_path: PathBuf,
        owner: u32,
        gate_ok: bool,
    }
    impl IssuanceHost for MockHost {
        fn origin(&self) -> Option<&ResourceTarget> {
            self.origin.as_ref()
        }
        fn image(&self) -> Result<(PathBuf, String), CustomerError> {
            Ok(self.image.clone())
        }
        fn readiness(&self) -> (CheckReport, CapacitySample, u64) {
            let f = Fixture::new();
            (f.report, f.lower, 1001)
        }
        fn gate_commit(&self, _: &SetupPlan) -> Result<(), CustomerError> {
            if self.gate_ok {
                Ok(())
            } else {
                Err(confinement_refusal("mock gate refusal"))
            }
        }
        fn lease_target(&self) -> (&Path, u32) {
            (&self.lease_path, self.owner)
        }
    }

    let f = Fixture::new();
    let paths = TransitionPaths::under(&root);
    let guard = acquire_transition(OperationOwner::Setup, &paths).unwrap();
    let empty_profile = ResourceProfile::parse("schema_version = 1\n").unwrap();
    let mut matched_profile = ResourceProfile::parse("schema_version = 1\n").unwrap();
    matched_profile.targets = vec![f.origin.clone()];

    let mut host = MockHost {
        origin: None,
        image: (daemon_path, daemon_hash),
        lease_path: lease_file.clone(),
        owner,
        gate_ok: false,
    };
    assert_eq!(
        apply_with(
            &f.plan,
            &f.approval,
            &matched_profile,
            &guard,
            &lease,
            false,
            &host
        )
        .unwrap_err()
        .reason,
        CustomerReason::IdentityChanged
    );
    host.origin = Some(f.origin.clone());
    assert_eq!(
        apply_with(
            &f.plan,
            &f.approval,
            &empty_profile,
            &guard,
            &lease,
            false,
            &host
        )
        .unwrap_err()
        .reason,
        CustomerReason::IdentityChanged
    );
    assert!(
        apply_with(
            &f.plan,
            &f.approval,
            &matched_profile,
            &guard,
            &lease,
            false,
            &host
        )
        .is_err()
    );
    assert!(previous(&guard).unwrap().is_none());

    host.gate_ok = true;
    let first = apply_with(
        &f.plan,
        &f.approval,
        &matched_profile,
        &guard,
        &lease,
        false,
        &host,
    )
    .unwrap();
    assert_eq!(first["configuration_generation"], 1);
    for reconcile in [false, true] {
        let replay = apply_with(
            &f.plan,
            &f.approval,
            &matched_profile,
            &guard,
            &lease,
            reconcile,
            &host,
        )
        .unwrap();
        assert_eq!(replay, first);
    }

    // Wrong approval fingerprint in issue, StorageOnly in prepare, wrong owner in commit_with, symlink read-back
    let mut bad_approval = f.approval.clone();
    bad_approval.plan_fingerprint = "e".repeat(64);
    assert!(
        issue(
            &Issuance {
                plan: &f.plan,
                approval: &bad_approval,
                origin: &f.origin,
                image: &f.image,
            },
            None,
            false,
            || panic!("plan mismatch"),
            |_| panic!("plan mismatch"),
        )
        .is_err()
    );
    let mut storage_plan = f.plan.clone();
    storage_plan.operation = SetupOperation::StorageOnly {};
    let mut storage_approval = f.approval.clone();
    storage_approval.plan_fingerprint = storage_plan.fingerprint();
    assert_eq!(
        prepare(
            &Issuance {
                plan: &storage_plan,
                approval: &storage_approval,
                origin: &f.origin,
                image: &f.image,
            },
            None,
            &f.report,
            &f.lower,
            1001,
        )
        .unwrap_err()
        .reason,
        CustomerReason::UnsupportedSurface
    );

    let policy = previous(&guard).unwrap().unwrap();
    drop(guard);
    let up_guard = acquire_transition(OperationOwner::Up, &paths).unwrap();
    assert_eq!(
        commit_with(&policy, &up_guard, &lease, &lease_file, owner)
            .unwrap_err()
            .reason,
        CustomerReason::UnsupportedSurface
    );
    drop(up_guard);

    let record_path = paths.transactions_root().join(CONFIGURATION_NAME);
    std::fs::remove_file(&record_path).unwrap();
    std::os::unix::fs::symlink(root.join("missing-target"), &record_path).unwrap();
    let check_guard = acquire_transition(OperationOwner::Setup, &paths).unwrap();
    assert!(previous(&check_guard).is_err());

    let production_host = ProductionHost(Some(&f.origin));
    assert_eq!(production_host.origin(), Some(&f.origin));
    assert_eq!(
        production_host.lease_target(),
        (ramshared_config::cache_configuration::lease_path(), 0)
    );
    assert!(production_host.gate_commit(&f.plan).is_err());
    let _ = installed_daemon();
    if rustix::process::geteuid().as_raw() != 0 {
        assert_eq!(
            acquire_generation().unwrap_err().reason,
            CustomerReason::ActiveSession
        );
    }
    if rustix::process::geteuid().as_raw() != 0
        || crate::readiness::probes::probe_platform().platform
            != Observation::Measured(PlatformKind::NativeLinux)
    {
        assert!(
            apply(
                &f.plan,
                &f.approval,
                &matched_profile,
                &check_guard,
                &lease,
                false
            )
            .is_err()
        );
    }
    drop(check_guard);
    drop(lease);
    std::fs::remove_dir_all(root).unwrap();
}
