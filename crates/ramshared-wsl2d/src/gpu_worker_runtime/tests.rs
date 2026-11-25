// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/gpu_worker_runtime/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use ramshared_block::{BestEffortCache, CacheMutation, CacheRead, IpcCacheClient};
use ramshared_dxg::{AdapterLuid, AdapterLuidNamespace, BudgetSnapshot};
use ramshared_vram::{GpuAdapterIdentity, GpuBudgetSnapshot, GpuBudgetSource, VramError};
use std::os::unix::net::UnixStream;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use std::time::Duration;

const GIB: u64 = 1024 * 1024 * 1024;
const TARGET: u64 = 4096;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Case {
    Host,
    SelectionGuardLost,
    SelectionBindingSubstituted,
    NativeBecameAvailable,
    SecondNode,
    ForeignSampler,
    MissingSamplerIncarnation,
    LateSamplerIncarnationLoss,
    ForeignSegment,
    ForeignNode,
    LateQueryScopeDrift,
    LateAllocatorNodeDrift,
    NativeUnavailable,
    Guest,
    NotFound,
    AllocatorError,
    WddmOpenError,
    WddmRevalidationError,
    ExpectedForeign,
    ReplacedUuid,
    ChangedScope,
    LateScopeDrift,
    FirstAllocatorError,
    FirstAllocatorStale,
    FirstAllocatorMissingIdentity,
    FirstAllocatorEstimate,
    FirstAllocatorInvertedOccupancy,
    NativeUnavailableAllocatorError,
    NativeUnavailableForeign,
}
fn budget() -> GpuBudgetSnapshot {
    GpuBudgetSnapshot {
        adapter: Some(GpuAdapterIdentity {
            node_mask: Some(1),
            backend: "cuda".into(),
            key: "12000000000000000000000000000000".into(),
            luid: Some("00000001:00000002".into()),
        }),
        total_bytes: Some(8 * GIB),
        budget_bytes: 8 * GIB,
        used_bytes: 0,
        source: GpuBudgetSource::DriverReported,
        sampled_at: Instant::now(),
    }
}
fn wddm() -> BudgetSnapshot {
    BudgetSnapshot {
        query_scope: ramshared_dxg::BudgetQueryScope::current_process_local(0),
        adapter: AdapterLuid { high: 1, low: 2 },
        adapter_namespace: AdapterLuidNamespace::WindowsHost,
        adapter_interface_id: None,
        budget: 8 * GIB,
        current_usage: 0,
        current_reservation: 0,
        available_for_reservation: 8 * GIB,
        sampled_at: Instant::now(),
    }
}
struct Memory(Vec<u8>);
impl VramMemory for Memory {
    fn len(&self) -> usize {
        self.0.len()
    }
    fn zero(&mut self) -> Result<(), VramError> {
        self.0.fill(0);
        Ok(())
    }
    fn read_at(&self, off: u64, dst: &mut [u8]) -> Result<(), VramError> {
        let fail = || VramError::OutOfRange {
            off,
            len: dst.len() as u64,
            size: self.0.len() as u64,
        };
        let start = usize::try_from(off).map_err(|_| fail())?;
        let bytes = self
            .0
            .get(start..)
            .and_then(|bytes| bytes.get(..dst.len()))
            .ok_or_else(fail)?;
        dst.copy_from_slice(bytes);
        Ok(())
    }
    fn write_at(&mut self, off: u64, src: &[u8]) -> Result<(), VramError> {
        let size = self.0.len() as u64;
        let fail = || VramError::OutOfRange {
            off,
            len: src.len() as u64,
            size,
        };
        let start = usize::try_from(off).map_err(|_| fail())?;
        let bytes = self
            .0
            .get_mut(start..)
            .and_then(|bytes| bytes.get_mut(..src.len()))
            .ok_or_else(fail)?;
        bytes.copy_from_slice(src);
        Ok(())
    }
}
struct Allocator {
    case: Case,
    reads: AtomicUsize,
    allocations: Arc<AtomicUsize>,
}
impl VramProvider for Allocator {
    type Mem<'a> = Memory;
    fn alloc(&self, bytes: usize) -> Result<Memory, VramError> {
        assert!(
            bytes <= TARGET as usize,
            "fixture must not allocate physical-scale memory"
        );
        self.allocations.fetch_add(1, Ordering::SeqCst);
        Ok(Memory(vec![0; bytes]))
    }
    fn mem_info(&self) -> Result<(u64, u64), VramError> {
        Ok((8 * GIB, 8 * GIB))
    }
    fn budget_snapshot(&self) -> Result<GpuBudgetSnapshot, VramError> {
        let call = self.reads.fetch_add(1, Ordering::SeqCst) + 1;
        if call == 1 && self.case == Case::FirstAllocatorError {
            return Err(VramError::Provider(
                "injected first observation failure".into(),
            ));
        }
        if call >= 2
            && matches!(
                self.case,
                Case::AllocatorError | Case::NativeUnavailableAllocatorError
            )
        {
            return Err(VramError::Provider(
                "injected allocator observation failure".into(),
            ));
        }
        let mut sample = budget();
        if self.case == Case::SecondNode || (call >= 3 && self.case == Case::LateAllocatorNodeDrift)
        {
            sample.adapter.as_mut().unwrap().node_mask = Some(2);
        }
        if call == 1 {
            match self.case {
                Case::FirstAllocatorStale => sample.sampled_at -= Duration::from_secs(30),
                Case::FirstAllocatorMissingIdentity => sample.adapter = None,
                Case::FirstAllocatorEstimate => {
                    sample.source = GpuBudgetSource::ProviderLocalEstimate
                }
                Case::FirstAllocatorInvertedOccupancy => {
                    sample.used_bytes = sample.budget_bytes + 1
                }
                _ => {}
            }
        }
        if call >= 2 && self.case == Case::ReplacedUuid {
            sample.adapter.as_mut().unwrap().key = "34000000000000000000000000000000".into();
        }
        Ok(sample)
    }
}
struct Wddm {
    case: Case,
    reads: AtomicUsize,
}
impl GpuBudgetProvider for Wddm {
    fn snapshot(&self) -> Result<BudgetSnapshot, DxgError> {
        let call = self.reads.fetch_add(1, Ordering::SeqCst) + 1;
        if (call >= 1 && self.case == Case::WddmOpenError)
            || (call >= 2 && self.case == Case::WddmRevalidationError)
        {
            return Err(DxgError::Io("injected WDDM observation failure".into()));
        }
        let mut sample = wddm();
        match self.case {
            Case::SecondNode | Case::ForeignNode => sample.query_scope.physical_adapter_index = 1,
            Case::ForeignSampler => {
                sample.query_scope.process =
                    ramshared_dxg::BudgetProcess::Windows(std::process::id())
            }
            Case::ForeignSegment => {
                sample.query_scope.segment = ramshared_dxg::MemorySegmentGroup::NonLocal
            }
            Case::MissingSamplerIncarnation => sample.query_scope.process_identity = None,
            Case::LateSamplerIncarnationLoss if call >= 3 => {
                sample.query_scope.process_identity = None
            }
            Case::LateQueryScopeDrift if call >= 3 => {
                sample.query_scope.process = ramshared_dxg::BudgetProcess::Linux(0)
            }
            _ => {}
        }
        if self.case == Case::Guest
            || (call >= 2 && self.case == Case::ChangedScope)
            || (call >= 3 && self.case == Case::LateScopeDrift)
        {
            sample.adapter_namespace = AdapterLuidNamespace::WslGuest;
        }
        Ok(sample)
    }
}
fn open(case: Case, requested: BudgetRequest) -> Result<Wddm, DxgError> {
    assert_eq!(requested.adapter, Some(wddm().adapter));
    assert_eq!(
        requested.physical_adapter_index,
        Some(if case == Case::SecondNode { 1 } else { 0 })
    );
    match case {
        Case::NativeUnavailable
        | Case::SelectionGuardLost
        | Case::NativeUnavailableAllocatorError
        | Case::NativeUnavailableForeign => Err(DxgError::Unavailable(
            "injected native Linux without DXG".into(),
        )),
        Case::NotFound => Err(DxgError::AdapterNotFound(requested.adapter.unwrap())),
        _ => Ok(Wddm {
            case,
            reads: AtomicUsize::new(0),
        }),
    }
}

fn journey(case: Case, admitted: bool) {
    journey_with_runtime_target(case, admitted, TARGET);
}

fn captured_selection(case: Case, config: GpuWorkerConfig) -> GpuAdapterCandidate {
    use super::super::gpu_worker_selection::candidate_from_budget;
    use ramshared_wsl2d::gpu_budget::{GpuBackendKind, select_gpu_candidate};
    let mut observed = budget();
    if case == Case::SecondNode {
        observed.adapter.as_mut().unwrap().node_mask = Some(2);
    }
    let selection_case = match case {
        Case::SecondNode => Case::SecondNode,
        Case::NativeUnavailable
        | Case::NativeUnavailableAllocatorError
        | Case::NativeUnavailableForeign
        | Case::NativeBecameAvailable => Case::NativeUnavailable,
        _ => Case::Host,
    };
    let captured = candidate_from_budget(observed, GpuBackendKind::Cuda, 2, config, |request| {
        open(selection_case, request)
    })
    .unwrap()
    .unwrap();
    let candidates = [captured];
    let mut selected = select_gpu_candidate(&candidates, None).cloned().unwrap();
    assert_eq!(
        selected.wddm_binding.is_some(),
        selection_case != Case::NativeUnavailable
    );
    if case == Case::SelectionBindingSubstituted {
        let mut other = budget();
        other.adapter.as_mut().unwrap().key = "34000000000000000000000000000000".into();
        selected.wddm_binding =
            candidate_from_budget(other, GpuBackendKind::Cuda, 3, config, |request| {
                open(Case::Host, request)
            })
            .unwrap()
            .unwrap()
            .wddm_binding;
    }
    assert_eq!(selected.safe_target_bytes, TARGET);
    if matches!(case, Case::ExpectedForeign | Case::NativeUnavailableForeign) {
        selected.identity.key = "34000000000000000000000000000000".into();
    }
    selected
}

fn journey_with_runtime_target(case: Case, admitted: bool, runtime_target: u64) {
    let config = GpuWorkerConfig {
        target_bytes: TARGET,
        chunk_bytes: TARGET as usize,
        reserve_floor_bytes: 0,
        compression_enabled: false,
    };
    let selected = captured_selection(case, config);
    let allocations = Arc::new(AtomicUsize::new(0));
    let provider = Allocator {
        case,
        reads: AtomicUsize::new(0),
        allocations: allocations.clone(),
    };
    let (parent, child) = UnixStream::pair().unwrap();
    let (done_tx, done_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = run_gpu_worker_with_selected_wddm(
            child,
            provider,
            &selected,
            GpuWorkerConfig {
                target_bytes: runtime_target,
                chunk_bytes: TARGET as usize,
                reserve_floor_bytes: 0,
                compression_enabled: false,
            },
            |requested| open(case, requested),
        );
        done_tx.send(result).unwrap();
    });
    let mut client = IpcCacheClient::with_pacing(
        parent,
        Duration::from_secs(2),
        TARGET,
        ramshared_block::ipc_cache_client::PacingPolicy::immediate(),
    );
    let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        client.perform_handshake().unwrap();
        assert_eq!(
            client.target_bytes(),
            if admitted { TARGET } else { 0 },
            "{case:?}"
        );
        let data = vec![0x5a; TARGET as usize];
        if admitted {
            assert_eq!(client.promote(0, &data), CacheMutation::Accepted);
            let mut actual = vec![0; data.len()];
            assert_eq!(client.read(0, &mut actual), CacheRead::Hit);
            assert_eq!(actual, data);
            assert_eq!(allocations.load(Ordering::SeqCst), 1);
            assert_eq!(client.disable(), CacheMutation::Accepted);
        } else {
            assert_ne!(client.promote(0, &data), CacheMutation::Accepted);
            assert_eq!(allocations.load(Ordering::SeqCst), 0);
        }
    }));
    drop(client);
    done_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .unwrap();
    worker.join().unwrap();
    if let Err(failure) = checked {
        std::panic::resume_unwind(failure);
    }
}

#[test]
fn host_scope_worker_admits_only_the_bound_adapter() {
    journey(Case::Host, true);
}

#[test]
fn native_unavailability_preserves_allocator_only_startup() {
    journey(Case::NativeUnavailable, true);
}

#[test]
fn unproven_or_changed_wddm_identity_keeps_worker_target_zero() {
    for case in [
        Case::Guest,
        Case::NotFound,
        Case::ReplacedUuid,
        Case::ChangedScope,
    ] {
        journey(case, false);
    }
}

#[test]
fn failed_revalidation_never_reaches_allocator() {
    for case in [
        Case::AllocatorError,
        Case::WddmOpenError,
        Case::WddmRevalidationError,
        Case::ExpectedForeign,
        Case::NativeUnavailableAllocatorError,
        Case::NativeUnavailableForeign,
    ] {
        journey(case, false);
    }
}

#[test]
fn established_guard_scope_drift_prevents_cache_allocation() {
    journey(Case::LateScopeDrift, false);
}

#[test]
fn captured_candidate_uses_scoped_headroom_and_refuses_provider_drift() {
    use super::super::gpu_worker_selection::candidate_from_budget;
    use ramshared_wsl2d::gpu_budget::GpuBackendKind;
    let config = GpuWorkerConfig {
        target_bytes: TARGET,
        chunk_bytes: TARGET as usize,
        reserve_floor_bytes: 0,
        compression_enabled: false,
    };
    let candidate = candidate_from_budget(budget(), GpuBackendKind::Cuda, 2, config, |request| {
        open(Case::Host, request)
    })
    .unwrap()
    .unwrap();
    assert_eq!(candidate.safe_target_bytes, TARGET);
    assert_eq!(candidate.ordinal, 2);
    assert_eq!(candidate.identity, budget().adapter.unwrap());
    for case in [Case::Guest, Case::ChangedScope, Case::WddmRevalidationError] {
        assert!(
            candidate_from_budget(budget(), GpuBackendKind::Cuda, 2, config, |request| open(
                case, request
            ))
            .is_err()
        );
    }
}

#[test]
fn first_allocator_observation_failure_does_not_skip_wddm_admission() {
    let failed: Vec<_> = [
        Case::FirstAllocatorError,
        Case::FirstAllocatorStale,
        Case::FirstAllocatorMissingIdentity,
        Case::FirstAllocatorEstimate,
        Case::FirstAllocatorInvertedOccupancy,
    ]
    .into_iter()
    .filter(|case| std::panic::catch_unwind(|| journey(*case, false)).is_err())
    .collect();
    assert!(
        failed.is_empty(),
        "initial observation failures bypassed WDDM admission: {failed:?}"
    );
}

#[test]
fn unavailable_provider_never_returns_memory_or_successful_io() {
    assert!(UnavailableVramProvider.alloc(1).is_err());
    assert!(UnavailableVramProvider.mem_info().is_err());
    let mut memory = UnavailableVramMemory;
    assert_eq!(memory.len(), 0);
    assert!(memory.zero().is_err());
    let mut data = [0x5a];
    assert!(memory.read_at(0, &mut data).is_err());
    assert_eq!(data, [0x5a]);
    assert!(memory.write_at(0, &data).is_err());
}

#[test]
fn linked_adapter_node_one_keeps_worker_cache_usable() {
    journey(Case::SecondNode, true);
}

#[test]
fn worker_query_scope_failures_keep_target_zero_and_skip_allocation() {
    for case in [
        Case::ForeignSampler,
        Case::ForeignSegment,
        Case::ForeignNode,
        Case::LateQueryScopeDrift,
        Case::LateAllocatorNodeDrift,
    ] {
        journey(case, false);
    }
}

#[test]
fn selected_candidate_cannot_drop_observed_wddm_guard() {
    journey(Case::SelectionGuardLost, false);
}

#[test]
fn selected_runtime_cannot_expand_reviewed_candidate_target() {
    journey_with_runtime_target(Case::Host, false, TARGET * 2);
}

#[test]
fn newly_available_guard_preserves_candidate_target_and_cache_io() {
    journey(Case::NativeBecameAvailable, true);
}

#[test]
fn selected_runtime_rejects_substituted_budget_binding() {
    journey(Case::SelectionBindingSubstituted, false);
}

#[test]
fn missing_sampler_incarnation_keeps_worker_target_zero() {
    journey(Case::MissingSamplerIncarnation, false);
}

#[test]
fn late_sampler_incarnation_loss_prevents_cache_allocation() {
    journey(Case::LateSamplerIncarnationLoss, false);
}

#[test]
fn wsl_host_catalog_guides_selected_worker_and_revocation_disables_cache() {
    use ramshared_ipc::gpu_catalog::{CatalogSession, CatalogTiming};
    use ramshared_ipc::{HandshakeTranscript, HeartbeatAck, HostChallenge, IPC_VERSION_4};
    use ramshared_vram::host_identity::{
        HostGpuIdentity, HostGpuIdentityReport, HostGpuObservation, ProducerProcess,
    };
    use ramshared_wsl2d::gpu_budget::{GpuBackendKind, select_gpu_candidate};
    use ramshared_wsl2d::host_catalog::SharedHostCatalog;

    struct FakeWslDxg {
        interface_id: String,
    }
    impl GpuBudgetProvider for FakeWslDxg {
        fn snapshot(&self) -> Result<BudgetSnapshot, DxgError> {
            Ok(BudgetSnapshot {
                query_scope: ramshared_dxg::BudgetQueryScope::current_process_local(0),
                adapter: AdapterLuid {
                    high: 0,
                    low: 0x0154_1119,
                },
                adapter_namespace: AdapterLuidNamespace::WslGuest,
                adapter_interface_id: Some(self.interface_id.clone()),
                budget: 8 * GIB,
                current_usage: 0,
                current_reservation: 0,
                available_for_reservation: 8 * GIB,
                sampled_at: Instant::now(),
            })
        }
    }

    let iface = r"\\?\PCI#VEN_10DE&DEV_1F08#runtime-test";
    let guest_budget = budget();
    let host_alloc_id = guest_budget.adapter.clone().unwrap();

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
            allocator: host_alloc_id,
            interface_id: iface.into(),
        }],
    };
    let secret = b"runtime-test-secret-32-bytes!!!!";
    let transcript = HandshakeTranscript {
        min_version: IPC_VERSION_4,
        max_version: IPC_VERSION_4,
        boot_id: "boot-rt".into(),
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
    let host_provider: Arc<dyn HostGpuCatalogProvider> = Arc::new(catalog.clone());

    let config = GpuWorkerConfig {
        target_bytes: TARGET,
        chunk_bytes: TARGET as usize,
        reserve_floor_bytes: 0,
        compression_enabled: false,
    };
    let candidate = super::super::gpu_worker_selection::candidate_from_budget_with_host(
        guest_budget.clone(),
        GpuBackendKind::Cuda,
        0,
        config,
        Some(Arc::clone(&host_provider)),
        |target| match target {
            DxgOpenTarget::Interface { interface_id, .. } => {
                assert_eq!(interface_id, iface);
                Ok(FakeWslDxg {
                    interface_id: iface.into(),
                })
            }
            DxgOpenTarget::Luid(_) => {
                panic!("expected Interface target while host catalog matches")
            }
        },
    )
    .unwrap()
    .unwrap();
    let selected = select_gpu_candidate(&[candidate], None).cloned().unwrap();

    struct WslGuestAllocator {
        snapshot: GpuBudgetSnapshot,
        allocations: Arc<AtomicUsize>,
    }
    impl VramProvider for WslGuestAllocator {
        type Mem<'a> = Memory;
        fn alloc(&self, bytes: usize) -> Result<Memory, VramError> {
            self.allocations.fetch_add(1, Ordering::SeqCst);
            Ok(Memory(vec![0; bytes]))
        }
        fn mem_info(&self) -> Result<(u64, u64), VramError> {
            Ok((8 * GIB, 8 * GIB))
        }
        fn budget_snapshot(&self) -> Result<GpuBudgetSnapshot, VramError> {
            Ok(GpuBudgetSnapshot {
                sampled_at: Instant::now(),
                ..self.snapshot.clone()
            })
        }
    }

    let allocations = Arc::new(AtomicUsize::new(0));
    let provider = WslGuestAllocator {
        snapshot: guest_budget,
        allocations: allocations.clone(),
    };
    let (parent, child) = UnixStream::pair().unwrap();
    let worker_host = Arc::clone(&host_provider);
    let selected_clone = selected.clone();
    let worker = std::thread::spawn(move || {
        run_gpu_worker_with_selected_wddm_and_host(
            child,
            provider,
            &selected_clone,
            config,
            Some(worker_host),
            |target| match target {
                DxgOpenTarget::Interface { interface_id, .. } => {
                    assert_eq!(interface_id, iface);
                    Ok(FakeWslDxg {
                        interface_id: iface.into(),
                    })
                }
                DxgOpenTarget::Luid(_) => {
                    Err(DxgError::Unavailable("unexpected LUID probe".into()))
                }
            },
        )
    });

    let mut client = IpcCacheClient::with_pacing(
        parent,
        Duration::from_secs(2),
        TARGET,
        ramshared_block::ipc_cache_client::PacingPolicy::immediate(),
    );
    client.perform_handshake().unwrap();
    assert_eq!(client.target_bytes(), TARGET);
    let data = vec![0x3c; TARGET as usize];
    assert_eq!(client.promote(0, &data), CacheMutation::Accepted);
    let mut actual = vec![0; data.len()];
    assert_eq!(client.read(0, &mut actual), CacheRead::Hit);
    assert_eq!(actual, data);
    assert_eq!(allocations.load(Ordering::SeqCst), 1);

    // Revoking the host catalog must fail the worker's next WddmBudgetGuard heartbeat.
    catalog.revoke();
    assert!(client.refresh_cached_bytes().is_err());
    drop(client);
    worker.join().unwrap().unwrap();
}
