// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/installed_image/release_tree.rs
//! Retained directory descriptors for one installed-release observation.

use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{AtFlags, FileType, Mode, OFlags, fstat, open, openat, readlinkat, statat};

use crate::release_identity::{
    MAX_IDENTITY_LINE_BYTES, open_regular_at, parse_identity_line, read_file_bytes,
};

use super::binary_hash::sha256_reader;

/// Pins the selected root, its selector entry, and the actual `bin` directory
/// for one observation. The final selector may be the normal `current` link;
/// its parent hierarchy and target path may not follow links.
pub(super) struct ReleaseTree {
    root: File,
    bin: File,
    selection: Selection,
}

impl ReleaseTree {
    pub(super) fn open(path: &Path) -> Option<Self> {
        let (selection, root) = Selection::open(path)?;
        let directory_flags = OFlags::RDONLY
            | OFlags::DIRECTORY
            | OFlags::CLOEXEC
            | OFlags::NONBLOCK
            | OFlags::NOFOLLOW;
        let bin = openat(&root, "bin", directory_flags, Mode::empty()).ok()?;
        Some(Self {
            root,
            bin: File::from(bin),
            selection,
        })
    }

    /// Refuse an observation when any parent, selector, target root, or `bin`
    /// directory has moved since this tree was opened.
    pub(super) fn matches_selected(&self, path: &Path) -> bool {
        let Some(selected) = Self::open(path) else {
            return false;
        };
        self.selection.same_entry(&selected.selection)
            && same_directory(&self.root, &selected.root)
            && same_directory(&self.bin, &selected.bin)
    }

    fn member(&self, path: &str) -> Option<File> {
        if let Some(name) = path.strip_prefix("bin/") {
            open_regular_at(&self.bin, name)
        } else {
            open_regular_at(&self.root, path)
        }
    }

    pub(super) fn line(&self, path: &str) -> Option<String> {
        parse_identity_line(&read_file_bytes(
            self.member(path)?,
            MAX_IDENTITY_LINE_BYTES,
        )?)
    }

    pub(super) fn verified_bytes(
        &self,
        path: &str,
        expected: &str,
        limit: usize,
    ) -> Option<Vec<u8>> {
        let bytes = read_file_bytes(self.member(path)?, limit)?;
        if sha256_reader(&mut bytes.as_slice()).as_deref() != Some(expected) {
            return None;
        }
        Some(bytes)
    }

    pub(super) fn sha256(&self, path: &str) -> Option<String> {
        sha256_reader(&mut self.member(path)?)
    }
}

struct Selection {
    path: PathBuf,
    parent: File,
    parent_identity: FileIdentity,
    leaf: OsString,
    entry: File,
    entry_identity: FileIdentity,
    target: Option<Vec<u8>>,
}

impl Selection {
    fn open(path: &Path) -> Option<(Self, File)> {
        if !path.is_absolute() {
            return None;
        }
        let parent_path = path.parent()?;
        let leaf = path.file_name()?.to_os_string();
        if matches!(leaf.to_str(), Some("." | "..")) {
            return None;
        }

        let parent = open_directory_no_follow(parent_path)?;
        let parent_identity = directory_identity(&parent)?;
        let stat = statat(&parent, &leaf, AtFlags::SYMLINK_NOFOLLOW).ok()?;
        let entry_identity = FileIdentity::from_stat(&stat);
        let entry_type = FileType::from_raw_mode(stat.st_mode);
        let entry = File::from(
            openat(
                &parent,
                &leaf,
                OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .ok()?,
        );
        let held = fstat(&entry).ok()?;
        if FileIdentity::from_stat(&held) != entry_identity
            || FileType::from_raw_mode(held.st_mode) != entry_type
        {
            return None;
        }

        let (root, target) = match entry_type {
            FileType::Directory => {
                let root = openat(&parent, &leaf, directory_flags(), Mode::empty()).ok()?;
                let root = File::from(root);
                if directory_identity(&root)? != entry_identity {
                    return None;
                }
                (root, None)
            }
            FileType::Symlink => {
                let target = read_selector_target(&parent, &leaf)?;
                let root = open_relative_directory_no_follow(&parent, &target)?;
                let current = statat(&parent, &leaf, AtFlags::SYMLINK_NOFOLLOW).ok()?;
                if FileIdentity::from_stat(&current) != entry_identity
                    || FileType::from_raw_mode(current.st_mode) != FileType::Symlink
                    || read_selector_target(&parent, &leaf).as_deref() != Some(target.as_slice())
                {
                    return None;
                }
                (root, Some(target))
            }
            _ => return None,
        };

        Some((
            Self {
                path: path.to_path_buf(),
                parent,
                parent_identity,
                leaf,
                entry,
                entry_identity,
                target,
            },
            root,
        ))
    }

    fn same_entry(&self, other: &Self) -> bool {
        self.path == other.path
            && self.parent_identity == other.parent_identity
            && same_directory(&self.parent, &other.parent)
            && self.leaf == other.leaf
            && fstat(&self.entry)
                .ok()
                .map(|stat| FileIdentity::from_stat(&stat))
                == Some(self.entry_identity)
            && fstat(&other.entry)
                .ok()
                .map(|stat| FileIdentity::from_stat(&stat))
                == Some(other.entry_identity)
            && self.entry_identity == other.entry_identity
            && self.target == other.target
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
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

fn directory_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW
}

fn directory_identity(directory: &File) -> Option<FileIdentity> {
    let stat = fstat(directory).ok()?;
    (FileType::from_raw_mode(stat.st_mode) == FileType::Directory)
        .then(|| FileIdentity::from_stat(&stat))
}

/// Open every parent component without following a link. The selector itself
/// is opened separately so a single, deliberate final `current` link can be
/// pinned and checked by identity.
fn open_directory_no_follow(path: &Path) -> Option<File> {
    if !path.is_absolute() {
        return None;
    }
    let root = open("/", directory_flags(), Mode::empty()).ok()?;
    let mut directory = File::from(root);
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                directory =
                    File::from(openat(&directory, name, directory_flags(), Mode::empty()).ok()?);
            }
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => return None,
        }
    }
    Some(directory)
}

fn read_selector_target(parent: &File, leaf: &OsStr) -> Option<Vec<u8>> {
    let target: CString = readlinkat(parent, leaf, Vec::new()).ok()?;
    let bytes = target.into_bytes();
    if bytes.is_empty() || bytes.len() > 4096 {
        return None;
    }
    let text = std::str::from_utf8(&bytes).ok()?;
    if text.starts_with('/')
        || text
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
        || text.chars().any(char::is_control)
    {
        return None;
    }
    Some(bytes)
}

fn open_relative_directory_no_follow(parent: &File, target: &[u8]) -> Option<File> {
    let target = std::str::from_utf8(target).ok()?;
    let mut directory = parent.try_clone().ok()?;
    for component in target.split('/') {
        directory =
            File::from(openat(&directory, component, directory_flags(), Mode::empty()).ok()?);
    }
    Some(directory)
}

fn same_directory(first: &File, second: &File) -> bool {
    match (directory_identity(first), directory_identity(second)) {
        (Some(first), Some(second)) => first == second,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn selector_descriptor_retains_the_original_link_after_replacement() {
        let root =
            std::env::temp_dir().join(format!("ramshared-selector-held-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join("release")).unwrap();
        symlink("release", root.join("current")).unwrap();
        let (selection, _) = Selection::open(&root.join("current")).unwrap();
        let original = FileIdentity::from_stat(&fstat(&selection.entry).unwrap());
        std::fs::remove_file(root.join("current")).unwrap();
        symlink("release", root.join("current")).unwrap();
        let (replacement, _) = Selection::open(&root.join("current")).unwrap();
        assert_eq!(
            FileIdentity::from_stat(&fstat(&selection.entry).unwrap()),
            original
        );
        assert_ne!(selection.entry_identity, replacement.entry_identity);
        assert!(!selection.same_entry(&replacement));
        std::fs::remove_dir_all(root).unwrap();
    }
}
