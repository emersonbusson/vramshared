// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/isolated_origin/cache_client.rs
//! Traits, transport framing, and fail-closed client adapters for isolated caches.

use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::time::{Duration, Instant};

use crate::origin_cache::CacheState;
use ramshared_vram::{
    GpuAdapterIdentity, GpuBudgetTelemetry, GpuUtilizationTelemetry, WorkerCacheTelemetry,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheRead {
    Hit,
    Miss,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheMutation {
    /// The cache implementation accepted the mutation. For the process-isolated
    /// GPU cache this means the complete frame was queued to the worker; it does
    /// not prove that the worker applied it or allocated VRAM. Origin data stays
    /// authoritative regardless of this result.
    Accepted,
    Skipped,
    Failed,
}

/// What a cache-release control request observed at the isolation boundary.
///
/// Neither arm carries a byte count. The isolated client reports the control
/// frame's acknowledgement and nothing else, so **how much** was released is
/// not observed across this boundary. Returning a placeholder `0` for that
/// missing figure — as [`AuthoritativeOriginBackend::release_cache`] used to —
/// let daemon log faces publish "reclaimed 0 MiB" as a measurement nobody
/// took. An absent figure is absence, not a measured zero.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheReleaseOutcome {
    /// A revoke was sent and the worker acknowledged it. The cache is off.
    Revoked,
    /// The cache was already released; nothing was sent and nothing changed.
    AlreadyReleased,
}

/// Cache-side data messages never carry the origin file handle.
#[derive(Debug)]
pub enum IsolatedCacheRequest {
    Read {
        offset: u64,
        len: usize,
        reply: SyncSender<Result<Option<Vec<u8>>, String>>,
    },
    Update {
        offset: u64,
        data: Vec<u8>,
    },
    Promote {
        offset: u64,
        data: Vec<u8>,
    },
}

/// Revocation uses a dedicated lane so a saturated data queue cannot conceal
/// release. The worker acknowledges only after its cache resources are gone.
#[derive(Debug)]
pub enum IsolatedCacheControl {
    Disable {
        reply: SyncSender<Result<(), String>>,
    },
}

pub struct IsolatedCacheWorker {
    pub requests: Receiver<IsolatedCacheRequest>,
    pub control: Receiver<IsolatedCacheControl>,
}

/// Largest payload a single `BestEffortCache::update` / `promote` call carries.
///
/// The GPU cache transport frames each mutation as one nonblocking send, so the
/// origin write path must split any larger write into consecutive slices of at
/// most this size. A block-layer request can be far larger (the NBD queue on
/// this surface admits up to 4 MiB), and forwarding one of those whole would
/// permanently revoke the cache on the first frame that does not fit.
pub const MAX_CACHE_MUTATION_BYTES: usize = 64 * 1024;

/// Split a logical cache mutation into consecutive transport-sized frames.
///
/// Yields `(offset, slice)` pairs covering `data` exactly, each slice at most
/// `MAX_CACHE_MUTATION_BYTES`. Empty `data` yields nothing: a zero-length
/// mutation is a no-op and must not occupy a frame.
pub(crate) fn mutation_frames(offset: u64, data: &[u8]) -> impl Iterator<Item = (u64, &[u8])> {
    data.chunks(MAX_CACHE_MUTATION_BYTES)
        .enumerate()
        .map(move |(index, slice)| {
            let slice_offset = offset
                .saturating_add((index as u64).saturating_mul(MAX_CACHE_MUTATION_BYTES as u64));
            (slice_offset, slice)
        })
}

pub trait BestEffortCache {
    fn read(&mut self, offset: u64, destination: &mut [u8]) -> CacheRead;
    fn update(&mut self, offset: u64, data: &[u8]) -> CacheMutation;
    fn promote(&mut self, offset: u64, data: &[u8]) -> CacheMutation;
    fn disable(&mut self) -> CacheMutation;
    fn state(&self) -> CacheState;

    fn cached_bytes(&self) -> u64 {
        0
    }

    fn refresh_cached_bytes(&mut self) -> Result<u64, &'static str> {
        Ok(self.cached_bytes())
    }

    fn target_bytes(&self) -> u64 {
        0
    }

    /// Static adapter ownership, independent of current budget freshness.
    fn bound_adapter_identity(&self) -> Option<GpuAdapterIdentity> {
        None
    }

    fn gpu_budget_telemetry(&self) -> Option<GpuBudgetTelemetry> {
        None
    }

    /// Adapter busy time from the same heartbeat as the budget.
    ///
    /// `None` is "not measured" — never a zero-percent idle sample. A provider
    /// without a utilization source answers here exactly as a driver that
    /// refused the call does.
    fn gpu_utilization_telemetry(&self) -> Option<GpuUtilizationTelemetry> {
        None
    }

    /// Cache occupancy and codec health from the last heartbeat (DT-8).
    ///
    /// Logical cache bytes are cache occupancy in the worker's address space —
    /// never guest or host RAM.
    fn cache_telemetry(&self) -> Option<WorkerCacheTelemetry> {
        None
    }
}

/// A composed operation supplies one deadline for all of its cache members.
/// Implementations may shorten it but never restart or extend that deadline.
pub trait BoundedCache: BestEffortCache {
    fn read_until(&mut self, offset: u64, destination: &mut [u8], deadline: Instant) -> CacheRead;
    fn disable_until(&mut self, deadline: Instant) -> CacheMutation;
    fn refresh_until(&mut self, deadline: Instant) -> Result<u64, &'static str>;
}

/// Fail-closed cache used until a separately supervised GPU worker is wired.
#[derive(Default)]
pub struct DisabledCache;

impl BestEffortCache for DisabledCache {
    fn read(&mut self, _offset: u64, _destination: &mut [u8]) -> CacheRead {
        CacheRead::Miss
    }

    fn update(&mut self, _offset: u64, _data: &[u8]) -> CacheMutation {
        CacheMutation::Skipped
    }

    fn promote(&mut self, _offset: u64, _data: &[u8]) -> CacheMutation {
        CacheMutation::Skipped
    }

    fn disable(&mut self) -> CacheMutation {
        // A permanently disabled cache has nothing to revoke (SPEC
        // swap-drain-safety RF-9). `Accepted` here made every control-pressure
        // pass report a fresh revocation and count one in `telemetry.releases`
        // — 443,800 log lines from the loop that serves swap. `Skipped` is
        // the honest outcome: nothing happened.
        CacheMutation::Skipped
    }

    fn state(&self) -> CacheState {
        CacheState::Unavailable
    }
}

/// Bounded client for an isolated cache worker. It never performs a blocking
/// send. Reads and release acknowledgements wait for `read_timeout` at most.
pub struct BoundedCacheClient {
    requests: SyncSender<IsolatedCacheRequest>,
    control: SyncSender<IsolatedCacheControl>,
    read_timeout: Duration,
    state: CacheState,
}

pub fn isolated_cache_channel(
    capacity: usize,
    read_timeout: Duration,
) -> (BoundedCacheClient, IsolatedCacheWorker) {
    let (requests, receiver) = sync_channel(capacity);
    let (control, control_receiver) = sync_channel(1);
    (
        BoundedCacheClient {
            requests,
            control,
            read_timeout,
            state: CacheState::Active,
        },
        IsolatedCacheWorker {
            requests: receiver,
            control: control_receiver,
        },
    )
}

impl BoundedCacheClient {
    fn fail(&mut self) -> CacheMutation {
        self.state = CacheState::Unavailable;
        CacheMutation::Failed
    }

    fn send_mutation(&mut self, request: IsolatedCacheRequest) -> CacheMutation {
        if self.state != CacheState::Active {
            return CacheMutation::Skipped;
        }
        match self.requests.try_send(request) {
            Ok(()) => CacheMutation::Accepted,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => self.fail(),
        }
    }
}

impl BestEffortCache for BoundedCacheClient {
    fn read(&mut self, offset: u64, destination: &mut [u8]) -> CacheRead {
        if self.state != CacheState::Active {
            return CacheRead::Miss;
        }
        let (reply, response) = sync_channel(1);
        let request = IsolatedCacheRequest::Read {
            offset,
            len: destination.len(),
            reply,
        };
        if self.requests.try_send(request).is_err() {
            self.fail();
            return CacheRead::Failed;
        }
        match response.recv_timeout(self.read_timeout) {
            Ok(Ok(Some(bytes))) if bytes.len() == destination.len() => {
                destination.copy_from_slice(&bytes);
                CacheRead::Hit
            }
            Ok(Ok(None)) => CacheRead::Miss,
            Ok(Ok(Some(_)))
            | Ok(Err(_))
            | Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                self.fail();
                CacheRead::Failed
            }
        }
    }

    fn update(&mut self, offset: u64, data: &[u8]) -> CacheMutation {
        self.send_mutation(IsolatedCacheRequest::Update {
            offset,
            data: data.to_vec(),
        })
    }

    fn promote(&mut self, offset: u64, data: &[u8]) -> CacheMutation {
        self.send_mutation(IsolatedCacheRequest::Promote {
            offset,
            data: data.to_vec(),
        })
    }

    fn disable(&mut self) -> CacheMutation {
        if self.state == CacheState::Off {
            return CacheMutation::Skipped;
        }
        let (reply, acknowledgement) = sync_channel(1);
        if self
            .control
            .try_send(IsolatedCacheControl::Disable { reply })
            .is_err()
        {
            self.state = CacheState::Stuck;
            return CacheMutation::Failed;
        }
        match acknowledgement.recv_timeout(self.read_timeout) {
            Ok(Ok(())) => {
                self.state = CacheState::Off;
                CacheMutation::Accepted
            }
            Ok(Err(_))
            | Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                self.state = CacheState::Stuck;
                CacheMutation::Failed
            }
        }
    }

    fn state(&self) -> CacheState {
        self.state
    }
}

/// Cache admission authority (SPEC `native-vsock-host-guest-control-plane`
/// ITEM-3, DT-9).
///
/// The backend consults this before it serves from, populates, or mirrors
/// into the cache. The production implementation is the daemon's
/// `ControlPlaneAuthority`: cache admission requires a live vsock lease and
/// a verified origin, so a host with no vsock peer is `origin_only` and its
/// cache is refused. Tests and lab callers opt in with
/// [`AllowCacheAdmission`].
pub trait CacheAdmission: Send + Sync {
    /// `Ok` when the cache may be used right now.
    fn admit_cache(&self) -> Result<(), String>;

    /// True while the authority has **not granted a lease yet** and the
    /// refusal is only "the peer has not arrived".
    ///
    /// A pre-lease refusal is not a revocation: there is nothing that could
    /// serve, and the cache must be able to serve as soon as the lease
    /// arrives. Revoking on this refusal turned the cache `Off` — a state the
    /// product never recovers — so a host whose peer connected a few seconds
    /// late kept a permanently empty cache.
    fn cache_revocation_pending(&self) -> bool {
        false
    }
}

/// Always admits: the test and lab authority.
pub struct AllowCacheAdmission;

impl CacheAdmission for AllowCacheAdmission {
    fn admit_cache(&self) -> Result<(), String> {
        Ok(())
    }
}

/// Always refuses: the fail-closed default of [`AuthoritativeOriginBackend::new`].
///
/// A caller that does not supply an authority cannot accidentally populate a
/// cache DT-9 says requires a lease; it must opt in explicitly.
pub struct RefuseCacheAdmission;

impl CacheAdmission for RefuseCacheAdmission {
    fn admit_cache(&self) -> Result<(), String> {
        Err("cache admission requires a supplied authority (DT-9)".into())
    }
}
