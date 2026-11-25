// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/nbd_runtime/residency.rs
//! NBD residency canary, reserve-floor binding, and demote policy helpers.

use std::time::Instant;

use ramshared_vram::{
    ReserveFloorPolicy, SEALED_RESERVE_MIN_MIB, SEALED_RESERVE_PERCENT, VramMemory,
};
use ramshared_wsl2d::swap::{DemoteRearm, SwapoffOutcome};
use ramshared_wsl2d::{
    Cadence, Canary, CanaryProbe, DemoteReason, DemoteStatusFile, ResidencyConfig,
    ResidencySampler, Verdict,
};

/// Per-request residency shared by NBD workers (single and broker): arms the latency
/// canary (§9, baseline→Canary; serve-only, DT-16) and runs the §9.4 probe (content/free in
/// cadence, with hysteresis via streak). Returns `Some(reason)` if any signal requests DEMOTE;
/// the caller decides the ACTION (local swapoff in single, `DemoteAll` via broker in multi-slice).
pub(crate) struct ResidencyCheckState<'a, M: VramMemory> {
    pub(crate) canary: &'a mut Option<Canary>,
    pub(crate) baseline: &'a mut Vec<u64>,
    pub(crate) sampler: &'a mut ResidencySampler,
    pub(crate) cadence: &'a mut Cadence,
    pub(crate) probe: &'a mut CanaryProbe<M>,
    pub(crate) free_floor_bytes: u64,
}

pub(crate) fn residency_check<M: VramMemory, F: Fn() -> Option<u64>>(
    lat_us: u64,
    state: &mut ResidencyCheckState<'_, M>,
    mem_free: F,
) -> Option<DemoteReason> {
    // §9: per-request latency canary. content_ok=true/free=u64::MAX ON PURPOSE — the signal
    // here is latency; content and free-floor come from the probe §9.4 below.
    let mut latency_reason = None;
    match state.canary.as_mut() {
        None => {
            state.baseline.push(lat_us);
            if state.baseline.len() >= 16 {
                state.baseline.sort_unstable();
                let med = state.baseline[state.baseline.len() / 2].max(1);
                *state.canary = Some(Canary::new(ResidencyConfig::default(), med));
                eprintln!("[ramsharedd] canary armed (baseline={med} us)");
            }
        }
        Some(c) => {
            if let Verdict::Demote(reason) = c.sample(lat_us, true, u64::MAX) {
                latency_reason = Some(reason);
            }
        }
    }
    // §9.4: dedicated content/free probe in cadence (corrupted content demotes immediately;
    // free-floor/transient error require streak).
    let mut probe_reason = None;
    if state.cadence.tick() {
        let content = state.probe.check_content().ok();
        let free = mem_free();
        let verdict = state.sampler.sample(content, free);
        let streak = state.sampler.bad_streak();
        let trace_probe = std::env::var("RAMSHARED_TRACE_PROBE").ok().as_deref() == Some("1");
        if should_log_probe_sample(content, free, state.free_floor_bytes, streak, trace_probe) {
            eprintln!(
                "[ramsharedd] probe §9.4 sample: content={content:?} free={free:?} \
                 floor={} streak={streak}",
                state.free_floor_bytes
            );
        }
        if let Verdict::Demote(reason) = verdict {
            eprintln!(
                "[ramsharedd] probe §9.4: content={content:?} free={free:?} streak={}",
                streak
            );
            probe_reason = Some(reason);
        }
    }
    choose_residency_reason(latency_reason, probe_reason)
}

pub(crate) fn choose_residency_reason(
    latency: Option<DemoteReason>,
    probe: Option<DemoteReason>,
) -> Option<DemoteReason> {
    probe.or(latency)
}

pub(crate) fn should_log_probe_sample(
    content: Option<bool>,
    free: Option<u64>,
    free_floor_bytes: u64,
    streak: u32,
    trace_probe: bool,
) -> bool {
    trace_probe
        || content != Some(true)
        || free.is_none()
        || free.is_some_and(|f| f < free_floor_bytes.saturating_mul(2))
        || streak > 0
}

pub(crate) fn sparse_residency_config(reserve_floor_bytes: u64) -> ResidencyConfig {
    ResidencyConfig {
        free_floor_bytes: reserve_floor_bytes,
        ..ResidencyConfig::default()
    }
}

/// Whether a residency DEMOTE should `swapoff` the tier in **sparse** mode.
///
/// Sparse latency is **chunk-alloc latency**, not WDDM eviction, so it is not
/// the §9 / ADR-0003 eviction signal and must not tear the tier down
/// (`false DEMOTE` — `Documentation/architecture/three-tier-cascade.rst`
/// ITEM-2). Only a real integrity failure (`Corruption`) forces the tier out.
///
/// `FreeFloor` **does** swapoff (`e5ef5fcf`): the sparse cache holds live swap
/// pages, so `sparse_vram::try_reclaim` cannot free anything while
/// `nbd_used_kb > 0`, and an external VRAM consumer below the sealed reserve
/// floor needs the tier drained.
pub(crate) fn sparse_residency_requests_swapoff(reason: DemoteReason) -> bool {
    !matches!(reason, DemoteReason::Latency)
}

/// Whether the NBD DEMOTE path may spawn a new `swapoff` right now.
///
/// One predicate for every steady-state spawn site so a later trigger cannot
/// silently bypass the retry policy. Detection keeps
/// sampling through a backoff — only the spawn waits — so a transient
/// rejection delays the attempt without blinding the canary.
pub(crate) fn demote_spawn_authorized(
    rearm: &DemoteRearm,
    demote_rx: &Option<std::sync::mpsc::Receiver<SwapoffOutcome>>,
    demoted: bool,
) -> bool {
    !demoted && demote_rx.is_none() && rearm.may_attempt_at(Instant::now())
}

/// Build the DEMOTE status record the CLI `status --json` surface reads.
///
/// `pending_age_secs` is `None` for every routine publish; it is set only when
/// a drain has outlived the stuck-detection window (SPEC §9.2), which is the
/// one place the loop is allowed to describe a hang.
pub(crate) fn demote_status(
    total: u64,
    reason: &Option<String>,
    in_progress: bool,
    pending_age_secs: Option<u64>,
) -> DemoteStatusFile {
    DemoteStatusFile {
        total,
        last_reason: reason.clone(),
        in_progress,
        pending_age_secs,
    }
}

/// `true` only when global GPU free was measured at or above the sealed
/// floor.
///
/// This is a recovery licence, so absence does not grant it: putting the
/// tier back into service is only safe while GPU headroom is *known* to be
/// above the floor. The sibling WDDM budget probe already fails closed when
/// its snapshot errors; this is the same rule for the global probe.
pub(crate) fn global_free_at_or_above_floor(free_bytes: Option<u64>, floor_bytes: u64) -> bool {
    free_bytes.is_some_and(|free| free >= floor_bytes)
}

/// Fold one global GPU free sample into the demote streak.
///
/// The streak counts consecutive samples measured below the floor. Only a
/// measurement can move it in either direction: an unreadable probe is not a
/// reading of headroom, so it must not erase the readings that were, and it
/// is not a reading of pressure either, so it must not stand in for one. The
/// demote keeps its requirement of `required` actual measurements.
pub(crate) fn observe_global_free_floor(
    free_bytes: Option<u64>,
    floor_bytes: u64,
    committed_bytes: u64,
    streak: &mut u32,
    required: u32,
) -> bool {
    if committed_bytes == 0 {
        // Nothing is committed, so there is no tier whose GPU residency this
        // floor protects. A streak with nothing behind it is not a constraint.
        *streak = 0;
        return false;
    }
    match free_bytes {
        Some(free) if free < floor_bytes => {
            *streak = streak.saturating_add(1);
            *streak >= required.max(1)
        }
        Some(_) => {
            // A measured observation of headroom is the only thing that ends
            // a constraint streak.
            *streak = 0;
            false
        }
        None => false,
    }
}

/// DT-7 check (b): bind the resolved policy to what the seal verified.
///
/// The verified literals come from the single authority in
/// `ramshared_vram::reserve_policy` (`SEALED_RESERVE_MIN_MIB` /
/// `SEALED_RESERVE_PERCENT`); a local copy would be a drift hazard the seal
/// cannot detect.
pub(crate) fn enforce_reserve_policy_binding(
    resolved: &ReserveFloorPolicy,
    verified_min_mib: u64,
    verified_percent: u64,
) -> Result<(), String> {
    let verified_min_bytes = verified_min_mib.saturating_mul(1024 * 1024);
    if resolved.min_floor_bytes < verified_min_bytes {
        return Err(format!(
            "resolved reserve floor {} B undercuts sealed authority {} B",
            resolved.min_floor_bytes, verified_min_bytes
        ));
    }
    if resolved.sealed_percent != verified_percent {
        return Err(format!(
            "resolved sealed percent {} differs from verified {}",
            resolved.sealed_percent, verified_percent
        ));
    }
    Ok(())
}

/// Resolve and bind the configured reserve floor at NBD startup (DT-1/DT-7/DT-10).
pub(crate) fn resolve_reserve_floor_policy(
    native_origin_mode: bool,
    total: u64,
) -> Result<ReserveFloorPolicy, Box<dyn std::error::Error>> {
    if native_origin_mode {
        // GPU overrides cannot deny access to an existing native SSD origin.
        // Native cache authority remains unavailable in this admission stage.
        return Ok(ReserveFloorPolicy::sealed());
    }
    let reserve_policy = ramshared_block::sparse_vram::sealed_reserve_policy_from_env()
        .map_err(|error| error.to_string())?;
    enforce_reserve_policy_binding(
        &reserve_policy,
        SEALED_RESERVE_MIN_MIB,
        SEALED_RESERVE_PERCENT,
    )?;
    let sealed_only =
        ReserveFloorPolicy::from_manifest(SEALED_RESERVE_MIN_MIB, SEALED_RESERVE_PERCENT)
            .map_err(|error| error.to_string())?;
    let runtime_headroom = ramshared_block::gpu_cache_worker::RUNTIME_FREE_BUFFER_BYTES;
    let label_capacity = ReserveFloorPolicy::helper_capacity(Some(total), total);
    let source = reserve_policy.source_label(&sealed_only, label_capacity);
    let override_mib = ramshared_block::sparse_vram::reserve_floor_override_mib();
    let subsumed_note = match (&override_mib, source) {
        (Some(_), ramshared_vram::ReserveFloorSource::SealedManifest) => " (override subsumed)",
        _ => "",
    };
    eprintln!(
        "[ramsharedd] reserve-floor: enforced={} MiB source={}{} sealed_min={} MiB override={:?} percent={}% runtime={} MiB",
        reserve_policy.min_floor_bytes >> 20,
        source,
        subsumed_note,
        SEALED_RESERVE_MIN_MIB,
        override_mib,
        reserve_policy.sealed_percent,
        runtime_headroom >> 20,
    );
    Ok(reserve_policy)
}
