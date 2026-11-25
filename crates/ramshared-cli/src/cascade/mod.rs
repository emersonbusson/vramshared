// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/mod.rs
//! Orchestration of the zram→VRAM→VHDX cascade (SPEC §6.2–6.4). Runs as root.
//!
//! **Anti-hang contract (invariant checkWSL freeze):**
//! 1. **Never** kill `ramsharedd` while any managed swap (nbd/ublk/zram) is still
//!    listed in `/proc/swaps` — that creates ghost `(deleted)` swap entries and freezes WSL.
//! 2. **Always** `swapoff` managed devices **before** NBD disconnect / daemon stop.
//! 3. **Refuse** `up` if ghost/deleted or unbound managed swap is present.
//!    Live enumeration is detection-only: zero-used devices are still active and
//!    never authorize recovery without an exact sealed lifecycle binding.
//! 4. **zram** algorithm is best-effort with fallbacks (WSL kernels disagree on `lzo-rle`).
//!
//! Mounts tiers by `swapon` priority and unmounts in reverse order.

mod supervisor_status;
pub(in crate::cascade) use supervisor_status::supervisor_status_shape_is_valid;

pub(in crate::cascade) use ramshared_tier::TierPriorities;
#[allow(unused_imports)]
pub(in crate::cascade) use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
#[allow(unused_imports)]
pub(in crate::cascade) use std::path::Path;
use std::process::Command;
#[allow(unused_imports)]
pub(in crate::cascade) use std::time::{Duration, SystemTime};

/// The daemon's published cache-status document — the one that carries the
/// `gpu_budget` sample. Declared once, in [`crate::published_budget`].
pub(in crate::cascade) use crate::published_budget::{
    CACHE_STATUS_FILE, read_status_document, trusted_budget, trusted_cache_telemetry,
    trusted_origin_cache_io, trusted_utilization,
};
pub(in crate::cascade) use ramshared_vram::{
    GpuBudgetTelemetry, GpuUtilizationTelemetry, WorkerCacheTelemetry,
};

#[cfg(test)]
use std::cell::RefCell;
#[cfg(test)]
use std::collections::VecDeque;

// Test seams (no process-global env — avoids unsafe set_var under clippy deny).
#[cfg(test)]
thread_local! {
    static SH_SCRIPT: RefCell<VecDeque<(String, Result<String, String>)>> =
        const { RefCell::new(VecDeque::new()) };
    static TEST_SWAPS: RefCell<Option<String>> = const { RefCell::new(None) };
    static TEST_SWAPS_SEQUENCE: RefCell<VecDeque<Result<String, String>>> =
        const { RefCell::new(VecDeque::new()) };
    static TEST_SWAPS_ERROR: RefCell<Option<String>> = const { RefCell::new(None) };
    static TEST_ORIGIN_CONFIG: RefCell<Option<Result<String, String>>> = const { RefCell::new(None) };
    static TEST_CASCADE_CONF: RefCell<Option<Result<String, String>>> = const { RefCell::new(None) };
    static TEST_MEM_AVAILABLE: RefCell<Option<u64>> = const { RefCell::new(None) };
    static TEST_ENV_MB: RefCell<Option<(String, u64)>> = const { RefCell::new(None) };
}

pub(in crate::cascade) const SOCK: &str = "/run/ramshared/wsl2d.sock";
pub(in crate::cascade) const NBD: &str = "/dev/nbd0";
pub(in crate::cascade) const ZRAM_DEV_FILE: &str = "/run/ramshared/zram-dev";
pub(in crate::cascade) const SWAP_DEV_FILE: &str = "/run/ramshared/swap-dev";
pub(in crate::cascade) const PID_FILE: &str = "/run/ramshared/ramsharedd.pid";
/// Daemon demote counters for status --json (written by ramsharedd).
pub(in crate::cascade) const DEMOTE_STATUS_FILE: &str = "/run/ramshared/demote-status.json";
pub(in crate::cascade) const CAPACITY_STATUS_FILE: &str = "/run/ramshared/capacity-guaranteed";
pub(crate) const ORIGIN_CONFIG_FILE: &str = "/etc/ramshared/origin.conf";
/// Largest sealed origin manifest this reader will accept. The bound is this
/// reader's own (it predates the descriptor fix); it is named here so the
/// check and the read enforce the same number.
pub(in crate::cascade) const SEALED_ORIGIN_CONFIG_MAX_BYTES: u64 = 65_536;
/// Host cascade sizing policy (DT-3), shared with `boot` and `resource_config`.
pub(crate) const CASCADE_CONF_FILE: &str = "/etc/ramshared/cascade.conf";
pub(in crate::cascade) const DEFAULT_PHYSICAL_CACHE_CAP_MIB: u64 = 1024;
pub(in crate::cascade) const SUPERVISOR_STATUS_FILE: &str = "/run/ramshared/supervisor-state.json";
pub(in crate::cascade) const CONTROL_STATUS_MAX_AGE_MS: u64 = 15_000;
pub(in crate::cascade) const SUPERVISOR_ACTION_ERROR_MAX_BYTES: usize = 128 * 1024;
pub(in crate::cascade) const SAFE_MODE_FILE: &str = "/var/lib/ramshared/safe-mode.json";
// Guardian health/status locations are derived from the sealed selected-distro
// identity (`sealed_identity`), never from a hardcoded distro name (AUD-40).
/// Forensic "armed" marker (survives WSL death if under /mnt/c).
pub(in crate::cascade) const ARMED_MARKER_CANDIDATES: &[&str] =
    &["/mnt/c/wsl-forensics/.armed", "/run/ramshared/.armed"];

/// zram compressors in preference order. This list answers "which compressor
/// does the product want", not "what may we fall back to on any error": the
/// walk advances only when the kernel does not offer the current name
/// (`ZramAllocationFailure::AlgorithmUnsupported` in `cascade_io`).
pub(in crate::cascade) const ZRAM_ALGOS: &[&str] = &["lzo-rle", "lzo", "zstd", "lz4", "deflate"];

#[derive(Debug)]
pub enum CascadeError {
    Shell { cmd: String, msg: String },
    Arg(String),
    Io(String),
    Precondition(String),
    UnsafeContainment(String),
}

impl fmt::Display for CascadeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CascadeError::Shell { cmd, msg } => write!(f, "command `{cmd}` failed: {msg}"),
            CascadeError::Arg(m) => write!(f, "invalid argument: {m}"),
            CascadeError::Io(m) => write!(f, "I/O: {m}"),
            CascadeError::Precondition(m) => write!(f, "{m}"),
            CascadeError::UnsafeContainment(m) => write!(f, "unsafe containment: {m}"),
        }
    }
}

impl std::error::Error for CascadeError {}

pub(in crate::cascade) fn sh(cmd: &str, args: &[&str]) -> Result<String, CascadeError> {
    #[cfg(test)]
    {
        let scripted = SH_SCRIPT.with(|q| {
            let mut q = q.borrow_mut();
            // Match by command name, then by full "cmd arg0", then wildcard "*"
            let full = format!("{cmd} {}", args.join(" "));
            if let Some(i) = q.iter().position(|(p, _)| {
                p == cmd || p == &full || p == "*" || full.starts_with(p.as_str())
            }) {
                return q.remove(i);
            }
            None
        });
        if let Some((_pat, res)) = scripted {
            return res.map_err(|msg| CascadeError::Shell {
                cmd: format!("{cmd} {}", args.join(" ")),
                msg,
            });
        }
    }
    let out = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| CascadeError::Shell {
            cmd: cmd.to_string(),
            msg: e.to_string(),
        })?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(CascadeError::Shell {
            cmd: format!("{cmd} {}", args.join(" ")),
            msg: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }
}

pub(in crate::cascade) fn mem_available_bytes() -> u64 {
    #[cfg(test)]
    if let Some(n) = TEST_MEM_AVAILABLE.with(|c| *c.borrow()) {
        return n;
    }
    fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("MemAvailable:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<u64>().ok())
        })
        .map(|kib| kib * 1024)
        .unwrap_or(0)
}

/// Sealed selected-distro identity (AUD-40). One identity determines every
/// runtime health/status/origin location; the host-sealed config is the only
/// authority.
mod sealed_identity;

mod lifecycle;
pub(in crate::cascade) use crate::release_coherence::observe_release_coherence;
pub(in crate::cascade) use lifecycle::{
    CacheState, CascadeSnapshot, ControlState, DemoteSnapshot, GuardianState, LifecycleView,
    OriginState, TierSample, TopologyFacts, active_threshold_kib_from_env, derive_lifecycle,
    fallback_swap_occupancy_kib, overall_state, protection_reason, protection_state,
    render_status_json, status_ok,
};

/// Customer-mutation custody admission shared by every cascade mutation
/// entrypoint (ITEM-6). Nested lifecycle calls borrow the already held guard.
mod mutation_guard;

pub mod boot;
mod preparation;
mod preparation_process;
mod relay_health;

/// The posture decision bound to the activation token (SPEC
/// wsl2-transparent-tier-service DT-1..DT-3). Pure: no device is touched.
pub(crate) mod posture;

/// Bounded readers for the runtime health documents and the additive
/// `status --json` fragments (SPEC wsl2-transparent-tier-service ITEM-7).
mod health_documents;

/// Verified root-published non-secret status summary for unprivileged `status`/`top` readers.
pub(crate) mod root_status_cache;

/// The boot-policy enablement effect (ITEM-6). Consumes the decided
/// [`crate::customer::service::BootPolicyPlan`] and changes only what a future
/// boot may do; it never stops a live tier.
pub(crate) mod boot_policy_effect;

/// The systemd process boundary behind [`boot_policy_effect::BootPolicyHost`].
/// Spawn only; the live proof is the attended host install.
pub(crate) mod boot_policy_host;

mod swaps;
#[allow(unused_imports)]
pub use swaps::is_ramshared_logical_path;
#[allow(unused_imports)]
pub(in crate::cascade) use swaps::{
    LegacyMigrationPlan, OrphanPlan, active_vram_block_swap, chrono_like_now, default_daemon,
    disk_swap_used_kib, diskstats_name_for_swap_path, has_device_path_shape,
    is_allowlisted_managed_path, is_fallback_swap_entry, is_ramshared_mapper_path,
    lower_tier_free_bytes, numbered_device_basename, plan_legacy_migration, plan_orphan_action,
    read_swaps, refuse_ghost_swap_state, refuse_unbound_managed_devices,
};
pub use swaps::{
    SwapEntry, SwapTierRole, canonicalize_swap_path, daemon_kill_allowed, diskstats_role_name,
    dm_name_aliases, fallback_swap_device_names, ghost_vram_swaps, is_ghost_swap_filename,
    parse_proc_swaps, swap_tier_role,
};
pub(crate) use swaps::{is_nbd_device_path, is_ublk_device_path, is_zram_device_path};

mod up_config;
pub use up_config::cascade_already_healthy;
#[cfg(test)]
pub(crate) use up_config::parse_up_args_verdict_for_test;
#[allow(unused_imports)]
pub(in crate::cascade) use up_config::{
    Transport, UpArgs, UpEnv, check_transport, check_transport_for, default_zram_mb,
    ensure_single_ring_ublk, is_wsl2, nbd_admission_notice, nbd_refusal, parse_up_args_from,
    refuse_half_cascade, resolve_transport, ublk_refusal,
};

mod sealed_origin;
pub(crate) use sealed_origin::cache_status_is_current_daemon;
#[allow(unused_imports)]
pub(in crate::cascade) use sealed_origin::{
    SealedOriginConfig, cache_status_matches_current_daemon, cache_status_shape_is_valid,
    canonical_device_number, canonical_origin_uuid, canonical_sha256,
    control_plane_status_freshness, daemon_instance_id_from_pid, guardian_state_from_files,
    origin_partuuid, parse_sealed_origin_config, process_start_ticks, proof_is_fresh,
    read_sealed_origin_config, read_sealed_origin_config_from, sealed_distro_or_unknown,
    sealed_guardian_state, sealed_guardian_state_from, sealed_origin_document_trust,
    supervisor_status_matches_current_daemon, unix_time_ms, validate_origin_logical_capacity,
};
#[cfg(test)]
#[allow(unused_imports)]
pub(in crate::cascade) use sealed_origin::{
    control_plane_status_is_current, sealed_origin_test_fixture,
};

mod snapshot;
#[allow(unused_imports)]
pub(in crate::cascade) use snapshot::{
    PressureGate, alloc_bound_kib, budget_record_and_headroom, cache_event_counters, cache_record,
    cache_state_from_status, cache_unavailable_reason_from_status, device_basename,
    gpu_budget_telemetry_invalid_or_stale, gpu_cache_telemetry_invalid_or_stale,
    gpu_utilization_telemetry_invalid_or_stale, pressure_gate_from_snapshot, pressure_refusals,
    stress_continuation_from_snapshot, stress_readiness_from_snapshot, topology_facts,
    utilization_record,
};
pub use snapshot::{build_cascade_snapshot, stress_continuation, stress_readiness};

mod status_view;
pub use status_view::status;
#[allow(unused_imports)]
pub(in crate::cascade) use status_view::{
    cache_line, capacity_field, capacity_field_u64, capacity_guaranteed_from_stat,
    daemon_alive_pid, daemon_alive_pid_from, observe_capacity_guaranteed, overall_line,
    parse_demote_status_file, print_tier, read_capacity_disk_baseline, read_demote_snapshot,
    status_timestamp,
};
pub(crate) use status_view::{
    current_daemon_instance_id, preparation_is_off, status_json_document,
};

mod cascade_io;
pub use boot::{BootConfig, BootError};
pub use cascade_io::{
    down, down_plan, down_recover_released_peer, ensure_sealed_guardian_running,
    ensure_sealed_origin_attached, migrate_legacy_cascade, parse_active_zram_algorithm,
    recover_nbd_peer, up_with_args,
};
pub(crate) use cascade_io::{
    down_quiet, evaluate_live_down_plan, observe_leased_host_memory, swap_rebalance_apply,
    swap_rebalance_plan,
};

#[cfg(test)]
mod tests;
