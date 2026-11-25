// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/spill_pump.rs
/*
 * RamShared - the zram spill pump (SPEC zram-writeback-vram-tier, DT-8).
 *
 * The writeback topology: zram holds the tier's swap pages and spills the
 * compressed cold ones to the tier device. This kernel writes to the
 * backing device only when the idle pages are marked and the writeback is
 * triggered, and its `writeback_limit` is a one-shot budget that returns
 * `-EIO` when exhausted. So somebody has to own the pump: this module is
 * that owner.
 *
 * The policy is pure and testable (plan, classification, ordering); the
 * runner is the thin part that writes sysfs on a cadence. Nothing here
 * decides anything about the tier's worth: the pump's loss is safe by
 * construction (the zram stays full and the overflow reaches the fallback).
 */

use serde::Serialize;
use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The kernel's writeback budget is a one-shot allowance: the pump sets it
/// per interval and the kernel returns `-EIO` once it is spent.
pub const DEFAULT_BUDGET_PAGES: u64 = 8192;

/// The pump's cadence. Five seconds keeps the spill inside the tier's
/// measured serving band without a busy loop.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(5);

/// How long a page must be idle before the pump is willing to spill it.
/// The kernel's age tracking (`CONFIG_ZRAM_TRACK_ENTRY_ACTIME`) is the
/// prerequisite: without it the only accepted marking is "all", which
/// would spill the hot working set.
pub const DEFAULT_IDLE_SECONDS: u64 = 60;

/// What the pump does in one tick, in order. The order is part of the
/// contract: the pages must be marked idle before the writeback is
/// triggered, and the budget must be armed before it too.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PumpStep {
    /// The sysfs attribute to write.
    pub attribute: &'static str,
    /// The value to write.
    pub value: String,
}

/// The plan for one tick, in the order the writes must happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PumpPlan {
    pub steps: Vec<PumpStep>,
}

// The kernel's age tracking (`CONFIG_ZRAM_TRACK_ENTRY_ACTIME`) is the
// prerequisite for the age-based marking; a kernel without it refuses the
// marking and the pump's proactive spill stays off (the kernel's own
// cold-page writeback still applies).

/// The outcome of the writeback trigger.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WritebackOutcome {
    /// The budget was spent: the kernel answered `-EIO`. This is the
    /// designed signal, not a failure — the next interval refills it.
    BudgetExhausted,
    /// No zram is backed by the tier yet (the cascade binds it after the
    /// daemon starts). The pump waits for the next tick.
    NoTarget,
    /// The kernel cannot mark pages idle by age (no
    /// `CONFIG_ZRAM_TRACK_ENTRY_ACTIME`), so the **proactive** spill is
    /// off. The kernel's own writeback still applies: when the zram's
    /// pool cannot take a page, the kernel writes the cold pages back to
    /// the tier by itself. Spilling everything idle instead would put the
    /// working set on the tier — the class this architecture exists to
    /// avoid — so the pump does nothing and says so.
    ProactiveUnavailable,
    /// The writeback completed.
    Completed,
    /// Anything else: the pump cannot continue safely.
    Failed(String),
}

/// Classify a writeback trigger's result. The kernel's `EIO` on the
/// writeback attribute is the budget's exhaustion signal by design
/// (`zram_drv.c`: `if (zram->wb_limit_enable && !zram->bd_wb_limit) ret
/// = -EIO`), so it must never be reported as a failure.
pub fn classify_writeback(result: &io::Result<String>) -> WritebackOutcome {
    match result {
        Ok(_) => WritebackOutcome::Completed,
        Err(error) => {
            let text = error.to_string();
            if text.contains("Input/output error") || text.contains("EIO") {
                WritebackOutcome::BudgetExhausted
            } else {
                WritebackOutcome::Failed(text)
            }
        }
    }
}

/// Build the tick's plan. The budget is the tier's allowance for one
/// interval; the idle marking uses the age cutoff.
pub fn plan(zram_device: &str, budget_pages: u64, idle_seconds: u64) -> PumpPlan {
    PumpPlan {
        steps: vec![
            PumpStep {
                attribute: "idle",
                value: idle_seconds.to_string(),
            },
            PumpStep {
                attribute: "writeback_limit",
                value: budget_pages.to_string(),
            },
            PumpStep {
                attribute: "writeback",
                value: "idle".to_string(),
            },
        ],
    }
    .tap(zram_device)
}

impl PumpPlan {
    /// The sysfs path of one step, under the zram device's sysfs directory.
    pub fn path(&self, zram_device: &str, attribute: &str) -> PathBuf {
        let name = zram_device.rsplit('/').next().unwrap_or(zram_device);
        Path::new("/sys/block").join(name).join(attribute)
    }
}

/// Small helper so the plan can name the device it belongs to.
trait Tap {
    fn tap(self, device: &str) -> Self;
}

impl Tap for PumpPlan {
    fn tap(self, _device: &str) -> Self {
        self
    }
}

/// Find the zram device that is backed by `tier_device`.
///
/// The daemon may start before the cascade binds zram's `backing_dev`, so
/// the pump discovers its zram instead of being told one: the only zram
/// whose `backing_dev` names the tier is the tier's spill target. An
/// unreadable or absent attribute yields no device — the pump waits for
/// the next tick rather than guessing.
pub fn discover_zram_for_tier(sys_block_root: &Path, tier_device: &str) -> Option<String> {
    let tier = canonical_device_name(tier_device);
    let mut found = None;
    for entry in std::fs::read_dir(sys_block_root).ok()? {
        let entry = entry.ok()?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("zram") {
            continue;
        }
        let backing = std::fs::read_to_string(entry.path().join("backing_dev")).ok()?;
        let backing = backing.trim();
        if backing.is_empty() {
            continue;
        }
        if canonical_device_name(backing) == tier {
            found = Some(format!("/dev/{name}"));
            break;
        }
    }
    found
}

/// The device name without its `/dev/` prefix, so the sysfs name and the
/// device path compare equal.
fn canonical_device_name(device: &str) -> &str {
    device.trim().rsplit('/').next().unwrap_or(device.trim())
}

/// DT-9: the floor applies only to a cycle that wrote at least 90% of its
/// budget. A smaller cycle is `underfilled`, not a throughput failure.
pub const FULL_BUDGET_PERCENT: u64 = 90;

/// DT-9: the measured floor for a full-budget cycle (the measured band is
/// 29–33 MiB/s at a 32 MiB cycle).
pub const DEFAULT_RATE_FLOOR_KIB_PER_S: u64 = 5 * 1024;

/// DT-9: one cycle over this ceiling stops the pump.
pub const DEFAULT_HARD_CEILING: Duration = Duration::from_secs(30);

/// DT-9: consecutive slow full-budget cycles before the pump stops.
pub const DEFAULT_SLOW_CYCLES_BEFORE_STOP: u8 = 3;

/// DT-10: the only pump-status surface.
pub const PUMP_HEALTH_PATH: &str = "/run/ramshared/pump-health.json";

/// DT-10: the document is bounded so a reader can refuse a runaway writer.
pub const PUMP_HEALTH_MAX_BYTES: usize = 4096;

/// DT-10: schema 1 (additive fields would bump it).
pub const PUMP_HEALTH_SCHEMA_VERSION: u32 = 1;

/// How many cycle durations feed the p99. Bounded memory: the health
/// document only reports the tail, not a history.
const CYCLE_WINDOW: usize = 100;

/// The pinned pump states (SPEC Observability).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PumpState {
    /// A target is bound and the last cycle did not cross a threshold.
    Bound,
    /// The writeback failed with a non-budget error: the tier is gone.
    TierLost,
    /// The pump was stopped explicitly (shutdown); never a teardown.
    #[cfg_attr(not(test), allow(dead_code))]
    PumpStopped,
    /// A threshold stopped the pump: slow cycles or the hard ceiling.
    Degraded,
}

/// DT-10: absent means not measured, never zero. Every rate/count is
/// nullable so a reader can tell "wrote nothing" from "did not measure".
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PumpHealth {
    pub schema_version: u32,
    pub state: PumpState,
    pub last_cycle_mib: Option<u64>,
    pub effective_mib_per_s: Option<u32>,
    pub p99_cycle_ms: Option<u64>,
    pub consecutive_slow_cycles: u8,
}

impl Default for PumpHealth {
    fn default() -> Self {
        Self::not_measured()
    }
}

impl PumpHealth {
    /// The document before the first tick: schema and state only.
    pub fn not_measured() -> Self {
        PumpHealth {
            schema_version: PUMP_HEALTH_SCHEMA_VERSION,
            state: PumpState::Bound,
            last_cycle_mib: None,
            effective_mib_per_s: None,
            p99_cycle_ms: None,
            consecutive_slow_cycles: 0,
        }
    }
}

/// One measured cycle: the `bd_stat` write delta and the duration of the
/// `writeback` sysfs write (DT-9). `wrote_kib` is `None` when the kernel
/// does not expose `bd_stat`; that is not the same fact as zero.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PumpCycleSample {
    pub wrote_kib: Option<u64>,
    pub elapsed: Duration,
    pub effective_kib_per_s: Option<u64>,
    pub full_budget: bool,
    pub slow: bool,
    pub over_ceiling: bool,
}

/// The stop policy (DT-9). Pure data so a drill can tighten the floor or
/// the ceiling without a kernel. The slow-cycle count is the pinned three
/// (`stop_after_three_slow_cycles`) and is not tunable per host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PumpPolicy {
    pub rate_floor_kib_per_s: u64,
    pub hard_ceiling: Duration,
}

impl Default for PumpPolicy {
    fn default() -> Self {
        PumpPolicy {
            rate_floor_kib_per_s: DEFAULT_RATE_FLOOR_KIB_PER_S,
            hard_ceiling: DEFAULT_HARD_CEILING,
        }
    }
}

/// DT-9: three consecutive slow cycles stop the pump. Pure so the threshold
/// is provable without time or a tier.
pub fn stop_after_three_slow_cycles(consecutive_slow_cycles: u8) -> bool {
    consecutive_slow_cycles >= DEFAULT_SLOW_CYCLES_BEFORE_STOP
}

/// Parse the zram `bd_stat` body: `bd_count bd_reads bd_writes`, in 4 KiB
/// blocks. The third field is the write count the cycle delta uses.
pub fn parse_bd_stat(text: &str) -> Option<u64> {
    text.split_whitespace().nth(2)?.parse().ok()
}

/// Read the cycle counter from `/sys/block/<zram>/bd_stat`. Absent or
/// malformed is `None`, never zero.
pub fn read_bd_stat(path: &Path) -> Option<u64> {
    parse_bd_stat(&std::fs::read_to_string(path).ok()?)
}

/// The pump's configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PumpConfig {
    /// The tier device the spill target is bound to. The zram that carries
    /// it is discovered from the kernel's `backing_dev`, because the daemon
    /// starts before the cascade binds it.
    pub tier_device: String,
    /// Where the kernel's block devices live.
    pub sys_block_root: PathBuf,
    /// The per-interval writeback budget, in 4 KiB pages.
    pub budget_pages: u64,
    /// The idle cutoff, in seconds.
    pub idle_seconds: u64,
    /// The tick cadence.
    pub interval: Duration,
}

impl PumpConfig {
    pub fn new(tier_device: impl Into<String>) -> Self {
        PumpConfig {
            tier_device: tier_device.into(),
            sys_block_root: PathBuf::from("/sys/block"),
            budget_pages: DEFAULT_BUDGET_PAGES,
            idle_seconds: DEFAULT_IDLE_SECONDS,
            interval: DEFAULT_INTERVAL,
        }
    }

    /// The zram device that is backed by the tier, if the cascade has
    /// bound it yet.
    pub fn target(&self) -> Option<String> {
        discover_zram_for_tier(&self.sys_block_root, &self.tier_device)
    }
}

/// The result of one tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TickOutcome {
    pub writeback: WritebackOutcome,
    /// The steps that were written before the outcome was known.
    pub written: usize,
    /// The age-based idle marking was refused and the kernel's "all"
    /// fallback was used: the spill covers the working set too.
    pub idle_fallback: bool,
}

/// Run one tick without measurement. This is the test seam for the step
/// order and the refusal paths; production goes through `tick_measured`,
/// which owns the same step loop and additionally samples `bd_stat`.
#[cfg(test)]
pub fn tick<F>(config: &PumpConfig, write: F) -> TickOutcome
where
    F: FnMut(&Path, &str) -> io::Result<String>,
{
    tick_measured(
        config,
        &PumpPolicy::default(),
        write,
        |_| None,
        || Duration::ZERO,
    )
    .0
}

/// Run one tick and measure its cycle (DT-9): the `bd_stat` write delta
/// across the `writeback` write and the write's duration. The clock is a
/// closure so the measurement is deterministic in tests; production passes
/// `Instant::now().elapsed()`.
pub fn tick_measured<W, R, N>(
    config: &PumpConfig,
    policy: &PumpPolicy,
    mut write: W,
    read_counter: R,
    mut now: N,
) -> (TickOutcome, Option<PumpCycleSample>)
where
    W: FnMut(&Path, &str) -> io::Result<String>,
    R: Fn(&Path) -> Option<u64>,
    N: FnMut() -> Duration,
{
    let Some(zram_device) = config.target() else {
        return (
            TickOutcome {
                writeback: WritebackOutcome::NoTarget,
                written: 0,
                idle_fallback: false,
            },
            None,
        );
    };
    let plan = plan(&zram_device, config.budget_pages, config.idle_seconds);
    let counter_path = plan.path(&zram_device, "bd_stat");
    let mut written = 0;
    for step in &plan.steps {
        if step.attribute == "writeback" {
            let before = read_counter(&counter_path);
            let started = now();
            let result = write(&plan.path(&zram_device, step.attribute), &step.value);
            let elapsed = now().saturating_sub(started);
            let after = read_counter(&counter_path);
            let outcome = classify_writeback(&result);
            let sample = measure_cycle(
                before,
                after,
                elapsed,
                config.budget_pages,
                policy,
                &outcome,
            );
            return (
                TickOutcome {
                    writeback: outcome,
                    written: written + 1,
                    idle_fallback: false,
                },
                sample,
            );
        }
        // The order is a contract: without the idle marking or the armed
        // budget the writeback would spill nothing (or everything), so a
        // missing attribute stops the tick and is reported.
        if let Err(error) = write(&plan.path(&zram_device, step.attribute), &step.value) {
            if step.attribute == "idle" {
                return (
                    TickOutcome {
                        writeback: WritebackOutcome::ProactiveUnavailable,
                        written,
                        idle_fallback: true,
                    },
                    None,
                );
            }
            return (
                TickOutcome {
                    writeback: WritebackOutcome::Failed(format!("{}: {error}", step.attribute)),
                    written,
                    idle_fallback: false,
                },
                None,
            );
        }
        written += 1;
    }
    (
        TickOutcome {
            writeback: WritebackOutcome::Failed("the writeback step was not reached".into()),
            written,
            idle_fallback: false,
        },
        None,
    )
}

/// DT-9: classify one cycle. The floor is applied only when the delta is at
/// least 90% of the budget; an absent counter yields no throughput fact.
fn measure_cycle(
    before: Option<u64>,
    after: Option<u64>,
    elapsed: Duration,
    budget_pages: u64,
    policy: &PumpPolicy,
    outcome: &WritebackOutcome,
) -> Option<PumpCycleSample> {
    // Only a completed trigger (budget exhaustion included) is a cycle.
    match outcome {
        WritebackOutcome::Completed
        | WritebackOutcome::BudgetExhausted
        | WritebackOutcome::Failed(_) => {}
        WritebackOutcome::NoTarget | WritebackOutcome::ProactiveUnavailable => return None,
    }
    // `bd_stat` counts 4 KiB blocks; a shrinking counter is a reset, not a
    // negative cycle.
    let wrote_kib = match (before, after) {
        (Some(before), Some(after)) if after >= before => {
            Some(after.saturating_sub(before).saturating_mul(4))
        }
        _ => None,
    };
    let effective_kib_per_s = wrote_kib.map(|kib| {
        let micros = elapsed.as_micros().max(1);
        let rate = (u128::from(kib) * 1_000_000) / micros;
        u64::try_from(rate).unwrap_or(u64::MAX)
    });
    let budget_kib = budget_pages.saturating_mul(4);
    let full_budget = wrote_kib.is_some_and(|kib| {
        u128::from(kib) * 100 >= u128::from(budget_kib) * u128::from(FULL_BUDGET_PERCENT)
    });
    let slow =
        full_budget && effective_kib_per_s.is_some_and(|rate| rate < policy.rate_floor_kib_per_s);
    Some(PumpCycleSample {
        wrote_kib,
        elapsed,
        effective_kib_per_s,
        full_budget,
        slow,
        over_ceiling: elapsed > policy.hard_ceiling,
    })
}

/// The bounded p99 of the recent cycle durations.
fn p99_cycle_ms(cycles: &VecDeque<u64>) -> Option<u64> {
    if cycles.is_empty() {
        return None;
    }
    let mut sorted: Vec<u64> = cycles.iter().copied().collect();
    sorted.sort_unstable();
    let rank = (99 * sorted.len()).div_ceil(100);
    sorted
        .get(rank.saturating_sub(1).min(sorted.len() - 1))
        .copied()
}

/// The runner and its health, without a thread. Each `step` is one cycle;
/// a stop is sticky and never retried (DT-8).
pub struct PumpRunner {
    config: PumpConfig,
    policy: PumpPolicy,
    health: PumpHealth,
    warned_fallback: bool,
    cycles: VecDeque<u64>,
    stopped: bool,
    next_interval: Duration,
    two_phase: two_phase::TwoPhasePump,
}

impl PumpRunner {
    pub fn new(config: PumpConfig, policy: PumpPolicy) -> Self {
        let next_interval = config.interval;
        PumpRunner {
            config,
            policy,
            health: PumpHealth::not_measured(),
            warned_fallback: false,
            cycles: VecDeque::new(),
            stopped: false,
            next_interval,
            two_phase: two_phase::TwoPhasePump::inactive(),
        }
    }

    pub fn health(&self) -> &PumpHealth {
        &self.health
    }

    /// The sleep interval selected by the most recent cycle's pressure band.
    pub fn next_interval(&self) -> Duration {
        self.next_interval
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn stopped(&self) -> bool {
        self.stopped
    }

    /// Idempotent explicit stop. It never joins the thread and never
    /// touches the tier. The daemon's shutdown path drops the thread
    /// instead of waiting for it (DT-8); this is the in-process contract.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn stop(&mut self) {
        if !self.stopped {
            self.stopped = true;
            self.health.state = PumpState::PumpStopped;
        }
    }

    /// Run at most one cycle at the base schedule; returns whether the pump must stop.
    #[cfg(test)]
    pub fn step<W, R, N>(&mut self, write: W, read_counter: R, now: N) -> bool
    where
        W: FnMut(&Path, &str) -> io::Result<String>,
        R: Fn(&Path) -> Option<u64>,
        N: FnMut() -> Duration,
    {
        self.step_with_utilization(None, write, read_counter, now)
    }

    /// Run at most one cycle using the pressure-adaptive schedule for the
    /// observed zram utilization percentage.
    pub fn step_with_utilization<W, R, N>(
        &mut self,
        zram_util_pct: Option<u8>,
        mut write: W,
        read_counter: R,
        mut now: N,
    ) -> bool
    where
        W: FnMut(&Path, &str) -> io::Result<String>,
        R: Fn(&Path) -> Option<u64>,
        N: FnMut() -> Duration,
    {
        if self.stopped {
            return true;
        }
        let schedule = adaptive::EffectiveSchedule::for_utilization(&self.config, zram_util_pct);
        self.next_interval = schedule.interval;
        let effective = schedule.apply_to(&self.config);
        let (outcome, sample) = if self.two_phase.is_active() {
            self.two_phase.step(
                &effective,
                &self.policy,
                &mut write,
                &read_counter,
                &mut now,
            )
        } else {
            // On real Linux sysfs, enforce the per-cycle budget before any
            // writeback trigger when `writeback_limit_enable` is present.
            if let Some(zdev) = effective.target() {
                let name = zdev.rsplit('/').next().unwrap_or(&zdev);
                if effective
                    .sys_block_root
                    .join(name)
                    .join("writeback_limit_enable")
                    .exists()
                    && let Err(error) = two_phase::enable_writeback_limit(&zdev, &mut write)
                {
                    let fail = TickOutcome {
                        writeback: WritebackOutcome::Failed(error),
                        written: 0,
                        idle_fallback: false,
                    };
                    report(&fail, &mut self.warned_fallback);
                    self.apply(&fail.writeback, None);
                    return self.stopped;
                }
            }
            let (direct_outcome, direct_sample) = tick_measured(
                &effective,
                &self.policy,
                &mut write,
                &read_counter,
                &mut now,
            );
            if direct_outcome.writeback == WritebackOutcome::ProactiveUnavailable {
                self.two_phase.activate();
                self.two_phase.step(
                    &effective,
                    &self.policy,
                    &mut write,
                    &read_counter,
                    &mut now,
                )
            } else {
                (direct_outcome, direct_sample)
            }
        };
        report(&outcome, &mut self.warned_fallback);
        self.apply(&outcome.writeback, sample.as_ref());
        self.stopped
    }

    fn apply(&mut self, outcome: &WritebackOutcome, sample: Option<&PumpCycleSample>) {
        match outcome {
            WritebackOutcome::NoTarget | WritebackOutcome::ProactiveUnavailable => {}
            WritebackOutcome::Failed(_) => {
                // DT-8: one non-budget failure is enough. Never retry into a
                // dead tier and never unbind the backing device.
                self.health.state = PumpState::TierLost;
                self.stopped = true;
                return;
            }
            WritebackOutcome::Completed | WritebackOutcome::BudgetExhausted => {}
        }
        let Some(sample) = sample else {
            // Absent `bd_stat`: the cycle's throughput is not measured. It
            // neither counts as slow nor resets a real slow run.
            return;
        };
        self.cycles.push_back(sample.elapsed.as_millis() as u64);
        while self.cycles.len() > CYCLE_WINDOW {
            self.cycles.pop_front();
        }
        self.health.p99_cycle_ms = p99_cycle_ms(&self.cycles);
        self.health.last_cycle_mib = sample.wrote_kib.map(|kib| kib / 1024);
        self.health.effective_mib_per_s = sample
            .effective_kib_per_s
            .map(|kib| u32::try_from(kib / 1024).unwrap_or(u32::MAX));
        if sample.over_ceiling {
            // DT-9: one cycle over the ceiling stops the pump. The tier may
            // still be alive; this is a degraded pump, not a teardown.
            self.health.state = PumpState::Degraded;
            self.stopped = true;
            return;
        }
        if sample.slow {
            self.health.consecutive_slow_cycles =
                self.health.consecutive_slow_cycles.saturating_add(1);
            if stop_after_three_slow_cycles(self.health.consecutive_slow_cycles) {
                self.health.state = PumpState::Degraded;
                self.stopped = true;
            }
        } else {
            self.health.consecutive_slow_cycles = 0;
        }
    }
}

/// The sysfs writer used in production.
pub fn write_sysfs(path: &Path, value: &str) -> io::Result<String> {
    std::fs::write(path, value).map(|_| String::new())
}

/// Render the health document. Bounded before it can reach the filesystem.
pub fn render_health_json(health: &PumpHealth) -> io::Result<String> {
    let body = serde_json::to_string(health).map_err(io::Error::other)?;
    if body.len() > PUMP_HEALTH_MAX_BYTES {
        return Err(io::Error::other("pump health exceeds the 4 KiB bound"));
    }
    Ok(body)
}

/// DT-10: atomic replace at 0644. A symlink at either the target or the
/// temporary path is refused rather than followed.
pub fn write_health(path: &Path, health: &PumpHealth) -> io::Result<()> {
    let body = render_health_json(health)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    for candidate in [path, temporary.as_path()] {
        match std::fs::symlink_metadata(candidate) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(io::Error::other("pump health path is a symlink"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    std::fs::write(&temporary, body)?;
    std::fs::File::open(&temporary)?.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o644))?;
    }
    std::fs::rename(&temporary, path)
}

/// Spawn the pump thread. The pump stops with the process; its loss is safe
/// (the zram stays full, the overflow reaches the fallback).
/// The pump's reporting for one tick. The fallback is a configuration
/// fact, not a per-tick event: it is reported once.
pub fn report(outcome: &TickOutcome, warned_fallback: &mut bool) {
    if outcome.idle_fallback && !*warned_fallback {
        *warned_fallback = true;
        eprintln!(
            "[ramsharedd] spill pump: kernel refused age-based idle marking \
             (CONFIG_ZRAM_TRACK_ENTRY_ACTIME is missing); engaging bounded two-phase \
             idle aging + huge-page writeback"
        );
    }
    match &outcome.writeback {
        WritebackOutcome::Completed
        | WritebackOutcome::BudgetExhausted
        | WritebackOutcome::NoTarget
        | WritebackOutcome::ProactiveUnavailable => {}
        WritebackOutcome::Failed(error) => {
            eprintln!("[ramsharedd] spill pump: writeback failed: {error}");
        }
    }
}

/// DT-8/DT-10: spawn the pump thread. The sink receives the health after
/// every cycle (the first sample is after the first tick) and is called
/// once after a stop. The thread is detached: the caller drops the handle
/// and shutdown never joins it.
pub fn spawn<S>(config: PumpConfig, policy: PumpPolicy, mut sink: S) -> std::thread::JoinHandle<()>
where
    S: FnMut(&PumpHealth) + Send + 'static,
{
    std::thread::spawn(move || {
        let epoch = Instant::now();
        let mut runner = PumpRunner::new(config.clone(), policy);
        loop {
            let util_pct = adaptive::read_zram_utilization_pct(&config);
            let stopped = runner
                .step_with_utilization(util_pct, write_sysfs, read_bd_stat, || epoch.elapsed());
            sink(runner.health());
            if stopped {
                break;
            }
            std::thread::sleep(runner.next_interval());
        }
    })
}

pub mod adaptive;
mod two_phase;

#[cfg(test)]
mod tests;
