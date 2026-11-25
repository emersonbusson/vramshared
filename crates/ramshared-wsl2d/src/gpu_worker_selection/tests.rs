// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/gpu_worker_selection/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use ramshared_dxg::DxgBudgetProvider;
use ramshared_vram::GpuBudgetSource;
use ramshared_vulkan::VulkanDeviceObservation;
use std::cell::Cell;
use std::time::Duration;

fn observation(ordinal: u32, total: u64, used: u64) -> VulkanDeviceObservation {
    VulkanDeviceObservation {
        ordinal,
        name: format!("synthetic adapter {ordinal}"),
        hardware_gpu: true,
        budget: GpuBudgetSnapshot {
            adapter: Some(GpuAdapterIdentity {
                node_mask: None,
                backend: "vulkan".into(),
                key: format!("{:032x}", ordinal + 1),
                luid: None,
            }),
            total_bytes: Some(total),
            budget_bytes: total,
            used_bytes: used,
            source: GpuBudgetSource::DriverReported,
            sampled_at: Instant::now(),
        },
    }
}

fn unavailable(_: BudgetRequest) -> Result<DxgBudgetProvider, DxgError> {
    Err(DxgError::Unavailable("injected native absence".into()))
}

#[test]
fn off_observation_keeps_exhaustion_and_rejects_estimates_invalid_identity_and_guard_failure() {
    let exhausted = observation(0, 96 << 30, 96 << 30).budget;
    assert_eq!(
        observation_budget(exhausted.clone(), unavailable)
            .unwrap()
            .available_bytes(),
        0
    );
    for mutation in 0..5 {
        let mut invalid = exhausted.clone();
        match mutation {
            0 => invalid.source = GpuBudgetSource::ProviderLocalEstimate,
            1 => invalid.used_bytes = invalid.budget_bytes + 1,
            2 => invalid.adapter = None,
            3 => invalid.sampled_at = Instant::now() + Duration::from_secs(1),
            _ => invalid.adapter.as_mut().unwrap().key = "ordinal:0".into(),
        }
        assert!(observation_budget(invalid, unavailable).is_err());
    }
    assert!(
        observation_budget(exhausted, |_| -> Result<DxgBudgetProvider, DxgError> {
            Err(DxgError::PermissionDenied)
        })
        .is_err()
    );
}

#[test]
fn vulkan_software_and_foreign_metadata_never_open_budget_guard() {
    let config = GpuWorkerConfig::default();
    let calls = Cell::new(0);
    let mut software = observation(0, 96 << 30, 0);
    software.hardware_gpu = false;
    assert!(
        vulkan_candidate(software, None, config, |luid| {
            calls.set(calls.get() + 1);
            unavailable(luid)
        })
        .unwrap()
        .is_none()
    );
    let expected = observation(1, 32 << 30, 0).budget.adapter.unwrap();
    assert!(
        vulkan_candidate(
            observation(0, 96 << 30, 0),
            Some(&expected),
            config,
            |luid| {
                calls.set(calls.get() + 1);
                unavailable(luid)
            }
        )
        .unwrap()
        .is_none()
    );
    assert_eq!(calls.get(), 0);
}

#[test]
fn vulkan_matching_metadata_request_preserves_selected_candidate() {
    let config = GpuWorkerConfig::default();
    let metadata = observation(1, 96 << 30, 8 << 30);
    let expected = metadata.budget.adapter.clone().unwrap();
    let selected = vulkan_candidate(metadata, Some(&expected), config, unavailable)
        .unwrap()
        .expect("matching explicit adapter request must remain usable");
    assert_eq!(selected.identity, expected);
    assert_eq!(selected.ordinal, 1);
    assert_eq!(selected.safe_target_bytes, config.target_bytes);
}

#[test]
fn vulkan_metadata_candidates_preserve_independent_occupied_budgets() {
    let config = GpuWorkerConfig {
        target_bytes: u64::MAX,
        ..GpuWorkerConfig::default()
    };
    let small = vulkan_candidate(
        observation(0, 32 << 30, 12 << 30),
        None,
        config,
        unavailable,
    )
    .unwrap()
    .unwrap();
    let large = vulkan_candidate(observation(1, 96 << 30, 8 << 30), None, config, unavailable)
        .unwrap()
        .unwrap();
    assert_eq!(small.backend, GpuBackendKind::Vulkan);
    assert_eq!(small.ordinal, 0);
    assert_eq!(large.ordinal, 1);
    assert_ne!(small.identity, large.identity);
    assert!(small.safe_target_bytes < 20 << 30);
    assert!(large.safe_target_bytes < 88 << 30);
    assert!(large.safe_target_bytes > small.safe_target_bytes);
}

#[test]
fn vulkan_stale_or_missing_metadata_cannot_become_a_candidate() {
    let config = GpuWorkerConfig::default();
    let mut stale = observation(0, 32 << 30, 0);
    stale.budget.sampled_at -= Duration::from_secs(30);
    assert!(vulkan_candidate(stale, None, config, unavailable).is_err());
    let mut missing = observation(1, 96 << 30, 0);
    missing.budget.adapter = None;
    assert!(vulkan_candidate(missing, None, config, unavailable).is_err());
}

#[test]
fn wsl_host_catalog_opens_cuda_by_interface_and_rejects_unmatched_vulkan_or_revocation() {
    use ramshared_dxg::{AdapterLuid, AdapterLuidNamespace, BudgetQueryScope, BudgetSnapshot};
    use ramshared_ipc::gpu_catalog::{CatalogSession, CatalogTiming};
    use ramshared_ipc::{HandshakeTranscript, HeartbeatAck, HostChallenge, IPC_VERSION_4};
    use ramshared_vram::host_identity::{
        HostGpuIdentity, HostGpuIdentityReport, HostGpuObservation, ProducerProcess,
    };
    use ramshared_wsl2d::host_catalog::SharedHostCatalog;

    struct FakeWslDxg {
        interface_id: Option<String>,
    }
    impl GpuBudgetProvider for FakeWslDxg {
        fn snapshot(&self) -> Result<BudgetSnapshot, DxgError> {
            Ok(BudgetSnapshot {
                query_scope: BudgetQueryScope::current_process_local(0),
                adapter: AdapterLuid {
                    high: 0,
                    low: 0x0154_1119,
                },
                adapter_namespace: AdapterLuidNamespace::WslGuest,
                adapter_interface_id: self.interface_id.clone(),
                budget: 8 << 30,
                current_usage: 0,
                current_reservation: 0,
                available_for_reservation: 8 << 30,
                sampled_at: Instant::now(),
            })
        }
    }

    let iface = r"\\?\PCI#VEN_10DE&DEV_1F08#wsl-select";
    let report = HostGpuIdentityReport {
        schema_version: 1,
        producer: ProducerProcess {
            pid: 42,
            creation_time_100ns: 77,
        },
        started_at_unix_ms: 1_700_000_000_000,
        elapsed_ms: 5,
        adapters: vec![HostGpuIdentity {
            ordinal: 0,
            allocator: GpuAdapterIdentity {
                node_mask: Some(1),
                backend: "cuda".into(),
                key: format!("{:032x}", 1u8),
                luid: Some("00000001:00000002".into()),
            },
            interface_id: iface.into(),
        }],
    };
    let secret = b"selection-test-secret-32-bytes!!";
    let transcript = HandshakeTranscript {
        min_version: IPC_VERSION_4,
        max_version: IPC_VERSION_4,
        boot_id: "boot-sel".into(),
        distro_id: "ubuntu".into(),
        guest_nonce: [0x11; 32],
    };
    let challenge = HostChallenge {
        accepted_version: IPC_VERSION_4,
        heartbeat_secs: 2,
        lease_timeout_secs: 6,
        host_nonce: [0x22; 32],
    };
    let mut hs = CatalogSession::new(secret, &transcript, &challenge, 7).unwrap();
    let mut gs = CatalogSession::new(secret, &transcript, &challenge, 7).unwrap();
    let ack = HeartbeatAck {
        timestamp_ms: 6000,
        lease_remaining_ms: 6000,
        host_memory: None,
    };
    let encoded = hs
        .seal(
            secret,
            &ack,
            Some(HostGpuObservation { report, age_ms: 10 }),
        )
        .unwrap();
    let now = Instant::now();
    let entry = gs
        .open(
            secret,
            &ack,
            &encoded,
            4,
            CatalogTiming {
                started: now,
                received: now,
            },
        )
        .unwrap()
        .unwrap();
    let catalog = SharedHostCatalog::new();
    catalog.publish(Some(&entry));
    let host: Arc<dyn HostGpuCatalogProvider> = Arc::new(catalog.clone());

    // Host catalog only admits canonical CUDA allocators; Vulkan still falls back cleanly when native DXG is absent.
    let vk = vulkan_candidate_with_host(
        observation(1, 96 << 30, 8 << 30),
        None,
        GpuWorkerConfig::default(),
        Some(Arc::clone(&host)),
        |target| match target {
            DxgOpenTarget::Luid(req) => unavailable(req),
            DxgOpenTarget::Interface { .. } => {
                panic!("Vulkan adapter must not match CUDA-only host catalog row")
            }
        },
    )
    .unwrap()
    .unwrap();
    assert!(vk.wddm_binding.is_none());

    let cuda_snapshot = GpuBudgetSnapshot {
        adapter: Some(GpuAdapterIdentity {
            node_mask: Some(1),
            backend: "cuda".into(),
            key: format!("{:032x}", 1u8),
            luid: Some("00000001:00000002".into()),
        }),
        total_bytes: Some(8 << 30),
        budget_bytes: 8 << 30,
        used_bytes: 0,
        source: GpuBudgetSource::DriverReported,
        sampled_at: Instant::now(),
    };
    let selected = candidate_from_budget_with_host(
        cuda_snapshot.clone(),
        GpuBackendKind::Cuda,
        0,
        GpuWorkerConfig::default(),
        Some(Arc::clone(&host)),
        |target| match target {
            DxgOpenTarget::Interface {
                interface_id,
                physical_adapter_index,
            } => {
                assert_eq!(interface_id, iface);
                assert_eq!(physical_adapter_index, 0);
                Ok(FakeWslDxg {
                    interface_id: Some(iface.into()),
                })
            }
            DxgOpenTarget::Luid(_) => {
                panic!("expected Interface target while host catalog matches")
            }
        },
    )
    .unwrap()
    .expect("matched WSL CUDA candidate must be selected");
    assert!(selected.wddm_binding.is_some());

    // After host catalog revocation, WslGuest DXG refuses LUID fallback (fail closed).
    catalog.revoke();
    assert!(
        candidate_from_budget_with_host(
            cuda_snapshot,
            GpuBackendKind::Cuda,
            0,
            GpuWorkerConfig::default(),
            Some(host),
            |target| match target {
                DxgOpenTarget::Luid(_) => Ok(FakeWslDxg { interface_id: None }),
                DxgOpenTarget::Interface { .. } => unreachable!(),
            },
        )
        .is_err()
    );
}
