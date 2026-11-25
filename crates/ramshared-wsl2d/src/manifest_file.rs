// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/manifest_file.rs
//! Descriptor-bound, bounded manifest reads for origin admission.

use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

const ORIGIN_MANIFEST_MAX_BYTES: u64 = 64 * 1024;

#[cfg(test)]
use tests::invoke_manifest_read_hook;

#[cfg(not(test))]
fn invoke_manifest_read_hook(_path: &Path) {}

pub(super) fn read_bounded_manifest_file(
    path: &Path,
    require_root_seal: bool,
) -> Result<Vec<u8>, String> {
    let opened = rustix::fs::openat2(
        rustix::fs::CWD,
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
        rustix::fs::ResolveFlags::NO_SYMLINKS | rustix::fs::ResolveFlags::NO_MAGICLINKS,
    )
    .map_err(|error| format!("open manifest without following links: {error}"))?;
    let mut file = File::from(opened);
    let before = file
        .metadata()
        .map_err(|error| format!("inspect opened manifest: {error}"))?;
    let named_before = std::fs::symlink_metadata(path)
        .map_err(|error| format!("inspect named manifest: {error}"))?;
    let unsafe_metadata = !before.file_type().is_file()
        || before.nlink() != 1
        || before.len() == 0
        || before.len() > ORIGIN_MANIFEST_MAX_BYTES
        || !named_before.file_type().is_file()
        || named_before.file_type().is_symlink()
        || !same_snapshot(&before, &named_before)
        || require_root_seal && (before.uid() != 0 || before.mode() & 0o022 != 0);
    if unsafe_metadata {
        return Err("manifest is not a bounded, singly linked sealed regular file".into());
    }

    invoke_manifest_read_hook(path);
    let mut bytes = Vec::new();
    (&mut file)
        .take(ORIGIN_MANIFEST_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read bounded manifest: {error}"))?;
    if bytes.len() as u64 > ORIGIN_MANIFEST_MAX_BYTES {
        return Err("manifest exceeds its bounded read limit".into());
    }

    let after = file
        .metadata()
        .map_err(|error| format!("reinspect opened manifest: {error}"))?;
    let named_after = std::fs::symlink_metadata(path)
        .map_err(|error| format!("reinspect named manifest: {error}"))?;
    if !after.file_type().is_file()
        || !named_after.file_type().is_file()
        || named_after.file_type().is_symlink()
        || !same_snapshot(&before, &after)
        || !same_snapshot(&before, &named_after)
        || bytes.len() as u64 != before.len()
    {
        return Err("manifest identity or metadata changed during bounded read".into());
    }
    Ok(bytes)
}

fn same_snapshot(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.len() == right.len()
        && left.nlink() == right.nlink()
        && left.uid() == right.uid()
        && left.mode() == right.mode()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

#[cfg(test)]
mod tests;
