// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/swap_absorber.rs
//! Descriptor-owned Linux emergency swap backing (SPEC swap-drain-safety).

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

use rustix::fs::{
    AtFlags, FileType, Mode, OFlags, ResolveFlags, StatxFlags, XattrFlags, fgetxattr, fsetxattr,
    statat, unlinkat,
};
use serde::{Deserialize, Serialize};

const RETAINED_TAG: &str = "user.ramshared-absorber";
const PARENT_TAG: &str = "user.ramshared-absorber-parent";

/// Stable identity of an owner-controlled directory containing an absorber.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AbsorberParentIdentity {
    tag: [u8; 32],
    ino: u64,
    birth_seconds: i64,
    birth_nanos: u32,
}

impl AbsorberParentIdentity {
    fn observe(directory: &File) -> io::Result<Option<Self>> {
        let Some(tag) = read_tag(directory, PARENT_TAG)? else {
            return Ok(None);
        };
        let stat = rustix::fs::statx(
            directory,
            "",
            AtFlags::EMPTY_PATH,
            StatxFlags::INO | StatxFlags::BTIME,
        )?;
        let observed = StatxFlags::from_bits_retain(stat.stx_mask);
        if !observed.contains(StatxFlags::INO | StatxFlags::BTIME)
            || stat.stx_ino == 0
            || (stat.stx_btime.tv_sec == 0 && stat.stx_btime.tv_nsec == 0)
        {
            return Ok(None);
        }
        Ok(Some(Self {
            tag,
            ino: stat.stx_ino,
            birth_seconds: stat.stx_btime.tv_sec,
            birth_nanos: stat.stx_btime.tv_nsec,
        }))
    }
}

/// Guest filesystem capacity alone is authoritative only on measured native
/// Linux. WSL needs a separate, fresh proof of the VHDX host volume; until
/// that proof is wired into both release owners, refuse guest-only admission.
pub fn guest_only_capacity_is_admissible(platform: Option<crate::platform::PlatformKind>) -> bool {
    matches!(platform, Some(crate::platform::PlatformKind::NativeLinux))
}

/// Exact formatted backing for a same-boot lifecycle record.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedAbsorberIdentity {
    path: PathBuf,
    dev: u64,
    ino: u64,
    uid: u32,
    size_bytes: u64,
    mount_id: u64,
    birth_seconds: i64,
    birth_nanos: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tag: Option<[u8; 32]>,
}

impl RetainedAbsorberIdentity {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Bind an original file across reboot without trusting recycled device
    /// or mount numbers. The caller must independently prove inactivity.
    pub fn matches_other_boot(&self, current: &Self) -> bool {
        let mut expected = self.clone();
        expected.dev = current.dev;
        expected.mount_id = current.mount_id;
        self.tag.is_some() && expected == *current
    }

    /// An absent file is conclusive only under the same mounted parent.
    pub fn prove_absent(&self) -> io::Result<()> {
        self.absent_directory().map(drop)
    }

    /// Commit a previously attempted unlink before retiring its receipt.
    pub fn sync_absence(&self) -> io::Result<()> {
        self.absent_directory()?.sync_all()
    }

    /// Verify a previously removed name and withhold authority on a live alias.
    pub fn prove_prior_boot_absence<'a>(
        &self,
        expected: &AbsorberParentIdentity,
        swap_paths: impl IntoIterator<Item = &'a str>,
        synchronize: bool,
    ) -> io::Result<()> {
        let path = self.canonical_path()?;
        let (directory, name) = open_parent(path)?;
        let parent = directory.metadata()?;
        if self.tag.is_none()
            || parent.uid() != self.uid
            || parent.mode() & 0o022 != 0
            || AbsorberParentIdentity::observe(&directory)?.as_ref() != Some(expected)
        {
            return Err(io::Error::other("prior-boot absorber parent changed"));
        }
        for row in swap_paths {
            let swap = Path::new(row);
            if !swap.is_absolute() || row.contains('\\') || row.contains(char::is_whitespace) {
                return Err(io::Error::other(
                    "prior-boot swap row cannot be resolved exactly",
                ));
            }
            if swap == path {
                return Err(io::Error::other("prior-boot absorber is listed as active"));
            }
            let observed = std::fs::metadata(swap)?;
            if observed.dev() == parent.dev() && observed.ino() == self.ino {
                return Err(io::Error::other("prior-boot absorber has an active alias"));
            }
        }
        verify_absent(&directory, name)?;
        if synchronize {
            directory.sync_all()?;
        }
        Ok(())
    }

    fn absent_directory(&self) -> io::Result<File> {
        let path = self.canonical_path()?;
        let (directory, name) = open_parent(path)?;
        let stat = rustix::fs::statx(&directory, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID)?;
        if !StatxFlags::from_bits_retain(stat.stx_mask).contains(StatxFlags::MNT_ID)
            || stat.stx_mnt_id != self.mount_id
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "absorber mount changed",
            ));
        }
        verify_absent(&directory, name)?;
        Ok(directory)
    }

    fn canonical_path(&self) -> io::Result<&Path> {
        let path = &self.path;
        if !path.is_absolute()
            || path.file_name().is_none()
            || path.components().any(|part| part == Component::ParentDir)
            || path.components().collect::<PathBuf>().as_os_str() != path.as_os_str()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "retained absorber path must be absolute and canonical",
            ));
        }
        Ok(path)
    }
}

#[derive(Debug)]
pub struct SwapAbsorber {
    file: File,
    path: PathBuf,
    dev: u64,
    ino: u64,
    uid: u32,
}

impl SwapAbsorber {
    /// Read-only test of the filesystem class before reserving an inode.
    pub fn check_directory_filesystem(path: &Path) -> io::Result<()> {
        use rustix::fs::{Mode, OFlags};
        let directory = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Self::validate_filesystem_type(rustix::fs::fstatfs(&directory)?.f_type as u32)
    }

    pub fn reserve(path: &Path) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "absorber path must be absolute",
            ));
        }
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        let meta = file.metadata()?;
        let backing = Self {
            file,
            path: path.to_path_buf(),
            dev: meta.dev(),
            ino: meta.ino(),
            uid: meta.uid(),
        };
        backing.validate()?;
        if let Err(error) = backing.validate_filesystem() {
            // Activation has not been attempted; remove only our reserved inode.
            backing.remove_inactive(|| true)?;
            return Err(error);
        }
        Ok(backing)
    }

    /// Stamp a private recorded file before activation. Keep a valid old
    /// stamp if publication is retried on the same descriptor.
    pub fn seal_retained_identity(&self) -> io::Result<RetainedAbsorberIdentity> {
        self.validate()?;
        let (directory, _) = self.pin_named_parent()?;
        ensure_tag(&self.file, RETAINED_TAG)?;
        self.file.sync_all()?;
        directory.sync_all()?;
        self.retained_identity()
    }

    /// Record a portable witness when the containing directory supports it.
    pub fn seal_parent_identity(&self) -> io::Result<Option<AbsorberParentIdentity>> {
        let (directory, _) = self.pin_named_parent()?;
        let parent = directory.metadata()?;
        if parent.dev() != self.dev || parent.uid() != self.uid || parent.mode() & 0o022 != 0 {
            return Ok(None);
        }
        ensure_tag(&directory, PARENT_TAG)?;
        let identity = AbsorberParentIdentity::observe(&directory)?;
        if identity.is_some() {
            directory.sync_all()?;
        }
        Ok(identity)
    }

    /// Capture the exact backing after formatting and before activation.
    pub fn retained_identity(&self) -> io::Result<RetainedAbsorberIdentity> {
        self.validate()?;
        self.validate_filesystem()?;
        let stat = rustix::fs::statx(
            &self.file,
            "",
            AtFlags::EMPTY_PATH,
            StatxFlags::BTIME | StatxFlags::MNT_ID | StatxFlags::SIZE,
        )?;
        let observed = StatxFlags::from_bits_retain(stat.stx_mask);
        if !observed.contains(StatxFlags::BTIME | StatxFlags::MNT_ID | StatxFlags::SIZE)
            || stat.stx_mnt_id == 0
            || stat.stx_size == 0
            || (stat.stx_btime.tv_sec == 0 && stat.stx_btime.tv_nsec == 0)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "formatted absorber identity is unavailable",
            ));
        }
        Ok(RetainedAbsorberIdentity {
            path: self.path.clone(),
            dev: self.dev,
            ino: self.ino,
            uid: self.uid,
            size_bytes: stat.stx_size,
            mount_id: stat.stx_mnt_id,
            birth_seconds: stat.stx_btime.tv_sec,
            birth_nanos: stat.stx_btime.tv_nsec,
            tag: read_tag(&self.file, RETAINED_TAG)?,
        })
    }

    /// Reopen only the exact backing from an authorized same-boot record.
    /// This binds a descriptor. The caller still owns proof of record custody,
    /// current boot, live swap state and authorization for any later effect.
    pub fn reopen_retained(expected: &RetainedAbsorberIdentity) -> io::Result<Self> {
        Self::reopen(expected, false)
    }

    /// Restore only the original inode for a current-boot inactivity check.
    /// This API provides no authority to reactivate or credit an old swap.
    pub fn reopen_prior_boot(expected: &RetainedAbsorberIdentity) -> io::Result<Self> {
        Self::reopen(expected, true)
    }

    fn reopen(expected: &RetainedAbsorberIdentity, prior_boot: bool) -> io::Result<Self> {
        let path = expected.canonical_path()?;
        let file = rustix::fs::openat2(
            rustix::fs::CWD,
            path,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
            ResolveFlags::NO_SYMLINKS,
        )?;
        let file = File::from(file);
        let metadata = file.metadata()?;
        let backing = Self {
            file,
            path: path.to_owned(),
            dev: metadata.dev(),
            ino: metadata.ino(),
            uid: metadata.uid(),
        };
        let observed = backing.retained_identity()?;
        let matches = if prior_boot {
            expected.matches_other_boot(&observed)
        } else {
            observed == *expected
        };
        if !matches {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "retained absorber no longer matches its recorded identity",
            ));
        }
        Ok(backing)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn file(&self) -> &File {
        &self.file
    }

    /// Helpers open the parent's pinned descriptor; no inherited-fd exception
    /// or close-on-exec change is required. Named identity is checked separately.
    pub fn effect_path(&self) -> PathBuf {
        PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            self.file.as_raw_fd()
        ))
    }

    pub fn validate(&self) -> io::Result<()> {
        for meta in [
            self.file.metadata()?,
            std::fs::symlink_metadata(&self.path)?,
        ] {
            if !meta.is_file()
                || meta.dev() != self.dev
                || meta.ino() != self.ino
                || meta.uid() != self.uid
                || meta.mode() & 0o7777 != 0o600
                || meta.nlink() != 1
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "absorber identity or custody changed",
                ));
            }
        }
        Ok(())
    }

    fn validate_filesystem(&self) -> io::Result<()> {
        Self::validate_filesystem_type(rustix::fs::fstatfs(&self.file)?.f_type as u32)
    }

    fn validate_filesystem_type(filesystem_type: u32) -> io::Result<()> {
        // ext2/3/4 share a magic value. Btrfs needs a separate NOCOW and
        // extent-placement contract; tmpfs, network and Windows mounts are refused.
        const EXT_SUPER_MAGIC: u32 = 0xef53;
        const XFS_SUPER_MAGIC: u32 = 0x5846_5342;
        if !matches!(filesystem_type, EXT_SUPER_MAGIC | XFS_SUPER_MAGIC) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "emergency swap requires an ext or XFS filesystem",
            ));
        }
        Ok(())
    }

    pub fn matches_file(&self, path: &Path) -> io::Result<bool> {
        self.validate()?;
        let meta = std::fs::metadata(path)?;
        Ok(meta.dev() == self.dev && meta.ino() == self.ino)
    }

    /// Caller supplies a fresh, strictly parsed complete swap snapshot in bytes.
    pub fn free_bytes<'a>(
        &self,
        rows: impl IntoIterator<Item = (&'a str, u64, u64)>,
    ) -> Option<u64> {
        self.validate().ok()?;
        let capacity = self.file.metadata().ok()?.len();
        let name = self.path.to_str()?;
        let mut found = None;
        for (path, size, used) in rows {
            if path == name
                && (size > capacity
                    || used > size
                    || found.replace(size.checked_sub(used)?).is_some())
            {
                return None;
            }
        }
        found
    }

    /// The lifecycle owner supplies a fresh proof of inactivity (or proves
    /// activation was never attempted). Drop itself never removes backing.
    pub fn remove_inactive(&self, inactive: impl FnOnce() -> bool) -> io::Result<()> {
        self.remove_inactive_with_sync(inactive, File::sync_all)
    }

    fn remove_inactive_with_sync(
        &self,
        inactive: impl FnOnce() -> bool,
        sync_directory: impl FnOnce(&File) -> io::Result<()>,
    ) -> io::Result<()> {
        if !inactive() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "absorber inactivity unproven",
            ));
        }
        self.validate()?;
        let (directory, name) = self.pin_named_parent()?;
        unlinkat(&directory, name, AtFlags::empty())?;
        sync_directory(&directory)
    }

    fn pin_named_parent(&self) -> io::Result<(File, &std::ffi::OsStr)> {
        let (directory, name) = open_parent(&self.path)?;
        let named = statat(&directory, name, AtFlags::SYMLINK_NOFOLLOW)?;
        if named.st_dev != self.dev
            || named.st_ino != self.ino
            || named.st_uid != self.uid
            || FileType::from_raw_mode(named.st_mode) != FileType::RegularFile
            || named.st_mode & 0o7777 != 0o600
            || named.st_nlink != 1
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "absorber identity changed under its parent",
            ));
        }
        Ok((directory, name))
    }
}

fn open_parent(path: &Path) -> io::Result<(File, &std::ffi::OsStr)> {
    let (parent, name) = path.parent().zip(path.file_name()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "absorber parent or file name is missing",
        )
    })?;
    let directory = rustix::fs::openat2(
        rustix::fs::CWD,
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::NO_SYMLINKS,
    )?;
    Ok((directory.into(), name))
}

fn verify_absent(directory: &File, name: &std::ffi::OsStr) -> io::Result<()> {
    match statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
        Err(rustix::io::Errno::NOENT) => Ok(()),
        Err(error) => Err(error.into()),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "absorber path is present",
        )),
    }
}

fn ensure_tag(file: &File, name: &str) -> io::Result<()> {
    if read_tag(file, name)?.is_some() {
        return Ok(());
    }
    let mut tag = [0u8; 32];
    File::open("/dev/urandom")?.read_exact(&mut tag)?;
    match fsetxattr(file, name, &tag, XattrFlags::CREATE) {
        Ok(()) | Err(rustix::io::Errno::EXIST | rustix::io::Errno::NOTSUP) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn read_tag(file: &File, name: &str) -> io::Result<Option<[u8; 32]>> {
    let mut tag = [0u8; 32];
    match fgetxattr(file, name, &mut tag[..]) {
        Ok(size) if size == tag.len() && tag != [0; 32] => Ok(Some(tag)),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid retained absorber tag",
        )),
        Err(rustix::io::Errno::NODATA | rustix::io::Errno::NOTSUP) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    const MIB: u64 = 1024 * 1024;
    static SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "ramshared-absorber-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn absorber(&self) -> SwapAbsorber {
            let backing = SwapAbsorber::reserve(&self.0.join("owned.swap")).unwrap();
            backing.file().set_len(4 * MIB).unwrap();
            backing
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn absorber_reservation_is_exclusive_private_and_pinned() {
        let dir = Fixture::new();
        let backing = dir.absorber();
        assert_eq!(
            backing.file().metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        backing.validate().unwrap();
        assert!(SwapAbsorber::reserve(backing.path()).is_err());
        assert_eq!(
            std::fs::metadata(backing.effect_path()).unwrap().ino(),
            backing.file().metadata().unwrap().ino()
        );
        let link = dir.0.join("link.swap");
        symlink(backing.path(), &link).unwrap();
        assert!(SwapAbsorber::reserve(&link).is_err());
    }

    #[test]
    fn retained_absorber_reopens_after_creator_releases_its_descriptor() {
        let dir = Fixture::new();
        let backing = dir.absorber();
        let receipt = backing.retained_identity().unwrap();
        let persisted = serde_json::to_vec(&receipt).unwrap();
        drop(backing);
        let recovered_receipt = serde_json::from_slice(&persisted).unwrap();

        for _ in 0..2 {
            let restored = SwapAbsorber::reopen_retained(&recovered_receipt).unwrap();
            assert_eq!(restored.retained_identity().unwrap(), receipt);
            let path = restored.path().to_str().unwrap();
            assert_eq!(restored.free_bytes([(path, 4 * MIB, MIB)]), Some(3 * MIB));
            assert_eq!(restored.free_bytes([("/dev/fallback", 4 * MIB, MIB)]), None);
            assert!(restored.remove_inactive(|| false).is_err());
        }
        assert!(receipt.path.exists());
    }

    #[test]
    fn retained_absorber_reopens_in_fresh_processes() {
        let dir = Fixture::new();
        let test_binary = std::env::current_exe().unwrap();
        for phase in ["create", "reopen", "reopen"] {
            let result = std::process::Command::new(&test_binary)
                .args([
                    "--ignored",
                    "--nocapture",
                    "--exact",
                    "swap_absorber::tests::retained_absorber_process_fixture",
                ])
                .env("RAMSHARED_TEST_RETAINED_ROOT", &dir.0)
                .env("RAMSHARED_TEST_RETAINED_PHASE", phase)
                .output()
                .unwrap();
            assert!(
                result.status.success()
                    && String::from_utf8_lossy(&result.stdout)
                        .contains("retained absorber fixture complete"),
                "{phase}: {}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
        }
    }

    #[test]
    #[ignore = "invoked with a private directory by the retained absorber process test"]
    fn retained_absorber_process_fixture() {
        let root = PathBuf::from(std::env::var_os("RAMSHARED_TEST_RETAINED_ROOT").unwrap());
        let receipt_path = root.join("identity.json");
        match std::env::var("RAMSHARED_TEST_RETAINED_PHASE")
            .unwrap()
            .as_str()
        {
            "create" => {
                let backing = SwapAbsorber::reserve(&root.join("owned.swap")).unwrap();
                backing.file().set_len(4 * MIB).unwrap();
                let receipt = backing.retained_identity().unwrap();
                std::fs::write(receipt_path, serde_json::to_vec(&receipt).unwrap()).unwrap();
            }
            "reopen" => {
                let expected =
                    serde_json::from_slice(&std::fs::read(receipt_path).unwrap()).unwrap();
                let backing = SwapAbsorber::reopen_retained(&expected).unwrap();
                let path = backing.path().to_str().unwrap();
                assert_eq!(backing.free_bytes([(path, 4 * MIB, MIB)]), Some(3 * MIB));
            }
            _ => panic!("unknown retained absorber process fixture phase"),
        }
        println!("retained absorber fixture complete");
    }

    #[test]
    fn prior_boot_identity_accepts_renumbering_only_with_exact_tag_and_inode() {
        let dir = Fixture::new();
        let backing = dir.absorber();
        let legacy = backing.retained_identity().unwrap();
        assert!(SwapAbsorber::reopen_prior_boot(&legacy).is_err());
        let mut previous = backing.seal_retained_identity().unwrap();
        assert_eq!(backing.seal_retained_identity().unwrap(), previous);
        previous.dev ^= 1;
        previous.mount_id ^= 1;
        assert!(SwapAbsorber::reopen_retained(&previous).is_err());
        let reopened = SwapAbsorber::reopen_prior_boot(&previous).unwrap();
        assert_eq!(reopened.path(), backing.path());

        let mut replaced = previous.clone();
        replaced.tag = Some([42; 32]);
        assert!(SwapAbsorber::reopen_prior_boot(&replaced).is_err());
        previous.ino ^= 1;
        assert!(SwapAbsorber::reopen_prior_boot(&previous).is_err());
        assert!(backing.path().exists());
    }

    #[test]
    fn retained_absorber_absence_requires_the_original_mount() {
        let dir = Fixture::new();
        let backing = dir.absorber();
        let receipt = backing.retained_identity().unwrap();
        assert!(receipt.prove_absent().is_err());
        backing.remove_inactive(|| true).unwrap();
        receipt.prove_absent().unwrap();

        let mut wrong_mount = receipt.clone();
        wrong_mount.mount_id ^= 1;
        assert!(wrong_mount.prove_absent().is_err());
        assert!(wrong_mount.sync_absence().is_err());
        let alias = dir.0.join("alias");
        symlink(&dir.0, &alias).unwrap();
        let mut linked_parent = receipt;
        linked_parent.path = alias.join("owned.swap");
        assert!(linked_parent.prove_absent().is_err());
    }

    #[test]
    fn retained_unlink_recovers_after_directory_sync_failure() {
        let dir = Fixture::new();
        let backing = dir.absorber();
        let receipt = backing.seal_retained_identity().unwrap();
        let sibling = dir.0.join("sibling");
        std::fs::write(&sibling, b"keep").unwrap();
        let error = backing
            .remove_inactive_with_sync(
                || true,
                |directory| {
                    assert_eq!(
                        directory.metadata()?.ino(),
                        std::fs::metadata(&dir.0)?.ino()
                    );
                    assert!(!backing.path().exists());
                    Err(io::Error::other("injected directory sync failure"))
                },
            )
            .unwrap_err();
        assert_eq!(error.to_string(), "injected directory sync failure");
        receipt.sync_absence().unwrap();
        receipt.sync_absence().unwrap();
        assert_eq!(std::fs::read(&sibling).unwrap(), b"keep");
        std::fs::write(backing.path(), b"replacement").unwrap();
        assert!(receipt.sync_absence().is_err());
        assert_eq!(std::fs::read(backing.path()).unwrap(), b"replacement");
    }

    #[test]
    fn retained_absorber_refuses_unmatched_identity_and_links() {
        let dir = Fixture::new();
        let backing = dir.absorber();
        let receipt = backing.retained_identity().unwrap();
        let path = receipt.path.clone();
        let mut wrong_mount = receipt.clone();
        wrong_mount.mount_id ^= 1;
        let mut wrong_birth = receipt.clone();
        wrong_birth.birth_seconds ^= 1;
        let mut dot_path = receipt.clone();
        dot_path.path = dir.0.join("./owned.swap");
        let folder_alias = dir.0.join("ancestor-link");
        symlink(&dir.0, &folder_alias).unwrap();
        let mut linked_path = receipt.clone();
        linked_path.path = folder_alias.join("owned.swap");
        for foreign in [wrong_mount, wrong_birth, dot_path, linked_path] {
            assert!(SwapAbsorber::reopen_retained(&foreign).is_err());
        }

        let hardlink = dir.0.join("extra-link");
        std::fs::hard_link(&path, &hardlink).unwrap();
        assert!(SwapAbsorber::reopen_retained(&receipt).is_err());
        std::fs::remove_file(hardlink).unwrap();
        backing.file().set_len(MIB).unwrap();
        assert!(SwapAbsorber::reopen_retained(&receipt).is_err());
        backing.file().set_len(4 * MIB).unwrap();
        assert!(SwapAbsorber::reopen_retained(&receipt).is_ok());

        let moved = dir.0.join("different-name.swap");
        std::fs::rename(&path, &moved).unwrap();
        drop(backing);
        symlink(&moved, &path).unwrap();
        assert!(SwapAbsorber::reopen_retained(&receipt).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        assert!(SwapAbsorber::reopen_retained(&receipt).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
        assert_eq!(std::fs::metadata(&moved).unwrap().len(), 4 * MIB);
    }

    #[test]
    fn absorber_credit_requires_one_valid_live_owned_row() {
        let dir = Fixture::new();
        let backing = dir.absorber();
        let name = backing.path().to_str().unwrap();
        assert_eq!(backing.free_bytes([(name, 4 * MIB, MIB)]), Some(3 * MIB));
        assert_eq!(backing.free_bytes([("/dev/fallback", 8 * MIB, 0)]), None);
        assert_eq!(backing.free_bytes([(name, 4 * MIB, 5 * MIB)]), None);
        assert_eq!(backing.free_bytes([(name, 5 * MIB, 0)]), None);
        assert_eq!(
            backing.free_bytes([(name, 4 * MIB, 0), (name, 4 * MIB, 0)]),
            None
        );
    }

    #[test]
    fn absorber_identity_changes_revoke_credit_and_unlink_authority() {
        for case in ["replacement", "symlink", "permissions", "hardlink"] {
            let dir = Fixture::new();
            let backing = dir.absorber();
            match case {
                "replacement" | "symlink" => {
                    let moved = dir.0.join("original.swap");
                    std::fs::rename(backing.path(), &moved).unwrap();
                    if case == "replacement" {
                        std::fs::write(backing.path(), b"foreign").unwrap();
                    } else {
                        symlink(&moved, backing.path()).unwrap();
                    }
                }
                "permissions" => {
                    std::fs::set_permissions(backing.path(), std::fs::Permissions::from_mode(0o644))
                        .unwrap()
                }
                _ => std::fs::hard_link(backing.path(), dir.0.join("alias.swap")).unwrap(),
            }
            assert!(backing.validate().is_err(), "{case}");
            assert_eq!(
                backing.free_bytes([(backing.path().to_str().unwrap(), MIB, 0)]),
                None
            );
            assert!(backing.remove_inactive(|| true).is_err());
            assert!(
                backing.path().exists(),
                "foreign/uncertain evidence must survive"
            );
        }
    }

    #[test]
    fn guest_only_capacity_never_admits_wsl_or_unknown_platforms() {
        use crate::platform::PlatformKind;
        assert!(guest_only_capacity_is_admissible(Some(
            PlatformKind::NativeLinux
        )));
        assert!(!guest_only_capacity_is_admissible(Some(PlatformKind::Wsl1)));
        assert!(!guest_only_capacity_is_admissible(Some(PlatformKind::Wsl2)));
        assert!(!guest_only_capacity_is_admissible(None));
    }

    #[test]
    fn unsupported_filesystem_refuses_reservation_without_residue() {
        let root = PathBuf::from("/dev/shm").join(format!(
            "ramshared-absorber-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        let dir = Fixture(root);
        let path = dir.0.join("unsupported.swap");
        let error = SwapAbsorber::reserve(&path)
            .expect_err("tmpfs cannot back the emergency swap absorber");
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(
            !path.exists(),
            "unsupported backing must not leave a reservation"
        );
    }

    #[test]
    fn absorber_removal_keeps_descriptor_custody_until_verified_unlink() {
        let dir = Fixture::new();
        let backing = dir.absorber();
        assert!(backing.remove_inactive(|| false).is_err());
        backing.remove_inactive(|| true).unwrap();
        assert!(!backing.path().exists());
        assert_eq!(backing.file().metadata().unwrap().nlink(), 0);
        assert!(backing.validate().is_err());
    }
}
