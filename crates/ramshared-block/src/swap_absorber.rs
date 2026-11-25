// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/swap_absorber.rs
//! Descriptor-owned Linux emergency swap backing (SPEC swap-drain-safety).

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// Guest filesystem capacity alone is authoritative only on measured native
/// Linux. WSL needs a separate, fresh proof of the VHDX host volume; until
/// that proof is wired into both release owners, refuse guest-only admission.
pub fn guest_only_capacity_is_admissible(platform: Option<crate::platform::PlatformKind>) -> bool {
    matches!(platform, Some(crate::platform::PlatformKind::NativeLinux))
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
        let stats = rustix::fs::fstatfs(&self.file)?;
        // ext2/3/4 share a magic value. Btrfs needs a separate NOCOW and
        // extent-placement contract; tmpfs, network and Windows mounts are refused.
        const EXT_SUPER_MAGIC: u32 = 0xef53;
        const XFS_SUPER_MAGIC: u32 = 0x5846_5342;
        if !matches!(stats.f_type as u32, EXT_SUPER_MAGIC | XFS_SUPER_MAGIC) {
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
        if !inactive() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "absorber inactivity unproven",
            ));
        }
        self.validate()?;
        std::fs::remove_file(&self.path)
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
