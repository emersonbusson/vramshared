// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/recommended.rs
//! Hardware-detected 3-tier recommendation engine and atomic `cascade.conf` writer.
//!
//! Computes safe, non-overcommitted starting ceilings for all three tiers
//! (Tier 1 ZRAM, Tier 2 GPU VRAM + SSD Origin backing, Tier 3 Fallback SSD)
//! from fresh live hardware observations, and can atomically materialize
//! `/etc/ramshared/cascade.conf` when absent on install or explicitly confirmed
//! by the operator in `ramshared config`.

use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use ramshared_config::resource_profile::DISK_RESERVE_FLOOR_BYTES;
use serde::Serialize;

use super::gpu_inventory::{self, GpuObservation, Preset};
use super::{
    DraftStorageIdentity, DraftVolumeCandidate, InventoryError, ResourceSnapshot,
    draft_volume_candidates, format_gib,
};
use crate::cascade::CASCADE_CONF_FILE;
use crate::cascade::boot::{
    DEFAULT_MIN_VRAM_HEADROOM_MIB, DEFAULT_ZRAM_MIB, Env, resolve_boot_config_text,
};

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
const STEP_BYTES: u64 = 256 * MIB;

const RECOMMENDED_ZRAM_MIN_BYTES: u64 = GIB;
const RECOMMENDED_ZRAM_MAX_BYTES: u64 = 8 * GIB;
const CONSERVATIVE_ZRAM_MIN_BYTES: u64 = 512 * MIB;
const CONSERVATIVE_ZRAM_MAX_BYTES: u64 = 4 * GIB;

const RECOMMENDED_ORIGIN_MIN_BYTES: u64 = 2 * GIB;
const RECOMMENDED_ORIGIN_MAX_BYTES: u64 = 8 * GIB;
const CONSERVATIVE_ORIGIN_MAX_BYTES: u64 = 4 * GIB;

const RECOMMENDED_FALLBACK_MIN_BYTES: u64 = 4 * GIB;
const RECOMMENDED_FALLBACK_MAX_BYTES: u64 = 8 * GIB;
const CONSERVATIVE_FALLBACK_MAX_BYTES: u64 = 4 * GIB;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct StorageChoice {
    pub(super) volume_index: usize,
    pub(super) bytes: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct StoragePairRecommendation {
    pub(super) origin: Option<StorageChoice>,
    pub(super) fallback: Option<StorageChoice>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct GpuTierRecommendation {
    pub(super) uuid: String,
    pub(super) cap_bytes: u64,
    pub(super) host_share_bytes: u64,
    pub(super) runtime_headroom_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct ThreeTierRecommendation {
    pub(super) zram_bytes: Option<u64>,
    pub(super) gpus: Vec<GpuTierRecommendation>,
    pub(super) origin_bytes: Option<u64>,
    pub(super) origin_volume_display: Option<String>,
    pub(super) fallback_bytes: Option<u64>,
    pub(super) fallback_volume_display: Option<String>,
    pub(super) cascade_zram_mib: u64,
    pub(super) cascade_vram_mib: u64,
    pub(super) min_vram_headroom_mib: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CascadeConfWrite {
    Written,
    PreservedExisting,
}

struct EmptyEnv;
impl Env for EmptyEnv {
    fn var(&self, _key: &str) -> Option<String> {
        None
    }
}

pub(super) fn align_down_step(bytes: u64) -> u64 {
    bytes / STEP_BYTES * STEP_BYTES
}

pub(super) fn recommend_zram_bytes(guest_ram_bytes: Option<u64>, preset: Preset) -> Option<u64> {
    let ram = guest_ram_bytes.filter(|&b| b >= STEP_BYTES)?;
    let (raw, min_cap, max_cap) = match preset {
        Preset::Recommended => (
            ram / 4,
            RECOMMENDED_ZRAM_MIN_BYTES,
            RECOMMENDED_ZRAM_MAX_BYTES,
        ),
        Preset::Conservative => (
            ram / 8,
            CONSERVATIVE_ZRAM_MIN_BYTES,
            CONSERVATIVE_ZRAM_MAX_BYTES,
        ),
    };
    let aligned = align_down_step(raw).max(STEP_BYTES);
    let bounded = if ram >= min_cap.saturating_mul(2) {
        aligned.clamp(min_cap, max_cap)
    } else {
        aligned.min(ram / 2).max(STEP_BYTES)
    };
    Some(align_down_step(bounded).min(align_down_step(ram)))
}

pub(super) fn recommend_storage_pair(
    zram_bytes: Option<u64>,
    volumes_free_bytes: &[u64],
    preferred_index: Option<usize>,
    preset: Preset,
) -> StoragePairRecommendation {
    if volumes_free_bytes.is_empty() {
        return StoragePairRecommendation::default();
    }
    let usable = |free: u64| free.saturating_sub(DISK_RESERVE_FLOOR_BYTES);
    let best_idx = preferred_index
        .filter(|&idx| {
            idx < volumes_free_bytes.len() && usable(volumes_free_bytes[idx]) >= STEP_BYTES
        })
        .or_else(|| {
            volumes_free_bytes
                .iter()
                .enumerate()
                .filter(|(_, free)| usable(**free) >= STEP_BYTES)
                .max_by_key(|(_, free)| **free)
                .map(|(idx, _)| idx)
        });
    let Some(origin_idx) = best_idx else {
        return StoragePairRecommendation::default();
    };
    let origin_usable = usable(volumes_free_bytes[origin_idx]);
    let base_target = zram_bytes.unwrap_or(4 * GIB);
    let (origin_desired, fallback_desired) = match preset {
        Preset::Recommended => (
            base_target.clamp(RECOMMENDED_ORIGIN_MIN_BYTES, RECOMMENDED_ORIGIN_MAX_BYTES),
            base_target.clamp(
                RECOMMENDED_FALLBACK_MIN_BYTES,
                RECOMMENDED_FALLBACK_MAX_BYTES,
            ),
        ),
        Preset::Conservative => (
            (base_target / 2).clamp(RECOMMENDED_ORIGIN_MIN_BYTES, CONSERVATIVE_ORIGIN_MAX_BYTES),
            (base_target / 2).clamp(
                RECOMMENDED_ORIGIN_MIN_BYTES,
                CONSERVATIVE_FALLBACK_MAX_BYTES,
            ),
        ),
    };
    // Leave half of usable headroom above the 10 GiB reserve for fallback swap
    // when both targets share the same volume.
    let origin_cap = if volumes_free_bytes.len() == 1 {
        align_down_step(origin_usable / 2).max(align_down_step(origin_usable.min(STEP_BYTES)))
    } else {
        align_down_step(origin_usable)
    };
    let origin_bytes = align_down_step(origin_desired).min(origin_cap);
    let origin = (origin_bytes >= STEP_BYTES).then_some(StorageChoice {
        volume_index: origin_idx,
        bytes: origin_bytes,
    });

    // Pick fallback volume: prefer the same preferred/fastest volume if enough
    // space remains after origin, else fall back to another volume with more space.
    let remaining_on = |idx: usize| {
        let used = origin
            .filter(|choice| choice.volume_index == idx)
            .map_or(0, |choice| choice.bytes);
        usable(volumes_free_bytes[idx]).saturating_sub(used)
    };
    let fb_idx = if remaining_on(origin_idx) >= align_down_step(fallback_desired).min(STEP_BYTES) {
        Some(origin_idx)
    } else {
        (0..volumes_free_bytes.len())
            .filter(|&idx| remaining_on(idx) >= STEP_BYTES)
            .max_by_key(|&idx| remaining_on(idx))
    };
    let fallback = fb_idx.and_then(|idx| {
        let bytes = align_down_step(fallback_desired).min(align_down_step(remaining_on(idx)));
        (bytes >= STEP_BYTES).then_some(StorageChoice {
            volume_index: idx,
            bytes,
        })
    });
    StoragePairRecommendation { origin, fallback }
}

pub(super) fn preferred_volume_index(
    snapshot: &ResourceSnapshot,
    candidates: &[DraftVolumeCandidate],
) -> Option<usize> {
    let diag = super::benchmark::windows_host::read_latest_diagnostic_for(snapshot)?;
    let leader = diag.recommendation.recommended_stable_id.as_deref()?;
    candidates
        .iter()
        .position(|candidate| match &candidate.storage {
            DraftStorageIdentity::Wsl2 { volume_id, .. } => volume_id.eq_ignore_ascii_case(leader),
            DraftStorageIdentity::NativeLinux {
                filesystem_uuid, ..
            } => filesystem_uuid.eq_ignore_ascii_case(leader),
        })
}

pub(super) fn recommend_three_tiers(
    snapshot: &ResourceSnapshot,
    preset: Preset,
    now_unix_ms: u64,
) -> ThreeTierRecommendation {
    let guest_ram = snapshot
        .guest_memory
        .total_bytes
        .filter(|_| snapshot.guest_memory.required_counters_available);
    let zram_bytes = recommend_zram_bytes(guest_ram, preset);
    let gpus = recommend_gpus(&snapshot.gpu_inventory, preset);
    let candidates = draft_volume_candidates(snapshot, now_unix_ms);
    let free_list: Vec<u64> = candidates.iter().map(|c| c.free_bytes).collect();
    let preferred = preferred_volume_index(snapshot, &candidates);
    let storage = recommend_storage_pair(zram_bytes, &free_list, preferred, preset);

    let cascade_zram_mib = zram_bytes
        .map(|b| (b / MIB).max(256))
        .unwrap_or(DEFAULT_ZRAM_MIB);
    let cascade_vram_mib = storage
        .origin
        .map(|c| (c.bytes / MIB).max(256))
        .unwrap_or(cascade_zram_mib);
    let min_vram_headroom_mib = gpus
        .iter()
        .map(|gpu| gpu.host_share_bytes.div_ceil(MIB))
        .max()
        .unwrap_or(DEFAULT_MIN_VRAM_HEADROOM_MIB)
        .max(DEFAULT_MIN_VRAM_HEADROOM_MIB);

    ThreeTierRecommendation {
        zram_bytes,
        gpus,
        origin_bytes: storage.origin.map(|c| c.bytes),
        origin_volume_display: storage
            .origin
            .map(|c| candidates[c.volume_index].short_display.clone()),
        fallback_bytes: storage.fallback.map(|c| c.bytes),
        fallback_volume_display: storage
            .fallback
            .map(|c| candidates[c.volume_index].short_display.clone()),
        cascade_zram_mib,
        cascade_vram_mib,
        min_vram_headroom_mib,
    }
}

fn recommend_gpus(devices: &[GpuObservation], preset: Preset) -> Vec<GpuTierRecommendation> {
    devices
        .iter()
        .map(|gpu| {
            let estimate = gpu_inventory::preview(gpu, preset);
            GpuTierRecommendation {
                uuid: gpu.uuid.clone(),
                cap_bytes: estimate.cap_bytes,
                host_share_bytes: estimate.host_share_bytes,
                runtime_headroom_bytes: estimate.runtime_headroom_bytes,
            }
        })
        .collect()
}

/// Render a canonical `/etc/ramshared/cascade.conf` body and validate it
/// against the boot resolver before it can ever touch disk.
pub(super) fn render_cascade_conf(
    zram_mib: u64,
    vram_mib: u64,
    min_vram_headroom_mib: u64,
    note: &str,
) -> Result<String, InventoryError> {
    let clamped_headroom = min_vram_headroom_mib.max(DEFAULT_MIN_VRAM_HEADROOM_MIB);
    let body = format!(
        "# Auto-generated by RamShared ({note})\n\
         # All 3 tiers enabled in writeback posture: Tier 1 ZRAM -> Tier 2 Origin/VRAM -> Tier 3 Fallback SSD\n\
         VRAM_MIB={vram_mib}\n\
         ZRAM_MIB={zram_mib}\n\
         MIN_VRAM_HEADROOM_MIB={clamped_headroom}\n"
    );
    resolve_boot_config_text(&body, &EmptyEnv).map_err(|error| {
        InventoryError(format!("generated cascade.conf failed validation: {error}"))
    })?;
    Ok(body)
}

/// Atomically write `/etc/ramshared/cascade.conf` without following symlinks.
/// When `overwrite == false`, an existing regular file is preserved untouched.
pub(super) fn write_cascade_conf_atomic(
    path: &Path,
    content: &str,
    overwrite: bool,
) -> Result<CascadeConfWrite, InventoryError> {
    resolve_boot_config_text(content, &EmptyEnv)
        .map_err(|error| InventoryError(format!("refusing invalid cascade.conf: {error}")))?;
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(InventoryError(format!(
                "refusing symlinked cascade config at {}",
                path.display()
            )));
        }
        Ok(_) if !overwrite => return Ok(CascadeConfWrite::PreservedExisting),
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(InventoryError(format!(
                "cannot inspect {}: {err}",
                path.display()
            )));
        }
    }
    let parent = path
        .parent()
        .ok_or_else(|| InventoryError("cascade config path has no parent directory".into()))?;
    fs::create_dir_all(parent).map_err(|err| {
        InventoryError(format!(
            "cannot create config directory {}: {err}",
            parent.display()
        ))
    })?;
    let parent_meta = fs::symlink_metadata(parent).map_err(|err| {
        InventoryError(format!(
            "cannot inspect config directory {}: {err}",
            parent.display()
        ))
    })?;
    if parent_meta.file_type().is_symlink() || !parent_meta.is_dir() {
        return Err(InventoryError(format!(
            "refusing non-directory or symlink parent {}",
            parent.display()
        )));
    }
    let tmp_name = format!(".cascade.conf.{}.tmp", std::process::id());
    let tmp_path = parent.join(tmp_name);
    let _ = fs::remove_file(&tmp_path);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o644)
        .open(&tmp_path)
        .map_err(|err| {
            InventoryError(format!(
                "cannot create temporary config {}: {err}",
                tmp_path.display()
            ))
        })?;
    if let Err(err) = file
        .write_all(content.as_bytes())
        .and_then(|()| file.sync_all())
        .and_then(|()| fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o644)))
    {
        let _ = fs::remove_file(&tmp_path);
        return Err(InventoryError(format!(
            "cannot write temporary config {}: {err}",
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
    if let Ok(dir) = fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(CascadeConfWrite::Written)
}

/// Ensure `/etc/ramshared/cascade.conf` exists with hardware-detected 3-tier
/// defaults before post-install activation. Existing operator files are kept.
pub(super) fn ensure_recommended_cascade_conf_at(
    path: &Path,
    snapshot: &ResourceSnapshot,
    now_unix_ms: u64,
) -> Result<Option<ThreeTierRecommendation>, InventoryError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(InventoryError(format!(
                "refusing symlinked cascade config at {}",
                path.display()
            )));
        }
        Ok(_) => return Ok(None),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(InventoryError(format!(
                "cannot inspect {}: {err}",
                path.display()
            )));
        }
    }
    let rec = recommend_three_tiers(snapshot, Preset::Recommended, now_unix_ms);
    let body = render_cascade_conf(
        rec.cascade_zram_mib,
        rec.cascade_vram_mib,
        rec.min_vram_headroom_mib,
        "recommended 3-tier hardware profile",
    )?;
    match write_cascade_conf_atomic(path, &body, false)? {
        CascadeConfWrite::Written => Ok(Some(rec)),
        CascadeConfWrite::PreservedExisting => Ok(None),
    }
}

/// Production hook invoked by `ramshared install` before activating units.
pub(crate) fn ensure_recommended_cascade_conf_live(stdout: &mut dyn std::io::Write) {
    let path = Path::new(CASCADE_CONF_FILE);
    if path.exists() {
        return;
    }
    let Ok(snapshot) = super::collect_snapshot() else {
        return;
    };
    if let Ok(Some(rec)) = ensure_recommended_cascade_conf_at(path, &snapshot, super::unix_millis())
    {
        let _ = writeln!(
            stdout,
            "NBD_INSTALL_CASCADE_CONF=auto-recommended (zram_mib={} vram_mib={} headroom_mib={})",
            rec.cascade_zram_mib, rec.cascade_vram_mib, rec.min_vram_headroom_mib
        );
    }
}

/// Human-readable 3-tier recommendation block for `ramshared config show`.
pub(super) fn render_recommendation_summary(rec: &ThreeTierRecommendation) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Recommended 3-tier configuration (all tiers active in writeback posture):"
    );
    let _ = writeln!(
        out,
        "  Tier 1 (ZRAM):        {} (cascade ZRAM_MIB={})",
        format_gib(rec.zram_bytes),
        rec.cascade_zram_mib
    );
    if rec.gpus.is_empty() {
        let _ = writeln!(
            out,
            "  Tier 2 (GPU VRAM):    no GPU detected; origin-only backing (MIN_VRAM_HEADROOM_MIB={})",
            rec.min_vram_headroom_mib
        );
    } else {
        for gpu in &rec.gpus {
            let _ = writeln!(
                out,
                "  Tier 2 (GPU VRAM):    {} cap {} (host reserve {}, runtime headroom {})",
                gpu.uuid,
                format_gib(Some(gpu.cap_bytes)),
                format_gib(Some(gpu.host_share_bytes)),
                format_gib(Some(gpu.runtime_headroom_bytes))
            );
        }
    }
    let origin_vol = rec.origin_volume_display.as_deref().unwrap_or("unselected");
    let _ = writeln!(
        out,
        "  Tier 2 (SSD origin):  {} on {} (cascade VRAM_MIB={})",
        format_gib(rec.origin_bytes),
        origin_vol,
        rec.cascade_vram_mib
    );
    let fb_vol = rec
        .fallback_volume_display
        .as_deref()
        .unwrap_or("unselected");
    let _ = writeln!(
        out,
        "  Tier 3 (Fallback):    {} on {}",
        format_gib(rec.fallback_bytes),
        fb_vol
    );
    out
}

#[cfg(test)]
mod tests;
