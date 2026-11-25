// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/stress/measure.rs
//! The measurement band of one stress run: peaks, rate windows, percentiles
//! and the KiB↔MiB publish conversions.
//!
//! Extracted from `stress.rs` as the registered `stress/measure.rs` slice
//! (see `workspace module layout`). This band turns already-read
//! counters into the figures a run publishes. It reads nothing from the host
//! itself — the caller supplies every counter — applies no admission or pacing
//! policy and renders no terminal output.
//!
//! The conversions live here rather than at each call site because a wrong one
//! is invisible at the call site: floor division turns a present 511 KiB
//! reading into a measured `0`, and a ratio taken between two rounded MiB
//! faces is not the ratio that was measured. `published_mib` is the only
//! whole-MiB **view** in this module for exactly that reason.

use std::time::{Duration, Instant};

use crate::zram_compression::ZramCompression;

use super::StressOptions;
use super::classify::{StressMode, stress_passes};
use super::model::{GpuCacheRunTelemetry, StressReport};
use super::phase_ramp::StressRunState;

/// Narrowest rate window a byte delta is divided by.
///
/// Below this `dt` the division invents a rate rather than measuring one, so
/// the window is refused and the delta is kept for the next attempt.
pub(super) const RATE_WINDOW_MIN_DT_SECS: f64 = 0.05;

/// Keep the compressed-RAM sample that holds the most uncompressed data.
///
/// That sample is the saturation point the triage ratio is asked about. Folded
/// here, at sample time, for the reason the swap peaks are: every safety break
/// lands between the sample and its fold, and by report time the ramp's buffers
/// are dropped and the device counters with them.
pub(super) fn fold_peak_zram_compression(
    peak: &mut Option<ZramCompression>,
    sample: Option<ZramCompression>,
) {
    let Some(sample) = sample else {
        return;
    };
    let is_worse = peak
        .as_ref()
        .is_none_or(|current| sample.orig_data_bytes > current.orig_data_bytes);
    if is_worse {
        *peak = Some(sample);
    }
}

/// KiB in one MiB. The cache document and the report disagree on scale by
/// exactly this, and it is the only conversion factor in this module.
pub(super) const KIB_PER_MIB: u64 = 1024;

/// The whole-MiB **publish** conversion, in one place.
///
/// Ceiling division on purpose. A report field that names MiB has to round
/// somehow; floor division is what turns a present 511 KiB reading into a
/// measured `0`, and `round-to-nearest` turns 1..=511 KiB into the same lie
/// from the other direction. A non-zero measurement must never publish as
/// zero; a measured zero stays a zero.
///
/// This is a **view** function. Every caller below already holds the figure in
/// KiB and only calls this when something on screen or on the wire names MiB.
/// Feeding a result back into a comparison or a percentage is exactly the
/// mistake this name is here to make hard: see [`TierCapacityStats::pct`].
pub(super) fn published_mib(kib: u64) -> u64 {
    kib.div_ceil(KIB_PER_MIB)
}

/// The physical-cache fill ratio, or its refusal.
///
/// A ratio needs both a numerator that was measured and a denominator that
/// exists. `samples == 0` is the first refusal: the run never read a cache
/// document, so the percent was never a thing — the same witness
/// [`StressReport::physical_cache_samples`] publishes. `peak_target_kib == 0`
/// is the second: the samples were taken but reported no target, so there is
/// nothing to be full *of*. Both publish `None`, never a fabricated `0%`,
/// which would claim a fullness measured against a denominator that does not
/// exist.
///
/// The comparison itself stays in KiB — the unit the samples carry. Rounding
/// the peaks to whole MiB first would move the number off the ratio the cache
/// document reported (512 / 2048 KiB is 25%; its MiB halves say 50%).
pub(super) fn cache_fill_pct(
    samples: usize,
    peak_cached_kib: u64,
    peak_target_kib: u64,
) -> Option<u64> {
    if samples == 0 || peak_target_kib == 0 {
        return None;
    }
    Some(peak_cached_kib.saturating_mul(100) / peak_target_kib)
}

/// A run-wide peak swap occupancy, paired with the capacity it was taken against.
///
/// The pairing is the measurement. A run-wide peak
/// and a capacity re-read after the run are two moments: a device that held
/// memory at peak and is gone at report time divides by a zero denominator,
/// and even when both figures exist they describe different snapshots. The
/// capacity is therefore folded beside the peak at sample time — the same move
/// [`fold_peak_zram_compression`] makes for its companion figure — so the
/// published percent is one observation, never a ratio of two.
///
/// Only measured figures live here. No "run-attributable" occupancy is
/// derived: `peak_kib` is the highest used-swap the run observed and
/// `capacity_kib` is the device capacity in that same sample.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct PeakSwapOccupancy {
    /// Highest used-swap the run observed, or `None` when no sample folded in.
    ///
    /// `None` is not a measured `0`: a run that never read the swap table
    /// must not publish an empty tier.
    pub(super) peak_kib: Option<u64>,
    /// Capacity from the same `/proc/swaps` sample that set `peak_kib`.
    pub(super) capacity_kib: u64,
}

impl PeakSwapOccupancy {
    /// Fold one sample's used-and-capacity pair.
    ///
    /// A strictly higher occupancy replaces both figures. A sample that only
    /// matches the running peak fills in a missing denominator, so a measured
    /// empty tier (`0` of a capacity that was there) keeps that capacity and
    /// publishes `Some(0)` rather than the no-device refusal. A sample below
    /// the peak leaves the pair alone: a later vanished device must not become
    /// the denominator of an earlier occupancy.
    ///
    /// The first sample establishes the pair whatever its value: a measured
    /// `0` of a real capacity is an empty tier, and `0` of a zero capacity is
    /// a tier with no device. Neither is "never observed".
    pub(super) fn fold(&mut self, sample_used_kib: u64, sample_capacity_kib: u64) {
        match self.peak_kib {
            None => {
                self.peak_kib = Some(sample_used_kib);
                self.capacity_kib = sample_capacity_kib;
            }
            Some(peak) => {
                if sample_used_kib > peak {
                    self.peak_kib = Some(sample_used_kib);
                    self.capacity_kib = sample_capacity_kib;
                } else if sample_used_kib == peak && self.capacity_kib == 0 {
                    self.capacity_kib = sample_capacity_kib;
                }
            }
        }
    }

    /// The peak as a whole-MiB publish view, or its absence.
    pub(super) fn published_mib(&self) -> Option<u64> {
        self.peak_kib.map(published_mib)
    }

    /// The paired occupancy percent, or its refusal. See [`capacity_share_pct`].
    ///
    /// `None` for a never-observed peak as well as for a peak with no
    /// denominator: neither is a ratio that exists.
    pub(super) fn pct(&self) -> Option<u64> {
        capacity_share_pct(self.peak_kib?, self.capacity_kib)
    }
}

/// One swap tier's run-wide peak occupancy as a percent of its capacity, or
/// its refusal.
///
/// [`cache_fill_pct`]'s shape, for the three swap tiers: a ratio needs a
/// denominator that exists. `capacity_kib == 0` means the peak sample had no
/// swap device, so no percent of that is a thing — `None`, never `0`.
///
/// Both figures are **co-sampled**: `peak_kib` and `capacity_kib` come from
/// the same `/proc/swaps` observation, folded together by
/// [`PeakSwapOccupancy::fold`]. A post-run capacity re-read is a different
/// moment and is not this denominator.
///
/// The `0` refusal is still the point of the option. A device that never had
/// a denominator at the peak sample cannot publish `0%` — a real occupancy
/// reported as an empty tier. `None` says the ratio was never a thing. A
/// measured empty tier is `Some(0)`: zero occupancy of a capacity that was
/// there.
///
/// The comparison stays in KiB. Rounding the peak to whole MiB first would
/// move the number off the ratio `/proc/swaps` reported (1536 / 2048 KiB is
/// 75%; its MiB halves say 50%).
pub(super) fn capacity_share_pct(peak_kib: u64, capacity_kib: u64) -> Option<u64> {
    if capacity_kib == 0 {
        return None;
    }
    Some(peak_kib.saturating_mul(100) / capacity_kib)
}

pub fn compute_latency_percentiles(latencies: &[f64]) -> (f64, f64, f64, f64, f64) {
    if latencies.is_empty() {
        return (0.0, 0.0, 0.0, 0.0, 0.0);
    }
    let mut sorted = latencies.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let len = sorted.len();
    let sum: f64 = sorted.iter().sum();
    let avg = sum / len as f64;
    let p50 = sorted[(len as f64 * 0.50) as usize % len];
    let p90 = sorted[(len as f64 * 0.90) as usize % len];
    let p99 = sorted[(len as f64 * 0.99) as usize % len];
    let max = *sorted.last().unwrap_or(&0.0);
    (
        (avg * 10000.0).round() / 10000.0,
        (p50 * 10000.0).round() / 10000.0,
        (p90 * 10000.0).round() / 10000.0,
        (p99 * 10000.0).round() / 10000.0,
        (max * 10000.0).round() / 10000.0,
    )
}

/// Separate per-tier block I/O read/write byte counters from `/proc/diskstats`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct TierDiskIoBytes {
    pub(super) zram_read: u64,
    pub(super) zram_write: u64,
    pub(super) vram_read: u64,
    pub(super) vram_write: u64,
    pub(super) ssd_read: u64,
    pub(super) ssd_write: u64,
}

impl TierDiskIoBytes {
    pub(super) fn from_totals(totals: (u64, u64, u64)) -> Self {
        Self {
            zram_write: totals.0,
            vram_write: totals.1,
            ssd_write: totals.2,
            ..Self::default()
        }
    }

    pub(super) fn totals(self) -> (u64, u64, u64) {
        (
            self.zram_read.saturating_add(self.zram_write),
            self.vram_read.saturating_add(self.vram_write),
            self.ssd_read.saturating_add(self.ssd_write),
        )
    }
}

/// Accumulates tier byte counters into run-wide peak rates, sustained averages,
/// and per-tier read/write byte deltas.
pub(super) struct TierRateSampler {
    start_io: TierDiskIoBytes,
    last_io: TierDiskIoBytes,
    prev_z_bytes: u64,
    prev_v_bytes: u64,
    prev_s_bytes: u64,
    peak_zram: f64,
    peak_vram: f64,
    peak_ssd: f64,
    total_dt_secs: f64,
    pub(super) windows: u64,
}

impl TierRateSampler {
    /// Starts from an initial counter reading, with no window accepted yet.
    #[cfg(test)]
    pub(super) fn new(bytes: (u64, u64, u64)) -> Self {
        Self::with_io(TierDiskIoBytes::from_totals(bytes))
    }

    pub(super) fn with_io(io: TierDiskIoBytes) -> Self {
        let totals = io.totals();
        Self {
            start_io: io,
            last_io: io,
            prev_z_bytes: totals.0,
            prev_v_bytes: totals.1,
            prev_s_bytes: totals.2,
            peak_zram: 0.0,
            peak_vram: 0.0,
            peak_ssd: 0.0,
            total_dt_secs: 0.0,
            windows: 0,
        }
    }

    #[cfg(test)]
    pub(super) fn advance(&mut self, cur: (u64, u64, u64), dt_secs: f64) -> bool {
        self.advance_io(TierDiskIoBytes::from_totals(cur), dt_secs)
    }

    /// Folds one read/write counter snapshot into peak and sustained averages.
    pub(super) fn advance_io(&mut self, cur_io: TierDiskIoBytes, dt_secs: f64) -> bool {
        if dt_secs <= RATE_WINDOW_MIN_DT_SECS {
            return false;
        }
        let cur = cur_io.totals();
        let per_second =
            |prev: u64, cur: u64| cur.saturating_sub(prev) as f64 / (1024.0 * 1024.0) / dt_secs;
        self.peak_zram = self.peak_zram.max(per_second(self.prev_z_bytes, cur.0));
        self.peak_vram = self.peak_vram.max(per_second(self.prev_v_bytes, cur.1));
        self.peak_ssd = self.peak_ssd.max(per_second(self.prev_s_bytes, cur.2));
        self.prev_z_bytes = cur.0;
        self.prev_v_bytes = cur.1;
        self.prev_s_bytes = cur.2;
        self.last_io = cur_io;
        self.total_dt_secs += dt_secs;
        self.windows += 1;
        true
    }

    pub(super) fn peak_zram_mbs(&self) -> Option<f64> {
        self.publish(self.peak_zram)
    }

    pub(super) fn peak_vram_mbs(&self) -> Option<f64> {
        self.publish(self.peak_vram)
    }

    pub(super) fn peak_ssd_mbs(&self) -> Option<f64> {
        self.publish(self.peak_ssd)
    }

    pub(super) fn avg_zram_mbs(&self) -> Option<f64> {
        self.sustained_rate(self.start_io.totals().0, self.last_io.totals().0)
    }

    pub(super) fn avg_vram_mbs(&self) -> Option<f64> {
        self.sustained_rate(self.start_io.totals().1, self.last_io.totals().1)
    }

    pub(super) fn avg_ssd_mbs(&self) -> Option<f64> {
        self.sustained_rate(self.start_io.totals().2, self.last_io.totals().2)
    }

    pub(super) fn delta_mb(&self, selector: fn(TierDiskIoBytes) -> u64) -> Option<u64> {
        (self.windows > 0).then_some(
            selector(self.last_io).saturating_sub(selector(self.start_io)) / (1024 * 1024),
        )
    }

    fn sustained_rate(&self, start_bytes: u64, end_bytes: u64) -> Option<f64> {
        if self.windows == 0 || self.total_dt_secs <= RATE_WINDOW_MIN_DT_SECS {
            return None;
        }
        let mbs =
            end_bytes.saturating_sub(start_bytes) as f64 / (1024.0 * 1024.0) / self.total_dt_secs;
        Some((mbs * 10.0).round() / 10.0)
    }

    /// The published form of a peak rate.
    ///
    /// Peaks round to one decimal because that is the figure the summary
    /// prints. `None` when no window was ever accepted — an empty maximum
    /// never wears the face of a measured idle.
    fn publish(&self, peak_mbs: f64) -> Option<f64> {
        (self.windows > 0).then_some((peak_mbs * 10.0).round() / 10.0)
    }
}

/// Compute Phase 2/3 hold end timestamp, honoring optional `--duration-sec` total soak target.
pub(super) fn sustained_hold_deadline(
    run_start: Instant,
    now: Instant,
    hold_sec: u64,
    duration_sec: Option<u64>,
) -> Instant {
    let min_hold_end = now + Duration::from_secs(hold_sec);
    match duration_sec {
        Some(total_sec) => (run_start + Duration::from_secs(total_sec)).max(min_hold_end),
        None => min_hold_end,
    }
}

fn round2(val: f64) -> f64 {
    (val * 100.0).round() / 100.0
}

/// Parse one `/run/ramshared/cache-status.json` document into typed GPU cache run telemetry.
pub(super) fn parse_gpu_cache_run_telemetry(
    value: &serde_json::Value,
) -> Option<GpuCacheRunTelemetry> {
    let gc = value.get("gpu_cache")?;
    let logical_b = gc.get("logical_cached_bytes")?.as_u64()?;
    let compressed_b = gc.get("compressed_payload_bytes")?.as_u64()?;
    let raw_b = gc.get("raw_payload_bytes")?.as_u64()?;
    let slab_b = gc.get("physical_cache_slab_bytes")?.as_u64()?;
    let stored_b = compressed_b.saturating_add(raw_b);
    let effective_ratio =
        (stored_b > 0 && logical_b > 0).then(|| round2(logical_b as f64 / stored_b as f64));
    let mib = |b: u64| round2(b as f64 / (1024.0 * 1024.0));
    let io = value.get("origin_cache_io");
    Some(GpuCacheRunTelemetry {
        logical_mb: Some(mib(logical_b)),
        compressed_mb: Some(mib(compressed_b)),
        raw_mb: Some(mib(raw_b)),
        physical_slab_mb: Some(mib(slab_b)),
        effective_ratio,
        hits: value
            .get("cache_hits")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        fallback_reads: value
            .get("cache_fallback_reads")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        batched_writes: io
            .and_then(|v| v.get("batched_writes"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cache_read_mb: round2(
            io.and_then(|v| v.get("cache_read_bytes"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as f64
                / (1024.0 * 1024.0),
        ),
        integrity_errors: gc
            .get("codec_integrity_errors")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        decode_errors: gc
            .get("codec_decode_errors")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        timeouts: gc
            .get("codec_timeouts")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    })
}

/// Retain the peak GPU cache occupancy and cumulative I/O counters across samples.
pub(super) fn fold_gpu_cache_run_telemetry(
    peak: &mut Option<GpuCacheRunTelemetry>,
    cur: Option<GpuCacheRunTelemetry>,
) {
    let Some(sample) = cur else {
        return;
    };
    let Some(existing) = peak else {
        *peak = Some(sample);
        return;
    };
    let sample_logical = sample.logical_mb.unwrap_or(0.0);
    let existing_logical = existing.logical_mb.unwrap_or(0.0);
    if sample_logical >= existing_logical && sample_logical > 0.0 {
        existing.logical_mb = sample.logical_mb;
        existing.compressed_mb = sample.compressed_mb;
        existing.raw_mb = sample.raw_mb;
        existing.physical_slab_mb = sample.physical_slab_mb;
        existing.effective_ratio = sample.effective_ratio;
    }
    existing.hits = existing.hits.max(sample.hits);
    existing.fallback_reads = existing.fallback_reads.max(sample.fallback_reads);
    existing.batched_writes = existing.batched_writes.max(sample.batched_writes);
    existing.cache_read_mb = existing.cache_read_mb.max(sample.cache_read_mb);
    existing.integrity_errors = existing.integrity_errors.max(sample.integrity_errors);
    existing.decode_errors = existing.decode_errors.max(sample.decode_errors);
    existing.timeouts = existing.timeouts.max(sample.timeouts);
}

pub(super) struct ReportBuildInputs<'a> {
    pub(super) opts: &'a StressOptions,
    pub(super) state: &'a StressRunState,
    pub(super) cascade_mode: bool,
    pub(super) physical_cache_required_mib: u64,
    pub(super) initial_ssd_used_kib: Option<u64>,
    pub(super) kernel_faults: Option<u64>,
    pub(super) hung_tasks: Option<u64>,
    pub(super) post_free_kib: Option<u64>,
    pub(super) reclaim_duration: Duration,
    pub(super) elapsed: Duration,
}

/// Assemble the complete typed [`StressReport`] from accumulated measurements.
pub(super) fn build_stress_report(inputs: ReportBuildInputs<'_>) -> StressReport {
    let ReportBuildInputs {
        opts,
        state,
        cascade_mode,
        physical_cache_required_mib,
        initial_ssd_used_kib,
        kernel_faults,
        hung_tasks,
        post_free_kib,
        reclaim_duration,
        elapsed,
    } = inputs;
    let (
        avg_cycle_latency_ms,
        p50_cycle_latency_ms,
        p90_cycle_latency_ms,
        p99_cycle_latency_ms,
        max_cycle_latency_ms,
    ) = compute_latency_percentiles(&state.latencies_ms);
    let mut report = StressReport {
        metric_version: 2,
        battery_mode: opts.battery,
        cascade_mode,
        tier3_only: opts.tier3_only,
        tier1_target_pct: opts.tier1_target_pct,
        tier2_target_pct: opts.tier2_target_pct,
        tier3_target_pct: opts.tier3_target_pct.unwrap_or(100),
        physical_cache_required_mib,
        max_safe_pct: state.max_safe_pct,
        total_allocated_mb: state.total_allocated_mb,
        peak_swap_mb: state.peak_total_swap_kib.map(published_mib),
        tier1_zram_mb: state.peak_zram.published_mib(),
        tier1_zram_compression: state.peak_zram_compression.clone(),
        tier1_zram_pct: state.peak_zram.pct(),
        tier2_vram_mb: (state.physical_cache_samples > 0)
            .then(|| published_mib(state.peak_physical_vram_kib)),
        tier2_vram_pct: cache_fill_pct(
            state.physical_cache_samples,
            state.peak_physical_vram_kib,
            state.peak_physical_target_kib,
        ),
        tier2_logical_swap_mb: state.peak_vram.published_mib(),
        tier2_logical_swap_pct: state.peak_vram.pct(),
        tier2_nbd_throughput_mbs: state.rates.peak_vram_mbs(),
        tier2_physical_cache_target_mb: published_mib(state.peak_physical_target_kib),
        simultaneous_physical_cache_target_mib: state.simultaneous_physical_cache_target,
        simultaneous_physical_cache_mib: state.simultaneous_physical_cache,
        simultaneous_full_tiers: state.simultaneous_full_tiers,
        physical_cache_samples: state.physical_cache_samples,
        tier3_ssd_mb: state.peak_ssd.published_mib(),
        tier3_ssd_initial_mb: initial_ssd_used_kib.map(published_mib),
        tier3_ssd_pct: state.peak_ssd.pct(),
        tier1_throughput_mbs: state.rates.peak_zram_mbs(),
        tier2_throughput_mbs: None,
        tier3_throughput_mbs: state.rates.peak_ssd_mbs(),
        tier2_speedup_vs_ssd: None,
        rate_windows: state.rates.windows,
        peak_pressure_index: state.peak_pressure,
        telemetry_readings_count: state.readings_count,
        active_io_cycles_completed: state.active_cycles_done,
        reclaim_duration_ms: None,
        reclaim_speed_gbs: None,
        buffer_drop_duration_ms: reclaim_duration.as_secs_f64() * 1000.0,
        post_reclaim_free_ram_mb: post_free_kib.map(published_mib),
        status: if stress_passes(
            kernel_faults,
            if opts.tier3_only {
                StressMode::Tier3Only
            } else {
                StressMode::FullCascade
            },
            state.physical_cache_samples,
            state.cache_lost_during_stress,
            state.safety_halt,
            if opts.tier3_only {
                state.tier3_target_achieved
            } else {
                state.simultaneous_full_tiers
            },
            state.peak_pressure,
        ) {
            "PASS_ZERO_PANIC".to_string()
        } else {
            "INCONCLUSIVE".to_string()
        },
        avg_cycle_latency_ms,
        p50_cycle_latency_ms,
        p90_cycle_latency_ms,
        p99_cycle_latency_ms,
        max_cycle_latency_ms,
        tier3_spill_samples: Some(state.tier3_admission.spill_samples),
        tier3_spill_with_free_upper_tiers: Some(state.tier3_admission.spill_with_free_upper_tiers),
        kernel_d_state_hung_tasks: hung_tasks,
        ..StressReport::default()
    };
    apply_sustained_report_telemetry(
        &mut report,
        &state.rates,
        state.peak_gpu_cache,
        opts.duration_sec,
        elapsed,
    );
    report
}

/// Populate sustained I/O throughput, cumulative read/write MiB, elapsed time,
/// and GPU CUDA cache compression telemetry on a finalized [`StressReport`].
pub(super) fn apply_sustained_report_telemetry(
    report: &mut StressReport,
    rates: &TierRateSampler,
    gpu_cache: Option<GpuCacheRunTelemetry>,
    duration_sec: Option<u64>,
    elapsed: Duration,
) {
    report.duration_sec = duration_sec;
    report.elapsed_sec = (elapsed.as_secs_f64() * 100.0).round() / 100.0;
    report.tier1_avg_throughput_mbs = rates.avg_zram_mbs();
    report.tier1_read_mb = rates.delta_mb(|io| io.zram_read);
    report.tier1_write_mb = rates.delta_mb(|io| io.zram_write);
    report.tier2_nbd_avg_throughput_mbs = rates.avg_vram_mbs();
    report.tier2_nbd_read_mb = rates.delta_mb(|io| io.vram_read);
    report.tier2_nbd_write_mb = rates.delta_mb(|io| io.vram_write);
    report.tier3_avg_throughput_mbs = rates.avg_ssd_mbs();
    report.tier3_read_mb = rates.delta_mb(|io| io.ssd_read);
    report.tier3_write_mb = rates.delta_mb(|io| io.ssd_write);
    report.gpu_cache_telemetry = gpu_cache;
}
