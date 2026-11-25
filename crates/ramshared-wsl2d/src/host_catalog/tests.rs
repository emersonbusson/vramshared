// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/host_catalog/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ramshared_ipc::gpu_catalog::{CatalogSession, CatalogTiming};
use ramshared_ipc::{HandshakeTranscript, HeartbeatAck, HostChallenge, IPC_VERSION_4};
use ramshared_vram::host_identity::{
    HostGpuIdentity, HostGpuIdentityReport, HostGpuObservation, ProducerProcess,
};
use ramshared_vram::{GPU_BUDGET_MAX_AGE_MS, GpuAdapterIdentity};

use crate::host_gate::SharedAuthority;

use super::*;

fn sample_report(hex: u8, iface: &str) -> HostGpuIdentityReport {
    HostGpuIdentityReport {
        schema_version: 1,
        producer: ProducerProcess {
            pid: 99,
            creation_time_100ns: 1234,
        },
        started_at_unix_ms: 5000,
        elapsed_ms: 10,
        adapters: vec![HostGpuIdentity {
            ordinal: 0,
            allocator: GpuAdapterIdentity {
                backend: "cuda".into(),
                key: format!("{:032x}", hex),
                luid: Some("00000000:00011287".into()),
                node_mask: Some(1),
            },
            interface_id: iface.into(),
        }],
    }
}

fn mint_catalog(report: HostGpuIdentityReport, age_ms: u64) -> AuthenticatedCatalog {
    let secret = b"test-vsock-shared-secret-32-bytes";
    let transcript = HandshakeTranscript {
        min_version: IPC_VERSION_4,
        max_version: IPC_VERSION_4,
        boot_id: "boot-1".into(),
        distro_id: "ubuntu".into(),
        guest_nonce: [0x11; 32],
    };
    let challenge = HostChallenge {
        accepted_version: IPC_VERSION_4,
        heartbeat_secs: 2,
        lease_timeout_secs: 6,
        host_nonce: [0x22; 32],
    };
    let mut host = CatalogSession::new(secret, &transcript, &challenge, 7).unwrap();
    let mut guest = CatalogSession::new(secret, &transcript, &challenge, 7).unwrap();
    let ack = HeartbeatAck {
        timestamp_ms: 6000,
        lease_remaining_ms: 6000,
        host_memory: None,
    };
    let obs = HostGpuObservation { report, age_ms };
    let encoded = host.seal(secret, &ack, Some(obs)).unwrap();
    let now = Instant::now();
    guest
        .open(
            secret,
            &ack,
            &encoded,
            4,
            CatalogTiming {
                started: now,
                received: now,
            },
        )
        .unwrap()
        .unwrap()
}

#[test]
fn worker_catalog_receives_preseed_updates_revocation_and_eof() {
    let shared = SharedHostCatalog::new();
    let initial = sample_report(1, r"\\?\PCI#VEN_10DE&DEV_1F08#first");
    let first = mint_catalog(initial.clone(), 50);
    shared.publish(Some(&first));

    let rx = shared.subscribe().unwrap();
    let worker = WorkerHostCatalog::from_stream(rx).unwrap();
    assert_eq!(worker.current_report().as_ref(), Some(&initial));
    assert_eq!(shared.current_report().as_ref(), Some(&initial));

    let updated = sample_report(2, r"\\?\PCI#VEN_10DE&DEV_1F08#second");
    let second = mint_catalog(updated.clone(), 50);
    shared.publish(Some(&second));
    assert_eq!(worker.current_report().as_ref(), Some(&updated));

    shared.revoke();
    assert!(worker.current_report().is_none());
    assert!(shared.current_report().is_none());

    // Re-publish then drop parent sender: EOF must fail worker closed permanently.
    let third = mint_catalog(updated.clone(), 50);
    shared.publish(Some(&third));
    assert_eq!(worker.current_report().as_ref(), Some(&updated));
    drop(shared);
    assert!(worker.current_report().is_none());
}

#[test]
fn worker_catalog_expires_and_rejects_malformed_frames() {
    let shared = SharedHostCatalog::new();
    let report = sample_report(3, r"\\?\PCI#VEN_10DE&DEV_1F08#ttl");
    let short_ttl = mint_catalog(report.clone(), GPU_BUDGET_MAX_AGE_MS - 35);
    shared.publish(Some(&short_ttl));

    let rx = shared.subscribe().unwrap();
    let worker = WorkerHostCatalog::from_stream(rx).unwrap();
    assert_eq!(worker.current_report().as_ref(), Some(&report));
    std::thread::sleep(Duration::from_millis(60));
    assert!(worker.current_report().is_none());
    assert!(shared.current_report().is_none());

    // Malformed frame length or JSON fails closed permanently even before expiry.
    for bad in [
        0u32.to_le_bytes().to_vec(),
        [
            ((MAX_CATALOG_NOTICE_BYTES as u32) + 1)
                .to_le_bytes()
                .as_slice(),
            b"{",
        ]
        .concat(),
        [5u32.to_le_bytes().as_slice(), b"bad!!"].concat(),
    ] {
        let (mut writer, reader) = UnixStream::pair().unwrap();
        let seed = encode_notice(&CatalogWireNotice::Catalog {
            expires_monotonic_ns: monotonic_ns().unwrap() + 5_000_000_000,
            report: report.clone(),
        })
        .unwrap();
        writer.write_all(&seed).unwrap();
        writer.write_all(&bad).unwrap();
        let consumer = WorkerHostCatalog::from_stream(reader).unwrap();
        assert!(consumer.current_report().is_none());
    }
}

#[test]
fn delayed_worker_cannot_extend_catalog_lifetime() {
    let shared = SharedHostCatalog::new();
    let catalog = mint_catalog(sample_report(1, "interface"), GPU_BUDGET_MAX_AGE_MS - 200);
    shared.publish(Some(&catalog));
    let worker = WorkerHostCatalog::from_stream(shared.subscribe().unwrap()).unwrap();
    // The frame remains queued until the worker observes it.
    std::thread::sleep(Duration::from_millis(250));
    assert!(shared.current_report().is_none());
    assert!(worker.current_report().is_none());

    // An expired queued frame is a revocation, not a permanent stream error.
    let fresh = mint_catalog(sample_report(2, "fresh"), 50);
    shared.publish(Some(&fresh));
    assert_eq!(worker.current_report(), shared.current_report());
    assert!(worker.current_report().is_some());
}

#[test]
fn worker_rejects_deadlines_beyond_the_freshness_limit() {
    let (mut writer, reader) = UnixStream::pair().unwrap();
    let notice = CatalogWireNotice::Catalog {
        expires_monotonic_ns: monotonic_ns().unwrap() + 60_000_000_000,
        report: sample_report(1, "interface"),
    };
    writer.write_all(&encode_notice(&notice).unwrap()).unwrap();
    let worker = WorkerHostCatalog::from_stream(reader).unwrap();
    assert!(worker.current_report().is_none());
    assert!(worker.state.lock().unwrap().closed);
}

#[test]
fn blocked_publisher_does_not_revive_an_expired_catalog() {
    let shared = SharedHostCatalog::new();
    let worker = WorkerHostCatalog::from_stream(shared.subscribe().unwrap()).unwrap();
    let catalog = mint_catalog(sample_report(1, "interface"), GPU_BUDGET_MAX_AGE_MS - 200);
    let guard = shared.inner.lock().unwrap();
    let publisher = shared.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let task = std::thread::spawn(move || {
        ready_tx.send(()).unwrap();
        publisher.publish(Some(&catalog));
    });
    ready_rx.recv().unwrap();
    std::thread::sleep(Duration::from_millis(250));
    drop(guard);
    task.join().unwrap();
    assert!(shared.current_report().is_none());
    assert!(worker.current_report().is_none());
    assert_eq!(shared.live_generation(), None);
}

#[test]
fn blocked_reader_checks_expiry_after_acquiring_the_lock() {
    let shared = SharedHostCatalog::new();
    let catalog = mint_catalog(sample_report(1, "interface"), GPU_BUDGET_MAX_AGE_MS - 200);
    shared.publish(Some(&catalog));
    let guard = shared.inner.lock().unwrap();
    let reader = shared.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let task = std::thread::spawn(move || {
        ready_tx.send(()).unwrap();
        reader.current_report()
    });
    ready_rx.recv().unwrap();
    std::thread::sleep(Duration::from_millis(250));
    drop(guard);
    assert!(task.join().unwrap().is_none());
}

#[test]
fn superseded_session_cannot_revoke_replacement_catalog() {
    let shared = Arc::new(SharedHostCatalog::new());
    let authority = SharedAuthority::new();
    let session1 = authority.begin_session();
    let report1 = sample_report(4, r"\\?\PCI#VEN_10DE&DEV_1F08#s1");
    let cat1 = mint_catalog(report1.clone(), 50);
    assert!(session1.mutate(|_| shared.publish(Some(&cat1))));
    assert_eq!(shared.current_report().as_ref(), Some(&report1));

    let session2 = authority.begin_session();
    let report2 = sample_report(5, r"\\?\PCI#VEN_10DE&DEV_1F08#s2");
    let cat2 = mint_catalog(report2.clone(), 50);
    assert!(session2.mutate(|_| shared.publish(Some(&cat2))));
    assert_eq!(shared.current_report().as_ref(), Some(&report2));

    // Draining session1 must not overwrite or revoke session2's live catalog.
    assert!(!session1.retire(|_| shared.revoke()));
    assert_eq!(shared.current_report().as_ref(), Some(&report2));
}

#[test]
fn catalog_generation_changes_only_after_authority_was_absent() {
    let shared = SharedHostCatalog::new();
    let catalog = mint_catalog(sample_report(1, "interface"), 50);
    assert_eq!(shared.live_generation(), None);
    shared.publish(Some(&catalog));
    assert_eq!(shared.live_generation(), Some(1));
    shared.publish(Some(&catalog));
    assert_eq!(shared.live_generation(), Some(1));
    shared.revoke();
    assert_eq!(shared.live_generation(), None);
    shared.publish(Some(&catalog));
    assert_eq!(shared.live_generation(), Some(2));
    shared.publish(None);
    shared.publish(Some(&catalog));
    assert_eq!(shared.live_generation(), Some(3));
}

#[test]
fn live_generation_refuses_expired_and_poisoned_catalogs() {
    let shared = SharedHostCatalog::new();
    let almost_expired = mint_catalog(sample_report(1, "interface"), GPU_BUDGET_MAX_AGE_MS - 250);
    shared.publish(Some(&almost_expired));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(shared.live_generation(), None);

    let live = mint_catalog(sample_report(2, "interface"), 50);
    shared.publish(Some(&live));
    assert!(shared.live_generation().is_some());
    let lock = shared.clone();
    assert!(
        std::thread::spawn(move || {
            let _guard = lock.inner.lock().unwrap();
            panic!("poison catalog fixture");
        })
        .join()
        .is_err()
    );
    assert_eq!(shared.live_generation(), None);
}

#[test]
fn recovery_catalog_observation_never_waits_for_a_busy_publisher() {
    let shared = SharedHostCatalog::new();
    let catalog = mint_catalog(sample_report(1, r"\\?\PCI#VEN_10DE&DEV_1F08#test"), 50);
    shared.publish(Some(&catalog));
    assert!(shared.try_has_current_report());
    let lock = shared.inner.lock().unwrap();
    let started = Instant::now();
    assert!(!shared.try_has_current_report());
    assert!(started.elapsed() < Duration::from_millis(100));
    drop(lock);
    assert!(shared.try_has_current_report());
    shared.revoke();
    assert!(!shared.try_has_current_report());
}

#[test]
fn recovery_generation_observation_never_waits_for_a_busy_publisher() {
    let shared = SharedHostCatalog::new();
    let catalog = mint_catalog(sample_report(1, "interface"), 50);
    shared.publish(Some(&catalog));
    assert_eq!(shared.live_generation(), Some(1));
    let lock = shared.inner.lock().unwrap();
    let observer = shared.clone();
    let (sender, receiver) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || sender.send(observer.live_generation()));
    let observed = receiver.recv_timeout(Duration::from_millis(100));
    drop(lock);
    thread.join().unwrap().unwrap();
    assert!(
        matches!(observed, Ok(None)),
        "busy catalog must not block recovery"
    );
    assert_eq!(shared.live_generation(), Some(1));
}

#[test]
fn wait_for_current_report_wakes_on_publish_and_respects_timeout() {
    let shared = SharedHostCatalog::new();
    assert!(!shared.wait_for_current_report(Duration::ZERO));
    assert!(!shared.wait_for_current_report(Duration::from_millis(15)));

    let publisher = shared.clone();
    let report = sample_report(2, r"\\?\PCI#VEN_10DE&DEV_1F08#wake");
    let catalog = mint_catalog(report.clone(), 50);
    let handle = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        publisher.publish(Some(&catalog));
    });
    assert!(shared.wait_for_current_report(Duration::from_secs(2)));
    handle.join().unwrap();
    assert_eq!(shared.current_report().as_ref(), Some(&report));
    assert!(shared.wait_for_current_report(Duration::ZERO));

    let poison = shared.clone();
    let _ = std::thread::spawn(move || {
        let _guard = poison.inner.lock().unwrap();
        panic!("poison for wait");
    })
    .join();
    assert!(!shared.wait_for_current_report(Duration::from_millis(10)));
}
