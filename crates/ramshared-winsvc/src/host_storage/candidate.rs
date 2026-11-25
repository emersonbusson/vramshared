// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/host_storage/candidate.rs
//! Conversion of supervised observations into request-bound evidence.
//!
//! This does not establish catalog completeness, guest ownership or a reserve.
use super::HostStorageReport;
use ramshared_ipc::storage_proof::{MAX_STORAGE_AGE_MS, StorageCandidate, VerifiedStorageRequest};

/// `age_ms` is the monotonic age from the supervised producer's start, not
/// the age since its result was received. A caller must preserve custody.
pub fn from_supervised_report(
    request: &VerifiedStorageRequest,
    report: &HostStorageReport,
    age_ms: u64,
) -> Option<StorageCandidate> {
    report.validate().ok()?;
    let disk_id = report.virtual_disk_id?;
    if disk_id != request.disk_id() || age_ms < report.elapsed_ms || age_ms >= MAX_STORAGE_AGE_MS {
        return None;
    }
    let sample = &report.sample;
    if sample.file_identity.file_index == 0 {
        return None;
    }
    Some(StorageCandidate {
        disk_id,
        volume_id: sample.volume_id.clone(),
        file_serial: sample.file_identity.volume_serial,
        file_index: sample.file_identity.file_index,
        total_bytes: sample.total_bytes,
        available_bytes: sample.available_bytes,
        age_ms,
    })
}
