// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/tier_release/observation.rs
//! Bounded procfs observations for managed swap release.

use ramshared_block::swap_absorber::SwapAbsorber;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

type HostMemorySource = Arc<dyn Fn() -> Option<ramshared_tier::HostDrainMemory> + Send + Sync>;

static ROUTINE_HOST_MEMORY: Mutex<Option<(u64, HostMemorySource)>> = Mutex::new(None);
static NEXT_HOST_MEMORY_EPOCH: AtomicU64 = AtomicU64::new(1);
#[cfg(test)]
pub(super) static TEST_HOST_MEMORY_LOCK: Mutex<()> = Mutex::new(());

/// Scoped registration of an authenticated WSL host-memory source for routine drains.
pub struct RoutineHostMemoryGuard(u64);

impl Drop for RoutineHostMemoryGuard {
    fn drop(&mut self) {
        let mut slot = ROUTINE_HOST_MEMORY
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if slot.as_ref().is_some_and(|(epoch, _)| *epoch == self.0) {
            *slot = None;
        }
    }
}

pub fn register_routine_host_memory(
    source: impl Fn() -> Option<ramshared_tier::HostDrainMemory> + Send + Sync + 'static,
) -> RoutineHostMemoryGuard {
    let epoch = NEXT_HOST_MEMORY_EPOCH.fetch_add(1, Ordering::Relaxed);
    let mut slot = ROUTINE_HOST_MEMORY
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    *slot = Some((epoch, Arc::new(source)));
    RoutineHostMemoryGuard(epoch)
}

pub(super) fn observed_routine_host_memory() -> Option<ramshared_tier::HostDrainMemory> {
    let source = ROUTINE_HOST_MEMORY
        .try_lock()
        .ok()?
        .as_ref()
        .map(|(_, source)| Arc::clone(source))?;
    source()
}

type HostStorageSource = Arc<dyn Fn(&Path, u64) -> Option<u64> + Send + Sync>;

static ROUTINE_HOST_STORAGE: Mutex<Option<(u64, HostStorageSource)>> = Mutex::new(None);
static NEXT_HOST_STORAGE_EPOCH: AtomicU64 = AtomicU64::new(1);

/// Scoped registration of an authenticated WSL host-storage headroom cap.
pub struct RoutineHostStorageGuard(u64);

impl Drop for RoutineHostStorageGuard {
    fn drop(&mut self) {
        let mut slot = ROUTINE_HOST_STORAGE
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if slot.as_ref().is_some_and(|(epoch, _)| *epoch == self.0) {
            *slot = None;
        }
    }
}

pub fn register_routine_host_storage(
    source: impl Fn(&Path, u64) -> Option<u64> + Send + Sync + 'static,
) -> RoutineHostStorageGuard {
    let epoch = NEXT_HOST_STORAGE_EPOCH.fetch_add(1, Ordering::Relaxed);
    let mut slot = ROUTINE_HOST_STORAGE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    *slot = Some((epoch, Arc::new(source)));
    RoutineHostStorageGuard(epoch)
}

pub(super) fn observed_routine_host_storage_free(root: &Path, guest_free: u64) -> Option<u64> {
    let (epoch, source) = ROUTINE_HOST_STORAGE
        .try_lock()
        .ok()?
        .as_ref()
        .map(|(epoch, source)| (*epoch, Arc::clone(source)))?;
    let observed = source(root, guest_free)?;
    let slot = ROUTINE_HOST_STORAGE.try_lock().ok()?;
    slot.as_ref()
        .is_some_and(|(current, active)| *current == epoch && Arc::ptr_eq(active, &source))
        .then_some(observed)
}

pub(super) fn has_routine_host_storage() -> bool {
    ROUTINE_HOST_STORAGE
        .try_lock()
        .ok()
        .is_some_and(|slot| slot.is_some())
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct DrainObservation {
    pub(super) memory: ramshared_tier::DrainMemory,
    pub(super) free_bytes: Option<u64>,
    pub(super) target_used: u64,
    pub(super) target: String,
    pub(super) rows: Vec<(String, u64, u64)>,
}
impl DrainObservation {
    #[cfg(test)]
    pub(super) fn decision(
        &self,
        absorber: Option<&SwapAbsorber>,
    ) -> Option<ramshared_tier::DrainDecision> {
        self.decision_with_host(absorber, None)
    }

    pub(super) fn decision_with_host(
        &self,
        absorber: Option<&SwapAbsorber>,
        wsl_host: Option<Option<ramshared_tier::HostDrainMemory>>,
    ) -> Option<ramshared_tier::DrainDecision> {
        let emergency_bytes = match absorber {
            Some(backing) => {
                if backing.path() == Path::new(&self.target) {
                    return None;
                }
                backing.free_bytes(
                    self.rows
                        .iter()
                        .map(|(name, size, used)| (name.as_str(), *size, *used)),
                )?
            }
            None => 0,
        };
        let budget = ramshared_tier::DrainBudget {
            total_bytes: self.memory.total_bytes,
            available_bytes: self.memory.available_bytes,
            managed_bytes: self.target_used,
            emergency_bytes,
        };
        match wsl_host {
            None => budget.evaluate(),
            Some(host) => budget.evaluate_wsl(self.free_bytes, host),
        }
    }
}

pub(super) fn parse_swap_rows(text: &str) -> Option<Vec<(String, u64, u64)>> {
    if text.len() > ramshared_tier::DRAIN_OBSERVATION_MAX_BYTES {
        return None;
    }
    let mut lines = text.lines();
    if !lines
        .next()?
        .split_whitespace()
        .eq(["Filename", "Type", "Size", "Used", "Priority"])
    {
        return None;
    }
    let mut rows = Vec::new();
    let mut identities = std::collections::HashSet::new();
    for line in lines {
        let mut fields = line.split_whitespace();
        let name = fields.next()?;
        if !matches!(fields.next()?, "file" | "partition") || !identities.insert(name) {
            return None;
        }
        let size = fields.next()?.parse::<u64>().ok()?.checked_mul(1024)?;
        let used = fields.next()?.parse::<u64>().ok()?.checked_mul(1024)?;
        fields.next()?.parse::<i32>().ok()?;
        if fields.next().is_some() || used > size {
            return None;
        }
        rows.push((name.to_string(), size, used));
    }
    Some(rows)
}

pub(super) fn parse_drain_absorbers(
    meminfo: &str,
    swaps: &str,
    device: &str,
) -> Option<DrainObservation> {
    let memory = ramshared_tier::DrainMemory::parse(meminfo)?;
    let free_bytes = ramshared_tier::parse_free_bytes(meminfo);
    if free_bytes.is_some_and(|free| free > memory.total_bytes) {
        return None;
    }
    let rows = parse_swap_rows(swaps)?;
    let target_used = rows.iter().find(|(name, _, _)| name == device)?.2;
    Some(DrainObservation {
        memory,
        free_bytes,
        target_used,
        target: device.to_string(),
        rows,
    })
}

pub(super) fn read_proc(path: &str) -> Option<String> {
    use std::io::Read;
    let mut text = String::new();
    std::fs::File::open(path)
        .ok()?
        .take((ramshared_tier::DRAIN_OBSERVATION_MAX_BYTES + 1) as u64)
        .read_to_string(&mut text)
        .ok()?;
    (text.len() <= ramshared_tier::DRAIN_OBSERVATION_MAX_BYTES).then_some(text)
}
