// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/stress/phase_ramp.rs
//! Phase 1 progressive 1%-by-1% pressure ramp and shared run accumulators.
//!
//! Extracted from `stress.rs` (`run_with_environment`) so the ramp safety
//! interlocks, writeback recovery wait, and step pacing fit inside the module
//! review budget.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::cascade;
use crate::zram_compression::ZramCompression;

use super::budget::{
    full_tier_snapshot, safe_allocation_kib, step_interval_ms, tier3_active_since_baseline,
    tier3_only_target_reached, tier3_target_reached,
};
use super::classify::Tier3AdmissionWitness;
use super::format::{
    append_telemetry_log, compute_telemetry_reading, mb_cell, mb_mark, psi_mark, tier_pct_mark,
};
use super::host::{
    BuddyInterlockAction, StressPsi, decide_buddyinfo_action_with_threshold, psi_at_or_over_limit,
    psi_below_limit, required_memavailable_floor_kib, stress_psi_sample,
};
use super::measure::{
    KIB_PER_MIB, PeakSwapOccupancy, TierDiskIoBytes, TierRateSampler, fold_gpu_cache_run_telemetry,
    fold_peak_zram_compression, published_mib,
};
use super::model::GpuCacheRunTelemetry;
use super::runner::StressEnvironment;
use super::{
    BARE_METAL_MULTI_TIER_FLOOR_MB, CASCADE_RAMP_LIMIT_PCT, MIN_ALLOCATION_STEP_KIB,
    MULTI_TIER_MIN_USABLE_AVAIL_KIB, SINGLE_TIER_MIN_USABLE_AVAIL_KIB, StressOptions,
    TIER3_HEADROOM_RESERVE_KIB, TIER3_QUALIFICATION_RAMP_LIMIT_PCT, TierCapacityStats,
    WSL2_MIN_PHYSICAL_HEADROOM_MB,
};

/// Mutable sample accumulators shared across Phase 1, Phase 2/3, and Phase 4.
pub(super) struct StressRunState {
    pub(super) total_allocated_mb: u64,
    pub(super) max_safe_pct: u64,
    pub(super) peak_zram: PeakSwapOccupancy,
    pub(super) peak_zram_compression: Option<ZramCompression>,
    pub(super) peak_vram: PeakSwapOccupancy,
    pub(super) peak_physical_vram_kib: u64,
    pub(super) peak_physical_target_kib: u64,
    pub(super) simultaneous_physical_cache_target: u64,
    pub(super) simultaneous_physical_cache: u64,
    pub(super) physical_cache_samples: usize,
    pub(super) cache_lost_during_stress: bool,
    pub(super) safety_halt: bool,
    pub(super) peak_gpu_cache: Option<GpuCacheRunTelemetry>,
    pub(super) simultaneous_full_tiers: bool,
    pub(super) tier3_target_achieved: bool,
    pub(super) peak_ssd: PeakSwapOccupancy,
    pub(super) tier3_admission: Tier3AdmissionWitness,
    pub(super) peak_total_swap_kib: Option<u64>,
    pub(super) peak_pressure: f64,
    pub(super) readings_count: usize,
    pub(super) active_cycles_done: usize,
    pub(super) rates: TierRateSampler,
    pub(super) prev_sample_time: Instant,
    pub(super) latencies_ms: Vec<f64>,
}

impl StressRunState {
    #[cfg(test)]
    pub(super) fn new(initial_disk_bytes: (u64, u64, u64)) -> Self {
        Self::with_io(TierDiskIoBytes::from_totals(initial_disk_bytes))
    }

    pub(super) fn with_io(initial_io: TierDiskIoBytes) -> Self {
        Self {
            total_allocated_mb: 0,
            max_safe_pct: 0,
            peak_zram: PeakSwapOccupancy::default(),
            peak_zram_compression: None,
            peak_vram: PeakSwapOccupancy::default(),
            peak_physical_vram_kib: 0,
            peak_physical_target_kib: 0,
            simultaneous_physical_cache_target: 0,
            simultaneous_physical_cache: 0,
            physical_cache_samples: 0,
            cache_lost_during_stress: false,
            safety_halt: false,
            peak_gpu_cache: None,
            simultaneous_full_tiers: false,
            tier3_target_achieved: false,
            peak_ssd: PeakSwapOccupancy::default(),
            tier3_admission: Tier3AdmissionWitness::default(),
            peak_total_swap_kib: None,
            peak_pressure: 1.0,
            readings_count: 0,
            active_cycles_done: 0,
            rates: TierRateSampler::with_io(initial_io),
            prev_sample_time: Instant::now(),
            latencies_ms: Vec::new(),
        }
    }
}

/// Read-only runtime parameters threaded through all stress phases.
pub(super) struct RunExecutionContext<'a> {
    pub(super) opts: &'a StressOptions,
    pub(super) qualification_opts: &'a StressOptions,
    pub(super) cascade_mode: bool,
    pub(super) required_psi: bool,
    pub(super) ram_total_kib: u64,
    pub(super) ram_total_mb: Option<u64>,
    pub(super) initial_ssd_used_kib: Option<u64>,
    pub(super) term_signal: &'a Arc<AtomicBool>,
    pub(super) last_heartbeat: &'a Arc<AtomicU64>,
    pub(super) chunks: &'a Arc<Mutex<Vec<Vec<u8>>>>,
    pub(super) run_start: Instant,
}

const STRESS_PAGE_MAGIC: u64 = 0x5241_4D53_4852_4456;
const STRESS_PAGE_BYTES: usize = 4096;
const STRESS_COMPRESSIBLE_BODY_END: usize = 1408;

/// Stamp a 4 KiB page with an exact integrity signature and ~2.9:1 compressible body.
pub(super) fn fill_deterministic_compressible_page(
    page: &mut [u8],
    chunk_index: usize,
    page_index: usize,
) {
    if page.len() < STRESS_PAGE_BYTES {
        return;
    }
    let key = ((chunk_index as u64) << 32) | (page_index as u64);
    let tag = STRESS_PAGE_MAGIC ^ key;
    let check = tag.rotate_left(17) ^ 0xA5A5_A5A5_5A5A_5A5A;
    page[0..8].copy_from_slice(&tag.to_le_bytes());
    page[8..16].copy_from_slice(&(chunk_index as u64).to_le_bytes());
    page[16..24].copy_from_slice(&(page_index as u64).to_le_bytes());
    page[24..32].copy_from_slice(&check.to_le_bytes());
    const TEMPLATE: &[u8; 64] = b"RAMSHARED-CUDA-PTX-LZ4-COMPRESSIBLE-SWAP-TIER-VERIFICATION-BLOCK";
    let salt = ((chunk_index ^ page_index) & 0x0F) as u8;
    let mut off = 32;
    while off + TEMPLATE.len() <= STRESS_COMPRESSIBLE_BODY_END {
        page[off..off + TEMPLATE.len()].copy_from_slice(TEMPLATE);
        page[off] ^= salt;
        off += TEMPLATE.len();
    }
}

/// Verify deterministic page signatures across sampled pages of one chunk (`(verified, mismatches)`).
pub(super) fn verify_sampled_chunk_pages(
    chunk: &[u8],
    chunk_index: usize,
    page_stride: usize,
) -> (usize, usize) {
    let page_count = chunk.len() / STRESS_PAGE_BYTES;
    if page_count == 0 {
        return (0, 0);
    }
    let step = page_stride.max(1);
    let mut verified = 0usize;
    let mut mismatches = 0usize;
    for page_index in (0..page_count).step_by(step) {
        let start = page_index * STRESS_PAGE_BYTES;
        let page = &chunk[start..start + STRESS_PAGE_BYTES];
        let key = ((chunk_index as u64) << 32) | (page_index as u64);
        let expected_tag = STRESS_PAGE_MAGIC ^ key;
        let expected_check = expected_tag.rotate_left(17) ^ 0xA5A5_A5A5_5A5A_5A5A;
        let tag = u64::from_le_bytes(page[0..8].try_into().unwrap_or_default());
        let c_idx = u64::from_le_bytes(page[8..16].try_into().unwrap_or_default());
        let p_idx = u64::from_le_bytes(page[16..24].try_into().unwrap_or_default());
        let check = u64::from_le_bytes(page[24..32].try_into().unwrap_or_default());
        verified += 1;
        if tag != expected_tag
            || c_idx != chunk_index as u64
            || p_idx != page_index as u64
            || check != expected_check
            || page[32] != (b'R' ^ ((chunk_index ^ page_index) & 0x0F) as u8)
        {
            mismatches += 1;
        }
    }
    (verified, mismatches)
}

/// Execute Phase 1 progressive 1%-by-1% pressure escalation until target, floor, or interlock halt.
pub(super) fn run_phase1_ramp<E: StressEnvironment>(
    ctx: &RunExecutionContext<'_>,
    environment: &E,
    state: &mut StressRunState,
) {
    // Phase 1: 1%-by-1% Micro-Step Ramp
    let effective_target = if (ctx.opts.tier3_target_pct.is_some() || ctx.opts.cascade)
        && ctx.opts.target_pct == 100
    {
        if ctx.opts.tier3_target_pct.is_some() {
            TIER3_QUALIFICATION_RAMP_LIMIT_PCT
        } else {
            CASCADE_RAMP_LIMIT_PCT
        }
    } else {
        ctx.opts.target_pct
    };
    let mut current_target = ctx.opts.start_pct;
    while current_target <= effective_target {
        // Continuation, not admission: the supervisor closing admission is
        // expected while this ramp is the pressure source.
        if ctx.cascade_mode
            && let Err(error) = cascade::stress_continuation()
        {
            eprintln!("[stress] safety_halt: {error}");
            state.safety_halt = true;
            break;
        }
        if ctx.term_signal.load(Ordering::Relaxed) {
            eprintln!("[stress] safety_halt: term_signal (watchdog)");
            state.safety_halt = true;
            break;
        }
        if (ctx.opts.cascade || ctx.opts.tier3_target_pct.is_some())
            && environment.current_kernel_faults() != Some(0)
        {
            eprintln!("[stress] safety_halt: kernel_faults");
            state.safety_halt = true;
            break;
        }

        ctx.last_heartbeat.store(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            Ordering::Relaxed,
        );

        let mut avail_kib = environment.read_mem_info().map(|m| m.avail_kib);
        // A tolerated absence continues the step with `None`, not with a
        // fabricated `0.0` that would disarm the pressure interlock below.
        let psi_full = match stress_psi_sample(environment.read_psi_full(), ctx.required_psi) {
            StressPsi::Measured(value) => Some(value),
            StressPsi::Uninformed => None,
            StressPsi::Invalid => {
                eprintln!("[stress] safety_halt: psi_telemetry_invalid");
                state.safety_halt = true;
                break;
            }
        };
        let lat_ms = environment.probe_allocation_latency_ms();
        state.latencies_ms.push(lat_ms);
        // Both swap faces come from `/proc/swaps`. A half-read is not an
        // observation of either figure, so the pair is accepted only together;
        // an absent sample leaves the ratios at their `None` refusal rather
        // than at a synthesized `0`.
        let swap_sample = environment
            .read_swap_tiers()
            .zip(environment.read_swap_tier_capacities());
        let (tiers, (cap1, cap2, cap3)) = match swap_sample {
            Some(sample) => (Some(sample.0), sample.1),
            None => (
                None,
                (
                    TierCapacityStats::default(),
                    TierCapacityStats::default(),
                    TierCapacityStats::default(),
                ),
            ),
        };
        // The admission witness is folded here, not at the peak fold: it needs
        // the two upper tiers' occupancy **at the instant the fallback was
        // read**, and a peak is a run-wide maximum that can come from another
        // sample entirely. This is the one place in the iteration where all
        // three readings exist together.
        //
        // An unreadable table is not a sample. `observe` takes the pair only
        // when the occupancy reading exists; a synthesized zero would count as
        // "no fallback swap seen" and blank out a real spill.
        if let Some(tiers) = tiers {
            state.tier3_admission.observe(tiers.ssd_kib, cap1, cap2);
        }
        let cache_sample = if ctx.opts.tier3_only {
            None
        } else {
            environment.read_cache_status_sample()
        };
        if ctx.cascade_mode && cache_sample.is_none() {
            eprintln!("[stress] safety_halt: physical_cache_unavailable");
            state.cache_lost_during_stress = true;
            state.safety_halt = true;
            break;
        }
        if let Some(sample) = cache_sample {
            state.physical_cache_samples += 1;
            state.peak_physical_vram_kib = state.peak_physical_vram_kib.max(sample.cached_kib);
            state.peak_physical_target_kib = state.peak_physical_target_kib.max(sample.target_kib);
        }
        fold_gpu_cache_run_telemetry(
            &mut state.peak_gpu_cache,
            environment.read_gpu_cache_telemetry(),
        );
        if !ctx.opts.tier3_only
            && let Some(sample) = full_tier_snapshot(
                cap1.pct(),
                cap2.pct(),
                cap3.pct(),
                ctx.qualification_opts,
                cache_sample,
            )
        {
            state.simultaneous_full_tiers = true;
            if sample.target_mib() > state.simultaneous_physical_cache_target {
                state.simultaneous_physical_cache_target = sample.target_mib();
                state.simultaneous_physical_cache = sample.cached_mib();
            }
        }

        let reading = compute_telemetry_reading(
            lat_ms,
            psi_full,
            state.total_allocated_mb,
            ctx.ram_total_mb,
            tiers.map(|tiers| tiers.total_mb()),
        )
        .with_tier_pcts(cap1.pct(), cap2.pct(), cap3.pct());
        state.peak_pressure = state.peak_pressure.max(reading.pressure_index);
        // Fold the swap peaks at sample time so a Zero Hang Protection break on
        // this sample still counts. The buddy interlock, PSI limit, latency
        // limit, and RAM floor all break before the step's allocation, so a
        // peak folded after that point drops exactly the highest-stress sample.
        // These are the same values the telemetry line records, which keeps
        // peak_swap_mb equal to the maximum `Swap:` value in the log. Each
        // peak takes the capacity from this same iteration's tier read, so
        // the published percent pairs one observation with itself.
        //
        // An absent sample is not folded at all. Folding `0` would leave the
        // peaks where they are, but it would also let a run that never read
        // the swap table look like one that measured an empty table.
        if let Some(tiers) = tiers {
            state.peak_zram.fold(tiers.zram_kib, cap1.total_kib);
            state.peak_vram.fold(tiers.vram_kib, cap2.total_kib);
            state.peak_ssd.fold(tiers.ssd_kib, cap3.total_kib);
            let total_kib = tiers.total_kib();
            state.peak_total_swap_kib = Some(
                state
                    .peak_total_swap_kib
                    .map_or(total_kib, |peak| peak.max(total_kib)),
            );
        }
        fold_peak_zram_compression(
            &mut state.peak_zram_compression,
            environment.read_zram_compression(),
        );
        state.readings_count += 1;
        append_telemetry_log(&ctx.opts.telemetry_log, &reading);

        let sysctl_min_free_kib = environment.read_sysctl_min_free_kib();
        let is_multi_tier = ctx.cascade_mode || ctx.opts.tier3_only;
        const SWAP_DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(150);
        const MAX_SWAP_DRAIN_IDLE_CYCLES: usize = 80; // 80 * 150ms = 12.0s of zero swap growth before declaring limit

        // Under WSL2, Hyper-V synthetic devices (hv_balloon, vmicvmswitch) and host-guest
        // heartbeat require at least 600 MB of total physical memory headroom.
        // In Linux, /proc/meminfo MemAvailable ALREADY discounts the kernel's
        // reserved pages (min_free_kbytes). Therefore: physical_headroom =
        // MemAvailable + min_free_kbytes. We calibrate hard_floor against
        // MemAvailable such that:
        //   hard_floor + min_free_kbytes >= target_physical_floor (600 MB on WSL2).
        // The policy constants are whole MiB and are converted to the unit the
        // measurement carries with an exact `* 1024`, so the comparison below
        // never rounds either side. The multiply saturates: an operator floor
        // above what KiB can express means "unreachable", which is exactly the
        // state the drain loop must see rather than a wrapped-around floor.
        let target_physical_floor_kib = if environment.is_wsl2() {
            ctx.opts
                .min_ram_mb
                .max(WSL2_MIN_PHYSICAL_HEADROOM_MB)
                .saturating_mul(KIB_PER_MIB)
        } else {
            ctx.opts
                .min_ram_mb
                .max(BARE_METAL_MULTI_TIER_FLOOR_MB)
                .saturating_mul(KIB_PER_MIB)
        };

        let min_usable_avail_kib = if is_multi_tier {
            MULTI_TIER_MIN_USABLE_AVAIL_KIB
        } else {
            SINGLE_TIER_MIN_USABLE_AVAIL_KIB
        };
        let hard_floor_kib = required_memavailable_floor_kib(
            target_physical_floor_kib,
            sysctl_min_free_kib,
            min_usable_avail_kib,
        );

        // Drain detection compares two live reads of the same counter, so it
        // holds the unit those reads carry. A rounded whole-MiB pair would call
        // a 512 KiB-per-poll drain "idle" whenever each step stayed under half
        // a megabyte — the exact signal this loop is here to notice.
        //
        // The baseline is `Option` for the same reason: a run whose first
        // `/proc/swaps` read failed has no counter to grow from, and folding
        // that absence into `0` would make the first successful read look like
        // infinite growth.
        let mut last_swap_kib = tiers.map(|tiers| tiers.total_kib());
        let mut idle_cycles = 0;
        let max_idle_cycles = MAX_SWAP_DRAIN_IDLE_CYCLES;

        // An unmeasured headroom is treated as at the floor, so the drain wait
        // is entered on absence rather than skipped: the conservative answer,
        // which waits for a reading instead of proceeding on one that never
        // arrived.
        while avail_kib.is_none_or(|avail| avail <= hard_floor_kib) && is_multi_tier {
            if ctx.term_signal.load(Ordering::Relaxed) {
                break;
            }
            ctx.last_heartbeat.store(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                Ordering::Relaxed,
            );
            thread::sleep(SWAP_DRAIN_POLL_INTERVAL);
            avail_kib = environment.read_mem_info().map(|m| m.avail_kib);
            // An unreadable table has not observed growth, so the idle counter
            // advances. The floor check beside this loop is what bounds the
            // wait, and it fails closed on the same absence.
            let cur_swap_kib = environment.read_swap_tiers().map(|tiers| tiers.total_kib());
            match (cur_swap_kib, last_swap_kib) {
                (Some(cur), Some(last)) if cur > last => {
                    // kswapd is actively draining dirty pages into VRAM/SSD
                    last_swap_kib = Some(cur);
                    idle_cycles = 0;
                }
                (Some(cur), None) => {
                    // The first readable counter is a baseline, not growth.
                    last_swap_kib = Some(cur);
                    idle_cycles += 1;
                }
                _ => {
                    idle_cycles += 1;
                }
            }
            if idle_cycles >= max_idle_cycles
                || avail_kib.is_some_and(|avail| avail > hard_floor_kib)
            {
                break;
            }
        }

        if ctx.term_signal.load(Ordering::Relaxed) {
            state.safety_halt = true;
            break;
        }

        // An unmeasured headroom is treated as at the floor: the conservative
        // answer, which refuses to allocate rather than permitting a step on a
        // figure nobody read.
        let floor_breached = if is_multi_tier {
            avail_kib.is_none_or(|avail| avail <= hard_floor_kib) && idle_cycles >= max_idle_cycles
        } else {
            avail_kib.is_none_or(|avail| avail <= hard_floor_kib)
        };

        if floor_breached {
            state.safety_halt = true;
            if !ctx.opts.json {
                println!(
                    "│ {:>4}%  │ {:>8} MB │ {} │ {} │ {} │ {} │ {:>6} │ {:>6.2}ms │ {:>11} │ 🛑 RAM FLOOR REACHED      │",
                    current_target,
                    state.total_allocated_mb,
                    mb_cell(state.peak_zram.published_mib()),
                    mb_cell(state.peak_vram.published_mib()),
                    mb_cell(state.peak_ssd.published_mib()),
                    mb_cell(state.peak_total_swap_kib.map(published_mib)),
                    psi_mark(psi_full),
                    lat_ms,
                    reading.gauge
                );
                println!(
                    "\n[🛑 SAFETY FLOOR REACHED] Available RAM reached floor ({} <= {}). Halted at {}% (Zero Hang Protection).",
                    mb_mark(avail_kib.map(published_mib)),
                    published_mib(hard_floor_kib),
                    state.max_safe_pct
                );
            }
            break;
        }

        if environment.is_wsl2() && ctx.opts.min_order_7_chunks > 0 {
            let mut action = decide_buddyinfo_action_with_threshold(
                environment.read_buddyinfo_reading(),
                ctx.opts.min_order_7_chunks,
            );
            if let BuddyInterlockAction::CompactionTriggered = action {
                thread::sleep(Duration::from_millis(150));
            } else if let BuddyInterlockAction::Halt { .. } = action {
                // Before halting, trigger proactive compaction and allow kcompactd to coalesce pages
                for _ in 0..3 {
                    environment.trigger_proactive_compaction();
                    thread::sleep(Duration::from_millis(200));
                    action = decide_buddyinfo_action_with_threshold(
                        environment.read_buddyinfo_reading(),
                        ctx.opts.min_order_7_chunks,
                    );
                    if !matches!(action, BuddyInterlockAction::Halt { .. }) {
                        break;
                    }
                }
            }
            if let BuddyInterlockAction::Halt { detected_chunks } = action {
                state.safety_halt = true;
                if !ctx.opts.json {
                    println!(
                        "│ {:>4}%  │ {:>8} MB │ {} │ {} │ {} │ {} │ {:>6} │ {:>6.2}ms │ {:>11} │ 🛡️  BUDDY INTERLOCK   │",
                        current_target,
                        state.total_allocated_mb,
                        mb_cell(state.peak_zram.published_mib()),
                        mb_cell(state.peak_vram.published_mib()),
                        mb_cell(state.peak_ssd.published_mib()),
                        mb_cell(state.peak_total_swap_kib.map(published_mib)),
                        psi_mark(psi_full),
                        lat_ms,
                        reading.gauge
                    );
                    println!(
                        "\n[🛡️ VMBUS BUDDY INTERLOCK] Order-7 physical memory depleted (<{} chunks, detected {}). Halted at {}% (Zero Hang Protection).",
                        ctx.opts.min_order_7_chunks, detected_chunks, state.max_safe_pct
                    );
                }
                break;
            }
        }

        if psi_at_or_over_limit(psi_full, ctx.opts.max_psi_full) {
            state.safety_halt = true;
            if !ctx.opts.json {
                println!(
                    "\n[⚠️ PRESSURE LIMIT] PSI Full {} reached the {:.1}% limit. Halted at {}%.",
                    psi_mark(psi_full),
                    ctx.opts.max_psi_full,
                    state.max_safe_pct
                );
            }
            break;
        }

        if lat_ms >= ctx.opts.max_latency_ms {
            state.safety_halt = true;
            if !ctx.opts.json {
                println!(
                    "│ {:>4}%  │ {:>8} MB │ {} │ {} │ {} │ {} │ {:>6} │ {:>6.2}ms │ {:>11} │ ⏱️  LATENCY SPIKE DAMP    │",
                    current_target,
                    state.total_allocated_mb,
                    mb_cell(state.peak_zram.published_mib()),
                    mb_cell(state.peak_vram.published_mib()),
                    mb_cell(state.peak_ssd.published_mib()),
                    mb_cell(state.peak_total_swap_kib.map(published_mib)),
                    psi_mark(psi_full),
                    lat_ms,
                    reading.gauge
                );
                println!(
                    "\n[⏱️  LATENCY SPIKE DAMPING] Memory latency spiked to {:.2} ms >= {:.2} ms. Halted at {}%.",
                    lat_ms, ctx.opts.max_latency_ms, state.max_safe_pct
                );
            }
            break;
        }

        // An unreadable table has no percents and no occupancy. The ratios
        // refuse via `pct()` on the empty default; the raw `used_kib` is kept
        // as `None` so the pacing decision beside it cannot read a synthesized
        // zero as a measured idle.
        let capacities = environment.read_swap_tier_capacities();
        let cap3_used_kib = capacities.map(|(_, _, cap3)| cap3.used_kib);
        let (cap1, cap2, cap3) = capacities.unwrap_or((
            TierCapacityStats::default(),
            TierCapacityStats::default(),
            TierCapacityStats::default(),
        ));
        if let Some(t3_target) = ctx.opts.tier3_target_pct
            && (ctx.opts.tier3_only
                || (cap1
                    .pct()
                    .is_some_and(|pct| pct >= ctx.opts.tier1_target_pct)
                    && cap2
                        .pct()
                        .is_some_and(|pct| pct >= ctx.opts.tier2_target_pct)))
            && if ctx.opts.tier3_only {
                tier3_only_target_reached(cap3.pct(), t3_target, state.total_allocated_mb)
            } else {
                tier3_target_reached(cap3.pct(), t3_target)
            }
        {
            state.tier3_target_achieved = true;
            if !ctx.opts.json {
                if ctx.opts.tier3_only {
                    println!(
                        "\n[🎯 TIER 3 TARGET REACHED] Storage swap: {} (Target: {}). This run does not qualify the full cascade.",
                        tier_pct_mark(cap3.pct()),
                        t3_target
                    );
                } else {
                    println!(
                        "\n[🎯 LOGICAL SWAP TARGET REACHED] ZRAM: {}, NBD: {}, SSD: {} (Target: {}). Physical VRAM remains a separate check.",
                        tier_pct_mark(cap1.pct()),
                        tier_pct_mark(cap2.pct()),
                        tier_pct_mark(cap3.pct()),
                        t3_target
                    );
                }
            }
            break;
        }

        // The step budget is taken from the exact KiB total. Rounding the total
        // to a whole-MiB face first would move the percentage off the ratio the
        // kernel reported, and the `.max` floor is a policy minimum step — not
        // a rounding of anything measured.
        let one_pct_kib =
            ((ctx.ram_total_kib * ctx.opts.step_pct) / 100).max(MIN_ALLOCATION_STEP_KIB);
        let tier3_active = tier3_active_since_baseline(cap3_used_kib, ctx.initial_ssd_used_kib);
        let safe_alloc_kib = safe_allocation_kib(
            is_multi_tier,
            tier3_active,
            avail_kib,
            hard_floor_kib,
            one_pct_kib,
        );

        if safe_alloc_kib == 0 {
            // Recovery is licensed only by a *measured* sub-limit pressure.
            // An uninformed cycle has not certified that it is safe to sit
            // through a writeback wait, so it takes the halt path instead.
            if is_multi_tier && !floor_breached && psi_below_limit(psi_full, ctx.opts.max_psi_full)
            {
                // Headroom is temporarily below allocation threshold.
                // Wait for kswapd to complete ongoing writebacks and recover headroom above hard_floor.
                let mut recovered = false;
                for _ in 0..60 {
                    // 60 * 100ms = 6.0s max wait for disk writeback
                    if ctx.term_signal.load(Ordering::Relaxed) {
                        break;
                    }
                    let fresh_psi =
                        match stress_psi_sample(environment.read_psi_full(), ctx.required_psi) {
                            StressPsi::Measured(value) => Some(value),
                            StressPsi::Uninformed => None,
                            StressPsi::Invalid => {
                                eprintln!(
                                    "[stress] safety_halt: psi_telemetry_invalid_during_recovery"
                                );
                                break;
                            }
                        };
                    if psi_at_or_over_limit(fresh_psi, ctx.opts.max_psi_full) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(100));
                    ctx.last_heartbeat.store(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                        Ordering::Relaxed,
                    );
                    let fresh_avail_kib = environment.read_mem_info().map(|m| m.avail_kib);
                    let threshold_kib = hard_floor_kib.saturating_add(TIER3_HEADROOM_RESERVE_KIB);
                    // An unmeasured headroom has not recovered above the
                    // reserve: `is_some_and` keeps waiting rather than
                    // releasing the step on a figure nobody read.
                    if fresh_avail_kib.is_some_and(|avail| avail > threshold_kib) {
                        avail_kib = fresh_avail_kib;
                        recovered = true;
                        break;
                    }
                }
                if !recovered {
                    state.safety_halt = true;
                    if !ctx.opts.json {
                        println!(
                            "\n[🛑 RAM FLOOR BOUND] Memory headroom cannot recover above safe floor ({} <= {}). Halting ramp safely at {}%.",
                            mb_mark(avail_kib.map(published_mib)),
                            published_mib(hard_floor_kib),
                            state.max_safe_pct
                        );
                    }
                    break;
                }
                // Re-read swap capacity and headroom before allocating. The values that
                // triggered the wait are stale after writeback recovery.
                continue;
            } else {
                state.safety_halt = true;
                break;
            }
        }

        // Allocate and dirty pages with realistic compressible workload entropy + integrity tags.
        let num_bytes = (safe_alloc_kib as usize) * 1024;
        let mut slice = environment.allocate_buffer(num_bytes);
        let chunk_idx = ctx.chunks.lock().map(|g| g.len()).unwrap_or(0);
        for (page_idx, page) in slice
            .as_chunks_mut::<STRESS_PAGE_BYTES>()
            .0
            .iter_mut()
            .enumerate()
        {
            fill_deterministic_compressible_page(page, chunk_idx, page_idx);
            if (page_idx & 0x1FF) == 0 {
                // Every 2 MiB, yield CPU so Hyper-V VMBus IC heartbeat interrupt handler is never starved
                thread::yield_now();
                ctx.last_heartbeat.store(
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    Ordering::Relaxed,
                );
            }
        }
        if ctx.cascade_mode || ctx.opts.battery {
            environment.pageout_buffer(&mut slice);
        }

        let allocated_mb = (slice.len() as u64) / (1024 * 1024);
        if let Ok(mut guard) = ctx.chunks.lock() {
            guard.push(slice);
        }
        state.total_allocated_mb += allocated_mb;

        state.max_safe_pct = current_target;
        environment.report_progress(
            "stress_ramp",
            Some(format!(
                "{}% · {} MiB (ZRAM {} / NBD {} / SSD {})",
                current_target,
                state.total_allocated_mb,
                mb_mark(tiers.map(|t| t.zram_mb())),
                mb_mark(tiers.map(|t| t.vram_mb())),
                mb_mark(tiers.map(|t| t.ssd_mb()))
            )),
            1,
            3,
            state.total_allocated_mb.saturating_mul(1024 * 1024),
            (ctx.ram_total_kib.max(state.total_allocated_mb * 1024)).saturating_mul(1024),
        );

        if !ctx.opts.json {
            // The four swap columns print the **current** sample, not the
            // run-wide peaks the halt rows print: this row is the reading of
            // the step that just completed. An absent sample is `unmeasured`
            // in those columns rather than a `0` that would claim a measured
            // empty tier for this step.
            println!(
                "│ {:>4}%  │ {:>8} MB │ {} │ {} │ {} │ {} │ {:>6} │ {:>6.2}ms │ {:>11} │ {:<25} │",
                current_target,
                state.total_allocated_mb,
                mb_cell(tiers.map(|tiers| tiers.zram_mb())),
                mb_cell(tiers.map(|tiers| tiers.vram_mb())),
                mb_cell(tiers.map(|tiers| tiers.ssd_mb())),
                mb_cell(tiers.map(|tiers| tiers.total_mb())),
                psi_mark(psi_full),
                lat_ms,
                reading.gauge,
                reading.classification
            );
        }

        // Adaptive StorVSC I/O Pacing:
        // Tier 3 is backed by Hyper-V synthetic SCSI (storvsc) writing to swap.vhdx on NTFS.
        // If Tier 3 is active, pace steps by at least 500ms to allow StorVSC ring buffer completions.
        let step_interval =
            step_interval_ms(environment.is_wsl2(), tier3_active, ctx.opts.interval_ms);
        // Heartbeat before the planned sleep so the watchdog (4s stall) does not
        // misclassify a deliberate step_interval as a hang.
        ctx.last_heartbeat.store(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            Ordering::Relaxed,
        );
        thread::sleep(Duration::from_millis(step_interval));
        let now = Instant::now();
        let dt = now.duration_since(state.prev_sample_time).as_secs_f64();
        if state.rates.advance_io(environment.read_tier_disk_io(), dt) {
            state.prev_sample_time = now;
        }
        current_target += ctx.opts.step_pct;
    }
}
