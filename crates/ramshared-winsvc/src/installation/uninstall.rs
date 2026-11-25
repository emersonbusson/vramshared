// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/installation/uninstall.rs
//! Durable progress for the destructive half of uninstall.
//!
//! `uninstall` verifies the active version tree before any effect. Once the
//! driver package and the services are gone, a failed `remove_dir_all` can
//! leave the tree partially deleted; the next run would refuse at verification
//! even though the product is uninstalled in substance. The marker records
//! that the destructive phase started for one exact version directory, so the
//! retry can resume without re-verifying bytes the previous attempt was
//! deleting.
//!
//! The marker is written after the package and services are gone and before
//! the tree removal. A marker for a different version never skips
//! verification; it is evidence of an older incomplete attempt, not of this
//! tree's state.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;

/// Schema of `uninstall-in-progress.json`.
pub const UNINSTALL_MARKER_SCHEMA: u32 = 1;

/// One uninstall that has passed the point of no return.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UninstallMarker {
    pub schema: u32,
    pub version_directory: String,
}

impl UninstallMarker {
    pub fn new(version_directory: &str) -> Self {
        Self {
            schema: UNINSTALL_MARKER_SCHEMA,
            version_directory: version_directory.to_string(),
        }
    }
}

/// Parse a marker. Absent is `None`; every other failure is a refusal.
pub fn read_marker(path: &Path) -> Result<Option<UninstallMarker>, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let marker: UninstallMarker =
        serde_json::from_slice(&bytes).map_err(|error| format!("{}: {error}", path.display()))?;
    if marker.schema != UNINSTALL_MARKER_SCHEMA {
        return Err(format!(
            "{}: schema {} is not {}",
            path.display(),
            marker.schema,
            UNINSTALL_MARKER_SCHEMA
        ));
    }
    Ok(Some(marker))
}

/// Persist the marker before the tree removal. The rename is atomic, so a
/// crash leaves either the previous marker or this one, never a half file.
pub fn write_marker(path: &Path, version_directory: &str) -> Result<(), String> {
    let marker = UninstallMarker::new(version_directory);
    let bytes = serde_json::to_vec_pretty(&marker)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    let temporary = path.with_extension("tmp");
    let mut file = std::fs::File::create(&temporary)
        .map_err(|error| format!("{}: {error}", temporary.display()))?;
    file.write_all(&bytes)
        .map_err(|error| format!("{}: {error}", temporary.display()))?;
    file.sync_all()
        .map_err(|error| format!("{}: {error}", temporary.display()))?;
    drop(file);
    std::fs::rename(&temporary, path).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(())
}

/// Remove the marker if it exists; absence is the postcondition.
pub fn remove_marker(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

/// Only the exact version directory the marker names may skip verification.
pub fn resumes_unverified(marker: Option<&UninstallMarker>, version_directory: &str) -> bool {
    marker.is_some_and(|marker| marker.version_directory == version_directory)
}

/// An unfinished uninstall owns the product root until it converges. Install
/// must refuse before staging over a tree the previous attempt may still be
/// deleting; only a completed uninstall clears the marker.
pub fn install_refusal(marker: Option<&UninstallMarker>) -> Option<String> {
    marker.map(|marker| {
        format!(
            "refuse install while uninstall of {} is incomplete; run uninstall to finish it",
            marker.version_directory
        )
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ramshared-uninstall-marker-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_marker_round_trips_and_names_one_version_directory() {
        let dir = scratch("round-trip");
        let path = dir.join("uninstall-in-progress.json");
        assert_eq!(read_marker(&path).unwrap(), None);
        write_marker(&path, "0.15.0-ad15c339de2e").unwrap();
        assert_eq!(
            read_marker(&path).unwrap(),
            Some(UninstallMarker::new("0.15.0-ad15c339de2e"))
        );
        assert!(!path.with_extension("tmp").exists());
        remove_marker(&path).unwrap();
        assert_eq!(read_marker(&path).unwrap(), None);
        remove_marker(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_or_wrong_schema_marker_is_refused() {
        let dir = scratch("corrupt");
        let path = dir.join("uninstall-in-progress.json");
        std::fs::write(&path, b"{not json").unwrap();
        assert!(read_marker(&path).is_err());
        std::fs::write(
            &path,
            br#"{"schema":2,"version_directory":"0.15.0-ad15c339de2e"}"#,
        )
        .unwrap();
        let error = read_marker(&path).unwrap_err();
        assert!(error.contains("schema 2"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_the_same_version_resumes_without_verification() {
        let marker = UninstallMarker::new("0.15.0-ad15c339de2e");
        assert!(resumes_unverified(Some(&marker), "0.15.0-ad15c339de2e"));
        assert!(!resumes_unverified(Some(&marker), "0.14.1-1006-g63768dee"));
        assert!(!resumes_unverified(None, "0.15.0-ad15c339de2e"));
    }

    #[test]
    fn a_second_marker_write_replaces_the_first() {
        let dir = scratch("replace");
        let path = dir.join("uninstall-in-progress.json");
        write_marker(&path, "0.15.0-ad15c339de2e").unwrap();
        write_marker(&path, "0.14.1-1006-g63768dee").unwrap();
        assert_eq!(
            read_marker(&path).unwrap(),
            Some(UninstallMarker::new("0.14.1-1006-g63768dee"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_partial_tree_removal_resumes_for_the_same_version_only() {
        let dir = scratch("partial-tree");
        let tree = dir.join("versions/0.15.0-ad15c339de2e");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join("ramshared.exe"), b"binary").unwrap();
        let marker_path = dir.join("uninstall-in-progress.json");
        // The first run writes the marker before attempting the removal, then
        // deletes part of the tree and fails on a locked file.
        write_marker(&marker_path, "0.15.0-ad15c339de2e").unwrap();
        std::fs::remove_file(tree.join("ramshared.exe")).unwrap();
        // The second run must skip verification for this exact version, and a
        // marker for another version must not.
        let marker = read_marker(&marker_path).unwrap();
        assert!(resumes_unverified(marker.as_ref(), "0.15.0-ad15c339de2e"));
        assert!(!resumes_unverified(
            marker.as_ref(),
            "0.14.1-1006-g63768dee"
        ));
        std::fs::remove_dir_all(&tree).unwrap();
        assert!(!tree.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unfinished_uninstall_refuses_install_until_the_marker_is_cleared() {
        assert_eq!(install_refusal(None), None);
        let marker = UninstallMarker::new("0.15.0-ad15c339de2e");
        let refusal = install_refusal(Some(&marker)).unwrap();
        assert!(refusal.contains("0.15.0-ad15c339de2e"), "{refusal}");
        assert!(refusal.contains("run uninstall"), "{refusal}");
    }

    #[test]
    fn a_marker_write_that_cannot_create_its_directory_is_refused() {
        let dir = scratch("blocked");
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, b"a file, not a directory").unwrap();
        let error =
            write_marker(&blocker.join("uninstall-in-progress.json"), "0.15.0").unwrap_err();
        assert!(error.contains("blocker"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
