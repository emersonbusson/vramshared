// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/snapshot.rs
//! GPU cache telemetry validation, cascade snapshot assembly, and readiness/continuation pressure gates.
//!
//! Extracted from `cascade/mod.rs` to isolate snapshot construction and pressure gating from formatting and swap-table parsing.

use super::*;
use std::fs;
use std::path::Path;

/// True only when the daemon published a `gpu_budget` object that cannot be
/// trusted right now.
///
/// A missing key or a JSON `null` is an honest "no budget bound" report (the
/// daemon runs `gpu_budget_guard=allocator_only`). That is valid telemetry,
/// not invalid or stale telemetry, so it must not alarm. The reduction itself
/// is the shared one in [`crate::published_budget`].
pub(super) fn gpu_budget_telemetry_invalid_or_stale(
    cache_status: Option<&serde_json::Value>,
    now_unix_ms: Option<u64>,
) -> bool {
    let Some(status) = cache_status else {
        return false;
    };
    match status.get("gpu_budget") {
        None | Some(serde_json::Value::Null) => false,
        Some(_) => trusted_budget(status, now_unix_ms).is_none(),
    }
}

/// Bytes → KiB for an **allocation bound** — "you may take at most this".
///
/// Floor, never ceil. Rounding up would grant more than the exact remainder:
/// 512 free bytes published as `1 KiB` is twice the bound. A remainder of
/// `1..=1023` bytes therefore floors to `0 KiB`, and that zero is a bound of
/// "at most 0 KiB", not a measured emptiness. The third state lives in the
/// raw `available_bytes` on the [`GpuBudgetTelemetry`] this projection sits
/// beside, which is exactly why the document publishes the record and the
/// bound as two fields: a reader that must tell a measured zero from a
/// sub-KiB remainder reads the record, never this number.
///
/// This is the opposite direction from a hold or a size, which ceils so a
/// non-zero measurement never publishes as zero. The daemon's twin of that
/// face is `hold_kib` in `ramshared-wsl2d`; one helper cannot serve both
/// quantities and this module only needs the bound half. The MiB twin of
/// this face is `monitor::render::free_bound_label`.
pub(super) const fn alloc_bound_kib(bytes: u64) -> u64 {
    bytes / 1024
}

/// The published budget sample as a record, and the headroom one may act on.
///
/// Two different facts, and the pair is the point. The record is kept even
/// when it is too old to act on: `gpu_budget: null` means the daemon never
/// bound a budget (`gpu_budget_guard=allocator_only`), and spending that
/// `null` on a sample that merely aged out would tell the operator the daemon
/// never had one. The projection is the opposite and does fail closed — `None`
/// whenever the sample fails [`GpuBudgetTelemetry::trusted_available_at`] —
/// which is why `capacity.gpu_headroom_kib: null` beside a present
/// `gpu_budget` is the correct pairing and not a contradiction.
///
/// The projected figure takes [`alloc_bound_kib`] because headroom is an
/// allocation bound: it floors, and a sub-KiB remainder publishes as `0` at
/// KiB resolution while the raw remainder stays on the record.
///
/// The alarm is a third fact and is unchanged: a present sample that cannot be
/// trusted is what [`gpu_budget_telemetry_invalid_or_stale`] reports. Keeping
/// the record is what lets that alarm name a sample at all — collapsing the
/// sample to `null` first would leave the alarm with nothing to point at.
pub(super) fn budget_record_and_headroom(
    status: Option<&serde_json::Value>,
    now_unix_ms: Option<u64>,
) -> (Option<GpuBudgetTelemetry>, Option<u64>) {
    let record = status.and_then(GpuBudgetTelemetry::from_published_status);
    let headroom_kib = record.as_ref().and_then(|budget| {
        now_unix_ms
            .and_then(|now| budget.trusted_available_at(now, crate::monitor::GPU_BUDGET_MAX_AGE_MS))
            .map(alloc_bound_kib)
    });
    (record, headroom_kib)
}

/// The published busy-time sample as a record, never as an absence marker.
///
/// Sibling of [`budget_record_and_headroom`], same split. [`trusted_utilization`]
/// is the admission collapse: a sample that merely aged out lands on the same
/// `None` as a sample the driver never produced. That is exactly what
/// [`gpu_utilization_telemetry_invalid_or_stale`] wants — "present but not
/// trustworthy" — and it is not what the published record wants. `null` in this
/// stream means never measured, so spending it on a reading that exists but is
/// no longer current erases the reading and tells the operator the driver never
/// spoke when it spoke seconds ago.
///
/// The display gate lives at the renderer: `utilization_row` compares the
/// sample's own `sampled_at_unix_ms` against the observation epoch through
/// [`GpuUtilizationTelemetry::trusted_at`] and spells an aged sample `stale`
/// while a malformed one still reads `not measured`. A present `gpu_utilization`
/// beside a `stale` row is therefore the correct pairing, not a contradiction —
/// the record and the figure one may act on are two different facts.
///
/// Deliberately not folded together with the cache-occupancy record
/// ([`cache_record`]), which is the same split one seam further along and must
/// land under its own name.
pub(super) fn utilization_record(
    status: Option<&serde_json::Value>,
) -> Option<GpuUtilizationTelemetry> {
    status.and_then(GpuUtilizationTelemetry::from_published_status)
}

/// The cache-event counters the daemon publishes as document-level keys.
///
/// These are cumulative event counts, not sizes and not a heartbeat sample.
/// They are read under their own names and returned under those names for the
/// composed document — never folded into [`OriginCacheIo`] (which deliberately
/// excludes them so a reader cannot mistake them for members of the origin-plane
/// accounting) and never into `capacity` (which holds KiB sizes).
///
/// A cumulative count has no age of its own, which is why this reduction takes
/// no clock: there is no per-counter timestamp that could go stale the way
/// [`budget_record_and_headroom`] and [`utilization_record`] have one. That is
/// not the same as saying the count survives a refused document. The composer
/// admits a document only when it has the current shape **and** names the
/// current daemon instance inside the control-plane window, and a refused
/// document is dropped whole — these three with every other key on it. That
/// uniform refusal is deliberate: a count a dead instance accumulated is not
/// the live instance's count, and publishing it would attribute one process's
/// events to another. Only the per-field collapse is refused, because that one
/// spends `null` on a reading the admitted document actually carried.
///
/// `None` therefore means "no admitted daemon document carried a measured
/// value": no document at all, a document refused at the seam above, or an
/// admitted older producer that never published the key. It is never a zero
/// count. A present `0` is a real zero and is kept as `Some(0)` for the same
/// reason `Some(budget)` is kept beside a `None` headroom: in this stream
/// `null` means never measured, and spending it on a measured zero erases the
/// measurement.
///
/// Returned as a triple in the daemon's own key order: fallback reads,
/// invalidations, releases.
pub(super) fn cache_event_counters(
    status: Option<&serde_json::Value>,
) -> (Option<u64>, Option<u64>, Option<u64>) {
    let number = |key: &str| {
        status
            .and_then(|value| value.get(key))
            .and_then(serde_json::Value::as_u64)
    };
    (
        number("cache_fallback_reads"),
        number("cache_invalidations"),
        number("cache_releases"),
    )
}

/// The published cache-occupancy sample as a record, never as an absence marker.
///
/// Sibling of [`budget_record_and_headroom`] and [`utilization_record`], same
/// split. [`trusted_cache_telemetry`] is the admission collapse: a sample that
/// merely aged out lands on the same `None` as a sample the worker never
/// produced. That is exactly what [`gpu_cache_telemetry_invalid_or_stale`] wants
/// — "present but not trustworthy" — and it is not what the published record
/// wants. `null` in this stream means never measured, so spending it on a
/// reading that exists but is no longer current erases the reading and tells
/// the operator the worker never spoke when it spoke seconds ago.
///
/// Unlike the budget there is no separate projection field the way
/// `capacity.gpu_headroom_kib` is one: the cache sample is the only carrier of
/// its occupancy, so — exactly like the busy time — the gate lives at the
/// renderer rather than onto a sibling wire field. That renderer
/// (`monitor::cache_telemetry_row`) splits the two refusals itself through
/// [`WorkerCacheTelemetry::is_well_formed`] and
/// [`WorkerCacheTelemetry::is_fresh_at`], never through the combined
/// [`WorkerCacheTelemetry::trusted_at`], because the two refusals read as
/// different things to an operator.
///
/// Deliberately not confused with `capacity.vram_cached_kib`, which is the
/// physical frame-header occupancy from a different source and is policed as
/// never being an alias of this sample.
pub(super) fn cache_record(status: Option<&serde_json::Value>) -> Option<WorkerCacheTelemetry> {
    status
        .and_then(|value| value.get("gpu_cache"))
        .and_then(|field| {
            if field.is_null() {
                None
            } else {
                serde_json::from_value::<WorkerCacheTelemetry>(field.clone()).ok()
            }
        })
}

/// True only when the daemon published a `gpu_utilization` object that cannot
/// be trusted right now.
///
/// A missing key or a JSON `null` is an honest "not measured" report — the
/// adapter had no utilization source, or the driver refused the call. That is
/// valid telemetry, not invalid or stale telemetry, so it must not alarm. A
/// present sample that is malformed, out of range or too old does alarm: a
/// bad percent must never be silently dropped and then rendered as idle.
pub(super) fn gpu_utilization_telemetry_invalid_or_stale(
    cache_status: Option<&serde_json::Value>,
    now_unix_ms: Option<u64>,
) -> bool {
    let Some(status) = cache_status else {
        return false;
    };
    match status.get("gpu_utilization") {
        None | Some(serde_json::Value::Null) => false,
        Some(_) => trusted_utilization(status, now_unix_ms).is_none(),
    }
}

/// True only when the daemon published a `gpu_cache` object that cannot be
/// trusted right now.
///
/// A missing key or a JSON `null` is an honest "no cache telemetry" report —
/// the worker is not running a cache, or it never sampled one. That is valid
/// telemetry, not invalid or stale telemetry, so it must not alarm, and it
/// must never be read as a measured empty cache. A present sample that is
/// malformed, unknown-schema or too old does alarm: a stale occupancy figure
/// silently republished is the "stale green" the dashboard refuses.
pub(super) fn gpu_cache_telemetry_invalid_or_stale(
    cache_status: Option<&serde_json::Value>,
    now_unix_ms: Option<u64>,
) -> bool {
    let Some(status) = cache_status else {
        return false;
    };
    match status.get("gpu_cache") {
        None | Some(serde_json::Value::Null) => false,
        Some(_) => trusted_cache_telemetry(status, now_unix_ms).is_none(),
    }
}

/// Map the daemon's published cache verdict onto the operator-facing state.
///
/// `ok` exists to stop a lying `"ACTIVE"` from being believed, not to erase the
/// daemon's own verdict about a cache that is deliberately off. A released
/// cache under control pressure (`"OFF"`) and a dead one (`"UNAVAILABLE"`) are
/// different states for the operator and for recovery policy, so the named
/// non-serving verdicts survive an `ok: false`.
pub(super) fn cache_state_from_status(
    published: Option<&str>,
    product_active: bool,
    cache_status_ok: bool,
) -> CacheState {
    match published {
        Some("OFF") => CacheState::Off,
        Some("STUCK") => CacheState::Stuck,
        Some("UNAVAILABLE") => CacheState::Unavailable,
        Some("ACTIVE") if product_active && cache_status_ok => CacheState::Active,
        Some("RESTRICTED") if product_active && cache_status_ok => CacheState::Restricted,
        Some("ACTIVE") | Some("RESTRICTED") => CacheState::Unavailable,
        _ if product_active => CacheState::Unavailable,
        _ => CacheState::Off,
    }
}

/// The device name a device path names.
///
/// `/dev/nbd0` and `nbd0` are the same device for every comparison the
/// topology makes, and the kernel's own attributes use the short name.
pub(super) fn device_basename(path: &str) -> String {
    Path::new(path.trim())
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_string()
}

/// The topology facts the health rules read (SPEC DT-1).
///
/// Two measurements, both from the running system: the kernel's `backing_dev`
/// attribute on the live zram device names the device the zram writes its
/// pages back to, and the product's own runtime record names the tier it
/// attached. Their **agreement** is what makes the tier a writeback target
/// rather than a swap device — the tier is then never in the swap table, and
/// the swap-tier rules must not be applied to it.
///
/// An attribute that never answered leaves the backing device unknown, and an
/// unknown backing device is not a writeback topology: the facts say
/// `None`, never a guess.
pub(super) fn topology_facts(entries: &[SwapEntry]) -> TopologyFacts {
    let zram_backing_dev = entries
        .iter()
        .filter(|entry| !entry.is_ghost() && swap_tier_role(&entry.filename) == SwapTierRole::Zram)
        .filter_map(|entry| {
            Path::new(&entry.filename)
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
        })
        .find_map(|name| {
            fs::read_to_string(Path::new("/sys/block").join(name).join("backing_dev"))
                .ok()
                .map(|text| text.trim().to_string())
                .filter(|value| !value.is_empty() && value != "none")
        });
    let tier_device = match fs::read_to_string(SWAP_DEV_FILE) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            root_status_cache::verified_tier_device()
        }
        Err(_) => None,
    }
    .map(|text| text.trim().to_string())
    .filter(|value| !value.is_empty())
    .map(|value| device_basename(&value))
    .filter(|value| !value.is_empty());
    let writeback_target_is_tier = match (&zram_backing_dev, &tier_device) {
        (Some(backing), Some(tier)) => device_basename(backing) == *tier,
        _ => false,
    };
    TopologyFacts {
        zram_backing_dev,
        tier_device,
        writeback_target_is_tier,
    }
}

/// The reason the daemon published beside a cache that is not serving.
///
/// The daemon owns the cache and the reason; the reader only carries it. An
/// absent field is "the daemon said nothing", which is not the same fact as
/// a published cause.
pub(super) fn cache_unavailable_reason_from_status(
    cache_status: Option<&serde_json::Value>,
) -> Option<String> {
    cache_status?
        .get("cache_unavailable_reason")?
        .as_str()
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
        .map(str::to_string)
}

pub fn build_cascade_snapshot(entries: &[SwapEntry]) -> CascadeSnapshot {
    let pairs: Vec<(&str, u64, u64, i32)> = entries
        .iter()
        .filter(|e| !e.is_ghost())
        .map(|e| (e.filename.as_str(), e.size_kb, e.used_kb, e.priority))
        .collect();
    let (zram, vram, disk, order_ok) = lifecycle::tiers_from_swap_names(&pairs);
    let topology = topology_facts(entries);
    let ghosts = ghost_vram_swaps(entries);
    let (daemon_alive, daemon_pid) = daemon_alive_pid();
    let daemon_identity_unreadable = matches!(
        fs::read_to_string(PID_FILE),
        Err(ref error) if error.kind() == std::io::ErrorKind::PermissionDenied
    );
    let product_active = daemon_alive || vram.present;
    let cache_status = read_status_document(Path::new(CACHE_STATUS_FILE));
    let supervisor_status = fs::read_to_string(SUPERVISOR_STATUS_FILE)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
    let daemon_instance_id = daemon_pid.and_then(daemon_instance_id_from_pid);
    let (cache_status_current, supervisor_status_current) = daemon_instance_id
        .as_deref()
        .zip(unix_time_ms())
        .map_or((false, false), |(daemon_instance_id, now_unix_ms)| {
            control_plane_status_freshness(
                cache_status.as_ref(),
                supervisor_status.as_ref(),
                daemon_instance_id,
                now_unix_ms,
            )
        });
    let cache_status = cache_status_current.then_some(cache_status).flatten();
    let supervisor_status = supervisor_status_current
        .then_some(supervisor_status)
        .flatten();
    let status_text = |key: &str| {
        cache_status
            .as_ref()
            .and_then(|value| value.get(key))
            .and_then(serde_json::Value::as_str)
    };
    let status_number = |key: &str| {
        cache_status
            .as_ref()
            .and_then(|value| value.get(key))
            .and_then(serde_json::Value::as_u64)
    };
    let (gpu_budget, gpu_headroom_kib) =
        budget_record_and_headroom(cache_status.as_ref(), unix_time_ms());
    let gpu_utilization = utilization_record(cache_status.as_ref());
    let host_gpu_activity = cache_status.as_ref().and_then(|status| {
        let sample: ramshared_vram::host_gpu_activity::HostGpuActivityTelemetry =
            serde_json::from_value(status.get("host_gpu_activity")?.clone()).ok()?;
        sample.report_at(unix_time_ms()?)?;
        Some(sample)
    });
    // DT-8: cache occupancy and codec health travel beside the adapter budget
    // and the busy-time sample, never merged into either. The record is kept;
    // only the renderer decides whether it may be shown.
    let gpu_cache = cache_record(cache_status.as_ref());
    // Origin-plane I/O accounting travels beside them. `None` is an older
    // producer that never published the field, never a zero-filled accounting.
    let origin_cache_io = cache_status
        .as_ref()
        .and_then(|value| trusted_origin_cache_io(value.get("origin_cache_io")));
    let (cache_fallback_reads, cache_invalidations, cache_releases) =
        cache_event_counters(cache_status.as_ref());
    let cache_hits = cache_status
        .as_ref()
        .and_then(|value| value.get("cache_hits"))
        .and_then(serde_json::Value::as_u64);
    let host_memory = cache_status
        .as_ref()
        .and_then(|value| value.get("host_memory"))
        .filter(|value| !value.is_null())
        .and_then(|value| {
            serde_json::from_value::<ramshared_ipc::HostMemoryTelemetry>(value.clone()).ok()
        });
    let host_storage = cache_status
        .as_ref()
        .and_then(|value| value.get("host_storage"))
        .filter(|value| !value.is_null())
        .and_then(|value| {
            serde_json::from_value::<ramshared_ipc::storage_proof::HostStorageTelemetry>(
                value.clone(),
            )
            .ok()
        });
    let cache_status_ok = cache_status
        .as_ref()
        .and_then(|value| value.get("ok"))
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    let origin_state = match status_text("origin_state") {
        Some("READY") => OriginState::Ready,
        Some("DEGRADED") => OriginState::Degraded,
        Some("FAILED") => OriginState::Failed,
        _ if product_active && capacity_field("mode").as_deref() == Some("origin-cache") => {
            OriginState::Degraded
        }
        _ => OriginState::Off,
    };
    // `ok` exists to stop a lying "ACTIVE" from being believed, not to erase
    // the daemon's own verdict about a cache that is deliberately off. A
    // released cache under control pressure (`OFF`) and a dead one
    // (`UNAVAILABLE`) are different states for the operator and for recovery
    // policy, so the named non-serving verdicts survive an `ok: false`.
    let cache_state =
        cache_state_from_status(status_text("cache_state"), product_active, cache_status_ok);
    let cache_unavailable_reason = cache_unavailable_reason_from_status(cache_status.as_ref());
    let control_plane_state = cache_status
        .as_ref()
        .and_then(|status| status.get("control_plane_state"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|state| !state.is_empty())
        .map(str::to_string);
    let control_plane_lease_deadline_ms = cache_status
        .as_ref()
        .and_then(|status| status.get("control_plane_lease_deadline_ms"))
        .and_then(serde_json::Value::as_u64);
    let control_state = supervisor_status
        .as_ref()
        .and_then(|value| value.get("control_state")?.as_str())
        .map_or(ControlState::Guarded, |state| match state {
            "GUARDED" => ControlState::Guarded,
            "CRITICAL" => ControlState::Critical,
            "EMERGENCY" => ControlState::Emergency,
            "SAFE_MODE" => ControlState::SafeMode,
            "HEALTHY" => ControlState::Healthy,
            _ => ControlState::Guarded,
        });
    let (guardian_state, guardian_error) = sealed_guardian_state();
    let mut measurement_errors = Vec::new();
    if vram.present && daemon_identity_unreadable {
        measurement_errors.push("daemon_identity_unreadable".to_string());
    }
    if product_active && !cache_status_current {
        measurement_errors.push("cache_status_not_current".to_string());
    }
    if product_active
        && gpu_budget_telemetry_invalid_or_stale(cache_status.as_ref(), unix_time_ms())
    {
        measurement_errors.push("gpu_budget_telemetry_invalid_or_stale".to_string());
    }
    if product_active
        && gpu_utilization_telemetry_invalid_or_stale(cache_status.as_ref(), unix_time_ms())
    {
        measurement_errors.push("gpu_utilization_telemetry_invalid_or_stale".to_string());
    }
    if product_active && gpu_cache_telemetry_invalid_or_stale(cache_status.as_ref(), unix_time_ms())
    {
        measurement_errors.push("gpu_cache_telemetry_invalid_or_stale".to_string());
    }
    if product_active && !supervisor_status_current {
        measurement_errors.push("supervisor_status_not_current".to_string());
    }
    if let Some(error) = guardian_error {
        measurement_errors.push(error);
    }
    // An unlisted fallback tier publishes `None`, never a measured `0`.
    let fallback_swap_used_kib = fallback_swap_occupancy_kib(&disk);
    let capacity_guaranteed = observe_capacity_guaranteed();
    if vram.present && capacity_guaranteed.is_none() {
        measurement_errors.push("capacity_guarantee_unreadable".to_string());
    }
    CascadeSnapshot {
        zram,
        vram,
        origin_cache_mode: (product_active && cache_status_current)
            .then(|| capacity_field("mode"))
            .flatten()
            .and_then(|mode| (mode == "origin-cache").then_some(true)),
        disk,
        ghost: !ghosts.is_empty(),
        order_ok,
        daemon_alive,
        daemon_pid,
        capacity_guaranteed,
        disk_baseline_kib: read_capacity_disk_baseline(),
        demote: read_demote_snapshot(),
        active_kib: active_threshold_kib_from_env(),
        control_state,
        origin_state,
        cache_state,
        guardian_state,
        cache_target_kib: status_number("cache_target_kib"),
        logical_capacity_kib: capacity_field_u64("logical_capacity_kib"),
        vram_cached_kib: status_number("vram_cached_kib"),
        gpu_headroom_kib,
        gpu_budget,
        gpu_utilization,
        host_gpu_activity,
        gpu_cache,
        origin_cache_io,
        // Cache-event counters the daemon publishes as document-level keys and
        // `OriginCacheIo` deliberately leaves out. Copied through under those
        // names so the composed document and the JSONL stop dropping a measured
        // counter at this seam.
        cache_fallback_reads,
        cache_hits,
        host_memory,
        host_storage,
        cache_invalidations,
        cache_releases,
        zram_compression: crate::zram_compression::read_zram_compression(),
        // SPEC DT-3: the one derivation of Tier 1's state, read here for the
        // published document and by the dashboard row. It reads the same
        // swap table this snapshot was built from.
        zram_tier_state: crate::monitor::zram_tier_state_from(Path::new("/sys/block"), entries),
        ssd_origin_written_kib: status_number("ssd_origin_written_kib"),
        fallback_swap_used_kib,
        release_coherence: observe_release_coherence(),
        measurement_errors,
        topology,
        cache_unavailable_reason,
        control_plane_state,
        control_plane_lease_deadline_ms,
    }
}

/// Which pressure gate a caller needs.
///
/// The supervisor's first protective step is `ControlState::Guarded`, whose
/// only action is `CloseAdmission`: refuse new work without aborting work
/// already in flight. A pressure qualification is itself the pressure source,
/// so it necessarily trips that step. Applying the admission rule to
/// continuation makes the three-tier contract unreachable: the ramp can never
/// push the cascade far enough to fill the tiers if closing admission aborts
/// the ramp.
///
/// Escalation to `ControlState::Critical` and above starts destructive
/// actions (reduce VRAM cache, freeze discardable, request reclaim). That is
/// where a running pressure workload must stop.
#[derive(Clone, Copy)]
pub(super) enum PressureGate {
    /// Refuse a new pressure workload unless everything is fully healthy.
    Admission,
    /// Keep a running pressure workload alive while the supervisor is only
    /// closing admission; stop it once the supervisor escalates.
    Continuation,
}

impl PressureGate {
    /// Whether this gate treats the observed control state as a refusal.
    pub(super) fn control_state_refuses(self, state: ControlState) -> bool {
        match self {
            Self::Admission => state != ControlState::Healthy,
            Self::Continuation => matches!(
                state,
                ControlState::Critical | ControlState::Emergency | ControlState::SafeMode
            ),
        }
    }

    /// Whether this gate treats the observed cache state as a refusal.
    ///
    /// `Restricted` is the cache doing its job under load: the GPU sample says
    /// headroom is gone, so the physical target shrinks below what is already
    /// cached, and the cache stops growing and starts reclaiming. That is the
    /// product returning VRAM to a workload that asked for it. A pressure
    /// qualification is its own pressure source and trips this step the same
    /// way a game does, so treating the reaction as a fault makes the three-tier
    /// contract unreachable — the same conflation that was already removed for
    /// `ControlState::Guarded`. Only a cache that is not serving stops the run.
    pub(super) fn cache_state_refuses(self, state: CacheState) -> bool {
        match self {
            Self::Admission => state != CacheState::Active,
            Self::Continuation => matches!(
                state,
                CacheState::Off | CacheState::Unavailable | CacheState::Stuck
            ),
        }
    }
}

/// Named reasons a pressure gate refuses this snapshot.
///
/// Every predicate reports its own token. The previous shape printed only the
/// lifecycle reasons and the measurement errors, and both can be empty while a
/// structural or state predicate still refuses — which is the exact signature
/// that left a three-tier stress halt undiagnosable from its own artifact.
pub(super) fn pressure_refusals(
    snapshot: &CascadeSnapshot,
    gate: PressureGate,
) -> Vec<&'static str> {
    let lifecycle = derive_lifecycle(snapshot);
    let mut refusals: Vec<&'static str> = Vec::new();
    if !lifecycle.ok {
        refusals.push("lifecycle_not_ok");
    }
    if snapshot.ghost {
        refusals.push("ghost_device");
    }
    if !snapshot.order_ok {
        refusals.push("tier_order");
    }
    if !snapshot.zram.present {
        refusals.push("zram_missing");
    }
    if !snapshot.vram.present {
        refusals.push("vram_missing");
    }
    if !snapshot.disk.present {
        refusals.push("disk_missing");
    }
    if !snapshot.daemon_alive {
        refusals.push("daemon_dead");
    }
    match snapshot.capacity_guaranteed {
        Some(true) => {}
        Some(false) => refusals.push("capacity_not_guaranteed"),
        None => refusals.push("capacity_guarantee_unmeasured"),
    }
    if gate.control_state_refuses(snapshot.control_state) {
        refusals.push("control_state_refuses");
    }
    if snapshot.origin_state != OriginState::Ready {
        refusals.push("origin_not_ready");
    }
    if gate.cache_state_refuses(snapshot.cache_state) {
        refusals.push("cache_state_refuses");
    }
    if snapshot.guardian_state != GuardianState::Healthy {
        refusals.push("guardian_unhealthy");
    }
    if !snapshot.measurement_errors.is_empty() {
        refusals.push("measurement_errors");
    }
    refusals
}

pub(super) fn pressure_gate_from_snapshot(
    snapshot: &CascadeSnapshot,
    gate: PressureGate,
) -> Result<(), String> {
    let refusals = pressure_refusals(snapshot, gate);
    if refusals.is_empty() {
        return Ok(());
    }
    let lifecycle = derive_lifecycle(snapshot);
    Err(format!(
        "cascade stress requires healthy zram/NBD/disk, daemon, physical cache, supervisor, and host guardian (refused: {}; lifecycle reasons: {:?}; measurement errors: {:?}; control_state: {:?}; origin_state: {:?}; cache_state: {:?}; guardian_state: {:?})",
        refusals.join(", "),
        lifecycle.reasons,
        snapshot.measurement_errors,
        snapshot.control_state,
        snapshot.origin_state,
        snapshot.cache_state,
        snapshot.guardian_state,
    ))
}

pub(super) fn stress_readiness_from_snapshot(snapshot: &CascadeSnapshot) -> Result<(), String> {
    pressure_gate_from_snapshot(snapshot, PressureGate::Admission)
}

pub(super) fn stress_continuation_from_snapshot(snapshot: &CascadeSnapshot) -> Result<(), String> {
    pressure_gate_from_snapshot(snapshot, PressureGate::Continuation)
}

/// Read-only admission check for a new pressure workload.
pub fn stress_readiness() -> Result<(), String> {
    let entries = read_swaps().map_err(|error| error.to_string())?;
    stress_readiness_from_snapshot(&build_cascade_snapshot(&entries))
}

/// Read-only continuation check for a pressure workload already in flight.
///
/// See [`PressureGate`]: the supervisor closing admission is expected while
/// the workload is itself the pressure source, so only destructive escalation
/// or a degraded cascade aborts the run.
pub fn stress_continuation() -> Result<(), String> {
    let entries = read_swaps().map_err(|error| error.to_string())?;
    stress_continuation_from_snapshot(&build_cascade_snapshot(&entries))
}
