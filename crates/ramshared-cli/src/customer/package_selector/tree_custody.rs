// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_selector/tree_custody.rs
//! Bounded, continuously revalidatable tree custody for selector operations.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use rustix::fs::{Mode, OFlags, openat};

use super::super::CustomerError;
use super::super::package_prepare::PreparedRelease;
use super::super::package_publish::inventory::{inspect_expected_tree, mount_id, verify_all};
use super::super::package_source::DeclaredBundle;
use super::super::package_tree::revalidate_owned_tree;
use super::super::transition::{CustomerTransitionGuard, PackagePending, recovery_refusal};
use super::super::transition_custody::{
    open_directory_no_follow, validate_directory, validate_regular,
};
pub(in crate::customer) use super::descriptor_budget::DescriptorBudget;
#[cfg(test)]
use super::descriptor_budget::{MAX_HELD_TREE_FDS, RESERVED_HEADROOM_FDS, open_fd_count};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DirSnapshot {
    dev: u64,
    ino: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    mtime_sec: i64,
    mtime_nsec: i64,
    ctime_sec: i64,
    ctime_nsec: i64,
    mount_id: u64,
}

impl DirSnapshot {
    fn capture(dir: &File, label: &str) -> Result<Self, CustomerError> {
        validate_directory(dir, label)?;
        let meta = dir
            .metadata()
            .map_err(|_| recovery_refusal(format!("{label} metadata unavailable")))?;
        if !meta.is_dir() {
            return Err(recovery_refusal(format!("{label} is not a directory")));
        }
        Ok(Self {
            dev: meta.dev(),
            ino: meta.ino(),
            mode: meta.mode() & 0o7777,
            uid: meta.uid(),
            gid: meta.gid(),
            mtime_sec: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
            ctime_sec: meta.ctime(),
            ctime_nsec: meta.ctime_nsec(),
            mount_id: mount_id(dir)?,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileSnapshot {
    dev: u64,
    ino: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    nlink: u64,
    size: u64,
    mtime_sec: i64,
    mtime_nsec: i64,
    ctime_sec: i64,
    ctime_nsec: i64,
    mount_id: u64,
}

impl FileSnapshot {
    fn capture(file: &File, label: &str) -> Result<Self, CustomerError> {
        validate_regular(file, label)?;
        let meta = file
            .metadata()
            .map_err(|_| recovery_refusal(format!("{label} metadata unavailable")))?;
        if !meta.is_file() || meta.nlink() != 1 {
            return Err(recovery_refusal(format!("{label} link or type changed")));
        }
        Ok(Self {
            dev: meta.dev(),
            ino: meta.ino(),
            mode: meta.mode() & 0o7777,
            uid: meta.uid(),
            gid: meta.gid(),
            nlink: meta.nlink(),
            size: meta.len(),
            mtime_sec: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
            ctime_sec: meta.ctime(),
            ctime_nsec: meta.ctime_nsec(),
            mount_id: mount_id(file)?,
        })
    }
}

struct HeldDir {
    fd: File,
    snapshot: DirSnapshot,
}

struct HeldFile {
    fd: File,
    parent_rel: PathBuf,
    leaf: OsString,
    expected_sha256: String,
    snapshot: FileSnapshot,
}

/// Held descriptors and immutable metadata/digest baselines across a release tree.
pub(in crate::customer) struct TreeCustody {
    label: &'static str,
    root: File,
    root_snapshot: DirSnapshot,
    dirs: BTreeMap<PathBuf, HeldDir>,
    files: BTreeMap<String, HeldFile>,
    expected_dirs: BTreeSet<PathBuf>,
    expected_files: BTreeSet<PathBuf>,
}

impl TreeCustody {
    /// Capture bounded continuous custody over a prepared release tree.
    pub(in crate::customer) fn capture_prepared(
        guard: &CustomerTransitionGuard,
        pending: &PackagePending,
        bundle: &DeclaredBundle,
        release: &PreparedRelease,
        budget: &mut DescriptorBudget,
    ) -> Result<Self, CustomerError> {
        revalidate_owned_tree(guard, pending, bundle, &release.tree, false)?;
        let roles = bundle
            .stage_roles()
            .ok_or_else(|| recovery_refusal("prepared release roles are unavailable"))?;
        let mut role_items = BTreeMap::new();
        let mut expected_files = BTreeSet::new();
        for role in roles {
            let (destination, mode, digest) = bundle
                .publication_member(&role)
                .ok_or_else(|| recovery_refusal("prepared release member missing"))?;
            if !expected_files.insert(PathBuf::from(&destination))
                || role_items
                    .insert(role, (destination, mode & 0o7777, digest))
                    .is_some()
            {
                return Err(recovery_refusal("prepared release destination repeats"));
            }
        }
        let expected_dirs = super::previous_inventory::expected_dirs(&expected_files)
            .ok_or_else(|| recovery_refusal("prepared release paths are unsafe"))?;
        let needed = 1usize
            .checked_add(expected_dirs.len())
            .and_then(|n| n.checked_add(expected_files.len()))
            .ok_or_else(|| recovery_refusal("prepared release entry count overflow"))?;
        budget.reserve(needed)?;

        let mut verified = verify_all(
            guard,
            pending,
            bundle,
            release.tree.path(),
            release.tree.identity(),
        )?;
        if verified.len() != role_items.len() {
            return Err(recovery_refusal("prepared release member count changed"));
        }

        let root = open_directory_no_follow(release.tree.path(), false)?
            .ok_or_else(|| recovery_refusal("prepared release root disappeared"))?;
        let root_snapshot = DirSnapshot::capture(&root, "prepared release root")?;
        if (root_snapshot.dev, root_snapshot.ino) != release.tree.identity() {
            return Err(recovery_refusal("prepared release root identity changed"));
        }
        let dirs = capture_dirs(&root, root_snapshot.mount_id, &expected_dirs, "prepared")?;

        let mut files = BTreeMap::new();
        for (role, (destination, expected_mode, expected_digest)) in role_items {
            let fd = verified
                .remove(&role)
                .ok_or_else(|| recovery_refusal("prepared member handle absent"))?;
            if let Some(orig) = release.members.get(&role) {
                let orig_meta = orig
                    .metadata()
                    .map_err(|_| recovery_refusal("prepared original handle unavailable"))?;
                let cur_meta = fd
                    .metadata()
                    .map_err(|_| recovery_refusal("prepared handle unavailable"))?;
                if (orig_meta.dev(), orig_meta.ino()) != (cur_meta.dev(), cur_meta.ino()) {
                    return Err(recovery_refusal("prepared member inode changed"));
                }
            }
            let held = build_held_file(
                &root,
                &dirs,
                root_snapshot.mount_id,
                &destination,
                fd,
                Some(expected_mode),
                Some(&expected_digest),
                "prepared release member",
            )?;
            if files.insert(destination, held).is_some() {
                return Err(recovery_refusal("prepared member destination repeats"));
            }
        }
        inspect_expected_tree(
            &root,
            &expected_dirs,
            &expected_files,
            (root_snapshot.dev, root_snapshot.ino),
        )?;
        Ok(Self {
            label: "prepared release",
            root,
            root_snapshot,
            dirs,
            files,
            expected_dirs,
            expected_files,
        })
    }

    /// Capture bounded continuous custody over an authenticated prior release tree.
    pub(in crate::customer) fn capture_previous(
        root_dir: &File,
        manifest: &[u8],
        limits: (u64, u64),
        transformation: Option<&super::PreviousTransformation<'_>>,
        budget: &mut DescriptorBudget,
    ) -> Result<Self, CustomerError> {
        let signed = super::previous_members::parse_signed(manifest)
            .ok_or_else(|| recovery_refusal("previous signed manifest is malformed"))?;
        let mut expected_files: BTreeSet<PathBuf> = signed.keys().map(PathBuf::from).collect();
        for receipt in super::previous_inventory::RECEIPTS {
            if !expected_files.insert(PathBuf::from(receipt)) {
                return Err(recovery_refusal(
                    "previous manifest overlaps required receipts",
                ));
            }
        }
        let expected_dirs = super::previous_inventory::expected_dirs(&expected_files)
            .ok_or_else(|| recovery_refusal("previous release paths are unsafe"))?;
        let needed = 1usize
            .checked_add(expected_dirs.len())
            .and_then(|n| n.checked_add(expected_files.len()))
            .ok_or_else(|| recovery_refusal("previous release entry count overflow"))?;
        budget.reserve(needed)?;

        let mut collected =
            super::previous_members::verify_and_collect(root_dir, manifest, limits, transformation)
                .ok_or_else(|| recovery_refusal("previous signed members do not match"))?;

        let root = open_root_clone(root_dir)?;
        let root_snapshot = DirSnapshot::capture(&root, "previous release root")?;
        let dirs = capture_dirs(&root, root_snapshot.mount_id, &expected_dirs, "previous")?;

        let mut files = BTreeMap::new();
        for (path, (fd, digest)) in std::mem::take(&mut collected) {
            let held = build_held_file(
                &root,
                &dirs,
                root_snapshot.mount_id,
                &path,
                fd,
                None,
                Some(&digest),
                "previous release member",
            )?;
            files.insert(path, held);
        }

        let mut manifest_slice = manifest;
        let input_manifest_digest = crate::installed_image::sha256_reader(&mut manifest_slice)
            .ok_or_else(|| recovery_refusal("previous manifest hash unavailable"))?;
        let receipt_limit =
            u64::try_from(crate::release_identity::MAX_SEALED_TEXT_BYTES).unwrap_or(1024 * 1024);
        for receipt in super::previous_inventory::RECEIPTS {
            let parent_fd = parent_dir_for(&root, &dirs, Path::new(""))?;
            let fd = open_regular_at_dir(parent_fd, OsStr::new(receipt), "previous receipt")?;
            let size = fd
                .metadata()
                .map_err(|_| recovery_refusal("previous receipt unavailable"))?
                .len();
            if size > receipt_limit {
                return Err(recovery_refusal("previous receipt exceeds size limit"));
            }
            let expected =
                (receipt == "INPUT_BUNDLE_SHA256SUMS").then_some(input_manifest_digest.as_str());
            let held = build_held_file(
                &root,
                &dirs,
                root_snapshot.mount_id,
                receipt,
                fd,
                None,
                expected,
                "previous release receipt",
            )?;
            files.insert(receipt.to_owned(), held);
        }

        inspect_expected_tree(
            &root,
            &expected_dirs,
            &expected_files,
            (root_snapshot.dev, root_snapshot.ino),
        )?;
        Ok(Self {
            label: "previous release",
            root,
            root_snapshot,
            dirs,
            files,
            expected_dirs,
            expected_files,
        })
    }

    /// Revalidate every held directory, member descriptor, byte digest, and exact layout.
    pub(in crate::customer) fn revalidate(&self) -> Result<(), CustomerError> {
        if DirSnapshot::capture(&self.root, self.label)? != self.root_snapshot {
            return Err(recovery_refusal(format!("{} root changed", self.label)));
        }
        for (rel, held) in &self.dirs {
            if DirSnapshot::capture(&held.fd, self.label)? != held.snapshot {
                return Err(recovery_refusal(format!(
                    "{} directory modified in place: {}",
                    self.label,
                    rel.display()
                )));
            }
            let parent_rel = rel.parent().unwrap_or_else(|| Path::new(""));
            let leaf = rel
                .file_name()
                .ok_or_else(|| recovery_refusal("directory name missing"))?;
            let parent_fd = parent_dir_for(&self.root, &self.dirs, parent_rel)?;
            let probe = open_dir_at(parent_fd, leaf, self.label)?;
            if DirSnapshot::capture(&probe, self.label)? != held.snapshot {
                return Err(recovery_refusal(format!(
                    "{} directory replaced: {}",
                    self.label,
                    rel.display()
                )));
            }
        }
        for (path, held) in &self.files {
            if FileSnapshot::capture(&held.fd, self.label)? != held.snapshot {
                return Err(recovery_refusal(format!(
                    "{} file modified: {path}",
                    self.label
                )));
            }
            let parent_fd = parent_dir_for(&self.root, &self.dirs, &held.parent_rel)?;
            let probe = open_regular_at_dir(parent_fd, &held.leaf, self.label)?;
            if FileSnapshot::capture(&probe, self.label)? != held.snapshot {
                return Err(recovery_refusal(format!(
                    "{} file replaced: {path}",
                    self.label
                )));
            }
            verify_file_digest(&held.fd, held.snapshot, Some(&held.expected_sha256), path)?;
        }
        inspect_expected_tree(
            &self.root,
            &self.expected_dirs,
            &self.expected_files,
            (self.root_snapshot.dev, self.root_snapshot.ino),
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub(in crate::customer) fn held_fd_count(&self) -> usize {
        1 + self.dirs.len() + self.files.len()
    }

    pub(in crate::customer) fn root_identity(&self) -> (u64, u64) {
        (self.root_snapshot.dev, self.root_snapshot.ino)
    }

    pub(in crate::customer) fn subdir_file(&self, rel: &Path) -> Option<&File> {
        if rel.as_os_str().is_empty() {
            Some(&self.root)
        } else {
            self.dirs.get(rel).map(|held| &held.fd)
        }
    }

    pub(in crate::customer) fn member_file(&self, path: &str) -> Option<&File> {
        self.files.get(path).map(|held| &held.fd)
    }

    #[cfg(test)]
    pub(in crate::customer) fn expected_sha256(&self, path: &str) -> Option<&str> {
        self.files
            .get(path)
            .map(|held| held.expected_sha256.as_str())
    }

    /// Capture bounded continuous custody over a finalized `0555`/`0444`-sealed release.
    pub(in crate::customer) fn capture_sealed(
        root_path: &Path,
        expected_identity: (u64, u64),
        installed_manifest_sha256: &str,
        input_manifest_sha256: &str,
        budget: &mut DescriptorBudget,
    ) -> Result<Self, CustomerError> {
        let root = open_directory_no_follow(root_path, false)?
            .ok_or_else(|| recovery_refusal("sealed release root is absent"))?;
        let root_snapshot = DirSnapshot::capture(&root, "sealed release root")?;
        if (root_snapshot.dev, root_snapshot.ino) != expected_identity
            || !is_sealed_dir(root_snapshot)
        {
            return Err(recovery_refusal(
                "sealed release root identity or mode is invalid",
            ));
        }
        let signed = read_sealed_manifest(&root, root_path, installed_manifest_sha256)?;
        let mut expected_files: BTreeSet<PathBuf> = signed
            .keys()
            .map(|path| PathBuf::from(path.as_str()))
            .collect();
        for meta_file in [
            crate::release_identity::SHA256SUMS_FILE,
            "INSTALLED_MANIFEST_SHA256",
        ] {
            if !expected_files.insert(PathBuf::from(meta_file)) {
                return Err(recovery_refusal("sealed manifest lists receipt file"));
            }
        }
        let expected_dirs = super::previous_inventory::expected_dirs(&expected_files)
            .ok_or_else(|| recovery_refusal("sealed release paths are unsafe"))?;
        let needed = 1usize
            .checked_add(expected_dirs.len())
            .and_then(|n| n.checked_add(expected_files.len()))
            .ok_or_else(|| recovery_refusal("sealed release entry count overflow"))?;
        budget.reserve(needed)?;

        let dirs = capture_dirs(&root, root_snapshot.mount_id, &expected_dirs, "sealed")?;
        if dirs.values().any(|held| !is_sealed_dir(held.snapshot)) {
            return Err(recovery_refusal(
                "sealed directory mode or owner is invalid",
            ));
        }
        let files = collect_sealed_files(
            &root,
            &dirs,
            root_snapshot.mount_id,
            &signed,
            installed_manifest_sha256,
        )?;
        inspect_expected_tree(
            &root,
            &expected_dirs,
            &expected_files,
            (root_snapshot.dev, root_snapshot.ino),
        )?;
        if crate::installed_image::verify_release_manifest(root_path).is_none()
            || crate::installed_image::observe_installed_input_bundle_digest(root_path).as_deref()
                != Some(input_manifest_sha256)
        {
            return Err(recovery_refusal(
                "sealed release manifest verification failed",
            ));
        }
        Ok(Self {
            label: "sealed release",
            root,
            root_snapshot,
            dirs,
            files,
            expected_dirs,
            expected_files,
        })
    }
}

fn read_sealed_manifest(
    root: &File,
    root_path: &Path,
    installed_manifest_sha256: &str,
) -> Result<BTreeMap<String, String>, CustomerError> {
    let receipt =
        crate::release_identity::read_regular_line(&root_path.join("INSTALLED_MANIFEST_SHA256"))
            .filter(|line| line == installed_manifest_sha256)
            .ok_or_else(|| recovery_refusal("installed manifest receipt does not match"))?;
    let sha_fd = open_regular_at_dir(
        root,
        OsStr::new(crate::release_identity::SHA256SUMS_FILE),
        "sealed manifest",
    )?;
    let sha_snap = FileSnapshot::capture(&sha_fd, "sealed manifest")?;
    let max_bytes =
        u64::try_from(crate::release_identity::MAX_SEALED_TEXT_BYTES).unwrap_or(1024 * 1024);
    if sha_snap.size > max_bytes {
        return Err(recovery_refusal("sealed manifest exceeds size bound"));
    }
    let bytes = read_bounded_fd(&sha_fd, sha_snap.size)?;
    let mut slice = bytes.as_slice();
    if crate::installed_image::sha256_reader(&mut slice).as_deref() != Some(receipt.as_str()) {
        return Err(recovery_refusal("sealed manifest digest mismatch"));
    }
    let parsed = super::previous_members::parse_signed(&bytes)
        .ok_or_else(|| recovery_refusal("sealed manifest is malformed"))?;
    for required in ["bin/ramshared", "bin/ramsharedd", "INPUT_BUNDLE_SHA256SUMS"] {
        if !parsed.contains_key(required) {
            return Err(recovery_refusal("sealed manifest omits required member"));
        }
    }
    for required in crate::release_identity::SEALED_IDENTITY_FILES {
        if !parsed.contains_key(required) {
            return Err(recovery_refusal("sealed manifest omits identity file"));
        }
    }
    Ok(parsed
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect())
}

fn collect_sealed_files(
    root: &File,
    dirs: &BTreeMap<PathBuf, HeldDir>,
    root_mount: u64,
    signed: &BTreeMap<String, String>,
    installed_manifest_sha256: &str,
) -> Result<BTreeMap<String, HeldFile>, CustomerError> {
    let mut files = BTreeMap::new();
    for (rel, digest) in signed {
        let path = Path::new(rel);
        let parent_rel = path.parent().unwrap_or_else(|| Path::new(""));
        let leaf = path
            .file_name()
            .ok_or_else(|| recovery_refusal("sealed member name missing"))?;
        let parent_fd = parent_dir_for(root, dirs, parent_rel)?;
        let fd = open_regular_at_dir(parent_fd, leaf, "sealed member")?;
        let held = build_held_file(
            root,
            dirs,
            root_mount,
            rel,
            fd,
            None,
            Some(digest),
            "sealed member",
        )?;
        if !is_sealed_file(held.snapshot) {
            return Err(recovery_refusal(format!("sealed file mode invalid: {rel}")));
        }
        files.insert(rel.clone(), held);
    }
    for (meta_rel, digest) in [
        (
            crate::release_identity::SHA256SUMS_FILE,
            Some(installed_manifest_sha256),
        ),
        ("INSTALLED_MANIFEST_SHA256", None),
    ] {
        let fd = open_regular_at_dir(root, OsStr::new(meta_rel), "sealed receipt")?;
        let held = build_held_file(
            root,
            dirs,
            root_mount,
            meta_rel,
            fd,
            Some(0o444),
            digest,
            "sealed receipt",
        )?;
        if !is_sealed_file(held.snapshot) {
            return Err(recovery_refusal("sealed receipt owner invalid"));
        }
        files.insert(meta_rel.to_owned(), held);
    }
    Ok(files)
}

fn read_bounded_fd(file: &File, size: u64) -> Result<Vec<u8>, CustomerError> {
    use std::io::Read;
    let len = usize::try_from(size)
        .map_err(|_| recovery_refusal("sealed file length overflows usize"))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len)
        .map_err(|_| recovery_refusal("cannot allocate buffer for sealed file"))?;
    bytes.resize(len, 0);
    let mut reader = file;
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|_| recovery_refusal("cannot rewind sealed file"))?;
    reader
        .read_exact(&mut bytes)
        .map_err(|_| recovery_refusal("cannot read sealed file"))?;
    Ok(bytes)
}

fn owner_matches(uid: u32, gid: u32) -> bool {
    let euid = rustix::process::geteuid().as_raw();
    if euid == 0 {
        uid == 0 && gid == 0
    } else {
        uid == euid
    }
}

fn is_sealed_dir(snap: DirSnapshot) -> bool {
    owner_matches(snap.uid, snap.gid) && snap.mode == 0o555
}

fn is_sealed_file(snap: FileSnapshot) -> bool {
    let expected = if snap.mode & 0o111 != 0 { 0o555 } else { 0o444 };
    owner_matches(snap.uid, snap.gid) && snap.mode == expected
}

fn capture_dirs(
    root: &File,
    root_mount: u64,
    expected_dirs: &BTreeSet<PathBuf>,
    label: &str,
) -> Result<BTreeMap<PathBuf, HeldDir>, CustomerError> {
    let mut dirs: BTreeMap<PathBuf, HeldDir> = BTreeMap::new();
    for rel in expected_dirs {
        let parent_rel = rel.parent().unwrap_or_else(|| Path::new(""));
        let leaf = rel
            .file_name()
            .ok_or_else(|| recovery_refusal("subdirectory name is invalid"))?;
        let parent_fd = parent_dir_for(root, &dirs, parent_rel)?;
        let fd = open_dir_at(parent_fd, leaf, label)?;
        let snapshot = DirSnapshot::capture(&fd, label)?;
        if snapshot.mount_id != root_mount {
            return Err(recovery_refusal(format!(
                "{label} directory crosses a mount boundary"
            )));
        }
        dirs.insert(rel.clone(), HeldDir { fd, snapshot });
    }
    Ok(dirs)
}

#[allow(clippy::too_many_arguments)]
fn build_held_file(
    root: &File,
    dirs: &BTreeMap<PathBuf, HeldDir>,
    root_mount: u64,
    rel: &str,
    fd: File,
    expected_mode: Option<u32>,
    expected_sha256: Option<&str>,
    label: &str,
) -> Result<HeldFile, CustomerError> {
    let snapshot = FileSnapshot::capture(&fd, label)?;
    if snapshot.mount_id != root_mount {
        return Err(recovery_refusal(format!(
            "{label} crosses a mount boundary: {rel}"
        )));
    }
    if expected_mode.is_some_and(|mode| snapshot.mode != mode) {
        return Err(recovery_refusal(format!("{label} mode changed: {rel}")));
    }
    let path = Path::new(rel);
    let parent_rel = path.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
    let leaf = path
        .file_name()
        .ok_or_else(|| recovery_refusal("member filename missing"))?
        .to_os_string();
    let parent_fd = parent_dir_for(root, dirs, &parent_rel)?;
    let probe = open_regular_at_dir(parent_fd, &leaf, label)?;
    if FileSnapshot::capture(&probe, label)? != snapshot {
        return Err(recovery_refusal(format!(
            "{label} changed during custody capture: {rel}"
        )));
    }
    let digest = verify_file_digest(&fd, snapshot, expected_sha256, rel)?;
    Ok(HeldFile {
        fd,
        parent_rel,
        leaf,
        expected_sha256: digest,
        snapshot,
    })
}

fn verify_file_digest(
    file: &File,
    snapshot: FileSnapshot,
    expected: Option<&str>,
    label: &str,
) -> Result<String, CustomerError> {
    validate_regular(file, label)?;
    if FileSnapshot::capture(file, label)? != snapshot {
        return Err(recovery_refusal(format!("{label} metadata changed")));
    }
    let mut reader = file;
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|_| recovery_refusal(format!("{label} is not seekable")))?;
    let observed = crate::installed_image::sha256_reader_with_limit(&mut reader, snapshot.size)
        .ok_or_else(|| recovery_refusal(format!("{label} digest check failed")))?;
    if expected.is_some_and(|want| want != observed) {
        return Err(recovery_refusal(format!("{label} content digest changed")));
    }
    if FileSnapshot::capture(file, label)? != snapshot {
        return Err(recovery_refusal(format!(
            "{label} mutated during verification"
        )));
    }
    Ok(observed)
}

fn parent_dir_for<'a>(
    root: &'a File,
    dirs: &'a BTreeMap<PathBuf, HeldDir>,
    parent_rel: &Path,
) -> Result<&'a File, CustomerError> {
    if parent_rel.as_os_str().is_empty() {
        Ok(root)
    } else {
        dirs.get(parent_rel)
            .map(|held| &held.fd)
            .ok_or_else(|| recovery_refusal("held parent directory is absent"))
    }
}

fn open_root_clone(root_dir: &File) -> Result<File, CustomerError> {
    let cloned = root_dir
        .try_clone()
        .map_err(|_| recovery_refusal("cannot retain root directory handle"))?;
    validate_directory(&cloned, "release root")?;
    Ok(cloned)
}

pub(super) fn open_dir_at(parent: &File, leaf: &OsStr, label: &str) -> Result<File, CustomerError> {
    let fd = openat(
        parent,
        leaf,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| recovery_refusal(format!("{label} subdirectory unavailable")))?;
    let file = File::from(fd);
    validate_directory(&file, label)?;
    Ok(file)
}

fn open_regular_at_dir(parent: &File, leaf: &OsStr, label: &str) -> Result<File, CustomerError> {
    let fd = openat(
        parent,
        leaf,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|_| recovery_refusal(format!("{label} member unavailable")))?;
    let file = File::from(fd);
    validate_regular(&file, label)?;
    Ok(file)
}

#[cfg(test)]
mod tests;
