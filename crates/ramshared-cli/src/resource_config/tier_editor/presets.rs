// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/tier_editor/presets.rs
//! Hardware-detected 3-tier preset population and `cascade.conf` projection for [`Editor`].

use super::{Editor, MIB, Tier};
use crate::cascade::boot::DEFAULT_MIN_VRAM_HEADROOM_MIB;
use crate::resource_config::gpu_inventory::Preset;
use crate::resource_config::{InventoryError, ResourceSnapshot, recommended};

impl Editor {
    /// Build an interactive editor pre-populated with hardware-recommended
    /// values across ALL three tiers (ZRAM, GPU VRAM, SSD Origin, Fallback).
    pub(in crate::resource_config) fn from_snapshot_recommended(
        snapshot: &ResourceSnapshot,
        now: u64,
    ) -> Self {
        let mut editor = Self::from_snapshot(snapshot, now);
        let preferred = recommended::preferred_volume_index(
            snapshot,
            &crate::resource_config::draft_volume_candidates(snapshot, now),
        );
        editor.apply_all_tier_preset(Preset::Recommended, preferred, now);
        editor
    }

    /// Cycle between `Recommended` and `Conservative` across all 3 tiers,
    /// respecting fresh hardware limits and never unlocking a sealed origin.
    pub(in crate::resource_config) fn cycle_all_tier_preset(&mut self, now: u64) {
        let next = self.preset.next();
        self.apply_all_tier_preset(next, None, now);
    }

    pub(in crate::resource_config) fn preset_label(&self) -> &'static str {
        match self.preset {
            Preset::Recommended => "recommended",
            Preset::Conservative => "conservative",
        }
    }

    pub(in crate::resource_config) fn apply_all_tier_preset(
        &mut self,
        preset: Preset,
        preferred_volume: Option<usize>,
        now: u64,
    ) {
        self.preset = preset;
        // 1. Tier 1: ZRAM from fresh guest RAM ceiling.
        let mut chosen_zram = None;
        if let Some(zidx) = self.rows.iter().position(|r| r.tier == Tier::Zram)
            && let Some(ram_max) = self.maximum(zidx, now)
            && let Some(rec_zram) = recommended::recommend_zram_bytes(Some(ram_max), preset)
        {
            let clamped = rec_zram.min(ram_max);
            self.rows[zidx].value = clamped;
            chosen_zram = Some(clamped);
        }

        // 2. Tier 2a: every detected GPU adapter from its fresh safe preview.
        for i in 0..self.rows.len() {
            if matches!(self.rows[i].tier, Tier::Gpu(_))
                && let Some(maximum) = self.maximum(i, now)
            {
                self.rows[i].value = match preset {
                    Preset::Recommended => maximum,
                    Preset::Conservative => maximum / 2 / MIB * MIB,
                };
            }
        }

        // 3. Tier 2b (SSD Origin) + Tier 3 (Fallback SSD) sharing the 10 GiB
        // reserve floor across fresh eligible volumes.
        let fresh_free: Vec<u64> = self
            .volumes
            .iter()
            .map(|v| {
                if crate::resource_config::sample_is_fresh(Some(v.observed_unix_ms), now) {
                    v.free_bytes
                } else {
                    0
                }
            })
            .collect();
        let storage =
            recommended::recommend_storage_pair(chosen_zram, &fresh_free, preferred_volume, preset);

        if !self.prepared.locked()
            && let Some(oidx) = self.rows.iter().position(|r| r.tier == Tier::Origin)
            && let Some(choice) = storage.origin
        {
            self.rows[oidx].volume = Some(self.volumes[choice.volume_index].key.clone());
            self.rows[oidx].value = 0;
            if let Some(limit) = self.maximum(oidx, now) {
                self.rows[oidx].value = choice.bytes.min(limit);
            }
        }
        if let Some(fidx) = self.rows.iter().position(|r| r.tier == Tier::Fallback)
            && let Some(choice) = storage.fallback
        {
            self.rows[fidx].volume = Some(self.volumes[choice.volume_index].key.clone());
            self.rows[fidx].value = 0;
            if let Some(limit) = self.maximum(fidx, now) {
                self.rows[fidx].value = choice.bytes.min(limit);
            }
        }
    }

    /// Derive validated `(zram_mib, vram_mib, min_vram_headroom_mib)` for
    /// `/etc/ramshared/cascade.conf` from the user's current bar values.
    pub(in crate::resource_config) fn cascade_conf_values(
        &self,
        now: u64,
    ) -> Result<(u64, u64, u64), InventoryError> {
        let mut zram_mib = 0u64;
        let mut origin_mib = 0u64;
        let mut gpu_mib = 0u64;
        let mut headroom_mib = DEFAULT_MIN_VRAM_HEADROOM_MIB;

        for (index, row) in self.rows.iter().enumerate() {
            if row.value > 0
                && !(row.tier == Tier::Origin && self.prepared.locked())
                && self
                    .maximum(index, now)
                    .is_none_or(|limit| row.value > limit)
            {
                return Err(InventoryError(
                    "a tier bar exceeds its current fresh limit; reduce or refresh before applying"
                        .into(),
                ));
            }
            match &row.tier {
                Tier::Zram => zram_mib = row.value / MIB,
                Tier::Origin => origin_mib = row.value / MIB,
                Tier::Gpu(_) => {
                    gpu_mib = gpu_mib.max(row.value / MIB);
                    if let Some(share) = row.host_share {
                        headroom_mib = headroom_mib.max(share.div_ceil(MIB));
                    }
                }
                Tier::Fallback | Tier::Emergency => {}
            }
        }
        let vram_mib = if origin_mib > 0 {
            origin_mib
        } else if gpu_mib > 0 {
            gpu_mib
        } else {
            zram_mib
        };
        if zram_mib == 0 || vram_mib == 0 {
            return Err(InventoryError(
                "set positive ZRAM and Tier 2 (SSD origin or GPU) bar values before applying"
                    .into(),
            ));
        }
        Ok((zram_mib, vram_mib, headroom_mib))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::resource_config::{gpu_inventory, unix_millis};

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn from_snapshot_recommended_prefills_all_three_tiers_and_cycles_presets() {
        let mut snapshot = crate::resource_config::tests::fixture_snapshot();
        let now = unix_millis();
        snapshot.gpu_inventory = gpu_inventory::parse_inventory("GPU-a, 6144, 6144").unwrap();
        snapshot.gpu_observed_unix_ms = now;

        let mut editor = Editor::from_snapshot_recommended(&snapshot, now);
        assert_eq!(editor.preset_label(), "recommended");
        let zram = editor.index_of(&Tier::Zram).unwrap();
        let gpu = editor.index_of(&Tier::Gpu("GPU-a".into())).unwrap();
        let origin = editor.index_of(&Tier::Origin).unwrap();
        let fallback = editor.index_of(&Tier::Fallback).unwrap();
        assert_eq!(editor.rows[zram].value, 4 * GIB);
        assert_eq!(editor.rows[gpu].value, 3840 * MIB);
        assert_eq!(editor.rows[origin].value, 4 * GIB);
        assert_eq!(editor.rows[fallback].value, 4 * GIB);
        assert_eq!(editor.cascade_conf_values(now).unwrap(), (4096, 4096, 2048));

        // Cycling to Conservative reduces all 3 tiers proportionally.
        editor.cycle_all_tier_preset(now);
        assert_eq!(editor.preset_label(), "conservative");
        assert_eq!(editor.rows[zram].value, 2 * GIB);
        assert_eq!(editor.rows[gpu].value, 1920 * MIB);
        assert_eq!(editor.rows[origin].value, 2 * GIB);
        assert_eq!(editor.rows[fallback].value, 2 * GIB);
        assert_eq!(editor.cascade_conf_values(now).unwrap(), (2048, 2048, 2048));
    }

    #[test]
    fn cascade_conf_values_rejects_zero_or_stale_excess_bars() {
        let snapshot = crate::resource_config::tests::fixture_snapshot();
        let now = unix_millis();
        let mut editor = Editor::from_snapshot(&snapshot, now);
        // Zero ZRAM/origin cannot be applied.
        assert!(editor.cascade_conf_values(now).is_err());

        // Excess over fresh limit is refused.
        let zram = editor.index_of(&Tier::Zram).unwrap();
        editor.rows[zram].value = u64::MAX;
        assert!(editor.cascade_conf_values(now).is_err());

        // ZRAM-only falls back vram_mib to zram_mib when origin/GPU are 0.
        editor.rows[zram].value = 2 * GIB;
        assert_eq!(editor.cascade_conf_values(now).unwrap(), (2048, 2048, 2048));
    }
}
