// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/latency.rs
//! Per-tier busy-window accumulation and page-fault latency estimation.

use super::*;
use serde::{Deserialize, Serialize};

/// Run-wide I/O extrema for one tier, accumulated across accepted rate windows.
///
/// Two sample sets live here and they are not the same. `windows` counts every
/// accepted window and feeds the directional peaks; `count` counts only the
/// windows whose combined rate reached the busy floor and feeds the
/// min/avg/max trio. A tier observed entirely below that floor therefore has
/// directional peaks and no busy statistics, and each family publishes its own
/// absence instead of borrowing the other's zero.
///
/// The floor is [`BUSY_WINDOW_MIN_MBS`].
#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct TierAccumulator {
    pub(super) min_mbs: f64,
    pub(super) max_mbs: f64,
    pub(super) read_max_mbs: f64,
    pub(super) write_max_mbs: f64,
    total_mbs: f64,
    pub(super) count: u64,
    pub(super) windows: u64,
}

/// Combined rate at which an accepted window joins the busy statistics.
///
/// This gates a **sample set**: a window below it still counts in `windows`
/// and still moves the directional peaks, it simply has no place in the
/// min/avg/max trio. It is one of three thresholds whose value is `5.0` and
/// which are not one policy — see [`BUSY_MOVED_MIN_MBS`] and
/// [`MIN_BASELINE_TIER_MBS`]. Merging any two would change what a reader is
/// told.
pub(super) const BUSY_WINDOW_MIN_MBS: f64 = 5.0;

impl TierAccumulator {
    pub(super) fn record(&mut self, read_speed: f64, write_speed: f64) {
        let speed = read_speed + write_speed;
        self.windows += 1;
        self.read_max_mbs = self.read_max_mbs.max(read_speed);
        self.write_max_mbs = self.write_max_mbs.max(write_speed);
        if speed >= BUSY_WINDOW_MIN_MBS {
            self.min_mbs = if self.min_mbs == 0.0 {
                speed
            } else {
                self.min_mbs.min(speed)
            };
            self.max_mbs = self.max_mbs.max(speed);
            self.total_mbs += speed;
            self.count += 1;
        }
    }

    /// Busy-band average, or `None` while the busy set is empty.
    ///
    /// `None` is an empty average, not a measured `0.0`: no accepted window
    /// reached the floor, so there is no busy sample for this to be the mean
    /// of.
    pub(super) fn is_finite_non_negative(&self) -> bool {
        [
            self.min_mbs,
            self.max_mbs,
            self.read_max_mbs,
            self.write_max_mbs,
            self.total_mbs,
        ]
        .into_iter()
        .all(|value| value.is_finite() && value >= 0.0)
    }

    pub(super) fn avg_mbs(&self) -> Option<f64> {
        if self.count > 0 {
            Some(self.total_mbs / self.count as f64)
        } else {
            None
        }
    }

    pub(super) fn apply_to_plane_io(&self, plane_io: &mut TierIoStats) {
        // The busy trio is all-or-nothing: one missing sample leaves the whole
        // statistic undefined, the same fail-closed rule `has_rate_sample`
        // applies to a half-populated rate pair.
        if self.count > 0 {
            plane_io.min_mbs = Some(self.min_mbs);
            plane_io.avg_mbs = self.avg_mbs();
            plane_io.max_mbs = Some(self.max_mbs);
            plane_io.peak_mbs = Some(self.max_mbs);
        } else {
            plane_io.min_mbs = None;
            plane_io.avg_mbs = None;
            plane_io.max_mbs = None;
            plane_io.peak_mbs = None;
        }
        // Directional peaks take every window. A tier that only ever ran below
        // the busy floor still has a real read and write peak, and a measured
        // idle is a present `0.0`.
        if self.windows > 0 {
            plane_io.read_peak_mbs = Some(self.read_max_mbs);
            plane_io.write_peak_mbs = Some(self.write_max_mbs);
        } else {
            plane_io.read_peak_mbs = None;
            plane_io.write_peak_mbs = None;
        }
    }
}

/// Order-of-magnitude latency profile for one swap tier.
///
/// These are engineering estimates, not host measurements. No producer
/// publishes a measured latency on the wire today, so every projected figure
/// is labelled [`LatencySource::Estimated`] wherever it is shown. The numbers
/// are the rough band the dashboard uses to rank the three tiers and to scale
/// that ranking with throughput; they are not a calibration of any device, and
/// a real figure depends on the device, the queue depth and the load.
///
/// A producer that measures its own latency sets [`LatencySource::Measured`]
/// on the wire and [`project_tier_latency`] leaves those values alone: a
/// measured figure outranks a throughput-scaled constant in every case.
#[derive(Clone, Copy, Debug)]
pub(super) struct TierLatencyEstimate {
    /// Latency of an idle access to this device class, in microseconds.
    /// Published as `min_lat_us`. A property of the tier, not of a frame: it
    /// is correct for a quiet tier and is not raised by a busy one.
    pub(super) min_us: f64,
    /// Average latency at zero throughput when the tier has devices, in
    /// microseconds. Deliberately independent of `min_us` — the idle average
    /// is its own figure, not the floor plus the slope.
    pub(super) idle_avg_us: f64,
    /// Throughput at which the average-latency slope completes, in MiB/s.
    pub(super) avg_slope_at_mbs: f64,
    /// Average latency added at `avg_slope_at_mbs`, in microseconds.
    pub(super) avg_slope_us: f64,
    /// Throughput at which the max-latency slope completes, in MiB/s.
    pub(super) max_slope_at_mbs: f64,
    /// Maximum latency added at `max_slope_at_mbs`, in microseconds.
    pub(super) max_slope_us: f64,
    /// Never publish a maximum below this, in microseconds. Also the maximum
    /// when the tier has no devices this frame.
    pub(super) max_floor_us: f64,
}

/// Compressed host RAM. The fastest plane and the shallowest slope: a zram
/// hit is a memory access with a decompression step, not a device I/O.
pub(super) const ZRAM_LATENCY_ESTIMATE: TierLatencyEstimate = TierLatencyEstimate {
    min_us: 0.04,
    idle_avg_us: 0.08,
    avg_slope_at_mbs: 2_000.0,
    avg_slope_us: 0.08,
    max_slope_at_mbs: 1_000.0,
    max_slope_us: 0.15,
    max_floor_us: 0.15,
};

/// The product's VRAM-backed tier. Slower than compressed RAM and faster
/// than the disk it protects.
pub(super) const VRAM_LATENCY_ESTIMATE: TierLatencyEstimate = TierLatencyEstimate {
    min_us: 0.85,
    idle_avg_us: 1.45,
    avg_slope_at_mbs: 1_000.0,
    avg_slope_us: 1.20,
    max_slope_at_mbs: 500.0,
    max_slope_us: 2.00,
    max_floor_us: 3.20,
};

/// The last-resort disk plane. Two to four orders of magnitude slower than
/// the tiers above it, which is why the cascade keeps it last.
pub(super) const DISK_LATENCY_ESTIMATE: TierLatencyEstimate = TierLatencyEstimate {
    min_us: 85.0,
    idle_avg_us: 180.0,
    avg_slope_at_mbs: 100.0,
    avg_slope_us: 120.0,
    max_slope_at_mbs: 50.0,
    max_slope_us: 600.0,
    max_floor_us: 1_200.0,
};

/// Page-fault latency when no tier is taking traffic.
///
/// A fault that reached no swap plane is not a swap access, so this is a
/// different quantity from the three profiles above and is named separately
/// rather than reusing a tier's floor. It stands in for a resident fault
/// while the dashboard still wants one number to print.
pub(super) const RESIDENT_PAGE_FAULT_LAT_US: f64 = 0.85;

/// Project one tier's latency band from its profile and its throughput.
///
/// `busy_windows` is how many accepted rate windows for this tier reached
/// [`BUSY_WINDOW_MIN_MBS`]. It is a **sample-set size, not a device count**:
/// zero means the tier saw no busy window this frame, which is a quiet tier,
/// not necessarily an absent one. The average then falls back to the idle
/// figure and the maximum to the profile floor rather than to a scaled curve
/// built from throughputs that were never observed as busy.
///
/// `min_us` is published either way. It is a property of the tier's device
/// class — the latency of an idle access to this kind of device — and not a
/// per-frame measurement, so a quiet frame does not make it fabricated and a
/// busy frame does not raise it.
pub(super) fn project_tier_latency(
    profile: &TierLatencyEstimate,
    avg_mbs: f64,
    max_mbs: f64,
    busy_windows: u64,
) -> (f64, f64, f64) {
    if busy_windows == 0 {
        return (profile.min_us, profile.idle_avg_us, profile.max_floor_us);
    }
    (
        profile.min_us,
        profile.min_us + (avg_mbs / profile.avg_slope_at_mbs) * profile.avg_slope_us,
        f64::max(
            profile.idle_avg_us + (max_mbs / profile.max_slope_at_mbs) * profile.max_slope_us,
            profile.max_floor_us,
        ),
    )
}

/// Project a tier's latency band onto its wire form, unless it was measured.
///
/// A measured latency outranks this projection and is left exactly as the
/// producer published it. The rest of the record is untouched either way.
pub(super) fn project_onto_plane(
    plane_io: &mut TierIoStats,
    profile: &TierLatencyEstimate,
    busy_windows: u64,
) {
    if plane_io.latency_source == LatencySource::Measured {
        return;
    }
    // A missing busy statistic projects to zero throughput rather than to a
    // published peak: the throughput-scaled term then vanishes and the base
    // stands alone. The published peaks stay `Option`; this is an estimate's
    // input, not a wire figure.
    let (min_us, avg_us, max_us) = project_tier_latency(
        profile,
        plane_io.avg_mbs.unwrap_or(0.0),
        plane_io.max_mbs.unwrap_or(0.0),
        busy_windows,
    );
    plane_io.latency_source = LatencySource::Estimated;
    plane_io.min_lat_us = min_us;
    plane_io.avg_lat_us = avg_us;
    plane_io.max_lat_us = max_us;
}

/// Project every tier's latency band and pick the page-fault estimate.
///
/// The three counts are the accumulators' busy-window counts
/// ([`TierAccumulator::count`]), which are sample-set sizes and not device
/// counts. Each tier is projected independently; the estimate then follows the
/// **deepest** tier [`tier_is_paging`] accepts — disk before VRAM before zram —
/// falling back to [`RESIDENT_PAGE_FAULT_LAT_US`] when none is.
///
/// Deepest, not fastest. A fault hits whichever device holds the page, and a
/// disk that is taking traffic means some faults reach it. The figure an
/// operator reads while paging is happening has to name the worst latency that
/// can actually occur, not the cheapest tier that happens to be busy: with all
/// three tiers busy, picking the zram band would say paging is cheap while the
/// disk is serving faults. The chain is that worst case, and its order is the
/// cascade's own fallback order.
pub(super) fn update_tier_latencies(
    cp: &mut ControlPlaneObservation,
    zram_busy_windows: u64,
    vram_busy_windows: u64,
    disk_busy_windows: u64,
) {
    project_onto_plane(&mut cp.zram_io, &ZRAM_LATENCY_ESTIMATE, zram_busy_windows);
    project_onto_plane(&mut cp.vram_io, &VRAM_LATENCY_ESTIMATE, vram_busy_windows);
    project_onto_plane(&mut cp.disk_io, &DISK_LATENCY_ESTIMATE, disk_busy_windows);

    cp.estimated_page_fault_lat_us = if tier_is_paging(&cp.disk_io) {
        cp.disk_io.avg_lat_us
    } else if tier_is_paging(&cp.vram_io) {
        cp.vram_io.avg_lat_us
    } else if tier_is_paging(&cp.zram_io) {
        cp.zram_io.avg_lat_us
    } else {
        RESIDENT_PAGE_FAULT_LAT_US
    };
}
