// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/assistant.rs
//! Unified Setup & Configuration Assistant model, live observer, atomic
//! configuration writers, and text/JSON renderers.
//!

use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use rustix::fs::{Mode, OFlags, openat};
use serde::Serialize;

use super::kernel_capabilities::{
    self, CapabilityState, KernelCapabilities, MglruState, UblkState,
};
use super::{InventoryError, ResourceSnapshot, RuntimePlatform};
use crate::cascade::CASCADE_CONF_FILE;

pub(crate) const AUTO_UPDATE_CONF_FILE: &str = "/etc/ramshared/auto-update.conf";
pub(crate) const WIZARD_COMMAND: &str = "ramshared config wizard";
const MAX_CONF_READ_BYTES: usize = 64 * 1024;
const DEFAULT_BUNDLE_ROOT: &str = "/opt/ramshared/bundles";
const DEFAULT_RELEASE_KEYRING: &str = "/etc/ramshared/release-keyring.gpg";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ChecklistItemId {
    SealedInstall,
    ThreeTierConfig,
    KernelOptimizations,
    Tier2Transport,
    GpuAndControlPlane,
    BootAndAutoUpdate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ItemStatus {
    Ok,
    Pending,
    Optional,
    Attention,
    Unavailable,
}

impl ItemStatus {
    pub(crate) const fn badge(self) -> &'static str {
        match self {
            Self::Ok => "[OK]",
            Self::Pending => "[PENDING]",
            Self::Optional => "[OPTIONAL]",
            Self::Attention => "[ATTENTION]",
            Self::Unavailable => "[UNAVAILABLE]",
        }
    }

    pub(crate) const fn counts_as_pending(self) -> bool {
        matches!(self, Self::Pending | Self::Optional | Self::Attention)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ChecklistItem {
    pub(crate) id: ChecklistItemId,
    pub(crate) title: &'static str,
    pub(crate) status: ItemStatus,
    pub(crate) summary: String,
    pub(crate) detail: String,
    pub(crate) action_hint: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub(crate) struct AutoUpdateFileState {
    pub(crate) present: bool,
    pub(crate) enabled: Option<bool>,
    pub(crate) lower_sink: Option<String>,
    pub(crate) bundle_root: Option<String>,
    pub(crate) release_keyring: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AssistantObservation {
    pub(crate) installed_release: Option<String>,
    pub(crate) cascade_conf_present: bool,
    pub(crate) cascade_conf_summary: Option<String>,
    pub(crate) gpu_count: usize,
    pub(crate) gpu_cache_active: Option<bool>,
    pub(crate) control_plane_state: Option<String>,
    pub(crate) cascade_service_enabled: Option<bool>,
    pub(crate) auto_update_timer_enabled: Option<bool>,
    pub(crate) auto_update_conf: AutoUpdateFileState,
    pub(crate) capabilities: KernelCapabilities,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct AssistantChecklist {
    pub(crate) items: Vec<ChecklistItem>,
    pub(crate) ok_count: usize,
    pub(crate) pending_count: usize,
    pub(crate) capabilities: KernelCapabilities,
    pub(crate) recommended_command: &'static str,
}

pub(crate) fn build_checklist(obs: &AssistantObservation) -> AssistantChecklist {
    let items = vec![
        build_sealed_install_item(obs),
        build_three_tier_item(obs),
        build_kernel_item(obs),
        build_transport_item(obs),
        build_gpu_item(obs),
        build_boot_and_update_item(obs),
    ];
    let ok_count = items.iter().filter(|i| i.status == ItemStatus::Ok).count();
    let pending_count = items
        .iter()
        .filter(|i| i.status.counts_as_pending())
        .count();
    AssistantChecklist {
        items,
        ok_count,
        pending_count,
        capabilities: obs.capabilities.clone(),
        recommended_command: WIZARD_COMMAND,
    }
}

fn build_sealed_install_item(obs: &AssistantObservation) -> ChecklistItem {
    if let Some(release) = obs.installed_release.as_deref() {
        ChecklistItem {
            id: ChecklistItemId::SealedInstall,
            title: "Sealed installation & release identity",
            status: ItemStatus::Ok,
            summary: format!("Installed sealed release {release}"),
            detail: "Release selector /opt/ramshared/current is present and versioned.".into(),
            action_hint: "No action required.".into(),
        }
    } else {
        ChecklistItem {
            id: ChecklistItemId::SealedInstall,
            title: "Sealed installation & release identity",
            status: ItemStatus::Pending,
            summary: "No sealed release found at /opt/ramshared/current".into(),
            detail: "Install a verified release bundle with `ramshared install` to enable managed units and auto-update.".into(),
            action_hint: "Run `sudo ramshared install --plan` against a signed release bundle.".into(),
        }
    }
}

fn build_three_tier_item(obs: &AssistantObservation) -> ChecklistItem {
    if obs.cascade_conf_present {
        ChecklistItem {
            id: ChecklistItemId::ThreeTierConfig,
            title: "3-Tier sizing (/etc/ramshared/cascade.conf)",
            status: ItemStatus::Ok,
            summary: obs
                .cascade_conf_summary
                .clone()
                .unwrap_or_else(|| "Configured at /etc/ramshared/cascade.conf".into()),
            detail: "All 3 tiers (ZRAM -> Origin/VRAM -> Fallback) use explicit sizing policy."
                .into(),
            action_hint:
                "Press `e` to fine-tune bars or `a` to apply fresh hardware-recommended values."
                    .into(),
        }
    } else {
        ChecklistItem {
            id: ChecklistItemId::ThreeTierConfig,
            title: "3-Tier sizing (/etc/ramshared/cascade.conf)",
            status: ItemStatus::Pending,
            summary: "Missing /etc/ramshared/cascade.conf (running on conservative built-in defaults)".into(),
            detail: "Generating cascade.conf tailors Tier 1 ZRAM, Tier 2 VRAM/Origin, and host VRAM reserve to this machine.".into(),
            action_hint: "Press `a` to preview and atomically write recommended 3-tier sizing.".into(),
        }
    }
}

fn build_kernel_item(obs: &AssistantObservation) -> ChecklistItem {
    let caps = &obs.capabilities;
    if caps.kernel_release.is_none() && caps.zram_writeback == CapabilityState::Unknown {
        return ChecklistItem {
            id: ChecklistItemId::KernelOptimizations,
            title: "Kernel optimizations (ACTIME, ZRAM multi-comp, MGLRU)",
            status: ItemStatus::Unavailable,
            summary: "Kernel capability probes unavailable on this surface".into(),
            detail: "Neither /proc/version nor /sys/devices/virtual/block/zram* was readable."
                .into(),
            action_hint: "Run on a Linux/WSL2 host with /proc and /sys mounted.".into(),
        };
    }
    if caps.kernel_gaps.is_empty() && caps.zram_writeback == CapabilityState::Supported {
        ChecklistItem {
            id: ChecklistItemId::KernelOptimizations,
            title: "Kernel optimizations (ACTIME, ZRAM multi-comp, MGLRU)",
            status: ItemStatus::Ok,
            summary: format!(
                "Optimal ({}; aging={}, recomp={})",
                caps.kernel_release.as_deref().unwrap_or("active kernel"),
                caps.adaptive_plan.spill_aging_mode,
                caps.adaptive_plan
                    .secondary_recomp_algo
                    .as_deref()
                    .unwrap_or("none")
            ),
            detail: "Kernel exposes writeback, ACTIME, multi-comp zstd, MGLRU, and PSI.".into(),
            action_hint: "No kernel action required.".into(),
        }
    } else if caps.zram_writeback == CapabilityState::Unsupported {
        ChecklistItem {
            id: ChecklistItemId::KernelOptimizations,
            title: "Kernel optimizations (ACTIME, ZRAM multi-comp, MGLRU)",
            status: ItemStatus::Attention,
            summary: "CONFIG_ZRAM_WRITEBACK is disabled; Tier 1 -> Tier 2 writeback unavailable"
                .into(),
            detail: format_gaps_detail(&caps.kernel_gaps),
            action_hint: "Press `g` to view the exact safe custom kernel build & install steps."
                .into(),
        }
    } else {
        let missing_names: Vec<&str> = caps
            .kernel_gaps
            .iter()
            .map(|gap| gap.config_symbol)
            .collect();
        let summary = if missing_names.is_empty() {
            format!(
                "Compatible mode (aging={}, MGLRU={})",
                caps.adaptive_plan.spill_aging_mode,
                mglru_short(caps.mglru)
            )
        } else {
            format!(
                "Compatible mode; {} optional kernel upgrade(s) available",
                missing_names.len()
            )
        };
        ChecklistItem {
            id: ChecklistItemId::KernelOptimizations,
            title: "Kernel optimizations (ACTIME, ZRAM multi-comp, MGLRU)",
            status: ItemStatus::Optional,
            summary,
            detail: format_gaps_detail(&caps.kernel_gaps),
            action_hint:
                "Press `g` for guided kernel upgrade steps (safe manual WSL restart; never forced)."
                    .into(),
        }
    }
}

fn build_transport_item(obs: &AssistantObservation) -> ChecklistItem {
    let caps = &obs.capabilities;
    let status = if caps.nbd_present {
        ItemStatus::Ok
    } else {
        ItemStatus::Attention
    };
    let summary = match caps.ublk {
        UblkState::PresentLabGatedWsl2 => {
            "Active transport: NBD (ublk present in kernel, lab-gated on WSL2)".into()
        }
        UblkState::PresentNativeCandidate => {
            "Active transport: NBD (ublk kernel module detected for future native SPEC)".into()
        }
        UblkState::Absent if caps.nbd_present => {
            "Active transport: NBD (qualified product transport)".into()
        }
        UblkState::Absent => "Neither NBD nor ublk kernel module detected".into(),
    };
    ChecklistItem {
        id: ChecklistItemId::Tier2Transport,
        title: "Tier 2 transport policy (NBD / ublk)",
        status,
        summary,
        detail: caps.adaptive_plan.transport_note.to_string(),
        action_hint: "NBD is automatically selected; ublk stays fail-closed on WSL2 until isolated lab closure.".into(),
    }
}

fn build_gpu_item(obs: &AssistantObservation) -> ChecklistItem {
    if obs.gpu_count == 0 {
        return ChecklistItem {
            id: ChecklistItemId::GpuAndControlPlane,
            title: "GPU VRAM cache & host control plane",
            status: ItemStatus::Optional,
            summary: "No GPU adapter detected; running in durable origin-only mode".into(),
            detail:
                "Tier 1 ZRAM + Tier 2 SSD origin + Tier 3 fallback remain functional without a GPU."
                    .into(),
            action_hint: "Verify GPU drivers / /dev/dxg if you expect a discrete GPU on this host."
                .into(),
        };
    }
    match (obs.gpu_cache_active, obs.control_plane_state.as_deref()) {
        (Some(true), Some(cp)) if cp == "vsock_leased" => ChecklistItem {
            id: ChecklistItemId::GpuAndControlPlane,
            title: "GPU VRAM cache & host control plane",
            status: ItemStatus::Ok,
            summary: format!("{} GPU(s) detected; VRAM cache ACTIVE ({cp})", obs.gpu_count),
            detail: "Host Windows service lease and isolated GPU cache worker are healthy.".into(),
            action_hint: "No action required.".into(),
        },
        (Some(false), Some(cp)) => ChecklistItem {
            id: ChecklistItemId::GpuAndControlPlane,
            title: "GPU VRAM cache & host control plane",
            status: ItemStatus::Attention,
            summary: format!("{} GPU(s) detected; cache inactive (control plane: {cp})", obs.gpu_count),
            detail: "Check RamSharedWinSvc on Windows and `ramshared status` for cache refusal reason.".into(),
            action_hint: "Run `ramshared doctor` or verify `RamSharedWinSvc` is Running on Windows.".into(),
        },
        _ => ChecklistItem {
            id: ChecklistItemId::GpuAndControlPlane,
            title: "GPU VRAM cache & host control plane",
            status: ItemStatus::Optional,
            summary: format!("{} GPU(s) detected; live cache telemetry not yet observed", obs.gpu_count),
            detail: "GPU VRAM acceleration activates automatically when the cascade and host lease are up.".into(),
            action_hint: "Start the cascade or verify `ramshared status` after activation.".into(),
        },
    }
}

fn build_boot_and_update_item(obs: &AssistantObservation) -> ChecklistItem {
    let auto_enabled = obs.auto_update_conf.enabled == Some(true);
    let boot_enabled = obs.cascade_service_enabled == Some(true);
    let timer_enabled = obs.auto_update_timer_enabled == Some(true);

    if auto_enabled && boot_enabled && timer_enabled {
        ChecklistItem {
            id: ChecklistItemId::BootAndAutoUpdate,
            title: "Auto-boot & signed auto-update",
            status: ItemStatus::Ok,
            summary: "Auto-boot enrolled and OpenPGP auto-update enabled".into(),
            detail: "ramshared-cascade.service and ramshared-auto-update.timer are enabled.".into(),
            action_hint: "Press `u` to toggle /etc/ramshared/auto-update.conf enabled=1|0.".into(),
        }
    } else if !auto_enabled {
        ChecklistItem {
            id: ChecklistItemId::BootAndAutoUpdate,
            title: "Auto-boot & signed auto-update",
            status: ItemStatus::Pending,
            summary: format!(
                "Auto-update disabled/missing (boot={}, timer={})",
                bool_opt(obs.cascade_service_enabled),
                bool_opt(obs.auto_update_timer_enabled)
            ),
            detail: "Enabling /etc/ramshared/auto-update.conf lets verified signed bundles upgrade safely when drain headroom allows.".into(),
            action_hint: "Press `u` to enable /etc/ramshared/auto-update.conf atomically.".into(),
        }
    } else {
        ChecklistItem {
            id: ChecklistItemId::BootAndAutoUpdate,
            title: "Auto-boot & signed auto-update",
            status: ItemStatus::Optional,
            summary: format!(
                "Auto-update conf enabled; systemd boot={}, timer={}",
                bool_opt(obs.cascade_service_enabled),
                bool_opt(obs.auto_update_timer_enabled)
            ),
            detail: "Use `sudo ramshared service enable` to enroll automatic start at boot.".into(),
            action_hint:
                "Press `u` to adjust auto-update.conf or run `sudo ramshared service status`."
                    .into(),
        }
    }
}

fn mglru_short(state: MglruState) -> &'static str {
    match state {
        MglruState::Enabled(_) => "enabled",
        MglruState::Disabled => "disabled",
        MglruState::Absent => "absent",
        MglruState::Unknown => "unknown",
    }
}

fn bool_opt(v: Option<bool>) -> &'static str {
    match v {
        Some(true) => "on",
        Some(false) => "off",
        None => "unknown",
    }
}

fn format_gaps_detail(gaps: &[kernel_capabilities::KernelGap]) -> String {
    if gaps.is_empty() {
        return "All probed kernel capabilities are active or running in compatible fallback."
            .into();
    }
    let mut out = String::from("Missing optional kernel configs:");
    for gap in gaps {
        let _ = write!(out, " {} ({});", gap.config_symbol, gap.reason);
    }
    out
}

pub(crate) fn observe_live(snapshot: &ResourceSnapshot) -> AssistantObservation {
    let installed_release =
        read_text_no_follow(Path::new("/opt/ramshared/current/RELEASE_VERSION"))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
    let cascade_text = read_text_no_follow(Path::new(CASCADE_CONF_FILE));
    let cascade_conf_present = cascade_text.is_some();
    let cascade_conf_summary = cascade_text.as_deref().map(summarize_cascade_conf);
    let auto_update_conf = read_text_no_follow(Path::new(AUTO_UPDATE_CONF_FILE))
        .map(|text| parse_auto_update_conf(&text))
        .unwrap_or_default();
    let (gpu_cache_active, control_plane_state) = read_root_status_cache_hints();
    let cascade_service_enabled = Some(
        Path::new("/etc/systemd/system/multi-user.target.wants/ramshared-cascade.service").exists(),
    );
    let auto_update_timer_enabled = Some(
        Path::new("/etc/systemd/system/timers.target.wants/ramshared-auto-update.timer").exists(),
    );
    let capabilities = kernel_capabilities::probe_live(snapshot.platform);

    AssistantObservation {
        installed_release,
        cascade_conf_present,
        cascade_conf_summary,
        gpu_count: snapshot.gpu_inventory.len(),
        gpu_cache_active,
        control_plane_state,
        cascade_service_enabled,
        auto_update_timer_enabled,
        auto_update_conf,
        capabilities,
    }
}

fn read_root_status_cache_hints() -> (Option<bool>, Option<String>) {
    for candidate in [
        "/run/ramshared/cache-status.json",
        "/var/run/ramshared/cache-status.json",
    ] {
        let Some(text) = read_text_no_follow(Path::new(candidate)) else {
            continue;
        };
        let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let cache = val
            .get("cache_state")
            .and_then(serde_json::Value::as_str)
            .map(|s| s.eq_ignore_ascii_case("ACTIVE"));
        let cp = val
            .get("control_plane_state")
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string);
        if cache.is_some() || cp.is_some() {
            return (cache, cp);
        }
    }
    (None, None)
}

fn summarize_cascade_conf(text: &str) -> String {
    let mut zram = None;
    let mut vram = None;
    let mut headroom = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once('=') {
            match k.trim() {
                "ZRAM_MIB" => zram = Some(v.trim()),
                "VRAM_MIB" => vram = Some(v.trim()),
                "MIN_VRAM_HEADROOM_MIB" => headroom = Some(v.trim()),
                _ => {}
            }
        }
    }
    format!(
        "ZRAM={} MiB · Origin/VRAM={} MiB · Host reserve={} MiB",
        zram.unwrap_or("default"),
        vram.unwrap_or("default"),
        headroom.unwrap_or("default")
    )
}

pub(crate) fn parse_auto_update_conf(text: &str) -> AutoUpdateFileState {
    let mut state = AutoUpdateFileState {
        present: true,
        ..AutoUpdateFileState::default()
    };
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, val)) = trimmed.split_once('=') else {
            continue;
        };
        let v = val.trim();
        match key.trim() {
            "enabled" => {
                state.enabled = match v {
                    "1" => Some(true),
                    "0" => Some(false),
                    _ => None,
                };
            }
            "lower_sink" if !v.is_empty() => state.lower_sink = Some(v.to_string()),
            "bundle_root" if !v.is_empty() => state.bundle_root = Some(v.to_string()),
            "release_keyring" if !v.is_empty() => state.release_keyring = Some(v.to_string()),
            _ => {}
        }
    }
    state
}

pub(crate) fn render_auto_update_conf(existing: &AutoUpdateFileState, enabled: bool) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "enabled={}", if enabled { 1 } else { 0 });
    if let Some(sink) = existing.lower_sink.as_deref() {
        let _ = writeln!(out, "lower_sink={sink}");
    }
    let bundle_root = existing
        .bundle_root
        .as_deref()
        .unwrap_or(DEFAULT_BUNDLE_ROOT);
    let _ = writeln!(out, "bundle_root={bundle_root}");
    let keyring = existing
        .release_keyring
        .as_deref()
        .unwrap_or(DEFAULT_RELEASE_KEYRING);
    let _ = writeln!(out, "release_keyring={keyring}");
    out
}

/// Atomically write `/etc/ramshared/auto-update.conf`, preserving existing
/// `lower_sink`, `bundle_root`, and `release_keyring` values and refusing
/// symlinked destinations (`O_NOFOLLOW`).
pub(crate) fn write_auto_update_conf_atomic(
    path: &Path,
    enabled: bool,
) -> Result<AutoUpdateFileState, InventoryError> {
    if let Ok(meta) = fs::symlink_metadata(path)
        && meta.file_type().is_symlink()
    {
        return Err(InventoryError(format!(
            "refusing symlinked auto-update config at {}",
            path.display()
        )));
    }
    let existing = read_text_no_follow(path)
        .map(|text| parse_auto_update_conf(&text))
        .unwrap_or_default();
    let body = render_auto_update_conf(&existing, enabled);
    let parent = path.parent().ok_or_else(|| {
        InventoryError(format!(
            "auto-update config path has no parent: {}",
            path.display()
        ))
    })?;
    if fs::symlink_metadata(parent)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(InventoryError(format!(
            "refusing symlinked parent directory {}",
            parent.display()
        )));
    }
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| InventoryError("invalid auto-update config filename".into()))?;
    let tmp_path = parent.join(format!(".{file_name}.tmp.{}", std::process::id()));
    let _ = fs::remove_file(&tmp_path);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .mode(0o644)
        .open(&tmp_path)
        .map_err(|err| {
            InventoryError(format!(
                "cannot create temporary auto-update config {}: {err}",
                tmp_path.display()
            ))
        })?;
    if let Err(err) = file
        .write_all(body.as_bytes())
        .and_then(|()| file.sync_all())
        .and_then(|()| fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o644)))
    {
        let _ = fs::remove_file(&tmp_path);
        return Err(InventoryError(format!(
            "cannot write temporary auto-update config {}: {err}",
            tmp_path.display()
        )));
    }
    drop(file);
    if let Err(err) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(InventoryError(format!(
            "cannot commit {}: {err}",
            path.display()
        )));
    }
    Ok(parse_auto_update_conf(&body))
}

pub(crate) fn render_checklist_text(checklist: &AssistantChecklist) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "RamShared — Unified Setup & Configuration Assistant");
    let _ = writeln!(
        out,
        "Summary: {} OK · {} pending/optional",
        checklist.ok_count, checklist.pending_count
    );
    let _ = writeln!(out);
    for (idx, item) in checklist.items.iter().enumerate() {
        let _ = writeln!(
            out,
            "{:<13} {}. {}",
            item.status.badge(),
            idx + 1,
            item.title
        );
        let _ = writeln!(out, "              {}", item.summary);
        if item.status != ItemStatus::Ok {
            let _ = writeln!(out, "              Next: {}", item.action_hint);
        }
    }
    let _ = writeln!(out);
    out.push_str(&kernel_capabilities::render_capabilities_text(
        &checklist.capabilities,
    ));
    if checklist.pending_count > 0 {
        let _ = writeln!(
            out,
            "\nSingle command to configure pending items interactively: `{}` (or `ramshared setup wizard`)",
            checklist.recommended_command
        );
    }
    out
}

pub(crate) fn render_checklist_json(
    checklist: &AssistantChecklist,
) -> Result<String, InventoryError> {
    serde_json::to_string_pretty(checklist)
        .map_err(|err| InventoryError(format!("cannot serialize assistant checklist: {err}")))
}

pub(crate) fn render_post_install_summary(checklist: &AssistantChecklist) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "NBD_INSTALL_ASSISTANT_OK={} NBD_INSTALL_ASSISTANT_PENDING={}",
        checklist.ok_count, checklist.pending_count
    );
    for item in &checklist.items {
        let _ = writeln!(
            out,
            "  {:<13} {}: {}",
            item.status.badge(),
            item.title,
            item.summary
        );
    }
    if checklist.pending_count > 0 {
        let _ = writeln!(
            out,
            "Next step: run `{}` (or `ramshared setup wizard`) anytime to configure the remaining optional items.",
            checklist.recommended_command
        );
    }
    out
}

pub(crate) fn emit_post_install_assistant_summary_live(stdout: &mut dyn std::io::Write) {
    let Ok(snapshot) = super::collect_snapshot() else {
        return;
    };
    let obs = observe_live(&snapshot);
    let checklist = build_checklist(&obs);
    let _ = write!(stdout, "{}", render_post_install_summary(&checklist));
}

pub(crate) fn kernel_upgrade_guidance_text(caps: &KernelCapabilities) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Kernel Optimization Guidance (Non-Destructive)");
    let _ = writeln!(
        out,
        "Current kernel: {}",
        caps.kernel_release.as_deref().unwrap_or("unknown")
    );
    let _ = writeln!(out);
    if caps.kernel_gaps.is_empty() {
        let _ = writeln!(
            out,
            "All recommended RamShared kernel capabilities are already active."
        );
        return out;
    }
    let _ = writeln!(
        out,
        "Optional capabilities not enabled in the running kernel:"
    );
    for gap in &caps.kernel_gaps {
        let _ = writeln!(out, "  - {} — {}", gap.config_symbol, gap.reason);
    }
    let _ = writeln!(out);
    match caps.platform {
        RuntimePlatform::Wsl2 => {
            let _ = writeln!(out, "To build a custom WSL2 kernel with all optimal flags:");
            let _ = writeln!(out, "  bash scripts/kernel/build-wsl-kernel.sh");
            let _ = writeln!(
                out,
                "Then install the sealed kernel pair and restart WSL manually at your convenience."
            );
            let _ = writeln!(
                out,
                "Safety guarantee: RamShared never triggers `wsl --shutdown` automatically."
            );
        }
        RuntimePlatform::NativeLinux => {
            let _ = writeln!(
                out,
                "Enable the listed CONFIG_* symbols in your Linux kernel config (or load a distro kernel that enables them)."
            );
        }
    }
    out
}

fn read_text_no_follow(path: &Path) -> Option<String> {
    let fd = openat(
        rustix::fs::CWD,
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    let file = File::from(fd);
    let mut buf = Vec::new();
    file.take(MAX_CONF_READ_BYTES.saturating_add(1) as u64)
        .read_to_end(&mut buf)
        .ok()?;
    if buf.len() > MAX_CONF_READ_BYTES {
        return None;
    }
    String::from_utf8(buf).ok()
}

#[cfg(test)]
mod tests;
