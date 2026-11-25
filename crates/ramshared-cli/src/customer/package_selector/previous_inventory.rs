// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_selector/previous_inventory.rs
//! Exact prior-release layout for signed members and generated receipts.

use std::collections::BTreeSet;
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

pub(super) const RECEIPTS: [&str; 4] = [
    crate::release_identity::SHA256SUMS_FILE,
    "INSTALLED_MANIFEST_SHA256",
    "INPUT_BUNDLE_SHA256SUMS",
    "INSTALL_PROVENANCE.json",
];

pub(super) fn expected_dirs(files: &BTreeSet<PathBuf>) -> Option<BTreeSet<PathBuf>> {
    let mut directories = BTreeSet::new();
    for path in files {
        let text = path.to_str()?;
        if text.is_empty()
            || text.chars().any(char::is_control)
            || text.split('/').any(|part| matches!(part, "" | "." | ".."))
        {
            return None;
        }
        let mut parent = path.parent();
        while let Some(part) = parent {
            if part.as_os_str().is_empty() {
                break;
            }
            directories.insert(part.to_path_buf());
            parent = part.parent();
        }
    }
    Some(directories)
}

/// Refuse every entry outside the signed layout and required generated receipts.
pub(super) fn verify(root: &File, signed: &BTreeSet<PathBuf>) -> Option<()> {
    if signed.is_empty() {
        return None;
    }
    crate::customer::transition_custody::validate_directory(root, "previous release root").ok()?;
    let metadata = root.metadata().ok()?;
    let mut files = signed.clone();
    for name in RECEIPTS {
        if !files.insert(PathBuf::from(name)) {
            return None;
        }
    }
    let directories = expected_dirs(&files)?;
    crate::customer::package_publish::inventory::inspect_expected_tree(
        root,
        &directories,
        &files,
        (metadata.dev(), metadata.ino()),
    )
    .ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    struct Fixture(std::path::PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn refuses_unsigned_extras_missing_entries_and_links() {
        let path =
            std::env::temp_dir().join(format!("ramshared-prior-inventory-{}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        let fixture = Fixture(path);
        std::fs::set_permissions(&fixture.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::create_dir(fixture.0.join("nested")).unwrap();
        std::fs::set_permissions(
            fixture.0.join("nested"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        std::fs::write(fixture.0.join("nested/signed"), b"member").unwrap();
        std::fs::set_permissions(
            fixture.0.join("nested/signed"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        for name in [
            "SHA256SUMS",
            "INSTALLED_MANIFEST_SHA256",
            "INPUT_BUNDLE_SHA256SUMS",
            "INSTALL_PROVENANCE.json",
        ] {
            std::fs::write(fixture.0.join(name), b"receipt").unwrap();
            std::fs::set_permissions(fixture.0.join(name), std::fs::Permissions::from_mode(0o600))
                .unwrap();
        }
        let root = std::fs::File::open(&fixture.0).unwrap();
        let signed = std::collections::BTreeSet::from([std::path::PathBuf::from("nested/signed")]);
        assert!(verify(&root, &signed).is_some());
        std::fs::set_permissions(
            fixture.0.join("nested/signed"),
            std::fs::Permissions::from_mode(0o666),
        )
        .unwrap();
        assert!(verify(&root, &signed).is_none());
        std::fs::set_permissions(
            fixture.0.join("nested/signed"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        std::fs::create_dir(fixture.0.join("extra-directory")).unwrap();
        assert!(verify(&root, &signed).is_none());
        std::fs::remove_dir(fixture.0.join("extra-directory")).unwrap();
        std::fs::write(fixture.0.join("extra"), b"unsigned").unwrap();
        assert!(verify(&root, &signed).is_none());
        std::fs::remove_file(fixture.0.join("extra")).unwrap();
        std::fs::remove_file(fixture.0.join("nested/signed")).unwrap();
        assert!(verify(&root, &signed).is_none());
        symlink("/etc/passwd", fixture.0.join("nested/signed")).unwrap();
        assert!(verify(&root, &signed).is_none());
        assert!(
            verify(
                &root,
                &std::collections::BTreeSet::from([std::path::PathBuf::from("../escape")])
            )
            .is_none()
        );
        assert!(
            verify(
                &root,
                &std::collections::BTreeSet::from([std::path::PathBuf::from("SHA256SUMS")])
            )
            .is_none()
        );
    }

    #[test]
    #[ignore = "read-only observation requires an installed derived release"]
    fn live_installed_inventory_matches_input_names() {
        let selector = std::path::Path::new(crate::installed_image::PRODUCT_ROOT).join("current");
        let root = File::open(selector).unwrap();
        let input = crate::release_identity::read_file_bytes(
            crate::release_identity::open_regular_at(&root, "INPUT_BUNDLE_SHA256SUMS").unwrap(),
            crate::release_identity::MAX_SEALED_TEXT_BYTES,
        )
        .unwrap();
        let mut signed = BTreeSet::new();
        for line in std::str::from_utf8(&input).unwrap().lines() {
            let (digest, path) = line.split_once("  ./").unwrap();
            assert!(crate::customer::package::is_lowercase_sha256(digest));
            assert!(signed.insert(PathBuf::from(path)));
        }
        assert!(verify(&root, &signed).is_some());
    }
}
