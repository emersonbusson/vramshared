// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/plan/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::readiness::capacity::CapacityObservation;
use crate::readiness::probes::{
    BackendProbe, GpuProviderKind, GpuProviderProbe, IoUringRuntime, KernelConfig, KernelFeatures,
    Observation, PlatformKind, PlatformProbe, Status, SysctlTuning,
};
use crate::readiness::{SurfaceSelection, TransportKind};

use super::super::gpu_plan::GpuPreparationPlan;
use super::super::setup_scope::CachePreparationPlan;
use ramshared_tier::nbd_readiness::{LowerTierSink, minimum_lower_tier_bytes};
use ramshared_vram::{RUNTIME_FREE_BUFFER_BYTES, ReserveFloorPolicy};

fn cache(plan: &SetupPlan) -> &CachePreparationPlan {
    let SetupOperation::GpuCache { cache } = &plan.operation else {
        panic!("cache scope expected")
    };
    cache
}

fn plan_setup(
    report: &CheckReport,
    profile: &ResourceProfile,
    profile_fingerprint: &str,
    release: &ReleaseIdentity,
    lower_sink: &CapacitySample,
) -> Result<SetupPlan, CustomerError> {
    super::plan_setup(
        SetupScope::GpuCache,
        SetupInputs {
            report,
            profile,
            profile_fingerprint,
            release,
            lower_sink,
        },
    )
}

fn mib(value: u64) -> u64 {
    value * 1024 * 1024
}

/// A ready report whose capacity is exactly `capacity_bytes`.
///
/// Constructed field-by-field on the one typed report
/// [`crate::readiness::CheckReport`] so the plan reads the real type
/// rather than a lookalike.
fn ready_report(capacity_bytes: u64) -> CheckReport {
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
        capacity: crate::readiness::capacity::effective_capacity(
            Some(&crate::customer::gpu_plan::tests::fixture_budget(
                capacity_bytes,
                0,
            )),
            1001,
            Some(GpuProviderKind::Cuda),
        ),
        blockers: Vec::new(),
        warnings: Vec::new(),
    }
}

fn empty_profile() -> ResourceProfile {
    ResourceProfile {
        schema_version: 1,
        planned_caps: Default::default(),
        targets: Vec::new(),
    }
}

fn observed_sink(free_absorbable_bytes: u64) -> CapacitySample {
    CapacitySample::Observed {
        sink: LowerTierSink::Known("lab-lower-sink".to_string()),
        free_absorbable_bytes,
        alignment_bytes: 4096,
    }
}

#[test]
fn setup_preserves_canonical_reserve_and_lower_sink_floor() {
    let capacity = mib(6 * 1024);
    let report = ready_report(capacity);
    let profile = empty_profile();
    let release = ReleaseIdentity::new("0.15.0");

    // The lower sink is measured at exactly the requirement: a plan is
    // produced and must record the canonical numbers, not tuned-down ones.
    let target = GpuPreparationPlan::capture(&report.capacity, &profile.planned_caps.vram_bytes)
        .unwrap()
        .target_bytes;
    let required = minimum_lower_tier_bytes(target).unwrap();
    let aligned_required = required.div_ceil(4096) * 4096;
    let plan = plan_setup(
        &report,
        &profile,
        "profile-fingerprint",
        &release,
        &observed_sink(aligned_required),
    )
    .expect("a plan is produced when every margin is satisfied");

    let policy = ReserveFloorPolicy::sealed();
    assert_eq!(
        cache(&plan).gpus[0].configured_reserve_bytes,
        policy.configured_reserve_bytes(capacity),
        "the canonical configured reserve must be preserved, not re-derived"
    );
    assert_eq!(
        cache(&plan).gpus[0].runtime_free_buffer_bytes,
        RUNTIME_FREE_BUFFER_BYTES,
        "cache admission retains the shared runtime buffer independently of the display share"
    );
    assert_eq!(
        cache(&plan).required_lower_sink_bytes,
        required,
        "the lower-sink requirement must be the NBD demotion formula's"
    );
    assert_eq!(cache(&plan).lower_sink_identity, "lab-lower-sink");
    assert_eq!(plan.release_version, "0.15.0");

    // A short lower sink must not be accepted by lowering the floor.
    let short = aligned_required.saturating_sub(4096);
    assert!(short < required, "the refusal fixture must really be short");
    let refused_short = plan_setup(
        &report,
        &profile,
        "profile-fingerprint",
        &release,
        &observed_sink(short),
    )
    .expect_err("a short lower sink must be refused");
    assert_eq!(refused_short.reason, CustomerReason::CapacityRefused);
    assert_eq!(refused_short.state, CustomerState::InstalledOff);

    // Unknown capacity is never a zero-capacity plan.
    let mut unknown = ready_report(capacity);
    unknown.capacity = CapacityObservation::absent(None);
    let refused_unknown = plan_setup(
        &unknown,
        &profile,
        "profile-fingerprint",
        &release,
        &observed_sink(aligned_required),
    )
    .expect_err("missing capacity must be refused, not planned as zero");
    assert_eq!(
        refused_unknown.reason,
        CustomerReason::ObservationUnavailable
    );
    assert_eq!(refused_unknown.state, CustomerState::UnknownBlocked);

    // Unknown and stale lower-sink samples are refusals, not zeros.
    for sample in [CapacitySample::Unknown, CapacitySample::Stale] {
        let refused_sample =
            plan_setup(&report, &profile, "profile-fingerprint", &release, &sample)
                .expect_err("a lower-sink sample that cannot be measured is a refusal");
        assert_eq!(refused_sample.reason, CustomerReason::CapacityRefused);
    }

    // Desired GPU ceilings need same-adapter admission, not an inference
    // from a headroom-only readiness summary.
    let mut greedy = empty_profile();
    greedy
        .planned_caps
        .vram_bytes
        .insert("fixture-adapter".into(), capacity);
    let refused_greedy = plan_setup(
        &report,
        &greedy,
        "profile-fingerprint",
        &release,
        &observed_sink(aligned_required),
    )
    .expect_err("unbound desired GPU ceilings cannot receive an apply plan");
    assert_eq!(refused_greedy.reason, CustomerReason::IdentityChanged);

    // Storage targets remain part of profile identity, but their sizes
    // are admitted against storage by the resource owner, not VRAM.
    let mut every_kind = empty_profile();
    every_kind.targets.push(ResourceTarget::LinuxSwapfile {
        filesystem_uuid: "uuid".to_string(),
        device_identity: "dev".to_string(),
        managed_relative_path: "swapfile".to_string(),
        bytes: 4096,
        priority: 100,
    });
    every_kind.targets.push(ResourceTarget::LinuxFileOrigin {
        filesystem_uuid: "uuid".to_string(),
        device_identity: "dev".to_string(),
        managed_relative_path: "origin".to_string(),
        inode: 1,
        allocated_bytes: 4096,
        identity_field_hash: "a".repeat(64),
    });
    every_kind
        .targets
        .push(ResourceTarget::LinuxFileOriginRequest {
            filesystem_uuid: "uuid".to_string(),
            device_identity: "dev".to_string(),
            managed_relative_path: "origin-request".to_string(),
            allocated_bytes: 4096,
        });
    let every_kind_plan = plan_setup(
        &report,
        &every_kind,
        "profile-fingerprint",
        &release,
        &observed_sink(aligned_required),
    )
    .expect("every target kind fits the floors at lab sizes");
    assert_eq!(
        cache(&every_kind_plan).lower_sink_identity,
        "lab-lower-sink"
    );

    // A blocked report is not a source of setup arithmetic.
    let mut blocked = ready_report(capacity);
    blocked.blockers.push("lab blocker".to_string());
    let refused_blocked = plan_setup(
        &blocked,
        &profile,
        "profile-fingerprint",
        &release,
        &observed_sink(aligned_required),
    )
    .expect_err("a blocked report must not produce a plan");
    assert_eq!(refused_blocked.reason, CustomerReason::UnsupportedSurface);
    assert_eq!(refused_blocked.state, CustomerState::InstalledOff);

    // Zero effective capacity is a capacity refusal, not an overflow in
    // the demotion formula and not a plan with zeroed floors.
    let mut zero = ready_report(capacity);
    zero.capacity = crate::readiness::capacity::effective_capacity(
        Some(&crate::customer::gpu_plan::tests::fixture_budget(0, 0)),
        1001,
        Some(GpuProviderKind::Cuda),
    );
    let refused_zero = plan_setup(
        &zero,
        &profile,
        "profile-fingerprint",
        &release,
        &observed_sink(aligned_required),
    )
    .expect_err("zero effective capacity must be refused");
    assert_eq!(refused_zero.reason, CustomerReason::CapacityRefused);

    // A capacity the demotion formula cannot represent is a refusal at
    // the formula, never a wrapped or saturated floor.
    let mut unrepresentable = ready_report(capacity);
    unrepresentable.capacity = crate::readiness::capacity::effective_capacity(
        Some(&crate::customer::gpu_plan::tests::fixture_budget(
            u64::MAX,
            0,
        )),
        1001,
        Some(GpuProviderKind::Cuda),
    );
    let refused_overflow = plan_setup(
        &unrepresentable,
        &profile,
        "profile-fingerprint",
        &release,
        &observed_sink(aligned_required),
    )
    .expect_err("a capacity the lower-sink formula cannot represent is refused");
    assert_eq!(refused_overflow.reason, CustomerReason::CapacityRefused);
}

#[test]
fn plan_rejects_oversize_unknown_fields_and_replayed_approval() {
    let consumed = BTreeSet::from(["already-used".to_string()]);

    // Oversize document.
    let oversize = format!(
        "{{\"approval_id\":\"{}\",\"plan_fingerprint\":\"fp\",\"issued_unix\":1,\"expires_unix\":2}}",
        "a".repeat(MAX_RESOURCE_PROFILE_BYTES)
    );
    let refused_oversize =
        parse_approval(&oversize, &consumed).expect_err("an oversize approval document is refused");
    assert_eq!(refused_oversize.reason, CustomerReason::UnsupportedSurface);

    // Identifier bounds are checked after the strict schema, so an empty
    // or oversized approval_id is a refusal on this surface and never a
    // binding that later collides in the consumed set.
    for approval_id in ["", &"a".repeat(MAX_APPROVAL_ID_BYTES + 1)] {
        let document = format!(
            "{{\"approval_id\":\"{approval_id}\",\"plan_fingerprint\":\"fp\",\"issued_unix\":1,\"expires_unix\":2}}"
        );
        let refused_id = parse_approval(&document, &consumed)
            .expect_err("an approval_id outside the bound is refused");
        assert_eq!(refused_id.reason, CustomerReason::UnsupportedSurface);
        assert_eq!(refused_id.state, CustomerState::InstalledOff);
    }

    // The plan fingerprint is the identity the binding authorizes. An
    // empty or oversized one cannot name one plan.
    for plan_fingerprint in ["", &"f".repeat(129)] {
        let document = format!(
            "{{\"approval_id\":\"a1\",\"plan_fingerprint\":\"{plan_fingerprint}\",\"issued_unix\":1,\"expires_unix\":2}}"
        );
        let refused_fp = parse_approval(&document, &consumed)
            .expect_err("a plan_fingerprint outside the bound is refused");
        assert_eq!(refused_fp.reason, CustomerReason::UnsupportedSurface);
        assert_eq!(refused_fp.state, CustomerState::InstalledOff);
    }

    // Unknown field — strict schema, not "ignore what you do not know".
    let unknown_field = r#"{"approval_id":"a1","plan_fingerprint":"fp","issued_unix":1,"expires_unix":2,"extra":true}"#;
    let refused_unknown =
        parse_approval(unknown_field, &consumed).expect_err("an unknown field is refused");
    assert_eq!(refused_unknown.reason, CustomerReason::UnsupportedSurface);

    // Malformed document on the new surface reports through the customer
    // model, never a bare errno.
    let refused_malformed = parse_approval("not-json", &consumed)
        .expect_err("a malformed approval document is refused");
    assert_eq!(refused_malformed.reason, CustomerReason::UnsupportedSurface);

    // Non-positive lifetime.
    let expired =
        r#"{"approval_id":"a1","plan_fingerprint":"fp","issued_unix":5,"expires_unix":5}"#;
    let refused_expired = parse_approval(expired, &consumed)
        .expect_err("an approval that expires at issue is refused");
    assert_eq!(refused_expired.reason, CustomerReason::UnsupportedSurface);

    // Replay of a consumed approval: identity_changed, no new effects.
    let replay = r#"{"approval_id":"already-used","plan_fingerprint":"fp","issued_unix":1,"expires_unix":2}"#;
    let refused_replay =
        parse_approval(replay, &consumed).expect_err("a replayed approval is refused");
    assert_eq!(refused_replay.reason, CustomerReason::IdentityChanged);
    assert_eq!(refused_replay.state, CustomerState::InstalledOff);

    // A fresh unused approval is admitted, and it binds to exactly one plan.
    let fresh = r#"{"approval_id":"a2","plan_fingerprint":"fp","issued_unix":1,"expires_unix":2}"#;
    let binding = parse_approval(fresh, &consumed).expect("a fresh approval is admitted");
    assert_eq!(binding.approval_id, "a2");
    assert_eq!(binding.plan_fingerprint, "fp");

    let report = ready_report(mib(6 * 1024));
    let profile = empty_profile();
    let release = ReleaseIdentity::new("0.15.0");
    let capacity = mib(6 * 1024);
    let required = minimum_lower_tier_bytes(capacity).unwrap();
    let aligned = required
        .checked_add(4096 - (required % 4096))
        .unwrap_or(required);
    let plan = plan_setup(
        &report,
        &profile,
        "profile-fingerprint",
        &release,
        &observed_sink(aligned),
    )
    .expect("plan");

    assert!(
        !binding_matches(&binding, &plan, 1),
        "a binding for a different plan must not authorize this one"
    );

    let bound = ApprovalBinding {
        approval_id: "a3".to_string(),
        plan_fingerprint: plan.fingerprint(),
        issued_unix: 10,
        expires_unix: 20,
    };
    assert!(
        binding_matches(&bound, &plan, 15),
        "in-window binding matches"
    );
    assert!(!binding_matches(&bound, &plan, 9), "not yet issued");
    assert!(!binding_matches(&bound, &plan, 20), "expired at the bound");
}
#[test]
fn setup_plan_separates_disk_allocations_from_gpu_caps() {
    let capacity = mib(6 * 1024);
    let report = ready_report(capacity);
    let sink_bytes = minimum_lower_tier_bytes(capacity)
        .expect("sink requirement")
        .div_ceil(4096)
        * 4096;
    let release = ReleaseIdentity::new("0.15.0");
    let mut profile = empty_profile();
    profile
        .targets
        .push(ResourceTarget::LinuxFileOriginRequest {
            filesystem_uuid: "local-fs".into(),
            device_identity: "local-device".into(),
            managed_relative_path: "origin/large.img".into(),
            allocated_bytes: 128 * 1024 * 1024 * 1024,
        });
    plan_setup(
        &report,
        &profile,
        "profile",
        &release,
        &observed_sink(sink_bytes),
    )
    .expect("disk size does not consume GPU bytes");
    profile
        .planned_caps
        .vram_bytes
        .insert("gpu-a".into(), 4 * 1024 * 1024 * 1024);
    let unbound = plan_setup(
        &report,
        &profile,
        "profile",
        &release,
        &observed_sink(sink_bytes),
    )
    .expect_err("a desired ceiling cannot be bound from anonymous headroom");
    assert_eq!(unbound.reason, CustomerReason::IdentityChanged);
    assert_eq!(unbound.state, CustomerState::InstalledOff);
    profile.planned_caps.vram_bytes.insert("gpu-b".into(), 1);
    assert!(
        plan_setup(
            &report,
            &profile,
            "profile",
            &release,
            &observed_sink(sink_bytes)
        )
        .is_err()
    );
    profile
        .planned_caps
        .vram_bytes
        .insert("gpu-a".into(), u64::MAX);
    let overflow = plan_setup(
        &report,
        &profile,
        "profile",
        &release,
        &observed_sink(sink_bytes),
    )
    .expect_err("checked aggregation fails before adapter admission");
    assert_eq!(overflow.reason, CustomerReason::CapacityRefused);
}
#[test]
fn setup_plan_refuses_unknown_schema_platform_and_invalid_targets() {
    let capacity = mib(6 * 1024);
    let mut report = ready_report(capacity);
    let sink_bytes = minimum_lower_tier_bytes(capacity)
        .expect("sink requirement")
        .div_ceil(4096)
        * 4096;
    let sink = observed_sink(sink_bytes);
    let release = ReleaseIdentity::new("0.15.0");
    let mut profile = empty_profile();
    profile.schema_version = 999;
    assert!(plan_setup(&report, &profile, "profile", &release, &sink).is_err());
    profile = empty_profile();
    profile
        .targets
        .push(ResourceTarget::LinuxFileOriginRequest {
            filesystem_uuid: "fs".into(),
            device_identity: "device".into(),
            managed_relative_path: "../outside".into(),
            allocated_bytes: 4096,
        });
    assert!(plan_setup(&report, &profile, "profile", &release, &sink).is_err());
    profile.targets.clear();
    profile.targets.push(ResourceTarget::WslFallback {
        windows_volume_id: "volume".into(),
        path: "C:\\swap.vhdx".into(),
        bytes: 4096,
    });
    assert!(plan_setup(&report, &profile, "profile", &release, &sink).is_err());
    report.platform.platform = crate::readiness::probes::Observation::Measured(
        crate::readiness::probes::PlatformKind::Wsl2,
    );
    plan_setup(&report, &profile, "profile", &release, &sink)
        .expect("valid WSL profile on WSL report");
    report.platform.platform = crate::readiness::probes::Observation::Unknown;
    let unknown =
        plan_setup(&report, &profile, "profile", &release, &sink).expect_err("unknown platform");
    assert_eq!(unknown.reason, CustomerReason::ObservationUnavailable);
    assert_eq!(unknown.state, CustomerState::UnknownBlocked);
}
#[test]
fn setup_floor_uses_budget_geometry_instead_of_headroom() {
    let gib = 1024 * 1024 * 1024;
    let mut report = ready_report(7 * gib);
    let budget = ramshared_vram::GpuBudgetTelemetry {
        schema_version: 1,
        adapter: Some(ramshared_vram::GpuAdapterIdentity {
            node_mask: None,
            backend: "cuda".into(),
            key: "00112233445566778899aabbccddeeff".into(),
            luid: None,
        }),
        total_bytes: Some(24 * gib),
        budget_bytes: 24 * gib,
        used_bytes: 17 * gib,
        available_bytes: 7 * gib,
        source: ramshared_vram::GpuBudgetSource::DriverReported,
        sampled_at_unix_ms: 1000,
    };
    report.capacity = crate::readiness::capacity::effective_capacity(
        Some(&budget),
        1000,
        Some(GpuProviderKind::Cuda),
    );
    let plan = plan_setup(
        &report,
        &empty_profile(),
        "profile",
        &ReleaseIdentity::new("0.15.0"),
        &observed_sink(32 * gib),
    )
    .expect("captured driver proof");
    assert_eq!(
        cache(&plan).gpus[0].configured_reserve_bytes,
        ReserveFloorPolicy::sealed().configured_reserve_bytes(24 * gib)
    );
}
#[test]
fn cache_plan_consumes_published_members_and_checks_the_combined_sink() {
    let gib = 1024 * 1024 * 1024;
    let mut a = crate::customer::gpu_plan::tests::fixture_budget(96 * gib, 10 * gib);
    let mut b = crate::customer::gpu_plan::tests::fixture_budget(32 * gib, 20 * gib);
    a.adapter.as_mut().unwrap().key = "00112233445566778899aabbccddeeff".into();
    b.adapter.as_mut().unwrap().key = "ffeeddccbbaa99887766554433221100".into();
    let mut profile = empty_profile();
    for (budget, target) in [(&a, 64 * gib), (&b, 2 * gib)] {
        profile.planned_caps.vram_bytes.insert(
            budget
                .adapter
                .as_ref()
                .unwrap()
                .resource_profile_key()
                .unwrap(),
            target,
        );
    }
    let rows:Vec<_>=[&a,&b].iter().map(|budget|serde_json::json!({
        "key":budget.adapter.as_ref().unwrap().key,"state":"ACTIVE","requested_target_bytes":gib,"cached_bytes":0,"failures":0,"adapter":budget.adapter,"budget":budget
    })).collect();
    let document = serde_json::json!({"gpu_budget":a,"cache_members":rows});
    let mut report = ready_report(6 * gib);
    report.gpu_capacities = crate::readiness::published_members::observe(Some(&document), 1001);
    let required = minimum_lower_tier_bytes(66 * gib).unwrap();
    let aligned = required.div_ceil(4096) * 4096;
    let release = ReleaseIdentity::new("0.15.0");
    let plan = plan_setup(
        &report,
        &profile,
        "profile",
        &release,
        &observed_sink(aligned),
    )
    .unwrap();
    assert_eq!(cache(&plan).gpus.len(), 2);
    assert_eq!(
        cache(&plan)
            .gpus
            .iter()
            .map(|m| m.gpu.target_bytes)
            .sum::<u64>(),
        66 * gib
    );
    assert_eq!(cache(&plan).required_lower_sink_bytes, required);
    assert_eq!(
        plan_setup(
            &report,
            &profile,
            "profile",
            &release,
            &observed_sink(required - 1)
        )
        .unwrap_err()
        .reason,
        CustomerReason::CapacityRefused
    );
    report.gpu_capacities = crate::readiness::published_members::observe(
        Some(&serde_json::json!({"gpu_budget":a,"cache_members":null})),
        1001,
    );
    assert_eq!(
        plan_setup(
            &report,
            &profile,
            "profile",
            &release,
            &observed_sink(aligned)
        )
        .unwrap_err()
        .reason,
        CustomerReason::ObservationUnavailable
    );
    report.gpu_capacities = crate::readiness::published_members::observe(Some(&document), 1001);
    report.surface.gpu_backend = Observation::Measured(GpuProviderKind::SoftwareAdapter);
    assert_eq!(
        plan_setup(
            &report,
            &profile,
            "profile",
            &release,
            &observed_sink(aligned)
        )
        .unwrap_err()
        .reason,
        CustomerReason::UnsupportedSurface
    );
}
