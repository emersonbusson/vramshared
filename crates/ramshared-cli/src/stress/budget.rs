// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/stress/budget.rs
//! Cache admission, three-tier snapshot qualification, and KiB step-budget policy.
//!
//! Extracted from `stress.rs` to keep admission and step sizing separate from
//! host procfs parsing and the multi-phase execution loop.

use std::time::{SystemTime, UNIX_EPOCH};

use super::measure::{KIB_PER_MIB, published_mib};
use super::{
    PRE_TIER3_FULL_STEP_HEADROOM_KIB, PRE_TIER3_HEADROOM_RESERVE_KIB, PRE_TIER3_MAX_STEP_KIB,
    SINGLE_TIER_MAX_STEP_KIB, StressOptions, TIER3_ACTIVITY_MIN_GROWTH_KIB,
    TIER3_HEADROOM_RESERVE_KIB, TIER3_MAX_STEP_KIB, TierCapacityStats, WSL2_TIER3_STEP_INTERVAL_MS,
};

/// One fresh cache-status sample, measured in the unit the wire carries.
///
/// The document reports KiB, so the sample keeps KiB. Collapsing to whole MiB
/// at construction was a measured-zero lie: a 512 KiB occupancy became `0`,
/// and a sub-MiB target became `0`, which then failed the `target > 0`
/// presence check and hid a non-empty cache from the qualification entirely.
///
/// Comparisons happen here in KiB. [`CacheSample::cached_mib`] and
/// [`CacheSample::target_mib`] are the **publish** conversion, for the report
/// fields that are named in MiB; they round up so a non-zero measurement can
/// never publish as zero.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct CacheSample {
    pub(super) cached_kib: u64,
    pub(super) target_kib: u64,
    pub(super) at_target: bool,
}

impl CacheSample {
    /// The published MiB figure for a measured KiB occupancy. See [`published_mib`].
    pub(super) fn cached_mib(&self) -> u64 {
        published_mib(self.cached_kib)
    }

    /// The published MiB figure for a measured KiB target. See [`published_mib`].
    pub(super) fn target_mib(&self) -> u64 {
        published_mib(self.target_kib)
    }
}

/// A physical-cache request expressed in whole MiB, in the sample's unit.
///
/// The CLI option and the report name MiB; the wire names KiB. Convert once at
/// the boundary so every comparison inside this module is in one unit and no
/// comparison can be rounded before it is made.
pub(super) fn requested_kib(requested_mib: u64) -> u64 {
    requested_mib.saturating_mul(KIB_PER_MIB)
}

/// Reduce a cache-status document to the tier-residency sample the stress
/// harness needs, or refuse it.
///
/// Admission is the cascade's gate, not a second one: shape, identity and age
/// in one answer, with the production write-time window and no future-skew
/// allowance. A document this refuses is a leftover from an earlier instance
/// or a foreign one's, and its counters are not this instance's counters —
/// the same document-level refusal the readiness run applies before reducing
/// anything. What remains here is the harness's own narrowing on top: the
/// tier is only usable when the plane is up and healthy, not merely "in the
/// enum", and the counters have to describe a cache that can exist.
pub(super) fn effective_slab_target_kib(value: &serde_json::Value, target_kib: u64) -> u64 {
    const STAGING_KIB: u64 = 776;
    const FRAME_KIB: u64 = 64;
    let codec_ready = value
        .get("gpu_cache")
        .and_then(|gc| gc.get("codec"))
        .and_then(|c| c.get("state"))
        .and_then(|s| s.as_str())
        == Some("ready");
    let physical_slab_bytes = value
        .get("gpu_cache")
        .and_then(|gc| gc.get("physical_cache_slab_bytes"))
        .and_then(|b| b.as_u64())
        .filter(|b| *b > 0);
    if codec_ready
        && let Some(slab_bytes) = physical_slab_bytes
        && target_kib >= STAGING_KIB + 2 * FRAME_KIB
    {
        slab_bytes
            .div_ceil(1024)
            .saturating_add(STAGING_KIB)
            .min(target_kib)
    } else {
        target_kib
    }
}

pub(super) fn parse_cache_status_sample(
    value: &serde_json::Value,
    now_ms: u64,
    daemon_instance_id: &str,
) -> Option<CacheSample> {
    if !crate::cascade::cache_status_is_current_daemon(value, daemon_instance_id, now_ms) {
        return None;
    }
    if !value.get("ok")?.as_bool()?
        || value.get("origin_state")?.as_str()? != "READY"
        || value.get("cache_state")?.as_str()? != "ACTIVE"
    {
        return None;
    }
    let cached_kib = value.get("vram_cached_kib")?.as_u64()?;
    let target_kib = value.get("cache_target_kib")?.as_u64()?;
    if target_kib == 0 || cached_kib > value.get("logical_capacity_kib")?.as_u64()? {
        return None;
    }
    let admitted_target_kib = effective_slab_target_kib(value, target_kib);
    Some(CacheSample {
        cached_kib,
        target_kib,
        at_target: cached_kib >= admitted_target_kib,
    })
}

pub(super) fn read_cache_status_sample() -> Option<CacheSample> {
    let value = crate::published_budget::read_status_document(std::path::Path::new(
        crate::published_budget::CACHE_STATUS_FILE,
    ))?;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis() as u64;
    parse_cache_status_sample(
        &value,
        now_ms,
        &crate::cascade::current_daemon_instance_id()?,
    )
}

pub(super) fn require_physical_cache_before_cascade(
    cascade: bool,
    physical_cache_target_mib: Option<u64>,
    sample: Option<CacheSample>,
) -> Result<(), String> {
    if cascade && sample.is_none() {
        return Err(
            "cascade stress requires fresh ACTIVE physical GPU cache telemetry with a nonzero target"
                .into(),
        );
    }
    if let Some(requested_mib) = physical_cache_target_mib
        && sample.is_none_or(|cache| cache.target_kib < requested_kib(requested_mib))
    {
        return Err(format!(
            "sealed physical GPU cache target is below requested {requested_mib} MiB"
        ));
    }
    Ok(())
}

/// Refuse `--tier3-only` when a storage-backed Tier 3 swap device is not
/// established.
///
/// Two refusals, not one. `None` is "the swap table was not readable", so the
/// run has not measured whether a device is there; it must not report "no
/// device", which is the answer for `Some` with a zero total. Both refuse —
/// the run cannot qualify tier 3 without the device, and it cannot claim
/// there is none when it never looked.
pub(super) fn require_tier3_only_capacity(
    enabled: bool,
    tier3: Option<TierCapacityStats>,
) -> Result<(), String> {
    if !enabled {
        return Ok(());
    }
    match tier3 {
        None => Err(
            "--tier3-only could not read the swap table: Tier 3 device presence is unmeasured"
                .into(),
        ),
        Some(tier3) if tier3.total_kib == 0 => {
            Err("--tier3-only requires an active storage-backed Tier 3 swap device".into())
        }
        Some(_) => Ok(()),
    }
}

/// Whether the storage tier has reached its qualification target.
///
/// `None` is a tier with no swap device, so it has reached nothing — a target
/// of 1..=100 cannot be met by a ratio that does not exist. The refusal is
/// kept as a refusal here rather than folded to `0`, so a caller cannot
/// mistake "no device" for "empty and done".
pub(super) fn tier3_only_target_reached(
    tier3_pct: Option<u64>,
    target_pct: u64,
    allocated_mb: u64,
) -> bool {
    allocated_mb > 0 && tier3_pct.is_some_and(|pct| pct >= target_pct)
}

pub(super) fn full_profile_cache_target(
    requested_mib: Option<u64>,
    cache: Option<CacheSample>,
) -> Result<u64, String> {
    let cache =
        cache.ok_or("full three-tier profile requires fresh ACTIVE physical cache telemetry")?;
    // The request is whole MiB and the sample is measured in KiB. Compare in
    // KiB so a sub-MiB active target is not rounded away before the check;
    // under a floor-to-MiB conversion that target read as `0` and the call
    // refused a cache that was actually there.
    let wanted_kib = requested_mib.map_or(cache.target_kib, requested_kib);
    if wanted_kib == 0 || wanted_kib > cache.target_kib {
        let requested_mib = requested_mib.unwrap_or_else(|| cache.target_mib());
        return Err(format!(
            "requested physical cache target {requested_mib} MiB exceeds active target {} MiB",
            cache.target_mib()
        ));
    }
    // The report field this feeds is named in MiB. Publish the ceiling, never
    // a floor that turns a present target into zero.
    Ok(cache.target_kib.min(wanted_kib).div_ceil(KIB_PER_MIB))
}

/// Qualify logical occupancy and physical cache from one fresh sample.
///
/// A tier whose ratio refused — no swap device, so no denominator — is not at
/// target and cannot be: `None` here is a missing tier, not an empty one, and
/// a full-tiers claim needs all three to exist.
pub(super) fn full_tier_snapshot(
    zram_pct: Option<u64>,
    logical_nbd_pct: Option<u64>,
    ssd_pct: Option<u64>,
    opts: &StressOptions,
    cache: Option<CacheSample>,
) -> Option<CacheSample> {
    if zram_pct.is_none_or(|pct| pct < opts.tier1_target_pct)
        || logical_nbd_pct.is_none_or(|pct| pct < opts.tier2_target_pct)
        || ssd_pct.is_none_or(|pct| pct < opts.tier3_target_pct.unwrap_or(100))
    {
        return None;
    }

    cache.filter(|sample| {
        sample.target_kib > 0
            && sample.at_target
            && opts.physical_cache_target_mib.is_none_or(|requested_mib| {
                let wanted = requested_kib(requested_mib);
                sample.target_kib >= wanted && sample.cached_kib >= wanted
            })
    })
}

/// How much the next step may take, in KiB.
///
/// The return is a **bound**, not a face. It is what the ramp may allocate, so
/// every rounding in here has to point down: a headroom rounded up would let
/// the step cross the floor it is supposed to respect. That is the opposite
/// direction from [`published_mib`], which exists so a figure can never reach
/// the screen as zero — both are views of a KiB measurement, and they answer
/// different questions.
///
/// `avail_kib: None` is an unmeasured headroom. It is treated as `0` here,
/// which is the conservative answer and not a measurement: every branch
/// below subtracts a floor from it, so `0` returns no allocation rather than
/// granting one. Same shape as [`avail_kib_for_floor`].
///
/// Everything is KiB, so there is no rounding left to get wrong: the policy
/// step caps and reserves are whole-MiB values stored as KiB, and the
/// difference between `avail` and the floor is the exact one `/proc/meminfo`
/// reported.
pub(super) fn safe_allocation_kib(
    is_multi_tier: bool,
    tier3_active: bool,
    avail_kib: Option<u64>,
    hard_floor_kib: u64,
    one_pct_kib: u64,
) -> u64 {
    let avail_kib = avail_kib.unwrap_or(0);
    if !is_multi_tier {
        return one_pct_kib
            .min(avail_kib.saturating_sub(hard_floor_kib))
            .min(SINGLE_TIER_MAX_STEP_KIB);
    }
    let reserve = if tier3_active {
        TIER3_HEADROOM_RESERVE_KIB
    } else {
        PRE_TIER3_HEADROOM_RESERVE_KIB
    };
    let budget = avail_kib.saturating_sub(hard_floor_kib.saturating_add(reserve));
    let step_cap = if tier3_active
        || avail_kib <= hard_floor_kib.saturating_add(PRE_TIER3_FULL_STEP_HEADROOM_KIB)
    {
        TIER3_MAX_STEP_KIB
    } else {
        PRE_TIER3_MAX_STEP_KIB
    };
    one_pct_kib.min(budget).min(step_cap)
}

/// Whether fallback swap grew past the pre-existing baseline during the run.
///
/// Both figures are KiB, and so is the threshold: a whole-MiB comparison here
/// would call a 33 MiB growth "not active" whenever the baseline's rounding
/// absorbed a megabyte, which is the wrong branch of a pacing decision.
///
/// Either figure absent is the conservative answer `true` — assume the
/// fallback is active and take the slower WSL2 step interval. A synthesized
/// zero on either side would claim the device is idle or started empty and
/// take the fast branch on a thrashing host, which is the opposite of the
/// safe direction.
pub(super) fn tier3_active_since_baseline(
    used_kib: Option<u64>,
    initial_used_kib: Option<u64>,
) -> bool {
    let (Some(used_kib), Some(initial_used_kib)) = (used_kib, initial_used_kib) else {
        return true;
    };
    used_kib > initial_used_kib.saturating_add(TIER3_ACTIVITY_MIN_GROWTH_KIB)
}

pub(super) fn step_interval_ms(is_wsl2_host: bool, tier3_active: bool, requested_ms: u64) -> u64 {
    if is_wsl2_host && tier3_active {
        requested_ms.max(WSL2_TIER3_STEP_INTERVAL_MS)
    } else {
        requested_ms
    }
}

/// Whether the storage tier has reached its qualification target.
///
/// See [`tier3_only_target_reached`]: `None` has reached nothing.
pub(super) fn tier3_target_reached(tier3_pct: Option<u64>, target_pct: u64) -> bool {
    tier3_pct.is_some_and(|pct| pct >= target_pct)
}
