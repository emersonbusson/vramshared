// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_selector/effect.rs
//! Atomic selector switch journal, publication, and exact rollback.

use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use rustix::fs::{
    AtFlags, FileType, Gid, Uid, chownat, readlinkat, renameat, statat, symlinkat, unlinkat,
};
use rustix::process::{geteuid, getpid};
use serde::{Deserialize, Serialize};

use super::super::CustomerError;
use super::super::transition::{
    CustomerTransitionGuard, PackageAction, PackagePending, read_journal_body, recovery_refusal,
    write_journal_record,
};
use super::super::transition_custody::{open_directory_no_follow, validate_directory};
use super::intent::IntentCustody;
use super::sealing::SealedReleaseCustody;
use super::tree_custody::TreeCustody;

pub(super) const EFFECT_JOURNAL: &str = "package-selector-effect.json";

pub(super) type Identity = (u64, u64);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::customer) enum SelectorEffectPhase {
    IntentBound,
    SelectorStaged,
    SelectorPublished,
    Verified,
    RolledBack,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::customer) struct PreviousSelectorRecord {
    pub(in crate::customer) target: PathBuf,
    pub(in crate::customer) entry_identity: Identity,
    pub(in crate::customer) root_identity: Identity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::customer) input_manifest_sha256: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::customer) struct SelectorEffectRecord {
    pub(in crate::customer) schema: u8,
    pub(in crate::customer) phase: SelectorEffectPhase,
    pub(in crate::customer) pending: PackagePending,
    pub(in crate::customer) selector: PathBuf,
    pub(in crate::customer) parent_identity: Identity,
    pub(in crate::customer) target: PathBuf,
    pub(in crate::customer) target_identity: Identity,
    pub(in crate::customer) input_manifest_sha256: String,
    pub(in crate::customer) sealed_manifest_sha256: String,
    pub(in crate::customer) staging_name: String,
    pub(in crate::customer) staging_identity: Option<Identity>,
    pub(in crate::customer) previous: Option<PreviousSelectorRecord>,
}

pub(in crate::customer) struct SelectorEffectHooks<'a> {
    pub after_stage: &'a mut dyn FnMut() -> Result<(), CustomerError>,
    pub before_rename: &'a mut dyn FnMut() -> Result<(), CustomerError>,
    pub after_rename: &'a mut dyn FnMut() -> Result<(), CustomerError>,
}

pub(in crate::customer) fn effect_journal_present(
    paths: &super::super::transition::TransitionPaths,
) -> Result<bool, CustomerError> {
    read_journal_body(paths, OsStr::new(EFFECT_JOURNAL), "selector effect journal")
        .map(|body| body.is_some())
        .map_err(|_| recovery_refusal("selector effect journal unavailable; retain evidence"))
}

/// Atomically stage and publish the selector to a verified, sealed release.
pub(in crate::customer) fn publish_sealed_selector(
    intent: &IntentCustody<'_, '_>,
    sealed: &SealedReleaseCustody,
) -> Result<SelectorEffectRecord, CustomerError> {
    let mut noop1 = || Ok(());
    let mut noop2 = || Ok(());
    let mut noop3 = || Ok(());
    let mut hooks = SelectorEffectHooks {
        after_stage: &mut noop1,
        before_rename: &mut noop2,
        after_rename: &mut noop3,
    };
    publish_sealed_selector_with_hooks(intent, sealed, &mut hooks)
}

pub(in crate::customer) fn publish_sealed_selector_with_hooks(
    intent: &IntentCustody<'_, '_>,
    sealed: &SealedReleaseCustody,
    hooks: &mut SelectorEffectHooks<'_>,
) -> Result<SelectorEffectRecord, CustomerError> {
    let guard = intent.guard();
    if effect_journal_present(guard.paths())? {
        return Err(recovery_refusal(
            "selector effect journal already exists; requires reconciliation",
        ));
    }
    intent.revalidate_prior_and_intent()?;
    sealed.revalidate()?;
    verify_custody_alignment(intent, sealed)?;

    let parent = intent.parent_file();
    let parent_path = intent
        .selector()
        .parent()
        .ok_or_else(|| recovery_refusal("selector parent is invalid"))?;
    let leaf = intent
        .selector()
        .file_name()
        .ok_or_else(|| recovery_refusal("selector name is invalid"))?;
    let leaf_str = leaf
        .to_str()
        .ok_or_else(|| recovery_refusal("selector name is not UTF-8"))?;

    let target_str = intent
        .target_rel()
        .to_str()
        .ok_or_else(|| recovery_refusal("selector target is not UTF-8"))?;
    let staging_name = format!(".{leaf_str}.{}.staging", getpid().as_raw_pid());
    ensure_absent(parent, staging_name.as_str())?;

    let mut record = SelectorEffectRecord {
        schema: 1,
        phase: SelectorEffectPhase::IntentBound,
        pending: intent.pending().clone(),
        selector: intent.selector().to_path_buf(),
        parent_identity: intent.parent_identity(),
        target: intent.target_rel().to_path_buf(),
        target_identity: intent.target_identity(),
        input_manifest_sha256: sealed.input_manifest_sha256().to_owned(),
        sealed_manifest_sha256: sealed.installed_manifest_sha256().to_owned(),
        staging_name: staging_name.clone(),
        staging_identity: None,
        previous: intent.previous_record_parts().map(
            |(target, entry_identity, root_identity, input_hash)| PreviousSelectorRecord {
                target: target.to_path_buf(),
                entry_identity,
                root_identity,
                input_manifest_sha256: input_hash.map(str::to_owned),
            },
        ),
    };
    persist_record(guard, &record)?;

    let outcome = execute_switch_steps(
        guard,
        parent,
        parent_path,
        leaf,
        target_str,
        intent,
        sealed,
        &mut record,
        hooks,
    );
    match outcome {
        Ok(()) => Ok(record),
        Err(failure) => {
            if let Err(rollback_err) = rollback_selector_effect(
                guard,
                parent,
                parent_path,
                &mut record,
                intent.previous_custody(),
            ) {
                return Err(recovery_refusal(format!(
                    "selector effect failed ({}) and rollback refused ({}); retain evidence",
                    failure.detail, rollback_err.detail
                )));
            }
            Err(failure)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_switch_steps(
    guard: &CustomerTransitionGuard,
    parent: &File,
    parent_path: &Path,
    leaf: &OsStr,
    target_str: &str,
    intent: &IntentCustody<'_, '_>,
    sealed: &SealedReleaseCustody,
    record: &mut SelectorEffectRecord,
    hooks: &mut SelectorEffectHooks<'_>,
) -> Result<(), CustomerError> {
    symlinkat(target_str, parent, record.staging_name.as_str())
        .map_err(|_| recovery_refusal("cannot stage selector symlink"))?;
    if geteuid().as_raw() == 0 {
        chownat(
            parent,
            record.staging_name.as_str(),
            Some(Uid::ROOT),
            Some(Gid::ROOT),
            AtFlags::SYMLINK_NOFOLLOW,
        )
        .map_err(|_| recovery_refusal("cannot chown staged selector symlink"))?;
    }
    parent
        .sync_all()
        .map_err(|_| recovery_refusal("cannot sync selector parent after stage"))?;
    let staging_id = verify_symlink(parent, OsStr::new(&record.staging_name), target_str)?;
    record.staging_identity = Some(staging_id);
    record.phase = SelectorEffectPhase::SelectorStaged;
    persist_record(guard, record)?;
    (hooks.after_stage)()?;

    intent.revalidate_prior_and_intent()?;
    sealed.revalidate()?;
    verify_custody_alignment(intent, sealed)?;
    let observed_staging = verify_symlink(parent, OsStr::new(&record.staging_name), target_str)?;
    if observed_staging != staging_id {
        return Err(recovery_refusal("staged selector symlink identity changed"));
    }
    check_live_selector_matches_prior(parent, leaf, record.previous.as_ref())?;
    (hooks.before_rename)()?;

    renameat(parent, record.staging_name.as_str(), parent, leaf)
        .map_err(|_| recovery_refusal("cannot publish selector symlink atomically"))?;
    parent
        .sync_all()
        .map_err(|_| recovery_refusal("cannot sync selector parent after publish"))?;
    let live_id = verify_symlink(parent, leaf, target_str)?;
    if live_id != staging_id {
        return Err(recovery_refusal(
            "published selector identity disagrees with staged symlink",
        ));
    }
    record.phase = SelectorEffectPhase::SelectorPublished;
    persist_record(guard, record)?;
    (hooks.after_rename)()?;

    intent.revalidate_published_intent()?;
    sealed.revalidate()?;
    let post_id = verify_symlink(parent, leaf, target_str)?;
    if post_id != staging_id || dir_identity(parent_path)? != record.parent_identity {
        return Err(recovery_refusal("published selector changed before verify"));
    }
    if crate::installed_image::verify_release_manifest(intent.selector()).is_none()
        || crate::installed_image::observe_installed_input_bundle_digest(intent.selector())
            .as_deref()
            != Some(sealed.input_manifest_sha256())
    {
        return Err(recovery_refusal(
            "published selector release manifest check failed",
        ));
    }
    record.phase = SelectorEffectPhase::Verified;
    persist_record(guard, record)
}

pub(super) fn rollback_selector_effect(
    guard: &CustomerTransitionGuard,
    parent: &File,
    parent_path: &Path,
    record: &mut SelectorEffectRecord,
    previous_custody: Option<&TreeCustody>,
) -> Result<(), CustomerError> {
    guard.revalidate()?;
    if dir_identity(parent_path)? != record.parent_identity {
        return Err(recovery_refusal(
            "selector parent identity changed before rollback",
        ));
    }
    let leaf = record
        .selector
        .file_name()
        .ok_or_else(|| recovery_refusal("selector name is invalid"))?;
    let target_str = record
        .target
        .to_str()
        .ok_or_else(|| recovery_refusal("selector target is not UTF-8"))?;

    cleanup_owned_staging_link(
        parent,
        record.staging_name.as_str(),
        record.staging_identity,
    )?;

    let selector_stat = stat_no_follow(parent, leaf)?;
    match (selector_stat, record.staging_identity, &record.previous) {
        (Some(live_id), Some(owned_id), None) if live_id == owned_id => {
            if verify_symlink(parent, leaf, target_str)? != owned_id {
                return Err(recovery_refusal("selector target changed before unlink"));
            }
            unlinkat(parent, leaf, AtFlags::empty())
                .map_err(|_| recovery_refusal("cannot unlink published selector on rollback"))?;
            parent
                .sync_all()
                .map_err(|_| recovery_refusal("cannot sync parent on selector unlink"))?;
        }
        (Some(live_id), Some(owned_id), Some(prev)) if live_id == owned_id => {
            if verify_symlink(parent, leaf, target_str)? != owned_id {
                return Err(recovery_refusal("selector target changed before restore"));
            }
            if let Some(custody) = previous_custody {
                custody.revalidate()?;
            }
            restore_prior_symlink(parent, parent_path, leaf, prev)?;
        }
        (None, _, None) => {}
        (Some(live_id), _, Some(prev)) if live_id == prev.entry_identity => {
            check_live_selector_matches_prior(parent, leaf, Some(prev))?;
        }
        _ => {
            return Err(recovery_refusal(
                "selector state is foreign or ambiguous; refusing destructive rollback",
            ));
        }
    }
    record.phase = SelectorEffectPhase::RolledBack;
    persist_record(guard, record)
}

pub(super) fn restore_prior_symlink(
    parent: &File,
    parent_path: &Path,
    leaf: &OsStr,
    prev: &PreviousSelectorRecord,
) -> Result<(), CustomerError> {
    let prev_target = prev
        .target
        .to_str()
        .ok_or_else(|| recovery_refusal("previous target is not UTF-8"))?;
    let prev_root = open_directory_no_follow(&parent_path.join(&prev.target), false)?
        .ok_or_else(|| recovery_refusal("previous target directory is absent"))?;
    validate_directory(&prev_root, "previous release root")?;
    let prev_meta = prev_root
        .metadata()
        .map_err(|_| recovery_refusal("previous root metadata unavailable"))?;
    if (prev_meta.dev(), prev_meta.ino()) != prev.root_identity {
        return Err(recovery_refusal(
            "previous release root identity changed before rollback",
        ));
    }
    let rollback_name = format!(
        ".rollback-{}.{}.staging",
        leaf.to_str().unwrap_or("current"),
        getpid().as_raw_pid()
    );
    ensure_absent(parent, rollback_name.as_str())?;
    symlinkat(prev_target, parent, rollback_name.as_str())
        .map_err(|_| recovery_refusal("cannot create rollback selector symlink"))?;
    if geteuid().as_raw() == 0 {
        chownat(
            parent,
            rollback_name.as_str(),
            Some(Uid::ROOT),
            Some(Gid::ROOT),
            AtFlags::SYMLINK_NOFOLLOW,
        )
        .map_err(|_| recovery_refusal("cannot chown rollback selector symlink"))?;
    }
    parent
        .sync_all()
        .map_err(|_| recovery_refusal("cannot sync parent for rollback selector"))?;
    let rb_id = verify_symlink(parent, OsStr::new(&rollback_name), prev_target)?;
    renameat(parent, rollback_name.as_str(), parent, leaf)
        .map_err(|_| recovery_refusal("cannot restore prior selector atomically"))?;
    parent
        .sync_all()
        .map_err(|_| recovery_refusal("cannot sync parent after selector restore"))?;
    if verify_symlink(parent, leaf, prev_target)? != rb_id {
        return Err(recovery_refusal("restored selector identity mismatch"));
    }
    Ok(())
}

pub(super) fn persist_record(
    guard: &CustomerTransitionGuard,
    record: &SelectorEffectRecord,
) -> Result<(), CustomerError> {
    let mut encoded = serde_json::to_string(record)
        .map_err(|_| recovery_refusal("cannot encode selector effect record"))?;
    encoded.push('\n');
    write_journal_record(
        guard,
        OsStr::new(EFFECT_JOURNAL),
        "selector effect journal",
        &encoded,
    )?;
    let readback = read_journal_body(
        guard.paths(),
        OsStr::new(EFFECT_JOURNAL),
        "selector effect journal",
    )?
    .ok_or_else(|| recovery_refusal("selector effect journal disappeared"))?;
    if readback != encoded {
        return Err(recovery_refusal(
            "selector effect journal readback mismatch",
        ));
    }
    Ok(())
}

fn verify_custody_alignment(
    intent: &IntentCustody<'_, '_>,
    sealed: &SealedReleaseCustody,
) -> Result<(), CustomerError> {
    let expected_action = intent.pending().action;
    if !matches!(
        expected_action,
        PackageAction::Install | PackageAction::Upgrade
    ) || intent.target_path() != sealed.root_path()
        || intent.target_identity() != sealed.root_identity()
        || intent.pending().new_hash != sealed.input_manifest_sha256()
    {
        return Err(recovery_refusal(
            "sealed release disagrees with selector intent",
        ));
    }
    Ok(())
}

fn dir_identity(path: &Path) -> Result<Identity, CustomerError> {
    let dir = open_directory_no_follow(path, false)?
        .ok_or_else(|| recovery_refusal("selector parent disappeared"))?;
    validate_directory(&dir, "selector parent")?;
    let meta = dir
        .metadata()
        .map_err(|_| recovery_refusal("selector parent metadata unavailable"))?;
    Ok((meta.dev(), meta.ino()))
}

fn ensure_absent(parent: &File, name: &str) -> Result<(), CustomerError> {
    match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Err(rustix::io::Errno::NOENT) => Ok(()),
        _ => Err(recovery_refusal("selector staging entry already exists")),
    }
}

pub(super) fn stat_no_follow(
    parent: &File,
    name: &OsStr,
) -> Result<Option<Identity>, CustomerError> {
    match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => {
            if FileType::from_raw_mode(st.st_mode) != FileType::Symlink {
                return Err(recovery_refusal("selector entry is not a symlink"));
            }
            Ok(Some((st.st_dev, st.st_ino)))
        }
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(_) => Err(recovery_refusal("selector stat failed")),
    }
}

pub(super) fn verify_symlink(
    parent: &File,
    name: &OsStr,
    expected_target: &str,
) -> Result<Identity, CustomerError> {
    let st = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|_| recovery_refusal("selector symlink unavailable"))?;
    if FileType::from_raw_mode(st.st_mode) != FileType::Symlink {
        return Err(recovery_refusal("selector entry is not a symlink"));
    }
    let target = readlinkat(parent, name, Vec::new())
        .map_err(|_| recovery_refusal("selector readlink failed"))?;
    if target.to_str().ok() != Some(expected_target) {
        return Err(recovery_refusal("selector symlink target disagrees"));
    }
    Ok((st.st_dev, st.st_ino))
}

pub(super) fn check_live_selector_matches_prior(
    parent: &File,
    leaf: &OsStr,
    prior: Option<&PreviousSelectorRecord>,
) -> Result<(), CustomerError> {
    let live = stat_no_follow(parent, leaf)?;
    match (live, prior) {
        (None, None) => Ok(()),
        (Some(id), Some(prev)) if id == prev.entry_identity => {
            let prev_target = prev
                .target
                .to_str()
                .ok_or_else(|| recovery_refusal("previous target is not UTF-8"))?;
            let _ = verify_symlink(parent, leaf, prev_target)?;
            Ok(())
        }
        _ => Err(recovery_refusal("live selector changed externally")),
    }
}

pub(super) fn cleanup_owned_staging_link(
    parent: &File,
    staging_name: &str,
    expected_id: Option<Identity>,
) -> Result<(), CustomerError> {
    let observed = match statat(parent, staging_name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => {
            if FileType::from_raw_mode(st.st_mode) != FileType::Symlink {
                return Err(recovery_refusal("selector staging entry is not a symlink"));
            }
            Some((st.st_dev, st.st_ino))
        }
        Err(rustix::io::Errno::NOENT) => None,
        Err(_) => return Err(recovery_refusal("selector staging stat failed")),
    };
    if let Some(obs_id) = observed {
        if expected_id != Some(obs_id) {
            return Err(recovery_refusal(
                "foreign selector staging link found; refusing unlink",
            ));
        }
        unlinkat(parent, staging_name, AtFlags::empty())
            .map_err(|_| recovery_refusal("cannot unlink selector staging symlink"))?;
        parent
            .sync_all()
            .map_err(|_| recovery_refusal("cannot sync parent after staging unlink"))?;
    }
    Ok(())
}
