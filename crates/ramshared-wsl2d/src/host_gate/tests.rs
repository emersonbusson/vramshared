// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/host_gate/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

fn make_manifest_json(capacity_mib: u64) -> Vec<u8> {
    serde_json::json!({
        "logical_capacity_mib": capacity_mib,
        "partuuid": "11111111-2222-3333-4444-555555555555",
        "origin_vhdx": "C:\\ramshared\\origin.vhdx"
    })
    .to_string()
    .into_bytes()
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn validate_origin_manifest_matches_script() {
    let data = make_manifest_json(4096);
    let hash = sha256_hex(&data);
    let result = validate_origin_manifest(&data, &hash).expect("valid manifest");
    assert_eq!(result.logical_capacity_mib, 4096);
    assert_eq!(result.partuuid, "11111111-2222-3333-4444-555555555555");
    assert_eq!(result.origin_vhdx, "C:\\ramshared\\origin.vhdx");
}

#[test]
fn validate_origin_manifest_rejects_bad_hash() {
    let data = make_manifest_json(4096);
    assert!(matches!(
        validate_origin_manifest(&data, "bad-hash"),
        Err(GateError::ManifestHashMismatch)
    ));
}

#[test]
fn validate_origin_manifest_rejects_empty() {
    assert!(matches!(
        validate_origin_manifest(b"", "any"),
        Err(GateError::ManifestInvalid(_))
    ));
}

#[test]
fn validate_origin_manifest_rejects_oversized() {
    let big = vec![0u8; 64 * 1024 + 1];
    assert!(matches!(
        validate_origin_manifest(&big, "any"),
        Err(GateError::ManifestInvalid(_))
    ));
}

#[test]
fn validate_origin_manifest_rejects_missing_fields() {
    let data = b"{\"logical_capacity_mib\": 100}";
    let hash = sha256_hex(data);
    assert!(matches!(
        validate_origin_manifest(data, &hash),
        Err(GateError::ManifestInvalid(_))
    ));
}

#[test]
fn check_guardian_health_rejects_stale() {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let stale = GuardianHealthStatus {
        timestamp_ms: now_ms - 120_000, // 120s old
        healthy: true,
    };
    assert!(matches!(
        check_guardian_health(&stale, 60),
        Err(GateError::GuardianStale)
    ));
}

#[test]
fn check_guardian_health_rejects_unhealthy() {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let unhealthy = GuardianHealthStatus {
        timestamp_ms: now_ms,
        healthy: false,
    };
    assert!(matches!(
        check_guardian_health(&unhealthy, 60),
        Err(GateError::GuardianUnhealthy)
    ));
}

#[test]
fn check_guardian_health_accepts_fresh() {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let fresh = GuardianHealthStatus {
        timestamp_ms: now_ms,
        healthy: true,
    };
    assert!(check_guardian_health(&fresh, 60).is_ok());
}

#[test]
fn evaluate_safe_mode_refuses_foreign_boot_id() {
    assert!(matches!(
        evaluate_safe_mode(false, "boot-abc", "boot-xyz"),
        Err(GateError::SafeModeForeignBootId)
    ));
}

#[test]
fn evaluate_safe_mode_allows_matching_boot_id() {
    assert_eq!(
        evaluate_safe_mode(false, "boot-abc", "boot-abc"),
        Ok(SafeModeDecision::Allow)
    );
}

#[test]
fn evaluate_safe_mode_denies_when_safe_mode_active() {
    assert_eq!(
        evaluate_safe_mode(true, "boot-abc", "boot-abc"),
        Ok(SafeModeDecision::Deny)
    );
}

#[test]
fn mint_lease_requires_all_gates() {
    let origin = SealedOrigin {
        logical_capacity_mib: 4096,
        partuuid: "p".into(),
        origin_vhdx: "v".into(),
    };

    // Happy path
    let lease =
        mint_lease(&origin, true, SafeModeDecision::Allow, 1, 15_000).expect("lease should mint");
    assert_eq!(lease.lease_id, 1);

    // Guardian not healthy
    assert!(matches!(
        mint_lease(&origin, false, SafeModeDecision::Allow, 1, 15_000),
        Err(GateError::LeaseDenied(_))
    ));

    // Safe mode active
    assert!(matches!(
        mint_lease(&origin, true, SafeModeDecision::Deny, 1, 15_000),
        Err(GateError::LeaseDenied(_))
    ));

    // Invalid origin
    let bad_origin = SealedOrigin {
        logical_capacity_mib: 0,
        partuuid: "p".into(),
        origin_vhdx: "v".into(),
    };
    assert!(matches!(
        mint_lease(&bad_origin, true, SafeModeDecision::Allow, 1, 15_000),
        Err(GateError::LeaseDenied(_))
    ));
}

#[test]
fn lease_expiry_is_detected() {
    let origin = SealedOrigin {
        logical_capacity_mib: 4096,
        partuuid: "p".into(),
        origin_vhdx: "v".into(),
    };
    let lease = mint_lease(&origin, true, SafeModeDecision::Allow, 1, 0).expect("mint");
    // TTL 0 means deadline == now → already expired
    assert!(lease_expired(&lease));
}

#[test]
fn host_gate_shadow_comparison() {
    // Shadow comparison: validate_origin_manifest must produce identical
    // decisions to the Python logic in ramshared-host-gate.sh on known fixtures.
    let fixtures: Vec<(Vec<u8>, &str, bool)> = vec![
        (make_manifest_json(4096), "", true), // valid (hash computed below)
        (make_manifest_json(0), "", false),   // zero capacity
        (b"{}".to_vec(), "", false),          // missing fields
        (vec![0u8; 100_000], "", false),      // oversized
    ];

    for (data, _placeholder, expect_valid) in fixtures {
        let hash = sha256_hex(&data);
        let result = validate_origin_manifest(&data, &hash);
        if expect_valid {
            assert!(result.is_ok(), "fixture should be valid: {data:?}");
        } else {
            assert!(result.is_err(), "fixture should be invalid");
        }
    }
}

// --- adopt_verified_origin (RF-3) -----------------------------------

fn origin_fixture() -> SealedOrigin {
    SealedOrigin {
        logical_capacity_mib: 4096,
        partuuid: "p".into(),
        origin_vhdx: "v".into(),
    }
}

#[test]
fn a_verified_origin_admitted_at_construction_survives_lease_loss_and_disconnect() {
    let mut authority = ControlPlaneAuthority::disconnected();
    assert!(
        authority.admit_origin_io().is_err(),
        "no origin yet: origin I/O must be refused"
    );

    authority.adopt_verified_origin(origin_fixture());
    assert!(authority.admit_origin_io().is_ok());

    // RF-3: a transport disconnect must not take the authoritative path
    // down. Only the verified origin identity may.
    authority.on_vsock_disconnect();
    assert_eq!(authority.state(), ControlPlaneState::SafeMode);
    assert!(
        authority.admit_origin_io().is_ok(),
        "disconnect must preserve the verified origin identity"
    );
    assert!(authority.admit_cache(1_000).is_err());

    // And a lease loss leaves the origin path up as well.
    let mut leased = ControlPlaneAuthority::disconnected();
    leased.adopt_verified_origin(origin_fixture());
    leased
        .accept_lease(
            LeaseToken {
                lease_id: 7,
                deadline_ms: 60_000,
            },
            origin_fixture(),
            1_000,
        )
        .expect("accept lease");
    assert!(leased.admit_cache(1_000).is_ok());
    leased.on_lease_expired();
    assert_eq!(leased.state(), ControlPlaneState::OriginOnly);
    assert!(leased.admit_cache(1_000).is_err());
    assert!(leased.admit_origin_io().is_ok());
}

#[test]
fn losing_the_origin_identity_blocks_every_path() {
    let mut authority = ControlPlaneAuthority::disconnected();
    authority.adopt_verified_origin(origin_fixture());
    assert!(authority.admit_origin_io().is_ok());
    authority.lose_origin_identity();
    assert!(matches!(
        authority.admit_origin_io(),
        Err(GateError::OriginAuthorityRevoked)
    ));
    // Cache is refused before a lease exists too; the refusal code is the
    // lease check, which is still fail-closed.
    assert!(authority.admit_cache(1_000).is_err());
}

#[test]
fn origin_loss_on_a_leased_authority_is_reported_as_origin_authority_revoked() {
    // The informative case: the lease is live and unexpired, so the only
    // reason cache can be refused is the lost origin identity.
    let mut authority = ControlPlaneAuthority::disconnected();
    authority.adopt_verified_origin(origin_fixture());
    authority
        .accept_lease(
            LeaseToken {
                lease_id: 9,
                deadline_ms: 60_000,
            },
            origin_fixture(),
            1_000,
        )
        .expect("accept lease");
    assert!(authority.admit_cache(1_000).is_ok());

    authority.lose_origin_identity();
    assert!(matches!(
        authority.admit_cache(1_000),
        Err(GateError::OriginAuthorityRevoked)
    ));
    assert!(matches!(
        authority.admit_origin_io(),
        Err(GateError::OriginAuthorityRevoked)
    ));
}

// SPEC control-plane-and-release-resilience T4 (RF-4/DT-4): a superseded
// lease session must not clobber a newer one. This is the stale-thread
// race the re-lease loop closes.
#[test]
fn stale_session_cannot_clobber_newer_lease() {
    let shared = SharedAuthority::new();
    let stale = shared.begin_session();
    let fresh = shared.begin_session(); // supersedes `stale`

    // The stale session's writes are all no-ops.
    assert!(
        !stale.mutate(|a| a.on_lease_expired()),
        "a stale session must not expire the newer lease"
    );
    assert!(
        !stale.publish(&ControlPlaneAuthority::disconnected()),
        "a stale session must not overwrite the newer authority"
    );
    let flag = std::sync::atomic::AtomicBool::new(false);
    assert!(
        !stale.set_lease_lost_if_current(&flag),
        "a stale session must not mark the newer lease lost"
    );
    assert!(!flag.load(std::sync::atomic::Ordering::Acquire));
    assert!(!stale.retire(|_| panic!("stale retirement cannot touch replacement")));

    // The fresh session's writes still land.
    assert!(fresh.mutate(|a| {
        a.adopt_verified_origin(origin_fixture());
        a.accept_lease(
            LeaseToken {
                lease_id: 1,
                deadline_ms: 60_000,
            },
            origin_fixture(),
            1_000,
        )
        .expect("accept lease")
    }));
    assert_eq!(
        shared.slot().lock().expect("slot").state(),
        ControlPlaneState::VsockLeased,
        "the fresh session's lease must be the live one"
    );
}

#[test]
fn retired_session_rejects_every_writer_and_allows_final_cleanup() {
    let shared = SharedAuthority::new();
    let session = shared.begin_session();
    let late = session.clone();
    assert!(session.retire(|slot| slot.adopt_verified_origin(origin_fixture())));
    assert!(!late.is_current());
    assert!(!late.mutate(|_| panic!("retired mutation")));
    assert!(!late.publish(&ControlPlaneAuthority::disconnected()));
    assert!(!late.lease_is_current(1));
    let lost = std::sync::atomic::AtomicBool::new(false);
    assert!(!late.set_lease_lost_if_current(&lost));
    assert!(!lost.load(std::sync::atomic::Ordering::Acquire));
    assert!(late.retire(|slot| slot.on_lease_expired()));
    assert!(shared.slot().lock().unwrap().admit_origin_io().is_ok());
}

// A poisoned authority slot is an observable failure, not a silent
// success: a writer that could not take the lock must report false so the
// caller does not believe a write landed.
#[test]
fn poisoned_slot_refuses_the_write_instead_of_claiming_success() {
    let shared = SharedAuthority::new();
    let slot = shared.slot();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = slot.lock().expect("slot");
        panic!("poison the authority slot on purpose");
    }));
    assert!(shared.slot().lock().is_err(), "the slot must be poisoned");

    let session = shared.begin_session();
    assert!(
        !session.publish(&ControlPlaneAuthority::disconnected()),
        "a poisoned slot must not report a published authority"
    );
    assert!(
        !session.mutate(|a| a.on_lease_expired()),
        "a poisoned slot must not report a mutation"
    );
}

// SPEC T5 (RF-5): lease loss revokes only the cache, keeps origin I/O, and
// re-acquisition restores cache admission.
#[test]
fn lease_loss_keeps_origin_io_and_reacquisition_restores_cache() {
    let mut authority = ControlPlaneAuthority::disconnected();
    authority.adopt_verified_origin(origin_fixture());
    authority
        .accept_lease(
            LeaseToken {
                lease_id: 1,
                deadline_ms: 60_000,
            },
            origin_fixture(),
            1_000,
        )
        .expect("accept lease");
    assert!(authority.admit_cache(1_000).is_ok());

    // Loss revokes the cache but keeps the verified origin path.
    authority.on_lease_expired();
    assert!(authority.admit_cache(1_000).is_err());
    assert!(authority.admit_origin_io().is_ok());

    // Re-acquisition (a new accepted lease) restores the cache.
    authority
        .accept_lease(
            LeaseToken {
                lease_id: 2,
                deadline_ms: 120_000,
            },
            origin_fixture(),
            2_000,
        )
        .expect("re-accept lease");
    assert_eq!(authority.state(), ControlPlaneState::VsockLeased);
    assert!(authority.admit_cache(2_000).is_ok());
}
