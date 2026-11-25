// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-exec/src/inherited_lease.rs
//! A protected named lease whose open-file description survives child handoff.
use rustix::fs::{FlockOperation, Mode, OFlags, ResolveFlags, flock, openat2};
use std::fs::{File, Metadata};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};

fn refusal(detail: &str) -> io::Error {
    io::Error::other(detail)
}

fn hierarchy(path: &Path, owner: u32) -> io::Result<()> {
    if !path.is_absolute()
        || path.file_name().is_none()
        || path.components().any(|c| {
            matches!(
                c,
                Component::CurDir | Component::ParentDir | Component::Prefix(_)
            )
        })
    {
        return Err(refusal("lease path must be absolute and normalized"));
    }
    for parent in path
        .parent()
        .ok_or_else(|| refusal("lease parent absent"))?
        .ancestors()
    {
        let metadata = std::fs::symlink_metadata(parent)?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || ![0, owner].contains(&metadata.uid())
            || metadata.mode() & 0o022 != 0
        {
            return Err(refusal(
                "lease hierarchy is not protected by its native owner",
            ));
        }
    }
    Ok(())
}
fn sealed(metadata: &Metadata, owner: u32) -> bool {
    metadata.is_file()
        && metadata.uid() == owner
        && metadata.nlink() == 1
        && metadata.mode() & 0o7777 == 0o600
        && metadata.len() == 0
}
fn matches(file: &File, path: &Path, owner: u32) -> io::Result<()> {
    let held = file.metadata()?;
    let named = std::fs::symlink_metadata(path)?;
    if !sealed(&held, owner)
        || !sealed(&named, owner)
        || held.dev() != named.dev()
        || held.ino() != named.ino()
    {
        return Err(refusal(
            "lease name no longer identifies the sealed held inode",
        ));
    }
    Ok(())
}

/// Never unlink this lease. A child may retain the same description after its
/// parent dies; a new process must contend on that inode until actual release.
pub fn acquire(path: &Path, owner: u32) -> io::Result<File> {
    hierarchy(path, owner)?;
    let fd = openat2(
        rustix::fs::CWD,
        path,
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::RUSR | Mode::WUSR,
        ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
    )?;
    let file = File::from(fd);
    validate(&file, path, owner)?;
    Ok(file)
}

/// Parent checks immediately before handoff; child checks its inherited fd.
/// flock on the inherited open-file description preserves its exclusive lock.
/// An error never consumes or closes the caller's descriptor.
pub fn validate(file: &File, path: &Path, owner: u32) -> io::Result<()> {
    hierarchy(path, owner)?;
    matches(file, path, owner)?;
    flock(file, FlockOperation::NonBlockingLockExclusive)?;
    matches(file, path, owner)
}

#[cfg(test)]
mod tests;
