// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/stress/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

pub(super) use super::*;

pub(super) use std::cell::{Cell, RefCell};

pub(super) use std::collections::VecDeque;

pub(super) struct StubStressEnvironment {
    pub(super) buddyinfo: RefCell<VecDeque<BuddyinfoReading>>,
    pub(super) compaction_calls: Cell<usize>,
    pub(super) latency_probes: Cell<usize>,
    pub(super) requested_allocations: RefCell<Vec<usize>>,
    pub(super) backing_allocations: RefCell<Vec<usize>>,
    /// Scripted per-role used-swap samples, in KiB. The total is derived
    /// from the three roles, so a stub cannot feed a state `/proc/swaps`
    /// could never produce — which is what a free fourth slot used to allow.
    /// Empty falls back to zero, which is what every pre-existing stub run
    /// wants.
    pub(super) swap_samples: RefCell<VecDeque<SwapTiersKib>>,
    /// Scripted PSI-full samples. Empty falls back to a measured `0.0`,
    /// which is what every pre-existing stub run wants. Absence is a
    /// different fact and gets its own switch, exactly as
    /// [`Self::meminfo_unreadable`] records: a sentinel hiding inside a
    /// queue of readings cannot say "the file could not be read".
    pub(super) psi_samples: RefCell<VecDeque<f64>>,
    /// Set when the fixture world's `/proc/pressure/memory` cannot be
    /// read at all. Every read of the face returns `None` while this is
    /// set — never a measured `0.0`.
    pub(super) psi_unreadable: Cell<bool>,
    /// Whether the fixture host is a WSL2 guest. Defaults to `true`,
    /// which is the world every pre-existing stub run was written
    /// against. Only a test that is specifically about the non-WSL2
    /// platform turns it off.
    pub(super) wsl2: Cell<bool>,
    /// Scripted `mm_stat` samples. Empty falls back to `None`, which is
    /// what every pre-existing stub run wants: the tier was not measured,
    /// and a synthesized zero would claim otherwise.
    pub(super) zram_compression_samples: RefCell<VecDeque<Option<ZramCompression>>>,
    /// Scripted `/proc/swaps` capacity triples, in the same unit the real
    /// fold carries. Empty (or exhausted) falls back to
    /// `TierCapacityStats::default()`, which is what every pre-existing
    /// stub run wants: no swap device, so no denominator.
    pub(super) capacity_samples:
        RefCell<VecDeque<(TierCapacityStats, TierCapacityStats, TierCapacityStats)>>,
    /// Set when the fixture world's `/proc/meminfo` cannot be read at all.
    ///
    /// The queue only scripts *what a readable file says*. Absence is a
    /// different fact — the production `fs::read_to_string(...).ok()?`
    /// shape — so it gets its own switch rather than a sentinel value
    /// hiding inside a queue of readings. Every read of the face returns
    /// `None` while this is set.
    pub(super) meminfo_unreadable: Cell<bool>,
    /// Set when the fixture world's `/proc/swaps` cannot be read at all.
    /// Same distinction as [`Self::meminfo_unreadable`]; covers both
    /// swap faces, which the production path co-samples from one file.
    pub(super) swap_table_unreadable: Cell<bool>,
}

impl StubStressEnvironment {
    /// The fixture world's readable `/proc/meminfo`: 8 GiB, all available.
    /// A fixture, not a product number. Held in KiB like the real fold, so
    /// a stub cannot hand the ramp a unit the production path never carries.
    const WORLD_MEM_INFO: MemInfoKib = MemInfoKib {
        total_kib: 8 * 1024 * 1024,
        avail_kib: 8 * 1024 * 1024,
    };

    fn new(buddyinfo: impl IntoIterator<Item = BuddyinfoReading>) -> Self {
        Self {
            buddyinfo: RefCell::new(buddyinfo.into_iter().collect()),
            compaction_calls: Cell::new(0),
            latency_probes: Cell::new(0),
            requested_allocations: RefCell::new(Vec::new()),
            backing_allocations: RefCell::new(Vec::new()),
            swap_samples: RefCell::new(VecDeque::new()),
            psi_samples: RefCell::new(VecDeque::new()),
            psi_unreadable: Cell::new(false),
            wsl2: Cell::new(true),
            zram_compression_samples: RefCell::new(VecDeque::new()),
            capacity_samples: RefCell::new(VecDeque::new()),
            meminfo_unreadable: Cell::new(false),
            swap_table_unreadable: Cell::new(false),
        }
    }

    fn with_swap_samples(mut self, samples: impl IntoIterator<Item = SwapTiersKib>) -> Self {
        self.swap_samples = RefCell::new(samples.into_iter().collect());
        self
    }

    /// The fixture world's `/proc/meminfo` is unreadable for the whole run.
    fn with_unreadable_meminfo(self) -> Self {
        self.meminfo_unreadable.set(true);
        self
    }

    /// The fixture world's `/proc/swaps` is unreadable for the whole run.
    /// Both swap faces report their absence; neither is a measured zero.
    fn with_unreadable_swap_table(self) -> Self {
        self.swap_table_unreadable.set(true);
        self
    }

    /// The fixture world's `/proc/pressure/memory` is unreadable for the
    /// whole run. Every PSI read reports its absence; none reports a
    /// measured `0.0`.
    fn with_unreadable_psi(self) -> Self {
        self.psi_unreadable.set(true);
        self
    }

    /// The fixture host is not a WSL2 guest. Used only by tests that are
    /// about the platform distinction; every other stub run stays on the
    /// WSL2 world it was written against.
    fn without_wsl2(self) -> Self {
        self.wsl2.set(false);
        self
    }

    fn with_capacity_samples(
        mut self,
        samples: impl IntoIterator<Item = (TierCapacityStats, TierCapacityStats, TierCapacityStats)>,
    ) -> Self {
        self.capacity_samples = RefCell::new(samples.into_iter().collect());
        self
    }

    fn with_psi_samples(mut self, samples: impl IntoIterator<Item = f64>) -> Self {
        self.psi_samples = RefCell::new(samples.into_iter().collect());
        self
    }

    fn with_zram_compression_samples(
        mut self,
        samples: impl IntoIterator<Item = Option<ZramCompression>>,
    ) -> Self {
        self.zram_compression_samples = RefCell::new(samples.into_iter().collect());
        self
    }
}

impl StressEnvironment for StubStressEnvironment {
    fn is_wsl2(&self) -> bool {
        self.wsl2.get()
    }

    fn read_mem_info(&self) -> Option<MemInfoKib> {
        if self.meminfo_unreadable.get() {
            return None;
        }
        Some(Self::WORLD_MEM_INFO)
    }

    fn read_swap_tiers(&self) -> Option<SwapTiersKib> {
        if self.swap_table_unreadable.get() {
            return None;
        }
        Some(
            self.swap_samples
                .borrow_mut()
                .pop_front()
                .unwrap_or_default(),
        )
    }

    fn read_swap_tier_capacities(
        &self,
    ) -> Option<(TierCapacityStats, TierCapacityStats, TierCapacityStats)> {
        if self.swap_table_unreadable.get() {
            return None;
        }
        Some(self.capacity_samples.borrow_mut().pop_front().unwrap_or((
            TierCapacityStats::default(),
            TierCapacityStats::default(),
            TierCapacityStats::default(),
        )))
    }

    fn read_cache_status_sample(&self) -> Option<CacheSample> {
        None
    }

    fn current_kernel_faults(&self) -> Option<u64> {
        Some(0)
    }

    fn read_psi_full(&self) -> Option<f64> {
        if self.psi_unreadable.get() {
            return None;
        }
        self.psi_samples.borrow_mut().pop_front().or(Some(0.0))
    }

    fn probe_allocation_latency_ms(&self) -> f64 {
        self.latency_probes.set(self.latency_probes.get() + 1);
        0.25
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
        self.zram_compression_samples
            .borrow_mut()
            .pop_front()
            .flatten()
    }

    fn count_kernel_hung_tasks(&self) -> Option<u64> {
        Some(0)
    }

    fn allocate_buffer(&self, requested_bytes: usize) -> Vec<u8> {
        self.requested_allocations
            .borrow_mut()
            .push(requested_bytes);
        let buffer = vec![0; requested_bytes.min(4096)];
        self.backing_allocations.borrow_mut().push(buffer.len());
        buffer
    }
}

#[path = "tests/allocation_and_buddyinfo.rs"]
mod allocation_and_buddyinfo;
#[path = "tests/cli_flags_and_psi.rs"]
mod cli_flags_and_psi;
#[path = "tests/phase_ramp_branch_tests.rs"]
mod phase_ramp_branch_tests;
#[path = "tests/ramp_and_rate_metrics.rs"]
mod ramp_and_rate_metrics;
#[path = "tests/stress_extra.rs"]
mod stress_extra;
#[path = "tests/tier_folds_and_budget.rs"]
mod tier_folds_and_budget;
