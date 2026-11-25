// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/stress.rs
//! Native Rust Memory & Swap Stress Governor & Qualification Battery for RamShared.
//!
//! Provides deterministic, GC-free, microsecond-accurate 1%-by-1%
//! memory escalation with closed-loop safety floor and latency probing.
//!
//! Features:
//! 1. Telemetry Gauge & Pressure Level Logger.
//! 2. Multi-Phase Qualification Battery (--battery):
//!    - Phase 1: Gradual 1%-by-1% Micro-Step Ramp.
//!    - Phase 2: Tier Waterfall Overflow (RAM -> ZRAM -> GPU VRAM).
//!    - Phase 3: Active Page Swapper & Cycler (animates live TUI speedometers).
//!    - Phase 4: Flash-Reclaim & Atomic Deallocation Benchmark.
//! 3. Autonomous Watchdog Daemon Thread for fail-closed Hyper-V / WSL2 anti-hang protection.

#[cfg(test)]
use crate::zram_compression::ZramCompression;
#[cfg(test)]
use std::fs;
#[cfg(test)]
use std::time::{SystemTime, UNIX_EPOCH};

mod classify;

pub const WSL2_MIN_PHYSICAL_HEADROOM_MB: u64 = 600;
pub const MIN_ORDER_7_BUDDY_CHUNKS: u64 = 8;
pub const PROACTIVE_COMPACTION_TRIGGER_CHUNKS: u64 = 16;
const BARE_METAL_MULTI_TIER_FLOOR_MB: u64 = 256;
/// Policy floors and step caps that are compared against a measured KiB figure
/// are stored in KiB, each a whole-MiB policy times `1024`. A name in `_MB`
/// beside a KiB comparison invites the reader to convert one side with a
/// rounding of its own — the mistake [`TIER3_ACTIVITY_MIN_GROWTH_KIB`] and
/// [`MemInfoKib`] exist to make hard. The conversion here is exact, so nothing
/// is lost; only the second rounding disappears.
const MULTI_TIER_MIN_USABLE_AVAIL_KIB: u64 = 100 * 1024;
const SINGLE_TIER_MIN_USABLE_AVAIL_KIB: u64 = 200 * 1024;
const TIER1_AND_TIER2_QUALIFICATION_PCT: u64 = 95;
const TIER3_HEADROOM_RESERVE_KIB: u64 = 16 * 1024;
const TIER3_MAX_STEP_KIB: u64 = 8 * 1024;
/// Tier-3 growth that counts as "the run used the fallback", in the unit the
/// `/proc/swaps` fold holds. The policy is 32 MiB; it is stored in KiB so the
/// comparison beside it never converts a rounded half back into the unit it
/// came from.
const TIER3_ACTIVITY_MIN_GROWTH_KIB: u64 = 32 * 1024;
const PRE_TIER3_FULL_STEP_HEADROOM_KIB: u64 = 200 * 1024;
const PRE_TIER3_HEADROOM_RESERVE_KIB: u64 = 50 * 1024;
const PRE_TIER3_MAX_STEP_KIB: u64 = 128 * 1024;
/// Smallest step a ramp will take. A whole-MiB policy, in the unit the step is
/// budgeted in — the ramp never nudges by less than this, however little is
/// left above the floor.
const MIN_ALLOCATION_STEP_KIB: u64 = 50 * 1024;
/// Single-tier step ceiling. Same number as [`PRE_TIER3_MAX_STEP_KIB`] and a
/// different policy: one caps a ramp with no fallback tier to protect, the
/// other caps a ramp that still has one. They are named apart so neither can be
/// edited thinking it is the other.
const SINGLE_TIER_MAX_STEP_KIB: u64 = 128 * 1024;
const WSL2_TIER3_STEP_INTERVAL_MS: u64 = 500;
const TIER3_HEAVY_LOAD_PCT: u64 = 80;
const TIER3_HEAVY_HOLD_INTERVAL_MS: u64 = 1_000;
const NORMAL_HOLD_INTERVAL_MS: u64 = 500;
const TIER3_HEAVY_TOUCH_BYTES: usize = 2 * 1024 * 1024;
const NORMAL_TOUCH_BYTES: usize = 16 * 1024 * 1024;
const TIER3_HEAVY_TOUCH_STRIDE_BYTES: usize = 32 * 1024;
const NORMAL_TOUCH_STRIDE_BYTES: usize = 16 * 1024;
const CASCADE_RAMP_LIMIT_PCT: u64 = 1_000;
const TIER3_QUALIFICATION_RAMP_LIMIT_PCT: u64 = 6_000;
#[derive(Clone, Debug, PartialEq)]
pub struct StressOptions {
    pub start_pct: u64,
    pub target_pct: u64,
    pub step_pct: u64,
    pub interval_ms: u64,
    pub hold_sec: u64,
    pub duration_sec: Option<u64>,
    pub min_ram_mb: u64,
    pub max_psi_full: f64,
    pub max_latency_ms: f64,
    pub tier1_target_pct: u64,
    pub tier2_target_pct: u64,
    pub tier3_target_pct: Option<u64>,
    pub physical_cache_target_mib: Option<u64>,
    pub full_three_tier_profile: bool,
    pub tier3_only: bool,
    pub battery: bool,
    pub cascade: bool,
    pub telemetry_log: String,
    pub json: bool,
    pub threads: u64,
    pub min_order_7_chunks: u64,
}

impl Default for StressOptions {
    fn default() -> Self {
        Self {
            start_pct: 1,
            target_pct: 90,
            step_pct: 1,
            interval_ms: 1500,
            hold_sec: 10,
            duration_sec: None,
            min_ram_mb: 600,
            max_psi_full: 20.0,
            max_latency_ms: 8.0,
            tier1_target_pct: TIER1_AND_TIER2_QUALIFICATION_PCT,
            tier2_target_pct: TIER1_AND_TIER2_QUALIFICATION_PCT,
            tier3_target_pct: None,
            physical_cache_target_mib: None,
            full_three_tier_profile: false,
            tier3_only: false,
            battery: false,
            cascade: false,
            telemetry_log: "/tmp/ramshared-stress-telemetry.log".to_string(),
            json: false,
            threads: 1,
            min_order_7_chunks: MIN_ORDER_7_BUDDY_CHUNKS,
        }
    }
}

/// One tier's swap-device capacity and occupancy, held in the unit
/// `/proc/swaps` publishes: **KiB**.
///
/// Every comparison and every percentage is taken on these two figures. When
/// something on screen or on the wire names MiB, it goes through
/// [`published_mib`] — never through a second rounding of its own. A floor
/// division at construction is what collapses a real 511 KiB occupancy into a
/// measured `0 MiB`, and it also moves the percentage off the true ratio
/// (1536 / 2048 KiB is 75%, and its rounded MiB halves say 50%).
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize)]
pub struct TierCapacityStats {
    pub total_kib: u64,
    pub used_kib: u64,
}

impl TierCapacityStats {
    /// Occupancy percent, from the exact KiB figures, or its refusal.
    ///
    /// Never from a whole-MiB view: rounding first would move the number
    /// off the ratio `/proc/swaps` reported.
    ///
    /// A tier with no swap device has no denominator, and that is a refusal,
    /// not a `0` — the same shape [`cache_fill_pct`] publishes for a cache
    /// with no target. `None` is "there is no such tier"; an empty tier is
    /// `Some(0)`, a measured zero of a real denominator. Collapsing the two
    /// published a `0%` that claimed an emptiness measured against a capacity
    /// that was never there.
    pub fn pct(&self) -> Option<u64> {
        if self.total_kib == 0 {
            return None;
        }
        Some(self.used_kib.saturating_mul(100) / self.total_kib)
    }
}

/// Per-tier used swap from one `/proc/swaps` read, held in KiB.
///
/// A named struct rather than a 4-tuple of `u64`, because the tuple's four
/// identical slots gave the reader no way to tell which number was in which
/// unit — which is how a rounded MiB total sat in the same destructuring as a
/// raw KiB figure for long enough to hide the collapse.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SwapTiersKib {
    pub zram_kib: u64,
    pub vram_kib: u64,
    pub ssd_kib: u64,
}

impl SwapTiersKib {
    /// Total across the three roles. Derived, so it can never disagree with
    /// the members — the old fold summed three rounded halves, which is not
    /// the rounding of their sum (512+512+512 KiB is 2 MiB, not 3).
    pub fn total_kib(&self) -> u64 {
        self.zram_kib + self.vram_kib + self.ssd_kib
    }

    /// Whole-MiB publish conversion for a report that names MiB — see
    /// [`published_mib`]. A **view** — never a comparison input.
    pub fn total_mb(&self) -> u64 {
        published_mib(self.total_kib())
    }

    /// See [`Self::total_mb`].
    pub fn zram_mb(&self) -> u64 {
        published_mib(self.zram_kib)
    }

    /// See [`Self::total_mb`].
    pub fn vram_mb(&self) -> u64 {
        published_mib(self.vram_kib)
    }

    /// See [`Self::total_mb`].
    pub fn ssd_mb(&self) -> u64 {
        published_mib(self.ssd_kib)
    }
}

/// One `/proc/meminfo` read, held in the unit the file publishes: **KiB**.
///
/// The same discipline as [`SwapTiersKib`]. These two figures decide whether a
/// pressure ramp may allocate at all (`avail <= hard_floor`) and how much it
/// may take, so the comparison runs on what `/proc/meminfo` said. A whole-MiB
/// view taken at the fold is both a rounding and a direction: round-to-nearest
/// can overstate availability by half a megabyte (1537 KiB reads as `2 MiB`),
/// which is exactly the side that would leave the drain loop early.
///
/// Figures that name MiB on screen or on the wire go through [`published_mib`]
/// at that boundary, and nowhere else.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemInfoKib {
    pub total_kib: u64,
    pub avail_kib: u64,
}

// The typed result and report model lives in `stress/model.rs` — the
// registered `stress/model.rs` extraction slice. Re-exported here so the
// callers of this module keep one path to the published shapes.
mod model;
pub use model::{StressReport, TelemetryReading};

pub fn parse_stress_args(args: &[String]) -> Result<StressOptions, String> {
    let mut opts = StressOptions::default();
    let mut target_explicit = false;
    let mut cascade_explicit = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--start" => {
                i += 1;
                opts.start_pct = args
                    .get(i)
                    .ok_or_else(|| "--start requires a value (1-99)".to_string())?
                    .parse()
                    .map_err(|_| "invalid --start value")?;
            }
            "--target" => {
                i += 1;
                opts.target_pct = args
                    .get(i)
                    .ok_or_else(|| "--target requires a value (1-99)".to_string())?
                    .parse()
                    .map_err(|_| "invalid --target value")?;
                target_explicit = true;
            }
            "--step" => {
                i += 1;
                opts.step_pct = args
                    .get(i)
                    .ok_or_else(|| "--step requires a value".to_string())?
                    .parse()
                    .map_err(|_| "invalid --step value")?;
            }
            "--interval-ms" => {
                i += 1;
                opts.interval_ms = args
                    .get(i)
                    .ok_or_else(|| "--interval-ms requires a value".to_string())?
                    .parse()
                    .map_err(|_| "invalid --interval-ms value")?;
            }
            "--hold-sec" => {
                i += 1;
                opts.hold_sec = args
                    .get(i)
                    .ok_or_else(|| "--hold-sec requires a value".to_string())?
                    .parse()
                    .map_err(|_| "invalid --hold-sec value")?;
            }
            "--duration-sec" => {
                i += 1;
                let secs: u64 = args
                    .get(i)
                    .ok_or_else(|| "--duration-sec requires a value".to_string())?
                    .parse()
                    .map_err(|_| "invalid --duration-sec value")?;
                if !(1..=86_400).contains(&secs) {
                    return Err("--duration-sec must be within 1-86400 seconds".into());
                }
                opts.duration_sec = Some(secs);
            }
            "--min-ram-mb" => {
                i += 1;
                opts.min_ram_mb = args
                    .get(i)
                    .ok_or_else(|| "--min-ram-mb requires a value".to_string())?
                    .parse()
                    .map_err(|_| "invalid --min-ram-mb value")?;
            }
            "--battery" => {
                opts.battery = true;
            }
            "--cascade" => {
                cascade_explicit = true;
                opts.cascade = true;
                opts.battery = true;
                if !target_explicit {
                    opts.target_pct = 100;
                }
                if opts.tier3_target_pct.is_none() {
                    opts.tier3_target_pct = Some(15);
                }
                if opts.step_pct == 1 {
                    opts.step_pct = 5;
                }
                if opts.interval_ms == 1500 {
                    opts.interval_ms = 500;
                }
            }
            "--tier3-target-pct" => {
                i += 1;
                let val: u64 = args
                    .get(i)
                    .ok_or_else(|| "--tier3-target-pct requires a value (1-100)".to_string())?
                    .parse()
                    .map_err(|_| "invalid --tier3-target-pct value")?;
                if !(1..=100).contains(&val) {
                    return Err("--tier3-target-pct must be within 1-100".into());
                }
                opts.tier3_target_pct = Some(val);
                opts.cascade = true;
                opts.battery = true;
                if !target_explicit {
                    opts.target_pct = 100;
                }
                if opts.step_pct == 1 {
                    opts.step_pct = 5;
                }
                if opts.interval_ms == 1500 {
                    opts.interval_ms = 500;
                }
            }
            "--tier1-target-pct" => {
                i += 1;
                opts.tier1_target_pct = args
                    .get(i)
                    .ok_or_else(|| "--tier1-target-pct requires a value (1-100)".to_string())?
                    .parse()
                    .map_err(|_| "invalid --tier1-target-pct value")?;
            }
            "--tier2-target-pct" => {
                i += 1;
                opts.tier2_target_pct = args
                    .get(i)
                    .ok_or_else(|| "--tier2-target-pct requires a value (1-100)".to_string())?
                    .parse()
                    .map_err(|_| "invalid --tier2-target-pct value")?;
            }
            "--physical-cache-target-mib" => {
                i += 1;
                opts.physical_cache_target_mib = Some(
                    args.get(i)
                        .ok_or_else(|| "--physical-cache-target-mib requires a value".to_string())?
                        .parse()
                        .map_err(|_| "invalid --physical-cache-target-mib value")?,
                );
            }
            "--full-three-tier" => {
                cascade_explicit = true;
                opts.full_three_tier_profile = true;
                opts.cascade = true;
                opts.battery = true;
            }
            "--tier3-only" => opts.tier3_only = true,
            "--max-psi-full" => {
                i += 1;
                opts.max_psi_full = args
                    .get(i)
                    .ok_or_else(|| "--max-psi-full requires a value".to_string())?
                    .parse()
                    .map_err(|_| "invalid --max-psi-full value")?;
            }
            "--max-latency-ms" => {
                i += 1;
                opts.max_latency_ms = args
                    .get(i)
                    .ok_or_else(|| "--max-latency-ms requires a value".to_string())?
                    .parse()
                    .map_err(|_| "invalid --max-latency-ms value")?;
            }
            "--min-order-7-chunks" => {
                i += 1;
                opts.min_order_7_chunks = args
                    .get(i)
                    .ok_or_else(|| "--min-order-7-chunks requires a value".to_string())?
                    .parse()
                    .map_err(|_| "invalid --min-order-7-chunks value")?;
            }
            "--log" => {
                i += 1;
                opts.telemetry_log = args
                    .get(i)
                    .ok_or_else(|| "--log requires a file path".to_string())?
                    .clone();
            }
            "--json" => {
                opts.json = true;
            }
            "--threads" => {
                i += 1;
                opts.threads = args
                    .get(i)
                    .ok_or_else(|| "--threads requires a value".to_string())?
                    .parse()
                    .map_err(|_| "invalid --threads value")?;
            }
            other => return Err(format!("unknown stress argument: {other}")),
        }
        i += 1;
    }

    let max_threads = std::thread::available_parallelism()
        .map(|n| n.get() as u64)
        .unwrap_or(1);

    if opts.threads > max_threads {
        return Err(format!(
            "thread count {} exceeds physical hardware limit ({})",
            opts.threads, max_threads
        ));
    }
    opts.threads = opts.threads.max(1);
    opts.start_pct = opts.start_pct.clamp(1, 200);
    opts.target_pct = opts.target_pct.clamp(opts.start_pct, 200);
    opts.step_pct = opts.step_pct.clamp(1, 25);
    if opts.tier3_only {
        if cascade_explicit || opts.full_three_tier_profile {
            return Err(
                "--tier3-only cannot be combined with --cascade or --full-three-tier".into(),
            );
        }
        if opts.tier3_target_pct.is_none() {
            return Err("--tier3-only requires --tier3-target-pct".into());
        }
        if opts.physical_cache_target_mib.is_some() {
            return Err("--tier3-only cannot set --physical-cache-target-mib".into());
        }
        opts.cascade = false;
    }
    if opts.full_three_tier_profile {
        if opts.start_pct > 100 {
            return Err("--full-three-tier requires --start at most 100".into());
        }
        opts.target_pct = 100;
        opts.tier1_target_pct = 100;
        opts.tier2_target_pct = 100;
        opts.tier3_target_pct = Some(99);
    }
    if !(1..=100).contains(&opts.tier1_target_pct)
        || !(1..=100).contains(&opts.tier2_target_pct)
        || opts.physical_cache_target_mib == Some(0)
    {
        return Err("tier targets must be within 1-100% and physical cache target nonzero".into());
    }
    if opts.cascade && is_wsl2() {
        // WSL2 clamp. The supervisor's Continuation gate binds before this
        // budget: `SupervisorState::Critical` starts at `psi_full_avg10 >= 5.0`
        // for five consecutive samples, and Continuation refuses `Critical`.
        // Raising this clamp cannot make `--full-three-tier` reachable while
        // that streak rule holds. Two contracts, not one — see verified test case.
        opts.max_psi_full = opts.max_psi_full.min(10.0);
    }
    Ok(opts)
}

mod budget;
#[cfg(test)]
use budget::{
    CacheSample, full_profile_cache_target, full_tier_snapshot, parse_cache_status_sample,
    require_physical_cache_before_cascade, require_tier3_only_capacity, safe_allocation_kib,
    step_interval_ms, tier3_active_since_baseline, tier3_only_target_reached, tier3_target_reached,
};
mod format;
#[cfg(test)]
use format::{
    append_telemetry_log, archive_and_compare_benchmark, compute_telemetry_reading,
    format_cache_fill, format_system_time, format_throughput_mbs, format_tier_capacity,
    format_tier1_compression, mb_cell, mb_mark, psi_mark, tier_pct_mark,
};
mod host;
pub use host::is_wsl2;
#[cfg(test)]
use host::{
    BuddyInterlockAction, BuddyinfoReading, StressPsi, count_hung_task_lines,
    count_kernel_fault_lines, decide_buddyinfo_action, is_order_7_depleted, mem_info_from,
    parse_buddyinfo_reading, parse_min_free_kbytes, probe_allocation_latency_ms,
    psi_at_or_over_limit, psi_below_limit, read_buddyinfo_reading, read_mem_info, read_psi_full,
    read_swap_tier_capacities, read_swap_tiers, read_sysctl_min_free_kib,
    read_tier_disk_total_bytes, required_memavailable_floor_kib, stress_psi_sample,
    swap_tiers_from, tier_disk_bytes_from,
};
// `stress/measure.rs` extraction slice. Re-exported here so the run body
// keeps one path to them.
mod measure;
use measure::published_mib;
#[cfg(test)]
use measure::{
    KIB_PER_MIB, PeakSwapOccupancy, RATE_WINDOW_MIN_DT_SECS, TierRateSampler, cache_fill_pct,
    capacity_share_pct, compute_latency_percentiles, fold_peak_zram_compression,
};
mod phase_ramp;
mod runner;
pub use runner::run;
#[cfg(test)]
use runner::{StressEnvironment, run_with_environment};

#[cfg(test)]
#[path = "stress/tests.rs"]
mod tests;
