// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/absorber_record.rs
//! Durable, exclusive publication for a formatted temporary swap file.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;

use ramshared_block::swap_absorber::{
    AbsorberParentIdentity, RetainedAbsorberIdentity, SwapAbsorber,
};
use rustix::fs::{
    AtFlags, FileType, Mode, OFlags, RenameFlags, ResolveFlags, Stat, fchmod, fstat, openat,
    openat2, renameat_with, statat, unlinkat,
};
use serde::{Deserialize, Serialize};

use super::{
    RuntimePaths, canonical_boot_id, current_boot_id, effect_binding::runtime_staging_path,
};

const NAME: &str = "emergency-absorber.json";
const LIMIT: u64 = 4096;

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Prepared,
    ActiveObserved,
    InactiveVerified,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Record {
    schema: u32,
    boot_id: String,
    phase: Phase,
    pub(super) backing: RetainedAbsorberIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent: Option<AbsorberParentIdentity>,
}

fn boot_id() -> io::Result<String> {
    #[cfg(test)]
    if let Some(kernel_id) = READ_KERNEL_BOOT_ID
        .with(std::cell::Cell::get)
        .then(|| std::fs::read_to_string("/proc/sys/kernel/random/boot_id"))
    {
        let id = kernel_id?.trim().to_owned();
        return if canonical_boot_id(&id) {
            Ok(id)
        } else {
            Err(refusal("invalid fixture kernel boot identity"))
        };
    }
    current_boot_id().map_err(io::Error::other)
}

#[cfg(test)]
std::thread_local! {
    static READ_KERNEL_BOOT_ID: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(super) fn with_kernel_boot_id<T>(test: impl FnOnce() -> T) -> T {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            READ_KERNEL_BOOT_ID.with(|slot| slot.set(self.0));
        }
    }
    let _reset = Reset(READ_KERNEL_BOOT_ID.with(|slot| slot.replace(true)));
    test()
}

pub(super) struct AbsorberRecordStore {
    directory: File,
}

impl AbsorberRecordStore {
    pub(super) fn open(paths: &RuntimePaths) -> io::Result<Self> {
        let root = if paths.runtime_dir == Path::new("/run/ramshared") {
            Path::new("/var/lib/ramshared")
        } else {
            &paths.runtime_dir
        };
        let directory = openat2(
            rustix::fs::CWD,
            root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::NO_SYMLINKS,
        )?;
        let stat = fstat(&directory)?;
        if stat.st_uid != owner() || stat.st_mode & 0o022 != 0 {
            return Err(refusal("invalid retained absorber record directory"));
        }
        Ok(Self {
            directory: directory.into(),
        })
    }

    pub(super) fn publish(&self, backing: &SwapAbsorber) -> io::Result<()> {
        let record = Record {
            schema: 1,
            boot_id: boot_id()?,
            phase: Phase::Prepared,
            backing: backing.seal_retained_identity()?,
            parent: backing.seal_parent_identity()?,
        };
        self.persist(&record, None)
    }

    pub(super) fn mark_active_observed(&self, backing: &SwapAbsorber) -> io::Result<()> {
        let held = self
            .load()?
            .ok_or_else(|| refusal("retained absorber record is missing"))?;
        if held.record.backing != backing.retained_identity()? {
            return Err(refusal("retained absorber record names different backing"));
        }
        if held.inactive_verified() {
            return Err(refusal("retained absorber must remain inactive"));
        }
        if held.record.phase == Phase::ActiveObserved {
            return Ok(());
        }
        let mut next = held.record.clone();
        next.phase = Phase::ActiveObserved;
        self.persist(&next, Some(&held))
    }

    /// Replace the old receipt only after the caller has proven current
    /// inactivity and opened the exact file by its portable identity.
    pub(super) fn rebind_inactive(
        &self,
        held: &HeldRecord,
        backing: &SwapAbsorber,
    ) -> io::Result<HeldRecord> {
        let current = boot_id()?;
        let observed = backing.retained_identity()?;
        if held.record.boot_id == current || !held.record.backing.matches_other_boot(&observed) {
            return Err(refusal("prior-boot absorber identity is unproven"));
        }
        let next = Record {
            schema: 1,
            boot_id: current,
            phase: Phase::InactiveVerified,
            backing: observed,
            parent: backing.seal_parent_identity()?,
        };
        self.persist(&next, Some(held))?;
        self.load()?
            .ok_or_else(|| refusal("inactive absorber record is missing"))
    }

    fn persist(&self, record: &Record, replace: Option<&HeldRecord>) -> io::Result<()> {
        let bytes = serde_json::to_vec(record)?;
        if bytes.len() as u64 > LIMIT {
            return Err(refusal("retained absorber record exceeds its size limit"));
        }
        let staging = runtime_staging_path(Path::new(""), "emergency-absorber");
        let staging = staging
            .file_name()
            .ok_or_else(|| refusal("missing retained absorber staging name"))?;
        let file = openat(
            &self.directory,
            staging,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        )?;
        let staged = (|| -> io::Result<()> {
            let mut file = File::from(file);
            fchmod(&file, Mode::RUSR | Mode::WUSR)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            let flags = if let Some(held) = replace {
                held.revalidate(self)?;
                RenameFlags::empty()
            } else {
                RenameFlags::NOREPLACE
            };
            renameat_with(&self.directory, staging, &self.directory, NAME, flags)?;
            Ok(())
        })();
        if let Err(error) = staged {
            let _ = unlinkat(&self.directory, staging, AtFlags::empty());
            return Err(error);
        }
        self.directory.sync_all()
    }

    pub(super) fn load(&self) -> io::Result<Option<HeldRecord>> {
        let held = self.load_any_boot()?;
        if let Some(record) = &held
            && !record.current_boot()?
        {
            return Err(refusal("retained absorber record boot does not match"));
        }
        Ok(held)
    }

    pub(super) fn load_any_boot(&self) -> io::Result<Option<HeldRecord>> {
        let file = match openat(
            &self.directory,
            NAME,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        ) {
            Ok(file) => File::from(file),
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let snapshot = fstat(&file)?;
        verify_metadata(&snapshot)?;
        let mut bytes = Vec::new();
        (&file).take(LIMIT + 1).read_to_end(&mut bytes)?;
        let record: Record = serde_json::from_slice(&bytes)?;
        let held = HeldRecord {
            record,
            file,
            snapshot,
        };
        held.revalidate(self)?;
        if bytes.len() as u64 > LIMIT || usize::try_from(snapshot.st_size).ok() != Some(bytes.len())
        {
            return Err(refusal("retained absorber record length changed"));
        }
        if held.record.schema != 1 || !canonical_boot_id(&held.record.boot_id) {
            return Err(refusal("invalid retained absorber schema or boot identity"));
        }
        Ok(Some(held))
    }
}

pub(super) struct HeldRecord {
    pub(super) record: Record,
    file: File,
    snapshot: Stat,
}

impl HeldRecord {
    fn revalidate(&self, store: &AbsorberRecordStore) -> io::Result<()> {
        let opened = fstat(&self.file)?;
        let named = statat(&store.directory, NAME, AtFlags::SYMLINK_NOFOLLOW)?;
        for observed in [opened, named] {
            verify_metadata(&observed)?;
            if observed.st_dev != self.snapshot.st_dev
                || observed.st_ino != self.snapshot.st_ino
                || observed.st_size != self.snapshot.st_size
                || observed.st_mtime != self.snapshot.st_mtime
                || observed.st_mtime_nsec != self.snapshot.st_mtime_nsec
                || observed.st_ctime != self.snapshot.st_ctime
                || observed.st_ctime_nsec != self.snapshot.st_ctime_nsec
            {
                return Err(refusal("retained absorber record identity changed"));
            }
        }
        Ok(())
    }

    pub(super) fn current_boot(&self) -> io::Result<bool> {
        Ok(self.record.boot_id == boot_id()?)
    }

    pub(super) fn inactive_verified(&self) -> bool {
        self.record.phase == Phase::InactiveVerified
    }

    pub(super) fn can_finish_cleanup(&self) -> bool {
        self.record.phase != Phase::Prepared
    }

    /// Called only after a fresh proof of inactive cleanup.
    pub(super) fn clear(&self, store: &AbsorberRecordStore) -> io::Result<()> {
        if !self.can_finish_cleanup() || !self.current_boot()? {
            return Err(refusal("activation or current boot is unproven"));
        }
        self.remove_record(store)
    }

    pub(super) fn prove_prior_boot_absence<'a>(
        &self,
        store: &AbsorberRecordStore,
        swap_paths: impl IntoIterator<Item = &'a str>,
        reconcile: bool,
    ) -> io::Result<()> {
        if !self.can_finish_cleanup() || self.current_boot()? {
            return Err(refusal(
                "prior-boot receipt is not eligible for absent cleanup",
            ));
        }
        let parent = self
            .record
            .parent
            .as_ref()
            .ok_or_else(|| refusal("portable absorber parent is missing"))?;
        self.record
            .backing
            .prove_prior_boot_absence(parent, swap_paths, reconcile)?;
        if reconcile {
            self.remove_record(store)?;
        }
        Ok(())
    }

    fn remove_record(&self, store: &AbsorberRecordStore) -> io::Result<()> {
        self.revalidate(store)?;
        unlinkat(&store.directory, NAME, AtFlags::empty())?;
        store.directory.sync_all()
    }
}

fn owner() -> u32 {
    #[cfg(test)]
    {
        rustix::process::geteuid().as_raw()
    }
    #[cfg(not(test))]
    {
        0
    }
}

fn verify_metadata(stat: &Stat) -> io::Result<()> {
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_mode & 0o7777 != 0o600
        || stat.st_uid != owner()
        || stat.st_nlink != 1
        || stat.st_size <= 0
        || stat.st_size as u64 > LIMIT
    {
        return Err(refusal("invalid retained absorber record identity"));
    }
    Ok(())
}

fn refusal(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}
