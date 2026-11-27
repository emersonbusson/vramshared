// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/nbd_runtime.rs
//! NBD daemon startup composition and multi-connection serve loop.

pub(super) mod demote;
mod recovery_cadence;
pub(super) mod residency;
mod sparse;
pub(super) mod starter;

use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::time::Instant;

use ramshared_block::protocol::{
    NBD_FLAG_CAN_MULTI_CONN, NBD_FLAG_HAS_FLAGS, NBD_FLAG_SEND_FLUSH, NBD_FLAG_SEND_FUA,
};
use ramshared_block::{BlockBackend, Command, Request, ServeOutcome, serve};
use ramshared_vram::VramProvider;
use ramshared_wsl2d::{CHAN_CAP, Job, Reply, WMsg};

use crate::cache_admission::ControlPlaneCacheAdmission;
use crate::cache_status::{CacheStatusPublisher, PublicationContext};
use crate::memory_lock::relock_resident;
use crate::nbd_origin;
use crate::origin_runtime::{self, OpenedOrigin, OriginRuntime};
use crate::serve_pool::{ServePool, SubmitError, refusal_reply, spawn_serve_pool};
use crate::shutdown::ShutdownPermit;
use crate::shutdown_signals::SHUTDOWN;
use crate::unix_listener::bind_owned_unix_listener;
use crate::worker_set::release_cache_after_lease_loss;
use crate::{daemon_instance_id, require_origin_daemon_identity, zero_window};

use self::demote::NbdServeState;
use self::residency::resolve_reserve_floor_policy;
use self::starter::{critical_cache_reclaim_pending, probe_origin_rate_limited};

#[cfg(test)]
pub(crate) use self::demote::{RecoveryActivation, RecoveryActivationPoll};
#[cfg(test)]
pub(crate) use self::residency::{
    ResidencyCheckState, choose_residency_reason, enforce_reserve_policy_binding,
    global_free_at_or_above_floor, observe_global_free_floor, residency_check,
    should_log_probe_sample, sparse_residency_config, sparse_residency_requests_swapoff,
};
#[cfg(test)]
pub(crate) use self::starter::{
    NbdBudgetGate, NbdBudgetProvider, NbdBudgetSnapshot, ProductionNbdRuntimeAdapters,
    origin_cache_target_bytes_from_manifest,
};
pub(crate) use self::starter::{NbdRuntimeStarter, ProductionNbdRuntimeStarter};

trait NbdServeBackend {
    fn publication_due(&self) -> bool {
        false
    }

    fn zero_export(&mut self, base: u64, len: u64) -> bool;

    fn submit_job(&mut self, job: Job) -> Option<Job> {
        Some(job)
    }

    fn serve_inline(&mut self, req: &Request, payload: &[u8]) -> ServeOutcome;

    fn on_inline_served(
        &mut self,
        _req: &Request,
        _reply_errno: u32,
        _lat_us: u64,
        _state: &mut NbdServeState,
        _nbd_dev: &str,
        _starter: &mut dyn NbdRuntimeStarter,
    ) {
    }

    fn after_message(
        &mut self,
        _nbd_dev: &str,
        _state: &mut NbdServeState,
        _starter: &mut dyn NbdRuntimeStarter,
    ) {
    }

    fn on_tick(
        &mut self,
        nbd_dev: &str,
        state: &mut NbdServeState,
        starter: &mut dyn NbdRuntimeStarter,
    );

    fn probe_gpu_lost(&self) -> Option<String> {
        None
    }

    fn poll_budget_and_recovery(
        &mut self,
        _nbd_dev: &str,
        _state: &mut NbdServeState,
        _starter: &mut dyn NbdRuntimeStarter,
    ) {
    }
}

struct OriginServeBackend<'a> {
    runtime: &'a mut OriginRuntime,
    serve_pool: &'a ServePool,
    configuration_generation: Option<u64>,
    daemon_instance_id: &'a str,
    publisher: &'a mut CacheStatusPublisher,
    last_origin_probe: Option<Instant>,
    recovery_cadence: recovery_cadence::RecoveryCadence,
}

impl OriginServeBackend<'_> {
    fn publish_status(&mut self, starter: &mut dyn NbdRuntimeStarter, threads: usize, served: u64) {
        let cache = &self.runtime.backend;
        let admission = self.runtime.admission.as_ref();
        release_cache_after_lease_loss(cache, admission, self.runtime.workers.iter_mut());
        let admission_reason = match admission {
            Some(admission) => ramshared_block::CacheAdmission::admit_cache(admission).err(),
            None => cache.admission_refusal_reason(),
        };
        let critical_reclaim = critical_cache_reclaim_pending(self.daemon_instance_id);
        probe_origin_rate_limited(cache, &mut self.last_origin_probe);
        let pressure_revoked = self.publisher.publish(
            starter,
            cache,
            PublicationContext {
                configuration_generation: self.configuration_generation,
                daemon_instance_id: self.daemon_instance_id,
                serve_threads: threads,
                serve_requests_total: served,
                worker: self
                    .runtime
                    .workers
                    .observation(critical_reclaim, admission_reason.as_deref()),
                control_admission: admission,
                pooled_session: self.runtime.pooled_session.as_deref(),
            },
        );
        self.runtime
            .workers
            .record_pressure_release(pressure_revoked);
    }
}

impl NbdServeBackend for OriginServeBackend<'_> {
    fn publication_due(&self) -> bool {
        self.publisher.publication_due()
    }

    fn zero_export(&mut self, base: u64, len: u64) -> bool {
        let mut backend = &*self.runtime.backend;
        zero_window(&mut backend, base, len).is_ok()
    }

    fn submit_job(&mut self, job: Job) -> Option<Job> {
        match self.serve_pool.submit(job) {
            Ok(()) => None,
            Err(SubmitError::Full(job)) => Some(job),
            Err(SubmitError::Degraded(job)) => {
                // DT-5: a dead worker is refused loudly, never a silent shrink.
                eprintln!(
                    "[ramsharedd] serve pool degraded: refusing job handle={}",
                    job.req.handle
                );
                let _ = job.reply.send(refusal_reply(&job.req, job.credit));
                None
            }
        }
    }

    fn serve_inline(&mut self, req: &Request, payload: &[u8]) -> ServeOutcome {
        let mut backend = &*self.runtime.backend;
        serve(req, payload, &mut backend)
    }

    fn on_inline_served(
        &mut self,
        _req: &Request,
        _reply_errno: u32,
        _lat_us: u64,
        _state: &mut NbdServeState,
        _nbd_dev: &str,
        _starter: &mut dyn NbdRuntimeStarter,
    ) {
        // Queue-full backpressure: this job was served inline, so the
        // published total stays accurate.
        self.serve_pool.record_served();
    }

    fn on_tick(
        &mut self,
        _nbd_dev: &str,
        _state: &mut NbdServeState,
        starter: &mut dyn NbdRuntimeStarter,
    ) {
        if self.recovery_cadence.due_at(Instant::now()) {
            let critical_reclaim = critical_cache_reclaim_pending(self.daemon_instance_id);
            let lease_live = self
                .runtime
                .admission
                .as_ref()
                .is_none_or(ControlPlaneCacheAdmission::allows_worker_recovery);
            let pressure_allowed = !critical_reclaim
                && crate::unix_time_ms().is_some_and(|now| {
                    self.runtime
                        .workers
                        .pressure_recovery_allowed(self.daemon_instance_id, now)
                });
            #[cfg(test)]
            let pressure_allowed = !critical_reclaim
                && starter
                    .test_recovery_admission()
                    .unwrap_or(pressure_allowed);
            let authority = self
                .runtime
                .workers
                .startup_authority(self.daemon_instance_id, self.runtime.admission.clone());
            #[cfg(test)]
            let authority = match starter.test_recovery_admission() {
                Some(allowed) => {
                    std::sync::Arc::new(move || allowed) as crate::worker_startup::StartupAuthority
                }
                None => authority,
            };
            self.runtime.workers.recover_with_authority(
                starter,
                &self.runtime.backend,
                lease_live && pressure_allowed,
                authority,
            );
            self.runtime
                .workers
                .finish_pressure_recovery_if_active(&self.runtime.backend);
        }
        self.publish_status(starter, self.serve_pool.threads(), self.serve_pool.served());
    }
}

pub(super) fn start_nbd_acceptor(
    listener: UnixListener,
    device_size: u64,
    jobs_tx: SyncSender<WMsg>,
    starter: &mut dyn NbdRuntimeStarter,
) -> Result<(), Box<dyn std::error::Error>> {
    let tx_flags =
        NBD_FLAG_HAS_FLAGS | NBD_FLAG_SEND_FLUSH | NBD_FLAG_SEND_FUA | NBD_FLAG_CAN_MULTI_CONN;
    let exports = Arc::new(vec![ramshared_block::handshake::Export::production(
        device_size,
    )]);
    starter.start_acceptor(listener, exports, tx_flags, jobs_tx)?;
    eprintln!("[ramsharedd] transmitting (multi-connection NBD)");
    Ok(())
}

pub(super) fn notify_nbd_listening(
    sock: &str,
    nbd_dev: &str,
    notify_socket: Option<ramshared_wsl2d::systemd_notify::NotifySocket>,
) {
    if let Some(socket) = notify_socket.filter(|_| !SHUTDOWN.load(Ordering::SeqCst)) {
        let socket = Arc::new(socket);
        if let Err(error) = socket.send_ready() {
            eprintln!("[ramsharedd] systemd READY failed: {error}");
        } else if let Some(interval) = ramshared_wsl2d::systemd_notify::watchdog_interval() {
            let _ = ramshared_wsl2d::systemd_notify::spawn_watchdog(socket, interval);
        }
    }
    eprintln!("[ramsharedd] listening on {sock}");
    eprintln!(
        "[ramsharedd] connect: sudo nbd-client -N {} -C <N> -unix {sock} {nbd_dev}",
        ramshared_block::handshake::PRODUCTION_EXPORT_NAME
    );
}

pub(super) fn start_nbd_acceptor_and_notify(
    listener: UnixListener,
    device_size: u64,
    sock: &str,
    nbd_dev: &str,
    notify_socket: Option<ramshared_wsl2d::systemd_notify::NotifySocket>,
    jobs_tx: SyncSender<WMsg>,
    starter: &mut dyn NbdRuntimeStarter,
) -> Result<(), Box<dyn std::error::Error>> {
    start_nbd_acceptor(listener, device_size, jobs_tx, starter)?;
    notify_nbd_listening(sock, nbd_dev, notify_socket);
    Ok(())
}

fn drive_nbd_loop(
    nbd_dev: &str,
    jobs_rx: &Receiver<WMsg>,
    backend: &mut dyn NbdServeBackend,
    starter: &mut dyn NbdRuntimeStarter,
) -> ShutdownPermit {
    let mut state = NbdServeState::new(starter);

    'serve: loop {
        if SHUTDOWN.load(Ordering::SeqCst) && !state.shutdown_requested {
            state.request_shutdown();
        }
        state.poll_recovery_activation(nbd_dev, starter);
        state.poll_demote_rx(nbd_dev, starter, false);
        if let Some(permit) = state.maybe_claim_shutdown(nbd_dev, starter) {
            return permit;
        }

        let recv_tick = state.recv_tick();
        let msg = match jobs_rx.recv_timeout(recv_tick) {
            Ok(m) => Some(m),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => {
                state.request_shutdown();
                // A disconnected producer cannot deliver further NBD jobs. Keep
                // the backend alive for an owned recovery child, but yield so a
                // closed channel cannot turn terminal observation into a spin.
                std::thread::sleep(recv_tick);
                None
            }
        };

        if msg.is_none() && state.recovery_activation.is_pending() && !backend.publication_due() {
            continue;
        }

        if let Some(msg) = msg {
            let job = match msg {
                WMsg::Opened => {
                    state.live.on_open();
                    None
                }
                WMsg::Closed => {
                    if state.handle_closed(nbd_dev, starter) {
                        continue;
                    }
                    None
                }
                WMsg::Shutdown => {
                    state.request_shutdown();
                    continue 'serve;
                }
                WMsg::Job(job) => backend.submit_job(job),
                WMsg::ZeroExport { base, len, done } => {
                    let ok = backend.zero_export(base, len);
                    let _ = done.send(ok);
                    None
                }
            };

            if let Some(job) = job {
                let t0 = Instant::now();
                let out = backend.serve_inline(&job.req, &job.payload);
                let lat_us = starter.elapsed_us(t0);
                let _ = job.reply.send(Reply {
                    reply: out.reply,
                    data: out.read_data,
                    credit: job.credit,
                    disconnect: out.disconnect,
                    silent_disconnect: job.req.cmd == Command::Disc,
                });
                let reply_errno =
                    u32::from_be_bytes([out.reply[4], out.reply[5], out.reply[6], out.reply[7]]);
                backend.on_inline_served(
                    &job.req,
                    reply_errno,
                    lat_us,
                    &mut state,
                    nbd_dev,
                    starter,
                );
            }

            backend.after_message(nbd_dev, &mut state, starter);
        }

        state.poll_demote_rx(nbd_dev, starter, true);
        backend.on_tick(nbd_dev, &mut state, starter);

        // verified test case: residency re-lock runs on every heartbeat tick in every
        // backend mode — including `Origin`, the production path — so pages
        // allocated after `lock_memory` cannot land on the swap this daemon serves.
        if state.heartbeat_due() {
            if !relock_resident() {
                eprintln!(
                    "[ramsharedd] re-lock of resident pages failed; pages allocated since the lock may still be swappable to {nbd_dev}"
                );
            }
            if let Some(error) = backend.probe_gpu_lost()
                && state.demote_spawn_authorized()
            {
                eprintln!(
                    "[ramsharedd] GPU probe failed during idle tick: {error} -> swapoff {nbd_dev}"
                );
                state.spawn_swapoff("GpuLost", nbd_dev, starter);
            }
        }

        backend.poll_budget_and_recovery(nbd_dev, &mut state, starter);
    }
}

struct OriginLaunchConfig {
    size: u64,
    sock: String,
    force: bool,
    nbd_dev: String,
    serve_threads: usize,
}

fn run_origin_nbd(
    origin: OpenedOrigin,
    cfg: OriginLaunchConfig,
    starter: &mut dyn NbdRuntimeStarter,
    notify_socket: Option<ramshared_wsl2d::systemd_notify::NotifySocket>,
) -> Result<(), Box<dyn std::error::Error>> {
    let OriginLaunchConfig {
        size,
        sock,
        force,
        nbd_dev,
        serve_threads,
    } = cfg;
    let native_origin_mode = origin.is_native();
    let daemon_id = require_origin_daemon_identity(true, daemon_instance_id)?
        .ok_or("origin daemon instance identity is unavailable")?;
    starter.lock_memory(force, false)?;
    let reserve_policy = resolve_reserve_floor_policy(native_origin_mode, 0)?;

    let (listener, socket_guard) = bind_owned_unix_listener(Path::new(&sock))?;
    let (jobs_tx, jobs_rx) = std::sync::mpsc::sync_channel::<WMsg>(CHAN_CAP);
    let _shutdown_bridge = starter.start_shutdown_bridge(jobs_tx.clone())?;
    if SHUTDOWN.load(Ordering::SeqCst) {
        return Ok(());
    }

    let mut origin_runtime =
        origin_runtime::build(origin, size, reserve_policy.min_floor_bytes, starter)?;
    let configuration_generation = origin_runtime
        .native_session
        .as_ref()
        .and_then(|session| session.generation());
    if SHUTDOWN.load(Ordering::SeqCst) {
        return Ok(());
    }

    let device_size = origin_runtime.backend.size_bytes();
    let _routine_host_memory = origin_runtime.admission.as_ref().cloned().map(|admission| {
        ramshared_wsl2d::tier_release::register_routine_host_memory(move || {
            let host = admission.host_memory()?;
            Some(ramshared_tier::HostDrainMemory {
                total_bytes: host.total_bytes,
                available_bytes: host.available_bytes,
                commit_total_bytes: host.commit_total_bytes,
                commit_limit_bytes: host.commit_limit_bytes,
            })
        })
    });
    let _routine_host_storage = origin_runtime.admission.as_ref().cloned().map(|admission| {
        ramshared_wsl2d::tier_release::register_routine_host_storage(move |root, guest_free| {
            admission.bounded_storage_free_space(root, guest_free)
        })
    });
    let serve_pool = spawn_serve_pool(Arc::clone(&origin_runtime.backend), serve_threads, CHAN_CAP);
    let mut cache_status_publisher = CacheStatusPublisher::default();
    let shutdown_permit = {
        let mut backend = OriginServeBackend {
            runtime: &mut origin_runtime,
            serve_pool: &serve_pool,
            configuration_generation,
            daemon_instance_id: &daemon_id,
            publisher: &mut cache_status_publisher,
            last_origin_probe: None,
            recovery_cadence: recovery_cadence::RecoveryCadence::default(),
        };

        // Publish initial live cache status before opening the NBD client loop
        // (the pool starts just below with zero served jobs).
        start_nbd_acceptor(listener, device_size, jobs_tx, starter)?;
        backend.publish_status(starter, serve_threads, 0);
        notify_nbd_listening(&sock, &nbd_dev, notify_socket);

        drive_nbd_loop(&nbd_dev, &jobs_rx, &mut backend, starter)
    };
    serve_pool.join();
    let release_result = nbd_origin::release_cache(&origin_runtime.backend);
    origin_runtime.workers.wait_for_shutdown();
    drop(origin_runtime);
    drop(socket_guard);
    drop(shutdown_permit);
    release_result
}

/// Injectable NBD composition. Product selection reaches this function only
/// with an authoritative origin; the sparse branch is a non-product test seam.
#[allow(clippy::too_many_arguments)] // explicit daemon boundary keeps OS-facing test seams injectable
pub(crate) fn run_nbd_with_startup<P: VramProvider, S: NbdRuntimeStarter>(
    provider: P,
    origin: Option<OpenedOrigin>,
    size: u64,
    sock: String,
    force: bool,
    nbd_dev: String,
    use_dxg_budget: bool,
    serve_threads: usize,
    starter: &mut S,
) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(not(test))]
    if origin.is_none() {
        return Err(nbd_origin::PRODUCT_ORIGIN_REQUIRED.into());
    }
    // Consume NOTIFY_SOCKET in the single-threaded startup window; READY is
    // published below, after the listener exists.
    let notify_socket = match starter.take_notify_socket() {
        Ok(socket) => socket,
        Err(error) => {
            eprintln!("[ramsharedd] systemd notify unavailable: {error}");
            None
        }
    };
    if let Some(origin) = origin {
        run_origin_nbd(
            origin,
            OriginLaunchConfig {
                size,
                sock,
                force,
                nbd_dev,
                serve_threads,
            },
            starter,
            notify_socket,
        )
    } else {
        sparse::run_sparse_nbd(
            &provider,
            sparse::SparseLaunchConfig {
                size,
                sock,
                force,
                nbd_dev,
                use_dxg_budget,
            },
            starter,
            notify_socket,
        )
    }
}
