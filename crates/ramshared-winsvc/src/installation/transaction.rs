// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/installation/transaction.rs
//! Immutable package staging for the install transaction (DT-13/14).
//!
//! `install` copies a package into a version directory once, verifies every
//! artifact against the manifest, validates the configs, protects the tree,
//! then renames the staging directory into place. The rename is the commit
//! point: a failure before it leaves nothing active, and a failure after it
//! leaves a complete immutable tree.
//!
//! The transaction owns the copy/verify/protect/rename order. The two policy
//! hooks stay injected: the composition supplies config validation and the
//! ACL it wants, while this module decides when each runs and what a failure
//! cleans up. That keeps the order testable on any host with fakes.

use crate::package::{ProductManifestV1, verify_staged_files};
use std::fmt;
use std::path::{Path, PathBuf};

/// A version directory that is ready to be activated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedPackage {
    pub root: PathBuf,
    /// True when this call created the directory, false when it already
    /// existed and was only re-verified.
    pub created: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransactionError {
    /// A filesystem operation failed.
    Io(String),
    /// The copied bytes or the configs do not match the manifest.
    Ambiguous(String),
    /// The protection step refused the staged tree.
    Protect(String),
}

impl fmt::Display for TransactionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(reason) => write!(f, "install transaction: {reason}"),
            Self::Ambiguous(reason) => write!(f, "install transaction: {reason}"),
            Self::Protect(reason) => write!(f, "install transaction: {reason}"),
        }
    }
}

impl std::error::Error for TransactionError {}

impl From<std::io::Error> for TransactionError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

/// Stage `manifest` under `versions` and return the committed directory.
///
/// An existing version directory is verified, never overwritten: its contents
/// are immutable by contract. A fresh staging tree is removed on every
/// failure, so a refused package cannot leave a half-tree behind.
pub fn stage_package(
    source_root: &Path,
    versions: &Path,
    manifest_bytes: &[u8],
    manifest: &ProductManifestV1,
    validate: impl Fn(&Path, &ProductManifestV1) -> Result<(), String>,
    protect: impl Fn(&Path) -> Result<(), String>,
) -> Result<StagedPackage, TransactionError> {
    let final_root = versions.join(manifest.version_directory());
    if final_root.exists() {
        verify_staged_files(&final_root, manifest).map_err(TransactionError::Ambiguous)?;
        validate(&final_root, manifest).map_err(TransactionError::Ambiguous)?;
        return Ok(StagedPackage {
            root: final_root,
            created: false,
        });
    }

    std::fs::create_dir_all(versions)?;
    let run_id = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| TransactionError::Io(error.to_string()))?
            .as_nanos()
    );
    let staging = versions.join(format!(".staging-{run_id}"));
    std::fs::create_dir(&staging)?;

    let staged = (|| -> Result<(), TransactionError> {
        for artifact in &manifest.artifacts {
            let source = source_root.join(&artifact.relative_path);
            let target = staging.join(&artifact.relative_path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(source, target)?;
        }
        // The manifest is written before verification so the staged tree is
        // self-describing even if the process dies before the rename.
        std::fs::write(staging.join("product-manifest.json"), manifest_bytes)?;
        verify_staged_files(&staging, manifest).map_err(TransactionError::Ambiguous)?;
        validate(&staging, manifest).map_err(TransactionError::Ambiguous)?;
        protect(&staging).map_err(TransactionError::Protect)?;
        // Re-hash after protection: the ACL step must not have changed a byte.
        verify_staged_files(&staging, manifest).map_err(TransactionError::Ambiguous)?;
        std::fs::rename(&staging, &final_root)?;
        Ok(())
    })();

    if let Err(error) = staged {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    Ok(StagedPackage {
        root: final_root,
        created: true,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::package::{
        ArtifactRole, ManifestArtifact, REQUIRED_ARTIFACTS, ServiceIdentities, StartPolicy,
        parse_manifest,
    };
    use sha2::{Digest, Sha256};

    fn sha256(bytes: &[u8]) -> String {
        let mut digest = Sha256::new();
        digest.update(bytes);
        digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect()
    }

    /// A real package directory: seven artifacts with real hashes and a
    /// manifest the installer parser accepts.
    fn fixture(name: &str) -> (PathBuf, Vec<u8>, ProductManifestV1) {
        let root = std::env::temp_dir().join(format!(
            "ramshared-transaction-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let source = root.join("package");
        std::fs::create_dir_all(&source).unwrap();
        let artifacts: Vec<ManifestArtifact> = REQUIRED_ARTIFACTS
            .iter()
            .enumerate()
            .map(|(index, role)| {
                let relative = format!("artifact-{index}.bin");
                let bytes = format!("{name}-{index}").into_bytes();
                std::fs::write(source.join(&relative), &bytes).unwrap();
                ManifestArtifact {
                    role: *role,
                    relative_path: relative,
                    sha256: sha256(&bytes),
                }
            })
            .collect();
        let manifest = ProductManifestV1 {
            schema: 1,
            version: "0.15.0".into(),
            commit: "abcdef1234567890".into(),
            architecture: "x86_64-pc-windows-msvc".into(),
            start_policy: StartPolicy::Demand,
            services: ServiceIdentities {
                broker_name: "RamSharedBroker".into(),
                broker_account: r"NT SERVICE\RamSharedBroker".into(),
                consumer_name: "RamSharedWinSvc".into(),
                consumer_account: "LocalSystem".into(),
            },
            artifacts,
        };
        let bytes = serde_json::to_vec(&manifest).unwrap();
        let parsed = parse_manifest(&bytes).unwrap();
        (root, bytes, parsed)
    }

    fn no_validate(_: &Path, _: &ProductManifestV1) -> Result<(), String> {
        Ok(())
    }

    fn no_protect(_: &Path) -> Result<(), String> {
        Ok(())
    }

    fn staging_dirs(versions: &Path) -> Vec<String> {
        std::fs::read_dir(versions)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".staging-"))
            .collect()
    }

    #[test]
    fn staging_copies_the_declared_artifacts_and_manifest() {
        let (root, bytes, manifest) = fixture("happy");
        let versions = root.join("versions");
        let staged = stage_package(
            &root.join("package"),
            &versions,
            &bytes,
            &manifest,
            no_validate,
            no_protect,
        )
        .unwrap();
        assert!(staged.created);
        assert_eq!(staged.root, versions.join(manifest.version_directory()));
        for artifact in &manifest.artifacts {
            assert!(staged.root.join(&artifact.relative_path).is_file());
        }
        assert_eq!(
            std::fs::read(staged.root.join("product-manifest.json")).unwrap(),
            bytes
        );
        assert!(staging_dirs(&versions).is_empty());
    }

    #[test]
    fn staging_an_existing_version_reverifies_and_does_not_overwrite() {
        let (root, bytes, manifest) = fixture("repeat");
        let versions = root.join("versions");
        let first = stage_package(
            &root.join("package"),
            &versions,
            &bytes,
            &manifest,
            no_validate,
            no_protect,
        )
        .unwrap();
        let marker = first.root.join("marker");
        std::fs::write(&marker, b"keep").unwrap();
        let second = stage_package(
            &root.join("package"),
            &versions,
            &bytes,
            &manifest,
            no_validate,
            no_protect,
        )
        .unwrap();
        assert!(!second.created);
        assert_eq!(std::fs::read(&marker).unwrap(), b"keep");
    }

    #[test]
    fn a_refused_validation_removes_the_staging_tree() {
        let (root, bytes, manifest) = fixture("validate");
        let versions = root.join("versions");
        let error = stage_package(
            &root.join("package"),
            &versions,
            &bytes,
            &manifest,
            |_, _| Err("tenant mismatch".into()),
            no_protect,
        )
        .unwrap_err();
        assert!(matches!(error, TransactionError::Ambiguous(_)), "{error}");
        assert!(error.to_string().contains("tenant mismatch"), "{error}");
        assert!(staging_dirs(&versions).is_empty());
        assert!(!versions.join(manifest.version_directory()).exists());
    }

    #[test]
    fn a_refused_protection_removes_the_staging_tree() {
        let (root, bytes, manifest) = fixture("protect");
        let versions = root.join("versions");
        let error = stage_package(
            &root.join("package"),
            &versions,
            &bytes,
            &manifest,
            no_validate,
            |_| Err("ACL refused".into()),
        )
        .unwrap_err();
        assert!(matches!(error, TransactionError::Protect(_)), "{error}");
        assert!(staging_dirs(&versions).is_empty());
        assert!(!versions.join(manifest.version_directory()).exists());
    }

    #[test]
    fn a_hash_mismatch_removes_the_staging_tree() {
        let (root, bytes, mut manifest) = fixture("hash");
        let versions = root.join("versions");
        manifest.artifacts[0].sha256 = "A".repeat(64);
        let error = stage_package(
            &root.join("package"),
            &versions,
            &bytes,
            &manifest,
            no_validate,
            no_protect,
        )
        .unwrap_err();
        assert!(matches!(error, TransactionError::Ambiguous(_)), "{error}");
        assert!(staging_dirs(&versions).is_empty());
        assert!(!versions.join(manifest.version_directory()).exists());
    }

    #[test]
    fn a_missing_source_artifact_is_an_io_refusal() {
        let (root, bytes, manifest) = fixture("missing");
        let versions = root.join("versions");
        std::fs::remove_file(
            root.join("package")
                .join(&manifest.artifacts[0].relative_path),
        )
        .unwrap();
        let error = stage_package(
            &root.join("package"),
            &versions,
            &bytes,
            &manifest,
            no_validate,
            no_protect,
        )
        .unwrap_err();
        assert!(matches!(error, TransactionError::Io(_)), "{error}");
        assert!(staging_dirs(&versions).is_empty());
    }

    #[test]
    fn an_existing_version_with_a_bad_hash_is_refused_and_kept() {
        let (root, bytes, manifest) = fixture("existing-bad");
        let versions = root.join("versions");
        let staged = stage_package(
            &root.join("package"),
            &versions,
            &bytes,
            &manifest,
            no_validate,
            no_protect,
        )
        .unwrap();
        std::fs::write(
            staged.root.join(&manifest.artifacts[0].relative_path),
            b"tampered",
        )
        .unwrap();
        let error = stage_package(
            &root.join("package"),
            &versions,
            &bytes,
            &manifest,
            no_validate,
            no_protect,
        )
        .unwrap_err();
        assert!(matches!(error, TransactionError::Ambiguous(_)), "{error}");
        assert!(
            staged.root.exists(),
            "an existing tree is refused, never silently rebuilt"
        );
    }

    #[test]
    fn a_versions_path_that_is_a_file_is_an_io_refusal() {
        let (root, bytes, manifest) = fixture("versions-file");
        let versions = root.join("versions-file");
        std::fs::write(&versions, b"not a directory").unwrap();
        let error = stage_package(
            &root.join("package"),
            &versions,
            &bytes,
            &manifest,
            no_validate,
            no_protect,
        )
        .unwrap_err();
        assert!(matches!(error, TransactionError::Io(_)), "{error}");
    }

    #[test]
    fn transaction_errors_name_every_category() {
        for (error, needle) in [
            (TransactionError::Io("disk".into()), "disk"),
            (TransactionError::Ambiguous("hash".into()), "hash"),
            (TransactionError::Protect("ACL".into()), "ACL"),
        ] {
            assert!(error.to_string().contains(needle), "{error}");
            let boxed: Box<dyn std::error::Error> = Box::new(error);
            assert!(boxed.to_string().contains(needle));
        }
    }

    #[test]
    fn staged_package_keeps_role_identity() {
        // The staging order must not depend on the artifact order in the
        // manifest; the committed tree keeps every role's path verbatim.
        let (root, bytes, manifest) = fixture("roles");
        let versions = root.join("versions");
        let staged = stage_package(
            &root.join("package"),
            &versions,
            &bytes,
            &manifest,
            no_validate,
            no_protect,
        )
        .unwrap();
        for role in [ArtifactRole::DriverInf, ArtifactRole::BrokerConfig] {
            let artifact = manifest.artifact(role).unwrap();
            assert!(staged.root.join(&artifact.relative_path).is_file());
        }
    }
}
