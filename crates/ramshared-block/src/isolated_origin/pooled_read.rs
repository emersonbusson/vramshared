// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/isolated_origin/pooled_read.rs
//! Locally fenced optional pooled read cache over an authoritative SSD origin.
//! SPEC: the protocol specification §DT-5, §DT-6, §DT-9.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::range_lock::{RangeLocks, STRIPES};

pub const POOLED_PAGE_BYTES: usize = 4096;
pub const DEFAULT_POOLED_LOOKUP_BUDGET: Duration = Duration::from_millis(2);
pub const POOLED_STAGING_SLOTS: usize = 2;

/// Deterministic Castagnoli CRC32C matching `ramshared_ipc::cache_wire::crc32c`.
#[must_use]
pub fn pooled_crc32c(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0x82F6_3B78 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PooledLookupReply {
    Hit {
        epoch: u64,
        slot_gen: u64,
        content_gen: u64,
        crc: u32,
    },
    Miss,
    Busy,
    TimedOut,
    TransportError,
}

/// Bounded remote lookup/offer transport plugged into [`PooledReadState`].
pub trait PooledCacheClient: Send + Sync {
    fn lookup_into(
        &self,
        epoch: u64,
        offset: u64,
        content_gen: u64,
        slot_gen: u64,
        deadline: Instant,
        dst: &mut [u8; POOLED_PAGE_BYTES],
    ) -> PooledLookupReply;

    fn offer_copy(
        &self,
        epoch: u64,
        offset: u64,
        content_gen: u64,
        page: &[u8; POOLED_PAGE_BYTES],
        crc: u32,
    ) -> bool;

    fn close_poisoned(&self, epoch: u64);
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PooledReadTelemetry {
    pub hits: u64,
    pub misses: u64,
    pub busy_skips: u64,
    pub deadline_overruns: u64,
    pub stale_hit_refusals: u64,
    pub poison_retirements: u64,
    pub offers_submitted: u64,
    pub offers_dropped: u64,
    pub origin_fallbacks: u64,
    pub quarantined_slots: u64,
}

struct StagingSlot {
    in_use: bool,
    quarantined: bool,
    slot_gen: u64,
    buf: Option<Box<[u8; POOLED_PAGE_BYTES]>>,
}

impl StagingSlot {
    fn new() -> Self {
        Self {
            in_use: false,
            quarantined: false,
            slot_gen: 0,
            buf: Some(Box::new([0u8; POOLED_PAGE_BYTES])),
        }
    }
}

struct StagingPool {
    lookup: [StagingSlot; POOLED_STAGING_SLOTS],
    offer: [StagingSlot; POOLED_STAGING_SLOTS],
    telemetry: PooledReadTelemetry,
}

impl StagingPool {
    fn new() -> Self {
        Self {
            lookup: std::array::from_fn(|_| StagingSlot::new()),
            offer: std::array::from_fn(|_| StagingSlot::new()),
            telemetry: PooledReadTelemetry::default(),
        }
    }

    fn try_acquire(
        slots: &mut [StagingSlot; POOLED_STAGING_SLOTS],
    ) -> Option<(usize, u64, Box<[u8; POOLED_PAGE_BYTES]>)> {
        for (idx, slot) in slots.iter_mut().enumerate() {
            if !slot.in_use
                && !slot.quarantined
                && let Some(buf) = slot.buf.take()
            {
                slot.in_use = true;
                slot.slot_gen = slot.slot_gen.saturating_add(1);
                return Some((idx, slot.slot_gen, buf));
            }
        }
        None
    }
}

pub(super) struct LookupAttempt {
    slot_idx: usize,
    slot_gen: u64,
    deadline: Instant,
    reply: PooledLookupReply,
    buf: Box<[u8; POOLED_PAGE_BYTES]>,
}

pub struct PooledReadState {
    epoch: AtomicU64,
    stripes: [AtomicU64; STRIPES],
    lookup_budget: Duration,
    client: Box<dyn PooledCacheClient>,
    pool: Mutex<StagingPool>,
}

impl PooledReadState {
    pub fn new(epoch: u64, client: impl PooledCacheClient + 'static) -> Self {
        Self::with_budget(epoch, DEFAULT_POOLED_LOOKUP_BUDGET, client)
    }

    pub fn with_budget(
        epoch: u64,
        lookup_budget: Duration,
        client: impl PooledCacheClient + 'static,
    ) -> Self {
        Self {
            epoch: AtomicU64::new(epoch),
            stripes: std::array::from_fn(|_| AtomicU64::new(1)),
            lookup_budget,
            client: Box::new(client),
            pool: Mutex::new(StagingPool::new()),
        }
    }

    pub fn active_epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    fn with_pool<R>(&self, f: impl FnOnce(&mut StagingPool) -> R) -> Option<R> {
        self.pool.lock().ok().map(|mut p| f(&mut p))
    }

    pub fn telemetry(&self) -> PooledReadTelemetry {
        self.with_pool(|p| p.telemetry).unwrap_or_default()
    }

    pub fn retire_epoch(&self) {
        let prev = self.epoch.swap(0, Ordering::AcqRel);
        if prev != 0 {
            let _ = self.with_pool(|pool| {
                pool.telemetry.poison_retirements =
                    pool.telemetry.poison_retirements.saturating_add(1);
            });
            self.client.close_poisoned(prev);
        }
    }

    /// Advance all touched stripe generations under the write range guard before origin I/O.
    pub fn advance_written_stripes(&self, offset: u64, len: usize) {
        for (stripe, touched) in self.stripes.iter().zip(RangeLocks::touched(offset, len)) {
            if !touched {
                continue;
            }
            let current = stripe.load(Ordering::Acquire);
            match current.checked_add(1) {
                Some(next) => stripe.store(next, Ordering::Release),
                None => {
                    self.retire_epoch();
                    return;
                }
            }
        }
    }

    /// Snapshot `(epoch, content_gen)` for an eligible aligned 4 KiB read under range lock.
    pub fn snapshot(&self, offset: u64, len: usize) -> Option<(u64, u64)> {
        let epoch = self.active_epoch();
        if epoch == 0
            || len != POOLED_PAGE_BYTES
            || !offset.is_multiple_of(POOLED_PAGE_BYTES as u64)
        {
            return None;
        }
        let gen_val = self.stripes[RangeLocks::stripe_index(offset)].load(Ordering::Acquire);
        Some((epoch, gen_val))
    }

    pub(super) fn try_lookup_remote(
        &self,
        epoch: u64,
        offset: u64,
        content_gen: u64,
        inherited_deadline: Option<Instant>,
    ) -> Option<LookupAttempt> {
        let now = Instant::now();
        let deadline = now.checked_add(self.lookup_budget).unwrap_or(now);
        let deadline = inherited_deadline.map_or(deadline, |inh| deadline.min(inh));
        if now >= deadline {
            let _ = self.with_pool(|pool| {
                pool.telemetry.deadline_overruns =
                    pool.telemetry.deadline_overruns.saturating_add(1);
            });
            return None;
        }
        let acquired = self
            .with_pool(|pool| {
                let slot = StagingPool::try_acquire(&mut pool.lookup);
                if slot.is_none() {
                    pool.telemetry.busy_skips = pool.telemetry.busy_skips.saturating_add(1);
                }
                slot
            })
            .flatten()?;
        let (slot_idx, slot_gen, mut buf) = acquired;
        let reply =
            self.client
                .lookup_into(epoch, offset, content_gen, slot_gen, deadline, &mut buf);
        Some(LookupAttempt {
            slot_idx,
            slot_gen,
            deadline,
            reply,
            buf,
        })
    }

    /// Validate under reacquired range guard; copy into `destination` only if every fence passes.
    pub(super) fn validate_and_copy_hit(
        &self,
        attempt: LookupAttempt,
        offset: u64,
        snapshot_epoch: u64,
        snapshot_gen: u64,
        destination: &mut [u8],
    ) -> bool {
        let mut accepted = false;
        let mut poison = false;
        let timed_out = matches!(attempt.reply, PooledLookupReply::TimedOut);
        let deadline_exceeded = Instant::now() > attempt.deadline;
        match attempt.reply {
            PooledLookupReply::Hit {
                epoch,
                slot_gen,
                content_gen,
                crc,
            } => {
                let current_epoch = self.active_epoch();
                let current_gen =
                    self.stripes[RangeLocks::stripe_index(offset)].load(Ordering::Acquire);
                let computed_crc = pooled_crc32c(&attempt.buf[..]);
                if epoch != snapshot_epoch || slot_gen != attempt.slot_gen || computed_crc != crc {
                    poison = true;
                } else if deadline_exceeded {
                    let _ = self.with_pool(|pool| {
                        pool.telemetry.deadline_overruns =
                            pool.telemetry.deadline_overruns.saturating_add(1);
                    });
                } else if current_epoch != snapshot_epoch
                    || current_gen != snapshot_gen
                    || content_gen != snapshot_gen
                    || destination.len() != POOLED_PAGE_BYTES
                {
                    let _ = self.with_pool(|pool| {
                        pool.telemetry.stale_hit_refusals =
                            pool.telemetry.stale_hit_refusals.saturating_add(1);
                    });
                } else {
                    destination.copy_from_slice(&attempt.buf[..]);
                    accepted = true;
                }
            }
            PooledLookupReply::Miss => {
                let _ = self.with_pool(|pool| {
                    pool.telemetry.misses = pool.telemetry.misses.saturating_add(1);
                });
            }
            PooledLookupReply::Busy => {
                let _ = self.with_pool(|pool| {
                    pool.telemetry.busy_skips = pool.telemetry.busy_skips.saturating_add(1);
                });
            }
            PooledLookupReply::TimedOut => {
                let _ = self.with_pool(|pool| {
                    pool.telemetry.deadline_overruns =
                        pool.telemetry.deadline_overruns.saturating_add(1);
                });
            }
            PooledLookupReply::TransportError => poison = true,
        }
        let _ = self.with_pool(|pool| {
            let slot = &mut pool.lookup[attempt.slot_idx];
            slot.buf = Some(attempt.buf);
            slot.in_use = false;
            if timed_out {
                slot.quarantined = true;
                pool.telemetry.quarantined_slots =
                    pool.telemetry.quarantined_slots.saturating_add(1);
            }
            if accepted {
                pool.telemetry.hits = pool.telemetry.hits.saturating_add(1);
            }
        });
        if poison {
            self.retire_epoch();
        }
        accepted
    }

    pub(super) fn record_origin_fallback(&self) {
        let _ = self.with_pool(|pool| {
            pool.telemetry.origin_fallbacks = pool.telemetry.origin_fallbacks.saturating_add(1);
        });
    }

    pub(super) fn offer_after_origin_read(
        &self,
        epoch: u64,
        offset: u64,
        content_gen: u64,
        data: &[u8],
    ) {
        if self.active_epoch() != epoch || data.len() != POOLED_PAGE_BYTES {
            return;
        }
        let acquired = self
            .with_pool(|pool| {
                let slot = StagingPool::try_acquire(&mut pool.offer);
                if slot.is_none() {
                    pool.telemetry.offers_dropped = pool.telemetry.offers_dropped.saturating_add(1);
                }
                slot
            })
            .flatten();
        let Some((slot_idx, _, mut buf)) = acquired else {
            return;
        };
        buf.copy_from_slice(data);
        let crc = pooled_crc32c(&buf[..]);
        let submitted = self
            .client
            .offer_copy(epoch, offset, content_gen, &buf, crc);
        let _ = self.with_pool(|pool| {
            let slot = &mut pool.offer[slot_idx];
            slot.buf = Some(buf);
            slot.in_use = false;
            if submitted {
                pool.telemetry.offers_submitted = pool.telemetry.offers_submitted.saturating_add(1);
            } else {
                pool.telemetry.offers_dropped = pool.telemetry.offers_dropped.saturating_add(1);
            }
        });
    }

    /// Release a previously quarantined lookup slot only after the caller verified settlement.
    pub fn settle_quarantined_slot(&self, slot_idx: usize) -> bool {
        self.with_pool(|pool| {
            let Some(slot) = pool.lookup.get_mut(slot_idx) else {
                return false;
            };
            if !slot.quarantined {
                return false;
            }
            slot.quarantined = false;
            pool.telemetry.quarantined_slots = pool.telemetry.quarantined_slots.saturating_sub(1);
            true
        })
        .unwrap_or(false)
    }
}
