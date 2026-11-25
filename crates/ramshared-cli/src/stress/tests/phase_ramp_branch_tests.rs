// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/stress/tests/phase_ramp_branch_tests.rs
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::stress::budget::CacheSample;
use crate::stress::host::BuddyinfoReading;
use crate::stress::phase_ramp::{RunExecutionContext, StressRunState, run_phase1_ramp};
use crate::stress::runner::StressEnvironment;
use crate::stress::{MemInfoKib, StressOptions, SwapTiersKib, TierCapacityStats};
use crate::zram_compression::ZramCompression;

struct RampTestEnv {
    wsl2: bool,
    mem_samples: RefCell<VecDeque<Option<MemInfoKib>>>,
    default_mem: Option<MemInfoKib>,
    mem_reads: Cell<usize>,
    term_after_mem_reads: Option<(usize, Arc<AtomicBool>)>,
    swap_samples: RefCell<VecDeque<Option<SwapTiersKib>>>,
    default_swap: Option<SwapTiersKib>,
    capacity_samples:
        RefCell<VecDeque<Option<(TierCapacityStats, TierCapacityStats, TierCapacityStats)>>>,
    default_caps: Option<(TierCapacityStats, TierCapacityStats, TierCapacityStats)>,
    cache_samples: RefCell<VecDeque<Option<CacheSample>>>,
    psi_samples: RefCell<VecDeque<Option<f64>>>,
    latency_samples: RefCell<VecDeque<f64>>,
    buddyinfo: RefCell<VecDeque<BuddyinfoReading>>,
    kernel_faults: Option<u64>,
    compaction_calls: Cell<usize>,
}

impl RampTestEnv {
    fn new() -> Self {
        Self {
            wsl2: false,
            mem_samples: RefCell::new(VecDeque::new()),
            default_mem: Some(MemInfoKib {
                total_kib: 8 * 1024 * 1024,
                avail_kib: 4 * 1024 * 1024,
            }),
            mem_reads: Cell::new(0),
            term_after_mem_reads: None,
            swap_samples: RefCell::new(VecDeque::new()),
            default_swap: Some(SwapTiersKib::default()),
            capacity_samples: RefCell::new(VecDeque::new()),
            default_caps: Some((
                TierCapacityStats::default(),
                TierCapacityStats::default(),
                TierCapacityStats::default(),
            )),
            cache_samples: RefCell::new(VecDeque::new()),
            psi_samples: RefCell::new(VecDeque::new()),
            latency_samples: RefCell::new(VecDeque::new()),
            buddyinfo: RefCell::new(VecDeque::new()),
            kernel_faults: Some(0),
            compaction_calls: Cell::new(0),
        }
    }
}

impl StressEnvironment for RampTestEnv {
    fn is_wsl2(&self) -> bool {
        self.wsl2
    }

    fn read_mem_info(&self) -> Option<MemInfoKib> {
        let count = self.mem_reads.get() + 1;
        self.mem_reads.set(count);
        if let Some((limit, sig)) = &self.term_after_mem_reads
            && count >= *limit
        {
            sig.store(true, Ordering::Relaxed);
        }
        self.mem_samples
            .borrow_mut()
            .pop_front()
            .unwrap_or(self.default_mem)
    }

    fn read_swap_tiers(&self) -> Option<SwapTiersKib> {
        self.swap_samples
            .borrow_mut()
            .pop_front()
            .unwrap_or(self.default_swap)
    }

    fn read_swap_tier_capacities(
        &self,
    ) -> Option<(TierCapacityStats, TierCapacityStats, TierCapacityStats)> {
        self.capacity_samples
            .borrow_mut()
            .pop_front()
            .unwrap_or(self.default_caps)
    }

    fn read_cache_status_sample(&self) -> Option<CacheSample> {
        self.cache_samples.borrow_mut().pop_front().flatten()
    }

    fn current_kernel_faults(&self) -> Option<u64> {
        self.kernel_faults
    }

    fn read_psi_full(&self) -> Option<f64> {
        self.psi_samples
            .borrow_mut()
            .pop_front()
            .unwrap_or(Some(0.0))
    }

    fn probe_allocation_latency_ms(&self) -> f64 {
        self.latency_samples.borrow_mut().pop_front().unwrap_or(0.2)
    }

    fn read_sysctl_min_free_kib(&self) -> u64 {
        0
    }

    fn read_buddyinfo_reading(&self) -> BuddyinfoReading {
        self.buddyinfo
            .borrow_mut()
            .pop_front()
            .unwrap_or(BuddyinfoReading::Measured(64))
    }

    fn trigger_proactive_compaction(&self) {
        self.compaction_calls.set(self.compaction_calls.get() + 1);
    }

    fn read_tier_disk_total_bytes(&self) -> (u64, u64, u64) {
        (0, 0, 0)
    }

    fn read_zram_compression(&self) -> Option<ZramCompression> {
        None
    }

    fn count_kernel_hung_tasks(&self) -> Option<u64> {
        Some(0)
    }

    fn allocate_buffer(&self, requested_bytes: usize) -> Vec<u8> {
        vec![0u8; requested_bytes.min(1024 * 1024)]
    }
}

fn temp_log(name: &str) -> String {
    std::env::temp_dir()
        .join(format!(
            "ramshared-ramp-{name}-{}-{}.log",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ))
        .display()
        .to_string()
}

fn run_ramp_case(
    opts: &StressOptions,
    env: &RampTestEnv,
    cascade_mode: bool,
    term_signal: Arc<AtomicBool>,
) -> StressRunState {
    let last_heartbeat = Arc::new(AtomicU64::new(0));
    let chunks = Arc::new(Mutex::new(Vec::new()));
    let mut state = StressRunState::new((0, 0, 0));
    let ctx = RunExecutionContext {
        opts,
        qualification_opts: opts,
        cascade_mode,
        required_psi: true,
        ram_total_kib: 8 * 1024 * 1024,
        ram_total_mb: Some(8192),
        initial_ssd_used_kib: Some(0),
        term_signal: &term_signal,
        last_heartbeat: &last_heartbeat,
        chunks: &chunks,
        run_start: std::time::Instant::now(),
    };
    run_phase1_ramp(&ctx, env, &mut state);
    let _ = fs::remove_file(&opts.telemetry_log);
    state
}

#[test]
fn phase_ramp_covers_full_tier_target_compaction_and_non_json_halt_paths() {
    // 1. Non-JSON run: proactive compaction -> retry success -> full_tier_snapshot -> tier3 target reached.
    let mut opts = StressOptions {
        start_pct: 1,
        target_pct: 100,
        step_pct: 1,
        interval_ms: 0,
        min_ram_mb: 256,
        min_order_7_chunks: 8,
        tier1_target_pct: 95,
        tier2_target_pct: 95,
        tier3_target_pct: Some(15),
        json: false,
        telemetry_log: temp_log("full-tier"),
        ..StressOptions::default()
    };
    let mut env = RampTestEnv::new();
    env.wsl2 = true;
    let full_caps = (
        TierCapacityStats {
            total_kib: 100,
            used_kib: 95,
        },
        TierCapacityStats {
            total_kib: 100,
            used_kib: 95,
        },
        TierCapacityStats {
            total_kib: 100,
            used_kib: 15,
        },
    );
    let empty_caps = (
        TierCapacityStats::default(),
        TierCapacityStats::default(),
        TierCapacityStats::default(),
    );
    *env.capacity_samples.borrow_mut() = VecDeque::from([
        Some(empty_caps),
        Some(empty_caps),
        Some(full_caps),
        Some(full_caps),
    ]);
    let full_cache = CacheSample {
        cached_kib: 1024 * 1024,
        target_kib: 1024 * 1024,
        at_target: true,
    };
    *env.cache_samples.borrow_mut() = VecDeque::from([
        Some(full_cache),
        Some(full_cache),
        Some(full_cache),
        Some(full_cache),
    ]);
    *env.buddyinfo.borrow_mut() = VecDeque::from([
        BuddyinfoReading::Measured(0),
        BuddyinfoReading::Measured(64),
        BuddyinfoReading::Measured(9),
    ]);
    let st = run_ramp_case(&opts, &env, false, Arc::new(AtomicBool::new(false)));
    assert_eq!(env.compaction_calls.get(), 1);
    assert!(st.tier3_target_achieved);
    assert!(st.simultaneous_full_tiers);

    // 2. tier3_only target reached + cascade limit target=100 branch + buddy Halt.
    opts.tier3_only = true;
    opts.telemetry_log = temp_log("tier3-only");
    *env.capacity_samples.borrow_mut() = VecDeque::from([
        Some(empty_caps),
        Some(empty_caps),
        Some(full_caps),
        Some(full_caps),
    ]);
    *env.buddyinfo.borrow_mut() = VecDeque::from([
        BuddyinfoReading::Measured(64),
        BuddyinfoReading::Measured(64),
    ]);
    let st2 = run_ramp_case(&opts, &env, false, Arc::new(AtomicBool::new(false)));
    assert!(st2.tier3_target_achieved);

    // 3. Buddy Halt, latency halt, PSI invalid, term_signal, faults, and cascade continuation.
    let mut halt_opts = StressOptions {
        start_pct: 1,
        target_pct: 2,
        cascade: true,
        interval_ms: 0,
        json: false,
        min_order_7_chunks: 8,
        telemetry_log: temp_log("buddy-halt"),
        ..StressOptions::default()
    };
    let mut buddy_env = RampTestEnv::new();
    buddy_env.wsl2 = true;
    *buddy_env.buddyinfo.borrow_mut() = VecDeque::from([
        BuddyinfoReading::Measured(1),
        BuddyinfoReading::Measured(1),
        BuddyinfoReading::Measured(1),
        BuddyinfoReading::Measured(1),
    ]);
    let st_buddy = run_ramp_case(
        &halt_opts,
        &buddy_env,
        false,
        Arc::new(AtomicBool::new(false)),
    );
    assert!(st_buddy.safety_halt);

    halt_opts.cascade = false;
    halt_opts.telemetry_log = temp_log("lat-halt");
    let lat_env = RampTestEnv::new();
    *lat_env.latency_samples.borrow_mut() = VecDeque::from([50.0]);
    let st_lat = run_ramp_case(
        &halt_opts,
        &lat_env,
        false,
        Arc::new(AtomicBool::new(false)),
    );
    assert!(st_lat.safety_halt);

    halt_opts.telemetry_log = temp_log("psi-invalid");
    let psi_env = RampTestEnv::new();
    *psi_env.psi_samples.borrow_mut() = VecDeque::from([Some(-1.0)]);
    assert!(
        run_ramp_case(
            &halt_opts,
            &psi_env,
            false,
            Arc::new(AtomicBool::new(false))
        )
        .safety_halt
    );

    halt_opts.target_pct = 100;
    halt_opts.cascade = true;
    halt_opts.telemetry_log = temp_log("term-preset");
    assert!(
        run_ramp_case(
            &halt_opts,
            &RampTestEnv::new(),
            false,
            Arc::new(AtomicBool::new(true))
        )
        .safety_halt
    );

    halt_opts.tier3_target_pct = Some(15);
    halt_opts.telemetry_log = temp_log("faults-halt");
    let mut fault_env = RampTestEnv::new();
    fault_env.kernel_faults = Some(1);
    assert!(
        run_ramp_case(
            &halt_opts,
            &fault_env,
            false,
            Arc::new(AtomicBool::new(false))
        )
        .safety_halt
    );

    halt_opts.telemetry_log = temp_log("cascade-cont");
    assert!(
        run_ramp_case(
            &halt_opts,
            &RampTestEnv::new(),
            true,
            Arc::new(AtomicBool::new(false))
        )
        .safety_halt
    );
}

#[test]
fn phase_ramp_covers_drain_wait_and_zero_budget_recovery_paths() {
    // 1. Single-tier floor halt when avail_kib <= hard_floor_kib.
    let single_opts = StressOptions {
        start_pct: 1,
        target_pct: 2,
        interval_ms: 0,
        min_ram_mb: 256,
        min_order_7_chunks: 1,
        json: false,
        telemetry_log: temp_log("single-floor"),
        ..StressOptions::default()
    };
    let mut single_env = RampTestEnv::new();
    single_env.default_mem = Some(MemInfoKib {
        total_kib: 8 * 1024 * 1024,
        avail_kib: 250 * 1024,
    });
    let st_single = run_ramp_case(
        &single_opts,
        &single_env,
        false,
        Arc::new(AtomicBool::new(false)),
    );
    assert_eq!(st_single.total_allocated_mb, 0);

    // 2. Multi-tier safe_alloc_kib == 0 recovery then halt via PSI spike.
    let multi_opts = StressOptions {
        start_pct: 1,
        target_pct: 2,
        interval_ms: 0,
        min_ram_mb: 256,
        tier3_only: true,
        max_psi_full: 10.0,
        min_order_7_chunks: 1,
        json: false,
        telemetry_log: temp_log("zero-budget-recover"),
        ..StressOptions::default()
    };
    let recover_env = RampTestEnv::new();
    *recover_env.mem_samples.borrow_mut() = VecDeque::from([
        Some(MemInfoKib {
            total_kib: 8 * 1024 * 1024,
            avail_kib: 263 * 1024,
        }),
        Some(MemInfoKib {
            total_kib: 8 * 1024 * 1024,
            avail_kib: 600 * 1024,
        }),
    ]);
    let st_rec = run_ramp_case(
        &multi_opts,
        &recover_env,
        false,
        Arc::new(AtomicBool::new(false)),
    );
    assert!(st_rec.total_allocated_mb > 0);

    let mut psi_abort_env = RampTestEnv::new();
    psi_abort_env.default_mem = Some(MemInfoKib {
        total_kib: 8 * 1024 * 1024,
        avail_kib: 263 * 1024,
    });
    *psi_abort_env.psi_samples.borrow_mut() = VecDeque::from([Some(0.0), Some(15.0)]);
    let mut psi_opts = multi_opts.clone();
    psi_opts.telemetry_log = temp_log("zero-budget-psi");
    let _ = run_ramp_case(
        &psi_opts,
        &psi_abort_env,
        false,
        Arc::new(AtomicBool::new(false)),
    );

    // 3. Multi-tier drain wait: swap growth + availability recovery, and term_signal exit.
    let mut drain_recover_env = RampTestEnv::new();
    drain_recover_env.wsl2 = true;
    *drain_recover_env.mem_samples.borrow_mut() = VecDeque::from([
        Some(MemInfoKib {
            total_kib: 8 * 1024 * 1024,
            avail_kib: 500 * 1024,
        }),
        Some(MemInfoKib {
            total_kib: 8 * 1024 * 1024,
            avail_kib: 900 * 1024,
        }),
    ]);
    *drain_recover_env.swap_samples.borrow_mut() = VecDeque::from([
        Some(SwapTiersKib {
            zram_kib: 100,
            vram_kib: 0,
            ssd_kib: 0,
        }),
        Some(SwapTiersKib {
            zram_kib: 200,
            vram_kib: 0,
            ssd_kib: 0,
        }),
    ]);
    let drain_opts = StressOptions {
        start_pct: 1,
        target_pct: 2,
        interval_ms: 0,
        min_ram_mb: 600,
        tier3_only: true,
        min_order_7_chunks: 1,
        json: false,
        telemetry_log: temp_log("drain-recover"),
        ..StressOptions::default()
    };
    let _ = run_ramp_case(
        &drain_opts,
        &drain_recover_env,
        false,
        Arc::new(AtomicBool::new(false)),
    );

    let term = Arc::new(AtomicBool::new(false));
    let mut drain_term_env = RampTestEnv::new();
    drain_term_env.default_mem = Some(MemInfoKib {
        total_kib: 8 * 1024 * 1024,
        avail_kib: 200 * 1024,
    });
    drain_term_env.term_after_mem_reads = Some((2, term.clone()));
    let mut term_opts = multi_opts.clone();
    term_opts.telemetry_log = temp_log("drain-term");
    let _ = run_ramp_case(&term_opts, &drain_term_env, false, term);
}
