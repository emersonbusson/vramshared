// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main_tests/worker_set.rs
//! Collection lifecycle: real creators/processes, synthetic adapter authority.
use super::*;
use crate::worker_set::WorkerSet;
use std::sync::mpsc::{SyncSender, sync_channel};

fn empty_runtime(index: usize) -> IsolatedWorkerRuntime {
    IsolatedWorkerRuntime {
        cache_member: index,
        startup: None,
        supervisor: None,
        params: WorkerSpawnParams {
            generation: None,
            target_bytes: 4096,
            chunk_bytes: 4096,
            reserve_floor: 0,
            expected_adapter: None,
            host_catalog: None,
        },
        budget: WorkerRespawnBudget::new(),
        unavailable_reason: None,
        lease_lost_release: false,
        permanent_fallback_logged: false,
    }
}

fn held_creator(index: usize) -> (IsolatedWorkerRuntime, SyncSender<()>) {
    let custody = Arc::new(worker_startup::StartupCustody::default());
    let (entered_tx, entered_rx) = sync_channel(1);
    let (release_tx, release_rx) = sync_channel(1);
    let startup = custody
        .begin(Box::new(move || {
            entered_tx.send(()).unwrap();
            // Dropping the sender on assertion failure also releases this
            // owned barrier, so no test leaves an abandoned creator behind.
            let _ = release_rx.recv_timeout(Duration::from_secs(3));
            worker_startup::StartOutcome {
                result: Err("fixture creator released".into()),
                creator_child: None,
            }
        }))
        .unwrap();
    entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let mut runtime = empty_runtime(index);
    runtime.startup = Some(startup);
    (runtime, release_tx)
}

#[test]
fn worker_set_shutdown_uses_one_deadline_for_all_creators() {
    let (mut members, mut gates) = (Vec::new(), Vec::new());
    for index in 0..4 {
        let (runtime, gate) = held_creator(index);
        members.push(runtime);
        gates.push(gate);
    }
    let mut workers = WorkerSet::new(members);
    let start = Instant::now();
    let complete = workers.shutdown(Duration::from_millis(30), Duration::from_millis(1));
    let elapsed = start.elapsed();
    let cancelled = workers
        .iter_mut()
        .filter(|worker| worker.startup.as_ref().is_some_and(|s| s.is_cancelled()))
        .count();
    drop(gates);
    let reconciled = workers.shutdown(Duration::from_secs(3), Duration::from_millis(1));
    assert!(!complete, "held creators cannot be reported quiescent");
    assert_eq!(cancelled, 4, "shutdown skipped a later creator");
    assert!(reconciled, "released creators did not reconcile");
    assert!(
        elapsed < Duration::from_millis(90),
        "collection shutdown multiplied the per-member wait: {elapsed:?}"
    );
}

#[test]
fn pending_first_creator_cannot_skip_later_native_stop() {
    let (first, gate) = held_creator(0);
    let mut child = ProcessCommand::new("/bin/sh")
        .args(["-c", "read line"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let input = child.stdin.take().unwrap();
    let owner = Arc::new(worker_custody::WorkerCustody::default());
    let resources = owner.reserve().unwrap().bind(child);
    let mut second = empty_runtime(1);
    second.supervisor = Some(IsolatedWorkerSupervisor {
        child: Some(WorkerChildHandle::Process(
            ChildWorkerProcess::from_resources(resources.clone()),
        )),
    });
    let mut workers = WorkerSet::new(vec![first, second]);
    let complete = workers.shutdown(Duration::ZERO, Duration::from_millis(1));
    let stop_admitted = resources.stop_active.load(Ordering::Acquire);
    let owned = owner.reserve().is_err();
    drop(input);
    drop(gate);
    let reconciled = workers.shutdown(Duration::from_secs(3), Duration::from_millis(1));
    assert!(!complete, "held creator was reported complete");
    assert!(
        stop_admitted,
        "pending first creator skipped a later native stop"
    );
    assert!(owned, "unconfirmed native child lost its exact custody");
    assert!(reconciled);
    assert!(resources.exit_status.get().is_some());
    assert!(owner.reserve().is_ok());
}

#[test]
fn unknown_native_exit_cannot_skip_later_creator_cancellation() {
    let mut child = ProcessCommand::new("/bin/sh")
        .args(["-c", "read line"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let input = child.stdin.take().unwrap();
    let owner = Arc::new(worker_custody::WorkerCustody::default());
    let resources = owner.reserve().unwrap().bind(child);
    let observation = resources.child.lock().unwrap();
    let mut first = empty_runtime(0);
    first.supervisor = Some(IsolatedWorkerSupervisor {
        child: Some(WorkerChildHandle::Process(
            ChildWorkerProcess::from_resources(resources.clone()),
        )),
    });
    let (second, gate) = held_creator(1);
    let mut workers = WorkerSet::new(vec![first, second]);
    let complete = workers.shutdown(Duration::ZERO, Duration::from_millis(1));
    let cancelled = workers
        .iter_mut()
        .nth(1)
        .unwrap()
        .startup
        .as_ref()
        .unwrap()
        .is_cancelled();
    let owned = owner.reserve().is_err();
    drop(observation);
    drop(input);
    drop(gate);
    let reconciled = workers.shutdown(Duration::from_secs(3), Duration::from_millis(1));
    assert!(
        !complete,
        "unknown native observation became a successful exit"
    );
    assert!(cancelled, "unknown first child skipped a later creator");
    assert!(owned);
    assert!(reconciled);
    assert!(owner.reserve().is_ok());
}

#[test]
fn collection_observation_keeps_later_failure_and_pending_startup() {
    let first = empty_runtime(0);
    let (mut second, gate) = held_creator(1);
    second.unavailable_reason = Some("second adapter startup pending".into());
    let mut workers = WorkerSet::new(vec![first, second]);
    let observation = workers.observation(true, None);
    assert!(observation.critical_reclaim);
    assert!(observation.startup_pending);
    assert_eq!(
        observation.unavailable_reason,
        Some("second adapter startup pending")
    );
    assert_eq!(
        workers
            .observation(false, Some("lease unavailable"))
            .unavailable_reason,
        Some("lease unavailable")
    );
    drop(gate);
    assert!(workers.shutdown(Duration::from_secs(3), Duration::from_millis(1)));
    assert!(!workers.observation(false, None).startup_pending);
}

#[test]
fn collection_empty_zero_and_overflow_waits_are_explicit() {
    let mut workers = WorkerSet::default();
    let observation = workers.observation(false, None);
    assert!(!observation.critical_reclaim);
    assert!(!observation.startup_pending);
    assert!(observation.unavailable_reason.is_none());
    assert!(workers.shutdown(Duration::ZERO, Duration::ZERO));
    assert!(!workers.shutdown(Duration::MAX, Duration::from_millis(1)));
    let (pending, gate) = held_creator(0);
    let mut workers = WorkerSet::new(vec![pending]);
    assert!(!workers.shutdown(Duration::from_secs(3), Duration::ZERO));
    assert!(
        workers
            .iter_mut()
            .next()
            .unwrap()
            .startup
            .as_ref()
            .unwrap()
            .is_cancelled()
    );
    drop(gate);
    assert!(workers.shutdown(Duration::from_secs(3), Duration::from_millis(1)));
}

#[test]
fn lease_loss_marks_and_cancels_every_pending_member() {
    let (backend, _, _starter) = respawn_fixture(false, true);
    let (first, first_gate) = held_creator(0);
    let (second, second_gate) = held_creator(1);
    let mut workers = WorkerSet::new(vec![first, second]);
    let admission = ControlPlaneCacheAdmission {
        lifetime: None,
        authority: Arc::new(Mutex::new(ControlPlaneAuthority::disconnected())),
        lease_lost: Arc::new(AtomicBool::new(false)),
        lease_seen: Arc::new(AtomicBool::new(true)),
        host_memory: Arc::new(Mutex::new(None)),
        host_catalog: ramshared_wsl2d::host_catalog::SharedHostCatalog::new(),
        host_activity: Default::default(),
        host_storage: ramshared_wsl2d::host_storage_state::SharedHostStorage::new(),
    };
    release_cache_after_lease_loss(&backend, Some(&admission), workers.iter_mut());
    assert!(workers.iter_mut().all(|worker| !worker.lease_lost_release));
    admission.lease_lost.store(true, Ordering::Release);
    for _ in 0..2 {
        release_cache_after_lease_loss(&backend, Some(&admission), workers.iter_mut());
    }
    for worker in workers.iter_mut() {
        assert!(
            worker.lease_lost_release,
            "lease loss skipped a later member"
        );
        assert!(worker.startup.as_ref().unwrap().is_cancelled());
        assert_eq!(worker.budget.respawns_since_healthy, 1);
    }
    drop((first_gate, second_gate));
    assert!(workers.shutdown(Duration::from_secs(3), Duration::from_millis(1)));
}

const FIRST_KEY: &str = "00112233445566778899aabbccddeeff";
const SECOND_KEY: &str = "ffeeddccbbaa99887766554433221100";

struct SetStarter {
    published: Vec<OriginCacheStatus>,
    members: std::collections::BTreeMap<String, RespawnTestStarter>,
    requests: Vec<String>,
    refuse: bool,
}
impl NbdRuntimeStarter for SetStarter {
    fn publish_origin_cache(&mut self, status: &OriginCacheStatus) {
        self.published.push(status.clone());
    }
    fn begin_isolated_worker_start(
        &mut self,
        params: &WorkerSpawnParams,
    ) -> std::io::Result<worker_startup::Startup> {
        let key = &params.expected_adapter.as_ref().unwrap().key;
        self.requests.push(key.clone());
        if self.refuse {
            return Err(std::io::ErrorKind::WouldBlock.into());
        }
        self.members
            .get_mut(key)
            .unwrap()
            .begin_isolated_worker_start(params)
    }
    fn lock_memory(&mut self, _: bool, _: bool) -> Result<(), Box<dyn std::error::Error>> {
        panic!("collection recovery must not lock memory")
    }
    fn start_acceptor(
        &mut self,
        _: UnixListener,
        _: Arc<Vec<ramshared_block::handshake::Export>>,
        _: u16,
        _: std::sync::mpsc::SyncSender<WMsg>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        panic!("collection recovery must not start an acceptor")
    }
    fn nbd_used_kb(&mut self, _: &str) -> u64 {
        panic!("collection recovery must not inspect swap")
    }
    fn try_claim_shutdown(&mut self, _: &str) -> Option<ShutdownPermit> {
        panic!("collection recovery must not claim transport shutdown")
    }
    fn publish_demote(&mut self, _: &DemoteStatusFile) {
        panic!("collection recovery must not demote")
    }
    fn elapsed_us(&mut self, _: Instant) -> u64 {
        panic!("collection recovery must not measure demotion")
    }
    fn spawn_swapoff(&mut self, _: &str) -> std::sync::mpsc::Receiver<SwapoffOutcome> {
        panic!("collection recovery must not swapoff")
    }
    fn spawn_recovery_activation(
        &mut self,
        _: &str,
        _: i16,
    ) -> Result<std::sync::mpsc::Receiver<bool>, Box<dyn std::error::Error>> {
        panic!("collection recovery must not activate swap")
    }
}

struct SetFixture {
    backend: AuthoritativeOriginBackend<FileOrigin, OriginCache>,
    workers: WorkerSet,
    starter: SetStarter,
}
impl SetFixture {
    fn new() -> Self {
        let (backend, mut first, first_starter) =
            respawn_fixture_with_admission(true, true, ramshared_block::AllowCacheAdmission);
        let (other_backend, mut second, second_starter) = respawn_fixture(true, true);
        drop(other_backend);
        let mut caches = Vec::new();
        for (index, (key, runtime)) in [(FIRST_KEY, &mut first), (SECOND_KEY, &mut second)]
            .into_iter()
            .enumerate()
        {
            runtime.cache_member = index;
            runtime.params.expected_adapter = Some(GpuAdapterIdentity {
                node_mask: None,
                backend: "cuda".into(),
                key: key.into(),
                luid: None,
            });
            caches.push(ramshared_block::cache_group::CacheMember::new(
                key.into(),
                runtime.params.target_bytes,
                OriginCache::Disabled(DisabledCache),
            ));
        }
        backend.replace_cache(OriginCache::Group(Box::new(
            ramshared_block::cache_group::CacheGroup::new(
                caches,
                first.params.chunk_bytes,
                ramshared_block::DEFAULT_READ_TIMEOUT,
                ramshared_block::ipc_cache_client::DISABLE_TIMEOUT,
            )
            .unwrap(),
        )));
        Self {
            backend,
            workers: WorkerSet::new(vec![first, second]),
            starter: SetStarter {
                published: Vec::new(),
                members: std::collections::BTreeMap::from([
                    (FIRST_KEY.into(), first_starter),
                    (SECOND_KEY.into(), second_starter),
                ]),
                requests: Vec::new(),
                refuse: false,
            },
        }
    }

    fn recover_until(&mut self, phase: &str, mut done: impl FnMut(&mut WorkerSet) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            self.workers.recover(&mut self.starter, &self.backend, true);
            if done(&mut self.workers) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "collection {phase} did not settle: {:?}",
                self.workers.observation(false, None).unavailable_reason
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

#[test]
fn collection_recovery_rotates_admission_without_moving_member_slots() {
    let mut fixture = SetFixture::new();
    fixture.starter.refuse = true;
    WorkerSet::default().recover(&mut fixture.starter, &fixture.backend, true);
    assert!(fixture.starter.requests.is_empty());
    for _ in 0..2 {
        fixture
            .workers
            .recover(&mut fixture.starter, &fixture.backend, true);
    }
    assert_eq!(
        fixture.starter.requests,
        [FIRST_KEY, SECOND_KEY, SECOND_KEY, FIRST_KEY]
    );
    assert_eq!(
        fixture
            .workers
            .iter_mut()
            .map(|worker| worker.cache_member)
            .collect::<Vec<_>>(),
        [0, 1]
    );
    assert!(
        fixture
            .workers
            .iter_mut()
            .all(|worker| worker.budget.respawns_since_healthy == 0)
    );
}

struct ReleaseHandshake(Option<SyncSender<()>>);
impl ReleaseHandshake {
    fn release(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.try_send(());
        }
    }
}
impl Drop for ReleaseHandshake {
    fn drop(&mut self) {
        self.release();
    }
}

#[test]
fn delayed_member_does_not_block_peer_start_or_independent_recovery() {
    let mut fixture = SetFixture::new();
    let (entered_tx, entered_rx) = sync_channel(1);
    let (release_tx, release_rx) = sync_channel(1);
    let mut release = ReleaseHandshake(Some(release_tx));
    fixture
        .starter
        .members
        .get_mut(FIRST_KEY)
        .unwrap()
        .handshake_gate = Some(Arc::new(StartupDelay {
        entered: entered_tx,
        release: Mutex::new(release_rx),
    }));
    fixture
        .workers
        .recover(&mut fixture.starter, &fixture.backend, true);
    entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    fixture.recover_until("independent peer start", |workers| {
        workers.iter_mut().nth(1).unwrap().supervisor.is_some()
    });
    assert!(fixture.workers.iter_mut().next().unwrap().startup.is_some());
    assert!(fixture.workers.observation(false, None).startup_pending);
    fixture
        .backend
        .write_at_with_options(0, &[71; 4096], WriteOptions { fua: true })
        .unwrap();
    fixture.backend.flush().unwrap();
    let mut actual = [0; 4096];
    fixture.backend.read_at(0, &mut actual).unwrap();
    assert_eq!(
        actual, [71; 4096],
        "origin I/O failed during member startup"
    );
    release.release();
    fixture.recover_until("held member handoff", |workers| {
        workers
            .iter_mut()
            .all(|w| w.startup.is_none() && w.supervisor.is_some())
    });
    assert_eq!(fixture.backend.cache_state(), OriginCacheState::Active);
    drop(fixture.workers.iter_mut().nth(1).unwrap().supervisor.take());
    fixture.recover_until("second member replacement", |workers| {
        workers.iter_mut().nth(1).unwrap().supervisor.is_some()
    });
    assert_eq!(
        fixture.starter.members[FIRST_KEY]
            .attempts
            .load(Ordering::Acquire),
        1
    );
    assert_eq!(
        fixture.starter.members[SECOND_KEY]
            .attempts
            .load(Ordering::Acquire),
        2
    );
    assert_eq!(fixture.backend.cache_state(), OriginCacheState::Active);
    fixture
        .backend
        .try_control_cache(|cache| {
            let OriginCache::Group(group) = cache else {
                panic!("collection lost its group")
            };
            let statuses = group.member_statuses();
            assert_eq!(statuses[0].adapter.as_ref().unwrap().key, FIRST_KEY);
            assert_eq!(statuses[1].adapter.as_ref().unwrap().key, SECOND_KEY);
        })
        .unwrap();
    fixture.backend.release_cache().unwrap();
    assert!(
        fixture
            .workers
            .shutdown(Duration::from_secs(3), Duration::from_millis(1))
    );
}

#[test]
fn publication_keeps_each_real_ipc_budget_without_an_aggregate_gpu() {
    let mut fixture = SetFixture::new();
    fixture.recover_until("both publication members", |workers| {
        workers
            .iter_mut()
            .all(|w| w.startup.is_none() && w.supervisor.is_some())
    });
    publish_origin_cache_status(
        &mut fixture.starter,
        &fixture.backend,
        "synthetic-collection",
        2,
        0,
        fixture.workers.observation(false, None),
        None,
    );
    let status = fixture.starter.published.last().unwrap();
    assert!(status.gpu_budget.is_none());
    assert!(status.gpu_headroom_kib.is_none());
    let members = status.cache_members.as_ref().unwrap();
    assert_eq!(members.len(), 2);
    for (member, key) in members.iter().zip([FIRST_KEY, SECOND_KEY]) {
        assert_eq!(member.state, OriginCacheState::Active);
        assert_eq!(member.adapter.as_ref().unwrap().key, key);
        assert_eq!(member.budget.as_ref().unwrap().adapter, member.adapter);
        assert_eq!(member.cached_bytes, Some(0));
    }
    let encoded = serde_json::to_value(status).unwrap();
    let decoded: Vec<ramshared_block::cache_group::CacheMemberStatus> =
        serde_json::from_value(encoded["cache_members"].clone()).unwrap();
    assert_eq!(decoded[1].adapter.as_ref().unwrap().key, SECOND_KEY);
    fixture.backend.release_cache().unwrap();
    assert!(
        fixture
            .workers
            .shutdown(Duration::from_secs(3), Duration::from_millis(1))
    );
}

#[test]
fn oversized_member_status_cannot_replace_the_published_document() {
    let mut fixture = SetFixture::new();
    publish_origin_cache_status(
        &mut fixture.starter,
        &fixture.backend,
        "synthetic-status-size",
        0,
        0,
        fixture.workers.observation(false, None),
        None,
    );
    let mut status = fixture.starter.published.pop().unwrap();
    let root = std::env::temp_dir().join(format!(
        "ramshared-publication-bound-{}-{}",
        std::process::id(),
        unix_time_ms().unwrap()
    ));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("status.json");
    write_origin_cache_status(&path, &status).unwrap();
    let original = std::fs::read(&path).unwrap();
    {
        use ramshared_vram::host_gpu_activity::{
            HostGpuActivityReport, HostGpuActivitySource, HostGpuActivityTelemetry,
            HostGpuAdapterActivity, HostGpuEngineActivity,
        };
        status.host_gpu_activity = Some(HostGpuActivityTelemetry {
            valid_before_unix_ms: 200,
            sampled_at_guest_unix_ms: Some(90),
            preferred_host_adapter: None,
            report: HostGpuActivityReport {
                schema_version: 1,
                source: HostGpuActivitySource::WddmPdh,
                sampled_at_unix_ms: 100,
                interval_ms: 1_000,
                adapters: vec![HostGpuAdapterActivity {
                    luid: "00000000:00000001".into(),
                    physical_index: 0,
                    engines: (0..640)
                        .map(|index| HostGpuEngineActivity {
                            index,
                            kind: "X".repeat(110),
                            busy_pct: 44,
                            processes: None,
                        })
                        .collect(),
                }],
            },
        });
        write_origin_cache_status(&path, &status).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }
    status.daemon_instance_id = "x".repeat(ramshared_block::cache_group::MAX_CACHE_STATUS_BYTES);
    let refused = write_origin_cache_status(&path, &status).is_err();
    let unchanged = std::fs::read(&path).unwrap() == original;
    let entries = std::fs::read_dir(&root).unwrap().count();
    std::fs::remove_file(&path).unwrap();
    std::fs::remove_dir(&root).unwrap();
    assert!(refused, "oversized publication was accepted");
    assert!(
        unchanged,
        "oversized publication replaced the previous document"
    );
    assert_eq!(entries, 1, "refused publication left staging residue");
}

#[test]
fn pressure_release_marker_requires_acknowledged_revocation_and_is_sticky() {
    let (_, first, _) = respawn_fixture(true, true);
    let mut workers = WorkerSet::new(vec![first]);
    workers.record_pressure_release(vec![]);
    assert!(!workers.pressure_release_pending());
    workers.record_pressure_release(vec![(0, false)]);
    assert!(workers.pressure_release_pending());
    workers.record_pressure_release(vec![]);
    assert!(workers.pressure_release_pending());
}

#[test]
fn pressure_recovery_refuses_foreign_or_missing_supervisor_observation() {
    let (_, first, _) = respawn_fixture(true, true);
    let mut workers = WorkerSet::new(vec![first]);
    assert!(
        !workers.pressure_recovery_allowed("foreign-daemon", 1),
        "an independently failed worker must not restart without matching healthy supervision"
    );
    workers.record_pressure_release(vec![(0, false)]);
    assert!(!workers.pressure_recovery_allowed("foreign-daemon", 1));
    assert!(workers.pressure_release_pending());
}

#[test]
fn pressure_recovery_arms_only_attributed_multi_gpu_members() {
    let (_, first, _) = respawn_fixture(true, true);
    let (_, second, _) = respawn_fixture(true, true);
    let mut workers = WorkerSet::new(vec![first, second]);
    for (index, member) in workers.iter_mut().enumerate() {
        member.cache_member = index;
        member.lease_lost_release = false;
    }
    workers.record_pressure_release(vec![(0, false)]);
    workers.arm_pressure_members();
    assert_eq!(
        workers
            .iter_mut()
            .map(|m| m.lease_lost_release)
            .collect::<Vec<_>>(),
        vec![true, false]
    );
    assert!(workers.pressure_release_pending());
}

#[test]
fn pressure_recovery_does_not_rearm_already_recovered_members() {
    let (_, first, _) = respawn_fixture(true, true);
    let (_, second, _) = respawn_fixture(true, true);
    let mut workers = WorkerSet::new(vec![first, second]);
    for (index, member) in workers.iter_mut().enumerate() {
        member.cache_member = index;
    }
    workers.record_pressure_release(vec![(0, false), (1, false)]);
    workers.arm_pressure_members();
    workers.iter_mut().next().unwrap().lease_lost_release = false;
    workers.arm_pressure_members();
    let flags: Vec<_> = workers
        .iter_mut()
        .map(|member| member.lease_lost_release)
        .collect();
    assert_eq!(flags, vec![false, true]);
    workers.record_pressure_release(vec![(0, false), (1, false)]);
    workers.arm_pressure_members();
    assert!(workers.iter_mut().all(|member| member.lease_lost_release));
}

#[test]
#[ignore = "requires the installed healthy supervisor and root observation"]
fn healthy_supervisor_arms_only_the_released_worker_on_live_host() {
    let bytes = crate::manifest_file::read_bounded_manifest_file(
        std::path::Path::new("/run/ramshared/supervisor-state.json"),
        true,
    )
    .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let daemon = value["daemon_instance_id"].as_str().unwrap();
    let now = crate::control_requests::unix_time_ms().unwrap();
    assert!(crate::pressure_recovery::healthy_observation(
        &value, daemon, now
    ));
    let mut workers = WorkerSet::new(vec![empty_runtime(0), empty_runtime(1)]);
    workers.record_pressure_release(vec![(0, false)]);
    assert!(workers.pressure_recovery_allowed(daemon, now));
    let armed: Vec<_> = workers
        .iter_mut()
        .map(|member| member.lease_lost_release)
        .collect();
    assert_eq!(armed, vec![true, false]);
}
