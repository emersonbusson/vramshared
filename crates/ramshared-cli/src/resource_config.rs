// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config.rs
//! Resource discovery, read-only planning, and user-draft creation.

use std::fmt;
#[allow(unused_imports)]
use std::fs;
#[allow(unused_imports)]
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use ramshared_block::storage_inventory::{
    BlockDevice, LINUX_INVENTORY_OUTPUT_LIMIT, LSBLK_ARGS, MountInfo,
};
use ramshared_config::resource_profile::{
    DEFAULT_RESOURCE_PROFILE_PATH, MAX_RESOURCE_PROFILE_BYTES, PlannedTierCaps,
    RESOURCE_PROFILE_SCHEMA_VERSION, ResourcePlatform, ResourceProfile, ResourceTarget,
    StorageVolumeIdentity,
};
use serde::Serialize;

pub(crate) mod assistant;
pub(crate) mod assistant_ui;
pub(crate) mod benchmark;
mod draft;
mod editor_apply;
mod editor_save;
#[cfg(test)]
mod emergency_tests;
mod gpu_inventory;
mod inventory;
pub(crate) mod kernel_capabilities;
/// Pure Linux swap apply/activate/cleanup policy. The privileged helper
/// `scripts/linux/ramshared-resource-config-helper` executes the mutations;
/// this module decides whether they may proceed.
pub(crate) mod linux;
mod linux_storage;
mod native_origin;
pub(crate) mod native_preparation;
mod plan;
pub(crate) mod prepared_origin;
pub(crate) mod recommended;
mod render_ui;
mod tier_editor;

pub(crate) use benchmark::{BenchmarkAction, BenchmarkRequest, BenchmarkUseCase};
#[allow(unused_imports)]
pub(in crate::resource_config) use draft::{
    draft_volume_candidates, draft_wsl_target_path_is_valid, materialize_draft_target,
    resolve_user_draft_path, run_draft_wizard, run_draft_wizard_with_io, save_profile_draft,
};
pub(crate) use gpu_inventory::driver_profile_key as driver_gpu_profile_key;
pub(crate) use inventory::{collect_snapshot, unix_millis};
#[allow(unused_imports)]
pub(in crate::resource_config) use inventory::{sample_is_fresh, storage_eligibility};
#[allow(unused_imports)]
pub(in crate::resource_config) use plan::{
    build_resource_plan, profile_platform, render_plan_json, render_plan_text,
    resource_volume_identity, windows_target_free_bytes, windows_volume_eligibility,
};
pub(crate) use plan::{load_profile_text, profile_digest};
#[allow(unused_imports)]
pub(in crate::resource_config) use render_ui::{
    draw_config_frame, format_cap_mib, format_gib, render_json, render_text, run_interactive,
    run_interactive_profile,
};

pub(in crate::resource_config) const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
pub(in crate::resource_config) const WINDOWS_INVENTORY_OUTPUT_LIMIT: usize = 256 * 1024;
pub(in crate::resource_config) const STORAGE_SAMPLE_MAX_AGE_MS: u64 = 30_000;

/// The one configured profile directory.
///
/// `pub(crate)` so [`crate::customer::setup_command`] and [`crate::setup`] name
/// the same root `config plan` already loads from. There is exactly one profile
/// source (DT-2): a second root would be a second authority.
pub(crate) fn default_configuration_root() -> PathBuf {
    // Split the compile-time constant once so the root and the file stem share
    // one string. A constant without a separator yields an empty root, which
    // the profile-name confinement then refuses rather than inventing a path.
    let (root, _) = DEFAULT_RESOURCE_PROFILE_PATH
        .rsplit_once('/')
        .unwrap_or(("", DEFAULT_RESOURCE_PROFILE_PATH));
    PathBuf::from(root)
}
pub(in crate::resource_config) const STORAGE_SAMPLE_FUTURE_TOLERANCE_MS: u64 = 5_000;
pub(in crate::resource_config) const MAX_DRAFT_LINE_BYTES: usize = 128;
pub(in crate::resource_config) const MAX_DRAFT_TARGETS: usize = 16;
pub(in crate::resource_config) const BYTES_PER_MIB: u64 = 1024 * 1024;
pub(in crate::resource_config) const DRAFT_LINUX_SWAP_PATH: &str = "swap/ramshared-fallback.swap";
pub(in crate::resource_config) const DRAFT_LINUX_ORIGIN_PATH: &str = "origin/ramshared-origin.img";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ConfigMode {
    Interactive,
    Edit {
        profile_path: String,
    },
    Show {
        json: bool,
    },
    Draft {
        output_path: String,
    },
    Plan {
        json: bool,
        profile_path: Option<String>,
    },
    Wizard {
        json: bool,
    },
    Benchmark(BenchmarkRequest),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuntimePlatform {
    NativeLinux,
    Wsl2,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(in crate::resource_config) struct MemorySnapshot {
    total_bytes: Option<u64>,
    available_bytes: Option<u64>,
    swap_total_bytes: Option<u64>,
    swap_free_bytes: Option<u64>,
    required_counters_available: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(in crate::resource_config) struct SwapDevice {
    filename: String,
    kind: String,
    size_kib: u64,
    used_kib: u64,
    priority: i32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(in crate::resource_config) struct HostMemorySnapshot {
    total_bytes: Option<u64>,
    free_bytes: Option<u64>,
    committed_bytes: Option<u64>,
    commit_limit_bytes: Option<u64>,
}

impl HostMemorySnapshot {
    fn commit_headroom_bytes(&self) -> Option<u64> {
        self.commit_limit_bytes?.checked_sub(self.committed_bytes?)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(in crate::resource_config) struct WindowsVolume {
    drive_letter: Option<String>,
    label: Option<String>,
    file_system: Option<String>,
    drive_type: String,
    size_bytes: Option<u64>,
    free_bytes: Option<u64>,
    volume_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(in crate::resource_config) struct WslInventory {
    distro: Option<String>,
    vhdx_path: Option<String>,
    drive_letter: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(in crate::resource_config) struct WindowsSnapshot {
    observed_utc: String,
    observed_unix_ms: Option<u64>,
    host_memory: HostMemorySnapshot,
    volumes: Vec<WindowsVolume>,
    wsl: Option<WslInventory>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(in crate::resource_config) struct PlannedTarget {
    kind: &'static str,
    volume_identity: String,
    path: String,
    observed_mount_id: Option<u64>,
    requested_bytes: u64,
    required_free_bytes: u64,
    observed_free_bytes: Option<u64>,
    status: &'static str,
    reason: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::resource_config) struct StorageObservation {
    free_bytes: u64,
    mount_id: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::resource_config) struct DraftVolumeCandidate {
    display: String,
    short_display: String,
    free_bytes: u64,
    observed_unix_ms: u64,
    storage: DraftStorageIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::resource_config) enum DraftStorageIdentity {
    NativeLinux {
        filesystem_uuid: String,
        device_identity: String,
    },
    Wsl2 {
        volume_id: String,
        drive_letter: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::resource_config) enum DraftTargetRole {
    Emergency,
    FallbackSwap,
    RamSharedOrigin,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(in crate::resource_config) enum PlannedPathIdentity {
    Linux {
        filesystem_uuid: String,
        device_identity: String,
        relative_path: String,
    },
    Windows {
        volume_id: String,
        relative_path: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(in crate::resource_config) struct ResourcePlan {
    schema_version: u32,
    platform: RuntimePlatform,
    observed_unix_ms: u64,
    profile_state: &'static str,
    profile_sha256: Option<String>,
    unenforced_planned_caps: Option<PlannedTierCaps>,
    targets: Vec<PlannedTarget>,
    gpu_budget_status: String,
    warnings: Vec<String>,
    writes_performed: bool,
    apply_enabled: bool,
    status: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ResourceSnapshot {
    platform: RuntimePlatform,
    observed_unix_ms: u64,
    guest_memory: MemorySnapshot,
    guest_observed_unix_ms: u64,
    swaps: Vec<SwapDevice>,
    block_devices: Vec<BlockDevice>,
    windows: Option<WindowsSnapshot>,
    gpu_inventory: Vec<gpu_inventory::GpuObservation>,
    gpu_observed_unix_ms: u64,
    gpu_budget_status: String,
    warnings: Vec<String>,
    #[serde(skip)]
    prepared_native_origin: prepared_origin::Observation,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct InventoryError(pub(crate) String);

impl fmt::Display for InventoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

pub(crate) fn run(mode: ConfigMode, stdout: &mut dyn Write, stderr: &mut dyn Write) -> ExitCode {
    let result = match mode {
        ConfigMode::Interactive => run_interactive(),
        ConfigMode::Edit { profile_path } => run_interactive_profile(Some(&profile_path)),
        ConfigMode::Draft { output_path } => run_draft_wizard(&output_path, stdout),
        ConfigMode::Show { json } => collect_snapshot().and_then(|snapshot| {
            let output = if json {
                render_json(&snapshot)?
            } else {
                render_text(&snapshot)
            };
            writeln!(stdout, "{output}")
                .map_err(|error| InventoryError(format!("cannot write config output: {error}")))
        }),
        ConfigMode::Plan { json, profile_path } => (|| {
            let snapshot = collect_snapshot()?;
            let require_root_owned = profile_path.is_none();
            let path = profile_path.map_or_else(
                || PathBuf::from(DEFAULT_RESOURCE_PROFILE_PATH),
                PathBuf::from,
            );
            let profile_text = load_profile_text(&path, require_root_owned)?;
            if !require_root_owned && profile_text.is_none() {
                return Err(InventoryError(
                    "requested profile file does not exist".into(),
                ));
            }
            let plan = build_resource_plan(&snapshot, profile_text.as_deref())?;
            let output = if json {
                render_plan_json(&plan)?
            } else {
                render_plan_text(&plan)
            };
            writeln!(stdout, "{output}")
                .map_err(|error| InventoryError(format!("cannot write resource plan: {error}")))
        })(),
        ConfigMode::Wizard { json } => assistant_ui::run_wizard(json, stdout),
        ConfigMode::Benchmark(request) => benchmark::run(request, stdout),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(stderr, "resource config failed: {error}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
#[cfg(test)]
#[path = "resource_config/tests.rs"]
mod tests;
