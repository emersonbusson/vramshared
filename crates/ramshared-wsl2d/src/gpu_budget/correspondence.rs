// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/gpu_budget/correspondence.rs
//! Immutable adapter pairing; cardinality and equal cross-scope LUIDs are not proof.

use super::constrained_budget;
use crate::host_catalog::HostGpuCatalogProvider;
use ramshared_dxg::{
    AdapterLuid, AdapterLuidNamespace, BudgetQueryScope, BudgetRequest, BudgetSnapshot, DxgError,
    GpuBudgetProvider,
};
use ramshared_vram::{GpuAdapterIdentity, GpuBudgetSnapshot, VramError, VramProvider};
use std::sync::Arc;
use std::time::Instant;

/// How a WDDM `/dev/dxg` handle was opened for correspondence verification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DxgOpenTarget<'a> {
    Luid(BudgetRequest),
    Interface {
        interface_id: &'a str,
        physical_adapter_index: u32,
    },
}

#[derive(Clone)]
struct CatalogHandle(Arc<dyn HostGpuCatalogProvider>);

impl std::fmt::Debug for CatalogHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CatalogHandle(..)")
    }
}

impl PartialEq for CatalogHandle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for CatalogHandle {}

/// A verified pair of native identities. Only provider opening can create a binding.
/// Every subsequent sample must retain both identities, their LUID namespace,
/// and live host catalog correspondence for `WslGuest`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdapterCorrespondence {
    allocator: GpuAdapterIdentity,
    wddm: AdapterLuid,
    adapter_namespace: AdapterLuidNamespace,
    adapter_interface_id: Option<String>,
    query_scope: BudgetQueryScope,
    host_catalog: Option<CatalogHandle>,
}

impl AdapterCorrespondence {
    #[cfg(test)]
    pub(super) fn from_scoped_snapshots(
        allocator: &GpuBudgetSnapshot,
        wddm: &BudgetSnapshot,
    ) -> Result<Self, String> {
        Self::from_scoped_snapshots_with_host(allocator, wddm, None)
    }

    pub(super) fn from_scoped_snapshots_with_host(
        allocator: &GpuBudgetSnapshot,
        wddm: &BudgetSnapshot,
        host_catalog: Option<Arc<dyn HostGpuCatalogProvider>>,
    ) -> Result<Self, String> {
        let identity = allocator
            .adapter
            .as_ref()
            .ok_or("allocator adapter missing")?;
        if !identity.is_stable_driver_identity() {
            return Err("allocator adapter identity is untrusted".into());
        }
        let alloc_luid = identity
            .luid
            .as_deref()
            .ok_or("allocator Windows LUID missing")?;
        let alloc_luid =
            AdapterLuid::parse_normalized(alloc_luid).map_err(|error| error.to_string())?;
        let node = identity
            .wddm_physical_adapter_index()
            .ok_or("allocator WDDM physical node missing or invalid")?;
        if !wddm.query_scope.is_current_process_local()
            || wddm.query_scope.physical_adapter_index != node
        {
            return Err(
                "WDDM query scope does not match the current allocator process and physical node"
                    .into(),
            );
        }

        match wddm.adapter_namespace {
            AdapterLuidNamespace::WindowsHost => {
                if alloc_luid != wddm.adapter {
                    return Err(
                        "WDDM returned an adapter different from the selected allocator LUID"
                            .into(),
                    );
                }
                Ok(Self {
                    query_scope: wddm.query_scope.clone(),
                    allocator: identity.clone(),
                    wddm: wddm.adapter,
                    adapter_namespace: AdapterLuidNamespace::WindowsHost,
                    adapter_interface_id: None,
                    host_catalog: None,
                })
            }
            AdapterLuidNamespace::WslGuest => {
                let catalog = host_catalog.ok_or(
                    "WDDM adapter correspondence unproven: guest and host LUID namespaces differ",
                )?;
                let report = catalog
                    .current_report()
                    .ok_or("WDDM adapter correspondence unproven: host GPU catalog unavailable")?;
                let guest_iface = wddm
                    .adapter_interface_id
                    .as_deref()
                    .ok_or("WDDM adapter correspondence unproven: vGPU interface ID missing")?;
                let row = report
                    .verify_wsl_guest_adapter(identity, guest_iface, node)
                    .map_err(|error| format!("WDDM adapter correspondence unproven: {error}"))?;
                let host_luid = row
                    .allocator
                    .luid
                    .as_deref()
                    .ok_or("WDDM adapter correspondence unproven: host adapter LUID missing")?;
                let host_luid =
                    AdapterLuid::parse_normalized(host_luid).map_err(|error| error.to_string())?;
                if host_luid != alloc_luid {
                    return Err(
                        "WDDM adapter correspondence unproven: host and guest allocator LUIDs differ"
                            .into(),
                    );
                }
                Ok(Self {
                    query_scope: wddm.query_scope.clone(),
                    allocator: identity.clone(),
                    wddm: wddm.adapter,
                    adapter_namespace: AdapterLuidNamespace::WslGuest,
                    adapter_interface_id: Some(guest_iface.to_string()),
                    host_catalog: Some(CatalogHandle(catalog)),
                })
            }
        }
    }

    pub(super) fn matches_current_authority(&self, observed: &Self) -> bool {
        let iface_match = match (&self.adapter_interface_id, &observed.adapter_interface_id) {
            (None, None) => true,
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
            _ => false,
        };
        let catalog_match = match self.adapter_namespace {
            AdapterLuidNamespace::WindowsHost => true,
            AdapterLuidNamespace::WslGuest => {
                self.host_catalog.is_some() && observed.host_catalog.is_some()
            }
        };
        self.allocator == observed.allocator
            && self.wddm == observed.wddm
            && self.adapter_namespace == observed.adapter_namespace
            && iface_match
            && catalog_match
            && self
                .query_scope
                .matches_current_scope(&observed.query_scope)
    }

    pub(super) fn validate(
        &self,
        allocator: &GpuBudgetSnapshot,
        wddm: &BudgetSnapshot,
    ) -> Result<(), VramError> {
        if allocator.adapter.as_ref() != Some(&self.allocator)
            || wddm.adapter != self.wddm
            || wddm.adapter_namespace != self.adapter_namespace
            || !self.query_scope.matches_current_scope(&wddm.query_scope)
        {
            return Err(VramError::Provider(
                "WDDM budget guard rejected sample: adapter binding changed".into(),
            ));
        }
        if self.adapter_namespace == AdapterLuidNamespace::WslGuest {
            let expected_iface = self.adapter_interface_id.as_deref().ok_or_else(|| {
                VramError::Provider(
                    "WDDM budget guard rejected sample: missing bound interface ID".into(),
                )
            })?;
            let observed_iface = wddm.adapter_interface_id.as_deref().ok_or_else(|| {
                VramError::Provider(
                    "WDDM budget guard rejected sample: observed vGPU interface ID missing".into(),
                )
            })?;
            if !expected_iface.eq_ignore_ascii_case(observed_iface) {
                return Err(VramError::Provider(
                    "WDDM budget guard rejected sample: vGPU interface ID changed".into(),
                ));
            }
            let catalog = self.host_catalog.as_ref().ok_or_else(|| {
                VramError::Provider(
                    "WDDM budget guard rejected sample: host GPU catalog missing".into(),
                )
            })?;
            let report = catalog.0.current_report().ok_or_else(|| {
                VramError::Provider(
                    "WDDM budget guard rejected sample: host GPU catalog expired or revoked".into(),
                )
            })?;
            report
                .verify_wsl_guest_adapter(
                    &self.allocator,
                    observed_iface,
                    self.query_scope.physical_adapter_index,
                )
                .map_err(|error| {
                    VramError::Provider(format!("WDDM budget guard rejected sample: {error}"))
                })?;
        }
        Ok(())
    }
}

/// Open an exact, same-namespace WDDM provider. Genuine native unavailability
/// permits allocator-only startup; unproven correspondence is a cache refusal.
pub fn open_matching_wddm_provider<P, B, F>(
    allocator: &P,
    mut open: F,
) -> Result<Option<(B, AdapterCorrespondence)>, String>
where
    P: VramProvider,
    B: GpuBudgetProvider,
    F: FnMut(BudgetRequest) -> Result<B, DxgError>,
{
    open_matching_wddm_provider_with_host(allocator, None, |target| match target {
        DxgOpenTarget::Luid(request) => open(request),
        DxgOpenTarget::Interface { interface_id, .. } => {
            Err(DxgError::InterfaceNotFound(interface_id.to_string()))
        }
    })
}

pub fn open_matching_wddm_provider_with_host<P, B, F>(
    allocator: &P,
    host_catalog: Option<Arc<dyn HostGpuCatalogProvider>>,
    open: F,
) -> Result<Option<(B, AdapterCorrespondence)>, String>
where
    P: VramProvider,
    B: GpuBudgetProvider,
    F: FnMut(DxgOpenTarget<'_>) -> Result<B, DxgError>,
{
    match allocator.budget_snapshot() {
        Ok(snapshot) => open_matching_wddm_for_snapshot_with_host(&snapshot, host_catalog, open),
        Err(error) => Err(format!("allocator budget observation failed: {error}")),
    }
}

/// The captured budget is observation only; neither a sole adapter nor a numeric
/// LUID collision across namespaces can authorize combining physical budgets.
pub fn open_matching_wddm_for_snapshot<B, F>(
    snapshot: &GpuBudgetSnapshot,
    mut open: F,
) -> Result<Option<(B, AdapterCorrespondence)>, String>
where
    B: GpuBudgetProvider,
    F: FnMut(BudgetRequest) -> Result<B, DxgError>,
{
    open_matching_wddm_for_snapshot_with_host(snapshot, None, |target| match target {
        DxgOpenTarget::Luid(request) => open(request),
        DxgOpenTarget::Interface { interface_id, .. } => {
            Err(DxgError::InterfaceNotFound(interface_id.to_string()))
        }
    })
}

pub fn open_matching_wddm_for_snapshot_with_host<B, F>(
    snapshot: &GpuBudgetSnapshot,
    host_catalog: Option<Arc<dyn HostGpuCatalogProvider>>,
    mut open: F,
) -> Result<Option<(B, AdapterCorrespondence)>, String>
where
    B: GpuBudgetProvider,
    F: FnMut(DxgOpenTarget<'_>) -> Result<B, DxgError>,
{
    if !snapshot.can_admit(0)
        || snapshot.used_bytes > snapshot.budget_bytes
        || snapshot
            .total_bytes
            .is_some_and(|total| snapshot.budget_bytes > total)
    {
        return Err("allocator budget is untrusted, stale or inconsistent".into());
    }
    let identity = snapshot
        .adapter
        .as_ref()
        .ok_or("allocator adapter missing")?;
    if !identity.is_stable_driver_identity() {
        return Err("allocator adapter identity is untrusted".into());
    }
    let requested = identity
        .luid
        .as_deref()
        .map(AdapterLuid::parse_normalized)
        .transpose()
        .map_err(|error| error.to_string())?;
    let node = identity.wddm_physical_adapter_index();
    let host_row_iface = match (&host_catalog, node) {
        (Some(catalog), Some(node)) => match catalog.current_report() {
            Some(report) => report
                .find_wsl_host_adapter(identity, node)
                .map_err(|error| format!("WDDM adapter correspondence unproven: {error}"))?
                .map(|row| row.interface_id.clone()),
            None => None,
        },
        _ => None,
    };
    let provider = match (host_row_iface.as_deref(), node) {
        (Some(interface_id), Some(physical_adapter_index)) => {
            match open(DxgOpenTarget::Interface {
                interface_id,
                physical_adapter_index,
            }) {
                Ok(provider) => provider,
                Err(error) if error.permits_startup_fallback() => return Ok(None),
                Err(error) => {
                    return Err(format!("WDDM adapter correspondence unproven: {error}"));
                }
            }
        }
        _ => {
            let request = BudgetRequest {
                adapter: requested,
                physical_adapter_index: node,
            };
            match open(DxgOpenTarget::Luid(request)) {
                Ok(provider) => provider,
                Err(error) if error.permits_startup_fallback() => return Ok(None),
                Err(error) => {
                    return Err(format!("WDDM adapter correspondence unproven: {error}"));
                }
            }
        }
    };
    if requested.is_none() {
        return Err("WDDM adapter correspondence unproven: allocator Windows LUID missing".into());
    }
    let observed = provider.snapshot().map_err(|error| error.to_string())?;
    let binding =
        AdapterCorrespondence::from_scoped_snapshots_with_host(snapshot, &observed, host_catalog)?;
    constrained_budget(snapshot.clone(), observed, Instant::now(), &binding)
        .map_err(|error| error.to_string())?;
    Ok(Some((provider, binding)))
}

#[cfg(test)]
mod tests;
