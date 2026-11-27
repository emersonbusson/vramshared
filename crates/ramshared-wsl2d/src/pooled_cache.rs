// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/pooled_cache.rs
//! Guest-side pooled cache session, origin exclusivity check, and bounded wire client.
//! SPEC: the protocol specification §DT-1, §DT-2, §DT-6, §DT-10, §DT-11.

pub mod connector;
pub mod profile;

use std::io::{Read, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use ramshared_block::isolated_origin::{
    POOLED_PAGE_BYTES, PooledCacheClient, PooledLookupReply, PooledReadTelemetry,
};
use ramshared_ipc::cache_wire::{
    CacheFrameBuffer, CacheFrameHeader, CacheOpcode, CacheScope, read_into, write_frame_with,
};
use serde::{Deserialize, Serialize};

use profile::{CacheProfile, CacheProfileSelection, ProfileError};

pub const VMADDR_CID_HOST: u32 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectiveCacheMode {
    Independent,
    PooledActive,
    PooledOriginOnly,
}

impl EffectiveCacheMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Independent => "independent",
            Self::PooledActive => "pooled_active",
            Self::PooledOriginOnly => "pooled_origin_only",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct PooledGuestStatus {
    pub configured_profile: CacheProfile,
    pub effective_mode: EffectiveCacheMode,
    pub reason: Option<String>,
    pub epoch: u64,
    pub generation_changes: u64,
    pub hits: u64,
    pub misses: u64,
    pub deadline_overruns: u64,
    pub peer_refusals: u64,
    pub origin_fallback_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostBootstrapProof {
    pub host_cid: u32,
    pub scope: CacheScope,
    pub host_proof: [u8; 32],
    pub key: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GuestBootError {
    AliasedSwapBacking(String),
}

impl std::fmt::Display for GuestBootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AliasedSwapBacking(id) => {
                write!(f, "refused aliased swap/origin backing identity: {id}")
            }
        }
    }
}
impl std::error::Error for GuestBootError {}

/// Verify that the guest's prepared origin identity does not alias any active swap or root disk.
pub fn verify_exclusive_origin(
    origin_identity: &str,
    active_swaps_and_roots: &[&str],
) -> Result<(), GuestBootError> {
    let trimmed = origin_identity.trim();
    if trimmed.is_empty() {
        return Err(GuestBootError::AliasedSwapBacking("empty".into()));
    }
    if active_swaps_and_roots
        .iter()
        .any(|entry| entry.trim().eq_ignore_ascii_case(trimmed))
    {
        return Err(GuestBootError::AliasedSwapBacking(trimmed.to_string()));
    }
    Ok(())
}

/// Guest pooled cache session owning boot-profile resolution, fencing, and status projection.
pub struct GuestCacheSession {
    configured_profile: CacheProfile,
    effective_mode: Mutex<EffectiveCacheMode>,
    reason: Mutex<Option<String>>,
    scope: Option<CacheScope>,
    epoch: AtomicU64,
    generation_changes: AtomicU64,
    peer_refusals: AtomicU64,
}

impl GuestCacheSession {
    /// Resolve boot profile against exclusive origin and optional host bootstrap proof (DT-1/DT-2).
    pub fn from_boot_selection(
        selection: Result<CacheProfileSelection, ProfileError>,
        origin_identity: &str,
        active_swaps_and_roots: &[&str],
        host: Option<HostBootstrapProof>,
    ) -> Result<Self, GuestBootError> {
        verify_exclusive_origin(origin_identity, active_swaps_and_roots)?;
        match selection {
            Err(err) => Ok(Self::new_inner(
                CacheProfile::Pooled,
                EffectiveCacheMode::PooledOriginOnly,
                Some(format!("malformed_profile:{err}")),
                None,
                0,
                1,
            )),
            Ok(CacheProfileSelection::DefaultIndependent) => Ok(Self::new_inner(
                CacheProfile::Independent,
                EffectiveCacheMode::Independent,
                None,
                None,
                0,
                0,
            )),
            Ok(CacheProfileSelection::Staged(doc)) if doc.profile == CacheProfile::Independent => {
                Ok(Self::new_inner(
                    CacheProfile::Independent,
                    EffectiveCacheMode::Independent,
                    None,
                    None,
                    0,
                    0,
                ))
            }
            Ok(CacheProfileSelection::Staged(_)) => match host {
                None => Ok(Self::new_inner(
                    CacheProfile::Pooled,
                    EffectiveCacheMode::PooledOriginOnly,
                    Some("host_cache_endpoint_absent".into()),
                    None,
                    0,
                    0,
                )),
                Some(proof) => {
                    if proof.host_cid != VMADDR_CID_HOST
                        || !proof.scope.verify_host_proof(&proof.key, &proof.host_proof)
                    {
                        return Ok(Self::new_inner(
                            CacheProfile::Pooled,
                            EffectiveCacheMode::PooledOriginOnly,
                            Some("host_proof_or_cid_refused".into()),
                            None,
                            0,
                            1,
                        ));
                    }
                    let epoch = proof.scope.epoch;
                    Ok(Self::new_inner(
                        CacheProfile::Pooled,
                        EffectiveCacheMode::PooledActive,
                        None,
                        Some(proof.scope),
                        epoch,
                        0,
                    ))
                }
            },
        }
    }

    fn new_inner(
        configured_profile: CacheProfile,
        effective_mode: EffectiveCacheMode,
        reason: Option<String>,
        scope: Option<CacheScope>,
        epoch: u64,
        peer_refusals: u64,
    ) -> Self {
        Self {
            configured_profile,
            effective_mode: Mutex::new(effective_mode),
            reason: Mutex::new(reason),
            scope,
            epoch: AtomicU64::new(epoch),
            generation_changes: AtomicU64::new(u64::from(epoch > 0)),
            peer_refusals: AtomicU64::new(peer_refusals),
        }
    }

    /// Pass 2 DT-7 invariant: a Pooled guest NEVER launches an independent GPU worker.
    pub fn spawns_independent_worker(&self) -> bool {
        self.configured_profile == CacheProfile::Independent
    }

    pub fn active_epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    pub fn scope(&self) -> Option<&CacheScope> {
        self.scope.as_ref()
    }

    pub fn retire_to_origin_only(&self, reason: &str) {
        let prev = self.epoch.swap(0, Ordering::AcqRel);
        if prev != 0 {
            self.generation_changes.fetch_add(1, Ordering::AcqRel);
        }
        let _ = self.effective_mode.lock().map(|mut mode| {
            if *mode == EffectiveCacheMode::PooledActive {
                *mode = EffectiveCacheMode::PooledOriginOnly;
            }
        });
        let _ = self
            .reason
            .lock()
            .map(|mut slot| *slot = Some(reason.to_string()));
    }

    pub fn status(&self, telemetry: Option<PooledReadTelemetry>) -> PooledGuestStatus {
        let t = telemetry.unwrap_or_default();
        PooledGuestStatus {
            configured_profile: self.configured_profile,
            effective_mode: self
                .effective_mode
                .lock()
                .map(|m| *m)
                .unwrap_or(EffectiveCacheMode::PooledOriginOnly),
            reason: self.reason.lock().ok().and_then(|r| r.clone()),
            epoch: self.active_epoch(),
            generation_changes: self.generation_changes.load(Ordering::Acquire),
            hits: t.hits,
            misses: t.misses,
            deadline_overruns: t.deadline_overruns,
            peer_refusals: self.peer_refusals.load(Ordering::Acquire) + t.poison_retirements,
            origin_fallback_bytes: t.origin_fallbacks.saturating_mul(POOLED_PAGE_BYTES as u64),
        }
    }
}

/// Bidirectional byte stream used by [`StreamPooledClient`].
pub trait CacheByteStream: Read + Write + Send {}
impl<T: Read + Write + Send> CacheByteStream for T {}

struct StreamClientInner {
    stream: Box<dyn CacheByteStream>,
    wbuf: CacheFrameBuffer,
    rbuf: CacheFrameBuffer,
    next_corr: u64,
}

/// Bounded wire client implementing [`PooledCacheClient`] over an authenticated stream.
pub struct StreamPooledClient {
    scope: CacheScope,
    closed: AtomicBool,
    inner: Mutex<StreamClientInner>,
}

impl StreamPooledClient {
    pub fn new(scope: CacheScope, stream: impl CacheByteStream + 'static) -> Self {
        Self {
            scope,
            closed: AtomicBool::new(false),
            inner: Mutex::new(StreamClientInner {
                stream: Box::new(stream),
                wbuf: CacheFrameBuffer::new(),
                rbuf: CacheFrameBuffer::new(),
                next_corr: 1,
            }),
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

impl PooledCacheClient for StreamPooledClient {
    fn lookup_into(
        &self,
        epoch: u64,
        offset: u64,
        content_gen: u64,
        slot_gen: u64,
        deadline: Instant,
        dst: &mut [u8; POOLED_PAGE_BYTES],
    ) -> PooledLookupReply {
        if self.closed.load(Ordering::Acquire) || Instant::now() >= deadline {
            return PooledLookupReply::TimedOut;
        }
        let Ok(mut guard) = self.inner.try_lock() else {
            return PooledLookupReply::Busy;
        };
        let corr = guard.next_corr;
        let Ok(req) = CacheFrameHeader::new(CacheOpcode::Lookup, corr, epoch, offset, content_gen)
        else {
            return PooledLookupReply::TransportError;
        };
        guard.next_corr = corr.saturating_add(1);
        let inner = &mut *guard;
        if let Err(err) = write_frame_with(&mut inner.stream, &mut inner.wbuf, req, None) {
            eprintln!("[ramsharedd] pooled lookup write failed: {err}");
            self.closed.store(true, Ordering::Release);
            return PooledLookupReply::TransportError;
        }
        let reply = match read_into(&mut inner.stream, &mut inner.rbuf) {
            Ok(reply) => reply,
            Err(err) => {
                eprintln!("[ramsharedd] pooled lookup read failed: {err}");
                self.closed.store(true, Ordering::Release);
                return PooledLookupReply::TransportError;
            }
        };
        if let Err(err) = self.scope.admit_reply(&req, &reply.header) {
            eprintln!("[ramsharedd] pooled lookup reply refused: {err}");
            self.closed.store(true, Ordering::Release);
            return PooledLookupReply::TransportError;
        }
        match (reply.header.opcode, reply.page, reply.crc) {
            (CacheOpcode::Hit, Some(page), Some(crc)) => {
                dst.copy_from_slice(page);
                PooledLookupReply::Hit {
                    epoch: reply.header.epoch,
                    slot_gen,
                    content_gen: reply.header.content_gen,
                    crc,
                }
            }
            (CacheOpcode::Miss, _, _) => PooledLookupReply::Miss,
            _ => PooledLookupReply::Busy,
        }
    }

    fn offer_copy(
        &self,
        epoch: u64,
        offset: u64,
        content_gen: u64,
        page: &[u8; POOLED_PAGE_BYTES],
        _crc: u32,
    ) -> bool {
        if self.closed.load(Ordering::Acquire) {
            return false;
        }
        let Ok(mut guard) = self.inner.try_lock() else {
            return false;
        };
        let corr = guard.next_corr;
        let Ok(req) = CacheFrameHeader::new(CacheOpcode::Offer, corr, epoch, offset, content_gen)
        else {
            return false;
        };
        guard.next_corr = corr.saturating_add(1);
        let inner = &mut *guard;
        if write_frame_with(&mut inner.stream, &mut inner.wbuf, req, Some(page)).is_err() {
            self.closed.store(true, Ordering::Release);
            return false;
        }
        match read_into(&mut inner.stream, &mut inner.rbuf) {
            Ok(reply)
                if self.scope.admit_reply(&req, &reply.header).is_ok()
                    && reply.header.opcode == CacheOpcode::Accepted =>
            {
                true
            }
            Ok(reply) if self.scope.admit_reply(&req, &reply.header).is_ok() => false,
            _ => {
                self.closed.store(true, Ordering::Release);
                false
            }
        }
    }

    fn close_poisoned(&self, _epoch: u64) {
        self.closed.store(true, Ordering::Release);
    }
}
