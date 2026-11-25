// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_selector/reconciliation.rs
//! Attended crash recovery and safe cleanup for selector effects and releases.

use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use rustix::fs::{AtFlags, Dir, Mode, OFlags, renameat, statat, unlinkat};

use super::super::CustomerError;
use super::super::package_trust::VerifiedSignature;
use super::super::transition::{
    CustomerTransitionGuard, OperationOwner, PackagePending, TransitionPaths, read_journal_body,
    read_pending, recovery_refusal,
};
use super::super::transition_custody::{open_directory_no_follow, validate_directory};
use super::effect::{
    EFFECT_JOURNAL, Identity, SelectorEffectPhase, SelectorEffectRecord,
    check_live_selector_matches_prior, persist_record, rollback_selector_effect, stat_no_follow,
    verify_symlink,
};
use super::sealing::SealedReleaseCustody;
use super::tree_custody::TreeCustody;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::customer) enum SelectorRecoveryAction {
    Rollback,
    Complete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::customer) enum SelectorRecoveryOutcome {
    Verified,
    RolledBack,
}

/// Inspect the selector effect journal safely without repairing or removing foreign data.
pub(in crate::customer) fn inspect_effect_journal(
    paths: &TransitionPaths,
) -> Result<Option<SelectorEffectRecord>, CustomerError> {
    let Some(body) =
        read_journal_body(paths, OsStr::new(EFFECT_JOURNAL), "selector effect journal")
            .map_err(|_| recovery_refusal("selector effect journal unreadable; retain evidence"))?
    else {
        return Ok(None);
    };
    let parsed: SelectorEffectRecord = serde_json::from_str(&body)
        .map_err(|_| recovery_refusal("selector effect journal is malformed; retain evidence"))?;
    if parsed.schema != 1 || parsed.staging_name.is_empty() || parsed.staging_name.contains('/') {
        return Err(recovery_refusal(
            "selector effect journal fields are invalid; retain evidence",
        ));
    }
    Ok(Some(parsed))
}

/// Reconcile an interrupted selector switch: either roll back cleanly or verify & complete.
pub(in crate::customer) fn reconcile_selector(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    action: SelectorRecoveryAction,
    completion_custody: Option<(&SealedReleaseCustody, &VerifiedSignature)>,
    previous_custody: Option<&TreeCustody>,
) -> Result<SelectorRecoveryOutcome, CustomerError> {
    if guard.owner() != OperationOwner::Package {
        return Err(recovery_refusal(
            "selector reconciliation requires package transition owner",
        ));
    }
    guard.revalidate()?;
    if read_pending(guard.paths())?.as_ref() != Some(pending) {
        return Err(recovery_refusal(
            "pending record does not match selector reconciliation target",
        ));
    }
    let mut record = inspect_effect_journal(guard.paths())?
        .ok_or_else(|| recovery_refusal("selector effect journal is absent"))?;
    if &record.pending != pending {
        return Err(recovery_refusal(
            "selector effect journal pending disagrees with active transaction",
        ));
    }

    let parent_path = record
        .selector
        .parent()
        .filter(|p| p.is_absolute())
        .ok_or_else(|| recovery_refusal("selector parent is invalid"))?
        .to_path_buf();
    let parent = open_directory_no_follow(&parent_path, false)?
        .ok_or_else(|| recovery_refusal("selector parent disappeared"))?;
    validate_directory(&parent, "selector parent")?;
    let parent_meta = parent
        .metadata()
        .map_err(|_| recovery_refusal("selector parent metadata unavailable"))?;
    if (parent_meta.dev(), parent_meta.ino()) != record.parent_identity {
        return Err(recovery_refusal("selector parent identity changed"));
    }

    match (record.phase, action) {
        (SelectorEffectPhase::RolledBack, SelectorRecoveryAction::Rollback) => {
            verify_rolled_back_state(&parent, &record)?;
            Ok(SelectorRecoveryOutcome::RolledBack)
        }
        (SelectorEffectPhase::Verified, SelectorRecoveryAction::Complete) => {
            let (sealed, signature) = completion_custody
                .ok_or_else(|| recovery_refusal("verified completion requires sealed custody"))?;
            verify_completed_selector(&parent, &parent_path, &record, sealed, signature)?;
            Ok(SelectorRecoveryOutcome::Verified)
        }
        (
            SelectorEffectPhase::IntentBound | SelectorEffectPhase::SelectorStaged,
            SelectorRecoveryAction::Complete,
        ) => complete_from_staged(
            guard,
            &parent,
            &parent_path,
            &mut record,
            completion_custody,
        ),
        (SelectorEffectPhase::SelectorPublished, SelectorRecoveryAction::Complete) => {
            let (sealed, signature) = completion_custody
                .ok_or_else(|| recovery_refusal("verified completion requires sealed custody"))?;
            verify_completed_selector(&parent, &parent_path, &record, sealed, signature)?;
            record.phase = SelectorEffectPhase::Verified;
            persist_record(guard, &record)?;
            Ok(SelectorRecoveryOutcome::Verified)
        }
        (_, SelectorRecoveryAction::Rollback) => {
            rollback_selector_effect(guard, &parent, &parent_path, &mut record, previous_custody)?;
            Ok(SelectorRecoveryOutcome::RolledBack)
        }
        (SelectorEffectPhase::RolledBack, SelectorRecoveryAction::Complete) => Err(
            recovery_refusal("cannot complete an already rolled back selector transaction"),
        ),
    }
}

fn verify_rolled_back_state(
    parent: &File,
    record: &SelectorEffectRecord,
) -> Result<(), CustomerError> {
    let leaf = record
        .selector
        .file_name()
        .ok_or_else(|| recovery_refusal("selector name is invalid"))?;
    if statat(
        parent,
        record.staging_name.as_str(),
        AtFlags::SYMLINK_NOFOLLOW,
    )
    .is_ok()
    {
        return Err(recovery_refusal(
            "staging symlink still present on rolled back selector",
        ));
    }
    check_live_selector_matches_prior(parent, leaf, record.previous.as_ref())
}

fn complete_from_staged(
    guard: &CustomerTransitionGuard,
    parent: &File,
    parent_path: &Path,
    record: &mut SelectorEffectRecord,
    completion_custody: Option<(&SealedReleaseCustody, &VerifiedSignature)>,
) -> Result<SelectorRecoveryOutcome, CustomerError> {
    let (sealed, signature) = completion_custody
        .ok_or_else(|| recovery_refusal("selector completion requires sealed release custody"))?;
    validate_completion_inputs(parent_path, record, sealed, signature)?;
    let leaf = record
        .selector
        .file_name()
        .ok_or_else(|| recovery_refusal("selector name is invalid"))?;
    let target_str = record
        .target
        .to_str()
        .ok_or_else(|| recovery_refusal("selector target is not UTF-8"))?;

    let staged_id = match record.staging_identity {
        Some(expected_id) => {
            let observed = verify_symlink(parent, OsStr::new(&record.staging_name), target_str)?;
            if observed != expected_id {
                return Err(recovery_refusal("staged selector identity mismatch"));
            }
            expected_id
        }
        None => {
            if stat_no_follow(parent, OsStr::new(&record.staging_name))?.is_some() {
                return Err(recovery_refusal("unexpected staging symlink exists"));
            }
            rustix::fs::symlinkat(target_str, parent, record.staging_name.as_str())
                .map_err(|_| recovery_refusal("cannot stage selector symlink on recovery"))?;
            parent
                .sync_all()
                .map_err(|_| recovery_refusal("cannot sync parent on recovery stage"))?;
            let id = verify_symlink(parent, OsStr::new(&record.staging_name), target_str)?;
            record.staging_identity = Some(id);
            record.phase = SelectorEffectPhase::SelectorStaged;
            persist_record(guard, record)?;
            id
        }
    };
    check_live_selector_matches_prior(parent, leaf, record.previous.as_ref())?;
    renameat(parent, record.staging_name.as_str(), parent, leaf)
        .map_err(|_| recovery_refusal("cannot publish selector during reconciliation"))?;
    parent
        .sync_all()
        .map_err(|_| recovery_refusal("cannot sync parent after recovery publish"))?;
    if verify_symlink(parent, leaf, target_str)? != staged_id {
        return Err(recovery_refusal(
            "published recovery selector identity mismatch",
        ));
    }
    record.phase = SelectorEffectPhase::SelectorPublished;
    persist_record(guard, record)?;
    verify_completed_selector(parent, parent_path, record, sealed, signature)?;
    record.phase = SelectorEffectPhase::Verified;
    persist_record(guard, record)?;
    Ok(SelectorRecoveryOutcome::Verified)
}

fn validate_completion_inputs(
    parent_path: &Path,
    record: &SelectorEffectRecord,
    sealed: &SealedReleaseCustody,
    signature: &VerifiedSignature,
) -> Result<(), CustomerError> {
    if signature.revalidate().is_none()
        || signature.manifest_sha256() != record.input_manifest_sha256
    {
        return Err(recovery_refusal(
            "signature no longer authorizes selector record",
        ));
    }
    sealed.revalidate()?;
    if parent_path.join(&record.target) != sealed.root_path()
        || record.target_identity != sealed.root_identity()
        || record.input_manifest_sha256 != sealed.input_manifest_sha256()
        || record.sealed_manifest_sha256 != sealed.installed_manifest_sha256()
    {
        return Err(recovery_refusal(
            "sealed release does not match selector effect record",
        ));
    }
    Ok(())
}

fn verify_completed_selector(
    parent: &File,
    parent_path: &Path,
    record: &SelectorEffectRecord,
    sealed: &SealedReleaseCustody,
    signature: &VerifiedSignature,
) -> Result<(), CustomerError> {
    validate_completion_inputs(parent_path, record, sealed, signature)?;
    let leaf = record
        .selector
        .file_name()
        .ok_or_else(|| recovery_refusal("selector name is invalid"))?;
    let target_str = record
        .target
        .to_str()
        .ok_or_else(|| recovery_refusal("selector target is not UTF-8"))?;
    let observed_id = verify_symlink(parent, leaf, target_str)?;
    if record.staging_identity != Some(observed_id) {
        return Err(recovery_refusal(
            "live selector inode disagrees with recorded selector effect",
        ));
    }
    if crate::installed_image::verify_release_manifest(&record.selector).is_none()
        || crate::installed_image::observe_installed_input_bundle_digest(&record.selector)
            .as_deref()
            != Some(record.input_manifest_sha256.as_str())
    {
        return Err(recovery_refusal(
            "completed selector manifest verification failed",
        ));
    }
    Ok(())
}

/// Safely remove an owned, uncommitted release directory after checking `(dev, ino)`
/// and confirming `selector` does not reference it.
pub(in crate::customer) fn remove_owned_unselected_release(
    release_path: &Path,
    expected_identity: Identity,
    selector: &Path,
) -> Result<(), CustomerError> {
    if let Some(parent_path) = selector.parent().filter(|p| p.is_absolute())
        && let Some(leaf) = selector.file_name()
        && let Ok(Some(parent)) = open_directory_no_follow(parent_path, false)
        && let Ok(st) = statat(&parent, leaf, AtFlags::SYMLINK_NOFOLLOW)
        && rustix::fs::FileType::from_raw_mode(st.st_mode) == rustix::fs::FileType::Symlink
        && let Ok(link) = rustix::fs::readlinkat(&parent, leaf, Vec::new())
        && let Ok(link_str) = link.to_str()
        && parent_path.join(link_str) == release_path
    {
        return Err(recovery_refusal(
            "refusing removal of actively selected release directory",
        ));
    }
    let parent_dir_path = release_path
        .parent()
        .filter(|p| p.is_absolute())
        .ok_or_else(|| recovery_refusal("release parent path is invalid"))?;
    let leaf = release_path
        .file_name()
        .ok_or_else(|| recovery_refusal("release directory name is missing"))?;
    let releases_parent = open_directory_no_follow(parent_dir_path, false)?
        .ok_or_else(|| recovery_refusal("releases parent directory is missing"))?;
    validate_directory(&releases_parent, "releases parent")?;
    let st = match statat(&releases_parent, leaf, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => st,
        Err(rustix::io::Errno::NOENT) => return Ok(()),
        Err(_) => return Err(recovery_refusal("cannot stat uncommitted release")),
    };
    if (st.st_dev, st.st_ino) != expected_identity
        || rustix::fs::FileType::from_raw_mode(st.st_mode) != rustix::fs::FileType::Directory
    {
        return Err(recovery_refusal(
            "release directory identity changed; refusing removal",
        ));
    }
    let dir_fd = File::from(
        rustix::fs::openat(
            &releases_parent,
            leaf,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| recovery_refusal("cannot open uncommitted release"))?,
    );
    clear_dir_recursive(&dir_fd)?;
    unlinkat(&releases_parent, leaf, AtFlags::REMOVEDIR)
        .map_err(|_| recovery_refusal("cannot remove uncommitted release root"))?;
    releases_parent
        .sync_all()
        .map_err(|_| recovery_refusal("cannot sync releases parent after cleanup"))
}

fn clear_dir_recursive(dir: &File) -> Result<(), CustomerError> {
    let _ = rustix::fs::fchmod(dir, Mode::from_raw_mode(0o755));
    let mut entries =
        Dir::read_from(dir).map_err(|_| recovery_refusal("cannot read uncommitted release dir"))?;
    let mut names = Vec::new();
    while let Some(item) = entries.read() {
        let item = item.map_err(|_| recovery_refusal("cannot read entry in release dir"))?;
        let name = item
            .file_name()
            .to_str()
            .map_err(|_| recovery_refusal("non-UTF-8 name during cleanup"))?;
        if matches!(name, "." | "..") {
            continue;
        }
        names.push(name.to_owned());
    }
    for name in names {
        let st = statat(dir, name.as_str(), AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|_| recovery_refusal("cannot stat entry during cleanup"))?;
        if rustix::fs::FileType::from_raw_mode(st.st_mode) == rustix::fs::FileType::Directory {
            let sub = File::from(
                rustix::fs::openat(
                    dir,
                    name.as_str(),
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|_| recovery_refusal("cannot open subdir during cleanup"))?,
            );
            clear_dir_recursive(&sub)?;
            unlinkat(dir, name.as_str(), AtFlags::REMOVEDIR)
                .map_err(|_| recovery_refusal("cannot remove release subdir"))?;
        } else {
            unlinkat(dir, name.as_str(), AtFlags::empty())
                .map_err(|_| recovery_refusal("cannot unlink file in release"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
