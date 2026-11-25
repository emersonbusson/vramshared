// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/stress/classify.rs
//! Pure result classification over already-collected stress evidence.
//!
//! This module decides what a finished run's collected evidence *verdict* is:
//! whether the qualification passed, and whether the unintended-fallback
//! admission condition fired. It reads nothing from the host, opens nothing for
//! writing, allocates no pressure buffers, applies no admission or pacing
//! policy and renders no terminal output. The worker in the parent module
//! folds samples into these types and publishes their answers on the report.

use super::TierCapacityStats;

/// Which qualification shape the run was executing.
///
/// The verdict rule differs by mode: a tier-3-only run has no physical cache
/// to observe, so its pass condition cannot require one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StressMode {
    FullCascade,
    Tier3Only,
}

/// The run verdict from collected evidence.
///
/// `PASS_ZERO_PANIC` requires a completed run with the mode's own simultaneous
/// tier evidence, no kernel-fault lines, a finite peak pressure below the
/// ceiling, and no safety halt. A safety stop means the full workload was not
/// completed, even if an earlier sample met the tier thresholds.
pub(super) fn stress_passes(
    kernel_faults: Option<u64>,
    mode: StressMode,
    physical_samples: usize,
    cache_lost: bool,
    safety_halt: bool,
    simultaneous_full_tiers: bool,
    peak_pressure: f64,
) -> bool {
    // A safety stop means the full workload was not completed, even if an
    // earlier sample met the tier thresholds.
    let common = kernel_faults == Some(0)
        && peak_pressure.is_finite()
        && peak_pressure < 10.0
        && !safety_halt;
    common
        && match mode {
            StressMode::Tier3Only => !cache_lost && simultaneous_full_tiers,
            StressMode::FullCascade => {
                physical_samples > 0 && !cache_lost && simultaneous_full_tiers
            }
        }
}

/// Same-point witness for the tier-3 fallback admission condition.
///
/// The benchmark contract alarms on fallback swap occupancy only while tier-1
/// and tier-2 still had free capacity **at the same observation point in the
/// same load snapshot**. Independent run-wide peaks cannot decide that: the
/// spill peak and the upper-tier peaks may come from different samples, and
/// the published percentages are computed against capacities re-read after the
/// run. So each sample is folded here at the instant all three readings exist
/// together, and the report carries the count rather than a reconstruction.
///
/// Free capacity is `used < total` for each tier's own swap-device view — the
/// same figure the published `tier1_zram_pct` / `tier2_logical_swap_pct` rows
/// already rest on. No second notion of "budget" is invented here.
///
/// The comparison is made on the **KiB** figures, never on a whole-MiB view.
/// Rounding first hides free capacity: 1536 KiB used of 2048 KiB is 2/2 MiB,
/// so a spill beside 512 KiB of free tier-1 would not have been counted at
/// all. That is a false negative on the very alarm this witness exists for.
///
/// `spill_samples` counts samples that observed fallback occupancy at all.
/// The admission condition is `spill_with_free_upper_tiers > 0`; a zero there
/// is the proof it did not fire, and `spill_samples == 0` means no sample
/// observed fallback swap in the first place. The two counts are kept apart
/// for exactly the reason the rate-window witness is: an empty sample set is
/// not a measured zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Tier3AdmissionWitness {
    pub(super) spill_samples: u64,
    pub(super) spill_with_free_upper_tiers: u64,
}

impl Tier3AdmissionWitness {
    /// Fold one co-sampled observation. `fallback_used_kib` is the fallback
    /// device's used swap; the two upper figures are each tier's own
    /// `used_kib`/`total_kib` at that same instant.
    pub(super) fn observe(
        &mut self,
        fallback_used_kib: u64,
        tier1: TierCapacityStats,
        tier2: TierCapacityStats,
    ) {
        if fallback_used_kib == 0 {
            return;
        }
        self.spill_samples += 1;
        if tier1.used_kib < tier1.total_kib && tier2.used_kib < tier2.total_kib {
            self.spill_with_free_upper_tiers += 1;
        }
    }

    /// The admission condition, in the contract's own terms.
    pub(super) fn unintended_fallback_spill(&self) -> bool {
        self.spill_with_free_upper_tiers > 0
    }
}

#[cfg(test)]
mod tests {
    use super::super::{TierCapacityStats, count_kernel_fault_lines, published_mib};
    use super::{StressMode, Tier3AdmissionWitness, stress_passes};

    /// Safety case: the admission witness exists to alarm on fallback swap
    /// that appeared while the upper tiers still had room. A whole-MiB
    /// comparison cannot see that room — 1536 KiB of 2048 KiB is "2 of 2" —
    /// so the rounded view missed exactly the spill it was built to catch.
    #[test]
    fn tier3_admission_witness_sees_free_capacity_the_mib_views_hide() {
        let almost_full = TierCapacityStats {
            total_kib: 2048,
            used_kib: 1536,
        };
        let free = TierCapacityStats {
            total_kib: 4096,
            used_kib: 0,
        };

        // The rounded view calls this tier full. It is not.
        assert_eq!(published_mib(almost_full.used_kib), 2);
        assert_eq!(published_mib(almost_full.total_kib), 2);
        assert!(almost_full.used_kib < almost_full.total_kib);

        let mut witness = Tier3AdmissionWitness::default();
        witness.observe(19 * 1024, almost_full, free);
        assert_eq!(witness.spill_samples, 1);
        assert_eq!(
            witness.spill_with_free_upper_tiers, 1,
            "512 KiB of free tier-1 is free capacity, and the spill beside it is unintended"
        );
        assert!(witness.unintended_fallback_spill());

        // Negative: the same fold against a truly full tier-1 must not alarm.
        let full = TierCapacityStats {
            total_kib: 2048,
            used_kib: 2048,
        };
        let mut witness = Tier3AdmissionWitness::default();
        witness.observe(19 * 1024, full, free);
        assert_eq!(witness.spill_with_free_upper_tiers, 0);
        assert!(!witness.unintended_fallback_spill());
    }

    /// The admission condition is a same-point fold, not a run-wide peak. Each
    /// arm is pinned here so a future change cannot reconstruct it from the
    /// peaks and start adjudicating a sample that never existed.
    #[test]
    fn tier3_admission_witness_folds_the_condition_at_one_observation_point() {
        let free = TierCapacityStats {
            total_kib: 100 * 1024,
            used_kib: 10 * 1024,
        };
        let full = TierCapacityStats {
            total_kib: 100 * 1024,
            used_kib: 100 * 1024,
        };
        let mut witness = Tier3AdmissionWitness::default();

        // No fallback occupancy: the sample is not a spill sample at all, and
        // the condition cannot have fired.
        witness.observe(0, free, free);
        assert_eq!(witness.spill_samples, 0);
        assert_eq!(witness.spill_with_free_upper_tiers, 0);
        assert!(!witness.unintended_fallback_spill());

        // Spill beside free capacity on both upper tiers: this is the alarm.
        witness.observe(19 * 1024, free, free);
        assert_eq!(witness.spill_samples, 1);
        assert_eq!(witness.spill_with_free_upper_tiers, 1);
        assert!(witness.unintended_fallback_spill());

        // Spill beside a full tier-1: the fallback is the designed last resort
        // and must not be counted as unintended, even though it is still a
        // spill sample.
        let mut witness = Tier3AdmissionWitness::default();
        witness.observe(19 * 1024, full, free);
        assert_eq!(witness.spill_samples, 1);
        assert_eq!(witness.spill_with_free_upper_tiers, 0);
        assert!(!witness.unintended_fallback_spill());

        // The same for a full tier-2 with a free tier-1: both conjuncts are
        // required, so either one full upper tier is enough to excuse the
        // spill.
        let mut witness = Tier3AdmissionWitness::default();
        witness.observe(19 * 1024, free, full);
        assert_eq!(witness.spill_samples, 1);
        assert_eq!(witness.spill_with_free_upper_tiers, 0);
        assert!(!witness.unintended_fallback_spill());

        // A full tier-3 fallback reading with both upper tiers full is still a
        // spill sample — occupancy, not growth.
        let mut witness = Tier3AdmissionWitness::default();
        witness.observe(19 * 1024, full, full);
        assert_eq!(witness.spill_samples, 1);
        assert_eq!(witness.spill_with_free_upper_tiers, 0);
    }

    /// An empty sample set is not a measured zero. `spill_samples == 0` means
    /// no complete sample observed fallback occupancy; it cannot be published
    /// as proof that the condition did not fire.
    #[test]
    fn an_empty_admission_witness_is_absence_not_a_measured_zero() {
        let witness = Tier3AdmissionWitness::default();
        assert_eq!(witness.spill_samples, 0);
        assert_eq!(witness.spill_with_free_upper_tiers, 0);
        // The predicate answers the question it was asked. It is the
        // publisher that must keep the two counts apart so a reader can tell
        // "observed spill, condition held" from "observed nothing".
        assert!(!witness.unintended_fallback_spill());
    }

    #[test]
    fn stress_verdict_refuses_nbd_faults_and_missing_kernel_evidence() {
        let clean = "nbd0: detected capacity change from 0 to 8388608";
        let fault = "block nbd0: Possible stuck request: Runtime 30 seconds\nI/O error, dev nbd0\nMCE: Killing cron due to hardware memory corruption";
        assert_eq!(count_kernel_fault_lines(clean), 0);
        assert_eq!(count_kernel_fault_lines(fault), 3);
        assert!(!stress_passes(
            None,
            StressMode::FullCascade,
            10,
            false,
            false,
            true,
            1.0
        ));
        assert!(!stress_passes(
            Some(1),
            StressMode::FullCascade,
            10,
            false,
            false,
            true,
            1.0
        ));
        assert!(!stress_passes(
            Some(0),
            StressMode::FullCascade,
            10,
            true,
            false,
            true,
            1.0
        ));
        assert!(!stress_passes(
            Some(0),
            StressMode::FullCascade,
            10,
            false,
            true,
            true,
            1.0
        ));
        assert!(!stress_passes(
            Some(0),
            StressMode::FullCascade,
            10,
            false,
            false,
            false,
            1.0
        ));
        assert!(!stress_passes(
            Some(0),
            StressMode::FullCascade,
            0,
            false,
            false,
            true,
            1.0
        ));
        assert!(stress_passes(
            Some(0),
            StressMode::FullCascade,
            10,
            false,
            false,
            true,
            1.0
        ));
        assert!(stress_passes(
            Some(0),
            StressMode::Tier3Only,
            0,
            false,
            false,
            true,
            1.0
        ));
        assert!(!stress_passes(
            Some(0),
            StressMode::Tier3Only,
            0,
            false,
            true,
            true,
            1.0
        ));
    }
}
