// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/readiness/capacity.rs
//! Physical-VRAM capacity authority.
//!
//! (DT-3, DT-7). One place turns a published GPU budget sample into the figure
//! readiness is allowed to report: **admission headroom**, not adapter
//! inventory. Source, age and headroom are always reported together; a refused
//! sample is `effective_bytes: None`, never a zero that reads as measured empty
//! capacity. A software adapter is never physical VRAM.

use ramshared_vram::{GPU_BUDGET_MAX_AGE_MS, GpuBudgetSource, GpuBudgetTelemetry};

use super::probes::GpuProviderKind;

/// Where a capacity figure came from, or why there is no figure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CapacitySource {
    /// Fresh, adapter-bound, driver-reported budget (the only kind that may
    /// back a physical-VRAM claim).
    DriverReported,
    /// The provider only counts its own allocations. Reported as context,
    /// never as device capacity.
    ProviderLocalEstimate,
    /// Sample outside the freshness window.
    Stale,
    /// Sample unbound, malformed or inconsistent.
    Unbound,
    /// No sample was published at all.
    Absent,
}

impl CapacitySource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            CapacitySource::DriverReported => "driver-reported",
            CapacitySource::ProviderLocalEstimate => "provider-local-estimate",
            CapacitySource::Stale => "stale",
            CapacitySource::Unbound => "unbound",
            CapacitySource::Absent => "absent",
        }
    }
}

/// The admission headroom readiness is allowed to claim, with its provenance.
///
/// This is **not** adapter inventory. `effective_bytes` is the published
/// sample's trusted `available_bytes` — what admission may still spend — and is
/// `None` exactly when the sample cannot support a physical-VRAM claim. It is
/// never `Some(0)` standing in for "unknown".
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CapacityObservation {
    pub(crate) source: CapacitySource,
    pub(crate) age_ms: Option<u64>,
    pub(crate) effective_bytes: Option<u64>,
    pub(crate) provider: Option<GpuProviderKind>,
    pub(crate) driver_budget: Option<DriverBudgetObservation>,
}

/// The exact accepted sample and the time at which readiness reduced it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DriverBudgetObservation {
    budget: GpuBudgetTelemetry,
    observed_at_unix_ms: u64,
}

impl DriverBudgetObservation {
    pub(crate) fn budget(&self) -> &GpuBudgetTelemetry {
        &self.budget
    }
    pub(crate) fn observed_at_unix_ms(&self) -> u64 {
        self.observed_at_unix_ms
    }
}

impl CapacityObservation {
    /// Summary fields cannot widen or contradict the captured proof.
    pub(crate) fn admitted_driver_budget(&self) -> Option<&DriverBudgetObservation> {
        let proof = self.driver_budget.as_ref()?;
        let available = proof
            .budget
            .trusted_available_at(proof.observed_at_unix_ms, GPU_BUDGET_MAX_AGE_MS)?;
        (self.source == CapacitySource::DriverReported
            && self.provider != Some(GpuProviderKind::SoftwareAdapter)
            && self.age_ms
                == proof
                    .observed_at_unix_ms
                    .checked_sub(proof.budget.sampled_at_unix_ms)
            && self.effective_bytes == Some(available))
        .then_some(proof)
    }

    /// No sample at all. Unknown, not zero.
    pub(crate) fn absent(provider: Option<GpuProviderKind>) -> Self {
        Self {
            source: CapacitySource::Absent,
            age_ms: None,
            effective_bytes: None,
            provider,
            driver_budget: None,
        }
    }
}

/// Reduce a published budget sample to the figure readiness may report.
///
/// Delegates the validity predicate to
/// [`GpuBudgetTelemetry::trusted_available_at`] and to
/// [`GPU_BUDGET_MAX_AGE_MS`] — this is the same authority the daemon
/// publisher and the cascade already hold, not a second policy.
///
/// A software adapter never produces a physical-VRAM figure regardless of how
/// fresh or well-formed its sample is (DT-7).
pub(crate) fn effective_capacity(
    budget: Option<&GpuBudgetTelemetry>,
    now_unix_ms: u64,
    provider: Option<GpuProviderKind>,
) -> CapacityObservation {
    let Some(budget) = budget else {
        return CapacityObservation::absent(provider);
    };

    let age_ms = now_unix_ms.checked_sub(budget.sampled_at_unix_ms);

    if matches!(provider, Some(GpuProviderKind::SoftwareAdapter)) {
        return CapacityObservation {
            source: CapacitySource::ProviderLocalEstimate,
            age_ms,
            effective_bytes: None,
            provider,
            driver_budget: None,
        };
    }

    if budget.source == GpuBudgetSource::ProviderLocalEstimate {
        return CapacityObservation {
            source: CapacitySource::ProviderLocalEstimate,
            age_ms,
            effective_bytes: None,
            provider,
            driver_budget: None,
        };
    }

    match budget.trusted_available_at(now_unix_ms, GPU_BUDGET_MAX_AGE_MS) {
        Some(available) => CapacityObservation {
            source: CapacitySource::DriverReported,
            age_ms,
            effective_bytes: Some(available),
            provider,
            driver_budget: Some(DriverBudgetObservation {
                budget: budget.clone(),
                observed_at_unix_ms: now_unix_ms,
            }),
        },
        None if age_ms.is_some_and(|age| age > GPU_BUDGET_MAX_AGE_MS) => CapacityObservation {
            source: CapacitySource::Stale,
            age_ms,
            effective_bytes: None,
            provider,
            driver_budget: None,
        },
        None => CapacityObservation {
            source: CapacitySource::Unbound,
            age_ms,
            effective_bytes: None,
            provider,
            driver_budget: None,
        },
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use ramshared_vram::GpuAdapterIdentity;

    fn driver_budget(sampled_at_unix_ms: u64) -> GpuBudgetTelemetry {
        GpuBudgetTelemetry {
            schema_version: 1,
            adapter: Some(GpuAdapterIdentity {
                node_mask: None,
                backend: "cuda".to_string(),
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

    /// Local unit check for the source labels the typed report prints.
    #[test]
    fn capacity_source_names_are_stable() {
        assert_eq!(CapacitySource::DriverReported.as_str(), "driver-reported");
        assert_eq!(
            CapacitySource::ProviderLocalEstimate.as_str(),
            "provider-local-estimate"
        );
        assert_eq!(CapacitySource::Stale.as_str(), "stale");
        assert_eq!(CapacitySource::Unbound.as_str(), "unbound");
        assert_eq!(CapacitySource::Absent.as_str(), "absent");
    }

    /// A refused sample never becomes a zero figure that reads as measured
    /// empty capacity (AUD-39 / DT-7).
    #[test]
    fn refused_sample_is_unknown_not_zero() {
        let now = 1_000_000u64;
        let stale = driver_budget(now - GPU_BUDGET_MAX_AGE_MS - 1);
        let refused = effective_capacity(Some(&stale), now, Some(GpuProviderKind::Cuda));
        assert_eq!(refused.effective_bytes, None);
        assert_ne!(refused.effective_bytes, Some(0));
    }
    #[test]
    fn accepted_capacity_retains_exact_driver_proof() {
        let budget = driver_budget(1000);
        let observed = effective_capacity(Some(&budget), 1001, Some(GpuProviderKind::Cuda));
        let proof = observed
            .admitted_driver_budget()
            .expect("accepted driver record");
        assert_eq!(proof.budget(), &budget);
        assert_eq!(proof.observed_at_unix_ms(), 1001);
        assert_eq!(observed.effective_bytes, Some(budget.available_bytes));
    }

    #[test]
    fn refused_capacity_never_retains_driver_proof() {
        let base = driver_budget(1000);
        let mut local = base.clone();
        local.source = GpuBudgetSource::ProviderLocalEstimate;
        let mut malformed = base.clone();
        malformed.available_bytes += 1;
        let mut unbound = base.clone();
        unbound.adapter = None;
        for (sample, now, provider) in [
            (None, 1001, Some(GpuProviderKind::Cuda)),
            (
                Some(&base),
                1000 + GPU_BUDGET_MAX_AGE_MS + 1,
                Some(GpuProviderKind::Cuda),
            ),
            (Some(&base), 999, Some(GpuProviderKind::Cuda)),
            (Some(&base), 1001, Some(GpuProviderKind::SoftwareAdapter)),
            (Some(&local), 1001, Some(GpuProviderKind::Cuda)),
            (Some(&malformed), 1001, Some(GpuProviderKind::Cuda)),
            (Some(&unbound), 1001, Some(GpuProviderKind::Cuda)),
        ] {
            let observation = effective_capacity(sample, now, provider);
            assert!(observation.driver_budget.is_none());
            assert!(observation.admitted_driver_budget().is_none());
        }
    }

    #[test]
    fn changed_capacity_summary_cannot_reuse_driver_proof() {
        let base = effective_capacity(
            Some(&driver_budget(1000)),
            1001,
            Some(GpuProviderKind::Cuda),
        );
        let mut altered = base.clone();
        altered.effective_bytes = Some(0);
        assert!(altered.admitted_driver_budget().is_none());
        altered = base.clone();
        altered.age_ms = None;
        assert!(altered.admitted_driver_budget().is_none());
        altered = base.clone();
        altered.source = CapacitySource::Unbound;
        assert!(altered.admitted_driver_budget().is_none());
        altered = base;
        altered.provider = Some(GpuProviderKind::SoftwareAdapter);
        assert!(altered.admitted_driver_budget().is_none());
    }
}
