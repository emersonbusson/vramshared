// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/gpu_budget/correspondence/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use std::cell::Cell;

const GIB: u64 = 1024 * 1024 * 1024;
fn allocator() -> GpuBudgetSnapshot {
    GpuBudgetSnapshot {
        adapter: Some(GpuAdapterIdentity {
            node_mask: Some(1),
            backend: "cuda".into(),
            key: "12000000000000000000000000000000".into(),
            luid: Some("00000001:00000002".into()),
        }),
        total_bytes: Some(16 * GIB),
        budget_bytes: 12 * GIB,
        used_bytes: 4 * GIB,
        source: ramshared_vram::GpuBudgetSource::DriverReported,
        sampled_at: Instant::now(),
    }
}
fn host() -> BudgetSnapshot {
    BudgetSnapshot {
        query_scope: ramshared_dxg::BudgetQueryScope::current_process_local(0),
        adapter: AdapterLuid { high: 1, low: 2 },
        adapter_namespace: AdapterLuidNamespace::WindowsHost,
        adapter_interface_id: None,
        budget: 8 * GIB,
        current_usage: 3 * GIB,
        current_reservation: 0,
        available_for_reservation: 2 * GIB,
        sampled_at: Instant::now(),
    }
}

#[test]
fn inactive_captured_sampler_cannot_match_even_an_equal_binding() {
    let allocation = allocator();
    let observed = host();
    let mut binding = AdapterCorrespondence::from_scoped_snapshots(&allocation, &observed).unwrap();
    binding.query_scope.process_identity = None;
    let candidate = super::super::GpuAdapterCandidate {
        backend: super::super::GpuBackendKind::Cuda,
        ordinal: 0,
        identity: allocation.adapter.unwrap(),
        safe_target_bytes: 4096,
        wddm_binding: Some(binding.clone()),
    };
    assert!(
        candidate
            .validate_runtime_authority(4096, Some(&binding))
            .is_err(),
        "an equal binding with inactive captured sampler authority was accepted"
    );
}
struct Provider(BudgetSnapshot);
impl GpuBudgetProvider for Provider {
    fn snapshot(&self) -> Result<BudgetSnapshot, DxgError> {
        Ok(self.0.clone())
    }
}

#[test]
fn equal_numeric_luids_across_namespaces_never_prove_correspondence() {
    let allocator = allocator();
    let mut guest = host();
    guest.adapter_namespace = AdapterLuidNamespace::WslGuest;
    assert!(AdapterCorrespondence::from_scoped_snapshots(&allocator, &guest).is_err());
    assert!(
        !allocator
            .adapter
            .as_ref()
            .unwrap()
            .matches_physical_adapter(guest.to_vram_budget().adapter.as_ref().unwrap())
    );
    assert!(open_matching_wddm_for_snapshot(&allocator, |_| Ok(Provider(guest.clone()))).is_err());
}

#[test]
fn host_scope_binding_preserves_lower_headroom_and_both_identities() {
    let allocator = allocator();
    let (provider, binding) = open_matching_wddm_for_snapshot(&allocator, |requested| {
        assert_eq!(requested.adapter, Some(host().adapter));
        assert_eq!(requested.physical_adapter_index, Some(0));
        Ok(Provider(host()))
    })
    .unwrap()
    .unwrap();
    let sample = constrained_budget(
        allocator.clone(),
        provider.snapshot().unwrap(),
        Instant::now(),
        &binding,
    )
    .unwrap();
    assert_eq!(sample.adapter, allocator.adapter);
    assert_eq!(sample.total_bytes, allocator.total_bytes);
    assert_eq!(sample.used_bytes, allocator.used_bytes);
    assert_eq!(sample.available_bytes(), 2 * GIB);
}

#[test]
fn missing_allocator_luid_cannot_drop_an_available_wddm_guard() {
    let mut allocator = allocator();
    allocator.adapter.as_mut().unwrap().node_mask = None;
    allocator.adapter.as_mut().unwrap().luid = None;
    let calls = Cell::new(0);
    assert!(
        open_matching_wddm_for_snapshot(&allocator, |requested| {
            calls.set(calls.get() + 1);
            assert_eq!(requested.adapter, None);
            assert_eq!(requested.physical_adapter_index, None);
            Ok(Provider(host()))
        })
        .is_err()
    );
    assert_eq!(calls.get(), 1);
    let unavailable: Result<Option<(Provider, AdapterCorrespondence)>, _> =
        open_matching_wddm_for_snapshot(&allocator, |_| {
            Err(DxgError::Unavailable("native support absent".into()))
        });
    assert!(unavailable.unwrap().is_none());
}

#[test]
fn stale_or_future_first_wddm_sample_cannot_establish_a_binding() {
    let allocator = allocator();
    for sampled_at in [
        Instant::now() - std::time::Duration::from_secs(30),
        Instant::now() + std::time::Duration::from_secs(30),
    ] {
        let observed = BudgetSnapshot {
            sampled_at,
            ..host()
        };
        assert!(
            open_matching_wddm_for_snapshot(&allocator, |_| Ok(Provider(observed.clone())))
                .is_err()
        );
    }
}

#[test]
fn invalid_allocator_identity_is_refused_before_native_open() {
    for key in ["", "invalid", "00000000000000000000000000000000"] {
        let mut allocator = allocator();
        allocator.adapter.as_mut().unwrap().key = key.into();
        let outcome: Result<Option<(Provider, AdapterCorrespondence)>, _> =
            open_matching_wddm_for_snapshot(&allocator, |_| {
                panic!("invalid identity reached native open")
            });
        assert!(outcome.is_err());
    }
}

#[test]
fn invalid_initial_budget_is_never_reported_as_native_absence() {
    let original = allocator();
    let mut invalid = Vec::new();
    let mut sample = original.clone();
    sample.adapter = None;
    invalid.push(sample);
    let mut sample = original.clone();
    sample.source = ramshared_vram::GpuBudgetSource::ProviderLocalEstimate;
    invalid.push(sample);
    let mut sample = original.clone();
    sample.used_bytes = sample.budget_bytes + 1;
    invalid.push(sample);
    let mut sample = original.clone();
    sample.total_bytes = Some(sample.budget_bytes - 1);
    invalid.push(sample);
    let mut sample = original.clone();
    sample.sampled_at -= std::time::Duration::from_secs(30);
    invalid.push(sample);
    let mut sample = original;
    sample.sampled_at += std::time::Duration::from_secs(30);
    invalid.push(sample);
    for sample in invalid {
        let result: Result<Option<(Provider, AdapterCorrespondence)>, _> =
            open_matching_wddm_for_snapshot(&sample, |_| {
                panic!("invalid initial budget reached native open")
            });
        assert!(
            result.is_err(),
            "invalid initial budget was treated as native WDDM absence"
        );
    }
}

#[test]
fn bound_sample_rejects_backend_luid_and_namespace_drift() {
    let allocator = allocator();
    let original = host();
    let binding = AdapterCorrespondence::from_scoped_snapshots(&allocator, &original).unwrap();
    let mut other_backend = allocator.clone();
    other_backend.adapter.as_mut().unwrap().backend = "vulkan".into();
    assert!(constrained_budget(other_backend, original.clone(), Instant::now(), &binding).is_err());
    let mut other_luid = allocator.clone();
    other_luid.adapter.as_mut().unwrap().luid = Some("00000001:00000003".into());
    assert!(constrained_budget(other_luid, original.clone(), Instant::now(), &binding).is_err());
    let other_wddm = BudgetSnapshot {
        adapter: AdapterLuid { high: 1, low: 3 },
        ..original.clone()
    };
    assert!(constrained_budget(allocator.clone(), other_wddm, Instant::now(), &binding).is_err());
    let guest = BudgetSnapshot {
        adapter_namespace: AdapterLuidNamespace::WslGuest,
        ..original.clone()
    };
    assert!(constrained_budget(allocator, guest, Instant::now(), &binding).is_err());
}

#[test]
fn wddm_query_scope_must_match_the_current_worker_and_allocator_node() {
    use ramshared_dxg::{BudgetProcess, MemorySegmentGroup};
    let mut allocation = allocator();
    allocation.adapter.as_mut().unwrap().node_mask = Some(2);
    let mut observed = host();
    observed.query_scope.physical_adapter_index = 1;
    let (_, binding) = open_matching_wddm_for_snapshot(&allocation, |request| {
        assert_eq!(request.adapter, Some(observed.adapter));
        assert_eq!(request.physical_adapter_index, Some(1));
        Ok(Provider(observed.clone()))
    })
    .unwrap()
    .unwrap();
    assert!(
        constrained_budget(
            allocation.clone(),
            observed.clone(),
            Instant::now(),
            &binding
        )
        .is_ok()
    );
    for query_scope in [
        BudgetQueryScope {
            process_identity: None,
            ..observed.query_scope.clone()
        },
        BudgetQueryScope {
            process: BudgetProcess::Windows(std::process::id()),
            ..observed.query_scope.clone()
        },
        BudgetQueryScope {
            process: BudgetProcess::Linux(0),
            ..observed.query_scope.clone()
        },
        BudgetQueryScope {
            segment: MemorySegmentGroup::NonLocal,
            ..observed.query_scope.clone()
        },
        BudgetQueryScope {
            physical_adapter_index: 0,
            ..observed.query_scope.clone()
        },
    ] {
        let sample = BudgetSnapshot {
            query_scope,
            ..observed.clone()
        };
        assert!(
            open_matching_wddm_for_snapshot(&allocation, |_| Ok(Provider(sample.clone()))).is_err()
        );
    }
    allocation.adapter.as_mut().unwrap().node_mask = None;
    assert!(
        open_matching_wddm_for_snapshot(&allocation, |request| {
            assert_eq!(request.physical_adapter_index, None);
            Ok(Provider(observed.clone()))
        })
        .is_err()
    );
}

#[test]
fn bound_query_scope_cannot_drift() {
    use ramshared_dxg::{BudgetProcess, MemorySegmentGroup};
    let allocation = allocator();
    let observed = host();
    let (_, binding) =
        open_matching_wddm_for_snapshot(&allocation, |_| Ok(Provider(observed.clone())))
            .unwrap()
            .unwrap();
    for query_scope in [
        BudgetQueryScope {
            process_identity: None,
            ..observed.query_scope.clone()
        },
        BudgetQueryScope {
            process: BudgetProcess::Linux(0),
            ..observed.query_scope.clone()
        },
        BudgetQueryScope {
            segment: MemorySegmentGroup::NonLocal,
            ..observed.query_scope.clone()
        },
        BudgetQueryScope {
            physical_adapter_index: 1,
            ..observed.query_scope.clone()
        },
    ] {
        assert!(
            constrained_budget(
                allocation.clone(),
                BudgetSnapshot {
                    query_scope,
                    ..observed.clone()
                },
                Instant::now(),
                &binding
            )
            .is_err()
        );
    }
    let mut changed_node = allocation;
    changed_node.adapter.as_mut().unwrap().node_mask = Some(2);
    assert!(constrained_budget(changed_node, observed, Instant::now(), &binding).is_err());
}

struct MutableCatalog(
    std::sync::Mutex<Option<ramshared_vram::host_identity::HostGpuIdentityReport>>,
);
impl HostGpuCatalogProvider for MutableCatalog {
    fn current_report(&self) -> Option<ramshared_vram::host_identity::HostGpuIdentityReport> {
        self.0.lock().ok().and_then(|g| g.clone())
    }
}

struct DummyMem;
impl ramshared_vram::VramMemory for DummyMem {
    fn len(&self) -> usize {
        0
    }
    fn zero(&mut self) -> Result<(), VramError> {
        Ok(())
    }
    fn read_at(&self, _off: u64, _dst: &mut [u8]) -> Result<(), VramError> {
        Ok(())
    }
    fn write_at(&mut self, _off: u64, _src: &[u8]) -> Result<(), VramError> {
        Ok(())
    }
}

struct AllocProvider(GpuBudgetSnapshot);
impl VramProvider for AllocProvider {
    type Mem<'a> = DummyMem;

    fn alloc(&self, _bytes: usize) -> Result<Self::Mem<'_>, VramError> {
        Ok(DummyMem)
    }

    fn mem_info(&self) -> Result<(u64, u64), VramError> {
        Ok((self.0.available_bytes(), self.0.total_bytes.unwrap_or(0)))
    }

    fn budget_snapshot(&self) -> Result<GpuBudgetSnapshot, VramError> {
        Ok(self.0.clone())
    }
}

#[test]
fn wsl_guest_correspondence_opens_by_interface_and_revalidates_live_catalog() {
    use ramshared_vram::host_identity::{HostGpuIdentity, HostGpuIdentityReport, ProducerProcess};

    let alloc = allocator();
    let iface = r"\\?\PCI#VEN_10DE&DEV_1F08&SUBSYS_0000#{1111}";
    let report = HostGpuIdentityReport {
        schema_version: 1,
        producer: ProducerProcess {
            pid: 77,
            creation_time_100ns: 99,
        },
        started_at_unix_ms: 1000,
        elapsed_ms: 4,
        adapters: vec![HostGpuIdentity {
            ordinal: 0,
            allocator: alloc.adapter.clone().unwrap(),
            interface_id: iface.into(),
        }],
    };
    let catalog: Arc<MutableCatalog> =
        Arc::new(MutableCatalog(std::sync::Mutex::new(Some(report))));
    let guest_wddm = BudgetSnapshot {
        adapter: AdapterLuid {
            high: 0,
            low: 0x0154_1119,
        },
        adapter_namespace: AdapterLuidNamespace::WslGuest,
        adapter_interface_id: Some(iface.to_ascii_lowercase()),
        ..host()
    };

    let catalog_dyn: Arc<dyn HostGpuCatalogProvider> = catalog.clone();
    let (provider, binding) = open_matching_wddm_provider_with_host(
        &AllocProvider(alloc.clone()),
        Some(catalog_dyn.clone()),
        |target| {
            assert_eq!(
                target,
                DxgOpenTarget::Interface {
                    interface_id: iface,
                    physical_adapter_index: 0,
                }
            );
            Ok(Provider(guest_wddm.clone()))
        },
    )
    .unwrap()
    .unwrap();

    assert!(binding.matches_current_authority(&binding));
    assert!(!binding.matches_current_authority(
        &AdapterCorrespondence::from_scoped_snapshots(&alloc, &host()).unwrap()
    ));

    let combined = constrained_budget(
        alloc.clone(),
        provider.snapshot().unwrap(),
        Instant::now(),
        &binding,
    )
    .unwrap();
    assert_eq!(combined.available_bytes(), 2 * GIB);

    // Changing guest interface ID, dropping it, or revoking/expiring the live host catalog fails closed.
    let mut wrong_iface = guest_wddm.clone();
    wrong_iface.adapter_interface_id = Some(r"\\?\other".into());
    assert!(constrained_budget(alloc.clone(), wrong_iface, Instant::now(), &binding).is_err());

    let mut missing_iface = guest_wddm.clone();
    missing_iface.adapter_interface_id = None;
    assert!(constrained_budget(alloc.clone(), missing_iface, Instant::now(), &binding).is_err());

    *catalog.0.lock().unwrap() = None;
    assert!(constrained_budget(alloc, guest_wddm, Instant::now(), &binding).is_err());
}

#[test]
fn wsl_guest_rejects_mismatched_host_luid_missing_adapter_and_allows_native_absence() {
    use ramshared_vram::host_identity::{HostGpuIdentity, HostGpuIdentityReport, ProducerProcess};

    let alloc = allocator();
    let iface = r"\\?\PCI#VEN_10DE&DEV_1F08&SUBSYS_0000#{2222}";
    let mut mismatched_host_alloc = alloc.adapter.clone().unwrap();
    mismatched_host_alloc.luid = Some("00000001:00000099".into());
    let mismatched_report = HostGpuIdentityReport {
        schema_version: 1,
        producer: ProducerProcess {
            pid: 77,
            creation_time_100ns: 99,
        },
        started_at_unix_ms: 1000,
        elapsed_ms: 4,
        adapters: vec![HostGpuIdentity {
            ordinal: 0,
            allocator: mismatched_host_alloc,
            interface_id: iface.into(),
        }],
    };
    let guest_wddm = BudgetSnapshot {
        adapter: AdapterLuid {
            high: 0,
            low: 0x0154_1119,
        },
        adapter_namespace: AdapterLuidNamespace::WslGuest,
        adapter_interface_id: Some(iface.into()),
        ..host()
    };
    let catalog: Arc<dyn HostGpuCatalogProvider> = Arc::new(MutableCatalog(std::sync::Mutex::new(
        Some(mismatched_report),
    )));
    assert!(
        open_matching_wddm_for_snapshot_with_host(&alloc, Some(catalog.clone()), |_| {
            Ok(Provider(guest_wddm.clone()))
        })
        .is_err()
    );

    // Unmapped GPU: falls back to Luid target, succeeds with Ok(None) if /dev/dxg is absent, fails closed if present.
    let empty_report = HostGpuIdentityReport {
        schema_version: 1,
        producer: ProducerProcess {
            pid: 77,
            creation_time_100ns: 99,
        },
        started_at_unix_ms: 1000,
        elapsed_ms: 4,
        adapters: vec![],
    };
    let empty_catalog: Arc<dyn HostGpuCatalogProvider> =
        Arc::new(MutableCatalog(std::sync::Mutex::new(Some(empty_report))));
    let native_absent: Result<Option<(Provider, AdapterCorrespondence)>, _> =
        open_matching_wddm_for_snapshot_with_host(&alloc, Some(empty_catalog.clone()), |target| {
            assert!(matches!(target, DxgOpenTarget::Luid(_)));
            Err(DxgError::Unavailable("absent".into()))
        });
    assert!(native_absent.unwrap().is_none());

    assert!(
        open_matching_wddm_for_snapshot_with_host(&alloc, Some(empty_catalog), |_| {
            Err::<Provider, DxgError>(DxgError::AdapterNotFound(AdapterLuid { high: 1, low: 2 }))
        })
        .is_err()
    );
    assert!(
        open_matching_wddm_for_snapshot_with_host(&alloc, Some(catalog), |_| {
            Err::<Provider, DxgError>(DxgError::InterfaceNotFound(iface.into()))
        })
        .is_err()
    );
}
