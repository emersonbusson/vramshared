// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_publish.rs
//! Durable, exclusive publication of one member into an owned release root.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use rustix::fs::{
    AtFlags, Mode, OFlags, XattrFlags, fchmod, fgetxattr, fsetxattr, linkat, openat, statat,
};

use super::CustomerError;
use super::package_source::DeclaredBundle;
use super::package_stage::StageSlot;
use super::package_stage_journal::inventory::verify_staged_inventory;
use super::transition::{
    CustomerTransitionGuard, OperationOwner, PackageAction, PackagePending, TransitionPaths,
    read_journal_body, read_pending, recovery_refusal,
};
use super::transition_custody::{
    MAX_JOURNAL_BODY_BYTES, open_directory_no_follow, validate_directory, validate_regular,
};

pub(super) mod inventory;
mod journal;
pub(super) mod reconciliation;
#[cfg(test)]
mod test_support;
#[cfg(test)]
pub(super) use test_support::{
    publish_owned_with_after_link, publish_owned_with_before_link,
    publish_signed_owned_with_after_link, publish_signed_owned_with_before_link,
};

const CREATION_ATTRIBUTE: &str = "user.ramshared.publication";
type Identity = (u64, u64);

use journal::{JOURNAL, Phase, Record, persist, read_history, read_record};
pub(super) use journal::{clear_publication_journal, publication_journal_present};

fn directory(path: &Path, expected: Identity) -> Result<File, CustomerError> {
    if !path.is_absolute() {
        return Err(recovery_refusal("release directory must be absolute"));
    }
    let file = open_directory_no_follow(path, false)?
        .ok_or_else(|| recovery_refusal("release directory is absent"))?;
    validate_directory(&file, "release directory")?;
    let metadata = file
        .metadata()
        .map_err(|_| recovery_refusal("release directory is unavailable"))?;
    if (metadata.dev(), metadata.ino()) != expected {
        return Err(recovery_refusal("release directory identity changed"));
    }
    Ok(file)
}

fn current_identity(file: &File) -> Result<Identity, CustomerError> {
    let metadata = file
        .metadata()
        .map_err(|_| recovery_refusal("publication identity unavailable"))?;
    Ok((metadata.dev(), metadata.ino()))
}

fn safe_publication_mode(mode: u32) -> bool {
    mode & !0o777 == 0 && mode & 0o022 == 0 && mode & 0o400 != 0
}

fn target(root: &Path, destination: &str) -> Result<(PathBuf, String), CustomerError> {
    let path = Path::new(destination);
    if path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return Err(recovery_refusal("publication destination is unsafe"));
    }
    let leaf = path
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| recovery_refusal("publication name is invalid"))?;
    Ok((
        root.join(path.parent().unwrap_or(Path::new(""))),
        leaf.into(),
    ))
}

fn verify_custody(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
) -> Result<(), CustomerError> {
    if guard.owner() != OperationOwner::Package
        || pending.action == PackageAction::Remove
        || bundle.manifest_sha256() != pending.new_hash
        || read_pending(guard.paths())?.as_ref() != Some(pending)
    {
        return Err(recovery_refusal(
            "publication transaction identity does not match",
        ));
    }
    guard.revalidate()
}

fn verify_inventory(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
) -> Result<std::collections::BTreeMap<String, StageSlot>, CustomerError> {
    let mut slots = verify_staged_inventory(guard, pending, bundle)?;
    bundle
        .verify_staged_semantics(&mut slots)
        .ok_or_else(|| recovery_refusal("staged package identity is invalid"))?;
    Ok(slots)
}

fn verify_recorded_stage(
    record: &Record,
    slots: &std::collections::BTreeMap<String, StageSlot>,
) -> Result<(), CustomerError> {
    let slot = slots
        .get(&record.role)
        .ok_or_else(|| recovery_refusal("publication stage is absent"))?;
    if record.stage_identity.is_none()
        || record.stage_parent_identity.is_none()
        || slot.identity() != record.stage_identity
        || slot.parent_identity() != record.stage_parent_identity
    {
        return Err(recovery_refusal(
            "publication stage identity changed or is absent; retain evidence",
        ));
    }
    Ok(())
}

fn fill_anonymous(
    parent: &File,
    source: &mut StageSlot,
    digest: &str,
    mode: u32,
    creation_nonce: &[u8; 32],
) -> Result<(File, u64), CustomerError> {
    let size = source
        .file_mut()
        .metadata()
        .map_err(|_| recovery_refusal("stage size unavailable"))?
        .len();
    source
        .verify_bytes(digest, size)
        .ok_or_else(|| recovery_refusal("stage content changed"))?;
    let mut file = File::from(
        openat(
            parent,
            ".",
            OFlags::RDWR | OFlags::TMPFILE | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|_| recovery_refusal("anonymous publication is unsupported or unavailable"))?,
    );
    let input = source.file_mut();
    input
        .seek(SeekFrom::Start(0))
        .map_err(|_| recovery_refusal("stage cannot be read"))?;
    let limit = size
        .checked_add(1)
        .ok_or_else(|| recovery_refusal("stage size overflow"))?;
    let copied = std::io::copy(&mut input.take(limit), &mut file)
        .map_err(|_| recovery_refusal("anonymous publication copy failed"))?;
    if copied != size {
        return Err(recovery_refusal("stage size changed during copy"));
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|_| recovery_refusal("anonymous publication cannot be read"))?;
    if crate::installed_image::sha256_reader_with_limit(&mut file, size).as_deref() != Some(digest)
    {
        return Err(recovery_refusal("anonymous publication digest differs"));
    }
    fsetxattr(
        &file,
        CREATION_ATTRIBUTE,
        creation_nonce,
        XattrFlags::CREATE,
    )
    .map_err(|_| recovery_refusal("cannot seal publication creation identity"))?;
    verify_creation(&file, creation_nonce)?;
    fchmod(&file, Mode::from_raw_mode(mode))
        .map_err(|_| recovery_refusal("cannot set publication mode"))?;
    file.sync_all()
        .map_err(|_| recovery_refusal("cannot sync anonymous publication"))?;
    source
        .verify_bytes(digest, size)
        .ok_or_else(|| recovery_refusal("stage changed during publication copy"))?;
    Ok((file, size))
}

/// Publish one member; the caller must supply a root identity from its durable tree owner.
#[cfg(test)]
pub(super) fn publish_member(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    root: &Path,
    root_identity: Identity,
    role: &str,
) -> Result<(), CustomerError> {
    publish_with_hook(guard, pending, bundle, root, root_identity, role, || Ok(()))
}

#[cfg(test)]
pub(super) fn publish_with_hook(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    root: &Path,
    root_identity: Identity,
    role: &str,
    after_link: impl FnOnce() -> Result<(), CustomerError>,
) -> Result<(), CustomerError> {
    publish_with_hooks(
        guard,
        pending,
        bundle,
        root,
        root_identity,
        role,
        (after_link, || Ok(())),
    )
}

#[cfg(test)]
pub(super) fn publish_with_hooks(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    root: &Path,
    root_identity: Identity,
    role: &str,
    hooks: (
        impl FnOnce() -> Result<(), CustomerError>,
        impl FnOnce() -> Result<(), CustomerError>,
    ),
) -> Result<(), CustomerError> {
    publish_impl(
        guard,
        pending,
        bundle,
        (root, root_identity),
        role,
        None,
        PublicationHooks {
            before_link: || Ok(()),
            after_link: hooks.0,
            after_record: hooks.1,
            before_effect: || Ok(()),
            before_confirmation: || Ok(()),
        },
    )
}

pub(super) fn publish_owned_member(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    tree: &super::package_tree::OwnedTree,
    role: &str,
    signature: &super::package_trust::VerifiedSignature,
) -> Result<(), CustomerError> {
    super::package_selector::require_absent(guard.paths())?;
    publish_impl(
        guard,
        pending,
        bundle,
        (tree.path(), tree.identity()),
        role,
        Some(tree),
        PublicationHooks {
            before_link: || Ok(()),
            after_link: || Ok(()),
            after_record: || Ok(()),
            before_effect: || publication_signature(signature, pending),
            before_confirmation: || publication_signature(signature, pending),
        },
    )
}

fn publication_signature(
    signature: &super::package_trust::VerifiedSignature,
    pending: &PackagePending,
) -> Result<(), CustomerError> {
    if signature.manifest_sha256() != pending.new_hash || signature.revalidate().is_none() {
        return Err(recovery_refusal(
            "publication trust changed; retain evidence",
        ));
    }
    Ok(())
}

fn revalidate_tree(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    tree: Option<&super::package_tree::OwnedTree>,
) -> Result<(), CustomerError> {
    if let Some(tree) = tree {
        super::package_tree::revalidate_owned_tree(guard, pending, bundle, tree, false)?;
    }
    Ok(())
}

struct PublicationHooks<BeforeLink, AfterLink, AfterRecord, BeforeEffect, BeforeConfirmation> {
    before_link: BeforeLink,
    after_link: AfterLink,
    after_record: AfterRecord,
    before_effect: BeforeEffect,
    before_confirmation: BeforeConfirmation,
}

fn publish_impl(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    root_request: (&Path, Identity),
    role: &str,
    tree: Option<&super::package_tree::OwnedTree>,
    hooks: PublicationHooks<
        impl FnOnce() -> Result<(), CustomerError>,
        impl FnOnce() -> Result<(), CustomerError>,
        impl FnOnce() -> Result<(), CustomerError>,
        impl FnOnce() -> Result<(), CustomerError>,
        impl FnOnce() -> Result<(), CustomerError>,
    >,
) -> Result<(), CustomerError> {
    let (root, root_identity) = root_request;
    let PublicationHooks {
        before_link,
        after_link,
        after_record,
        before_effect,
        before_confirmation,
    } = hooks;
    revalidate_tree(guard, pending, bundle, tree)?;
    let mut prepared = prepare_member(guard, pending, bundle, root, root_identity, role)?;
    if read_history(guard.paths())? != prepared.history {
        return Err(recovery_refusal(
            "publication history changed; retain evidence",
        ));
    }
    let mut journal = if prepared.history.is_some() {
        StageSlot::open_existing(guard.paths().transactions_root(), JOURNAL, None)
            .ok_or_else(|| recovery_refusal("publication journal identity unavailable"))?
    } else {
        StageSlot::create(guard.paths().transactions_root(), JOURNAL)
            .map_err(|_| recovery_refusal("cannot create publication journal; retain evidence"))?
    };
    persist(guard, &mut journal, &prepared.record)?;
    verify_custody(guard, pending, bundle)?;
    verify_recorded_stage(&prepared.record, &verify_inventory(guard, pending, bundle)?)?;
    directory(root, root_identity)?;
    directory(&prepared.parent_path, prepared.record.parent_identity)?;
    journal
        .revalidate()
        .ok_or_else(|| recovery_refusal("publication journal changed"))?;
    before_link()?;
    revalidate_tree(guard, pending, bundle, tree)?;
    before_effect()?;
    linkat(
        &prepared.file,
        "",
        &prepared.parent,
        &prepared.leaf,
        AtFlags::EMPTY_PATH,
    )
    .map_err(|_| recovery_refusal("exclusive publication failed; retain journal"))?;
    after_link().map_err(|_| recovery_refusal("publication interrupted; retain evidence"))?;
    finish_publication(
        guard,
        bundle,
        &mut journal,
        &mut prepared,
        tree,
        after_record,
        before_confirmation,
    )
}

struct PreparedMember {
    record: Record,
    history: Option<Vec<Record>>,
    file: File,
    parent: File,
    parent_path: PathBuf,
    leaf: String,
}

fn prepare_member(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    root: &Path,
    root_identity: Identity,
    role: &str,
) -> Result<PreparedMember, CustomerError> {
    verify_custody(guard, pending, bundle)?;
    let history = read_history(guard.paths())?;
    let mut inventory = verify_inventory(guard, pending, bundle)?;
    let (destination, mode, digest) = bundle
        .publication_member(role)
        .ok_or_else(|| recovery_refusal("publication member is not declared"))?;
    if let Some(entries) = &history {
        let last = entries
            .last()
            .ok_or_else(|| recovery_refusal("publication history is empty"))?;
        if last.phase != Phase::Published
            || last.pending != *pending
            || last.root != root
            || last.root_identity != root_identity
            || entries
                .iter()
                .any(|item| item.role == role || item.destination == destination)
        {
            return Err(recovery_refusal(
                "publication history requires reconciliation or repeats a member",
            ));
        }
        for entry in entries
            .iter()
            .filter(|entry| entry.phase == Phase::Published)
        {
            verify_recorded_stage(entry, &inventory)?;
            drop(open_published_record(entry, bundle)?);
        }
        verify_custody(guard, pending, bundle)?;
        if read_history(guard.paths())?.as_ref() != Some(entries) {
            return Err(recovery_refusal(
                "publication history changed; retain evidence",
            ));
        }
    }
    if !safe_publication_mode(mode) {
        return Err(recovery_refusal("publication mode is unsafe or unreadable"));
    }
    directory(root, root_identity)?;
    let (parent_path, leaf) = target(root, &destination)?;
    let parent = open_directory_no_follow(&parent_path, false)?
        .ok_or_else(|| recovery_refusal("publication parent is absent"))?;
    validate_directory(&parent, "publication parent")?;
    let parent_identity = current_identity(&parent)?;
    match statat(&parent, &leaf, AtFlags::SYMLINK_NOFOLLOW) {
        Err(error) if error == rustix::io::Errno::NOENT => (),
        _ => {
            return Err(recovery_refusal(
                "publication destination exists or is unavailable",
            ));
        }
    }
    let slot = inventory
        .get_mut(role)
        .ok_or_else(|| recovery_refusal("publication stage is absent"))?;
    let creation_nonce = ramshared_ipc::random_nonce()
        .map_err(|_| recovery_refusal("publication creation randomness unavailable"))?;
    let (file, size) = fill_anonymous(&parent, slot, &digest, mode, &creation_nonce)?;
    let stage_identity = slot
        .identity()
        .ok_or_else(|| recovery_refusal("publication stage identity unavailable"))?;
    let stage_parent_identity = slot
        .parent_identity()
        .ok_or_else(|| recovery_refusal("publication stage parent identity unavailable"))?;
    verify_custody(guard, pending, bundle)?;
    directory(root, root_identity)?;
    directory(&parent_path, parent_identity)?;
    let record = Record {
        pending: pending.clone(),
        role: role.into(),
        root: root.into(),
        root_identity,
        destination,
        parent_identity,
        file_identity: current_identity(&file)?,
        stage_identity: Some(stage_identity),
        stage_parent_identity: Some(stage_parent_identity),
        creation_nonce,
        digest,
        size,
        mode,
        phase: Phase::Prepared,
    };
    let encoded = serde_json::to_vec(&record)
        .map_err(|_| recovery_refusal("cannot encode publication intent"))?;
    let used = read_journal_body(
        guard.paths(),
        OsStr::new(JOURNAL),
        "package publication journal",
    )?
    .map_or(0, |body| body.len() as u64);
    let mut confirmation = record.clone();
    confirmation.phase = Phase::Published;
    let confirmed = serde_json::to_vec(&confirmation)
        .map_err(|_| recovery_refusal("cannot encode publication confirmation"))?;
    let added = (encoded.len() as u64)
        .checked_add(confirmed.len() as u64)
        .and_then(|v| v.checked_add(2));
    if added
        .and_then(|v| used.checked_add(v))
        .is_none_or(|size| size > MAX_JOURNAL_BODY_BYTES)
    {
        return Err(recovery_refusal(
            "publication intent exceeds journal capacity",
        ));
    }
    if read_history(guard.paths())? != history {
        return Err(recovery_refusal(
            "publication history changed during preparation",
        ));
    }
    Ok(PreparedMember {
        record,
        history,
        file,
        parent,
        parent_path,
        leaf,
    })
}

fn finish_publication(
    guard: &CustomerTransitionGuard,
    bundle: &DeclaredBundle,
    journal: &mut StageSlot,
    prepared: &mut PreparedMember,
    tree: Option<&super::package_tree::OwnedTree>,
    after_record: impl FnOnce() -> Result<(), CustomerError>,
    before_confirmation: impl FnOnce() -> Result<(), CustomerError>,
) -> Result<(), CustomerError> {
    let PreparedMember {
        record,
        file,
        parent,
        parent_path,
        leaf,
        ..
    } = prepared;
    verify_custody(guard, &record.pending, bundle)?;
    verify_recorded_stage(record, &verify_inventory(guard, &record.pending, bundle)?)?;
    revalidate_tree(guard, &record.pending, bundle, tree)?;
    verify_published(record, file, parent, parent_path, leaf)?;
    file.sync_all()
        .and_then(|_| parent.sync_all())
        .map_err(|_| recovery_refusal("cannot sync published member"))?;
    revalidate_tree(guard, &record.pending, bundle, tree)?;
    before_confirmation()?;
    record.phase = Phase::Published;
    persist(guard, journal, record)?;
    after_record()
        .map_err(|_| recovery_refusal("publication confirmation interrupted; retain evidence"))?;
    verify_custody(guard, &record.pending, bundle)?;
    revalidate_tree(guard, &record.pending, bundle, tree)?;
    verify_published(record, file, parent, parent_path, leaf)
}

fn verify_creation(file: &File, expected: &[u8; 32]) -> Result<(), CustomerError> {
    let mut actual = [0u8; 32];
    let size = fgetxattr(file, CREATION_ATTRIBUTE, actual.as_mut_slice())
        .map_err(|_| recovery_refusal("publication creation identity unavailable"))?;
    if size != expected.len() || &actual != expected {
        return Err(recovery_refusal("publication creation identity changed"));
    }
    Ok(())
}

fn verify_published(
    record: &Record,
    file: &File,
    parent: &File,
    parent_path: &Path,
    leaf: &str,
) -> Result<(), CustomerError> {
    directory(&record.root, record.root_identity)?;
    directory(parent_path, record.parent_identity)?;
    let named = statat(parent, leaf, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|_| recovery_refusal("published member is absent"))?;
    if (named.st_dev, named.st_ino) != record.file_identity
        || current_identity(file)? != record.file_identity
    {
        return Err(recovery_refusal("published member identity changed"));
    }
    validate_regular(file, "published member")
        .map_err(|_| recovery_refusal("published member ownership or links changed"))?;
    verify_creation(file, &record.creation_nonce)?;
    let metadata = file
        .metadata()
        .map_err(|_| recovery_refusal("published member unavailable"))?;
    if metadata.len() != record.size || metadata.mode() & 0o7777 != record.mode {
        return Err(recovery_refusal("published member mode or size changed"));
    }
    let mut readback = file
        .try_clone()
        .map_err(|_| recovery_refusal("published member cannot be read"))?;
    readback
        .seek(SeekFrom::Start(0))
        .map_err(|_| recovery_refusal("published member cannot be read"))?;
    if crate::installed_image::sha256_reader_with_limit(&mut readback, record.size).as_deref()
        != Some(&record.digest)
    {
        return Err(recovery_refusal("published member digest differs"));
    }
    Ok(())
}

fn open_publication_target(parent: &File, leaf: &str, phase: Phase) -> Result<File, CustomerError> {
    openat(
        parent,
        leaf,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| match (phase, error) {
        (Phase::Prepared, rustix::io::Errno::NOENT) => recovery_refusal(
            "prepared publication target is absent; effect may have occurred; retain evidence for reviewed reconciliation",
        ),
        (Phase::Published, rustix::io::Errno::NOENT) => {
            recovery_refusal("published member is absent; retain evidence")
        }
        _ => recovery_refusal("publication target is unavailable; retain evidence"),
    })
}

fn open_published_record(record: &Record, bundle: &DeclaredBundle) -> Result<File, CustomerError> {
    let (destination, mode, digest) = bundle
        .publication_member(&record.role)
        .ok_or_else(|| recovery_refusal("publication member is not declared"))?;
    if (destination, mode, digest)
        != (
            record.destination.clone(),
            record.mode,
            record.digest.clone(),
        )
    {
        return Err(recovery_refusal("publication declaration changed"));
    }
    directory(&record.root, record.root_identity)?;
    let (parent_path, leaf) = target(&record.root, &record.destination)?;
    let parent = directory(&parent_path, record.parent_identity)?;
    let file = open_publication_target(&parent, &leaf, record.phase)?;
    verify_published(record, &file, &parent, &parent_path, &leaf)?;
    Ok(file)
}

/// Reconcile only a visible member matching the original durable intent.
#[cfg(test)]
pub(super) fn resume_publication(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
) -> Result<(), CustomerError> {
    resume_publication_inner(guard, pending, bundle, None, None)
}

pub(super) fn resume_owned_publication(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    tree: &super::package_tree::OwnedTree,
    signature: &super::package_trust::VerifiedSignature,
) -> Result<(), CustomerError> {
    super::package_selector::require_absent(guard.paths())?;
    resume_publication_inner(guard, pending, bundle, Some(tree), Some(signature))
}

fn resume_publication_inner(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    tree: Option<&super::package_tree::OwnedTree>,
    signature: Option<&super::package_trust::VerifiedSignature>,
) -> Result<(), CustomerError> {
    verify_custody(guard, pending, bundle)?;
    revalidate_tree(guard, pending, bundle, tree)?;
    let record = read_record(guard.paths())?
        .ok_or_else(|| recovery_refusal("publication journal is absent"))?;
    if record.pending != *pending
        || tree.is_some_and(|tree| {
            record.root != tree.path() || record.root_identity != tree.identity()
        })
    {
        return Err(recovery_refusal("publication transaction changed"));
    }
    let (destination, mode, digest) = bundle
        .publication_member(&record.role)
        .ok_or_else(|| recovery_refusal("publication member is not declared"))?;
    if (destination, mode, digest)
        != (
            record.destination.clone(),
            record.mode,
            record.digest.clone(),
        )
    {
        return Err(recovery_refusal("publication declaration changed"));
    }
    verify_recorded_stage(&record, &verify_inventory(guard, pending, bundle)?)?;
    directory(&record.root, record.root_identity)?;
    let (parent_path, leaf) = target(&record.root, &record.destination)?;
    let parent = directory(&parent_path, record.parent_identity)?;
    let file = open_publication_target(&parent, &leaf, record.phase)?;
    if record.phase == Phase::Published {
        return Err(recovery_refusal(
            "publication already recorded; full transaction reconciliation required",
        ));
    }
    let mut journal = StageSlot::open_existing(guard.paths().transactions_root(), JOURNAL, None)
        .ok_or_else(|| recovery_refusal("publication journal unavailable"))?;
    if read_record(guard.paths())?.as_ref() != Some(&record) {
        return Err(recovery_refusal("publication evidence changed"));
    }
    let mut prepared = PreparedMember {
        record,
        history: None,
        file,
        parent,
        parent_path,
        leaf,
    };
    finish_publication(
        guard,
        bundle,
        &mut journal,
        &mut prepared,
        tree,
        || Ok(()),
        || match signature {
            Some(signature) => publication_signature(signature, pending),
            #[cfg(test)]
            None => Ok(()),
            #[cfg(not(test))]
            None => Err(recovery_refusal("publication signature absent")),
        },
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn target_diagnosis_distinguishes_ambiguous_missing_and_unsafe_entries() {
        let root = std::env::temp_dir().join(format!(
            "ramshared-publication-diagnosis-{}",
            std::process::id()
        ));
        std::fs::create_dir(&root).unwrap();
        let parent = File::open(&root).unwrap();
        let absent = open_publication_target(&parent, "member", Phase::Prepared).unwrap_err();
        assert!(absent.detail.contains("effect may have occurred"));
        let absent = open_publication_target(&parent, "member", Phase::Published).unwrap_err();
        assert!(absent.detail.contains("published member is absent"));
        std::os::unix::fs::symlink("missing", root.join("member")).unwrap();
        let redirected = open_publication_target(&parent, "member", Phase::Prepared).unwrap_err();
        assert!(redirected.detail.contains("target is unavailable"));
        std::fs::remove_file(root.join("member")).unwrap();
        std::fs::write(root.join("member"), b"member").unwrap();
        assert!(open_publication_target(&parent, "member", Phase::Prepared).is_ok());
        std::fs::remove_file(root.join("member")).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn publication_journal_refuses_foreign_or_malformed_evidence() {
        let root = std::env::temp_dir().join(format!(
            "ramshared-publication-record-{}",
            std::process::id()
        ));
        let paths = TransitionPaths::under(&root);
        std::fs::create_dir_all(paths.transactions_root()).unwrap();
        let path = paths.transactions_root().join(JOURNAL);
        assert!(!publication_journal_present(&paths).unwrap());
        assert!(read_record(&paths).unwrap().is_none());
        let record = Record {
            pending: PackagePending {
                target_release: None,
                action: PackageAction::Upgrade,
                old_hash: "a".repeat(64),
                new_hash: "b".repeat(64),
                issued_unix: 1,
            },
            role: "ramshared".into(),
            root: root.join("release"),
            root_identity: (1, 2),
            destination: "bin/ramshared".into(),
            parent_identity: (1, 3),
            file_identity: (1, 4),
            stage_identity: Some((1, 5)),
            stage_parent_identity: Some((1, 6)),
            creation_nonce: [42; 32],
            digest: "c".repeat(64),
            size: 64,
            mode: 0o755,
            phase: Phase::Prepared,
        };
        let encode = |rows: &[Record]| {
            rows.iter()
                .map(|r| format!("{}\n", serde_json::to_string(r).unwrap()))
                .collect::<String>()
        };
        std::fs::write(&path, encode(std::slice::from_ref(&record))).unwrap();
        assert!(publication_journal_present(&paths).unwrap());
        assert_eq!(read_record(&paths).unwrap(), Some(record.clone()));
        let mut legacy = record.clone();
        legacy.stage_identity = None;
        legacy.stage_parent_identity = None;
        std::fs::write(&path, encode(std::slice::from_ref(&legacy))).unwrap();
        assert_eq!(read_record(&paths).unwrap(), Some(legacy.clone()));
        let slot = StageSlot::create(&root, "stage").unwrap();
        let mut bound = record.clone();
        bound.stage_identity = slot.identity();
        bound.stage_parent_identity = slot.parent_identity();
        let slots = std::collections::BTreeMap::from([(record.role.clone(), slot)]);
        assert!(verify_recorded_stage(&bound, &slots).is_ok());
        assert!(verify_recorded_stage(&legacy, &slots).is_err());
        bound.stage_parent_identity = Some((1, u64::MAX));
        assert!(verify_recorded_stage(&bound, &slots).is_err());
        let mut published = record.clone();
        published.phase = Phase::Published;
        std::fs::write(&path, encode(&[record.clone(), published.clone()])).unwrap();
        assert_eq!(read_record(&paths).unwrap(), Some(published.clone()));
        let mut second = record.clone();
        second.role = "ramsharedd".into();
        second.destination = "bin/ramsharedd".into();
        second.file_identity.1 = 5;
        std::fs::write(
            &path,
            encode(&[record.clone(), published.clone(), second.clone()]),
        )
        .unwrap();
        assert_eq!(read_record(&paths).unwrap(), Some(second.clone()));
        let mut repeated_role = second.clone();
        repeated_role.role = record.role.clone();
        let mut repeated_destination = second.clone();
        repeated_destination.destination = record.destination.clone();
        let mut different_root = second.clone();
        different_root.root_identity.1 += 1;
        let mut different_pending = second.clone();
        different_pending.pending.issued_unix += 1;
        let mut foreign = published.clone();
        foreign.file_identity.1 += 1;
        let mut invalid = record.clone();
        invalid.destination = "../escape".into();
        let mut unsafe_mode = record.clone();
        unsafe_mode.mode = 0o777;
        let mut setuid_mode = record.clone();
        setuid_mode.mode = 0o4755;
        let mut out_of_range_mode = record.clone();
        out_of_range_mode.mode = 0o100755;
        let mut empty_nonce = record.clone();
        empty_nonce.creation_nonce = [0; 32];
        let mut incomplete_stage = record.clone();
        incomplete_stage.stage_identity = None;
        for body in [
            String::new(),
            "{bad}\n".into(),
            "{}".into(),
            encode(&[published.clone()]),
            encode(&[record.clone(), foreign]),
            encode(&[record.clone(), published.clone(), published.clone()]),
            encode(&[record.clone(), second.clone()]),
            encode(&[record.clone(), published.clone(), repeated_role]),
            encode(&[record.clone(), published.clone(), repeated_destination]),
            encode(&[record.clone(), published.clone(), different_root]),
            encode(&[record.clone(), published, different_pending]),
            encode(&[invalid]),
            encode(&[unsafe_mode]),
            encode(&[setuid_mode]),
            encode(&[out_of_range_mode]),
            encode(&[empty_nonce]),
            encode(&[incomplete_stage]),
        ] {
            std::fs::write(&path, body).unwrap();
            assert!(read_record(&paths).is_err());
            assert!(publication_journal_present(&paths).unwrap());
        }
        assert!(target(Path::new("/tmp"), "../outside").is_err());
        assert!(target(Path::new("/tmp"), "/absolute").is_err());
        assert!(target(Path::new("/tmp"), "").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
