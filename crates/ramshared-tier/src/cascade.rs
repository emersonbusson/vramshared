// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-tier/src/cascade.rs
//! Invariant safety net for DEMOTE (finding A1). SPEC §6.2 (step 4), §9.2.
//!
//! DEMOTE (§9.2) runs `swapoff` only on the VRAM tier when the canary detects eviction latency.
//! Resident VRAM pages are migrated to the lower-priority active tier.
//! This migration is only **safe** if there is a lower destination below VRAM — otherwise,
//! `swapoff` cannot drain pages and may trigger out-of-memory (OOM) conditions.
//! Thus: VRAM must not be armed without a safety net tier active.
//!
//! A1 is a **capacity** invariant, not a presence one. A sink that exists but has no
//! free space cannot receive the pages `swapoff` has to drain, so the DEMOTE stalls at
//! the exact moment a host consumer is demanding the VRAM back. The check therefore
//! compares free absorber space against a total VRAM evacuation.

use crate::nbd_readiness::{ProductTransport, minimum_lower_tier_bytes};

/// Tiers of the swap cascade, ordered from hottest to coldest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tier {
    /// zram — Compressed RAM, low latency (HOT).
    Zram,
    /// VRAM via `nbd-vram` — High bandwidth, volatile latency under pressure (COLD).
    Vram,
    /// WSL2 default swap VHDX — Last resort.
    Vhdx,
}

impl Tier {
    /// Returns the product transport owned by this tier, if it has one.
    ///
    /// The VRAM tier uses the NBD product interface when transport admission
    /// is open. WSL2 admission currently refuses its swap-backed path; ublk
    /// capability is never a product dependency.
    pub const fn product_transport(self) -> Option<ProductTransport> {
        match self {
            Self::Vram => Some(ProductTransport::Nbd),
            Self::Zram | Self::Vhdx => None,
        }
    }
}

/// Safety net status for the VRAM demotion path (Invariant A1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SafetyNet {
    /// Free space on a lower-priority non-managed swap absorbs a total VRAM
    /// evacuation: DEMOTE will spill into it without stalling.
    VhdxBelow,
    /// Absorber free space is short, but `MemAvailable >= vram_size`: DEMOTE can
    /// still spill into RAM (zram / page cache).
    RamHeadroom,
    /// No safety net active. Arming VRAM requires `--force-no-safety-net` (§6.2 step 4).
    None,
}

impl SafetyNet {
    /// Returns `true` if it is safe to mount VRAM swap without the `--force` override.
    pub fn is_safe(self) -> bool {
        !matches!(self, SafetyNet::None)
    }
}

/// Determines the safety net availability for the VRAM tier (A1).
///
/// `absorber_free` is the free capacity, in bytes, of every non-managed swap tier
/// below the VRAM priority — the VHDX/WSL fallback that receives DEMOTE pages
/// without depending on RAM. Presence is not enough: a present-but-full sink
/// cannot receive the evacuation, and `swapoff` then stalls while a host consumer
/// is already demanding the VRAM back.
///
/// The requirement is the NBD demotion formula, [`minimum_lower_tier_bytes`]:
/// `V + max(ceil(0.10 * V), 512 MiB)` — a full evacuation plus the margin for
/// pages still in flight while the drain runs. Bare `V` is not enough. A
/// `vram_size` whose requirement cannot be represented, including zero, is
/// refused as `None` (fail-closed).
///
/// This is the same formula `boot` enforces through `validate_lower_tier_capacity`,
/// so the attended and autonomous paths cannot disagree about what A1 requires.
///
/// Returns `VhdxBelow` when the absorber covers the requirement, else `RamHeadroom`
/// when `mem_available` covers it, else `None`.
pub fn vram_safety_net(absorber_free: u64, mem_available: u64, vram_size: u64) -> SafetyNet {
    let Ok(required) = minimum_lower_tier_bytes(vram_size) else {
        return SafetyNet::None;
    };

    if absorber_free >= required {
        return SafetyNet::VhdxBelow;
    }

    if mem_available >= required {
        return SafetyNet::RamHeadroom;
    }

    SafetyNet::None
}

/// Errors that can occur during tier resizing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResizeError {
    /// The requested tier capacity exceeds the physical host RAM capacity limit.
    ExceedsHostRamCapacity,
    /// The requested tier capacity exceeds the physical VRAM capacity limit.
    ExceedsVramCapacity,
}

impl core::fmt::Display for ResizeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ResizeError::ExceedsHostRamCapacity => {
                f.write_str("requested tier capacity exceeds the physical host RAM limit")
            }
            ResizeError::ExceedsVramCapacity => {
                f.write_str("requested tier capacity exceeds the physical VRAM limit")
            }
        }
    }
}

impl core::error::Error for ResizeError {}

/// Errors that can occur during tier migration speed validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MigrationError {
    /// The requested tier migration speed exceeds the physical bus bandwidth limit.
    ExceedsBusBandwidth,
}

impl core::fmt::Display for MigrationError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            MigrationError::ExceedsBusBandwidth => f.write_str(
                "requested tier migration speed exceeds the physical bus bandwidth limit",
            ),
        }
    }
}

impl core::error::Error for MigrationError {}

/// Validates that a requested dynamic tier migration speed is within the physical bus bandwidth limit.
pub fn validate_migration_speed(
    requested_bytes_per_sec: u64,
    bus_bandwidth_bytes_per_sec: u64,
) -> Result<(), MigrationError> {
    if requested_bytes_per_sec > bus_bandwidth_bytes_per_sec {
        return Err(MigrationError::ExceedsBusBandwidth);
    }
    Ok(())
}

/// Validates that a requested tier resize is within physical hardware limits.
pub fn validate_tier_resize(
    tier: Tier,
    requested_bytes: u64,
    host_ram_bytes: u64,
    vram_capacity_bytes: u64,
) -> Result<(), ResizeError> {
    if requested_bytes > host_ram_bytes {
        return Err(ResizeError::ExceedsHostRamCapacity);
    }
    if tier == Tier::Vram && requested_bytes > vram_capacity_bytes {
        return Err(ResizeError::ExceedsVramCapacity);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;
    /// A1 requirement for a 1 GiB evacuation: `V + max(ceil(0.10*V), 512 MiB)`.
    /// Pinned by `a1_requirement_is_the_nbd_demotion_formula` below.
    const GIB_REQUIRED: u64 = GIB + 512 * 1024 * 1024;

    #[test]
    fn a1_requirement_is_the_nbd_demotion_formula() {
        // One source of truth: `up` and `boot` must require the same amount.
        assert_eq!(minimum_lower_tier_bytes(GIB), Ok(GIB_REQUIRED));
    }

    #[test]
    fn absorber_free_covering_the_requirement_is_the_safety_net() {
        let net = vram_safety_net(GIB_REQUIRED, 0, GIB);
        assert_eq!(net, SafetyNet::VhdxBelow);
        assert!(net.is_safe());
    }

    #[test]
    fn absorber_present_but_full_is_not_a_safety_net() {
        // Regression: A1 is capacity, not presence. A sink that exists but has no
        // free space cannot receive the evacuation — `swapoff` would stall exactly
        // while a host consumer is demanding the VRAM back. 100 MiB free must not
        // certify a 1 GiB evacuation, and with RAM also short it is None.
        let net = vram_safety_net(100 * 1024 * 1024, 0, GIB);
        assert_eq!(net, SafetyNet::None);
        assert!(!net.is_safe());
    }

    #[test]
    fn bare_evacuation_size_is_short_of_the_demotion_margin() {
        // Bare V is not enough: the drain runs while the host is still allocating,
        // so the requirement carries a margin. An absorber holding exactly V must
        // not be certified as VhdxBelow.
        assert_eq!(vram_safety_net(GIB, 0, GIB), SafetyNet::None);
        assert_eq!(vram_safety_net(GIB_REQUIRED - 1, 0, GIB), SafetyNet::None);
    }

    #[test]
    fn zero_or_unrepresentable_vram_refuses_fail_closed() {
        assert_eq!(vram_safety_net(u64::MAX, u64::MAX, 0), SafetyNet::None);
        assert_eq!(
            vram_safety_net(u64::MAX, u64::MAX, u64::MAX),
            SafetyNet::None
        );
    }

    #[test]
    fn short_absorber_falls_through_to_ram_headroom() {
        // swap=0 in .wslconfig, but 4 GiB of free RAM covers a 1 GiB evacuation.
        let net = vram_safety_net(0, 4 * GIB, GIB);
        assert_eq!(net, SafetyNet::RamHeadroom);
        assert!(net.is_safe());
    }

    #[test]
    fn no_absorber_and_no_ram_is_unsafe() {
        // swap disabled and insufficient RAM: arming VRAM would trigger OOM during DEMOTE.
        let net = vram_safety_net(0, 256 * 1024 * 1024, GIB);
        assert_eq!(net, SafetyNet::None);
        assert!(!net.is_safe());
    }

    #[test]
    fn ram_covering_the_requirement_is_safe() {
        assert_eq!(
            vram_safety_net(0, GIB_REQUIRED, GIB),
            SafetyNet::RamHeadroom
        );
    }

    #[test]
    fn ram_just_short_of_the_requirement_is_unsafe() {
        assert_eq!(vram_safety_net(0, GIB_REQUIRED - 1, GIB), SafetyNet::None);
    }

    #[test]
    fn absorber_free_has_precedence_over_ram_headroom() {
        assert_eq!(
            vram_safety_net(GIB_REQUIRED, 4 * GIB, GIB),
            SafetyNet::VhdxBelow
        );
    }

    #[test]
    fn free_absorber_just_short_falls_through_not_vhdx_below() {
        // One byte short of the requirement must not claim the durable sink.
        assert_eq!(vram_safety_net(GIB_REQUIRED - 1, 0, GIB), SafetyNet::None);
        assert_eq!(
            vram_safety_net(GIB_REQUIRED - 1, GIB_REQUIRED, GIB),
            SafetyNet::RamHeadroom
        );
    }

    #[test]
    fn ublk_service_is_not_a_product_dependency() {
        assert_eq!(Tier::Vram.product_transport(), Some(ProductTransport::Nbd));
        assert_eq!(Tier::Zram.product_transport(), None);
        assert_eq!(Tier::Vhdx.product_transport(), None);
    }

    #[test]
    fn migration_speed_within_bandwidth_limit_is_ok() {
        assert_eq!(validate_migration_speed(GIB, 2 * GIB), Ok(()));
        assert_eq!(validate_migration_speed(GIB, GIB), Ok(()));
        assert_eq!(validate_migration_speed(0, 0), Ok(()));
        assert_eq!(validate_migration_speed(0, GIB), Ok(()));
        assert_eq!(validate_migration_speed(u64::MAX, u64::MAX), Ok(()));
    }

    #[test]
    fn migration_speed_exceeding_bandwidth_fails() {
        assert_eq!(
            validate_migration_speed(2 * GIB, GIB),
            Err(MigrationError::ExceedsBusBandwidth)
        );
        assert_eq!(
            validate_migration_speed(GIB + 1, GIB),
            Err(MigrationError::ExceedsBusBandwidth)
        );
        assert_eq!(
            validate_migration_speed(GIB, 0),
            Err(MigrationError::ExceedsBusBandwidth)
        );
        assert_eq!(
            validate_migration_speed(u64::MAX, u64::MAX - 1),
            Err(MigrationError::ExceedsBusBandwidth)
        );
    }

    #[test]
    fn migration_error_display_and_error_trait() {
        let err = MigrationError::ExceedsBusBandwidth;
        assert_eq!(
            err.to_string(),
            "requested tier migration speed exceeds the physical bus bandwidth limit"
        );
        let std_err: &dyn std::error::Error = &err;
        assert!(std_err.source().is_none());
    }

    #[test]
    fn migration_speed_edge_cases() {
        // Zero requested speed is always valid
        assert_eq!(validate_migration_speed(0, 0), Ok(()));
        assert_eq!(validate_migration_speed(0, GIB), Ok(()));

        // Off-by-one boundary conditions
        assert_eq!(validate_migration_speed(GIB, GIB), Ok(()));
        assert_eq!(
            validate_migration_speed(GIB + 1, GIB),
            Err(MigrationError::ExceedsBusBandwidth)
        );

        // Maximum integer bounds
        assert_eq!(validate_migration_speed(u64::MAX, u64::MAX), Ok(()));
        assert_eq!(
            validate_migration_speed(u64::MAX, u64::MAX - 1),
            Err(MigrationError::ExceedsBusBandwidth)
        );
    }

    #[test]
    fn migration_error_display_and_trait() {
        let err = MigrationError::ExceedsBusBandwidth;
        assert_eq!(
            err.to_string(),
            "requested tier migration speed exceeds the physical bus bandwidth limit"
        );
        let _err_trait: &dyn std::error::Error = &err;
    }

    #[test]
    fn tier_resize_within_physical_limits_is_ok() {
        assert_eq!(validate_tier_resize(Tier::Zram, 0, GIB, GIB), Ok(()));
        assert_eq!(validate_tier_resize(Tier::Vram, GIB, GIB, GIB), Ok(()));
        assert_eq!(validate_tier_resize(Tier::Vhdx, GIB / 2, GIB, GIB), Ok(()));
        assert_eq!(validate_tier_resize(Tier::Vram, GIB / 2, GIB, GIB), Ok(()));
    }

    #[test]
    fn tier_resize_exceeding_host_ram_fails() {
        assert_eq!(
            validate_tier_resize(Tier::Zram, GIB + 1, GIB, GIB),
            Err(ResizeError::ExceedsHostRamCapacity)
        );
        assert_eq!(
            validate_tier_resize(Tier::Vram, GIB + 1, GIB, 2 * GIB),
            Err(ResizeError::ExceedsHostRamCapacity)
        );
        assert_eq!(
            validate_tier_resize(Tier::Vhdx, 2 * GIB, GIB, GIB),
            Err(ResizeError::ExceedsHostRamCapacity)
        );
    }

    #[test]
    fn tier_resize_exceeding_vram_fails() {
        assert_eq!(
            validate_tier_resize(Tier::Vram, GIB + 1, 2 * GIB, GIB),
            Err(ResizeError::ExceedsVramCapacity)
        );
    }

    #[test]
    fn non_vram_tier_can_exceed_vram_capacity_if_within_host_ram() {
        assert_eq!(
            validate_tier_resize(Tier::Zram, 2 * GIB, 4 * GIB, GIB),
            Ok(())
        );
        assert_eq!(
            validate_tier_resize(Tier::Vhdx, 3 * GIB, 4 * GIB, GIB),
            Ok(())
        );
    }

    #[test]
    fn free_absorber_returns_early_ignoring_ram() {
        // Even with insufficient RAM, covering absorber free capacity returns early.
        let net = vram_safety_net(GIB_REQUIRED, 0, GIB);
        assert_eq!(net, SafetyNet::VhdxBelow);
    }
}
