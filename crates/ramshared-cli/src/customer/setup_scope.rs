// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/setup_scope.rs
//! Explicit storage intent and captured cache geometry for setup planning.
//!
//! Storage preparation is independent of GPU/transport readiness. It carries
//! no cache or activation authority; the effect owner still needs the exact
//! profile plus fresh storage observations. Cache geometry is only captured
//! when the operator explicitly selects that scope.

use ramshared_config::resource_profile::ResourceProfile;
use ramshared_tier::nbd_readiness::{
    CapacitySample, LowerTierSink, minimum_lower_tier_bytes, validate_lower_tier_capacity,
};
use serde::{Deserialize, Serialize};

use super::gpu_set::{self, CacheAdapterPlan};
use crate::readiness::published_members::PublishedGpuCapacities;

pub(crate) const CACHE_PLAN_SCHEMA_VERSION: u32 = 3;
use super::{CustomerError, CustomerReason, CustomerState};
use crate::readiness::{CheckReport, Decision};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum SetupScope {
    #[default]
    StorageOnly,
    GpuCache,
}

impl SetupScope {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::StorageOnly => "storage-only",
            Self::GpuCache => "gpu-cache",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, CustomerError> {
        match value {
            "storage-only" => Ok(Self::StorageOnly),
            "gpu-cache" => Ok(Self::GpuCache),
            _ => Err(refused(
                CustomerReason::UnsupportedSurface,
                "setup scope must be storage-only or gpu-cache",
            )),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum SetupOperation {
    StorageOnly {},
    GpuCache { cache: Box<CachePreparationPlan> },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CachePreparationPlan {
    pub(crate) schema_version: u32,
    pub(crate) gpus: Vec<CacheAdapterPlan>,
    /// Evacuation bound for the complete cache set, not logical-device release.
    pub(crate) required_lower_sink_bytes: u64,
    pub(crate) lower_sink_identity: String,
}

fn refused(reason: CustomerReason, detail: impl Into<String>) -> CustomerError {
    CustomerError {
        reason,
        detail: detail.into(),
        state: CustomerState::InstalledOff,
    }
}

impl CachePreparationPlan {
    fn capture(
        report: &CheckReport,
        profile: &ResourceProfile,
        lower_sink: &CapacitySample,
    ) -> Result<Self, CustomerError> {
        if report.decision() != Decision::Ready || !report.surface.claims_physical_vram() {
            return Err(refused(
                CustomerReason::UnsupportedSurface,
                "cache setup needs Ready; storage-only preparation is a separate scope",
            ));
        }
        let capacities = match &report.gpu_capacities {
            PublishedGpuCapacities::Legacy => vec![report.capacity.clone()],
            PublishedGpuCapacities::Observed(observations)
            | PublishedGpuCapacities::OffObserved(observations) => observations
                .iter()
                .map(|row| row.capacity.clone())
                .collect(),
            PublishedGpuCapacities::Unavailable(reason) => {
                return Err(refused(CustomerReason::ObservationUnavailable, *reason));
            }
        };
        let gpus = gpu_set::capture(&capacities, &profile.planned_caps.vram_bytes)?;
        let target = gpu_set::validate(&gpus)?;
        validate_lower_tier_capacity(target, lower_sink).map_err(|error| {
            refused(
                CustomerReason::CapacityRefused,
                format!("lower-tier sink does not satisfy the demotion formula: {error:?}"),
            )
        })?;
        let lower_sink_identity = match lower_sink {
            CapacitySample::Observed {
                sink: LowerTierSink::Known(identity),
                ..
            } => identity.clone(),
            _ => {
                return Err(refused(
                    CustomerReason::CapacityRefused,
                    "lower-tier sink identity is not exactly one known sink",
                ));
            }
        };
        let plan = Self {
            schema_version: CACHE_PLAN_SCHEMA_VERSION,
            gpus,
            required_lower_sink_bytes: minimum_lower_tier_bytes(target).map_err(|_| {
                refused(
                    CustomerReason::CapacityRefused,
                    "lower-sink requirement is not computable",
                )
            })?,
            lower_sink_identity,
        };
        plan.validate()?;
        Ok(plan)
    }

    fn validate(&self) -> Result<(), CustomerError> {
        if self.schema_version != CACHE_PLAN_SCHEMA_VERSION {
            return Err(refused(
                CustomerReason::UnsupportedSurface,
                "unknown cache-plan schema; re-plan required",
            ));
        }
        let target = gpu_set::validate(&self.gpus)?;
        let required = minimum_lower_tier_bytes(target).map_err(|_| {
            refused(
                CustomerReason::CapacityRefused,
                "setup target cannot be represented by the lower-sink policy",
            )
        })?;
        if self.required_lower_sink_bytes != required || self.lower_sink_identity.trim().is_empty()
        {
            return Err(refused(
                CustomerReason::CapacityRefused,
                "setup lower-sink metadata differs from its captured GPU set",
            ));
        }
        Ok(())
    }
}

impl SetupOperation {
    pub(crate) fn capture(
        scope: SetupScope,
        report: &CheckReport,
        profile: &ResourceProfile,
        lower_sink: &CapacitySample,
    ) -> Result<Self, CustomerError> {
        match scope {
            SetupScope::StorageOnly => {
                if profile.targets.is_empty() {
                    return Err(refused(
                        CustomerReason::UnsupportedSurface,
                        "storage-only setup needs at least one storage target; no tier is implicitly enabled",
                    ));
                }
                Ok(Self::StorageOnly {})
            }
            SetupScope::GpuCache => Ok(Self::GpuCache {
                cache: Box::new(CachePreparationPlan::capture(report, profile, lower_sink)?),
            }),
        }
    }

    pub(crate) fn validate(&self, decision: Decision) -> Result<(), CustomerError> {
        match self {
            Self::StorageOnly {} => Ok(()),
            Self::GpuCache { cache } => {
                if decision != Decision::Ready {
                    return Err(refused(
                        CustomerReason::UnsupportedSurface,
                        "blocked readiness cannot authorize cache setup; re-plan required",
                    ));
                }
                cache.validate()
            }
        }
    }

    pub(crate) fn identity_values(&self) -> Vec<String> {
        match self {
            Self::StorageOnly {} => vec!["storage_only".into()],
            Self::GpuCache { cache } => {
                let CachePreparationPlan {
                    schema_version,
                    gpus,
                    required_lower_sink_bytes,
                    lower_sink_identity,
                } = cache.as_ref();
                [
                    "gpu_cache".into(),
                    schema_version.to_string(),
                    gpus.len().to_string(),
                    required_lower_sink_bytes.to_string(),
                    lower_sink_identity.clone(),
                ]
                .into_iter()
                .chain(gpus.iter().flat_map(CacheAdapterPlan::identity_values))
                .collect()
            }
        }
    }
}
