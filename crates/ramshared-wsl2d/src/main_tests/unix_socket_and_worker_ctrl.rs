// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main_tests/unix_socket_and_worker_ctrl.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;

#[test]
// TestName: unix_socket_parent_symlink_is_refused_before_bind
fn unix_socket_parent_symlink_is_refused_before_bind() {
    let dir = std::env::temp_dir().join(format!("ramshared-socket-parent-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir(&dir).unwrap();
    let real = dir.join("real");
    let alias = dir.join("alias");
    std::fs::create_dir(&real).unwrap();
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let refused = prepare_unix_socket_path(&alias.join("daemon.sock")).is_err();
    std::fs::remove_dir_all(&dir).unwrap();

    assert!(refused, "socket parent symlink was accepted");
}

#[test]
// TestName: owned_unix_socket_cleanup_removes_only_the_exact_bound_identity
fn owned_unix_socket_cleanup_removes_only_the_exact_bound_identity() {
    let dir = std::env::temp_dir().join(format!("ramshared-owned-socket-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("daemon.sock");
    let (listener, guard) = bind_owned_unix_listener(&path).unwrap();
    assert!(std::os::unix::net::UnixStream::connect(&path).is_ok());
    drop(listener);
    drop(guard);
    assert!(!path.exists(), "exact owned socket was not cleaned");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn daemon_listener_backlog_is_capped_to_system_limits() {
    let max = system_max_backlog();
    assert!(
        max >= 128,
        "system_max_backlog fallback should be at least 128"
    );

    // The socket is bound and we can apply backlog up to min(4096, max)
    let path = std::env::temp_dir().join(format!("ramshared-backlog-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();

    let apply_result = apply_listen_backlog(&listener);
    assert!(apply_result.is_ok(), "apply_listen_backlog should succeed");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn daemon_args_parse_auto_backend() {
    let args = AppArgs::parse_from(&daemon_argv(&[
        "ramsharedd",
        "--size",
        "1024",
        "--sock",
        "/tmp/test-auto.sock",
        "--backend",
        "auto",
        "--slices",
        "1",
        "--slice-mb",
        "1024",
        "--arbiter-listen",
        "127.0.0.1:9090",
    ]))
    .expect("auto backend must parse");
    assert!(matches!(args.backend, BackendKind::Auto));
}

#[test]
fn daemon_survives_abrupt_gpu_worker_kill() {
    let root = std::env::temp_dir().join(format!("ramshared-worker-kill-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let origin_path = root.join("origin.bin");
    let origin_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&origin_path)
        .unwrap();
    origin_file.set_len(16 * 1024 * 1024).unwrap();
    let origin = FileOrigin::from_file(origin_file);

    let (client_sock, worker_sock) = std::os::unix::net::UnixStream::pair().unwrap();
    let worker_fd: std::os::fd::OwnedFd = worker_sock.into();

    // Spawn a child process holding worker_sock that will be abruptly killed
    let child = std::process::Command::new("sleep")
        .arg("60")
        .stdin(worker_fd)
        .spawn()
        .unwrap();

    let worker_pid = child.id();
    let mut supervisor = IsolatedWorkerSupervisor {
        child: Some(WorkerChildHandle::Process(ChildWorkerProcess::new(child))),
    };

    let client = IpcCacheClient::new(client_sock, Duration::from_millis(50), 4 * 1024 * 1024);

    let backend = AuthoritativeOriginBackend::new_with_admission(
        origin,
        OriginCache::Ipc(Box::new(client)),
        16 * 1024 * 1024,
        BLOCK_SIZE,
        ramshared_block::AllowCacheAdmission,
    )
    .unwrap();

    let write_data = vec![0x33; 4096];
    backend.write_at(0, &write_data).unwrap();

    // Abruptly kill the worker child process (SIGKILL). The arm is
    // exhaustive on purpose: a silently skipped kill leaves the peer alive,
    // the read below then promotes against a live socket, and the
    // `cache_state` assertion reports `Active` for a reason that has nothing
    // to do with the cache. Fail here, at the step that was skipped.
    match supervisor.child.as_mut() {
        Some(WorkerChildHandle::Process(child)) => {
            let _ = WorkerProcessControl::kill(child);
            assert!(
                wait_for_worker_exit(child, Duration::from_secs(2), Duration::from_millis(10))
                    .unwrap(),
                "the SIGKILLed worker must be observed to exit before the cache is judged"
            );
        }
        #[cfg(test)]
        _ => panic!("the worker-kill test requires a process worker"),
    }

    // Read at offset 0: transparently falls back to origin storage with 0 error!
    let mut read_buf = vec![0u8; 4096];
    let read_result = backend.read_at(0, &mut read_buf);
    assert!(
        read_result.is_ok(),
        "daemon must serve read from origin after worker kill"
    );
    assert_eq!(
        read_buf, write_data,
        "data from origin must match written data"
    );
    assert_eq!(backend.origin_state(), DurableOriginState::Ready);
    assert_eq!(backend.cache_state(), OriginCacheState::Unavailable);

    let (_, mut runtime, mut starter) = respawn_fixture(false, true);
    runtime.supervisor = Some(supervisor);
    recover_isolated_gpu_worker_if_failed(&mut starter, &backend, &mut runtime, false);
    let reason = runtime.unavailable_reason.as_deref().unwrap();
    assert!(reason.contains(&format!("pid {worker_pid}")));
    assert!(reason.contains("SIGKILL"));
    assert!(runtime.supervisor.is_none());

    // Subsequent writes and reads also succeed against origin
    let next_data = vec![0x77; 4096];
    assert!(backend.write_at(4096, &next_data).is_ok());
    let mut second_buf = vec![0u8; 4096];
    assert!(backend.read_at(4096, &mut second_buf).is_ok());
    assert_eq!(second_buf, next_data);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn recovered_worker_preserves_original_adapter_request() {
    let (backend, mut runtime, mut starter) = respawn_fixture(false, true);
    let adapter = GpuAdapterIdentity {
        node_mask: None,
        backend: "cuda".into(),
        key: "a".repeat(32),
        luid: None,
    };
    let (socket, peer) = std::os::unix::net::UnixStream::pair().unwrap();
    let mut client = IpcCacheClient::new(socket, Duration::from_millis(50), 4 * 1024 * 1024);
    client.expect_adapter(adapter.clone()).unwrap();
    drop(peer);
    assert!(client.refresh_cached_bytes().is_err());
    assert!(
        client.gpu_budget_telemetry().is_none(),
        "live samples are already gone"
    );
    backend.replace_cache(OriginCache::Ipc(Box::new(client)));
    assert!(runtime.params.expected_adapter.is_none());
    recover_and_settle(&mut starter, &backend, &mut runtime, true);
    assert_eq!(runtime.params.expected_adapter.as_ref(), Some(&adapter));
    let requests = starter.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].expected_adapter.as_ref(), Some(&adapter));
    assert_eq!(requests[0].target_bytes, 4 * 1024 * 1024);
    assert_eq!(backend.cache_state(), OriginCacheState::Unavailable);
    assert_eq!(
        runtime.params.expected_adapter.as_ref(),
        Some(&adapter),
        "failed replacement cannot erase the original pin"
    );
}

#[test]
fn the_gpu_cache_unavailability_is_declared_with_its_reason() {
    // SPEC DT-3: a worker that exited while the cache is not serving leaves a
    // cause behind, so the status document never carries a bare "the cache is
    // off" with no explanation.
    let reason = gpu_cache_unavailable_reason(OriginCacheState::Off, true, None)
        .expect("an exited worker with a non-serving cache has a cause");
    assert!(
        reason.contains("worker exited") && reason.contains("OFF"),
        "the reason names both the cause and the state it found: {reason}"
    );
    // A cache that is still serving has no cause to publish.
    assert_eq!(
        gpu_cache_unavailable_reason(OriginCacheState::Active, true, None),
        None
    );
    // A live worker with a released cache is a deliberate release, not a
    // death: the release path publishes its own reason.
    assert_eq!(
        gpu_cache_unavailable_reason(OriginCacheState::Off, false, None),
        None
    );
}

#[test]
fn dead_gpu_worker_is_replaced_and_origin_keeps_serving() {
    let (backend, mut runtime, mut starter) = respawn_fixture(true, true);
    assert_eq!(backend.cache_state(), OriginCacheState::Unavailable);

    recover_and_settle(&mut starter, &backend, &mut runtime, true);

    assert_eq!(
        backend.cache_state(),
        OriginCacheState::Active,
        "a successful respawn must restore the cache to Active"
    );
    assert!(
        runtime.supervisor.is_some(),
        "the replacement worker must be supervised"
    );
    assert_eq!(starter.attempts.load(Ordering::SeqCst), 1);
    assert_eq!(
        runtime.budget.respawns_since_healthy, 1,
        "a successful spawn spends one unit of the respawn budget"
    );
    assert_eq!(backend.origin_state(), DurableOriginState::Ready);
}

#[test]
fn previous_worker_is_stopped_before_the_replacement_is_spawned() {
    let (backend, mut runtime, mut starter) = respawn_fixture(true, false);
    // The mock starter asserts, at spawn time, that this exact flag is
    // already set — so it is wired as the predecessor's own stop flag and
    // the assertion observes the real teardown, not a stand-in.
    let predecessor_stop = std::sync::Arc::clone(&starter.predecessor_was_stopped);
    let peer_stop = std::sync::Arc::clone(&predecessor_stop);
    let handle = std::thread::spawn(move || {
        while !peer_stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    runtime.supervisor = Some(IsolatedWorkerSupervisor {
        child: Some(WorkerChildHandle::Thread {
            stop: predecessor_stop,
            handle: Some(handle),
        }),
    });

    recover_and_settle(&mut starter, &backend, &mut runtime, true);

    assert_eq!(starter.attempts.load(Ordering::SeqCst), 1);
    assert!(
        runtime.supervisor.is_some(),
        "the replacement must own the supervisor slot"
    );
    assert!(
        starter.predecessor_was_stopped.load(Ordering::SeqCst),
        "the predecessor must be stopped and joined before the replacement runs"
    );
}

#[test]
fn respawn_budget_exhausts_and_origin_fallback_enters_cooldown() {
    let (backend, mut runtime, mut starter) = respawn_fixture(false, true);

    for attempt in 1..=WORKER_RESPAWN_MAX_CONSECUTIVE {
        recover_and_settle(&mut starter, &backend, &mut runtime, true);
        assert_eq!(starter.attempts.load(Ordering::SeqCst), attempt);
        assert_eq!(backend.cache_state(), OriginCacheState::Unavailable);
        assert!(runtime.supervisor.is_none());
    }

    // A worker can complete startup and then stop serving without an exit
    // observation. Cooldown still explains the unavailable cache.
    runtime.unavailable_reason = None;
    recover_and_settle(&mut starter, &backend, &mut runtime, true);
    assert_eq!(
        starter.attempts.load(Ordering::SeqCst),
        WORKER_RESPAWN_MAX_CONSECUTIVE,
        "a crash-loop must stop after the consecutive-respawn cap"
    );
    assert_eq!(backend.cache_state(), OriginCacheState::Unavailable);
    assert!(
        !runtime.budget.allows_respawn(),
        "the retry budget must remain closed during cooldown"
    );
    assert_eq!(
        runtime.unavailable_reason.as_deref(),
        Some("GPU cache is in retry cooldown after 3 worker respawns"),
    );
    assert_eq!(backend.origin_state(), DurableOriginState::Ready);
}

#[test]
fn lease_loss_release_is_rebuilt_only_after_the_lease_returns() {
    let (backend, mut runtime, mut starter) = respawn_fixture(true, true);
    recover_and_settle(&mut starter, &backend, &mut runtime, true);
    assert_eq!(backend.cache_state(), OriginCacheState::Active);
    assert_eq!(starter.attempts.load(Ordering::SeqCst), 1);

    backend.release_cache().unwrap();
    runtime.supervisor = None;
    runtime.lease_lost_release = true;
    assert_eq!(backend.cache_state(), OriginCacheState::Off);

    // The lease is still down: do not rebuild yet.
    recover_and_settle(&mut starter, &backend, &mut runtime, false);
    assert_eq!(
        starter.attempts.load(Ordering::SeqCst),
        1,
        "a lease-lost release must wait for a live lease"
    );
    assert_eq!(backend.cache_state(), OriginCacheState::Off);

    // The lease is back: rebuild and re-admit the cache.
    recover_and_settle(&mut starter, &backend, &mut runtime, true);
    assert_eq!(starter.attempts.load(Ordering::SeqCst), 2);
    assert_eq!(backend.cache_state(), OriginCacheState::Active);
    assert!(
        !runtime.lease_lost_release,
        "a successful respawn clears the lease-loss marker"
    );
}

#[test]
fn deliberate_cache_release_is_never_treated_as_worker_death() {
    let (backend, mut runtime, mut starter) = respawn_fixture(true, true);
    recover_and_settle(&mut starter, &backend, &mut runtime, true);
    assert_eq!(backend.cache_state(), OriginCacheState::Active);
    assert_eq!(starter.attempts.load(Ordering::SeqCst), 1);

    // Control-pressure `release_cache` is deliberate: state becomes Off.
    backend.release_cache().unwrap();
    assert_eq!(backend.cache_state(), OriginCacheState::Off);

    recover_and_settle(&mut starter, &backend, &mut runtime, true);
    assert_eq!(
        starter.attempts.load(Ordering::SeqCst),
        1,
        "an intentional Off must not be recovered"
    );
    assert_eq!(backend.cache_state(), OriginCacheState::Off);
}

#[test]
fn released_cache_reaps_its_exited_worker_without_respawning() {
    let (backend, mut runtime, mut starter) = respawn_fixture(true, true);
    recover_and_settle(&mut starter, &backend, &mut runtime, true);
    assert_eq!(backend.cache_state(), OriginCacheState::Active);
    assert_eq!(starter.attempts.load(Ordering::SeqCst), 1);

    // Control pressure released the cache, then the worker exited on the
    // disable acknowledgement. That child is a zombie until it is waited.
    backend.release_cache().unwrap();
    assert_eq!(backend.cache_state(), OriginCacheState::Off);
    attach_exited_worker(&mut runtime);
    assert!(gpu_worker_has_exited(&mut runtime));

    recover_and_settle(&mut starter, &backend, &mut runtime, true);

    assert_eq!(
        starter.attempts.load(Ordering::SeqCst),
        1,
        "a released cache must not spawn a replacement worker"
    );
    assert!(
        runtime.supervisor.is_none(),
        "the exited worker must be reaped, not left as a zombie under a live handle"
    );
    assert_eq!(backend.cache_state(), OriginCacheState::Off);
}

#[test]
fn exited_worker_under_a_serving_cache_is_replaced() {
    let (backend, mut runtime, mut starter) = respawn_fixture(true, true);
    recover_and_settle(&mut starter, &backend, &mut runtime, true);
    assert_eq!(backend.cache_state(), OriginCacheState::Active);
    assert_eq!(starter.attempts.load(Ordering::SeqCst), 1);

    // The mirror still claims Active but the worker process is gone —
    // the observable case the status mapping cannot see on its own.
    attach_exited_worker(&mut runtime);
    assert!(gpu_worker_has_exited(&mut runtime));
    assert_eq!(backend.cache_state(), OriginCacheState::Active);

    recover_and_settle(&mut starter, &backend, &mut runtime, true);

    assert_eq!(
        starter.attempts.load(Ordering::SeqCst),
        2,
        "a serving cache with a dead worker must be rebuilt, not believed"
    );
    assert!(
        runtime.supervisor.is_some(),
        "the replacement must be supervised"
    );
    assert_eq!(backend.cache_state(), OriginCacheState::Active);
}

#[test]
fn catalog_recovery_resets_budget_only_for_a_new_generation() {
    let mut budget = WorkerRespawnBudget::new();
    budget.note_catalog_generation(1);
    for _ in 0..WORKER_RESPAWN_MAX_CONSECUTIVE {
        budget.note_spawn_failed();
    }
    assert!(!budget.allows_respawn());
    budget.note_catalog_generation(1);
    assert!(!budget.allows_respawn(), "heartbeat must not renew retries");
    budget.note_catalog_generation(0);
    assert!(
        !budget.allows_respawn(),
        "stale generation must not renew retries"
    );
    budget.note_catalog_generation(2);
    assert!(budget.allows_respawn());
    assert_eq!(budget.respawns_since_healthy, 0);
    assert!(budget.spawned_at.is_none());
    assert_eq!(budget.last_catalog_generation, 2);
}

#[test]
fn healthy_worker_resets_the_respawn_budget() {
    let mut budget = WorkerRespawnBudget::new();
    for _ in 0..WORKER_RESPAWN_MAX_CONSECUTIVE {
        budget.note_spawned();
    }
    assert!(!budget.allows_respawn());

    // Still inside the healthy window: the budget stays closed.
    let soon = Instant::now();
    budget.mark_healthy_if_settled_at(OriginCacheState::Active, soon);
    assert!(
        !budget.allows_respawn(),
        "a worker that has not settled must not earn a new burst"
    );

    let settled = Instant::now() + WORKER_RESPAWN_HEALTHY_AFTER + Duration::from_secs(1);
    budget.mark_healthy_if_settled_at(OriginCacheState::Active, settled);
    assert!(
        budget.allows_respawn(),
        "a worker that survived the healthy window earns the next burst"
    );
    assert_eq!(budget.respawns_since_healthy, 0);

    // A dead worker that has not settled must not reset the counter.
    let mut stalled = WorkerRespawnBudget::new();
    stalled.note_spawned();
    stalled.mark_healthy_if_settled_at(OriginCacheState::Unavailable, settled);
    assert_eq!(
        stalled.respawns_since_healthy, 1,
        "an Unavailable cache is never healthy, so it cannot clear the budget"
    );
}

struct UnresponsiveWorkerProcess {
    kill_requested: std::sync::Arc<std::sync::atomic::AtomicBool>,
    reaper_started: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl WorkerProcessControl for UnresponsiveWorkerProcess {
    fn id(&self) -> u32 {
        42
    }

    fn try_wait(&mut self) -> std::io::Result<bool> {
        Ok(false)
    }

    fn kill(&mut self) -> std::io::Result<()> {
        self.kill_requested.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn reap_in_background(&mut self) -> std::io::Result<()> {
        self.reaper_started.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[test]
fn isolated_worker_shutdown_stays_bounded_when_kill_is_not_observed() {
    let kill_requested = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reaper_started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut process = UnresponsiveWorkerProcess {
        kill_requested: std::sync::Arc::clone(&kill_requested),
        reaper_started: std::sync::Arc::clone(&reaper_started),
    };
    let started = Instant::now();
    let outcome = stop_worker_process(
        &mut process,
        Duration::from_millis(10),
        Duration::from_millis(10),
        Duration::from_millis(1),
    );

    assert_eq!(outcome, WorkerStopOutcome::ReaperStarted);
    assert!(kill_requested.load(Ordering::SeqCst));
    assert!(reaper_started.load(Ordering::SeqCst));
    assert!(
        started.elapsed() < Duration::from_millis(250),
        "worker shutdown must not wait indefinitely after kill"
    );
}

/// A worker whose exit observations fail transiently before succeeding.
struct TransientlyUnobservableWorkerProcess {
    transient_failures_left: std::sync::Arc<std::sync::atomic::AtomicU32>,
    confirmed_exit: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl WorkerProcessControl for TransientlyUnobservableWorkerProcess {
    fn id(&self) -> u32 {
        7
    }

    fn try_wait(&mut self) -> std::io::Result<bool> {
        let left = self.transient_failures_left.load(Ordering::SeqCst);
        if left > 0 {
            self.transient_failures_left
                .store(left - 1, Ordering::SeqCst);
            return Err(std::io::Error::from(std::io::ErrorKind::Interrupted));
        }
        self.confirmed_exit.store(true, Ordering::SeqCst);
        Ok(true)
    }

    fn kill(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    fn reap_in_background(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A worker whose exit observations never succeed at all.
struct PermanentlyUnobservableWorkerProcess {
    observations: std::sync::Arc<std::sync::atomic::AtomicU32>,
}

impl WorkerProcessControl for PermanentlyUnobservableWorkerProcess {
    fn id(&self) -> u32 {
        8
    }

    fn try_wait(&mut self) -> std::io::Result<bool> {
        self.observations.fetch_add(1, Ordering::SeqCst);
        Err(std::io::Error::other("exit observation is unavailable"))
    }

    fn kill(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    fn reap_in_background(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A failed observation is not an observation of "still running".
///
/// `try_wait` is one `waitpid(WNOHANG)`. When that call is interrupted it
/// hands `EINTR` back, and the old fold of every error into `false`
/// published "did not exit" for a child that may already have. The retry
/// keeps the strong claim — only a confirmed `Some` is an exit — and keeps
/// the deadline the budget.
#[test]
fn wait_for_worker_exit_retries_a_transient_observation_error() {
    let transient_failures_left = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(3));
    let confirmed_exit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut process = TransientlyUnobservableWorkerProcess {
        transient_failures_left: std::sync::Arc::clone(&transient_failures_left),
        confirmed_exit: std::sync::Arc::clone(&confirmed_exit),
    };
    assert!(
        wait_for_worker_exit(
            &mut process,
            Duration::from_secs(2),
            Duration::from_millis(1)
        )
        .unwrap(),
        "three interrupted observations must not be published as a non-exit"
    );
    assert!(confirmed_exit.load(Ordering::SeqCst));
    assert_eq!(transient_failures_left.load(Ordering::SeqCst), 0);
}

/// A persistent observation failure still refuses to claim an exit.
///
/// The retry is for a classified transient signature only. An observation
/// path that is broken stays broken, and the honest answer is still
/// "not observed to exit" — never a fabricated `true`.
#[test]
fn wait_for_worker_exit_refuses_to_invent_an_exit_when_observation_is_broken() {
    let observations = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let mut process = PermanentlyUnobservableWorkerProcess {
        observations: std::sync::Arc::clone(&observations),
    };
    let started = Instant::now();
    assert!(
        wait_for_worker_exit(
            &mut process,
            Duration::from_millis(20),
            Duration::from_millis(1)
        )
        .is_err(),
        "a broken observation path must not report an exit it never saw"
    );
    assert_eq!(
        observations.load(Ordering::SeqCst),
        1,
        "a deterministic failure is fail-fast: one look, one refusal, no deadline burn"
    );
    assert!(
        started.elapsed() < Duration::from_millis(250),
        "a persistent observation error must not wait out the whole budget"
    );
}

/// The deadline is still the budget when the child simply never exits.
///
/// Retrying a transient error must not widen the bound: a child that is
/// genuinely still running is still reported `false` at the deadline, not
/// held open.
#[test]
fn wait_for_worker_exit_still_reports_false_at_the_deadline_when_the_child_never_exits() {
    let kill_requested = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reaper_started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut process = UnresponsiveWorkerProcess {
        kill_requested: std::sync::Arc::clone(&kill_requested),
        reaper_started: std::sync::Arc::clone(&reaper_started),
    };
    let started = Instant::now();
    assert!(
        !wait_for_worker_exit(
            &mut process,
            Duration::from_millis(20),
            Duration::from_millis(1)
        )
        .unwrap()
    );
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(20),
        "the deadline is the bound; a live child is waited out, not returned early"
    );
    assert!(
        elapsed < Duration::from_millis(250),
        "and it is still a bounded wait"
    );
}

#[test]
fn exhausted_worker_retry_budget_recovers_after_cooldown_without_restart() {
    let (backend, mut runtime, mut starter) = respawn_fixture(false, true);
    for _ in 0..WORKER_RESPAWN_MAX_CONSECUTIVE {
        recover_and_settle(&mut starter, &backend, &mut runtime, true);
    }
    recover_and_settle(&mut starter, &backend, &mut runtime, true);
    assert_eq!(
        starter.attempts.load(Ordering::SeqCst),
        WORKER_RESPAWN_MAX_CONSECUTIVE
    );
    assert_eq!(backend.cache_state(), OriginCacheState::Unavailable);
    assert!(runtime.budget.cooldown_started_at.is_some());
    backend.write_at(0, &[0x52; 4096]).unwrap();
    let mut data = [0u8; 4096];
    backend.read_at(0, &mut data).unwrap();
    assert_eq!(
        data, [0x52; 4096],
        "origin stays available throughout the cooldown"
    );
    starter.succeed.store(true, Ordering::SeqCst);
    runtime.budget.cooldown_started_at =
        Some(Instant::now() - crate::worker_recovery::WORKER_RESPAWN_COOLDOWN);
    recover_and_settle(&mut starter, &backend, &mut runtime, false);
    assert_eq!(
        starter.attempts.load(Ordering::SeqCst),
        WORKER_RESPAWN_MAX_CONSECUTIVE,
        "a missing lease must still refuse recovery"
    );
    recover_and_settle(&mut starter, &backend, &mut runtime, true);
    assert_eq!(
        starter.attempts.load(Ordering::SeqCst),
        WORKER_RESPAWN_MAX_CONSECUTIVE + 1
    );
    assert_eq!(backend.cache_state(), OriginCacheState::Active);
    assert!(runtime.supervisor.is_some());
}

#[test]
fn repeated_retry_bursts_preserve_origin_and_eventually_recover() {
    let (backend, mut runtime, mut starter) = respawn_fixture(false, true);
    for burst in 0..2u32 {
        for _ in 0..WORKER_RESPAWN_MAX_CONSECUTIVE {
            recover_and_settle(&mut starter, &backend, &mut runtime, true);
        }
        recover_and_settle(&mut starter, &backend, &mut runtime, true);
        let attempts = (burst + 1) * WORKER_RESPAWN_MAX_CONSECUTIVE;
        assert_eq!(starter.attempts.load(Ordering::SeqCst), attempts);
        assert_eq!(backend.cache_state(), OriginCacheState::Unavailable);
        for _ in 0..3 {
            recover_and_settle(&mut starter, &backend, &mut runtime, true);
            assert_eq!(starter.attempts.load(Ordering::SeqCst), attempts);
        }
        let written = [0x31 + burst as u8; 4096];
        backend.write_at(0, &written).unwrap();
        let mut read = [0u8; 4096];
        backend.read_at(0, &mut read).unwrap();
        assert_eq!(read, written);
        assert_eq!(backend.origin_state(), DurableOriginState::Ready);
        runtime.budget.cooldown_started_at =
            Some(Instant::now() - crate::worker_recovery::WORKER_RESPAWN_COOLDOWN);
    }
    starter.succeed.store(true, Ordering::SeqCst);
    recover_and_settle(&mut starter, &backend, &mut runtime, true);
    assert_eq!(
        starter.attempts.load(Ordering::SeqCst),
        2 * WORKER_RESPAWN_MAX_CONSECUTIVE + 1
    );
    assert_eq!(backend.cache_state(), OriginCacheState::Active);
    let mut read = [0u8; 4096];
    backend.read_at(0, &mut read).unwrap();
    assert_eq!(read, [0x32; 4096]);
}

#[test]
fn exhausted_worker_retry_cooldown_has_strict_time_boundaries() {
    let mut budget = WorkerRespawnBudget::new();
    for _ in 0..WORKER_RESPAWN_MAX_CONSECUTIVE {
        budget.note_spawn_failed();
    }
    let start = Instant::now();
    assert!(!budget.refresh_cooldown_at(start));
    assert!(!budget.refresh_cooldown_at(start - Duration::from_millis(1)));
    assert!(!budget.refresh_cooldown_at(
        start + crate::worker_recovery::WORKER_RESPAWN_COOLDOWN - Duration::from_millis(1)
    ));
    assert!(!budget.allows_respawn());
    assert!(budget.refresh_cooldown_at(start + crate::worker_recovery::WORKER_RESPAWN_COOLDOWN));
    assert!(budget.allows_respawn());
    assert!(budget.cooldown_started_at.is_none());
}
