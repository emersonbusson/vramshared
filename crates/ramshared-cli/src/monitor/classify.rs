// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/classify.rs
//! Pure classification over snapshot model values.
//!
//! This module decides what an already-collected observation *is*: which
//! memory scope a guest reports, whether a sample is stale, and which process
//! footprints count as unmanaged memory. It reads nothing from the host, opens
//! nothing for writing, applies no lifecycle or pressure policy and renders no
//! terminal output. The composer in the parent module calls it; the
//! presentation band renders its answers.

use super::model::{MemoryScope, Observation, ProcessObservation};

/// Smallest process footprint, in KiB, that `classify_unmanaged_memory_usage`
/// reports as unmanaged memory.
///
/// Below this a process is routine background noise and is not worth a panel
/// row. The floor is a product display threshold, not a host tuning value: it
/// is named here so the classifier and its tests agree on one number instead of
/// each carrying a bare literal.
pub(super) const UNMANAGED_FOOTPRINT_FLOOR_KIB: u64 = 512 * 1024;

/// Which guest the memory numbers describe, from the kernel release string.
///
/// WSL2 is recognised by its release tag; WSL1 by the Microsoft tag without
/// the WSL2 marker, or by a live `WSLInterop` binfmt. Anything else is a
/// native Linux host. The answer is a [`MemoryScope`] display contract, so the
/// caller never invents a panel title.
pub(super) fn detect_memory_scope(osrelease: &str, wsl_interop_available: bool) -> MemoryScope {
    let normalized = osrelease.to_ascii_lowercase();
    if normalized.contains("microsoft-standard-wsl2")
        || (normalized.contains("microsoft") && normalized.contains("wsl2"))
    {
        MemoryScope::Wsl2
    } else if (normalized.contains("microsoft") && normalized.contains("wsl"))
        || wsl_interop_available
    {
        MemoryScope::Wsl
    } else {
        MemoryScope::LinuxHost
    }
}

/// True when the sample carries the `sample_refresh_failed` marker, which means
/// the numbers on screen are older than one collection interval.
///
/// The marker is written by [`super::mark_observation_refresh_failed`]. This
/// is the read side of that same contract: one string, two call sites, no
/// second spelling of "stale".
pub(super) fn observation_sample_is_stale(observation: &Observation) -> bool {
    observation
        .errors
        .iter()
        .any(|error| error == "sample_refresh_failed")
}

/// Fold the visible process list into the unmanaged-memory panel row.
///
/// Returns the row label, the summed footprint in KiB and the process count.
/// Only unmanaged processes at or above [`UNMANAGED_FOOTPRINT_FLOOR_KIB`] of
/// resident plus swapped memory are counted; managed ones are already owned by
/// the product and are not reported as external usage.
///
/// Three arms, and a footprint that never arrived is absence, not a distinct
/// cause:
/// - `UNMANAGED_MEMORY` — at least one unmanaged process has a measured
///   footprint at or above the floor. Positive evidence wins even when another
///   process's footprint is absent.
/// - `NONE` — every unmanaged process has a measured footprint and none of them
///   is at or above the floor. A managed process's absent footprint cannot
///   refute this arm: management, not size, already excludes it.
/// - `UNCLASSIFIED` — no positive finding and at least one unmanaged process's
///   footprint is absent, so the refutation cannot be measured either.
pub(super) fn classify_unmanaged_memory_usage(
    processes: &[ProcessObservation],
) -> (&'static str, u64, u64) {
    let mut count = 0u64;
    let mut kib = 0u64;
    let mut any_absent = false;
    for process in processes {
        if process.managed {
            continue;
        }
        let Some(footprint) = process.footprint_kib() else {
            any_absent = true;
            continue;
        };
        if footprint >= UNMANAGED_FOOTPRINT_FLOOR_KIB {
            count = count.saturating_add(1);
            kib = kib.saturating_add(footprint);
        }
    }
    if count > 0 {
        ("UNMANAGED_MEMORY", kib, count)
    } else if any_absent {
        ("UNCLASSIFIED", 0, 0)
    } else {
        ("NONE", 0, 0)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::collections::BTreeMap;

    use super::super::model::{
        CgroupMemoryObservation, ControlPlaneObservation, HyperVBalloonObservation,
        MemoryObservation, ProcessMemoryTotals, TierDeviceNames,
    };
    use super::*;

    /// A sample is an aggregate of sub-observations, so it has no `Default`.
    /// The classifier only reads `errors`; the rest is inert fixture shape.
    fn sample(errors: &[&str]) -> Observation {
        Observation {
            status: BTreeMap::new(),
            epoch_ms: 0,
            sample_age_ms: 0,
            memory_scope: MemoryScope::LinuxHost,
            mem: MemoryObservation::default(),
            cgroup_memory: CgroupMemoryObservation::default(),
            hyperv_balloon: HyperVBalloonObservation::default(),
            process_totals: ProcessMemoryTotals::default(),
            control_plane: ControlPlaneObservation::default(),
            gpu: None,
            tier_devices: TierDeviceNames::default(),
            operation_progress: None,
            top_processes: Vec::new(),
            errors: errors.iter().map(|error| (*error).to_string()).collect(),
        }
    }

    #[test]
    fn memory_scope_distinguishes_wsl2_wsl1_and_native_linux() {
        let wsl2 = detect_memory_scope("6.18.40.1-microsoft-standard-WSL2+", false);
        assert_eq!(wsl2, MemoryScope::Wsl2);
        assert_eq!(wsl2.panel_title(), "WSL2 Guest RAM & Swap");
        assert_eq!(wsl2.ram_label(), "WSL2 Guest RAM");

        let wsl = detect_memory_scope("4.4.0-Microsoft", true);
        assert_eq!(wsl, MemoryScope::Wsl);
        assert_eq!(wsl.panel_title(), "WSL Guest RAM & Swap");

        let linux = detect_memory_scope("6.12.0-generic", false);
        assert_eq!(linux, MemoryScope::LinuxHost);
        assert_eq!(linux.panel_title(), "Host RAM & Swap");
        assert_eq!(linux.ram_label(), "Host RAM");
    }

    #[test]
    fn observation_sample_is_stale_reads_only_the_refresh_marker() {
        // Positive: the one marker the refresh path writes.
        assert!(observation_sample_is_stale(&sample(&[
            "sample_refresh_failed"
        ])));
        assert!(observation_sample_is_stale(&sample(&[
            "memory_psi_unavailable",
            "sample_refresh_failed",
        ])));

        // Negative: a healthy sample, and one carrying a different error.
        assert!(!observation_sample_is_stale(&sample(&[])));
        assert!(!observation_sample_is_stale(&sample(&[
            "memory_psi_unavailable"
        ])));
    }

    #[test]
    fn unmanaged_footprint_floor_is_one_named_threshold() {
        // The floor the classifier uses is the constant its tests cite, not a
        // second literal that can drift from it.
        let at_floor = ProcessObservation {
            rss_kib: Some(UNMANAGED_FOOTPRINT_FLOOR_KIB),
            swap_kib: Some(0),
            managed: false,
            ..ProcessObservation::default()
        };
        assert_eq!(
            classify_unmanaged_memory_usage(&[at_floor]),
            ("UNMANAGED_MEMORY", UNMANAGED_FOOTPRINT_FLOOR_KIB, 1)
        );

        let below_floor = ProcessObservation {
            rss_kib: Some(UNMANAGED_FOOTPRINT_FLOOR_KIB - 1),
            swap_kib: Some(0),
            managed: false,
            ..ProcessObservation::default()
        };
        assert_eq!(
            classify_unmanaged_memory_usage(&[below_floor]),
            ("NONE", 0, 0),
            "a footprint under the observation floor is not flagged"
        );
    }

    #[test]
    // TestName: monitor_classify_never_publishes_none_from_an_absent_footprint
    fn absent_unmanaged_footprint_never_publishes_a_measured_none() {
        // Absence is not a measured refutation. An unmanaged process whose
        // footprint never arrived blocks the NONE arm; it is not a cause arm
        // of its own and it is never folded into a zero footprint.
        let absent = ProcessObservation {
            rss_kib: None,
            swap_kib: Some(1024),
            managed: false,
            ..ProcessObservation::default()
        };
        assert_eq!(
            classify_unmanaged_memory_usage(&[absent]),
            ("UNCLASSIFIED", 0, 0)
        );
    }

    #[test]
    // TestName: monitor_classify_positive_evidence_outranks_absent_footprint
    fn positive_unmanaged_finding_outranks_an_absent_sibling_footprint() {
        // A measured footprint at or above the floor is the positive claim;
        // one sibling whose footprint never arrived cannot cancel it.
        let large = ProcessObservation {
            rss_kib: Some(UNMANAGED_FOOTPRINT_FLOOR_KIB),
            swap_kib: Some(0),
            managed: false,
            ..ProcessObservation::default()
        };
        let absent = ProcessObservation {
            rss_kib: None,
            swap_kib: None,
            managed: false,
            ..ProcessObservation::default()
        };
        assert_eq!(
            classify_unmanaged_memory_usage(&[absent, large]),
            ("UNMANAGED_MEMORY", UNMANAGED_FOOTPRINT_FLOOR_KIB, 1)
        );
    }

    #[test]
    // TestName: monitor_classify_managed_absence_does_not_block_none
    fn managed_absent_footprint_does_not_block_the_none_refutation() {
        // Management already excludes the process from external usage, so its
        // absent footprint cannot un-measure the remaining refutation.
        let managed_absent = ProcessObservation {
            rss_kib: None,
            swap_kib: None,
            managed: true,
            ..ProcessObservation::default()
        };
        let below_floor = ProcessObservation {
            rss_kib: Some(UNMANAGED_FOOTPRINT_FLOOR_KIB - 1),
            swap_kib: Some(0),
            managed: false,
            ..ProcessObservation::default()
        };
        assert_eq!(
            classify_unmanaged_memory_usage(&[managed_absent, below_floor]),
            ("NONE", 0, 0)
        );
    }
}
