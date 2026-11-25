// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/tier_editor/prepared.rs
//! Immutable prepared-origin draft binding and existing-allocation accounting.

use super::{Editor, Row, Tier, VolumeKey, fresh};
use crate::resource_config::{self as config, InventoryError, ResourceSnapshot, ResourceTarget};

#[derive(Clone, Debug)]
enum Fault {
    Transient(String),
    Review(String),
}

impl Fault {
    fn detail(&self) -> &str {
        let (Self::Transient(detail) | Self::Review(detail)) = self;
        detail
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct PreparedBar {
    identity: Option<ResourceTarget>,
    desired: Option<ResourceTarget>,
    observed_unix_ms: Option<u64>,
    fault: Option<Fault>,
}

fn refused(detail: impl Into<String>) -> InventoryError {
    InventoryError(format!("prepared origin bar: {}", detail.into()))
}

fn volume_key(target: &ResourceTarget) -> Option<VolumeKey> {
    if let ResourceTarget::LinuxFileOrigin {
        filesystem_uuid,
        device_identity,
        ..
    } = target
    {
        Some(VolumeKey::Linux {
            filesystem_uuid: filesystem_uuid.clone(),
            device_identity: device_identity.clone(),
        })
    } else {
        None
    }
}

impl PreparedBar {
    fn observe(snapshot: &ResourceSnapshot, now: u64) -> Self {
        match config::prepared_origin::verified(snapshot, now) {
            Ok(Some(proof)) => Self::bound(proof.target().clone(), now),
            Ok(None) => Self::default(),
            Err(error) => Self {
                fault: Some(Fault::Transient(error.to_string())),
                ..Self::default()
            },
        }
    }

    /// Bind metadata already verified by the observation owner. Controller
    /// tests supply explicit metadata; saving always revalidates the real fd.
    fn bound(target: ResourceTarget, now: u64) -> Self {
        if volume_key(&target).is_none() {
            return Self {
                fault: Some(Fault::Review("a sealed native identity is required".into())),
                ..Self::default()
            };
        }
        Self {
            identity: Some(target.clone()),
            desired: Some(target),
            observed_unix_ms: Some(now),
            fault: None,
        }
    }

    pub(super) fn locked(&self) -> bool {
        self.identity.is_some() || self.fault.is_some()
    }

    fn valid(&self, now: u64) -> bool {
        self.fault.is_none()
            && self
                .observed_unix_ms
                .is_some_and(|sample| fresh(sample, now, config::STORAGE_SAMPLE_MAX_AGE_MS))
    }

    pub(super) fn seed(&self, row: &mut Row) {
        if let Some(identity) = &self.identity
            && self.fault.is_none()
        {
            row.volume = volume_key(identity);
            row.value = self
                .desired
                .as_ref()
                .map_or(0, ResourceTarget::allocated_bytes);
        }
    }

    fn matches_row(&self, row: &Row) -> bool {
        self.identity.as_ref().is_some_and(|identity| {
            row.volume == volume_key(identity)
                && row.value
                    == self
                        .desired
                        .as_ref()
                        .map_or(0, ResourceTarget::allocated_bytes)
        })
    }

    pub(super) fn maximum(&self, row: &Row, now: u64) -> Option<u64> {
        (self.valid(now) && self.matches_row(row))
            .then(|| self.identity.as_ref().map(ResourceTarget::allocated_bytes))
            .flatten()
    }

    pub(super) fn new_bytes(&self, row: &Row, now: u64) -> Option<u64> {
        if row.tier == Tier::Origin && self.locked() {
            return self.maximum(row, now).map(|_| 0);
        }
        Some(row.value)
    }

    pub(super) fn omit(&mut self) {
        self.desired = None;
    }
    pub(super) fn desired(&self) -> Option<&ResourceTarget> {
        self.desired.as_ref()
    }

    fn validate_current(
        &self,
        row: &Row,
        current: Option<&ResourceTarget>,
        now: u64,
    ) -> Result<(), InventoryError> {
        if let Some(fault) = &self.fault {
            return Err(refused(fault.detail()));
        }
        match (&self.identity, current) {
            (None, None) => Ok(()),
            (Some(expected), Some(current))
                if expected == current && self.valid(now) && self.matches_row(row) =>
            {
                Ok(())
            }
            _ => Err(refused(
                "identity, desired geometry or proof changed; refresh or reopen the reviewed profile",
            )),
        }
    }

    pub(super) fn validate(
        &self,
        snapshot: &ResourceSnapshot,
        row: &Row,
        now: u64,
    ) -> Result<(), InventoryError> {
        let proof = config::prepared_origin::verified(snapshot, now)?;
        self.validate_current(row, proof.as_ref().map(|proof| proof.target()), now)
    }

    fn restore(
        &mut self,
        snapshot: &ResourceSnapshot,
        target: &ResourceTarget,
        row: &mut Row,
        now: u64,
    ) -> Result<bool, InventoryError> {
        if !matches!(
            target,
            ResourceTarget::LinuxFileOrigin { .. } | ResourceTarget::LinuxFileOriginRequest { .. }
        ) {
            return Ok(false);
        }
        if !self.locked() {
            return Ok(false);
        }
        let proof = config::prepared_origin::verified(snapshot, now)?
            .ok_or_else(|| refused("prepared identity is unavailable"))?;
        self.validate_current(row, Some(proof.target()), now)?;
        if !proof.accepts(target) {
            return Err(refused(
                "size, path or identity differs from the prepared origin",
            ));
        }
        self.desired = Some(target.clone());
        self.seed(row);
        Ok(true)
    }

    fn reconcile(&mut self, previous: &Self, previous_row: Option<&Row>) {
        if let Some(expected) = &previous.identity {
            let same = self.identity.as_ref() == Some(expected);
            if same && self.fault.is_none() && !matches!(previous.fault, Some(Fault::Review(_))) {
                self.desired.clone_from(&previous.desired);
                return;
            }
            let fault = if matches!(previous.fault, Some(Fault::Review(_))) {
                previous.fault.clone()
            } else if self.identity.is_some() && !same {
                Some(Fault::Review(
                    "prepared identity changed; preserve the old binding and review recovery"
                        .into(),
                ))
            } else {
                let detail = self
                    .fault
                    .as_ref()
                    .map_or("prepared proof disappeared", Fault::detail);
                Some(Fault::Transient(format!("{detail}; binding preserved")))
            };
            *self = previous.clone();
            self.observed_unix_ms = None;
            self.fault = fault;
        } else if self.identity.is_some() {
            if previous_row.is_some_and(|row| row.value != 0) {
                self.fault = Some(Fault::Review(
                    "origin was prepared while a draft was open; reopen the reviewed profile"
                        .into(),
                ));
            } else {
                self.desired = None;
            }
        } else if self.fault.is_none() {
            self.fault.clone_from(&previous.fault);
        }
    }

    pub(super) fn render(&self, now: u64) -> String {
        if !self.locked() {
            return String::new();
        }
        let mut text = String::from("    Prepared origin: size and path locked.\n");
        if let Some(
            target @ ResourceTarget::LinuxFileOrigin {
                managed_relative_path,
                ..
            },
        ) = &self.identity
        {
            text.push_str(&format!(
                "    Existing allocation {}; path {}.{}\n",
                display_bytes(Some(target.allocated_bytes())),
                managed_relative_path,
                if self.desired.is_none() {
                    " Omitted from this desired profile."
                } else {
                    ""
                }
            ));
        }
        if let Some(fault) = &self.fault {
            text.push_str(&format!("    {}\n", fault.detail()));
        } else if !self.valid(now) {
            text.push_str("    Prepared identity proof is stale; refresh before saving.\n");
        } else {
            text.push_str("    Existing allocation is already reflected in free space.\n");
        }
        text
    }
}

impl Editor {
    pub(super) fn observe_prepared(&mut self, snapshot: &ResourceSnapshot, now: u64) {
        self.prepared = PreparedBar::observe(snapshot, now);
        if let Some(row) = self.rows.iter_mut().find(|row| row.tier == Tier::Origin) {
            self.prepared.seed(row);
        }
    }
    pub(super) fn reconcile_prepared(&mut self, previous: &Self) {
        self.prepared.reconcile(
            &previous.prepared,
            previous.rows.iter().find(|row| row.tier == Tier::Origin),
        );
        if let Some(row) = self.rows.iter_mut().find(|row| row.tier == Tier::Origin) {
            self.prepared.seed(row);
        }
    }
    pub(super) fn restore_prepared(
        &mut self,
        snapshot: &ResourceSnapshot,
        target: &ResourceTarget,
        now: u64,
    ) -> Result<bool, InventoryError> {
        let row = self
            .rows
            .iter_mut()
            .find(|row| row.tier == Tier::Origin)
            .ok_or_else(|| refused("origin row is absent"))?;
        self.prepared.restore(snapshot, target, row, now)
    }
}

pub(super) fn display_bytes(bytes: Option<u64>) -> String {
    let Some(bytes) = bytes else {
        return "unavailable".into();
    };
    if bytes >= 1024 * super::MIB {
        config::format_gib(Some(bytes))
    } else if bytes >= super::MIB {
        format!("{:.2} MiB", bytes as f64 / super::MIB as f64)
    } else if bytes >= 1024 {
        format!("{:.2} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::resource_config::tier_editor::Volume;
    use ramshared_config::resource_profile::DISK_RESERVE_FLOOR_BYTES;
    const GIB: u64 = 1024 * super::super::MIB;
    fn fixture(now: u64) -> Editor {
        Editor::from_parts(
            Some(16 * GIB),
            &[],
            vec![
                Volume {
                    key: VolumeKey::Linux {
                        filesystem_uuid: "fs".into(),
                        device_identity: "device".into(),
                    },
                    display: "fixture".into(),
                    free_bytes: DISK_RESERVE_FLOOR_BYTES + GIB,
                    observed_unix_ms: now,
                },
                Volume {
                    key: VolumeKey::Linux {
                        filesystem_uuid: "other".into(),
                        device_identity: "other".into(),
                    },
                    display: "other".into(),
                    free_bytes: DISK_RESERVE_FLOOR_BYTES + GIB,
                    observed_unix_ms: now,
                },
            ],
            now,
        )
    }
    #[test]
    fn prepared_bar_keeps_geometry_and_credits_only_existing_allocation() {
        let now = config::unix_millis();
        let mut editor = fixture(now);
        let origin = editor.index_of(&Tier::Origin).unwrap();
        let key = VolumeKey::Linux {
            filesystem_uuid: "fs".into(),
            device_identity: "device".into(),
        };
        editor.volumes[0].key = key.clone();
        editor.volumes[0].free_bytes = DISK_RESERVE_FLOOR_BYTES + GIB;
        let target = ResourceTarget::LinuxFileOrigin {
            filesystem_uuid: "fs".into(),
            device_identity: "device".into(),
            managed_relative_path: "origin/preserved.img".into(),
            inode: 42,
            allocated_bytes: 4 * GIB,
            identity_field_hash: "a".repeat(64),
        };
        editor.prepared = PreparedBar::bound(target.clone(), now);
        editor.prepared.seed(&mut editor.rows[origin]);
        editor.selected = origin;
        let before = editor.rows[origin].clone();
        editor.clear_selected();
        editor.fill_selected(now);
        editor.adjust_selected(true, now);
        editor.adjust_selected(false, now);
        editor.cycle_selected(now);
        assert_eq!(editor.rows[origin].value, before.value);
        assert_eq!(editor.rows[origin].volume, before.volume);
        assert_eq!(editor.maximum(origin, now), Some(4 * GIB));
        let fallback = editor.index_of(&Tier::Fallback).unwrap();
        editor.rows[fallback].volume = Some(key);
        assert_eq!(editor.maximum(fallback, now), Some(GIB));
        assert!(editor.render(now).contains("size and path locked"));
    }

    fn target(bytes: u64, inode: u64) -> ResourceTarget {
        ResourceTarget::LinuxFileOrigin {
            filesystem_uuid: "fs".into(),
            device_identity: "device".into(),
            managed_relative_path: "origin/preserved.img".into(),
            inode,
            allocated_bytes: bytes,
            identity_field_hash: "a".repeat(64),
        }
    }

    fn bound_editor(now: u64, identity: ResourceTarget) -> Editor {
        let mut editor = fixture(now);
        editor.prepared = PreparedBar::bound(identity, now);
        let row = editor.index_of(&Tier::Origin).unwrap();
        editor.prepared.seed(&mut editor.rows[row]);
        editor
    }

    #[test]
    fn prepared_bar_refresh_preserves_identity_desired_shape_and_uncertainty() {
        let now = config::unix_millis();
        let identity = target(4 * GIB, 42);
        let request = ResourceTarget::LinuxFileOriginRequest {
            filesystem_uuid: "fs".into(),
            device_identity: "device".into(),
            managed_relative_path: "origin/preserved.img".into(),
            allocated_bytes: 4 * GIB,
        };
        let mut editor = bound_editor(now, identity.clone());
        editor.prepared.desired = Some(request.clone());
        editor.origin_cap_override = Some(None);
        let row = editor.index_of(&Tier::Origin).unwrap();
        editor.reconcile(bound_editor(now, identity.clone()));
        assert_eq!(editor.prepared.desired(), Some(&request));
        assert_eq!(editor.origin_cap_override, Some(None));
        editor.reconcile(fixture(now));
        assert!(editor.prepared.locked());
        assert_eq!(editor.rows[row].value, 4 * GIB);
        assert!(editor.maximum(row, now).is_none());
        assert!(editor.render(now).contains("binding preserved"));
        let mut unavailable = fixture(now);
        unavailable.prepared.fault = Some(Fault::Transient("unreadable".into()));
        editor.reconcile(unavailable);
        assert_eq!(editor.prepared.desired(), Some(&request));
        editor.reconcile(bound_editor(now, identity.clone()));
        assert!(editor.prepared.fault.is_none());
        assert_eq!(editor.maximum(row, now), Some(4 * GIB));
        editor.reconcile(bound_editor(now, target(4 * GIB, 43)));
        assert_eq!(editor.prepared.identity.as_ref(), Some(&identity));
        assert!(matches!(editor.prepared.fault, Some(Fault::Review(_))));
        assert!(editor.render(now).contains("prepared identity changed"));
        editor.reconcile(bound_editor(now, identity.clone()));
        assert!(matches!(editor.prepared.fault, Some(Fault::Review(_))));
        assert_eq!(editor.prepared.desired(), Some(&request));

        let mut unknown = fixture(now);
        unknown.prepared.fault = Some(Fault::Transient("unknown".into()));
        unknown.reconcile(fixture(now));
        assert!(unknown.prepared.locked());
        unknown.reconcile(bound_editor(now, identity.clone()));
        assert!(unknown.prepared.fault.is_none());
        assert!(unknown.prepared.desired().is_none());
        assert_eq!(unknown.rows[row].value, 0);
        assert!(
            unknown
                .render(now)
                .contains("Omitted from this desired profile")
        );

        let mut outstanding = fixture(now);
        outstanding.rows[row].value = GIB;
        outstanding.reconcile(bound_editor(now, identity));
        assert_eq!(outstanding.rows[row].value, GIB);
        assert!(matches!(outstanding.prepared.fault, Some(Fault::Review(_))));
        outstanding.clear_selected();
        assert!(outstanding.prepared.locked());
    }

    #[test]
    fn prepared_bar_profile_policy_refuses_missing_changed_or_unselected_geometry() {
        let now = config::unix_millis();
        let identity = target(8192, 42);
        let mut editor = bound_editor(now, identity.clone());
        let index = editor.index_of(&Tier::Origin).unwrap();
        let row = editor.rows[index].clone();
        assert!(
            editor
                .prepared
                .validate_current(&row, Some(&identity), now)
                .is_ok()
        );
        assert!(editor.prepared.validate_current(&row, None, now).is_err());
        assert!(
            editor
                .prepared
                .validate_current(&row, Some(&target(8192, 43)), now)
                .is_err()
        );
        assert!(
            editor
                .prepared
                .validate_current(
                    &row,
                    Some(&identity),
                    now + config::STORAGE_SAMPLE_MAX_AGE_MS + 1
                )
                .is_err()
        );
        let mut wrong = row.clone();
        wrong.value = 0;
        assert!(
            editor
                .prepared
                .validate_current(&wrong, Some(&identity), now)
                .is_err()
        );
        wrong = row.clone();
        wrong.volume = None;
        assert!(
            editor
                .prepared
                .validate_current(&wrong, Some(&identity), now)
                .is_err()
        );
        editor.selected = index;
        editor.fill_selected(now);
        editor.clear_selected();
        assert_eq!(editor.rows[index].value, 8192);
        assert!(editor.render(now).contains("8.00 KiB"));
        assert!(
            editor
                .render(now + config::STORAGE_SAMPLE_MAX_AGE_MS + 1)
                .contains("proof is stale")
        );
        editor.prepared.omit();
        editor.prepared.seed(&mut editor.rows[index]);
        assert!(
            editor
                .prepared
                .validate_current(&editor.rows[index], Some(&identity), now)
                .is_ok()
        );
        let empty = PreparedBar::default();
        assert!(empty.validate_current(&row, None, now).is_ok());
        assert!(empty.validate_current(&row, Some(&identity), now).is_err());
        assert_eq!(display_bytes(None), "unavailable");
        assert_eq!(display_bytes(Some(512)), "512 B");
        assert_eq!(display_bytes(Some(super::super::MIB)), "1.00 MiB");
        let invalid = ResourceTarget::LinuxFileOriginRequest {
            filesystem_uuid: "fs".into(),
            device_identity: "device".into(),
            managed_relative_path: "origin/x".into(),
            allocated_bytes: 8192,
        };
        assert!(PreparedBar::bound(invalid, now).locked());
    }
}
