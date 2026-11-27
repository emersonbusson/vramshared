// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/stress/format.rs
//! Stress telemetry readings, flight logging, terminal table formatting, and
//! observation archiving.
//!
//! Extracted from `stress.rs` to keep presentation and serialization separate
//! from host probes and stress loop control.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::zram_compression::{ZramCompression, all_stored_pages_same_filled};

use super::classify::Tier3AdmissionWitness;
use super::measure::published_mib;
use super::{StressOptions, StressReport, SwapTiersKib, TelemetryReading};

pub fn compute_telemetry_reading(
    latency_ms: f64,
    psi_full: Option<f64>,
    ram_alloc_mb: u64,
    ram_total_mb: Option<u64>,
    swap_used_mb: Option<u64>,
) -> TelemetryReading {
    // Three gauge contributions, not interlocks, and each folds an absent
    // input into a **contribution** of zero. That `0.0` is not a published
    // measurement of no pressure: the index is a composite of what was
    // measured, and a term whose input nobody read has nothing to add. What
    // would be a fold is publishing a measured `0` for the term itself — see
    // `psi_full` and `swap_used_mb` on [`TelemetryReading`] and
    // [`StressReport::post_reclaim_free_ram_mb`] for where a figure *is* the
    // measurement and therefore must be `None` here.
    let ram_ratio = ram_total_mb
        .filter(|total| *total > 0)
        .map_or(0.0, |total| ram_alloc_mb as f64 / total as f64);

    let psi_term = psi_full.map_or(0.0, |psi| psi * 0.25);
    let swap_term = swap_used_mb.map_or(0.0, |mb| (mb as f64 / 1024.0) * 0.6);
    let idx_raw = 1.0 + (latency_ms * 0.8) + psi_term + (ram_ratio * 3.5) + swap_term;
    let pressure_index = idx_raw.clamp(1.0, 10.0);

    let (gauge, classification) = if pressure_index < 2.5 {
        ("[██░░░░░░░░]", "🟢 NOMINAL (RAM Active)")
    } else if pressure_index < 4.5 {
        ("[████░░░░░░]", "🟢 TIER-1 ACTIVE (In-RAM LZ4)")
    } else if pressure_index < 6.5 {
        ("[██████░░░░]", "🟡 TIER-2 ACTIVE (PCIe DMA VRAM)")
    } else if pressure_index < 8.5 {
        ("[████████░░]", "🟠 HIGH PRESSURE (Floor Protected)")
    } else {
        ("[██████████]", "🛡️  CEILING INTERLOCK (Damped)")
    };

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    TelemetryReading {
        timestamp_ms: ts,
        pressure_index,
        gauge: gauge.to_string(),
        latency_ms,
        psi_full,
        ram_used_mb: ram_alloc_mb,
        swap_used_mb,
        tier1_zram_pct: None,
        tier2_logical_swap_pct: None,
        tier3_ssd_pct: None,
        classification: classification.to_string(),
    }
}

/// Render one whole-MiB figure, or the absence of one.
///
/// The log line and the banner both claim a measurement. When there is not
/// one, they say so rather than printing a `0` the reader will take as a
/// measured empty figure.
pub(super) fn mb_mark(mb: Option<u64>) -> String {
    match mb {
        Some(mb) => format!("{mb:>5}MB"),
        None => "unmeasured".to_string(),
    }
}

/// Render one PSI full figure, or the absence of one.
///
/// Same rule as [`mb_mark`]: the log line claims a measurement, so when there
/// is not one it says so rather than printing a `0.0%` the reader will take
/// as measured zero pressure.
pub(super) fn psi_mark(psi_full: Option<f64>) -> String {
    match psi_full {
        Some(psi) => format!("{psi:.1}%"),
        None => "n/a".to_string(),
    }
}

/// One ramp-table cell carrying a whole-MiB figure and its unit, or its
/// absence.
///
/// The cell is one piece so the literal `MB` never attaches to an absence
/// marker: `     208 MB` is a measurement and ` unmeasured` is not one. Both
/// faces are 11 characters wide — the width `{:>8} MB` already drew — so the
/// row does not shift when a figure is missing.
pub(super) fn mb_cell(mb: Option<u64>) -> String {
    match mb {
        Some(mb) => format!("{mb:>8} MB"),
        None => format!("{:>11}", "unmeasured"),
    }
}

pub fn append_telemetry_log(path: &str, reading: &TelemetryReading) {
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(
            f,
            "[{}] IDX {:>4.1} {} Lat: {:>5.2}ms │ PSI: {:>5} │ RAM: {:>5}MB │ Swap: {} [T1:{:>4}|T2:{:>4}|T3:{:>4}] │ {}",
            reading.timestamp_ms,
            reading.pressure_index,
            reading.gauge,
            reading.latency_ms,
            psi_mark(reading.psi_full),
            reading.ram_used_mb,
            mb_mark(reading.swap_used_mb),
            tier_pct_mark(reading.tier1_zram_pct),
            tier_pct_mark(reading.tier2_logical_swap_pct),
            tier_pct_mark(reading.tier3_ssd_pct),
            reading.classification
        );
    }
}

/// Print the introductory header and column layout before Phase 1 starts.
pub(super) fn print_phase1_banner(
    opts: &StressOptions,
    ram_scope_label: &str,
    ram_total_mb: Option<u64>,
    ram_avail_init: Option<u64>,
    swap_init: Option<SwapTiersKib>,
) {
    println!("{}", "═".repeat(105));
    println!(" 🚀 RamShared Native Stress Governor & Multi-Tier Qualification Battery");
    println!(
        " Mode: {} │ Range: {}% ➔ {}% (+{}%) │ Safety Floor: >= {} MB{}{} │ Log: {}",
        if opts.battery {
            "FULL BATTERY (4 Phases)"
        } else {
            "Progressive 1%-by-1% Governor"
        },
        opts.start_pct,
        opts.target_pct,
        opts.step_pct,
        opts.min_ram_mb,
        opts.duration_sec
            .map(|s| format!(" │ Soak Target: {s}s"))
            .unwrap_or_default(),
        if opts.hold_sec > 0 {
            format!(" (Hold >= {}s)", opts.hold_sec)
        } else {
            String::new()
        },
        opts.telemetry_log
    );
    println!(
        "[i] {ram_scope_label}: {} (Available: {}) │ Active Swap: {}",
        mb_mark(ram_total_mb),
        mb_mark(ram_avail_init),
        mb_mark(swap_init.map(|tiers| tiers.total_mb())),
    );
    println!("{}", "═".repeat(105));
    println!(
        "┌───────┬────────────┬──────────────┬──────────────┬──────────────┬──────────────┬────────┬──────────┬─────────────┬───────────────────────────┐"
    );
    println!(
        "│ Level │ Alloc RAM  │ ZRAM (Tier1) │ VRAM (Tier2) │ SSD (Tier3)  │ Total Swap   │ PSI-F  │ Latency  │ Stress Bar  │ Tier Operating Status     │"
    );
    println!(
        "├───────┼────────────┼──────────────┼──────────────┼──────────────┼──────────────┼────────┼──────────┼─────────────┼───────────────────────────┤"
    );
}

/// Print the Phase 4 human-readable stress qualification summary.
#[allow(clippy::too_many_arguments)]
pub(super) fn print_stress_summary(
    opts: &StressOptions,
    report: &StressReport,
    post_swap_mb: Option<u64>,
    post_free_kib: Option<u64>,
    peak_total_swap_kib: Option<u64>,
    peak_ssd_mib: Option<u64>,
    tier3_admission: &Tier3AdmissionWitness,
) {
    println!("{}", "═".repeat(105));
    println!(" 🧹 PHASE 4: ATOMIC MEMORY RECLAIM & FLASH DEALLOCATION BENCHMARK");
    println!("{}", "═".repeat(105));
    println!(
        "[i] Test buffer drop duration: {:.2} ms (not physical reclaim)",
        report.buffer_drop_duration_ms
    );
    // Both figures name a measurement. When the post-run read did not
    // succeed they say so; the `✓` is a phase marker, not a claim that
    // every number on the line was observed.
    println!("[✓] Post-Reclaim Swap:      {}", mb_mark(post_swap_mb));
    println!(
        "[✓] Post-Reclaim Free RAM:  {} available",
        mb_mark(post_free_kib.map(published_mib))
    );
    println!("{}", "-".repeat(105));
    println!(" 📊 STRESS BATTERY QUALIFICATION REPORT:");
    println!(
        "  • Execution Mode:          {}",
        if report.cascade_mode {
            "MULTI-TIER CASCADE OBSERVATION"
        } else if report.battery_mode {
            "FULL 4-PHASE BATTERY"
        } else {
            "PROGRESSIVE GOVERNOR"
        }
    );
    println!(
        "  • Max Observed Allocation: {}% of RAM",
        report.max_safe_pct
    );
    println!(
        "  • Peak Memory Pressure:    {:.1} / 10.0",
        report.peak_pressure_index
    );
    println!("  • Flight Telemetry Log:    {}", opts.telemetry_log);
    println!(
        "  • Peak Allocated Memory:   {} MB",
        report.total_allocated_mb
    );
    // The operator is reading a live summary, so it gets the observation
    // itself: `unmeasured` when the swap table was never read. The report
    // archived below carries the same absence as `null`, which is what
    // the comparison gate sees and refuses to evaluate.
    println!(
        "  • Peak Total Swap Used:    {}",
        mb_mark(peak_total_swap_kib.map(published_mib))
    );
    println!(
        "  • Duration Elapsed:        {:.2} s{}",
        report.elapsed_sec,
        report
            .duration_sec
            .map(|s| format!(" (configured target {s}s)"))
            .unwrap_or_default()
    );
    println!(
        "  • Tier 1 (ZRAM Swap):      {} Peak ({}, peak {} / sustained avg {}, R={} / W={} MiB)",
        mb_mark(report.tier1_zram_mb),
        format_tier_capacity(report.tier1_zram_pct),
        format_throughput_mbs(report.tier1_throughput_mbs),
        format_throughput_mbs(report.tier1_avg_throughput_mbs),
        report
            .tier1_read_mb
            .map_or_else(|| "n/a".into(), |v| v.to_string()),
        report
            .tier1_write_mb
            .map_or_else(|| "n/a".into(), |v| v.to_string()),
    );
    // Named, and only named, when a real sample exists. "0.00:1" would
    // claim a compression measurement the harness never took. The report
    // is still archived below, so the sample is borrowed, not consumed.
    println!(
        "  • Tier 1 compression:      {}",
        format_tier1_compression(report.tier1_zram_compression.as_ref())
    );
    println!(
        "  • Physical VRAM cache:    {} Peak ({}); NBD logical swap {}, block peak {} / avg {} (R={} / W={} MiB)",
        mb_mark(report.tier2_vram_mb),
        format_cache_fill(report.tier2_vram_pct, report.tier2_physical_cache_target_mb),
        mb_mark(report.tier2_logical_swap_mb),
        format_throughput_mbs(report.tier2_nbd_throughput_mbs),
        format_throughput_mbs(report.tier2_nbd_avg_throughput_mbs),
        report
            .tier2_nbd_read_mb
            .map_or_else(|| "n/a".into(), |v| v.to_string()),
        report
            .tier2_nbd_write_mb
            .map_or_else(|| "n/a".into(), |v| v.to_string()),
    );
    if let Some(gpu) = report.gpu_cache_telemetry {
        let ratio_str = gpu
            .effective_ratio
            .map_or_else(|| "unmeasured".to_string(), |r| format!("{r:.2}:1"));
        println!(
            "  • Tier 2 GPU compression:  {ratio_str} (logical={:.2} MiB -> compressed={:.2} MiB, slab={:.1} MiB, writes={}, hits={}, fallback_reads={}, read={:.2} MiB, codec_err={})",
            gpu.logical_mb.unwrap_or(0.0),
            gpu.compressed_mb.unwrap_or(0.0),
            gpu.physical_slab_mb.unwrap_or(0.0),
            gpu.batched_writes,
            gpu.hits,
            gpu.fallback_reads,
            gpu.cache_read_mb,
            gpu.integrity_errors + gpu.decode_errors + gpu.timeouts,
        );
    } else if let Some(gpu_line) = format_live_gpu_cache_compression() {
        println!("  • Tier 2 GPU compression:  {gpu_line}");
    }
    println!(
        "  • Tier 3 (SSD Storage):    {} Peak ({}, peak {} / sustained avg {}, R={} / W={} MiB)",
        mb_mark(peak_ssd_mib),
        format_tier_capacity(report.tier3_ssd_pct),
        format_throughput_mbs(report.tier3_throughput_mbs),
        format_throughput_mbs(report.tier3_avg_throughput_mbs),
        report
            .tier3_read_mb
            .map_or_else(|| "n/a".into(), |v| v.to_string()),
        report
            .tier3_write_mb
            .map_or_else(|| "n/a".into(), |v| v.to_string()),
    );
    // The admission condition is a same-point fact the peaks cannot
    // answer, so it is stated beside them rather than left to the reader
    // to reconstruct. Three outcomes, each named: nothing spilled; spill
    // that the rule alarms on; and spill that is the designed last resort
    // because an upper tier was full at the same instant.
    // The occupancy face here is the observation, and the sentence is
    // exactly where "measured empty" and "never read" must stay apart.
    let admission_note = match (
            report.tier3_spill_samples.unwrap_or(0),
            tier3_admission.unintended_fallback_spill(),
            peak_ssd_mib,
        ) {
            (0, _, None) => {
                "the swap table was never read, so fallback occupancy is unmeasured".to_string()
            }
            (0, _, Some(0)) => "fallback swap not observed".to_string(),
            (0, _, Some(_)) => {
                "spill peak present but no complete sample, so the admission condition cannot be evaluated"
                    .to_string()
            }
            (samples, true, _) => format!(
                "ALARM: {samples} sample(s) with tier-1 and tier-2 free at the same instant"
            ),
            (samples, false, _) => format!(
                "spill in {samples} sample(s), each with an upper tier full (designed fallback)"
            ),
        };
    println!("  • Tier 3 admission:       {admission_note}");
    println!(
        "  • Active I/O Cycles:       {} cycles completed",
        report.active_io_cycles_completed
    );
    println!(
        "  • Simultaneous full tiers: {}",
        report.simultaneous_full_tiers
    );
    println!(
        "  • Physical cache samples:  {}",
        report.physical_cache_samples
    );
    println!(
        "  • Allocation Latency (P50): {:.4} ms (Median) │ P99: {:.4} ms (Tail Jitter) │ Max: {:.4} ms",
        report.p50_cycle_latency_ms, report.p99_cycle_latency_ms, report.max_cycle_latency_ms
    );
    println!(
        "  • Qualification verdict:  {} (requires independent integrity and kernel-log evidence)",
        report.status
    );
    println!("{}", "═".repeat(105));
}

/// The human figure for a dt-derived rate.
///
/// `not measured` is the harness's own absence wording and is only for a rate
/// with no accepted window behind it. A present `0.0` is a measured idle — a
/// window that really did see no traffic — and prints as a number. Collapsing
/// the two published a measured zero as if the harness had never looked.
pub(super) fn format_throughput_mbs(rate: Option<f64>) -> String {
    rate.map_or_else(
        || "not measured".to_string(),
        |mbs| format!("{mbs:.1} MB/s"),
    )
}

/// The human face of the physical-cache fill ratio.
///
/// `Some(p)` is a measured occupancy against a target that exists, and prints
/// beside that target so the percentage is never read without its denominator.
/// `None` is one of the two refusals [`cache_fill_pct`] names — no sample, or
/// no denominator — and prints `fill not measured` where the ratio goes. A
/// `0%` would claim a fullness measured against a target the document never
/// gave.
pub(super) fn format_cache_fill(pct: Option<u64>, target_mb: u64) -> String {
    pct.map_or_else(
        || "fill not measured".to_string(),
        |pct| format!("{pct}% of {target_mb} MB target"),
    )
}

/// The compact face of one tier occupancy percent.
///
/// `Some(p)` prints as the percent with its sign. `None` is a tier with no
/// swap device — the ratio has no denominator — and prints `n/a`. A `0%`
/// would claim an emptiness measured against a capacity that is not there.
///
/// Unpadded: the flight log pads to its `T1`/`T2`/`T3` column width and a
/// diagnostic error string does not want the column's spaces.
pub(super) fn tier_pct_mark(pct: Option<u64>) -> String {
    pct.map_or_else(|| "n/a".to_string(), |pct| format!("{pct}%"))
}

/// The sentence face of one tier occupancy percent against its capacity.
///
/// `Some(p)` is a measured occupancy and prints beside the word `capacity`, so
/// the percentage is never read without what it is a percentage of. `None` is
/// the refusal [`TierCapacityStats::pct`] names — no swap device, so no
/// denominator — and says so. `0% capacity` would claim an emptiness measured
/// against a capacity that is not there.
pub(super) fn format_tier_capacity(pct: Option<u64>) -> String {
    pct.map_or_else(
        || "no swap device".to_string(),
        |pct| format!("{pct}% capacity"),
    )
}

/// The human line for Tier 1 compression at the peak sample.
///
/// Three distinct quantities, and only three: the uncompressed data the device
/// held, the compressed form that data was stored as, and what that form
/// actually **costs** the host. The cost is `mem_used_bytes` — the device's own
/// bookkeeping included — and it is the number an operator budgets against.
/// Printing the compressed form without the cost invites reading the smaller
/// byte count as what the host paid. The kernel's compressor sits beside them
/// for the same reason the dashboard names it: it is part of the measurement
/// the sample describes.
///
/// Absence and emptiness are different claims and print different words.
/// `not measured` is for a sample that was never taken, and for a pair of
/// counts the kernel cannot produce. A sample that was taken and holds
/// nothing is a measurement of emptiness — the byte counts read `0`
/// deliberately — and says so. Collapsing the two would publish a measured
/// zero as if the harness had never looked. `0.00:1` is still never printed:
/// there is no ratio to report, and a ratio of `1.0` or `0.0` would claim a
/// compression measurement that did not happen.
///
/// A ratio below `1:1` is a real measurement — the data expanded — and is
/// printed, never refused.
///
/// A zero on one side is two different facts and which side it is on decides
/// which. `compr == 0` with data held is what `mm_stat` reports when every
/// stored page was **same-filled** — each page's elements all hold one value,
/// so the device stores a fill reference and allocates no compressed body.
/// Those pages count toward `orig` and never toward `compr`. This is not
/// deduplication: zram does not compare pages against each other. That is the
/// most-compressed state the tier can be in, not a torn sample, and the line
/// prints `all same-filled` where the ratio goes — the same three-state
/// contract the dashboard's compression row draws. The classification is only
/// published while the `same_pages` counter does not contradict it; see
/// [`all_stored_pages_same_filled`]. The impossible mirror pair
/// (`orig == 0` while a compressed form exists) has no reading and is
/// `not measured`, never a rewritten body.
pub(super) fn format_tier1_compression(sample: Option<&ZramCompression>) -> String {
    let Some(sample) = sample else {
        return "not measured".to_string();
    };
    let Some((original, compressed)) = sample.resident_compression() else {
        return "not measured".to_string();
    };
    let compressor = sample.compressor.as_deref().filter(|name| !name.is_empty());
    let compressor = compressor.unwrap_or("compressor unreported");
    // The wire field keeps the kernel's own column name (`huge_pages`); the
    // face names what that column counts. docs.kernel.org: "the number of
    // incompressible pages". It is not THP, so the operator-facing noun is
    // `incompressible` on both faces — matching the dashboard's compression
    // row, which must not publish two names for one counter.
    let huge_pages = sample.huge_pages.map_or_else(
        || "incompressible unreported".to_string(),
        |count| format!("{count} incompressible"),
    );
    // The measured body is shared by every face that has one, so the three
    // states differ only in the ratio's slot. A reader comparing two lines
    // sees the same columns in the same order.
    let body = format!(
        "({} -> {} B, cost {} B, {compressor}, {huge_pages})",
        original, compressed, sample.mem_used_bytes
    );
    let ratio_text = match sample.compression_ratio() {
        Some(ratio) => format!("{ratio:.2}:1 at peak"),
        None if original == 0 && sample.backed_data_bytes.is_some_and(|bytes| bytes > 0) => {
            return "no resident pages (backing storage in use)".to_string();
        }
        None if original == 0 && compressed == 0 => {
            return "holding nothing (no ratio yet)".to_string();
        }
        None if compressed == 0 => {
            // The bytes say every stored page was same-filled; a measured
            // `same_pages` of zero says none was. A sample that disagrees
            // with itself supports no classification.
            if !all_stored_pages_same_filled(original, compressed, sample.same_pages) {
                return "not measured".to_string();
            }
            "all same-filled at peak".to_string()
        }
        None => return "not measured".to_string(),
    };
    format!("{ratio_text} {body}")
}

pub(super) fn format_system_time(st: SystemTime) -> String {
    let dur = st.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let days = dur / 86400;
    let rem_sec = dur % 86400;
    let hours = rem_sec / 3600;
    let minutes = (rem_sec % 3600) / 60;
    let seconds = rem_sec % 60;
    let mut y = 1970;
    let mut d = days;
    loop {
        let leap = (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0);
        let days_in_year = if leap { 366 } else { 365 };
        if d < days_in_year {
            break;
        }
        d -= days_in_year;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0);
    let days_in_months = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 1;
    for &dim in &days_in_months {
        if d < dim {
            break;
        }
        d -= dim;
        m += 1;
    }
    let day = d + 1;
    format!("{y:04}-{m:02}-{day:02}_{hours:02}-{minutes:02}-{seconds:02}")
}

pub(super) fn format_live_gpu_cache_compression() -> Option<String> {
    format_live_gpu_cache_compression_from(std::path::Path::new(
        crate::published_budget::CACHE_STATUS_FILE,
    ))
}

pub(super) fn format_live_gpu_cache_compression_from(path: &std::path::Path) -> Option<String> {
    let value = crate::published_budget::read_status_document(path)?;
    let gpu_cache = value.get("gpu_cache")?;
    let codec_state = gpu_cache.get("codec")?.get("state")?.as_str()?;
    let logical = gpu_cache.get("logical_cached_bytes")?.as_u64()?;
    let compressed = gpu_cache.get("compressed_payload_bytes")?.as_u64()?;
    let raw = gpu_cache.get("raw_payload_bytes")?.as_u64()?;
    let slab = gpu_cache.get("physical_cache_slab_bytes")?.as_u64()?;
    let stored = compressed.saturating_add(raw);
    let hits = value
        .get("cache_hits")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let fallback = value
        .get("cache_fallback_reads")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let batched_writes = value
        .get("origin_cache_io")
        .and_then(|v| v.get("batched_writes"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let read_bytes = value
        .get("origin_cache_io")
        .and_then(|v| v.get("cache_read_bytes"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    if stored > 0 && logical > 0 {
        let ratio = logical as f64 / stored as f64;
        Some(format!(
            "{ratio:.2}:1 ({logical} -> {stored} B in {slab} B VRAM slab, codec={codec_state}, batched_writes={batched_writes}, hits={hits}, fallback={fallback}, read_bytes={read_bytes} B)"
        ))
    } else {
        Some(format!(
            "codec={codec_state} (logical={logical} B, stored={stored} B, slab={slab} B, batched_writes={batched_writes}, hits={hits}, fallback={fallback}, read_bytes={read_bytes} B)"
        ))
    }
}

pub(super) fn archive_and_compare_benchmark(report: &StressReport, suppress_stdout: bool) {
    if cfg!(test) {
        return;
    }
    let directory = std::env::temp_dir().join("ramshared-benchmarks");
    if fs::create_dir_all(&directory).is_err() {
        return;
    }
    let filename = format!("observation-{}.json", format_system_time(SystemTime::now()));
    let path = directory.join(filename);
    if let Ok(encoded) = serde_json::to_vec_pretty(report)
        && fs::write(&path, encoded).is_ok()
        && !suppress_stdout
    {
        println!(
            "Observation saved at {} (unqualified until evidence review)",
            path.display()
        );
    }
}
