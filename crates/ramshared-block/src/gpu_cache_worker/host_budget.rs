// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/gpu_cache_worker/host_budget.rs
//! Cgroup- and host-memory-aware admission budget for GPU slab metadata.
//!
//! Replaces any fixed `1024`-frame (64 MiB) slab cap with a live host-RAM and
//! cgroup-v2 bound shared across concurrent GPU worker adapters.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ramshared_vram::VramProvider;
use ramshared_vram::device_codec::{GpuDeviceCodec, ReferenceLz4Codec};
use ramshared_vram::slab_pool::VramSlabPool;

use crate::gpu_worker_slab_bridge::{GpuWorkerSlabBridge, SlabBridgeConfig};

const MIB: u64 = 1024 * 1024;
const MIN_HOST_RESERVE_BYTES: u64 = 128 * MIB;
const MAX_HOST_RESERVE_BYTES: u64 = 512 * MIB;

/// Conservative startup metadata reservation per admitted 64 KiB GPU slab frame.
pub const STARTUP_HOST_BYTES_PER_FRAME: u64 = 1024;

/// Conservative per-cached-page host overhead across `slab_pages` (`BTreeMap`),
/// `slab_frame_pages` (`BTreeSet`), `slot_epochs`, and codec `slots` (`HashMap`).
pub const LIVE_PAGE_METADATA_BYTES: u64 = 160;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostMemoryObservation {
    pub total_host_bytes: u64,
    pub available_host_bytes: u64,
    pub cgroup_headroom_bytes: Option<u64>,
}

impl HostMemoryObservation {
    pub fn probe_live() -> Option<Self> {
        let meminfo = fs::read_to_string("/proc/meminfo").ok()?;
        let proc_cgroup = fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
        Self::from_proc_strings(&meminfo, &proc_cgroup, Path::new("/sys/fs/cgroup"))
    }

    pub fn from_proc_strings(meminfo: &str, proc_cgroup: &str, cgroup_root: &Path) -> Option<Self> {
        let total_host_bytes = parse_meminfo_kib(meminfo, "MemTotal")?.checked_mul(1024)?;
        let available_host_bytes = parse_meminfo_kib(meminfo, "MemAvailable")?.checked_mul(1024)?;
        if total_host_bytes == 0 || available_host_bytes > total_host_bytes {
            return None;
        }
        let cgroup_headroom_bytes = probe_cgroup_v2_headroom(proc_cgroup, cgroup_root);
        Some(Self {
            total_host_bytes,
            available_host_bytes,
            cgroup_headroom_bytes,
        })
    }

    /// Host-memory metadata ceiling before worker partitioning.
    pub fn total_metadata_ceiling_bytes(&self) -> u64 {
        let reserve = (self.total_host_bytes / 10)
            .clamp(MIN_HOST_RESERVE_BYTES, MAX_HOST_RESERVE_BYTES)
            .min(self.total_host_bytes / 2);
        let host_free = self.available_host_bytes.saturating_sub(reserve);
        let cgroup_free = self
            .cgroup_headroom_bytes
            .map_or(u64::MAX, |headroom| headroom / 2);
        host_free.min(cgroup_free) / 2
    }

    /// Per-worker ceiling given `worker_count` concurrent adapters.
    pub fn worker_metadata_ceiling_bytes(&self, worker_count: u32) -> u64 {
        self.total_metadata_ceiling_bytes() / u64::from(worker_count.max(1))
    }
}

#[derive(Debug, Default)]
struct LedgerState {
    reserved_by_worker: BTreeMap<u64, u64>,
    next_id: u64,
    fallback_ceiling_bytes: Option<u64>,
}

/// Shared host-metadata admission ledger coordinated across concurrent adapter workers.
#[derive(Clone, Debug, Default)]
pub struct SharedHostMetadataBudget {
    inner: Arc<Mutex<LedgerState>>,
}

impl SharedHostMetadataBudget {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fixed-observation ledger for deterministic scaling and exhaustion tests.
    pub fn with_ceiling_bytes(ceiling_bytes: u64) -> Self {
        Self {
            inner: Arc::new(Mutex::new(LedgerState {
                reserved_by_worker: BTreeMap::new(),
                next_id: 0,
                fallback_ceiling_bytes: Some(ceiling_bytes),
            })),
        }
    }

    pub fn allocate_worker_id(&self) -> Option<u64> {
        let mut state = self.inner.lock().ok()?;
        let id = state.next_id;
        state.next_id = id.checked_add(1)?;
        Some(id)
    }

    pub fn total_reserved_bytes(&self) -> u64 {
        self.inner
            .lock()
            .map(|state| {
                state
                    .reserved_by_worker
                    .values()
                    .copied()
                    .fold(0u64, u64::saturating_add)
            })
            .unwrap_or(0)
    }

    pub fn worker_reserved_bytes(&self, worker_id: u64) -> u64 {
        self.inner
            .lock()
            .ok()
            .and_then(|state| state.reserved_by_worker.get(&worker_id).copied())
            .unwrap_or(0)
    }

    pub fn release(&self, worker_id: u64) {
        if let Ok(mut state) = self.inner.lock() {
            state.reserved_by_worker.remove(&worker_id);
        }
    }

    /// Try to update this worker's host-metadata reservation to `desired_bytes`
    /// under the live ceiling (after subtracting other workers' active reservations).
    pub fn try_set_reservation(
        &self,
        worker_id: u64,
        desired_bytes: u64,
        ceiling_bytes: u64,
    ) -> bool {
        let Ok(mut state) = self.inner.lock() else {
            return false;
        };
        let total_cap = state.fallback_ceiling_bytes.unwrap_or(ceiling_bytes);
        let others: u64 = state
            .reserved_by_worker
            .iter()
            .filter(|(id, _)| **id != worker_id)
            .map(|(_, bytes)| *bytes)
            .fold(0u64, u64::saturating_add);
        if others.saturating_add(desired_bytes) > total_cap {
            return false;
        }
        state.reserved_by_worker.insert(worker_id, desired_bytes);
        true
    }
}

/// Active per-worker host-metadata admission and tracking state.
#[derive(Debug)]
pub(super) struct WorkerHostMetadataGuard {
    ledger: SharedHostMetadataBudget,
    worker_id: u64,
    limit_bytes: u64,
    override_observation: Option<HostMemoryObservation>,
}

impl WorkerHostMetadataGuard {
    pub(super) fn admit_frames(
        vram_usable_frames: u32,
        worker_count: u32,
        ledger: Option<SharedHostMetadataBudget>,
        override_observation: Option<HostMemoryObservation>,
    ) -> (u32, Self) {
        let ledger = ledger.unwrap_or_default();
        let worker_id = ledger.allocate_worker_id().unwrap_or(u64::MAX);
        let obs = override_observation.or_else(HostMemoryObservation::probe_live);
        let ceiling_total = obs.map_or(64 * MIB, |o| o.total_metadata_ceiling_bytes());
        let worker_limit = obs.map_or(64 * MIB, |o| o.worker_metadata_ceiling_bytes(worker_count));
        let host_cap_frames = worker_limit / STARTUP_HOST_BYTES_PER_FRAME.max(1);
        let mut admitted = u64::from(vram_usable_frames).min(host_cap_frames);
        let per_fixed = VramSlabPool::per_frame_fixed_metadata_bytes() as u64;
        while admitted > 0 {
            let fixed = admitted.saturating_mul(per_fixed);
            if ledger.try_set_reservation(worker_id, fixed, ceiling_total) {
                break;
            }
            admitted /= 2;
        }
        let frames = u32::try_from(admitted).unwrap_or(u32::MAX);
        let limit_bytes = frames_metadata_limit(frames, worker_limit);
        (
            frames,
            Self {
                ledger,
                worker_id,
                limit_bytes,
                override_observation,
            },
        )
    }

    pub(super) fn limit_bytes(&self) -> u64 {
        self.limit_bytes
    }

    pub(super) fn update_usage(&self, new_bytes: u64) -> bool {
        if new_bytes > self.limit_bytes {
            return false;
        }
        let total_cap = self
            .override_observation
            .or_else(HostMemoryObservation::probe_live)
            .map_or(self.limit_bytes, |o| o.total_metadata_ceiling_bytes());
        self.ledger
            .try_set_reservation(self.worker_id, new_bytes, total_cap)
    }

    pub(super) fn release(&self) {
        self.ledger.release(self.worker_id);
    }
}

impl Drop for WorkerHostMetadataGuard {
    fn drop(&mut self) {
        self.release();
    }
}

fn frames_metadata_limit(admitted_frames: u32, worker_ceiling_bytes: u64) -> u64 {
    let fixed = u64::from(admitted_frames)
        .saturating_mul(VramSlabPool::per_frame_fixed_metadata_bytes() as u64);
    let max_dynamic = u64::from(admitted_frames)
        .saturating_mul(128)
        .saturating_mul(LIVE_PAGE_METADATA_BYTES);
    worker_ceiling_bytes
        .min(fixed.saturating_add(max_dynamic))
        .max(fixed)
}

pub(super) fn live_worker_metadata_bytes(
    pool: &VramSlabPool,
    slab_pages_len: usize,
    slab_frame_pages_len: usize,
) -> u64 {
    let dynamic_pages = slab_pages_len
        .max(slab_frame_pages_len)
        .saturating_mul(LIVE_PAGE_METADATA_BYTES as usize) as u64;
    pool.known_total_metadata_bytes()
        .saturating_add(dynamic_pages)
}

fn parse_meminfo_kib(text: &str, key: &str) -> Option<u64> {
    for line in text.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        if name.trim() != key {
            continue;
        }
        return rest.split_whitespace().next()?.parse::<u64>().ok();
    }
    None
}

fn probe_cgroup_v2_headroom(proc_cgroup: &str, cgroup_root: &Path) -> Option<u64> {
    let rel = proc_cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(str::trim)?;
    let mut dirs = Vec::new();
    let trimmed = rel.trim_start_matches('/');
    let mut cursor = PathBuf::from(cgroup_root);
    dirs.push(cursor.clone());
    if !trimmed.is_empty() {
        for component in Path::new(trimmed).components() {
            if let std::path::Component::Normal(part) = component {
                cursor.push(part);
                dirs.push(cursor.clone());
            }
        }
    }
    let mut tightest: Option<u64> = None;
    for dir in dirs {
        let Ok(max_raw) = fs::read_to_string(dir.join("memory.max")) else {
            continue;
        };
        let Ok(cur_raw) = fs::read_to_string(dir.join("memory.current")) else {
            continue;
        };
        let max_s = max_raw.trim();
        if max_s == "max" {
            continue;
        }
        let Ok(limit) = max_s.parse::<u64>() else {
            continue;
        };
        let Ok(used) = cur_raw.trim().parse::<u64>() else {
            continue;
        };
        let headroom = limit.saturating_sub(used);
        tightest = Some(tightest.map_or(headroom, |prev| prev.min(headroom)));
    }
    tightest
}

pub(super) fn admit_slab_bridge_with_codec<'p, P: VramProvider + 'p, F>(
    provider: &'p P,
    effective_target_bytes: u64,
    raw_chunk_bytes: u64,
    worker_count: u32,
    shared_budget: Option<SharedHostMetadataBudget>,
    host_observation: Option<HostMemoryObservation>,
    make_codec: Option<F>,
) -> Option<(GpuWorkerSlabBridge<'p>, WorkerHostMetadataGuard)>
where
    F: FnOnce(&'p P, u64, u32) -> Result<Box<dyn GpuDeviceCodec + 'p>, String>,
{
    let raw_reserve =
        if raw_chunk_bytes > 0 && effective_target_bytes >= raw_chunk_bytes.saturating_mul(4) {
            raw_chunk_bytes
        } else {
            0
        };
    let vram_usable_frames = SlabBridgeConfig::sealed()
        .frame_count(effective_target_bytes, raw_reserve)
        .ok()?;
    let (frames, host_guard) = WorkerHostMetadataGuard::admit_frames(
        vram_usable_frames,
        worker_count,
        shared_budget,
        host_observation,
    );
    if frames == 0 {
        return None;
    }
    let pool = VramSlabPool::new(frames).ok()?;
    let _ = host_guard.update_usage(live_worker_metadata_bytes(&pool, 0, 0));
    let codec = match make_codec {
        Some(factory) => match factory(provider, effective_target_bytes, frames) {
            Ok(codec) => codec,
            Err(err) => {
                eprintln!("[ramsharedd] gpu slab codec disabled: {err}");
                return None;
            }
        },
        None => Box::new(ReferenceLz4Codec::new()),
    };
    Some((GpuWorkerSlabBridge::new(pool, Some(codec)), host_guard))
}

#[cfg(test)]
#[path = "host_budget/tests.rs"]
mod tests;
