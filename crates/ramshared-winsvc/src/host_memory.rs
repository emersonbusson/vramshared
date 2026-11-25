// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/host_memory.rs
//! Native Windows host-memory sample for the authenticated heartbeat.
//!
//! This is an observation only: it never allocates, changes admission, or
//! substitutes for guest PSI. The host has no PSI; these figures are physical
//! and commit headroom, labelled as such by the dashboard.

use ramshared_ipc::HostMemoryTelemetry;

/// Convert page counts from `GetPerformanceInfo` (or a test fixture) to the
/// wire sample. A zero page size is a malformed sample, not a zero host.
pub fn sample_from_pages(
    sampled_at_unix_ms: u64,
    page_size_bytes: u64,
    physical_total_pages: u64,
    physical_available_pages: u64,
    commit_total_pages: u64,
    commit_limit_pages: u64,
) -> Option<HostMemoryTelemetry> {
    if page_size_bytes == 0 || commit_total_pages > commit_limit_pages {
        return None;
    }
    Some(HostMemoryTelemetry {
        sampled_at_unix_ms,
        total_bytes: physical_total_pages.saturating_mul(page_size_bytes),
        available_bytes: physical_available_pages.saturating_mul(page_size_bytes),
        commit_total_bytes: commit_total_pages.saturating_mul(page_size_bytes),
        commit_limit_bytes: commit_limit_pages.saturating_mul(page_size_bytes),
        page_size_bytes,
        vmmem_wsl_working_set_bytes: None,
    })
}

#[cfg(windows)]
pub fn sample() -> Option<HostMemoryTelemetry> {
    use std::mem::size_of;
    use std::time::{SystemTime, UNIX_EPOCH};
    use windows_sys::Win32::System::ProcessStatus::{GetPerformanceInfo, PERFORMANCE_INFORMATION};

    let mut info = PERFORMANCE_INFORMATION::default();
    let ok = unsafe {
        GetPerformanceInfo(
            &mut info,
            u32::try_from(size_of::<PERFORMANCE_INFORMATION>()).unwrap_or(u32::MAX),
        )
    };
    if ok == 0 {
        return None;
    }
    let sampled_at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())?;
    let page_size = u64::try_from(info.PageSize).ok()?;
    let total = u64::try_from(info.PhysicalTotal).ok()?;
    let available = u64::try_from(info.PhysicalAvailable).ok()?;
    let commit_total = u64::try_from(info.CommitTotal).ok()?;
    let commit_limit = u64::try_from(info.CommitLimit).ok()?;
    sample_from_pages(
        sampled_at_unix_ms,
        page_size,
        total,
        available,
        commit_total,
        commit_limit,
    )
}

#[cfg(not(windows))]
pub fn sample() -> Option<HostMemoryTelemetry> {
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;

    #[test]
    fn pages_convert_exactly_and_refuse_malformed_input() {
        let sample = sample_from_pages(1, 4096, 4_194_304, 1_048_576, 2_097_152, 3_145_728)
            .expect("valid sample");
        assert_eq!(sample.total_bytes, 16 * 1024 * 1024 * 1024);
        assert_eq!(sample.available_bytes, 4 * 1024 * 1024 * 1024);
        assert_eq!(sample.commit_limit_bytes, 12 * 1024 * 1024 * 1024);
        assert_eq!(sample.page_size_bytes, 4096);
        assert!(sample_from_pages(1, 0, 1, 1, 1, 1).is_none());
        assert!(sample_from_pages(1, 4096, 1, 1, 2, 1).is_none());
    }

    #[cfg(not(windows))]
    #[test]
    fn native_sampler_is_absent_off_windows() {
        assert!(sample().is_none());
    }
}
