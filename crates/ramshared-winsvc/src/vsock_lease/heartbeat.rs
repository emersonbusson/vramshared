// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/vsock_lease/heartbeat.rs
//! Bounded heartbeat response composition; observation effects are injected.
use super::{LeasePolicy, LeaseServerError, LeaseSession};
use ramshared_ipc::deadline::DeadlineIo;
use ramshared_ipc::gpu_catalog::HostGpuActivityObservation;
use ramshared_ipc::vsock::VsockStreamTrait;
use ramshared_ipc::{
    Heartbeat, HeartbeatAck, HostMemoryTelemetry, MAX_CONTROL_PAYLOAD, MSG_GPU_CATALOG,
    MSG_HEARTBEAT, MSG_HEARTBEAT_ACK, MSG_LEASE_RELEASE, MSG_STORAGE_REQUEST, decode_control,
    encode_control, read_frame, write_frame,
};
use ramshared_vram::host_identity::HostGpuObservation;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

/// Supervised Windows storage observation returned for an authenticated v5
/// `MSG_STORAGE_REQUEST`.
#[derive(Clone)]
pub struct StorageSample {
    pub report: crate::host_storage::HostStorageReport,
    pub age_ms: u64,
    pub captured_since: Instant,
    pub complete: bool,
}

pub type StorageObserver = Arc<dyn Fn([u8; 16]) -> Option<StorageSample> + Send + Sync>;

#[derive(Default)]
pub struct HostSamples {
    pub memory: Option<HostMemoryTelemetry>,
    pub gpu: Option<HostGpuObservation>,
    pub activity: Option<HostGpuActivityObservation>,
    pub storage: Option<StorageObserver>,
}

fn write_heartbeat(
    stream: &mut impl std::io::Write,
    session: &mut LeaseSession,
    policy: LeasePolicy,
    secret: &[u8],
    now_ms: u64,
    correlation: u64,
    samples: HostSamples,
) -> Result<u64, LeaseServerError> {
    let deadline = now_ms.saturating_add(policy.timeout_ms());
    let expired = now_ms >= session.deadline_ms;
    let ack = HeartbeatAck {
        timestamp_ms: now_ms,
        lease_remaining_ms: if expired {
            0
        } else {
            deadline.saturating_sub(now_ms)
        },
        host_memory: samples.memory,
    };
    // Admit both complete payloads before emitting the successful ack.
    let control = encode_control(&ack)?;
    let catalog = match session.catalog.as_mut() {
        Some(catalog) if !expired => {
            Some(catalog.seal_with_activity(secret, &ack, samples.gpu, samples.activity)?)
        }
        _ => None,
    };
    write_frame(stream, MSG_HEARTBEAT_ACK, &control, correlation)?;
    if expired {
        return Err(LeaseServerError::LeaseExpired);
    }
    if let Some(catalog) = catalog {
        write_frame(stream, MSG_GPU_CATALOG, &catalog, correlation)?;
    }
    Ok(deadline)
}

/// One response deadline follows the bounded idle wait. Failed exchanges are
/// never replayed; original observation ages arrive from the custody owner.
pub fn serve_heartbeats<S: VsockStreamTrait, F: FnMut() -> u64, H: FnMut() -> HostSamples>(
    stream: &mut S,
    session: &mut LeaseSession,
    policy: LeasePolicy,
    now: F,
    max_beats: Option<usize>,
    samples: H,
    secret: &[u8],
) -> Result<usize, LeaseServerError> {
    serve_heartbeats_with_stop(
        stream,
        session,
        policy,
        now,
        max_beats,
        samples,
        SessionControl { secret, stop: None },
    )
}

pub(super) struct SessionControl<'a> {
    pub secret: &'a [u8],
    pub stop: Option<&'a AtomicBool>,
}

pub(super) fn serve_heartbeats_with_stop<
    S: VsockStreamTrait,
    F: FnMut() -> u64,
    H: FnMut() -> HostSamples,
>(
    stream: &mut S,
    session: &mut LeaseSession,
    policy: LeasePolicy,
    mut now: F,
    max_beats: Option<usize>,
    mut samples: H,
    control: SessionControl<'_>,
) -> Result<usize, LeaseServerError> {
    let mut served = 0;
    loop {
        if max_beats.is_some_and(|max| served >= max) {
            return Ok(served);
        }
        let budget = Duration::from_secs(policy.lease_timeout_secs.saturating_add(2));
        let mut exchange = match control.stop {
            Some(stop) => DeadlineIo::new_cancellable(&mut *stream, budget, stop),
            None => DeadlineIo::new(&mut *stream, budget),
        }
        .map_err(LeaseServerError::Io)?;
        let (msg_type, payload, correlation) = read_frame(&mut exchange, MAX_CONTROL_PAYLOAD)?;
        exchange
            .shorten(Duration::from_secs(5))
            .map_err(LeaseServerError::Io)?;
        if msg_type == MSG_LEASE_RELEASE {
            exchange.finish().map_err(LeaseServerError::Io)?;
            return Ok(served);
        }
        if msg_type == MSG_STORAGE_REQUEST {
            handle_storage_request(
                &mut exchange,
                session,
                control.secret,
                now(),
                correlation,
                &payload,
                &samples(),
            )?;
            exchange.finish().map_err(LeaseServerError::Io)?;
            continue;
        }
        if session
            .catalog
            .as_ref()
            .is_some_and(|catalog| correlation != catalog.round())
        {
            return Err(LeaseServerError::Protocol("heartbeat round mismatch"));
        }
        if msg_type != MSG_HEARTBEAT {
            return Err(LeaseServerError::Protocol(
                "expected heartbeat or lease release",
            ));
        }
        let _beat: Heartbeat = decode_control(&payload)?;
        let response_round = if session.catalog.is_some() {
            correlation
        } else {
            4
        };
        let deadline = write_heartbeat(
            &mut exchange,
            session,
            policy,
            control.secret,
            now(),
            response_round,
            samples(),
        )?;
        exchange.finish().map_err(LeaseServerError::Io)?;
        session.deadline_ms = deadline;
        served += 1;
    }
}

fn handle_storage_request(
    writer: &mut impl std::io::Write,
    session: &mut LeaseSession,
    secret: &[u8],
    now_ms: u64,
    round: u64,
    payload: &[u8],
    samples: &HostSamples,
) -> Result<(), LeaseServerError> {
    if now_ms >= session.deadline_ms {
        return Err(LeaseServerError::LeaseExpired);
    }
    let Some(storage_session) = session.storage.as_mut() else {
        return Err(LeaseServerError::Protocol(
            "storage request requires protocol v5",
        ));
    };
    let remaining_ms = session.deadline_ms.saturating_sub(now_ms);
    let lease_deadline = Instant::now()
        .checked_add(Duration::from_millis(remaining_ms))
        .ok_or(LeaseServerError::LeaseExpired)?;
    crate::host_storage::exchange::respond_to_received_with_supervised_sample(
        writer,
        storage_session,
        secret,
        round,
        payload,
        lease_deadline,
        |disk_id| {
            samples
                .storage
                .as_ref()
                .and_then(|observer| observer(disk_id))
                .map(|s| (s.report, s.age_ms, s.captured_since, s.complete))
                .ok_or("no fresh Windows host storage report matches disk_id")
        },
    )
    .map_err(LeaseServerError::Frame)
}
