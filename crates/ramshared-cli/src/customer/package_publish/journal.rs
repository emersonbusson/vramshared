// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_publish/journal.rs
//! Durable publication history and strict transition validation.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;

use super::{
    CustomerError, CustomerTransitionGuard, Identity, MAX_JOURNAL_BODY_BYTES, PackageAction,
    PackagePending, StageSlot, TransitionPaths, read_journal_body, recovery_refusal,
    safe_publication_mode, target,
};

pub(super) const JOURNAL: &str = "package-publish.json";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Phase {
    Prepared,
    Published,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Record {
    pub(super) pending: PackagePending,
    pub(super) role: String,
    pub(super) root: PathBuf,
    pub(super) root_identity: Identity,
    pub(super) destination: String,
    pub(super) parent_identity: Identity,
    pub(super) file_identity: Identity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) stage_identity: Option<Identity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) stage_parent_identity: Option<Identity>,
    pub(super) creation_nonce: [u8; 32],
    pub(super) digest: String,
    pub(super) size: u64,
    pub(super) mode: u32,
    pub(super) phase: Phase,
}

/// Any existing or unreadable record blocks unrelated mutation and pending clearance.
pub(in crate::customer) fn publication_journal_present(
    paths: &TransitionPaths,
) -> Result<bool, CustomerError> {
    read_journal_body(paths, OsStr::new(JOURNAL), "package publication journal")
        .map(|value| value.is_some())
        .map_err(|_| recovery_refusal("package publication journal unavailable; retain evidence"))
}

/// Remove the publication journal only under `OperationOwner::Package` for a matching transaction.
pub(in crate::customer) fn clear_publication_journal(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
) -> Result<(), CustomerError> {
    if guard.owner() != super::super::transition::OperationOwner::Package {
        return Err(recovery_refusal(
            "clearing publication journal requires package transition owner",
        ));
    }
    guard.revalidate()?;
    if super::super::transition::read_pending(guard.paths())?.as_ref() != Some(pending) {
        return Err(recovery_refusal(
            "pending record does not match publication journal clear target",
        ));
    }
    if let Some(history) = read_history(guard.paths())?
        && history.iter().any(|entry| &entry.pending != pending)
    {
        return Err(recovery_refusal(
            "publication journal belongs to another transaction; retain evidence",
        ));
    }
    super::super::transition_custody::clear_journal_file(
        guard,
        OsStr::new(JOURNAL),
        "package publication journal",
    )
}

pub(super) fn read_record(paths: &TransitionPaths) -> Result<Option<Record>, CustomerError> {
    Ok(read_history(paths)?.and_then(|mut history| history.pop()))
}

pub(super) fn read_history(paths: &TransitionPaths) -> Result<Option<Vec<Record>>, CustomerError> {
    let Some(body) = read_journal_body(paths, OsStr::new(JOURNAL), "package publication journal")?
    else {
        return Ok(None);
    };
    if !body.ends_with('\n') {
        return Err(recovery_refusal("publication journal is incomplete"));
    }
    let mut history: Vec<Record> = Vec::new();
    let mut roles = BTreeSet::new();
    let mut destinations = BTreeSet::new();
    for line in body.lines() {
        let record: Record = serde_json::from_str(line)
            .map_err(|_| recovery_refusal("publication journal is malformed"))?;
        validate_record(&record)?;
        if let Some(prior) = history.last() {
            if prior.pending != record.pending
                || prior.root != record.root
                || prior.root_identity != record.root_identity
            {
                return Err(recovery_refusal("publication transaction or root changed"));
            }
            match (prior.phase, record.phase) {
                (Phase::Prepared, Phase::Published) => {
                    let mut expected = prior.clone();
                    expected.phase = Phase::Published;
                    if record != expected {
                        return Err(recovery_refusal("publication confirmation changed"));
                    }
                }
                (Phase::Published, Phase::Prepared) => (),
                _ => return Err(recovery_refusal("publication journal sequence changed")),
            }
        } else if record.phase != Phase::Prepared {
            return Err(recovery_refusal("publication intent is absent"));
        }
        if record.phase == Phase::Prepared
            && (!roles.insert(record.role.clone())
                || !destinations.insert(record.destination.clone()))
        {
            return Err(recovery_refusal(
                "publication journal repeats a role or destination",
            ));
        }
        history.push(record);
    }
    if history.is_empty() {
        return Err(recovery_refusal("publication journal is empty"));
    }
    Ok(Some(history))
}

fn validate_record(record: &Record) -> Result<(), CustomerError> {
    if !record.root.is_absolute()
        || !super::super::package::is_role(&record.role)
        || !super::super::package::is_lowercase_sha256(&record.digest)
        || !super::super::package::is_lowercase_sha256(&record.pending.old_hash)
        || !super::super::package::is_lowercase_sha256(&record.pending.new_hash)
        || record.root.components().any(|part| {
            matches!(
                part,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        })
        || target(&record.root, &record.destination).is_err()
        || !safe_publication_mode(record.mode)
        || record.creation_nonce.iter().all(|byte| *byte == 0)
        || record.file_identity.1 == 0
        || record.parent_identity.1 == 0
        || record.root_identity.1 == 0
        || match (record.stage_identity, record.stage_parent_identity) {
            (None, None) => false,
            (Some((_, file)), Some((_, parent))) => file == 0 || parent == 0,
            _ => true,
        }
        || record.pending.action == PackageAction::Remove
    {
        return Err(recovery_refusal("publication journal identity is invalid"));
    }
    Ok(())
}

pub(super) fn persist(
    guard: &CustomerTransitionGuard,
    slot: &mut StageSlot,
    record: &Record,
) -> Result<(), CustomerError> {
    let bytes = serde_json::to_vec(record)
        .map_err(|_| recovery_refusal("cannot encode publication journal"))?;
    guard.revalidate()?;
    slot.revalidate()
        .ok_or_else(|| recovery_refusal("publication journal identity changed"))?;
    let file = slot.file_mut();
    let size = file
        .metadata()
        .map_err(|_| recovery_refusal("cannot inspect publication journal"))?
        .len();
    if size
        .checked_add(bytes.len() as u64)
        .and_then(|n| n.checked_add(1))
        .is_none_or(|n| n > MAX_JOURNAL_BODY_BYTES)
    {
        return Err(recovery_refusal("publication journal exceeds its limit"));
    }
    file.seek(SeekFrom::End(0))
        .and_then(|_| file.write_all(&bytes))
        .and_then(|_| file.write_all(b"\n"))
        .and_then(|_| file.sync_all())
        .map_err(|_| recovery_refusal("cannot persist publication journal; retain evidence"))?;
    slot.revalidate()
        .ok_or_else(|| recovery_refusal("publication journal identity changed"))?;
    slot.sync_directory()
        .ok_or_else(|| recovery_refusal("cannot sync publication journal directory"))?;
    guard.revalidate()?;
    if read_record(guard.paths())?.as_ref() != Some(record) {
        return Err(recovery_refusal("publication journal readback changed"));
    }
    slot.revalidate()
        .ok_or_else(|| recovery_refusal("publication journal identity changed"))?;
    Ok(())
}
