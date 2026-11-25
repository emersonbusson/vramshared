// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/host_storage/exchange.rs
//! Bounded protocol adapter for a previously supervised host sample.
//!
//! A sample does not establish guest identity, global inventory completeness,
//! or reserved capacity. The caller must independently establish all three.

use super::{HostStorageReport, candidate};
use ramshared_ipc::FrameError;
use ramshared_ipc::storage_proof::{StorageCandidate, StorageSession, VerifiedStorageRequest};
use ramshared_ipc::storage_transport::{respond_to_received_or_unavailable, respond_to_request};
use std::io::{Read, Write};
use std::time::Instant;

/// Observation taken under external process custody. `captured_since` must be
/// recorded immediately before retrieving the supervised snapshot, never after.
pub struct SupervisedSample<'a> {
    pub report: &'a HostStorageReport,
    pub age_ms: u64,
    pub captured_since: Instant,
    /// Caller-established completeness for the requested observation scope.
    /// This alone never proves global inventory completeness.
    pub complete: bool,
}

fn build_verified_candidate(
    request: &VerifiedStorageRequest,
    sample: SupervisedSample<'_>,
) -> Result<(bool, Vec<StorageCandidate>), FrameError> {
    let elapsed = Instant::now()
        .checked_duration_since(sample.captured_since)
        .ok_or_else(|| refuse("sample clock moved backwards"))?;
    let elapsed_ms = u64::try_from(elapsed.as_nanos().div_ceil(1_000_000))
        .map_err(|_| refuse("sample age overflow"))?;
    let age = sample
        .age_ms
        .checked_add(elapsed_ms)
        .ok_or_else(|| refuse("sample age overflow"))?;
    let candidate = candidate::from_supervised_report(request, sample.report, age)
        .ok_or_else(|| refuse("sample is stale or does not match the request"))?;
    Ok((sample.complete, vec![candidate]))
}

/// Reply only after the transport authenticates the requested disk. The caller
/// must apply an I/O deadline no later than `lease_deadline`, use a supervised
/// sample, and discard the stream after any failure.
pub fn respond_with_supervised_sample(
    stream: &mut (impl Read + Write),
    session: &mut StorageSession,
    secret: &[u8],
    lease_deadline: Instant,
    sample: SupervisedSample<'_>,
) -> Result<(), FrameError> {
    respond_to_request(stream, session, secret, lease_deadline, |request| {
        build_verified_candidate(request, sample)
    })
}

/// Authenticate a pre-read v5 storage request frame, then retrieve and validate
/// the supervised sample for `request.disk_id()`. When no fresh complete
/// sample is available, answer with an authenticated unavailable observation
/// so the live v5 vsock lease survives transient NTFS stalls.
pub fn respond_to_received_with_supervised_sample<T>(
    writer: &mut impl Write,
    session: &mut StorageSession,
    secret: &[u8],
    round: u64,
    payload: &[u8],
    lease_deadline: Instant,
    mut fetch: impl FnMut([u8; 16]) -> Result<(HostStorageReport, u64, Instant, bool), T>,
) -> Result<(), FrameError>
where
    T: std::fmt::Display,
{
    respond_to_received_or_unavailable(
        writer,
        session,
        secret,
        round,
        payload,
        lease_deadline,
        |request| {
            let Ok((report, age_ms, captured_since, true)) = fetch(request.disk_id()) else {
                return Ok(None);
            };
            match build_verified_candidate(
                request,
                SupervisedSample {
                    report: &report,
                    age_ms,
                    captured_since,
                    complete: true,
                },
            ) {
                Ok((complete, candidates)) => Ok(Some((complete, candidates))),
                Err(_) => Ok(None),
            }
        },
    )
}

fn refuse(reason: &str) -> FrameError {
    FrameError::PayloadDeserialization(format!("host storage exchange refused: {reason}"))
}

#[cfg(test)]
#[path = "exchange/tests.rs"]
#[allow(clippy::unwrap_used)]
mod tests;
