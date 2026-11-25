// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/workload/ledger_lock.rs
//! Exclusive transition lock, quarantine snapshots, and stale-owner recovery for the ledger.

use super::{
    MAX_LEDGER_BYTES, OwnerIdentity, OwnerProbe, RESERVATION_LEDGER_SCHEMA_VERSION, WorkloadClass,
    unique_nonce,
};
use rustix::fs::{
    AtFlags, FileType, FlockOperation, Mode, OFlags, flock, fstat, fsync, mkdirat, open, openat,
    statat,
};
use rustix::process::geteuid;
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Reservation {
    pub id: String,
    pub owner: OwnerIdentity,
    pub class: WorkloadClass,
    pub memory_bytes: u64,
    pub unit: String,
    pub invocation_id: Option<String>,
    pub issued_ordinal: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct ReservationLedger {
    pub(super) schema_version: u32,
    pub(super) next_ordinal: u64,
    pub(super) reservations: Vec<Reservation>,
}

impl ReservationLedger {
    /// The one shape of a ledger that has never issued a reservation.
    ///
    /// `next_ordinal` starts at 1 because `validate_ledger` refuses a zero
    /// sequence and because durable issuance ordinals are strictly positive.
    /// Both the production initializer and the test fixtures construct this
    /// here, so an empty ledger has exactly one home.
    pub(super) fn empty() -> Self {
        Self {
            schema_version: RESERVATION_LEDGER_SCHEMA_VERSION,
            next_ordinal: 1,
            reservations: Vec::new(),
        }
    }
}

pub(super) struct LedgerPaths {
    pub(super) root: PathBuf,
    #[cfg(test)]
    pub(super) ledger: PathBuf,
    #[cfg(test)]
    pub(super) transition: PathBuf,
    pub(super) admission_state: PathBuf,
    #[cfg(test)]
    pub(super) quarantine: PathBuf,
}

impl LedgerPaths {
    pub(super) fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            #[cfg(test)]
            ledger: root.join("reservations.json"),
            #[cfg(test)]
            transition: root.join("ledger-transition.lock"),
            admission_state: root.join("admission-state.json"),
            #[cfg(test)]
            quarantine: root.join("quarantine"),
        }
    }
}

pub(super) struct LedgerLock {
    pub(super) paths: LedgerPaths,
    pub(super) root_file: File,
    pub(super) root_identity: FileIdentity,
    pub(super) transition_file: File,
    pub(super) transition_identity: FileIdentity,
    pub(super) quarantine_file: File,
    pub(super) quarantine_identity: FileIdentity,
}

impl Drop for LedgerLock {
    fn drop(&mut self) {
        let _ = flock(&self.root_file, FlockOperation::Unlock);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FileIdentity {
    pub(super) device: u64,
    pub(super) inode: u64,
}

impl FileIdentity {
    pub(super) fn from_stat(stat: &rustix::fs::Stat) -> Self {
        Self {
            device: stat.st_dev,
            inode: stat.st_ino,
        }
    }
}

pub(super) fn open_directory_path(path: &Path, create_missing: bool) -> Result<File, String> {
    if path.as_os_str().is_empty() {
        return Err("ledger authority path is empty".into());
    }
    let start = if path.is_absolute() { "/" } else { "." };
    let mut directory = File::from(
        open(
            start,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| format!("open ledger authority anchor: {error}"))?,
    );
    for component in path.components() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => name,
            Component::ParentDir | Component::Prefix(_) => {
                return Err("ledger authority path contains an unsafe component".into());
            }
        };
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let opened = match openat(&directory, name, flags, Mode::empty()) {
            Ok(opened) => opened,
            Err(rustix::io::Errno::NOENT) if create_missing => {
                mkdirat(&directory, name, Mode::RWXU)
                    .map_err(|error| format!("create ledger authority component: {error}"))?;
                fsync(&directory).map_err(|error| {
                    format!("sync ledger authority parent after creation: {error}")
                })?;
                openat(&directory, name, flags, Mode::empty())
                    .map_err(|error| format!("open created ledger authority component: {error}"))?
            }
            Err(error) => {
                return Err(format!(
                    "open ledger authority component without following links: {error}"
                ));
            }
        };
        directory = File::from(opened);
    }
    Ok(directory)
}

pub(super) fn validate_directory(file: &File, label: &str) -> Result<FileIdentity, String> {
    let stat = fstat(file).map_err(|error| format!("inspect {label}: {error}"))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::Directory
        || stat.st_uid != geteuid().as_raw()
        || stat.st_mode & 0o022 != 0
    {
        return Err(format!(
            "{label} must be a directory owned by the effective user without group/world write"
        ));
    }
    Ok(FileIdentity::from_stat(&stat))
}

pub(super) fn validate_regular(file: &File, label: &str) -> Result<FileIdentity, String> {
    let stat = fstat(file).map_err(|error| format!("inspect {label}: {error}"))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_uid != geteuid().as_raw()
        || stat.st_mode & 0o022 != 0
        || stat.st_nlink != 1
    {
        return Err(format!(
            "{label} must be a singly linked regular file owned by the effective user without group/world write"
        ));
    }
    Ok(FileIdentity::from_stat(&stat))
}

pub(super) fn named_identity(
    directory: &File,
    name: &OsStr,
    expected_type: FileType,
    label: &str,
) -> Result<FileIdentity, String> {
    let stat = statat(directory, name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|error| format!("inspect named {label}: {error}"))?;
    if FileType::from_raw_mode(stat.st_mode) != expected_type {
        return Err(format!("named {label} has an unsafe filesystem type"));
    }
    Ok(FileIdentity::from_stat(&stat))
}

pub(super) fn open_or_create_regular(
    directory: &File,
    name: &OsStr,
    label: &str,
) -> Result<(File, bool), String> {
    let existing_flags = OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let (opened, created) = match openat(directory, name, existing_flags, Mode::empty()) {
        Ok(opened) => (opened, false),
        Err(rustix::io::Errno::NOENT) => (
            openat(
                directory,
                name,
                existing_flags | OFlags::CREATE | OFlags::EXCL,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(|error| format!("create {label}: {error}"))?,
            true,
        ),
        Err(error) => return Err(format!("open {label} without following links: {error}")),
    };
    let file = File::from(opened);
    validate_regular(&file, label)?;
    if created {
        fsync(directory).map_err(|error| format!("sync parent after creating {label}: {error}"))?;
    }
    Ok((file, created))
}

pub(super) fn open_or_create_directory(
    directory: &File,
    name: &OsStr,
    label: &str,
) -> Result<(File, bool), String> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let (opened, created) = match openat(directory, name, flags, Mode::empty()) {
        Ok(opened) => (opened, false),
        Err(rustix::io::Errno::NOENT) => {
            mkdirat(directory, name, Mode::RWXU)
                .map_err(|error| format!("create {label}: {error}"))?;
            let opened = openat(directory, name, flags, Mode::empty())
                .map_err(|error| format!("open created {label}: {error}"))?;
            (opened, true)
        }
        Err(error) => return Err(format!("open {label} without following links: {error}")),
    };
    let file = File::from(opened);
    validate_directory(&file, label)?;
    if created {
        fsync(directory).map_err(|error| format!("sync parent after creating {label}: {error}"))?;
    }
    Ok((file, created))
}

impl LedgerLock {
    pub(super) fn revalidate(&self) -> Result<(), String> {
        if validate_directory(&self.root_file, "ledger authority")? != self.root_identity {
            return Err("opened ledger directory authority identity changed".into());
        }
        let named_root = open_directory_path(&self.paths.root, false)?;
        if validate_directory(&named_root, "named ledger authority")? != self.root_identity {
            return Err("named ledger directory authority was replaced".into());
        }
        if validate_regular(&self.transition_file, "transition owner")? != self.transition_identity
            || named_identity(
                &self.root_file,
                OsStr::new("ledger-transition.lock"),
                FileType::RegularFile,
                "transition owner",
            )? != self.transition_identity
        {
            return Err("transition owner identity changed".into());
        }
        if validate_directory(&self.quarantine_file, "quarantine authority")?
            != self.quarantine_identity
            || named_identity(
                &self.root_file,
                OsStr::new("quarantine"),
                FileType::Directory,
                "quarantine authority",
            )? != self.quarantine_identity
        {
            return Err("quarantine authority identity changed".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct TransitionOwnerRecord {
    pub(super) schema_version: u32,
    pub(super) owner: OwnerIdentity,
}

pub(super) const TRANSITION_OWNER_SCHEMA_VERSION: u32 = 1;

pub(super) fn transition_record(
    lock: &mut LedgerLock,
) -> Result<Option<TransitionOwnerRecord>, String> {
    lock.revalidate()?;
    lock.transition_file
        .seek(SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    (&mut lock.transition_file)
        .take(MAX_LEDGER_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_LEDGER_BYTES {
        return Err("reservation ledger transition owner exceeds its bounded size".into());
    }
    lock.revalidate()?;
    if bytes.is_empty() {
        return Ok(None);
    }
    let record: TransitionOwnerRecord = serde_json::from_slice(&bytes).map_err(|_| {
        "reservation ledger transition owner is malformed or from an unsupported protocol"
            .to_string()
    })?;
    if record.schema_version != TRANSITION_OWNER_SCHEMA_VERSION
        || record.owner.boot_id.is_empty()
        || record.owner.pid == 0
        || record.owner.start_time == 0
        || record.owner.nonce.is_empty()
        || !record
            .owner
            .nonce
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        return Err(
            "reservation ledger transition owner is malformed or from an unsupported protocol"
                .into(),
        );
    }
    Ok(Some(record))
}

#[cfg(test)]
pub(super) type TransitionRecoveryHook = Box<dyn Fn(&LedgerPaths)>;

#[cfg(test)]
thread_local! {
    pub(super) static TRANSITION_RECOVERY_HOOK: std::cell::RefCell<Option<TransitionRecoveryHook>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
pub(super) fn invoke_transition_recovery_hook(paths: &LedgerPaths) {
    TRANSITION_RECOVERY_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook(paths);
        }
    });
}

#[cfg(not(test))]
pub(super) fn invoke_transition_recovery_hook(_paths: &LedgerPaths) {}

pub(super) fn write_transition_record(
    lock: &mut LedgerLock,
    owner: &OwnerIdentity,
) -> Result<(), String> {
    lock.revalidate()?;
    let record = TransitionOwnerRecord {
        schema_version: TRANSITION_OWNER_SCHEMA_VERSION,
        owner: owner.clone(),
    };
    let bytes = serde_json::to_vec(&record).map_err(|error| error.to_string())?;
    let result = (|| {
        lock.transition_file
            .set_len(0)
            .map_err(|error| error.to_string())?;
        lock.transition_file
            .seek(SeekFrom::Start(0))
            .map_err(|error| error.to_string())?;
        lock.transition_file
            .write_all(&bytes)
            .map_err(|error| error.to_string())?;
        lock.transition_file
            .sync_all()
            .map_err(|error| error.to_string())
    })();
    if result.is_err() {
        let _ = lock.transition_file.set_len(0);
        let _ = lock.transition_file.sync_all();
    }
    result?;
    lock.revalidate()
}

/// Preserve exact bytes in the quarantine before a recovery step discards
/// them. `create_new` + `NOFOLLOW` + `O_EXCL` makes the snapshot a new
/// append-only record rather than a rewrite of anything that existed, and the
/// containing directory is synced so the evidence survives a crash.
///
/// This is the one recovery-evidence path: the transition owner record and a
/// recovered reservation row both land here, under a name that says which
/// they are.
pub(super) fn quarantine_snapshot(
    lock: &mut LedgerLock,
    name_prefix: &str,
    contents: &[u8],
    what: &str,
) -> Result<(), String> {
    if contents.len() as u64 > MAX_LEDGER_BYTES {
        return Err(format!("{what} recovery evidence exceeds its bounded size"));
    }
    lock.revalidate().map_err(|_| {
        "ledger authority changed during recovery; containment retained".to_string()
    })?;
    let snapshot_name = format!("{name_prefix}-{}.json", unique_nonce());
    let snapshot = openat(
        &lock.quarantine_file,
        snapshot_name.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(|error| format!("preserve {what} before recovery: {error}"))?;
    let mut snapshot = File::from(snapshot);
    validate_regular(&snapshot, "recovery quarantine snapshot")?;
    snapshot
        .write_all(contents)
        .map_err(|error| format!("preserve {what} before recovery: {error}"))?;
    snapshot
        .sync_all()
        .map_err(|error| format!("sync {what} recovery evidence: {error}"))?;
    fsync(&lock.quarantine_file).map_err(|error| format!("sync quarantine directory: {error}"))?;
    lock.revalidate()
        .map_err(|_| "ledger authority changed during recovery; containment retained".to_string())
}

pub(super) fn quarantine_transition_snapshot(lock: &mut LedgerLock) -> Result<(), String> {
    lock.revalidate().map_err(|_| {
        "transition owner changed during recovery; containment retained".to_string()
    })?;
    lock.transition_file
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("read transition owner for recovery: {error}"))?;
    let mut contents = Vec::with_capacity((MAX_LEDGER_BYTES + 1) as usize);
    (&mut lock.transition_file)
        .take(MAX_LEDGER_BYTES + 1)
        .read_to_end(&mut contents)
        .map_err(|error| format!("read transition owner for recovery: {error}"))?;
    if contents.len() as u64 > MAX_LEDGER_BYTES {
        return Err("transition owner recovery evidence exceeds its bounded size".into());
    }
    quarantine_snapshot(lock, "transition-owner", &contents, "transition owner")
}

pub(super) fn acquire_lock(
    paths: &LedgerPaths,
    owner: &OwnerIdentity,
    probe: &dyn OwnerProbe,
) -> Result<LedgerLock, String> {
    let root_file = open_directory_path(&paths.root, true)?;
    let root_identity = validate_directory(&root_file, "ledger authority")?;
    match flock(&root_file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(rustix::io::Errno::WOULDBLOCK) => {
            return Err("reservation ledger transition is busy or ambiguous".into());
        }
        Err(error) => {
            return Err(format!("lock reservation ledger transition: {error}"));
        }
    }
    let (transition_file, _) = open_or_create_regular(
        &root_file,
        OsStr::new("ledger-transition.lock"),
        "reservation ledger transition owner",
    )?;
    let transition_identity = validate_regular(&transition_file, "transition owner")?;
    let (quarantine_file, _) = open_or_create_directory(
        &root_file,
        OsStr::new("quarantine"),
        "reservation ledger quarantine",
    )?;
    let quarantine_identity = validate_directory(&quarantine_file, "quarantine authority")?;
    let mut lock = LedgerLock {
        paths: LedgerPaths::new(&paths.root),
        root_file,
        root_identity,
        transition_file,
        transition_identity,
        quarantine_file,
        quarantine_identity,
    };
    lock.revalidate()?;
    let existing = transition_record(&mut lock)?;
    if let Some(existing) = existing {
        // A successful advisory acquisition is the liveness authority for a
        // compliant owner: it cannot still be inside a transition.  A dead
        // or prior-boot record is crash evidence, so copy it before reuse.
        // Never rename or unlink the canonical path: the opened inode must
        // remain the inode named by the path throughout recovery.
        let stale_owner =
            existing.owner.boot_id != owner.boot_id || !probe.is_alive(&existing.owner);
        if stale_owner {
            quarantine_transition_snapshot(&mut lock)?;
        }
        invoke_transition_recovery_hook(paths);
        lock.revalidate().map_err(|_| {
            "transition owner changed during recovery; containment retained".to_string()
        })?;
    }
    write_transition_record(&mut lock, owner)?;
    lock.revalidate().map_err(|_| {
        "transition owner changed while claiming lock; containment retained".to_string()
    })?;
    Ok(lock)
}
