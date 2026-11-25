// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/runtime_cleanup.rs
//! Completion reporting for runtime records after a verified teardown.

use super::{CascadeError, RuntimePaths, fs};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Keep the observed file alive and refuse a changed name or contents.
pub(super) struct StaleRecordCustody {
    path: PathBuf,
    file: fs::File,
    snapshot: (u64, u64, u64, i64, i64, i64, i64),
}

impl StaleRecordCustody {
    pub(super) fn open(path: &Path) -> Result<Self, CascadeError> {
        let fd = rustix::fs::open(
            path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NONBLOCK,
            rustix::fs::Mode::empty(),
        )
        .map_err(|error| {
            CascadeError::UnsafeContainment(format!(
                "open stale record {}: {error}",
                path.display()
            ))
        })?;
        let file = fs::File::from(fd);
        let metadata = file
            .metadata()
            .map_err(|error| CascadeError::Io(format!("stat stale record: {error}")))?;
        if !metadata.is_file() {
            return Err(CascadeError::UnsafeContainment(
                "stale record is not a regular file".into(),
            ));
        }
        let held = Self {
            path: path.to_path_buf(),
            file,
            snapshot: Self::identity(&metadata),
        };
        held.revalidate()?;
        Ok(held)
    }

    fn identity(metadata: &fs::Metadata) -> (u64, u64, u64, i64, i64, i64, i64) {
        (
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        )
    }

    pub(super) fn revalidate(&self) -> Result<(), CascadeError> {
        let opened = self
            .file
            .metadata()
            .map_err(|error| CascadeError::Io(format!("stat held stale record: {error}")))?;
        let named = fs::symlink_metadata(&self.path)
            .map_err(|error| CascadeError::Io(format!("stat named stale record: {error}")))?;
        if !named.is_file()
            || Self::identity(&opened) != self.snapshot
            || Self::identity(&named) != self.snapshot
        {
            return Err(CascadeError::UnsafeContainment(format!(
                "stale record changed at {}; ownership evidence retained",
                self.path.display()
            )));
        }
        Ok(())
    }

    pub(super) fn remove(&self) -> Result<(), CascadeError> {
        self.revalidate()?;
        fs::remove_file(&self.path).map_err(|error| {
            CascadeError::UnsafeContainment(format!(
                "remove stale record {}: {error}",
                self.path.display()
            ))
        })
    }
}

/// Missing records are idempotent; every other unlink failure is observable.
pub(super) fn remove_released_record(path: &Path) -> Result<(), CascadeError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(CascadeError::UnsafeContainment(format!(
            "runtime cleanup incomplete at {}: {error}; remaining ownership evidence retained",
            path.display()
        ))),
    }
}

/// Keep the sealed binding until all earlier removals have succeeded.
/// This is forward-only cleanup, not an atomic filesystem transaction.
pub(super) fn clear_released_runtime(paths: &RuntimePaths) -> Result<(), CascadeError> {
    for path in [
        &paths.socket,
        &paths.zram_dev_file,
        &paths.swap_dev_file,
        &paths.capacity_status_file,
        &paths.cache_status_file,
        &paths.supervisor_status_file,
        &paths.cache_target_request_file,
        &paths.reclaim_request_file,
        &paths.pid_file,
    ]
    .into_iter()
    .chain(paths.forensics_markers.iter())
    .chain(std::iter::once(&paths.lifecycle_binding_file))
    {
        remove_released_record(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn stale_record_custody_refuses_replacement() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "stale-custody-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let path = root.join("record");
        fs::write(&path, "original").unwrap();
        let held = StaleRecordCustody::open(&path).unwrap();
        fs::rename(&path, root.join("original")).unwrap();
        fs::write(&path, "foreign").unwrap();
        assert!(held.remove().is_err());
        assert_eq!(fs::read(&path).unwrap(), b"foreign");
        assert_eq!(fs::read(root.join("original")).unwrap(), b"original");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_record_custody_refuses_in_place_change_and_symlink() {
        let root =
            std::env::temp_dir().join(format!("stale-custody-mutation-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let path = root.join("record");
        fs::write(&path, "original").unwrap();
        let held = StaleRecordCustody::open(&path).unwrap();
        fs::write(&path, "changed content").unwrap();
        assert!(held.remove().is_err());
        assert_eq!(fs::read(&path).unwrap(), b"changed content");
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("missing", &path).unwrap();
        assert!(StaleRecordCustody::open(&path).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cleanup_reports_failure_and_can_resume_its_file_removals() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "cleanup-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let paths = RuntimePaths::under(&root);
        fs::create_dir_all(&paths.runtime_dir).unwrap();
        fs::write(&paths.lifecycle_binding_file, "sealed binding").unwrap();
        fs::create_dir(&paths.pid_file).unwrap();
        let error = clear_released_runtime(&paths).unwrap_err();
        assert!(error.to_string().contains("runtime cleanup incomplete"));
        assert_eq!(
            fs::read(&paths.lifecycle_binding_file).unwrap(),
            b"sealed binding"
        );
        assert!(paths.pid_file.is_dir());
        fs::remove_dir(&paths.pid_file).unwrap();
        clear_released_runtime(&paths).unwrap();
        assert!(!paths.lifecycle_binding_file.exists());
        clear_released_runtime(&paths).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
