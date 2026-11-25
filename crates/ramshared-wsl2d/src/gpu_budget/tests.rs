// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/gpu_budget/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
pub(super) use ramshared_dxg::{AdapterLuid, AdapterLuidNamespace, DxgError};
pub(super) use std::cell::Cell;
pub(super) use std::rc::Rc;

pub(super) use ramshared_block::GpuCacheWorker;
pub(super) use ramshared_vram::{GpuAdapterIdentity, VramMemory};

pub(super) fn allocator_budget(sampled_at: Instant) -> GpuBudgetSnapshot {
    GpuBudgetSnapshot {
        adapter: Some(GpuAdapterIdentity {
            node_mask: Some(1),
            backend: "cuda".into(),
            key: "12000000000000000000000000000000".into(),
            luid: Some("aabbccdd:00001122".into()),
        }),
        total_bytes: Some(8 * 1024 * 1024 * 1024),
        budget_bytes: 6 * 1024 * 1024 * 1024,
        used_bytes: 1024 * 1024 * 1024,
        source: GpuBudgetSource::DriverReported,
        sampled_at,
    }
}

/// ITEM-4: the broker admits at exactly the shared helper threshold over
/// an identical snapshot, with the headroom from the one named constant.
/// A second reserve formula, or any admission below the helper value,
/// trips this.
#[test]
pub(super) fn broker_admission_uses_resolved_policy() {
    const GIB: u64 = 1024 * 1024 * 1024;
    let now = Instant::now();
    let budget = allocator_budget(now);
    let policy = ramshared_vram::ReserveFloorPolicy::from_manifest(1, 20).expect("sealed literals");
    let capacity = budget
        .total_bytes
        .unwrap_or(budget.budget_bytes)
        .min(budget.budget_bytes);
    let configured = policy.configured_reserve_bytes(capacity);

    // The one helper value, and the policy's own three-term floor, agree.
    assert_eq!(
        budget.required_free_bytes(configured, RUNTIME_FREE_BUFFER_BYTES),
        policy.enforced_free_floor_bytes(capacity, RUNTIME_FREE_BUFFER_BYTES)
    );

    // The broker's target is the shared `safe_target_bytes`, not an
    // independent reserve.
    let requested = 10 * GIB;
    let expected = budget.safe_target_bytes(requested, configured, RUNTIME_FREE_BUFFER_BYTES);
    assert_eq!(
        safe_cache_target(&budget, requested, configured, now),
        Some(expected)
    );

    // An admission request that would land below the helper threshold is
    // refused (target 0 collapses to `None`).
    let shortfall = expected.saturating_sub(1);
    assert!(
        budget.required_free_bytes(configured, RUNTIME_FREE_BUFFER_BYTES) > 0,
        "fixture must exercise a non-zero floor"
    );
    assert!(shortfall < expected, "boundary case must be strictly below");
}

/// DT-2: `mem_info` reports the one named capacity expression, not the
/// uncapped physical total. The residency canary (DT-3/9/11) floors against
/// this figure, so a total above the admitted budget would demand free bytes
/// the budget can never supply. An absent heap still falls back to the
/// observed budget and never to zero.
#[test]
pub(super) fn mem_info_reports_the_named_dt2_capacity_not_the_uncapped_physical_total() {
    const GIB: u64 = 1024 * 1024 * 1024;
    let now = Instant::now();
    let mut snapshot = allocator_budget(now);
    let physical_total = snapshot.total_bytes.expect("fixture reports a heap total");
    assert_eq!(physical_total, 8 * GIB);
    assert_eq!(snapshot.budget_bytes, 6 * GIB);

    let uncapped_provider = |budget: GpuBudgetSnapshot| OptionalWddmBudgetProvider {
        allocator: Allocator {
            budget,
            allocations: Rc::new(Cell::new(0)),
            fail_budget: false,
        },
        wddm: None::<(FailingWddm, AdapterCorrespondence)>,
    };

    // A measured heap total above the admitted budget is capped at the budget.
    let (_, total) = uncapped_provider(snapshot.clone())
        .mem_info()
        .expect("fixture snapshot is fresh");
    assert_eq!(total, snapshot.budget_bytes);
    assert_ne!(total, physical_total);
    assert_eq!(
        total,
        ReserveFloorPolicy::helper_capacity(snapshot.total_bytes, snapshot.budget_bytes)
    );

    // Absence is not a measured zero: it falls back to the observed budget.
    snapshot.total_bytes = None;
    let (_, total) = uncapped_provider(snapshot.clone())
        .mem_info()
        .expect("fixture snapshot is fresh");
    assert_eq!(total, snapshot.budget_bytes);
    assert_ne!(total, 0);
    assert_eq!(
        total,
        ReserveFloorPolicy::helper_capacity(snapshot.total_bytes, snapshot.budget_bytes)
    );
}

#[test]
pub(super) fn candidate_target_applies_reserve_freshness_and_request_cap() {
    const GIB: u64 = 1024 * 1024 * 1024;
    let now = Instant::now();
    let budget = allocator_budget(now);
    assert_eq!(safe_cache_target(&budget, 500, 100, now), Some(500));
    // Available 5G retains ceil(budget 6G / 5) plus runtime headroom.
    assert_eq!(
        safe_cache_target(&budget, 10 * GIB, 100, now),
        Some(5 * GIB - (6 * GIB).div_ceil(5) - RUNTIME_FREE_BUFFER_BYTES)
    );
    assert_eq!(safe_cache_target(&budget, 10 * GIB, 6 * GIB, now), None);
    assert_eq!(
        safe_cache_target(
            &allocator_budget(now - WDDM_BUDGET_MAX_AGE - Duration::from_millis(1)),
            500,
            100,
            now
        ),
        None
    );
    assert_eq!(
        safe_cache_target(
            &allocator_budget(now + Duration::from_secs(1)),
            500,
            100,
            now
        ),
        None
    );
}

#[test]
pub(super) fn direct_broker_slice_preserves_live_reserve_canary_and_alignment() {
    const GIB: u64 = 1024 * 1024 * 1024;
    let now = Instant::now();
    let canary = 16 * 1024 * 1024;
    // Retain the configured reserve, runtime headroom and canary before
    // truncating the slice to the required alignment boundary.
    let expected_unaligned =
        5 * GIB - BROKER_DISPLAY_RESERVE_BYTES - RUNTIME_FREE_BUFFER_BYTES - canary;
    let expected_aligned =
        expected_unaligned / BROKER_SLICE_ALIGNMENT_BYTES * BROKER_SLICE_ALIGNMENT_BYTES;

    assert_eq!(
        safe_broker_slice_bytes(
            &allocator_budget(now),
            6 * GIB,
            1,
            canary,
            BROKER_DISPLAY_RESERVE_BYTES,
            now
        ),
        Some(expected_aligned)
    );
    assert_eq!(
        safe_broker_slice_bytes(
            &allocator_budget(now),
            u64::MAX,
            2,
            canary,
            BROKER_DISPLAY_RESERVE_BYTES,
            now
        ),
        None,
        "slice multiplication overflow must refuse allocation"
    );
    assert_eq!(
        safe_broker_slice_bytes(
            &allocator_budget(now),
            1,
            0,
            canary,
            BROKER_DISPLAY_RESERVE_BYTES,
            now
        ),
        None
    );
}

#[test]
pub(super) fn selected_gpu_identity_survives_capacity_and_enumeration_changes() {
    let identity = |key: char| GpuAdapterIdentity {
        node_mask: None,
        backend: "cuda".into(),
        key: key.to_string().repeat(32),
        luid: None,
    };
    let expected = identity('a');
    let mut devices = vec![
        GpuAdapterCandidate {
            wddm_binding: None,
            backend: GpuBackendKind::Cuda,
            ordinal: 0,
            identity: expected.clone(),
            safe_target_bytes: 6 << 30,
        },
        GpuAdapterCandidate {
            wddm_binding: None,
            backend: GpuBackendKind::Cuda,
            ordinal: 1,
            identity: identity('b'),
            safe_target_bytes: 96 << 30,
        },
    ];
    assert_ne!(
        select_gpu_candidate(&devices, None).unwrap().identity,
        expected
    );
    assert_eq!(
        select_gpu_candidate(&devices, Some(&expected))
            .unwrap()
            .identity,
        expected
    );
    devices.reverse();
    devices[0].ordinal = 0;
    devices[1].ordinal = 1;
    devices[1].safe_target_bytes = 1 << 30;
    assert_eq!(
        select_gpu_candidate(&devices, Some(&expected))
            .unwrap()
            .ordinal,
        1
    );
    devices.pop();
    assert!(select_gpu_candidate(&devices, Some(&expected)).is_none());
    assert!(select_gpu_candidate(&[], Some(&expected)).is_none());
}

#[test]
pub(super) fn candidate_selection_prefers_largest_safe_target_then_stable_ties() {
    let identity = |key: &str| GpuAdapterIdentity {
        node_mask: None,
        backend: "test".into(),
        key: key.into(),
        luid: None,
    };
    let candidates = [
        GpuAdapterCandidate {
            wddm_binding: None,
            backend: GpuBackendKind::Cuda,
            ordinal: 1,
            identity: identity("cuda-1"),
            safe_target_bytes: 2_000,
        },
        GpuAdapterCandidate {
            wddm_binding: None,
            backend: GpuBackendKind::Vulkan,
            ordinal: 0,
            identity: identity("vulkan-0"),
            safe_target_bytes: 4_000,
        },
        GpuAdapterCandidate {
            wddm_binding: None,
            backend: GpuBackendKind::Cuda,
            ordinal: 0,
            identity: identity("cuda-0"),
            safe_target_bytes: 4_000,
        },
    ];
    let selected = select_gpu_candidate(&candidates, None).expect("candidate exists");
    assert_eq!(selected.backend, GpuBackendKind::Cuda);
    assert_eq!(selected.ordinal, 0);
}

#[test]
pub(super) fn selected_candidate_caps_worker_target_without_expanding_user_request() {
    let requested = GpuWorkerConfig {
        target_bytes: 4_000,
        chunk_bytes: 512,
        reserve_floor_bytes: 1_000,
        compression_enabled: false,
    };

    assert_eq!(
        worker_config_for_candidate(requested, 1_000).target_bytes,
        1_000
    );
    assert_eq!(
        worker_config_for_candidate(requested, 8_000).target_bytes,
        4_000
    );
}

#[test]
pub(super) fn selected_candidate_shrinks_chunk_to_fit_tight_safe_target() {
    // Live headroom can collapse below one configured chunk (DT-3 reserve
    // plus runtime buffer subtracted from already-free bytes). The worker
    // must still be able to place at least one chunk inside the target.
    let requested = GpuWorkerConfig {
        target_bytes: 4 * 1024 * 1024 * 1024,
        chunk_bytes: 128 * 1024 * 1024,
        reserve_floor_bytes: 2 * 1024 * 1024 * 1024,
        compression_enabled: false,
    };

    let fitted = worker_config_for_candidate(requested, 34_603_008);
    assert_eq!(fitted.target_bytes, 34_603_008);
    assert_eq!(fitted.chunk_bytes, 34_603_008);

    // A chunk that already fits is left alone.
    let roomy = worker_config_for_candidate(requested, 512 * 1024 * 1024);
    assert_eq!(roomy.chunk_bytes, 128 * 1024 * 1024);

    // Zero target disables chunking rather than leaving a stale size.
    let none = worker_config_for_candidate(requested, 0);
    assert_eq!(none.target_bytes, 0);
    assert_eq!(none.chunk_bytes, 0);
}

pub(super) fn wddm_budget(high: u32, low: u32, sampled_at: Instant) -> BudgetSnapshot {
    BudgetSnapshot {
        query_scope: ramshared_dxg::BudgetQueryScope::current_process_local(0),
        adapter: AdapterLuid { high, low },
        adapter_namespace: AdapterLuidNamespace::WindowsHost,
        adapter_interface_id: None,
        budget: 900,
        current_usage: 300,
        current_reservation: 100,
        available_for_reservation: 450,
        sampled_at,
    }
}

pub(super) fn test_binding() -> AdapterCorrespondence {
    let now = Instant::now();
    AdapterCorrespondence::from_scoped_snapshots(
        &allocator_budget(now),
        &wddm_budget(0xaabb_ccdd, 0x1122, now),
    )
    .unwrap()
}

#[test]
pub(super) fn same_adapter_budget_uses_lower_allocator_and_wddm_headroom() {
    let now = Instant::now();
    let combined = constrained_budget(
        allocator_budget(now),
        wddm_budget(0xaabb_ccdd, 0x1122, now),
        now,
        &test_binding(),
    )
    .expect("same adapter budgets must combine");

    assert_eq!(combined.budget_bytes, 1024 * 1024 * 1024 + 450);
    assert_eq!(combined.used_bytes, 1024 * 1024 * 1024);
    assert_eq!(combined.available_bytes(), 450);
    assert!(combined.can_admit_at(450, now, WDDM_BUDGET_MAX_AGE));
    assert!(!combined.can_admit_at(451, now, WDDM_BUDGET_MAX_AGE));
}

#[test]
pub(super) fn mismatched_stale_future_and_malformed_budgets_are_rejected() {
    let now = Instant::now();
    assert!(
        constrained_budget(
            allocator_budget(now),
            wddm_budget(0xaabb_ccdd, 0x3344, now),
            now,
            &test_binding(),
        )
        .is_err()
    );

    let stale = wddm_budget(
        0xaabb_ccdd,
        0x1122,
        now - WDDM_BUDGET_MAX_AGE - Duration::from_millis(1),
    );
    assert!(constrained_budget(allocator_budget(now), stale, now, &test_binding(),).is_err());
    assert!(
        constrained_budget(
            allocator_budget(now + Duration::from_millis(1)),
            wddm_budget(0xaabb_ccdd, 0x1122, now),
            now,
            &test_binding(),
        )
        .is_err()
    );

    let mut malformed = allocator_budget(now);
    malformed.used_bytes = malformed.budget_bytes + 1;
    assert!(
        constrained_budget(
            malformed,
            wddm_budget(0xaabb_ccdd, 0x1122, now),
            now,
            &test_binding(),
        )
        .is_err()
    );
}

#[test]
pub(super) fn shared_binding_refuses_split_wsl2_luid_namespaces() {
    let now = Instant::now();
    assert!(
        constrained_budget(
            allocator_budget(now),
            wddm_budget(0x0102, 0x0304, now),
            now,
            &test_binding(),
        )
        .is_err()
    );
}

pub(super) struct Memory(Vec<u8>);

impl VramMemory for Memory {
    fn len(&self) -> usize {
        self.0.len()
    }

    fn zero(&mut self) -> Result<(), VramError> {
        self.0.fill(0);
        Ok(())
    }

    fn read_at(&self, off: u64, dst: &mut [u8]) -> Result<(), VramError> {
        let size = u64::try_from(self.0.len()).unwrap_or(u64::MAX);
        let len = u64::try_from(dst.len()).unwrap_or(u64::MAX);
        let start = usize::try_from(off).map_err(|_| VramError::OutOfRange { off, len, size })?;
        let end = start
            .checked_add(dst.len())
            .ok_or(VramError::OutOfRange { off, len, size })?;
        dst.copy_from_slice(self.0.get(start..end).ok_or(VramError::OutOfRange {
            off,
            len,
            size,
        })?);
        Ok(())
    }

    fn write_at(&mut self, off: u64, src: &[u8]) -> Result<(), VramError> {
        let size = u64::try_from(self.0.len()).unwrap_or(u64::MAX);
        let len = u64::try_from(src.len()).unwrap_or(u64::MAX);
        let start = usize::try_from(off).map_err(|_| VramError::OutOfRange { off, len, size })?;
        let end = start
            .checked_add(src.len())
            .ok_or(VramError::OutOfRange { off, len, size })?;
        self.0
            .get_mut(start..end)
            .ok_or(VramError::OutOfRange { off, len, size })?
            .copy_from_slice(src);
        Ok(())
    }
}

pub(super) struct Allocator {
    pub(super) budget: GpuBudgetSnapshot,
    pub(super) allocations: Rc<Cell<usize>>,
    pub(super) fail_budget: bool,
}

impl VramProvider for Allocator {
    type Mem<'p> = Memory;

    fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, VramError> {
        self.allocations.set(self.allocations.get() + 1);
        Ok(Memory(vec![0; bytes]))
    }

    fn mem_info(&self) -> Result<(u64, u64), VramError> {
        Ok((
            self.budget.available_bytes(),
            self.budget.total_bytes.unwrap_or(0),
        ))
    }

    fn budget_snapshot(&self) -> Result<GpuBudgetSnapshot, VramError> {
        if self.fail_budget {
            Err(VramError::Provider("injected allocator failure".into()))
        } else {
            Ok(self.budget.clone())
        }
    }
}

pub(super) struct FailingWddm;

impl GpuBudgetProvider for FailingWddm {
    fn snapshot(&self) -> Result<BudgetSnapshot, DxgError> {
        Err(DxgError::Io("injected query failure".into()))
    }
}

#[path = "tests/provider_and_admission.rs"]
mod provider_and_admission;
