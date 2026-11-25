// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/cache_configuration/budget.rs
//! Re-observe exact reviewed adapters and lower sink; never expand an approval.
use super::{CustomerError, changed, confinement_refusal};
use crate::customer::setup_scope::CachePreparationPlan;
use crate::readiness::{CheckReport, Decision, published_members::PublishedGpuCapacities};
use ramshared_config::cache_configuration::CacheMemberConfiguration;
use ramshared_tier::nbd_readiness::{CapacitySample, LowerTierSink, validate_lower_tier_capacity};

pub(super) fn revalidate(
    cache: &CachePreparationPlan,
    report: &CheckReport,
    lower: &CapacitySample,
    now: u64,
) -> Result<(), CustomerError> {
    use crate::readiness::probes::{Observation, PlatformKind};
    if report.decision() != Decision::Ready
        || report.platform.platform != Observation::Measured(PlatformKind::NativeLinux)
    {
        return Err(confinement_refusal(
            "cache configuration needs fresh native Linux readiness",
        ));
    }
    let capacities = match &report.gpu_capacities {
        PublishedGpuCapacities::Legacy => vec![&report.capacity],
        PublishedGpuCapacities::Observed(rows) | PublishedGpuCapacities::OffObserved(rows) => {
            rows.iter().map(|row| &row.capacity).collect()
        }
        PublishedGpuCapacities::Unavailable(reason) => return Err(changed(*reason)),
    };
    for member in &cache.gpus {
        let wanted = member
            .gpu
            .budget
            .adapter
            .as_ref()
            .ok_or_else(|| changed("reviewed adapter is absent"))?;
        let matches = capacities
            .iter()
            .filter(|capacity| {
                capacity.admitted_driver_budget().is_some_and(|proof| {
                    proof.budget().adapter.as_ref() == Some(wanted)
                        && proof
                            .budget()
                            .trusted_available_at(now, ramshared_vram::GPU_BUDGET_MAX_AGE_MS)
                            .is_some()
                })
            })
            .collect::<Vec<_>>();
        let [capacity] = matches.as_slice() else {
            return Err(changed(
                "reviewed GPU needs exactly one fresh matching driver budget",
            ));
        };
        let current =
            crate::customer::gpu_plan::GpuPreparationPlan::capture(capacity, &Default::default())?;
        if current.capacity_bytes() != member.gpu.capacity_bytes()
            || current.target_bytes < member.gpu.target_bytes
        {
            return Err(changed(
                "current GPU capacity no longer covers the reviewed target",
            ));
        }
    }
    let target = crate::customer::gpu_set::validate(&cache.gpus)?;
    validate_lower_tier_capacity(target, lower).map_err(|e| {
        changed(format!(
            "current lower sink cannot cover the reviewed cache: {e:?}"
        ))
    })?;
    if !matches!(lower,CapacitySample::Observed{sink:LowerTierSink::Known(id),..} if *id==cache.lower_sink_identity)
    {
        return Err(changed("lower sink changed since cache review"));
    }
    Ok(())
}

pub(super) fn members(
    cache: &CachePreparationPlan,
) -> Result<Vec<CacheMemberConfiguration>, CustomerError> {
    cache
        .gpus
        .iter()
        .map(|member| {
            let mut adapter = member
                .gpu
                .budget
                .adapter
                .clone()
                .ok_or_else(|| changed("reviewed adapter is absent"))?;
            adapter.key.make_ascii_lowercase();
            if let Some(luid) = &mut adapter.luid {
                luid.make_ascii_lowercase();
            }
            Ok(CacheMemberConfiguration {
                adapter,
                capacity_bytes: member.gpu.capacity_bytes(),
                target_bytes: member.gpu.target_bytes,
                reserve_bytes: member.configured_reserve_bytes,
                runtime_buffer_bytes: member.runtime_free_buffer_bytes,
            })
        })
        .collect()
}
