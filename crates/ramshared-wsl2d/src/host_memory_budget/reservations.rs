// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/host_memory_budget/reservations.rs
//! Shared custody for host-memory admission. This is not a memory collector.
//!
//! The daemon must retain each reservation until the corresponding worker
//! has exited and its allocations are gone. A reservation is never evidence
//! of process exit, and this owner does not terminate or observe a worker.
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LockResult, Mutex, MutexGuard, TryLockResult};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::{CgroupIdentity, CgroupSample, HostScope, MemorySample, shared_headroom};

/// A complete observation collected by the trusted daemon. The collector must
/// establish provenance; missing or empty ancestry and stale samples refuse.
pub struct MemoryObservation<'a> {
    pub guest: MemorySample,
    pub host: HostScope,
    pub cgroups: Option<&'a [CgroupSample]>,
    pub guest_reserve_bytes: u64,
    pub host_reserve_bytes: u64,
    pub max_age_ms: u64,
}

impl MemoryObservation<'_> {
    fn epochs(&self) -> Option<SampleEpochs> {
        Some(SampleEpochs {
            guest: self.guest.sampled_at_unix_ms,
            host: match self.host {
                HostScope::Native => None,
                HostScope::Wsl(host) => Some(host.physical.sampled_at_unix_ms),
            },
            cgroups: self
                .cgroups?
                .iter()
                .map(|group| (group.identity, group.sampled_at_unix_ms))
                .collect(),
        })
    }

    fn remaining_age_ms(&self, now_unix_ms: u64) -> Option<u64> {
        let remaining = |sampled: u64| {
            self.max_age_ms
                .checked_sub(now_unix_ms.checked_sub(sampled)?)
        };
        let mut age = remaining(self.guest.sampled_at_unix_ms)?;
        if let HostScope::Wsl(host) = self.host {
            age = age.min(remaining(host.physical.sampled_at_unix_ms)?);
        }
        for group in self.cgroups? {
            age = age.min(remaining(group.sampled_at_unix_ms)?);
        }
        (age > 0).then_some(age)
    }
}

/// Per-source epochs prevent an unrelated newer sample from hiding a replay.
struct SampleEpochs {
    guest: u64,
    host: Option<u64>,
    cgroups: Vec<(CgroupIdentity, u64)>,
}

impl SampleEpochs {
    fn same_sources(&self, old: &Self) -> bool {
        self.host.is_some() == old.host.is_some()
            && self.cgroups.len() == old.cgroups.len()
            && self
                .cgroups
                .iter()
                .zip(&old.cgroups)
                .all(|(new, previous)| new.0 == previous.0)
    }

    fn follows(&self, old: &Self, strict: bool) -> bool {
        let later = |new: u64, previous: u64| {
            if strict {
                new > previous
            } else {
                new >= previous
            }
        };
        later(self.guest, old.guest)
            && match (self.host, old.host) {
                (Some(new), Some(previous)) => later(new, previous),
                (None, None) => true,
                _ => false,
            }
            && self.cgroups.len() == old.cgroups.len()
            && self.cgroups.iter().zip(&old.cgroups).all(
                |(&(identity, new), &(old_identity, previous))| {
                    identity == old_identity && later(new, previous)
                },
            )
    }
}

#[derive(Default)]
struct State {
    available: Option<u64>,
    reserved: u64,
    expires: Option<Instant>,
    updated_at: Option<Instant>,
    latest_epochs: Option<SampleEpochs>,
    invalid: bool,
}

const RELEASE_FAULT: u64 = 1 << 63;
const MAX_ACCOUNTED: u64 = RELEASE_FAULT - 1;

/// Pending releases remain charged until reconciled under the state lock.
#[derive(Default)]
struct Accounting {
    state: Mutex<State>,
    pending_releases: AtomicU64,
}

impl Accounting {
    fn lock(&self) -> LockResult<MutexGuard<'_, State>> {
        self.state.lock()
    }

    fn try_lock(&self) -> TryLockResult<MutexGuard<'_, State>> {
        self.state.try_lock()
    }

    fn settle(&self, state: &mut State) -> bool {
        if state.invalid {
            return false;
        }
        let released = self.pending_releases.swap(0, Ordering::AcqRel);
        let Some(remaining) = (released != RELEASE_FAULT)
            .then(|| state.reserved.checked_sub(released))
            .flatten()
        else {
            state.invalid = true;
            state.available = None;
            state.expires = None;
            return false;
        };
        state.reserved = remaining;
        true
    }

    fn defer_release(&self, bytes: u64) {
        // The high bit is a permanent accounting fault, not a wrapped sum.
        let _ = self
            .pending_releases
            .try_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
                Some(
                    pending
                        .checked_add(bytes)
                        .filter(|sum| *sum <= MAX_ACCOUNTED)
                        .unwrap_or(RELEASE_FAULT),
                )
            });
    }
}

/// All adapter workers in one daemon must share the same owner. This owner
/// intentionally cannot coordinate independent daemon processes.
#[derive(Clone, Default)]
pub struct SharedMemoryReservations {
    state: Arc<Accounting>,
}

/// Exclusive custody of one reservation; it must outlive the actual memory.
#[must_use]
pub struct MemoryReservation {
    state: Arc<Accounting>,
    bytes: u64,
    alive: Arc<AtomicBool>,
}

impl SharedMemoryReservations {
    /// Replace the admission ceiling. Invalid observations revoke new grants;
    /// existing reservations remain owned until their holders release them.
    pub fn refresh(&self, observation: &MemoryObservation<'_>) -> bool {
        let started = Instant::now();
        let now_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|now| u64::try_from(now.as_millis()).ok());
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if !self.state.settle(&mut state) {
            return false;
        }
        if state.updated_at.is_some_and(|previous| started <= previous) {
            return false;
        }
        state.updated_at = Some(started);
        let available = now_unix_ms.and_then(|now| {
            shared_headroom(
                observation.guest,
                observation.host,
                observation.cgroups.filter(|groups| !groups.is_empty()),
                observation.guest_reserve_bytes,
                observation.host_reserve_bytes,
                now,
                observation.max_age_ms,
            )
        });
        let expires = now_unix_ms
            .and_then(|now| observation.remaining_age_ms(now))
            .and_then(|remaining| started.checked_add(Duration::from_millis(remaining)))
            .filter(|expires| Instant::now() < *expires);
        if available.is_some()
            && let Some(epochs) = observation.epochs()
        {
            if state
                .latest_epochs
                .as_ref()
                .is_some_and(|latest| !epochs.same_sources(latest))
            {
                state.available = None;
                state.expires = None;
                return false;
            }
            if state.latest_epochs.as_ref().is_some_and(|latest| {
                let ceiling_increases = available
                    .zip(state.available)
                    .is_some_and(|(new, previous)| new > previous);
                !epochs.follows(latest, state.available.is_none() || ceiling_increases)
            }) {
                return false;
            }
            state.latest_epochs = Some(epochs);
        }
        state.available = available.filter(|_| expires.is_some());
        state.expires = state.available.and(expires);
        state.available.is_some()
    }

    /// Reserve a portion of the last valid ceiling without overcommitting it.
    /// Refuse on zero size, stale observations, overflow or busy/poisoned custody.
    pub fn try_reserve(&self, bytes: u64) -> Option<MemoryReservation> {
        self.try_reserve_inner(bytes, None)
    }

    #[cfg(test)]
    fn try_reserve_at(&self, bytes: u64, now: Instant) -> Option<MemoryReservation> {
        self.try_reserve_inner(bytes, Some(now))
    }

    fn try_reserve_inner(&self, bytes: u64, at: Option<Instant>) -> Option<MemoryReservation> {
        if bytes == 0 {
            return None;
        }
        let mut state = self.state.try_lock().ok()?;
        if !self.state.settle(&mut state) {
            return None;
        }
        let now = at.unwrap_or_else(Instant::now);
        if now >= state.expires? {
            return None;
        }
        let reserved = state.reserved.checked_add(bytes)?;
        if reserved > state.available? || reserved > MAX_ACCOUNTED {
            return None;
        }
        state.reserved = reserved;
        Some(MemoryReservation {
            state: Arc::clone(&self.state),
            bytes,
            alive: Arc::new(AtomicBool::new(true)),
        })
    }
}

impl MemoryReservation {
    /// Observe this exact reservation without retaining it. A replacement
    /// reservation cannot revive the observation after its owner is dropped.
    pub fn probe(&self) -> Arc<dyn Fn() -> bool + Send + Sync> {
        let state = Arc::downgrade(&self.state);
        let alive = Arc::downgrade(&self.alive);
        let bytes = self.bytes;
        Arc::new(move || {
            let (Some(state), Some(alive)) = (state.upgrade(), alive.upgrade()) else {
                return false;
            };
            let Ok(mut guard) = state.try_lock() else {
                return false;
            };
            alive.load(Ordering::Acquire)
                && state.settle(&mut guard)
                && state_covers(&guard, bytes, Instant::now())
        })
    }

    /// Observe whether the local accounting still covers this reservation.
    /// Contention, poisoning, expiry and revoked or reduced ceilings refuse.
    /// This snapshot neither reserves memory against other processes nor
    /// establishes that the host authority remains valid after the check.
    pub fn is_current(&self) -> bool {
        self.is_current_inner(None)
    }

    #[cfg(test)]
    fn is_current_at(&self, now: Instant) -> bool {
        self.is_current_inner(Some(now))
    }

    fn is_current_inner(&self, at: Option<Instant>) -> bool {
        let Ok(mut state) = self.state.try_lock() else {
            return false;
        };
        let now = at.unwrap_or_else(Instant::now);
        self.alive.load(Ordering::Acquire)
            && self.state.settle(&mut state)
            && state_covers(&state, self.bytes, now)
    }
}

fn state_covers(state: &State, bytes: u64, now: Instant) -> bool {
    !state.invalid
        && bytes > 0
        && state.reserved >= bytes
        && state
            .available
            .is_some_and(|available| state.reserved <= available)
        && state.expires.is_some_and(|expires| now < expires)
}

impl Drop for MemoryReservation {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
        self.state.defer_release(self.bytes);
    }
}

#[cfg(test)]
mod tests;
