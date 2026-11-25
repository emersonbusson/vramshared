// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/gpu_budget.rs
//! Conservative composition of allocator-reported and WDDM video-memory budgets.

use std::time::{Duration, Instant};

use ramshared_block::{GpuWorkerConfig, RUNTIME_FREE_BUFFER_BYTES};
use ramshared_dxg::{BudgetSnapshot, GpuBudgetProvider};

mod correspondence;
pub use correspondence::{
    AdapterCorrespondence, DxgOpenTarget, open_matching_wddm_for_snapshot,
    open_matching_wddm_for_snapshot_with_host, open_matching_wddm_provider,
    open_matching_wddm_provider_with_host,
};
use ramshared_vram::{
    GpuAdapterIdentity, GpuBudgetSnapshot, GpuBudgetSource, GpuUtilizationSnapshot,
    ReserveFloorPolicy, VramError, VramProvider,
};

/// Maximum age of a driver-reported budget sample before the freshness
/// predicate refuses it (DT-6), in the millisecond units
/// [`ramshared_vram::GpuBudgetTelemetry::trusted_available_at`] takes.
///
/// This is the product freshness authority
/// ([`ramshared_vram::GPU_BUDGET_MAX_AGE_MS`]) named for this crate. The
/// publisher and its readers must agree on one number, so there is one.
pub use ramshared_vram::GPU_BUDGET_MAX_AGE_MS;

/// [`GPU_BUDGET_MAX_AGE_MS`] as a [`Duration`].
pub const WDDM_BUDGET_MAX_AGE: Duration = Duration::from_millis(GPU_BUDGET_MAX_AGE_MS);

/// Idle serve-loop tick for the cache/status publisher.
///
/// Must stay strictly below [`WDDM_BUDGET_MAX_AGE`]. The loop wakes on this
/// tick to heartbeat the GPU cache worker, and that heartbeat stamps
/// `sampled_at_unix_ms` at request time. A tick equal to the trust window
/// publishes samples that are already stale to `trusted_available_at` by the
/// time a CLI read lands, which is the signature that halted the three-tier
/// stress with `gpu_budget_telemetry_invalid_or_stale`. One fifth of the
/// window matches `MIN_HEARTBEAT_INTERVAL` and leaves the rest as scheduler
/// jitter margin.
pub const SERVE_IDLE_TICK: Duration = Duration::from_secs(1);
/// Display cushion the **caller** supplies to [`safe_broker_slice_bytes`] and
/// [`BudgetAdmissionProvider`]. Not a second floor authority (RF-1): the
/// formula takes the configured reserve as an argument and the sealed policy
/// (`ReserveFloorPolicy::min_floor_bytes`) is what production passes.
pub const BROKER_DISPLAY_RESERVE_BYTES: u64 = 1536 * 1024 * 1024;
const BROKER_SLICE_ALIGNMENT_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GpuBackendKind {
    Cuda,
    Vulkan,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GpuAdapterCandidate {
    pub backend: GpuBackendKind,
    pub ordinal: u32,
    pub identity: GpuAdapterIdentity,
    pub safe_target_bytes: u64,
    /// Captured selection authority. `None` records genuine native absence;
    /// an observed guard must survive selected-provider reopening unchanged.
    pub wddm_binding: Option<AdapterCorrespondence>,
}

impl GpuAdapterCandidate {
    /// Reopening may strengthen initial native absence, but cannot lose or
    /// replace a captured binding or increase the selected target ceiling.
    pub fn validate_runtime_authority(
        &self,
        requested_target_bytes: u64,
        reopened: Option<&AdapterCorrespondence>,
    ) -> Result<(), VramError> {
        if requested_target_bytes > self.safe_target_bytes {
            return Err(VramError::Provider(
                "selected candidate target ceiling exceeded".into(),
            ));
        }
        if self.wddm_binding.as_ref().is_some_and(|binding| {
            reopened.is_none_or(|observed| !binding.matches_current_authority(observed))
        }) {
            return Err(VramError::Provider(
                "selected WDDM budget authority lost or changed".into(),
            ));
        }
        Ok(())
    }
}

/// Keeps the worker's advertised target at or below the candidate target used for ranking.
pub fn worker_config_for_candidate(
    mut config: GpuWorkerConfig,
    candidate_safe_target_bytes: u64,
) -> GpuWorkerConfig {
    config.target_bytes = config.target_bytes.min(candidate_safe_target_bytes);
    // One chunk must fit inside the admitted target. When live headroom is
    // tight the safe target can collapse below the configured chunk size
    // (DT-3 subtracts the display reserve and runtime buffer from what is
    // already free). A chunk larger than the target can never be allocated
    // (`allocate_and_write` refuses when `cached + chunk > target`), so the
    // cache would report ACTIVE yet stay inert at zero bytes.
    if config.target_bytes == 0 {
        config.chunk_bytes = 0;
    } else {
        let max_chunk = usize::try_from(config.target_bytes).unwrap_or(usize::MAX);
        config.chunk_bytes = config.chunk_bytes.min(max_chunk).max(1);
    }
    config
}

/// Calculates the same conservative capacity that the worker can actually commit.
pub fn safe_cache_target(
    budget: &GpuBudgetSnapshot,
    requested_bytes: u64,
    reserve_floor_bytes: u64,
    now: Instant,
) -> Option<u64> {
    safe_cache_target_with_runtime(
        budget,
        requested_bytes,
        reserve_floor_bytes,
        RUNTIME_FREE_BUFFER_BYTES,
        now,
    )
}

fn safe_cache_target_with_runtime(
    budget: &GpuBudgetSnapshot,
    requested_bytes: u64,
    reserve_floor_bytes: u64,
    runtime_headroom_bytes: u64,
    now: Instant,
) -> Option<u64> {
    if budget.source != GpuBudgetSource::DriverReported
        || budget.used_bytes > budget.budget_bytes
        || budget
            .total_bytes
            .is_some_and(|total| budget.budget_bytes > total)
        || budget.adapter.is_none()
        || now.checked_duration_since(budget.sampled_at)? > WDDM_BUDGET_MAX_AGE
    {
        return None;
    }
    let target =
        budget.safe_target_bytes(requested_bytes, reserve_floor_bytes, runtime_headroom_bytes);
    (target > 0).then_some(target)
}

/// Bounds the legacy direct broker's per-slice allocation against fresh live headroom,
/// preserving its canary, configured reserve, and the one named runtime buffer.
pub fn safe_broker_slice_bytes(
    budget: &GpuBudgetSnapshot,
    requested_slice_bytes: u64,
    slices: u16,
    canary_bytes: u64,
    reserve_floor_bytes: u64,
    now: Instant,
) -> Option<u64> {
    if slices == 0 || requested_slice_bytes == 0 {
        return None;
    }
    let requested_total = requested_slice_bytes.checked_mul(u64::from(slices))?;
    let requested_with_canary = requested_total.checked_add(canary_bytes)?;
    let safe_total = safe_cache_target_with_runtime(
        budget,
        requested_with_canary,
        reserve_floor_bytes,
        // DT-5: the one named headroom constant. The former
        // `BROKER_RUNTIME_HEADROOM_BYTES` (768 MiB) was a second source of
        // this buffer and is deleted.
        RUNTIME_FREE_BUFFER_BYTES,
        now,
    )?;
    let data_capacity = safe_total.checked_sub(canary_bytes)?;
    let safe_slice = data_capacity / u64::from(slices);
    if safe_slice == 0 {
        return None;
    }
    if safe_slice >= requested_slice_bytes {
        return Some(requested_slice_bytes);
    }
    let aligned = safe_slice / BROKER_SLICE_ALIGNMENT_BYTES * BROKER_SLICE_ALIGNMENT_BYTES;
    Some(if aligned > 0 { aligned } else { safe_slice })
}

/// Chooses the adapter with the largest safe cache target. Ties prefer CUDA, then the
/// lowest ordinal, with the normalized adapter key as the final stable tie-break.
pub fn select_gpu_candidate<'a>(
    candidates: &'a [GpuAdapterCandidate],
    expected: Option<&GpuAdapterIdentity>,
) -> Option<&'a GpuAdapterCandidate> {
    candidates
        .iter()
        .filter(|candidate| expected.is_none_or(|identity| candidate.identity == *identity))
        .max_by(|left, right| {
            left.safe_target_bytes
                .cmp(&right.safe_target_bytes)
                .then_with(|| backend_tie_rank(left.backend).cmp(&backend_tie_rank(right.backend)))
                .then_with(|| right.ordinal.cmp(&left.ordinal))
                .then_with(|| right.identity.key.cmp(&left.identity.key))
        })
}

const fn backend_tie_rank(backend: GpuBackendKind) -> u8 {
    match backend {
        GpuBackendKind::Cuda => 1,
        GpuBackendKind::Vulkan => 0,
    }
}

/// Returns the lower same-adapter allocator and WDDM headroom snapshot.
///
/// `correspondence` is the proof that both samples describe the same physical
/// adapter. It is established when the WDDM provider is opened; this function
/// re-checks it so a bare pair of snapshots cannot be combined by accident.
pub fn constrained_budget(
    allocator: GpuBudgetSnapshot,
    wddm: BudgetSnapshot,
    now: Instant,
    correspondence: &AdapterCorrespondence,
) -> Result<GpuBudgetSnapshot, VramError> {
    let provider_error =
        |reason: &str| VramError::Provider(format!("WDDM budget guard rejected sample: {reason}"));
    if allocator.source != GpuBudgetSource::DriverReported
        || allocator.used_bytes > allocator.budget_bytes
        || !allocator.can_admit_at(0, now, WDDM_BUDGET_MAX_AGE)
        || now.checked_duration_since(allocator.sampled_at).is_none()
        || now
            .checked_duration_since(allocator.sampled_at)
            .is_some_and(|age| age > WDDM_BUDGET_MAX_AGE)
    {
        return Err(provider_error("allocator_budget_untrusted_or_stale"));
    }
    let wddm_age = now
        .checked_duration_since(wddm.sampled_at)
        .ok_or_else(|| provider_error("wddm_sample_from_future"))?;
    if wddm_age > WDDM_BUDGET_MAX_AGE {
        return Err(provider_error("wddm_sample_stale"));
    }

    correspondence.validate(&allocator, &wddm)?;

    let wddm_available = wddm
        .budget
        .saturating_sub(wddm.current_usage)
        .min(wddm.available_for_reservation);
    let available_bytes = allocator.available_bytes().min(wddm_available);
    let budget_bytes = allocator
        .used_bytes
        .checked_add(available_bytes)
        .ok_or_else(|| provider_error("combined_budget_overflow"))?;
    Ok(GpuBudgetSnapshot {
        adapter: allocator.adapter,
        total_bytes: allocator.total_bytes,
        budget_bytes,
        used_bytes: allocator.used_bytes,
        source: GpuBudgetSource::DriverReported,
        sampled_at: allocator.sampled_at.min(wddm.sampled_at),
    })
}

/// Uses WDDM only when a provider for the exact adapter is available.
pub struct OptionalWddmBudgetProvider<P, B> {
    pub allocator: P,
    pub wddm: Option<(B, AdapterCorrespondence)>,
}

impl<P, B> VramProvider for OptionalWddmBudgetProvider<P, B>
where
    P: VramProvider,
    B: GpuBudgetProvider,
{
    type Mem<'p>
        = P::Mem<'p>
    where
        Self: 'p;

    fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, VramError> {
        self.allocator.alloc(bytes)
    }

    fn mem_info(&self) -> Result<(u64, u64), VramError> {
        let budget = self.budget_snapshot()?;
        // DT-2: the one named capacity expression, never the uncapped physical
        // total. The residency canary (DT-3/9/11) floors against this figure, so
        // a total above the admitted budget would demand free bytes the budget
        // can never supply. An absent heap still falls back to the observed
        // budget rather than to zero.
        let total = ReserveFloorPolicy::helper_capacity(budget.total_bytes, budget.budget_bytes);
        Ok((budget.available_bytes().min(total), total))
    }

    fn budget_snapshot(&self) -> Result<GpuBudgetSnapshot, VramError> {
        match &self.wddm {
            Some((wddm, correspondence)) => {
                let allocator = self.allocator.budget_snapshot()?;
                let wddm = wddm
                    .snapshot()
                    .map_err(|error| VramError::Provider(error.to_string()))?;
                constrained_budget(allocator, wddm, Instant::now(), correspondence)
            }
            None => self.allocator.budget_snapshot(),
        }
    }

    fn utilization_snapshot(&self) -> Option<GpuUtilizationSnapshot> {
        self.allocator.utilization_snapshot()
    }
}

/// Checks each direct broker allocation against fresh headroom after all reserves.
pub struct BudgetAdmissionProvider<P> {
    pub inner: P,
    pub reserve_floor_bytes: u64,
    pub runtime_headroom_bytes: u64,
}

impl<P> BudgetAdmissionProvider<P> {
    pub fn new(inner: P, reserve_floor_bytes: u64, runtime_headroom_bytes: u64) -> Self {
        Self {
            inner,
            reserve_floor_bytes,
            runtime_headroom_bytes,
        }
    }
}

impl<P: VramProvider> VramProvider for BudgetAdmissionProvider<P> {
    type Mem<'p>
        = P::Mem<'p>
    where
        Self: 'p;

    fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, VramError> {
        let budget = self.inner.budget_snapshot()?;
        let bytes = u64::try_from(bytes).map_err(|_| VramError::OutOfMemory)?;
        let required = budget
            .required_free_bytes(self.reserve_floor_bytes, self.runtime_headroom_bytes)
            .checked_add(bytes)
            .ok_or(VramError::OutOfMemory)?;
        if !budget.can_admit(required) {
            return Err(VramError::OutOfMemory);
        }
        self.inner
            .alloc(usize::try_from(bytes).map_err(|_| VramError::OutOfMemory)?)
    }

    fn mem_info(&self) -> Result<(u64, u64), VramError> {
        self.inner.mem_info()
    }

    fn budget_snapshot(&self) -> Result<GpuBudgetSnapshot, VramError> {
        self.inner.budget_snapshot()
    }

    fn utilization_snapshot(&self) -> Option<GpuUtilizationSnapshot> {
        self.inner.utilization_snapshot()
    }
}

/// Forwards allocation to the active provider while constraining every budget read.
pub struct WddmBudgetGuard<P, B> {
    pub allocator: P,
    pub wddm: B,
    pub correspondence: AdapterCorrespondence,
}

impl<P, B> VramProvider for WddmBudgetGuard<P, B>
where
    P: VramProvider,
    B: GpuBudgetProvider,
{
    type Mem<'p>
        = P::Mem<'p>
    where
        Self: 'p;

    fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, VramError> {
        self.allocator.alloc(bytes)
    }

    fn mem_info(&self) -> Result<(u64, u64), VramError> {
        self.allocator.mem_info()
    }

    fn budget_snapshot(&self) -> Result<GpuBudgetSnapshot, VramError> {
        let allocator = self.allocator.budget_snapshot()?;
        let wddm = self
            .wddm
            .snapshot()
            .map_err(|error| VramError::Provider(error.to_string()))?;
        constrained_budget(allocator, wddm, Instant::now(), &self.correspondence)
    }

    fn utilization_snapshot(&self) -> Option<GpuUtilizationSnapshot> {
        self.allocator.utilization_snapshot()
    }
}

#[cfg(test)]
mod tests;
