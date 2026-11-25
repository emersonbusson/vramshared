// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main_tests/startup_recovery.rs
//! Startup-delay and authority-loss origin-serving regressions.
use super::*;

#[test]
fn lease_loss_revokes_restricted_group_without_origin_io() {
    let (backend, mut runtime, mut starter) = respawn_fixture(true, true);
    recover_and_settle(&mut starter, &backend, &mut runtime, true);
    let live = backend
        .try_control_cache(|cache| std::mem::replace(cache, OriginCache::Disabled(DisabledCache)))
        .unwrap();
    let group = ramshared_block::cache_group::CacheGroup::new(
        vec![
            ramshared_block::cache_group::CacheMember::new(
                "a".into(),
                runtime.params.target_bytes,
                OriginCache::Disabled(DisabledCache),
            ),
            ramshared_block::cache_group::CacheMember::new(
                "b".into(),
                runtime.params.target_bytes,
                live,
            ),
        ],
        runtime.params.chunk_bytes,
        ramshared_block::DEFAULT_READ_TIMEOUT,
        ramshared_block::ipc_cache_client::DISABLE_TIMEOUT,
    )
    .unwrap();
    backend.replace_cache(OriginCache::Group(Box::new(group)));
    assert_eq!(backend.cache_state(), OriginCacheState::Restricted);
    let admission = ControlPlaneCacheAdmission {
        lifetime: None,
        authority: Arc::new(Mutex::new(ControlPlaneAuthority::disconnected())),
        lease_lost: Arc::new(AtomicBool::new(true)),
        lease_seen: Arc::new(AtomicBool::new(true)),
        host_memory: Arc::new(Mutex::new(None)),
        host_catalog: ramshared_wsl2d::host_catalog::SharedHostCatalog::new(),
        host_storage: ramshared_wsl2d::host_storage_state::SharedHostStorage::new(),
    };
    release_cache_after_lease_loss(&backend, Some(&admission), None);
    backend
        .try_control_cache(|cache| {
            let OriginCache::Group(group) = cache else {
                panic!("group vanished")
            };
            assert_eq!(
                group.member_statuses()[1].state,
                OriginCacheState::Off,
                "lease loss left a serving peer in the restricted group"
            );
        })
        .unwrap();
    drop(runtime);
}

#[test]
fn recovering_group_member_keeps_healthy_peer_identity_and_cache() {
    let (backend, mut runtime, mut starter) = respawn_fixture(true, true);
    let (peer_backend, mut peer_runtime, mut peer_starter) = respawn_fixture(true, true);
    let peer_identity = GpuAdapterIdentity {
        node_mask: None,
        backend: "cuda".into(),
        key: "ffeeddccbbaa99887766554433221100".into(),
        luid: None,
    };
    peer_runtime.params.expected_adapter = Some(peer_identity.clone());
    recover_and_settle(&mut peer_starter, &peer_backend, &mut peer_runtime, true);
    let peer_cache = peer_backend
        .try_control_cache(|cache| std::mem::replace(cache, OriginCache::Disabled(DisabledCache)))
        .unwrap();
    let group = ramshared_block::cache_group::CacheGroup::new(
        vec![
            ramshared_block::cache_group::CacheMember::new(
                "a".into(),
                runtime.params.target_bytes,
                OriginCache::Disabled(DisabledCache),
            ),
            ramshared_block::cache_group::CacheMember::new(
                "b".into(),
                peer_runtime.params.target_bytes,
                peer_cache,
            ),
        ],
        runtime.params.chunk_bytes,
        ramshared_block::DEFAULT_READ_TIMEOUT,
        ramshared_block::ipc_cache_client::DISABLE_TIMEOUT,
    )
    .unwrap();
    backend.replace_cache(OriginCache::Group(Box::new(group)));
    assert_eq!(backend.cache_state(), OriginCacheState::Restricted);
    recover_and_settle(&mut starter, &backend, &mut runtime, true);
    assert_eq!(starter.attempts.load(Ordering::Acquire), 1);
    assert_eq!(backend.cache_state(), OriginCacheState::Active);
    backend
        .try_control_cache(|cache| {
            let OriginCache::Group(group) = cache else {
                panic!("recovery replaced the complete group")
            };
            let members = group.member_statuses();
            assert_eq!(members[1].adapter, Some(peer_identity));
            assert_eq!(members[1].state, OriginCacheState::Active);
            assert_eq!(members[1].failures, 0);
        })
        .unwrap();
    runtime.cache_member = 5;
    recover_isolated_gpu_worker_if_failed(&mut starter, &backend, &mut runtime, true);
    assert_eq!(
        starter.attempts.load(Ordering::Acquire),
        1,
        "invalid member touched another worker"
    );
    drop(runtime);
    drop(peer_runtime);
    drop(backend);
    drop(peer_backend);
}

#[test]
fn busy_or_invalid_cache_control_refuses_without_spending_a_retry() {
    let (backend, mut runtime, mut starter) = respawn_fixture(true, true);
    let backend = Arc::new(backend);
    let held_backend = Arc::clone(&backend);
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let holder = std::thread::spawn(move || {
        held_backend
            .try_control_cache(|_| {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            })
            .unwrap();
    });
    entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let started = Instant::now();
    recover_isolated_gpu_worker_if_failed(&mut starter, &backend, &mut runtime, true);
    let elapsed = started.elapsed();
    let _ = release_tx.send(());
    holder.join().unwrap();
    assert!(
        elapsed < Duration::from_millis(250),
        "recovery waited for a busy cache client"
    );
    assert_eq!(starter.attempts.load(Ordering::Acquire), 0);
    runtime.cache_member = 1;
    recover_isolated_gpu_worker_if_failed(&mut starter, &backend, &mut runtime, true);
    assert_eq!(starter.attempts.load(Ordering::Acquire), 0);
    runtime.cache_member = 0;
    recover_and_settle(&mut starter, &backend, &mut runtime, true);
    assert_eq!(backend.cache_state(), OriginCacheState::Active);
    drop(runtime);
}

fn origin_io_while_startup_is_held(non_cache_delay: Duration) {
    let (backend, runtime, mut starter) = respawn_fixture(true, true);
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    starter.handshake_gate = Some(Arc::new(StartupDelay {
        entered: entered_tx,
        release: Mutex::new(release_rx),
    }));
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
    let serving = std::thread::spawn(move || {
        let mut runtime = runtime;
        let recovery_started = Instant::now();
        recover_isolated_gpu_worker_if_failed(&mut starter, &backend, &mut runtime, true);
        let recovery_elapsed = recovery_started.elapsed();
        // Explicit fixture work outside cache recovery. Storage has its own
        // latency; it must not impersonate a blocked IPC startup observation.
        std::thread::sleep(non_cache_delay);
        let payload = vec![0x69; 4096];
        backend.write_at(0, &payload).unwrap();
        backend.flush().unwrap();
        let mut actual = vec![0; 4096];
        backend.read_at(0, &mut actual).unwrap();
        done_tx
            .send((runtime, actual == payload, recovery_elapsed))
            .unwrap();
    });
    entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let timely = done_rx.recv_timeout(Duration::from_secs(3));
    let served_during_startup = timely.is_ok();
    let _ = release_tx.send(());
    let (runtime, correct, recovery_elapsed) =
        timely.unwrap_or_else(|_| done_rx.recv_timeout(Duration::from_secs(3)).unwrap());
    serving.join().unwrap();
    drop(runtime);
    assert!(
        served_during_startup,
        "origin I/O did not complete while the startup handshake remained held"
    );
    assert!(correct, "origin bytes changed during startup");
    assert!(
        recovery_elapsed < Duration::from_millis(250),
        "cache recovery waited for startup: {recovery_elapsed:?}"
    );
}

#[test]
fn worker_startup_delay_keeps_authoritative_origin_serving() {
    origin_io_while_startup_is_held(Duration::ZERO);
}

#[test]
fn slow_non_cache_phase_cannot_impersonate_a_startup_wait() {
    origin_io_while_startup_is_held(Duration::from_millis(300));
}

#[test]
fn cancelled_worker_startup_cannot_reactivate_cache() {
    for lease_live in [true, false] {
        let (backend, mut runtime, mut starter) = respawn_fixture(true, true);
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        starter.handshake_gate = Some(Arc::new(StartupDelay {
            entered: entered_tx,
            release: Mutex::new(release_rx),
        }));
        recover_isolated_gpu_worker_if_failed(&mut starter, &backend, &mut runtime, true);
        entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        backend.release_cache().unwrap();
        recover_isolated_gpu_worker_if_failed(&mut starter, &backend, &mut runtime, lease_live);
        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while runtime.startup.is_some() {
            assert!(
                Instant::now() < deadline,
                "cancelled startup did not retire"
            );
            recover_isolated_gpu_worker_if_failed(&mut starter, &backend, &mut runtime, true);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            backend.cache_state(),
            OriginCacheState::Off,
            "late result reactivated released cache"
        );
        assert!(runtime.supervisor.is_none());
        assert_eq!(starter.attempts.load(Ordering::Acquire), 1);
    }
}

#[test]
fn lease_return_cannot_accept_startup_cancelled_by_prior_loss() {
    let (backend, mut runtime, mut starter) = respawn_fixture(true, true);
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    starter.handshake_gate = Some(Arc::new(StartupDelay {
        entered: entered_tx,
        release: Mutex::new(release_rx),
    }));
    recover_isolated_gpu_worker_if_failed(&mut starter, &backend, &mut runtime, true);
    entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let admission = ControlPlaneCacheAdmission {
        lifetime: None,
        authority: Arc::new(Mutex::new(ControlPlaneAuthority::disconnected())),
        lease_lost: Arc::new(AtomicBool::new(true)),
        lease_seen: Arc::new(AtomicBool::new(true)),
        host_memory: Arc::new(Mutex::new(None)),
        host_catalog: ramshared_wsl2d::host_catalog::SharedHostCatalog::new(),
        host_storage: ramshared_wsl2d::host_storage_state::SharedHostStorage::new(),
    };
    release_cache_after_lease_loss(&backend, Some(&admission), Some(&mut runtime));
    assert!(runtime.startup.as_ref().unwrap().is_cancelled());
    admission.lease_lost.store(false, Ordering::Release);
    release_tx.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while runtime.startup.is_some() {
        assert!(Instant::now() < deadline);
        recover_isolated_gpu_worker_if_failed(&mut starter, &backend, &mut runtime, true);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(runtime.supervisor.is_none());
    assert_eq!(starter.attempts.load(Ordering::Acquire), 1);
    assert_eq!(runtime.budget.respawns_since_healthy, 1);
    assert_eq!(backend.cache_state(), OriginCacheState::Unavailable);
    assert!(runtime.lease_lost_release);
}

#[test]
fn pending_startup_shutdown_keeps_unconfirmed_ownership() {
    let (backend, mut runtime, mut starter) = respawn_fixture(true, true);
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    starter.handshake_gate = Some(Arc::new(StartupDelay {
        entered: entered_tx,
        release: Mutex::new(release_rx),
    }));
    recover_isolated_gpu_worker_if_failed(&mut starter, &backend, &mut runtime, true);
    entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    backend.release_cache().unwrap();
    assert!(!worker_recovery::shutdown_worker_runtime(
        &mut runtime,
        Duration::ZERO,
        Duration::from_millis(1)
    ));
    assert!(runtime.startup.as_ref().unwrap().is_cancelled());
    assert!(!runtime.startup.as_ref().unwrap().is_quiescent());
    assert!(runtime.supervisor.is_none());
    release_tx.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !runtime.startup.as_ref().unwrap().is_quiescent() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(worker_recovery::shutdown_worker_runtime(
        &mut runtime,
        Duration::ZERO,
        Duration::from_millis(1)
    ));
    assert!(runtime.startup.is_none());
    assert_eq!(backend.cache_state(), OriginCacheState::Off);
}

#[test]
fn busy_startup_custody_does_not_spend_another_retry() {
    let (backend, mut runtime, mut starter) = respawn_fixture(false, true);
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let mut held = starter
        .startup_custody
        .begin(Box::new(move || {
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            worker_startup::StartOutcome {
                result: Err("held fixture task finished".into()),
                creator_child: None,
            }
        }))
        .unwrap();
    entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    for _ in 0..100 {
        recover_isolated_gpu_worker_if_failed(&mut starter, &backend, &mut runtime, true);
    }
    assert_eq!(starter.attempts.load(Ordering::Acquire), 0);
    assert_eq!(runtime.budget.respawns_since_healthy, 0);
    assert!(runtime.startup.is_none());
    release_tx.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !held.is_quiescent() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(held.poll().is_err());
    recover_and_settle(&mut starter, &backend, &mut runtime, true);
    assert_eq!(starter.attempts.load(Ordering::Acquire), 1);
    assert_eq!(runtime.budget.respawns_since_healthy, 1);
}

#[test]
fn exited_worker_cannot_reset_exhausted_retry_budget() {
    let (backend, mut runtime, mut starter) = respawn_fixture(true, true);
    recover_and_settle(&mut starter, &backend, &mut runtime, true);
    while runtime.budget.allows_respawn() {
        runtime.budget.note_spawned();
    }
    runtime.budget.spawned_at = Some(Instant::now() - WORKER_RESPAWN_HEALTHY_AFTER);
    attach_exited_worker(&mut runtime);
    recover_isolated_gpu_worker_if_failed(&mut starter, &backend, &mut runtime, true);
    assert_eq!(starter.attempts.load(Ordering::Acquire), 1);
    assert!(!runtime.budget.allows_respawn());
    assert!(runtime.permanent_fallback_logged);
    assert_eq!(backend.cache_state(), OriginCacheState::Unavailable);
}

#[test]
fn revoked_supervisor_during_startup_never_reactivates_cache() {
    let (backend, mut runtime, mut starter) = respawn_fixture(true, true);
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    starter.handshake_gate = Some(Arc::new(StartupDelay {
        entered: entered_tx,
        release: Mutex::new(release_rx),
    }));
    let healthy = Arc::new(AtomicBool::new(true));
    let gate = Arc::clone(&healthy);
    let authority: worker_startup::StartupAuthority =
        Arc::new(move || gate.load(Ordering::Acquire));
    super::super::worker_recovery::recover_isolated_gpu_worker_guarded(
        &mut starter,
        &backend,
        &mut runtime,
        true,
        Arc::clone(&authority),
    );
    entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    healthy.store(false, Ordering::Release);
    release_tx.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while runtime.startup.is_some() {
        assert!(Instant::now() < deadline);
        super::super::worker_recovery::recover_isolated_gpu_worker_guarded(
            &mut starter,
            &backend,
            &mut runtime,
            true,
            Arc::clone(&authority),
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_ne!(backend.cache_state(), OriginCacheState::Active);
    assert!(runtime.supervisor.is_none());
}

#[test]
fn worker_set_rejects_handoff_after_supervisor_becomes_unhealthy() {
    let (backend, runtime, mut starter) = respawn_fixture(true, true);
    let mut workers = WorkerSet::new(vec![runtime]);
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    starter.handshake_gate = Some(Arc::new(StartupDelay {
        entered: entered_tx,
        release: Mutex::new(release_rx),
    }));
    let healthy = Arc::new(AtomicBool::new(true));
    let gate = Arc::clone(&healthy);
    let authority: worker_startup::StartupAuthority =
        Arc::new(move || gate.load(Ordering::Acquire));
    workers.recover_with_authority(&mut starter, &backend, true, Arc::clone(&authority));
    entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    healthy.store(false, Ordering::Release);
    release_tx.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while workers.iter_mut().any(|member| member.startup.is_some()) {
        assert!(Instant::now() < deadline);
        workers.recover_with_authority(&mut starter, &backend, true, Arc::clone(&authority));
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_ne!(backend.cache_state(), OriginCacheState::Active);
    assert!(workers.iter_mut().all(|member| member.supervisor.is_none()));
}

#[test]
#[ignore = "requires the installed healthy supervisor and root observation"]
fn live_startup_authority_revalidates_the_pinned_supervisor() {
    let path = std::path::Path::new("/run/ramshared/supervisor-state.json");
    let bytes = crate::manifest_file::read_bounded_manifest_file(path, true).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let daemon = value["daemon_instance_id"].as_str().unwrap();
    let now = crate::control_requests::unix_time_ms().unwrap();
    let mut workers = WorkerSet::default();
    assert!(workers.pressure_recovery_allowed(daemon, now));
    let authority = workers.startup_authority(daemon, None);
    assert!(authority(), "fresh pinned supervisor must admit startup");
    let foreign = workers.startup_authority("foreign-daemon", None);
    assert!(!foreign(), "different daemon identity must be refused");
}

#[test]
fn startup_authority_refuses_missing_or_non_pidfd_supervisor() {
    let mut workers = WorkerSet::default();
    assert!(!workers.startup_authority("test-daemon", None)());
    let fd = std::os::fd::OwnedFd::from(std::fs::File::open("/dev/null").unwrap());
    workers.set_supervisor_fixture(fd);
    assert!(!workers.startup_authority("test-daemon", None)());
    let unrelated = rustix::process::pidfd_open(
        rustix::process::getpid(),
        rustix::process::PidfdFlags::empty(),
    )
    .unwrap();
    workers.set_supervisor_fixture(unrelated);
    assert!(!workers.startup_authority("test-daemon", None)());
}

#[test]
fn worker_skips_spawn_until_catalog_live_and_revives_on_reconnect() {
    use ramshared_wsl2d::host_catalog::SharedHostCatalog;

    let (backend, mut runtime, mut starter) = respawn_fixture(true, true);
    let empty_catalog = SharedHostCatalog::new();
    runtime.params.host_catalog = Some(empty_catalog);
    backend.replace_cache(OriginCache::Disabled(DisabledCache));

    // A configured but empty/expired host catalog must not burn respawn tokens.
    recover_isolated_gpu_worker_if_failed(&mut starter, &backend, &mut runtime, true);
    assert_eq!(starter.attempts.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.budget.respawns_since_healthy, 0);
    assert_eq!(
        runtime.unavailable_reason.as_deref(),
        Some("awaiting fresh authenticated host GPU catalog")
    );

    // Once the respawn burst is exhausted during unbroken authority, a lease loss
    // followed by live restoration revives without waiting out the 300s cooldown.
    runtime.params.host_catalog = None;
    for _ in 0..3 {
        runtime.budget.note_spawn_failed();
    }
    assert!(!runtime.budget.allows_respawn());
    recover_isolated_gpu_worker_if_failed(&mut starter, &backend, &mut runtime, false);
    assert_eq!(starter.attempts.load(Ordering::SeqCst), 0);

    recover_and_settle(&mut starter, &backend, &mut runtime, true);
    assert_eq!(starter.attempts.load(Ordering::SeqCst), 1);
    assert_eq!(backend.cache_state(), OriginCacheState::Active);
    drop(runtime);
}

#[test]
fn native_driver_reload_pauses_spawns_and_revives_cache_without_origin_loss() {
    use crate::native_driver_probe::NativeDriverObservation;

    let (backend, mut runtime, mut starter) = respawn_fixture(false, true);
    *starter.native_driver.lock().unwrap() = Some(NativeDriverObservation {
        ready: false,
        epoch: "nv=going;vk=1".into(),
        detail: Some("awaiting NVIDIA kernel driver readiness".into()),
    });

    // While the Linux driver is reloading, recovery does not burn retry budget and keeps origin I/O working.
    recover_isolated_gpu_worker_if_failed(&mut starter, &backend, &mut runtime, true);
    assert_eq!(starter.attempts.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.budget.respawns_since_healthy, 0);
    assert_eq!(
        runtime.unavailable_reason.as_deref(),
        Some("awaiting NVIDIA kernel driver readiness")
    );
    backend.write_at(0, &[0xA5; 4096]).unwrap();

    // Even if retries had already been exhausted earlier, a driver transition back to ready clears cooldown cleanly.
    for _ in 0..3 {
        runtime.budget.note_spawn_failed();
    }
    assert!(!runtime.budget.allows_respawn());
    starter.succeed.store(true, Ordering::SeqCst);
    *starter.native_driver.lock().unwrap() = Some(NativeDriverObservation {
        ready: true,
        epoch: "nv=live:560.35;vk=1".into(),
        detail: None,
    });
    recover_and_settle(&mut starter, &backend, &mut runtime, true);
    assert_eq!(starter.attempts.load(Ordering::SeqCst), 1);
    assert_eq!(backend.cache_state(), OriginCacheState::Active);

    let mut verify = [0u8; 4096];
    backend.read_at(0, &mut verify).unwrap();
    assert_eq!(verify, [0xA5; 4096]);
    let _ = ProductionNbdRuntimeStarter.native_gpu_driver_observation();
    drop(runtime);
}
