// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_tree.rs
//! Durable ownership of an exclusive private release directory tree.

pub(super) mod rollback;
mod space;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{
    AtFlags, Dir, Mode, OFlags, XattrFlags, fgetxattr, fsetxattr, mkdirat, openat, statat,
};
use serde::{Deserialize, Serialize};

use super::CustomerError;
use super::package_source::DeclaredBundle;
use super::package_stage::StageSlot;
use super::package_stage_journal::inventory::verify_staged_inventory;
use super::transition::{
    CustomerTransitionGuard, OperationOwner, PackageAction, PackagePending, TransitionPaths,
    read_journal_body, read_pending, recovery_refusal,
};
use super::transition_custody::{
    MAX_JOURNAL_BODY_BYTES, open_directory_no_follow, validate_directory,
};

const JOURNAL: &str = "package-tree.json";
const CREATION_ATTRIBUTE: &str = "user.ramshared.tree";
type Identity = (u64, u64);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Intent,
    Ready,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Record {
    pending: PackagePending,
    parent: PathBuf,
    parent_identity: Identity,
    root_name: String,
    relative: PathBuf,
    directory_parent_identity: Identity,
    mount_id: u64,
    nonce: [u8; 32],
    identity: Option<Identity>,
    phase: Phase,
}

/// Identity established by the durable directory journal, not by a name alone.
pub(super) struct OwnedTree {
    root: PathBuf,
    identity: Identity,
}

impl OwnedTree {
    pub(super) fn path(&self) -> &Path {
        &self.root
    }
    pub(super) fn identity(&self) -> Identity {
        self.identity
    }
}

pub(super) fn tree_journal_present(paths: &TransitionPaths) -> Result<bool, CustomerError> {
    read_journal_body(paths, OsStr::new(JOURNAL), "package tree journal")
        .map(|body| body.is_some())
        .map_err(|_| recovery_refusal("package tree journal unavailable; retain evidence"))
}

/// Remove the tree journal only under `OperationOwner::Package` for a matching transaction.
pub(super) fn clear_tree_journal(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
) -> Result<(), CustomerError> {
    if guard.owner() != OperationOwner::Package {
        return Err(recovery_refusal(
            "clearing tree journal requires package transition owner",
        ));
    }
    guard.revalidate()?;
    if read_pending(guard.paths())?.as_ref() != Some(pending) {
        return Err(recovery_refusal(
            "pending record does not match tree journal clear target",
        ));
    }
    if let Some(history) = read_history(guard.paths())?
        && history.iter().any(|entry| &entry.pending != pending)
    {
        return Err(recovery_refusal(
            "tree journal belongs to another transaction; retain evidence",
        ));
    }
    super::transition_custody::clear_journal_file(
        guard,
        OsStr::new(JOURNAL),
        "package tree journal",
    )
}

/// Canonical validated evidence for the read-only reconciliation observation.
pub(super) fn reconciliation_evidence(paths: &TransitionPaths) -> Result<Vec<u8>, CustomerError> {
    let history =
        read_history(paths)?.ok_or_else(|| recovery_refusal("package tree history is absent"))?;
    serde_json::to_vec(&history)
        .map_err(|_| recovery_refusal("cannot encode package tree evidence"))
}

fn valid_relative(path: &Path) -> bool {
    path.components()
        .all(|part| matches!(part, Component::Normal(_)))
}

fn validate(record: &Record) -> Result<(), CustomerError> {
    if !record.parent.is_absolute()
        || record
            .parent
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::Prefix(_)))
        || !super::package::is_role(&record.root_name)
        || record.root_name.contains('/')
        || !valid_relative(&record.relative)
        || record.parent_identity.1 == 0
        || record.directory_parent_identity.1 == 0
        || record.mount_id == 0
        || record.nonce.iter().all(|byte| *byte == 0)
        || !super::package::is_lowercase_sha256(&record.pending.old_hash)
        || !super::package::is_lowercase_sha256(&record.pending.new_hash)
        || record.pending.action == PackageAction::Remove
        || match record.phase {
            Phase::Intent => record.identity.is_some(),
            Phase::Ready => record.identity.is_none_or(|id| id.1 == 0),
        }
    {
        return Err(recovery_refusal("package tree journal identity is invalid"));
    }
    Ok(())
}

fn read_history(paths: &TransitionPaths) -> Result<Option<Vec<Record>>, CustomerError> {
    let Some(body) = read_journal_body(paths, OsStr::new(JOURNAL), "package tree journal")? else {
        return Ok(None);
    };
    if !body.ends_with('\n') {
        return Err(recovery_refusal("package tree journal is incomplete"));
    }
    let mut entries: Vec<Record> = Vec::new();
    let mut names = BTreeSet::new();
    for line in body.lines() {
        let entry: Record = serde_json::from_str(line)
            .map_err(|_| recovery_refusal("package tree journal is malformed"))?;
        validate(&entry)?;
        if let Some(prior) = entries.last() {
            if prior.pending != entry.pending
                || prior.parent != entry.parent
                || prior.parent_identity != entry.parent_identity
                || prior.root_name != entry.root_name
                || prior.mount_id != entry.mount_id
            {
                return Err(recovery_refusal("package tree transaction changed"));
            }
            match (prior.phase, entry.phase) {
                (Phase::Intent, Phase::Ready) => {
                    let mut expected = prior.clone();
                    expected.phase = Phase::Ready;
                    expected.identity = entry.identity;
                    if expected != entry {
                        return Err(recovery_refusal("package tree confirmation changed"));
                    }
                }
                (Phase::Ready, Phase::Intent) => (),
                _ => return Err(recovery_refusal("package tree journal sequence changed")),
            }
        } else if entry.phase != Phase::Intent || !entry.relative.as_os_str().is_empty() {
            return Err(recovery_refusal("package tree root intent is absent"));
        }
        if entry.phase == Phase::Intent && !names.insert(entry.relative.clone()) {
            return Err(recovery_refusal("package tree journal repeats a directory"));
        }
        entries.push(entry);
    }
    if entries.is_empty() {
        return Err(recovery_refusal("package tree journal is empty"));
    }
    Ok(Some(entries))
}

fn persist(
    guard: &CustomerTransitionGuard,
    journal: &mut StageSlot,
    record: &Record,
) -> Result<(), CustomerError> {
    let bytes =
        serde_json::to_vec(record).map_err(|_| recovery_refusal("cannot encode tree journal"))?;
    guard.revalidate()?;
    journal
        .revalidate()
        .ok_or_else(|| recovery_refusal("tree journal identity changed"))?;
    let file = journal.file_mut();
    let used = file
        .metadata()
        .map_err(|_| recovery_refusal("tree journal unavailable"))?
        .len();
    if used
        .checked_add(bytes.len() as u64)
        .and_then(|n| n.checked_add(1))
        .is_none_or(|n| n > MAX_JOURNAL_BODY_BYTES)
    {
        return Err(recovery_refusal("tree journal exceeds its limit"));
    }
    file.seek(SeekFrom::End(0))
        .and_then(|_| file.write_all(&bytes))
        .and_then(|_| file.write_all(b"\n"))
        .and_then(|_| file.sync_all())
        .map_err(|_| recovery_refusal("cannot persist tree journal; retain evidence"))?;
    journal
        .revalidate()
        .ok_or_else(|| recovery_refusal("tree journal identity changed"))?;
    journal
        .sync_directory()
        .ok_or_else(|| recovery_refusal("cannot sync tree journal directory"))?;
    guard.revalidate()?;
    if read_history(guard.paths())?
        .and_then(|history| history.last().cloned())
        .as_ref()
        != Some(record)
    {
        return Err(recovery_refusal("tree journal readback changed"));
    }
    Ok(())
}

fn identity(file: &File) -> Result<Identity, CustomerError> {
    let meta = file
        .metadata()
        .map_err(|_| recovery_refusal("tree directory identity unavailable"))?;
    Ok((meta.dev(), meta.ino()))
}

fn open_directory(path: &Path, expected: Identity, mount: u64) -> Result<File, CustomerError> {
    let file = open_directory_no_follow(path, false)?
        .ok_or_else(|| recovery_refusal("tree directory is absent"))?;
    validate_directory(&file, "package tree directory")?;
    if identity(&file)? != expected || super::package_publish::inventory::mount_id(&file)? != mount
    {
        return Err(recovery_refusal("tree directory identity or mount changed"));
    }
    Ok(file)
}

fn stage_ready(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
) -> Result<(), CustomerError> {
    if guard.owner() != OperationOwner::Package
        || pending.action == PackageAction::Remove
        || bundle.manifest_sha256() != pending.new_hash
        || read_pending(guard.paths())?.as_ref() != Some(pending)
    {
        return Err(recovery_refusal("tree transaction identity does not match"));
    }
    guard.revalidate()?;
    let mut inventory = verify_staged_inventory(guard, pending, bundle)?;
    bundle
        .verify_staged_semantics(&mut inventory)
        .ok_or_else(|| recovery_refusal("staged package identity is invalid"))?;
    Ok(())
}

fn plan(bundle: &DeclaredBundle) -> Result<Vec<PathBuf>, CustomerError> {
    let roles = bundle
        .stage_roles()
        .ok_or_else(|| recovery_refusal("tree inventory disagrees"))?;
    let mut dirs = BTreeSet::new();
    for role in roles {
        let (destination, _, _) = bundle
            .publication_member(&role)
            .ok_or_else(|| recovery_refusal("tree member is unavailable"))?;
        let mut parent = Path::new(&destination).parent();
        while let Some(path) = parent {
            if path.as_os_str().is_empty() {
                break;
            }
            if !valid_relative(path) {
                return Err(recovery_refusal("tree destination is unsafe"));
            }
            dirs.insert(path.to_path_buf());
            parent = path.parent();
        }
    }
    let mut result = vec![PathBuf::new()];
    result.extend(dirs);
    Ok(result)
}

fn journal_capacity(
    pending: &PackagePending,
    parent: &Path,
    parent_identity: Identity,
    root_name: &str,
    dirs: &[PathBuf],
) -> Option<u64> {
    let mut total = 0u64;
    for relative in dirs {
        let mut record = Record {
            pending: pending.clone(),
            parent: parent.into(),
            parent_identity,
            root_name: root_name.into(),
            relative: relative.clone(),
            directory_parent_identity: (u64::MAX, u64::MAX),
            mount_id: u64::MAX,
            nonce: [u8::MAX; 32],
            identity: None,
            phase: Phase::Intent,
        };
        for phase in [Phase::Intent, Phase::Ready] {
            record.phase = phase;
            if phase == Phase::Ready {
                record.identity = Some((u64::MAX, u64::MAX));
            }
            total = total
                .checked_add(u64::try_from(serde_json::to_vec(&record).ok()?.len()).ok()?)?
                .checked_add(1)?;
        }
    }
    Some(total)
}

fn verify_nonce(file: &File, expected: &[u8; 32]) -> Result<(), CustomerError> {
    let mut actual = [0u8; 32];
    let size = fgetxattr(file, CREATION_ATTRIBUTE, actual.as_mut_slice())
        .map_err(|_| recovery_refusal("tree creation identity is unavailable"))?;
    if size != expected.len() || &actual != expected {
        return Err(recovery_refusal("tree creation identity changed"));
    }
    Ok(())
}

fn check_empty(file: &File) -> Result<(), CustomerError> {
    let mut entries =
        Dir::read_from(file).map_err(|_| recovery_refusal("tree directory unreadable"))?;
    while let Some(entry) = entries.read() {
        let entry = entry.map_err(|_| recovery_refusal("tree directory unreadable"))?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            return Err(recovery_refusal("tree directory is not empty"));
        }
    }
    Ok(())
}

fn record_path(record: &Record) -> PathBuf {
    record.parent.join(&record.root_name).join(&record.relative)
}

fn verify_ready(record: &Record) -> Result<File, CustomerError> {
    let expected = record
        .identity
        .ok_or_else(|| recovery_refusal("tree directory identity absent"))?;
    let file = open_directory(&record_path(record), expected, record.mount_id)?;
    if file
        .metadata()
        .map_err(|_| recovery_refusal("tree directory unavailable"))?
        .mode()
        & 0o7777
        != 0o700
    {
        return Err(recovery_refusal("tree directory mode changed"));
    }
    verify_nonce(&file, &record.nonce)?;
    Ok(file)
}

fn parent_for(
    record: &Record,
    completed: &BTreeMap<PathBuf, Record>,
) -> Result<File, CustomerError> {
    let anchor = open_directory(&record.parent, record.parent_identity, record.mount_id)?;
    if record.relative.as_os_str().is_empty() {
        if record.directory_parent_identity != record.parent_identity {
            return Err(recovery_refusal("tree root parent identity differs"));
        }
        return Ok(anchor);
    }
    let relative_parent = record.relative.parent().unwrap_or(Path::new(""));
    let prior = completed
        .get(relative_parent)
        .ok_or_else(|| recovery_refusal("tree parent has no durable identity"))?;
    if prior.identity != Some(record.directory_parent_identity) {
        return Err(recovery_refusal("tree parent identity differs"));
    }
    verify_ready(prior)
}

fn create_or_resume(
    record: &Record,
    completed: &BTreeMap<PathBuf, Record>,
) -> Result<Identity, CustomerError> {
    let parent = parent_for(record, completed)?;
    let leaf = if record.relative.as_os_str().is_empty() {
        OsStr::new(&record.root_name)
    } else {
        record
            .relative
            .file_name()
            .ok_or_else(|| recovery_refusal("tree name is invalid"))?
    };
    let created = match mkdirat(&parent, leaf, Mode::RWXU) {
        Ok(()) => true,
        Err(error) if error == rustix::io::Errno::EXIST => false,
        Err(_) => {
            return Err(recovery_refusal(
                "tree directory creation failed; retain evidence",
            ));
        }
    };
    let directory = File::from(
        openat(
            &parent,
            leaf,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| recovery_refusal("tree directory is unavailable; retain evidence"))?,
    );
    validate_directory(&directory, "package tree directory")?;
    if super::package_publish::inventory::mount_id(&directory)? != record.mount_id
        || directory
            .metadata()
            .map_err(|_| recovery_refusal("tree directory unavailable"))?
            .mode()
            & 0o7777
            != 0o700
    {
        return Err(recovery_refusal("tree directory mount or mode differs"));
    }
    check_empty(&directory)?;
    if created {
        fsetxattr(
            &directory,
            CREATION_ATTRIBUTE,
            &record.nonce,
            XattrFlags::CREATE,
        )
        .map_err(|_| recovery_refusal("cannot seal tree directory; retain evidence"))?;
    }
    verify_nonce(&directory, &record.nonce)?;
    directory
        .sync_all()
        .and_then(|_| parent.sync_all())
        .map_err(|_| recovery_refusal("cannot sync tree directory; retain evidence"))?;
    let id = identity(&directory)?;
    let named = statat(&parent, leaf, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|_| recovery_refusal("tree name unavailable"))?;
    if (named.st_dev, named.st_ino) != id {
        return Err(recovery_refusal("tree name changed; retain evidence"));
    }
    Ok(id)
}

fn verify_prefix(
    history: &[Record],
    dirs: &[PathBuf],
    pending: &PackagePending,
    parent: &Path,
    anchor: Identity,
    name: &str,
    mount: u64,
) -> Result<(usize, BTreeMap<PathBuf, Record>), CustomerError> {
    let mut count = 0;
    let mut completed = BTreeMap::new();
    for record in history {
        if record.pending != *pending
            || record.parent != parent
            || record.parent_identity != anchor
            || record.root_name != name
            || record.mount_id != mount
        {
            return Err(recovery_refusal("tree history does not match request"));
        }
        match record.phase {
            Phase::Intent => {
                if dirs.get(count) != Some(&record.relative) {
                    return Err(recovery_refusal("tree history differs from plan"));
                }
                count += 1;
            }
            Phase::Ready => {
                parent_for(record, &completed)?;
                verify_ready(record)?;
                completed.insert(record.relative.clone(), record.clone());
            }
        }
    }
    Ok((count, completed))
}

/// Create an exclusive tree after a full bounded intent preflight.
pub(super) fn create_tree(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    parent: &Path,
    parent_identity: Identity,
    name: &str,
    reserve_bytes: u64,
) -> Result<OwnedTree, CustomerError> {
    super::package_selector::require_absent(guard.paths())?;
    continue_tree(
        guard,
        pending,
        bundle,
        (parent, parent_identity, reserve_bytes),
        name,
        false,
        &mut |_| Ok(()),
    )
}

/// Resume recorded creation; ambiguous existing directories remain untouched.
pub(super) fn resume_tree(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    parent: &Path,
    parent_identity: Identity,
    name: &str,
    reserve_bytes: u64,
) -> Result<OwnedTree, CustomerError> {
    super::package_selector::require_absent(guard.paths())?;
    continue_tree(
        guard,
        pending,
        bundle,
        (parent, parent_identity, reserve_bytes),
        name,
        true,
        &mut |_| Ok(()),
    )
}

fn check_intent(
    guard: &CustomerTransitionGuard,
    journal: &StageSlot,
    record: &Record,
) -> Result<(), CustomerError> {
    guard.revalidate()?;
    journal
        .revalidate()
        .ok_or_else(|| recovery_refusal("tree journal identity changed"))?;
    if read_pending(guard.paths())?.as_ref() != Some(&record.pending)
        || super::package_publish::publication_journal_present(guard.paths())?
        || read_history(guard.paths())?
            .and_then(|history| history.last().cloned())
            .as_ref()
            != Some(record)
    {
        return Err(recovery_refusal("tree intent or transaction changed"));
    }
    Ok(())
}

pub(super) fn continue_tree(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    anchor_request: (&Path, Identity, u64),
    name: &str,
    resume: bool,
    after_seal: &mut dyn FnMut(&Path) -> Result<(), CustomerError>,
) -> Result<OwnedTree, CustomerError> {
    let (parent, parent_identity, reserve_bytes) = anchor_request;
    stage_ready(guard, pending, bundle)?;
    if super::package_publish::publication_journal_present(guard.paths())? {
        return Err(recovery_refusal(
            "publication requires reconciliation before tree mutation",
        ));
    }
    if !parent.is_absolute() || !super::package::is_role(name) || name.contains('/') {
        return Err(recovery_refusal("tree root request is invalid"));
    }
    let anchor = open_directory_no_follow(parent, false)?
        .ok_or_else(|| recovery_refusal("tree parent is absent"))?;
    validate_directory(&anchor, "tree parent")?;
    if identity(&anchor)? != parent_identity {
        return Err(recovery_refusal("tree parent identity changed"));
    }
    let mount = super::package_publish::inventory::mount_id(&anchor)?;
    let dirs = plan(bundle)?;
    let journal_bytes = journal_capacity(pending, parent, parent_identity, name, &dirs)
        .filter(|size| *size <= MAX_JOURNAL_BODY_BYTES)
        .ok_or_else(|| recovery_refusal("tree plan exceeds journal capacity"))?;
    let history = read_history(guard.paths())?;
    if resume != history.is_some() {
        return Err(recovery_refusal(
            "tree history is not in the expected state",
        ));
    }
    let (started, mut completed) = match &history {
        Some(entries) => verify_prefix(
            entries,
            &dirs,
            pending,
            parent,
            parent_identity,
            name,
            mount,
        )?,
        None => (0, BTreeMap::new()),
    };
    let journal_directory = open_directory_no_follow(guard.paths().transactions_root(), false)?
        .ok_or_else(|| recovery_refusal("tree journal directory is absent"))?;
    validate_directory(&journal_directory, "tree journal directory")?;
    space::check_space(
        &anchor,
        &journal_directory,
        dirs.len().saturating_sub(completed.len()),
        journal_bytes,
        reserve_bytes,
        history.is_none(),
    )?;
    if !resume {
        match statat(&anchor, name, AtFlags::SYMLINK_NOFOLLOW) {
            Err(error) if error == rustix::io::Errno::NOENT => (),
            _ => {
                return Err(recovery_refusal(
                    "tree root already exists or is unavailable",
                ));
            }
        }
    }
    guard.revalidate()?;
    if read_pending(guard.paths())?.as_ref() != Some(pending)
        || read_history(guard.paths())? != history
    {
        return Err(recovery_refusal("tree evidence changed during preflight"));
    }
    let mut journal = if history.is_some() {
        StageSlot::open_existing(guard.paths().transactions_root(), JOURNAL, None)
            .ok_or_else(|| recovery_refusal("tree journal identity unavailable"))?
    } else {
        StageSlot::create(guard.paths().transactions_root(), JOURNAL)
            .map_err(|_| recovery_refusal("cannot create tree journal; retain evidence"))?
    };
    if let Some(last) = history
        .as_ref()
        .and_then(|entries| entries.last())
        .filter(|entry| entry.phase == Phase::Intent)
    {
        let mut record = last.clone();
        check_intent(guard, &journal, &record)?;
        let id = create_or_resume(&record, &completed)?;
        after_seal(&record.relative)
            .map_err(|_| recovery_refusal("tree creation interrupted; retain evidence"))?;
        check_intent(guard, &journal, &record)?;
        record.identity = Some(id);
        record.phase = Phase::Ready;
        persist(guard, &mut journal, &record)?;
        completed.insert(record.relative.clone(), record);
    }
    for relative in &dirs[started..] {
        let parent_id = if relative.as_os_str().is_empty() {
            parent_identity
        } else {
            completed
                .get(relative.parent().unwrap_or(Path::new("")))
                .and_then(|entry| entry.identity)
                .ok_or_else(|| recovery_refusal("tree parent has no durable identity"))?
        };
        let nonce = ramshared_ipc::random_nonce()
            .map_err(|_| recovery_refusal("tree creation randomness unavailable"))?;
        let mut record = Record {
            pending: pending.clone(),
            parent: parent.into(),
            parent_identity,
            root_name: name.into(),
            relative: relative.clone(),
            directory_parent_identity: parent_id,
            mount_id: mount,
            nonce,
            identity: None,
            phase: Phase::Intent,
        };
        persist(guard, &mut journal, &record)?;
        check_intent(guard, &journal, &record)?;
        let id = create_or_resume(&record, &completed)?;
        after_seal(relative)
            .map_err(|_| recovery_refusal("tree creation interrupted; retain evidence"))?;
        check_intent(guard, &journal, &record)?;
        record.identity = Some(id);
        record.phase = Phase::Ready;
        persist(guard, &mut journal, &record)?;
        completed.insert(relative.clone(), record);
    }
    verify_tree(guard, pending, bundle, parent, parent_identity, name)
}

// Before publication, the tree must contain exactly the directories in the
// journal. Walk via opened descriptors and reject every unexpected entry.
fn verify_exact_tree(
    root: &File,
    completed: &BTreeMap<PathBuf, Record>,
) -> Result<(), CustomerError> {
    let mut stack = vec![(
        root.try_clone()
            .map_err(|_| recovery_refusal("tree root unavailable"))?,
        PathBuf::new(),
    )];
    let mut seen = BTreeSet::new();
    while let Some((directory, relative)) = stack.pop() {
        let mut entries = Dir::read_from(&directory)
            .map_err(|_| recovery_refusal("tree directory unreadable"))?;
        while let Some(entry) = entries.read() {
            let entry = entry.map_err(|_| recovery_refusal("tree directory unreadable"))?;
            let name = entry.file_name();
            if name.to_bytes() == b"." || name.to_bytes() == b".." {
                continue;
            }
            let child_path = relative.join(OsStr::from_bytes(name.to_bytes()));
            let record = completed
                .get(&child_path)
                .ok_or_else(|| recovery_refusal("tree contains an unexpected entry"))?;
            if !seen.insert(child_path.clone()) {
                return Err(recovery_refusal("tree entry repeated"));
            }
            let child = File::from(
                openat(
                    &directory,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|_| recovery_refusal("tree entry is not a directory"))?,
            );
            validate_directory(&child, "package tree directory")?;
            if Some(identity(&child)?) != record.identity
                || super::package_publish::inventory::mount_id(&child)? != record.mount_id
                || child
                    .metadata()
                    .map_err(|_| recovery_refusal("tree directory unavailable"))?
                    .mode()
                    & 0o7777
                    != 0o700
            {
                return Err(recovery_refusal("tree entry identity changed"));
            }
            verify_nonce(&child, &record.nonce)?;
            stack.push((child, child_path));
        }
    }
    if seen.len() != completed.len().saturating_sub(1) {
        return Err(recovery_refusal("tree directory is missing"));
    }
    Ok(())
}

/// Revalidate every recorded directory and return the root identity.
pub(super) fn verify_tree(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    parent: &Path,
    parent_identity: Identity,
    name: &str,
) -> Result<OwnedTree, CustomerError> {
    verify_tree_inner(guard, pending, bundle, parent, parent_identity, name, true)
}

/// Reopen a tree for publication recovery; publication inventory remains
/// responsible for authenticating the files already present in the tree.
pub(super) fn reopen_owned_tree(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    parent: &Path,
    parent_identity: Identity,
    name: &str,
) -> Result<OwnedTree, CustomerError> {
    if !super::package_publish::publication_journal_present(guard.paths())? {
        return Err(recovery_refusal("publication history is absent"));
    }
    verify_tree_inner(guard, pending, bundle, parent, parent_identity, name, false)
}

fn verify_tree_inner(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    parent: &Path,
    parent_identity: Identity,
    name: &str,
    empty: bool,
) -> Result<OwnedTree, CustomerError> {
    stage_ready(guard, pending, bundle)?;
    let anchor = open_directory_no_follow(parent, false)?
        .ok_or_else(|| recovery_refusal("tree parent is absent"))?;
    validate_directory(&anchor, "tree parent")?;
    if identity(&anchor)? != parent_identity {
        return Err(recovery_refusal("tree parent identity changed"));
    }
    let mount = super::package_publish::inventory::mount_id(&anchor)?;
    let dirs = plan(bundle)?;
    let history =
        read_history(guard.paths())?.ok_or_else(|| recovery_refusal("tree history is absent"))?;
    let (started, completed) = verify_prefix(
        &history,
        &dirs,
        pending,
        parent,
        parent_identity,
        name,
        mount,
    )?;
    if started != dirs.len()
        || completed.len() != dirs.len()
        || history
            .last()
            .is_none_or(|entry| entry.phase != Phase::Ready)
    {
        return Err(recovery_refusal("tree creation is incomplete"));
    }
    let root_record = completed
        .get(Path::new(""))
        .ok_or_else(|| recovery_refusal("tree root identity absent"))?;
    let root = verify_ready(root_record)?;
    if empty {
        verify_exact_tree(&root, &completed)?;
    }
    guard.revalidate()?;
    if read_pending(guard.paths())?.as_ref() != Some(pending)
        || read_history(guard.paths())?.as_ref() != Some(&history)
    {
        return Err(recovery_refusal(
            "tree evidence changed during verification",
        ));
    }
    Ok(OwnedTree {
        root: parent.join(name),
        identity: identity(&root)?,
    })
}

/// Recheck a previously admitted handle against durable directory identities.
pub(in crate::customer) fn revalidate_owned_tree(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    tree: &OwnedTree,
    empty: bool,
) -> Result<(), CustomerError> {
    let history =
        read_history(guard.paths())?.ok_or_else(|| recovery_refusal("tree history is absent"))?;
    let first = history
        .first()
        .ok_or_else(|| recovery_refusal("tree history is empty"))?;
    let current = verify_tree_inner(
        guard,
        pending,
        bundle,
        &first.parent,
        first.parent_identity,
        &first.root_name,
        empty,
    )?;
    if current.path() != tree.path()
        || current.identity() != tree.identity()
        || read_history(guard.paths())?.as_ref() != Some(&history)
    {
        return Err(recovery_refusal("owned tree identity changed"));
    }
    Ok(())
}
