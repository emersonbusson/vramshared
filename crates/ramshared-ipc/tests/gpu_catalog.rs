// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-ipc/tests/gpu_catalog.rs
//! Session authentication is metadata proof, never GPU allocation authority.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use ramshared_ipc::gpu_catalog::{CatalogSession, CatalogTiming};
use ramshared_ipc::{HandshakeTranscript, HeartbeatAck, HostChallenge, HostMemoryTelemetry};
use ramshared_vram::host_identity::{
    HostGpuIdentity, HostGpuIdentityReport, HostGpuObservation, ProducerProcess,
};
use ramshared_vram::{GPU_BUDGET_MAX_AGE_MS, GpuAdapterIdentity};
use std::time::{Duration, Instant};

const SECRET: &[u8] = b"catalog-test-secret";
fn session(lease: u32, nonce: u8) -> CatalogSession {
    CatalogSession::new(
        SECRET,
        &HandshakeTranscript {
            min_version: 2,
            max_version: 4,
            boot_id: "boot".into(),
            distro_id: "distro".into(),
            guest_nonce: [1; 32],
        },
        &HostChallenge {
            accepted_version: 4,
            heartbeat_secs: 5,
            lease_timeout_secs: 15,
            host_nonce: [nonce; 32],
        },
        lease,
    )
    .unwrap()
}
fn ack() -> HeartbeatAck {
    HeartbeatAck {
        timestamp_ms: 100,
        lease_remaining_ms: 15_000,
        host_memory: None,
    }
}
fn observation(age_ms: u64) -> HostGpuObservation {
    HostGpuObservation {
        age_ms,
        report: HostGpuIdentityReport {
            schema_version: 1,
            producer: ProducerProcess {
                pid: 42,
                creation_time_100ns: 77,
            },
            started_at_unix_ms: 100,
            elapsed_ms: 2,
            adapters: vec![HostGpuIdentity {
                ordinal: 0,
                allocator: GpuAdapterIdentity {
                    backend: "cuda".into(),
                    key: format!("{:032x}", 1),
                    luid: Some("00000000:00000001".into()),
                    node_mask: Some(1),
                },
                interface_id: "device-1".into(),
            }],
        },
    }
}
fn timing(started: Instant, ms: u64) -> CatalogTiming {
    CatalogTiming {
        started,
        received: started + Duration::from_millis(ms),
    }
}

#[test]
fn signed_host_memory_is_bound_to_ack_and_original_freshness() {
    let start = Instant::now();
    let mut heartbeat = ack();
    heartbeat.lease_remaining_ms = 1_000;
    heartbeat.host_memory = Some(HostMemoryTelemetry {
        sampled_at_unix_ms: 10_000,
        total_bytes: 1000,
        available_bytes: 800,
        commit_total_bytes: 100,
        commit_limit_bytes: 1200,
        page_size_bytes: 4096,
        vmmem_wsl_working_set_bytes: None,
    });
    let wire = session(7, 2).seal(SECRET, &heartbeat, None).unwrap();
    let mut changed = heartbeat.clone();
    changed.host_memory.as_mut().unwrap().available_bytes = 900;
    assert!(
        session(7, 2)
            .open_verified(SECRET, &changed, &wire, 4, timing(start, 100))
            .is_err()
    );
    let verified = session(7, 2)
        .open_verified(SECRET, &heartbeat, &wire, 4, timing(start, 100))
        .unwrap();
    assert!(verified.catalog.is_none());
    let memory = verified.host_memory.unwrap();
    assert_eq!(
        memory
            .sample(start + Duration::from_millis(100), 10_001)
            .unwrap()
            .available_bytes,
        800
    );
    assert!(
        memory
            .sample(start + Duration::from_millis(99), 10_001)
            .is_none()
    );
    assert!(
        memory
            .sample(start + Duration::from_millis(999), 9_999)
            .is_none()
    );
    assert!(
        memory
            .sample(start + Duration::from_millis(999), 15_000)
            .is_none()
    );
    assert!(
        memory
            .sample(start + Duration::from_millis(1_000), 10_001)
            .is_none()
    );
}

#[test]
fn signed_but_impossible_host_memory_never_becomes_a_permit() {
    let start = Instant::now();
    let mut heartbeat = ack();
    heartbeat.host_memory = Some(HostMemoryTelemetry {
        sampled_at_unix_ms: 10_000,
        total_bytes: 1000,
        available_bytes: 1001,
        commit_total_bytes: 100,
        commit_limit_bytes: 1200,
        page_size_bytes: 4096,
        vmmem_wsl_working_set_bytes: None,
    });
    let wire = session(7, 2).seal(SECRET, &heartbeat, None).unwrap();
    let verified = session(7, 2)
        .open_verified(SECRET, &heartbeat, &wire, 4, timing(start, 1))
        .unwrap();
    assert!(verified.host_memory.is_none());
}
#[test]
fn catalog_round_trip_preserves_age_and_unavailable() {
    let start = Instant::now();
    let mut host = session(7, 2);
    let mut guest = session(7, 2);
    let wire = host.seal(SECRET, &ack(), Some(observation(100))).unwrap();
    let catalog = guest
        .open(SECRET, &ack(), &wire, 4, timing(start, 10))
        .unwrap()
        .unwrap();
    let received = start + Duration::from_millis(10);
    assert_eq!(catalog.report(received).unwrap(), &observation(100).report);
    assert!(
        catalog
            .report(received - Duration::from_millis(1))
            .is_none()
    );
    assert!(
        catalog
            .report(start + Duration::from_millis(GPU_BUDGET_MAX_AGE_MS - 100))
            .is_none()
    );
    assert!(
        catalog
            .report(start + Duration::from_millis(4000))
            .is_some()
    );
    assert_eq!(host.round(), 5);
    assert_eq!(guest.round(), 5);
    assert!(
        guest
            .open(SECRET, &ack(), &wire, 4, timing(start, 10))
            .is_err()
    );
    let missing = host.seal(SECRET, &ack(), None).unwrap();
    assert!(
        guest
            .open(SECRET, &ack(), &missing, 5, timing(start, 11))
            .unwrap()
            .is_none()
    );
    assert_eq!(guest.round(), 6);
}
#[test]
fn catalog_refuses_other_session_lease_ack_sequence_and_tampering() {
    let start = Instant::now();
    let wire = session(7, 2)
        .seal(SECRET, &ack(), Some(observation(100)))
        .unwrap();
    assert!(
        session(7, 3)
            .open(SECRET, &ack(), &wire, 4, timing(start, 1))
            .is_err()
    );
    assert!(
        session(8, 2)
            .open(SECRET, &ack(), &wire, 4, timing(start, 1))
            .is_err()
    );
    let mut modified = ack();
    modified.lease_remaining_ms += 1;
    assert!(
        session(7, 2)
            .open(SECRET, &modified, &wire, 4, timing(start, 1))
            .is_err()
    );
    assert!(
        session(7, 2)
            .open(b"wrong", &ack(), &wire, 4, timing(start, 1))
            .is_err()
    );
    for field in [
        "round",
        "lease_id",
        "age",
        "proof",
        "unknown",
        "duplicate",
        "ack_unknown",
        "report_unknown",
        "proof_short",
    ] {
        let mut json: serde_json::Value = serde_json::from_slice(&wire).unwrap();
        match field {
            "round" => json["body"]["round"] = 9.into(),
            "lease_id" => json["body"]["lease_id"] = 9.into(),
            "age" => json["body"]["observation"]["age_ms"] = 0.into(),
            "proof" => json["proof"][0] = 255.into(),
            "unknown" => json["extra"] = true.into(),
            "ack_unknown" => json["body"]["ack"]["extra"] = true.into(),
            "report_unknown" => json["body"]["observation"]["report"]["extra"] = true.into(),
            "proof_short" => json["proof"] = serde_json::json!([1]),
            "duplicate" => {
                json["body"]["observation"]["report"]["adapters"][0]["ordinal"] = 8.into()
            }
            _ => unreachable!(),
        }
        let changed = serde_json::to_vec(&json).unwrap();
        let mut guest = session(7, 2);
        assert!(
            guest
                .open(SECRET, &ack(), &changed, 4, timing(start, 1))
                .is_err(),
            "{field}"
        );
        assert_eq!(guest.round(), 4);
        assert!(
            guest
                .open(SECRET, &ack(), &wire, 4, timing(start, 1))
                .unwrap()
                .is_some()
        );
    }
}
#[test]
fn catalog_bounds_complete_maximum_inventory() {
    let start = Instant::now();
    let mut sample = observation(100);
    sample.report.adapters = (0..64)
        .map(|index| HostGpuIdentity {
            ordinal: index,
            allocator: GpuAdapterIdentity {
                backend: "cuda".into(),
                key: format!("{:032x}", index + 1),
                luid: Some(format!("00000000:{:08x}", index + 1)),
                node_mask: Some(1),
            },
            interface_id: format!("{index:02}{}", "\u{e000}".repeat(257)),
        })
        .collect();
    let wire = session(7, 2).seal(SECRET, &ack(), Some(sample)).unwrap();
    assert!(wire.len() > 4096);
    assert!(wire.len() <= 65536);
    assert_eq!(
        session(7, 2)
            .open(SECRET, &ack(), &wire, 4, timing(start, 1))
            .unwrap()
            .unwrap()
            .report(start + Duration::from_millis(1))
            .unwrap()
            .adapters
            .len(),
        64
    );
    assert!(
        session(7, 2)
            .open(SECRET, &ack(), &vec![b' '; 65537], 4, timing(start, 1))
            .is_err()
    );
    for age in [0, 1, GPU_BUDGET_MAX_AGE_MS, u64::MAX] {
        assert!(
            session(7, 2)
                .seal(SECRET, &ack(), Some(observation(age)))
                .is_err()
        );
    }
    let wire = session(7, 2)
        .seal(SECRET, &ack(), Some(observation(GPU_BUDGET_MAX_AGE_MS - 1)))
        .unwrap();
    assert!(
        session(7, 2)
            .open(SECRET, &ack(), &wire, 4, timing(start, 1))
            .unwrap()
            .is_none()
    );
    assert!(
        session(7, 2)
            .open(
                SECRET,
                &ack(),
                &wire,
                4,
                CatalogTiming {
                    started: start,
                    received: start - Duration::from_millis(1)
                }
            )
            .is_err()
    );
}

#[test]
fn catalog_lifetime_never_outlives_its_authenticated_lease() {
    let start = Instant::now();
    let mut short = ack();
    short.lease_remaining_ms = 5;
    let wire = session(7, 2)
        .seal(SECRET, &short, Some(observation(100)))
        .unwrap();
    let catalog = session(7, 2)
        .open(SECRET, &short, &wire, 4, timing(start, 1))
        .unwrap()
        .unwrap();
    assert!(catalog.report(start + Duration::from_millis(4)).is_some());
    assert!(
        catalog.report(start + Duration::from_millis(5)).is_none(),
        "metadata access must end with the lease"
    );
    assert!(
        session(7, 2)
            .open(SECRET, &short, &wire, 4, timing(start, 5))
            .is_err()
    );
}

#[test]
fn catalog_requires_negotiated_version_nonempty_key_and_fresh_nonces() {
    let transcript = HandshakeTranscript {
        min_version: 2,
        max_version: 4,
        boot_id: "boot".into(),
        distro_id: "distro".into(),
        guest_nonce: [1; 32],
    };
    let challenge = HostChallenge {
        accepted_version: 4,
        heartbeat_secs: 5,
        lease_timeout_secs: 15,
        host_nonce: [2; 32],
    };
    assert!(CatalogSession::new(&[], &transcript, &challenge, 7).is_err());
    assert!(CatalogSession::new(SECRET, &transcript, &challenge, 0).is_err());
    for field in ["version", "window", "host_nonce", "guest_nonce"] {
        let mut t = transcript.clone();
        let mut c = challenge.clone();
        match field {
            "version" => c.accepted_version = 3,
            "window" => t.max_version = 3,
            "host_nonce" => c.host_nonce = [0; 32],
            "guest_nonce" => t.guest_nonce = [0; 32],
            _ => unreachable!(),
        }
        assert!(CatalogSession::new(SECRET, &t, &c, 7).is_err(), "{field}");
    }
    let start = Instant::now();
    let wire = session(7, 2)
        .seal(SECRET, &ack(), Some(observation(4998)))
        .unwrap();
    assert!(
        session(7, 2)
            .open(
                SECRET,
                &ack(),
                &wire,
                4,
                CatalogTiming {
                    started: start,
                    received: start + Duration::from_millis(1) + Duration::from_micros(500)
                }
            )
            .unwrap()
            .is_none()
    );
    assert!(
        session(7, 2)
            .open(SECRET, &ack(), b"not JSON", 4, timing(start, 1))
            .is_err()
    );
}

#[test]
fn catalog_expiry_in_transit_keeps_the_authenticated_round_and_memory() {
    let start = Instant::now();
    let mut heartbeat = ack();
    heartbeat.host_memory = Some(HostMemoryTelemetry {
        sampled_at_unix_ms: 100,
        total_bytes: 1024,
        available_bytes: 800,
        commit_total_bytes: 512,
        commit_limit_bytes: 2048,
        page_size_bytes: 4096,
        vmmem_wsl_working_set_bytes: None,
    });
    let mut host = session(7, 2);
    let mut guest = host.clone();
    let wire = host
        .seal(
            SECRET,
            &heartbeat,
            Some(observation(GPU_BUDGET_MAX_AGE_MS - 1)),
        )
        .unwrap();
    let received = start + Duration::from_millis(1);
    let expired = guest
        .open_verified(SECRET, &heartbeat, &wire, 4, timing(start, 1))
        .expect("an expired observation must not reject a valid heartbeat");
    assert!(expired.catalog.is_none());
    assert_eq!(
        expired
            .host_memory
            .unwrap()
            .sample(received, 101)
            .unwrap()
            .available_bytes,
        800
    );
    assert_eq!(guest.round(), 5);
    assert!(
        guest
            .open(SECRET, &heartbeat, &wire, 4, timing(start, 1))
            .is_err()
    );

    let fresh = host
        .seal(SECRET, &heartbeat, Some(observation(100)))
        .unwrap();
    assert!(
        guest
            .open(SECRET, &heartbeat, &fresh, 5, timing(start, 2))
            .unwrap()
            .is_some()
    );
}
