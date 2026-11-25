// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/tier_editor.rs
//! Non-mutating per-tier draft controls and shared capacity accounting.

use super::gpu_inventory::{self, GpuObservation, Preset};
use super::{DraftStorageIdentity, ResourceSnapshot};
use ramshared_config::resource_profile::DISK_RESERVE_FLOOR_BYTES;
use std::fmt::Write as _;

mod prepared;
mod presets;
mod profile_emit;
mod profile_load;

const MIB: u64 = 1024 * 1024;
const STEP: u64 = 256 * MIB;
const BAR_WIDTH: usize = 20;

#[derive(Clone, Debug, Eq, PartialEq)]
enum VolumeKey {
    Linux {
        filesystem_uuid: String,
        device_identity: String,
    },
    Windows(String),
}
#[derive(Clone, Debug)]
struct Volume {
    key: VolumeKey,
    display: String,
    free_bytes: u64,
    observed_unix_ms: u64,
}
#[derive(Clone, Debug, Eq, PartialEq)]
enum Tier {
    Zram,
    Gpu(String),
    Origin,
    Fallback,
    Emergency,
}
#[derive(Clone, Debug)]
struct Row {
    tier: Tier,
    value: u64,
    volume: Option<VolumeKey>,
    maximum: Option<u64>,
    observed_unix_ms: u64,
    host_share: Option<u64>,
    runtime_headroom: Option<u64>,
}
#[derive(Clone, Debug)]
pub(super) struct Editor {
    rows: Vec<Row>,
    volumes: Vec<Volume>,
    selected: usize,
    preset: Preset,
    origin_cap_override: Option<Option<u64>>,
    prepared: prepared::PreparedBar,
}

fn fresh(sample: u64, now: u64, max_age: u64) -> bool {
    sample != 0 && now != 0 && now.checked_sub(sample).is_some_and(|age| age <= max_age)
}

impl Editor {
    pub(super) fn from_snapshot(snapshot: &ResourceSnapshot, now: u64) -> Self {
        let volumes = super::draft_volume_candidates(snapshot, now)
            .into_iter()
            .map(|candidate| Volume {
                key: match candidate.storage {
                    DraftStorageIdentity::NativeLinux {
                        filesystem_uuid,
                        device_identity,
                    } => VolumeKey::Linux {
                        filesystem_uuid,
                        device_identity,
                    },
                    DraftStorageIdentity::Wsl2 { volume_id, .. } => {
                        VolumeKey::Windows(volume_id.to_ascii_lowercase())
                    }
                },
                display: candidate.short_display,
                free_bytes: candidate.free_bytes,
                observed_unix_ms: candidate.observed_unix_ms,
            })
            .collect();
        let mut editor = Self::build(
            snapshot
                .guest_memory
                .total_bytes
                .filter(|_| snapshot.guest_memory.required_counters_available),
            snapshot.guest_observed_unix_ms,
            &snapshot.gpu_inventory,
            snapshot.gpu_observed_unix_ms,
            volumes,
        );
        editor.observe_prepared(snapshot, now);
        editor
    }

    #[cfg(test)]
    fn from_parts(
        ram: Option<u64>,
        devices: &[GpuObservation],
        volumes: Vec<Volume>,
        now: u64,
    ) -> Self {
        Self::build(ram, now, devices, now, volumes)
    }

    fn build(
        ram: Option<u64>,
        ram_sample: u64,
        devices: &[GpuObservation],
        gpu_sample: u64,
        volumes: Vec<Volume>,
    ) -> Self {
        let mut rows = vec![Row {
            tier: Tier::Zram,
            value: 0,
            volume: None,
            maximum: ram.filter(|v| *v > 0),
            observed_unix_ms: ram_sample,
            host_share: None,
            runtime_headroom: None,
        }];
        for gpu in devices {
            let estimate = gpu_inventory::preview(gpu, Preset::Recommended);
            rows.push(Row {
                tier: Tier::Gpu(gpu.uuid.clone()),
                value: estimate.cap_bytes,
                volume: None,
                maximum: Some(estimate.cap_bytes),
                observed_unix_ms: gpu_sample,
                host_share: Some(estimate.host_share_bytes),
                runtime_headroom: Some(estimate.runtime_headroom_bytes),
            });
        }
        for tier in [Tier::Origin, Tier::Fallback, Tier::Emergency] {
            rows.push(Row {
                tier,
                value: 0,
                volume: volumes.first().map(|v| v.key.clone()),
                maximum: None,
                observed_unix_ms: 0,
                host_share: None,
                runtime_headroom: None,
            });
        }
        Self {
            rows,
            volumes,
            selected: 0,
            preset: Preset::Recommended,
            origin_cap_override: None,
            prepared: prepared::PreparedBar::default(),
        }
    }

    fn maximum(&self, index: usize, now: u64) -> Option<u64> {
        let row = self.rows.get(index)?;
        if row.tier == Tier::Origin && self.prepared.locked() {
            return self.prepared.maximum(row, now);
        }
        match &row.tier {
            Tier::Zram | Tier::Gpu(_) => {
                let age = if matches!(row.tier, Tier::Gpu(_)) {
                    ramshared_vram::GPU_BUDGET_MAX_AGE_MS
                } else {
                    super::STORAGE_SAMPLE_MAX_AGE_MS
                };
                fresh(row.observed_unix_ms, now, age)
                    .then_some(row.maximum)
                    .flatten()
            }
            _ => {
                let key = row.volume.as_ref()?;
                let volume = self.volumes.iter().find(|v| &v.key == key)?;
                if !super::sample_is_fresh(Some(volume.observed_unix_ms), now) {
                    return None;
                }
                let other = self
                    .rows
                    .iter()
                    .enumerate()
                    .filter(|(i, other)| *i != index && other.volume.as_ref() == Some(key))
                    .try_fold(0u64, |sum, (_, row)| {
                        sum.checked_add(self.prepared.new_bytes(row, now)?)
                    })?;
                Some(
                    volume
                        .free_bytes
                        .saturating_sub(DISK_RESERVE_FLOOR_BYTES)
                        .saturating_sub(other)
                        / MIB
                        * MIB,
                )
            }
        }
    }

    fn adjust(&mut self, index: usize, increase: bool, now: u64) -> bool {
        let Some(row) = self.rows.get(index) else {
            return false;
        };
        if row.tier == Tier::Origin && self.prepared.locked() {
            return false;
        }
        let value = row.value;
        let next = if increase {
            let Some(maximum) = self.maximum(index, now) else {
                return false;
            };
            value.saturating_add(STEP).min(maximum).max(value)
        } else {
            value.saturating_sub(STEP)
        };
        if next == value {
            return false;
        }
        self.rows[index].value = next;
        true
    }

    fn cycle_volume(&mut self, index: usize, now: u64) -> bool {
        let Some(row) = self.rows.get(index) else {
            return false;
        };
        if row.tier == Tier::Origin && self.prepared.locked() {
            return false;
        }
        let Some(old) = row.volume.clone() else {
            return false;
        };
        if self.volumes.is_empty() {
            return false;
        }
        let position = self.volumes.iter().position(|v| v.key == old);
        if position.is_some() && self.volumes.len() < 2 {
            return false;
        }
        let next = position.map_or(0, |position| (position + 1) % self.volumes.len());
        let new = self.volumes[next].key.clone();
        self.rows[index].volume = Some(new);
        if self
            .maximum(index, now)
            .is_none_or(|limit| self.rows[index].value > limit)
        {
            self.rows[index].volume = Some(old);
            return false;
        }
        true
    }

    pub(super) fn reconcile(&mut self, mut updated: Self) {
        for row in &mut updated.rows {
            if let Some(previous) = self.rows.iter().find(|old| same_tier(&old.tier, &row.tier)) {
                row.value = previous.value;
                if previous.volume.is_some() {
                    row.volume.clone_from(&previous.volume);
                }
            }
        }
        for old in &self.rows {
            if matches!(old.tier, Tier::Gpu(_))
                && !updated
                    .rows
                    .iter()
                    .any(|row| same_tier(&row.tier, &old.tier))
            {
                let mut missing = old.clone();
                missing.maximum = None;
                missing.observed_unix_ms = 0;
                updated.rows.push(missing);
            }
        }
        updated.selected = self.selected.min(updated.rows.len().saturating_sub(1));
        updated.preset = self.preset;
        updated.origin_cap_override = self.origin_cap_override;
        updated.reconcile_prepared(self);
        *self = updated;
    }

    pub(super) fn next(&mut self) {
        self.selected = (self.selected + 1) % self.rows.len();
    }
    pub(super) fn adjust_selected(&mut self, increase: bool, now: u64) {
        self.adjust(self.selected, increase, now);
    }
    pub(super) fn clear_selected(&mut self) {
        if self.rows[self.selected].tier == Tier::Origin && self.prepared.locked() {
            return;
        }
        self.rows[self.selected].value = 0;
    }
    pub(super) fn fill_selected(&mut self, now: u64) {
        if self.rows[self.selected].tier == Tier::Origin && self.prepared.locked() {
            return;
        }
        if let Some(limit) = self.maximum(self.selected, now) {
            self.rows[self.selected].value = limit / MIB * MIB;
        }
    }
    pub(super) fn cycle_selected(&mut self, now: u64) {
        self.cycle_volume(self.selected, now);
    }
    #[cfg(test)]
    pub(super) fn toggle_preset(&mut self, now: u64) {
        self.preset = self.preset.next();
        for i in 0..self.rows.len() {
            if matches!(self.rows[i].tier, Tier::Gpu(_))
                && let Some(maximum) = self.maximum(i, now)
            {
                self.rows[i].value = match self.preset {
                    Preset::Recommended => maximum,
                    Preset::Conservative => maximum / 2 / MIB * MIB,
                };
            }
        }
    }
    #[cfg(test)]
    fn index_of(&self, tier: &Tier) -> Option<usize> {
        self.rows.iter().position(|row| same_tier(&row.tier, tier))
    }

    pub(super) fn render(&self, now: u64) -> String {
        let mut out = format!(
            "Preset: {} (3-tier) — Desired values are not applied; a applies cascade.conf, s saves a draft.\n",
            self.preset_label()
        );
        if let Some(cap) = self.origin_cap_override {
            let _ = writeln!(
                out,
                "Retained origin logical ceiling: {} (separate from storage size).",
                super::format_cap_mib(cap)
            );
        }
        if !self.rows.iter().any(|r| matches!(r.tier, Tier::Gpu(_))) {
            out.push_str("GPU estimate unavailable.\n");
        }
        for (i, row) in self.rows.iter().enumerate() {
            let label = match &row.tier {
                Tier::Zram => "ZRAM".to_string(),
                Tier::Gpu(id) => format!("GPU {id}"),
                Tier::Origin => "SSD origin".into(),
                Tier::Fallback => "Fallback swap".into(),
                Tier::Emergency => "Emergency swap".into(),
            };
            let maximum = self.maximum(i, now);
            let filled = maximum.filter(|m| *m > 0).map_or(0, |m| {
                ((u128::from(row.value.min(m)) * BAR_WIDTH as u128) / u128::from(m)) as usize
            });
            let _ = writeln!(
                out,
                "{} {label}  [{}{}] {} / {}{}",
                if i == self.selected { ">" } else { " " },
                "█".repeat(filled),
                "░".repeat(BAR_WIDTH - filled),
                prepared::display_bytes(Some(row.value)),
                prepared::display_bytes(maximum),
                if maximum.is_some_and(|m| row.value > m) {
                    " — above current limit"
                } else if maximum.is_none() {
                    " — limit unavailable or stale"
                } else {
                    ""
                }
            );
            if row.tier == Tier::Origin {
                out.push_str(&self.prepared.render(now));
            }
            match &row.tier {
                Tier::Zram => out.push_str(
                    "    RAM reserve unavailable; ZRAM size is logical, not physical RAM.\n",
                ),
                Tier::Gpu(_) => {
                    let _ = writeln!(
                        out,
                        "    host/display share {}; runtime headroom {}; device estimate only.",
                        super::format_gib(row.host_share),
                        super::format_gib(row.runtime_headroom)
                    );
                }
                _ => {
                    let name = row
                        .volume
                        .as_ref()
                        .and_then(|key| self.volumes.iter().find(|v| &v.key == key))
                        .map_or("unavailable", |v| v.display.as_str());
                    let _ = writeln!(
                        out,
                        "    {name}; shared free-space safety floor {} (not reserved).",
                        super::format_gib(Some(DISK_RESERVE_FLOOR_BYTES))
                    );
                }
            }
        }
        if self
            .rows
            .iter()
            .filter(|row| matches!(row.tier, Tier::Gpu(_)) && row.value > 0)
            .count()
            > 1
        {
            out.push_str("GPU ceilings are separate. Simultaneous multi-GPU cache use is not yet available.\n");
        }
        out
    }
}
fn same_tier(a: &Tier, b: &Tier) -> bool {
    match (a, b) {
        (Tier::Gpu(a), Tier::Gpu(b)) => a.eq_ignore_ascii_case(b),
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;

    fn fixture() -> Editor {
        let devices = super::super::gpu_inventory::parse_inventory("GPU-a, 6144, 6144\n").unwrap();
        Editor::from_parts(
            Some(16 * GIB),
            &devices,
            vec![
                Volume {
                    key: VolumeKey::Windows("volume-a".into()),
                    display: "C:".into(),
                    free_bytes: 20 * GIB,
                    observed_unix_ms: 1_000,
                },
                Volume {
                    key: VolumeKey::Windows("volume-b".into()),
                    display: "I:".into(),
                    free_bytes: 12 * GIB,
                    observed_unix_ms: 1_000,
                },
            ],
            1_000,
        )
    }

    fn set(editor: &mut Editor, tier: Tier, value: u64) {
        let index = editor.index_of(&tier).unwrap();
        editor.rows[index].value = value;
    }

    #[test]
    fn gpu_bar_maximum_matches_physical_admission() {
        for (free_mib, expected_mib) in [(6144, 3840), (800, 0), (2304, 0), (2305, 1)] {
            let devices =
                gpu_inventory::parse_inventory(&format!("GPU-measured, 6144, {free_mib}")).unwrap();
            let mut editor = Editor::from_parts(Some(16 * GIB), &devices, Vec::new(), 1000);
            let gpu = editor.index_of(&Tier::Gpu("GPU-measured".into())).unwrap();
            assert_eq!(editor.maximum(gpu, 1000), Some(expected_mib * MIB));
            assert_eq!(editor.rows[gpu].value, expected_mib * MIB);
            assert!(!editor.adjust(gpu, true, 1000));
        }
    }

    #[test]
    fn gpu_bar_clamps_to_fresh_estimate_and_displays_reserved_share() {
        let mut editor = fixture();
        let index = editor.index_of(&Tier::Gpu("GPU-a".into())).unwrap();
        assert_eq!(editor.maximum(index, 1_000), Some(3840 * MIB));
        assert_eq!(editor.rows[index].value, 3840 * MIB);
        assert!(!editor.adjust(index, true, 1_000));
        assert!(editor.adjust(index, false, 1_000));
        assert_eq!(editor.rows[index].value, 3584 * MIB);
        let text = editor.render(1_000);
        assert!(text.contains("2.00 GiB"));
        assert!(text.contains("host/display"));
        assert!(text.contains("not applied"));
        assert!(!text.contains("Simultaneous multi-GPU"));
    }

    #[test]
    fn gpu_bars_support_measured_capacities_without_a_model_ceiling() {
        // Includes 6/8/10/12/20 GiB, the 32 GiB desktop reference and 96 GiB
        // workstation reference, plus every intermediate capacity and beyond.
        // These are synthetic byte observations, not physical qualifications.
        for total_mib in
            (1024..=128 * 1024)
                .step_by(1024)
                .chain([6143, 32767, 98303, 192 * 1024, 256 * 1024])
        {
            for free_mib in [0, 100, 800, total_mib / 2, total_mib] {
                let devices = gpu_inventory::parse_inventory(&format!(
                    "GPU-measured, {total_mib}, {free_mib}"
                ))
                .unwrap();
                let mut editor = Editor::from_parts(Some(16 * GIB), &devices, Vec::new(), 1000);
                let gpu = editor.index_of(&Tier::Gpu("GPU-measured".into())).unwrap();
                let maximum = editor.maximum(gpu, 1000).unwrap();
                assert!(
                    maximum
                        <= (free_mib * MIB).saturating_sub(
                            editor.rows[gpu].host_share.unwrap()
                                + ramshared_vram::RUNTIME_FREE_BUFFER_BYTES
                        )
                );
                assert!(
                    maximum
                        <= (total_mib * MIB).saturating_sub(editor.rows[gpu].host_share.unwrap())
                );
                assert_eq!(maximum % MIB, 0);
                assert!(
                    !editor.adjust(gpu, true, 1000),
                    "cannot exceed this adapter's safe estimate"
                );
                editor.selected = gpu;
                editor.toggle_preset(1000);
                assert!(editor.rows[gpu].value <= maximum);
                editor.fill_selected(1000);
                assert_eq!(editor.rows[gpu].value, maximum);
                assert_eq!(
                    editor.maximum(gpu, 1001 + ramshared_vram::GPU_BUDGET_MAX_AGE_MS),
                    None
                );
            }
        }
    }

    #[test]
    fn multiple_gpu_bars_keep_identity_budget_and_reserve_independent() {
        let now = super::super::unix_millis();
        let mut snapshot = super::super::tests::fixture_snapshot();
        snapshot.gpu_inventory = gpu_inventory::parse_inventory(
            "GPU-workstation, 98304, 98304\nGPU-desktop, 32768, 12288",
        )
        .unwrap();
        snapshot.gpu_observed_unix_ms = now;
        let mut editor = Editor::from_snapshot(&snapshot, now);
        let workstation = Tier::Gpu("GPU-workstation".into());
        let desktop = Tier::Gpu("GPU-desktop".into());
        let wi = editor.index_of(&workstation).unwrap();
        let di = editor.index_of(&desktop).unwrap();
        assert!(editor.maximum(wi, now).unwrap() > 32 * GIB);
        assert!(editor.maximum(di, now).unwrap() < 12 * GIB);
        assert!(editor.rows[wi].host_share.unwrap() > editor.rows[di].host_share.unwrap());
        set(&mut editor, workstation.clone(), 40 * GIB);
        set(&mut editor, desktop.clone(), 512 * MIB);
        let expected = editor
            .profile(&snapshot, now)
            .unwrap()
            .planned_caps
            .vram_bytes;
        assert!(
            editor
                .render(now)
                .contains("Simultaneous multi-GPU cache use is not yet available")
        );
        assert_eq!(expected.len(), 2);
        assert_eq!(
            expected.get("nvidia-uuid:gpu-workstation"),
            Some(&(40 * GIB))
        );
        assert_eq!(expected.get("nvidia-uuid:gpu-desktop"), Some(&(512 * MIB)));
        snapshot.gpu_inventory.reverse();
        editor.reconcile(Editor::from_snapshot(&snapshot, now));
        assert_eq!(
            editor
                .profile(&snapshot, now)
                .unwrap()
                .planned_caps
                .vram_bytes,
            expected
        );
        // A missing adapter cannot donate its capacity to the remaining GPU.
        snapshot
            .gpu_inventory
            .retain(|gpu| gpu.uuid == "GPU-desktop");
        editor.reconcile(Editor::from_snapshot(&snapshot, now));
        assert!(editor.profile(&snapshot, now).is_err());
        let di = editor.index_of(&desktop).unwrap();
        assert!(editor.maximum(di, now).unwrap() < 12 * GIB);
        assert_eq!(editor.rows[di].value, 512 * MIB);
    }

    #[test]
    fn shared_disk_caps_count_all_three_tiers_and_one_reserve() {
        let mut editor = fixture();
        set(&mut editor, Tier::Origin, 6 * GIB);
        set(&mut editor, Tier::Fallback, 3 * GIB);
        set(&mut editor, Tier::Emergency, GIB);
        let emergency = editor.index_of(&Tier::Emergency).unwrap();
        assert_eq!(editor.maximum(emergency, 1_000), Some(GIB));
        assert!(!editor.adjust(emergency, true, 1_000));
        let origin = editor.index_of(&Tier::Origin).unwrap();
        assert!(editor.adjust(origin, false, 1_000));
        assert!(editor.adjust(emergency, true, 1_000));
        assert_eq!(editor.rows[emergency].value, GIB + 256 * MIB);
    }

    #[test]
    fn stale_or_missing_limits_block_increases_but_allow_reductions() {
        let mut editor = fixture();
        let gpu = editor.index_of(&Tier::Gpu("GPU-a".into())).unwrap();
        assert_eq!(editor.maximum(gpu, 7_000), None);
        assert!(!editor.adjust(gpu, true, 7_000));
        assert!(editor.adjust(gpu, false, 7_000));
        let origin = editor.index_of(&Tier::Origin).unwrap();
        assert_eq!(editor.maximum(origin, 32_000), None);
        let empty = Editor::from_parts(None, &[], Vec::new(), 1_000);
        assert_eq!(
            empty.maximum(empty.index_of(&Tier::Zram).unwrap(), 1_000),
            None
        );
        assert_eq!(
            empty.maximum(empty.index_of(&Tier::Origin).unwrap(), 1_000),
            None
        );
    }

    #[test]
    fn changing_volume_refuses_insufficient_space_and_refresh_preserves_values() {
        let mut editor = fixture();
        set(&mut editor, Tier::Origin, 6 * GIB);
        let origin = editor.index_of(&Tier::Origin).unwrap();
        assert!(!editor.cycle_volume(origin, 1_000));
        assert_eq!(editor.rows[origin].value, 6 * GIB);
        assert_eq!(
            editor.rows[origin].volume,
            Some(VolumeKey::Windows("volume-a".into()))
        );
        let mut lower = fixture();
        lower.volumes[0].free_bytes = 12 * GIB;
        editor.reconcile(lower);
        assert_eq!(editor.rows[origin].value, 6 * GIB);
        assert_eq!(editor.maximum(origin, 1_000), Some(2 * GIB));
        assert!(editor.render(1_000).contains("above current limit"));
    }

    #[test]
    fn missing_volume_after_refresh_keeps_the_old_choice_and_blocks_increase() {
        let mut editor = fixture();
        set(&mut editor, Tier::Origin, GIB);
        let origin = editor.index_of(&Tier::Origin).unwrap();
        let mut update = fixture();
        update.volumes.remove(0);
        editor.reconcile(update);
        assert_eq!(editor.rows[origin].value, GIB);
        assert_eq!(editor.maximum(origin, 1_000), None);
        assert!(!editor.adjust(origin, true, 1_000));
        assert!(editor.adjust(origin, false, 1_000));
        assert!(editor.cycle_volume(origin, 1_000));
        assert_eq!(
            editor.rows[origin].volume,
            Some(VolumeKey::Windows("volume-b".into()))
        );
    }

    #[test]
    fn overflow_and_future_samples_never_authorize_an_increase() {
        let mut editor = fixture();
        set(&mut editor, Tier::Fallback, u64::MAX);
        set(&mut editor, Tier::Emergency, GIB);
        let origin = editor.index_of(&Tier::Origin).unwrap();
        assert_eq!(editor.maximum(origin, 1_000), None);
        assert!(!editor.adjust(origin, true, 1_000));
        let gpu = editor.index_of(&Tier::Gpu("GPU-a".into())).unwrap();
        assert_eq!(editor.maximum(gpu, 999), None);
        assert_eq!(editor.maximum(gpu, 0), None);
    }

    #[test]
    fn storage_bars_share_provider_clock_skew_and_staleness_policy() {
        let mut editor = fixture();
        let origin = editor.index_of(&Tier::Origin).unwrap();
        let now = 1_000;
        let current = editor.maximum(origin, now).unwrap();
        editor.volumes[0].observed_unix_ms = now + super::super::STORAGE_SAMPLE_FUTURE_TOLERANCE_MS;
        assert!(super::super::sample_is_fresh(
            Some(editor.volumes[0].observed_unix_ms),
            now
        ));
        assert_eq!(editor.maximum(origin, now), Some(current));
        editor.volumes[0].observed_unix_ms += 1;
        assert_eq!(editor.maximum(origin, now), None);
        editor.volumes[0].observed_unix_ms = 1;
        assert_eq!(
            editor.maximum(origin, 2 + super::super::STORAGE_SAMPLE_MAX_AGE_MS),
            None
        );
        editor.volumes[0].observed_unix_ms = 0;
        assert!(!super::super::sample_is_fresh(Some(0), now));
        assert_eq!(editor.maximum(origin, now), None);
        assert_eq!(editor.maximum(origin, 0), None);
    }

    #[test]
    fn volume_change_and_preset_preserve_independent_bars() {
        let mut editor = fixture();
        let origin = editor.index_of(&Tier::Origin).unwrap();
        editor.rows[origin].value = GIB;
        assert!(editor.cycle_volume(origin, 1_000));
        assert_eq!(
            editor.rows[origin].volume,
            Some(VolumeKey::Windows("volume-b".into()))
        );
        editor.toggle_preset(1_000);
        let gpu = editor.index_of(&Tier::Gpu("GPU-a".into())).unwrap();
        assert_eq!(editor.rows[gpu].value, 1920 * MIB);
        assert_eq!(editor.rows[origin].value, GIB);
        editor.toggle_preset(7_000);
        assert_eq!(
            editor.rows[gpu].value,
            1920 * MIB,
            "stale presets must not raise the value"
        );
    }
    #[test]
    fn profile_conversion_preserves_all_requested_tiers_and_refuses_disappeared_gpu() {
        let mut snapshot = super::super::tests::fixture_snapshot();
        let now = super::super::unix_millis();
        snapshot.gpu_inventory = gpu_inventory::parse_inventory("GPU-a, 6144, 6144").unwrap();
        snapshot.gpu_observed_unix_ms = now;
        let mut editor = Editor::from_snapshot(&snapshot, now);
        set(&mut editor, Tier::Zram, GIB);
        for tier in [Tier::Origin, Tier::Fallback, Tier::Emergency] {
            set(&mut editor, tier, GIB);
        }
        let profile = editor.profile(&snapshot, now).unwrap();
        assert_eq!(profile.planned_caps.zram_bytes, Some(GIB));
        assert_eq!(
            profile.planned_caps.vram_bytes.get("nvidia-uuid:gpu-a"),
            Some(&(3840 * MIB))
        );
        assert_eq!(profile.targets.len(), 3);
        let text = profile.to_toml().unwrap();
        assert_eq!(
            super::super::ResourceProfile::parse(&text).unwrap(),
            profile
        );
        assert!(editor.profile(&snapshot, now + 31_000).is_err());
        snapshot.gpu_inventory.clear();
        editor.reconcile(Editor::from_snapshot(&snapshot, now));
        assert!(editor.index_of(&Tier::Gpu("GPU-a".into())).is_some());
        assert!(editor.profile(&snapshot, now).is_err());
        let gpu = editor.index_of(&Tier::Gpu("GPU-a".into())).unwrap();
        editor.rows[gpu].value = 0;
        assert!(editor.profile(&snapshot, now).is_ok());
    }

    #[test]
    fn profile_refuses_unknown_volume_or_invalid_selected_size() {
        let snapshot = super::super::tests::fixture_snapshot();
        let now = super::super::unix_millis();
        let mut editor = Editor::from_snapshot(&snapshot, now);
        let origin = editor.index_of(&Tier::Origin).unwrap();
        editor.rows[origin].value = 1;
        assert!(editor.profile(&snapshot, now).is_err());
        editor.rows[origin].value = GIB;
        editor.rows[origin].volume = Some(VolumeKey::Windows("missing".into()));
        assert!(editor.profile(&snapshot, now).is_err());
    }
    #[test]
    fn explicit_home_and_end_correct_loaded_excess_without_using_stale_limits() {
        let snapshot = super::super::tests::fixture_snapshot();
        let now = super::super::unix_millis();
        let mut editor = Editor::from_snapshot(&snapshot, now);
        editor.rows[0].value = u64::MAX;
        editor.fill_selected(now);
        assert!(editor.rows[0].value <= snapshot.guest_memory.total_bytes.expect("RAM"));
        assert_eq!(editor.rows[0].value % MIB, 0);
        editor.clear_selected();
        assert_eq!(editor.rows[0].value, 0);
        editor.fill_selected(now + 31_000);
        assert_eq!(editor.rows[0].value, 0);
    }
}
