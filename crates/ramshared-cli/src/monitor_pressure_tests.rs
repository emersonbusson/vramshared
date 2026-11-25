// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor_pressure_tests.rs
use super::classify::{UNMANAGED_FOOTPRINT_FLOOR_KIB, classify_unmanaged_memory_usage};
use super::model::ProcessObservation;

fn process(rss_kib: u64, swap_kib: u64, managed: bool) -> ProcessObservation {
    ProcessObservation {
        rss_kib: Some(rss_kib),
        swap_kib: Some(swap_kib),
        managed,
        ..ProcessObservation::default()
    }
}

fn process_with_optional_footprint(
    rss_kib: Option<u64>,
    swap_kib: Option<u64>,
    managed: bool,
) -> ProcessObservation {
    ProcessObservation {
        rss_kib,
        swap_kib,
        managed,
        ..ProcessObservation::default()
    }
}

#[test]
fn large_external_footprint_is_reported_as_usage() {
    let footprint_kib = 1024 * 1024;
    assert_eq!(
        classify_unmanaged_memory_usage(&[process(footprint_kib, 0, false)]),
        ("UNMANAGED_MEMORY", footprint_kib, 1)
    );
}

#[test]
fn managed_memory_is_not_reported_as_external_usage() {
    assert_eq!(
        classify_unmanaged_memory_usage(&[process(1024 * 1024, 0, true)]),
        ("NONE", 0, 0)
    );
}

#[test]
fn external_footprint_below_observation_floor_is_not_flagged() {
    assert_eq!(
        classify_unmanaged_memory_usage(&[process(UNMANAGED_FOOTPRINT_FLOOR_KIB - 1, 0, false)]),
        ("NONE", 0, 0)
    );
}

#[test]
// TestName: monitor_pressure_absent_footprint_is_unclassified_not_none
fn absent_external_footprint_is_unclassified_not_measured_none() {
    // Neither counter of the footprint arrived. That is absence, not a
    // measured zero footprint, so the NONE refutation cannot stand.
    assert_eq!(
        classify_unmanaged_memory_usage(&[process_with_optional_footprint(None, None, false)]),
        ("UNCLASSIFIED", 0, 0)
    );

    // One counter present is still an absent footprint.
    assert_eq!(
        classify_unmanaged_memory_usage(&[process_with_optional_footprint(
            None,
            Some(1024),
            false
        )]),
        ("UNCLASSIFIED", 0, 0)
    );
}
