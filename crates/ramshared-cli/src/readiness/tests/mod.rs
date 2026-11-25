// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/readiness/tests/mod.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

pub(super) use super::probes::{
    BackendEnv, GpuInfo, GpuProviderProbe, IoUringRuntime, PlatformProbe, Status, SwapEntry,
};

pub(super) use super::*;

pub(super) use ramshared_vram::{GPU_BUDGET_MAX_AGE_MS, GpuAdapterIdentity, GpuBudgetSource};

pub(super) fn platform_probe(kind: Observation<PlatformKind>, release: &str) -> PlatformProbe {
    PlatformProbe {
        platform: kind,
        release: release.to_string(),
        version: String::new(),
    }
}

pub(super) fn kernel_ready() -> KernelFeatures {
    KernelFeatures {
        config_source: Some("/boot/config-test".to_string()),
        swap: Some(KernelConfig::BuiltIn),
        io_uring: Some(KernelConfig::BuiltIn),
        io_uring_runtime: Some(IoUringRuntime::Enabled),
        nbd: Some(KernelConfig::BuiltIn),
        ublk: Some(KernelConfig::Module),
        zram: Some(KernelConfig::Module),
    }
}

pub(super) fn backends_ready() -> BackendProbe {
    BackendProbe {
        nbd_status: Status::Ok,
        nbd_detail: "device-present".to_string(),
        ublk_status: Status::Ok,
        ublk_detail: "ready".to_string(),
    }
}

pub(super) fn gpu_cuda() -> GpuProviderProbe {
    GpuProviderProbe {
        kind: Observation::Measured(GpuProviderKind::Cuda),
        status: Status::Ok,
        libcuda_path: Some("/usr/lib/wsl/lib/libcuda.so.1".to_string()),
        dxg_present: true,
        nvidia_smi_path: Some("/usr/lib/wsl/lib/nvidia-smi".to_string()),
        nvidia_smi_status: Some(0),
        nvidia_smi_output: Some("NVIDIA-SMI 570.00".to_string()),
        gpu: Some(GpuInfo {
            name: "NVIDIA RTX 4090".to_string(),
            total_bytes: 24 * 1024 * 1024 * 1024,
            free_bytes: 20 * 1024 * 1024 * 1024,
        }),
        detail: "ready".to_string(),
    }
}

pub(super) fn gpu_vulkan_hardware() -> GpuProviderProbe {
    GpuProviderProbe {
        kind: Observation::Measured(GpuProviderKind::Vulkan),
        status: Status::Ok,
        libcuda_path: None,
        dxg_present: false,
        nvidia_smi_path: None,
        nvidia_smi_status: None,
        nvidia_smi_output: None,
        gpu: None,
        detail: "ready".to_string(),
    }
}

pub(super) fn gpu_software_adapter() -> GpuProviderProbe {
    GpuProviderProbe {
        kind: Observation::Measured(GpuProviderKind::SoftwareAdapter),
        status: Status::Ok,
        libcuda_path: None,
        dxg_present: false,
        nvidia_smi_path: None,
        nvidia_smi_status: None,
        nvidia_smi_output: None,
        gpu: None,
        detail: "software Vulkan adapter only; not physical VRAM".to_string(),
    }
}

pub(super) fn gpu_unknown() -> GpuProviderProbe {
    GpuProviderProbe {
        kind: Observation::Unknown,
        status: Status::Fail,
        libcuda_path: None,
        dxg_present: false,
        nvidia_smi_path: None,
        nvidia_smi_status: None,
        nvidia_smi_output: None,
        gpu: None,
        detail: "/dev/dxg is absent".to_string(),
    }
}

pub(super) fn sysctl_ok() -> SysctlTuning {
    SysctlTuning {
        min_free_kbytes: Some(524288),
        swappiness: Some(100),
        vfs_cache_pressure: Some(50),
        watermark_boost_factor: Some(0),
        watermark_scale_factor: Some(125),
        page_lock_unfairness: Some(1),
    }
}

pub(super) fn obs(
    platform: PlatformProbe,
    gpu: GpuProviderProbe,
    backends: BackendProbe,
    swaps: Observation<Vec<SwapEntry>>,
) -> Observations {
    Observations {
        platform,
        swaps,
        kernel: kernel_ready(),
        gpu,
        backends,
        sysctl: sysctl_ok(),
    }
}

pub(super) fn driver_budget(sampled_at_unix_ms: u64) -> GpuBudgetTelemetry {
    GpuBudgetTelemetry {
        schema_version: 1,
        adapter: Some(GpuAdapterIdentity {
            node_mask: None,
            backend: "vulkan".to_string(),
            key: "0".to_string(),
            luid: None,
        }),
        total_bytes: Some(8 * 1024 * 1024 * 1024),
        budget_bytes: 4 * 1024 * 1024 * 1024,
        used_bytes: 1024 * 1024 * 1024,
        available_bytes: 3 * 1024 * 1024 * 1024,
        source: GpuBudgetSource::DriverReported,
        sampled_at_unix_ms,
    }
}

/// SPEC §12 · ITEM-2 · AUD-39 · invariant check.
///
/// Native Linux with a hardware Vulkan provider is Ready on its own
/// requirements. It must not grow a "not WSL2" blocker or a "CUDA
/// unavailable" blocker.
#[test]
fn native_linux_vulkan_readiness_does_not_require_wsl_cuda() {
    let report = report_from_observations(obs(
        platform_probe(
            Observation::Measured(PlatformKind::NativeLinux),
            "6.8.0-45-generic",
        ),
        gpu_vulkan_hardware(),
        backends_ready(),
        Observation::Measured(Vec::new()),
    ));

    assert_eq!(
        report.surface.platform,
        Observation::Measured(PlatformKind::NativeLinux)
    );
    assert_eq!(
        report.surface.gpu_backend,
        Observation::Measured(GpuProviderKind::Vulkan)
    );
    assert_eq!(report.surface.transport, TransportKind::Nbd);
    assert_eq!(
        report.decision(),
        Decision::Ready,
        "blockers: {:?}",
        report.blockers
    );

    for blocker in &report.blockers {
        let lowered = blocker.to_lowercase();
        assert!(
            !lowered.contains("wsl"),
            "native Linux must not carry a WSL blocker: {blocker}"
        );
        assert!(
            !lowered.contains("cuda"),
            "Vulkan surface must not carry a CUDA blocker: {blocker}"
        );
    }
}

/// AUD-39. Selection and requirement are the same surface. CUDA requires
/// the WSL GPU stack; Vulkan does not.
#[test]
fn vulkan_selection_does_not_require_cuda() {
    let wsl_vulkan = report_from_observations(obs(
        platform_probe(
            Observation::Measured(PlatformKind::Wsl2),
            "6.6.87.2-microsoft-standard-WSL2",
        ),
        gpu_vulkan_hardware(),
        backends_ready(),
        Observation::Measured(Vec::new()),
    ));
    assert_eq!(
        wsl_vulkan.decision(),
        Decision::Ready,
        "{:?}",
        wsl_vulkan.blockers
    );
    assert!(
        !wsl_vulkan.blockers.iter().any(|b| b.contains("CUDA")),
        "Vulkan selection must not require CUDA: {:?}",
        wsl_vulkan.blockers
    );

    // A CUDA selection still does require it.
    let wsl_cuda = report_from_observations(obs(
        platform_probe(
            Observation::Measured(PlatformKind::Wsl2),
            "6.6.87.2-microsoft-standard-WSL2",
        ),
        GpuProviderProbe {
            kind: Observation::Measured(GpuProviderKind::Cuda),
            status: Status::Fail,
            ..gpu_unknown()
        },
        backends_ready(),
        Observation::Measured(Vec::new()),
    ));
    assert_eq!(wsl_cuda.decision(), Decision::Blocked);
    assert!(
        wsl_cuda
            .blockers
            .iter()
            .any(|b| b.contains("CUDA unavailable"))
    );
}

/// AUD-39. WSL1 is refused for the surfaces this mode serves.
#[test]
fn wsl1_is_refused() {
    let report = report_from_observations(obs(
        platform_probe(
            Observation::Measured(PlatformKind::Wsl1),
            "4.4.0-19041-Microsoft",
        ),
        gpu_cuda(),
        backends_ready(),
        Observation::Measured(Vec::new()),
    ));
    assert_eq!(report.decision(), Decision::Blocked);
    assert!(report.blockers.iter().any(|b| b.contains("WSL1")));
}

/// AUD-39. An unavailable transport is its own blocker, reported against
/// the transport column, not against the GPU or the platform.
#[test]
fn unavailable_transport_blocks_without_blaming_gpu_or_platform() {
    let report = report_from_observations(obs(
        platform_probe(
            Observation::Measured(PlatformKind::Wsl2),
            "6.6.87.2-microsoft-standard-WSL2",
        ),
        gpu_cuda(),
        BackendProbe {
            nbd_status: Status::Fail,
            nbd_detail: "CONFIG_BLK_DEV_NBD disabled or unknown".to_string(),
            ublk_status: Status::Fail,
            ublk_detail: "/dev/ublk-control missing".to_string(),
        },
        Observation::Measured(Vec::new()),
    ));
    assert_eq!(report.surface.transport, TransportKind::Unavailable);
    assert_eq!(report.decision(), Decision::Blocked);
    assert!(
        report
            .blockers
            .iter()
            .any(|b| b.contains("no block backend"))
    );
    assert!(
        !report
            .blockers
            .iter()
            .any(|b| b.contains("CUDA unavailable"))
    );
}

/// AUD-39 / DT-7. A software adapter is never a physical-VRAM claim and
/// cannot back the VRAM tier.
#[test]
fn software_adapter_is_refused_for_physical_vram_claims() {
    let report = report_from_observations(obs(
        platform_probe(
            Observation::Measured(PlatformKind::NativeLinux),
            "6.8.0-45-generic",
        ),
        gpu_software_adapter(),
        backends_ready(),
        Observation::Measured(Vec::new()),
    ));
    assert_eq!(report.decision(), Decision::Blocked);
    assert!(
        report
            .blockers
            .iter()
            .any(|b| b.contains("software adapter"))
    );
    assert!(!report.surface.claims_physical_vram());
}

/// AUD-39. Normal CUDA + WSL2 success path stays Ready.
#[test]
fn normal_cuda_wsl2_check_is_ready() {
    let report = report_from_observations(obs(
        platform_probe(
            Observation::Measured(PlatformKind::Wsl2),
            "6.6.87.2-microsoft-standard-WSL2",
        ),
        gpu_cuda(),
        backends_ready(),
        Observation::Measured(Vec::new()),
    ));
    assert_eq!(report.decision(), Decision::Ready, "{:?}", report.blockers);
    assert_eq!(report.surface.transport, TransportKind::Nbd);
    assert!(report.surface.claims_physical_vram());
}

/// AUD-39. An unclassifiable guest is blocked as unknown, not as a zero
/// capacity or as "not WSL2" alone.
#[test]
fn unknown_platform_is_blocked_as_unknown() {
    let report = report_from_observations(obs(
        platform_probe(Observation::Unknown, ""),
        gpu_unknown(),
        backends_ready(),
        Observation::Measured(Vec::new()),
    ));
    assert_eq!(report.decision(), Decision::Blocked);
    assert!(
        report
            .blockers
            .iter()
            .any(|b| b.contains("could not classify"))
    );
}

/// AUD-40 adjacency: a managed swap already active blocks a new activation
/// even when every backend is ready.
#[test]
fn managed_swap_blocks_activation_even_when_backends_are_ready() {
    let report = report_from_observations(obs(
        platform_probe(
            Observation::Measured(PlatformKind::Wsl2),
            "6.6.87.2-microsoft-standard-WSL2",
        ),
        gpu_cuda(),
        backends_ready(),
        Observation::Measured(vec![SwapEntry {
            filename: "/dev/nbd0".to_string(),
            kind: "partition".to_string(),
            size_kib: 1024 * 1024,
            used_kib: 256 * 1024,
            priority: 10,
        }]),
    ));
    assert_eq!(report.decision(), Decision::Blocked);
    assert!(
        report
            .blockers
            .iter()
            .any(|b| b.contains("managed-style swap is already active"))
    );
}

/// An unreadable swap table blocks a new activation for the same reason an
/// already-active managed swap does: one cannot be ruled out, so the look
/// that did not happen is not an authorisation. A measured empty table is
/// the opposite answer and does not block.
#[test]
fn an_unread_swap_table_blocks_activation_and_a_measured_empty_one_does_not() {
    let blocked = active_swap_activation_blockers(&Observation::Unknown);
    assert_eq!(blocked.len(), 1);
    assert!(blocked[0].contains("could not be read"));
    assert!(blocked[0].contains("refuse a new activation"));

    assert!(
        active_swap_activation_blockers(&Observation::Measured(Vec::new())).is_empty(),
        "a measured empty table is 'nothing is active', not an unknown"
    );
}

/// The three swap-table faces stay distinct in every published form: a
/// named entry, a measured empty table, and a table that could not be
/// read. `unknown` is never `none`, and `null` is never `[]`.
#[test]
fn an_unread_swap_table_is_not_published_as_a_measured_empty_one() {
    let unknown = report_from_observations(obs(
        platform_probe(
            Observation::Measured(PlatformKind::Wsl2),
            "6.6.87.2-microsoft-standard-WSL2",
        ),
        gpu_cuda(),
        backends_ready(),
        Observation::Unknown,
    ));
    let empty = report_from_observations(obs(
        platform_probe(
            Observation::Measured(PlatformKind::Wsl2),
            "6.6.87.2-microsoft-standard-WSL2",
        ),
        gpu_cuda(),
        backends_ready(),
        Observation::Measured(Vec::new()),
    ));

    let mut unknown_buf = Vec::new();
    print_text_report(&unknown, &mut unknown_buf).expect("report renders");
    let unknown_text = String::from_utf8(unknown_buf).expect("text is UTF-8");
    let mut empty_buf = Vec::new();
    print_text_report(&empty, &mut empty_buf).expect("report renders");
    let empty_text = String::from_utf8(empty_buf).expect("text is UTF-8");

    assert!(unknown_text.contains("Current swap: unknown"));
    assert!(empty_text.contains("Current swap: none"));
    assert!(!unknown_text.contains("Current swap: none"));
    assert!(!empty_text.contains("Current swap: unknown"));

    let unknown_json = render_json(&unknown);
    let empty_json = render_json(&empty);
    assert!(unknown_json.contains("\"swap\":null"));
    assert!(empty_json.contains("\"swap\":[]"));
    assert!(!unknown_json.contains("\"swap\":[]"));
    assert!(!empty_json.contains("\"swap\":null"));
}

/// SPEC §12 · ITEM-2 · DT-7 · invariant check.
///
/// Stale samples are refused; a current driver-bound sample is accepted;
/// a software or locally-estimated sample is never physical VRAM. A
/// missing sample is unknown, never a zero figure.
#[test]
fn readiness_refuses_stale_budget_and_accepts_current_provider() {
    let now = 1_000_000u64;

    // Current, adapter-bound, driver-reported → accepted.
    let current = driver_budget(now - 1_000);
    let accepted = effective_capacity(Some(&current), now, Some(GpuProviderKind::Cuda));
    assert_eq!(accepted.source, CapacitySource::DriverReported);
    assert_eq!(accepted.effective_bytes, Some(3 * 1024 * 1024 * 1024));
    assert_eq!(accepted.age_ms, Some(1_000));

    // Outside the product freshness window → refused, age reported.
    let stale = driver_budget(now - GPU_BUDGET_MAX_AGE_MS - 1);
    let refused = effective_capacity(Some(&stale), now, Some(GpuProviderKind::Cuda));
    assert_eq!(refused.source, CapacitySource::Stale);
    assert_eq!(refused.effective_bytes, None);
    assert_eq!(
        refused.age_ms,
        Some(GPU_BUDGET_MAX_AGE_MS + 1),
        "the age that caused the refusal is reported, not hidden"
    );

    // Never-bound: no sample at all is unknown, not zero capacity.
    let absent = effective_capacity(None, now, Some(GpuProviderKind::Cuda));
    assert_eq!(absent.source, CapacitySource::Absent);
    assert_eq!(absent.effective_bytes, None);
    assert!(absent.effective_bytes.is_none());
    assert_ne!(absent.effective_bytes, Some(0));

    // A local estimate is not device capacity (DT-7).
    let mut local = driver_budget(now - 1_000);
    local.source = GpuBudgetSource::ProviderLocalEstimate;
    let estimated = effective_capacity(Some(&local), now, Some(GpuProviderKind::Vulkan));
    assert_eq!(estimated.source, CapacitySource::ProviderLocalEstimate);
    assert_eq!(estimated.effective_bytes, None);

    // A software adapter is never physical VRAM, however fresh its sample.
    let software = effective_capacity(Some(&current), now, Some(GpuProviderKind::SoftwareAdapter));
    assert_eq!(software.effective_bytes, None);
    assert_eq!(software.provider, Some(GpuProviderKind::SoftwareAdapter));

    // Unbound / malformed sample (adapter missing) is refused.
    let mut unbound = driver_budget(now - 100);
    unbound.adapter = None;
    let refused = effective_capacity(Some(&unbound), now, Some(GpuProviderKind::Cuda));
    assert_eq!(refused.source, CapacitySource::Unbound);
    assert_eq!(refused.effective_bytes, None);
}

/// Capacity provenance is attached to the same typed report, and a stale
/// sample never becomes a Ready figure.
#[test]
fn report_attaches_capacity_provenance() {
    let now = 1_000_000u64;
    let fresh = driver_budget(now - 500);
    let report = report_with_capacity(
        obs(
            platform_probe(
                Observation::Measured(PlatformKind::Wsl2),
                "6.6.87.2-microsoft-standard-WSL2",
            ),
            gpu_vulkan_hardware(),
            backends_ready(),
            Observation::Measured(Vec::new()),
        ),
        Some(&fresh),
        now,
    );
    assert_eq!(report.capacity.source, CapacitySource::DriverReported);
    assert_eq!(
        report.capacity.effective_bytes,
        Some(3 * 1024 * 1024 * 1024)
    );

    let stale = driver_budget(now - GPU_BUDGET_MAX_AGE_MS - 1);
    let refused = report_with_capacity(
        obs(
            platform_probe(
                Observation::Measured(PlatformKind::Wsl2),
                "6.6.87.2-microsoft-standard-WSL2",
            ),
            gpu_vulkan_hardware(),
            backends_ready(),
            Observation::Measured(Vec::new()),
        ),
        Some(&stale),
        now,
    );
    assert_eq!(refused.capacity.source, CapacitySource::Stale);
    assert_eq!(refused.capacity.effective_bytes, None);
}

/// The published document's `gpu_budget` field, as the daemon writes it.
///
/// Deliberately unbound: the pure seam does not gate identity, so its
/// fixtures must not pretend to carry one. Use [`bound_published_document`]
/// for anything that goes through the gate.
pub(super) fn published_document(budget: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "ok": true, "gpu_budget": budget })
}

/// A cache-status document as the gate sees it: well-shaped, bound to a
/// named daemon instance, and stamped with its write time.
pub(super) fn bound_published_document(
    budget: &serde_json::Value,
    daemon_instance_id: &str,
    written_at_unix_ms: u64,
) -> serde_json::Value {
    serde_json::json!({
        "schema_version": 1,
        "daemon_instance_id": daemon_instance_id,
        "written_at_unix_ms": written_at_unix_ms,
        "ok": true,
        "origin_state": "READY",
        "cache_state": "ACTIVE",
        "gpu_budget": budget,
    })
}

pub(super) fn published_driver_budget(sampled_at_unix_ms: u64) -> serde_json::Value {
    serde_json::json!({
        "schema_version": 1,
        "adapter": { "backend": "vulkan", "key": "0", "luid": null },
        "total_bytes": 8 * 1024 * 1024 * 1024i64,
        "budget_bytes": 4 * 1024 * 1024 * 1024i64,
        "used_bytes": 1024 * 1024 * 1024i64,
        "available_bytes": 3 * 1024 * 1024 * 1024i64,
        "source": "driver_reported",
        "sampled_at_unix_ms": sampled_at_unix_ms,
    })
}

pub(super) fn ready_obs_for_capacity() -> Observations {
    obs(
        platform_probe(
            Observation::Measured(PlatformKind::Wsl2),
            "6.6.87.2-microsoft-standard-WSL2",
        ),
        gpu_vulkan_hardware(),
        backends_ready(),
        Observation::Measured(Vec::new()),
    )
}

/// RF-2 / DT-2: with nothing published the answer is `absent`, and an
/// absent sample is the documented pre-publisher answer — never a warning
/// that would train the operator to ignore the warning class.
#[test]
fn no_published_document_reports_absent_without_a_warning() {
    let report = report_with_published_document(ready_obs_for_capacity(), None, 1_000_000);
    assert_eq!(report.capacity.source, CapacitySource::Absent);
    assert_eq!(report.capacity.effective_bytes, None);
    assert!(
        !report.warnings.iter().any(|w| w.contains("capacity")),
        "absent must not warn: {:?}",
        report.warnings
    );
}

#[path = "admission_and_recommendations.rs"]
mod admission_and_recommendations;
#[path = "member_composition.rs"]
mod member_composition;
