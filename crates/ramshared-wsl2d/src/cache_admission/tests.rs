// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/cache_admission/tests.rs
#![allow(clippy::unwrap_used)]
use super::*;
use ramshared_wsl2d::host_catalog::HostGpuCatalogProvider;
#[test]
fn live_supervisor_waits_for_the_first_authenticated_catalog() {
    let stop = Arc::new(AtomicBool::new(false));
    let lifetime = start_supervisor(stop, |stop| {
        while !stop.load(Ordering::Acquire) {
            std::thread::park_timeout(Duration::from_millis(1));
        }
    })
    .unwrap();
    let admission = ControlPlaneCacheAdmission {
        authority: Arc::new(Mutex::new(ControlPlaneAuthority::disconnected())),
        lease_lost: Arc::new(AtomicBool::new(false)),
        lease_seen: Arc::new(AtomicBool::new(false)),
        host_memory: Arc::new(Mutex::new(None)),
        host_catalog: SharedHostCatalog::new(),
        host_storage: SharedHostStorage::new(),
        lifetime: Some(lifetime),
    };
    assert!(!admission.lease_lost());
    assert!(!admission.allows_worker_recovery());
}

#[test]
fn admission_last_owner_joins_supervisor_before_release() {
    let stop = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let gate = release.clone();
    let owner = start_supervisor(stop.clone(), move |stop| {
        while !stop.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(1));
        }
        while !gate.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(1));
        }
    })
    .unwrap();
    let second = owner.clone();
    drop(owner);
    assert!(!stop.load(Ordering::Acquire));
    let joined = std::thread::spawn(move || drop(second));
    struct Release {
        gate: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    }
    impl Drop for Release {
        fn drop(&mut self) {
            self.gate.store(true, Ordering::Release);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }
    let mut cleanup = Release {
        gate: release.clone(),
        handle: Some(joined),
    };
    let until = std::time::Instant::now() + Duration::from_secs(2);
    while !stop.load(Ordering::Acquire) && std::time::Instant::now() < until {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(stop.load(Ordering::Acquire));
    assert!(!cleanup.handle.as_ref().unwrap().is_finished());
    release.store(true, Ordering::Release);
    cleanup.handle.take().unwrap().join().unwrap();
}

#[test]
fn admission_real_protocol_session_stops_and_preserves_origin() {
    use ramshared_block::CacheAdmission;
    use ramshared_ipc::vsock::{MockVsockStream, VsockStreamTrait};
    use ramshared_winsvc::vsock_lease::{HostSamples, LeasePolicy, serve_connection};
    use std::os::unix::net::UnixStream;
    const SECRET: &[u8] = b"local-session-custody-test-secret";
    struct Peers(Arc<Mutex<Vec<JoinHandle<()>>>>);
    impl Drop for Peers {
        fn drop(&mut self) {
            for peer in self.0.lock().unwrap().drain(..) {
                peer.join().unwrap();
            }
        }
    }
    let sealed = SealedOrigin {
        logical_capacity_mib: 16,
        partuuid: "local-custody-origin".into(),
        origin_vhdx: "/dev/null".into(),
    };
    let origin = sealed.clone();
    let peers = Peers(Arc::new(Mutex::new(Vec::new())));
    let children = peers.0.clone();
    let peer_error = Arc::new(Mutex::new(None::<String>));
    let peer_error_report = peer_error.clone();
    let attempted = AtomicBool::new(false);
    let admission = compose_cache_admission_with(
        sealed,
        move |session| {
            if attempted.swap(true, Ordering::AcqRel) {
                return None;
            }
            let (guest, host) = UnixStream::pair().unwrap();
            let peer_error = peer_error_report.clone();
            children.lock().unwrap().push(std::thread::spawn(move || {
                let mut host = MockVsockStream(host);
                let result = serve_connection(
                    &mut host,
                    SECRET,
                    LeasePolicy::new(1, 3).unwrap(),
                    || unix_time_ms().unwrap(),
                    || HostSamples {
                        memory: Some(ramshared_ipc::HostMemoryTelemetry {
                            sampled_at_unix_ms: unix_time_ms().unwrap(),
                            total_bytes: 8 << 30,
                            available_bytes: 4 << 30,
                            commit_total_bytes: 1 << 30,
                            commit_limit_bytes: 10 << 30,
                            page_size_bytes: 4096,
                            vmmem_wsl_working_set_bytes: None,
                        }),
                        gpu: None,
                        ..HostSamples::default()
                    },
                );
                if let Err(error) = result {
                    *peer_error.lock().unwrap() = Some(error.to_string());
                }
            }));
            let mut plane = VsockControlPlane::new(
                Box::new(MockVsockStream(guest)) as Box<dyn VsockStreamTrait>,
                SECRET.to_vec(),
                "custody-test-boot".into(),
                "custody-test-distro".into(),
                origin.clone(),
                Duration::from_secs(1),
            )
            .with_authority_mirror(session);
            plane.perform_handshake(unix_time_ms().unwrap()).unwrap();
            let cadence = plane.cadence().unwrap();
            Some((plane, cadence))
        },
        Duration::from_millis(1),
    );
    let until = std::time::Instant::now() + Duration::from_secs(5);
    while admission.host_memory().is_none() && std::time::Instant::now() < until {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        admission.host_memory().is_some(),
        "authenticated heartbeat sample reached owned refresher; peer error: {:?}",
        peer_error.lock().unwrap()
    );
    let locked_memory = admission.host_memory.lock().unwrap();
    assert!(
        admission.host_memory().is_none(),
        "busy memory observation must not wait"
    );
    drop(locked_memory);
    let disconnected = ControlPlaneCacheAdmission {
        authority: Arc::new(Mutex::new(ControlPlaneAuthority::disconnected())),
        lease_lost: Arc::new(AtomicBool::new(false)),
        lease_seen: Arc::new(AtomicBool::new(true)),
        host_memory: Arc::new(Mutex::new(admission.host_memory.lock().unwrap().clone())),
        host_catalog: SharedHostCatalog::new(),
        host_storage: SharedHostStorage::new(),
        lifetime: None,
    };
    assert!(disconnected.host_memory.lock().unwrap().is_some());
    assert!(
        disconnected.host_memory().is_none(),
        "a valid sample cannot restore revoked authority"
    );
    assert!(admission.lease_seen.load(Ordering::Acquire));
    assert!(!admission.lease_lost());
    assert_eq!(admission.control_state(), "vsock_leased");
    assert!(admission.lease_deadline_ms().is_some());
    assert!(admission.admit_cache().is_ok());
    let authority = admission.authority.clone();
    let memory = admission.host_memory.clone();
    let lost = admission.lease_lost.clone();
    let clone = admission.clone();
    drop(admission);
    assert!(clone.admit_cache().is_ok());
    drop(clone);
    assert!(lost.load(Ordering::Acquire));
    assert!(memory.lock().unwrap().is_none());
    assert!(
        authority
            .lock()
            .unwrap()
            .admit_cache(unix_time_ms().unwrap())
            .is_err()
    );
    assert!(authority.lock().unwrap().admit_origin_io().is_ok());
    drop(peers);
}

#[test]
fn admission_tracks_host_gpu_catalog_and_worker_recovery_lifecycle() {
    use ramshared_ipc::gpu_catalog::{CatalogSession, CatalogTiming};
    use ramshared_ipc::vsock::{MockVsockStream, VsockStreamTrait};
    use ramshared_ipc::{HandshakeTranscript, HeartbeatAck, HostChallenge, IPC_VERSION_4};
    use ramshared_vram::GpuAdapterIdentity;
    use ramshared_vram::host_identity::{
        HostGpuIdentity, HostGpuIdentityReport, HostGpuObservation, ProducerProcess,
    };
    use ramshared_winsvc::vsock_lease::{HostSamples, LeasePolicy, serve_connection};
    use std::os::unix::net::UnixStream;

    const SECRET: &[u8] = b"local-session-catalog-test-secret";
    let pre_lease_catalog = SharedHostCatalog::new();
    let pre_lease = ControlPlaneCacheAdmission {
        authority: Arc::new(Mutex::new(ControlPlaneAuthority::disconnected())),
        lease_lost: Arc::new(AtomicBool::new(false)),
        lease_seen: Arc::new(AtomicBool::new(false)),
        host_memory: Arc::new(Mutex::new(None)),
        host_catalog: pre_lease_catalog.clone(),
        host_storage: SharedHostStorage::new(),
        lifetime: None,
    };
    assert!(pre_lease.allows_worker_recovery());
    let empty_report = HostGpuIdentityReport {
        schema_version: 1,
        producer: ProducerProcess {
            pid: 42,
            creation_time_100ns: 77,
        },
        started_at_unix_ms: 1_700_000_000_000,
        elapsed_ms: 5,
        adapters: vec![],
    };
    let transcript = HandshakeTranscript {
        min_version: IPC_VERSION_4,
        max_version: IPC_VERSION_4,
        boot_id: "boot-adm".into(),
        distro_id: "ubuntu".into(),
        guest_nonce: [0x11; 32],
    };
    let challenge = HostChallenge {
        accepted_version: IPC_VERSION_4,
        heartbeat_secs: 2,
        lease_timeout_secs: 6,
        host_nonce: [0x22; 32],
    };
    let mut hs = CatalogSession::new(SECRET, &transcript, &challenge, 7).unwrap();
    let mut gs = CatalogSession::new(SECRET, &transcript, &challenge, 7).unwrap();
    let ack = HeartbeatAck {
        timestamp_ms: 6000,
        lease_remaining_ms: 6000,
        host_memory: None,
    };
    let sealed_obs = hs
        .seal(
            SECRET,
            &ack,
            Some(HostGpuObservation {
                report: empty_report,
                age_ms: 10,
            }),
        )
        .unwrap();
    let now = std::time::Instant::now();
    let pre_entry = gs
        .open(
            SECRET,
            &ack,
            &sealed_obs,
            4,
            CatalogTiming {
                started: now,
                received: now,
            },
        )
        .unwrap()
        .unwrap();
    pre_lease_catalog.publish(Some(&pre_entry));
    pre_lease.lease_lost.store(true, Ordering::Release);
    assert!(
        !pre_lease.allows_worker_recovery(),
        "lost lease must deny worker recovery even when catalog still holds a sample"
    );

    let sealed = SealedOrigin {
        logical_capacity_mib: 16,
        partuuid: "local-catalog-origin".into(),
        origin_vhdx: "/dev/null".into(),
    };
    let origin = sealed.clone();
    let peers: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
    let children = peers.clone();
    let attempted = AtomicBool::new(false);
    let admission = compose_cache_admission_with(
        sealed,
        move |session| {
            if attempted.swap(true, Ordering::AcqRel) {
                return None;
            }
            let (guest, host) = UnixStream::pair().unwrap();
            children.lock().unwrap().push(std::thread::spawn(move || {
                let mut host = MockVsockStream(host);
                let _ = serve_connection(
                    &mut host,
                    SECRET,
                    LeasePolicy::new(1, 3).unwrap(),
                    || unix_time_ms().unwrap(),
                    || HostSamples {
                        memory: None,
                        gpu: Some(HostGpuObservation {
                            report: HostGpuIdentityReport {
                                schema_version: 1,
                                producer: ProducerProcess {
                                    pid: 4242,
                                    creation_time_100ns: 99,
                                },
                                started_at_unix_ms: unix_time_ms().unwrap(),
                                elapsed_ms: 5,
                                adapters: vec![HostGpuIdentity {
                                    ordinal: 0,
                                    allocator: GpuAdapterIdentity {
                                        node_mask: Some(1),
                                        backend: "cuda".into(),
                                        key: format!("{:032x}", 1u8),
                                        luid: Some("11111111:22222222".into()),
                                    },
                                    interface_id: r"\\?\PCI#VEN_10DE&DEV_1F08#catalog-test".into(),
                                }],
                            },
                            age_ms: 5,
                        }),
                        ..HostSamples::default()
                    },
                );
            }));
            let mut plane = VsockControlPlane::new(
                Box::new(MockVsockStream(guest)) as Box<dyn VsockStreamTrait>,
                SECRET.to_vec(),
                "catalog-test-boot".into(),
                "catalog-test-distro".into(),
                origin.clone(),
                Duration::from_secs(1),
            )
            .with_authority_mirror(session);
            plane.perform_handshake(unix_time_ms().unwrap()).unwrap();
            let cadence = plane.cadence().unwrap();
            Some((plane, cadence))
        },
        Duration::from_millis(1),
    );
    let until = std::time::Instant::now() + Duration::from_secs(5);
    while admission.host_catalog().current_report().is_none() && std::time::Instant::now() < until {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        admission.host_catalog().current_report().is_some(),
        "authenticated host GPU catalog reached shared admission owner"
    );
    let report = admission.host_catalog().current_report().unwrap();
    assert_eq!(report.producer.pid, 4242);
    assert!(admission.lease_seen.load(Ordering::Acquire));
    assert!(admission.allows_worker_recovery());

    // Revoking/expiring the catalog while a lease was seen immediately denies worker recovery.
    admission.host_catalog().revoke();
    assert!(!admission.allows_worker_recovery());

    let host_catalog = admission.host_catalog().clone();
    drop(admission);
    assert!(host_catalog.current_report().is_none());
    for peer in peers.lock().unwrap().drain(..) {
        peer.join().unwrap();
    }
}

#[test]
fn initial_catalog_wait_respects_zero_lost_lease_and_timely_publish() {
    use ramshared_ipc::gpu_catalog::{CatalogSession, CatalogTiming};
    use ramshared_ipc::{HandshakeTranscript, HeartbeatAck, HostChallenge, IPC_VERSION_4};
    use ramshared_vram::GpuAdapterIdentity;
    use ramshared_vram::host_identity::{
        HostGpuIdentity, HostGpuIdentityReport, HostGpuObservation, ProducerProcess,
    };
    let secret = b"test-vsock-shared-secret-32-bytes";
    let transcript = HandshakeTranscript {
        min_version: IPC_VERSION_4,
        max_version: IPC_VERSION_4,
        boot_id: "boot-wait".into(),
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
    let report = HostGpuIdentityReport {
        schema_version: 1,
        producer: ProducerProcess {
            pid: 77,
            creation_time_100ns: 456,
        },
        started_at_unix_ms: 5000,
        elapsed_ms: 5,
        adapters: vec![HostGpuIdentity {
            ordinal: 0,
            allocator: GpuAdapterIdentity {
                backend: "cuda".into(),
                key: format!("{:032x}", 3u8),
                luid: Some("00000000:00011287".into()),
                node_mask: Some(1),
            },
            interface_id: r"\\?\PCI#VEN_10DE#wait".into(),
        }],
    };
    let encoded = host
        .seal(secret, &ack, Some(HostGpuObservation { report, age_ms: 5 }))
        .unwrap();
    let now = std::time::Instant::now();
    let cat = guest
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
        .unwrap();

    let admission = ControlPlaneCacheAdmission {
        authority: Arc::new(Mutex::new(ControlPlaneAuthority::disconnected())),
        lease_lost: Arc::new(AtomicBool::new(false)),
        lease_seen: Arc::new(AtomicBool::new(false)),
        host_memory: Arc::new(Mutex::new(None)),
        host_catalog: SharedHostCatalog::new(),
        host_storage: SharedHostStorage::new(),
        lifetime: None,
    };
    assert!(!admission.wait_for_initial_catalog(Duration::ZERO));
    let catalog_handle = admission.host_catalog().clone();
    let worker = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(15));
        catalog_handle.publish(Some(&cat));
    });
    assert!(admission.wait_for_initial_catalog(Duration::from_secs(2)));
    worker.join().unwrap();
    admission.lease_lost.store(true, Ordering::Release);
    assert!(admission.wait_for_initial_catalog(Duration::from_millis(20)));
}

#[test]
fn ipc_v5_storage_proof_publishes_host_volume_telemetry_and_bounds_free_space() {
    use ramshared_block::vhd_identity::RootVhdObservation;
    use ramshared_ipc::vsock::{MockVsockStream, VsockStreamTrait};
    use ramshared_winsvc::gpu_identity::ProducerProcess;
    use ramshared_winsvc::host_storage::{HostFileIdentity, HostStorageReport, HostVolumeSample};
    use ramshared_winsvc::vsock_lease::{
        HostSamples, LeasePolicy, StorageSample, serve_connection,
    };
    use std::os::unix::net::UnixStream;
    const SECRET: &[u8] = b"local-v5-storage-test-secret-01234";

    const TEST_DISK_ID: [u8; 16] = [0xAB; 16];
    fn fake_root_vhd() -> Option<RootVhdObservation> {
        Some(RootVhdObservation {
            major: 8,
            minor: 64,
            identifier: TEST_DISK_ID,
            observed_at_unix_ms: unix_time_ms().unwrap_or(1_000),
            observed_at: std::time::Instant::now(),
        })
    }

    let attempted = Arc::new(AtomicBool::new(false));
    let children = Arc::new(Mutex::new(Vec::new()));
    let sealed = SealedOrigin {
        logical_capacity_mib: 64,
        partuuid: "33333333-4444-5555-6666-777777777777".into(),
        origin_vhdx: "C:\\ramshared\\origin.vhdx".into(),
    };
    let origin = sealed.clone();
    let admission = compose_cache_admission_with(
        sealed,
        {
            let attempted = Arc::clone(&attempted);
            let children = Arc::clone(&children);
            move |session| {
                if attempted.swap(true, Ordering::AcqRel) {
                    return None;
                }
                let (guest, host) = UnixStream::pair().unwrap();
                children.lock().unwrap().push(std::thread::spawn(move || {
                    let mut host = MockVsockStream(host);
                    let _ = serve_connection(
                        &mut host,
                        SECRET,
                        LeasePolicy::new(1, 3).unwrap(),
                        || unix_time_ms().unwrap(),
                        || {
                            let started_at_unix_ms = unix_time_ms().unwrap().saturating_sub(1);
                            let report = HostStorageReport {
                                schema_version: 1,
                                producer: ProducerProcess {
                                    pid: 4242,
                                    creation_time_100ns: 99,
                                },
                                started_at_unix_ms,
                                elapsed_ms: 2,
                                sample: HostVolumeSample {
                                    sampled_at_unix_ms: started_at_unix_ms + 1,
                                    volume_id: r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\"
                                        .into(),
                                    file_identity: HostFileIdentity {
                                        volume_serial: 1,
                                        file_index: 42,
                                    },
                                    total_bytes: 100 << 30,
                                    free_bytes: 60 << 30,
                                    available_bytes: 50 << 30,
                                },
                                virtual_disk_id: Some(TEST_DISK_ID),
                            };
                            HostSamples {
                                storage: Some(Arc::new(move |req_id| {
                                    (req_id == TEST_DISK_ID).then(|| StorageSample {
                                        report: report.clone(),
                                        age_ms: 5,
                                        captured_since: std::time::Instant::now(),
                                        complete: true,
                                    })
                                })),
                                ..HostSamples::default()
                            }
                        },
                    );
                }));
                let mut plane = VsockControlPlane::new(
                    Box::new(MockVsockStream(guest)) as Box<dyn VsockStreamTrait>,
                    SECRET.to_vec(),
                    "v5-storage-boot".into(),
                    "v5-storage-distro".into(),
                    origin.clone(),
                    Duration::from_secs(1),
                )
                .with_authority_mirror(session);
                plane.set_root_vhd_observer(fake_root_vhd);
                plane.perform_handshake(unix_time_ms().unwrap()).unwrap();
                let cadence = plane.cadence().unwrap();
                Some((plane, cadence))
            }
        },
        Duration::from_millis(1),
    );
    let until = std::time::Instant::now() + Duration::from_secs(5);
    while admission.host_storage().is_none() && std::time::Instant::now() < until {
        std::thread::sleep(Duration::from_millis(10));
    }
    let telemetry = admission.host_storage().unwrap();
    assert_eq!(telemetry.available_bytes, 50 << 30);
    assert_eq!(
        telemetry.disk_id,
        ramshared_ipc::storage_proof::format_disk_id_hex(&TEST_DISK_ID)
    );
    assert_eq!(
        admission.host_storage.bounded_free_bytes_with(
            std::path::Path::new("/"),
            794 << 30,
            admission.lease_lost(),
            |_| fake_root_vhd(),
        ),
        Some(49 << 30)
    );
    admission.lease_lost.store(true, Ordering::Release);
    assert!(admission.host_storage().is_none());
    drop(admission);
    for child in children.lock().unwrap().drain(..) {
        let _ = child.join();
    }
}

#[test]
fn compose_cache_admission_with_stop_cancels_blocking_attempt_on_drop() {
    let sealed = SealedOrigin {
        logical_capacity_mib: 16,
        partuuid: "stop-cancel-origin".into(),
        origin_vhdx: "/dev/null".into(),
    };
    let entered = Arc::new(AtomicBool::new(false));
    let entered_attempt = Arc::clone(&entered);
    let saw_stop = Arc::new(AtomicBool::new(false));
    let saw_stop_attempt = Arc::clone(&saw_stop);
    let admission = compose_cache_admission_with_stop(
        sealed,
        move |_session, stop| {
            entered_attempt.store(true, Ordering::Release);
            while !stop.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(5));
            }
            saw_stop_attempt.store(true, Ordering::Release);
            None
        },
        Duration::from_secs(10),
    );
    let wait_entered = std::time::Instant::now() + Duration::from_secs(2);
    while !entered.load(Ordering::Acquire) && std::time::Instant::now() < wait_entered {
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(entered.load(Ordering::Acquire));
    let drop_started = std::time::Instant::now();
    drop(admission);
    assert!(
        drop_started.elapsed() < Duration::from_secs(1),
        "dropping admission must cancel the accept loop promptly"
    );
    assert!(saw_stop.load(Ordering::Acquire));
}
