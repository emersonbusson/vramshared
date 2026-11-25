// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/swaps.rs
//! `/proc/swaps` parsing, device path classification, ghost/orphan detection, and absorber accounting.
//!
//! Extracted from `cascade/mod.rs` to keep swap-table inspection separate from configuration, telemetry snapshot building, and status presentation.

use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

// --- /proc/swaps parsing (pure, unit-tested) ---------------------------------

/// One line from `/proc/swaps` after the header.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SwapEntry {
    /// Path as shown by the kernel (may contain ` (deleted)` or `\040(deleted)`).
    pub filename: String,
    pub size_kb: u64,
    pub used_kb: u64,
    pub priority: i32,
}

/// True when a `/proc/swaps` filename names a block device the kernel lost.
///
/// The kernel appends ` (deleted)` — or the `\040` spelling of that space — to
/// a swap path whose block device is gone. The row itself is well formed; the
/// device behind it is not. Every fold that attributes live usage to a tier
/// skips it for one reason: a device that no longer exists has no live usage
/// to attribute, and counting it makes the status tier tables and the stress
/// measurement tier tables disagree about the same host. The status snapshot
/// has always applied this before folding; the measurement parsers now apply
/// the same rule rather than growing a second spelling of it.
pub fn is_ghost_swap_filename(filename: &str) -> bool {
    filename.contains("(deleted)") || filename.contains("\\040(deleted)")
}

impl SwapEntry {
    /// True if the kernel already lost the block device (ghost swap).
    pub fn is_ghost(&self) -> bool {
        is_ghost_swap_filename(&self.filename)
    }

    /// True if this looks like a RamShared-managed or dangerous orphan tier.
    pub fn is_managed_or_orphan_vram_tier(&self) -> bool {
        is_allowlisted_managed_path(&self.bare_path())
    }

    /// Basename-ish key for matching recorded paths.
    pub fn bare_path(&self) -> String {
        self.filename
            .replace("\\040(deleted)", " (deleted)")
            .split_whitespace()
            .next()
            .unwrap_or(&self.filename)
            .to_string()
    }

    /// Canonical `/dev/...` path for swapoff (kernel may list `/nbd0` without `/dev`).
    pub fn canonical_path(&self) -> String {
        canonicalize_swap_path(&self.bare_path())
    }
}

/// `/nbd0` → `/dev/nbd0`; `/dev/nbd0` unchanged; `nbd0` → `/dev/nbd0`.
/// SPEC: wsl2-cascade-orphan-recover ITEM-1
pub fn canonicalize_swap_path(p: &str) -> String {
    let p = p.trim();
    if p.is_empty() {
        return String::new();
    }
    if p.starts_with("/dev/") {
        return p.to_string();
    }
    if let Some(rest) = p.strip_prefix('/') {
        return format!("/dev/{rest}");
    }
    format!("/dev/{p}")
}

pub(super) fn numbered_device_basename(path: &str, prefix: &str) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path);
    let Some(number) = base.strip_prefix(prefix) else {
        return false;
    };
    !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
}

pub(super) fn has_device_path_shape(path: &str) -> bool {
    if let Some(rest) = path.strip_prefix("/dev/") {
        !rest.is_empty() && !rest.contains('/')
    } else if let Some(rest) = path.strip_prefix('/') {
        !rest.is_empty() && !rest.contains('/')
    } else {
        !path.is_empty() && !path.contains('/')
    }
}

pub(crate) fn is_nbd_device_path(path: &str) -> bool {
    let bare = path
        .trim()
        .replace("\\040(deleted)", " (deleted)")
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    has_device_path_shape(&bare) && numbered_device_basename(&bare, "nbd")
}

pub(crate) fn is_ublk_device_path(path: &str) -> bool {
    let bare = path
        .trim()
        .replace("\\040(deleted)", " (deleted)")
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    has_device_path_shape(&bare) && numbered_device_basename(&bare, "ublkb")
}

pub(crate) fn is_zram_device_path(path: &str) -> bool {
    let bare = path
        .trim()
        .replace("\\040(deleted)", " (deleted)")
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    has_device_path_shape(&bare) && numbered_device_basename(&bare, "zram")
}

/// `/dev/mapper/ramshared…` with a non-empty product suffix. The device-mapper
/// spelling of the product logical device; `has_device_path_shape` rejects it
/// because of the extra `mapper/` component, so it is its own branch.
pub(super) fn is_ramshared_mapper_path(path: &str) -> bool {
    path.strip_prefix("/dev/mapper/ramshared")
        .is_some_and(|suffix| {
            !suffix.is_empty()
                && suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
}

/// The product logical block device: `/dev/ramsharedN`, its bare kernel name
/// `ramsharedN`, or the device-mapper spelling `/dev/mapper/ramshared…`.
///
/// This is a **product role name**, not a host literal. `/proc/diskstats` names
/// a device-mapper target `dm-N`; callers resolve that through
/// [`diskstats_role_name`] before asking for the role, so the alias — not the
/// kernel name — is what gets classified.
pub fn is_ramshared_logical_path(path: &str) -> bool {
    let bare = path
        .trim()
        .replace("\\040(deleted)", " (deleted)")
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    is_ramshared_mapper_path(&bare)
        || (has_device_path_shape(&bare) && numbered_device_basename(&bare, "ramshared"))
}

/// Allowlist for automatic lifecycle operations: exact product block-device
/// identities only, never a similarly named file or disk.
pub(super) fn is_allowlisted_managed_path(path: &str) -> bool {
    let bare = path
        .trim()
        .replace("\\040(deleted)", " (deleted)")
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    is_nbd_device_path(&bare)
        || is_ublk_device_path(&bare)
        || is_zram_device_path(&bare)
        || is_ramshared_mapper_path(&bare)
}

/// Which swap tier a filename belongs to, **by role**. Tier membership is never
/// named by a host block-device literal: the host only decides which filename
/// lands in which role.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SwapTierRole {
    /// Compressed host RAM (tier 1 in the measurement scheme).
    Zram,
    /// VRAM cache behind the logical block device (tier 2).
    Vram,
    /// Live swap that is neither compressed RAM nor the logical VRAM device:
    /// the last-resort fallback sink (tier 3).
    Fallback,
}

/// Role classification of a `/proc/swaps` filename. This is the single tier
/// definition shared by `tiers_from_swap_names`, `disk_swap_used_kib` and
/// `fallback_swap_device_names`.
pub fn swap_tier_role(path: &str) -> SwapTierRole {
    if is_zram_device_path(path) {
        SwapTierRole::Zram
    } else if is_nbd_device_path(path)
        || is_ublk_device_path(path)
        || is_ramshared_logical_path(path)
    {
        SwapTierRole::Vram
    } else {
        SwapTierRole::Fallback
    }
}

/// True when a swap entry is the tier-3 fallback sink **by role**: live swap
/// that is neither the compressed-RAM device nor the logical VRAM device.
/// Ghosts are excluded — the kernel already lost that block device, so it has
/// no live `diskstats` row.
pub fn is_fallback_swap_entry(entry: &SwapEntry) -> bool {
    !entry.is_ghost() && swap_tier_role(&entry.filename) == SwapTierRole::Fallback
}

/// Join a `/proc/swaps` spelling to the bare kernel name `/proc/diskstats`
/// uses in field 3.
///
/// `has_device_path_shape` accepts both a `/dev/…` path and a bare kernel name
/// (and the kernel's occasional `/nbd0` spelling), so the join is the basename
/// after the last `/`. A swap **file** has no `diskstats` row of its own — the
/// kernel attributes its I/O to the filesystem's disk — so a multi-component
/// file path contributes nothing. A top-level `/name` file is kept and is
/// inert: no `diskstats` row carries that name, so it can never match.
pub(super) fn diskstats_name_for_swap_path(filename: &str) -> Option<String> {
    let bare = filename
        .trim()
        .replace("\\040(deleted)", " (deleted)")
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !has_device_path_shape(&bare) {
        return None;
    }
    let base = bare.rsplit('/').next().unwrap_or(bare.as_str());
    if base.is_empty() {
        None
    } else {
        Some(base.to_string())
    }
}

/// Kernel `diskstats` names of the live tier-3 fallback swap devices.
///
/// The returned names are the membership test the `/proc/diskstats` parsers
/// use. Classification of `diskstats` by exclusion ("whatever is not zram or
/// nbd") is wrong: that would attribute the root filesystem's disk and every
/// data volume to the fallback swap. Tier 3 is exactly the devices `/proc/swaps`
/// puts in the fallback role, and nothing else.
pub fn fallback_swap_device_names(entries: &[SwapEntry]) -> BTreeSet<String> {
    entries
        .iter()
        .filter(|entry| is_fallback_swap_entry(entry))
        .filter_map(|entry| diskstats_name_for_swap_path(&entry.filename))
        .collect()
}

/// Device-mapper kernel names (`dm-N`) to the alias each target records in
/// `/sys/block/dm-N/dm/name`.
///
/// `/proc/diskstats` names a device-mapper target by its kernel name. The swap
/// table and the role predicates speak the alias. This map is the other half of
/// the path↔name join: [`diskstats_name_for_swap_path`] maps a swap path onto a
/// kernel name, this maps a kernel name onto the alias the roles understand.
///
/// An unreadable or absent root yields an empty map, so a host whose sysfs is
/// unavailable gets honest omission rather than a guessed identity.
pub fn dm_name_aliases(sys_block: &Path) -> BTreeMap<String, String> {
    let mut aliases = BTreeMap::new();
    let Ok(entries) = fs::read_dir(sys_block) else {
        return aliases;
    };
    for entry in entries.flatten() {
        let kernel_name = entry.file_name();
        let Some(kernel_name) = kernel_name.to_str() else {
            continue;
        };
        if !kernel_name.starts_with("dm-") {
            continue;
        }
        let Ok(alias) = fs::read_to_string(entry.path().join("dm").join("name")) else {
            continue;
        };
        let alias = alias.trim();
        if !alias.is_empty() {
            aliases.insert(kernel_name.to_string(), alias.to_string());
        }
    }
    aliases
}

/// The name a `diskstats` row is classified under: its device-mapper alias when
/// the row is a `dm-N` target with a recorded alias, the kernel name otherwise.
///
/// The lookup is case-insensitive on the key because the membership sets built
/// from `/proc/swaps` are lowercased. An unknown name is returned unchanged, so
/// a host without the alias observation keeps classifying exactly as before.
pub fn diskstats_role_name<'a>(dev: &'a str, dm_aliases: &'a BTreeMap<String, String>) -> &'a str {
    dm_aliases
        .get(&dev.to_ascii_lowercase())
        .map(String::as_str)
        .unwrap_or(dev)
}

/// Parse a complete `/proc/swaps` snapshot. Any malformed or duplicate row
/// invalidates the whole snapshot: absence is a safety proof, not a best-effort
/// observation.
pub fn parse_proc_swaps(text: &str) -> Result<Vec<SwapEntry>, CascadeError> {
    let mut lines = text.lines();
    let header = lines
        .next()
        .ok_or_else(|| CascadeError::Precondition("/proc/swaps is empty".into()))?;
    let header_fields = header.split_whitespace().collect::<Vec<_>>();
    if header_fields != ["Filename", "Type", "Size", "Used", "Priority"] {
        return Err(CascadeError::Precondition(
            "/proc/swaps header is malformed or unsupported".into(),
        ));
    }

    let mut entries = Vec::new();
    let mut identities = std::collections::BTreeSet::new();
    for (index, line) in lines.enumerate() {
        if line.trim().is_empty() {
            return Err(CascadeError::Precondition(format!(
                "/proc/swaps row {} is empty",
                index + 2
            )));
        }
        let cols = line.split_whitespace().collect::<Vec<_>>();
        if cols.len() < 5 {
            return Err(CascadeError::Precondition(format!(
                "/proc/swaps row {} is malformed",
                index + 2
            )));
        }
        let n = cols.len();
        if !matches!(cols[n - 4], "file" | "partition") {
            return Err(CascadeError::Precondition(format!(
                "/proc/swaps row {} has an unsupported type",
                index + 2
            )));
        }
        let priority = cols[n - 1].parse::<i32>().map_err(|_| {
            CascadeError::Precondition(format!(
                "/proc/swaps row {} has an invalid priority",
                index + 2
            ))
        })?;
        let used_kb = cols[n - 2].parse::<u64>().map_err(|_| {
            CascadeError::Precondition(format!(
                "/proc/swaps row {} has an invalid used count",
                index + 2
            ))
        })?;
        let size_kb = cols[n - 3].parse::<u64>().map_err(|_| {
            CascadeError::Precondition(format!("/proc/swaps row {} has an invalid size", index + 2))
        })?;
        let filename = cols[..n - 4].join(" ");
        if filename.is_empty() {
            return Err(CascadeError::Precondition(format!(
                "/proc/swaps row {} has no device path",
                index + 2
            )));
        }
        let identity = canonicalize_swap_path(
            filename
                .replace("\\040(deleted)", " (deleted)")
                .split_whitespace()
                .next()
                .unwrap_or_default(),
        );
        if identity.is_empty() || !identities.insert(identity) {
            return Err(CascadeError::Precondition(format!(
                "/proc/swaps row {} repeats or omits a device identity",
                index + 2
            )));
        }
        entries.push(SwapEntry {
            filename,
            size_kb,
            used_kb,
            priority,
        });
    }
    Ok(entries)
}

pub(super) fn read_swaps() -> Result<Vec<SwapEntry>, CascadeError> {
    #[cfg(test)]
    if let Some(message) = TEST_SWAPS_ERROR.with(|cell| cell.borrow().clone()) {
        return Err(CascadeError::Io(message));
    }
    #[cfg(test)]
    if let Some(snapshot) = TEST_SWAPS_SEQUENCE.with(|queue| queue.borrow_mut().pop_front()) {
        return snapshot
            .map_err(CascadeError::Io)
            .and_then(|text| parse_proc_swaps(&text));
    }
    #[cfg(test)]
    if let Some(s) = TEST_SWAPS.with(|c| c.borrow().clone()) {
        return parse_proc_swaps(&s);
    }
    fs::read_to_string("/proc/swaps")
        .map_err(|error| CascadeError::Io(format!("read /proc/swaps: {error}")))
        .and_then(|text| parse_proc_swaps(&text))
}

/// Ghost VRAM/zram entries that will hang `swapoff` / page-in if left alone.
pub fn ghost_vram_swaps(entries: &[SwapEntry]) -> Vec<&SwapEntry> {
    entries
        .iter()
        .filter(|e| e.is_ghost() && e.is_managed_or_orphan_vram_tier())
        .collect()
}

/// Whether any nbd/ublk (non-ghost) swap is still active — daemon kill is forbidden.
pub fn active_vram_block_swap(entries: &[SwapEntry]) -> bool {
    entries.iter().any(|e| {
        !e.is_ghost() && (is_nbd_device_path(&e.filename) || is_ublk_device_path(&e.filename))
    })
}

/// Free bytes available in the DEMOTE absorber: every non-managed swap tier below
/// the VRAM priority. zram and our own block devices are excluded — zram is
/// RAM-backed, so it is not a durable sink when the host is already under memory
/// pressure (A1 counts RAM separately, as `RamHeadroom`).
///
/// A1 is a capacity invariant: presence of a sink is not enough, so this reports
/// how much a total VRAM evacuation can actually land in.
pub(super) fn lower_tier_free_bytes() -> Result<u64, CascadeError> {
    let vram_prio = TierPriorities::default().vram;
    Ok(read_swaps()?
        .iter()
        .filter(|e| {
            // Ignore our managed tiers when looking for DEMOTE sink.
            !(is_zram_device_path(&e.filename)
                || is_nbd_device_path(&e.filename)
                || is_ublk_device_path(&e.filename))
                && e.priority < vram_prio
        })
        .map(|e| e.size_kb.saturating_sub(e.used_kb).saturating_mul(1024))
        .sum())
}

pub(super) fn default_daemon() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("ramsharedd")))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "ramsharedd".to_string())
}

pub(super) fn chrono_like_now() -> String {
    // Avoid chrono dep: unix seconds is enough for the marker.
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".into())
}

/// True if daemon process may be stopped (no active block VRAM swap).
pub fn daemon_kill_allowed(entries: &[SwapEntry]) -> bool {
    !active_vram_block_swap(entries) && ghost_vram_swaps(entries).is_empty()
}

pub(super) fn refuse_ghost_swap_state() -> Result<(), CascadeError> {
    let entries = read_swaps()?;
    let ghosts = ghost_vram_swaps(&entries);
    if ghosts.is_empty() {
        return Ok(());
    }
    let detail: Vec<String> = ghosts
        .iter()
        .map(|e| format!("{} used_kb={}", e.filename, e.used_kb))
        .collect();
    let distro = sealed_distro_or_unknown();
    Err(CascadeError::Precondition(format!(
        "estado sujo: swap fantasma (device deleted) em /proc/swaps: {}. \
         NAO e seguro continuar. No Windows: capture evidencias, rode \
         `wsl --terminate {distro}`, reabra a distro, \
         depois `sudo ramshared down` e `sudo ramshared up ...`. \
         Nunca mate o daemon com ublk/nbd ativo.",
        detail.join("; ")
    )))
}

/// Pure plan for orphan handling (unit-tested).
/// SPEC: wsl2-cascade-orphan-recover ITEM-2
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum OrphanPlan {
    /// No managed orphan context.
    None,
    /// Unbound live devices were detected. Enumeration is detection-only.
    DetectedUnboundZeroUsed,
    /// Dangerous: nbd/ublk with pages — no auto swapoff.
    RefuseDirtyBackend,
}

pub(super) fn plan_orphan_action(entries: &[SwapEntry], cascade_healthy: bool) -> OrphanPlan {
    if cascade_healthy {
        return OrphanPlan::None;
    }
    let live_managed: Vec<&SwapEntry> = entries
        .iter()
        .filter(|e| !e.is_ghost() && e.is_managed_or_orphan_vram_tier())
        .collect();
    if live_managed.is_empty() {
        return OrphanPlan::None;
    }
    let dirty_block = live_managed.iter().any(|e| {
        e.used_kb > 0 && (is_nbd_device_path(&e.filename) || is_ublk_device_path(&e.filename))
    });
    if dirty_block {
        return OrphanPlan::RefuseDirtyBackend;
    }
    OrphanPlan::DetectedUnboundZeroUsed
}

/// One attended transition plan from the pre-binding ZRAM → NBD topology.
///
/// It deliberately proves topology only. Runtime code must still bind the
/// selected devices and daemon immediately before each effect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct LegacyMigrationPlan {
    pub(super) nbd: SwapEntry,
    pub(super) zram: Option<SwapEntry>,
}

/// Build the only legacy topology the attended migration may retire.
///
/// Zero-used NBD is a necessary drain precondition, not ownership proof. The
/// caller supplies explicit operator intent and the runtime daemon/device proof
/// before this plan can cause any mutation.
pub(super) fn plan_legacy_migration(
    entries: &[SwapEntry],
) -> Result<LegacyMigrationPlan, CascadeError> {
    if entries
        .iter()
        .any(|entry| entry.is_ghost() && entry.is_managed_or_orphan_vram_tier())
    {
        return Err(CascadeError::Precondition(
            "legacy migration refused: managed ghost swap is present".into(),
        ));
    }

    let managed = entries
        .iter()
        .filter(|entry| !entry.is_ghost() && entry.is_managed_or_orphan_vram_tier())
        .collect::<Vec<_>>();
    let nbd = managed
        .iter()
        .filter(|entry| is_nbd_device_path(&entry.filename))
        .copied()
        .collect::<Vec<_>>();
    let zram = managed
        .iter()
        .filter(|entry| is_zram_device_path(&entry.filename))
        .copied()
        .collect::<Vec<_>>();
    let has_ublk = managed
        .iter()
        .any(|entry| is_ublk_device_path(&entry.filename));

    if has_ublk || nbd.len() != 1 || zram.len() > 1 || managed.len() != nbd.len() + zram.len() {
        return Err(CascadeError::Precondition(
            "legacy migration requires exactly one NBD, at most one ZRAM, and no ublk".into(),
        ));
    }
    let nbd = nbd[0];
    if nbd.used_kb != 0 {
        return Err(CascadeError::Precondition(format!(
            "legacy migration requires a drained NBD before handoff; used_kb={}",
            nbd.used_kb
        )));
    }
    if let Some(zram) = zram.first()
        && zram.priority <= nbd.priority
    {
        return Err(CascadeError::Precondition(
            "legacy migration requires ZRAM priority above the NBD tier".into(),
        ));
    }

    Ok(LegacyMigrationPlan {
        nbd: nbd.clone(),
        zram: zram.first().map(|entry| (*entry).clone()),
    })
}

pub(super) fn disk_swap_used_kib(entries: &[SwapEntry]) -> u64 {
    entries
        .iter()
        .filter(|entry| is_fallback_swap_entry(entry))
        .map(|entry| entry.used_kb)
        .sum()
}

/// Detect unbound managed devices without mutating them. Even used_kb == 0 is
/// active swap and therefore requires an exact sealed lifecycle binding.
pub(super) fn refuse_unbound_managed_devices() -> Result<(), CascadeError> {
    let entries = read_swaps()?;
    if cascade_already_healthy(&entries) {
        return Ok(());
    }

    let plan = plan_orphan_action(&entries, false);
    match plan {
        OrphanPlan::None => Ok(()),
        OrphanPlan::RefuseDirtyBackend => Err(CascadeError::Precondition(format!(
            "unbound nbd/ublk with used_kb>0 — device preserved: live enumeration does not authorize recovery (risk of hang on dead backend). \
             On Windows: capture evidence and use only `wsl --terminate {}`; \
             Never kill -9 ramsharedd with nbd/ublk in /proc/swaps.",
            sealed_distro_or_unknown()
        ))),
        OrphanPlan::DetectedUnboundZeroUsed => Err(CascadeError::Precondition(
            "unbound managed swap detected; live discovery is detection-only. Preserve every device and use the exact sealed lifecycle binding for recovery"
                .into(),
        )),
    }
}
