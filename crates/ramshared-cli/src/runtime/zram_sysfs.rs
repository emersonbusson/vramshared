// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/runtime/zram_sysfs.rs
// Pinned zram sysfs state machine (SPEC zero-script-native-runtime ITEM-3,
// RF-2, DT-2). A directory fd opened with O_NOFOLLOW anchors every attribute
// write, so a symlink swap under the sysfs root cannot redirect the write.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;

use rustix::fs::{Mode, OFlags, openat};
use rustix::io::Errno;

pub const SYSFS_MAGIC: i64 = 0x6265_6572;
pub const ZRAM_SYSFS_ROOT: &str = "/sys/devices/virtual/block";

#[derive(Debug)]
pub enum ZramError {
    Io(std::io::Error),
    SymlinkRefused,
    NotSysfs,
    PreparationUnproven(String),
    InvalidInput(&'static str),
}

impl std::fmt::Display for ZramError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "zram sysfs io: {error}"),
            Self::SymlinkRefused => formatter.write_str("zram sysfs symlink refused"),
            Self::NotSysfs => formatter.write_str("zram path is not sysfs"),
            Self::PreparationUnproven(reason) => {
                write!(formatter, "zram preparation unproven: {reason}")
            }
            Self::InvalidInput(reason) => write!(formatter, "zram invalid input: {reason}"),
        }
    }
}

impl std::error::Error for ZramError {}

impl From<std::io::Error> for ZramError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

fn map_open_error(error: Errno) -> ZramError {
    if error == Errno::LOOP {
        ZramError::SymlinkRefused
    } else {
        ZramError::Io(std::io::Error::from_raw_os_error(error.raw_os_error()))
    }
}

pub struct ZramSysfs {
    dir: OwnedFd,
}

impl ZramSysfs {
    pub fn open(zram_id: u32) -> Result<Self, ZramError> {
        Self::open_root_checked(Path::new(ZRAM_SYSFS_ROOT), zram_id, true)
    }

    /// Test/container constructor: the caller supplies the sysfs-like root.
    #[cfg(test)]
    pub fn open_root(root: &Path, zram_id: u32) -> Result<Self, ZramError> {
        Self::open_root_checked(root, zram_id, false)
    }

    pub fn open_root_checked(
        root: &Path,
        zram_id: u32,
        require_magic: bool,
    ) -> Result<Self, ZramError> {
        let name = root.join(format!("zram{zram_id}"));
        if std::fs::symlink_metadata(&name)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(ZramError::SymlinkRefused);
        }
        let dir = openat(
            rustix::fs::CWD,
            name.as_path(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(map_open_error)?;
        if require_magic {
            let stats = rustix::fs::fstatfs(&dir).map_err(|error| {
                ZramError::Io(std::io::Error::from_raw_os_error(error.raw_os_error()))
            })?;
            if stats.f_type != SYSFS_MAGIC {
                return Err(ZramError::NotSysfs);
            }
        }
        Ok(Self { dir })
    }

    /// Read one attribute through the same pinned directory descriptor.
    /// Read-only planning uses this; it never opens an attribute for write.
    pub fn read_attr(&self, name: &str) -> Result<String, ZramError> {
        let fd = openat(
            self.dir.as_fd(),
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(map_open_error)?;
        let mut file = File::from(fd);
        let mut value = String::new();
        file.read_to_string(&mut value)?;
        Ok(value.trim().to_string())
    }

    /// Reset the device through the pinned descriptor. This is the native
    /// replacement for `zramctl -r` and never derives a target from `$PATH`.
    pub fn reset(&self) -> Result<(), ZramError> {
        self.write_attr("reset", b"1")
    }

    /// Request the compressor before the size is set (the kernel locks the
    /// compressor on the first `disksize` write).
    pub fn set_algorithm(&self, algorithm: &str) -> Result<(), ZramError> {
        self.write_attr("comp_algorithm", algorithm.as_bytes())
    }

    /// Return true when the kernel exposes `recomp_algorithm` (`CONFIG_ZRAM_MULTI_COMP`).
    pub fn supports_recomp(&self) -> bool {
        openat(
            self.dir.as_fd(),
            "recomp_algorithm",
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .is_ok()
    }

    /// Configure a secondary recompression algorithm (`CONFIG_ZRAM_MULTI_COMP`)
    /// before `disksize` is set. Validates the algorithm name and priority
    /// (`1..=3`) before touching sysfs.
    pub fn set_recomp_algorithm(&self, algorithm: &str, priority: u8) -> Result<(), ZramError> {
        if !(1..=16).contains(&algorithm.len())
            || !algorithm
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(ZramError::InvalidInput("invalid zram algorithm name"));
        }
        if !(1..=3).contains(&priority) {
            return Err(ZramError::InvalidInput("recomp priority must be in 1..=3"));
        }
        let spec = format!("algo={algorithm} priority={priority}");
        self.write_attr("recomp_algorithm", spec.as_bytes())
    }

    /// Bind the backing device before the size is set.
    pub fn set_backing_dev(&self, backing_dev: &Path) -> Result<(), ZramError> {
        self.write_attr("backing_dev", backing_dev.as_os_str().as_encoded_bytes())
    }

    /// Initialize the device. This is the write that allocates the
    /// compression streams and can refuse an undersized backing device.
    pub fn set_disksize(&self, disksize_bytes: u64) -> Result<(), ZramError> {
        self.write_attr("disksize", disksize_bytes.to_string().as_bytes())
    }

    /// Prove an uninitialized device has no pre-existing backing.
    pub fn verify_unconfigured(&self) -> Result<(), ZramError> {
        let size = self.read_attr("disksize").map_err(|error| {
            ZramError::PreparationUnproven(format!("read current size ({error})"))
        })?;
        if size != "0" {
            return Err(ZramError::PreparationUnproven(
                "the selected device already has a size".into(),
            ));
        }
        self.verify_backing_dev(Path::new(""))
    }

    fn verify_backing_dev(&self, backing_dev: &Path) -> Result<(), ZramError> {
        let standalone = backing_dev.as_os_str().is_empty();
        let observed = match self.read_attr("backing_dev") {
            Ok(path) => path,
            Err(ZramError::Io(error))
                if standalone && error.kind() == std::io::ErrorKind::NotFound =>
            {
                return Ok(());
            }
            Err(error) => {
                return Err(ZramError::PreparationUnproven(format!(
                    "read current backing device ({error})"
                )));
            }
        };
        let path = Path::new(&observed);
        let matches = if standalone {
            observed == "none"
        } else {
            path.is_absolute()
                && backing_dev.is_absolute()
                && (path == backing_dev
                    || std::fs::canonicalize(path)
                        .ok()
                        .zip(std::fs::canonicalize(backing_dev).ok())
                        .is_some_and(|(actual, expected)| actual == expected))
        };
        if !matches {
            return Err(ZramError::PreparationUnproven(format!(
                "observed backing {observed:?} differs from expected {}",
                if standalone {
                    "none".into()
                } else {
                    backing_dev.display().to_string()
                }
            )));
        }
        Ok(())
    }

    /// Initialize the prepared device without repeating the earlier backing
    /// and compressor writes. Its lifecycle owner handles any required reset.
    pub fn configure(&self, backing_dev: &Path, disksize_bytes: u64) -> Result<(), ZramError> {
        self.verify_backing_dev(backing_dev)?;
        self.set_disksize(disksize_bytes)
    }

    fn write_attr(&self, name: &str, value: &[u8]) -> Result<(), ZramError> {
        let fd = openat(
            self.dir.as_fd(),
            name,
            OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(map_open_error)?;
        let mut file = File::from(fd);
        file.write_all(value)?;
        file.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::os::unix::fs::symlink;

    fn root() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("ramshared-zram-{}-{id}", std::process::id()))
    }

    fn prepare() -> std::path::PathBuf {
        let root = root();
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("zram0")).unwrap();
        for name in ["reset", "backing_dev", "disksize"] {
            std::fs::write(root.join("zram0").join(name), b"").unwrap();
        }
        root
    }

    #[test]
    fn zram_initialization_preserves_the_prepared_backing_and_reset() {
        let root = prepare();
        let dev = root.join("zram0");
        let zram = ZramSysfs::open_root(&root, 0).unwrap();
        std::fs::write(dev.join("backing_dev"), b"/dev/nbd0\n").unwrap();
        std::fs::write(dev.join("comp_algorithm"), b"[lzo-rle]").unwrap();
        zram.configure(Path::new("/dev/nbd0"), 1 << 20).unwrap();
        assert_eq!(std::fs::read(dev.join("disksize")).unwrap(), b"1048576");
        assert_eq!(
            std::fs::read(dev.join("backing_dev")).unwrap(),
            b"/dev/nbd0\n"
        );
        assert_eq!(
            std::fs::read(dev.join("comp_algorithm")).unwrap(),
            b"[lzo-rle]"
        );
        assert!(std::fs::read(dev.join("reset")).unwrap().is_empty());

        let size = dev.join("disksize");
        std::fs::remove_file(&size).unwrap();
        std::fs::create_dir(&size).unwrap();
        assert!(zram.configure(Path::new("/dev/nbd0"), 1 << 20).is_err());
        assert_eq!(
            std::fs::read(dev.join("backing_dev")).unwrap(),
            b"/dev/nbd0\n"
        );
        assert!(std::fs::read(dev.join("reset")).unwrap().is_empty());

        symlink(&dev, root.join("zram1")).unwrap();
        assert!(matches!(
            ZramSysfs::open_root(&root, 1),
            Err(ZramError::SymlinkRefused)
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn zram_standalone_initialization_accepts_no_writeback_attribute() {
        let root = prepare();
        let dev = root.join("zram0");
        let zram = ZramSysfs::open_root(&root, 0).unwrap();
        std::fs::write(dev.join("backing_dev"), b"none\n").unwrap();
        zram.configure(Path::new(""), 1 << 20).unwrap();
        std::fs::remove_file(dev.join("backing_dev")).unwrap();
        zram.configure(Path::new(""), 1 << 21).unwrap();
        assert_eq!(std::fs::read(dev.join("disksize")).unwrap(), b"2097152");
        assert!(matches!(
            zram.configure(Path::new("/dev/nbd0"), 1 << 20),
            Err(ZramError::PreparationUnproven(_))
        ));
        assert_eq!(std::fs::read(dev.join("disksize")).unwrap(), b"2097152");
        assert!(std::fs::read(dev.join("reset")).unwrap().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn zram_initialization_accepts_the_resolved_backing_alias() {
        let root = prepare();
        let actual = root.join("backing-device");
        let alias = root.join("backing-alias");
        std::fs::write(&actual, b"").unwrap();
        symlink(&actual, &alias).unwrap();
        std::fs::write(
            root.join("zram0/backing_dev"),
            actual.as_os_str().as_encoded_bytes(),
        )
        .unwrap();
        let zram = ZramSysfs::open_root(&root, 0).unwrap();
        zram.configure(&alias, 4096).unwrap();
        assert_eq!(std::fs::read(root.join("zram0/disksize")).unwrap(), b"4096");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn zram_unconfigured_preflight_refuses_existing_state() {
        let root = prepare();
        let dev = root.join("zram0");
        let zram = ZramSysfs::open_root(&root, 0).unwrap();
        std::fs::write(dev.join("disksize"), b"0\n").unwrap();
        std::fs::write(dev.join("backing_dev"), b"none\n").unwrap();
        zram.verify_unconfigured().unwrap();
        std::fs::write(dev.join("disksize"), b"4096\n").unwrap();
        assert!(matches!(
            zram.verify_unconfigured(),
            Err(ZramError::PreparationUnproven(_))
        ));
        std::fs::write(dev.join("disksize"), b"0\n").unwrap();
        std::fs::write(dev.join("backing_dev"), b"/dev/nbd9\n").unwrap();
        assert!(matches!(
            zram.verify_unconfigured(),
            Err(ZramError::PreparationUnproven(_))
        ));
        assert!(matches!(
            zram.configure(Path::new("/dev/nbd0"), 4096),
            Err(ZramError::PreparationUnproven(_))
        ));
        assert_eq!(std::fs::read(dev.join("disksize")).unwrap(), b"0\n");
        assert!(std::fs::read(dev.join("reset")).unwrap().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn zram_read_attr_is_read_only_and_refuses_symlinks() {
        let root = prepare();
        let zram = ZramSysfs::open_root(&root, 0).unwrap();
        std::fs::write(root.join("zram0/disksize"), b"1048576").unwrap();
        assert_eq!(zram.read_attr("disksize").unwrap(), "1048576");
        assert!(zram.read_attr("missing").is_err());

        let target = root.join("outside");
        std::fs::write(&target, b"outside").unwrap();
        std::fs::remove_file(root.join("zram0/backing_dev")).unwrap();
        symlink(&target, root.join("zram0/backing_dev")).unwrap();
        assert!(matches!(
            zram.read_attr("backing_dev"),
            Err(ZramError::SymlinkRefused)
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn zram_sysfs_refuses_a_non_sysfs_root_when_magic_is_required() {
        let root = prepare();
        assert!(matches!(
            ZramSysfs::open_root_checked(&root, 0, true),
            Err(ZramError::NotSysfs)
        ));
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod zram_edge_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn zram_error_display_and_production_open_are_total() {
        for error in [
            ZramError::SymlinkRefused,
            ZramError::NotSysfs,
            ZramError::PreparationUnproven("changed backing".into()),
            ZramError::InvalidInput("bad"),
            ZramError::Io(std::io::Error::other("x")),
        ] {
            assert!(!error.to_string().is_empty());
        }
        let io: ZramError = std::io::Error::other("x").into();
        assert!(matches!(io, ZramError::Io(_)));
        match ZramSysfs::open(0) {
            Ok(_) => {}
            Err(error) => assert!(!error.to_string().is_empty()),
        }
        let missing = std::env::temp_dir().join("ramshared-zram-missing-root");
        assert!(matches!(
            ZramSysfs::open_root(&missing, 0),
            Err(ZramError::Io(_))
        ));
    }

    #[test]
    fn zram_sysfs_production_root_is_a_real_directory_not_the_block_symlink() {
        // Regression: `/sys/block/zramN` is a symlink on a real kernel, so an
        // O_NOFOLLOW anchor there always refused. The anchor must be the
        // canonical device directory.
        assert_ne!(ZRAM_SYSFS_ROOT, "/sys/block");
        let Ok(entries) = std::fs::read_dir("/sys/block") else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(id) = name
                .to_str()
                .and_then(|name| name.strip_prefix("zram"))
                .and_then(|id| id.parse::<u32>().ok())
            else {
                continue;
            };
            assert!(entry.path().symlink_metadata().unwrap().is_symlink());
            assert!(ZramSysfs::open(id).is_ok(), "zram{id} must open");
        }
    }

    #[test]
    fn zram_attribute_symlink_is_refused_without_following() {
        let root = std::env::temp_dir().join(format!("ramshared-zram-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("zram0")).unwrap();
        for name in ["reset", "disksize"] {
            std::fs::write(root.join("zram0").join(name), b"").unwrap();
        }
        let target = root.join("outside");
        std::fs::write(&target, b"outside").unwrap();
        std::os::unix::fs::symlink(&target, root.join("zram0/backing_dev")).unwrap();
        let zram = ZramSysfs::open_root(&root, 0).unwrap();
        assert!(matches!(
            zram.configure(Path::new("/dev/nbd0"), 1 << 20),
            Err(ZramError::PreparationUnproven(ref reason)) if reason.contains("symlink refused")
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn recomp_algorithm_writes_valid_and_refuses_symlinks_or_invalid_inputs() {
        let root =
            std::env::temp_dir().join(format!("ramshared-zram-recomp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dev = root.join("zram0");
        std::fs::create_dir_all(&dev).unwrap();
        let zram = ZramSysfs::open_root(&root, 0).unwrap();
        assert!(!zram.supports_recomp());
        std::fs::write(dev.join("recomp_algorithm"), b"").unwrap();
        assert!(zram.supports_recomp());
        zram.set_recomp_algorithm("zstd", 1).unwrap();
        assert_eq!(
            std::fs::read_to_string(dev.join("recomp_algorithm")).unwrap(),
            "algo=zstd priority=1"
        );
        for bad_algo in [
            "",
            "zstd\necho",
            "ZSTD",
            "lz4;rm",
            "a_very_long_algorithm_name",
        ] {
            assert!(matches!(
                zram.set_recomp_algorithm(bad_algo, 1),
                Err(ZramError::InvalidInput(_))
            ));
        }
        for bad_prio in [0u8, 4, 255] {
            assert!(matches!(
                zram.set_recomp_algorithm("zstd", bad_prio),
                Err(ZramError::InvalidInput(_))
            ));
        }
        std::fs::remove_file(dev.join("recomp_algorithm")).unwrap();
        let outside = root.join("outside-recomp");
        std::fs::write(&outside, b"untouched").unwrap();
        std::os::unix::fs::symlink(&outside, dev.join("recomp_algorithm")).unwrap();
        assert!(!zram.supports_recomp());
        assert!(matches!(
            zram.set_recomp_algorithm("zstd", 1),
            Err(ZramError::SymlinkRefused)
        ));
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "untouched");
        let _ = std::fs::remove_dir_all(&root);
    }
}
