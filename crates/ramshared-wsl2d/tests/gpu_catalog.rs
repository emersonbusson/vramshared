// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/tests/gpu_catalog.rs
//! Real connected product host/guest exchange with synthetic metadata, no GPU effects.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use ramshared_ipc::gpu_catalog::HostGpuActivityObservation;
use ramshared_vram::GpuAdapterIdentity;
use ramshared_vram::host_gpu_activity::{
    HostGpuActivityReport, HostGpuActivitySource, HostGpuActivityTarget, HostGpuAdapterActivity,
    HostGpuEngineActivity,
};
use ramshared_vram::host_identity::{
    HostGpuIdentity, HostGpuIdentityReport, HostGpuObservation, ProducerProcess,
};
use ramshared_winsvc::vsock_lease::{HostSamples, LeasePolicy, serve_connection};
use ramshared_wsl2d::control_plane::SharedHostActivity;
use ramshared_wsl2d::control_plane::{ControlPlane, VsockControlPlane};
use ramshared_wsl2d::host_gate::{SealedOrigin, SharedAuthority};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
const SECRET: &[u8] = b"actual-connected-catalog-test-secret";
fn sample() -> HostGpuObservation {
    HostGpuObservation {
        age_ms: 100,
        report: HostGpuIdentityReport {
            schema_version: 1,
            producer: ProducerProcess {
                pid: 42,
                creation_time_100ns: 77,
            },
            started_at_unix_ms: 100,
            elapsed_ms: 1,
            adapters: vec![],
        },
    }
}
fn plane(stream: UnixStream) -> VsockControlPlane<UnixStream> {
    VsockControlPlane::new(
        stream,
        SECRET.to_vec(),
        "boot".into(),
        "distro".into(),
        SealedOrigin {
            logical_capacity_mib: 16,
            partuuid: "test-verified-origin".into(),
            origin_vhdx: "test-origin".into(),
        },
        Duration::from_secs(2),
    )
}

#[test]
fn activity_survives_cache_worker_pause() {
    let wall = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let activity = HostGpuActivityObservation {
        age_ms: 1,
        report: HostGpuActivityReport {
            schema_version: 1,
            source: HostGpuActivitySource::WddmPdh,
            sampled_at_unix_ms: wall + 60_000,
            interval_ms: 1_000,
            adapters: vec![],
        },
    };
    assert_activity_exchange(activity, wall, None, None);
}

#[test]
fn activity_matches_one_catalog_adapter_with_cache_paused() {
    let wall = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let mut catalog = sample();
    catalog.report.adapters.push(HostGpuIdentity {
        ordinal: 0,
        allocator: GpuAdapterIdentity {
            backend: "cuda".into(),
            key: format!("{:032x}", 1),
            luid: Some("00000000:0000000a".into()),
            node_mask: Some(1),
        },
        interface_id: "test-display-interface".into(),
    });
    let adapter = HostGpuAdapterActivity {
        luid: "00000000:0000000a".into(),
        physical_index: 0,
        engines: vec![HostGpuEngineActivity {
            index: 1,
            kind: "VideoEncode".into(),
            busy_pct: 37,
            processes: None,
        }],
    };
    let competing = HostGpuAdapterActivity {
        physical_index: 1,
        ..adapter.clone()
    };
    let expected = HostGpuActivityTarget {
        luid: adapter.luid.clone(),
        physical_index: 0,
    };
    let activity = HostGpuActivityObservation {
        age_ms: 1,
        report: HostGpuActivityReport {
            schema_version: 1,
            source: HostGpuActivitySource::WddmPdh,
            sampled_at_unix_ms: wall,
            interval_ms: 1_000,
            adapters: vec![competing, adapter],
        },
    };
    catalog.report.validate().unwrap();
    assert_activity_exchange(activity, wall, Some(catalog), Some(expected));
}

#[test]
#[ignore = "requires a fresh report from the native Windows GPU Engine probe"]
fn native_wddm_sample_crosses_authenticated_guest_exchange() {
    let path = std::env::var_os("RAMSHARED_NATIVE_WDDM_REPORT").unwrap();
    let report: HostGpuActivityReport =
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    report.validate().unwrap();
    let wall = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let age_ms: u64 = std::env::var("RAMSHARED_NATIVE_WDDM_AGE_MS")
        .expect("provide the monotonic upper bound on sample age at receipt")
        .parse()
        .unwrap();
    assert!(
        age_ms < 1_500,
        "native activity is {age_ms} ms old before exchange"
    );
    let adapter_count = report.adapters.len();
    let engine_count: usize = report
        .adapters
        .iter()
        .map(|adapter| adapter.engines.len())
        .sum();
    assert!(adapter_count > 0 && engine_count > 0);
    assert_activity_exchange(
        HostGpuActivityObservation { age_ms, report },
        wall,
        None,
        None,
    );
    eprintln!(
        "NATIVE_WDDM_EXCHANGE_PASS adapters={adapter_count} engines={engine_count} age_before_ms={age_ms}"
    );
}

fn assert_activity_exchange(
    activity: HostGpuActivityObservation,
    wall: u64,
    catalog: Option<HostGpuObservation>,
    preferred: Option<HostGpuActivityTarget>,
) {
    let expected = activity.report.clone();
    let (guest, mut host) = UnixStream::pair().unwrap();
    let server = std::thread::spawn(move || {
        let mut round = 0;
        serve_connection(
            &mut host,
            SECRET,
            LeasePolicy::new(5, 15).unwrap(),
            || 1000,
            || {
                round += 1;
                HostSamples {
                    gpu: if round == 1 { catalog.clone() } else { None },
                    activity: (round != 2).then(|| activity.clone()),
                    ..HostSamples::default()
                }
            },
        )
    });
    let shared = SharedAuthority::new();
    let mut guest = plane(guest).with_authority_mirror(shared.begin_session());
    let slot = SharedHostActivity::default();
    guest.attach_host_activity(slot.clone());
    guest.perform_handshake(1000).unwrap();
    assert!(slot.telemetry(Instant::now(), wall).is_none());
    guest.heartbeat(1001).unwrap();
    let first = slot.telemetry(Instant::now(), wall).unwrap();
    assert_eq!(first.report, expected);
    assert_eq!(first.preferred_host_adapter, preferred);
    assert_eq!(first.report_at(wall), Some(&expected));
    assert!(first.report_at(first.valid_before_unix_ms).is_none());
    assert!(
        slot.telemetry(Instant::now() + Duration::from_secs(5), wall + 5_000)
            .is_none()
    );
    guest.heartbeat(1002).unwrap();
    assert!(slot.telemetry(Instant::now(), wall).is_none());
    guest.heartbeat(1003).unwrap();
    let recovered = slot.telemetry(Instant::now(), wall).unwrap();
    assert_eq!(recovered.report, expected);
    assert!(recovered.preferred_host_adapter.is_none());
    let _new_session = shared.begin_session();
    assert!(slot.telemetry(Instant::now(), wall).is_none());
    drop(guest);
    assert!(server.join().unwrap().is_err());
}
#[test]
fn authenticated_catalog_tracks_availability_and_current_generation() {
    let (guest, mut host) = UnixStream::pair().unwrap();
    let server = std::thread::spawn(move || {
        let mut count = 0;
        serve_connection(
            &mut host,
            SECRET,
            LeasePolicy::new(5, 15).unwrap(),
            || 1000,
            move || {
                count += 1;
                HostSamples {
                    memory: None,
                    gpu: if count == 2 { None } else { Some(sample()) },
                    ..HostSamples::default()
                }
            },
        )
    });
    let shared = SharedAuthority::new();
    let mut guest = plane(guest).with_authority_mirror(shared.begin_session());
    guest.perform_handshake(1000).unwrap();
    assert!(guest.gpu_catalog().is_none());
    guest.heartbeat(1001).unwrap();
    assert_eq!(guest.gpu_catalog().unwrap(), &sample().report);
    guest.heartbeat(1002).unwrap();
    assert!(guest.gpu_catalog().is_none());
    guest.heartbeat(1003).unwrap();
    assert!(guest.gpu_catalog().is_some());
    shared.slot().lock().unwrap().on_lease_expired();
    assert!(
        guest.gpu_catalog().is_none(),
        "shared lease revocation removes metadata access"
    );
    guest.heartbeat(1004).unwrap();
    assert!(guest.gpu_catalog().is_some());
    let _new_session = shared.begin_session();
    assert!(
        guest.gpu_catalog().is_none(),
        "superseded sessions cannot expose observations"
    );
    assert!(guest.admit_origin_io().is_ok());
    drop(guest);
    assert!(server.join().unwrap().is_err());
}
#[test]
fn invalid_catalog_never_renews_cache_authority_or_revokes_origin() {
    use ramshared_ipc::{MAX_GPU_CATALOG_PAYLOAD, MSG_GPU_CATALOG, read_frame, write_frame};
    for fault in ["proof", "missing", "correlation"] {
        let (guest, mut relay_guest) = UnixStream::pair().unwrap();
        let (mut relay_host, mut host) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            serve_connection(
                &mut host,
                SECRET,
                LeasePolicy::new(5, 15).unwrap(),
                || 1000,
                || HostSamples {
                    memory: None,
                    gpu: Some(sample()),
                    ..HostSamples::default()
                },
            )
        });
        let relay = std::thread::spawn(move || {
            fn forward(from: &mut UnixStream, to: &mut UnixStream) {
                let (kind, bytes, round) = read_frame(from, MAX_GPU_CATALOG_PAYLOAD).unwrap();
                write_frame(to, kind, &bytes, round).unwrap();
            }
            forward(&mut relay_guest, &mut relay_host); // Opening
            forward(&mut relay_host, &mut relay_guest); // Challenge
            forward(&mut relay_guest, &mut relay_host); // Finish
            forward(&mut relay_guest, &mut relay_host); // Lease request
            forward(&mut relay_host, &mut relay_guest); // Grant
            forward(&mut relay_guest, &mut relay_host); // Beat
            forward(&mut relay_host, &mut relay_guest); // Ack
            let (kind, bytes, round) =
                read_frame(&mut relay_host, MAX_GPU_CATALOG_PAYLOAD).unwrap();
            assert_eq!(kind, MSG_GPU_CATALOG);
            if fault == "missing" {
                return;
            }
            let mut envelope: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            if fault == "proof" {
                envelope["proof"][0] = (envelope["proof"][0].as_u64().unwrap() ^ 1).into();
            }
            write_frame(
                &mut relay_guest,
                kind,
                &serde_json::to_vec(&envelope).unwrap(),
                round + u64::from(fault == "correlation"),
            )
            .unwrap();
        });
        let mut guest = plane(guest);
        guest.perform_handshake(1000).unwrap();
        assert!(guest.heartbeat(1001).is_err(), "{fault}");
        assert!(guest.gpu_catalog().is_none());
        assert!(guest.admit_cache(1001).is_err());
        assert!(guest.admit_origin_io().is_ok());
        assert_eq!(
            guest
                .liveness_handle()
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        drop(guest);
        relay.join().unwrap();
        assert!(server.join().unwrap().is_err());
    }
}

#[test]
fn catalog_expiry_in_transit_revokes_worker_metadata_without_reconnect() {
    use ramshared_ipc::HostMemoryTelemetry;
    use ramshared_vram::GPU_BUDGET_MAX_AGE_MS;
    use ramshared_wsl2d::host_catalog::{
        HostGpuCatalogProvider, SharedHostCatalog, WorkerHostCatalog,
    };
    use std::time::Instant;

    let (stream, mut host) = UnixStream::pair().unwrap();
    let server = std::thread::spawn(move || {
        let mut round = 0;
        serve_connection(
            &mut host,
            SECRET,
            LeasePolicy::new(5, 15).unwrap(),
            || 1000,
            || {
                round += 1;
                let mut gpu = sample();
                if round == 2 {
                    // The full exchange is strictly positive and rounded up,
                    // so this valid sender sample will expire on receipt.
                    gpu.age_ms = GPU_BUDGET_MAX_AGE_MS - 1;
                }
                HostSamples {
                    memory: Some(HostMemoryTelemetry {
                        sampled_at_unix_ms: 1000,
                        total_bytes: 8192,
                        available_bytes: 4096 + round,
                        commit_total_bytes: 2048,
                        commit_limit_bytes: 16384,
                        page_size_bytes: 4096,
                        vmmem_wsl_working_set_bytes: None,
                    }),
                    gpu: Some(gpu),
                    ..HostSamples::default()
                }
            },
        )
    });

    let catalog = SharedHostCatalog::new();
    let mut guest = plane(stream);
    guest.attach_host_catalog(catalog.clone());
    let worker = WorkerHostCatalog::from_stream(catalog.subscribe().unwrap()).unwrap();
    guest.perform_handshake(1000).unwrap();
    guest.heartbeat(1001).unwrap();
    let first_generation = catalog.live_generation().unwrap();
    assert!(worker.current_report().is_some());

    guest
        .heartbeat(1002)
        .expect("heartbeat outlives an optional GPU observation");
    assert!(guest.gpu_catalog().is_none());
    assert!(catalog.live_generation().is_none());
    assert!(worker.current_report().is_none());
    assert_eq!(
        guest
            .liveness_handle()
            .load(std::sync::atomic::Ordering::Relaxed),
        1002
    );
    assert_eq!(
        guest
            .host_memory_handle()
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .sample(Instant::now(), 1002)
            .unwrap()
            .available_bytes,
        4098
    );
    assert!(guest.admit_origin_io().is_ok());

    guest.heartbeat(1003).unwrap();
    assert!(catalog.live_generation().unwrap() > first_generation);
    assert!(worker.current_report().is_some());
    drop(guest);
    assert!(server.join().unwrap().is_err());
}
