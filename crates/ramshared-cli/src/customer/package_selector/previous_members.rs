// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_selector/previous_members.rs
//! Bounded point-in-time comparison of prior files with signed input bytes.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::os::unix::fs::MetadataExt;

use rustix::fs::{Mode, OFlags, openat};

pub(super) fn member(root: &File, path: &str) -> Option<File> {
    if path.is_empty()
        || path.chars().any(char::is_control)
        || path.split('/').any(|part| matches!(part, "" | "." | ".."))
    {
        return None;
    }
    let mut parts = path.split('/').peekable();
    let mut directory = root.try_clone().ok()?;
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            return crate::release_identity::open_regular_at(&directory, part);
        }
        directory = File::from(
            openat(
                &directory,
                part,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .ok()?,
        );
    }
    None
}

pub(super) fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    a.is_file()
        && b.is_file()
        && (
            a.dev(),
            a.ino(),
            a.len(),
            a.mtime(),
            a.mtime_nsec(),
            a.ctime(),
            a.ctime_nsec(),
        ) == (
            b.dev(),
            b.ino(),
            b.len(),
            b.mtime(),
            b.mtime_nsec(),
            b.ctime(),
            b.ctime_nsec(),
        )
}

pub(super) fn parse_signed(manifest: &[u8]) -> Option<BTreeMap<&str, &str>> {
    let text = std::str::from_utf8(manifest).ok()?;
    let mut signed = BTreeMap::new();
    for line in text.lines() {
        let (expected, path) = line.split_once("  ./")?;
        if !crate::customer::package::is_lowercase_sha256(expected)
            || signed.insert(path, expected).is_some()
        {
            return None;
        }
    }
    if signed.is_empty() {
        return None;
    }
    Some(signed)
}

/// Refuse changed or missing signed members and retain verified handles.
pub(super) fn verify_and_collect(
    root: &File,
    manifest: &[u8],
    limits: (u64, u64),
    transformation: Option<&super::PreviousTransformation<'_>>,
) -> Option<BTreeMap<String, (File, String)>> {
    let signed = parse_signed(manifest)?;
    let paths: BTreeSet<_> = signed
        .keys()
        .map(|path| std::path::PathBuf::from(*path))
        .collect();
    super::previous_inventory::verify(root, &paths)?;
    let transformed = match transformation {
        Some(request) => Some(super::transformation::observe(
            root, &signed, manifest, request, limits,
        )?),
        None => None,
    };
    let mut total = transformed.as_ref().map_or(0, |(_, _, consumed)| *consumed);
    let member_limit = limits
        .0
        .min(crate::customer::package_source::MEMBER_LIMIT as u64);
    let mut collected = BTreeMap::new();
    for (path, signed_digest) in signed {
        let expected = transformed
            .as_ref()
            .filter(|(name, _, _)| name == path)
            .map_or(signed_digest, |(_, digest, _)| digest.as_str());
        let mut file = member(root, path)?;
        let before = file.metadata().ok()?;
        if !before.is_file() || before.len() > member_limit {
            return None;
        }
        // The streaming verifier probes one extra byte to detect file growth.
        total = total.checked_add(before.len().checked_add(1)?)?;
        if total > limits.1
            || crate::installed_image::sha256_reader_with_limit(&mut file, before.len()).as_deref()
                != Some(expected)
        {
            return None;
        }
        let after = file.metadata().ok()?;
        let named = member(root, path)?.metadata().ok()?;
        if !same_file(&before, &after) || !same_file(&before, &named) {
            return None;
        }
        if collected
            .insert(path.to_owned(), (file, expected.to_owned()))
            .is_some()
        {
            return None;
        }
    }
    if let Some(request) = transformation {
        request.sink.revalidate()?;
    }
    super::previous_inventory::verify(root, &paths)?;
    Some(collected)
}

/// Refuse changed or missing signed members; this is not permission to switch.
#[cfg(test)]
pub(super) fn verify(
    root: &File,
    manifest: &[u8],
    limits: (u64, u64),
    transformation: Option<&super::PreviousTransformation<'_>>,
) -> Option<()> {
    verify_and_collect(root, manifest, limits, transformation).map(|_| ())
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
    fn verifies_nested_members_and_refuses_tampering_links_and_limits() {
        let path =
            std::env::temp_dir().join(format!("ramshared-prior-members-{}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        let fixture = Fixture(path);
        std::fs::set_permissions(&fixture.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::create_dir(fixture.0.join("nested")).unwrap();
        std::fs::set_permissions(
            fixture.0.join("nested"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        std::fs::write(fixture.0.join("nested/file"), b"original").unwrap();
        std::fs::set_permissions(
            fixture.0.join("nested/file"),
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
        let root = File::open(&fixture.0).unwrap();
        let hash = crate::installed_image::sha256_reader(&mut b"original".as_slice()).unwrap();
        let manifest = format!("{hash}  ./nested/file\n");
        assert!(verify(&root, manifest.as_bytes(), (8, 9), None).is_some());
        assert!(verify(&root, manifest.as_bytes(), (7, 9), None).is_none());
        assert!(verify(&root, manifest.as_bytes(), (8, 8), None).is_none());
        assert!(
            verify(
                &root,
                format!("{manifest}{manifest}").as_bytes(),
                (16, 16),
                None
            )
            .is_none()
        );
        assert!(
            verify(
                &root,
                format!("{hash}  ./../file\n").as_bytes(),
                (8, 9),
                None
            )
            .is_none()
        );
        std::fs::write(fixture.0.join("nested/file"), b"modified").unwrap();
        assert!(verify(&root, manifest.as_bytes(), (8, 9), None).is_none());
        std::fs::remove_file(fixture.0.join("nested/file")).unwrap();
        symlink("/etc/passwd", fixture.0.join("nested/file")).unwrap();
        assert!(verify(&root, manifest.as_bytes(), (8, 9), None).is_none());
        std::fs::remove_file(fixture.0.join("nested/file")).unwrap();
        std::fs::remove_dir(fixture.0.join("nested")).unwrap();
        symlink("/etc", fixture.0.join("nested")).unwrap();
        assert!(verify(&root, manifest.as_bytes(), (8, 9), None).is_none());
    }
}
