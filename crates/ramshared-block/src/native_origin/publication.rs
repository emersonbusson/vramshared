// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/native_origin/publication.rs
//! Inode-bound creation provenance, sealed before exclusive named publication.

use super::*;
use ramshared_config::native_origin::NativeOriginManifest;
use ramshared_config::resource_profile::MAX_RESOURCE_PROFILE_BYTES;
use rustix::fs::{FallocateFlags, XattrFlags, fallocate, fchmod, fgetxattr, fsetxattr, linkat};
use std::os::fd::AsRawFd;

const CREATION_ATTRIBUTE: &str = "user.ramshared.creation";

pub(super) fn link_anonymous_with_fallback(
    primary: rustix::io::Result<()>,
    fallback: &mut dyn FnMut() -> rustix::io::Result<()>,
) -> rustix::io::Result<()> {
    match primary {
        Ok(()) => Ok(()),
        // Linux < 6.10 returns ENOENT for linkat(AT_EMPTY_PATH) on an unlinked
        // O_TMPFILE without CAP_DAC_READ_SEARCH; link the exact held descriptor
        // via /proc/self/fd/<fd> with AT_SYMLINK_FOLLOW, still failing EEXIST on
        // any pre-existing target file or symlink.
        Err(rustix::io::Errno::NOENT) => fallback(),
        Err(err) => Err(err),
    }
}

/// Exclusively publish the held anonymous inode as a leaf in the held parent.
/// The caller owns validation of both descriptors and the destination name.
pub fn link_anonymous_inode(
    file: &File,
    parent: &File,
    leaf: &std::ffi::OsStr,
) -> rustix::io::Result<()> {
    link_anonymous_with_fallback(
        linkat(file, "", parent, leaf, AtFlags::EMPTY_PATH),
        &mut || link_anonymous_via_proc_fd(file, parent, leaf),
    )
}

pub(super) fn link_anonymous_via_proc_fd(
    file: &File,
    parent: &File,
    leaf: &std::ffi::OsStr,
) -> rustix::io::Result<()> {
    use rustix::fs::{PROC_SUPER_MAGIC, fstat, fstatfs, open, statat};
    use rustix::io::Errno;

    let proc_fds = open(
        "/proc/self/fd",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    if fstatfs(&proc_fds)?.f_type != PROC_SUPER_MAGIC {
        return Err(Errno::XDEV);
    }
    let source = file.as_raw_fd().to_string();
    let held = fstat(file)?;
    let observed = statat(&proc_fds, source.as_str(), AtFlags::empty())?;
    if held.st_dev != observed.st_dev || held.st_ino != observed.st_ino {
        return Err(Errno::STALE);
    }
    linkat(
        &proc_fds,
        source.as_str(),
        parent,
        leaf,
        AtFlags::SYMLINK_FOLLOW,
    )
}

fn read_creation(file: &File) -> Result<NativeOriginManifest, String> {
    let mut bytes = vec![0; MAX_RESOURCE_PROFILE_BYTES];
    let length = fgetxattr(file, CREATION_ATTRIBUTE, bytes.as_mut_slice())
        .map_err(|error| format!("cannot read native inode creation proof: {error}"))?;
    let text = std::str::from_utf8(&bytes[..length]).map_err(|error| error.to_string())?;
    NativeOriginManifest::parse(text)
}

impl NativeOriginVolume {
    /// Allocate anonymously and store the exact creator manifest on the inode
    /// before publishing its name. No named unsealed-file fallback is allowed.
    pub fn create_sealed(
        &self,
        request: &ResourceTarget,
        creator: impl FnOnce(ResourceTarget) -> Result<NativeOriginManifest, String>,
    ) -> Result<(File, NativeOriginManifest), String> {
        let ResourceTarget::LinuxFileOriginRequest {
            managed_relative_path,
            ..
        } = request
        else {
            return Err("a native file-origin request is required".into());
        };
        self.require_absent(managed_relative_path)?;
        let (parent, leaf) = self
            .parent(managed_relative_path, true)?
            .ok_or("native origin parent was not created")?;
        let (file, target) = self.allocate_inode(request, &parent)?;
        let manifest = creator(target.clone())?;
        if manifest.target != target {
            return Err("native creator proof does not identify the allocated inode".into());
        }
        let bytes = manifest.encode()?;
        fsetxattr(
            &file,
            CREATION_ATTRIBUTE,
            bytes.as_bytes(),
            XattrFlags::CREATE,
        )
        .map_err(|error| format!("seal native inode creation proof: {error}"))?;
        if read_creation(&file)? != manifest {
            return Err("native inode creation proof read-back differs".into());
        }
        file.sync_all().map_err(|error| error.to_string())?;
        self.revalidate()?;
        // This is the same held anonymous inode, never a pathname lookup of a
        // staging file. Existing final names, including symlinks, are refused.
        link_anonymous_inode(&file, &parent, &leaf)
            .map_err(|error| format!("publish sealed native origin exclusively: {error}"))?;
        parent.sync_all().map_err(|error| error.to_string())?;
        self.verify(&target)?;
        Ok((file, manifest))
    }

    fn allocate_inode(
        &self,
        request: &ResourceTarget,
        parent: &File,
    ) -> Result<(File, ResourceTarget), String> {
        let ResourceTarget::LinuxFileOriginRequest {
            filesystem_uuid,
            device_identity,
            managed_relative_path,
            allocated_bytes,
        } = request
        else {
            return Err("a native file-origin request is required".into());
        };
        let file = File::from(
            openat(
                parent,
                ".",
                OFlags::RDWR | OFlags::TMPFILE | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(|error| format!("allocate anonymous native origin: {error}"))?,
        );
        rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)
            .map_err(|error| format!("lock anonymous native origin: {error}"))?;
        fchmod(&file, Mode::RUSR | Mode::WUSR).map_err(|error| error.to_string())?;
        fallocate(&file, FallocateFlags::empty(), 0, *allocated_bytes)
            .map_err(|error| format!("allocate anonymous native origin bytes: {error}"))?;
        let inode = descriptor_stat(&file)?.stx_ino;
        let mut target = ResourceTarget::LinuxFileOrigin {
            filesystem_uuid: filesystem_uuid.clone(),
            device_identity: device_identity.clone(),
            managed_relative_path: managed_relative_path.clone(),
            inode,
            allocated_bytes: *allocated_bytes,
            identity_field_hash: String::new(),
        };
        let hash = identity_hash(&target)?;
        if let ResourceTarget::LinuxFileOrigin {
            identity_field_hash,
            ..
        } = &mut target
        {
            *identity_field_hash = hash;
        }
        // Before publication the anonymous inode must have no names; all
        // remaining private-file and allocation invariants already apply.
        verify_file(
            &file,
            &self.mount,
            inode,
            *allocated_bytes,
            self.owner_uid,
            0,
        )?;
        Ok((file, target))
    }

    /// Revalidate retained creation provenance immediately before its canonical
    /// publication. A successful earlier observation is not lasting authority.
    pub fn verify_creation(
        &self,
        file: &File,
        manifest: &NativeOriginManifest,
    ) -> Result<(), String> {
        manifest.validate()?;
        let ResourceTarget::LinuxFileOrigin {
            inode,
            allocated_bytes,
            ..
        } = &manifest.target
        else {
            return Err("native creation proof needs an existing file target".into());
        };
        verify_file(
            file,
            &self.mount,
            *inode,
            *allocated_bytes,
            self.owner_uid,
            1,
        )?;
        self.verify(&manifest.target)?;
        if read_creation(file)? != *manifest {
            return Err("native creation proof changed before publication".into());
        }
        Ok(())
    }

    /// Read creation provenance only from an existing private inode. Retain the
    /// serving lease and verify its exact current identity; no adoption or write.
    pub fn recover_creation(
        &self,
        request: &ResourceTarget,
    ) -> Result<(File, NativeOriginManifest), String> {
        let ResourceTarget::LinuxFileOriginRequest {
            managed_relative_path,
            allocated_bytes,
            ..
        } = request
        else {
            return Err("a native file-origin request is required".into());
        };
        self.revalidate()?;
        let file = self.named(managed_relative_path, OFlags::RDWR)?;
        let inode = descriptor_stat(&file)?.stx_ino;
        verify_file(
            &file,
            &self.mount,
            inode,
            *allocated_bytes,
            self.owner_uid,
            1,
        )?;
        rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)
            .map_err(|error| format!("native origin has an I/O owner: {error}"))?;
        let manifest = read_creation(&file)?;
        if !manifest.matches_request(request)
            || !matches!(&manifest.target, ResourceTarget::LinuxFileOrigin { inode: sealed, .. } if *sealed == inode)
        {
            return Err("native creation proof differs from the request or held inode".into());
        }
        self.verify_creation(&file, &manifest)?;
        Ok((file, manifest))
    }
}
