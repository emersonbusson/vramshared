// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/spill_pump/two_phase.rs
//! Bounded two-phase zram idle aging and incompressible (`huge`) writeback
//! fallback for Linux kernels built without `CONFIG_ZRAM_TRACK_ENTRY_ACTIME`.
//!
//! Safety invariant: writing `idle = all` marks every stored page idle, so the
//! pump **never** triggers `writeback = idle` on the same tick. It waits a full
//! `idle_seconds` window during which kernel reads/writes automatically clear
//! `ZRAM_IDLE` on hot working-set pages, then drains only the genuinely cold
//! pages that remained untouched, bounded per tick by `writeback_limit_enable=1`
//! and `writeback_limit=budget_pages`.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{
    PumpConfig, PumpCycleSample, PumpPolicy, TickOutcome, WritebackOutcome, classify_writeback,
    measure_cycle,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TwoPhaseState {
    Inactive,
    MarkNext,
    Aging {
        marked_at: Duration,
        huge_drained: bool,
    },
    DrainingIdle,
}

#[derive(Clone, Debug)]
pub(super) struct TwoPhasePump {
    state: TwoPhaseState,
}

pub(super) type WriteFn<'a> = &'a mut dyn FnMut(&Path, &str) -> io::Result<String>;
pub(super) type ReadCounterFn<'a> = &'a dyn Fn(&Path) -> Option<u64>;
pub(super) type NowFn<'a> = &'a mut dyn FnMut() -> Duration;

impl TwoPhasePump {
    pub(super) fn inactive() -> Self {
        Self {
            state: TwoPhaseState::Inactive,
        }
    }

    pub(super) fn is_active(&self) -> bool {
        self.state != TwoPhaseState::Inactive
    }

    pub(super) fn activate(&mut self) {
        if self.state == TwoPhaseState::Inactive {
            self.state = TwoPhaseState::MarkNext;
        }
    }

    #[cfg(test)]
    pub(super) fn state(&self) -> TwoPhaseState {
        self.state
    }

    pub(super) fn step(
        &mut self,
        config: &PumpConfig,
        policy: &PumpPolicy,
        write: WriteFn<'_>,
        read_counter: ReadCounterFn<'_>,
        now: NowFn<'_>,
    ) -> (TickOutcome, Option<PumpCycleSample>) {
        let Some(zram_device) = config.target() else {
            // If the zram target vanished or was rebound, start a fresh aging
            // epoch once a backing-bound zram appears again.
            self.state = TwoPhaseState::MarkNext;
            return (
                TickOutcome {
                    writeback: WritebackOutcome::NoTarget,
                    written: 0,
                    idle_fallback: true,
                },
                None,
            );
        };

        match self.state {
            TwoPhaseState::Inactive => (
                TickOutcome {
                    writeback: WritebackOutcome::ProactiveUnavailable,
                    written: 0,
                    idle_fallback: true,
                },
                None,
            ),
            TwoPhaseState::MarkNext => {
                self.mark_and_drain_huge(&zram_device, config, policy, write, read_counter, now)
            }
            TwoPhaseState::Aging {
                marked_at,
                huge_drained,
            } => {
                let elapsed = now().saturating_sub(marked_at);
                if elapsed >= Duration::from_secs(config.idle_seconds) {
                    self.state = TwoPhaseState::DrainingIdle;
                    self.drain_idle(&zram_device, config, policy, write, read_counter, now)
                } else if !huge_drained {
                    let (outcome, sample, done) =
                        run_huge_cycle(&zram_device, config, policy, 0, write, read_counter, now);
                    if done {
                        self.state = TwoPhaseState::Aging {
                            marked_at,
                            huge_drained: true,
                        };
                    }
                    (outcome, sample)
                } else {
                    (
                        TickOutcome {
                            writeback: WritebackOutcome::Completed,
                            written: 0,
                            idle_fallback: true,
                        },
                        None,
                    )
                }
            }
            TwoPhaseState::DrainingIdle => {
                self.drain_idle(&zram_device, config, policy, write, read_counter, now)
            }
        }
    }

    fn mark_and_drain_huge(
        &mut self,
        zram_device: &str,
        config: &PumpConfig,
        policy: &PumpPolicy,
        write: WriteFn<'_>,
        read_counter: ReadCounterFn<'_>,
        now: NowFn<'_>,
    ) -> (TickOutcome, Option<PumpCycleSample>) {
        if let Err(error) = write(&sysfs_path(zram_device, "idle"), "all") {
            return (
                TickOutcome {
                    writeback: WritebackOutcome::Failed(format!("idle: {error}")),
                    written: 0,
                    idle_fallback: true,
                },
                None,
            );
        }
        let marked_at = now();
        self.state = TwoPhaseState::Aging {
            marked_at,
            huge_drained: false,
        };
        let (outcome, sample, done) =
            run_huge_cycle(zram_device, config, policy, 1, write, read_counter, now);
        if done {
            self.state = TwoPhaseState::Aging {
                marked_at,
                huge_drained: true,
            };
        }
        (outcome, sample)
    }

    fn drain_idle(
        &mut self,
        zram_device: &str,
        config: &PumpConfig,
        policy: &PumpPolicy,
        write: WriteFn<'_>,
        read_counter: ReadCounterFn<'_>,
        now: NowFn<'_>,
    ) -> (TickOutcome, Option<PumpCycleSample>) {
        let written = match arm_bounded_budget(zram_device, config.budget_pages, write) {
            Ok(count) => count,
            Err(error) => {
                return (
                    TickOutcome {
                        writeback: WritebackOutcome::Failed(error),
                        written: 0,
                        idle_fallback: true,
                    },
                    None,
                );
            }
        };
        let (outcome, sample) = trigger_writeback(
            zram_device,
            "idle",
            config.budget_pages,
            policy,
            written,
            write,
            read_counter,
            now,
        );
        let wrote_zero = sample.as_ref().is_some_and(|s| s.wrote_kib == Some(0));
        match &outcome.writeback {
            WritebackOutcome::BudgetExhausted if !wrote_zero => {
                // More aged cold pages remain; keep draining on subsequent ticks
                // without re-marking hot pages.
                self.state = TwoPhaseState::DrainingIdle;
            }
            WritebackOutcome::Completed | WritebackOutcome::BudgetExhausted => {
                self.state = TwoPhaseState::MarkNext;
            }
            WritebackOutcome::Failed(_)
            | WritebackOutcome::NoTarget
            | WritebackOutcome::ProactiveUnavailable => {}
        }
        (outcome, sample)
    }
}

fn sysfs_path(zram_device: &str, attribute: &str) -> PathBuf {
    let name = zram_device.rsplit('/').next().unwrap_or(zram_device);
    Path::new("/sys/block").join(name).join(attribute)
}

pub(super) fn enable_writeback_limit(
    zram_device: &str,
    write: WriteFn<'_>,
) -> Result<usize, String> {
    match write(&sysfs_path(zram_device, "writeback_limit_enable"), "1") {
        Ok(_) => Ok(1),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(format!("writeback_limit_enable: {error}")),
    }
}

fn arm_bounded_budget(
    zram_device: &str,
    budget_pages: u64,
    write: WriteFn<'_>,
) -> Result<usize, String> {
    let enabled = enable_writeback_limit(zram_device, write)?;
    let budget = budget_pages.to_string();
    write(&sysfs_path(zram_device, "writeback_limit"), &budget)
        .map_err(|error| format!("writeback_limit: {error}"))?;
    Ok(enabled + 1)
}

#[allow(clippy::too_many_arguments)]
fn trigger_writeback(
    zram_device: &str,
    mode: &str,
    budget_pages: u64,
    policy: &PumpPolicy,
    prior_written: usize,
    write: WriteFn<'_>,
    read_counter: ReadCounterFn<'_>,
    now: NowFn<'_>,
) -> (TickOutcome, Option<PumpCycleSample>) {
    let counter_path = sysfs_path(zram_device, "bd_stat");
    let before = read_counter(&counter_path);
    let started = now();
    let result = write(&sysfs_path(zram_device, "writeback"), mode);
    let elapsed = now().saturating_sub(started);
    let after = read_counter(&counter_path);
    let outcome = classify_writeback(&result);
    let sample = measure_cycle(before, after, elapsed, budget_pages, policy, &outcome);
    (
        TickOutcome {
            writeback: outcome,
            written: prior_written + 1,
            idle_fallback: true,
        },
        sample,
    )
}

fn is_unsupported_mode_error(text: &str) -> bool {
    text.contains("Invalid argument") || text.contains("EINVAL")
}

fn run_huge_cycle(
    zram_device: &str,
    config: &PumpConfig,
    policy: &PumpPolicy,
    prior_written: usize,
    write: WriteFn<'_>,
    read_counter: ReadCounterFn<'_>,
    now: NowFn<'_>,
) -> (TickOutcome, Option<PumpCycleSample>, bool) {
    let budget_written = match arm_bounded_budget(zram_device, config.budget_pages, write) {
        Ok(count) => count,
        Err(error) => {
            return (
                TickOutcome {
                    writeback: WritebackOutcome::Failed(error),
                    written: prior_written,
                    idle_fallback: true,
                },
                None,
                false,
            );
        }
    };
    let (outcome, sample) = trigger_writeback(
        zram_device,
        "huge",
        config.budget_pages,
        policy,
        prior_written + budget_written,
        write,
        read_counter,
        now,
    );
    if let WritebackOutcome::Failed(ref msg) = outcome.writeback
        && is_unsupported_mode_error(msg)
    {
        return (
            TickOutcome {
                writeback: WritebackOutcome::Completed,
                written: outcome.written,
                idle_fallback: true,
            },
            None,
            true,
        );
    }
    let wrote_zero = sample.as_ref().is_some_and(|s| s.wrote_kib == Some(0));
    let done = match &outcome.writeback {
        WritebackOutcome::Completed => true,
        WritebackOutcome::BudgetExhausted => wrote_zero,
        _ => false,
    };
    (outcome, sample, done)
}

#[cfg(test)]
mod tests;
