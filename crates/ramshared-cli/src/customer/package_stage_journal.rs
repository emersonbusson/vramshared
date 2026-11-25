// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_stage_journal.rs
//! Durable intent for one package staging effect.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use super::CustomerError;
use super::package_source::DeclaredBundle;
use super::package_stage::StageSlot;
use super::transition::{
    CustomerTransitionGuard, OperationOwner, PackageAction, PackagePending, TransitionPaths,
    read_journal_body, read_pending, recovery_refusal,
};

pub(super) mod inventory;

#[cfg(test)]
use inventory::journal_capacity;

const JOURNAL: &str = "package-stage.json";

/// Any existing or unreadable record prevents a fresh mutation.
pub(super) fn stage_journal_present(paths: &TransitionPaths) -> Result<bool, CustomerError> {
    read_journal_body(paths, OsStr::new(JOURNAL), "package stage journal")
        .map(|body| body.is_some())
        .map_err(|_| recovery_refusal("package stage journal is unavailable; retain evidence"))
}

/// Remove the stage journal only under `OperationOwner::Package` for a matching transaction.
pub(super) fn clear_stage_journal(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
) -> Result<(), CustomerError> {
    if guard.owner() != OperationOwner::Package {
        return Err(recovery_refusal(
            "clearing stage journal requires package transition owner",
        ));
    }
    guard.revalidate()?;
    if read_pending(guard.paths())?.as_ref() != Some(pending) {
        return Err(recovery_refusal(
            "pending record does not match stage journal clear target",
        ));
    }
    if let Some(history) = read_stage_history(guard.paths())?
        && history.iter().any(|entry| &entry.pending != pending)
    {
        return Err(recovery_refusal(
            "stage journal belongs to another transaction; retain evidence",
        ));
    }
    super::transition_custody::clear_journal_file(
        guard,
        OsStr::new(JOURNAL),
        "package stage journal",
    )
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum StagePhase {
    Intent,
    Opened,
    Verified,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StageJournal {
    pending: PackagePending,
    source_boot_id: String,
    source_device: u64,
    source_inode: u64,
    pub(super) role: String,
    parent: PathBuf,
    name: String,
    limit: u64,
    pub(super) phase: StagePhase,
    parent_device: Option<u64>,
    parent_inode: Option<u64>,
    device: Option<u64>,
    pub(super) inode: Option<u64>,
}

pub(super) struct StageRequest<'a> {
    pub(super) pending: &'a PackagePending,
    pub(super) bundle: &'a DeclaredBundle,
    pub(super) role: &'a str,
    pub(super) parent: &'a Path,
    pub(super) name: &'a str,
    pub(super) limit: u64,
}

pub(super) fn read_stage_journal(
    paths: &TransitionPaths,
) -> Result<Option<StageJournal>, CustomerError> {
    Ok(read_stage_history(paths)?.and_then(|mut history| history.pop()))
}

/// Canonical validated evidence for the read-only reconciliation observation.
pub(super) fn reconciliation_evidence(paths: &TransitionPaths) -> Result<Vec<u8>, CustomerError> {
    let history = read_stage_history(paths)?
        .ok_or_else(|| recovery_refusal("package stage history is absent"))?;
    serde_json::to_vec(&history)
        .map_err(|_| recovery_refusal("cannot encode package stage evidence"))
}

fn read_stage_history(paths: &TransitionPaths) -> Result<Option<Vec<StageJournal>>, CustomerError> {
    let Some(body) = read_journal_body(paths, OsStr::new(JOURNAL), "package stage journal")? else {
        return Ok(None);
    };
    if !body.ends_with('\n') {
        return Err(recovery_refusal(
            "package stage journal has an incomplete record; retain evidence",
        ));
    }
    let mut previous: Option<StageJournal> = None;
    let mut roles = BTreeSet::new();
    let mut slots = BTreeSet::new();
    let mut history = Vec::new();
    for line in body.lines() {
        let journal: StageJournal = serde_json::from_str(line).map_err(|_| {
            recovery_refusal("package stage journal is malformed; retain the staging evidence")
        })?;
        validate_journal(&journal)?;
        if let Some(prior) = &previous {
            let next_member =
                prior.phase == StagePhase::Verified && journal.phase == StagePhase::Intent;
            let next_phase = matches!(
                (prior.phase, journal.phase),
                (StagePhase::Intent, StagePhase::Opened)
                    | (StagePhase::Opened, StagePhase::Verified)
            );
            let same_member = prior.role == journal.role
                && prior.parent == journal.parent
                && prior.name == journal.name
                && prior.limit == journal.limit;
            let same_parent_identity = prior.phase != StagePhase::Intent
                || prior.parent_device.is_none()
                || (prior.parent_device == journal.parent_device
                    && prior.parent_inode == journal.parent_inode);
            let same_opened_identity = prior.phase != StagePhase::Opened
                || (prior.device == journal.device
                    && prior.inode == journal.inode
                    && prior.parent_device == journal.parent_device
                    && prior.parent_inode == journal.parent_inode);
            if prior.pending != journal.pending
                || prior.source_boot_id != journal.source_boot_id
                || prior.source_device != journal.source_device
                || prior.source_inode != journal.source_inode
                || (!next_member
                    && (!next_phase
                        || !same_member
                        || !same_parent_identity
                        || !same_opened_identity))
            {
                return Err(recovery_refusal(
                    "package stage journal sequence changed; retain evidence",
                ));
            }
        } else if journal.phase != StagePhase::Intent {
            return Err(recovery_refusal(
                "package stage journal has no initial intent",
            ));
        }
        if journal.phase == StagePhase::Intent
            && (!roles.insert(journal.role.clone())
                || !slots.insert((journal.parent.clone(), journal.name.clone())))
        {
            return Err(recovery_refusal(
                "package stage journal repeats a role or slot",
            ));
        }
        previous = Some(journal.clone());
        history.push(journal);
    }
    if history.is_empty() {
        return Err(recovery_refusal(
            "package stage journal is empty; retain evidence",
        ));
    }
    Ok(Some(history))
}

fn validate_journal(journal: &StageJournal) -> Result<(), CustomerError> {
    let identity_valid = match journal.phase {
        StagePhase::Intent => {
            journal.device.is_none()
                && journal.inode.is_none()
                && ((journal.parent_device.is_none() && journal.parent_inode.is_none())
                    || (journal.parent_device.is_some()
                        && journal.parent_inode.is_some_and(|inode| inode != 0)))
        }
        StagePhase::Opened | StagePhase::Verified => {
            journal.device.is_some()
                && journal.inode.is_some_and(|inode| inode != 0)
                && journal.parent_device.is_some()
                && journal.parent_inode.is_some_and(|inode| inode != 0)
        }
    };
    if !identity_valid
        || !crate::workload::valid_boot_id(&journal.source_boot_id)
        || journal.source_inode == 0
        || !super::package::is_role(&journal.role)
        || !journal.parent.is_absolute()
        || journal
            .parent
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::Prefix(_)))
        || journal.name.is_empty()
        || matches!(journal.name.as_str(), "." | "..")
        || journal.name.contains('/')
        || journal.name.chars().any(char::is_control)
        || journal.pending.action == PackageAction::Remove
        || !super::package::is_lowercase_sha256(&journal.pending.old_hash)
        || !super::package::is_lowercase_sha256(&journal.pending.new_hash)
    {
        return Err(recovery_refusal(
            "package stage journal is inconsistent; retain the staging evidence",
        ));
    }
    Ok(())
}

fn observed_boot_id() -> Result<String, CustomerError> {
    let id = crate::workload::current_boot_id()
        .map_err(|_| recovery_refusal("package source boot identity unavailable"))?;
    if !crate::workload::valid_boot_id(&id) {
        return Err(recovery_refusal("package source boot identity invalid"));
    }
    Ok(id)
}

fn record(
    guard: &CustomerTransitionGuard,
    slot: &mut StageSlot,
    journal: &StageJournal,
) -> Result<(), CustomerError> {
    let body = serde_json::to_string(journal)
        .map_err(|_| recovery_refusal("cannot encode package stage journal"))?;
    guard
        .revalidate()
        .map_err(|_| recovery_refusal("package guard changed; retain stage evidence"))?;
    slot.revalidate()
        .ok_or_else(|| recovery_refusal("package stage journal identity changed"))?;
    let file = slot.file_mut();
    let current = file
        .metadata()
        .map_err(|_| recovery_refusal("cannot inspect package stage journal"))?
        .len();
    if current
        .checked_add(body.len() as u64)
        .and_then(|v| v.checked_add(1))
        .is_none_or(|size| size > super::transition_custody::MAX_JOURNAL_BODY_BYTES)
    {
        return Err(recovery_refusal(
            "package stage journal exceeds its read limit; retain evidence",
        ));
    }
    file.seek(SeekFrom::End(0))
        .and_then(|_| file.write_all(body.as_bytes()))
        .and_then(|_| file.write_all(b"\n"))
        .and_then(|_| file.sync_all())
        .map_err(|_| {
            recovery_refusal("cannot persist package stage journal; retain staging evidence")
        })?;
    slot.revalidate()
        .ok_or_else(|| recovery_refusal("package stage journal identity changed"))?;
    slot.sync_directory()
        .ok_or_else(|| recovery_refusal("cannot sync package stage journal directory"))?;
    guard
        .revalidate()
        .map_err(|_| recovery_refusal("package guard changed; retain stage evidence"))?;
    if read_stage_journal(guard.paths())?.as_ref() != Some(journal) {
        return Err(recovery_refusal(
            "package stage journal readback changed; retain evidence",
        ));
    }
    Ok(())
}

/// Stage one member after rechecking the most recent completed stage.
pub(super) fn stage_declared_member(
    guard: &CustomerTransitionGuard,
    request: StageRequest<'_>,
) -> Result<StageSlot, CustomerError> {
    stage_with_hook(guard, request, || Ok(()))
}

pub(super) fn require_stage_source(
    paths: &TransitionPaths,
    bundle: &DeclaredBundle,
) -> Result<(), CustomerError> {
    let journal = read_stage_journal(paths)?
        .ok_or_else(|| recovery_refusal("package stage source evidence is absent"))?;
    if journal.source_boot_id != observed_boot_id()?
        || bundle.source_identity() != Some((journal.source_device, journal.source_inode))
    {
        return Err(recovery_refusal(
            "package stage source identity changed; retain evidence",
        ));
    }
    Ok(())
}

fn prepare_stage_journal(
    guard: &CustomerTransitionGuard,
    request: &StageRequest<'_>,
) -> Result<StageSlot, CustomerError> {
    if guard.owner() != OperationOwner::Package
        || !request.parent.is_absolute()
        || request
            .parent
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::Prefix(_)))
        || request.name.is_empty()
        || matches!(request.name, "." | "..")
        || request.name.contains('/')
        || request.name.chars().any(char::is_control)
        || !super::package::is_role(request.role)
        || request.pending.action == PackageAction::Remove
        || request.bundle.manifest_sha256() != request.pending.new_hash
        || request.bundle.member_digest(request.role).is_none()
    {
        return Err(recovery_refusal(
            "package stage request or custody is invalid",
        ));
    }
    guard.revalidate()?;
    if super::package_publish::publication_journal_present(guard.paths())?
        || super::package_tree::tree_journal_present(guard.paths())?
        || super::package_selector::journal_present(guard.paths())?
    {
        return Err(recovery_refusal(
            "publication requires reconciliation before staging",
        ));
    }
    if read_pending(guard.paths())?.as_ref() != Some(request.pending) {
        return Err(recovery_refusal(
            "matching package pending record is required before staging",
        ));
    }
    let history = read_stage_history(guard.paths())?;
    let journal_slot = if let Some(history) = &history {
        require_stage_source(guard.paths(), request.bundle)?;
        if history.last().is_none_or(|last| {
            last.phase != StagePhase::Verified || last.pending != *request.pending
        }) || history.iter().any(|entry| {
            entry.role == request.role
                || (entry.parent == request.parent && entry.name == request.name)
        }) {
            return Err(recovery_refusal(
                "package stage is incomplete or repeats a role or slot",
            ));
        }
        let last = history
            .last()
            .ok_or_else(|| recovery_refusal("package stage history is empty"))?;
        reopen_recorded_stage(last, request.bundle)?;
        guard
            .revalidate()
            .map_err(|_| recovery_refusal("package guard changed while checking stages"))?;
        if read_stage_history(guard.paths())?.as_ref() != Some(history)
            || read_pending(guard.paths())?.as_ref() != Some(request.pending)
        {
            return Err(recovery_refusal(
                "package stage history changed; retain evidence",
            ));
        }
        StageSlot::open_existing(guard.paths().transactions_root(), JOURNAL, None)
            .ok_or_else(|| recovery_refusal("package stage journal identity unavailable"))?
    } else {
        StageSlot::create(guard.paths().transactions_root(), JOURNAL).map_err(|_| {
            recovery_refusal("package stage journal cannot be created exclusively; retain evidence")
        })?
    };
    Ok(journal_slot)
}

pub(super) fn stage_with_hook(
    guard: &CustomerTransitionGuard,
    request: StageRequest<'_>,
    after_copy: impl FnOnce() -> Result<(), CustomerError>,
) -> Result<StageSlot, CustomerError> {
    let parent = super::transition_custody::open_directory_no_follow(request.parent, false)?
        .ok_or_else(|| recovery_refusal("package stage directory is absent"))?;
    super::transition_custody::validate_directory(&parent, "package stage directory")?;
    let parent_metadata = parent
        .metadata()
        .map_err(|_| recovery_refusal("package stage parent identity unavailable"))?;
    let source_boot_id = observed_boot_id()?;
    let (source_device, source_inode) = request
        .bundle
        .source_identity()
        .ok_or_else(|| recovery_refusal("package source identity unavailable"))?;
    let mut journal_slot = prepare_stage_journal(guard, &request)?;
    let mut journal = StageJournal {
        pending: request.pending.clone(),
        source_boot_id,
        source_device,
        source_inode,
        role: request.role.into(),
        parent: request.parent.into(),
        name: request.name.into(),
        limit: request.limit,
        phase: StagePhase::Intent,
        parent_device: Some(parent_metadata.dev()),
        parent_inode: Some(parent_metadata.ino()),
        device: None,
        inode: None,
    };
    record(guard, &mut journal_slot, &journal)?;
    execute_stage_intent(
        guard,
        request.bundle,
        &mut journal_slot,
        &mut journal,
        after_copy,
    )
}

fn execute_stage_intent(
    guard: &CustomerTransitionGuard,
    bundle: &DeclaredBundle,
    journal_slot: &mut StageSlot,
    journal: &mut StageJournal,
    after_copy: impl FnOnce() -> Result<(), CustomerError>,
) -> Result<StageSlot, CustomerError> {
    if read_pending(guard.paths())?.as_ref() != Some(&journal.pending)
        || read_stage_journal(guard.paths())?.as_ref() != Some(journal)
    {
        return Err(recovery_refusal(
            "package pending changed after stage intent; retain evidence",
        ));
    }
    let parent_identity = (
        journal
            .parent_device
            .ok_or_else(|| recovery_refusal("stage parent identity absent"))?,
        journal
            .parent_inode
            .ok_or_else(|| recovery_refusal("stage parent identity absent"))?,
    );
    guard.revalidate()?;
    let mut slot = StageSlot::create_for_parent(&journal.parent, &journal.name, parent_identity)
        .map_err(|_| {
            recovery_refusal("package stage creation failed; retain journal and any stage")
        })?;
    let (device, inode) = slot
        .identity()
        .ok_or_else(|| recovery_refusal("package stage identity unavailable; retain evidence"))?;
    let (parent_device, parent_inode) = slot.parent_identity().ok_or_else(|| {
        recovery_refusal("package stage parent identity unavailable; retain evidence")
    })?;
    if (parent_device, parent_inode) != parent_identity {
        return Err(recovery_refusal(
            "package stage parent changed; retain evidence",
        ));
    }
    journal.device = Some(device);
    journal.inode = Some(inode);
    journal.phase = StagePhase::Opened;
    record(guard, journal_slot, journal)?;
    bundle
        .stage_member(&journal.role, &mut slot, journal.limit)
        .map_err(|_| recovery_refusal("package stage copy failed; retain journal and stage"))?;
    after_copy().map_err(|_| recovery_refusal("package staging interrupted; retain evidence"))?;
    guard
        .revalidate()
        .map_err(|_| recovery_refusal("package guard changed after staging; retain evidence"))?;
    journal.phase = StagePhase::Verified;
    record(guard, journal_slot, journal)?;
    Ok(slot)
}

/// Reopen only the recorded stage and recheck its bytes before continuing.
pub(super) fn resume_staged_member(
    guard: &CustomerTransitionGuard,
    expected: &PackagePending,
    bundle: &DeclaredBundle,
) -> Result<StageSlot, CustomerError> {
    if guard.owner() != OperationOwner::Package || bundle.manifest_sha256() != expected.new_hash {
        return Err(recovery_refusal(
            "package stage recovery identity does not match",
        ));
    }
    guard.revalidate()?;
    if super::package_publish::publication_journal_present(guard.paths())?
        || super::package_tree::tree_journal_present(guard.paths())?
        || super::package_selector::journal_present(guard.paths())?
    {
        return Err(recovery_refusal(
            "publication requires reconciliation before stage recovery",
        ));
    }
    if read_pending(guard.paths())?.as_ref() != Some(expected) {
        return Err(recovery_refusal(
            "matching package pending record is required for recovery",
        ));
    }
    let mut journal = read_stage_journal(guard.paths())?
        .ok_or_else(|| recovery_refusal("package stage journal is absent"))?;
    if journal.pending != *expected
        || journal.source_boot_id != observed_boot_id()?
        || bundle.source_identity() != Some((journal.source_device, journal.source_inode))
    {
        return Err(recovery_refusal(
            "package stage identity does not match; retain evidence",
        ));
    }
    if journal.phase == StagePhase::Intent {
        let mut journal_slot =
            StageSlot::open_existing(guard.paths().transactions_root(), JOURNAL, None)
                .ok_or_else(|| recovery_refusal("package stage journal identity unavailable"))?;
        return execute_stage_intent(guard, bundle, &mut journal_slot, &mut journal, || Ok(()));
    }
    let digest = bundle
        .member_digest(&journal.role)
        .ok_or_else(|| recovery_refusal("declared package stage digest is unavailable"))?;
    let mut slot = if journal.phase == StagePhase::Opened {
        open_recorded_stage(&journal)?
    } else {
        reopen_recorded_stage(&journal, bundle)?
    };
    guard
        .revalidate()
        .map_err(|_| recovery_refusal("package guard changed during stage recovery"))?;
    if read_pending(guard.paths())?.as_ref() != Some(expected)
        || read_stage_journal(guard.paths())?.as_ref() != Some(&journal)
    {
        return Err(recovery_refusal(
            "package stage evidence changed during recovery",
        ));
    }
    if journal.phase == StagePhase::Opened {
        bundle
            .resume_member(&journal.role, &mut slot, journal.limit)
            .map_err(|_| recovery_refusal("package stage continuation refused; retain evidence"))?;
        slot.sync_file_and_directory().ok_or_else(|| {
            recovery_refusal("cannot sync recovered package stage; retain evidence")
        })?;
        let mut journal_slot =
            StageSlot::open_existing(guard.paths().transactions_root(), JOURNAL, None)
                .ok_or_else(|| recovery_refusal("package stage journal identity unavailable"))?;
        if read_stage_journal(guard.paths())?.as_ref() != Some(&journal) {
            return Err(recovery_refusal(
                "package stage journal changed before recovery",
            ));
        }
        journal.phase = StagePhase::Verified;
        record(guard, &mut journal_slot, &journal)?;
        slot.verify_bytes(digest, journal.limit)
            .ok_or_else(|| recovery_refusal("package stage bytes changed after recovery"))?;
    }
    Ok(slot)
}

fn reopen_recorded_stage(
    journal: &StageJournal,
    bundle: &DeclaredBundle,
) -> Result<StageSlot, CustomerError> {
    if journal.source_boot_id != observed_boot_id()?
        || bundle.source_identity() != Some((journal.source_device, journal.source_inode))
    {
        return Err(recovery_refusal(
            "package stage source identity changed; retain evidence",
        ));
    }
    let digest = bundle
        .member_digest(&journal.role)
        .ok_or_else(|| recovery_refusal("declared package stage digest is unavailable"))?;
    let mut slot = open_recorded_stage(journal)?;
    slot.verify_bytes(digest, journal.limit)
        .ok_or_else(|| recovery_refusal("package stage bytes differ; retain evidence"))?;
    Ok(slot)
}

fn open_recorded_stage(journal: &StageJournal) -> Result<StageSlot, CustomerError> {
    let identity = (
        journal
            .device
            .ok_or_else(|| recovery_refusal("stage device is absent"))?,
        journal
            .inode
            .ok_or_else(|| recovery_refusal("stage inode is absent"))?,
    );
    let slot = StageSlot::open_existing(&journal.parent, &journal.name, Some(identity))
        .ok_or_else(|| recovery_refusal("package stage identity changed; retain evidence"))?;
    if slot.parent_identity()
        != Some((
            journal.parent_device.unwrap_or_default(),
            journal.parent_inode.unwrap_or_default(),
        ))
    {
        return Err(recovery_refusal(
            "package stage parent changed; retain evidence",
        ));
    }
    Ok(slot)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn stages_multiple_declared_members_without_reusing_roles_or_names() {
        use crate::customer::package::{
            ManifestFormat, ManifestKind, ManifestMember, tests::pinned_manifest,
        };
        use crate::customer::transition::{acquire_transition, enroll_pending};
        let root =
            std::env::temp_dir().join(format!("ramshared-multi-stage-{}", std::process::id()));
        std::fs::create_dir_all(root.join("scripts/safety")).unwrap();
        std::fs::create_dir_all(root.join("systemd")).unwrap();
        std::fs::create_dir(root.join("stage")).unwrap();
        let hash = |data: &[u8]| {
            let mut input = data;
            crate::installed_image::sha256_reader(&mut input).unwrap()
        };
        let mut payload = pinned_manifest();
        payload.contents.push(ManifestMember {
            role: "SHA256SUMS".into(),
            kind: ManifestKind::Generated,
            mode: "0644".into(),
            source: "SHA256SUMS".into(),
            formats: vec![ManifestFormat::Bundle],
            sha256: None,
            reason: Some("Generated for bundle".into()),
        });
        for (role, bytes) in [
            ("origin.conf.example", &b"first"[..]),
            ("ramshared-cascade.service", &b"second"[..]),
        ] {
            payload
                .contents
                .iter_mut()
                .find(|member| member.role == role)
                .unwrap()
                .sha256 = Some(hash(bytes));
        }
        let document = serde_json::to_vec(&payload).unwrap();
        std::fs::write(root.join("release-manifest.json"), &document).unwrap();
        std::fs::write(root.join("scripts/safety/origin.conf.example"), b"first").unwrap();
        std::fs::write(root.join("systemd/ramshared-cascade.service"), b"second").unwrap();
        let checksums = format!(
            "{}  ./release-manifest.json\n{}  ./scripts/safety/origin.conf.example\n{}  ./systemd/ramshared-cascade.service\n",
            hash(&document),
            hash(b"first"),
            hash(b"second")
        );
        std::fs::write(root.join("SHA256SUMS"), &checksums).unwrap();
        let bundle = DeclaredBundle::open(&root, &hash(checksums.as_bytes())).unwrap();
        let paths = TransitionPaths::under(&root.join("transaction"));
        let guard = acquire_transition(OperationOwner::Package, &paths).unwrap();
        let pending = PackagePending {
            target_release: None,
            action: PackageAction::Upgrade,
            old_hash: "a".repeat(64),
            new_hash: hash(checksums.as_bytes()),
            issued_unix: 1,
        };
        enroll_pending(&guard, &pending).unwrap();
        let stage = root.join("stage");
        let request = |role, name| StageRequest {
            pending: &pending,
            bundle: &bundle,
            role,
            parent: &stage,
            name,
            limit: 4096,
        };
        drop(stage_declared_member(&guard, request("origin.conf.example", "one")).unwrap());
        std::fs::write(stage.join("one"), b"altered").unwrap();
        assert!(
            stage_declared_member(&guard, request("ramshared-cascade.service", "two")).is_err()
        );
        assert!(!stage.join("two").exists());
        std::fs::write(stage.join("one"), b"first").unwrap();
        drop(stage_declared_member(&guard, request("ramshared-cascade.service", "two")).unwrap());
        assert_eq!(std::fs::read(stage.join("two")).unwrap(), b"second");
        assert!(stage_declared_member(&guard, request("origin.conf.example", "three")).is_err());
        assert!(!stage.join("three").exists());
        drop(stage_declared_member(&guard, request("SHA256SUMS", "manifest")).unwrap());
        assert_eq!(
            std::fs::read(stage.join("manifest")).unwrap(),
            checksums.as_bytes()
        );
        assert!(resume_staged_member(&guard, &pending, &bundle).is_ok());
        assert_eq!(
            read_stage_journal(&paths).unwrap().unwrap().role,
            "SHA256SUMS"
        );
        // A durable intent can resume only in its original directory and only
        // when exclusive creation finds no existing destination.
        let intent_parent = root.join("intent-stage");
        std::fs::create_dir(&intent_parent).unwrap();
        std::fs::write(intent_parent.join("slot"), b"foreign").unwrap();
        let intent_paths = TransitionPaths::under(&root.join("intent-transaction"));
        let intent_guard = acquire_transition(OperationOwner::Package, &intent_paths).unwrap();
        enroll_pending(&intent_guard, &pending).unwrap();
        let intent_request = StageRequest {
            pending: &pending,
            bundle: &bundle,
            role: "origin.conf.example",
            parent: &intent_parent,
            name: "slot",
            limit: 4096,
        };
        assert!(stage_declared_member(&intent_guard, intent_request).is_err());
        let intent = read_stage_journal(&intent_paths).unwrap().unwrap();
        assert_eq!(intent.phase, StagePhase::Intent);
        assert!(intent.parent_inode.is_some());
        let mut stale = intent.clone();
        stale.name = "unexpected".into();
        let mut held_journal =
            StageSlot::open_existing(intent_paths.transactions_root(), JOURNAL, None).unwrap();
        assert!(
            execute_stage_intent(
                &intent_guard,
                &bundle,
                &mut held_journal,
                &mut stale,
                || Ok(())
            )
            .is_err()
        );
        assert!(!intent_parent.join("unexpected").exists());
        assert!(resume_staged_member(&intent_guard, &pending, &bundle).is_err());
        assert_eq!(
            std::fs::read(intent_parent.join("slot")).unwrap(),
            b"foreign"
        );
        std::fs::remove_file(intent_parent.join("slot")).unwrap();
        let moved = root.join("intent-original");
        std::fs::rename(&intent_parent, &moved).unwrap();
        std::fs::create_dir(&intent_parent).unwrap();
        assert!(resume_staged_member(&intent_guard, &pending, &bundle).is_err());
        assert!(!intent_parent.join("slot").exists());
        std::fs::remove_dir(&intent_parent).unwrap();
        std::fs::rename(&moved, &intent_parent).unwrap();
        drop(resume_staged_member(&intent_guard, &pending, &bundle).unwrap());
        assert_eq!(std::fs::read(intent_parent.join("slot")).unwrap(), b"first");
        assert_eq!(
            read_stage_journal(&intent_paths).unwrap().unwrap().phase,
            StagePhase::Verified
        );
        drop(intent_guard);
        let legacy_paths = TransitionPaths::under(&root.join("legacy-transaction"));
        let legacy_guard = acquire_transition(OperationOwner::Package, &legacy_paths).unwrap();
        enroll_pending(&legacy_guard, &pending).unwrap();
        let mut legacy_slot = StageSlot::create(legacy_paths.transactions_root(), JOURNAL).unwrap();
        let legacy_intent = StageJournal {
            pending: pending.clone(),
            source_boot_id: observed_boot_id().unwrap(),
            source_device: bundle.source_identity().unwrap().0,
            source_inode: bundle.source_identity().unwrap().1,
            role: "origin.conf.example".into(),
            parent: stage.clone(),
            name: "legacy".into(),
            limit: 4096,
            phase: StagePhase::Intent,
            parent_device: None,
            parent_inode: None,
            device: None,
            inode: None,
        };
        record(&legacy_guard, &mut legacy_slot, &legacy_intent).unwrap();
        let journal_path = legacy_paths.transactions_root().join(JOURNAL);
        let mut foreign_intent = legacy_intent.clone();
        foreign_intent.pending.issued_unix += 1;
        std::fs::write(
            &journal_path,
            format!("{}\n", serde_json::to_string(&foreign_intent).unwrap()),
        )
        .unwrap();
        assert!(resume_staged_member(&legacy_guard, &pending, &bundle).is_err());
        assert!(!stage.join("legacy").exists());
        std::fs::write(
            &journal_path,
            format!("{}\n", serde_json::to_string(&legacy_intent).unwrap()),
        )
        .unwrap();
        assert!(resume_staged_member(&legacy_guard, &pending, &bundle).is_err());
        assert!(!stage.join("legacy").exists());
        let maximum = super::super::transition_custody::MAX_JOURNAL_BODY_BYTES;
        legacy_slot.file_mut().set_len(maximum).unwrap();
        assert!(record(&legacy_guard, &mut legacy_slot, &legacy_intent).is_err());
        assert_eq!(legacy_slot.file_mut().metadata().unwrap().len(), maximum);
        assert!(!stage.join("legacy").exists());
        drop(legacy_guard);
        drop(guard);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "subprocess fixture invoked by stage_recovers_after_process_is_killed"]
    fn stage_kill_child() {
        use std::io::{Read, Write};
        let root = PathBuf::from(std::env::var_os("RAMSHARED_STAGE_CHILD_DIR").unwrap());
        let checksum = std::fs::read(root.join("SHA256SUMS")).unwrap();
        let digest = crate::installed_image::sha256_reader(&mut checksum.as_slice()).unwrap();
        let bundle = DeclaredBundle::open(&root, &digest).unwrap();
        let paths = TransitionPaths::under(&root.join("transaction"));
        let guard =
            super::super::transition::acquire_transition(OperationOwner::Package, &paths).unwrap();
        let pending = read_pending(&paths).unwrap().unwrap();
        let stage = root.join("stage");
        let request = StageRequest {
            pending: &pending,
            bundle: &bundle,
            role: "origin.conf.example",
            parent: &stage,
            name: "payload",
            limit: 8,
        };
        let result = stage_with_hook(&guard, request, || {
            if std::env::var_os("RAMSHARED_STAGE_CHILD_PARTIAL").is_some() {
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .open(stage.join("payload"))
                    .map_err(|_| recovery_refusal("test stage open failed"))?;
                file.set_len(3)
                    .and_then(|_| file.sync_all())
                    .map_err(|_| recovery_refusal("test partial stage failed"))?;
            }
            println!("RAMSHARED_STAGE_TEST_READY");
            std::io::stdout().flush().unwrap();
            let mut byte = [0u8; 1];
            let _ = std::io::stdin().read(&mut byte);
            Err(recovery_refusal("test interruption"))
        });
        assert!(result.is_err());
    }

    #[test]
    fn stage_recovers_after_process_is_killed() {
        run_stage_kill_fixture(false);
        run_stage_kill_fixture(true);
    }

    fn run_stage_kill_fixture(partial: bool) {
        use crate::customer::package::tests::pinned_manifest;
        use crate::customer::transition::{acquire_transition, enroll_pending};
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};
        use std::sync::mpsc;
        use std::time::Duration;

        let root = std::env::temp_dir().join(format!(
            "ramshared-stage-crash-{}-{partial}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("scripts/safety")).unwrap();
        std::fs::create_dir(root.join("stage")).unwrap();
        let hash = |bytes: &[u8]| {
            let mut input = bytes;
            crate::installed_image::sha256_reader(&mut input).unwrap()
        };
        let mut payload = pinned_manifest();
        payload
            .contents
            .iter_mut()
            .find(|member| member.role == "origin.conf.example")
            .unwrap()
            .sha256 = Some(hash(b"approved"));
        let document = serde_json::to_vec(&payload).unwrap();
        std::fs::write(root.join("release-manifest.json"), &document).unwrap();
        std::fs::write(root.join("scripts/safety/origin.conf.example"), b"approved").unwrap();
        let manifest = format!(
            "{}  ./release-manifest.json\n{}  ./scripts/safety/origin.conf.example\n",
            hash(&document),
            hash(b"approved")
        );
        std::fs::write(root.join("SHA256SUMS"), &manifest).unwrap();
        let pending = PackagePending {
            target_release: None,
            action: PackageAction::Upgrade,
            old_hash: "a".repeat(64),
            new_hash: hash(manifest.as_bytes()),
            issued_unix: 1,
        };
        let paths = TransitionPaths::under(&root.join("transaction"));
        let guard = acquire_transition(OperationOwner::Package, &paths).unwrap();
        enroll_pending(&guard, &pending).unwrap();
        drop(guard);
        let mut command = Command::new(std::env::current_exe().unwrap());
        if partial {
            command.env("RAMSHARED_STAGE_CHILD_PARTIAL", "1");
        }
        let mut child = command
            .args([
                "--exact",
                "customer::package_stage_journal::tests::stage_kill_child",
                "--ignored",
                "--nocapture",
            ])
            .env("RAMSHARED_STAGE_CHILD_DIR", &root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (send, receive) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if line.contains("RAMSHARED_STAGE_TEST_READY") {
                    let _ = send.send(());
                    break;
                }
            }
        });
        let ready = receive.recv_timeout(Duration::from_secs(15));
        let killed = child.kill();
        let status = child.wait();
        reader.join().unwrap();
        assert!(
            ready.is_ok(),
            "child never reached the durable opened phase"
        );
        assert!(killed.is_ok());
        assert!(!status.unwrap().success());
        assert_eq!(
            read_stage_journal(&paths).unwrap().unwrap().phase,
            StagePhase::Opened
        );
        let before = std::fs::read(root.join("stage/payload")).unwrap();
        assert_eq!(
            before,
            if partial {
                &b"approved"[..3]
            } else {
                &b"approved"[..]
            }
        );
        let guard = acquire_transition(OperationOwner::Package, &paths).unwrap();
        let bundle = DeclaredBundle::open(&root, &pending.new_hash).unwrap();
        let journal_path = paths.transactions_root().join(JOURNAL);
        let original_journal = std::fs::read(&journal_path).unwrap();
        let mut history = read_stage_history(&paths).unwrap().unwrap();
        let replacement_boot =
            if history[0].source_boot_id == "00000000-0000-0000-0000-000000000000" {
                "12345678-1234-1234-1234-123456789012"
            } else {
                "00000000-0000-0000-0000-000000000000"
            };
        for entry in &mut history {
            entry.source_boot_id = replacement_boot.into();
        }
        let foreign_journal = history
            .iter()
            .map(|entry| format!("{}\n", serde_json::to_string(entry).unwrap()))
            .collect::<String>();
        std::fs::write(&journal_path, &foreign_journal).unwrap();
        assert!(resume_staged_member(&guard, &pending, &bundle).is_err());
        assert_eq!(
            std::fs::read(&journal_path).unwrap(),
            foreign_journal.as_bytes()
        );
        assert_eq!(std::fs::read(root.join("stage/payload")).unwrap(), before);
        std::fs::write(&journal_path, &original_journal).unwrap();
        let staged = resume_staged_member(&guard, &pending, &bundle).unwrap();
        assert!(staged.revalidate().is_some());
        assert_eq!(
            read_stage_journal(&paths).unwrap().unwrap().phase,
            StagePhase::Verified
        );
        assert_eq!(
            std::fs::read(root.join("stage/payload")).unwrap(),
            b"approved"
        );
        drop(staged);
        drop(guard);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn journal_refuses_truncation_reordering_and_identity_changes() {
        let root =
            std::env::temp_dir().join(format!("ramshared-stage-journal-{}", std::process::id()));
        let paths = TransitionPaths::under(&root);
        std::fs::create_dir_all(paths.transactions_root()).unwrap();
        let path = paths.transactions_root().join(JOURNAL);
        let intent = StageJournal {
            pending: PackagePending {
                target_release: None,
                action: PackageAction::Upgrade,
                old_hash: "a".repeat(64),
                new_hash: "b".repeat(64),
                issued_unix: 1,
            },
            source_boot_id: "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".into(),
            source_device: u64::MAX,
            source_inode: u64::MAX,
            role: "ramshared".into(),
            parent: root.join("stage"),
            name: "slot".into(),
            limit: 10,
            phase: StagePhase::Intent,
            parent_device: None,
            parent_inode: None,
            device: None,
            inode: None,
        };
        let maximum = journal_capacity(std::slice::from_ref(&intent)).unwrap();
        let mut largest = intent.clone();
        largest.phase = StagePhase::Opened;
        largest.parent_device = Some(u64::MAX);
        largest.parent_inode = Some(u64::MAX);
        largest.device = Some(u64::MAX);
        largest.inode = Some(u64::MAX);
        let opened_len = serde_json::to_vec(&largest).unwrap().len();
        largest.phase = StagePhase::Verified;
        let mut largest_intent = intent.clone();
        largest_intent.parent_device = Some(u64::MAX);
        largest_intent.parent_inode = Some(u64::MAX);
        let expected = serde_json::to_vec(&largest_intent).unwrap().len()
            + opened_len
            + serde_json::to_vec(&largest).unwrap().len()
            + 3;
        assert_eq!(maximum, expected as u64);
        assert_eq!(journal_capacity(&[]), Some(0));
        let mut opened = intent.clone();
        opened.phase = StagePhase::Opened;
        opened.parent_device = Some(4);
        opened.parent_inode = Some(5);
        opened.device = Some(1);
        opened.inode = Some(2);
        let mut verified = opened.clone();
        verified.phase = StagePhase::Verified;
        let encode = |records: &[StageJournal]| {
            records
                .iter()
                .map(|r| format!("{}\n", serde_json::to_string(r).unwrap()))
                .collect::<String>()
        };
        let mut identified_intent = intent.clone();
        identified_intent.parent_device = opened.parent_device;
        identified_intent.parent_inode = opened.parent_inode;
        let mut wrong_parent = opened.clone();
        wrong_parent.parent_inode = Some(99);
        std::fs::write(&path, encode(&[identified_intent, wrong_parent])).unwrap();
        assert!(read_stage_journal(&paths).is_err());
        let valid = encode(&[intent.clone(), opened.clone(), verified.clone()]);
        std::fs::write(&path, &valid).unwrap();
        assert_eq!(
            read_stage_journal(&paths).unwrap().unwrap().phase,
            StagePhase::Verified
        );
        let mut changed = verified.clone();
        changed.inode = Some(3);
        let mut changed_source = verified.clone();
        changed_source.source_inode -= 1;
        let mut changed_boot = verified.clone();
        changed_boot.source_boot_id = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".into();
        let mut malformed_boot = intent.clone();
        malformed_boot.source_boot_id = "INVALID".into();
        let mut missing_source = serde_json::to_value(&intent).unwrap();
        missing_source
            .as_object_mut()
            .unwrap()
            .remove("source_inode");
        let mut missing_boot = serde_json::to_value(&intent).unwrap();
        missing_boot
            .as_object_mut()
            .unwrap()
            .remove("source_boot_id");
        let mut invalid = opened.clone();
        invalid.inode = None;
        let mut reused_slot = intent.clone();
        reused_slot.role = "ramsharedd".into();
        let mut reused_role = intent.clone();
        reused_role.name = "another".into();
        for body in [
            String::new(),
            "not-json\n".to_string(),
            format!("{valid}{{partial"),
            encode(&[opened.clone()]),
            encode(&[intent.clone(), verified.clone()]),
            encode(&[intent.clone(), opened.clone(), changed]),
            encode(&[intent.clone(), opened.clone(), changed_source]),
            encode(&[intent.clone(), opened.clone(), changed_boot]),
            encode(&[malformed_boot]),
            format!("{}\n", missing_source),
            format!("{}\n", missing_boot),
            encode(&[intent.clone(), invalid]),
            encode(&[
                intent.clone(),
                opened.clone(),
                verified.clone(),
                reused_slot,
            ]),
            encode(&[
                intent.clone(),
                opened.clone(),
                verified.clone(),
                reused_role,
            ]),
        ] {
            std::fs::write(&path, body).unwrap();
            assert!(read_stage_journal(&paths).is_err());
            assert!(stage_journal_present(&paths).unwrap());
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
