// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/observations/block_io.rs
//! Bounded per-tier block I/O collection from `/sys/block/<dev>/stat` and
//! `/proc/diskstats` cross-checks (`topology-aware-observability` DT-4).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use super::super::model::TierIoStats;
use crate::cascade::{SwapEntry, SwapTierRole, diskstats_role_name, swap_tier_role};

/// How a block device kernel name maps onto the swap tiers.
enum DiskstatsTier {
    Zram,
    Vram,
    Fallback,
    /// A real disk that is not a swap tier. Deliberately not accumulated.
    Other,
}

fn classify_diskstats_dev(
    dev: &str,
    fallback: &BTreeSet<String>,
    dm_aliases: &BTreeMap<String, String>,
) -> DiskstatsTier {
    let role_name = diskstats_role_name(dev, dm_aliases);
    match swap_tier_role(role_name) {
        SwapTierRole::Zram => DiskstatsTier::Zram,
        SwapTierRole::Vram => DiskstatsTier::Vram,
        SwapTierRole::Fallback => {
            if fallback.contains(&dev.to_ascii_lowercase())
                || fallback.contains(&role_name.to_ascii_lowercase())
            {
                DiskstatsTier::Fallback
            } else {
                DiskstatsTier::Other
            }
        }
    }
}

/// Parses a Linux `/sys/block/<dev>/stat` line (`fields[2]` sectors read,
/// `fields[6]` sectors written) into `(read_bytes, write_bytes)` (`* 512`).
pub(in crate::monitor) fn parse_sysfs_block_stat(text: &str) -> Option<(u64, u64)> {
    let fields: Vec<&str> = text.split_whitespace().collect();
    if fields.len() < 11 {
        return None;
    }
    let read_sectors = fields[2].parse::<u64>().ok()?;
    let write_sectors = fields[6].parse::<u64>().ok()?;
    Some((
        read_sectors.saturating_mul(512),
        write_sectors.saturating_mul(512),
    ))
}

fn parse_diskstats_map(
    diskstats_text: &str,
    fallback: &BTreeSet<String>,
    dm_aliases: &BTreeMap<String, String>,
) -> BTreeMap<String, (DiskstatsTier, u64, u64)> {
    let mut diskstats_devs = BTreeMap::new();
    for line in diskstats_text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() >= 10
            && let (Ok(read_sectors), Ok(write_sectors)) =
                (fields[5].parse::<u64>(), fields[9].parse::<u64>())
        {
            let dev = fields[2].to_ascii_lowercase();
            let tier = classify_diskstats_dev(&dev, fallback, dm_aliases);
            if !matches!(tier, DiskstatsTier::Other) {
                diskstats_devs.insert(
                    dev,
                    (
                        tier,
                        read_sectors.saturating_mul(512),
                        write_sectors.saturating_mul(512),
                    ),
                );
            }
        }
    }
    diskstats_devs
}

fn device_counter_candidates(
    entries: &[SwapEntry],
    diskstats_devs: &BTreeMap<String, (DiskstatsTier, u64, u64)>,
    dm_aliases: &BTreeMap<String, String>,
) -> BTreeSet<String> {
    let mut candidates = BTreeSet::new();
    for entry in entries {
        if entry.is_ghost() {
            continue;
        }
        let path = entry.filename.split_whitespace().next().unwrap_or_default();
        // Bound sysfs probing to device-shaped swap entries (/dev/... or bare name);
        // swap files live on a mounted filesystem and do not own a /sys/block node.
        let is_dev_shape = path.starts_with("/dev/") || (!path.is_empty() && !path.contains('/'));
        if !is_dev_shape {
            continue;
        }
        let Some(base) = Path::new(path).file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let lower = base.to_ascii_lowercase();
        candidates.insert(lower.clone());
        for (dm_name, alias) in dm_aliases {
            if alias.eq_ignore_ascii_case(&lower) {
                candidates.insert(dm_name.to_ascii_lowercase());
            }
        }
    }
    for dev in diskstats_devs.keys() {
        candidates.insert(dev.clone());
    }
    candidates
}

/// Measures per-tier and total swap I/O from `/sys/block/<dev>/stat` first and
/// uses `/proc/diskstats` as a cross-check fallback when sysfs is missing
/// (`topology-aware-observability` DT-4). Reads are bounded to the devices
/// already listed by the snapshot (one `/stat` read per unique device).
pub(in crate::monitor) fn collect_per_tier_io(
    sys_block: &Path,
    entries: &[SwapEntry],
    diskstats_text: &str,
    fallback: &BTreeSet<String>,
    dm_aliases: &BTreeMap<String, String>,
) -> (
    TierIoStats,
    TierIoStats,
    TierIoStats,
    u64,
    u64,
    Option<&'static str>,
) {
    let diskstats_devs = parse_diskstats_map(diskstats_text, fallback, dm_aliases);
    let mut zram = TierIoStats::default();
    let mut vram = TierIoStats::default();
    let mut disk = TierIoStats::default();
    let mut used_sysfs = false;
    let mut used_diskstats = false;

    for dev in device_counter_candidates(entries, &diskstats_devs, dm_aliases) {
        let tier = classify_diskstats_dev(&dev, fallback, dm_aliases);
        if matches!(tier, DiskstatsTier::Other) {
            continue;
        }
        let sysfs_reading = fs::read_to_string(sys_block.join(&dev).join("stat"))
            .ok()
            .and_then(|text| parse_sysfs_block_stat(&text));
        let counters = if let Some((rb, wb)) = sysfs_reading {
            used_sysfs = true;
            Some((rb, wb))
        } else if let Some((_, rb, wb)) = diskstats_devs.get(&dev) {
            used_diskstats = true;
            Some((*rb, *wb))
        } else {
            None
        };
        if let Some((rb, wb)) = counters {
            let target = match tier {
                DiskstatsTier::Zram => &mut zram,
                DiskstatsTier::Vram => &mut vram,
                DiskstatsTier::Fallback => &mut disk,
                DiskstatsTier::Other => continue,
            };
            target.read_bytes = target.read_bytes.saturating_add(rb);
            target.write_bytes = target.write_bytes.saturating_add(wb);
        }
    }

    let swap_read_bytes = zram
        .read_bytes
        .saturating_add(vram.read_bytes)
        .saturating_add(disk.read_bytes);
    let swap_write_bytes = zram
        .write_bytes
        .saturating_add(vram.write_bytes)
        .saturating_add(disk.write_bytes);
    let source = match (used_sysfs, used_diskstats) {
        (true, true) => Some("sysfs_stat+diskstats_crosscheck"),
        (true, false) => Some("sysfs_stat"),
        (false, true) => Some("diskstats_crosscheck"),
        (false, false) => None,
    };
    (zram, vram, disk, swap_read_bytes, swap_write_bytes, source)
}

#[cfg(test)]
pub(in crate::monitor) fn parse_swap_diskstats(
    text: &str,
    fallback: &BTreeSet<String>,
    dm_aliases: &BTreeMap<String, String>,
) -> (u64, u64) {
    let mut read_bytes = 0u64;
    let mut write_bytes = 0u64;
    for (_, rb, wb) in parse_diskstats_map(text, fallback, dm_aliases).into_values() {
        read_bytes = read_bytes.saturating_add(rb);
        write_bytes = write_bytes.saturating_add(wb);
    }
    (read_bytes, write_bytes)
}

#[cfg(test)]
pub(in crate::monitor) fn parse_per_tier_diskstats(
    text: &str,
    fallback: &BTreeSet<String>,
    dm_aliases: &BTreeMap<String, String>,
) -> (TierIoStats, TierIoStats, TierIoStats) {
    let mut zram = TierIoStats::default();
    let mut vram = TierIoStats::default();
    let mut disk = TierIoStats::default();
    for (tier, rb, wb) in parse_diskstats_map(text, fallback, dm_aliases).into_values() {
        let target = match tier {
            DiskstatsTier::Zram => &mut zram,
            DiskstatsTier::Vram => &mut vram,
            DiskstatsTier::Fallback => &mut disk,
            DiskstatsTier::Other => continue,
        };
        target.read_bytes = target.read_bytes.saturating_add(rb);
        target.write_bytes = target.write_bytes.saturating_add(wb);
    }
    (zram, vram, disk)
}
