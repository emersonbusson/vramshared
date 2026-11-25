// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/readiness/published_members.rs
//! Per-adapter provenance from admitted current-cache or Off-only observations.
use super::capacity::{CapacityObservation, effective_capacity};
use super::probes::GpuProviderKind;
use ramshared_block::{CacheState, cache_group::CacheMemberStatus};

#[derive(Debug)]
pub(crate) struct PublishedMemberCapacity {
    pub(crate) key: String,
    pub(crate) adapter: Option<ramshared_vram::GpuAdapterIdentity>,
    pub(crate) capacity: CapacityObservation,
}

#[derive(Debug, Default)]
pub(crate) enum PublishedGpuCapacities {
    #[default]
    Legacy,
    Observed(Vec<PublishedMemberCapacity>),
    OffObserved(Vec<PublishedMemberCapacity>),
    Unavailable(&'static str),
}

pub(crate) fn observe(document: Option<&serde_json::Value>, now: u64) -> PublishedGpuCapacities {
    let Some(field) = document.and_then(|document| document.get("cache_members")) else {
        return PublishedGpuCapacities::Legacy;
    };
    match parse(field, now) {
        Ok(observations) => PublishedGpuCapacities::Observed(observations),
        Err(reason) => PublishedGpuCapacities::Unavailable(reason),
    }
}

fn parse(
    field: &serde_json::Value,
    now: u64,
) -> Result<Vec<PublishedMemberCapacity>, &'static str> {
    let members: Vec<CacheMemberStatus> =
        serde_json::from_value(field.clone()).map_err(|_| "cache member records are malformed")?;
    let mut keys = std::collections::BTreeSet::new();
    let mut identities = Vec::<ramshared_vram::GpuAdapterIdentity>::new();
    let mut observations = Vec::with_capacity(members.len());
    for member in members {
        if member.key.is_empty()
            || !keys.insert(member.key.clone())
            || member.requested_target_bytes == 0
        {
            return Err("cache member keys or geometry are invalid");
        }
        let Some(budget) = member.budget else {
            observations.push(PublishedMemberCapacity {
                key: member.key,
                adapter: member.adapter,
                capacity: CapacityObservation::absent(None),
            });
            continue;
        };
        let adapter = budget
            .adapter
            .as_ref()
            .ok_or("member budget lacks identity")?;
        if Some(adapter) != member.adapter.as_ref() || !adapter.is_stable_driver_identity() {
            return Err("member budget does not match its observed adapter");
        }
        if identities
            .iter()
            .any(|other| !adapter.proves_independent_memory(other))
        {
            return Err("member budgets do not prove independent physical memory");
        }
        identities.push(adapter.clone());
        let provider = match adapter.backend.as_str() {
            "cuda" => GpuProviderKind::Cuda,
            "vulkan" => GpuProviderKind::Vulkan,
            _ => return Err("member provider is unsupported"),
        };
        observations.push(PublishedMemberCapacity {
            key: member.key,
            adapter: member.adapter,
            capacity: if matches!(member.state, CacheState::Active | CacheState::Restricted) {
                effective_capacity(Some(&budget), now, Some(provider))
            } else {
                CapacityObservation::absent(Some(provider))
            },
        });
    }
    Ok(observations)
}

pub(crate) fn from_observations(
    budgets: &[ramshared_vram::GpuBudgetTelemetry],
    now: u64,
) -> PublishedGpuCapacities {
    let mut observations = Vec::new();
    for budget in budgets {
        let Some(adapter) = budget.adapter.as_ref() else {
            return PublishedGpuCapacities::Unavailable("Off observation lacks identity");
        };
        let provider = match adapter.backend.as_str() {
            "cuda" => GpuProviderKind::Cuda,
            "vulkan" => GpuProviderKind::Vulkan,
            _ => {
                return PublishedGpuCapacities::Unavailable("Off observation provider unsupported");
            }
        };
        observations.push(PublishedMemberCapacity {
            key: format!("{}:{}", adapter.backend, adapter.key),
            adapter: Some(adapter.clone()),
            capacity: effective_capacity(Some(budget), now, Some(provider)),
        });
    }
    PublishedGpuCapacities::OffObserved(observations)
}

#[cfg(test)]
mod tests;
