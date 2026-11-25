// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/host_storage_match.rs
//! Read-only joining of guest device observations with host storage samples.
//!
//! This join grants no allocation authority: catalog completeness, guest
//! ownership, backing capacity reservation and effect fencing are separate.

use std::time::Instant;

use ramshared_block::vhd_identity::RootVhdObservation;
use ramshared_ipc::storage_proof::{AuthenticatedStorage, StorageCandidate};

/// Select a sample only for the observed directory device and authenticated
/// request. The caller must independently establish catalog completeness.
pub fn match_directory_observation<'a>(
    directory: &RootVhdObservation,
    sample: &'a AuthenticatedStorage,
    catalog_complete: bool,
    now: Instant,
    lease_is_current: impl FnOnce(u32) -> bool,
) -> Option<&'a StorageCandidate> {
    const MAX_DIRECTORY_AGE: std::time::Duration = std::time::Duration::from_secs(5);
    if !catalog_complete
        || directory.identifier == [0; 16]
        || !now
            .checked_duration_since(directory.observed_at)
            .is_some_and(|age| age < MAX_DIRECTORY_AGE)
    {
        return None;
    }
    sample.candidate_for(directory.identifier, now, lease_is_current)
}

#[cfg(test)]
#[path = "host_storage_match/tests.rs"]
#[allow(clippy::unwrap_used)]
mod tests;
