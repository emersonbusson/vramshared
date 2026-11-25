// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/gpu_plan.rs
//! Captured same-adapter budget evidence for a read-only setup plan.

use std::collections::BTreeMap;

use ramshared_vram::{
    GPU_BUDGET_MAX_AGE_MS, GpuAdapterIdentity, GpuBudgetSnapshot, GpuBudgetSource,
    GpuBudgetTelemetry, RUNTIME_FREE_BUFFER_BYTES, ReserveFloorPolicy,
};
use serde::{Deserialize, Serialize};

use super::{CustomerError, CustomerReason, CustomerState};
use crate::readiness::capacity::CapacityObservation;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GpuPreparationPlan {
    pub(crate) budget: GpuBudgetTelemetry,
    pub(crate) observed_at_unix_ms: u64,
    pub(crate) requested_ceiling_bytes: Option<u64>,
    pub(crate) target_bytes: u64,
}

pub(super) fn refusal(reason: CustomerReason, detail: impl Into<String>) -> CustomerError {
    CustomerError {
        reason,
        detail: detail.into(),
        state: if reason == CustomerReason::ObservationUnavailable {
            CustomerState::UnknownBlocked
        } else {
            CustomerState::InstalledOff
        },
    }
}

fn stable_driver_identity(budget: &GpuBudgetTelemetry) -> bool {
    budget
        .adapter
        .as_ref()
        .is_some_and(|adapter| adapter.is_stable_driver_identity())
}

fn selected_ceiling(
    budget: &GpuBudgetTelemetry,
    caps: &BTreeMap<String, u64>,
) -> Result<Option<u64>, CustomerError> {
    if caps.len() > 1 {
        return Err(refusal(
            CustomerReason::UnsupportedSurface,
            "one published adapter budget cannot admit multiple adapter selections",
        ));
    }
    match caps.iter().next() {
        None => Ok(None),
        Some((key, value)) => {
            let matched = budget
                .adapter
                .as_ref()
                .and_then(crate::resource_config::driver_gpu_profile_key)
                .ok_or_else(|| {
                    refusal(
                        CustomerReason::ObservationUnavailable,
                        "no proven correspondence from this driver identity to a saved profile key",
                    )
                })?;
            if !matched.eq_ignore_ascii_case(key) {
                return Err(refusal(
                    CustomerReason::IdentityChanged,
                    "desired GPU ceiling does not match the admitted driver's proven profile identity",
                ));
            }
            Ok(Some(*value))
        }
    }
}

impl GpuPreparationPlan {
    pub(crate) fn capture(
        capacity: &CapacityObservation,
        caps: &BTreeMap<String, u64>,
    ) -> Result<Self, CustomerError> {
        caps.values()
            .try_fold(0u64, |sum, value| sum.checked_add(*value))
            .ok_or_else(|| {
                refusal(
                    CustomerReason::CapacityRefused,
                    "desired VRAM ceilings overflow byte capacity",
                )
            })?;
        let proof = capacity.admitted_driver_budget().ok_or_else(|| {
            refusal(
                CustomerReason::ObservationUnavailable,
                "setup needs an intact fresh driver budget with adapter identity and geometry",
            )
        })?;
        let budget = proof.budget();
        let provider = capacity
            .provider
            .filter(|provider| provider.claims_physical_vram());
        if !stable_driver_identity(budget)
            || budget
                .adapter
                .as_ref()
                .map(|adapter| adapter.backend.as_str())
                != provider.as_ref().map(|provider| provider.as_str())
        {
            return Err(refusal(
                CustomerReason::ObservationUnavailable,
                "setup budget lacks a stable identity for the selected provider",
            ));
        }
        let requested = selected_ceiling(budget, caps)?;
        let mut plan = Self {
            budget: budget.clone(),
            observed_at_unix_ms: proof.observed_at_unix_ms(),
            requested_ceiling_bytes: requested,
            target_bytes: 0,
        };
        plan.target_bytes = plan.safe_target();
        plan.validate()?;
        Ok(plan)
    }

    pub(crate) fn capacity_bytes(&self) -> u64 {
        ReserveFloorPolicy::helper_capacity(self.budget.total_bytes, self.budget.budget_bytes)
    }

    fn safe_target(&self) -> u64 {
        let snapshot = GpuBudgetSnapshot {
            adapter: self.budget.adapter.clone(),
            total_bytes: self.budget.total_bytes,
            budget_bytes: self.budget.budget_bytes,
            used_bytes: self.budget.used_bytes,
            source: self.budget.source,
            // safe_target_bytes is pure geometry; freshness uses the captured
            // wall-clock stamp separately, not this unused monotonic field.
            sampled_at: std::time::Instant::now(),
        };
        let reserve = ReserveFloorPolicy::sealed().configured_reserve_bytes(self.capacity_bytes());
        snapshot.safe_target_bytes(
            self.requested_ceiling_bytes.unwrap_or(u64::MAX),
            reserve,
            RUNTIME_FREE_BUFFER_BYTES,
        )
    }

    /// This validates captured evidence, not freshness at a later apply time.
    pub(crate) fn validate(&self) -> Result<(), CustomerError> {
        if self
            .budget
            .trusted_available_at(self.observed_at_unix_ms, GPU_BUDGET_MAX_AGE_MS)
            .is_none()
            || !stable_driver_identity(&self.budget)
        {
            return Err(refusal(
                CustomerReason::ObservationUnavailable,
                "setup GPU proof is not a fresh, stable driver sample at its observation time",
            ));
        }
        if self.target_bytes == 0 || self.target_bytes != self.safe_target() {
            return Err(refusal(
                CustomerReason::CapacityRefused,
                "setup cache target is zero or differs from the shared safe budget policy",
            ));
        }
        Ok(())
    }

    /// Every nested wire field participates in the outer versioned identity.
    pub(crate) fn identity_values(&self) -> [String; 15] {
        // Exhaustive patterns make future wire-field additions a compile error
        // until their identity projection is updated too.
        let Self {
            budget,
            observed_at_unix_ms,
            requested_ceiling_bytes,
            target_bytes,
        } = self;
        let GpuBudgetTelemetry {
            schema_version,
            adapter,
            total_bytes,
            budget_bytes,
            used_bytes,
            available_bytes,
            source,
            sampled_at_unix_ms,
        } = budget;
        let [present, backend, key, luid, node_mask] = match adapter {
            Some(GpuAdapterIdentity {
                backend,
                key,
                luid,
                node_mask,
            }) => [
                "true".into(),
                backend.clone(),
                key.clone(),
                luid.as_ref()
                    .map_or_else(|| "none".into(), |value| format!("some:{value}")),
                option_bytes(node_mask.map(u64::from)),
            ],
            None => [
                "false".into(),
                String::new(),
                String::new(),
                "none".into(),
                "none".into(),
            ],
        };
        [
            schema_version.to_string(),
            present,
            backend,
            key,
            luid,
            node_mask,
            option_bytes(*total_bytes),
            budget_bytes.to_string(),
            used_bytes.to_string(),
            available_bytes.to_string(),
            match source {
                GpuBudgetSource::DriverReported => "driver_reported",
                GpuBudgetSource::ProviderLocalEstimate => "provider_local_estimate",
            }
            .into(),
            sampled_at_unix_ms.to_string(),
            observed_at_unix_ms.to_string(),
            option_bytes(*requested_ceiling_bytes),
            target_bytes.to_string(),
        ]
    }
}

fn option_bytes(value: Option<u64>) -> String {
    value.map_or_else(|| "none".into(), |v| format!("some:{v}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub(super) mod tests {
    use super::*;
    use crate::readiness::capacity::effective_capacity;
    use crate::readiness::probes::GpuProviderKind;
    use ramshared_vram::GpuAdapterIdentity;

    const GIB: u64 = 1024 * 1024 * 1024;
    pub(crate) fn fixture_budget(total: u64, used: u64) -> GpuBudgetTelemetry {
        GpuBudgetTelemetry {
            schema_version: 1,
            adapter: Some(GpuAdapterIdentity {
                node_mask: None,
                backend: "cuda".into(),
                key: "00112233445566778899aabbccddeeff".into(),
                luid: None,
            }),
            total_bytes: Some(total),
            budget_bytes: total,
            used_bytes: used,
            available_bytes: total - used,
            source: GpuBudgetSource::DriverReported,
            sampled_at_unix_ms: 1000,
        }
    }

    pub(crate) fn fixture_plan() -> GpuPreparationPlan {
        let budget = fixture_budget(6 * GIB, 0);
        GpuPreparationPlan::capture(
            &effective_capacity(Some(&budget), 1001, Some(GpuProviderKind::Cuda)),
            &BTreeMap::new(),
        )
        .unwrap()
    }

    #[test]
    fn setup_gpu_plan_uses_capacity_and_live_headroom_separately() {
        let budget = fixture_budget(24 * GIB, 17 * GIB);
        let capacity = effective_capacity(Some(&budget), 1001, Some(GpuProviderKind::Cuda));
        let plan = GpuPreparationPlan::capture(&capacity, &BTreeMap::new()).unwrap();
        assert_eq!(plan.capacity_bytes(), 24 * GIB);
        let reserve = ReserveFloorPolicy::sealed()
            .configured_reserve_bytes(24 * GIB)
            .max((24 * GIB).div_ceil(5));
        assert_eq!(
            plan.target_bytes,
            7 * GIB - reserve - RUNTIME_FREE_BUFFER_BYTES
        );
        assert!(plan.target_bytes > 0);
        assert!(
            plan.target_bytes
                < plan.capacity_bytes()
                    - ReserveFloorPolicy::sealed().configured_reserve_bytes(plan.capacity_bytes())
        );
        assert_eq!(plan.budget, budget);
        let mut limited = fixture_budget(24 * GIB, 2 * GIB);
        limited.budget_bytes = 8 * GIB;
        limited.available_bytes = 6 * GIB;
        let observation = effective_capacity(Some(&limited), 1002, Some(GpuProviderKind::Cuda));
        let admitted = GpuPreparationPlan::capture(&observation, &BTreeMap::new()).unwrap();
        assert_eq!(admitted.capacity_bytes(), 8 * GIB);
        assert_eq!(
            admitted.target_bytes,
            6 * GIB
                - ReserveFloorPolicy::sealed()
                    .configured_reserve_bytes(8 * GIB)
                    .max((8 * GIB).div_ceil(5))
                - RUNTIME_FREE_BUFFER_BYTES
        );
    }

    #[test]
    fn setup_gpu_plan_refuses_memory_below_physical_free_floor() {
        for available in [0, RUNTIME_FREE_BUFFER_BYTES, 3 * GIB] {
            let budget = fixture_budget(24 * GIB, 24 * GIB - available);
            let capacity = effective_capacity(Some(&budget), 1001, Some(GpuProviderKind::Cuda));
            let error = GpuPreparationPlan::capture(&capacity, &BTreeMap::new()).unwrap_err();
            assert_eq!(error.reason, CustomerReason::CapacityRefused);
        }
    }

    #[test]
    fn workstation_capacity_never_overrides_driver_or_selected_adapter_budget() {
        // Synthetic 96 GiB capacity is a regression reference, not qualification
        // of a physical model. A smaller driver grant is the allocation boundary.
        for budget_gib in [4, 6, 12, 32, 48, 96] {
            let mut budget = fixture_budget(96 * GIB, GIB);
            budget.budget_bytes = budget_gib * GIB;
            budget.available_bytes = budget.budget_bytes - budget.used_bytes;
            let observation = effective_capacity(Some(&budget), 1001, Some(GpuProviderKind::Cuda));
            let plan = GpuPreparationPlan::capture(&observation, &BTreeMap::new()).unwrap();
            assert_eq!(plan.capacity_bytes(), budget.budget_bytes);
            assert!(plan.target_bytes < budget.available_bytes);
            assert!(plan.target_bytes < budget.budget_bytes);
            assert_eq!(plan.budget, budget);

            let foreign = BTreeMap::from([("nvidia-uuid:gpu-another-adapter".into(), GIB)]);
            assert_eq!(
                GpuPreparationPlan::capture(&observation, &foreign)
                    .unwrap_err()
                    .reason,
                CustomerReason::IdentityChanged
            );
            let two = BTreeMap::from([
                ("nvidia-uuid:gpu-first".into(), GIB),
                ("nvidia-uuid:gpu-second".into(), GIB),
            ]);
            assert_eq!(
                GpuPreparationPlan::capture(&observation, &two)
                    .unwrap_err()
                    .reason,
                CustomerReason::UnsupportedSurface
            );
        }
        let mut unavailable = fixture_budget(96 * GIB, 4 * GIB);
        unavailable.budget_bytes = 4 * GIB;
        unavailable.available_bytes = 0;
        let observation = effective_capacity(Some(&unavailable), 1001, Some(GpuProviderKind::Cuda));
        assert_eq!(
            GpuPreparationPlan::capture(&observation, &BTreeMap::new())
                .unwrap_err()
                .reason,
            CustomerReason::CapacityRefused
        );
    }

    #[test]
    fn setup_gpu_plan_matches_only_proven_adapter_caps() {
        let budget = fixture_budget(6 * GIB, 0);
        let capacity = effective_capacity(Some(&budget), 1001, Some(GpuProviderKind::Cuda));
        let key = crate::resource_config::driver_gpu_profile_key(budget.adapter.as_ref().unwrap())
            .unwrap();
        let mut caps = BTreeMap::from([(key.clone(), GIB)]);
        assert_eq!(
            GpuPreparationPlan::capture(&capacity, &caps)
                .unwrap()
                .target_bytes,
            GIB
        );
        caps.insert(key.clone(), 128 * GIB);
        let clamped = GpuPreparationPlan::capture(&capacity, &caps).unwrap();
        assert_eq!(clamped.requested_ceiling_bytes, Some(128 * GIB));
        assert_eq!(
            clamped.target_bytes,
            6 * GIB - 2 * GIB - RUNTIME_FREE_BUFFER_BYTES
        );
        let uppercase = BTreeMap::from([(key.to_ascii_uppercase(), GIB)]);
        assert_eq!(
            GpuPreparationPlan::capture(&capacity, &uppercase)
                .unwrap()
                .target_bytes,
            GIB
        );
        let mut vulkan = budget.clone();
        vulkan.adapter.as_mut().unwrap().backend = "vulkan".into();
        let native = effective_capacity(Some(&vulkan), 1001, Some(GpuProviderKind::Vulkan));
        GpuPreparationPlan::capture(&native, &BTreeMap::new())
            .expect("Vulkan automatic target uses its own admitted driver UUID");
        assert_eq!(
            GpuPreparationPlan::capture(&native, &uppercase)
                .unwrap_err()
                .reason,
            CustomerReason::ObservationUnavailable
        );

        caps.clear();
        caps.insert("nvidia-uuid:gpu-foreign".into(), GIB);
        assert_eq!(
            GpuPreparationPlan::capture(&capacity, &caps)
                .unwrap_err()
                .reason,
            CustomerReason::IdentityChanged
        );
        caps.insert(key.clone(), GIB);
        assert_eq!(
            GpuPreparationPlan::capture(&capacity, &caps)
                .unwrap_err()
                .reason,
            CustomerReason::UnsupportedSurface
        );
        caps.insert(key, u64::MAX);
        assert_eq!(
            GpuPreparationPlan::capture(&capacity, &caps)
                .unwrap_err()
                .reason,
            CustomerReason::CapacityRefused
        );
    }

    #[test]
    fn setup_gpu_plan_refuses_untrusted_or_zero_budget() {
        let budget = fixture_budget(6 * GIB, 0);
        for provider in [
            None,
            Some(GpuProviderKind::SoftwareAdapter),
            Some(GpuProviderKind::Vulkan),
        ] {
            let sample = effective_capacity(Some(&budget), 1001, provider);
            assert!(GpuPreparationPlan::capture(&sample, &BTreeMap::new()).is_err());
        }
        let stale = effective_capacity(
            Some(&budget),
            1000 + GPU_BUDGET_MAX_AGE_MS + 1,
            Some(GpuProviderKind::Cuda),
        );
        assert!(GpuPreparationPlan::capture(&stale, &BTreeMap::new()).is_err());
        let mut empty = fixture_budget(0, 0);
        assert_eq!(
            GpuPreparationPlan::capture(
                &effective_capacity(Some(&empty), 1001, Some(GpuProviderKind::Cuda)),
                &BTreeMap::new()
            )
            .unwrap_err()
            .reason,
            CustomerReason::CapacityRefused
        );
        empty = budget.clone();
        empty.adapter.as_mut().unwrap().luid = Some("bad-luid".into());
        assert!(
            GpuPreparationPlan::capture(
                &effective_capacity(Some(&empty), 1001, Some(GpuProviderKind::Cuda)),
                &BTreeMap::new()
            )
            .is_err()
        );
        empty = budget;
        empty.adapter.as_mut().unwrap().key = "0".into();
        assert!(
            GpuPreparationPlan::capture(
                &effective_capacity(Some(&empty), 1001, Some(GpuProviderKind::Cuda)),
                &BTreeMap::new()
            )
            .is_err()
        );
    }

    #[test]
    fn setup_gpu_plan_validates_captured_target() {
        let mut plan = fixture_plan();
        plan.validate().unwrap();
        plan.target_bytes += 1;
        assert_eq!(
            plan.validate().unwrap_err().reason,
            CustomerReason::CapacityRefused
        );
        plan = fixture_plan();
        plan.observed_at_unix_ms += GPU_BUDGET_MAX_AGE_MS + 1;
        assert_eq!(
            plan.validate().unwrap_err().reason,
            CustomerReason::ObservationUnavailable
        );
        plan = fixture_plan();
        plan.budget.source = GpuBudgetSource::ProviderLocalEstimate;
        assert!(plan.validate().is_err());
        let mut invalid = fixture_plan();
        invalid.budget.adapter = None;
        assert_ne!(fixture_plan().identity_values(), invalid.identity_values());
        let mut alternate = fixture_plan();
        alternate.budget.total_bytes = None;
        alternate.requested_ceiling_bytes = Some(GIB);
        assert_ne!(
            fixture_plan().identity_values(),
            alternate.identity_values()
        );
        let adapter = alternate.budget.adapter.as_mut().unwrap();
        adapter.luid = Some("12345678:00000001".into());
        adapter.key = "luid:12345678:00000001".into();
        alternate.target_bytes = alternate.safe_target();
        alternate.validate().unwrap();
        assert!(
            crate::resource_config::driver_gpu_profile_key(
                alternate.budget.adapter.as_ref().unwrap()
            )
            .is_none()
        );
        let source = alternate.identity_values();
        alternate.budget.adapter.as_mut().unwrap().luid = None;
        assert_ne!(source, alternate.identity_values());
        assert!(alternate.validate().is_err());
    }
}
