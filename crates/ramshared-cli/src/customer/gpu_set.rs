// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/gpu_set.rs
//! Canonical, independently budgeted adapters in a reviewed cache plan.
use super::gpu_plan::{GpuPreparationPlan, refusal};
use super::{CustomerError, CustomerReason};
use crate::readiness::capacity::CapacityObservation;
use ramshared_vram::{RUNTIME_FREE_BUFFER_BYTES, ReserveFloorPolicy};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CacheAdapterPlan {
    pub(crate) gpu: GpuPreparationPlan,
    pub(crate) configured_reserve_bytes: u64,
    pub(crate) runtime_free_buffer_bytes: u64,
}
impl CacheAdapterPlan {
    pub(crate) fn new(gpu: GpuPreparationPlan) -> Self {
        Self {
            configured_reserve_bytes: ReserveFloorPolicy::sealed()
                .configured_reserve_bytes(gpu.capacity_bytes()),
            runtime_free_buffer_bytes: RUNTIME_FREE_BUFFER_BYTES,
            gpu,
        }
    }
    pub(crate) fn identity_values(&self) -> Vec<String> {
        let Self {
            gpu,
            configured_reserve_bytes,
            runtime_free_buffer_bytes,
        } = self;
        gpu.identity_values()
            .into_iter()
            .chain([
                configured_reserve_bytes.to_string(),
                runtime_free_buffer_bytes.to_string(),
            ])
            .collect()
    }
}

fn key(plan: &CacheAdapterPlan) -> String {
    plan.gpu
        .budget
        .adapter
        .as_ref()
        .map_or_else(String::new, |a| {
            format!("{}:{}", a.backend, a.key.to_ascii_lowercase())
        })
}

pub(crate) fn capture(
    capacities: &[CapacityObservation],
    caps: &BTreeMap<String, u64>,
) -> Result<Vec<CacheAdapterPlan>, CustomerError> {
    validate_caps(caps)?;
    let mut plans = if caps.is_empty() {
        vec![automatic(capacities)?]
    } else {
        caps.iter()
            .filter(|(_, cap)| **cap > 0)
            .map(|(name, cap)| selected(capacities, name, *cap))
            .collect::<Result<Vec<_>, _>>()?
    };
    plans.sort_by_key(key);
    validate(&plans)?;
    Ok(plans)
}

fn validate_caps(caps: &BTreeMap<String, u64>) -> Result<(), CustomerError> {
    let mut keys = BTreeSet::new();
    let mut total = 0u64;
    for (name, cap) in caps {
        if !keys.insert(name.to_ascii_lowercase()) {
            return Err(refusal(
                CustomerReason::IdentityChanged,
                "duplicate GPU profile aliases",
            ));
        }
        total = total.checked_add(*cap).ok_or_else(|| {
            refusal(
                CustomerReason::CapacityRefused,
                "desired GPU ceilings overflow",
            )
        })?;
    }
    Ok(())
}

fn automatic(capacities: &[CapacityObservation]) -> Result<CacheAdapterPlan, CustomerError> {
    let mut error = None;
    let plans = capacities.iter().filter_map(|capacity| {
        match GpuPreparationPlan::capture(capacity, &BTreeMap::new()) {
            Ok(plan) => Some(CacheAdapterPlan::new(plan)),
            Err(reason) => {
                error.get_or_insert(reason);
                None
            }
        }
    });
    let best = plans.max_by(|a, b| {
        a.gpu
            .target_bytes
            .cmp(&b.gpu.target_bytes)
            .then_with(|| key(b).cmp(&key(a)))
    });
    best.ok_or_else(|| {
        error.unwrap_or_else(|| {
            refusal(
                CustomerReason::ObservationUnavailable,
                "no observed GPU budget is available",
            )
        })
    })
}

fn selected(
    capacities: &[CapacityObservation],
    name: &str,
    cap: u64,
) -> Result<CacheAdapterPlan, CustomerError> {
    let matches: Vec<_> = capacities
        .iter()
        .filter(|observation| {
            observation
                .admitted_driver_budget()
                .and_then(|proof| proof.budget().adapter.as_ref())
                .and_then(|adapter| adapter.resource_profile_key())
                .is_some_and(|key| key.eq_ignore_ascii_case(name))
        })
        .collect();
    let observation = match matches.as_slice() {
        [one] => *one,
        [] if capacities.len() == 1 => &capacities[0],
        _ => {
            return Err(refusal(
                CustomerReason::ObservationUnavailable,
                format!("selected GPU {name} needs exactly one fresh corresponding driver budget"),
            ));
        }
    };
    Ok(CacheAdapterPlan::new(GpuPreparationPlan::capture(
        observation,
        &BTreeMap::from([(name.into(), cap)]),
    )?))
}

pub(crate) fn validate(plans: &[CacheAdapterPlan]) -> Result<u64, CustomerError> {
    if plans.is_empty() {
        return Err(refusal(
            CustomerReason::CapacityRefused,
            "cache setup selects no GPU",
        ));
    }
    let mut total = 0u64;
    for (index, plan) in plans.iter().enumerate() {
        plan.gpu.validate()?;
        if plan.configured_reserve_bytes
            != ReserveFloorPolicy::sealed().configured_reserve_bytes(plan.gpu.capacity_bytes())
            || plan.runtime_free_buffer_bytes != RUNTIME_FREE_BUFFER_BYTES
        {
            return Err(refusal(
                CustomerReason::CapacityRefused,
                "per-adapter reserve differs from the shared policy",
            ));
        }
        let adapter = plan.gpu.budget.adapter.as_ref().ok_or_else(|| {
            refusal(
                CustomerReason::ObservationUnavailable,
                "GPU budget has no identity",
            )
        })?;
        if plans[..index].iter().any(|other| {
            other
                .gpu
                .budget
                .adapter
                .as_ref()
                .is_none_or(|other| !adapter.proves_independent_memory(other))
        }) {
            return Err(refusal(
                CustomerReason::IdentityChanged,
                "GPU budgets do not prove independent physical memory",
            ));
        }
        if index > 0 && key(&plans[index - 1]) >= key(plan) {
            return Err(refusal(
                CustomerReason::IdentityChanged,
                "GPU plan membership is not canonical",
            ));
        }
        total = total.checked_add(plan.gpu.target_bytes).ok_or_else(|| {
            refusal(
                CustomerReason::CapacityRefused,
                "GPU targets overflow byte capacity",
            )
        })?;
    }
    Ok(total)
}

#[cfg(test)]
mod tests;
