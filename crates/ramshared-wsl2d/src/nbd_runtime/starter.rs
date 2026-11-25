// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/nbd_runtime/starter.rs
//! Injectable OS/budget traits and production host adapters for NBD runtime composition.

use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use ramshared_block::{
    AuthoritativeOriginBackend, CommitBudgetGate, FileOrigin, OriginState as DurableOriginState,
};
use ramshared_wsl2d::autotier::{AutotierConfig, BudgetInput, commit_allowed};
use ramshared_wsl2d::swap::{
    DEMOTE_STUCK_DETECTION_WINDOW, SwapoffOutcome, spawn_activate_swap, spawn_swapoff,
};
use ramshared_wsl2d::{WMsg, spawn_acceptor};

use crate::cache_status::{OriginCacheStatus, write_origin_cache_status};
use crate::memory_lock::lock_memory;
use crate::origin_identity::{SealedOriginManifest, read_sealed_origin_manifest};
use crate::proc_swaps::{nbd_swap_is_explicitly_absent_from_path, nbd_used_kb_from_path};
use crate::shutdown::ShutdownPermit;
use crate::shutdown_signals::{
    NbdShutdownBridge, SHUTDOWN, install_shutdown_signals, spawn_nbd_shutdown_bridge,
};
use crate::{
    CACHE_TARGET_REQUEST_PATH, ORIGIN_MANIFEST_PATH, OriginCache, RECLAIM_REQUEST_PATH,
    WorkerSpawnParams, critical_cache_reclaim_requested_at, origin_plan, unix_time_ms,
    worker_launcher, worker_startup,
};

/// Synthetic budget view for sparse lifecycle simulation. Product NBD always
/// has an origin and never enters this injected path; live GPU authority belongs
/// to the isolated worker, not these test counters.
pub(crate) struct NbdBudgetSnapshot {
    pub(crate) budget: u64,
    pub(crate) current_usage: u64,
    pub(crate) sampled_at: Instant,
}

pub(crate) trait NbdBudgetProvider {
    fn snapshot(&self) -> Result<NbdBudgetSnapshot, String>;
}

pub(crate) struct NbdBudgetGate<'a> {
    pub(crate) provider: &'a dyn NbdBudgetProvider,
    pub(crate) config: AutotierConfig,
}

impl CommitBudgetGate for NbdBudgetGate<'_> {
    fn allow_commit(&self, committed: u64, next_chunk: u64) -> Result<(), String> {
        let snapshot = self.provider.snapshot()?;
        commit_allowed(
            BudgetInput {
                budget: snapshot.budget,
                current_usage: snapshot.current_usage,
                cuda_committed: committed,
                sampled_at: snapshot.sampled_at,
            },
            committed,
            next_chunk,
            &self.config,
        )
        .map(|_| ())
        .map_err(|error| error.to_string())
    }
}

/// OS-facing edges for the NBD worker. The production implementation owns the
/// process memory lock, protocol acceptor, `/proc/swaps` observation, and
/// demote-status file. Tests inject a deterministic in-memory implementation,
/// so exercising daemon logic never requires a CUDA context, NBD client, swap
/// device, or host status path.
pub(crate) trait NbdRuntimeStarter {
    #[cfg(test)]
    fn seed_test_catalog(&self, _catalog: &ramshared_wsl2d::host_catalog::SharedHostCatalog) {}

    /// Component fixtures may supply supervisor admission without impersonating
    /// the installed daemon or modifying its live supervisor state.
    #[cfg(test)]
    fn test_recovery_admission(&self) -> Option<bool> {
        None
    }

    fn take_notify_socket(
        &mut self,
    ) -> Result<
        Option<ramshared_wsl2d::systemd_notify::NotifySocket>,
        ramshared_wsl2d::systemd_notify::NotifyError,
    > {
        ramshared_wsl2d::systemd_notify::NotifySocket::from_env()
    }

    fn lock_memory(
        &mut self,
        force: bool,
        lock_future: bool,
    ) -> Result<(), Box<dyn std::error::Error>>;

    fn start_acceptor(
        &mut self,
        listener: UnixListener,
        exports: std::sync::Arc<Vec<ramshared_block::handshake::Export>>,
        tx_flags: u16,
        jobs_tx: std::sync::mpsc::SyncSender<WMsg>,
    ) -> Result<(), Box<dyn std::error::Error>>;

    fn start_shutdown_bridge(
        &mut self,
        _jobs_tx: std::sync::mpsc::SyncSender<WMsg>,
    ) -> Result<Option<NbdShutdownBridge>, Box<dyn std::error::Error>> {
        Ok(None)
    }

    fn nbd_used_kb(&mut self, nbd_dev: &str) -> u64;

    /// Exclusive shutdown custody, held through explicit backend drop.
    fn try_claim_shutdown(&mut self, nbd_dev: &str) -> Option<ShutdownPermit>;

    /// Absence must be proved by the production `/proc/swaps` identity
    /// observation or by an explicit test adapter. An ambiguous zero-used
    /// value is never absence proof.
    fn nbd_swap_is_explicitly_absent(&mut self, _nbd_dev: &str) -> bool {
        false
    }

    /// Publish the DEMOTE counters and, once a drain has outlived the stuck
    /// window, its age. Takes the whole record so a later field does not ripple
    /// through every adapter.
    fn publish_demote(&mut self, status: &ramshared_wsl2d::DemoteStatusFile);

    /// Age at which a still-pending drain is reported stuck (SPEC §9.2).
    ///
    /// Production uses [`DEMOTE_STUCK_DETECTION_WINDOW`]; tests inject a short
    /// window only after supplying deterministic terminal observations. The
    /// window is a **detection** bound, never an abort — see the constant.
    fn demote_stuck_window(&self) -> Duration {
        DEMOTE_STUCK_DETECTION_WINDOW
    }

    fn publish_origin_cache(&mut self, _status: &OriginCacheStatus) {}

    fn elapsed_us(&mut self, started: Instant) -> u64;

    fn spawn_swapoff(&mut self, nbd_dev: &str) -> std::sync::mpsc::Receiver<SwapoffOutcome>;

    fn spawn_peer_closed_swapoff(
        &mut self,
        nbd_dev: &str,
    ) -> std::sync::mpsc::Receiver<SwapoffOutcome> {
        self.spawn_swapoff(nbd_dev)
    }

    /// Starts recovery activation outside the only NBD serving thread. The
    /// caller owns and polls the returned receiver until it observes a terminal
    /// result, including during shutdown.
    fn spawn_recovery_activation(
        &mut self,
        nbd_dev: &str,
        priority: i16,
    ) -> Result<std::sync::mpsc::Receiver<bool>, Box<dyn std::error::Error>>;

    fn startup_budget(
        &mut self,
        _requested: bool,
    ) -> Result<Option<Box<dyn NbdBudgetProvider>>, Box<dyn std::error::Error>> {
        Ok(None)
    }

    fn global_free_bytes(&mut self, _timeout: Duration) -> Option<u64> {
        None
    }

    fn origin_worker_plan(&mut self, size: u64, reserve: u64) -> origin_plan::PlanResult {
        Ok(origin_plan::WorkerPlan::automatic(
            self.origin_cache_target_bytes(size)?,
            ramshared_block::chunk_bytes_from_env(),
            reserve,
        ))
    }

    fn origin_cache_target_bytes(
        &mut self,
        logical_size: u64,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        Ok(logical_size)
    }

    /// Production waits between fail-closed teardown observations. Tests inject
    /// zero only after supplying deterministic replacement observations.
    fn teardown_retry_delay(&mut self) -> Duration {
        Duration::from_secs(5)
    }

    #[cfg(test)]
    fn begin_isolated_worker_start(
        &mut self,
        params: &WorkerSpawnParams,
    ) -> std::io::Result<worker_startup::Startup> {
        worker_launcher::begin(params)
    }

    fn begin_isolated_worker_start_guarded(
        &mut self,
        params: &WorkerSpawnParams,
        authority: worker_startup::StartupAuthority,
    ) -> std::io::Result<worker_startup::Startup> {
        #[cfg(test)]
        {
            if !authority() {
                return Err(std::io::Error::other("startup authority revoked"));
            }
            self.begin_isolated_worker_start(params)
                .map(|startup| startup.restrict(authority))
        }
        #[cfg(not(test))]
        {
            let _ = (params, authority);
            Err(std::io::Error::other(
                "guarded worker startup is unsupported",
            ))
        }
    }

    fn spawn_isolated_gpu_worker(
        &mut self,
        params: &WorkerSpawnParams,
    ) -> worker_startup::InitialStartResult {
        worker_launcher::spawn(params)
    }

    /// Read-only Native Linux GPU driver observation used only while a worker is
    /// missing/recovering and no Windows host catalog governs admission.
    fn native_gpu_driver_observation(
        &self,
    ) -> Option<crate::native_driver_probe::NativeDriverObservation> {
        None
    }
}

/// Cadence for the origin recovery probe (SPEC swap-drain-safety RF-10).
pub(crate) const ORIGIN_PROBE_INTERVAL: Duration = Duration::from_secs(1);

/// Rate-limited origin recovery probe: reads a block and **fsyncs** it.
///
/// Left on the per-job path this is an fsync per NBD request in the very loop
/// that must answer them. One probe per interval keeps recovery (three
/// successes flip `Ready`) without starving the serve path.
pub(crate) fn probe_origin_rate_limited(
    cache: &AuthoritativeOriginBackend<FileOrigin, OriginCache>,
    last_origin_probe: &mut Option<Instant>,
) {
    let due = last_origin_probe.is_none_or(|at| at.elapsed() >= ORIGIN_PROBE_INTERVAL);
    if due
        && matches!(
            cache.origin_state(),
            DurableOriginState::Failed | DurableOriginState::Degraded
        )
    {
        *last_origin_probe = Some(Instant::now());
        let _ = cache.probe_origin();
    }
}

pub(crate) fn critical_cache_reclaim_pending(daemon_instance_id: &str) -> bool {
    unix_time_ms().is_some_and(|now_unix_ms| {
        critical_cache_reclaim_requested_at(
            Path::new(CACHE_TARGET_REQUEST_PATH),
            Path::new(RECLAIM_REQUEST_PATH),
            daemon_instance_id,
            now_unix_ms,
        )
    })
}

pub(crate) struct ProductionNbdRuntimeStarter;

/// Configurable filesystem/signal adapters backing [`ProductionNbdRuntimeStarter`].
pub(crate) struct ProductionNbdRuntimeAdapters {
    pub(crate) origin_manifest_path: PathBuf,
    pub(crate) swaps_path: PathBuf,
    pub(crate) demote_status_path: PathBuf,
    pub(crate) origin_cache_status_path: PathBuf,
    pub(crate) cli_cap_mib: Option<String>,
    pub(crate) shutdown_flag: &'static AtomicBool,
    pub(crate) shutdown_poll_interval: Duration,
    pub(crate) install_signals: fn() -> Result<(), Box<dyn std::error::Error>>,
}

impl ProductionNbdRuntimeAdapters {
    pub(crate) fn production() -> Self {
        Self {
            origin_manifest_path: PathBuf::from(ORIGIN_MANIFEST_PATH),
            swaps_path: PathBuf::from("/proc/swaps"),
            demote_status_path: PathBuf::from(ramshared_wsl2d::DEMOTE_STATUS_PATH),
            origin_cache_status_path: PathBuf::from("/run/ramshared/cache-status.json"),
            cli_cap_mib: std::env::var("RAMSHARED_VRAM_CACHE_CAP_MIB").ok(),
            shutdown_flag: &SHUTDOWN,
            shutdown_poll_interval: Duration::from_millis(100),
            install_signals: install_shutdown_signals,
        }
    }

    pub(crate) fn origin_cache_target_bytes(
        &self,
        logical_size: u64,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        let manifest_path = self.origin_manifest_path.to_string_lossy();
        let manifest = read_sealed_origin_manifest(&manifest_path)?;
        origin_cache_target_bytes_from_manifest(
            &manifest,
            logical_size,
            self.cli_cap_mib.as_deref(),
        )
    }

    pub(crate) fn start_shutdown_bridge(
        &self,
        jobs_tx: std::sync::mpsc::SyncSender<WMsg>,
    ) -> Result<Option<NbdShutdownBridge>, Box<dyn std::error::Error>> {
        (self.install_signals)()?;
        Ok(Some(spawn_nbd_shutdown_bridge(
            self.shutdown_flag,
            jobs_tx,
            self.shutdown_poll_interval,
        )))
    }

    pub(crate) fn nbd_used_kb(&self, nbd_dev: &str) -> u64 {
        nbd_used_kb_from_path(&self.swaps_path, nbd_dev).unwrap_or_else(|error| {
            eprintln!(
                "[ramsharedd] strict /proc/swaps observation failed for {nbd_dev}: {error}; keeping backend allocated"
            );
            u64::MAX
        })
    }

    pub(crate) fn nbd_swap_is_explicitly_absent(&self, nbd_dev: &str) -> bool {
        nbd_swap_is_explicitly_absent_from_path(&self.swaps_path, nbd_dev).unwrap_or_else(|error| {
            eprintln!(
                "[ramsharedd] strict /proc/swaps absence proof failed for {nbd_dev}: {error}; keeping backend allocated"
            );
            false
        })
    }

    pub(crate) fn publish_demote(&self, status: &ramshared_wsl2d::DemoteStatusFile) {
        if let Err(error) = ramshared_wsl2d::write_demote_status(&self.demote_status_path, status) {
            eprintln!("[ramsharedd] demote-status write: {error}");
        }
    }

    pub(crate) fn publish_origin_cache(&self, status: &OriginCacheStatus) {
        if let Err(error) = write_origin_cache_status(&self.origin_cache_status_path, status) {
            eprintln!("[ramsharedd] cache-status write: {error}");
        }
    }
}

pub(crate) fn origin_cache_target_bytes_from_manifest(
    manifest: &SealedOriginManifest,
    logical_size: u64,
    cli_cap_mib: Option<&str>,
) -> Result<u64, Box<dyn std::error::Error>> {
    let target = manifest.physical_cache_cap_mib * 1024 * 1024;
    if target == 0 || target > logical_size {
        return Err("sealed physical cache cap exceeds logical capacity".into());
    }
    if let Some(cli_cap) = cli_cap_mib
        && cli_cap.parse::<u64>().ok() != Some(manifest.physical_cache_cap_mib)
    {
        return Err("CLI cache cap differs from sealed origin manifest".into());
    }
    Ok(target)
}

impl NbdRuntimeStarter for ProductionNbdRuntimeStarter {
    fn begin_isolated_worker_start_guarded(
        &mut self,
        params: &WorkerSpawnParams,
        authority: worker_startup::StartupAuthority,
    ) -> std::io::Result<worker_startup::Startup> {
        worker_launcher::begin_guarded(params, authority)
    }

    fn try_claim_shutdown(&mut self, nbd_dev: &str) -> Option<ShutdownPermit> {
        ShutdownPermit::claim(nbd_dev)
    }

    fn origin_cache_target_bytes(
        &mut self,
        logical_size: u64,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        ProductionNbdRuntimeAdapters::production().origin_cache_target_bytes(logical_size)
    }

    fn lock_memory(
        &mut self,
        force: bool,
        lock_future: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        lock_memory(force, lock_future)
    }

    fn start_acceptor(
        &mut self,
        listener: UnixListener,
        exports: std::sync::Arc<Vec<ramshared_block::handshake::Export>>,
        tx_flags: u16,
        jobs_tx: std::sync::mpsc::SyncSender<WMsg>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let _ = spawn_acceptor(listener, exports, tx_flags, jobs_tx);
        Ok(())
    }

    fn start_shutdown_bridge(
        &mut self,
        jobs_tx: std::sync::mpsc::SyncSender<WMsg>,
    ) -> Result<Option<NbdShutdownBridge>, Box<dyn std::error::Error>> {
        ProductionNbdRuntimeAdapters::production().start_shutdown_bridge(jobs_tx)
    }

    fn nbd_used_kb(&mut self, nbd_dev: &str) -> u64 {
        ProductionNbdRuntimeAdapters::production().nbd_used_kb(nbd_dev)
    }

    fn nbd_swap_is_explicitly_absent(&mut self, nbd_dev: &str) -> bool {
        ProductionNbdRuntimeAdapters::production().nbd_swap_is_explicitly_absent(nbd_dev)
    }

    fn publish_demote(&mut self, status: &ramshared_wsl2d::DemoteStatusFile) {
        ProductionNbdRuntimeAdapters::production().publish_demote(status);
    }

    fn publish_origin_cache(&mut self, status: &OriginCacheStatus) {
        ProductionNbdRuntimeAdapters::production().publish_origin_cache(status);
    }

    fn elapsed_us(&mut self, started: Instant) -> u64 {
        started.elapsed().as_micros() as u64
    }

    fn spawn_swapoff(&mut self, nbd_dev: &str) -> std::sync::mpsc::Receiver<SwapoffOutcome> {
        spawn_swapoff(nbd_dev)
    }

    fn spawn_peer_closed_swapoff(
        &mut self,
        nbd_dev: &str,
    ) -> std::sync::mpsc::Receiver<SwapoffOutcome> {
        ramshared_wsl2d::swap::spawn_peer_closed_swapoff(nbd_dev)
    }

    fn spawn_recovery_activation(
        &mut self,
        nbd_dev: &str,
        priority: i16,
    ) -> Result<std::sync::mpsc::Receiver<bool>, Box<dyn std::error::Error>> {
        Ok(spawn_activate_swap(nbd_dev, priority)?)
    }

    fn native_gpu_driver_observation(
        &self,
    ) -> Option<crate::native_driver_probe::NativeDriverObservation> {
        crate::native_driver_probe::observe_system()
    }
}
