// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/stress/host.rs
//! Host procfs, sysfs, buddyinfo, PSI, swap-tier, and allocation latency probes.
//!
//! Extracted from `stress.rs` to keep host observation separate from pacing
//! policy, terminal formatting, and the phase execution loop.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::Path;
use std::time::Instant;

use crate::cascade;

use super::budget::{CacheSample, read_cache_status_sample};
use super::measure::{TierDiskIoBytes, parse_gpu_cache_run_telemetry};
use super::model::GpuCacheRunTelemetry;
use super::{
    MIN_ORDER_7_BUDDY_CHUNKS, MemInfoKib, PROACTIVE_COMPACTION_TRIGGER_CHUNKS, SwapTiersKib,
    TierCapacityStats,
};

pub fn is_wsl2() -> bool {
    fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.contains("microsoft") || s.contains("WSL"))
        .unwrap_or(false)
        || std::path::Path::new("/proc/sys/fs/binfmt_misc/WSLInterop").exists()
        || std::env::var_os("WSL_INTEROP").is_some()
}

/// One `/proc/meminfo` observation, or its absence.
///
/// `None` means the reading does not exist: the file could not be read, or
/// the text did not carry a measurement of both figures. Absence is not a
/// reading of zero available RAM, and a `0` in either field of `Some` is a
/// measured zero — what the kernel really published.
pub fn read_mem_info() -> Option<MemInfoKib> {
    let text = fs::read_to_string("/proc/meminfo").ok()?;
    mem_info_from(&text)
}

/// Fold one `/proc/meminfo` text into the two figures the admission path
/// compares, in KiB.
///
/// `None` when the text does not measure both figures. A document with
/// `MemTotal` and no `MemAvailable` has not measured availability; a document
/// with neither is not a meminfo reading at all; a row whose value will not
/// parse is the same absence. Folding either case to `0` would claim the
/// kernel published an empty figure.
///
/// No rounding here. The rows carry KiB and that is what the fold holds;
/// whole-MiB figures are views taken later, at publication. Rounding at the
/// fold is what turned a real 511 KiB `MemAvailable` into a measured `0`, and
/// a 1537 KiB one into a `2 MiB` that does not exist.
pub(super) fn mem_info_from(text: &str) -> Option<MemInfoKib> {
    let mut total_kib = None;
    let mut avail_kib = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total_kib = rest.split_whitespace().next().and_then(|s| s.parse().ok());
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            avail_kib = rest.split_whitespace().next().and_then(|s| s.parse().ok());
        }
    }
    Some(MemInfoKib {
        total_kib: total_kib?,
        avail_kib: avail_kib?,
    })
}

pub fn read_sysctl_min_free_kib() -> u64 {
    let text = fs::read_to_string("/proc/sys/vm/min_free_kbytes").ok();
    parse_min_free_kbytes(text.as_deref())
}

/// `min_free_kbytes`, in the unit the sysctl publishes: **KiB**.
///
/// A missing or unparseable value is `0`, which is the conservative answer
/// here and not a measurement: the floor formula subtracts this figure, so
/// treating it as absent demands *more* `MemAvailable` rather than less. The
/// old fold floor-divided to MiB, which pushed the same direction and so was
/// never unsafe — it only understated a real `1023` KiB reservation to `0`.
/// The figure now survives intact because the comparison beside it is in KiB
/// and there is no longer any reason to throw the remainder away.
pub(super) fn parse_min_free_kbytes(text: Option<&str>) -> u64 {
    text.and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// How much `MemAvailable` a run must leave before it may allocate.
///
/// `MemAvailable` already discounts the kernel's reserved pages, so the
/// physical headroom in play is `MemAvailable + min_free_kbytes`. Requiring
/// `floor + min_free >= target_physical_floor` is therefore the same as
/// `floor >= target - min_free`, floored at a usable minimum so a large
/// reservation cannot drive the requirement to zero.
///
/// Every input is KiB and so is the result. The policy constants at the call
/// site are whole MiB and are converted there with an exact `* 1024`.
pub(super) fn required_memavailable_floor_kib(
    target_physical_floor_kib: u64,
    min_free_kib: u64,
    minimum_usable_avail_kib: u64,
) -> u64 {
    target_physical_floor_kib
        .saturating_sub(min_free_kib)
        .max(minimum_usable_avail_kib)
}

/// Hung-task lines in one kernel ring-buffer dump.
///
/// Pure: the caller owns the observation, this owns the fold. Kept separate
/// for the same reason [`count_kernel_fault_lines`] is — a test must be able
/// to pin the fold without spawning anything.
pub(super) fn count_hung_task_lines(text: &str) -> u64 {
    text.lines()
        .filter(|line| line.contains("blocked for more than") || line.contains("hung_task"))
        .count() as u64
}

/// Observe the kernel ring buffer and count hung-task lines.
///
/// `Some(0)` is the proof none fired. `None` means the buffer was not
/// readable — a `dmesg` that will not spawn, or that exits non-zero, has not
/// measured zero hung tasks, and is not allowed to claim it has. This is the
/// same face [`current_kernel_faults`] uses, for the same reason.
pub fn count_kernel_hung_tasks() -> Option<u64> {
    let output = std::process::Command::new("dmesg").output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(count_hung_task_lines(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

pub(super) fn count_kernel_fault_lines(text: &str) -> u64 {
    text.lines()
        .filter(|line| {
            line.contains("Possible stuck request")
                || line.contains("I/O error, dev nbd")
                || line.contains("Receive control failed")
                || line.contains("MCE: Killing")
                || line.contains("blocked for more than")
                || line.contains("hung_task")
                || line.contains("BUG:")
                || line.contains("Oops:")
                || line.contains("Kernel panic")
        })
        .count() as u64
}

pub(super) fn current_kernel_faults() -> Option<u64> {
    let output = std::process::Command::new("dmesg").output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(count_kernel_fault_lines(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

/// What `/proc/buddyinfo` said about order-7 availability in the Normal zone.
///
/// Three facts, not one. This used to be `Option<u64>`, and all three
/// collapsed onto `None`, which [`is_order_7_depleted`] answered as "not
/// depleted" — so a table with no Normal zone, a table whose order-7 column
/// would not parse, and a `/proc/buddyinfo` that could not be opened all
/// became the same claim as a measured and healthy count. Only
/// [`BuddyinfoReading::Measured`] is an answer to the question the interlock
/// asks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuddyinfoReading {
    /// The Normal zone reported this many effective order-7 chunks, with
    /// orders 8-10 counted as the split-eligible contributions they are.
    Measured(u64),
    /// The table was read and has no Normal zone line. Order-7 depletion is
    /// not a question this machine can be asked.
    NoNormalZone,
    /// The table was unreadable, or a column the sum needs would not parse.
    /// Nothing was measured.
    Unreadable,
}

/// Reduce a `/proc/buddyinfo` document to its Normal-zone order-7 answer.
///
/// In the Linux buddy allocator, high-order chunks (order 8, 9, 10) are
/// split on demand into order-7 chunks (512 KiB), so genuine high-order
/// exhaustion only occurs when raw order-7 and every split-eligible higher
/// order are depleted below the floor. Each of those four columns is a
/// measurement; a column that will not parse is not a zero contribution to
/// the sum, which is what its `unwrap_or(0)` used to claim.
pub fn parse_buddyinfo_reading(content: &str) -> BuddyinfoReading {
    for line in content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 15 || parts.get(3).copied() != Some("Normal") {
            continue;
        }
        // Columns 11..15 are orders 7, 8, 9 and 10 of the Normal zone.
        let mut orders = [0u64; 4];
        for (dest, token) in orders.iter_mut().zip(&parts[11..15]) {
            let Ok(value) = token.parse::<u64>() else {
                return BuddyinfoReading::Unreadable;
            };
            *dest = value;
        }
        let [order_7, order_8, order_9, order_10] = orders;
        let higher_equivalent = (order_8 * 2) + (order_9 * 4) + (order_10 * 8);
        return BuddyinfoReading::Measured(order_7 + higher_equivalent);
    }
    BuddyinfoReading::NoNormalZone
}

/// Read `/proc/buddyinfo` and reduce it to the Normal-zone order-7 answer.
///
/// A file that will not open is [`BuddyinfoReading::Unreadable`], not
/// [`BuddyinfoReading::NoNormalZone`] and not a measured zero: the table
/// being absent and the table having no Normal zone are different facts, and
/// the interlock has to be able to tell them apart.
pub fn read_buddyinfo_reading() -> BuddyinfoReading {
    match fs::read_to_string("/proc/buddyinfo") {
        Ok(content) => parse_buddyinfo_reading(&content),
        Err(_) => BuddyinfoReading::Unreadable,
    }
}

/// Is this measured order-7 count below the floor?
///
/// Takes a count, not an [`Option`]: absence is not a measurement of
/// depletion and not a measurement of health, and a predicate that answers
/// for a count it was never given is how the interlock used to fail open.
pub fn is_order_7_depleted(order_7_chunks: u64, threshold: u64) -> bool {
    order_7_chunks < threshold
}

pub fn trigger_proactive_compaction() {
    let _ = fs::write("/proc/sys/vm/compact_memory", "1\n");
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuddyInterlockAction {
    Continue,
    CompactionTriggered,
    Halt { detected_chunks: u64 },
}

#[allow(dead_code)]
pub fn decide_buddyinfo_action(reading: BuddyinfoReading) -> BuddyInterlockAction {
    decide_buddyinfo_action_with_threshold(reading, MIN_ORDER_7_BUDDY_CHUNKS)
}

/// Decide what the buddy interlock does with one reading.
///
/// Only [`BuddyinfoReading::Measured`] can reach
/// [`BuddyInterlockAction::Halt`]: the halt is a claim about a count, and an
/// unreadable or inapplicable table has no count to make it from. The two
/// absence faces both continue, each in its own arm, so the polarity is
/// written down rather than being whatever a `_ =>` fall-through happened to
/// mean.
///
/// That polarity is deliberate and is not a fail-closed one. The case that
/// used to reach the fall-through most often is a machine with no Normal
/// zone, where continuing is the correct answer and not a compromise;
/// halting every stress run because `/proc/buddyinfo` will not open would
/// arm a hang interlock exactly where its premise — a Normal zone under
/// order-7 pressure — does not hold. The operator's `min_order_7_chunks`
/// floor is what arms the protection at all, and `0` leaves it off.
///
/// Registered, not decided here: whether an unreadable table on a machine
/// that does have a Normal zone should fail closed instead. That is a
/// behaviour change with a product cost, it needs its own witness, and there
/// is no reproduced hang behind the current polarity that would justify
/// flipping it in this change.
pub fn decide_buddyinfo_action_with_threshold(
    reading: BuddyinfoReading,
    threshold: u64,
) -> BuddyInterlockAction {
    if threshold == 0 {
        return BuddyInterlockAction::Continue;
    }
    match reading {
        BuddyinfoReading::Measured(o7) if is_order_7_depleted(o7, threshold) => {
            BuddyInterlockAction::Halt {
                detected_chunks: o7,
            }
        }
        BuddyinfoReading::Measured(o7)
            if (threshold..PROACTIVE_COMPACTION_TRIGGER_CHUNKS.max(threshold + 8))
                .contains(&o7) =>
        {
            trigger_proactive_compaction();
            BuddyInterlockAction::CompactionTriggered
        }
        // Measured, and at or above the floor.
        BuddyinfoReading::Measured(_) => BuddyInterlockAction::Continue,
        // No Normal zone: order-7 depletion is not a question this machine
        // can be asked. Continuing is the answer, not a fall-through.
        BuddyinfoReading::NoNormalZone => BuddyInterlockAction::Continue,
        // Nothing was measured. See the polarity note above.
        BuddyinfoReading::Unreadable => BuddyInterlockAction::Continue,
    }
}

pub fn read_psi_full() -> Option<f64> {
    let text = fs::read_to_string("/proc/pressure/memory").ok()?;
    crate::supervisor::parse_psi_full_avg10(&text)
}

/// What one PSI observation means for the stress cycle that took it.
///
/// Three faces, because `Option` cannot say "absent and tolerated" apart
/// from "absent and fatal". Absence is never a measured `0.0`: a fabricated
/// zero would hold every [`psi_below_limit`] licence open and every
/// [`psi_at_or_over_limit`] veto shut for the rest of the run, on a host
/// where PSI simply stopped being readable.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum StressPsi {
    /// A finite figure inside the PSI full range.
    Measured(f64),
    /// No valid figure, on a platform that does not require one. The cycle
    /// continues with the pressure interlock uninformed this iteration.
    Uninformed,
    /// No valid figure where one is required, or a figure outside the PSI
    /// range. A number that is not a PSI full reading is not published as
    /// one. The caller halts.
    Invalid,
}

/// Classify one raw PSI observation for a stress cycle.
///
/// `required` selects only whether an absence is fatal. It never selects a
/// figure: there is no input path that returns `Measured` for a sample
/// nobody read.
pub(super) fn stress_psi_sample(sample: Option<f64>, required: bool) -> StressPsi {
    let measured = sample.filter(|value| value.is_finite() && (0.0..=100.0).contains(value));
    match measured {
        Some(value) => StressPsi::Measured(value),
        None if required => StressPsi::Invalid,
        None => StressPsi::Uninformed,
    }
}

/// `true` only when PSI was measured at or above the limit.
///
/// This is a veto, so absence does not fire it: an uninformed cycle has not
/// observed dangerous pressure, and on a platform that does not require PSI
/// the run is allowed to exist without inventing an observation.
pub(super) fn psi_at_or_over_limit(psi_full: Option<f64>, max_psi_full: f64) -> bool {
    psi_full.is_some_and(|psi| psi >= max_psi_full)
}

/// `true` only when PSI was measured below the limit.
///
/// This is a licence, so absence does not grant it: waiting out a recovery
/// window is safe only while pressure is *known* to be under the limit.
/// Absence certifies nothing and must not open this gate.
pub(super) fn psi_below_limit(psi_full: Option<f64>, max_psi_full: f64) -> bool {
    psi_full.is_some_and(|psi| psi < max_psi_full)
}

/// One `/proc/swaps` row reduced to what the tier folds need.
pub(super) struct SwapRow {
    name: String,
    size_kib: u64,
    used_kib: u64,
}

/// Walk `/proc/swaps` rows after the header.
///
/// Columns are read from the end, the way the cascade's strict
/// `parse_proc_swaps` does, so a filename containing spaces (a ghost
/// `(deleted)` spelling) does not shift Size and Used. Malformed rows are
/// skipped rather than aborting the fold: this is a measurement, not a safety
/// proof.
///
/// Ghost rows are skipped too, and for a different reason than malformed ones.
/// A ghost is a well-formed row whose block device the kernel already lost, so
/// its Size and Used describe nothing that is still resident. The status
/// snapshot drops those rows before tier folding; this fold now does the same,
/// so the two published tier tables cannot disagree about the same host.
pub(super) fn swap_rows(text: &str) -> impl Iterator<Item = SwapRow> + '_ {
    text.lines().skip(1).filter_map(|line| {
        let cols: Vec<&str> = line.split_whitespace().collect();
        let n = cols.len();
        if n < 5 {
            return None;
        }
        let used_kib = cols[n - 2].parse::<u64>().ok()?;
        let size_kib = cols[n - 3].parse::<u64>().ok()?;
        let name = cols[..n - 4].join(" ");
        if name.is_empty() || cascade::is_ghost_swap_filename(&name) {
            return None;
        }
        Some(SwapRow {
            name,
            size_kib,
            used_kib,
        })
    })
}

/// Per-role used swap from one `/proc/swaps` observation, or its absence.
///
/// `None` means the reading does not exist — the file could not be read. A
/// successful read with no rows is `Some(SwapTiersKib::default())` and is a
/// measured zero: the host really has no swap devices. Folding the first into
/// the second would claim an empty tier set nobody observed.
pub fn read_swap_tiers() -> Option<SwapTiersKib> {
    let text = fs::read_to_string("/proc/swaps").ok()?;
    Some(swap_tiers_from(&text))
}

/// Fold one `/proc/swaps` text into per-role used swap, in KiB.
///
/// No rounding here. The unit the rows carry is KiB and that is what the fold
/// holds; whole-MiB figures are views taken later, at publication. A text with
/// no rows folds to the measured zeros of an empty table.
pub(super) fn swap_tiers_from(text: &str) -> SwapTiersKib {
    let mut zram_kib = 0u64;
    let mut vram_kib = 0u64;
    let mut ssd_kib = 0u64;
    for row in swap_rows(text) {
        match cascade::swap_tier_role(&row.name) {
            cascade::SwapTierRole::Zram => zram_kib += row.used_kib,
            cascade::SwapTierRole::Vram => vram_kib += row.used_kib,
            cascade::SwapTierRole::Fallback => ssd_kib += row.used_kib,
        }
    }
    SwapTiersKib {
        zram_kib,
        vram_kib,
        ssd_kib,
    }
}

/// Per-role swap capacity from one `/proc/swaps` observation, or its absence.
///
/// Same face as [`read_swap_tiers`]: `None` is an unreadable table, and a
/// successful read with no rows is a measured zero of an empty table. The two
/// readers take their own file reads, so a caller that needs both at one
/// instant must accept that they can disagree — and must not publish a
/// derived figure that assumes they cannot.
pub fn read_swap_tier_capacities()
-> Option<(TierCapacityStats, TierCapacityStats, TierCapacityStats)> {
    let text = fs::read_to_string("/proc/swaps").ok()?;
    Some(swap_capacities_from(&text))
}

/// Fold one `/proc/swaps` text into per-role capacity-and-occupancy, in KiB.
///
/// Pure: the caller owns the observation, this owns the fold. Same unit rule
/// as [`swap_tiers_from`].
pub(super) fn swap_capacities_from(
    text: &str,
) -> (TierCapacityStats, TierCapacityStats, TierCapacityStats) {
    let mut z_tot = 0u64;
    let mut z_use = 0u64;
    let mut v_tot = 0u64;
    let mut v_use = 0u64;
    let mut s_tot = 0u64;
    let mut s_use = 0u64;

    for row in swap_rows(text) {
        match cascade::swap_tier_role(&row.name) {
            cascade::SwapTierRole::Zram => {
                z_tot += row.size_kib;
                z_use += row.used_kib;
            }
            cascade::SwapTierRole::Vram => {
                v_tot += row.size_kib;
                v_use += row.used_kib;
            }
            cascade::SwapTierRole::Fallback => {
                s_tot += row.size_kib;
                s_use += row.used_kib;
            }
        }
    }

    (
        TierCapacityStats {
            total_kib: z_tot,
            used_kib: z_use,
        },
        TierCapacityStats {
            total_kib: v_tot,
            used_kib: v_use,
        },
        TierCapacityStats {
            total_kib: s_tot,
            used_kib: s_use,
        },
    )
}
// The measurement band — peaks, rate windows, percentiles and the KiB↔MiB
// publish conversions — lives in `stress/measure.rs`, the registered
// `stress/measure.rs` extraction slice. Re-exported here so the run body

pub fn read_tier_disk_total_bytes() -> (u64, u64, u64) {
    read_tier_disk_io().totals()
}

pub(super) fn read_tier_disk_io() -> TierDiskIoBytes {
    let text = fs::read_to_string("/proc/diskstats").unwrap_or_default();
    let swaps = fs::read_to_string("/proc/swaps").unwrap_or_default();
    let dm_aliases = cascade::dm_name_aliases(Path::new("/sys/block"));
    tier_disk_io_from(&text, &swaps, &dm_aliases)
}

#[cfg(test)]
pub(super) fn tier_disk_bytes_from(
    diskstats: &str,
    swaps: &str,
    dm_aliases: &BTreeMap<String, String>,
) -> (u64, u64, u64) {
    tier_disk_io_from(diskstats, swaps, dm_aliases).totals()
}

pub(super) fn tier_disk_io_from(
    diskstats: &str,
    swaps: &str,
    dm_aliases: &BTreeMap<String, String>,
) -> TierDiskIoBytes {
    let mut disk_devices: HashSet<String> = HashSet::new();
    for row in swap_rows(swaps) {
        if cascade::swap_tier_role(&row.name) != cascade::SwapTierRole::Fallback {
            continue;
        }
        let Some(base) = row.name.rsplit('/').next() else {
            continue;
        };
        let lower = base.to_ascii_lowercase();
        disk_devices.insert(cascade::diskstats_role_name(&lower, dm_aliases).to_string());
        disk_devices.insert(lower);
    }
    let mut io = TierDiskIoBytes::default();
    for line in diskstats.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() >= 10
            && let (Ok(read_sectors), Ok(write_sectors)) =
                (fields[5].parse::<u64>(), fields[9].parse::<u64>())
        {
            let dev = fields[2];
            let role_name = cascade::diskstats_role_name(dev, dm_aliases);
            let read_bytes = read_sectors.saturating_mul(512);
            let write_bytes = write_sectors.saturating_mul(512);
            match cascade::swap_tier_role(role_name) {
                cascade::SwapTierRole::Zram => {
                    io.zram_read = io.zram_read.saturating_add(read_bytes);
                    io.zram_write = io.zram_write.saturating_add(write_bytes);
                }
                cascade::SwapTierRole::Vram => {
                    io.vram_read = io.vram_read.saturating_add(read_bytes);
                    io.vram_write = io.vram_write.saturating_add(write_bytes);
                }
                cascade::SwapTierRole::Fallback => {
                    if disk_devices.contains(&dev.to_ascii_lowercase())
                        || disk_devices.contains(&role_name.to_ascii_lowercase())
                    {
                        io.ssd_read = io.ssd_read.saturating_add(read_bytes);
                        io.ssd_write = io.ssd_write.saturating_add(write_bytes);
                    }
                }
            }
        }
    }
    io
}

pub fn probe_allocation_latency_ms() -> f64 {
    let t0 = Instant::now();
    let mut page = vec![0u8; 4096];
    page[0] = 1;
    page[4095] = 2;
    std::hint::black_box(&page);
    t0.elapsed().as_secs_f64() * 1000.0
}

pub(super) struct SystemStressEnvironment {
    pub(super) progress: crate::operation_progress::OperationProgressGuard,
    pub(super) isolated_cgroup_dir: Option<std::path::PathBuf>,
    pub(super) orig_cgroup_dir: Option<std::path::PathBuf>,
}

impl SystemStressEnvironment {
    pub(super) fn new(_opts: &super::StressOptions) -> Self {
        let progress = crate::operation_progress::OperationProgressGuard::start(
            crate::operation_progress::OperationKind::StressTest,
            "stress_ramp",
            3,
        );
        let cg = fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
        let (isolated_cgroup_dir, orig_cgroup_dir) =
            enter_isolated_stress_cgroup(&cg, Path::new("/sys/fs/cgroup"), std::process::id());
        Self {
            progress,
            isolated_cgroup_dir,
            orig_cgroup_dir,
        }
    }
}

impl Drop for SystemStressEnvironment {
    fn drop(&mut self) {
        leave_isolated_stress_cgroup(
            self.isolated_cgroup_dir.as_deref(),
            self.orig_cgroup_dir.as_deref(),
            std::process::id(),
        );
    }
}

pub(super) fn enter_isolated_stress_cgroup(
    cgroup_contents: &str,
    sys_cgroup_root: &Path,
    pid: u32,
) -> (Option<std::path::PathBuf>, Option<std::path::PathBuf>) {
    let target_rel = cgroup_contents
        .lines()
        .find_map(|line| line.strip_prefix("0::/"))
        .map(str::trim)
        .filter(|rel| !rel.is_empty());
    let orig_dir = match target_rel {
        Some(rel) => sys_cgroup_root.join(rel),
        None => sys_cgroup_root.to_path_buf(),
    };
    let orig_opt = orig_dir.exists().then_some(orig_dir);
    let parent = if sys_cgroup_root.join("ramshared.slice").exists() {
        sys_cgroup_root.join("ramshared.slice")
    } else {
        sys_cgroup_root.to_path_buf()
    };
    let leaf = parent.join(format!("ramshared-stress-{pid}"));
    if fs::create_dir_all(&leaf).is_ok()
        && fs::write(leaf.join("cgroup.procs"), format!("{pid}\n")).is_ok()
    {
        (Some(leaf), orig_opt)
    } else {
        let _ = fs::remove_dir(&leaf);
        (None, orig_opt)
    }
}

pub(super) fn leave_isolated_stress_cgroup(
    isolated_dir: Option<&Path>,
    orig_dir: Option<&Path>,
    pid: u32,
) {
    if let Some(orig) = orig_dir {
        let _ = fs::write(orig.join("cgroup.procs"), format!("{pid}\n"));
    }
    if let Some(leaf) = isolated_dir
        && fs::remove_dir(leaf).is_err()
    {
        let _ = fs::remove_dir_all(leaf);
    }
}

pub(super) fn reclaim_cgroup_dir_bytes(cgroup_dir: &Path, total_bytes: usize) -> bool {
    const RECLAIM_SLICE_BYTES: usize = 32 * 1024 * 1024;
    if total_bytes == 0 {
        return false;
    }
    let reclaim_path = cgroup_dir.join("memory.reclaim");
    let mut remaining = total_bytes;
    let mut wrote_any = false;
    while remaining > 0 {
        let step = remaining.min(RECLAIM_SLICE_BYTES);
        match fs::write(&reclaim_path, format!("{step}\n")) {
            Ok(()) => {
                wrote_any = true;
                remaining -= step;
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                wrote_any = true;
                remaining -= step;
            }
            Err(_) => break,
        }
    }
    wrote_any
}

pub(super) fn reclaim_leaf_with_retry(
    isolated_dir: Option<&Path>,
    cgroup_contents: &str,
    sys_cgroup_root: &Path,
    bytes: usize,
) -> bool {
    if let Some(leaf) = isolated_dir {
        let ok = reclaim_cgroup_dir_bytes(leaf, bytes);
        if fs::read_to_string(leaf.join("memory.current"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .is_some_and(|cur| cur > 4 * 1024 * 1024)
        {
            let _ = reclaim_cgroup_dir_bytes(leaf, bytes);
        }
        ok
    } else {
        reclaim_cgroup_bytes(cgroup_contents, sys_cgroup_root, bytes)
    }
}

pub(super) fn reclaim_cgroup_bytes(
    cgroup_contents: &str,
    sys_cgroup_root: &Path,
    total_bytes: usize,
) -> bool {
    if total_bytes == 0 {
        return false;
    }
    let target_rel = cgroup_contents
        .lines()
        .find_map(|line| line.strip_prefix("0::/"))
        .map(str::trim)
        .filter(|rel| !rel.is_empty());
    let dir = match target_rel {
        Some(rel) => {
            let candidate = sys_cgroup_root.join(rel);
            if candidate.join("memory.reclaim").exists() {
                candidate
            } else {
                sys_cgroup_root.to_path_buf()
            }
        }
        None => sys_cgroup_root.to_path_buf(),
    };
    reclaim_cgroup_dir_bytes(&dir, total_bytes)
}

pub(super) fn is_cache_startup_pending(
    value: &serde_json::Value,
    now_ms: u64,
    daemon_instance_id: &str,
) -> bool {
    cascade::cache_status_is_current_daemon(value, daemon_instance_id, now_ms)
        && value.get("cache_state").and_then(|v| v.as_str()) == Some("UNAVAILABLE")
        && value
            .get("cache_unavailable_reason")
            .and_then(|v| v.as_str())
            .is_some_and(|r| r.contains("startup pending"))
}

pub(super) fn read_system_cache_status_sample() -> Option<CacheSample> {
    if let Some(sample) = read_cache_status_sample() {
        return Some(sample);
    }
    for _ in 0..8 {
        let value = crate::published_budget::read_status_document(Path::new(
            crate::published_budget::CACHE_STATUS_FILE,
        ))?;
        let daemon_id = cascade::current_daemon_instance_id()?;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis() as u64;
        if !is_cache_startup_pending(&value, now_ms, &daemon_id) {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        if let Some(sample) = read_cache_status_sample() {
            return Some(sample);
        }
    }
    None
}

pub(super) fn read_gpu_cache_run_telemetry() -> Option<GpuCacheRunTelemetry> {
    let value = crate::published_budget::read_status_document(Path::new(
        crate::published_budget::CACHE_STATUS_FILE,
    ))?;
    parse_gpu_cache_run_telemetry(&value)
}
