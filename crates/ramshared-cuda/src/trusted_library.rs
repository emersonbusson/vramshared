// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cuda/src/trusted_library.rs
//! Unix library-path authority. Aliases are discovery inputs, never loader inputs.

use std::collections::VecDeque;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

const MAX_PATH_BYTES: usize = 4096;
const MAX_COMPONENTS: usize = 256;
const MAX_ALIASES: usize = 40;

fn components(path: &Path) -> Result<VecDeque<std::ffi::OsString>, String> {
    if path.as_os_str().as_bytes().len() > MAX_PATH_BYTES {
        return Err("library path exceeds byte budget".into());
    }
    let parts: VecDeque<_> = path
        .components()
        .map(|part| part.as_os_str().to_os_string())
        .collect();
    if parts.len() > MAX_COMPONENTS {
        return Err("library path exceeds component budget".into());
    }
    Ok(parts)
}

fn check_authority(uid: u32, mode: u32, alias: bool) -> Result<(), String> {
    if uid != 0 {
        return Err("library path component is not owned by uid 0".into());
    }
    // Symlink mode bits do not control replacement; its already-verified
    // protected parent does. Only discovery may follow an owned alias.
    if !alias && mode & 0o022 != 0 {
        return Err("library path component is writable by group or other".into());
    }
    Ok(())
}

fn observe(path: &Path) -> Result<std::fs::Metadata, String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("cannot observe {}: {error}", path.display()))?;
    check_authority(metadata.uid(), metadata.mode(), metadata.is_symlink())?;
    Ok(metadata)
}

fn walk(path: &Path, allow_aliases: bool) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("library path must be absolute".into());
    }
    let mut pending = components(path)?;
    let mut current = PathBuf::from("/");
    let root = observe(&current)?;
    if !root.is_dir() {
        return Err("library root is not a directory".into());
    }
    let mut aliases = 0;
    let mut visits = 0;
    while let Some(part) = pending.pop_front() {
        visits += 1;
        if visits > MAX_COMPONENTS {
            return Err("library traversal exceeds component budget".into());
        }
        match Path::new(&part).components().next() {
            Some(Component::RootDir) => {
                current = PathBuf::from("/");
                continue;
            }
            Some(Component::ParentDir) => {
                current.pop();
                continue;
            }
            Some(Component::CurDir) => continue,
            _ => {}
        }
        current.push(part);
        if current.as_os_str().as_bytes().len() > MAX_PATH_BYTES {
            return Err("expanded library path exceeds byte budget".into());
        }
        let metadata = observe(&current)?;
        if metadata.is_symlink() {
            if !allow_aliases {
                return Err("library loader input contains a symlink".into());
            }
            aliases += 1;
            if aliases > MAX_ALIASES {
                return Err("library discovery exceeds alias budget".into());
            }
            let target = std::fs::read_link(&current)
                .map_err(|error| format!("cannot read driver alias: {error}"))?;
            let mut expansion = components(&target)?;
            expansion.append(&mut pending);
            if expansion.len() > MAX_COMPONENTS {
                return Err("library alias exceeds component budget".into());
            }
            pending = expansion;
            current.pop();
        } else if !pending.is_empty() && !metadata.is_dir() {
            return Err("library ancestor is not a directory".into());
        }
    }
    let metadata = observe(&current)?;
    if !metadata.is_file() || metadata.is_symlink() {
        return Err("library target is not a regular file".into());
    }
    let canonical = std::fs::canonicalize(&current)
        .map_err(|error| format!("cannot canonicalize library: {error}"))?;
    if canonical.as_os_str() != current.as_os_str() {
        return Err("library path changed during observation".into());
    }
    Ok(current)
}

/// Resolve only protected administrator-installed aliases during discovery.
pub(super) fn discover(path: &Path) -> Result<PathBuf, String> {
    walk(path, true)
}

/// Validate the final physical path without following any symlink component.
pub(super) fn validate(path: &Path) -> Result<(), String> {
    let canonical = walk(path, false)?;
    if canonical.as_os_str() != path.as_os_str() {
        return Err("library loader input is not canonical".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
