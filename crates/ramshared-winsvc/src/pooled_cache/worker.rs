// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/pooled_cache/worker.rs
//! Isolated host GPU cache worker slot custody, generation fencing, and quarantine.
//! SPEC: the protocol specification §DT-7, §DT-9.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use ramshared_ipc::cache_wire::{CACHE_PAGE_BYTES, CacheGuestRole, crc32c};

pub const WATCHDOG_RETIRE_BOUND: Duration = Duration::from_millis(100);
pub const WATCHDOG_DRAIN_BOUND: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct WorkerSlotKey {
    pub role: CacheGuestRole,
    pub epoch: u64,
    pub offset_bytes: u64,
    pub content_gen: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerFault {
    TimedOut,
    DriverHung,
    CorruptPayload,
    Quarantined,
    CapacityExhausted,
}

/// Owned GPU worker process boundary (real CUDA worker process or deterministic test harness).
pub trait GpuWorkerBackend {
    fn store_slot(&mut self, slot: usize, page: &[u8; CACHE_PAGE_BYTES])
    -> Result<(), WorkerFault>;
    fn load_slot(
        &mut self,
        slot: usize,
        dst: &mut [u8; CACHE_PAGE_BYTES],
    ) -> Result<(), WorkerFault>;
    fn scrub_slot(&mut self, slot: usize) -> Result<(), WorkerFault>;
    fn terminate_process(&mut self);
    fn context_destroyed(&self) -> bool;
}

#[derive(Clone, Debug)]
struct WorkerSlot {
    key: Option<WorkerSlotKey>,
    slot_gen: u64,
    crc: u32,
    needs_scrub: bool,
}

impl WorkerSlot {
    const fn empty() -> Self {
        Self {
            key: None,
            slot_gen: 0,
            crc: 0,
            needs_scrub: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreOutcome {
    StoredNew { slot_gen: u64 },
    ReusedExisting { slot_gen: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetireSummary {
    pub logically_fenced_bytes: u64,
    pub physically_reclaimed_bytes: u64,
    pub pending_scrub_bytes: u64,
}

pub struct OwnedGpuWorker<B: GpuWorkerBackend> {
    backend: B,
    slots: Vec<WorkerSlot>,
    index: HashMap<WorkerSlotKey, usize>,
    next_free_hint: usize,
    quarantined: bool,
    quarantined_since: Option<Instant>,
    active_process: bool,
}

impl<B: GpuWorkerBackend> OwnedGpuWorker<B> {
    pub fn new(backend: B, slot_capacity: usize) -> Self {
        Self {
            backend,
            slots: vec![WorkerSlot::empty(); slot_capacity],
            index: HashMap::with_capacity(slot_capacity),
            next_free_hint: 0,
            quarantined: false,
            quarantined_since: None,
            active_process: true,
        }
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    pub fn is_quarantined(&self) -> bool {
        self.quarantined
    }

    pub fn is_process_active(&self) -> bool {
        self.active_process
    }

    pub fn resident_bytes_for(&self, role: CacheGuestRole) -> u64 {
        let count = self
            .slots
            .iter()
            .filter(|s| s.key.is_some_and(|k| k.role == role))
            .count() as u64;
        count.saturating_mul(CACHE_PAGE_BYTES as u64)
    }

    pub fn quarantined_elapsed(&self, now: Instant) -> Option<Duration> {
        self.quarantined_since
            .map(|since| now.saturating_duration_since(since))
    }

    /// Quarantine the worker on timeout/hang; credits stay charged until context exit (DT-9).
    pub fn quarantine(&mut self, now: Instant) {
        if !self.quarantined {
            self.quarantined = true;
            self.quarantined_since = Some(now);
            self.index.clear();
            self.backend.terminate_process();
        }
    }

    /// Reap quarantined worker ONLY once the owned adapter context is proven destroyed.
    pub fn poll_quarantine_settlement(&mut self, now: Instant) -> bool {
        if !self.quarantined {
            return false;
        }
        let _elapsed = self.quarantined_elapsed(now);
        if !self.backend.context_destroyed() {
            return false;
        }
        for slot in &mut self.slots {
            slot.key = None;
            slot.needs_scrub = false;
        }
        self.index.clear();
        self.active_process = false;
        true
    }

    /// Publish an offered page under its authenticated `(role, epoch, offset, content_gen)` key.
    pub fn publish_offer(
        &mut self,
        key: WorkerSlotKey,
        page: &[u8; CACHE_PAGE_BYTES],
        expected_crc: u32,
        now: Instant,
    ) -> Result<StoreOutcome, WorkerFault> {
        if self.quarantined || !self.active_process {
            return Err(WorkerFault::Quarantined);
        }
        if crc32c(page) != expected_crc {
            return Err(WorkerFault::CorruptPayload);
        }
        if let Some(&existing) = self.index.get(&key) {
            return Ok(StoreOutcome::ReusedExisting {
                slot_gen: self.slots[existing].slot_gen,
            });
        }
        let Some(slot_idx) = self.find_free_clean_slot(now)? else {
            return Err(WorkerFault::CapacityExhausted);
        };
        if let Err(fault) = self.backend.store_slot(slot_idx, page) {
            if matches!(fault, WorkerFault::TimedOut | WorkerFault::DriverHung) {
                self.quarantine(now);
            }
            return Err(fault);
        }
        let slot = &mut self.slots[slot_idx];
        slot.slot_gen = slot.slot_gen.saturating_add(1);
        slot.key = Some(key);
        slot.crc = expected_crc;
        slot.needs_scrub = false;
        self.index.insert(key, slot_idx);
        Ok(StoreOutcome::StoredNew {
            slot_gen: slot.slot_gen,
        })
    }

    /// Read a page matching the exact authenticated key and verify CRC32C before returning.
    pub fn lookup_page(
        &mut self,
        key: WorkerSlotKey,
        dst: &mut [u8; CACHE_PAGE_BYTES],
        now: Instant,
    ) -> Result<Option<(u64, u32)>, WorkerFault> {
        if self.quarantined || !self.active_process {
            return Err(WorkerFault::Quarantined);
        }
        let Some(&slot_idx) = self.index.get(&key) else {
            return Ok(None);
        };
        let (slot_gen, expected_crc) = (self.slots[slot_idx].slot_gen, self.slots[slot_idx].crc);
        if let Err(fault) = self.backend.load_slot(slot_idx, dst) {
            if matches!(fault, WorkerFault::TimedOut | WorkerFault::DriverHung) {
                self.quarantine(now);
            }
            return Err(fault);
        }
        if crc32c(dst) != expected_crc {
            // Poison-pill invalidation before fallback (KRYPTON-15 / DT-9).
            self.index.remove(&key);
            self.slots[slot_idx].key = None;
            self.slots[slot_idx].needs_scrub = true;
            return Err(WorkerFault::CorruptPayload);
        }
        Ok(Some((slot_gen, expected_crc)))
    }

    /// Fence every slot belonging to `(role, epoch)` and scrub before physical reclamation.
    pub fn retire_scope(
        &mut self,
        role: CacheGuestRole,
        epoch: Option<u64>,
        now: Instant,
    ) -> RetireSummary {
        let mut summary = RetireSummary {
            logically_fenced_bytes: 0,
            physically_reclaimed_bytes: 0,
            pending_scrub_bytes: 0,
        };
        for idx in 0..self.slots.len() {
            let Some(key) = self.slots[idx].key else {
                continue;
            };
            if key.role != role || epoch.is_some_and(|e| key.epoch != e) {
                continue;
            }
            self.index.remove(&key);
            self.slots[idx].key = None;
            self.slots[idx].needs_scrub = true;
            summary.logically_fenced_bytes = summary
                .logically_fenced_bytes
                .saturating_add(CACHE_PAGE_BYTES as u64);
            if self.try_scrub_slot(idx, now) {
                summary.physically_reclaimed_bytes = summary
                    .physically_reclaimed_bytes
                    .saturating_add(CACHE_PAGE_BYTES as u64);
            } else {
                summary.pending_scrub_bytes = summary
                    .pending_scrub_bytes
                    .saturating_add(CACHE_PAGE_BYTES as u64);
            }
        }
        summary
    }

    /// Reclaim up to `target_bytes` of lower-priority guest slots for WSL2 demand (DT-8).
    pub fn evict_role_bytes(
        &mut self,
        role: CacheGuestRole,
        target_bytes: u64,
        now: Instant,
    ) -> u64 {
        let mut reclaimed = 0u64;
        for idx in 0..self.slots.len() {
            if reclaimed >= target_bytes || self.quarantined {
                break;
            }
            let Some(key) = self.slots[idx].key else {
                continue;
            };
            if key.role != role {
                continue;
            }
            self.index.remove(&key);
            self.slots[idx].key = None;
            self.slots[idx].needs_scrub = true;
            if self.try_scrub_slot(idx, now) {
                reclaimed = reclaimed.saturating_add(CACHE_PAGE_BYTES as u64);
            }
        }
        reclaimed
    }

    fn try_scrub_slot(&mut self, idx: usize, now: Instant) -> bool {
        if self.quarantined {
            return false;
        }
        match self.backend.scrub_slot(idx) {
            Ok(()) => {
                self.slots[idx].needs_scrub = false;
                true
            }
            Err(WorkerFault::TimedOut | WorkerFault::DriverHung) => {
                self.quarantine(now);
                false
            }
            Err(_) => false,
        }
    }

    fn find_free_clean_slot(&mut self, now: Instant) -> Result<Option<usize>, WorkerFault> {
        let len = self.slots.len();
        if len == 0 {
            return Ok(None);
        }
        let start = self.next_free_hint % len;
        for step in 0..len {
            let idx = (start + step) % len;
            if self.slots[idx].key.is_none() {
                if self.slots[idx].needs_scrub && !self.try_scrub_slot(idx, now) {
                    if self.quarantined {
                        return Err(WorkerFault::Quarantined);
                    }
                    continue;
                }
                self.next_free_hint = (idx + 1) % len;
                return Ok(Some(idx));
            }
        }
        Ok(None)
    }
}
