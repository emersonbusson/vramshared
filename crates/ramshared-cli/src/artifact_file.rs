// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/artifact_file.rs
//! Exclusive publication of bounded, operator-selected plans and profile drafts.
//!
//! Directory custody is shared with customer transitions. Publication keeps the
//! parent open, creates a private new inode, and verifies bytes and both names.
//! Failure after creation retains evidence: a pathname is never safe cleanup
//! authority once another actor may have replaced it.

use std::ffi::OsString;
use std::fmt;
use std::fs::{File, Metadata};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use ramshared_config::resource_profile::MAX_RESOURCE_PROFILE_BYTES;
use rustix::fs::{AtFlags, Mode, OFlags, fchmod, openat, statat};

use crate::customer::transition_custody::{
    FileIdentity, open_directory_no_follow, validate_directory,
};

#[derive(Debug)]
pub(crate) enum ArtifactError {
    Refused(String),
    Unavailable(String),
    Uncertain(String),
}

impl fmt::Display for ArtifactError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (Self::Refused(detail) | Self::Unavailable(detail) | Self::Uncertain(detail)) = self;
        formatter.write_str(detail)
    }
}

pub(crate) struct PreparedOutput {
    path: PathBuf,
    parent_path: PathBuf,
    leaf: OsString,
    parent: File,
    identity: FileIdentity,
}

fn normalized_output(path: &Path) -> Result<PathBuf, ArtifactError> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty()
        || (bytes.ends_with(b"/") || bytes.ends_with(b"/."))
        || bytes.contains(&0)
        || path.file_name().is_none()
    {
        return Err(ArtifactError::Refused("output must name a new file".into()));
    }
    if path
        .components()
        .any(|part| matches!(part, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(ArtifactError::Refused(
            "output cannot contain parent traversal; select an absolute path instead".into(),
        ));
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| {
                ArtifactError::Unavailable(format!("cannot observe current directory: {error}"))
            })?
            .join(path)
    };
    Ok(absolute
        .components()
        .filter(|part| !matches!(part, Component::CurDir))
        .collect())
}

impl PreparedOutput {
    fn open_parent(path: &Path) -> Result<Self, ArtifactError> {
        let path = normalized_output(path)?;
        let parent_path = path
            .parent()
            .ok_or_else(|| ArtifactError::Refused("output has no parent".into()))?
            .to_path_buf();
        let leaf = path
            .file_name()
            .ok_or_else(|| ArtifactError::Refused("output has no filename".into()))?
            .to_os_string();
        let parent = open_directory_no_follow(&parent_path, false)
            .map_err(|error| ArtifactError::Refused(error.detail))?
            .ok_or_else(|| {
                ArtifactError::Unavailable("output parent directory is absent".into())
            })?;
        let identity = validate_directory(&parent, "output parent")
            .map_err(|error| ArtifactError::Refused(error.detail))?;
        Ok(Self {
            path,
            parent_path,
            leaf,
            parent,
            identity,
        })
    }

    pub(crate) fn inspect(path: &Path) -> Result<Self, ArtifactError> {
        let prepared = Self::open_parent(path)?;
        match statat(&prepared.parent, &prepared.leaf, AtFlags::SYMLINK_NOFOLLOW) {
            Err(error) if error == rustix::io::Errno::NOENT => {}
            Ok(_) => {
                return Err(ArtifactError::Refused(
                    "output already exists; it was not overwritten".into(),
                ));
            }
            Err(error) => {
                return Err(ArtifactError::Unavailable(format!(
                    "cannot inspect output: {error}"
                )));
            }
        }
        Ok(prepared)
    }

    fn verify_parent(&self) -> io::Result<()> {
        let named = open_directory_no_follow(&self.parent_path, false)
            .map_err(|error| io::Error::other(error.detail))?
            .ok_or_else(|| io::Error::other("output parent disappeared"))?;
        let identity = validate_directory(&named, "output parent")
            .map_err(|error| io::Error::other(error.detail))?;
        if identity != self.identity {
            return Err(io::Error::other("output parent identity changed"));
        }
        Ok(())
    }

    pub(crate) fn publish(&self, body: &[u8]) -> Result<PathBuf, ArtifactError> {
        check_size(body.len())?;
        let file = self.create()?;
        self.finish(file, body)
    }

    fn stage(&self, body: &[u8]) -> Result<(Self, File), ArtifactError> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        check_size(body.len())?;
        self.verify_parent()
            .map_err(|error| ArtifactError::Refused(error.to_string()))?;
        let mut leaf = OsString::from(".ramshared-");
        leaf.push(&self.leaf);
        leaf.push(format!(
            ".{}.{}.pending",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let staged = Self {
            path: self.parent_path.join(&leaf),
            parent_path: self.parent_path.clone(),
            leaf,
            parent: self
                .parent
                .try_clone()
                .map_err(|error| ArtifactError::Unavailable(error.to_string()))?,
            identity: self.identity,
        };
        let mut file = staged.create()?;
        staged.write_verified(&mut file, body).map_err(|error| {
            ArtifactError::Uncertain(format!(
                "cannot stage {}; evidence retained: {error}",
                staged.path.display()
            ))
        })?;
        Ok((staged, file))
    }

    fn commit_stage(
        &self,
        staged: &Self,
        file: &mut File,
        body: &[u8],
    ) -> Result<PathBuf, ArtifactError> {
        let result = (|| {
            staged.verify_publication(file, body)?;
            self.verify_parent()?;
            rustix::fs::renameat_with(
                &staged.parent,
                &staged.leaf,
                &self.parent,
                &self.leaf,
                rustix::fs::RenameFlags::NOREPLACE,
            )?;
            self.verify_publication(file, body)
        })();
        result.map_err(|error: io::Error| {
            ArtifactError::Uncertain(format!(
                "cannot publish {} from {}; evidence retained: {error}",
                self.path.display(),
                staged.path.display()
            ))
        })?;
        Ok(self.path.clone())
    }

    /// Publish complete bytes atomically, retaining the staging fd and refusing
    /// any existing final name. Failed publication never unlinks evidence.
    pub(crate) fn publish_atomic(&self, body: &[u8]) -> Result<PathBuf, ArtifactError> {
        let (staged, mut file) = self.stage(body)?;
        self.commit_stage(&staged, &mut file, body)
    }

    fn write_verified(&self, file: &mut File, body: &[u8]) -> io::Result<()> {
        fchmod(&*file, Mode::RUSR | Mode::WUSR)?;
        file.write_all(body)?;
        file.sync_all()?;
        self.verify_publication(file, body)
    }

    fn create(&self) -> Result<File, ArtifactError> {
        self.verify_parent()
            .map_err(|error| ArtifactError::Refused(error.to_string()))?;
        openat(
            &self.parent,
            &self.leaf,
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map(File::from)
        .map_err(|error| {
            if error == rustix::io::Errno::EXIST {
                ArtifactError::Refused("output already exists; it was not overwritten".into())
            } else {
                ArtifactError::Unavailable(format!("cannot create output: {error}"))
            }
        })
    }

    fn finish(&self, mut file: File, body: &[u8]) -> Result<PathBuf, ArtifactError> {
        let result = self.write_verified(&mut file, body);
        result.map_err(|error: io::Error| ArtifactError::Uncertain(format!(
            "cannot verify publication of {}: {error}; evidence retained; inspect before choosing a new output",
            self.path.display(),
        )))?;
        Ok(self.path.clone())
    }

    fn verify_publication(&self, file: &mut File, body: &[u8]) -> io::Result<()> {
        let original = private_metadata(file, body.len())?;
        file.seek(SeekFrom::Start(0))?;
        let mut readback = vec![0; body.len()];
        file.read_exact(&mut readback)?;
        let mut trailing = [0; 1];
        if file.read(&mut trailing)? != 0 || readback != body {
            return Err(io::Error::other(
                "output bytes differ from the requested document",
            ));
        }
        // NONBLOCK prevents a replaced FIFO from hanging the verification open.
        let named = File::from(openat(
            &self.parent,
            &self.leaf,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )?);
        let current = private_metadata(&named, body.len())?;
        if original.dev() != current.dev() || original.ino() != current.ino() {
            return Err(io::Error::other(
                "output name no longer identifies the created inode",
            ));
        }
        self.parent.sync_all()?;
        self.verify_parent()
    }
}

fn private_metadata(file: &File, bytes: usize) -> io::Result<Metadata> {
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o7777 != 0o600
        || metadata.nlink() != 1
        || metadata.len() != bytes as u64
    {
        return Err(io::Error::other(
            "output type, owner, mode, link count or length changed",
        ));
    }
    Ok(metadata)
}

pub(crate) fn inspect_new_output(path: &Path) -> Result<PathBuf, ArtifactError> {
    Ok(PreparedOutput::inspect(path)?.path)
}

fn check_size(bytes: usize) -> Result<(), ArtifactError> {
    if bytes > MAX_RESOURCE_PROFILE_BYTES {
        return Err(ArtifactError::Refused(
            "output document exceeds the shared size limit".into(),
        ));
    }
    Ok(())
}

pub(crate) fn write_new(path: &Path, body: &[u8]) -> Result<PathBuf, ArtifactError> {
    check_size(body.len())?;
    PreparedOutput::inspect(path)?.publish(body)
}

/// Read private plan/approval/manifest input from retained directory/file fds.
/// This has no write or create effect, even when verification refuses.
pub(crate) fn read_private(path: &Path) -> Result<String, ArtifactError> {
    let prepared = PreparedOutput::open_parent(path)?;
    let opened = openat(
        &prepared.parent,
        &prepared.leaf,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|error| {
        let detail = format!(
            "cannot open reviewed input {} without following links: {error}",
            path.display()
        );
        if error == rustix::io::Errno::LOOP {
            ArtifactError::Refused(detail)
        } else {
            ArtifactError::Unavailable(detail)
        }
    })?;
    let mut file = File::from(opened);
    let inspect = (|| {
        let before = file.metadata()?;
        if before.len() > MAX_RESOURCE_PROFILE_BYTES as u64 {
            return Err(io::Error::other(
                "reviewed input exceeds the shared size limit",
            ));
        }
        private_metadata(&file, before.len() as usize)?;
        let mut bytes = vec![0; before.len() as usize];
        file.read_exact(&mut bytes)?;
        let mut trailing = [0; 1];
        if file.read(&mut trailing)? != 0 {
            return Err(io::Error::other("reviewed input grew during reading"));
        }
        let after = private_metadata(&file, bytes.len())?;
        let named = File::from(openat(
            &prepared.parent,
            &prepared.leaf,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )?);
        let current = private_metadata(&named, bytes.len())?;
        if before.dev() != current.dev()
            || before.ino() != current.ino()
            || before.mtime() != after.mtime()
            || before.mtime_nsec() != after.mtime_nsec()
            || before.ctime() != after.ctime()
            || before.ctime_nsec() != after.ctime_nsec()
        {
            return Err(io::Error::other("reviewed input changed during reading"));
        }
        prepared.verify_parent()?;
        String::from_utf8(bytes).map_err(io::Error::other)
    })();
    inspect.map_err(|error| {
        ArtifactError::Refused(format!(
            "cannot verify reviewed input {}: {error}",
            path.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "ramshared-artifact-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn artifact_creation_is_private_exclusive_and_exact() {
        let root = Fixture::new();
        let output = root.path("document");
        assert_eq!(inspect_new_output(&output).unwrap(), output);
        let bytes = b"a reviewed document\n";
        assert_eq!(write_new(&output, bytes).unwrap(), output);
        let metadata = fs::metadata(&output).unwrap();
        assert_eq!(metadata.mode() & 0o7777, 0o600);
        assert_eq!(metadata.uid(), rustix::process::geteuid().as_raw());
        assert_eq!(metadata.nlink(), 1);
        assert_eq!(fs::read(&output).unwrap(), bytes);
        assert!(matches!(
            write_new(&output, b"replacement"),
            Err(ArtifactError::Refused(_))
        ));
        assert_eq!(fs::read(&output).unwrap(), bytes);
        let link = root.path("symlink");
        symlink(&output, &link).unwrap();
        assert!(matches!(
            write_new(&link, b"replacement"),
            Err(ArtifactError::Refused(_))
        ));
        assert_eq!(fs::read(&output).unwrap(), bytes);
        let maximum = vec![b'x'; MAX_RESOURCE_PROFILE_BYTES];
        write_new(&root.path("maximum"), &maximum).unwrap();
        assert_eq!(fs::read(root.path("maximum")).unwrap(), maximum);
        assert!(matches!(
            write_new(
                &root.path("oversized"),
                &vec![0; MAX_RESOURCE_PROFILE_BYTES + 1]
            ),
            Err(ArtifactError::Refused(_))
        ));
        assert!(!root.path("oversized").exists());
        write_new(&root.path("empty"), b"").unwrap();
    }

    #[test]
    fn artifact_creation_refuses_invalid_or_unsafe_parents() {
        let root = Fixture::new();
        for path in [
            PathBuf::new(),
            PathBuf::from("/"),
            PathBuf::from("."),
            root.path("../escape"),
            root.path("trailing/"),
            root.path("dot/."),
            PathBuf::from("bad\0name"),
        ] {
            assert!(
                matches!(write_new(&path, b"x"), Err(ArtifactError::Refused(_))),
                "{}",
                path.display()
            );
        }
        assert!(matches!(
            write_new(&root.path("absent/document"), b"x"),
            Err(ArtifactError::Unavailable(_))
        ));
        let unsafe_parent = root.path("unsafe");
        fs::create_dir(&unsafe_parent).unwrap();
        fs::set_permissions(&unsafe_parent, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(matches!(
            write_new(&unsafe_parent.join("document"), b"x"),
            Err(ArtifactError::Refused(_))
        ));
        let linked_parent = root.path("linked");
        symlink(&unsafe_parent, &linked_parent).unwrap();
        assert!(matches!(
            write_new(&linked_parent.join("document"), b"x"),
            Err(ArtifactError::Refused(_))
        ));
        assert!(!unsafe_parent.join("document").exists());
        let regular_parent = root.path("file");
        fs::write(&regular_parent, b"foreign").unwrap();
        assert!(write_new(&regular_parent.join("document"), b"x").is_err());
        assert_eq!(fs::read(&regular_parent).unwrap(), b"foreign");
        // A rival publication between review and creation is still exclusive.
        let prepared = PreparedOutput::inspect(&root.path("rival")).unwrap();
        fs::write(&prepared.path, b"rival").unwrap();
        assert!(matches!(prepared.create(), Err(ArtifactError::Refused(_))));
        assert_eq!(fs::read(&prepared.path).unwrap(), b"rival");
    }

    #[test]
    fn artifact_parent_replacement_cannot_redirect_creation() {
        let root = Fixture::new();
        let parent = root.path("parent");
        let pinned = root.path("pinned");
        let foreign = root.path("foreign");
        fs::create_dir(&parent).unwrap();
        fs::create_dir(&foreign).unwrap();
        let prepared = PreparedOutput::inspect(&parent.join("document")).unwrap();
        // Replacement before creation is refused before opening a new inode.
        fs::rename(&parent, &pinned).unwrap();
        fs::create_dir(&parent).unwrap();
        assert!(matches!(prepared.create(), Err(ArtifactError::Refused(_))));
        assert!(!parent.join("document").exists());
        fs::remove_dir(&parent).unwrap();
        fs::rename(&pinned, &parent).unwrap();
        let file = prepared.create().unwrap();
        // After creation, a symlink at the parent name cannot redirect writes.
        fs::rename(&parent, &pinned).unwrap();
        symlink(&foreign, &parent).unwrap();
        fs::write(foreign.join("document"), b"foreign").unwrap();
        let error = prepared.finish(file, b"owned").unwrap_err();
        assert!(matches!(error, ArtifactError::Uncertain(_)));
        assert!(error.to_string().contains("evidence retained"));
        assert_eq!(fs::read(pinned.join("document")).unwrap(), b"owned");
        assert_eq!(fs::read(foreign.join("document")).unwrap(), b"foreign");
        fs::remove_file(&parent).unwrap();
        // Missing names are also a refusal, not a successful publication.
        assert!(matches!(prepared.create(), Err(ArtifactError::Refused(_))));
    }

    #[test]
    fn artifact_postwrite_uncertainty_preserves_owned_and_foreign_files() {
        let root = Fixture::new();
        for symlink_replacement in [false, true] {
            let name = if symlink_replacement {
                "linked"
            } else {
                "replaced"
            };
            let prepared = PreparedOutput::inspect(&root.path(name)).unwrap();
            let file = prepared.create().unwrap();
            let original = root.path(&format!("{name}-original"));
            fs::rename(&prepared.path, &original).unwrap();
            let foreign = root.path(&format!("{name}-foreign"));
            fs::write(&foreign, b"other").unwrap();
            fs::set_permissions(&foreign, fs::Permissions::from_mode(0o600)).unwrap();
            if symlink_replacement {
                symlink(&foreign, &prepared.path).unwrap();
            } else {
                fs::rename(&foreign, &prepared.path).unwrap();
            }
            assert!(matches!(
                prepared.finish(file, b"owned"),
                Err(ArtifactError::Uncertain(_))
            ));
            assert_eq!(fs::read(&original).unwrap(), b"owned");
            assert_eq!(fs::read(&prepared.path).unwrap(), b"other");
        }
        let prepared = PreparedOutput::inspect(&root.path("hardlinked")).unwrap();
        let file = prepared.create().unwrap();
        fs::hard_link(&prepared.path, root.path("second-name")).unwrap();
        assert!(matches!(
            prepared.finish(file, b"owned"),
            Err(ArtifactError::Uncertain(_))
        ));
        assert_eq!(fs::read(root.path("second-name")).unwrap(), b"owned");
    }

    #[test]
    fn artifact_verification_detects_changed_bytes_and_private_metadata() {
        let root = Fixture::new();
        let prepared = PreparedOutput::inspect(&root.path("document")).unwrap();
        let mut file = prepared.create().unwrap();
        file.write_all(b"wrong").unwrap();
        assert!(prepared.verify_publication(&mut file, b"owned").is_err());
        file.set_len(6).unwrap();
        assert!(prepared.verify_publication(&mut file, b"owned").is_err());
        file.set_len(5).unwrap();
        fs::set_permissions(&prepared.path, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(prepared.verify_publication(&mut file, b"wrong").is_err());
        assert_eq!(fs::read(&prepared.path).unwrap(), b"wrong");
    }
    #[test]
    fn reviewed_input_is_private_bounded_and_refuses_redirects() {
        let fixture = Fixture::new();
        let path = fixture.path("input");
        write_new(&path, b"reviewed bytes").unwrap();
        assert_eq!(read_private(&path).unwrap(), "reviewed bytes");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            read_private(&path),
            Err(ArtifactError::Refused(_))
        ));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let link = fixture.path("link");
        symlink(&path, &link).unwrap();
        assert!(matches!(
            read_private(&link),
            Err(ArtifactError::Refused(_))
        ));
        fs::write(&path, vec![0; MAX_RESOURCE_PROFILE_BYTES + 1]).unwrap();
        assert!(read_private(&path).is_err());
        fs::write(&path, [0xff]).unwrap();
        assert!(read_private(&path).is_err());
        fs::write(&path, b"").unwrap();
        assert_eq!(read_private(&path).unwrap(), "");
        assert!(matches!(
            read_private(&fixture.path("missing")),
            Err(ArtifactError::Unavailable(_))
        ));
        let fifo = fixture.path("fifo");
        rustix::fs::mkfifoat(rustix::fs::CWD, &fifo, Mode::RUSR | Mode::WUSR).unwrap();
        assert!(read_private(&fifo).is_err());
    }
    #[test]
    fn atomic_publication_exposes_only_complete_bytes_and_preserves_conflicts() {
        let fixture = Fixture::new();
        let path = fixture.path("manifest");
        let output = PreparedOutput::inspect(&path).unwrap();
        let (staged, mut file) = output.stage(b"complete metadata").unwrap();
        assert!(!path.exists());
        assert_eq!(fs::read(&staged.path).unwrap(), b"complete metadata");
        output
            .commit_stage(&staged, &mut file, b"complete metadata")
            .unwrap();
        assert_eq!(read_private(&path).unwrap(), "complete metadata");
        assert!(!staged.path.exists());
        let path = fixture.path("conflict");
        let output = PreparedOutput::inspect(&path).unwrap();
        let (staged, mut file) = output.stage(b"owned").unwrap();
        write_new(&path, b"foreign").unwrap();
        let inode = fs::metadata(&path).unwrap().ino();
        assert!(matches!(
            output.commit_stage(&staged, &mut file, b"owned"),
            Err(ArtifactError::Uncertain(_))
        ));
        assert_eq!(fs::read(&path).unwrap(), b"foreign");
        assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
        assert_eq!(fs::read(&staged.path).unwrap(), b"owned");
    }

    #[test]
    fn atomic_publication_refuses_replaced_stage_and_revalidates_parent() {
        let fixture = Fixture::new();
        let path = fixture.path("manifest");
        let output = PreparedOutput::inspect(&path).unwrap();
        let (staged, mut file) = output.stage(b"owned").unwrap();
        let kept = fixture.path("original");
        fs::rename(&staged.path, &kept).unwrap();
        write_new(&staged.path, b"other").unwrap();
        assert!(output.commit_stage(&staged, &mut file, b"owned").is_err());
        assert!(!path.exists());
        assert_eq!(fs::read(&staged.path).unwrap(), b"other");
        assert_eq!(fs::read(&kept).unwrap(), b"owned");
        let fresh = fixture.path("atomic");
        PreparedOutput::inspect(&fresh)
            .unwrap()
            .publish_atomic(b"fresh")
            .unwrap();
        assert_eq!(read_private(&fresh).unwrap(), "fresh");
        assert!(
            PreparedOutput::inspect(&fixture.path("large"))
                .unwrap()
                .publish_atomic(&vec![0; MAX_RESOURCE_PROFILE_BYTES + 1])
                .is_err()
        );
    }
}
