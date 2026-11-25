// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_selector/sealing.rs
//! Derived release finalization and immutable `0555`/`0444` sealing.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use rustix::fs::{Dir, Gid, Mode, OFlags, Stat, Uid, fchmod, fchown, openat, renameat, statat};
use rustix::process::geteuid;
use serde::Serialize;

use super::super::CustomerError;
use super::super::transition::recovery_refusal;
use super::super::transition_custody::{
    open_directory_no_follow, validate_directory, validate_regular,
};
use super::intent::IntentCustody;
use super::sink_binding::SinkWitness;
use super::tree_custody::{DescriptorBudget, TreeCustody};

const MAX_SEALED_FILES: usize = 512;

#[derive(Serialize)]
struct LowerSinkProvenance<'a> {
    canonical_path: &'a str,
    identity_sha256: &'a str,
    filesystem_block_bytes: u64,
    available_kib_at_bind: u64,
}

#[derive(Serialize)]
struct InstalledProvenance<'a> {
    schema_version: &'static str,
    input_bundle_manifest_sha256: &'a str,
    source_commit: &'a str,
    source_branch: &'a str,
    source_tree_state: &'a str,
    installed_at_utc: &'a str,
    lower_sink: LowerSinkProvenance<'a>,
}

/// Continuously held custody over a finalized and sealed installed release.
pub(in crate::customer) struct SealedReleaseCustody {
    tree: TreeCustody,
    root_path: PathBuf,
    input_manifest_sha256: String,
    installed_manifest_sha256: String,
}

impl SealedReleaseCustody {
    pub(in crate::customer) fn revalidate(&self) -> Result<(), CustomerError> {
        self.tree.revalidate()?;
        if crate::installed_image::verify_release_manifest(&self.root_path).is_none()
            || crate::installed_image::observe_installed_input_bundle_digest(&self.root_path)
                .as_deref()
                != Some(self.input_manifest_sha256.as_str())
        {
            return Err(recovery_refusal(
                "sealed release manifest verification failed on revalidation",
            ));
        }
        Ok(())
    }

    pub(in crate::customer) fn root_path(&self) -> &Path {
        &self.root_path
    }

    pub(in crate::customer) fn root_identity(&self) -> (u64, u64) {
        self.tree.root_identity()
    }

    pub(in crate::customer) fn input_manifest_sha256(&self) -> &str {
        &self.input_manifest_sha256
    }

    pub(in crate::customer) fn installed_manifest_sha256(&self) -> &str {
        &self.installed_manifest_sha256
    }
}

/// Bind lower sink, write provenance & receipts, seal `0555`/`0444`, and capture custody.
pub(in crate::customer) fn finalize_and_seal_release(
    custody: &IntentCustody<'_, '_>,
    sink: &SinkWitness,
    installed_at_utc: &str,
) -> Result<SealedReleaseCustody, CustomerError> {
    finalize_and_seal_release_with_hook(custody, sink, installed_at_utc, || Ok(()))
}

#[cfg(test)]
pub(in crate::customer) fn finalize_and_seal_release_with_hook(
    custody: &IntentCustody<'_, '_>,
    sink: &SinkWitness,
    installed_at_utc: &str,
    before_seal_perms: impl FnOnce() -> Result<(), CustomerError>,
) -> Result<SealedReleaseCustody, CustomerError> {
    finalize_impl(custody, sink, installed_at_utc, before_seal_perms)
}

#[cfg(not(test))]
fn finalize_and_seal_release_with_hook(
    custody: &IntentCustody<'_, '_>,
    sink: &SinkWitness,
    installed_at_utc: &str,
    before_seal_perms: impl FnOnce() -> Result<(), CustomerError>,
) -> Result<SealedReleaseCustody, CustomerError> {
    finalize_impl(custody, sink, installed_at_utc, before_seal_perms)
}

fn finalize_impl(
    custody: &IntentCustody<'_, '_>,
    sink: &SinkWitness,
    installed_at_utc: &str,
    before_seal_perms: impl FnOnce() -> Result<(), CustomerError>,
) -> Result<SealedReleaseCustody, CustomerError> {
    if !crate::installed_image::is_utc_timestamp(installed_at_utc) {
        return Err(recovery_refusal("invalid install timestamp format"));
    }
    custody.revalidate()?;
    sink.revalidate()
        .ok_or_else(|| recovery_refusal("lower sink custody failed before sealing"))?;
    let available_kib = sink
        .available_kib()
        .ok_or_else(|| recovery_refusal("lower sink capacity unavailable"))?;

    let input_manifest_bytes = custody
        .signature()
        .verified_manifest()
        .ok_or_else(|| recovery_refusal("verified bundle manifest unavailable"))?;
    let mut manifest_slice = input_manifest_bytes.as_slice();
    let input_hash = crate::installed_image::sha256_reader(&mut manifest_slice)
        .ok_or_else(|| recovery_refusal("cannot hash bundle manifest"))?;
    if input_hash != custody.pending().new_hash {
        return Err(recovery_refusal(
            "bundle manifest hash disagrees with pending",
        ));
    }

    let root = open_directory_no_follow(custody.target_path(), false)?
        .ok_or_else(|| recovery_refusal("prepared release directory missing"))?;
    validate_directory(&root, "prepared release root")?;
    let root_meta = root
        .metadata()
        .map_err(|_| recovery_refusal("prepared release metadata unavailable"))?;
    if (root_meta.dev(), root_meta.ino()) != custody.target_identity() {
        return Err(recovery_refusal("prepared release root identity changed"));
    }
    ensure_no_pre_derived_files(&root)?;

    let commit = read_held_line(custody.prepared_custody(), "SOURCE_COMMIT")?;
    let branch = read_held_line(custody.prepared_custody(), "SOURCE_BRANCH")?;
    let tree_state = read_held_line(custody.prepared_custody(), "SOURCE_TREE_STATE")?;
    let template = read_held_member(
        custody.prepared_custody(),
        "scripts/safety/cascade.conf.example",
        crate::release_identity::MAX_SEALED_TEXT_BYTES,
    )?;
    let bound_cfg = super::derived_config::reconstruct(
        &template,
        &sink.binding(),
        crate::release_identity::MAX_SEALED_TEXT_BYTES,
    )
    .ok_or_else(|| recovery_refusal("cannot bind lower sink into cascade.conf.example"))?;

    let safety_dir = custody
        .prepared_custody()
        .subdir_file(Path::new("scripts/safety"))
        .ok_or_else(|| recovery_refusal("scripts/safety directory missing in release"))?;
    write_atomic_in_dir(
        safety_dir,
        OsStr::new("cascade.conf.example"),
        &bound_cfg,
        0o644,
    )?;

    write_atomic_in_dir(
        &root,
        OsStr::new("INPUT_BUNDLE_SHA256SUMS"),
        &input_manifest_bytes,
        0o644,
    )?;

    let provenance_bytes = build_provenance_json(
        &input_hash,
        &commit,
        &branch,
        &tree_state,
        installed_at_utc,
        sink,
        available_kib,
    )?;
    write_atomic_in_dir(
        &root,
        OsStr::new("INSTALL_PROVENANCE.json"),
        &provenance_bytes,
        0o644,
    )?;

    let mut manifest_lines = String::new();
    let entries = collect_tree_entries(&root)?;
    for rel in &entries.files {
        let rel_str = rel
            .to_str()
            .ok_or_else(|| recovery_refusal("non-UTF-8 release file path"))?;
        if matches!(rel_str, "SHA256SUMS" | "INSTALLED_MANIFEST_SHA256") {
            continue;
        }
        let parent_rel = rel.parent().unwrap_or_else(|| Path::new(""));
        let leaf = rel
            .file_name()
            .ok_or_else(|| recovery_refusal("missing release entry filename"))?;
        let parent_fd = entries
            .dirs
            .get(parent_rel)
            .ok_or_else(|| recovery_refusal("missing parent dir handle"))?;
        let file = open_regular_for_seal(parent_fd, leaf)?;
        let digest = hash_regular_fd(&file)?;
        manifest_lines.push_str(&digest);
        manifest_lines.push_str("  ./");
        manifest_lines.push_str(rel_str);
        manifest_lines.push('\n');
    }
    let manifest_raw = manifest_lines.as_bytes();
    write_atomic_in_dir(
        &root,
        OsStr::new(crate::release_identity::SHA256SUMS_FILE),
        manifest_raw,
        0o644,
    )?;
    let mut sum_slice = manifest_raw;
    let installed_hash = crate::installed_image::sha256_reader(&mut sum_slice)
        .ok_or_else(|| recovery_refusal("cannot hash regenerated SHA256SUMS"))?;
    let receipt_body = format!("{installed_hash}\n");
    write_atomic_in_dir(
        &root,
        OsStr::new("INSTALLED_MANIFEST_SHA256"),
        receipt_body.as_bytes(),
        0o644,
    )?;

    before_seal_perms()?;
    seal_tree_permissions(&root)?;

    custody.revalidate_prior_and_intent()?;
    sink.revalidate()
        .ok_or_else(|| recovery_refusal("lower sink changed during release sealing"))?;
    let mut budget = DescriptorBudget::acquire()?;
    let tree = TreeCustody::capture_sealed(
        custody.target_path(),
        custody.target_identity(),
        &installed_hash,
        &input_hash,
        &mut budget,
    )?;
    Ok(SealedReleaseCustody {
        tree,
        root_path: custody.target_path().to_path_buf(),
        input_manifest_sha256: input_hash,
        installed_manifest_sha256: installed_hash,
    })
}

fn ensure_no_pre_derived_files(root: &File) -> Result<(), CustomerError> {
    for name in [
        "INPUT_BUNDLE_SHA256SUMS",
        "INSTALL_PROVENANCE.json",
        "INSTALLED_MANIFEST_SHA256",
    ] {
        match statat(root, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
            Err(rustix::io::Errno::NOENT) => {}
            _ => {
                return Err(recovery_refusal(format!(
                    "derived file {name} already present before sealing"
                )));
            }
        }
    }
    Ok(())
}

fn read_held_member(
    tree: &TreeCustody,
    path: &str,
    limit: usize,
) -> Result<Vec<u8>, CustomerError> {
    let mut file = tree
        .member_file(path)
        .ok_or_else(|| recovery_refusal(format!("prepared member absent: {path}")))?;
    let len = file
        .metadata()
        .map_err(|_| recovery_refusal("prepared member metadata unavailable"))?
        .len();
    let cap = usize::try_from(len)
        .ok()
        .filter(|&n| n <= limit)
        .ok_or_else(|| recovery_refusal(format!("prepared member exceeds limit: {path}")))?;
    let mut buf = Vec::new();
    buf.try_reserve_exact(cap)
        .map_err(|_| recovery_refusal("allocation failed for prepared member"))?;
    buf.resize(cap, 0);
    file.seek(SeekFrom::Start(0))
        .map_err(|_| recovery_refusal("seek failed on prepared member"))?;
    file.read_exact(&mut buf)
        .map_err(|_| recovery_refusal("read failed on prepared member"))?;
    Ok(buf)
}

fn read_held_line(tree: &TreeCustody, path: &str) -> Result<String, CustomerError> {
    let bytes = read_held_member(tree, path, crate::release_identity::MAX_IDENTITY_LINE_BYTES)?;
    crate::release_identity::parse_identity_line(&bytes)
        .ok_or_else(|| recovery_refusal(format!("invalid release identity line in {path}")))
}

fn build_provenance_json(
    input_hash: &str,
    commit: &str,
    branch: &str,
    tree_state: &str,
    installed_at_utc: &str,
    sink: &SinkWitness,
    available_kib: u64,
) -> Result<Vec<u8>, CustomerError> {
    let binding = sink.binding();
    let doc = InstalledProvenance {
        schema_version: crate::installed_image::INSTALLED_PROVENANCE_V2,
        input_bundle_manifest_sha256: input_hash,
        source_commit: commit,
        source_branch: branch,
        source_tree_state: tree_state,
        installed_at_utc,
        lower_sink: LowerSinkProvenance {
            canonical_path: binding.path,
            identity_sha256: binding.identity,
            filesystem_block_bytes: binding.block_bytes,
            available_kib_at_bind: available_kib,
        },
    };
    let mut encoded = serde_json::to_vec(&doc)
        .map_err(|_| recovery_refusal("cannot serialize installed provenance"))?;
    encoded.push(b'\n');
    Ok(encoded)
}

fn write_atomic_in_dir(
    dir: &File,
    target_name: &OsStr,
    bytes: &[u8],
    mode: u32,
) -> Result<(), CustomerError> {
    let target_str = target_name
        .to_str()
        .ok_or_else(|| recovery_refusal("non-UTF-8 target filename"))?;
    let staging = format!(
        ".{target_str}.{}.staging",
        rustix::process::getpid().as_raw_pid()
    );
    let mut file = File::from(
        openat(
            dir,
            staging.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(mode),
        )
        .map_err(|_| recovery_refusal("cannot create release staging file"))?,
    );
    file.write_all(bytes)
        .map_err(|_| recovery_refusal("cannot write release staging file"))?;
    fchmod(&file, Mode::from_raw_mode(mode))
        .map_err(|_| recovery_refusal("cannot set release staging file mode"))?;
    if geteuid().as_raw() == 0 {
        fchown(&file, Some(Uid::ROOT), Some(Gid::ROOT))
            .map_err(|_| recovery_refusal("cannot chown release staging file"))?;
    }
    file.sync_all()
        .map_err(|_| recovery_refusal("cannot sync release staging file"))?;
    drop(file);
    renameat(dir, staging.as_str(), dir, target_name)
        .map_err(|_| recovery_refusal("cannot rename release staging file"))?;
    dir.sync_all()
        .map_err(|_| recovery_refusal("cannot sync release directory"))
}

struct WalkedTree {
    dirs: BTreeMap<PathBuf, File>,
    files: BTreeSet<PathBuf>,
}

fn collect_tree_entries(root: &File) -> Result<WalkedTree, CustomerError> {
    let mut dirs = BTreeMap::new();
    let cloned = root
        .try_clone()
        .map_err(|_| recovery_refusal("cannot clone release root"))?;
    dirs.insert(PathBuf::new(), cloned);
    let mut files = BTreeSet::new();
    let mut queue = vec![PathBuf::new()];
    while let Some(rel_dir) = queue.pop() {
        let dir_fd = dirs
            .get(&rel_dir)
            .ok_or_else(|| recovery_refusal("walked dir handle missing"))?
            .try_clone()
            .map_err(|_| recovery_refusal("cannot clone walked dir handle"))?;
        let mut reader = Dir::read_from(&dir_fd)
            .map_err(|_| recovery_refusal("cannot read release directory"))?;
        let mut children = Vec::new();
        while let Some(entry) = reader.read() {
            let entry = entry.map_err(|_| recovery_refusal("cannot iterate release directory"))?;
            let name = entry
                .file_name()
                .to_str()
                .map_err(|_| recovery_refusal("non-UTF-8 release entry"))?;
            if matches!(name, "." | "..") {
                continue;
            }
            if name.starts_with('.') {
                return Err(recovery_refusal("unexpected hidden entry in release tree"));
            }
            children.push(name.to_owned());
        }
        for name in children {
            if dirs.len() + files.len() >= MAX_SEALED_FILES {
                return Err(recovery_refusal("release entry count exceeds bound"));
            }
            let child_rel = if rel_dir.as_os_str().is_empty() {
                PathBuf::from(&name)
            } else {
                rel_dir.join(&name)
            };
            let st: Stat = statat(
                &dir_fd,
                name.as_str(),
                rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
            )
            .map_err(|_| recovery_refusal("cannot stat release entry"))?;
            match rustix::fs::FileType::from_raw_mode(st.st_mode) {
                rustix::fs::FileType::Directory => {
                    let sub_fd = File::from(
                        openat(
                            &dir_fd,
                            name.as_str(),
                            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                            Mode::empty(),
                        )
                        .map_err(|_| recovery_refusal("cannot open release subdirectory"))?,
                    );
                    validate_directory(&sub_fd, "release subdirectory")?;
                    dirs.insert(child_rel.clone(), sub_fd);
                    queue.push(child_rel);
                }
                rustix::fs::FileType::RegularFile => {
                    files.insert(child_rel);
                }
                _ => {
                    return Err(recovery_refusal("non-regular object inside release tree"));
                }
            }
        }
    }
    Ok(WalkedTree { dirs, files })
}

fn open_regular_for_seal(parent: &File, leaf: &OsStr) -> Result<File, CustomerError> {
    let fd = openat(
        parent,
        leaf,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|_| recovery_refusal("cannot open release file for sealing"))?;
    let file = File::from(fd);
    validate_regular(&file, "release member")?;
    Ok(file)
}

fn hash_regular_fd(file: &File) -> Result<String, CustomerError> {
    let size = file
        .metadata()
        .map_err(|_| recovery_refusal("release file metadata unavailable"))?
        .len();
    let mut reader = file;
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|_| recovery_refusal("cannot seek release file"))?;
    crate::installed_image::sha256_reader_with_limit(&mut reader, size)
        .ok_or_else(|| recovery_refusal("release file digest computation failed"))
}

fn seal_tree_permissions(root: &File) -> Result<(), CustomerError> {
    let entries = collect_tree_entries(root)?;
    let as_root = geteuid().as_raw() == 0;
    for rel in &entries.files {
        let parent_rel = rel.parent().unwrap_or_else(|| Path::new(""));
        let leaf = rel
            .file_name()
            .ok_or_else(|| recovery_refusal("missing release entry filename"))?;
        let parent_fd = entries
            .dirs
            .get(parent_rel)
            .ok_or_else(|| recovery_refusal("missing parent dir handle"))?;
        let file = open_regular_for_seal(parent_fd, leaf)?;
        let mode = file
            .metadata()
            .map_err(|_| recovery_refusal("release file metadata unavailable"))?
            .mode();
        let target_mode = if mode & 0o111 != 0 { 0o555 } else { 0o444 };
        if as_root {
            fchown(&file, Some(Uid::ROOT), Some(Gid::ROOT))
                .map_err(|_| recovery_refusal("cannot chown release file"))?;
        }
        fchmod(&file, Mode::from_raw_mode(target_mode))
            .map_err(|_| recovery_refusal("cannot chmod release file"))?;
        file.sync_all()
            .map_err(|_| recovery_refusal("cannot sync sealed release file"))?;
    }
    let mut dir_paths: Vec<&PathBuf> = entries.dirs.keys().collect();
    dir_paths.sort_by(|a, b| {
        b.components()
            .count()
            .cmp(&a.components().count())
            .then_with(|| b.cmp(a))
    });
    for rel in dir_paths {
        let dir = entries
            .dirs
            .get(rel)
            .ok_or_else(|| recovery_refusal("missing directory during seal"))?;
        if as_root {
            fchown(dir, Some(Uid::ROOT), Some(Gid::ROOT))
                .map_err(|_| recovery_refusal("cannot chown release directory"))?;
        }
        fchmod(dir, Mode::from_raw_mode(0o555))
            .map_err(|_| recovery_refusal("cannot chmod release directory"))?;
        dir.sync_all()
            .map_err(|_| recovery_refusal("cannot sync sealed release directory"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;

    struct SealFixture(PathBuf);
    impl Drop for SealFixture {
        fn drop(&mut self) {
            if let Ok(md) = std::fs::symlink_metadata(&self.0)
                && md.is_dir()
            {
                let _ = crate::customer::package_selector::remove_owned_unselected_release(
                    &self.0,
                    (md.dev(), md.ino()),
                    &self.0.join("nonexistent-selector"),
                );
            }
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn sealing_primitives_refuse_pre_derived_hidden_links_and_overfull_trees() {
        let root = std::env::temp_dir().join(format!(
            "ramshared-sealing-primitives-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        let _fixture = SealFixture(root.clone());

        let root_fd = open_directory_no_follow(&root, false).unwrap().unwrap();
        for existing in [
            "INPUT_BUNDLE_SHA256SUMS",
            "INSTALL_PROVENANCE.json",
            "INSTALLED_MANIFEST_SHA256",
        ] {
            let file = root.join(existing);
            std::fs::write(&file, b"present").unwrap();
            assert!(ensure_no_pre_derived_files(&root_fd).is_err(), "{existing}");
            std::fs::remove_file(&file).unwrap();
        }
        assert!(ensure_no_pre_derived_files(&root_fd).is_ok());

        let hidden = root.join(".hidden");
        std::fs::write(&hidden, b"hidden").unwrap();
        assert!(collect_tree_entries(&root_fd).is_err());
        std::fs::remove_file(&hidden).unwrap();

        let link = root.join("link");
        symlink("target", &link).unwrap();
        assert!(collect_tree_entries(&root_fd).is_err());
        assert!(open_regular_for_seal(&root_fd, OsStr::new("link")).is_err());
        std::fs::remove_file(&link).unwrap();

        assert!(open_regular_for_seal(&root_fd, OsStr::new("missing")).is_err());

        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/exec"), b"x").unwrap();
        fchmod(
            File::open(root.join("sub/exec")).unwrap(),
            Mode::from_raw_mode(0o755),
        )
        .unwrap();
        std::fs::write(root.join("sub/data"), b"y").unwrap();
        let walked = collect_tree_entries(&root_fd).unwrap();
        assert_eq!(walked.files.len(), 2);

        let ro = root.join("readonly");
        std::fs::create_dir(&ro).unwrap();
        let ro_fd = open_directory_no_follow(&ro, false).unwrap().unwrap();
        fchmod(&ro_fd, Mode::from_raw_mode(0o555)).unwrap();
        if geteuid().as_raw() != 0 {
            assert!(write_atomic_in_dir(&ro_fd, OsStr::new("item"), b"x", 0o444).is_err());
        }
        fchmod(&ro_fd, Mode::from_raw_mode(0o755)).unwrap();
        seal_tree_permissions(&root_fd).unwrap();
        assert_eq!(
            std::fs::metadata(root.join("sub/exec")).unwrap().mode() & 0o777,
            0o555
        );
        assert_eq!(
            std::fs::metadata(root.join("sub/data")).unwrap().mode() & 0o777,
            0o444
        );
    }

    #[test]
    fn collect_tree_entries_refuses_entries_above_max_sealed_files() {
        let root =
            std::env::temp_dir().join(format!("ramshared-sealing-bound-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        let _fixture = SealFixture(root.clone());
        for idx in 0..=MAX_SEALED_FILES {
            std::fs::write(root.join(format!("f{idx}")), b"v").unwrap();
        }
        let root_fd = open_directory_no_follow(&root, false).unwrap().unwrap();
        assert!(collect_tree_entries(&root_fd).is_err());

        let file_fd = File::open(root.join("f0")).unwrap();
        assert!(collect_tree_entries(&file_fd).is_err());
        assert!(write_atomic_in_dir(&root_fd, OsStr::from_bytes(b"\xff"), b"v", 0o444).is_err());
        assert!(hash_regular_fd(&root_fd).is_err());
    }

    #[test]
    fn held_member_and_sealed_custody_validation_refuse_mismatch_and_limits() {
        let root =
            std::env::temp_dir().join(format!("ramshared-sealing-custody-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::create_dir_all(root.join("scripts/safety")).unwrap();
        let _fixture = SealFixture(root.clone());

        let cli = b"\x7fELF\x02cli".to_vec();
        let daemon = b"\x7fELF\x02daemon".to_vec();
        let cfg = b"NBD_LOWER_SINK=/tmp\n".to_vec();
        let bad_line = b"\n".to_vec();
        std::fs::write(root.join("bin/ramshared"), &cli).unwrap();
        std::fs::write(root.join("bin/ramsharedd"), &daemon).unwrap();
        std::fs::write(root.join("scripts/safety/cascade.conf.example"), &cfg).unwrap();
        std::fs::write(root.join("RELEASE_VERSION"), b"v1\n").unwrap();
        std::fs::write(
            root.join("SOURCE_COMMIT"),
            format!("{}\n", "1".repeat(40)).as_bytes(),
        )
        .unwrap();
        std::fs::write(root.join("SOURCE_TREE_STATE"), &bad_line).unwrap();
        let h = |b: &[u8]| {
            let mut s = b;
            crate::installed_image::sha256_reader(&mut s).unwrap()
        };
        let input_body = format!(
            "{}  ./bin/ramshared\n{}  ./bin/ramsharedd\n{}  ./RELEASE_VERSION\n{}  ./SOURCE_COMMIT\n{}  ./SOURCE_TREE_STATE\n{}  ./scripts/safety/cascade.conf.example\n",
            h(&cli),
            h(&daemon),
            h(b"v1\n"),
            h(format!("{}\n", "1".repeat(40)).as_bytes()),
            h(&bad_line),
            h(&cfg),
        );
        std::fs::write(root.join("INPUT_BUNDLE_SHA256SUMS"), input_body.as_bytes()).unwrap();
        let prov = serde_json::json!({
            "schema_version": crate::installed_image::INSTALLED_PROVENANCE_V2,
            "input_bundle_manifest_sha256": h(input_body.as_bytes()),
        })
        .to_string()
            + "\n";
        std::fs::write(root.join("INSTALL_PROVENANCE.json"), prov.as_bytes()).unwrap();
        let sha_sums = format!(
            "{input_body}{}  ./INPUT_BUNDLE_SHA256SUMS\n{}  ./INSTALL_PROVENANCE.json\n",
            h(input_body.as_bytes()),
            h(prov.as_bytes()),
        );
        std::fs::write(root.join("SHA256SUMS"), sha_sums.as_bytes()).unwrap();
        std::fs::write(
            root.join("INSTALLED_MANIFEST_SHA256"),
            format!("{}\n", h(sha_sums.as_bytes())),
        )
        .unwrap();

        let dir_fd = open_directory_no_follow(&root, false).unwrap().unwrap();
        let mut budget = DescriptorBudget::with_cap(32);
        let prev_tree = TreeCustody::capture_previous(
            &dir_fd,
            input_body.as_bytes(),
            (4096, 65536),
            None,
            &mut budget,
        )
        .unwrap();
        assert!(read_held_member(&prev_tree, "missing", 16).is_err());
        assert!(read_held_member(&prev_tree, "bin/ramshared", 1).is_err());
        assert!(read_held_line(&prev_tree, "SOURCE_TREE_STATE").is_err());
        assert_eq!(read_held_line(&prev_tree, "RELEASE_VERSION").unwrap(), "v1");
        drop(prev_tree);

        seal_tree_permissions(&dir_fd).unwrap();
        let md = std::fs::symlink_metadata(&root).unwrap();
        let mut budget2 = DescriptorBudget::with_cap(32);
        let sealed_tree = TreeCustody::capture_sealed(
            &root,
            (md.dev(), md.ino()),
            &h(sha_sums.as_bytes()),
            &h(input_body.as_bytes()),
            &mut budget2,
        )
        .unwrap();
        let bogus_sealed = SealedReleaseCustody {
            tree: sealed_tree,
            root_path: root.clone(),
            input_manifest_sha256: "0".repeat(64),
            installed_manifest_sha256: h(sha_sums.as_bytes()),
        };
        assert!(bogus_sealed.revalidate().is_err());
    }
}
