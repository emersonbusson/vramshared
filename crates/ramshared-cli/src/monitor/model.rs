// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/model.rs
//! Snapshot model for the observability stream.
//!
//! Pure data and the small accessors a reader needs to look inside a snapshot.
//! This module opens nothing, probes nothing, classifies nothing and renders
//! nothing: bounded reads live in the parent module, health/pressure decisions
//! live beside them, and terminal widgets live in the TUI code.
//!
//! Every struct here is also a wire form — the JSONL stream and the persisted
//! snapshot share these shapes — so field names and `serde` renames are part of
//! the published contract and must not be reshaped by a refactor.

use std::collections::BTreeMap;

use ramshared_vram::GpuBudgetSource;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The `/proc/meminfo` figures this observer publishes, and their absence.
///
/// Each of the four core counters is `Option<u64>`: a kernel that never
/// exported a counter and a value that did not parse are `None` and serialize
/// as `null`. A present `0` serializes as `0`. The previous plain `u64` fields
/// zero-filled all four behind `required_counters_available`, so a JSONL reader
/// who skipped the flag got a confident `0` for a counter that was never read —
/// the same fold this series has been removing on the per-process and tier
/// faces.
///
/// A pair is **one** measurement. The RAM pair (`total_kib`, `available_kib`)
/// and the swap pair (`swap_total_kib`, `swap_free_kib`) fail independently,
/// and each is `None` together when either half is absent or the halves
/// disagree (`available > total`, `swap_free > swap_total`). A tear has the
/// same face as absence — "this frame has no figure it can stand behind" —
/// which is the rule [`super::render::compression_row`] already applies to an
/// impossible count pair. `MemTotal: 0` is not a reading (a machine with no
/// RAM is a tear, not a measured empty machine); measured empty swap
/// (`SwapTotal: 0`, `SwapFree: 0`) stays `Some(0)`.
///
/// This is a published wire form (see the module docs). The widening from
/// `u64` and the removal of `required_counters_available` are recorded
/// contract changes: the flag was a side-channel that explained the
/// zero-fill, and the fields now carry the distinction themselves.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct MemoryObservation {
    pub total_kib: Option<u64>,
    pub available_kib: Option<u64>,
    pub swap_total_kib: Option<u64>,
    pub swap_free_kib: Option<u64>,
    pub anon_pages_kib: Option<u64>,
    pub shmem_kib: Option<u64>,
    pub slab_kib: Option<u64>,
    pub s_unreclaim_kib: Option<u64>,
    pub dirty_kib: Option<u64>,
    pub writeback_kib: Option<u64>,
}

impl MemoryObservation {
    /// The RAM pair as one coherent measurement, or `None` when either
    /// counter is absent or the pair disagrees with itself.
    ///
    /// `total > 0` is part of the contract: a machine reporting no RAM is a
    /// tear, not a measured empty machine, and has no reading. The same
    /// predicate the resource-inventory face already applies to these four
    /// counters (`resource_config.rs`).
    pub(super) fn ram_pair(&self) -> Option<(u64, u64)> {
        let total = self.total_kib?;
        let available = self.available_kib?;
        (total > 0 && available <= total).then_some((total, available))
    }

    /// The swap pair as one coherent measurement, or `None` when either
    /// counter is absent or the pair disagrees with itself.
    ///
    /// Measured empty swap (`SwapTotal: 0`, `SwapFree: 0`) is `Some((0, 0))`:
    /// a host with no swap is a product state, not a measurement gap.
    pub(super) fn swap_pair(&self) -> Option<(u64, u64)> {
        let swap_total = self.swap_total_kib?;
        let swap_free = self.swap_free_kib?;
        (swap_free <= swap_total).then_some((swap_total, swap_free))
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct HyperVBalloonObservation {
    pub debugfs_status: String,
    pub nr_balloon_pages: Option<u64>,
    pub host_version: Option<String>,
    pub capabilities: Option<String>,
    pub state: Option<String>,
    pub page_size: Option<u64>,
    pub pages_added: Option<u64>,
    pub pages_onlined: Option<u64>,
    pub pages_ballooned: Option<u64>,
    pub total_pages_committed: Option<u64>,
    pub max_dynamic_page_count: Option<u64>,
}

/// Process memory totals over every visible process, before any top-N cut.
///
/// Each figure is fail-closed: `Some` only when **every** visible process
/// contributed that figure. One missing per-process figure makes the matching
/// total `None`, never a partial sum presented as a total. An empty process
/// list is a measured empty set and totals to `Some(0)`, the same way a measured
/// empty reservation table totals to zero.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ProcessMemoryTotals {
    pub visible_processes: u64,
    pub rss_kib: Option<u64>,
    pub swap_kib: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct CgroupMemoryObservation {
    pub status: String,
    pub current_bytes: Option<u64>,
    pub events: Option<MemoryEvents>,
    pub subgroup_current_bytes: Option<u64>,
    pub subgroups_with_memory: u64,
    pub root_direct_processes: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    LinuxHost,
    Wsl,
    Wsl2,
}

impl MemoryScope {
    pub(super) fn ram_label(self) -> &'static str {
        match self {
            Self::LinuxHost => "Host RAM",
            Self::Wsl => "WSL Guest RAM",
            Self::Wsl2 => "WSL2 Guest RAM",
        }
    }

    pub(super) fn panel_title(self) -> &'static str {
        match self {
            Self::LinuxHost => "Host RAM & Swap",
            Self::Wsl => "WSL Guest RAM & Swap",
            Self::Wsl2 => "WSL2 Guest RAM & Swap",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LatencySource {
    #[default]
    Unavailable,
    Estimated,
    Measured,
}

/// Per-tier I/O counters, the rates derived from them, and their window extrema.
///
/// The `*_bytes` counters are monotonically observed and always present. The
/// instantaneous `read_mbs` / `write_mbs` are `Option` on purpose: each is the
/// byte delta across one accepted rate window divided by that window, so a
/// frame whose window was refused — the near-zero dt of the first refresh, or
/// a gap too wide after a blocked refresh — has no rate at all. This struct is
/// a published wire form (see the module docs), so those fields serialize as
/// `null`, which is the established "not measured" marker in this stream — the
/// same one `gpu_budget` and `anon_pages_kib` use. A plain `f64` turned "no
/// window yet" into a measured `0.0`, which is how the first frame of every run
/// reported an idle nobody had observed. A present `0.0` is a measured idle.
///
/// The run-wide peaks are `Option` for the same reason and across the whole
/// run rather than one frame. Each is a maximum over a sample set, and a
/// maximum over an empty set is not a number: [`ControlPlaneObservation::rate_windows`]
/// counts the accepted windows that fed them and is `0` before the first one.
/// A present `0.0` is then a measured idle — a window that really did see no
/// traffic — never an empty maximum wearing its face.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
pub struct TierIoStats {
    pub read_bytes: u64,
    pub write_bytes: u64,
    /// Rate over the last accepted window, or `null` when this frame has none.
    #[serde(default)]
    pub read_mbs: Option<f64>,
    /// Rate over the last accepted window, or `null` when this frame has none.
    #[serde(default)]
    pub write_mbs: Option<f64>,
    /// Run-wide peak of the read rate over every accepted window, or `null`
    /// when no window has been accepted.
    #[serde(default)]
    pub read_peak_mbs: Option<f64>,
    /// Run-wide peak of the write rate over every accepted window, or `null`
    /// when no window has been accepted.
    #[serde(default)]
    pub write_peak_mbs: Option<f64>,
    /// Busy-band extrema of the combined rate: the sample set is the accepted
    /// windows whose combined rate reached the busy floor, so the trio is
    /// `null` together whenever that set is empty. A tier that was observed
    /// only below the floor has no busy minimum, average or maximum — those
    /// are an empty set, not measured zeros. The directional peaks above take
    /// every window and are therefore independent of this.
    #[serde(default)]
    pub min_mbs: Option<f64>,
    /// See [`TierIoStats::min_mbs`].
    #[serde(default)]
    pub avg_mbs: Option<f64>,
    /// See [`TierIoStats::min_mbs`].
    #[serde(default)]
    pub max_mbs: Option<f64>,
    /// See [`TierIoStats::min_mbs`]. Mirrors `max_mbs` on the wire.
    #[serde(default)]
    pub peak_mbs: Option<f64>,
    pub min_lat_us: f64,
    pub avg_lat_us: f64,
    pub max_lat_us: f64,
    #[serde(default)]
    pub latency_source: LatencySource,
}

impl TierIoStats {
    /// Whether this frame carries a rate derived from an accepted window.
    ///
    /// Both instantaneous rates are written from that one window, so a
    /// half-populated pair is not a sample: absence wins, the same fail-closed
    /// rule the counter columns apply to an incomplete tier total.
    pub(super) fn has_rate_sample(&self) -> bool {
        self.read_mbs.is_some() && self.write_mbs.is_some()
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ControlPlaneObservation {
    #[serde(default)]
    pub memory_psi_available: bool,
    pub memory_psi_some_avg10: f64,
    pub memory_psi_some_avg60: f64,
    pub memory_psi_some_avg300: f64,
    pub memory_psi_full_avg10: f64,
    pub memory_psi_full_avg60: f64,
    pub memory_psi_full_avg300: f64,
    /// Cumulative base pages swapped in, or `null` when `/proc/vmstat` did not
    /// report `pswpin`. A machine that has swapped nothing is a real `0`; a
    /// counter the kernel did not export, a value that does not parse and a
    /// body that could not be read are absence. This is a published wire form
    /// (see the module docs) and the envelope has no version field, so the
    /// widening from `u64` is a recorded contract change.
    #[serde(default)]
    pub swap_in_pages: Option<u64>,
    /// Cumulative base pages swapped out, or `null` when `pswpout` was not
    /// reported. See [`Self::swap_in_pages`].
    #[serde(default)]
    pub swap_out_pages: Option<u64>,
    pub swap_read_bytes: u64,
    pub swap_write_bytes: u64,
    /// Swap-plane rate over the last accepted window, or `null` when this frame
    /// has none. See [`TierIoStats::read_mbs`].
    #[serde(default)]
    pub swap_read_mbs: Option<f64>,
    /// Swap-plane rate over the last accepted window, or `null` when this frame
    /// has none. See [`TierIoStats::read_mbs`].
    #[serde(default)]
    pub swap_write_mbs: Option<f64>,
    /// Run-wide peak of the swap-plane read rate over every accepted window,
    /// or `null` when no window has been accepted. See [`TierIoStats::read_peak_mbs`].
    #[serde(default)]
    pub swap_read_peak_mbs: Option<f64>,
    /// Run-wide peak of the swap-plane write rate over every accepted window,
    /// or `null` when no window has been accepted. See [`TierIoStats::read_peak_mbs`].
    #[serde(default)]
    pub swap_write_peak_mbs: Option<f64>,
    /// Run-wide peak of the combined swap-plane rate over every accepted
    /// window, or `null` when no window has been accepted. See
    /// [`TierIoStats::read_peak_mbs`].
    #[serde(default)]
    pub swap_peak_mbs: Option<f64>,
    /// How many accepted rate windows have contributed to the rates and peaks
    /// on this plane. `0` means none have: the peaks are then an empty maximum
    /// rather than a measured zero, and must not be read as one — which is why
    /// they publish `null` rather than a number. One counter covers every tier
    /// because a single dt window produces all of their rates together. This is
    /// a published wire form (see the module docs); the field is new and older
    /// envelopes simply lack it.
    #[serde(default)]
    pub rate_windows: u64,
    /// Run-wide peak of the tier's used capacity, in **KiB** — the unit the
    /// `/proc/swaps` measurement carries. The fold is a maximum over exact
    /// samples: converting each sample to whole MiB before the comparison
    /// would round them all first and could publish a peak that was never
    /// observed. The whole-MiB figure an operator reads is a publish
    /// conversion at the render boundary, not this field's unit.
    ///
    /// `null` when no sample ever contributed a `used_kib`: a maximum over an
    /// empty set is not a measured `0`, and publishing one would tell the
    /// operator this tier peaked at nothing when the wire never spoke about
    /// its occupancy. Same contract as [`Self::swap_peak_mbs`]. This is a
    /// published wire form (see the module docs) and the envelope has no
    /// version field, so the widening from `u64` is a recorded contract
    /// change.
    #[serde(default)]
    pub zram_peak_used_kib: Option<u64>,
    /// See [`Self::zram_peak_used_kib`].
    #[serde(default)]
    pub vram_peak_used_kib: Option<u64>,
    /// See [`Self::zram_peak_used_kib`].
    #[serde(default)]
    pub disk_peak_used_kib: Option<u64>,
    pub zram_io: TierIoStats,
    pub vram_io: TierIoStats,
    pub disk_io: TierIoStats,
    /// Provenance of the per-tier and total swap byte counters (`sysfs_stat`,
    /// `diskstats_crosscheck`, or combined) per SPEC `topology-aware-observability` DT-4.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier_io_source: Option<String>,
    pub boot_tier_latency_ms: Option<u64>,
    /// Seconds since the machine booted, or `null` when `/proc/uptime` could
    /// not be read. A machine that just booted is a real `0`; an unreadable
    /// counter is absence. This is a published wire form (see the module
    /// docs) and the envelope has no version field, so the widening from
    /// `u64` is a recorded contract change.
    #[serde(default)]
    pub uptime_seconds: Option<u64>,
    /// Cumulative page faults, or `null` when `/proc/vmstat` did not report
    /// `pgfault`. See [`Self::swap_in_pages`] for the absence contract and the
    /// recorded widening.
    #[serde(default)]
    pub pgfault_total: Option<u64>,
    /// Cumulative major page faults, or `null` when `pgmajfault` was not
    /// reported. See [`Self::swap_in_pages`].
    #[serde(default)]
    pub pgmajfault_total: Option<u64>,
    /// Fault rate over the last accepted window, or `null` when this frame has
    /// none. A present `0` is a measured idle; `null` is absence. Both counters
    /// are written from that one window, so an absent pair is not a sample.
    #[serde(default)]
    pub pgfault_per_sec: Option<u64>,
    /// Fault rate over the last accepted window, or `null` when this frame has
    /// none. See [`Self::pgfault_per_sec`].
    #[serde(default)]
    pub pgmajfault_per_sec: Option<u64>,
    pub memory_events: MemoryEvents,
    pub active_scopes: u64,
    /// Host memory held by the managed container scope, or `null` when the
    /// counter could not be read. Absence is unmeasured, never zero.
    #[serde(default)]
    pub docker_memory_current_bytes: Option<u64>,
    pub managed_reservations: u64,
    pub managed_reserved_bytes: u64,
    /// The v4 JSON key is retained for compatibility; the value describes
    /// unmanaged process memory use, not measured system pressure.
    ///
    /// Three arms from [`super::classify::classify_unmanaged_memory_usage`]:
    /// `UNMANAGED_MEMORY` (measured positive), `NONE` (measured refutation),
    /// `UNCLASSIFIED` (no positive finding and at least one unmanaged
    /// footprint never arrived). Absence is never folded into `NONE`.
    #[serde(rename = "unmanaged_pressure_state")]
    pub unmanaged_memory_state: String,
    #[serde(rename = "unmanaged_pressure_kib")]
    pub unmanaged_memory_kib: u64,
    pub unmanaged_processes: u64,
    pub reclaim_speed_gbs: f64,
    pub reclaim_duration_ms: f64,
    pub benchmark_status: String,
    pub benchmark_p50_lat_ms: f64,
    pub benchmark_p99_lat_ms: f64,
    pub estimated_page_fault_lat_us: f64,
}

/// Counters from a cgroup `memory.events` file.
///
/// Each counter is `Option` on purpose. This struct is a published wire form
/// (see the module docs): a key the kernel did not export and a value that did
/// not parse both serialize as `null`, which is the established "not measured"
/// marker in this stream — the same one `gpu_budget` and `anon_pages_kib` use.
/// A present `0` serializes as `0`. The previous plain `u64` fields turned
/// both of those cases into a fabricated `0`, which is how "we never saw an
/// OOM kill" and "we could not read the OOM kill counter" came out identical.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct MemoryEvents {
    pub high: Option<u64>,
    pub max: Option<u64>,
    pub oom: Option<u64>,
    pub oom_kill: Option<u64>,
}

/// One visible process and the figures `/proc` published for it.
///
/// Each of the five counters is `Option<u64>`: a kernel that never exported a
/// counter, or a value that did not parse, is `None` and serializes as `null`.
/// A present `0` serializes as `0`. The previous plain `u64` fields turned both
/// of those cases into a fabricated `0`, which is how "this process holds no
/// memory" and "we never read VmRSS" came out identical. Absence here is never
/// pushed onto `observation.errors` — it is a per-process property of the
/// measurement, not a collector failure.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ProcessObservation {
    pub comm: String,
    pub unit: String,
    pub cgroup: String,
    pub rss_kib: Option<u64>,
    pub swap_kib: Option<u64>,
    pub cpu_ticks: Option<u64>,
    pub read_bytes: Option<u64>,
    pub write_bytes: Option<u64>,
    pub managed: bool,
}

impl ProcessObservation {
    /// Resident plus swapped memory, or `None` when either figure is absent.
    ///
    /// The footprint is one fact about two counters: a process missing either
    /// has no measured footprint, and `None` is that absence. A `0` here would
    /// claim the process holds nothing, which is the fold this accessor refuses.
    pub fn footprint_kib(&self) -> Option<u64> {
        Some(self.rss_kib?.saturating_add(self.swap_kib?))
    }
}

/// DT-10: one observation of the GPU budget plus the enforced VRAM-reserve
/// floor. Every reserve field is named `vram_reserve_*` so a reader can never
/// confuse the GPU free floor with a system-RAM or swap threshold.
///
/// This is the **record** of a driver-reported budget sample, kept even when
/// it has aged. `sampled_at_unix_ms` is the worker's own stamp and is the
/// freshness witness the renderer needs in order to spell "a reading that is
/// no longer current" (`stale`) apart from "no reading at all" (the panel's
/// absence wording). Collapsing the two at projection time is what told an
/// operator the driver never spoke when it spoke seconds ago.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GpuObservation {
    pub adapter: ramshared_vram::GpuAdapterIdentity,
    pub source: GpuBudgetSource,
    /// Wall-clock stamp the worker put on this sample, in Unix milliseconds.
    /// The record's own clock, never a second measurement.
    pub sampled_at_unix_ms: u64,
    /// Driver-reported figures in the unit the telemetry carries: **bytes**.
    ///
    /// Every comparison and every percentage is made on these, never on a
    /// whole-MiB view of the same number. A floor division at construction is
    /// what collapses a real 512 KiB budget into a measured `0 MiB`, and it
    /// also moves the occupancy percent off the true ratio (1536 / 2048 KiB
    /// is 75%, not the 50% its rounded MiB halves report). The whole-MiB
    /// views below are the publish conversion for a display that names MiB.
    pub total_bytes: Option<u64>,
    pub budget_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
    /// Enforced VRAM free floor the admission path must leave untouched,
    /// in MiB. Three-term maximum plus runtime headroom.
    pub vram_reserve_enforced_mib: u64,
    /// Where that floor came from: `sealed-manifest` or `lab-override-raise`.
    pub vram_reserve_source: String,
    /// Sealed configured minimum from the manifest, in MiB.
    pub vram_reserve_sealed_min_mib: u64,
    /// Sealed percentage share. Never overridable (DT-8).
    pub vram_reserve_sealed_percent: u64,
    /// Runtime headroom added on top of the configured reserve, in MiB.
    pub vram_reserve_runtime_buffer_mib: u64,
}

impl GpuObservation {
    /// `true` when this record's sample is still current at `now_unix_ms`.
    ///
    /// Same formula as [`ramshared_vram::GpuBudgetTelemetry::is_fresh_at`],
    /// over the stamp this record carries: the renderer holds the clock, the
    /// record only holds when the reading was taken. An aged record is still
    /// the reading the worker produced and stays on screen marked `stale`.
    pub fn is_fresh_at(&self, now_unix_ms: u64, max_age_ms: u64) -> bool {
        ramshared_vram::GpuBudgetTelemetry::sample_is_fresh_at(
            self.sampled_at_unix_ms,
            now_unix_ms,
            max_age_ms,
        )
    }

    /// Whole-MiB publish conversion for a figure named in MiB.
    ///
    /// `div_ceil`, not floor: the rounding is unavoidable once the display
    /// names MiB, but floor division is what turns a present 512 KiB reading
    /// into a measured zero, and a non-zero measurement must never publish as
    /// zero. A measured zero stays a zero.
    ///
    /// These are **views**. They are never fed back into a comparison or a
    /// percentage — that is what [`Self::used_percent_of_budget`] is for, and
    /// it reads the exact figures.
    pub fn budget_mib(&self) -> u64 {
        self.budget_bytes.div_ceil(MIB_BYTES)
    }

    /// See [`Self::budget_mib`].
    pub fn used_mib(&self) -> u64 {
        self.used_bytes.div_ceil(MIB_BYTES)
    }

    /// Whole-MiB publish conversion for the **within-budget free figure**.
    ///
    /// This one floors, and it is the only one of these four that does.
    /// `free_bytes` is an **allocation bound** — "you may take at most this"
    /// — and rounding up would grant more than the exact remainder: a 256 KiB
    /// free figure published as `1 MiB` is four times the bound. Its three
    /// siblings are holds and capacities, where the whole-MiB view rounds the
    /// other way because a non-zero reading must never publish as zero.
    ///
    /// These are the two faces of the same two-view rule and they point in
    /// opposite directions, which is why one "round to MiB" helper must not
    /// serve both callers. The same figure already floors on its daemon face
    /// (`gpu_headroom_kib` is the named floor of the same division,
    /// `alloc_bound_kib`), so this is also what makes the two published faces
    /// of one driver reading agree.
    ///
    /// A positive remainder that floors to zero is not a zero. The renderer's
    /// `free_bound_label` is what publishes that third state; this face is
    /// the number.
    pub fn free_mib(&self) -> u64 {
        self.free_bytes / MIB_BYTES
    }

    /// See [`Self::budget_mib`]. `None` stays `None`: "not measured" is not
    /// a zero to round.
    pub fn total_mib(&self) -> Option<u64> {
        self.total_bytes.map(|bytes| bytes.div_ceil(MIB_BYTES))
    }

    /// Budget **occupancy** percent, from the exact byte figures.
    ///
    /// Never from the whole-MiB views: rounding first would move the number
    /// off the ratio the driver reported (1536 / 2048 bytes is 75%, and its
    /// rounded MiB halves say 50%).
    ///
    /// `None` is a budget with **no denominator**: the driver reported zero
    /// capacity, so no fullness ratio exists for this sample. That is not
    /// `Some(0)`. An idle 2 GiB budget is `Some(0)` — zero of two gigabytes is
    /// a measured zero percent — while `None` says the percent was never a
    /// thing. A synthesized `0` here would claim a fullness measured against a
    /// capacity the driver did not report, which is the same collapse the
    /// whole-MiB views refuse.
    pub fn used_percent_of_budget(&self) -> Option<u64> {
        self.used_bytes
            .saturating_mul(100)
            .checked_div(self.budget_bytes)
    }
}

/// Bytes in one MiB — the divisor of the whole-MiB **publish** conversion.
/// Comparisons never divide by this; see [`GpuObservation::budget_mib`].
const MIB_BYTES: u64 = 1024 * 1024;

/// Kernel basenames of the live swap devices each tier is served by.
///
/// The tier header names what the swap table listed. A tier is not always one
/// device, and the device is not always the one a product constant would
/// guess — tier 2 is an `nbd` node on one host, a `ublk` or logical device on
/// another — so the name is observed rather than asserted. A ghost — a path
/// the kernel marked `(deleted)` — is skipped: a device that no longer exists
/// has no name to print.
///
/// Each list is sorted and deduplicated, so a tier served by two paths that
/// share a kernel name prints that name once. A tier with no live swap device
/// is an empty list, which the header renders as no parenthetical at all rather
/// than as a name it did not read.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct TierDeviceNames {
    pub zram: Vec<String>,
    pub vram: Vec<String>,
    pub disk: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Observation {
    #[serde(flatten)]
    pub status: BTreeMap<String, Value>,
    /// When this frame was assembled — the one freshness clock it carries.
    ///
    /// Every aged figure rendered from an observation is aged against this
    /// stamp, never against the wall clock at draw or print time. The question
    /// a row answers is "was this sample current *when this frame was taken*?",
    /// which is the only reading that stays a property of the measurement: a
    /// wall-clock read at render time would make the same observation render
    /// differently depending on when it reaches the screen, and would let a
    /// frozen frame drift into the sample-age mark that belongs to the other
    /// staleness fact (see [`GpuObservation::is_fresh_at`]).
    ///
    /// [`super::observations::unix_epoch_ms`] stamps this once, at collection.
    /// Nothing in the presentation band calls it again.
    pub epoch_ms: u64,
    pub sample_age_ms: u64,
    pub memory_scope: MemoryScope,
    pub mem: MemoryObservation,
    pub cgroup_memory: CgroupMemoryObservation,
    pub hyperv_balloon: HyperVBalloonObservation,
    pub process_totals: ProcessMemoryTotals,
    pub control_plane: ControlPlaneObservation,
    pub gpu: Option<GpuObservation>,
    /// Observed swap-device identity per tier. Sibling of `status`'s `tiers`
    /// object, not part of it: that document is the cascade producer's claim
    /// and carries no device names, while this is the monitor's own `/proc/swaps`
    /// read. Both describe the same host at the same instant.
    pub tier_devices: TierDeviceNames,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_progress: Option<crate::operation_progress::OperationProgressV1>,
    pub top_processes: Vec<ProcessObservation>,
    pub errors: Vec<String>,
}

impl Observation {
    pub(super) fn value(&self, key: &str) -> Option<&Value> {
        self.status.get(key)
    }

    /// A measured member of the published `capacity` object.
    ///
    /// The capacity figures moved under `capacity`; the top-level names are a
    /// legacy observation shape. A present `null` in the object is an
    /// unmeasured figure and must not fall through to a stale duplicate, so
    /// only an absent key uses the legacy fallback.
    pub(super) fn capacity_u64(&self, key: &str) -> Option<u64> {
        match self
            .value("capacity")
            .and_then(|capacity| capacity.get(key))
        {
            Some(value) => value.as_u64(),
            None => self.value(key).and_then(Value::as_u64),
        }
    }

    pub(super) fn string(&self, key: &str) -> &str {
        self.value(key).and_then(Value::as_str).unwrap_or("unknown")
    }

    pub(super) fn bool_value(&self, key: &str) -> Option<bool> {
        self.value(key).and_then(Value::as_bool)
    }

    /// The daemon liveness the status document published, if it published
    /// one. Absence is `None` and never a measured `false`: a frame that
    /// carries no `daemon.alive` witness did not observe a stopped daemon,
    /// it observed nothing about the daemon at all.
    pub(super) fn daemon_alive(&self) -> Option<bool> {
        self.value("daemon")
            .and_then(|daemon| daemon.get("alive"))
            .and_then(Value::as_bool)
    }

    /// The guardian-plane state the status document published, if it
    /// published one. Absence is `None` and never a measured state: a frame
    /// that carries no `guardian_state` did not observe the guard, it
    /// observed nothing about the guard at all.
    pub(super) fn guardian_state(&self) -> Option<&str> {
        self.value("guardian_state").and_then(Value::as_str)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;

    /// Two different memory scopes must never publish the same label on the
    /// same face. A collision is a display-face defect: an operator reading
    /// "WSL Guest RAM History" could not tell which scope's history it was, and
    /// the same holds for the panel and the row label. The three faces are
    /// checked separately because a collision on one face is already a defect.
    ///
    /// This drives every arm of all three label functions, including the WSL1
    /// and native arms a dashboard-only suite never reaches.
    #[test]
    fn every_memory_scope_publishes_a_distinct_label_on_each_face() {
        let scopes = [MemoryScope::LinuxHost, MemoryScope::Wsl, MemoryScope::Wsl2];
        for (index, left) in scopes.iter().enumerate() {
            for right in scopes.iter().skip(index + 1) {
                assert_ne!(
                    left.ram_label(),
                    right.ram_label(),
                    "ram_label collides for {left:?} and {right:?}"
                );
                assert_ne!(
                    left.panel_title(),
                    right.panel_title(),
                    "panel_title collides for {left:?} and {right:?}"
                );
            }
        }
    }

    /// Each face's label for a scope must name that scope, not another. The
    /// injectivity check above would still pass a swap of two whole labels —
    /// "Host RAM History" on `Wsl` and "WSL Guest RAM History" on
    /// `LinuxHost` — so the name has to travel with the scope.
    #[test]
    fn each_face_label_names_the_scope_it_is_rendered_for() {
        let expectations = [
            (MemoryScope::LinuxHost, "Host RAM"),
            (MemoryScope::Wsl, "WSL Guest RAM"),
            (MemoryScope::Wsl2, "WSL2 Guest RAM"),
        ];
        for (scope, name) in expectations {
            assert_eq!(scope.ram_label(), name, "ram_label for {scope:?}");
            assert!(
                scope.panel_title().starts_with(name),
                "panel_title for {scope:?} must name its scope: {}",
                scope.panel_title()
            );
        }
    }

    /// A coherent pair is one measurement and reads back as its figures.
    /// Measured empty swap is `Some((0, 0))`, not absence: a host with no swap
    /// is a product state.
    #[test]
    fn coherent_memory_pairs_read_back_as_their_figures() {
        let memory = MemoryObservation {
            total_kib: Some(16_384),
            available_kib: Some(8_192),
            swap_total_kib: Some(4_096),
            swap_free_kib: Some(2_048),
            ..MemoryObservation::default()
        };
        assert_eq!(memory.ram_pair(), Some((16_384, 8_192)));
        assert_eq!(memory.swap_pair(), Some((4_096, 2_048)));

        let empty_swap = MemoryObservation {
            total_kib: Some(16_384),
            available_kib: Some(8_192),
            swap_total_kib: Some(0),
            swap_free_kib: Some(0),
            ..MemoryObservation::default()
        };
        assert_eq!(empty_swap.swap_pair(), Some((0, 0)));
    }

    /// The negative face: an absent half, a tear, and a zero RAM total all
    /// have no reading. They publish `None`, never a confident `Some(0)` or a
    /// pair the halves contradict. Each of the three is a different fact and
    /// each is refused.
    #[test]
    fn absent_or_disagreeing_memory_pairs_publish_absence_never_zero() {
        let default = MemoryObservation::default();
        assert_eq!(default.ram_pair(), None);
        assert_eq!(default.swap_pair(), None);

        let half_present = MemoryObservation {
            total_kib: Some(16_384),
            ..MemoryObservation::default()
        };
        assert_eq!(half_present.ram_pair(), None);

        let ram_tear = MemoryObservation {
            total_kib: Some(4_096),
            available_kib: Some(8_192),
            ..MemoryObservation::default()
        };
        assert_eq!(ram_tear.ram_pair(), None);

        let swap_tear = MemoryObservation {
            swap_total_kib: Some(4_096),
            swap_free_kib: Some(8_192),
            ..MemoryObservation::default()
        };
        assert_eq!(swap_tear.swap_pair(), None);

        let zero_total = MemoryObservation {
            total_kib: Some(0),
            available_kib: Some(0),
            ..MemoryObservation::default()
        };
        assert_eq!(zero_total.ram_pair(), None);
    }

    /// The published shape of an absent counter is `null`, never `0`. A
    /// present zero stays a zero. This is the wire face a JSONL reader sees,
    /// and it is the whole reason the four counters are `Option`.
    ///
    /// `required_counters_available` is gone with the zero-fill it explained:
    /// the fields carry the distinction, so a side-channel that a reader had
    /// to consult before trusting a `0` has no reason to exist.
    #[test]
    fn the_published_shape_never_reports_an_unmeasured_memory_field_as_zero() {
        let absent = MemoryObservation::default();
        let wire = serde_json::to_value(&absent).expect("serialize absent");
        for field in [
            "total_kib",
            "available_kib",
            "swap_total_kib",
            "swap_free_kib",
        ] {
            assert!(
                wire.get(field).is_some_and(Value::is_null),
                "{field} must publish null when never measured: {wire}"
            );
        }
        assert!(
            wire.get("required_counters_available").is_none(),
            "the zero-fill side-channel is gone: {wire}"
        );

        let measured_empty_swap = MemoryObservation {
            swap_total_kib: Some(0),
            swap_free_kib: Some(0),
            ..MemoryObservation::default()
        };
        let wire = serde_json::to_value(&measured_empty_swap).expect("serialize empty swap");
        assert_eq!(wire["swap_total_kib"], 0);
        assert_eq!(wire["swap_free_kib"], 0);
    }
}
