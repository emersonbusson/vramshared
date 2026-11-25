// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_stage.rs
//! Exclusive, descriptor-held storage for one staged package member.

use rustix::fs::{Mode, OFlags, openat};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use super::transition_custody::{
    FileIdentity, open_directory_no_follow, validate_directory, validate_regular,
};
use crate::release_identity::open_regular_at;

pub(super) struct StageSlot {
    parent_path: PathBuf,
    parent: File,
    parent_identity: FileIdentity,
    name: String,
    file: File,
    file_identity: FileIdentity,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum StageError {
    /// No content write was attempted; any existing stage is retained.
    Refused,
    /// An effect or custody check is uncertain; preserve the stage for recovery.
    RecoveryRequired,
}

/// Copy and read back an approved member; failure retains the staging inode.
pub(super) fn copy_verified(
    mut source: File,
    expected: &str,
    slot: &mut StageSlot,
    limit: u64,
) -> Result<u64, StageError> {
    let source_metadata = source.metadata().map_err(|_| StageError::Refused)?;
    let size = source_metadata.len();
    if size > limit {
        return Err(StageError::Refused);
    }
    let target_metadata = slot
        .file_mut()
        .metadata()
        .map_err(|_| StageError::RecoveryRequired)?;
    if (target_metadata.dev(), target_metadata.ino())
        == (source_metadata.dev(), source_metadata.ino())
    {
        return Err(StageError::RecoveryRequired);
    }
    source
        .seek(SeekFrom::Start(0))
        .map_err(|_| StageError::Refused)?;
    copy_stream(source, size, expected, slot)
}

pub(super) fn copy_verified_bytes(
    bytes: &[u8],
    expected: &str,
    slot: &mut StageSlot,
    limit: u64,
) -> Result<u64, StageError> {
    let size = u64::try_from(bytes.len()).map_err(|_| StageError::Refused)?;
    if size > limit {
        return Err(StageError::Refused);
    }
    copy_stream(bytes, size, expected, slot)
}

/// Continue an interrupted copy without truncating or replacing its inode.
pub(super) fn resume_verified(
    mut source: File,
    expected: &str,
    slot: &mut StageSlot,
    limit: u64,
) -> Result<u64, StageError> {
    let metadata = source.metadata().map_err(|_| StageError::Refused)?;
    let target = slot
        .file_mut()
        .metadata()
        .map_err(|_| StageError::RecoveryRequired)?;
    if metadata.len() > limit || (metadata.dev(), metadata.ino()) == (target.dev(), target.ino()) {
        return Err(StageError::Refused);
    }
    resume_stream(&mut source, metadata.len(), expected, slot)
}

pub(super) fn resume_verified_bytes(
    bytes: &[u8],
    expected: &str,
    slot: &mut StageSlot,
    limit: u64,
) -> Result<u64, StageError> {
    let size = u64::try_from(bytes.len()).map_err(|_| StageError::Refused)?;
    if size > limit {
        return Err(StageError::Refused);
    }
    resume_stream(&mut std::io::Cursor::new(bytes), size, expected, slot)
}

fn resume_stream(
    source: &mut (impl Read + Seek),
    size: u64,
    expected: &str,
    slot: &mut StageSlot,
) -> Result<u64, StageError> {
    source
        .seek(SeekFrom::Start(0))
        .map_err(|_| StageError::Refused)?;
    if crate::installed_image::sha256_reader_with_limit(source, size).as_deref() != Some(expected) {
        return Err(StageError::Refused);
    }
    source
        .seek(SeekFrom::Start(0))
        .map_err(|_| StageError::Refused)?;
    slot.revalidate().ok_or(StageError::RecoveryRequired)?;
    let target = slot.file_mut();
    let prefix = target
        .metadata()
        .map_err(|_| StageError::RecoveryRequired)?
        .len();
    if prefix > size {
        return Err(StageError::Refused);
    }
    compare_prefix(source, target, prefix)?;
    if target
        .metadata()
        .map_err(|_| StageError::RecoveryRequired)?
        .len()
        != prefix
    {
        return Err(StageError::RecoveryRequired);
    }
    let mut source_buffer = [0u8; 64 * 1024];
    let mut remaining = size - prefix;
    while remaining != 0 {
        let count = usize::try_from(remaining.min(source_buffer.len() as u64))
            .map_err(|_| StageError::Refused)?;
        source
            .read_exact(&mut source_buffer[..count])
            .map_err(|_| StageError::RecoveryRequired)?;
        target
            .write_all(&source_buffer[..count])
            .map_err(|_| StageError::RecoveryRequired)?;
        remaining -= count as u64;
    }
    let mut extra = [0u8; 1];
    if source
        .read(&mut extra)
        .map_err(|_| StageError::RecoveryRequired)?
        != 0
        || target
            .metadata()
            .map_err(|_| StageError::RecoveryRequired)?
            .len()
            != size
    {
        return Err(StageError::RecoveryRequired);
    }
    target
        .sync_all()
        .map_err(|_| StageError::RecoveryRequired)?;
    slot.verify_bytes(expected, size)
        .ok_or(StageError::RecoveryRequired)?;
    slot.sync_directory().ok_or(StageError::RecoveryRequired)?;
    Ok(size)
}

fn compare_prefix(
    source: &mut impl Read,
    target: &mut File,
    prefix: u64,
) -> Result<(), StageError> {
    target
        .seek(SeekFrom::Start(0))
        .map_err(|_| StageError::RecoveryRequired)?;
    let mut source_buffer = [0u8; 64 * 1024];
    let mut target_buffer = [0u8; 64 * 1024];
    let mut left = prefix;
    while left != 0 {
        let count = usize::try_from(left.min(source_buffer.len() as u64))
            .map_err(|_| StageError::Refused)?;
        source
            .read_exact(&mut source_buffer[..count])
            .map_err(|_| StageError::Refused)?;
        target
            .read_exact(&mut target_buffer[..count])
            .map_err(|_| StageError::RecoveryRequired)?;
        if source_buffer[..count] != target_buffer[..count] {
            return Err(StageError::Refused);
        }
        left -= count as u64;
    }
    Ok(())
}

fn copy_stream(
    mut source: impl Read,
    size: u64,
    expected: &str,
    slot: &mut StageSlot,
) -> Result<u64, StageError> {
    use sha2::{Digest, Sha256};
    slot.revalidate().ok_or(StageError::RecoveryRequired)?;
    let copied = (|| -> Option<()> {
        let target = slot.file_mut();
        let target_metadata = target.metadata().ok()?;
        if target_metadata.len() != 0 {
            return None;
        }
        target.seek(SeekFrom::Start(0)).ok()?;
        let mut left = size;
        let mut buffer = [0u8; 64 * 1024];
        let mut hasher = Sha256::new();
        while left != 0 {
            let chunk = usize::try_from(left.min(buffer.len() as u64)).ok()?;
            source.read_exact(&mut buffer[..chunk]).ok()?;
            target.write_all(&buffer[..chunk]).ok()?;
            hasher.update(&buffer[..chunk]);
            left -= chunk as u64;
        }
        let mut extra = [0u8; 1];
        let actual = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        if source.read(&mut extra).ok()? != 0 || actual != expected {
            return None;
        }
        target.sync_all().ok()?;
        target.seek(SeekFrom::Start(0)).ok()?;
        if crate::installed_image::sha256_reader_with_limit(&mut *target, size).as_deref()
            != Some(expected)
            || target.metadata().ok()?.len() != size
        {
            return None;
        }
        Some(())
    })();
    copied.ok_or(StageError::RecoveryRequired)?;
    slot.revalidate().ok_or(StageError::RecoveryRequired)?;
    slot.sync_directory().ok_or(StageError::RecoveryRequired)?;
    Ok(size)
}

impl StageSlot {
    /// Reopen without creating; the recorded inode is mandatory for a stage.
    pub(super) fn open_existing(
        parent_path: &Path,
        name: &str,
        expected: Option<(u64, u64)>,
    ) -> Option<Self> {
        if !parent_path.is_absolute()
            || name.is_empty()
            || matches!(name, "." | "..")
            || name.contains('/')
            || name.chars().any(char::is_control)
        {
            return None;
        }
        let parent = open_directory_no_follow(parent_path, false).ok()??;
        let parent_identity = validate_directory(&parent, "package staging directory").ok()?;
        let file = File::from(
            openat(
                &parent,
                name,
                OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
                Mode::empty(),
            )
            .ok()?,
        );
        let file_identity = validate_regular(&file, "package staging file").ok()?;
        let slot = Self {
            parent_path: parent_path.to_path_buf(),
            parent,
            parent_identity,
            name: name.into(),
            file,
            file_identity,
        };
        let actual = slot.identity()?;
        if expected.is_some_and(|wanted| actual != wanted) {
            return None;
        }
        Some(slot)
    }

    /// Create a new private name, never adopting or replacing an existing one.
    pub(super) fn create(parent_path: &Path, name: &str) -> Result<Self, StageError> {
        Self::create_with_parent(parent_path, name, None)
    }

    /// Create only beneath the directory identity durably recorded in the intent.
    pub(super) fn create_for_parent(
        parent_path: &Path,
        name: &str,
        expected_parent: (u64, u64),
    ) -> Result<Self, StageError> {
        Self::create_with_parent(parent_path, name, Some(expected_parent))
    }

    fn create_with_parent(
        parent_path: &Path,
        name: &str,
        expected_parent: Option<(u64, u64)>,
    ) -> Result<Self, StageError> {
        if !parent_path.is_absolute()
            || name.is_empty()
            || name == "."
            || name == ".."
            || name.contains('/')
            || name.chars().any(char::is_control)
        {
            return Err(StageError::Refused);
        }
        let parent = open_directory_no_follow(parent_path, false)
            .map_err(|_| StageError::Refused)?
            .ok_or(StageError::Refused)?;
        let parent_identity = validate_directory(&parent, "package staging directory")
            .map_err(|_| StageError::Refused)?;
        let metadata = parent.metadata().map_err(|_| StageError::Refused)?;
        if expected_parent.is_some_and(|expected| expected != (metadata.dev(), metadata.ino())) {
            return Err(StageError::Refused);
        }
        let file = File::from(
            openat(
                &parent,
                name,
                OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(|error| {
                if error == rustix::io::Errno::EXIST {
                    StageError::Refused
                } else {
                    StageError::RecoveryRequired
                }
            })?,
        );
        let file_identity = validate_regular(&file, "package staging file")
            .map_err(|_| StageError::RecoveryRequired)?;
        let slot = Self {
            parent_path: parent_path.to_path_buf(),
            parent,
            parent_identity,
            name: name.into(),
            file,
            file_identity,
        };
        slot.revalidate().ok_or(StageError::RecoveryRequired)?;
        slot.sync_directory().ok_or(StageError::RecoveryRequired)?;
        Ok(slot)
    }

    pub(super) fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }

    pub(super) fn identity(&self) -> Option<(u64, u64)> {
        self.revalidate()?;
        let metadata = self.file.metadata().ok()?;
        Some((metadata.dev(), metadata.ino()))
    }

    pub(super) fn parent_identity(&self) -> Option<(u64, u64)> {
        self.revalidate()?;
        let metadata = self.parent.metadata().ok()?;
        Some((metadata.dev(), metadata.ino()))
    }

    /// Recheck the held bytes and name; a later effect must recheck again.
    pub(super) fn verify_bytes(&mut self, expected: &str, limit: u64) -> Option<()> {
        self.revalidate()?;
        let before = self.file.metadata().ok()?;
        if before.len() > limit {
            return None;
        }
        self.file.seek(SeekFrom::Start(0)).ok()?;
        if crate::installed_image::sha256_reader_with_limit(&mut self.file, before.len()).as_deref()
            != Some(expected)
        {
            return None;
        }
        let after = self.file.metadata().ok()?;
        if before.len() != after.len() {
            return None;
        }
        self.revalidate()
    }

    /// Read a bounded prefix, optionally requiring the entire file to fit.
    pub(super) fn read_prefix(&mut self, limit: usize, whole: bool) -> Option<Vec<u8>> {
        self.revalidate()?;
        let size = self.file.metadata().ok()?.len();
        let limit = u64::try_from(limit).ok()?;
        if whole && size > limit {
            return None;
        }
        let count = usize::try_from(size.min(limit)).ok()?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(count).ok()?;
        bytes.resize(count, 0);
        self.file.seek(SeekFrom::Start(0)).ok()?;
        self.file.read_exact(&mut bytes).ok()?;
        if self.file.metadata().ok()?.len() != size {
            return None;
        }
        self.revalidate()?;
        Some(bytes)
    }

    pub(super) fn sync_file_and_directory(&self) -> Option<()> {
        self.revalidate()?;
        self.file.sync_all().ok()?;
        self.sync_directory()?;
        self.revalidate()
    }

    /// Verify both the named directory and named inode still match held handles.
    pub(super) fn revalidate(&self) -> Option<()> {
        let parent = open_directory_no_follow(&self.parent_path, false).ok()??;
        if validate_directory(&parent, "package staging directory").ok()? != self.parent_identity {
            return None;
        }
        let named = open_regular_at(&self.parent, &self.name)?;
        if validate_regular(&named, "package staging file").ok()? != self.file_identity
            || validate_regular(&self.file, "package staging file").ok()? != self.file_identity
        {
            return None;
        }
        Some(())
    }

    pub(super) fn sync_directory(&self) -> Option<()> {
        self.parent.sync_all().ok()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn partial_stage_resumes_only_an_authenticated_matching_prefix() {
        let root =
            std::env::temp_dir().join(format!("ramshared-partial-stage-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let data = b"approved contents";
        let digest = crate::installed_image::sha256_reader(&mut data.as_slice()).unwrap();
        let source_path = root.join("source");
        std::fs::write(&source_path, data).unwrap();
        let mut slot = StageSlot::create(&root, "stage").unwrap();
        slot.file_mut().write_all(b"wrong").unwrap();
        assert!(
            resume_verified(File::open(&source_path).unwrap(), &digest, &mut slot, 32).is_err()
        );
        assert_eq!(std::fs::read(root.join("stage")).unwrap(), b"wrong");
        slot.file_mut().set_len(0).unwrap();
        slot.file_mut().seek(SeekFrom::Start(0)).unwrap();
        slot.file_mut().write_all(b"appro").unwrap();
        let original = std::fs::read(root.join("stage")).unwrap();
        assert!(
            resume_verified(
                File::open(&source_path).unwrap(),
                &"0".repeat(64),
                &mut slot,
                32
            )
            .is_err()
        );
        assert!(resume_verified(File::open(&source_path).unwrap(), &digest, &mut slot, 2).is_err());
        assert_eq!(std::fs::read(root.join("stage")).unwrap(), original);
        assert!(
            resume_verified(
                File::open(root.join("stage")).unwrap(),
                &digest,
                &mut slot,
                32
            )
            .is_err()
        );
        assert_eq!(
            resume_verified(File::open(&source_path).unwrap(), &digest, &mut slot, 32).unwrap(),
            data.len() as u64
        );
        assert_eq!(std::fs::read(root.join("stage")).unwrap(), data);
        assert_eq!(
            resume_verified_bytes(data, &digest, &mut slot, 32).unwrap(),
            data.len() as u64
        );
        assert!(resume_verified_bytes(data, &digest, &mut slot, 2).is_err());
        slot.file_mut().seek(SeekFrom::End(0)).unwrap();
        slot.file_mut().write_all(b"extra").unwrap();
        let extended = std::fs::read(root.join("stage")).unwrap();
        assert!(resume_verified_bytes(data, &digest, &mut slot, 32).is_err());
        assert_eq!(std::fs::read(root.join("stage")).unwrap(), extended);
        drop(slot);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn partial_resume_can_be_retried_after_an_interrupted_append() {
        struct Interrupted {
            cursor: std::io::Cursor<Vec<u8>>,
            passes: usize,
        }
        impl Read for Interrupted {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                if self.passes >= 2 && self.cursor.position() >= 3 + 64 * 1024 {
                    return Err(std::io::Error::other("interrupted source"));
                }
                self.cursor.read(bytes)
            }
        }
        impl Seek for Interrupted {
            fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
                if matches!(position, SeekFrom::Start(0)) {
                    self.passes += 1;
                }
                self.cursor.seek(position)
            }
        }
        let root =
            std::env::temp_dir().join(format!("ramshared-partial-retry-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let data = vec![42u8; 64 * 1024 + 32];
        let digest = crate::installed_image::sha256_reader(&mut data.as_slice()).unwrap();
        let mut slot = StageSlot::create(&root, "stage").unwrap();
        slot.file_mut().write_all(&data[..3]).unwrap();
        let mut interrupted = Interrupted {
            cursor: std::io::Cursor::new(data.clone()),
            passes: 0,
        };
        assert_eq!(
            resume_stream(&mut interrupted, data.len() as u64, &digest, &mut slot),
            Err(StageError::RecoveryRequired)
        );
        let partial = std::fs::read(root.join("stage")).unwrap();
        assert!(partial.len() > 3 && partial.len() < data.len());
        assert_eq!(partial, data[..partial.len()]);
        assert_eq!(
            resume_verified_bytes(&data, &digest, &mut slot, data.len() as u64).unwrap(),
            data.len() as u64
        );
        assert_eq!(std::fs::read(root.join("stage")).unwrap(), data);
        drop(slot);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "subprocess fixture invoked by partial_resume_survives_process_kill"]
    fn partial_resume_kill_child() {
        struct BlockAfterAppend {
            file: File,
            passes: usize,
        }
        impl Read for BlockAfterAppend {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                if self.passes >= 2 && self.file.stream_position()? >= 3 + 64 * 1024 {
                    println!("RAMSHARED_PARTIAL_APPEND_READY");
                    std::io::stdout().flush()?;
                    let mut signal = [0u8; 1];
                    let _ = std::io::stdin().read(&mut signal);
                    return Err(std::io::Error::other("interrupted fixture"));
                }
                self.file.read(bytes)
            }
        }
        impl Seek for BlockAfterAppend {
            fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
                if matches!(position, SeekFrom::Start(0)) {
                    self.passes += 1;
                }
                self.file.seek(position)
            }
        }
        let root =
            std::path::PathBuf::from(std::env::var_os("RAMSHARED_PARTIAL_CHILD_DIR").unwrap());
        let source_path = root.join("source");
        let data = std::fs::read(&source_path).unwrap();
        let digest = crate::installed_image::sha256_reader(&mut data.as_slice()).unwrap();
        let mut source = BlockAfterAppend {
            file: File::open(source_path).unwrap(),
            passes: 0,
        };
        let mut slot = StageSlot::open_existing(&root, "stage", None).unwrap();
        assert!(resume_stream(&mut source, data.len() as u64, &digest, &mut slot).is_err());
    }

    #[test]
    fn partial_resume_survives_process_kill() {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};
        use std::sync::mpsc;
        use std::time::Duration;

        let root =
            std::env::temp_dir().join(format!("ramshared-partial-kill-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let data = vec![53u8; 64 * 1024 + 32];
        let digest = crate::installed_image::sha256_reader(&mut data.as_slice()).unwrap();
        std::fs::write(root.join("source"), &data).unwrap();
        let mut slot = StageSlot::create(&root, "stage").unwrap();
        slot.file_mut().write_all(&data[..3]).unwrap();
        slot.sync_file_and_directory().unwrap();
        let identity = slot.identity().unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "customer::package_stage::tests::partial_resume_kill_child",
                "--ignored",
                "--nocapture",
            ])
            .env("RAMSHARED_PARTIAL_CHILD_DIR", &root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (send, receive) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if line.contains("RAMSHARED_PARTIAL_APPEND_READY") {
                    let _ = send.send(());
                    break;
                }
            }
        });
        let ready = receive.recv_timeout(Duration::from_secs(15));
        let killed = child.kill();
        let status = child.wait();
        reader.join().unwrap();
        assert!(ready.is_ok(), "child did not reach the interrupted append");
        assert!(killed.is_ok());
        assert!(!status.unwrap().success());
        assert_eq!(slot.identity(), Some(identity));
        let partial = std::fs::read(root.join("stage")).unwrap();
        assert!(partial.len() > 3 && partial.len() < data.len());
        assert_eq!(partial, data[..partial.len()]);
        assert_eq!(
            resume_verified(
                File::open(root.join("source")).unwrap(),
                &digest,
                &mut slot,
                data.len() as u64
            )
            .unwrap(),
            data.len() as u64
        );
        assert_eq!(std::fs::read(root.join("stage")).unwrap(), data);
        drop(slot);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stage_slot_refuses_existing_names_and_detects_replacement() {
        let root =
            std::env::temp_dir().join(format!("ramshared-stage-slot-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        assert!(matches!(
            StageSlot::create(&root, "../escape"),
            Err(StageError::Refused)
        ));
        assert!(StageSlot::create(&root, "bad\nname").is_err());
        assert!(StageSlot::create(Path::new("relative"), "name").is_err());
        assert!(StageSlot::create(&root.join("missing"), "name").is_err());
        let mut slot = StageSlot::create(&root, "pending").unwrap();
        slot.file_mut().write_all(b"abcdef").unwrap();
        assert_eq!(slot.read_prefix(3, false).unwrap(), b"abc");
        assert!(slot.read_prefix(3, true).is_none());
        assert_eq!(slot.read_prefix(6, true).unwrap(), b"abcdef");
        assert!(slot.revalidate().is_some());
        assert!(matches!(
            StageSlot::create(&root, "pending"),
            Err(StageError::Refused)
        ));
        std::fs::rename(root.join("pending"), root.join("original")).unwrap();
        std::fs::write(root.join("pending"), b"foreign").unwrap();
        assert!(slot.revalidate().is_none());
        assert_eq!(std::fs::read(root.join("pending")).unwrap(), b"foreign");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stage_slot_detects_parent_replacement() {
        let root =
            std::env::temp_dir().join(format!("ramshared-stage-parent-{}", std::process::id()));
        let moved = root.with_extension("moved");
        std::fs::create_dir(&root).unwrap();
        let slot = StageSlot::create(&root, "pending").unwrap();
        std::fs::rename(&root, &moved).unwrap();
        std::fs::create_dir(&root).unwrap();
        assert!(slot.revalidate().is_none());
        assert!(moved.join("pending").exists());
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(moved).unwrap();
    }
}
