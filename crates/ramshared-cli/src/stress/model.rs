// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/stress/model.rs
//! The typed result and report model of one stress run.
//!
//! Extracted from `stress.rs` as the registered `stress/model.rs` slice
//! (see `workspace module layout`): the published shapes a run
//! hands back and logs, and nothing else. This band reads nothing from the
//! host, allocates no pressure buffers, applies no admission or pacing policy
//! and renders no terminal output — it is the wire form. The measurement that
//! fills it stays in `stress.rs` until its own registered slice.
//!
//! `TelemetryReading` and `StressReport` stay `pub` because they are the
//! published faces: the JSON report and the telemetry log are both this
//! shape, and a consumer outside the module reads them.

use crate::zram_compression::ZramCompression;

/// Compression accounting for the compressed-RAM tier.
///
/// The model, the kernel `mm_stat` contract and the Tier 1 device filter live
/// in [`crate::zram_compression`] — one definition shared with the live status
/// document. The counters are the device's **current** occupancy, not a
/// lifetime total, so they only mean something while the pressure that created
/// them is still resident. A read taken after the ramp drops its buffers, or
/// after a cascade teardown resets the device, measures nothing. That is why
/// the run folds this at sample time beside the swap peaks rather than at
/// report time; the fold itself stays here.

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct TelemetryReading {
    pub timestamp_ms: u64,
    pub pressure_index: f64,
    pub gauge: String,
    pub latency_ms: f64,
    /// Memory PSI full avg10 at this sample, or `None` when there was no
    /// valid reading. `Some(0.0)` is a measured idle; `None` is not a
    /// reading of zero. Rendered by [`psi_mark`], which prints the absence
    /// rather than a `0.0%` the reader would take as measured.
    pub psi_full: Option<f64>,
    pub ram_used_mb: u64,
    /// Swap occupancy at this sample, or `None` when the swap table was not
    /// readable. `Some(0)` is a measured idle; `None` is not a reading of
    /// zero. Rendered by [`mb_mark`], which prints the absence rather than a
    /// `0` the reader would take as measured.
    pub swap_used_mb: Option<u64>,
    /// Swap-tier occupancy against that tier's own device capacity, or `None`
    /// when the tier has no swap device and the ratio has no denominator.
    ///
    /// `None` is never `0`: an empty tier is `Some(0)`, a measured zero of a
    /// real denominator. See [`TierCapacityStats::pct`].
    pub tier1_zram_pct: Option<u64>,
    /// Occupancy of the **logical swap tier** (Tier 2 as a swap device), from
    /// `/proc/swaps`.
    ///
    /// Not the physical GPU cache. The cache has its own figure,
    /// [`StressReport::tier2_vram_pct`], which is a fill ratio against the
    /// cache document's target and has two refusals of its own. This one is
    /// the swap-device occupancy the three-tier row prints, and it shares its
    /// name with [`StressReport::tier2_logical_swap_pct`] because they are one
    /// fact.
    pub tier2_logical_swap_pct: Option<u64>,
    pub tier3_ssd_pct: Option<u64>,
    pub classification: String,
}

impl TelemetryReading {
    pub fn with_tier_pcts(mut self, t1: Option<u64>, t2: Option<u64>, t3: Option<u64>) -> Self {
        self.tier1_zram_pct = t1;
        self.tier2_logical_swap_pct = t2;
        self.tier3_ssd_pct = t3;
        self
    }
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct StressReport {
    #[serde(default)]
    pub metric_version: u32,
    pub battery_mode: bool,
    pub cascade_mode: bool,
    #[serde(default)]
    pub tier3_only: bool,
    #[serde(default)]
    pub tier1_target_pct: u64,
    #[serde(default)]
    pub tier2_target_pct: u64,
    #[serde(default)]
    pub tier3_target_pct: u64,
    #[serde(default)]
    pub physical_cache_required_mib: u64,
    pub max_safe_pct: u64,
    pub total_allocated_mb: u64,
    /// Highest swap occupancy observed during the pressure phase, in MB, or
    /// `null` when the swap table was never read.
    ///
    /// `Some(0)` is a measured empty tier; `null` is a run that has no swap
    /// observation at all. `compare-benchmarks.mjs` refuses to evaluate swap
    /// engagement on an unmeasured figure (`isMeasured` rejects `null`), so
    /// publishing a `0` here would claim an empty tier nobody observed.
    ///
    /// Folded at sample time from the same tier read that produces each
    /// telemetry line, so this equals the maximum `Swap:` value in the
    /// telemetry log. Folding after the step's allocation instead would drop
    /// the halting sample, because every Zero Hang Protection break happens
    /// between the sample and that fold.
    #[serde(default)]
    pub peak_swap_mb: Option<u64>,
    /// Run-wide peak Tier-1 occupancy, in MB, or `null` when no sample folded
    /// in. `Some(0)` is a measured empty tier; `null` is a run that never read
    /// the swap table. See [`PeakSwapOccupancy::published_mib`].
    #[serde(default)]
    pub tier1_zram_mb: Option<u64>,
    /// Run-wide peak Tier-1 occupancy as a percent of that tier's capacity,
    /// or `null` when the ratio has no denominator.
    ///
    /// Numerator and denominator are **co-sampled**: the capacity is the one
    /// from the same `/proc/swaps` observation that set the peak (see
    /// [`PeakSwapOccupancy`]). A post-run capacity re-read is a different
    /// moment and is not this figure.
    ///
    /// `null` means the peak sample had no swap device. A `0` here would
    /// claim an emptiness measured against a capacity that was never there.
    /// A measured empty tier is `Some(0)` — zero occupancy of a capacity that
    /// was there. See [`capacity_share_pct`].
    #[serde(default)]
    pub tier1_zram_pct: Option<u64>,
    /// Compressed-RAM accounting at the saturation sample, from `mm_stat`.
    ///
    /// `None` means it was not measured — the tier had no device, or the
    /// counters were already gone. It is never a synthesized zero.
    #[serde(default)]
    pub tier1_zram_compression: Option<ZramCompression>,
    /// Run-wide peak physical-cache occupancy, in MB, or `null` when the run
    /// never read a cache document. `Some(0)` is a measured empty cache.
    #[serde(default)]
    pub tier2_vram_mb: Option<u64>,
    /// Physical-cache occupancy as a ratio of its reported target, or `null`
    /// when there is no ratio to report.
    ///
    /// Two refusals, and only two. `physical_cache_samples == 0` — the run
    /// never read a cache document, so the percent was never a thing — and a
    /// zero target, where the ratio has no denominator. Both publish `null`;
    /// a `0` here would claim a fullness measured against a target the
    /// document never gave. The companion witness is
    /// [`Self::physical_cache_samples`], the same shape as
    /// [`Self::rate_windows`] for the rate family.
    ///
    /// The ratio compares the run-wide peak occupancy to the run-wide peak
    /// target. Those are two independent folds and can come from different
    /// samples, so this is not "percent of target at any single instant".
    /// Co-sampling them is a separate concern and is not done here.
    #[serde(default)]
    pub tier2_vram_pct: Option<u64>,
    /// Run-wide peak logical-swap occupancy, in MB, or `null` when no sample
    /// folded in. Same contract as [`Self::tier1_zram_mb`].
    #[serde(default)]
    pub tier2_logical_swap_mb: Option<u64>,
    /// Run-wide peak logical-swap occupancy as a percent of that tier's
    /// capacity, or `null` when the ratio has no denominator.
    ///
    /// Same contract as [`Self::tier1_zram_pct`], including the co-sampled
    /// pairing of peak and capacity. This is the **logical swap tier**, not
    /// the physical GPU cache: the cache fill is [`Self::tier2_vram_pct`] and
    /// has two refusals of its own.
    #[serde(default)]
    pub tier2_logical_swap_pct: Option<u64>,
    /// Run-wide peak of the NBD block-traffic rate over every accepted rate
    /// window, or `null` when no window was ever accepted. See
    /// [`Self::tier1_throughput_mbs`].
    #[serde(default)]
    pub tier2_nbd_throughput_mbs: Option<f64>,
    #[serde(default)]
    pub tier2_physical_cache_target_mb: u64,
    #[serde(default)]
    pub simultaneous_physical_cache_target_mib: u64,
    #[serde(default)]
    pub simultaneous_physical_cache_mib: u64,
    #[serde(default)]
    pub simultaneous_full_tiers: bool,
    #[serde(default)]
    pub physical_cache_samples: usize,
    /// Highest fallback-swap (Tier 3 SSD) occupancy observed during the run,
    /// in MB, or `null` when the swap table was never read.
    ///
    /// `Some(0)` is a measured empty fallback; `null` is a run that has no
    /// swap observation at all. This matters more here than anywhere else in
    /// the report: `compare-benchmarks.mjs` alarms on unintended fallback
    /// spill, and a synthesized `0` would read as "no spill observed" and
    /// **suppress** that alarm. `isMeasured` rejects `null`, so an unmeasured
    /// figure is refused for evaluation rather than silently passed.
    ///
    /// The one name for this fact: security and benchmark guidelines names it in
    /// the measurement taxonomy, `HARDWARE-METRICS-TRIAGE.md` scopes its
    /// alarm, and `compare-benchmarks.mjs` gates on it. It was previously
    /// published a second time as `tier3_spillover_mb`, assigned the same
    /// `peak_ssd` under a second name with no reader of either the field or
    /// the name; two labels for one measurement is how a reader ends up
    /// comparing the wrong two numbers, so the duplicate is gone rather than
    /// aliased.
    #[serde(default)]
    pub tier3_ssd_mb: Option<u64>,
    /// Fallback-swap (Tier 3 SSD) occupancy in MB at run start, from the
    /// same `/proc/swaps` fold that produces [`Self::tier3_ssd_mb`].
    ///
    /// The peak alone cannot tell a reader whether the fallback device was
    /// already occupied when the run began, and a pre-existing occupancy
    /// inflates `tier3_ssd_mb` exactly as run-caused spill would. This is the
    /// measured baseline that separates those two readings. It is published
    /// as a measurement only: no run-attributable spill figure is derived
    /// from it here, and
    /// [`Self::tier3_spill_with_free_upper_tiers`] remains the only thing
    /// that adjudicates the unintended-fallback alarm.
    ///
    /// `None` means it was not measured — a record written before this field
    /// existed. It is never a synthesized zero, which would claim the
    /// fallback started empty.
    #[serde(default)]
    pub tier3_ssd_initial_mb: Option<u64>,
    /// Run-wide peak fallback-swap occupancy as a percent of that tier's
    /// capacity, or `null` when the ratio has no denominator.
    ///
    /// Same contract as [`Self::tier1_zram_pct`], including the co-sampled
    /// pairing of peak and capacity.
    #[serde(default)]
    pub tier3_ssd_pct: Option<u64>,
    /// Run-wide peak of the tier-1 reclaim rate over every accepted rate
    /// window, or `null` when no window was ever accepted.
    ///
    /// A present `0.0` is a measured idle — a window that really did see no
    /// traffic — never an empty maximum wearing its face. This is a published
    /// wire form (see [`Self::metric_version`]); the widening from `f64` is a
    /// recorded contract change and does not bump that version, because the
    /// metric set is unchanged and no comparison gate reads the field.
    /// [`Self::rate_windows`] is the witness.
    #[serde(default)]
    pub tier1_throughput_mbs: Option<f64>,
    /// NBD block traffic does not measure GPU DMA throughput. Always `null`.
    #[serde(default)]
    pub tier2_throughput_mbs: Option<f64>,
    /// Run-wide peak of the tier-3 fallback-swap rate over every accepted rate
    /// window, or `null` when no window was ever accepted. See
    /// [`Self::tier1_throughput_mbs`].
    #[serde(default)]
    pub tier3_throughput_mbs: Option<f64>,
    #[serde(default)]
    pub tier2_speedup_vs_ssd: Option<f64>,
    /// How many accepted rate windows have contributed to the peaks above.
    ///
    /// `0` means none have: the peaks are then an empty maximum rather than a
    /// measured zero and publish `null`, which is why a reader can tell the
    /// two apart. One counter covers every tier because a single dt window
    /// produces all of their rates together.
    #[serde(default)]
    pub rate_windows: u64,
    pub peak_pressure_index: f64,
    pub telemetry_readings_count: usize,
    pub active_io_cycles_completed: usize,
    pub reclaim_duration_ms: Option<f64>,
    pub reclaim_speed_gbs: Option<f64>,
    #[serde(default)]
    pub buffer_drop_duration_ms: f64,
    /// Free RAM after the buffers were dropped, in MB, or `null` when the
    /// post-run `/proc/meminfo` read did not succeed. `Some(0)` is a measured
    /// floor; `null` is not a reading of zero free RAM.
    #[serde(default)]
    pub post_reclaim_free_ram_mb: Option<u64>,
    pub status: String,
    #[serde(default)]
    pub avg_cycle_latency_ms: f64,
    #[serde(default)]
    pub p50_cycle_latency_ms: f64,
    #[serde(default)]
    pub p90_cycle_latency_ms: f64,
    #[serde(default)]
    pub p99_cycle_latency_ms: f64,
    #[serde(default)]
    pub max_cycle_latency_ms: f64,
    #[serde(default)]
    pub estimated_page_fault_lat_us: Option<f64>,
    #[serde(default)]
    pub host_vram_min_free_mb: Option<u64>,
    #[serde(default)]
    pub vram_evicted_chunks_count: Option<usize>,
    #[serde(default)]
    pub dma_watchdog_trips_count: Option<u64>,
    /// Samples that observed fallback swap occupancy during the run. `None`
    /// on a record written before this witness existed — never a measured
    /// zero, which would claim no sample looked.
    #[serde(default)]
    pub tier3_spill_samples: Option<u64>,
    /// Of those, samples where tier-1 ZRAM and tier-2 logical swap both still
    /// had free capacity at the same instant. **This is the admission
    /// condition**: non-zero is the contract's unintended-fallback alarm, and
    /// `Some(0)` is the proof it did not fire. `None` is a record that cannot
    /// be adjudicated either way.
    #[serde(default)]
    pub tier3_spill_with_free_upper_tiers: Option<u64>,
    #[serde(default)]
    pub vram_eviction_p99_latency_ms: Option<f64>,
    /// Hung-task lines observed in the kernel ring buffer.
    ///
    /// `Some(0)` is the proof that none fired. `None` is a record of "the ring
    /// buffer was not readable", which answers a different question: a
    /// `dmesg` that will not spawn, or that exits non-zero, leaves the count
    /// unobserved rather than measured at zero. Its neighbours above use the
    /// same face for the same reason.
    #[serde(default)]
    pub kernel_d_state_hung_tasks: Option<u64>,
    /// Optional total soak duration target configured via `--duration-sec`.
    #[serde(default)]
    pub duration_sec: Option<u64>,
    /// Actual wall-clock elapsed duration of the run, in seconds.
    #[serde(default)]
    pub elapsed_sec: f64,
    /// Sustained average Tier-1 ZRAM block rate over the active measurement window.
    #[serde(default)]
    pub tier1_avg_throughput_mbs: Option<f64>,
    #[serde(default)]
    pub tier1_read_mb: Option<u64>,
    #[serde(default)]
    pub tier1_write_mb: Option<u64>,
    /// Sustained average Tier-2 NBD block rate over the active measurement window.
    #[serde(default)]
    pub tier2_nbd_avg_throughput_mbs: Option<f64>,
    #[serde(default)]
    pub tier2_nbd_read_mb: Option<u64>,
    #[serde(default)]
    pub tier2_nbd_write_mb: Option<u64>,
    /// Sustained average Tier-3 SSD fallback rate over the active measurement window.
    #[serde(default)]
    pub tier3_avg_throughput_mbs: Option<f64>,
    #[serde(default)]
    pub tier3_read_mb: Option<u64>,
    #[serde(default)]
    pub tier3_write_mb: Option<u64>,
    /// Peak observed GPU VRAM cache compression and I/O telemetry during the run.
    #[serde(default)]
    pub gpu_cache_telemetry: Option<GpuCacheRunTelemetry>,
}

/// GPU CUDA PTX LZ4 cache compression and I/O counters folded across the run.
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GpuCacheRunTelemetry {
    pub logical_mb: Option<f64>,
    pub compressed_mb: Option<f64>,
    pub raw_mb: Option<f64>,
    pub physical_slab_mb: Option<f64>,
    pub effective_ratio: Option<f64>,
    pub hits: u64,
    pub fallback_reads: u64,
    pub batched_writes: u64,
    pub cache_read_mb: f64,
    pub integrity_errors: u64,
    pub decode_errors: u64,
    pub timeouts: u64,
}
