// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/transition_custody.rs
//! Customer mutation custody: the layout, the held guard, and the one
//! no-symlink method that opens a named object beneath it (ITEM-5).
//!
//! One owner of "open a named object beneath a root without ever following a
//! symlink, and prove the opened object is still the one that was opened". The
//! locking pattern follows `crate::workload` — open every component with
//! `O_NOFOLLOW`, pin the `dev`+`inode` identity of what was actually opened,
//! revalidate after every step that can race — but the ledger semantics do not
//! apply here. This is a different resource with a different job: custody of
//! one customer mutation at a time.
//!
//! [`TransitionPaths`] is the one place the product layout is named. Record
//! schemas and filenames belong to the module that owns each record; this
//! module only confines how a named object is opened and what an already held
//! guard may write under. Who may take custody, and what a durable package
//! record blocks, is [`super::transition`]'s job.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use rustix::fs::{
    AtFlags, FileType, FlockOperation, Mode, OFlags, flock, fstat, fsync, mkdirat, open, openat,
    unlinkat,
};
use rustix::process::geteuid;

use super::{CustomerError, CustomerReason, CustomerState};

/// Who is asking to mutate.
///
/// The vocabulary is named before the first mutator consumes it (ITEM-5's
/// transaction and package owners) so a later command cannot invent an owner
/// and reach around the guard. Nested lifecycle calls **borrow the already
/// held typed guard**; they never acquire this non-reentrant lock again.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OperationOwner {
    /// `setup` — the supervised installation transaction.
    Setup,
    /// `up` — cascade activation.
    Up,
    /// boot-policy consent and application.
    Boot,
    /// tier/selection mutation.
    Selection,
    /// a package hook's enroll/commit/reconcile.
    Package,
}

impl OperationOwner {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Setup => "setup",
            Self::Up => "up",
            Self::Boot => "boot",
            Self::Selection => "selection",
            Self::Package => "package",
        }
    }
}

/// Where a transition lock and the package-pending journal live.
///
/// Injected so a test never writes to `/run` or `/var/lib`, and so
/// [`TransitionPaths::production`] is the single place those absolute paths
/// are named.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TransitionPaths {
    pub(crate) lock_root: PathBuf,
    pub(crate) transactions_root: PathBuf,
}

impl TransitionPaths {
    /// The product layout. One place names these absolute paths.
    pub(crate) fn production() -> Self {
        Self {
            lock_root: PathBuf::from("/run/ramshared"),
            transactions_root: PathBuf::from("/var/lib/ramshared/transactions"),
        }
    }

    /// A layout beneath an already isolated root. Tests only.
    #[cfg(test)]
    pub(crate) fn under(root: &Path) -> Self {
        Self {
            lock_root: root.join("run"),
            transactions_root: root.join("transactions"),
        }
    }

    /// The directory that holds durable transaction records. Record filenames
    /// belong to the module that owns each record, not to this one.
    pub(crate) fn transactions_root(&self) -> &Path {
        &self.transactions_root
    }

    pub(crate) fn lock_file_name() -> &'static OsStr {
        OsStr::new("transition.lock")
    }

    pub(crate) fn pending_file_name() -> &'static OsStr {
        OsStr::new("package-pending.json")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileIdentity {
    device: u64,
    inode: u64,
}

impl FileIdentity {
    fn from_stat(stat: &rustix::fs::Stat) -> Self {
        Self {
            device: stat.st_dev,
            inode: stat.st_ino,
        }
    }
}

pub(crate) fn confinement_refusal(detail: impl Into<String>) -> CustomerError {
    CustomerError {
        reason: CustomerReason::UnsupportedSurface,
        detail: detail.into(),
        state: CustomerState::InstalledOff,
    }
}

/// Open each component of `path` without ever following a symlink.
///
/// `..`, an empty path and a prefix component are refused rather than
/// normalized: a caller that needs to leave the root must name a different
/// root. Matches the `crate::workload` pattern this module follows.
///
/// `Ok(None)` means a component is absent and `create_missing` was `false` —
/// the normal empty state of a journal that has never been written, never an
/// error. A missing component with `create_missing` set is created `0700`.
pub(crate) fn open_directory_no_follow(
    path: &Path,
    create_missing: bool,
) -> Result<Option<File>, CustomerError> {
    if path.as_os_str().is_empty() {
        return Err(confinement_refusal("transition path is empty"));
    }
    let start = if path.is_absolute() { "/" } else { "." };
    let mut directory = File::from(
        open(
            start,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| confinement_refusal(format!("open transition path anchor: {error}")))?,
    );
    for component in path.components() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => name,
            Component::ParentDir | Component::Prefix(_) => {
                return Err(confinement_refusal(
                    "transition path contains an unsafe component",
                ));
            }
        };
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let opened = match openat(&directory, name, flags, Mode::empty()) {
            Ok(opened) => opened,
            Err(error) if error == rustix::io::Errno::NOENT && create_missing => {
                mkdirat(&directory, name, Mode::RWXU).map_err(|error| {
                    confinement_refusal(format!("create transition path component: {error}"))
                })?;
                fsync(&directory).map_err(|error| {
                    confinement_refusal(format!("sync transition path parent: {error}"))
                })?;
                openat(&directory, name, flags, Mode::empty()).map_err(|error| {
                    confinement_refusal(format!("open created transition path component: {error}"))
                })?
            }
            Err(error) if error == rustix::io::Errno::NOENT => return Ok(None),
            Err(error) => {
                return Err(confinement_refusal(format!(
                    "open transition path component without following links: {error}"
                )));
            }
        };
        directory = File::from(opened);
    }
    Ok(Some(directory))
}

pub(crate) fn validate_directory(file: &File, label: &str) -> Result<FileIdentity, CustomerError> {
    let stat =
        fstat(file).map_err(|error| confinement_refusal(format!("inspect {label}: {error}")))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::Directory
        || stat.st_uid != geteuid().as_raw()
        || stat.st_mode & 0o022 != 0
    {
        return Err(confinement_refusal(format!(
            "{label} must be a directory owned by the effective user without group/world write"
        )));
    }
    Ok(FileIdentity::from_stat(&stat))
}

pub(crate) fn validate_regular(file: &File, label: &str) -> Result<FileIdentity, CustomerError> {
    let stat =
        fstat(file).map_err(|error| confinement_refusal(format!("inspect {label}: {error}")))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_uid != geteuid().as_raw()
        || stat.st_mode & 0o022 != 0
        || stat.st_nlink != 1
    {
        return Err(confinement_refusal(format!(
            "{label} must be a singly linked regular file owned by the effective user without group/world write"
        )));
    }
    Ok(FileIdentity::from_stat(&stat))
}

/// Open (or optionally create) one named object beneath `directory_path`.
///
/// The parent directory is opened no-follow and validated first, so a symlink
/// standing in for either the directory or the name is refused before any
/// write happens. `Ok(None)` means the name — or an ancestor of it — is absent
/// and `create_missing` was `false`. That is the normal empty state of a
/// journal that has never been written, never an error and never a silent
/// success; a caller that must fail closed on absence says so at its own site.
///
/// This is the one confinement method for a named journal object. Record
/// schemas and filenames belong to the module that owns the record.
pub(crate) fn open_named_file(
    directory_path: &Path,
    name: &OsStr,
    label: &str,
    create_missing: bool,
) -> Result<Option<File>, CustomerError> {
    let Some(directory) = open_directory_no_follow(directory_path, create_missing)? else {
        return Ok(None);
    };
    // `label` names the object being opened. The check below is on its parent
    // directory, and naming the object there sends the operator to the wrong
    // file when confinement refuses.
    validate_directory(&directory, &format!("parent directory of {label}"))?;
    let existing_flags = OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let opened = match openat(&directory, name, existing_flags, Mode::empty()) {
        Ok(opened) => opened,
        Err(error) if error == rustix::io::Errno::NOENT && create_missing => {
            let created = openat(
                &directory,
                name,
                existing_flags | OFlags::CREATE | OFlags::EXCL,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(|error| confinement_refusal(format!("create {label}: {error}")))?;
            fsync(&directory).map_err(|error| {
                confinement_refusal(format!("sync parent after creating {label}: {error}"))
            })?;
            created
        }
        Err(error) if error == rustix::io::Errno::NOENT => return Ok(None),
        Err(error) => {
            return Err(confinement_refusal(format!(
                "open {label} without following links: {error}"
            )));
        }
    };
    let file = File::from(opened);
    validate_regular(&file, label)?;
    Ok(Some(file))
}

/// Exclusive mutation custody for exactly one operation.
///
/// Holding this is the authority to mutate. It is not an authorization to
/// mutate something else: every effect still validates its own target. The
/// lock is advisory `flock` on the named lock file and is released by process
/// death, which is the desired failure mode — a dead mutator holds nothing and
/// the durable package-pending record is what carries package exclusion
/// across that gap.
#[derive(Debug)]
pub(crate) struct CustomerTransitionGuard {
    lock_file: File,
    lock_identity: FileIdentity,
    owner: OperationOwner,
    paths: TransitionPaths,
}

impl Drop for CustomerTransitionGuard {
    fn drop(&mut self) {
        let _ = flock(&self.lock_file, FlockOperation::Unlock);
    }
}

impl CustomerTransitionGuard {
    /// Adopt an already opened and validated lock file as held custody.
    ///
    /// The caller has taken the exclusive lock and pinned the identity. This
    /// is the one constructor so a later owner cannot assemble a guard around
    /// a file it never validated.
    pub(crate) fn adopted(
        lock_file: File,
        lock_identity: FileIdentity,
        owner: OperationOwner,
        paths: TransitionPaths,
    ) -> Self {
        Self {
            lock_file,
            lock_identity,
            owner,
            paths,
        }
    }

    pub(crate) fn owner(&self) -> OperationOwner {
        self.owner
    }

    pub(crate) fn paths(&self) -> &TransitionPaths {
        &self.paths
    }

    /// Re-pin what was opened. Called after any step that can race.
    ///
    /// Absence is a refusal here, not an empty journal: this guard already
    /// holds an opened lock file, so a vanished name means custody was broken
    /// and the fail-closed answer is to stop mutating.
    pub(crate) fn revalidate(&self) -> Result<(), CustomerError> {
        let named = open_named_file(
            &self.paths.lock_root,
            TransitionPaths::lock_file_name(),
            "transition lock",
            false,
        )?
        .ok_or_else(|| confinement_refusal("transition lock vanished under the held guard"))?;
        let named_identity = validate_regular(&named, "transition lock")?;
        if named_identity != self.lock_identity {
            return Err(confinement_refusal(
                "transition lock identity changed under the held guard",
            ));
        }
        Ok(())
    }
}

/// Maximum size accepted when reading a durable journal record (fits real 45+ member bundles).
pub(crate) const MAX_JOURNAL_BODY_BYTES: u64 = 256 * 1024;

/// Read one journal body. `Ok(None)` is the normal empty state: the name — or
/// an ancestor of it — is absent and nothing has ever been enrolled under it.
///
/// Takes no lock: an observation command must be able to report a durable
/// record without becoming a mutator. The schema belongs to the caller.
pub(crate) fn read_journal_body(
    paths: &TransitionPaths,
    name: &OsStr,
    label: &str,
) -> Result<Option<String>, CustomerError> {
    let Some(file) = open_named_file(&paths.transactions_root, name, label, false)? else {
        return Ok(None);
    };
    let mut body = String::new();
    file.take(MAX_JOURNAL_BODY_BYTES + 1)
        .read_to_string(&mut body)
        .map_err(|error| confinement_refusal(format!("read {label}: {error}")))?;
    if body.len() as u64 > MAX_JOURNAL_BODY_BYTES {
        return Err(confinement_refusal(format!(
            "{label} exceeds the journal byte limit"
        )));
    }
    Ok(Some(body))
}

/// Write one journal body under an already held guard and make it durable
/// before returning. The name and the schema belong to the caller.
///
/// Fsync of both the record and its directory entry is the whole contract: if
/// the effect runs and the record is not durable, a crash cannot be told apart
/// from a clean finish. Identity is re-proved before and after the write, so
/// this is the single owner of "revalidate around a durable write".
pub(crate) fn write_journal_record(
    guard: &CustomerTransitionGuard,
    name: &OsStr,
    label: &str,
    body: &str,
) -> Result<(), CustomerError> {
    guard.revalidate()?;
    let mut file = open_named_file(guard.paths().transactions_root(), name, label, true)?
        .ok_or_else(|| {
            confinement_refusal(format!("{label} was not created under the journal root"))
        })?;
    file.seek(SeekFrom::Start(0))
        .map_err(|error| confinement_refusal(format!("rewind {label}: {error}")))?;
    file.set_len(0)
        .map_err(|error| confinement_refusal(format!("truncate {label}: {error}")))?;
    file.write_all(body.as_bytes())
        .map_err(|error| confinement_refusal(format!("write {label}: {error}")))?;
    file.sync_all()
        .map_err(|error| confinement_refusal(format!("sync {label}: {error}")))?;
    let directory = open_directory_no_follow(guard.paths().transactions_root(), false)?
        .ok_or_else(|| confinement_refusal(format!("{label} journal vanished during write")))?;
    fsync(&directory)
        .map_err(|error| confinement_refusal(format!("sync {label} journal: {error}")))?;
    guard.revalidate()
}

/// Remove one journal record beneath a held guard if present and fsync the journal directory.
pub(crate) fn clear_journal_file(
    guard: &CustomerTransitionGuard,
    name: &OsStr,
    label: &str,
) -> Result<(), CustomerError> {
    guard.revalidate()?;
    let Some(directory) = open_directory_no_follow(guard.paths().transactions_root(), false)?
    else {
        return Ok(());
    };
    validate_directory(&directory, label)?;
    match unlinkat(&directory, name, AtFlags::empty()) {
        Ok(()) | Err(rustix::io::Errno::NOENT) => {}
        Err(error) => {
            return Err(confinement_refusal(format!("clear {label}: {error}")));
        }
    }
    fsync(&directory)
        .map_err(|error| confinement_refusal(format!("sync {label} journal: {error}")))?;
    guard.revalidate()
}
