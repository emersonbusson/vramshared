// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-ipc/src/storage_transport.rs
//! Version-five storage frames for an already authenticated, leased stream.
//!
//! This module does not negotiate or advertise version five. Callers must use
//! an absolute stream deadline no later than the lease deadline, discard a failed
//! connection, and separately
//! establish guest ownership, catalog completeness and capacity reservation.

use crate::storage_proof::{
    AuthenticatedStorage, StorageCandidate, StorageSession, VerifiedStorageRequest,
};
#[cfg(target_os = "linux")]
use crate::storage_proof::{BootBoundStorageDeadline, storage_boot_clock};
use crate::{
    FRAME_HEADER_LEN, FrameError, IPC_MAGIC, IPC_VERSION_5, MSG_STORAGE_REQUEST,
    MSG_STORAGE_RESPONSE, VsockFrameHeader,
};
use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Instant;

const REQUEST_CAP: usize = 1024;
const RESPONSE_CAP: usize = 64 * 1024;

/// A single in-flight request bound to its session and monotonic start time.
pub struct PendingStorage {
    disk_id: [u8; 16],
    round: u64,
    #[cfg(not(target_os = "linux"))]
    started: Instant,
    lease_remaining_ms: u64,
    session: Arc<()>,
    #[cfg(target_os = "linux")]
    boot_deadline: BootBoundStorageDeadline,
}

fn frame_error(reason: &str) -> FrameError {
    FrameError::PayloadDeserialization(format!("storage transport refused: {reason}"))
}

#[cfg(target_os = "linux")]
fn suspend_aware_start(
    deadline: &BootBoundStorageDeadline,
    boot_id: &str,
    boot_ns: u64,
    now: Instant,
) -> Result<Instant, FrameError> {
    let elapsed = deadline
        .elapsed(boot_id, boot_ns)
        .ok_or_else(|| frame_error("boot deadline expired or changed"))?;
    now.checked_sub(elapsed)
        .ok_or_else(|| frame_error("boot elapsed time is unrepresentable"))
}

fn write_storage_frame(
    writer: &mut impl Write,
    kind: u8,
    round: u64,
    bytes: &[u8],
    cap: usize,
) -> Result<(), FrameError> {
    if bytes.is_empty() || bytes.len() > cap || round == 0 {
        return Err(frame_error("invalid frame size or round"));
    }
    let header = VsockFrameHeader {
        magic: IPC_MAGIC,
        version: IPC_VERSION_5,
        payload_len: u32::try_from(bytes.len()).map_err(|_| FrameError::PayloadLengthOverflow)?,
        flags: u32::from(kind),
        correlation_id: round,
    };
    writer
        .write_all(&header.encode())
        .map_err(|e| FrameError::StreamIo(e.to_string()))?;
    writer
        .write_all(bytes)
        .map_err(|e| FrameError::StreamIo(e.to_string()))?;
    writer
        .flush()
        .map_err(|e| FrameError::StreamIo(e.to_string()))
}

fn read_storage_frame(
    reader: &mut impl Read,
    kind: u8,
    round: u64,
    cap: usize,
) -> Result<Vec<u8>, FrameError> {
    let mut header = [0u8; FRAME_HEADER_LEN];
    reader
        .read_exact(&mut header)
        .map_err(|e| FrameError::StreamIo(e.to_string()))?;
    let word = |offset| {
        u32::from_le_bytes([
            header[offset],
            header[offset + 1],
            header[offset + 2],
            header[offset + 3],
        ])
    };
    let correlation = u64::from_le_bytes(
        header[16..24]
            .try_into()
            .map_err(|_| frame_error("header"))?,
    );
    let size = word(8) as usize;
    if word(0) != IPC_MAGIC
        || word(4) != IPC_VERSION_5
        || word(12) != u32::from(kind)
        || correlation != round
        || round == 0
        || size == 0
        || size > cap
    {
        return Err(frame_error(
            "frame identity, version, size or round mismatch",
        ));
    }
    let mut bytes = vec![0; size];
    reader
        .read_exact(&mut bytes)
        .map_err(|e| FrameError::StreamIo(e.to_string()))?;
    Ok(bytes)
}

/// Send a request under the authenticated lease's absolute monotonic deadline.
/// Any failed send retires this session; discard the stream.
pub fn send_request(
    writer: &mut impl Write,
    session: &mut StorageSession,
    secret: &[u8],
    disk_id: [u8; 16],
    lease_deadline: Instant,
) -> Result<PendingStorage, FrameError> {
    let round = session.transport_round();
    let result = (|| {
        #[cfg(target_os = "linux")]
        let (boot_id, boot_started_ns) =
            storage_boot_clock().ok_or_else(|| frame_error("boot clock unavailable"))?;
        let started = Instant::now();
        let lease_remaining_ms_at_start = lease_deadline
            .checked_duration_since(started)
            .and_then(|remaining| u64::try_from(remaining.as_millis()).ok())
            .filter(|remaining| *remaining > 0)
            .ok_or_else(|| frame_error("lease expired before request"))?;
        let bytes = session.request(secret, disk_id)?;
        write_storage_frame(writer, MSG_STORAGE_REQUEST, round, &bytes, REQUEST_CAP)?;
        if Instant::now() >= lease_deadline {
            return Err(frame_error("lease expired while sending request"));
        }
        Ok(PendingStorage {
            disk_id,
            round,
            #[cfg(not(target_os = "linux"))]
            started,
            lease_remaining_ms: lease_remaining_ms_at_start,
            session: session.transport_identity(),
            #[cfg(target_os = "linux")]
            boot_deadline: BootBoundStorageDeadline::new(
                &boot_id,
                boot_started_ns,
                0,
                lease_remaining_ms_at_start,
            )
            .ok_or_else(|| frame_error("invalid boot deadline"))?,
        })
    })();
    if result.is_err() {
        session.retire();
    }
    result
}

fn respond_with_payload(
    writer: &mut impl Write,
    session: &mut StorageSession,
    secret: &[u8],
    round: u64,
    bytes: &[u8],
    lease_deadline: Instant,
    observe: impl FnOnce(
        &VerifiedStorageRequest,
    ) -> Result<Option<(bool, Vec<StorageCandidate>)>, FrameError>,
) -> Result<(), FrameError> {
    if round == 0
        || round != session.transport_round()
        || bytes.is_empty()
        || bytes.len() > REQUEST_CAP
    {
        return Err(frame_error("invalid frame size or round"));
    }
    let request = session.verify_request(secret, bytes)?;
    if Instant::now() >= lease_deadline {
        return Err(frame_error("host lease expired before observation"));
    }
    let observed = observe(&request)?;
    if Instant::now() >= lease_deadline {
        return Err(frame_error("host lease expired during observation"));
    }
    let response = match observed {
        Some((complete, candidates)) => {
            session.seal_requested(secret, request, complete, candidates)?
        }
        None => session.seal_unavailable(secret, request)?,
    };
    write_storage_frame(writer, MSG_STORAGE_RESPONSE, round, &response, RESPONSE_CAP)?;
    if Instant::now() >= lease_deadline {
        return Err(frame_error("host lease expired while sending response"));
    }
    Ok(())
}

/// Authenticate a request before observing its disk and send one response.
/// The caller must supervise and bound the observer independently.
pub fn respond_to_request(
    stream: &mut (impl Read + Write),
    session: &mut StorageSession,
    secret: &[u8],
    lease_deadline: Instant,
    observe: impl FnOnce(&VerifiedStorageRequest) -> Result<(bool, Vec<StorageCandidate>), FrameError>,
) -> Result<(), FrameError> {
    let round = session.transport_round();
    let result = (|| {
        if Instant::now() >= lease_deadline {
            return Err(frame_error("host lease expired before request"));
        }
        let bytes = read_storage_frame(stream, MSG_STORAGE_REQUEST, round, REQUEST_CAP)?;
        respond_with_payload(
            stream,
            session,
            secret,
            round,
            &bytes,
            lease_deadline,
            |request| observe(request).map(Some),
        )
    })();
    if result.is_err() {
        session.retire();
    }
    result
}

/// Authenticate a request frame whose header and payload were already read by a
/// multiplexed v5 control loop, then write the single response frame.
pub fn respond_to_received_request(
    writer: &mut impl Write,
    session: &mut StorageSession,
    secret: &[u8],
    round: u64,
    payload: &[u8],
    lease_deadline: Instant,
    observe: impl FnOnce(&VerifiedStorageRequest) -> Result<(bool, Vec<StorageCandidate>), FrameError>,
) -> Result<(), FrameError> {
    respond_to_received_or_unavailable(
        writer,
        session,
        secret,
        round,
        payload,
        lease_deadline,
        |request| observe(request).map(Some),
    )
}

/// Authenticate a request frame whose header and payload were already read by a
/// multiplexed v5 control loop, answering either a candidate observation
/// (`Some`) or an authenticated transient-unavailable frame (`None`) so the
/// host lease survives transient NTFS inspection stalls.
pub fn respond_to_received_or_unavailable(
    writer: &mut impl Write,
    session: &mut StorageSession,
    secret: &[u8],
    round: u64,
    payload: &[u8],
    lease_deadline: Instant,
    observe: impl FnOnce(
        &VerifiedStorageRequest,
    ) -> Result<Option<(bool, Vec<StorageCandidate>)>, FrameError>,
) -> Result<(), FrameError> {
    let result = (|| {
        if Instant::now() >= lease_deadline {
            return Err(frame_error("host lease expired before request"));
        }
        respond_with_payload(
            writer,
            session,
            secret,
            round,
            payload,
            lease_deadline,
            observe,
        )
    })();
    if result.is_err() {
        session.retire();
    }
    result
}

fn receive_with<T>(
    reader: &mut impl Read,
    session: &mut StorageSession,
    pending: PendingStorage,
    open: impl FnOnce(
        &mut StorageSession,
        [u8; 16],
        &[u8],
        Instant,
        u64,
        &PendingStorage,
    ) -> Result<T, FrameError>,
) -> Result<T, FrameError> {
    let result = (|| {
        if !Arc::ptr_eq(&pending.session, &session.transport_identity())
            || pending.round != session.transport_round()
        {
            return Err(frame_error("pending request belongs to another session"));
        }
        let bytes = read_storage_frame(reader, MSG_STORAGE_RESPONSE, pending.round, RESPONSE_CAP)?;
        #[cfg(target_os = "linux")]
        let started = {
            let (boot_id, boot_ns) =
                storage_boot_clock().ok_or_else(|| frame_error("boot clock unavailable"))?;
            suspend_aware_start(&pending.boot_deadline, &boot_id, boot_ns, Instant::now())?
        };
        #[cfg(not(target_os = "linux"))]
        let started = pending.started;
        open(
            session,
            pending.disk_id,
            &bytes,
            started,
            pending.lease_remaining_ms,
            &pending,
        )
    })();
    if result.is_err() {
        session.retire();
    }
    result
}

/// Receive the exact response and apply the request and live lease age bounds.
pub fn receive_response(
    reader: &mut impl Read,
    session: &mut StorageSession,
    secret: &[u8],
    pending: PendingStorage,
) -> Result<AuthenticatedStorage, FrameError> {
    receive_with(
        reader,
        session,
        pending,
        |session, disk_id, bytes, started, lease_remaining_ms, pending| {
            let storage = session.open(
                secret,
                disk_id,
                bytes,
                started,
                Instant::now(),
                lease_remaining_ms,
            )?;
            #[cfg(target_os = "linux")]
            {
                let mut storage = storage;
                storage.bind_boot_deadline(&pending.boot_deadline)?;
                Ok(storage)
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = pending;
                Ok(storage)
            }
        },
    )
}

/// Receive an authenticated host response, returning `Ok(None)` when the host
/// proved the sample is transiently unavailable without retiring the session.
pub fn receive_response_optional(
    reader: &mut impl Read,
    session: &mut StorageSession,
    secret: &[u8],
    pending: PendingStorage,
) -> Result<Option<AuthenticatedStorage>, FrameError> {
    receive_with(
        reader,
        session,
        pending,
        |session, disk_id, bytes, started, lease_remaining_ms, pending| {
            let storage = session.open_optional(
                secret,
                disk_id,
                bytes,
                started,
                Instant::now(),
                lease_remaining_ms,
            )?;
            #[cfg(target_os = "linux")]
            {
                let mut storage = storage;
                if let Some(ref mut proof) = storage {
                    proof.bind_boot_deadline(&pending.boot_deadline)?;
                }
                Ok(storage)
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = pending;
                Ok(storage)
            }
        },
    )
}

#[cfg(all(test, target_os = "linux"))]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn suspend_elapsed_is_charged_before_response_authentication() {
        let boot = "11111111-2222-3333-4444-555555555555";
        let deadline = BootBoundStorageDeadline::new(boot, 1_000_000, 0, 5_000).unwrap();
        let now = Instant::now();
        let started = suspend_aware_start(&deadline, boot, 2_001_000_000, now).unwrap();
        assert_eq!(now.duration_since(started), Duration::from_secs(2));
        assert!(suspend_aware_start(&deadline, boot, 5_001_000_000, now).is_err());
        assert!(
            suspend_aware_start(
                &deadline,
                "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
                2_001_000_000,
                now,
            )
            .is_err()
        );
    }
}
