// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/nbd_runtime/sparse.rs
//! Non-product sparse VRAM NBD test seam and residency/WDDM poll backend.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use ramshared_block::{
    BlockBackend, Command, CommitBudgetGate, Request, ServeOutcome, SparseVramBackend,
    chunk_bytes_from_env, commit_cap_bytes_from_env, idle_free_secs_from_env, safe_commit_cap,
    serve,
};
use ramshared_vram::{ReserveFloorPolicy, VramProvider};
use ramshared_wsl2d::autotier::{AutotierConfig, BudgetInput, commit_allowed};
use ramshared_wsl2d::{
    CANARY_BYTES, CANARY_EVERY, Cadence, Canary, CanaryProbe, DemoteReason, ResidencySampler, WMsg,
};

use super::demote::NbdServeState;
use super::residency::{
    ResidencyCheckState, global_free_at_or_above_floor, observe_global_free_floor, residency_check,
    resolve_reserve_floor_policy, sparse_residency_config, sparse_residency_requests_swapoff,
};
use super::starter::{NbdBudgetGate, NbdBudgetProvider, NbdRuntimeStarter};
use super::{NbdServeBackend, drive_nbd_loop, start_nbd_acceptor_and_notify};
use crate::shutdown_signals::SHUTDOWN;
use crate::unix_listener::bind_owned_unix_listener;
use crate::{BLOCK_SIZE, zero_window};

const MAX_CONSECUTIVE_IO_ERRORS: u32 = 3;

struct SparseResidencyTracker {
    canary: Option<Canary>,
    baseline: Vec<u64>,
    sampler: ResidencySampler,
    cadence: Cadence,
    free_floor_bytes: u64,
}

pub(super) struct SparseLaunchConfig {
    pub(super) size: u64,
    pub(super) sock: String,
    pub(super) force: bool,
    pub(super) nbd_dev: String,
    pub(super) use_dxg_budget: bool,
}

trait SparseDeviceOps {
    fn mem_info(&self) -> Result<(u64, u64), String>;
    fn check_residency(
        &mut self,
        lat_us: u64,
        tracker: &mut SparseResidencyTracker,
        last_global_free: Option<u64>,
    ) -> Option<DemoteReason>;
    fn budget_refuses(&self) -> u64;
    fn committed_bytes(&self) -> u64;
    fn chunk_bytes(&self) -> u64;
    fn chunks_live(&self) -> usize;
    fn try_reclaim(
        &mut self,
        used_kb: u64,
        free_b: Option<u64>,
        probe_floor: u64,
        idle_free: Duration,
    ) -> Result<u64, String>;
    fn zero_export(&mut self, base: u64, len: u64) -> bool;
    fn serve_inline(&mut self, req: &Request, payload: &[u8]) -> ServeOutcome;
    fn finish_shutdown(&mut self);
}

struct TypedSparseDevice<'a, P: VramProvider + 'a> {
    provider: &'a P,
    sparse: SparseVramBackend<'a, P>,
    probe: CanaryProbe<P::Mem<'a>>,
}

impl<'a, P: VramProvider + 'a> SparseDeviceOps for TypedSparseDevice<'a, P> {
    fn mem_info(&self) -> Result<(u64, u64), String> {
        self.provider.mem_info().map_err(|error| error.to_string())
    }

    fn check_residency(
        &mut self,
        lat_us: u64,
        tracker: &mut SparseResidencyTracker,
        last_global_free: Option<u64>,
    ) -> Option<DemoteReason> {
        let mut residency_state = ResidencyCheckState {
            canary: &mut tracker.canary,
            baseline: &mut tracker.baseline,
            sampler: &mut tracker.sampler,
            cadence: &mut tracker.cadence,
            probe: &mut self.probe,
            free_floor_bytes: tracker.free_floor_bytes,
        };
        residency_check(lat_us, &mut residency_state, || {
            let cuda_free = self.provider.mem_info().ok().map(|(f, _)| f);
            match (cuda_free, last_global_free) {
                (Some(cuda), Some(global)) => Some(cuda.min(global)),
                (Some(cuda), None) => Some(cuda),
                (None, Some(global)) => Some(global),
                (None, None) => None,
            }
        })
    }

    fn budget_refuses(&self) -> u64 {
        self.sparse.budget_refuses
    }

    fn committed_bytes(&self) -> u64 {
        self.sparse.committed_bytes()
    }

    fn chunk_bytes(&self) -> u64 {
        self.sparse.chunk_bytes()
    }

    fn chunks_live(&self) -> usize {
        self.sparse.chunks_live()
    }

    fn try_reclaim(
        &mut self,
        used_kb: u64,
        free_b: Option<u64>,
        probe_floor: u64,
        idle_free: Duration,
    ) -> Result<u64, String> {
        self.sparse
            .try_reclaim(used_kb, free_b, probe_floor, idle_free)
            .map_err(|error| error.0)
    }

    fn zero_export(&mut self, base: u64, len: u64) -> bool {
        zero_window(&mut self.sparse, base, len).is_ok()
    }

    fn serve_inline(&mut self, req: &Request, payload: &[u8]) -> ServeOutcome {
        serve(req, payload, &mut self.sparse)
    }

    fn finish_shutdown(&mut self) {
        let freed = self.sparse.free_all_live();
        eprintln!(
            "[ramsharedd] shutdown: freed {} MiB sparse VRAM plus canary",
            freed >> 20
        );
        let _ = self.probe.zero();
    }
}

struct SparseServeBackend<'a> {
    device: &'a mut dyn SparseDeviceOps,
    dxg: Option<&'a dyn NbdBudgetProvider>,
    autotier_config: AutotierConfig,
    reserve_floor: u64,
    free_floor: u64,
    idle_free: Duration,
    residency: SparseResidencyTracker,
    consecutive_io_errors: u32,
    observed_budget_refuses: u64,
    trace_probe: bool,
    global_probe_interval: Duration,
    global_probe_timeout: Duration,
    next_global_probe_at: Instant,
    last_global_free: Option<u64>,
    global_free_streak: u32,
}

impl<'a> SparseServeBackend<'a> {
    fn new(
        device: &'a mut dyn SparseDeviceOps,
        dxg: Option<&'a dyn NbdBudgetProvider>,
        sampler: ResidencySampler,
        reserve_floor: u64,
        free_floor: u64,
    ) -> Self {
        Self {
            device,
            dxg,
            autotier_config: AutotierConfig::default(),
            reserve_floor,
            free_floor,
            idle_free: Duration::from_secs(idle_free_secs_from_env()),
            residency: SparseResidencyTracker {
                canary: None,
                baseline: Vec::new(),
                sampler,
                cadence: Cadence::new(CANARY_EVERY),
                free_floor_bytes: free_floor,
            },
            consecutive_io_errors: 0,
            observed_budget_refuses: 0,
            trace_probe: std::env::var("RAMSHARED_TRACE_PROBE").ok().as_deref() == Some("1"),
            global_probe_interval: Duration::from_secs(1),
            global_probe_timeout: Duration::from_secs(2),
            next_global_probe_at: Instant::now(),
            last_global_free: None,
            global_free_streak: 0,
        }
    }
}

impl NbdServeBackend for SparseServeBackend<'_> {
    fn zero_export(&mut self, base: u64, len: u64) -> bool {
        self.device.zero_export(base, len)
    }

    fn serve_inline(&mut self, req: &Request, payload: &[u8]) -> ServeOutcome {
        self.device.serve_inline(req, payload)
    }

    fn on_inline_served(
        &mut self,
        req: &Request,
        reply_errno: u32,
        lat_us: u64,
        state: &mut NbdServeState,
        nbd_dev: &str,
        starter: &mut dyn NbdRuntimeStarter,
    ) {
        let touches_vram = matches!(req.cmd, Command::Read | Command::Write);
        let is_io_error = touches_vram && reply_errno == ramshared_block::protocol::NBD_EIO;
        if is_io_error {
            self.consecutive_io_errors = self.consecutive_io_errors.saturating_add(1);
            if self.consecutive_io_errors >= MAX_CONSECUTIVE_IO_ERRORS
                && state.demote_spawn_authorized()
            {
                eprintln!(
                    "[ramsharedd] DEMOTE (IoErrorBurst) consecutive_errors={} -> swapoff {nbd_dev}",
                    self.consecutive_io_errors
                );
                state.spawn_swapoff("IoErrorBurst", nbd_dev, starter);
            }
        } else if touches_vram && reply_errno == ramshared_block::protocol::NBD_OK {
            self.consecutive_io_errors = 0;
        }

        if touches_vram
            && !state.demoted
            && state.demote_rx.is_none()
            && let Some(reason) =
                self.device
                    .check_residency(lat_us, &mut self.residency, self.last_global_free)
        {
            let skip = !sparse_residency_requests_swapoff(reason);
            if skip {
                eprintln!("[ramsharedd] sparse skip swapoff for {reason:?} lat={lat_us}us");
            }
            if !skip && state.demote_spawn_authorized() {
                eprintln!("[ramsharedd] DEMOTE ({reason:?}) lat={lat_us}us -> swapoff {nbd_dev}");
                state.spawn_swapoff(&format!("{reason:?}"), nbd_dev, starter);
            }
        }
    }

    fn after_message(
        &mut self,
        nbd_dev: &str,
        state: &mut NbdServeState,
        starter: &mut dyn NbdRuntimeStarter,
    ) {
        let budget_refuses = self.device.budget_refuses();
        if budget_refuses > self.observed_budget_refuses {
            self.observed_budget_refuses = budget_refuses;
            if state.demote_spawn_authorized() {
                eprintln!("[ramsharedd] WDDM constrained -> bounded swapoff {nbd_dev}");
                state.spawn_swapoff("WddmBudget", nbd_dev, starter);
            }
        }
    }

    fn on_tick(
        &mut self,
        nbd_dev: &str,
        _state: &mut NbdServeState,
        starter: &mut dyn NbdRuntimeStarter,
    ) {
        // SPEC ITEM-2: reclaim on worker thread (I/O or idle tick).
        let used_kb = starter.nbd_used_kb(nbd_dev);
        let (free_b, probe_floor) = match self.device.mem_info() {
            Ok((f, total)) => {
                let capacity = ReserveFloorPolicy::helper_capacity(Some(total), total);
                (
                    Some(f),
                    ramshared_vram::enforced_free_floor_from_configured(
                        self.reserve_floor,
                        capacity,
                        ramshared_block::gpu_cache_worker::RUNTIME_FREE_BUFFER_BYTES,
                    ),
                )
            }
            Err(_) => (None, self.free_floor),
        };
        match self
            .device
            .try_reclaim(used_kb, free_b, probe_floor, self.idle_free)
        {
            Ok(0) => {}
            Ok(n) => eprintln!(
                "[ramsharedd] sparse reclaim freed {} MiB (used_kb={used_kb} live={})",
                n >> 20,
                self.device.chunks_live()
            ),
            Err(e) => eprintln!("[ramsharedd] sparse reclaim err: {e}"),
        }
    }

    fn probe_gpu_lost(&self) -> Option<String> {
        self.device.mem_info().err()
    }

    fn poll_budget_and_recovery(
        &mut self,
        nbd_dev: &str,
        state: &mut NbdServeState,
        starter: &mut dyn NbdRuntimeStarter,
    ) {
        let Some(dxg_provider) = self.dxg else {
            return;
        };
        let now = Instant::now();
        if now >= self.next_global_probe_at {
            self.next_global_probe_at = now + self.global_probe_interval;
            self.last_global_free = starter.global_free_bytes(self.global_probe_timeout);
            if self.trace_probe
                || self
                    .last_global_free
                    .is_some_and(|free| free < self.free_floor * 2)
            {
                eprintln!(
                    "[ramsharedd] global GPU sample: free={:?} floor={} streak={}",
                    self.last_global_free, self.free_floor, self.global_free_streak
                );
            }
        }

        let committed = self.device.committed_bytes();
        let chunk = self.device.chunk_bytes();
        let budget_healthy = match dxg_provider.snapshot() {
            Ok(snapshot) => {
                let decision = commit_allowed(
                    BudgetInput {
                        budget: snapshot.budget,
                        current_usage: snapshot.current_usage,
                        cuda_committed: committed,
                        sampled_at: snapshot.sampled_at,
                    },
                    committed,
                    chunk,
                    &self.autotier_config,
                );
                if self.trace_probe {
                    eprintln!(
                        "[ramsharedd] WDDM poll sample: budget={} current_usage={} \
                         cuda_committed={} chunk={} allow={}",
                        snapshot.budget,
                        snapshot.current_usage,
                        committed,
                        chunk,
                        decision.is_ok()
                    );
                }
                decision.is_ok()
            }
            Err(error) => {
                if self.trace_probe {
                    eprintln!("[ramsharedd] WDDM poll sample error: {error}");
                }
                false
            }
        };
        let global_constrained = observe_global_free_floor(
            self.last_global_free,
            self.free_floor,
            committed,
            &mut self.global_free_streak,
            self.autotier_config.constrained_samples,
        );
        let global_healthy = global_free_at_or_above_floor(self.last_global_free, self.free_floor);

        if state.demote_spawn_authorized() && (!budget_healthy || global_constrained) {
            if global_constrained {
                eprintln!(
                    "[ramsharedd] global GPU free-floor constrained -> swapoff {nbd_dev} \
                     free={:?} floor={}",
                    self.last_global_free, self.free_floor
                );
                state.spawn_swapoff("GlobalGpuFreeFloor", nbd_dev, starter);
            } else {
                eprintln!("[ramsharedd] WDDM poll constrained -> swapoff {nbd_dev}");
                state.spawn_swapoff("WddmBudgetPoll", nbd_dev, starter);
            }
        } else if state.demoted && state.demote_rx.is_none() {
            let recovery_healthy = budget_healthy && global_healthy;
            let tier_empty = self.device.chunks_live() == 0;
            let recovery_ready = state.recovery.observe(recovery_healthy, tier_empty);
            let activation_allowed = state
                .recovery_activation
                .launch_allowed(recovery_healthy, tier_empty);
            if !state.shutdown_requested && recovery_ready && activation_allowed {
                match starter.spawn_recovery_activation(nbd_dev, 100) {
                    Ok(rx) => match state.recovery_activation.start(rx) {
                        Ok(()) => eprintln!(
                            "[ramsharedd] RECOVERING: swapon {nbd_dev} pending on owned worker"
                        ),
                        Err(error) => {
                            state.recovery_activation.mark_dispatch_failure();
                            state.recovery.reset();
                            eprintln!(
                                "[ramsharedd] RECOVERING: refusing duplicate swapon {nbd_dev}: {error}"
                            );
                        }
                    },
                    Err(error) => {
                        state.recovery_activation.mark_dispatch_failure();
                        state.recovery.reset();
                        eprintln!(
                            "[ramsharedd] RECOVERING: could not start swapon {nbd_dev}: {error}; parked"
                        );
                    }
                }
            }
        }
    }
}

pub(super) fn run_sparse_nbd<P: VramProvider>(
    provider: &P,
    cfg: SparseLaunchConfig,
    starter: &mut dyn NbdRuntimeStarter,
    notify_socket: Option<ramshared_wsl2d::systemd_notify::NotifySocket>,
) -> Result<(), Box<dyn std::error::Error>> {
    let SparseLaunchConfig {
        size,
        sock,
        force,
        nbd_dev,
        use_dxg_budget,
    } = cfg;
    let (free, total) = provider.mem_info()?;
    eprintln!(
        "[ramsharedd] VRAM free={} MiB total={} MiB",
        free >> 20,
        total >> 20
    );
    let dxg = starter.startup_budget(use_dxg_budget)?;
    let dxg_gate = dxg.as_deref().map(|provider| NbdBudgetGate {
        provider,
        config: AutotierConfig::default(),
    });
    let budget_gate = dxg_gate.as_ref().map(|gate| gate as &dyn CommitBudgetGate);
    starter.lock_memory(force, false)?;

    let probe = CanaryProbe::new(provider.alloc(CANARY_BYTES)?);
    let reserve_policy = resolve_reserve_floor_policy(false, total)?;
    let reserve_floor = reserve_policy.min_floor_bytes;
    let residency_cfg = sparse_residency_config(reserve_floor);
    let free_floor = residency_cfg.free_floor_bytes;
    let sampler = ResidencySampler::new(residency_cfg);

    let (listener, socket_guard) = bind_owned_unix_listener(Path::new(&sock))?;
    let (jobs_tx, jobs_rx) = std::sync::mpsc::sync_channel::<WMsg>(ramshared_wsl2d::CHAN_CAP);
    let _shutdown_bridge = starter.start_shutdown_bridge(jobs_tx.clone())?;
    if SHUTDOWN.load(Ordering::SeqCst) {
        return Ok(());
    }

    let chunk = chunk_bytes_from_env();
    let env_cap = commit_cap_bytes_from_env();
    let enforced_floor = reserve_policy.enforced_free_floor_bytes(
        ReserveFloorPolicy::helper_capacity(Some(total), total),
        ramshared_block::gpu_cache_worker::RUNTIME_FREE_BUFFER_BYTES,
    );
    let auto_cap = safe_commit_cap(size, total, enforced_floor);
    let commit_cap = env_cap.min(auto_cap);
    let sparse = SparseVramBackend::new_with_config(
        provider,
        ramshared_block::sparse_vram::SparseVramConfig {
            capacity: size,
            chunk_bytes: chunk,
            block_size: BLOCK_SIZE,
            reserve_policy,
            commit_cap_bytes: Some(commit_cap),
            budget_gate,
        },
    )
    .map_err(|e| e.0)?;
    eprintln!(
        "[ramsharedd] VRAM mode=sparse capacity={} MiB chunk={} MiB \
         commit_cap={} MiB reserve_floor={} MiB committed=0 (ondemand+safety)",
        size >> 20,
        chunk >> 20,
        commit_cap >> 20,
        reserve_floor >> 20
    );

    if SHUTDOWN.load(Ordering::SeqCst) {
        return Ok(());
    }
    let device_size = sparse.size_bytes();
    start_nbd_acceptor_and_notify(
        listener,
        device_size,
        &sock,
        &nbd_dev,
        notify_socket,
        jobs_tx,
        starter,
    )?;

    let mut device = TypedSparseDevice {
        provider,
        sparse,
        probe,
    };
    let shutdown_permit = {
        let mut backend = SparseServeBackend::new(
            &mut device,
            dxg.as_deref(),
            sampler,
            reserve_floor,
            free_floor,
        );
        drive_nbd_loop(&nbd_dev, &jobs_rx, &mut backend, starter)
    };
    device.finish_shutdown();
    drop(device);
    drop(socket_guard);
    drop(shutdown_permit);
    Ok(())
}
