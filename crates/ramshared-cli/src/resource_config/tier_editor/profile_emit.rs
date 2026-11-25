// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/tier_editor/profile_emit.rs
//! Preserve requested caps and exact storage targets when emitting a draft.

use super::{Editor, MIB, Row, Tier, VolumeKey};
use crate::resource_config::{
    self as config, DraftTargetRole, InventoryError, ResourceProfile, ResourceSnapshot,
    ResourceTarget,
};

impl Editor {
    pub(in crate::resource_config) fn profile(
        &self,
        snapshot: &ResourceSnapshot,
        now: u64,
    ) -> Result<ResourceProfile, InventoryError> {
        let mut profile = ResourceProfile {
            schema_version: config::RESOURCE_PROFILE_SCHEMA_VERSION,
            planned_caps: config::PlannedTierCaps::default(),
            targets: Vec::new(),
        };
        let candidates = config::draft_volume_candidates(snapshot, now);
        let origin_row = self
            .rows
            .iter()
            .find(|row| row.tier == Tier::Origin)
            .ok_or_else(|| InventoryError("origin row is absent".into()))?;
        self.prepared.validate(snapshot, origin_row, now)?;
        for (index, row) in self.rows.iter().enumerate() {
            if row.tier == Tier::Origin && self.prepared.locked() {
                if let Some(target) = self.prepared.desired() {
                    profile.targets.push(target.clone());
                }
                continue;
            }
            if row.value == 0 {
                continue;
            }
            if row.value % MIB != 0
                || self
                    .maximum(index, now)
                    .is_none_or(|limit| row.value > limit)
            {
                return Err(InventoryError("a selected tier exceeds its current limit or has no fresh observation; refresh or reduce it".into()));
            }
            if let Some(role) = record_row(&mut profile, row, snapshot)? {
                profile
                    .targets
                    .push(storage_target(row, role, &candidates)?);
            }
        }
        if let Some(cap) = self.origin_cap_override {
            profile.planned_caps.origin_bytes = cap;
        }
        profile
            .validate_for(config::profile_platform(snapshot.platform))
            .map_err(|error| InventoryError(error.to_string()))?;
        Ok(profile)
    }
}

fn record_row(
    profile: &mut ResourceProfile,
    row: &Row,
    snapshot: &ResourceSnapshot,
) -> Result<Option<DraftTargetRole>, InventoryError> {
    Ok(match &row.tier {
        Tier::Zram => {
            profile.planned_caps.zram_bytes = Some(row.value);
            None
        }
        Tier::Gpu(uuid) => {
            if !snapshot
                .gpu_inventory
                .iter()
                .any(|gpu| gpu.uuid.eq_ignore_ascii_case(uuid))
            {
                return Err(InventoryError("selected GPU is no longer detected".into()));
            }
            profile
                .planned_caps
                .vram_bytes
                .insert(config::gpu_inventory::profile_key(uuid), row.value);
            None
        }
        Tier::Origin => {
            profile.planned_caps.origin_bytes = Some(row.value);
            Some(DraftTargetRole::RamSharedOrigin)
        }
        Tier::Fallback => Some(DraftTargetRole::FallbackSwap),
        Tier::Emergency => Some(DraftTargetRole::Emergency),
    })
}

fn storage_target(
    row: &Row,
    role: DraftTargetRole,
    candidates: &[config::DraftVolumeCandidate],
) -> Result<ResourceTarget, InventoryError> {
    let candidate = candidates
        .iter()
        .find(|candidate| match (&row.volume, &candidate.storage) {
            (
                Some(VolumeKey::Linux {
                    filesystem_uuid: a,
                    device_identity: b,
                }),
                config::DraftStorageIdentity::NativeLinux {
                    filesystem_uuid: c,
                    device_identity: d,
                },
            ) => a == c && b == d,
            (
                Some(VolumeKey::Windows(a)),
                config::DraftStorageIdentity::Wsl2 { volume_id: b, .. },
            ) => a.eq_ignore_ascii_case(b),
            _ => false,
        })
        .ok_or_else(|| InventoryError("selected volume is no longer eligible".into()))?;
    config::materialize_draft_target(candidate, role, row.value)
}
