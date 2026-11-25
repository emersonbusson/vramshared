// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/ublk_server/residency_worker.rs
//! Residency-worker lifecycle for the DT-3 VRAM ublk server.

use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ramshared_block::Command;
use ramshared_cuda::Cuda;
use ramshared_vram::VramMemory;

use super::{
    RING_CHAN_CAP, WorkerReply, cuda_to_io, open_ublk_char_device, run_ring_owner, serve_request,
};
use crate::swap::{
    DEMOTE_STUCK_DETECTION_WINDOW, DemoteRearm, SwapoffOutcome, demote_stuck_report_due,
    spawn_swapoff,
};
use crate::ublk;
use crate::{
    CANARY_BYTES, CANARY_EVERY, Cadence, Canary, CanaryProbe, ResidencyConfig, ResidencySampler,
    Verdict, VramBackend,
};

/// Handle of the DT-3 VRAM server **with residency** (canary §9 + probe §9.4 inside the
/// worker). Besides `join`, exposes `demote_count` — how many DEMOTE verdicts the
/// canary emitted (observable without real swap).
pub struct ServerHandleDt3VramResidency {
    pub(super) ring: JoinHandle<io::Result<()>>,
    pub(super) worker: JoinHandle<io::Result<()>>,
    pub(super) demotes: Arc<AtomicU32>,
    pub(super) shutdown_tx: SyncSender<Arc<crate::tier_release::BackendShutdownLease<'static>>>,
}

impl ServerHandleDt3VramResidency {
    /// Share the lifecycle owner's exclusive lease before stopping the ring.
    /// The caller retains its Arc through stop/join/delete; the worker retains
    /// this Arc through the actual backend and canary destruction.
    pub fn authorize_shutdown(
        &self,
        permit: Arc<crate::tier_release::BackendShutdownLease<'static>>,
    ) -> io::Result<()> {
        match self.shutdown_tx.try_send(permit) {
            Ok(()) | Err(mpsc::TrySendError::Disconnected(_)) => Ok(()),
            Err(mpsc::TrySendError::Full(_)) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "ublk worker already owns a shutdown permit",
            )),
        }
    }

    /// Number of DEMOTEs emitted by the canary so far (latency §9 + probe §9.4).
    pub fn demote_count(&self) -> u32 {
        self.demotes.load(Ordering::Relaxed)
    }

    pub fn join(self) -> io::Result<()> {
        let ring_result = self
            .ring
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("ring owner panicked")));
        let worker_result = self
            .worker
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("vram residency worker panicked")));
        match ring_result {
            Ok(()) => worker_result,
            Err(error) => Err(error),
        }
    }
}

/// Loop of the worker DT-3 VRAM **with residency**, generic over the VRAM backend
/// (`M: VramMemory`): serves each request measuring serve-only latency (canary §9),
/// probes content/free in cadence (§9.4) and, upon a DEMOTE verdict, triggers `swapoff`
/// in a separate thread (Discipline 3). The free-floor is supplied via `mem_free` — decoupled
/// from the backend: CUDA passes `|| ctx.mem_info()`, a future Vulkan provider will pass its own.
/// On a closed work channel, preserves VRAM and the canary until drain
/// observation is terminal and the lifecycle owner proves shutdown is safe.
/// Preserving memory after ring loss does not restore the failed I/O path.
///
/// Runs **entirely on the calling thread** (context affinity): `backend`,
/// `probe` and the `mem_free` closure borrow the thread-affine context, which lives in the
/// caller until this function returns. Runtime observations and swapoff are injected so the
/// state machine can be tested without GPU allocation or host swap effects.
#[allow(clippy::too_many_arguments)] // explicit worker inputs and runtime observations
pub(super) fn serve_ublk_residency<M, F, E, S, T, G, R>(
    mut backend: VramBackend<M>,
    mut probe: CanaryProbe<M>,
    mem_free: F,
    work_rx: Receiver<ublk::IoWork>,
    reply_tx: Sender<WorkerReply>,
    swap_dev: &str,
    residency: ResidencyConfig,
    demotes: Arc<AtomicU32>,
    mut elapsed_us: E,
    mut spawn_swapoff: S,
    stuck_window: Duration,
    mut report_stuck: T,
    mut shutdown_guard: R,
) -> io::Result<()>
where
    M: VramMemory,
    F: Fn() -> Option<u64>,
    E: FnMut(Instant) -> u64,
    S: FnMut(&str) -> Receiver<SwapoffOutcome>,
    T: FnMut(u64),
    R: FnMut() -> Option<G>,
{
    // Residency state (mirrors the NBD worker of main.rs).
    let mut canary: Option<Canary> = None;
    let mut baseline: Vec<u64> = Vec::new();
    let mut sampler = ResidencySampler::new(residency);
    let mut cadence = Cadence::new(CANARY_EVERY);
    let mut demoted = false;
    let mut demote_rx: Option<Receiver<SwapoffOutcome>> = None;
    // a terminal `swapoff` outcome is classified before the
    // canary may re-arm. Shared with the NBD worker so the two loops cannot
    // drift into different retry behaviour.
    let mut rearm = DemoteRearm::new();
    // SPEC §9.2: a `swapoff` blocked inside `try_to_unuse` cannot be cancelled
    // from userspace. Detect the age and report it once per flight; never abort
    // the child and never tear the device down while a drain is in flight.
    let mut demote_started_at: Option<Instant> = None;
    let mut demote_stuck_reported = false;

    while let Ok(mut work) = work_rx.recv() {
        let touches_vram = matches!(work.req.cmd, Command::Read | Command::Write);

        // serve-only (DT-16): times only the VRAM op, not the queue wait.
        let t0 = Instant::now();
        let result = serve_request(&work.req, &mut backend, &mut work.payload);
        let lat_us = elapsed_us(t0);
        let is_read = work.req.cmd == Command::Read;
        let reply = WorkerReply {
            qid: work.qid,
            tag: work.tag,
            result,
            buf: work.payload,
            is_read,
        };
        if reply_tx.send(reply).is_err() {
            break; // ring owner dropped
        }

        // Non-blocking poll of the ongoing DEMOTE swapoff.
        if let Some(rx) = demote_rx.take() {
            match rx.try_recv() {
                Ok(outcome) => {
                    if matches!(outcome, SwapoffOutcome::Drained) {
                        demoted = true;
                    }
                    demote_started_at = None;
                    demote_stuck_reported = false;
                    rearm.record(outcome, Instant::now());
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    // SPEC §9.2: report a drain that outlived the detection
                    // window. The reporter is injected like `spawn_swapoff`:
                    // this worker has no status file of its own (the daemon
                    // owns `/run/ramshared/demote-status.json`), so production
                    // logs the age and the test counts the call.
                    if demote_stuck_report_due(
                        demote_started_at,
                        Instant::now(),
                        stuck_window,
                        demote_stuck_reported,
                    ) {
                        let age_secs = demote_started_at
                            .map(|started| started.elapsed().as_secs())
                            .unwrap_or(0);
                        report_stuck(age_secs);
                        demote_stuck_reported = true;
                    }
                    demote_rx = Some(rx);
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // The worker vanished without a terminal result. Treat it
                    // as a transient rejection rather than inventing success.
                    demote_started_at = None;
                    demote_stuck_reported = false;
                    rearm.record(SwapoffOutcome::Rejected { exit_code: None }, Instant::now());
                }
            }
        }

        let canary_arms = !demoted && demote_rx.is_none();
        // The canary keeps sampling through a backoff so detection is not
        // starved by the retry policy; only the spawn waits.
        let may_spawn = canary_arms && rearm.may_attempt_at(Instant::now());

        // Canary §9 (serve-only latency) — primary trigger.
        if touches_vram && canary_arms {
            match canary.as_mut() {
                None => {
                    if let Some(med) = calculate_baseline(&mut baseline, lat_us) {
                        canary = Some(Canary::new(residency, med));
                    }
                }
                Some(c) => {
                    // free=u64::MAX on purpose: the signal here is latency; free/
                    // content come from the probe §9.4 below. `sample` is the
                    // scrutinee, so it runs even while the spawn is held back.
                    if let Verdict::Demote(_) = c.sample(lat_us, true, u64::MAX)
                        && may_spawn
                    {
                        demotes.fetch_add(1, Ordering::Relaxed);
                        demote_started_at = Some(Instant::now());
                        demote_stuck_reported = false;
                        demote_rx = Some(spawn_swapoff(swap_dev));
                    }
                }
            }
        }

        // Dedicated probe §9.4 (content/free in cadence) with hysteresis.
        if touches_vram && canary_arms && cadence.tick() {
            let content = probe.check_content().ok();
            let free = mem_free();
            if let Verdict::Demote(_) = sampler.sample(content, free)
                && may_spawn
            {
                demotes.fetch_add(1, Ordering::Relaxed);
                demote_started_at = Some(Instant::now());
                demote_stuck_reported = false;
                demote_rx = Some(spawn_swapoff(swap_dev));
            }
        }
    }

    // A disconnected ring cannot serve more requests. Keep the memory owner
    // alive; neither an observation deadline nor channel loss proves drain.
    loop {
        if let Some(rx) = demote_rx.take()
            && matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty))
        {
            demote_rx = Some(rx);
        }
        if demote_rx.is_none()
            && let Some(permit) = shutdown_guard()
        {
            let _ = backend.zero();
            let _ = probe.zero();
            drop(backend);
            drop(probe);
            drop(permit);
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Helper function to calculate the baseline median from the latency samples.
/// Once enough samples (16) are collected, it returns the median value.
pub(super) fn calculate_baseline(baseline: &mut Vec<u64>, lat_us: u64) -> Option<u64> {
    baseline.push(lat_us);
    if baseline.len() >= 16 {
        baseline.sort_unstable();
        let med = baseline[baseline.len() / 2].max(1);
        Some(med)
    } else {
        None
    }
}

/// Logs an edge-triggered warning when a DEMOTE `swapoff` outlives its detection window.
pub(super) fn report_stuck_drain(age_secs: u64) {
    eprintln!(
        "[ramshared-ublk] DEMOTE: swapoff did not finish in {age_secs}s; \
         drain still in progress (SPEC §9.2: detection, not abort)"
    );
}

/// Runs [`serve_ublk_residency`] with the daemon's standard wall-clock timer,
/// stuck-drain warning reporter, and shutdown-lease channel/registry lookup.
#[allow(clippy::too_many_arguments)]
pub(super) fn serve_ublk_residency_default<M, F, S>(
    backend: VramBackend<M>,
    probe: CanaryProbe<M>,
    mem_free: F,
    work_rx: Receiver<ublk::IoWork>,
    reply_tx: Sender<WorkerReply>,
    swap_dev: &str,
    residency: ResidencyConfig,
    demotes: Arc<AtomicU32>,
    shutdown_rx: &Receiver<Arc<crate::tier_release::BackendShutdownLease<'static>>>,
    spawn_swapoff_fn: S,
) -> io::Result<()>
where
    M: VramMemory,
    F: Fn() -> Option<u64>,
    S: FnMut(&str) -> Receiver<SwapoffOutcome>,
{
    serve_ublk_residency(
        backend,
        probe,
        mem_free,
        work_rx,
        reply_tx,
        swap_dev,
        residency,
        demotes,
        |started| u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
        spawn_swapoff_fn,
        DEMOTE_STUCK_DETECTION_WINDOW,
        report_stuck_drain,
        || {
            shutdown_rx.try_recv().ok().or_else(|| {
                crate::tier_release::try_claim_backend_shutdown_for(swap_dev).map(Arc::new)
            })
        },
    )
}

/// Spawns the DT-3 residency worker and ring-owner threads around an already
/// opened queue server and device guard.
pub(super) fn spawn_residency_handle<Q, C, W>(
    server: Q,
    char_dev: C,
    queue_depth: u16,
    buf_size: usize,
    run_worker: W,
) -> ServerHandleDt3VramResidency
where
    Q: super::QueueServer + Send + 'static,
    C: Send + 'static,
    W: FnOnce(
            Receiver<ublk::IoWork>,
            Sender<WorkerReply>,
            Arc<AtomicU32>,
            Receiver<Arc<crate::tier_release::BackendShutdownLease<'static>>>,
        ) -> io::Result<()>
        + Send
        + 'static,
{
    let (work_tx, work_rx) = mpsc::sync_channel::<ublk::IoWork>(RING_CHAN_CAP);
    let (reply_tx, reply_rx) = mpsc::channel::<WorkerReply>();

    let demotes = Arc::new(AtomicU32::new(0));
    let demotes_worker = Arc::clone(&demotes);
    let (shutdown_tx, shutdown_rx) = mpsc::sync_channel(1);

    let worker = thread::spawn(move || run_worker(work_rx, reply_tx, demotes_worker, shutdown_rx));

    let ring = thread::spawn(move || {
        let _char_dev = char_dev;
        run_ring_owner(server, queue_depth, buf_size, work_tx, reply_rx)
    });

    ServerHandleDt3VramResidency {
        ring,
        worker,
        demotes,
        shutdown_tx,
    }
}

/// Like [`spawn_server_dt3_vram`], but the worker (owner of the CUDA context) **also runs
/// the residency machine** (Option 1 of PRD `ublk-daemon-integration`): measures
/// serve-only latency (canary §9), probes content/free in cadence (§9.4) and, upon
/// DEMOTE verdict, triggers `swapoff(swap_dev)` in a separate thread (Discipline 3).
/// Everything on the worker thread — no cross-thread CUDA calls (context affinity).
pub fn spawn_server_dt3_vram_with_residency(
    char_path: impl AsRef<Path>,
    queue_depth: u16,
    buf_size: usize,
    vram_bytes: usize,
    block_size: u32,
    swap_dev: String,
    residency: ResidencyConfig,
) -> io::Result<ServerHandleDt3VramResidency> {
    let char_dev = open_ublk_char_device(char_path)?;
    let server = ramshared_uring::UblkServer::new(char_dev.as_raw_fd(), queue_depth, buf_size)?;

    Ok(spawn_residency_handle(
        server,
        char_dev,
        queue_depth,
        buf_size,
        move |work_rx, reply_tx, demotes_worker, shutdown_rx| -> io::Result<()> {
            // The entire CUDA stack + the canary region live in this thread (context affinity).
            let cuda = Cuda::load().map_err(cuda_to_io)?;
            let device = cuda.device(0).map_err(cuda_to_io)?;
            let ctx = cuda.create_context(&device).map_err(cuda_to_io)?;
            let mut mem = ctx.alloc(vram_bytes).map_err(cuda_to_io)?;
            mem.zero().map_err(cuda_to_io)?;
            let backend = VramBackend::new(mem, block_size);
            // Dedicated canary region (§9.4): separated from swap, not addressable by I/O.
            let canary_region = ctx.alloc(CANARY_BYTES).map_err(cuda_to_io)?;
            let probe = CanaryProbe::new(canary_region);
            // The free-floor comes from the CUDA context (`mem_info`); the residency loop is
            // generic over the VRAM backend (RF-G1) — a future Vulkan provider reuses
            // `serve_ublk_residency` with its own `mem_free`.
            serve_ublk_residency_default(
                backend,
                probe,
                || ctx.mem_info().ok().map(|(f, _)| f as u64),
                work_rx,
                reply_tx,
                &swap_dev,
                residency,
                demotes_worker,
                &shutdown_rx,
                spawn_swapoff,
            )
        },
    ))
}
