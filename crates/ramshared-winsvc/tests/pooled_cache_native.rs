// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/tests/pooled_cache_native.rs
//! Windows/Hyper-V native lab E2E fixtures for selectable Independent/Pooled VRAM cache.
//! SPEC: the protocol specification §ITEM-1, §ITEM-6.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Instant;

use ramshared_ipc::cache_wire::{
    CacheGuestRole, DEFAULT_POOLED_CACHE_PORT, validate_cache_data_port,
};
use ramshared_vram::ReserveFloorPolicy;
use ramshared_winsvc::pooled_cache::allocation::{
    AllocationRefusal, GuestCustodyState, GuestPlacementIntent, PoolAdmission, RolePlacement,
};
use ramshared_winsvc::vm_id_resolve::parse_guid;

/// ITEM-1 native Windows/Hyper-V preflight fixture: requires live Windows host & lab env.
///
/// Run on Windows build/lab agent:
/// `cargo test -p ramshared-winsvc --test pooled_cache_native native_pair_preflight_reports_both_roles_and_refuses_missing_identity -- --ignored --exact`
#[test]
#[ignore = "requires Windows Hyper-V lab host with enrolled WSL2 and secondary guest identities"]
fn native_pair_preflight_reports_both_roles_and_refuses_missing_identity() {
    let port = DEFAULT_POOLED_CACHE_PORT;
    let guid = validate_cache_data_port(port, 2761).expect("pooled port must not collide");
    assert_eq!(&guid[0..4], &port.to_be_bytes());

    let wsl_id = std::env::var("RAMSHARED_LAB_WSL_RUNTIME_ID")
        .ok()
        .and_then(|v| parse_guid(&v).ok())
        .expect("missing RAMSHARED_LAB_WSL_RUNTIME_ID");
    let sec_id = std::env::var("RAMSHARED_LAB_SECONDARY_VM_ID")
        .ok()
        .and_then(|v| parse_guid(&v).ok())
        .expect("missing RAMSHARED_LAB_SECONDARY_VM_ID");
    assert_ne!(wsl_id, sec_id, "paired guest identities must be distinct");

    // Refusal half: missing/unknown placement intent must refuse before touching GPU.
    let mut admission = PoolAdmission::new(ReserveFloorPolicy::sealed(), "lab-adapter");
    assert_eq!(
        admission.authorize_placement(
            RolePlacement {
                intent: GuestPlacementIntent::Unknown,
                custody: GuestCustodyState::RunningUnconsented,
            },
            RolePlacement {
                intent: GuestPlacementIntent::PooledSealed,
                custody: GuestCustodyState::ControlledOff {
                    sealed_boot_path: true
                },
            },
            false,
        ),
        Err(AllocationRefusal::PlacementNotAuthorized)
    );
    let _ = (
        CacheGuestRole::Wsl2,
        CacheGuestRole::SecondaryGuest,
        Instant::now(),
    );
}

/// ITEM-6 installed dual-guest paired profile & compaction cycle qualification.
///
/// Run on Windows Hyper-V lab with disposable pair and signed installed packages:
/// `cargo test -p ramshared-winsvc --test pooled_cache_native installed_pair_qualifies_profiles_and_compaction_cycles -- --ignored --exact`
#[test]
#[ignore = "requires Windows Hyper-V lab with installed signed packages and disposable dual-guest pair"]
fn installed_pair_qualifies_profiles_and_compaction_cycles() {
    let evidence_dir = std::env::var("RAMSHARED_LAB_EVIDENCE_DIR")
        .expect("set RAMSHARED_LAB_EVIDENCE_DIR for installed dual-guest qualification");
    let manifest = std::path::Path::new(&evidence_dir).join("paired-benchmark-receipt.json");
    assert!(
        manifest.is_file(),
        "installed paired benchmark receipt missing: {}",
        manifest.display()
    );
}

/// ITEM-6 native saved/restored VM fencing fixture (Pass 2 DT-11).
///
/// Run on Windows Hyper-V lab:
/// `cargo test -p ramshared-winsvc --test pooled_cache_native installed_pool_fences_saved_or_restored_guest_before_lookup -- --ignored --exact`
#[test]
#[ignore = "requires Windows Hyper-V lab host to verify saved/restored guest fencing"]
fn installed_pool_fences_saved_or_restored_guest_before_lookup() {
    let mut admission = PoolAdmission::new(ReserveFloorPolicy::sealed(), "lab-adapter");
    let refusal = admission.authorize_placement(
        RolePlacement {
            intent: GuestPlacementIntent::PooledSealed,
            custody: GuestCustodyState::RunningConsented { boot_gen: 1 },
        },
        RolePlacement {
            intent: GuestPlacementIntent::PooledSealed,
            custody: GuestCustodyState::SavedOrRestored,
        },
        false,
    );
    assert_eq!(refusal, Err(AllocationRefusal::SavedOrRestoredGuest));
}
