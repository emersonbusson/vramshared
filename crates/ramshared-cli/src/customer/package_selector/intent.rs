// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_selector/intent.rs
//! Persist the observed selector and prepared target before selector effects.

use std::ffi::OsStr;
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{AtFlags, FileType, Mode, OFlags, fstat, openat, readlinkat, statat};
use serde::{Deserialize, Serialize};

use super::super::CustomerError;
use super::super::package_prepare::PreparedRelease;
use super::super::package_source::DeclaredBundle;
use super::super::package_stage::StageSlot;
use super::super::package_transition::AdmittedPackage;
use super::super::package_trust::VerifiedSignature;
use super::super::transition::{
    CustomerTransitionGuard, PackageAction, PackagePending, read_journal_body, read_pending,
    recovery_refusal,
};
use super::super::transition_custody::{open_directory_no_follow, validate_directory};

type Identity = (u64, u64);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Previous {
    target: PathBuf,
    entry_identity: Identity,
    root_identity: Identity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    input_bundle_manifest_sha256: Option<String>,
}

struct PreviousSnapshot {
    record: Option<Previous>,
    entry: Option<File>,
    root: Option<File>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    schema: u8,
    pending: PackagePending,
    selector: PathBuf,
    parent_identity: Identity,
    target: PathBuf,
    target_identity: Identity,
    previous: Option<Previous>,
}

/// Holds the original evidence; it is not permission to change the selector.
pub(in crate::customer) struct IntentCustody<'a, 'bundle> {
    journal: StageSlot,
    parent: File,
    record: Intent,
    bundle: &'a DeclaredBundle,
    previous_entry: Option<File>,
    previous_root: Option<File>,
    prepared_custody: super::tree_custody::TreeCustody,
    previous_custody: Option<super::tree_custody::TreeCustody>,
    transaction: &'a AdmittedPackage<'bundle>,
    release: &'a PreparedRelease,
    signature: &'a VerifiedSignature,
    previous: Option<&'a super::PreviousEvidence<'a>>,
}

impl<'a, 'bundle> IntentCustody<'a, 'bundle> {
    /// Revalidate guard, journal, selector parent/prior entry, and held tree custody.
    pub(in crate::customer) fn revalidate(&self) -> Result<(), CustomerError> {
        let guard = self.transaction.guard();
        revalidate_selector_state(
            guard,
            &self.record.pending,
            self.bundle,
            self.release,
            self.signature,
            self.previous,
            &self.parent,
            &self.record,
            &self.prepared_custody,
            self.previous_custody.as_ref(),
            self.previous_entry.as_ref(),
            self.previous_root.as_ref(),
        )?;
        verify_journal(guard, &self.record, &self.journal)
    }

    #[cfg(test)]
    pub(in crate::customer) fn held_fd_count(&self) -> usize {
        self.prepared_custody.held_fd_count()
            + self
                .previous_custody
                .as_ref()
                .map_or(0, super::tree_custody::TreeCustody::held_fd_count)
    }

    pub(in crate::customer) fn prepared_custody(&self) -> &super::tree_custody::TreeCustody {
        &self.prepared_custody
    }

    pub(in crate::customer) fn revalidate_prior_and_intent(&self) -> Result<(), CustomerError> {
        self.revalidate_intent_custody(true)
    }

    pub(in crate::customer) fn revalidate_published_intent(&self) -> Result<(), CustomerError> {
        self.revalidate_intent_custody(false)
    }

    fn revalidate_intent_custody(
        &self,
        require_live_prior_selector: bool,
    ) -> Result<(), CustomerError> {
        let guard = self.transaction.guard();
        guard.revalidate()?;
        let parent_path = self
            .record
            .selector
            .parent()
            .ok_or_else(|| recovery_refusal("selector parent is invalid"))?;
        if self.signature.revalidate().is_none()
            || !previous_signature_matches(&self.record.pending, self.previous)
            || read_pending(guard.paths())?.as_ref() != Some(&self.record.pending)
            || !parent_matches(parent_path, &self.parent, self.record.parent_identity)?
        {
            return Err(recovery_refusal(
                "selector observation changed; retain evidence",
            ));
        }
        if require_live_prior_selector {
            let leaf = self
                .record
                .selector
                .file_name()
                .ok_or_else(|| recovery_refusal("selector name is invalid"))?;
            if previous(&self.parent, parent_path, leaf)?.record != self.record.previous {
                return Err(recovery_refusal(
                    "selector observation changed; retain evidence",
                ));
            }
        }
        if let Some(prev_meta) = self.record.previous.as_ref() {
            let held_entry = self
                .previous_entry
                .as_ref()
                .and_then(|file| identity(file).ok())
                .ok_or_else(|| recovery_refusal("previous entry handle unavailable"))?;
            let held_root = self
                .previous_root
                .as_ref()
                .and_then(|file| identity(file).ok())
                .ok_or_else(|| recovery_refusal("previous root handle unavailable"))?;
            if held_entry != prev_meta.entry_identity || held_root != prev_meta.root_identity {
                return Err(recovery_refusal("previous selector handle changed"));
            }
            let custody = self
                .previous_custody
                .as_ref()
                .ok_or_else(|| recovery_refusal("previous release custody is absent"))?;
            custody.revalidate()?;
        }
        verify_journal(guard, &self.record, &self.journal)
    }

    pub(in crate::customer) fn guard(&self) -> &CustomerTransitionGuard {
        self.transaction.guard()
    }

    pub(in crate::customer) fn pending(&self) -> &PackagePending {
        &self.record.pending
    }

    pub(in crate::customer) fn selector(&self) -> &Path {
        &self.record.selector
    }

    pub(in crate::customer) fn parent_file(&self) -> &File {
        &self.parent
    }

    pub(in crate::customer) fn parent_identity(&self) -> Identity {
        self.record.parent_identity
    }

    pub(in crate::customer) fn target_rel(&self) -> &Path {
        &self.record.target
    }

    pub(in crate::customer) fn target_path(&self) -> &Path {
        self.release.tree.path()
    }

    pub(in crate::customer) fn target_identity(&self) -> Identity {
        self.record.target_identity
    }

    pub(in crate::customer) fn previous_record_parts(
        &self,
    ) -> Option<(&Path, Identity, Identity, Option<&str>)> {
        self.record.previous.as_ref().map(|prev| {
            (
                prev.target.as_path(),
                prev.entry_identity,
                prev.root_identity,
                prev.input_bundle_manifest_sha256.as_deref(),
            )
        })
    }

    pub(in crate::customer) fn signature(&self) -> &VerifiedSignature {
        self.signature
    }

    pub(in crate::customer) fn previous_custody(
        &self,
    ) -> Option<&super::tree_custody::TreeCustody> {
        self.previous_custody.as_ref()
    }
}

fn identity(file: &File) -> Result<Identity, CustomerError> {
    let metadata = file
        .metadata()
        .map_err(|_| recovery_refusal("selector identity unavailable"))?;
    Ok((metadata.dev(), metadata.ino()))
}

fn relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

fn previous(
    parent: &File,
    parent_path: &Path,
    leaf: &OsStr,
) -> Result<PreviousSnapshot, CustomerError> {
    let stat = match statat(parent, leaf, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(rustix::io::Errno::NOENT) => {
            return Ok(PreviousSnapshot {
                record: None,
                entry: None,
                root: None,
            });
        }
        Err(_) => return Err(recovery_refusal("selector entry unavailable")),
    };
    if FileType::from_raw_mode(stat.st_mode) != FileType::Symlink {
        return Err(recovery_refusal("existing selector is not a symlink"));
    }
    let entry = File::from(
        openat(
            parent,
            leaf,
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| recovery_refusal("selector entry cannot be held"))?,
    );
    let held = fstat(&entry).map_err(|_| recovery_refusal("selector entry unavailable"))?;
    if (held.st_dev, held.st_ino) != (stat.st_dev, stat.st_ino)
        || FileType::from_raw_mode(held.st_mode) != FileType::Symlink
    {
        return Err(recovery_refusal("selector entry changed"));
    }
    let target = readlinkat(parent, leaf, Vec::new())
        .map_err(|_| recovery_refusal("selector target unavailable"))?;
    let text = target
        .to_str()
        .map_err(|_| recovery_refusal("selector target is not UTF-8"))?;
    if text.len() > 4096
        || text.chars().any(char::is_control)
        || text
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || !relative(Path::new(text))
    {
        return Err(recovery_refusal("selector target is unsafe"));
    }
    let root = open_directory_no_follow(&parent_path.join(text), false)?
        .ok_or_else(|| recovery_refusal("previous selector target is absent"))?;
    validate_directory(&root, "previous selector target")?;
    let root_identity = identity(&root)?;
    let input_bundle_manifest_sha256 =
        crate::installed_image::observe_installed_input_bundle_digest(&parent_path.join(text));
    let named_root = open_directory_no_follow(&parent_path.join(text), false)?
        .ok_or_else(|| recovery_refusal("previous selector target disappeared"))?;
    if identity(&named_root)? != root_identity {
        return Err(recovery_refusal("previous selector target changed"));
    }
    let current = statat(parent, leaf, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|_| recovery_refusal("selector entry changed"))?;
    if (current.st_dev, current.st_ino) != (stat.st_dev, stat.st_ino)
        || readlinkat(parent, leaf, Vec::new()).ok().as_deref() != Some(target.as_c_str())
    {
        return Err(recovery_refusal("selector entry changed"));
    }
    Ok(PreviousSnapshot {
        record: Some(Previous {
            target: PathBuf::from(text),
            entry_identity: (stat.st_dev, stat.st_ino),
            root_identity,
            input_bundle_manifest_sha256,
        }),
        entry: Some(entry),
        root: Some(root),
    })
}

fn parent_matches(path: &Path, held: &File, expected: Identity) -> Result<bool, CustomerError> {
    let named = open_directory_no_follow(path, false)?
        .ok_or_else(|| recovery_refusal("selector parent disappeared"))?;
    validate_directory(&named, "selector parent")?;
    Ok(identity(held)? == expected && identity(&named)? == expected)
}

#[allow(clippy::too_many_arguments)]
fn revalidate_selector_state(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    release: &PreparedRelease,
    signature: &VerifiedSignature,
    evidence: Option<&super::PreviousEvidence<'_>>,
    parent: &File,
    record: &Intent,
    prepared_custody: &super::tree_custody::TreeCustody,
    previous_custody: Option<&super::tree_custody::TreeCustody>,
    previous_entry: Option<&File>,
    previous_root: Option<&File>,
) -> Result<(), CustomerError> {
    guard.revalidate()?;
    let parent_path = record
        .selector
        .parent()
        .ok_or_else(|| recovery_refusal("selector parent is invalid"))?;
    let leaf = record
        .selector
        .file_name()
        .ok_or_else(|| recovery_refusal("selector name is invalid"))?;
    let snapshot = previous(parent, parent_path, leaf)?;
    if signature.revalidate().is_none()
        || !previous_signature_matches(pending, evidence)
        || read_pending(guard.paths())?.as_ref() != Some(pending)
        || !parent_matches(parent_path, parent, record.parent_identity)?
        || snapshot.record != record.previous
    {
        return Err(recovery_refusal(
            "selector observation changed; retain evidence",
        ));
    }
    if let Some(prev_meta) = record.previous.as_ref() {
        let held_entry = previous_entry
            .and_then(|file| identity(file).ok())
            .ok_or_else(|| recovery_refusal("previous entry handle unavailable"))?;
        let held_root = previous_root
            .and_then(|file| identity(file).ok())
            .ok_or_else(|| recovery_refusal("previous root handle unavailable"))?;
        if held_entry != prev_meta.entry_identity || held_root != prev_meta.root_identity {
            return Err(recovery_refusal("previous selector handle changed"));
        }
        let proof =
            evidence.ok_or_else(|| recovery_refusal("previous signature proof is absent"))?;
        let custody = previous_custody
            .ok_or_else(|| recovery_refusal("previous release custody is absent"))?;
        custody.revalidate()?;
        if let Some(transform) = &proof.transformation
            && transform.sink.revalidate().is_none()
        {
            return Err(recovery_refusal("previous sink identity changed"));
        }
        if proof.signature.revalidate().is_none()
            || previous(parent, parent_path, leaf)?.record != record.previous
            || !parent_matches(parent_path, parent, record.parent_identity)?
        {
            return Err(recovery_refusal("previous signature proof changed"));
        }
    } else if previous_custody.is_some() || previous_entry.is_some() || previous_root.is_some() {
        return Err(recovery_refusal("unexpected previous release custody"));
    }
    super::super::package_tree::revalidate_owned_tree(
        guard,
        pending,
        bundle,
        &release.tree,
        false,
    )?;
    prepared_custody.revalidate()
}

fn previous_signature_matches(
    pending: &PackagePending,
    evidence: Option<&super::PreviousEvidence<'_>>,
) -> bool {
    match pending.action {
        PackageAction::Install => evidence.is_none(),
        PackageAction::Upgrade => evidence.is_some_and(|proof| {
            proof.signature.manifest_sha256() == pending.old_hash
                && proof.signature.revalidate().is_some()
                && proof
                    .transformation
                    .as_ref()
                    .is_none_or(|transform| transform.sink.revalidate().is_some())
        }),
        PackageAction::Remove => false,
    }
}

#[cfg(test)]
fn persist(guard: &CustomerTransitionGuard, record: &Intent) -> Result<StageSlot, CustomerError> {
    persist_with_hooks(guard, record, || {}, || {})
}

fn persist_with_hooks(
    guard: &CustomerTransitionGuard,
    record: &Intent,
    after_create: impl FnOnce(),
    after_sync: impl FnOnce(),
) -> Result<StageSlot, CustomerError> {
    let mut bytes = serde_json::to_vec(record)
        .map_err(|_| recovery_refusal("selector intent cannot be encoded"))?;
    bytes.push(b'\n');
    if bytes.len() as u64 > super::super::transition_custody::MAX_JOURNAL_BODY_BYTES {
        return Err(recovery_refusal("selector intent exceeds limit"));
    }
    let mut journal = StageSlot::create(guard.paths().transactions_root(), super::JOURNAL)
        .map_err(|_| recovery_refusal("selector journal cannot be created; retain evidence"))?;
    after_create();
    journal
        .file_mut()
        .write_all(&bytes)
        .and_then(|_| journal.file_mut().sync_all())
        .map_err(|_| recovery_refusal("selector intent could not be persisted; retain evidence"))?;
    journal
        .revalidate()
        .ok_or_else(|| recovery_refusal("selector journal changed"))?;
    journal
        .sync_directory()
        .ok_or_else(|| recovery_refusal("selector journal directory could not be synced"))?;
    after_sync();
    let body = read_journal_body(
        guard.paths(),
        OsStr::new(super::JOURNAL),
        "package selector journal",
    )?
    .ok_or_else(|| recovery_refusal("selector intent disappeared"))?;
    if body.as_bytes() != bytes {
        return Err(recovery_refusal("selector intent changed; retain evidence"));
    }
    Ok(journal)
}

fn verify_journal(
    guard: &CustomerTransitionGuard,
    record: &Intent,
    journal: &StageSlot,
) -> Result<(), CustomerError> {
    journal
        .revalidate()
        .ok_or_else(|| recovery_refusal("selector journal changed"))?;
    let body = read_journal_body(
        guard.paths(),
        OsStr::new(super::JOURNAL),
        "package selector journal",
    )?
    .ok_or_else(|| recovery_refusal("selector intent disappeared"))?;
    let mut expected = serde_json::to_vec(record)
        .map_err(|_| recovery_refusal("selector intent cannot be encoded"))?;
    expected.push(b'\n');
    if body.as_bytes() != expected {
        return Err(recovery_refusal("selector intent changed; retain evidence"));
    }
    journal
        .revalidate()
        .ok_or_else(|| recovery_refusal("selector journal changed"))
}

pub(super) fn verify_intent_pending_for_clear(
    paths: &super::super::transition::TransitionPaths,
    expected: &PackagePending,
) -> Result<(), CustomerError> {
    let Some(body) = read_journal_body(
        paths,
        OsStr::new(super::JOURNAL),
        "package selector journal",
    )
    .map_err(|_| recovery_refusal("package selector journal unavailable; retain evidence"))?
    else {
        return Ok(());
    };
    let parsed: Intent = serde_json::from_str(body.trim_end_matches('\n'))
        .map_err(|_| recovery_refusal("package selector journal is malformed; retain evidence"))?;
    if parsed.schema != 1 || &parsed.pending != expected {
        return Err(recovery_refusal(
            "package selector journal does not match transaction; retain evidence",
        ));
    }
    Ok(())
}

/// Write a durable observation of a fully prepared release, without switching it.
pub(in crate::customer) fn record_prepared_intent<'a, 'bundle>(
    transaction: &'a AdmittedPackage<'bundle>,
    pending: &PackagePending,
    bundle: &'a DeclaredBundle,
    release: &'a PreparedRelease,
    signature: &'a VerifiedSignature,
    previous: Option<&'a super::PreviousEvidence<'a>>,
    selector: &Path,
) -> Result<IntentCustody<'a, 'bundle>, CustomerError> {
    record_prepared_intent_with_hook(
        transaction,
        pending,
        bundle,
        release,
        (signature, previous),
        selector,
        || {},
    )
}

pub(in crate::customer) fn record_prepared_intent_with_hook<'a, 'bundle>(
    transaction: &'a AdmittedPackage<'bundle>,
    pending: &PackagePending,
    bundle: &'a DeclaredBundle,
    release: &'a PreparedRelease,
    signatures: (
        &'a VerifiedSignature,
        Option<&'a super::PreviousEvidence<'a>>,
    ),
    selector: &Path,
    after_sync: impl FnOnce(),
) -> Result<IntentCustody<'a, 'bundle>, CustomerError> {
    let (signature, prior_evidence) = signatures;
    let guard = transaction.guard();
    let limits = transaction
        .verification_limits()
        .ok_or_else(|| recovery_refusal("package verification limits are absent"))?;
    super::require_absent(guard.paths())?;
    if transaction.plan().action != pending.action
        || pending.action == PackageAction::Remove
        || !transaction.admits_source(bundle)
        || transaction.candidate_release() != pending.target_release.as_deref()
        || pending.new_hash != bundle.manifest_sha256()
        || pending.new_hash != signature.manifest_sha256()
        || signature.revalidate().is_none()
        || !previous_signature_matches(pending, prior_evidence)
        || read_pending(guard.paths())?.as_ref() != Some(pending)
    {
        return Err(recovery_refusal("selector transaction identity changed"));
    }
    let parent_path = selector
        .parent()
        .filter(|p| p.is_absolute())
        .ok_or_else(|| recovery_refusal("selector parent is invalid"))?;
    let leaf = selector
        .file_name()
        .ok_or_else(|| recovery_refusal("selector name is invalid"))?;
    let target = release
        .tree
        .path()
        .strip_prefix(parent_path)
        .map_err(|_| recovery_refusal("prepared release is outside selector parent"))?;
    if !relative(target) || target == Path::new(leaf) {
        return Err(recovery_refusal("prepared selector target is unsafe"));
    }
    let parent = open_directory_no_follow(parent_path, false)?
        .ok_or_else(|| recovery_refusal("selector parent is absent"))?;
    validate_directory(&parent, "selector parent")?;
    let parent_id = identity(&parent)?;
    let old = previous(&parent, parent_path, leaf)?;
    if (pending.action == PackageAction::Install) != old.record.is_none()
        || old.record.as_ref().is_some_and(|entry| {
            entry.input_bundle_manifest_sha256.as_deref() != Some(pending.old_hash.as_str())
        })
        || old
            .record
            .as_ref()
            .is_some_and(|entry| entry.target == target)
    {
        return Err(recovery_refusal("selector does not match package action"));
    }
    let mut budget = super::tree_custody::DescriptorBudget::acquire()?;
    let prepared_custody = super::tree_custody::TreeCustody::capture_prepared(
        guard,
        pending,
        bundle,
        release,
        &mut budget,
    )?;
    let previous_custody = match (&old.root, prior_evidence) {
        (Some(root), Some(proof)) => {
            let manifest = proof
                .signature
                .verified_manifest()
                .ok_or_else(|| recovery_refusal("previous signed manifest is unavailable"))?;
            Some(super::tree_custody::TreeCustody::capture_previous(
                root,
                &manifest,
                limits,
                proof.transformation.as_ref(),
                &mut budget,
            )?)
        }
        (None, None) => None,
        _ => {
            return Err(recovery_refusal(
                "selector observation changed; retain evidence",
            ));
        }
    };
    let record = Intent {
        schema: 1,
        pending: pending.clone(),
        selector: selector.to_path_buf(),
        parent_identity: parent_id,
        target: target.to_path_buf(),
        target_identity: release.tree.identity(),
        previous: old.record,
    };
    revalidate_selector_state(
        guard,
        pending,
        bundle,
        release,
        signature,
        prior_evidence,
        &parent,
        &record,
        &prepared_custody,
        previous_custody.as_ref(),
        old.entry.as_ref(),
        old.root.as_ref(),
    )?;
    let journal = persist_with_hooks(guard, &record, || {}, after_sync)?;
    revalidate_selector_state(
        guard,
        pending,
        bundle,
        release,
        signature,
        prior_evidence,
        &parent,
        &record,
        &prepared_custody,
        previous_custody.as_ref(),
        old.entry.as_ref(),
        old.root.as_ref(),
    )?;
    verify_journal(guard, &record, &journal)?;
    revalidate_selector_state(
        guard,
        pending,
        bundle,
        release,
        signature,
        prior_evidence,
        &parent,
        &record,
        &prepared_custody,
        previous_custody.as_ref(),
        old.entry.as_ref(),
        old.root.as_ref(),
    )?;
    Ok(IntentCustody {
        journal,
        parent,
        record,
        bundle,
        previous_entry: old.entry,
        previous_root: old.root,
        prepared_custody,
        previous_custody,
        transaction,
        release,
        signature,
        previous: prior_evidence,
    })
}

#[cfg(test)]
mod tests;
