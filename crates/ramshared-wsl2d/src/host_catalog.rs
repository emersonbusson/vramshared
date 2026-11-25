// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/host_catalog.rs
//! Authenticated host GPU catalog propagation across the parent/worker boundary.
//!
//! Only verified, unexpired [`ramshared_ipc::gpu_catalog::AuthenticatedCatalog`]
//! reports are published by the control plane. Workers receive snapshots and
//! immediate revocations over a kernel-created anonymous `UnixStream::pair()`.
use std::io::{self, ErrorKind, Read};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use ramshared_ipc::gpu_catalog::AuthenticatedCatalog;
use ramshared_vram::GPU_BUDGET_MAX_AGE_MS;
use ramshared_vram::host_identity::{HostGpuIdentityReport, MAX_IDENTITY_REPORT_BYTES};
use rustix::net::{SendFlags, send};
use serde::{Deserialize, Serialize};

pub const CATALOG_FD_ARG: &str = "--catalog-fd";
const MAX_CATALOG_NOTICE_BYTES: usize = MAX_IDENTITY_REPORT_BYTES + 256;
const MAX_RX_BUFFER_BYTES: usize = MAX_CATALOG_NOTICE_BYTES * 2;
const FRAME_HEADER_BYTES: usize = std::mem::size_of::<u32>();

/// Read-only view of the latest unexpired host GPU identity catalog.
pub trait HostGpuCatalogProvider: Send + Sync {
    fn current_report(&self) -> Option<HostGpuIdentityReport>;
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
enum CatalogWireNotice {
    Catalog {
        expires_monotonic_ns: u64,
        report: HostGpuIdentityReport,
    },
    Revoked,
}

#[derive(Debug, Default)]
struct SharedCatalogState {
    current: Option<AuthenticatedCatalog>,
    subscribers: Vec<UnixStream>,
    poisoned: bool,
    generation: u64,
}

/// Parent-owned live catalog state shared between the vsock heartbeat loop,
/// recovery admission gating, and child worker notification streams.
#[derive(Clone, Debug, Default)]
pub struct SharedHostCatalog {
    inner: Arc<Mutex<SharedCatalogState>>,
    ready: Arc<Condvar>,
}

impl SharedHostCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn publish(&self, catalog: Option<&AuthenticatedCatalog>) {
        let Ok(mut state) = self.inner.lock() else {
            return;
        };
        if state.poisoned {
            state.current = None;
            close_subscribers(&mut state.subscribers);
            return;
        }
        let next = catalog.and_then(catalog_notice);
        let frame = match encode_notice(
            &next
                .as_ref()
                .map_or(CatalogWireNotice::Revoked, |(_, notice)| notice.clone()),
        ) {
            Ok(frame) => frame,
            Err(_) => {
                state.current = None;
                close_subscribers(&mut state.subscribers);
                return;
            }
        };
        let now = Instant::now();
        let was_absent = state
            .current
            .as_ref()
            .and_then(|entry| entry.report(now))
            .is_none();
        let next_entry = next.map(|(entry, _)| entry);
        if was_absent && next_entry.is_some() {
            state.generation = state.generation.saturating_add(1);
        }
        state.current = next_entry;
        state
            .subscribers
            .retain(|subscriber| send_notice_nonblocking(subscriber, &frame));
        let notify = state.current.is_some() || state.poisoned;
        drop(state);
        if notify {
            self.ready.notify_all();
        }
    }

    /// Block until an unexpired host GPU catalog is published or `timeout` elapses.
    ///
    /// Intended strictly for bounded startup initialization before NBD serving.
    pub fn wait_for_current_report(&self, timeout: Duration) -> bool {
        let Ok(mut state) = self.inner.lock() else {
            return false;
        };
        let has_live = |state: &SharedCatalogState, now: Instant| {
            !state.poisoned
                && state
                    .current
                    .as_ref()
                    .and_then(|catalog| catalog.report(now))
                    .is_some()
        };
        let mut now = Instant::now();
        if has_live(&state, now) {
            return true;
        }
        let Some(deadline) = now.checked_add(timeout) else {
            return false;
        };
        while !state.poisoned {
            let Some(remaining) = deadline.checked_duration_since(now) else {
                return false;
            };
            if remaining.is_zero() {
                return false;
            }
            let Ok((next, _)) = self.ready.wait_timeout(state, remaining) else {
                return false;
            };
            state = next;
            now = Instant::now();
            if has_live(&state, now) {
                return true;
            }
        }
        false
    }

    /// A busy, poisoned or expired catalog cannot admit a recovery worker.
    /// This read never waits for the publisher or clones the report.
    pub fn try_has_current_report(&self) -> bool {
        self.inner.try_lock().ok().is_some_and(|state| {
            !state.poisoned
                && state
                    .current
                    .as_ref()
                    .and_then(|catalog| catalog.report(Instant::now()))
                    .is_some()
        })
    }

    /// Observe generation and authority under the same lock. A revoked,
    /// expired, busy or poisoned catalog cannot replenish a worker retry budget.
    pub fn live_generation(&self) -> Option<u64> {
        let state = self.inner.try_lock().ok()?;
        if state.poisoned {
            return None;
        }
        state.current.as_ref()?.report(Instant::now())?;
        Some(state.generation)
    }

    pub fn revoke(&self) {
        let frame = encode_notice(&CatalogWireNotice::Revoked).ok();
        let Ok(mut state) = self.inner.lock() else {
            return;
        };
        state.current = None;
        if let Some(frame) = frame {
            state
                .subscribers
                .retain(|subscriber| send_notice_nonblocking(subscriber, &frame));
        } else {
            close_subscribers(&mut state.subscribers);
        }
    }

    /// Open a non-blocking anonymous Unix stream for a child `__gpu_worker`,
    /// pre-seeding any currently valid unexpired catalog before inheritance.
    pub fn subscribe(&self) -> io::Result<UnixStream> {
        let (writer, reader) = UnixStream::pair()?;
        writer.set_nonblocking(true)?;
        reader.set_nonblocking(true)?;

        let Ok(mut state) = self.inner.lock() else {
            return Err(io::Error::other("host GPU catalog mutex is poisoned"));
        };
        if state.poisoned {
            return Err(io::Error::other("host GPU catalog is poisoned"));
        }

        if let Some((_, notice)) = state.current.as_ref().and_then(catalog_notice) {
            let frame = encode_notice(&notice).map_err(io::Error::other)?;
            if !send_notice_nonblocking(&writer, &frame) {
                return Err(io::Error::other(
                    "failed to pre-seed host GPU catalog subscriber stream",
                ));
            }
        }
        state.subscribers.push(writer);
        Ok(reader)
    }
}

impl HostGpuCatalogProvider for SharedHostCatalog {
    fn current_report(&self) -> Option<HostGpuIdentityReport> {
        let state = self.inner.lock().ok()?;
        if state.poisoned {
            return None;
        }
        state.current.as_ref()?.report(Instant::now()).cloned()
    }
}

#[derive(Debug)]
struct WorkerCatalogState {
    stream: Option<UnixStream>,
    rx_buf: Vec<u8>,
    current: Option<(HostGpuIdentityReport, Instant)>,
    closed: bool,
}

/// Child-process non-blocking consumer of parent host GPU catalog notices.
/// Fails permanently closed on EOF, protocol corruption, or lock poisoning.
pub struct WorkerHostCatalog {
    state: Mutex<WorkerCatalogState>,
}

impl WorkerHostCatalog {
    pub fn from_stream(stream: UnixStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self {
            state: Mutex::new(WorkerCatalogState {
                stream: Some(stream),
                rx_buf: Vec::new(),
                current: None,
                closed: false,
            }),
        })
    }
}

impl HostGpuCatalogProvider for WorkerHostCatalog {
    fn current_report(&self) -> Option<HostGpuIdentityReport> {
        let Ok(mut state) = self.state.lock() else {
            return None;
        };
        if state.closed {
            state.current = None;
            return None;
        }
        drain_worker_stream(&mut state);
        if state.closed {
            state.current = None;
            state.stream = None;
            state.rx_buf.clear();
            return None;
        }
        let now = Instant::now();
        let valid = state
            .current
            .as_ref()
            .filter(|(_, expires)| now < *expires)
            .map(|(report, _)| report.clone());
        if valid.is_none() {
            state.current = None;
        }
        valid
    }
}

fn close_subscribers(subscribers: &mut Vec<UnixStream>) {
    for subscriber in subscribers.drain(..) {
        let _ = subscriber.shutdown(Shutdown::Both);
    }
}

// CLOCK_MONOTONIC is shared by parent and child. Capture it before the
// Instant used to compute remaining TTL, so rounding and scheduling can only
// shorten the transmitted lifetime, never extend it.
fn monotonic_ns() -> Option<u64> {
    let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    u64::try_from(now.tv_sec)
        .ok()?
        .checked_mul(1_000_000_000)?
        .checked_add(u64::try_from(now.tv_nsec).ok()?)
}

fn catalog_notice(
    entry: &AuthenticatedCatalog,
) -> Option<(AuthenticatedCatalog, CatalogWireNotice)> {
    let monotonic = monotonic_ns()?;
    let now = Instant::now();
    let remaining_ns = entry.remaining_ms(now)?.checked_mul(1_000_000)?;
    let report = entry.report(now)?.clone();
    report.validate().ok()?;
    let expires_monotonic_ns = monotonic.checked_add(remaining_ns)?;
    Some((
        entry.clone(),
        CatalogWireNotice::Catalog {
            expires_monotonic_ns,
            report,
        },
    ))
}

fn encode_notice(notice: &CatalogWireNotice) -> Result<Vec<u8>, String> {
    if let CatalogWireNotice::Catalog {
        expires_monotonic_ns,
        report,
    } = notice
    {
        if *expires_monotonic_ns == 0 {
            return Err("host GPU catalog deadline is invalid".into());
        }
        report.validate()?;
    }
    let payload = serde_json::to_vec(notice).map_err(|error| error.to_string())?;
    if payload.len() > MAX_CATALOG_NOTICE_BYTES {
        return Err("host GPU catalog wire notice exceeds byte limit".into());
    }
    let len = u32::try_from(payload.len())
        .map_err(|_| "host GPU catalog wire notice length overflows u32".to_string())?;
    let mut frame = Vec::new();
    frame
        .try_reserve_exact(FRAME_HEADER_BYTES + payload.len())
        .map_err(|error| error.to_string())?;
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

fn send_notice_nonblocking(stream: &UnixStream, frame: &[u8]) -> bool {
    loop {
        match send(stream, frame, SendFlags::NOSIGNAL | SendFlags::DONTWAIT) {
            Ok(written) => {
                if written == frame.len() {
                    return true;
                }
                let _ = stream.shutdown(Shutdown::Both);
                return false;
            }
            Err(error) if error == rustix::io::Errno::INTR => continue,
            Err(_) => {
                let _ = stream.shutdown(Shutdown::Both);
                return false;
            }
        }
    }
}

fn drain_worker_stream(state: &mut WorkerCatalogState) {
    let Some(mut stream) = state.stream.take() else {
        state.closed = true;
        return;
    };
    let mut scratch = [0_u8; 8192];
    loop {
        match stream.read(&mut scratch) {
            Ok(0) => {
                state.closed = true;
                return;
            }
            Ok(n) => {
                if state.rx_buf.len().saturating_add(n) > MAX_RX_BUFFER_BYTES {
                    state.closed = true;
                    return;
                }
                state.rx_buf.extend_from_slice(&scratch[..n]);
                if !parse_available_frames(state) {
                    state.closed = true;
                    return;
                }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                state.stream = Some(stream);
                return;
            }
            Err(_) => {
                state.closed = true;
                return;
            }
        }
    }
}

fn parse_available_frames(state: &mut WorkerCatalogState) -> bool {
    let mut consumed = 0usize;
    let ok = loop {
        let remaining = &state.rx_buf[consumed..];
        if remaining.len() < FRAME_HEADER_BYTES {
            break true;
        }
        let Ok(len_bytes) = remaining[..FRAME_HEADER_BYTES].try_into() else {
            break false;
        };
        let payload_len = u32::from_le_bytes(len_bytes) as usize;
        if payload_len == 0 || payload_len > MAX_CATALOG_NOTICE_BYTES {
            break false;
        }
        let frame_len = FRAME_HEADER_BYTES + payload_len;
        if remaining.len() < frame_len {
            break true;
        }
        let payload = &remaining[FRAME_HEADER_BYTES..frame_len];
        let Ok(notice) = serde_json::from_slice::<CatalogWireNotice>(payload) else {
            break false;
        };
        match notice {
            CatalogWireNotice::Catalog {
                expires_monotonic_ns,
                report,
            } => {
                if report.validate().is_err() {
                    break false;
                }
                // Capture Instant first so the conversion cannot extend the
                // sender's absolute deadline while this thread is delayed.
                let instant = Instant::now();
                let Some(now) = monotonic_ns() else {
                    break false;
                };
                let Some(remaining_ns) = expires_monotonic_ns.checked_sub(now) else {
                    state.current = None;
                    consumed += frame_len;
                    continue;
                };
                if remaining_ns == 0 {
                    state.current = None;
                    consumed += frame_len;
                    continue;
                }
                if remaining_ns > GPU_BUDGET_MAX_AGE_MS.saturating_mul(1_000_000) {
                    break false;
                }
                let Some(expires) = instant.checked_add(Duration::from_nanos(remaining_ns)) else {
                    break false;
                };
                state.current = Some((report, expires));
            }
            CatalogWireNotice::Revoked => {
                state.current = None;
            }
        }
        consumed += frame_len;
    };
    if consumed > 0 {
        state.rx_buf.drain(..consumed);
    }
    ok
}

#[cfg(test)]
mod tests;
