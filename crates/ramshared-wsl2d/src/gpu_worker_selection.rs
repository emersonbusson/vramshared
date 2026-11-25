// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/gpu_worker_selection.rs
//! Candidate policy for captured budgets and selected-provider revalidation.
#[cfg(test)]
mod tests;
use ramshared_block::GpuWorkerConfig;
use ramshared_dxg::{BudgetRequest, DxgError, GpuBudgetProvider};
#[cfg(test)]
use ramshared_vram::VramProvider;
use ramshared_vram::{GpuAdapterIdentity, GpuBudgetSnapshot};
use ramshared_vulkan::VulkanDeviceObservation;
use ramshared_wsl2d::gpu_budget::{
    DxgOpenTarget, GpuAdapterCandidate, GpuBackendKind, constrained_budget, safe_cache_target,
};
use ramshared_wsl2d::host_catalog::HostGpuCatalogProvider;
use std::sync::Arc;
use std::time::Instant;

#[cfg(test)]
pub(super) fn vulkan_candidate<B, F>(
    observation: VulkanDeviceObservation,
    expected: Option<&GpuAdapterIdentity>,
    config: GpuWorkerConfig,
    mut open: F,
) -> Result<Option<GpuAdapterCandidate>, String>
where
    B: GpuBudgetProvider,
    F: FnMut(BudgetRequest) -> Result<B, DxgError>,
{
    vulkan_candidate_with_host(observation, expected, config, None, |target| match target {
        DxgOpenTarget::Luid(req) => open(req),
        DxgOpenTarget::Interface { interface_id, .. } => {
            Err(DxgError::InterfaceNotFound(interface_id.to_string()))
        }
    })
}

pub(super) fn vulkan_candidate_with_host<B, F>(
    observation: VulkanDeviceObservation,
    expected: Option<&GpuAdapterIdentity>,
    config: GpuWorkerConfig,
    host_catalog: Option<Arc<dyn HostGpuCatalogProvider>>,
    open: F,
) -> Result<Option<GpuAdapterCandidate>, String>
where
    B: GpuBudgetProvider,
    F: FnMut(DxgOpenTarget<'_>) -> Result<B, DxgError>,
{
    if !observation.hardware_gpu {
        eprintln!(
            "[ramsharedd] Vulkan adapter {} skipped: not a hardware GPU (name={:?})",
            observation.ordinal, observation.name
        );
        return Ok(None);
    }
    if expected.is_some_and(|identity| observation.budget.adapter.as_ref() != Some(identity)) {
        return Ok(None);
    }
    candidate_from_budget_with_host(
        observation.budget,
        GpuBackendKind::Vulkan,
        observation.ordinal,
        config,
        host_catalog,
        open,
    )
}

#[cfg(test)]
pub(super) fn gpu_candidate<P: VramProvider, B, F>(
    allocator: &P,
    backend: GpuBackendKind,
    ordinal: u32,
    config: GpuWorkerConfig,
    open: F,
) -> Result<Option<GpuAdapterCandidate>, String>
where
    B: GpuBudgetProvider,
    F: FnMut(BudgetRequest) -> Result<B, DxgError>,
{
    let budget = allocator
        .budget_snapshot()
        .map_err(|error| error.to_string())?;
    candidate_from_budget(budget, backend, ordinal, config, open)
}

#[cfg(test)]
pub(super) fn candidate_from_budget<B, F>(
    budget: GpuBudgetSnapshot,
    backend: GpuBackendKind,
    ordinal: u32,
    config: GpuWorkerConfig,
    mut open: F,
) -> Result<Option<GpuAdapterCandidate>, String>
where
    B: GpuBudgetProvider,
    F: FnMut(BudgetRequest) -> Result<B, DxgError>,
{
    candidate_from_budget_with_host(
        budget,
        backend,
        ordinal,
        config,
        None,
        |target| match target {
            DxgOpenTarget::Luid(req) => open(req),
            DxgOpenTarget::Interface { interface_id, .. } => {
                Err(DxgError::InterfaceNotFound(interface_id.to_string()))
            }
        },
    )
}

pub(super) fn candidate_from_budget_with_host<B, F>(
    budget: GpuBudgetSnapshot,
    backend: GpuBackendKind,
    ordinal: u32,
    config: GpuWorkerConfig,
    host_catalog: Option<Arc<dyn HostGpuCatalogProvider>>,
    open: F,
) -> Result<Option<GpuAdapterCandidate>, String>
where
    B: GpuBudgetProvider,
    F: FnMut(DxgOpenTarget<'_>) -> Result<B, DxgError>,
{
    let (budget, wddm_binding) = scoped_budget_with_host(budget, host_catalog, open)?;
    let Some(identity) = budget.adapter.clone() else {
        return Ok(None);
    };
    let Some(safe_target_bytes) = safe_cache_target(
        &budget,
        config.target_bytes,
        config.reserve_floor_bytes,
        Instant::now(),
    ) else {
        return Ok(None);
    };
    Ok(Some(GpuAdapterCandidate {
        wddm_binding,
        backend,
        ordinal,
        identity,
        safe_target_bytes,
    }))
}

pub(super) fn scoped_budget<B, F>(
    budget: GpuBudgetSnapshot,
    mut open: F,
) -> Result<
    (
        GpuBudgetSnapshot,
        Option<ramshared_wsl2d::gpu_budget::AdapterCorrespondence>,
    ),
    String,
>
where
    B: GpuBudgetProvider,
    F: FnMut(BudgetRequest) -> Result<B, DxgError>,
{
    scoped_budget_with_host(budget, None, |target| match target {
        DxgOpenTarget::Luid(req) => open(req),
        DxgOpenTarget::Interface { interface_id, .. } => {
            Err(DxgError::InterfaceNotFound(interface_id.to_string()))
        }
    })
}

pub(super) fn scoped_budget_with_host<B, F>(
    budget: GpuBudgetSnapshot,
    host_catalog: Option<Arc<dyn HostGpuCatalogProvider>>,
    open: F,
) -> Result<
    (
        GpuBudgetSnapshot,
        Option<ramshared_wsl2d::gpu_budget::AdapterCorrespondence>,
    ),
    String,
>
where
    B: GpuBudgetProvider,
    F: FnMut(DxgOpenTarget<'_>) -> Result<B, DxgError>,
{
    let wddm = ramshared_wsl2d::gpu_budget::open_matching_wddm_for_snapshot_with_host(
        &budget,
        host_catalog,
        open,
    )?;
    let (budget, wddm_binding) = match wddm {
        Some((wddm, correspondence)) => (
            constrained_budget(
                budget,
                wddm.snapshot().map_err(|error| error.to_string())?,
                Instant::now(),
                &correspondence,
            )
            .map_err(|error| error.to_string())?,
            Some(correspondence),
        ),
        None => (budget, None),
    };
    Ok((budget, wddm_binding))
}

pub(super) fn revalidate_selected_adapter(
    budget: GpuBudgetSnapshot,
    expected_identity: &GpuAdapterIdentity,
    config: GpuWorkerConfig,
) -> Result<(), String> {
    if budget.adapter.as_ref() != Some(expected_identity) {
        return Err("selected adapter identity changed during revalidation".into());
    }
    if safe_cache_target(
        &budget,
        config.target_bytes,
        config.reserve_floor_bytes,
        Instant::now(),
    )
    .is_none()
    {
        return Err("selected adapter no longer has a fresh safe cache budget".into());
    }
    Ok(())
}

/// Observation accepts exhausted driver budgets, but never local estimates.
pub(super) fn observation_budget<B, F>(
    budget: GpuBudgetSnapshot,
    open: F,
) -> Result<GpuBudgetSnapshot, String>
where
    B: GpuBudgetProvider,
    F: FnMut(BudgetRequest) -> Result<B, DxgError>,
{
    let (budget, _) = scoped_budget(budget, open)?;
    if budget.used_bytes > budget.budget_bytes
        || !budget
            .adapter
            .as_ref()
            .is_some_and(GpuAdapterIdentity::is_stable_driver_identity)
        || !budget.can_admit_at(
            0,
            Instant::now(),
            std::time::Duration::from_millis(ramshared_vram::GPU_BUDGET_MAX_AGE_MS),
        )
    {
        return Err("native observation budget is untrusted or stale".into());
    }
    Ok(budget)
}
