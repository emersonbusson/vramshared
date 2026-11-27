// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-ipc/tests/gpu_catalog.rs
//! Session authentication is metadata proof, never GPU allocation authority.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use ramshared_ipc::gpu_catalog::{CatalogSession, CatalogTiming, HostGpuActivityObservation};
use ramshared_ipc::{
    HandshakeTranscript, HeartbeatAck, HostChallenge, HostMemoryTelemetry, IPC_VERSION_4,
    IPC_VERSION_5, IPC_VERSION_6, IPC_VERSION_7, compute_hmac, host_proof,
};
use ramshared_vram::host_gpu_activity::{
    HostGpuActivityReport, HostGpuActivitySource, HostGpuAdapterActivity, HostGpuEngineActivity,
    HostGpuProcessActivity,
};
use ramshared_vram::host_identity::{
    HostGpuIdentity, HostGpuIdentityReport, HostGpuObservation, ProducerProcess,
};
use ramshared_vram::{GPU_BUDGET_MAX_AGE_MS, GpuAdapterIdentity};
use std::time::{Duration, Instant};

const SECRET: &[u8] = b"catalog-test-secret";
fn session(lease: u32, nonce: u8) -> CatalogSession {
    versioned_session(lease, nonce, IPC_VERSION_4)
}
fn transcript(version: u32) -> HandshakeTranscript {
    HandshakeTranscript {
        min_version: 2,
        max_version: version,
        boot_id: "boot".into(),
        distro_id: "distro".into(),
        guest_nonce: [1; 32],
    }
}
fn challenge(nonce: u8, version: u32) -> HostChallenge {
    HostChallenge {
        accepted_version: version,
        heartbeat_secs: 5,
        lease_timeout_secs: 15,
        host_nonce: [nonce; 32],
    }
}
fn versioned_session(lease: u32, nonce: u8, version: u32) -> CatalogSession {
    CatalogSession::new(
        SECRET,
        &transcript(version),
        &challenge(nonce, version),
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

fn activity(age_ms: u64) -> HostGpuActivityObservation {
    HostGpuActivityObservation {
        age_ms,
        report: HostGpuActivityReport {
            schema_version: 1,
            source: HostGpuActivitySource::WddmPdh,
            sampled_at_unix_ms: 10_000,
            interval_ms: 1_000,
            adapters: vec![HostGpuAdapterActivity {
                luid: "00000000:00000001".into(),
                physical_index: 0,
                engines: vec![HostGpuEngineActivity {
                    index: 7,
                    kind: "VideoEncode".into(),
                    busy_pct: 45,
                    processes: None,
                }],
            }],
        },
    }
}

#[test]
fn legacy_catalog_bytes_stay_compatible() {
    let heartbeat = ack();
    let legacy_body = format!(
        r#"{{"lease_id":7,"round":4,"ack":{},"observation":null}}"#,
        serde_json::to_string(&heartbeat).unwrap()
    );
    for version in [IPC_VERSION_4, IPC_VERSION_5] {
        let binding = host_proof(SECRET, &transcript(version), &challenge(2, version)).unwrap();
        let mut authenticated = b"ramshared-vsock-host-gpu-catalog-v1".to_vec();
        authenticated.extend_from_slice(&binding);
        authenticated.extend_from_slice(legacy_body.as_bytes());
        let proof = serde_json::to_string(&compute_hmac(SECRET, &authenticated)).unwrap();
        let legacy_wire = format!(r#"{{"body":{legacy_body},"proof":{proof}}}"#);
        let wire = versioned_session(7, 2, version)
            .seal_with_activity(SECRET, &heartbeat, None, Some(activity(50)))
            .unwrap();
        assert_eq!(wire, legacy_wire.as_bytes());
        assert!(
            versioned_session(7, 2, version)
                .open_verified(SECRET, &heartbeat, &wire, 4, timing(Instant::now(), 0))
                .unwrap()
                .activity
                .is_none()
        );
    }
}

#[test]
fn activity_is_bound_to_catalog_lease_and_round() {
    let mut heartbeat = ack();
    heartbeat.lease_remaining_ms = 1_000;
    let sample = activity(90);
    let mut host = versioned_session(7, 2, IPC_VERSION_6);
    let mut guest = host.clone();
    let wire = host
        .seal_with_activity(SECRET, &heartbeat, None, Some(sample.clone()))
        .unwrap();
    let started = Instant::now();
    let mut changed: serde_json::Value = serde_json::from_slice(&wire).unwrap();
    changed["body"]["activity"]["report"]["adapters"][0]["engines"][0]["busy_pct"] = 100.into();
    assert!(
        guest
            .open_verified(
                SECRET,
                &heartbeat,
                &serde_json::to_vec(&changed).unwrap(),
                4,
                timing(started, 100)
            )
            .is_err()
    );
    assert!(
        guest
            .open_verified(SECRET, &heartbeat, &wire, 5, timing(started, 100))
            .is_err()
    );
    assert!(
        versioned_session(8, 2, IPC_VERSION_6)
            .open_verified(SECRET, &heartbeat, &wire, 4, timing(started, 100))
            .is_err()
    );
    assert!(
        versioned_session(7, 2, IPC_VERSION_5)
            .open_verified(SECRET, &heartbeat, &wire, 4, timing(started, 100))
            .is_err()
    );
    assert_eq!(guest.round(), 4);

    let verified = guest
        .open_verified(SECRET, &heartbeat, &wire, 4, timing(started, 100))
        .unwrap()
        .activity
        .unwrap();
    let received = started + Duration::from_millis(100);
    assert_eq!(verified.report(received), Some(&sample.report));
    assert!(verified.report(started).is_none());
    assert!(
        verified
            .report(started + Duration::from_millis(1_000))
            .is_none()
    );
    assert!(
        guest
            .open_verified(SECRET, &heartbeat, &wire, 4, timing(started, 100))
            .is_err()
    );
}

#[test]
fn activity_freshness_uses_guest_time_with_skewed_host_clock() {
    let mut heartbeat = ack();
    heartbeat.lease_remaining_ms = 1_000;
    let sample = activity(90);
    let mut host = versioned_session(7, 2, IPC_VERSION_6);
    let mut guest = host.clone();
    let wire = host
        .seal_with_activity(SECRET, &heartbeat, None, Some(sample.clone()))
        .unwrap();
    let started = Instant::now();
    let verified = guest
        .open_verified(SECRET, &heartbeat, &wire, 4, timing(started, 100))
        .unwrap()
        .activity
        .unwrap();
    let now = started + Duration::from_millis(150);
    // Guest clocks on either side of the native Windows timestamp.
    for guest_unix_ms in [1_000, 100_000] {
        let telemetry = verified.telemetry(now, guest_unix_ms).unwrap();
        assert_eq!(telemetry.report, sample.report);
        assert_eq!(
            telemetry.sampled_at_guest_unix_ms,
            Some(guest_unix_ms - 240)
        );
        assert_eq!(telemetry.valid_before_unix_ms, guest_unix_ms + 850);
        assert_eq!(telemetry.report_at(guest_unix_ms), Some(&sample.report));
        assert!(telemetry.report_at(guest_unix_ms - 241).is_none());
        assert!(telemetry.report_at(guest_unix_ms + 850).is_none());
    }
    assert!(verified.telemetry(now, 240).is_none());
    assert!(verified.telemetry(now, u64::MAX).is_none());
}

#[test]
fn process_activity_is_negotiated_and_downgrades() {
    let mut sample = activity(0);
    let processes = Some(vec![HostGpuProcessActivity {
        pid: 123,
        busy_pct: 2,
    }]);
    sample.report.schema_version = 2;
    sample.report.adapters[0].engines[0].busy_pct = 40;
    sample.report.adapters[0].engines[0].processes = processes.clone();
    let heartbeat = ack();
    let verify = |version, sample: HostGpuActivityObservation| {
        let mut host = versioned_session(7, 2, version);
        let mut guest = host.clone();
        let wire = host
            .seal_with_activity(SECRET, &heartbeat, None, Some(sample))
            .unwrap();
        let start = Instant::now();
        let result = guest
            .open_verified(SECRET, &heartbeat, &wire, 4, timing(start, 10))
            .unwrap();
        result
            .activity
            .unwrap()
            .report(start + Duration::from_millis(10))
            .unwrap()
            .clone()
    };
    let new = verify(IPC_VERSION_7, sample.clone());
    assert_eq!(new.schema_version, 2);
    assert_eq!(new.adapters[0].engines[0].processes, processes);
    let old = verify(IPC_VERSION_6, sample.clone());
    assert_eq!(old.schema_version, 1);
    assert_eq!(old.adapters[0].engines[0].busy_pct, 40);
    assert_eq!(old.adapters[0].engines[0].processes, None);

    let engine = sample.report.adapters[0].engines[0].clone();
    sample.report.adapters[0].engines = (0..500)
        .map(|index| HostGpuEngineActivity {
            index,
            kind: "X".repeat(72),
            ..engine.clone()
        })
        .collect();
    let bounded = verify(IPC_VERSION_7, sample);
    assert_eq!(bounded.schema_version, 1);
    assert_eq!(bounded.adapters[0].engines.len(), 500);
    assert_eq!(bounded.adapters[0].engines[499].processes, None);
}

#[test]
fn stale_activity_keeps_current_lease_telemetry() {
    let mut stale = activity(4_990);
    let mut invalid = activity(0);
    invalid.report.adapters[0].engines[0].busy_pct = 101;
    for candidate in [stale.clone(), invalid] {
        let heartbeat = ack();
        let mut host = versioned_session(7, 2, IPC_VERSION_6);
        let mut guest = host.clone();
        let wire = host
            .seal_with_activity(SECRET, &heartbeat, Some(observation(100)), Some(candidate))
            .unwrap();
        let result = guest
            .open_verified(SECRET, &heartbeat, &wire, 4, timing(Instant::now(), 10))
            .unwrap();
        assert!(result.catalog.is_some());
        assert!(result.activity.is_none());
        assert_eq!(guest.round(), 5);
    }
    stale.age_ms = 0;
    stale.report.adapters[0].engines = (0..800)
        .map(|index| HostGpuEngineActivity {
            index,
            kind: "X".repeat(96),
            busy_pct: 10,
            processes: None,
        })
        .collect();
    assert!(stale.report.validate().is_ok());
    let heartbeat = ack();
    let mut host = versioned_session(7, 2, IPC_VERSION_6);
    let mut guest = host.clone();
    let wire = host
        .seal_with_activity(SECRET, &heartbeat, Some(observation(100)), Some(stale))
        .unwrap();
    let result = guest
        .open_verified(SECRET, &heartbeat, &wire, 4, timing(Instant::now(), 10))
        .unwrap();
    assert!(result.catalog.is_some());
    assert!(result.activity.is_none());
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
