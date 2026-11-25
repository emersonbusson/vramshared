// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/lifecycle.rs
//! Cascade lifecycle phase derivation (SPEC cascade-lifecycle-observability).
//!
//! Pure: no swapoff, no probe, no CUDA. Inputs are snapshots; outputs are phase + reasons.

use std::env;

use ramshared_block::OriginCacheIo;
use ramshared_vram::{GpuBudgetTelemetry, GpuUtilizationTelemetry, WorkerCacheTelemetry};

use super::{SwapTierRole, swap_tier_role};
// Type only: the aggregate value itself is observed by `release_coherence`.
use crate::monitor::ZramTierState;
use crate::release_coherence::ReleaseCoherence;
use crate::zram_compression::ZramCompression;

/// Default active-use threshold (KiB). Residual nbd under this still counts as Armed.
pub const DEFAULT_ACTIVE_KIB: u64 = 1024;

/// Stable phase tags (JSON + logs). SPEC DT-1.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CascadePhase {
    Off,
    Armed,
    UsingZram,
    UsingVram,
    UsingOrigin,
    UsingTier,
    UsingDisk,
    Demoting,
    Degraded,
}

impl CascadePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            CascadePhase::Off => "Off",
            CascadePhase::Armed => "Armed",
            CascadePhase::UsingZram => "UsingZram",
            CascadePhase::UsingVram => "UsingVram",
            CascadePhase::UsingOrigin => "UsingOrigin",
            CascadePhase::UsingTier => "UsingTier",
            CascadePhase::UsingDisk => "UsingDisk",
            CascadePhase::Demoting => "Demoting",
            CascadePhase::Degraded => "Degraded",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProtectionState {
    Off,
    Ready,
    Active,
    AtRisk,
    Blocked,
}

impl ProtectionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "OFF",
            Self::Ready => "READY",
            Self::Active => "ACTIVE",
            Self::AtRisk => "AT_RISK",
            Self::Blocked => "BLOCKED",
        }
    }

    pub fn is_ok(self) -> bool {
        matches!(self, Self::Ready | Self::Active)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlState {
    Healthy,
    Guarded,
    Critical,
    Emergency,
    SafeMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OriginState {
    Off,
    Ready,
    Degraded,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheState {
    Off,
    Active,
    Restricted,
    Unavailable,
    Stuck,
}

/// The role a managed device plays in the cascade's data path.
///
/// The role is a **fact of the running system**, not a policy: the zram's
/// `backing_dev` names the device the zram writes its pages back to, and a
/// device that names itself there is a writeback target — it never enters the
/// kernel's swap table and the swap-tier rules do not apply to it. Reading
/// the role is what keeps a serving writeback topology from being judged by
/// rules written for a swap topology.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TierRole {
    /// The device is a swap device in the kernel's swap table.
    SwapDevice,
    /// The device is the zram's writeback target: the zram's pages are
    /// written back to it and it is not a swap device.
    WritebackTarget,
}

impl TierRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SwapDevice => "swap_device",
            Self::WritebackTarget => "writeback_target",
        }
    }
}

/// The topology facts the health rules and the face read.
///
/// Every field is a measurement of the running system: the zram's bound
/// backing device and the managed tier device come from the kernel's own
/// attributes and the product's own runtime record. Nothing here is inferred
/// from the shape of the swap table alone.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TopologyFacts {
    /// The zram's bound backing device (`backing_dev`), when it is bound.
    /// `None` means the attribute never answered or named no device.
    pub zram_backing_dev: Option<String>,
    /// The managed tier device the product attached (the NBD export).
    pub tier_device: Option<String>,
    /// The zram's backing device **is** the managed tier: the tier is the
    /// zram's writeback target and never a swap device.
    pub writeback_target_is_tier: bool,
}

impl TopologyFacts {
    /// The role the managed tier plays.
    ///
    /// A tier that is the zram's writeback target is a writeback target; any
    /// other tier the product manages is a swap device.
    pub fn tier_role(&self) -> TierRole {
        if self.writeback_target_is_tier {
            TierRole::WritebackTarget
        } else {
            TierRole::SwapDevice
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianState {
    Healthy,
    SafeMode,
    Blocked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OverallState {
    Healthy,
    Guarded,
    Critical,
    Blocked,
    SafeMode,
    Emergency,
}

macro_rules! state_text {
    ($type:ty, {$($variant:ident => $text:literal),+ $(,)?}) => {
        impl $type {
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }
        }
    };
}

state_text!(ControlState, {
    Healthy => "HEALTHY", Guarded => "GUARDED", Critical => "CRITICAL",
    Emergency => "EMERGENCY", SafeMode => "SAFE_MODE"
});
state_text!(OriginState, {
    Off => "OFF", Ready => "READY", Degraded => "DEGRADED", Failed => "FAILED"
});
state_text!(CacheState, {
    Off => "OFF", Active => "ACTIVE", Restricted => "RESTRICTED",
    Unavailable => "UNAVAILABLE", Stuck => "STUCK"
});
state_text!(GuardianState, {
    Healthy => "HEALTHY", SafeMode => "SAFE_MODE", Blocked => "BLOCKED"
});
state_text!(OverallState, {
    Healthy => "HEALTHY", Guarded => "GUARDED", Critical => "CRITICAL",
    Blocked => "BLOCKED", SafeMode => "SAFE_MODE", Emergency => "EMERGENCY"
});

impl OverallState {
    pub fn is_ok(self) -> bool {
        self == Self::Healthy
    }

    fn rank(self) -> u8 {
        match self {
            Self::Healthy => 0,
            Self::Guarded => 1,
            Self::Critical => 2,
            Self::Blocked => 3,
            Self::SafeMode => 4,
            Self::Emergency => 5,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct TierSample {
    pub present: bool,
    pub prio: Option<i32>,
    pub size_kib: u64,
    pub used_kib: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct DemoteSnapshot {
    pub total: Option<u64>,
    pub last_reason: Option<String>,
    /// Measured demotion-in-progress flag from the daemon status file.
    /// `Some(true)` = measured positive, `Some(false)` = measured refutation,
    /// `None` = never taken (status unreadable, body corrupt, or key absent).
    pub in_progress: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CascadeSnapshot {
    pub zram: TierSample,
    pub vram: TierSample,
    /// Whether the capacity marker identifies the NBD tier as origin-backed.
    /// `None` means the marker did not provide a recognized mode.
    pub origin_cache_mode: Option<bool>,
    pub disk: TierSample,
    pub ghost: bool,
    pub order_ok: bool,
    pub daemon_alive: bool,
    pub daemon_pid: Option<u32>,
    /// Capacity-marker witness. `Some(true)` measured a regular file at the
    /// marker path; `Some(false)` measured its absence (or a non-file there);
    /// `None` never took the stat. `None` is neither a backing nor a
    /// refutation and must never publish as `capacity_not_guaranteed`.
    pub capacity_guaranteed: Option<bool>,
    /// Disk swap pages already present when the current RamShared activation began.
    /// `None` means no attributable product activation baseline exists.
    pub disk_baseline_kib: Option<u64>,
    pub demote: DemoteSnapshot,
    pub active_kib: u64,
    pub control_state: ControlState,
    pub origin_state: OriginState,
    pub cache_state: CacheState,
    pub guardian_state: GuardianState,
    /// The worker's admitted physical VRAM cache target. `None` means no
    /// current daemon document carried a target; `Some(0)` is the measured
    /// target of a cache the worker deliberately left off.
    pub cache_target_kib: Option<u64>,
    pub logical_capacity_kib: Option<u64>,
    pub vram_cached_kib: Option<u64>,
    pub gpu_headroom_kib: Option<u64>,
    pub gpu_budget: Option<GpuBudgetTelemetry>,
    /// Adapter busy time over the driver's trailing sample window. `None` is
    /// "not measured" — never a zero-percent idle sample the driver did not
    /// report. This is busy **time**, not VRAM capacity, and never shares a
    /// meter with `vram_cached_kib` or `gpu_headroom_kib`.
    pub gpu_utilization: Option<GpuUtilizationTelemetry>,
    /// Cache occupancy and codec health (DT-8). Logical cache bytes are cache
    /// occupancy in the worker's address space, never guest or host RAM.
    /// `None` is "no cache telemetry", never a measured empty cache.
    pub gpu_cache: Option<WorkerCacheTelemetry>,
    /// Origin-plane I/O accounting published beside the occupancy sample.
    /// `None` is "no admitted daemon document carried the field" — no document,
    /// a document refused at the control-plane seam, or an older producer —
    /// and never a zero-filled accounting. `valid_blocks` inside it is a gauge,
    /// not a counter.
    pub origin_cache_io: Option<OriginCacheIo>,
    /// Cache reads that missed and fell through to the authoritative origin.
    ///
    /// The daemon publishes this beside the origin-plane accounting, not inside
    /// it: [`OriginCacheIo`] deliberately excludes the document-level cache
    /// counters so a reader cannot mistake them for members of that accounting.
    /// It rides here under its own name for the same reason and is never folded
    /// into the `origin_cache_io` object.
    ///
    /// `None` is "no admitted daemon document carried a measured value" — no
    /// document, a document refused at the control-plane seam, or an older
    /// producer that never published the key — and never a zero count. A
    /// present `0` is a real zero. See `cascade::cache_event_counters` for why
    /// a refused document blanks these with every other key rather than
    /// surviving it.
    pub cache_fallback_reads: Option<u64>,
    /// Successful cache-hit reads, paired with `cache_fallback_reads` to derive
    /// the hit rate. `None` is an older producer, never a fabricated zero.
    pub cache_hits: Option<u64>,
    /// Last host-memory sample delivered on the authenticated heartbeat. This
    /// is host physical/commit statistics, not guest PSI; `None` is "the host
    /// did not publish one", never a zero.
    pub host_memory: Option<ramshared_ipc::HostMemoryTelemetry>,
    /// Last authenticated Windows host NTFS volume headroom observation for
    /// this WSL2 guest's root VHD (IPC v5). `None` when unverified or off-WSL2.
    pub host_storage: Option<ramshared_ipc::storage_proof::HostStorageTelemetry>,
    /// Cache entries revoked by an invalidation the daemon accepted. Same
    /// absence rule and same top-level placement as [`Self::cache_fallback_reads`].
    pub cache_invalidations: Option<u64>,
    /// Cache entries the daemon released by an accepted release. Same absence
    /// rule and same top-level placement as [`Self::cache_fallback_reads`].
    pub cache_releases: Option<u64>,
    /// How well the compressed host-RAM tier is compressing right now, from
    /// the kernel's `mm_stat`. `None` when the tier has no device or its
    /// counters are gone — which is what a teardown leaves behind, and must
    /// never be published as a zero measurement.
    pub zram_compression: Option<ZramCompression>,
    /// Tier 1's state, derived once in `monitor::observations` from the
    /// zram sysfs `backing_dev` attribute plus the swap table (SPEC DT-3).
    /// Both the dashboard row and the published document read this field;
    /// neither re-derives it.
    pub zram_tier_state: ZramTierState,
    pub ssd_origin_written_kib: Option<u64>,
    /// Last-resort SSD fallback swap occupancy (the benchmark taxonomy's
    /// `tier3_ssd_mb`) — not origin write-through and not the logical device's
    /// occupancy. `Some(used)` only when the fallback tier is listed in the
    /// swap table; `None` when it is not. A measured empty fallback is
    /// `Some(0)` and is a different fact from an unlisted tier.
    pub fallback_swap_used_kib: Option<u64>,
    /// Are the product processes running the selected release? DT-6 refuses
    /// `HEALTHY` while this is not `Coherent`.
    pub release_coherence: ReleaseCoherence,
    pub measurement_errors: Vec<String>,
    /// The cascade's topology facts, measured from the running system.
    pub topology: TopologyFacts,
    /// Why the origin cache is not serving, when the daemon published a
    /// reason. `None` is "no reason published" — the daemon said nothing
    /// about the cache's state, which is not the same news as a published
    /// cause. A published reason makes an unexplained cache absence a
    /// declared capability gap instead of a protection failure.
    pub cache_unavailable_reason: Option<String>,
    /// The control-plane authority's own state, as the daemon published it.
    ///
    /// The cache's state says whether the cache serves; this says why the
    /// authority admits it or not: the peer has not arrived, a lease is live,
    /// the lease was lost, or the transport failed. `None` is "the daemon
    /// published nothing", never a fabricated state.
    pub control_plane_state: Option<String>,
    /// The lease deadline the authority holds, in Unix milliseconds. `None` is
    /// "no lease", never a zero deadline.
    pub control_plane_lease_deadline_ms: Option<u64>,
}

pub fn disk_growth_kib(s: &CascadeSnapshot) -> Option<u64> {
    s.disk_baseline_kib
        .map(|baseline| s.disk.used_kib.saturating_sub(baseline))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LifecycleView {
    pub phase: CascadePhase,
    pub phase_reason: &'static str,
    pub ok: bool,
    pub reasons: Vec<String>,
}

pub fn overall_state(view: &LifecycleView, snap: &CascadeSnapshot) -> OverallState {
    let product_active = snap.daemon_alive || snap.vram.present;
    let mut states = vec![if view.ok {
        OverallState::Healthy
    } else {
        OverallState::Blocked
    }];
    states.push(match snap.control_state {
        ControlState::Healthy => OverallState::Healthy,
        ControlState::Guarded => OverallState::Guarded,
        ControlState::Critical => OverallState::Critical,
        ControlState::Emergency => OverallState::Emergency,
        ControlState::SafeMode => OverallState::SafeMode,
    });
    states.push(match snap.guardian_state {
        GuardianState::Healthy => OverallState::Healthy,
        GuardianState::SafeMode => OverallState::SafeMode,
        GuardianState::Blocked => OverallState::Blocked,
    });
    states.push(match snap.origin_state {
        OriginState::Off if !product_active => OverallState::Healthy,
        OriginState::Ready => OverallState::Healthy,
        OriginState::Degraded => OverallState::Critical,
        OriginState::Failed | OriginState::Off => OverallState::Blocked,
    });
    states.push(match snap.cache_state {
        CacheState::Off if !product_active => OverallState::Healthy,
        CacheState::Active => OverallState::Healthy,
        CacheState::Restricted => OverallState::Guarded,
        // An explained cache outage is a degraded capability, while the
        // independent protection state continues to report the active tiers.
        CacheState::Unavailable | CacheState::Stuck | CacheState::Off
            if snap.cache_unavailable_reason.is_some() =>
        {
            OverallState::Guarded
        }
        CacheState::Unavailable | CacheState::Stuck | CacheState::Off => OverallState::Blocked,
    });
    if !snap.measurement_errors.is_empty() {
        states.push(OverallState::Blocked);
    }
    // DT-6: a stale supervisor is enforcing a different contract than the
    // installed one, so `HEALTHY` next to it is the defect. `Stale` is proven
    // wrong bytes and is `Critical`; `Unknown` is an observation that could not
    // be made and is still not `HEALTHY`, but it is not the same news.
    match snap.release_coherence {
        ReleaseCoherence::Coherent => {}
        ReleaseCoherence::Stale => states.push(OverallState::Critical),
        ReleaseCoherence::Unknown => states.push(OverallState::Guarded),
    }
    states
        .into_iter()
        .max_by_key(|state| state.rank())
        .unwrap_or(OverallState::Blocked)
}

/// The single `ok` verdict both status surfaces report.
///
/// The human text and `status --json` used to compute this independently, and
/// the human line stopped at `view.ok && protection.is_ok()`. A guarded control
/// plane or a released cache therefore printed `ok: true` while the JSON — the
/// one the monitor reads — printed `ok: false`. One function, called from both
/// renderers, is what keeps the two from drifting apart again.
pub fn status_ok(view: &LifecycleView, snap: &CascadeSnapshot) -> bool {
    view.ok && protection_state(view, snap).is_ok() && overall_state(view, snap).is_ok()
}

/// Parse `RAMSHARED_STATUS_ACTIVE_KIB`; invalid or missing → default.
pub fn active_threshold_kib_from_env() -> u64 {
    match env::var("RAMSHARED_STATUS_ACTIVE_KIB") {
        Ok(s) => s.trim().parse::<u64>().unwrap_or(DEFAULT_ACTIVE_KIB),
        Err(_) => DEFAULT_ACTIVE_KIB,
    }
}

/// SPEC DT-2: first match wins.
/// Whether the product's VRAM tier is in place, in either role.
///
/// The tier is in place when it is a swap device in the kernel's swap table
/// **or** when it is the zram's writeback target (SPEC DT-1): the writeback
/// topology never puts it in the swap table, and a cascade that is serving
/// through it is armed.
fn cascade_tier_present(s: &CascadeSnapshot) -> bool {
    s.vram.present || s.topology.writeback_target_is_tier
}

pub fn derive_lifecycle(s: &CascadeSnapshot) -> LifecycleView {
    let thr = if s.active_kib == 0 {
        DEFAULT_ACTIVE_KIB
    } else {
        s.active_kib
    };
    let mut reasons: Vec<String> = Vec::new();

    if s.ghost {
        reasons.push("ghost".into());
    }
    if !s.order_ok {
        reasons.push("priority_order_bad".into());
    }
    let daemon_identity_unreadable = s.vram.present
        && s.measurement_errors
            .iter()
            .any(|error| error == "daemon_identity_unreadable");
    if daemon_identity_unreadable {
        reasons.push("daemon_identity_unreadable".into());
    }
    let hot_vram_no_daemon =
        s.vram.present && !s.daemon_alive && !daemon_identity_unreadable && s.vram.used_kib >= thr;
    if hot_vram_no_daemon {
        reasons.push("daemon_dead_hot_vram".into());
    }
    let vram_present_no_daemon =
        s.vram.present && !s.daemon_alive && !daemon_identity_unreadable && s.vram.used_kib < thr;
    // Half-state: vram swapon without daemon even if used low (degraded safety).
    if vram_present_no_daemon {
        reasons.push("vram_tier_without_daemon".into());
    }

    let degraded = s.ghost
        || !s.order_ok
        || daemon_identity_unreadable
        || hot_vram_no_daemon
        || vram_present_no_daemon;
    if degraded {
        return LifecycleView {
            phase: CascadePhase::Degraded,
            phase_reason: if s.ghost {
                "ghost"
            } else if !s.order_ok {
                "priority_order_bad"
            } else if daemon_identity_unreadable {
                "daemon_identity_unreadable"
            } else if hot_vram_no_daemon {
                "daemon_dead_hot_vram"
            } else {
                "vram_tier_without_daemon"
            },
            ok: false,
            reasons,
        };
    }

    if s.demote.in_progress == Some(true) {
        return LifecycleView {
            phase: CascadePhase::Demoting,
            phase_reason: "demote_in_progress",
            ok: true,
            reasons,
        };
    }

    if s.disk.present && disk_growth_kib(s).is_some_and(|growth| growth >= thr) {
        return LifecycleView {
            phase: CascadePhase::UsingDisk,
            phase_reason: "disk_growth_ge_threshold",
            ok: true,
            reasons,
        };
    }
    if s.vram.present && s.vram.used_kib >= thr {
        if s.origin_cache_mode == Some(true) {
            return LifecycleView {
                phase: CascadePhase::UsingOrigin,
                phase_reason: "origin_swap_used",
                ok: true,
                reasons,
            };
        }
        if s.origin_cache_mode.is_none() {
            return LifecycleView {
                phase: CascadePhase::UsingTier,
                phase_reason: "tier_swap_backing_unverified",
                ok: true,
                reasons,
            };
        }
        return LifecycleView {
            phase: CascadePhase::UsingVram,
            phase_reason: "vram_used_ge_threshold",
            ok: true,
            reasons,
        };
    }
    if s.zram.present && s.zram.used_kib >= thr {
        return LifecycleView {
            phase: CascadePhase::UsingZram,
            phase_reason: "zram_used_ge_threshold",
            ok: true,
            reasons,
        };
    }
    // The origin cache can be serving VRAM even when the VRAM tier is not a
    // swap device (the writeback topology) and no swap tier has crossed the
    // usage threshold yet. The cache worker is the VRAM consumer; an active
    // cache must not publish as `Off`.
    if s.daemon_alive && !s.vram.present && s.cache_state == CacheState::Active {
        return LifecycleView {
            phase: CascadePhase::UsingVram,
            phase_reason: "origin_cache_active",
            ok: true,
            reasons,
        };
    }
    // SPEC DT-1: "armed" is the product's tiers being in place with the
    // daemon serving them. In the writeback topology the VRAM tier is not a
    // swap device — it is the zram's writeback target — so demanding it in
    // the swap table would report a live, serving cascade as **off**.
    if s.daemon_alive && cascade_tier_present(s) && s.order_ok && !s.ghost {
        return LifecycleView {
            phase: CascadePhase::Armed,
            phase_reason: if s.origin_cache_mode == Some(false) {
                "armed_low_vram_used"
            } else {
                "armed_low_tier_used"
            },
            ok: true,
            reasons,
        };
    }

    LifecycleView {
        phase: CascadePhase::Off,
        phase_reason: "no_product_cascade",
        ok: true,
        reasons,
    }
}

/// Build tier samples + order_ok from swap lines.
///
/// Tier membership is decided by [`swap_tier_role`] — the same role definition
/// the monitor's per-tier I/O and the fallback device-name set use. The
/// logical VRAM device is tier 2, not tier 3.
pub fn tiers_from_swap_names(
    entries: &[(&str, u64, u64, i32)],
) -> (TierSample, TierSample, TierSample, bool) {
    let mut zram = TierSample::default();
    let mut vram = TierSample::default();
    let mut disk = TierSample::default();

    for (name, size, used, prio) in entries {
        let target = match swap_tier_role(name) {
            SwapTierRole::Zram => &mut zram,
            SwapTierRole::Vram => &mut vram,
            SwapTierRole::Fallback => &mut disk,
        };
        merge_tier(target, *size, *used, *prio);
    }

    let order_ok = compute_order_ok(&zram, &vram, &disk);
    (zram, vram, disk, order_ok)
}

fn merge_tier(t: &mut TierSample, size: u64, used: u64, prio: i32) {
    t.present = true;
    t.size_kib = t.size_kib.saturating_add(size);
    t.used_kib = t.used_kib.saturating_add(used);
    t.prio = Some(match t.prio {
        Some(p) => p.max(prio),
        None => prio,
    });
}

fn compute_order_ok(zram: &TierSample, vram: &TierSample, disk: &TierSample) -> bool {
    if zram.present && vram.present {
        match (zram.prio, vram.prio) {
            (Some(pz), Some(pv)) if pz <= pv => return false,
            _ => {}
        }
    }
    if vram.present && disk.present {
        match (vram.prio, disk.prio) {
            (Some(pv), Some(pd)) if pv <= pd => return false,
            _ => {}
        }
    }
    true
}

mod render;
pub use render::render_status_json;
#[cfg(test)]
use render::{json_escape, tier_json, zram_tier_json};

/// Last-resort SSD fallback swap occupancy.
///
/// `Some(used)` only when the fallback tier is listed in the swap table. An
/// unlisted tier publishes `None`, never `Some(0)`: `0` is the measured
/// occupancy of a present tier holding nothing. This is the benchmark
/// taxonomy's `tier3_ssd_mb` — fallback swap occupancy, not origin
/// write-through and not the logical device's occupancy.
pub fn fallback_swap_occupancy_kib(disk: &TierSample) -> Option<u64> {
    disk.present.then_some(disk.used_kib)
}

/// Tier 1 fill percentage, rounded to nearest whole percent.
///
/// `None` when the tier has no device, or a device with no capacity: there is
/// no percentage to report, and `0` would claim a measured empty tier.
/// `Some(0)` is that measurement — a present device holding nothing.
pub fn zram_utilization_pct(snap: &CascadeSnapshot) -> Option<u64> {
    if !snap.zram.present || snap.zram.size_kib == 0 {
        return None;
    }
    let pct = snap
        .zram
        .used_kib
        .saturating_mul(100)
        .saturating_add(snap.zram.size_kib / 2)
        / snap.zram.size_kib;
    Some(pct.min(100))
}

pub fn protection_state(view: &LifecycleView, snap: &CascadeSnapshot) -> ProtectionState {
    if !view.ok || view.phase == CascadePhase::Degraded {
        return ProtectionState::Blocked;
    }
    // Fail-closed policy, not a measurement word: a guarantee this frame
    // never took still refuses a positive protection claim. The *reason*
    // string below owns the distinction between a measured refutation and
    // an absence — two facts, two words.
    if snap.vram.present && snap.capacity_guaranteed != Some(true) {
        return ProtectionState::Blocked;
    }
    if snap.daemon_alive && snap.vram.present && snap.disk_baseline_kib.is_none() {
        return ProtectionState::Blocked;
    }
    match view.phase {
        CascadePhase::Off => ProtectionState::Off,
        CascadePhase::UsingVram if view.phase_reason == "origin_cache_active" => {
            ProtectionState::Active
        }
        CascadePhase::UsingVram => ProtectionState::Active,
        CascadePhase::UsingOrigin => ProtectionState::Active,
        CascadePhase::UsingTier => ProtectionState::Active,
        CascadePhase::UsingDisk | CascadePhase::Demoting => ProtectionState::AtRisk,
        // DT-1: the "zram is ≥80% full" arm is a **swap-tier** rule — it asks
        // whether the VRAM swap tier is being left idle while the zram fills.
        // In the writeback topology the tier is not a swap device at all: it
        // is the zram's writeback target, so "no VRAM use" is a statement
        // about a device that is not in the swap table and cannot be idle
        // while the zram spills into it. The rule therefore asks the
        // topology's own question instead: a zram with a bound writeback
        // target has somewhere to spill, and a zram without one is a dead
        // end whatever the tier table says.
        CascadePhase::UsingZram if snap.topology.writeback_target_is_tier => {
            if snap.topology.zram_backing_dev.is_some() {
                ProtectionState::Ready
            } else {
                ProtectionState::AtRisk
            }
        }
        CascadePhase::UsingZram
            if snap.vram.used_kib < snap.active_kib
                && zram_utilization_pct(snap).is_some_and(|pct| pct >= 80) =>
        {
            ProtectionState::AtRisk
        }
        CascadePhase::Armed | CascadePhase::UsingZram => ProtectionState::Ready,
        CascadePhase::Degraded => ProtectionState::Blocked,
    }
}

pub fn protection_reason(view: &LifecycleView, snap: &CascadeSnapshot) -> &'static str {
    if !view.ok || view.phase == CascadePhase::Degraded {
        return view.phase_reason;
    }
    // Two facts, two words. `capacity_not_guaranteed` is the measured
    // refutation — the marker was looked for and is not a regular file.
    // `capacity_guarantee_unmeasured` is a stat that never arrived. Neither
    // borrows the other's arm: a frame that could not look has not refuted
    // the guarantee.
    if snap.vram.present && snap.capacity_guaranteed == Some(false) {
        return "capacity_not_guaranteed";
    }
    if snap.vram.present && snap.capacity_guaranteed.is_none() {
        return "capacity_guarantee_unmeasured";
    }
    if snap.daemon_alive && snap.vram.present && snap.disk_baseline_kib.is_none() {
        return "disk_baseline_unavailable";
    }
    match view.phase {
        CascadePhase::Off => "product_cascade_off",
        CascadePhase::Armed => match snap.origin_cache_mode {
            Some(true) => "origin_tier_armed",
            Some(false) => "guaranteed_vram_tier_armed",
            None => "tier_backing_unverified",
        },
        CascadePhase::UsingVram if view.phase_reason == "origin_cache_active" => {
            "origin_cache_active"
        }
        CascadePhase::UsingVram => "guaranteed_vram_tier_active",
        CascadePhase::UsingOrigin => "origin_swap_in_use",
        CascadePhase::UsingTier => "tier_swap_backing_unverified",
        CascadePhase::UsingDisk => "disk_tier_in_use",
        CascadePhase::Demoting => "vram_demotion_in_progress",
        // DT-1: the writeback topology has its own reason, and it names what
        // is true: the tier is the zram's writeback target, so the swap-tier
        // rule never applied to it.
        CascadePhase::UsingZram if snap.topology.writeback_target_is_tier => {
            if snap.topology.zram_backing_dev.is_some() {
                "zram_writeback_target_ready"
            } else {
                "zram_utilization_ge_80_without_spill_target"
            }
        }
        CascadePhase::UsingZram if zram_utilization_pct(snap).is_some_and(|pct| pct >= 80) => {
            if snap.origin_cache_mode == Some(false) {
                "zram_utilization_ge_80_without_vram_use"
            } else {
                "zram_utilization_ge_80_without_tier_use"
            }
        }
        CascadePhase::UsingZram => match snap.origin_cache_mode {
            Some(true) => "origin_tier_ready",
            Some(false) => "guaranteed_vram_tier_ready",
            None => "tier_backing_unverified",
        },
        CascadePhase::Degraded => view.phase_reason,
    }
}

#[cfg(test)]
mod tests;
