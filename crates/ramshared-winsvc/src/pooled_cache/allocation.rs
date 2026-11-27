// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/pooled_cache/allocation.rs
//! Deterministic host VRAM admission, priority lending, and adapter placement custody.
//! SPEC: the protocol specification §DT-7, §DT-8, §DT-9.

use std::time::{Duration, Instant};

use ramshared_ipc::cache_wire::CacheGuestRole;
use ramshared_vram::{RUNTIME_FREE_BUFFER_BYTES, ReserveFloorPolicy};

pub const MAX_SAMPLE_AGE_MS: u64 = 2_000;
pub const LEND_STEP_BYTES: u64 = 128 * 1024 * 1024;
pub const MIN_HOST_RAM_RESERVE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdapterObservation {
    pub adapter_id: String,
    pub adapter_local_bytes: u64,
    pub process_budget_bytes: u64,
    pub wddm_current_usage_bytes: u64,
    pub cuda_free_bytes: u64,
    pub host_physical_ram_bytes: u64,
    pub host_available_ram_bytes: u64,
    pub staging_allowance_bytes: u64,
    pub sampled_at: Instant,
    pub signed_match: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestPlacementIntent {
    PooledSealed,
    Independent,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestCustodyState {
    RunningConsented { boot_gen: u64 },
    ControlledOff { sealed_boot_path: bool },
    RunningUnconsented,
    SavedOrRestored,
    IncompatibleBoot,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AllocationRefusal {
    PlacementNotAuthorized,
    CompetingAllocator,
    SavedOrRestoredGuest,
    IncompatibleOffTransition,
    AmbiguousAdapter,
    StaleObservation,
    HostReserveFloor,
    PrimaryPriority,
    ZeroHeadroom,
    ZeroRequest,
    QuarantineActive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RolePlacement {
    pub intent: GuestPlacementIntent,
    pub custody: GuestCustodyState,
}

impl RolePlacement {
    pub fn verify(self) -> Result<(), AllocationRefusal> {
        if self.intent != GuestPlacementIntent::PooledSealed {
            return Err(AllocationRefusal::PlacementNotAuthorized);
        }
        match self.custody {
            GuestCustodyState::RunningConsented { boot_gen } if boot_gen > 0 => Ok(()),
            GuestCustodyState::ControlledOff {
                sealed_boot_path: true,
            } => Ok(()),
            GuestCustodyState::SavedOrRestored => Err(AllocationRefusal::SavedOrRestoredGuest),
            GuestCustodyState::IncompatibleBoot
            | GuestCustodyState::ControlledOff {
                sealed_boot_path: false,
            } => Err(AllocationRefusal::IncompatibleOffTransition),
            _ => Err(AllocationRefusal::PlacementNotAuthorized),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PoolAdmission {
    policy: ReserveFloorPolicy,
    adapter_id: String,
    explicit_host_ram_floor_bytes: Option<u64>,
    placement_permit_held: bool,
    charged_wsl_bytes: u64,
    charged_secondary_bytes: u64,
    in_flight_bytes: u64,
    quarantined_bytes: u64,
}

impl PoolAdmission {
    pub fn new(policy: ReserveFloorPolicy, adapter_id: impl Into<String>) -> Self {
        Self {
            policy,
            adapter_id: adapter_id.into(),
            explicit_host_ram_floor_bytes: None,
            placement_permit_held: false,
            charged_wsl_bytes: 0,
            charged_secondary_bytes: 0,
            in_flight_bytes: 0,
            quarantined_bytes: 0,
        }
    }

    /// Override the proportional 25% host RAM reserve with an explicit host floor
    /// clamped to at least [`MIN_HOST_RAM_RESERVE_BYTES`] (4 GiB) for dual-VM hosts.
    pub fn with_host_ram_floor_bytes(mut self, floor_bytes: u64) -> Self {
        self.explicit_host_ram_floor_bytes = Some(floor_bytes.max(MIN_HOST_RAM_RESERVE_BYTES));
        self
    }

    pub fn placement_permit_held(&self) -> bool {
        self.placement_permit_held
    }

    pub fn charged_for(&self, role: CacheGuestRole) -> u64 {
        match role {
            CacheGuestRole::Wsl2 => self.charged_wsl_bytes,
            CacheGuestRole::SecondaryGuest => self.charged_secondary_bytes,
        }
    }

    pub fn in_flight_bytes(&self) -> u64 {
        self.in_flight_bytes
    }

    pub fn quarantined_bytes(&self) -> u64 {
        self.quarantined_bytes
    }

    pub fn total_accounted_bytes(&self) -> u64 {
        self.charged_wsl_bytes
            .saturating_add(self.charged_secondary_bytes)
            .saturating_add(self.in_flight_bytes)
            .saturating_add(self.quarantined_bytes)
    }

    /// Acquire or maintain the single adapter cache-placement permit.
    pub fn authorize_placement(
        &mut self,
        wsl: RolePlacement,
        secondary: RolePlacement,
        competing_allocator_active: bool,
    ) -> Result<(), AllocationRefusal> {
        if competing_allocator_active {
            return Err(AllocationRefusal::CompetingAllocator);
        }
        wsl.verify()?;
        secondary.verify()?;
        self.placement_permit_held = true;
        Ok(())
    }

    /// Release the adapter placement permit only when worker and quarantine fully settled.
    pub fn try_release_placement(&mut self, worker_active: bool) -> Result<(), AllocationRefusal> {
        if worker_active || self.quarantined_bytes > 0 || self.in_flight_bytes > 0 {
            return Err(AllocationRefusal::QuarantineActive);
        }
        self.placement_permit_held = false;
        Ok(())
    }

    /// Compute exact DT-7 eligible growth bytes from a fresh adapter & host sample.
    pub fn eligible_growth_bytes(
        &self,
        obs: &AdapterObservation,
        now: Instant,
    ) -> Result<u64, AllocationRefusal> {
        if !obs.signed_match || obs.adapter_id != self.adapter_id || obs.adapter_id.is_empty() {
            return Err(AllocationRefusal::AmbiguousAdapter);
        }
        if now.saturating_duration_since(obs.sampled_at) > Duration::from_millis(MAX_SAMPLE_AGE_MS)
        {
            return Err(AllocationRefusal::StaleObservation);
        }
        let base_host_floor = match self.explicit_host_ram_floor_bytes {
            Some(explicit) => explicit.max(MIN_HOST_RAM_RESERVE_BYTES),
            None => obs
                .host_physical_ram_bytes
                .div_ceil(4)
                .max(MIN_HOST_RAM_RESERVE_BYTES),
        };
        let host_floor = base_host_floor.saturating_add(obs.staging_allowance_bytes);
        if obs.host_available_ram_bytes < host_floor {
            return Err(AllocationRefusal::HostReserveFloor);
        }
        let capacity = ReserveFloorPolicy::helper_capacity(
            Some(obs.adapter_local_bytes),
            obs.process_budget_bytes,
        );
        let reserve = self
            .policy
            .enforced_free_floor_bytes(capacity, RUNTIME_FREE_BUFFER_BYTES);
        let cap_headroom = capacity.saturating_sub(self.total_accounted_bytes());
        let wddm_free = obs
            .process_budget_bytes
            .saturating_sub(obs.wddm_current_usage_bytes);
        let live_free = wddm_free
            .min(obs.cuda_free_bytes)
            .saturating_sub(reserve)
            .saturating_sub(self.in_flight_bytes);
        Ok(cap_headroom.min(live_free))
    }

    /// Charge the full requested allocation in-flight before worker submission (DT-7/DT-8).
    pub fn try_charge(
        &mut self,
        role: CacheGuestRole,
        requested_bytes: u64,
        obs: &AdapterObservation,
        now: Instant,
        wsl_waiting: bool,
    ) -> Result<u64, AllocationRefusal> {
        if !self.placement_permit_held {
            return Err(AllocationRefusal::PlacementNotAuthorized);
        }
        if requested_bytes == 0 {
            return Err(AllocationRefusal::ZeroRequest);
        }
        if role == CacheGuestRole::SecondaryGuest && wsl_waiting {
            return Err(AllocationRefusal::PrimaryPriority);
        }
        let eligible = self.eligible_growth_bytes(obs, now)?;
        let cap = match role {
            CacheGuestRole::Wsl2 => requested_bytes,
            CacheGuestRole::SecondaryGuest => requested_bytes.min(LEND_STEP_BYTES),
        };
        let granted = cap.min(eligible);
        if granted == 0 {
            return Err(AllocationRefusal::ZeroHeadroom);
        }
        self.in_flight_bytes = self.in_flight_bytes.saturating_add(granted);
        Ok(granted)
    }

    pub fn commit_charged(&mut self, role: CacheGuestRole, bytes: u64) {
        let settled = bytes.min(self.in_flight_bytes);
        self.in_flight_bytes = self.in_flight_bytes.saturating_sub(settled);
        match role {
            CacheGuestRole::Wsl2 => {
                self.charged_wsl_bytes = self.charged_wsl_bytes.saturating_add(settled);
            }
            CacheGuestRole::SecondaryGuest => {
                self.charged_secondary_bytes = self.charged_secondary_bytes.saturating_add(settled);
            }
        }
    }

    pub fn release_verified(&mut self, role: CacheGuestRole, bytes: u64) {
        match role {
            CacheGuestRole::Wsl2 => {
                self.charged_wsl_bytes = self.charged_wsl_bytes.saturating_sub(bytes);
            }
            CacheGuestRole::SecondaryGuest => {
                self.charged_secondary_bytes = self.charged_secondary_bytes.saturating_sub(bytes);
            }
        }
    }

    /// Move in-flight or committed bytes into quarantine; never freed until context exit (DT-9).
    pub fn quarantine_uncertain(&mut self, in_flight: u64, wsl: u64, secondary: u64) {
        let f = in_flight.min(self.in_flight_bytes);
        let w = wsl.min(self.charged_wsl_bytes);
        let s = secondary.min(self.charged_secondary_bytes);
        self.in_flight_bytes = self.in_flight_bytes.saturating_sub(f);
        self.charged_wsl_bytes = self.charged_wsl_bytes.saturating_sub(w);
        self.charged_secondary_bytes = self.charged_secondary_bytes.saturating_sub(s);
        self.quarantined_bytes = self
            .quarantined_bytes
            .saturating_add(f)
            .saturating_add(w)
            .saturating_add(s);
    }

    pub fn release_quarantined_after_exit(&mut self, bytes: u64) {
        self.quarantined_bytes = self.quarantined_bytes.saturating_sub(bytes);
    }
}
