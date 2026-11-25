// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/native_origin.rs
//! Descriptor custody and sealed identity of a native Linux file origin.
//!
//! Observation and exclusive creation never grant activation. Stable identity
//! comes from the profile and current volume resolver; live mount numbers are
//! checked against held descriptors and are never persisted as identity.

use std::fs::File;
use std::path::{Component, Path, PathBuf};

use ramshared_config::native_origin::identity_hash;
use ramshared_config::resource_profile::ResourceTarget;
use rustix::fs::{AtFlags, FileType, Mode, OFlags, Statx, StatxFlags, mkdirat, openat, statx};

mod publication;

const MANAGED_ROOT: &str = "var/lib/ramshared";
const STAT_BLOCK_BYTES: u64 = 512;

fn descriptor_stat(file: &File) -> Result<Statx, String> {
    let required = StatxFlags::TYPE
        | StatxFlags::MODE
        | StatxFlags::UID
        | StatxFlags::NLINK
        | StatxFlags::INO
        | StatxFlags::SIZE
        | StatxFlags::BLOCKS
        | StatxFlags::MNT_ID;
    let stat = statx(
        file,
        "",
        AtFlags::EMPTY_PATH | AtFlags::STATX_DONT_SYNC,
        required,
    )
    .map_err(|e| format!("cannot observe native origin descriptor: {e}"))?;
    if !StatxFlags::from_bits_retain(stat.stx_mask).contains(required) {
        return Err("native origin descriptor lacks required statx identity fields".into());
    }
    Ok(stat)
}

fn verify_mount(stat: &Statx, mount: &OriginMount) -> Result<(), String> {
    if stat.stx_mnt_id != mount.mount_id
        || format!("{}:{}", stat.stx_dev_major, stat.stx_dev_minor) != mount.major_minor
    {
        return Err("native origin descriptor differs from the current mount identity".into());
    }
    Ok(())
}

fn verify_directory(file: &File, mount: &OriginMount, owner_uid: u32) -> Result<(), String> {
    let stat = descriptor_stat(file)?;
    verify_mount(&stat, mount)?;
    if FileType::from_raw_mode(u32::from(stat.stx_mode)) != FileType::Directory
        || stat.stx_uid != owner_uid
        || stat.stx_mode & 0o022 != 0
    {
        return Err(
            "native origin directories must be root owned without group/world write".into(),
        );
    }
    Ok(())
}

fn managed_path(relative_path: &str) -> Result<PathBuf, String> {
    ramshared_config::native_origin::validate_managed_path(relative_path)?;
    Ok(Path::new(MANAGED_ROOT).join(relative_path))
}

/// Current mount observation supplied by the volume-resolution owner.
///
/// This is not a serving grant or a persisted filesystem/device identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OriginMount {
    pub mountpoint: String,
    pub mount_id: u64,
    pub major_minor: String,
}

/// A measured mount held across free-space observation and storage effects.
pub struct NativeOriginVolume {
    root: File,
    mount: OriginMount,
    owner_uid: u32,
}

impl NativeOriginVolume {
    /// Pin a currently admitted mount without following any path links.
    ///
    /// The caller owns platform, stable-volume identity and eligibility checks.
    /// UID, mount/device numbers and directory permissions are verified here.
    pub fn open(mount: &OriginMount, owner_uid: u32) -> Result<Self, String> {
        let path = Path::new(&mount.mountpoint);
        if !path.is_absolute()
            || path
                .components()
                .any(|part| matches!(part, Component::ParentDir))
        {
            return Err(
                "native origin mount path must be absolute without parent traversal".into(),
            );
        }
        let root = File::from(
            rustix::fs::openat2(
                rustix::fs::CWD,
                path,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
                rustix::fs::ResolveFlags::NO_SYMLINKS | rustix::fs::ResolveFlags::NO_MAGICLINKS,
            )
            .map_err(|error| format!("cannot open native origin mount without links: {error}"))?,
        );
        verify_directory(&root, mount, owner_uid)?;
        Ok(Self {
            root,
            mount: mount.clone(),
            owner_uid,
        })
    }

    /// Validate the shared managed namespace without touching the filesystem.
    pub fn validate_path(&self, path: &str) -> Result<(), String> {
        managed_path(path).map(|_| ())
    }

    /// Describe the reviewed location; effects must still use held descriptors.
    pub fn reviewed_path(&self, relative: &str) -> Result<PathBuf, String> {
        Ok(Path::new(&self.mount.mountpoint).join(managed_path(relative)?))
    }

    /// Refuse a changed mount or root pathname while retaining the original fd.
    pub fn revalidate(&self) -> Result<(), String> {
        verify_directory(&self.root, &self.mount, self.owner_uid)?;
        let named = Self::open(&self.mount, self.owner_uid)?;
        if descriptor_stat(&named.root)?.stx_ino != descriptor_stat(&self.root)?.stx_ino {
            return Err("native origin mount pathname identity changed".into());
        }
        Ok(())
    }

    fn parent(
        &self,
        relative: &str,
        create: bool,
    ) -> Result<Option<(File, std::ffi::OsString)>, String> {
        let relative = managed_path(relative)?;
        let leaf = relative
            .file_name()
            .ok_or("native origin path has no filename")?
            .to_os_string();
        let mut directory = self.root.try_clone().map_err(|error| error.to_string())?;
        let parent = relative
            .parent()
            .ok_or("native origin path has no parent")?;
        for component in parent.components() {
            let Component::Normal(name) = component else {
                return Err("unsafe native origin path component".into());
            };
            let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
            let opened = match openat(&directory, name, flags, Mode::empty()) {
                Ok(fd) => fd,
                Err(error) if error == rustix::io::Errno::NOENT && create => {
                    mkdirat(&directory, name, Mode::RWXU).map_err(|error| error.to_string())?;
                    directory.sync_all().map_err(|error| error.to_string())?;
                    openat(&directory, name, flags, Mode::empty())
                        .map_err(|error| error.to_string())?
                }
                Err(error) if error == rustix::io::Errno::NOENT && !create => return Ok(None),
                Err(error) => {
                    return Err(format!(
                        "cannot open managed native origin component: {error}"
                    ));
                }
            };
            directory = File::from(opened);
            verify_directory(&directory, &self.mount, self.owner_uid)?;
        }
        Ok(Some((directory, leaf)))
    }

    fn named(&self, relative: &str, access: OFlags) -> Result<File, String> {
        let (directory, leaf) = self
            .parent(relative, false)?
            .ok_or("native origin parent is absent")?;
        openat(
            &directory,
            leaf,
            access | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map(File::from)
        .map_err(|error| format!("cannot open native origin file: {error}"))
    }

    /// Observe absence without reserving a name; creation remains exclusive.
    pub fn require_absent(&self, relative: &str) -> Result<(), String> {
        self.revalidate()?;
        let Some((parent, leaf)) = self.parent(relative, false)? else {
            return Ok(());
        };
        match rustix::fs::statat(&parent, leaf, AtFlags::SYMLINK_NOFOLLOW) {
            Err(error) if error == rustix::io::Errno::NOENT => Ok(()),
            Ok(_) => {
                Err("native origin already exists without this receipt; it was not replaced".into())
            }
            Err(error) => Err(format!("cannot inspect native origin slot: {error}")),
        }
    }

    /// Measure available allocation bytes on the currently revalidated mount.
    pub fn available_bytes(&self) -> Result<u64, String> {
        self.revalidate()?;
        let stat = rustix::fs::fstatvfs(&self.root).map_err(|error| error.to_string())?;
        if stat.f_frsize == 0 || stat.f_flag.contains(rustix::fs::StatVfsMountFlags::RDONLY) {
            return Err(
                "native origin volume is read-only or has invalid allocation geometry".into(),
            );
        }
        let free = stat
            .f_bavail
            .checked_mul(stat.f_frsize)
            .ok_or("native volume free bytes overflow")?;
        let total = stat
            .f_blocks
            .checked_mul(stat.f_frsize)
            .ok_or("native volume total bytes overflow")?;
        if free > total {
            return Err("native volume free bytes exceed total bytes".into());
        }
        Ok(free)
    }

    /// Return a read-only fd after matching the shared identity and named inode.
    /// This observation grants neither writable serving nor activation authority.
    pub fn verify(&self, target: &ResourceTarget) -> Result<File, String> {
        self.verify_access(target, OFlags::RDONLY)
    }

    /// Admit existing origin I/O with a nonblocking exclusive file lease.
    /// No creation, allocation or resize is performed; the returned fd retains it.
    pub fn verify_for_io(&self, target: &ResourceTarget) -> Result<File, String> {
        let file = self.verify_access(target, OFlags::RDWR)?;
        rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).map_err(
            |error| format!("native origin already has an I/O owner or cannot be locked: {error}"),
        )?;
        self.verify(target)?;
        Ok(file)
    }

    fn verify_access(&self, target: &ResourceTarget, access: OFlags) -> Result<File, String> {
        let ResourceTarget::LinuxFileOrigin {
            managed_relative_path,
            inode,
            allocated_bytes,
            identity_field_hash,
            ..
        } = target
        else {
            return Err("an existing native file origin is required".into());
        };
        if identity_hash(target)? != *identity_field_hash {
            return Err(
                "native origin identity-field digest does not describe the declared identity"
                    .into(),
            );
        }
        let held = self.named(managed_relative_path, access)?;
        verify_file(
            &held,
            &self.mount,
            *inode,
            *allocated_bytes,
            self.owner_uid,
            1,
        )?;
        let named = self.named(managed_relative_path, OFlags::RDONLY)?;
        verify_file(
            &named,
            &self.mount,
            *inode,
            *allocated_bytes,
            self.owner_uid,
            1,
        )?;
        self.revalidate()?;
        Ok(held)
    }
}

fn verify_file(
    file: &File,
    mount: &OriginMount,
    inode: u64,
    allocated_bytes: u64,
    owner_uid: u32,
    link_count: u32,
) -> Result<Statx, String> {
    let stat = descriptor_stat(file)?;
    verify_mount(&stat, mount)?;
    if FileType::from_raw_mode(u32::from(stat.stx_mode)) != FileType::RegularFile
        || stat.stx_uid != owner_uid
        || stat.stx_mode & 0o7777 != 0o600
        || stat.stx_nlink != link_count
        || stat.stx_ino != inode
        || allocated_bytes == 0
        || stat.stx_size != allocated_bytes
        || stat
            .stx_blocks
            .checked_mul(STAT_BLOCK_BYTES)
            .is_none_or(|v| v < allocated_bytes)
    {
        return Err("native origin file ownership, inode, length or allocation differs from its sealed identity".into());
    }
    Ok(stat)
}

#[cfg(test)]
mod tests;
