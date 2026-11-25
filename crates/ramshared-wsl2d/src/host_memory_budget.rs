// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/host_memory_budget.rs
//! Checked arithmetic for combining host, guest and cgroup memory observations.
//!
//! This calculation is not an allocation permit. The caller must collect
//! authenticated, fresh observations and retain shared reservation custody.

mod reservations;
pub use reservations::{MemoryObservation, MemoryReservation, SharedMemoryReservations};
pub mod linux_observation;

pub use ramshared_tier::memory_headroom::{
    CgroupIdentity, CgroupSample, HostSample, HostScope, MemorySample, shared_headroom,
};

#[cfg(test)]
mod tests {
    #[test]
    fn shared_policy_remains_available_through_daemon_api() {
        let guest = super::MemorySample {
            total_bytes: 100,
            available_bytes: 80,
            sampled_at_unix_ms: 1000,
        };
        assert_eq!(
            super::shared_headroom(guest, super::HostScope::Native, Some(&[]), 10, 0, 1001, 5),
            Some(70)
        );
    }
}
