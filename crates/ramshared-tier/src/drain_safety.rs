// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-tier/src/drain_safety.rs
//! Shared managed-swap drain policy (SPEC swap-drain-safety RF-5).
//!
//! Disk capacity cannot supply resident RAM reserve. Callers may supply only
//! a freshly observed descriptor-owned emergency absorber; generic fallback
//! is never part of this budget. This module performs no I/O or effects.

pub const DRAIN_OBSERVATION_MAX_BYTES: usize = 64 * 1024;

/// One exact, bounded kernel counter. Duplicate or unknown units are absence.
#[must_use]
pub fn meminfo_counter_kib(text: &str, name: &str) -> Option<u64> {
    if text.len() > DRAIN_OBSERVATION_MAX_BYTES {
        return None;
    }
    let mut found = None;
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        if key != name {
            continue;
        }
        let mut fields = rest.split_whitespace();
        let value = fields.next()?.parse::<u64>().ok()?;
        if fields.next()? != "kB" || fields.next().is_some() || found.replace(value).is_some() {
            return None;
        }
    }
    found
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DrainMemory {
    pub total_bytes: u64,
    pub available_bytes: u64,
}
impl DrainMemory {
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let total_bytes = meminfo_counter_kib(text, "MemTotal")?.checked_mul(1024)?;
        let available_bytes = meminfo_counter_kib(text, "MemAvailable")?.checked_mul(1024)?;
        (total_bytes > 0 && available_bytes <= total_bytes).then_some(Self {
            total_bytes,
            available_bytes,
        })
    }
}

/// Optional `MemFree` counter in bytes for WSL unbacked-frame estimation.
#[must_use]
pub fn parse_free_bytes(text: &str) -> Option<u64> {
    meminfo_counter_kib(text, "MemFree")?.checked_mul(1024)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostDrainMemory {
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub commit_total_bytes: u64,
    pub commit_limit_bytes: u64,
}

pub const MIN_DRAIN_RESERVE_BYTES: u64 = 1024 * 1024 * 1024;
pub const DRAIN_MARGIN_BYTES: u64 = 512 * 1024 * 1024;

/// Reserve for Windows processes outside the observed guest allocation.
pub fn host_drain_reserve_bytes(total_host_bytes: u64, total_guest_bytes: u64) -> u64 {
    total_host_bytes
        .saturating_sub(total_guest_bytes)
        .div_ceil(10)
        .max(MIN_DRAIN_RESERVE_BYTES)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DrainBudget {
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub managed_bytes: u64,
    /// Fresh capacity of the exact descriptor-owned absorber, never fallback.
    pub emergency_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DrainDecision {
    pub reserve_bytes: u64,
    pub margin_bytes: u64,
    pub required_bytes: u64,
    pub deficit_bytes: u64,
    pub reason: &'static str,
}

impl DrainDecision {
    #[must_use]
    pub fn is_safe(self) -> bool {
        matches!(self.reason, "safe" | "no_managed_swap")
    }

    #[must_use]
    pub fn absorber_can_help(self) -> bool {
        self.reason == "managed_swap_exceeds_headroom"
    }
}

impl DrainBudget {
    /// Smallest prospective absorber that passes the exact platform decision.
    /// `None` means an absorber cannot satisfy the independent reserves, or
    /// the authority is unknown. Callers still must prove actual live credit.
    #[must_use]
    pub fn minimum_emergency_bytes(
        self,
        evaluate: impl Fn(Self) -> Option<DrainDecision>,
    ) -> Option<u64> {
        let accepts = |emergency_bytes| {
            evaluate(Self {
                emergency_bytes,
                ..self
            })
            .is_some_and(DrainDecision::is_safe)
        };
        if accepts(0) {
            return Some(0);
        }
        if !accepts(self.managed_bytes) {
            return None;
        }
        let mut first = 1;
        let mut last = self.managed_bytes;
        while first < last {
            let middle = first + (last - first) / 2;
            if accepts(middle) {
                last = middle;
            } else {
                first = middle + 1;
            }
        }
        Some(first)
    }

    /// Invalid or overflowed inputs are absence, never a saturated SAFE frame.
    #[must_use]
    pub fn evaluate(self) -> Option<DrainDecision> {
        if self.total_bytes == 0 || self.available_bytes > self.total_bytes {
            return None;
        }
        let reserve_bytes = self.total_bytes.div_ceil(10).max(MIN_DRAIN_RESERVE_BYTES);
        let floor = reserve_bytes.checked_add(DRAIN_MARGIN_BYTES)?;
        if self.managed_bytes == 0 {
            return Some(DrainDecision {
                reserve_bytes,
                margin_bytes: DRAIN_MARGIN_BYTES,
                required_bytes: 0,
                deficit_bytes: 0,
                reason: "no_managed_swap",
            });
        }
        let required_bytes = self.managed_bytes.checked_add(floor)?;
        let absorbable = self.available_bytes.checked_add(self.emergency_bytes)?;
        let deficit_bytes = required_bytes.saturating_sub(absorbable);
        let reason = if self.available_bytes < floor {
            "mem_available_below_reserve"
        } else if deficit_bytes != 0 {
            "managed_swap_exceeds_headroom"
        } else {
            "safe"
        };
        Some(DrainDecision {
            reserve_bytes,
            margin_bytes: DRAIN_MARGIN_BYTES,
            required_bytes,
            deficit_bytes: deficit_bytes.max(floor.saturating_sub(self.available_bytes)),
            reason,
        })
    }

    /// Cross-layer WSL drain evaluation without double-counting resident guest cache.
    #[must_use]
    pub fn evaluate_wsl(
        self,
        guest_free_bytes: Option<u64>,
        host: Option<HostDrainMemory>,
    ) -> Option<DrainDecision> {
        let guest = self.evaluate()?;
        if guest_free_bytes.is_some_and(|free| free > self.total_bytes) {
            return None;
        }
        if !guest.is_safe() {
            return Some(guest);
        }
        let Some(host) = host else {
            return Some(DrainDecision {
                reason: "host_memory_unverified",
                ..guest
            });
        };
        if host.total_bytes == 0
            || host.available_bytes > host.total_bytes
            || host.commit_limit_bytes == 0
            || host.commit_total_bytes > host.commit_limit_bytes
        {
            return None;
        }
        let host_reserve = host_drain_reserve_bytes(host.total_bytes, self.total_bytes);
        let host_floor = host_reserve.checked_add(DRAIN_MARGIN_BYTES)?;
        let commit_free = host
            .commit_limit_bytes
            .checked_sub(host.commit_total_bytes)?;
        if host.available_bytes < host_floor {
            return Some(DrainDecision {
                reserve_bytes: host_reserve,
                margin_bytes: DRAIN_MARGIN_BYTES,
                required_bytes: host_floor,
                deficit_bytes: host_floor - host.available_bytes,
                reason: "host_mem_available_below_reserve",
            });
        }
        if commit_free < host_floor {
            return Some(DrainDecision {
                reserve_bytes: host_reserve,
                margin_bytes: DRAIN_MARGIN_BYTES,
                required_bytes: host_floor,
                deficit_bytes: host_floor - commit_free,
                reason: "host_commit_below_reserve",
            });
        }
        if self.managed_bytes == 0 {
            return Some(guest);
        }
        let net_managed = self.managed_bytes.saturating_sub(self.emergency_bytes);
        let new_host_demand = guest_free_bytes.map_or(net_managed, |free| net_managed.min(free));
        let host_required = new_host_demand.checked_add(host_floor)?;
        if host.available_bytes < host_required {
            return Some(DrainDecision {
                reserve_bytes: host_reserve,
                margin_bytes: DRAIN_MARGIN_BYTES,
                required_bytes: host_required,
                deficit_bytes: host_required - host.available_bytes,
                reason: "host_memory_exceeds_headroom",
            });
        }
        if commit_free < host_required {
            return Some(DrainDecision {
                reserve_bytes: host_reserve,
                margin_bytes: DRAIN_MARGIN_BYTES,
                required_bytes: host_required,
                deficit_bytes: host_required - commit_free,
                reason: "host_commit_exceeds_headroom",
            });
        }
        Some(guest)
    }
}

#[cfg(test)]
mod budget_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn drain_memory_has_one_strict_counter_contract() {
        assert_eq!(
            DrainMemory::parse("MemTotal: 16 kB\nMemAvailable: 8 kB\n"),
            Some(DrainMemory {
                total_bytes: 16384,
                available_bytes: 8192
            })
        );
        for text in [
            "",
            "MemTotal: 0 kB\nMemAvailable: 0 kB\n",
            "MemTotal: 16 kB\nMemAvailable: 17 kB\n",
            "MemTotal: 16 MB\nMemAvailable: 8 kB\n",
            "MemTotal: 16 kB\nMemAvailable: 8 kB extra\n",
            "MemTotal: 16 kB\nMemAvailable: 8 kB\nMemAvailable: 1 kB\n",
            "MemTotal: 18446744073709551615 kB\nMemAvailable: 0 kB\n",
        ] {
            assert!(DrainMemory::parse(text).is_none(), "{text:?}");
        }
        assert!(
            meminfo_counter_kib(&"x".repeat(DRAIN_OBSERVATION_MAX_BYTES + 1), "MemTotal").is_none()
        );
    }

    #[test]
    fn drain_budget_uses_the_minimum_reserve_on_small_hosts() {
        let frame = DrainBudget {
            total_bytes: 2 * GIB,
            available_bytes: 2 * GIB,
            managed_bytes: GIB / 4,
            emergency_bytes: 0,
        };
        let decision = frame.evaluate().unwrap();
        assert_eq!(decision.reserve_bytes, GIB);
        assert!(decision.is_safe());
    }

    fn budget(available: u64, managed: u64, emergency: u64) -> DrainBudget {
        DrainBudget {
            total_bytes: 16 * GIB,
            available_bytes: available,
            managed_bytes: managed,
            emergency_bytes: emergency,
        }
    }

    #[test]
    fn absorber_capacity_covers_guest_and_host_shortfalls_without_replacing_reserves() {
        let host = HostDrainMemory {
            total_bytes: 32 * GIB,
            available_bytes: 3 * GIB - GIB / 4,
            commit_total_bytes: 30 * GIB,
            commit_limit_bytes: 34 * GIB,
        };
        let frame = budget(3 * GIB, 4 * GIB, 0);
        let evaluate = |candidate: DrainBudget| candidate.evaluate_wsl(Some(2 * GIB), Some(host));
        let required = frame.minimum_emergency_bytes(evaluate).unwrap();
        assert!(required > frame.evaluate().unwrap().deficit_bytes);
        let with_capacity = |emergency_bytes| DrainBudget {
            emergency_bytes,
            ..frame
        };
        assert!(evaluate(with_capacity(required)).unwrap().is_safe());
        assert!(!evaluate(with_capacity(required - 1)).unwrap().is_safe());
        assert_eq!(
            frame.minimum_emergency_bytes(|b| b.evaluate_wsl(None, None)),
            None
        );
        assert_eq!(
            budget(GIB, 4 * GIB, 0).minimum_emergency_bytes(evaluate),
            None
        );
        assert_eq!(
            frame.minimum_emergency_bytes(|b| b.evaluate_wsl(
                None,
                Some(HostDrainMemory {
                    available_bytes: GIB,
                    ..host
                })
            )),
            None
        );
        assert_eq!(
            budget(4 * GIB, 0, 0).minimum_emergency_bytes(DrainBudget::evaluate),
            Some(0)
        );
    }

    #[test]
    fn drain_budget_has_an_exact_emergency_boundary() {
        let blocked = budget(3 * GIB, 2 * GIB, 0).evaluate().unwrap();
        assert!(!blocked.is_safe());
        assert!(blocked.absorber_can_help());
        let exact = budget(3 * GIB, 2 * GIB, blocked.deficit_bytes)
            .evaluate()
            .unwrap();
        assert!(exact.is_safe());
        assert!(
            !budget(3 * GIB, 2 * GIB, blocked.deficit_bytes - 1)
                .evaluate()
                .unwrap()
                .is_safe()
        );
        assert_eq!(exact.reserve_bytes, (16 * GIB).div_ceil(10));
        assert_eq!(exact.margin_bytes, GIB / 2);
    }

    #[test]
    fn drain_budget_keeps_the_resident_reserve_independent_of_disk() {
        let floor = (16 * GIB).div_ceil(10) + GIB / 2;
        let blocked = budget(floor - 1, GIB, 4 * GIB).evaluate().unwrap();
        assert!(!blocked.is_safe());
        assert!(!blocked.absorber_can_help());
        assert_eq!(blocked.reason, "mem_available_below_reserve");
        assert!(budget(floor, GIB, GIB).evaluate().unwrap().is_safe());
    }

    #[test]
    fn drain_budget_refuses_unknown_inconsistent_and_overflowed_quantities() {
        for frame in [
            DrainBudget {
                total_bytes: 0,
                ..budget(0, 1, 0)
            },
            budget(17 * GIB, 1, 0),
            budget(3 * GIB, u64::MAX, 0),
            budget(3 * GIB, GIB, u64::MAX),
        ] {
            assert!(frame.evaluate().is_none(), "{frame:?}");
        }
        assert!(budget(0, 0, 0).evaluate().unwrap().is_safe());
        assert!(budget(5 * GIB, GIB, 0).evaluate().unwrap().is_safe());
    }

    #[test]
    fn wsl_cross_layer_drain_credits_reclaimable_guest_cache_without_double_counting() {
        assert_eq!(
            parse_free_bytes("MemTotal: 16 kB\nMemFree: 4 kB\nMemAvailable: 8 kB\n"),
            Some(4096)
        );
        let frame = budget(8 * GIB, 2 * GIB, 0);
        let unverified = frame.evaluate_wsl(Some(GIB), None).unwrap();
        assert!(!unverified.is_safe());
        assert_eq!(unverified.reason, "host_memory_unverified");

        let host = HostDrainMemory {
            total_bytes: 32 * GIB,
            available_bytes: 3 * GIB + 512 * 1024 * 1024,
            commit_total_bytes: 40 * GIB,
            commit_limit_bytes: 56 * GIB,
        };
        assert!(frame.evaluate_wsl(Some(GIB), Some(host)).unwrap().is_safe());

        let unbacked = frame.evaluate_wsl(Some(8 * GIB), Some(host)).unwrap();
        assert!(!unbacked.is_safe());
        assert_eq!(unbacked.reason, "host_memory_exceeds_headroom");

        let low_phys = HostDrainMemory {
            available_bytes: 2 * GIB,
            ..host
        };
        assert_eq!(
            frame.evaluate_wsl(Some(0), Some(low_phys)).unwrap().reason,
            "host_mem_available_below_reserve"
        );

        let low_commit_floor = HostDrainMemory {
            commit_total_bytes: 55 * GIB,
            ..host
        };
        assert_eq!(
            frame
                .evaluate_wsl(Some(0), Some(low_commit_floor))
                .unwrap()
                .reason,
            "host_commit_below_reserve"
        );

        let tight_commit = HostDrainMemory {
            commit_total_bytes: 53 * GIB + 256 * 1024 * 1024,
            ..host
        };
        assert_eq!(
            frame
                .evaluate_wsl(Some(GIB), Some(tight_commit))
                .unwrap()
                .reason,
            "host_commit_exceeds_headroom"
        );

        assert_eq!(
            budget(8 * GIB, 0, 0)
                .evaluate_wsl(Some(GIB), Some(host))
                .unwrap()
                .reason,
            "no_managed_swap"
        );
        assert!(frame.evaluate_wsl(Some(17 * GIB), Some(host)).is_none());
        assert!(
            frame
                .evaluate_wsl(
                    None,
                    Some(HostDrainMemory {
                        total_bytes: 0,
                        ..host
                    })
                )
                .is_none()
        );
    }
}
