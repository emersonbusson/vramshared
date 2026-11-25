// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/stress/runner.rs
//! Stress harness execution driver, watchdog, Phase 2/3 hold cycler, and
//! report assembly.
//!
//! Extracted from `stress.rs` to keep orchestration modular alongside
//! `stress/phase_ramp.rs`.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::cascade;
use crate::zram_compression::{ZramCompression, read_zram_compression};

use super::budget::{
    CacheSample, full_profile_cache_target, full_tier_snapshot, read_cache_status_sample,
    require_physical_cache_before_cascade, require_tier3_only_capacity,
};
use super::format::{
    append_telemetry_log, archive_and_compare_benchmark, compute_telemetry_reading, mb_mark,
    print_phase1_banner, print_stress_summary, psi_mark, tier_pct_mark,
};
use super::host::{
    BuddyinfoReading, StressPsi, SystemStressEnvironment, count_kernel_hung_tasks,
    current_kernel_faults, is_wsl2, probe_allocation_latency_ms, psi_at_or_over_limit,
    read_buddyinfo_reading, read_gpu_cache_run_telemetry, read_mem_info, read_psi_full,
    read_swap_tier_capacities, read_swap_tiers, read_sysctl_min_free_kib,
    read_system_cache_status_sample, read_tier_disk_io, read_tier_disk_total_bytes,
    reclaim_cgroup_dir_bytes, reclaim_leaf_with_retry, stress_psi_sample,
    trigger_proactive_compaction,
};
use super::measure::{
    KIB_PER_MIB, ReportBuildInputs, TierDiskIoBytes, build_stress_report,
    fold_gpu_cache_run_telemetry, fold_peak_zram_compression, published_mib,
    sustained_hold_deadline,
};
use super::model::GpuCacheRunTelemetry;
use super::phase_ramp::{
    RunExecutionContext, StressRunState, run_phase1_ramp, verify_sampled_chunk_pages,
};
use super::{
    MemInfoKib, NORMAL_HOLD_INTERVAL_MS, NORMAL_TOUCH_BYTES, NORMAL_TOUCH_STRIDE_BYTES,
    StressOptions, StressReport, SwapTiersKib, TIER3_HEAVY_HOLD_INTERVAL_MS, TIER3_HEAVY_LOAD_PCT,
    TIER3_HEAVY_TOUCH_BYTES, TIER3_HEAVY_TOUCH_STRIDE_BYTES, TierCapacityStats,
    WSL2_MIN_PHYSICAL_HEADROOM_MB,
};

pub fn run(opts: &StressOptions) -> Result<(), String> {
    let env = SystemStressEnvironment::new(opts);
    run_with_environment(opts, &env).map(|_| ())
}

pub(super) trait StressEnvironment {
    fn is_wsl2(&self) -> bool {
        is_wsl2()
    }

    fn read_mem_info(&self) -> Option<MemInfoKib> {
        read_mem_info()
    }

    fn read_swap_tiers(&self) -> Option<SwapTiersKib> {
        read_swap_tiers()
    }

    fn read_swap_tier_capacities(
        &self,
    ) -> Option<(TierCapacityStats, TierCapacityStats, TierCapacityStats)> {
        read_swap_tier_capacities()
    }

    fn read_cache_status_sample(&self) -> Option<CacheSample> {
        read_cache_status_sample()
    }

    fn current_kernel_faults(&self) -> Option<u64> {
        current_kernel_faults()
    }

    fn read_psi_full(&self) -> Option<f64> {
        read_psi_full()
    }

    fn probe_allocation_latency_ms(&self) -> f64 {
        probe_allocation_latency_ms()
    }

    fn read_sysctl_min_free_kib(&self) -> u64 {
        read_sysctl_min_free_kib()
    }

    fn read_buddyinfo_reading(&self) -> BuddyinfoReading {
        read_buddyinfo_reading()
    }

    fn trigger_proactive_compaction(&self) {
        trigger_proactive_compaction();
    }

    fn read_tier_disk_total_bytes(&self) -> (u64, u64, u64) {
        read_tier_disk_total_bytes()
    }

    fn read_tier_disk_io(&self) -> TierDiskIoBytes {
        TierDiskIoBytes::from_totals(self.read_tier_disk_total_bytes())
    }

    fn read_gpu_cache_telemetry(&self) -> Option<GpuCacheRunTelemetry> {
        None
    }

    fn read_zram_compression(&self) -> Option<ZramCompression> {
        read_zram_compression()
    }

    fn count_kernel_hung_tasks(&self) -> Option<u64> {
        count_kernel_hung_tasks()
    }

    fn allocate_buffer(&self, requested_bytes: usize) -> Vec<u8> {
        vec![0u8; requested_bytes]
    }

    fn pageout_buffer(&self, _slice: &mut [u8]) {}

    fn report_progress(
        &self,
        _phase: &str,
        _target: Option<String>,
        _step_index: u32,
        _total_steps: u32,
        _completed_bytes: u64,
        _total_bytes: u64,
    ) {
    }
}

impl StressEnvironment for SystemStressEnvironment {
    fn read_cache_status_sample(&self) -> Option<CacheSample> {
        read_system_cache_status_sample()
    }

    fn read_tier_disk_io(&self) -> TierDiskIoBytes {
        read_tier_disk_io()
    }

    fn read_gpu_cache_telemetry(&self) -> Option<GpuCacheRunTelemetry> {
        read_gpu_cache_run_telemetry()
    }

    fn pageout_buffer(&self, slice: &mut [u8]) {
        if slice.is_empty() {
            return;
        }
        for _ in 0..10 {
            if self.read_psi_full().is_some_and(|p| p >= 2.2) {
                thread::sleep(Duration::from_millis(150));
            } else {
                break;
            }
        }
        let cg = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
        let _ = reclaim_leaf_with_retry(
            self.isolated_cgroup_dir.as_deref(),
            &cg,
            std::path::Path::new("/sys/fs/cgroup"),
            slice.len(),
        );
        if let Some(orig) = self.orig_cgroup_dir.as_deref() {
            for _ in 0..6 {
                let low_avail = self
                    .read_mem_info()
                    .is_some_and(|m| m.avail_kib < 5_400 * 1024);
                let psi_ok = self.read_psi_full().is_none_or(|p| p < 2.2);
                if !low_avail || !psi_ok || !reclaim_cgroup_dir_bytes(orig, 64 * 1024 * 1024) {
                    break;
                }
            }
        }
    }

    fn report_progress(
        &self,
        phase: &str,
        target: Option<String>,
        step_index: u32,
        _total_steps: u32,
        completed_bytes: u64,
        total_bytes: u64,
    ) {
        self.progress
            .update(crate::operation_progress::ProgressUpdate {
                phase,
                step_index,
                target: target.as_deref(),
                completed_bytes,
                total_bytes,
                throughput_mib_per_s: None,
            });
    }
}

fn spawn_term_watchdog(
    chunks: &Arc<Mutex<Vec<Vec<u8>>>>,
    last_heartbeat: &Arc<AtomicU64>,
    term_signal: &Arc<AtomicBool>,
) -> JoinHandle<()> {
    // Autonomous Watchdog Thread: If main thread stalls > 30s, clears memory automatically
    // and trips term_signal (fail-closed Hyper-V / WSL2 anti-hang protection).
    // 30s threshold: allocation under swap pressure (vec![0u8; 32MB] zeroing) can
    // legitimately take 5-15s; 4s was too tight and caused false-positive halts.
    let chunks_watchdog = chunks.clone();
    let heartbeat_watchdog = last_heartbeat.clone();
    let term_watchdog = term_signal.clone();
    thread::spawn(move || {
        while !term_watchdog.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(500));
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let last = heartbeat_watchdog.load(Ordering::Relaxed);
            if now.saturating_sub(last) > 30 {
                if let Ok(mut guard) = chunks_watchdog.lock()
                    && !guard.is_empty()
                {
                    guard.clear();
                }
                term_watchdog.store(true, Ordering::Relaxed);
                break;
            }
        }
    })
}

fn run_phase2_3_hold_cycler<E: StressEnvironment>(
    ctx: &RunExecutionContext<'_>,
    environment: &E,
    state: &mut StressRunState,
) {
    // Phase 2 & 3: Sustained Endurance Soak & Multi-Tier Read/Write Cycler
    let soak_active = !state.safety_halt
        && (ctx.opts.battery || ctx.opts.hold_sec > 0 || ctx.opts.duration_sec.is_some());
    if soak_active {
        let hold_start = Instant::now();
        let hold_end = sustained_hold_deadline(
            ctx.run_start,
            hold_start,
            ctx.opts.hold_sec,
            ctx.opts.duration_sec,
        );
        let planned_hold_secs = hold_end.saturating_duration_since(hold_start).as_secs();
        if !ctx.opts.json {
            println!("{}", "═".repeat(105));
            println!(
                " 🌊 PHASE 2 & 3: SUSTAINED SOAK & MULTI-TIER R/W CYCLER (Peak {}% for {}s)",
                state.max_safe_pct, planned_hold_secs
            );
            println!(
                " (Sustained pageouts + bit-exact readback across ZRAM, GPU CUDA PTX LZ4 cache & SSD origin)"
            );
            println!("{}", "═".repeat(105));
        }
        if ctx.cascade_mode {
            thread::sleep(Duration::from_millis(300));
        }

        let mut cycle: usize = 0;
        // An unreadable table refuses the percent (`pct()` on the empty
        // default is `None`), so the heavy-touch branch cannot be taken on a
        // figure nobody measured.
        let init_cap3 = environment
            .read_swap_tier_capacities()
            .map(|(_, _, cap3)| cap3)
            .unwrap_or_default();
        let mut hold_cap3_pct = init_cap3.pct();
        while Instant::now() < hold_end && !ctx.term_signal.load(Ordering::Relaxed) {
            if ctx.cascade_mode
                && let Err(error) = cascade::stress_continuation()
            {
                eprintln!("[stress] safety_halt: {error}");
                state.safety_halt = true;
                break;
            }
            if (ctx.opts.cascade || ctx.opts.tier3_target_pct.is_some())
                && environment.current_kernel_faults() != Some(0)
            {
                state.safety_halt = true;
                break;
            }
            // The safety comparison runs on what `/proc/meminfo` said. The
            // policy floor is whole-MiB by construction and is converted up to
            // that unit at this boundary, exactly. Saturating for the same
            // reason as the ramp's floor: an unreachable operator floor must
            // read as unreachable, not wrap into a permissive one.
            //
            // An unmeasured headroom is treated as at the floor: refuse to
            // touch rather than run a cycle on a figure nobody read.
            let free_before_touch_kib = environment.read_mem_info().map(|m| m.avail_kib);
            let psi_at_cycle_start =
                match stress_psi_sample(environment.read_psi_full(), ctx.required_psi) {
                    StressPsi::Measured(value) => Some(value),
                    StressPsi::Uninformed => None,
                    StressPsi::Invalid => {
                        eprintln!("[stress] safety_halt: psi_telemetry_invalid");
                        state.safety_halt = true;
                        break;
                    }
                };
            let pre_touch_floor_mb = if environment.is_wsl2() {
                ctx.opts.min_ram_mb.max(WSL2_MIN_PHYSICAL_HEADROOM_MB)
            } else {
                ctx.opts.min_ram_mb
            };
            // The two halves refuse in opposite directions on absence, and
            // each is right for its own reason. Headroom is the physical
            // floor: unmeasured is treated as at the floor, so the cycle does
            // not run on a figure nobody read. PSI here is a veto on observed
            // danger; on a platform that does not require it an uninformed
            // cycle has no veto to fire. What is never done is reading that
            // absence as a measured `0.0%`.
            if psi_at_or_over_limit(psi_at_cycle_start, ctx.opts.max_psi_full)
                || free_before_touch_kib
                    .is_none_or(|free| free <= pre_touch_floor_mb.saturating_mul(KIB_PER_MIB))
            {
                state.safety_halt = true;
                break;
            }
            cycle += 1;
            ctx.last_heartbeat.store(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                Ordering::Relaxed,
            );

            // Modify chunk pages smoothly to stimulate active tier traffic without clobbering
            // the deterministic page integrity header in bytes [0..1408].
            let mut verified_cycle_pages = 0usize;
            let mut cycle_mismatches = 0usize;
            if let Ok(mut guard) = ctx.chunks.lock()
                && !guard.is_empty()
            {
                let len = guard.len();
                let mut total_mismatches = 0usize;
                if ctx.cascade_mode {
                    for slot in 0..len.min(2) {
                        let v_idx = ((slot * len) / 2 + cycle) % len;
                        let win_len = guard[v_idx].len().min(512 * 1024);
                        let (v_pages, v_bad) =
                            verify_sampled_chunk_pages(&guard[v_idx][..win_len], v_idx, 4);
                        verified_cycle_pages += v_pages;
                        total_mismatches += v_bad;
                    }
                } else {
                    for rev_step in 0..len.min(4) {
                        let v_idx = ((rev_step * len) / 4 + cycle) % len;
                        let (v_pages, v_bad) = verify_sampled_chunk_pages(&guard[v_idx], v_idx, 32);
                        verified_cycle_pages += v_pages;
                        total_mismatches += v_bad;
                    }
                }
                cycle_mismatches = total_mismatches;
                let target_chunk = if hold_cap3_pct.is_some_and(|pct| pct >= TIER3_HEAVY_LOAD_PCT) {
                    if let Some(last) = guard.last_mut() {
                        last
                    } else {
                        continue;
                    }
                } else {
                    let idx = (cycle.wrapping_mul(7)) % len;
                    &mut guard[idx]
                };
                let chunk_len = target_chunk.len();
                let (limit, stride) = if ctx.cascade_mode {
                    (chunk_len.min(2 * 1024 * 1024), 4096)
                } else if hold_cap3_pct.is_some_and(|pct| pct >= TIER3_HEAVY_LOAD_PCT) {
                    (
                        chunk_len.min(TIER3_HEAVY_TOUCH_BYTES),
                        TIER3_HEAVY_TOUCH_STRIDE_BYTES,
                    )
                } else {
                    (chunk_len.min(NORMAL_TOUCH_BYTES), NORMAL_TOUCH_STRIDE_BYTES)
                };
                let touch_start = if chunk_len >= 4096 { 2048 } else { 0 };
                for offset in (touch_start..limit).step_by(stride) {
                    let prev = std::hint::black_box(target_chunk[offset]).wrapping_add(cycle as u8);
                    target_chunk[offset] = prev.wrapping_add((offset & 0xFF) as u8);
                }
                if ctx.cascade_mode || ctx.opts.battery {
                    environment.pageout_buffer(&mut target_chunk[..limit]);
                }
            }
            if cycle_mismatches > 0 {
                eprintln!(
                    "[stress] safety_halt: data_integrity_mismatch ({cycle_mismatches} bad page(s) in {verified_cycle_pages} checked)"
                );
                state.safety_halt = true;
                break;
            }
            environment.report_progress(
                "stress_verify",
                Some(format!("cycle {cycle} · {verified_cycle_pages} pages OK")),
                2,
                3,
                (cycle as u64).saturating_mul(NORMAL_TOUCH_BYTES as u64),
                (ctx.opts.hold_sec.max(1)).saturating_mul(2 * NORMAL_TOUCH_BYTES as u64),
            );

            let free_kib = environment.read_mem_info().map(|m| m.avail_kib);
            // Both swap faces come from `/proc/swaps`; a half-read is not an
            // observation of either, so the pair is accepted only together.
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
            // An unreadable table is not a sample. Neither fold takes one: a
            // synthesized zero would count as "no fallback swap seen" and
            // blank out a real spill, and a peak folded at `0` would make a
            // run that never read the table look like one that measured an
            // empty table.
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
                // Same-point admission fold as the ramp loop. `tiers.ssd_kib`
                // is this iteration's fallback reading.
                state.tier3_admission.observe(tiers.ssd_kib, cap1, cap2);
            }
            fold_peak_zram_compression(
                &mut state.peak_zram_compression,
                environment.read_zram_compression(),
            );
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
            let cache_sample = if ctx.opts.tier3_only {
                None
            } else {
                environment.read_cache_status_sample()
            };
            if ctx.cascade_mode && cache_sample.is_none() {
                state.cache_lost_during_stress = true;
                state.safety_halt = true;
                break;
            }
            if let Some(sample) = cache_sample {
                state.physical_cache_samples += 1;
                state.peak_physical_vram_kib = state.peak_physical_vram_kib.max(sample.cached_kib);
                state.peak_physical_target_kib =
                    state.peak_physical_target_kib.max(sample.target_kib);
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
            hold_cap3_pct = cap3.pct();
            let reading = compute_telemetry_reading(
                lat_ms,
                psi_full,
                state.total_allocated_mb,
                ctx.ram_total_mb,
                tiers.map(|tiers| tiers.total_mb()),
            )
            .with_tier_pcts(cap1.pct(), cap2.pct(), cap3.pct());
            state.peak_pressure = state.peak_pressure.max(reading.pressure_index);
            state.readings_count += 1;
            state.active_cycles_done += 1;
            append_telemetry_log(&ctx.opts.telemetry_log, &reading);

            if !ctx.opts.json {
                print!(
                    " [🌊 Cycle #{:>2}] Idx: {:>4.1} {} Swap: {} │ Free RAM: {} │ PSI: {:>5} │ Lat: {:.2}ms\r",
                    cycle,
                    reading.pressure_index,
                    reading.gauge,
                    mb_mark(reading.swap_used_mb),
                    mb_mark(free_kib.map(published_mib)),
                    psi_mark(psi_full),
                    lat_ms
                );
                let _ = io::stdout().flush();
            }
            let hold_sleep_ms = if hold_cap3_pct.is_some_and(|pct| pct >= TIER3_HEAVY_LOAD_PCT) {
                TIER3_HEAVY_HOLD_INTERVAL_MS
            } else if ctx.cascade_mode {
                750
            } else {
                NORMAL_HOLD_INTERVAL_MS
            };
            thread::sleep(Duration::from_millis(hold_sleep_ms));
            let now = Instant::now();
            let dt = now.duration_since(state.prev_sample_time).as_secs_f64();
            if state.rates.advance_io(environment.read_tier_disk_io(), dt) {
                state.prev_sample_time = now;
            }
        }
        if !ctx.opts.json {
            println!();
        }
    }
}

pub(super) fn run_with_environment<E: StressEnvironment>(
    opts: &StressOptions,
    environment: &E,
) -> Result<StressReport, String> {
    let cascade_mode = !opts.tier3_only && (opts.cascade || opts.tier3_target_pct.is_some());
    if cascade_mode {
        // Admission: a new pressure workload starts only on a fully healthy
        // cascade. Once running, continuation uses the tolerant gate.
        cascade::stress_readiness()?;
    }
    let required_psi = environment.is_wsl2() || cascade_mode || opts.tier3_only;
    if required_psi
        && !matches!(
            stress_psi_sample(environment.read_psi_full(), true),
            StressPsi::Measured(_)
        )
    {
        return Err("required memory PSI telemetry is unavailable or malformed".to_string());
    }
    let initial_cache_sample = if opts.tier3_only {
        None
    } else {
        environment.read_cache_status_sample()
    };
    require_physical_cache_before_cascade(
        cascade_mode,
        opts.physical_cache_target_mib,
        initial_cache_sample,
    )?;
    let initial_tier3 = environment.read_swap_tier_capacities().map(|(_, _, t3)| t3);
    require_tier3_only_capacity(opts.tier3_only, initial_tier3)?;
    let mut physical_cache_required_mib = opts.physical_cache_target_mib.unwrap_or(0);
    if opts.full_three_tier_profile {
        physical_cache_required_mib =
            full_profile_cache_target(opts.physical_cache_target_mib, initial_cache_sample)?;
    }
    let mut qualification_opts = opts.clone();
    if opts.full_three_tier_profile {
        qualification_opts.physical_cache_target_mib = Some(physical_cache_required_mib);
    }
    if (cascade_mode || opts.tier3_only) && environment.current_kernel_faults() != Some(0) {
        return Err("kernel fault evidence is unavailable or already contains faults".to_string());
    }
    let term_signal = Arc::new(AtomicBool::new(false));
    let last_heartbeat = Arc::new(AtomicU64::new(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    ));

    let chunks = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let watchdog_handle = spawn_term_watchdog(&chunks, &last_heartbeat, &term_signal);

    // `ram_total_kib` is what the step budget is derived from, so it keeps the
    // unit `/proc/meminfo` published. The two `*_mb` figures are printed and
    // used as gauge inputs, never compared against a floor, so they take the
    // whole-MiB publish view. Every figure that *is* compared stays in KiB —
    // see [`MemInfoKib`].
    //
    // An unreadable `/proc/meminfo` is not a measurement of zero RAM. The
    // budget fold below treats the absence as `0`, which collapses the step to
    // the policy minimum beside a fail-closed floor; the banner, which is a
    // claim about a measurement, prints the absence instead.
    let mem_init = environment.read_mem_info();
    let ram_total_kib = mem_init.map(|m| m.total_kib).unwrap_or(0);
    let ram_total_mb = mem_init.map(|m| published_mib(m.total_kib));
    let ram_avail_init = mem_init.map(|m| published_mib(m.avail_kib));
    let swap_init = environment.read_swap_tiers();
    let initial_ssd_used_kib = swap_init.map(|tiers| tiers.ssd_kib);
    let ram_scope_label = if environment.is_wsl2() {
        "WSL2 RAM"
    } else {
        "Physical Host RAM"
    };

    if !opts.json {
        print_phase1_banner(
            opts,
            ram_scope_label,
            ram_total_mb,
            ram_avail_init,
            swap_init,
        );
    }

    let run_start = Instant::now();
    let mut state = StressRunState::with_io(environment.read_tier_disk_io());
    let ctx = RunExecutionContext {
        opts,
        qualification_opts: &qualification_opts,
        cascade_mode,
        required_psi,
        ram_total_kib,
        ram_total_mb,
        initial_ssd_used_kib,
        term_signal: &term_signal,
        last_heartbeat: &last_heartbeat,
        chunks: &chunks,
        run_start,
    };

    run_phase1_ramp(&ctx, environment, &mut state);
    run_phase2_3_hold_cycler(&ctx, environment, &mut state);

    if term_signal.load(Ordering::Relaxed) {
        state.safety_halt = true;
    }
    // Post-ramp structural check. The supervisor may still be in its recovery
    // window after the pressure we just created; that is not a failed run.
    if cascade_mode && cascade::stress_continuation().is_err() {
        state.safety_halt = true;
    }

    // Phase 4: drop the test buffers; this is not a physical reclaim benchmark.
    let t_reclaim_start = Instant::now();
    if let Ok(mut guard) = ctx.chunks.lock() {
        guard.clear();
    }
    let reclaim_duration = t_reclaim_start.elapsed();

    term_signal.store(true, Ordering::Relaxed);
    let _ = watchdog_handle.join();

    thread::sleep(Duration::from_millis(500));
    let post_free_kib = environment.read_mem_info().map(|m| m.avail_kib);
    let post_swap_mb = environment.read_swap_tiers().map(|tiers| tiers.total_mb());
    let report = build_stress_report(ReportBuildInputs {
        opts,
        state: &state,
        cascade_mode,
        physical_cache_required_mib,
        initial_ssd_used_kib,
        kernel_faults: environment.current_kernel_faults(),
        hung_tasks: environment.count_kernel_hung_tasks(),
        post_free_kib,
        reclaim_duration,
        elapsed: run_start.elapsed(),
    });

    if opts.json {
        let json_out = serde_json::to_string_pretty(&report)
            .map_err(|e| format!("failed to serialize stress report: {e}"))?;
        println!("{json_out}");
    } else {
        print_stress_summary(
            opts,
            &report,
            post_swap_mb,
            post_free_kib,
            state.peak_total_swap_kib,
            state.peak_ssd.published_mib(),
            &state.tier3_admission,
        );
    }

    archive_and_compare_benchmark(&report, opts.json);

    if opts.full_three_tier_profile && report.status != "PASS_ZERO_PANIC" {
        return Err(format!(
            "full-three-tier qualification failed: simultaneous={} ZRAM={}/{}% NBD logical={}/{}% SSD={}/{}% physical cache={} / {} MiB required",
            report.simultaneous_full_tiers,
            tier_pct_mark(report.tier1_zram_pct),
            report.tier1_target_pct,
            tier_pct_mark(report.tier2_logical_swap_pct),
            report.tier2_target_pct,
            tier_pct_mark(report.tier3_ssd_pct),
            report.tier3_target_pct,
            mb_mark(report.tier2_vram_mb),
            report.physical_cache_required_mib,
        ));
    }
    Ok(report)
}
