// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main.rs
//! RamShared userspace daemon: authoritative origin I/O and revocable GPU cache.
//!
//! Unix NBD connections use dedicated reader/writer threads and a bounded origin
//! serve pool. GPU cache operations run in isolated child processes; the parent
//! owns bounded IPC, worker recovery, live authority and ordered shutdown.
//! Validation, runtime loops, worker custody, and action executors live in
//! dedicated sibling modules so `main.rs` remains a compact entrypoint.

use std::time::Duration;

#[cfg(test)]
use ramshared_block::{
    AuthoritativeOriginBackend, Command, CommitBudgetGate, FileOrigin, GpuSample, GpuWorkerConfig,
    OriginCacheIo, OriginState as DurableOriginState, WriteOptions, WriteThroughCacheBackend,
};
use ramshared_block::{
    BestEffortCache, CacheMutation, CacheRead, CacheState as OriginCacheState, DisabledCache,
    IpcCacheClient,
};
use ramshared_vram::GpuAdapterIdentity;
#[cfg(test)]
use ramshared_vram::{
    GpuBudgetSnapshot, GpuBudgetTelemetry, SEALED_RESERVE_MIN_MIB, SEALED_RESERVE_PERCENT,
    VramMemory, VramProvider, WorkerCacheTelemetry,
};
use ramshared_wsl2d::gpu_budget::{GPU_BUDGET_MAX_AGE_MS, SERVE_IDLE_TICK};
use ramshared_wsl2d::tier_release;
#[cfg(test)]
use ramshared_wsl2d::{
    CANARY_BYTES, CANARY_EVERY, CHAN_CAP, Cadence, Canary, CanaryProbe, DemoteReason,
    DemoteStatusFile, RamBackend, Reply, ResidencyConfig, ResidencySampler, SliceIoCounters,
    VramBackend, VramGauge, WMsg, autotier::AutotierConfig, broker_srv::BrokerConfig,
    host_gate::ControlPlaneAuthority, spawn_acceptor, swap::SwapoffOutcome,
};

mod args;
mod broker_runtime;
mod budget_probe;
mod cache_admission;
mod cache_control;
mod cache_status;
mod command_runtime;
mod control_requests;
mod crash_record;
mod critical_devices;
mod daemon_dispatch;
mod gpu_worker_entry;
mod gpu_worker_request;
mod gpu_worker_runtime;
mod gpu_worker_selection;
mod heartbeat_campaign;
mod manifest_file;
mod memory_lock;
mod native_cache;
mod native_driver_probe;
#[cfg(test)]
#[path = "main_tests/native_fixture.rs"]
mod native_fixture;
mod native_origin;
mod nbd_origin;
mod nbd_runtime;
mod origin_cache_client;
mod origin_identity;
mod origin_plan;
mod origin_runtime;
mod origin_validation;
mod platform_admission;
mod pressure_recovery;
mod proc_swaps;
mod serve_pool;
mod shutdown;
mod shutdown_signals;
mod spill_pump;
mod ublk_runtime;
mod unix_listener;
mod worker_custody;
mod worker_launcher;
mod worker_recovery;
mod worker_registry;
mod worker_retry;
mod worker_set;
mod worker_startup;
mod worker_supervisor;

use args::{AppArgs, BackendKind, Transport, daemon_version_requested};
use cache_admission::{ControlPlaneCacheAdmission, compose_cache_admission};
use control_requests::{
    CACHE_TARGET_REQUEST_PATH, RECLAIM_REQUEST_PATH, attempt_vsock_lease,
    critical_cache_reclaim_requested_at, daemon_instance_id, origin_cache_runtime_ok,
    require_origin_daemon_identity, unix_time_ms,
};
use daemon_dispatch::{DaemonActionRunner, ProductionDaemonRunner, zero_window};
use gpu_worker_entry::run_isolated_gpu_worker_entry;
use gpu_worker_request::WorkerSpawnParams;
use gpu_worker_runtime::UnavailableVramProvider;
use heartbeat_campaign::spawn_heartbeat;
use memory_lock::lock_memory;
use nbd_runtime::{NbdRuntimeStarter, ProductionNbdRuntimeStarter, run_nbd_with_startup};
use origin_cache_client::OriginCache;
use origin_validation::open_validated_origin;
use platform_admission::{DaemonAction, select_daemon_action};
use proc_swaps::{
    ExactSwapState, canonical_ublk_identity, parse_strict_proc_swaps,
    strict_exact_swap_state_from_text,
};
use ublk_runtime::{UblkDevice, UblkRuntime, UblkServer, prove_ublk_swap_absent};
use unix_listener::{OwnedUnixSocketPath, apply_listen_backlog, bind_owned_unix_listener};
use worker_custody::{ChildWorkerProcess, WorkerProcessControl};
use worker_recovery::{IsolatedWorkerRuntime, WorkerRespawnBudget};
use worker_set::WorkerSet;
use worker_supervisor::{
    IsolatedWorkerSupervisor, WorkerChildHandle, WorkerStopOutcome, stop_worker_process,
};

#[cfg(test)]
use self::{
    args::{DEFAULT_ORIGIN_SIZE, DEFAULT_SIZE, parse_private_listen, validate_slice_flags},
    broker_runtime::{
        BrokerAcceptorStarter, BrokerJoinMonitor, BrokerRuntime, BrokerShutdown,
        BrokerShutdownWake, BrokerWorkerRuntime, bind_broker_listeners,
        broker_setup_with_acceptors, build_broker_config_with_tick, run_broker_ram_with_setup,
        serve_broker_jobs_with_poll, serve_broker_jobs_with_poll_and_reply_hook,
        serve_broker_jobs_with_poll_heartbeat_and_reply_hook,
    },
    cache_admission::compose_cache_admission_with,
    cache_status::{
        OriginCacheStatus, alloc_bound_kib, cache_status_staging_path, hold_kib,
        publish_origin_cache_status, published_cached_kib, write_origin_cache_status,
    },
    control_requests::{
        VsockLeaseInputs, attempt_vsock_lease_with, consume_critical_cache_request,
    },
    critical_devices::{
        collect_device_and_parent, collect_device_and_parent_with, linux_device_number,
        root_mount_source, validate_stable_critical_device_snapshot,
    },
    daemon_dispatch::{arm_spill_pump_at, execute_broker_action_with, execute_nbd_action_with},
    gpu_worker_entry::{
        GpuWorkerEntryHost, ProductionGpuWorkerEntryHost, run_isolated_gpu_worker_entry_with,
    },
    gpu_worker_runtime::UnavailableVramMemory,
    memory_lock::{MemoryLockStatus, memory_lock_status, prctl, relock_resident},
    nbd_origin::run_origin_nbd_with_startup,
    nbd_runtime::{
        NbdBudgetGate, NbdBudgetProvider, NbdBudgetSnapshot, ProductionNbdRuntimeAdapters,
        RecoveryActivation, RecoveryActivationPoll, ResidencyCheckState, choose_residency_reason,
        enforce_reserve_policy_binding, global_free_at_or_above_floor, observe_global_free_floor,
        origin_cache_target_bytes_from_manifest, residency_check, should_log_probe_sample,
        sparse_residency_config, sparse_residency_requests_swapoff,
    },
    origin_identity::read_sealed_origin_manifest,
    origin_validation::{
        OriginValidationHost, ProductionOriginValidationHost, block_identity_value,
        block_identity_value_with, block_metadata_rdev, open_parent_block_device_from_dev_t_at,
        open_validated_origin_with, process_fd_path,
    },
    platform_admission::{select_daemon_action_for, ublk_platform_guard},
    proc_swaps::{
        canonical_nbd_identity, nbd_swap_is_explicitly_absent_from_text, nbd_used_kb_from_text,
    },
    shutdown::ShutdownPermit,
    shutdown_signals::{NbdShutdownBridge, spawn_nbd_shutdown_bridge},
    ublk_runtime::{ProductionUblkRuntime, run_ublk_with_runtime},
    unix_listener::{open_socket_parent, prepare_unix_socket_path, system_max_backlog},
    worker_set::release_cache_after_lease_loss,
    worker_supervisor::wait_for_worker_exit,
};

const GIB: u64 = 1024 * 1024 * 1024;
const BLOCK_SIZE: u32 = 4096;
const UBLK_CONTROL: &str = "/dev/ublk-control";
const ORIGIN_MANIFEST_PATH: &str = "/etc/ramshared/origin.conf";
const HOST_ORIGIN_MANIFEST_PATH: &str =
    "/mnt/c/ProgramData/RamShared/ramshared-origin-manifest.json";
const SECTOR: u64 = 512;

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[ramsharedd] error: {e}");
            std::process::ExitCode::from(1)
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let raw_args = std::env::args().collect::<Vec<_>>();
    run_cli_with(
        &raw_args,
        run_isolated_gpu_worker_entry,
        budget_probe::run,
        crash_record::install_default,
        run_selected_daemon_app,
    )
}

fn run_selected_daemon_app(args: AppArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut runner = ProductionDaemonRunner;
    run_with(
        args,
        &mut runner,
        select_daemon_action,
        || ramshared_wsl2d::pid_file::PidFileGuard::claim().map_err(Into::into),
        |device| {
            let managed_tier = device.to_string();
            crash_record::arm_release_watchdog(move || {
                tier_release::release_managed_tier_on_fatal(&managed_tier, Duration::from_secs(30))
            });
        },
    )
}

fn run_cli_with(
    raw_args: &[String],
    run_worker: impl FnOnce(&[String]) -> Result<(), Box<dyn std::error::Error>>,
    run_budget_probe: impl FnOnce(&[String]) -> Result<(), Box<dyn std::error::Error>>,
    install_crash: impl FnOnce() -> Option<(std::path::PathBuf, Result<(), String>)>,
    run_app: impl FnOnce(AppArgs) -> Result<(), Box<dyn std::error::Error>>,
) -> Result<(), Box<dyn std::error::Error>> {
    if raw_args.get(1).map(String::as_str) == Some("__gpu_worker") {
        return run_worker(&raw_args[2..]);
    }
    if raw_args.get(1).map(String::as_str) == Some("__gpu_budget_probe") {
        return run_budget_probe(&raw_args[2..]);
    }
    if daemon_version_requested(raw_args) {
        println!("ramsharedd {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    // The durable crash record goes in before the daemon touches the GPU, the
    // tier or the NBD socket. When this peer dies, every guest process
    // holding a page on the tier takes a SIGBUS, and the kernel's own line is
    // the only evidence that survives a reboot — this record is what says
    // which fault it was. A failure to install is not fatal: the daemon must
    // still serve.
    match install_crash() {
        Some((path, Ok(()))) => {
            eprintln!("[ramsharedd] crash record: {}", path.display());
        }
        Some((path, Err(error))) => {
            eprintln!(
                "[ramsharedd] crash record unavailable at {}: {error}",
                path.display()
            );
        }
        None => {}
    }
    let args = AppArgs::parse_from(raw_args)?;
    run_app(args)
}

fn run_with<R: DaemonActionRunner, G>(
    args: AppArgs,
    runner: &mut R,
    select: impl FnOnce(AppArgs) -> Result<DaemonAction, Box<dyn std::error::Error>>,
    claim: impl FnOnce() -> Result<G, Box<dyn std::error::Error>>,
    arm: impl FnOnce(&str),
) -> Result<(), Box<dyn std::error::Error>> {
    let action = select(args)?;
    shutdown::run_claimed_action(action, runner, claim, arm)
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
