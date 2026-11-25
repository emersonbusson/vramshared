// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/worker_retry.rs
//! Bounded worker retry bursts, cooldown and verified reset conditions.
use super::OriginCacheState;
use std::time::{Duration, Instant};

/// Bounds retries across bursts while allowing automatic recovery after cooldown.
/// A settled worker or a newly authenticated catalog generation also resets
/// the consecutive-failure budget.
#[derive(Debug)]
pub(super) struct WorkerRespawnBudget {
    pub(super) respawns_since_healthy: u32,
    pub(super) spawned_at: Option<Instant>,
    pub(super) last_catalog_generation: u64,
    pub(super) cooldown_started_at: Option<Instant>,
    /// Limits immediate retries across repeated authority loss and return.
    last_catalog_reset_at: Option<Instant>,
    /// Tracks an intervening lease loss so reconnect with a fresh live catalog recovers promptly.
    saw_lease_loss: bool,
    /// True after a configured host GPU catalog goes absent/expired while the
    /// vsock lease stays connected (e.g., Windows GPU driver update in progress).
    saw_catalog_loss: bool,
    /// True after a Native Linux GPU kernel/userspace driver becomes unavailable.
    saw_native_driver_loss: bool,
    /// Last observed Native Linux GPU driver signature (`mod` srcversion/libs).
    last_native_driver_epoch: Option<String>,
    /// Minimum spacing between reconnect-triggered burst resets.
    last_reconnect_reset_at: Option<Instant>,
}

/// At most this many respawns between healthy periods.
pub(super) const WORKER_RESPAWN_MAX_CONSECUTIVE: u32 = 3;
/// A worker that survives this long earns the next burst of respawns.
pub(super) const WORKER_RESPAWN_HEALTHY_AFTER: Duration = Duration::from_secs(60);
/// Sustained failure allows at most one three-attempt burst per interval.
pub(super) const WORKER_RESPAWN_COOLDOWN: Duration = Duration::from_secs(300);
/// Minimum spacing between reconnect-driven resets to prevent disconnect-storm thrashing.
pub(super) const WORKER_RECONNECT_RESET_COOLDOWN: Duration = Duration::from_secs(5);

impl WorkerRespawnBudget {
    pub(super) fn new() -> Self {
        Self {
            respawns_since_healthy: 0,
            spawned_at: None,
            last_catalog_generation: 0,
            cooldown_started_at: None,
            last_catalog_reset_at: None,
            saw_lease_loss: false,
            saw_catalog_loss: false,
            saw_native_driver_loss: false,
            last_native_driver_epoch: None,
            last_reconnect_reset_at: None,
        }
    }

    fn reset_for_restore_at(&mut self, now: Instant) -> bool {
        if self.respawns_since_healthy == 0 {
            self.saw_lease_loss = false;
            self.saw_catalog_loss = false;
            self.saw_native_driver_loss = false;
            return false;
        }
        let may_reset = self.last_reconnect_reset_at.is_none_or(|last| {
            now.checked_duration_since(last)
                .is_some_and(|elapsed| elapsed >= WORKER_RECONNECT_RESET_COOLDOWN)
        });
        if may_reset {
            self.respawns_since_healthy = 0;
            self.spawned_at = None;
            self.cooldown_started_at = None;
            self.last_catalog_reset_at = None;
            self.saw_lease_loss = false;
            self.saw_catalog_loss = false;
            self.saw_native_driver_loss = false;
            self.last_reconnect_reset_at = Some(now);
            return true;
        }
        false
    }

    pub(super) fn note_lease_state(&mut self, lease_live: bool, catalog_live: bool) -> bool {
        self.note_lease_state_at(lease_live, catalog_live, Instant::now())
    }

    fn note_lease_state_at(&mut self, lease_live: bool, catalog_live: bool, now: Instant) -> bool {
        if !lease_live {
            self.saw_lease_loss = true;
            return false;
        }
        if !catalog_live {
            self.saw_catalog_loss = true;
            return false;
        }
        if !self.saw_lease_loss && !self.saw_catalog_loss {
            return false;
        }
        self.reset_for_restore_at(now)
    }

    pub(super) fn note_native_driver_state(&mut self, ready: bool, epoch: &str) -> bool {
        self.note_native_driver_state_at(ready, epoch, Instant::now())
    }

    fn note_native_driver_state_at(&mut self, ready: bool, epoch: &str, now: Instant) -> bool {
        if !ready {
            self.saw_native_driver_loss = true;
            self.last_native_driver_epoch = Some(epoch.to_string());
            return false;
        }
        let epoch_changed = self
            .last_native_driver_epoch
            .as_deref()
            .is_some_and(|prev| prev != epoch);
        if epoch_changed || self.last_native_driver_epoch.is_none() {
            self.last_native_driver_epoch = Some(epoch.to_string());
        }
        if !self.saw_native_driver_loss && !epoch_changed {
            return false;
        }
        self.reset_for_restore_at(now)
    }

    pub(super) fn note_catalog_generation(&mut self, generation: u64) {
        self.note_catalog_generation_at(generation, Instant::now());
    }

    fn note_catalog_generation_at(&mut self, generation: u64, now: Instant) {
        if generation <= self.last_catalog_generation {
            return;
        }
        self.last_catalog_generation = generation;
        let may_reset = self.last_catalog_reset_at.is_none_or(|last| {
            now.checked_duration_since(last)
                .is_some_and(|elapsed| elapsed >= WORKER_RESPAWN_COOLDOWN)
        });
        if self.respawns_since_healthy > 0 && may_reset {
            self.respawns_since_healthy = 0;
            self.spawned_at = None;
            self.cooldown_started_at = None;
            self.last_catalog_reset_at = Some(now);
        }
    }

    pub(super) fn allows_respawn(&self) -> bool {
        self.respawns_since_healthy < WORKER_RESPAWN_MAX_CONSECUTIVE
    }

    pub(super) fn refresh_cooldown_at(&mut self, now: Instant) -> bool {
        if self.allows_respawn() {
            return false;
        }
        match self.cooldown_started_at {
            None => {
                self.cooldown_started_at = Some(now);
                false
            }
            Some(started)
                if now
                    .checked_duration_since(started)
                    .is_some_and(|elapsed| elapsed >= WORKER_RESPAWN_COOLDOWN) =>
            {
                self.respawns_since_healthy = 0;
                self.spawned_at = None;
                self.cooldown_started_at = None;
                self.last_catalog_reset_at = None;
                true
            }
            Some(_) => false,
        }
    }

    pub(super) fn note_spawned(&mut self) {
        self.respawns_since_healthy = self.respawns_since_healthy.saturating_add(1);
        self.spawned_at = Some(Instant::now());
    }

    pub(super) fn note_spawn_failed(&mut self) {
        self.respawns_since_healthy = self.respawns_since_healthy.saturating_add(1);
        self.spawned_at = None;
    }

    /// A worker that has stayed up `WORKER_RESPAWN_HEALTHY_AFTER` is healthy
    /// and earns the next burst of respawns. `now` is explicit so the 60s
    /// window is testable without sleeping through it.
    pub(super) fn mark_healthy_if_settled_at(&mut self, state: OriginCacheState, now: Instant) {
        let settled = matches!(
            state,
            OriginCacheState::Active | OriginCacheState::Restricted
        ) && self
            .spawned_at
            .is_some_and(|at| now.saturating_duration_since(at) >= WORKER_RESPAWN_HEALTHY_AFTER);
        if settled {
            self.respawns_since_healthy = 0;
            self.spawned_at = None;
            self.cooldown_started_at = None;
            self.last_catalog_reset_at = None;
            self.saw_lease_loss = false;
            self.saw_catalog_loss = false;
            self.saw_native_driver_loss = false;
        }
    }

    pub(super) fn mark_healthy_if_settled(&mut self, state: OriginCacheState) {
        self.mark_healthy_if_settled_at(state, Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exhaust(budget: &mut WorkerRespawnBudget) {
        for _ in 0..WORKER_RESPAWN_MAX_CONSECUTIVE {
            budget.note_spawn_failed();
        }
        assert!(!budget.allows_respawn());
    }

    #[test]
    fn repeated_catalog_outages_cannot_cause_unbounded_retry_bursts() {
        let start = Instant::now();
        let mut budget = WorkerRespawnBudget::new();
        budget.note_catalog_generation_at(1, start);
        exhaust(&mut budget);
        budget.note_catalog_generation_at(2, start + Duration::from_secs(1));
        assert!(
            budget.allows_respawn(),
            "the first return retries immediately"
        );
        exhaust(&mut budget);

        budget.note_catalog_generation_at(3, start + Duration::from_secs(2));
        assert!(
            !budget.allows_respawn(),
            "a flapping catalog must not reset again"
        );
        budget.note_catalog_generation_at(4, start);
        assert!(
            !budget.allows_respawn(),
            "a backwards clock cannot grant retries"
        );
        budget.note_catalog_generation_at(5, start + WORKER_RESPAWN_COOLDOWN);
        assert!(
            !budget.allows_respawn(),
            "the full interval has not elapsed"
        );
        budget.note_catalog_generation_at(
            6,
            start + WORKER_RESPAWN_COOLDOWN + Duration::from_secs(1),
        );
        assert!(budget.allows_respawn());
    }

    #[test]
    fn lease_restore_after_disconnect_clears_respawn_cooldown() {
        let start = Instant::now();
        let mut budget = WorkerRespawnBudget::new();
        budget.note_catalog_generation_at(1, start);
        exhaust(&mut budget);
        budget.note_catalog_generation_at(2, start + Duration::from_secs(1));
        assert!(budget.allows_respawn());
        exhaust(&mut budget);

        // Disconnecting and reconnecting once the catalog is live clears the 300s lock
        // without waiting 5 minutes, while holding off until the catalog itself arrives.
        budget.note_lease_state_at(false, false, start + Duration::from_secs(2));
        budget.note_lease_state_at(true, false, start + Duration::from_secs(3));
        assert!(!budget.allows_respawn(), "must await live catalog");

        budget.note_lease_state_at(true, true, start + Duration::from_secs(3));
        assert!(
            budget.allows_respawn(),
            "live lease + live catalog restore after safe_mode must permit immediate recovery"
        );

        // Rapid disconnect/reconnect storms inside WORKER_RECONNECT_RESET_COOLDOWN stay bounded.
        exhaust(&mut budget);
        budget.note_lease_state_at(false, false, start + Duration::from_secs(4));
        budget.note_lease_state_at(true, true, start + Duration::from_secs(5));
        assert!(
            !budget.allows_respawn(),
            "rapid reconnect oscillation within min cooldown must not reset"
        );
    }

    #[test]
    fn catalog_loss_with_live_lease_and_native_driver_updates_clear_cooldown() {
        let start = Instant::now();
        let mut budget = WorkerRespawnBudget::new();
        exhaust(&mut budget);

        // Windows GPU driver update: RamSharedWinSvc lease stays live while host catalog
        // drops and returns after driver reinitialization.
        assert!(!budget.note_lease_state_at(true, false, start + Duration::from_secs(1)));
        assert!(!budget.allows_respawn());
        assert!(budget.note_lease_state_at(true, true, start + Duration::from_secs(10)));
        assert!(budget.allows_respawn());

        // Fast < 5s repeated catalog flap stays bounded; after >= 5s resets cleanly.
        exhaust(&mut budget);
        assert!(!budget.note_lease_state_at(true, false, start + Duration::from_secs(11)));
        assert!(!budget.note_lease_state_at(true, true, start + Duration::from_secs(12)));
        assert!(!budget.allows_respawn());

        // Healthy baseline does not spuriously flag an initial epoch change; native loss -> restore resets once spacing elapses.
        let mut native = WorkerRespawnBudget::new();
        assert!(!native.note_native_driver_state_at(true, "nv=550;vk=1", start));
        exhaust(&mut native);
        assert!(!native.note_native_driver_state_at(
            false,
            "nv=going;vk=1",
            start + Duration::from_secs(1)
        ));
        assert!(native.note_native_driver_state_at(
            true,
            "nv=560;vk=1",
            start + Duration::from_secs(10)
        ));
        assert!(native.allows_respawn());

        // Userspace-only ICD / CUDA symlink epoch change while kernel stays live also wakes recovery.
        exhaust(&mut native);
        assert!(native.note_native_driver_state_at(
            true,
            "amd=6.8;icd=v2",
            start + Duration::from_secs(20)
        ));
        assert!(native.allows_respawn());
    }
}
