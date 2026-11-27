// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/cache_status/tests.rs
//! Synthetic driver observations over real IPC and authoritative file custody.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::{DemoteStatusFile, ShutdownPermit, SwapoffOutcome, WMsg};
use crate::{native_fixture::Fixture, worker_recovery::CacheWorkerObservation};
use ramshared_block::{
    AllowCacheAdmission, DisabledCache, IpcCacheClient,
    cache_group::{CacheGroup, CacheMember},
};
use ramshared_vram::{
    GpuAdapterIdentity, GpuBudgetSnapshot, GpuBudgetSource, WorkerTelemetryEnvelope,
};
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, atomic::AtomicU64, mpsc};
use std::time::Duration;

#[derive(Default)]
struct Recorder {
    statuses: Vec<OriginCacheStatus>,
    output: Option<PathBuf>,
    failed_writes: usize,
}
impl NbdRuntimeStarter for Recorder {
    fn lock_memory(&mut self, _: bool, _: bool) -> Result<(), Box<dyn std::error::Error>> {
        panic!("publication only")
    }
    fn start_acceptor(
        &mut self,
        _: UnixListener,
        _: Arc<Vec<ramshared_block::handshake::Export>>,
        _: u16,
        _: mpsc::SyncSender<WMsg>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        panic!("publication only")
    }
    fn nbd_used_kb(&mut self, _: &str) -> u64 {
        panic!("publication only")
    }
    fn try_claim_shutdown(&mut self, _: &str) -> Option<ShutdownPermit> {
        panic!("publication only")
    }
    fn publish_demote(&mut self, _: &DemoteStatusFile) {
        panic!("publication only")
    }
    fn publish_origin_cache(&mut self, status: &OriginCacheStatus) {
        if let Some(path) = self.output.as_ref()
            && write_origin_cache_status(path, status).is_err()
        {
            self.failed_writes += 1;
        }
        self.statuses.push(status.clone());
    }
    fn elapsed_us(&mut self, _: Instant) -> u64 {
        panic!("publication only")
    }
    fn spawn_swapoff(&mut self, _: &str) -> mpsc::Receiver<SwapoffOutcome> {
        panic!("publication only")
    }
    fn spawn_recovery_activation(
        &mut self,
        _: &str,
        _: i16,
    ) -> Result<mpsc::Receiver<bool>, Box<dyn std::error::Error>> {
        panic!("publication only")
    }
}
fn backend(
    fixture: &Fixture,
    cache: OriginCache,
) -> AuthoritativeOriginBackend<FileOrigin, OriginCache> {
    let file = File::options()
        .read(true)
        .write(true)
        .open(fixture.origin_path())
        .unwrap();
    AuthoritativeOriginBackend::new_with_admission(
        FileOrigin::from_file(file),
        cache,
        8192,
        4096,
        AllowCacheAdmission,
    )
    .unwrap()
}
fn context() -> PublicationContext<'static> {
    PublicationContext {
        configuration_generation: None,
        daemon_instance_id: "publication-fixture",
        serve_threads: 2,
        serve_requests_total: 0,
        worker: CacheWorkerObservation {
            critical_reclaim: false,
            startup_pending: false,
            unavailable_reason: None,
        },
        control_admission: None,
    }
}

#[test]
fn monotonic_cadence_has_exact_boundaries_and_runtime_ownership() {
    let now = Instant::now();
    let mut first = CacheStatusPublisher::default();
    let second = CacheStatusPublisher::default();
    assert!(first.due_at(now));
    first.last_attempt = Some(now);
    assert!(!first.due_at(now));
    assert!(!first.due_at(now + SERVE_IDLE_TICK - Duration::from_nanos(1)));
    assert!(first.due_at(now + SERVE_IDLE_TICK));
    assert!(second.due_at(now));
    assert!(!first.due_at(now - Duration::from_millis(1)));
    assert!(SERVE_IDLE_TICK * 5 <= Duration::from_millis(GPU_BUDGET_MAX_AGE_MS));
}
#[test]
fn critical_reclaim_is_not_delayed_by_publication_cadence() {
    let f = Fixture::new();
    let b = backend(&f, OriginCache::Starting { released: false });
    let mut p = CacheStatusPublisher::default();
    let mut out = Recorder::default();
    let mut c = context();
    c.worker.startup_pending = true;
    assert!(p.publish(&mut out, &b, c).is_empty());
    let mut c = context();
    c.worker.startup_pending = true;
    c.worker.critical_reclaim = true;
    assert_eq!(p.publish(&mut out, &b, c), vec![(0, true)]);
    assert_eq!(b.cache_state(), OriginCacheState::Off);
    assert_eq!(out.statuses.len(), 2);
    assert_eq!(out.statuses[1].cache_target_kib, 0);
    assert!(!out.statuses[1].ok);
    assert!(
        p.publish(&mut out, &b, {
            let mut c = context();
            c.worker.startup_pending = true;
            c.worker.critical_reclaim = true;
            c
        })
        .is_empty()
    );
    assert_eq!(
        out.statuses.len(),
        2,
        "reclaim replay republished unchanged metadata"
    );
    let mut actual = [0; 4096];
    b.read_at(0, &mut actual).unwrap();
    assert_eq!(actual, [17; 4096]);
}

#[test]
fn pressure_reclaim_identifies_only_serving_and_starting_group_members() {
    let f = Fixture::new();
    let (active, worker) = peer(Arc::new(AtomicU64::new(unix_time_ms().unwrap())), 4096);
    let group = CacheGroup::new(
        vec![
            CacheMember::new("a".into(), 4096, active),
            CacheMember::new("b".into(), 4096, OriginCache::Disabled(DisabledCache)),
            CacheMember::new("c".into(), 4096, OriginCache::Starting { released: false }),
        ],
        4096,
        Duration::from_millis(50),
        Duration::from_millis(50),
    )
    .unwrap();
    let b = backend(&f, OriginCache::Group(Box::new(group)));
    assert_eq!(
        apply_critical_reclaim(&b, true, true),
        vec![(0, false), (2, true)]
    );
    assert!(apply_critical_reclaim(&b, true, true).is_empty());
    drop(b);
    worker.join().unwrap();
}

#[test]
fn pressure_reclaim_retains_confirmed_member_on_partial_failure() {
    let f = Fixture::new();
    let sample = Arc::new(AtomicU64::new(unix_time_ms().unwrap()));
    let (good, good_worker) = peer(Arc::clone(&sample), 4096);
    let (bad, bad_worker) = peer_with_failure(sample, 4096, true);
    let group = CacheGroup::new(
        vec![
            CacheMember::new("a".into(), 4096, good),
            CacheMember::new("b".into(), 4096, bad),
        ],
        4096,
        Duration::from_millis(50),
        Duration::from_millis(500),
    )
    .unwrap();
    let b = backend(&f, OriginCache::Group(Box::new(group)));
    assert_eq!(apply_critical_reclaim(&b, true, false), vec![(0, false)]);
    let states = b
        .try_control_cache(|cache| {
            cache
                .member_statuses()
                .unwrap()
                .into_iter()
                .map(|m| m.state)
                .collect::<Vec<_>>()
        })
        .unwrap();
    assert_eq!(states, vec![OriginCacheState::Off, OriginCacheState::Stuck]);
    drop(b);
    good_worker.join().unwrap();
    bad_worker.join().unwrap();
}
#[test]
fn counters_wait_but_worker_causes_and_state_changes_publish() {
    let f = Fixture::new();
    let b = backend(&f, OriginCache::Disabled(DisabledCache));
    let mut p = CacheStatusPublisher::default();
    let mut out = Recorder::default();
    p.publish(&mut out, &b, context());
    for count in 1..100 {
        let mut c = context();
        c.serve_requests_total = count;
        p.publish(&mut out, &b, c);
    }
    assert_eq!(out.statuses.len(), 1);
    let mut c = context();
    c.worker.unavailable_reason = Some("worker lost");
    p.publish(&mut out, &b, c);
    assert_eq!(out.statuses.len(), 2);
    let mut c = context();
    c.worker.unavailable_reason = Some("worker lost");
    c.worker.startup_pending = true;
    p.publish(&mut out, &b, c);
    assert_eq!(out.statuses.len(), 3);
    b.replace_cache(OriginCache::Starting { released: true });
    p.publish(&mut out, &b, context());
    assert_eq!(out.statuses.last().unwrap().cache_state, "OFF");
    assert_eq!(out.statuses.len(), 4);
    p.last_attempt = Some(Instant::now() - SERVE_IDLE_TICK);
    p.publish(&mut out, &b, context());
    assert_eq!(out.statuses.len(), 5);
    p.publish(&mut out, &b, context());
    assert_eq!(out.statuses.len(), 5, "catch-up burst");
}

fn peer(sample: Arc<AtomicU64>, target: u64) -> (OriginCache, std::thread::JoinHandle<()>) {
    peer_with_failure(sample, target, false)
}
fn peer_with_failure(
    sample: Arc<AtomicU64>,
    target: u64,
    fail_disable: bool,
) -> (OriginCache, std::thread::JoinHandle<()>) {
    use ramshared_block::gpu_cache_worker::*;
    let (socket, mut peer) = UnixStream::pair().unwrap();
    let handle = std::thread::spawn(move || {
        loop {
            let mut raw = [0; FRAME_HEADER_LEN];
            if peer.read_exact(&mut raw).is_err() {
                break;
            }
            let request = FrameHeader::decode(&raw).unwrap();
            if fail_disable && request.msg_type == MSG_DISABLE_REQ {
                break;
            }
            let kind = match request.msg_type {
                MSG_HANDSHAKE_REQ => MSG_HANDSHAKE_RESP,
                MSG_HEARTBEAT_REQ => MSG_HEARTBEAT_RESP,
                MSG_DISABLE_REQ => MSG_DISABLE_RESP,
                _ => panic!("unexpected payload"),
            };
            let now = sample.load(Ordering::Acquire);
            let budget = GpuBudgetTelemetry::from_snapshot(
                &GpuBudgetSnapshot {
                    adapter: Some(GpuAdapterIdentity {
                        node_mask: None,
                        backend: "cuda".into(),
                        key: "00112233445566778899aabbccddeeff".into(),
                        luid: None,
                    }),
                    total_bytes: Some(8 * 1024 * 1024 * 1024),
                    budget_bytes: 8 * 1024 * 1024 * 1024,
                    used_bytes: 0,
                    source: GpuBudgetSource::DriverReported,
                    sampled_at: Instant::now(),
                },
                now,
            );
            let payload = if request.msg_type == MSG_HEARTBEAT_REQ {
                WorkerTelemetryEnvelope::new(now, Some(budget), None, None)
                    .to_bounded_payload()
                    .unwrap()
            } else {
                vec![]
            };
            let reply = FrameHeader {
                msg_type: kind,
                status: STATUS_OK,
                correlation_id: request.correlation_id,
                offset: if request.msg_type == MSG_DISABLE_REQ {
                    0
                } else {
                    target
                },
                payload_len: payload.len() as u32,
                aux: 0,
            };
            if peer
                .write_all(&reply.encode())
                .and_then(|()| peer.write_all(&payload))
                .is_err()
            {
                break;
            }
        }
    });
    let mut client = IpcCacheClient::new(socket, Duration::from_millis(50), target);
    client.perform_handshake().unwrap();
    (OriginCache::Ipc(Box::new(client)), handle)
}
#[test]
fn idle_tick_refreshes_member_samples_before_max_age() {
    let f = Fixture::new();
    let old = unix_time_ms().unwrap() - GPU_BUDGET_MAX_AGE_MS - 100;
    let sample = Arc::new(AtomicU64::new(old));
    let (client, worker) = peer(Arc::clone(&sample), 4096);
    let group = CacheGroup::new(
        vec![CacheMember::new("gpu".into(), 4096, client)],
        4096,
        Duration::from_millis(50),
        Duration::from_millis(50),
    )
    .unwrap();
    let b = backend(&f, OriginCache::Group(Box::new(group)));
    let mut p = CacheStatusPublisher::default();
    let mut out = Recorder::default();
    p.publish(&mut out, &b, context());
    assert!(out.statuses[0].gpu_headroom_kib.is_none());
    let before = out.statuses[0].cache_members.as_ref().unwrap()[0]
        .budget
        .as_ref()
        .unwrap()
        .sampled_at_unix_ms;
    assert_eq!(before, old);
    p.publish(&mut out, &b, context());
    assert_eq!(out.statuses.len(), 1);
    std::thread::sleep(SERVE_IDLE_TICK);
    let fresh = unix_time_ms().unwrap();
    sample.store(fresh, Ordering::Release);

    p.publish(&mut out, &b, context());
    assert_eq!(out.statuses.len(), 2);
    let after = &out.statuses[1];
    assert!(after.gpu_headroom_kib.is_some());
    assert_eq!(
        after.cache_members.as_ref().unwrap()[0]
            .budget
            .as_ref()
            .unwrap()
            .sampled_at_unix_ms,
        fresh
    );
    assert!(fresh > before);
    drop(b);
    worker.join().unwrap();
}
#[test]
fn later_member_change_bypasses_cadence_without_aggregate_change() {
    let f = Fixture::new();
    let make = |state| OriginCache::Starting { released: state };
    let group = CacheGroup::new(
        vec![
            CacheMember::new("a".into(), 4096, make(false)),
            CacheMember::new("b".into(), 4096, make(false)),
        ],
        4096,
        Duration::from_millis(50),
        Duration::from_millis(50),
    )
    .unwrap();
    let b = backend(&f, OriginCache::Group(Box::new(group)));
    let mut p = CacheStatusPublisher::default();
    let mut out = Recorder::default();
    p.publish(&mut out, &b, context());
    let initial = b.cache_state();
    b.try_control_cache(|client| {
        let OriginCache::Group(group) = client else {
            panic!()
        };
        group.control_member(1).unwrap().replace(make(true));
    })
    .unwrap();
    assert_eq!(b.cache_state(), initial);
    p.publish(&mut out, &b, context());
    assert_eq!(out.statuses.len(), 2);
    let members = out.statuses[1].cache_members.as_ref().unwrap();
    assert_eq!(members[0].state, OriginCacheState::Unavailable);
    assert_eq!(members[1].state, OriginCacheState::Off);
    p.publish(&mut out, &b, context());
    assert_eq!(out.statuses.len(), 2);
}

#[test]
fn failed_atomic_write_does_not_retry_at_job_rate() {
    let f = Fixture::new();
    let b = backend(&f, OriginCache::Disabled(DisabledCache));
    let mut p = CacheStatusPublisher::default();
    let mut out = Recorder {
        output: Some(f.origin_path().join("not-a-directory")),
        ..Recorder::default()
    };
    for _ in 0..100 {
        p.publish(&mut out, &b, context());
    }
    assert_eq!(out.failed_writes, 1);
    p.last_attempt = Some(Instant::now() - SERVE_IDLE_TICK);
    p.publish(&mut out, &b, context());
    assert_eq!(out.failed_writes, 2);
    assert_eq!(std::fs::read(f.origin_path()).unwrap(), vec![17; 8192]);
}
#[test]
fn control_plane_state_changes_publish_before_the_interval() {
    use std::sync::{Mutex, atomic::AtomicBool};
    let f = Fixture::new();
    let b = backend(&f, OriginCache::Disabled(DisabledCache));
    let admission = ControlPlaneCacheAdmission {
        host_activity: Default::default(),
        lifetime: None,
        authority: Arc::new(Mutex::new(crate::ControlPlaneAuthority::disconnected())),
        lease_lost: Arc::new(AtomicBool::new(false)),
        lease_seen: Arc::new(AtomicBool::new(false)),
        host_memory: Arc::new(Mutex::new(None)),
        host_catalog: ramshared_wsl2d::host_catalog::SharedHostCatalog::new(),
        host_storage: ramshared_wsl2d::host_storage_state::SharedHostStorage::new(),
    };
    let mut p = CacheStatusPublisher::default();
    let mut out = Recorder::default();
    let publish = |p: &mut CacheStatusPublisher, out: &mut Recorder| {
        let mut c = context();
        c.control_admission = Some(&admission);
        p.publish(out, &b, c);
    };
    publish(&mut p, &mut out);
    assert_eq!(out.statuses.len(), 1);
    admission.authority.lock().unwrap().begin_handshake();
    publish(&mut p, &mut out);
    assert_eq!(out.statuses.len(), 2);
    assert_eq!(out.statuses[1].control_plane_state, Some("handshaking"));
    publish(&mut p, &mut out);
    assert_eq!(out.statuses.len(), 2);
    admission.authority.lock().unwrap().on_vsock_disconnect();
    publish(&mut p, &mut out);
    assert_eq!(out.statuses.len(), 3);
}

#[test]
fn origin_failure_bypasses_cadence_and_remains_explicit() {
    let f = Fixture::new();
    let b = backend(&f, OriginCache::Disabled(DisabledCache));
    let mut p = CacheStatusPublisher::default();
    let mut out = Recorder::default();
    p.publish(&mut out, &b, context());
    File::options()
        .write(true)
        .open(f.origin_path())
        .unwrap()
        .set_len(0)
        .unwrap();
    assert!(b.probe_origin().is_err());
    p.publish(&mut out, &b, context());
    assert_eq!(out.statuses.len(), 2);
    assert_eq!(out.statuses[1].origin_state, "FAILED");
    assert!(!out.statuses[1].ok);
    p.publish(&mut out, &b, context());
    assert_eq!(out.statuses.len(), 2);
}

#[test]
fn sub_kib_target_change_is_an_immediate_lifecycle_observation() {
    let f = Fixture::new();
    let sample = Arc::new(AtomicU64::new(unix_time_ms().unwrap()));
    let (first, first_worker) = peer(Arc::clone(&sample), 4097);
    let b = backend(&f, first);
    let mut p = CacheStatusPublisher::default();
    let mut out = Recorder::default();
    p.publish(&mut out, &b, context());
    let (second, second_worker) = peer(sample, 4098);
    b.replace_cache(second);
    first_worker.join().unwrap();
    p.publish(&mut out, &b, context());
    assert_eq!(out.statuses.len(), 2);
    assert_eq!(
        out.statuses[0].cache_target_kib, out.statuses[1].cache_target_kib,
        "fixture must keep the same rounded KiB"
    );
    assert_eq!(p.last_target_bytes, 4098);
    drop(b);
    second_worker.join().unwrap();
}
#[test]
fn busy_member_observation_does_not_start_a_metadata_retry_storm() {
    let f = Fixture::new();
    let b = Arc::new(backend(&f, OriginCache::Disabled(DisabledCache)));
    let mut p = CacheStatusPublisher::default();
    let mut out = Recorder::default();
    p.publish(&mut out, &b, context());
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let held = Arc::clone(&b);
    let thread = std::thread::spawn(move || {
        held.try_control_cache(|_| {
            entered_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(3));
        })
        .unwrap()
    });
    entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let started = Instant::now();
    p.publish(&mut out, &b, context());
    let elapsed = started.elapsed();
    drop(release_tx);
    thread.join().unwrap();
    assert!(
        elapsed < Duration::from_millis(250),
        "metadata observer waited for a busy cache: {elapsed:?}"
    );
    assert_eq!(out.statuses.len(), 1);
}

#[test]
fn omitted_member_capture_waits_for_cadence_instead_of_retrying_each_job() {
    let f = Fixture::new();
    let group = CacheGroup::new(
        vec![CacheMember::new(
            "member".into(),
            4096,
            OriginCache::Disabled(DisabledCache),
        )],
        4096,
        Duration::from_millis(50),
        Duration::from_millis(50),
    )
    .unwrap();
    let b = backend(&f, OriginCache::Group(Box::new(group)));
    let mut p = CacheStatusPublisher::default();
    let mut out = Recorder::default();
    p.publish(&mut out, &b, context());
    // Reproduce the documented wire absence from a busy optional snapshot;
    // no state transition has been observed since the preceding publication.
    p.last_status.as_mut().unwrap().cache_members = None;
    for _ in 0..100 {
        p.publish(&mut out, &b, context());
    }
    assert_eq!(
        out.statuses.len(),
        1,
        "missing optional metadata became a retry storm"
    );
    p.last_attempt = Some(Instant::now() - SERVE_IDLE_TICK);
    p.publish(&mut out, &b, context());
    assert_eq!(out.statuses.len(), 2);
    assert!(out.statuses[1].cache_members.is_some());
}

#[test]
fn isolated_cache_publication_does_not_invent_worker_counters() {
    let f = Fixture::new();
    let b = backend(&f, OriginCache::Starting { released: false });
    b.write_at(0, &[7; 4096]).unwrap();
    let mut publisher = CacheStatusPublisher::default();
    let mut out = Recorder::default();
    publisher.publish(&mut out, &b, context());
    let io = &out.statuses.last().unwrap().origin_cache_io;
    assert_eq!(io.batched_writes, 1);
    assert_eq!(io.promotion_refusals, None);
    assert_eq!(io.allocation_failures, None);
    assert_eq!(io.valid_blocks, None);
    let wire = serde_json::to_value(io).unwrap();
    assert!(wire["promotion_refusals"].is_null());
    assert!(wire["allocation_failures"].is_null());
    assert!(wire["valid_blocks"].is_null());
}

#[test]
fn unavailable_ipc_cache_keeps_origin_ok_true_with_unmeasured_occupancy() {
    let f = Fixture::new();
    let (client_end, worker_end) = UnixStream::pair().expect("socketpair");
    drop(worker_end);
    let mut client = IpcCacheClient::new(client_end, Duration::from_millis(50), 4096);
    let _ = client.refresh_cached_bytes();
    let b = backend(&f, OriginCache::Ipc(Box::new(client)));
    let mut publisher = CacheStatusPublisher::default();
    let mut out = Recorder::default();
    publisher.publish(&mut out, &b, context());
    let status = out.statuses.last().expect("published status");
    assert!(
        status.ok,
        "ready origin with unavailable cache must remain ok for startup and teardown"
    );
    assert_eq!(status.origin_state, "READY");
    assert_eq!(status.cache_state, "UNAVAILABLE");
    assert_eq!(status.vram_cached_kib, None);
}
