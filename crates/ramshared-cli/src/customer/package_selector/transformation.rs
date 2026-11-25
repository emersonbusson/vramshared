// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_selector/transformation.rs
//! Observe one configuration derived from an authenticated input template.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;

use super::PreviousTransformation;
use super::previous_members::{member, same_file};

fn read_member(root: &File, path: &str, max_bytes: u64, remaining: &mut u64) -> Option<Vec<u8>> {
    let mut file = member(root, path)?;
    let before = file.metadata().ok()?;
    let charged = before.len().checked_add(1)?;
    if !before.is_file() || before.len() > max_bytes || charged > *remaining {
        return None;
    }
    *remaining = remaining.checked_sub(charged)?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(before.len().checked_add(1)?)
        .read_to_end(&mut bytes)
        .ok()?;
    let after = file.metadata().ok()?;
    let named = member(root, path)?.metadata().ok()?;
    (u64::try_from(bytes.len()).ok()? == before.len()
        && same_file(&before, &after)
        && same_file(&before, &named))
    .then_some(bytes)
}

fn digest(bytes: &[u8]) -> Option<String> {
    let mut input = bytes;
    crate::installed_image::sha256_reader(&mut input)
}

/// This proves a byte transformation, not permission to switch a release.
pub(super) fn observe(
    root: &File,
    signed: &BTreeMap<&str, &str>,
    manifest: &[u8],
    transformation: &PreviousTransformation<'_>,
    limits: (u64, u64),
) -> Option<(String, String, u64)> {
    let text_limit = crate::release_identity::MAX_SEALED_TEXT_BYTES
        .min(usize::try_from(limits.0).unwrap_or(usize::MAX));
    if transformation.template.len() > text_limit {
        return None;
    }
    let mut remaining = limits
        .1
        .checked_sub(u64::try_from(transformation.template.len()).ok()?)?;
    transformation.sink.revalidate()?;
    let payload = read_member(
        root,
        "release-manifest.json",
        limits
            .0
            .min(crate::customer::package::MAX_PAYLOAD_MANIFEST_BYTES as u64),
        &mut remaining,
    )?;
    if digest(&payload)?.as_str() != *signed.get("release-manifest.json")? {
        return None;
    }
    let declaration = crate::customer::package::PayloadManifest::parse(&payload).ok()?;
    if !declaration
        .configuration
        .examples
        .iter()
        .any(|role| role == transformation.role)
    {
        return None;
    }
    let entry = declaration
        .contents
        .iter()
        .find(|entry| entry.role == transformation.role)?;
    if entry.kind != crate::customer::package::ManifestKind::ConfigExample {
        return None;
    }
    let destination = crate::customer::package_member::bundle_destination(entry).ok()??;
    let expected = *signed.get(destination.as_str())?;
    if entry.sha256.as_deref() != Some(expected)
        || digest(transformation.template)?.as_str() != expected
    {
        return None;
    }
    let provenance = read_member(
        root,
        "INSTALL_PROVENANCE.json",
        limits
            .0
            .min(crate::release_identity::MAX_SEALED_JSON_BYTES as u64),
        &mut remaining,
    )?;
    let record: serde_json::Value = serde_json::from_slice(&provenance).ok()?;
    let binding = transformation.sink.binding();
    let lower = record.get("lower_sink")?;
    if record.get("schema_version")?.as_str()
        != Some(crate::installed_image::INSTALLED_PROVENANCE_V2)
        || record.get("input_bundle_manifest_sha256")?.as_str() != Some(digest(manifest)?.as_str())
        || lower.get("canonical_path")?.as_str() != Some(binding.path)
        || lower.get("identity_sha256")?.as_str() != Some(binding.identity)
        || lower.get("filesystem_block_bytes")?.as_u64() != Some(binding.block_bytes)
    {
        return None;
    }
    let derived_limit = text_limit.min(usize::try_from(remaining).unwrap_or(usize::MAX));
    let derived =
        super::derived_config::reconstruct(transformation.template, &binding, derived_limit)?;
    remaining = remaining.checked_sub(u64::try_from(derived.len()).ok()?)?;
    let consumed = limits.1.checked_sub(remaining)?;
    transformation.sink.revalidate()?;
    Some((destination, digest(&derived)?, consumed))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    struct Fixture(std::path::PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn member_read_charges_the_growth_probe_before_reading() {
        let root =
            std::env::temp_dir().join(format!("ramshared-transform-budget-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let fixture = Fixture(root);
        std::fs::write(fixture.0.join("member"), b"12345678").unwrap();
        let directory = File::open(&fixture.0).unwrap();
        let mut short = 8;
        assert!(read_member(&directory, "member", 8, &mut short).is_none());
        assert_eq!(short, 8);
        let mut exact = 9;
        assert_eq!(
            read_member(&directory, "member", 8, &mut exact).unwrap(),
            b"12345678"
        );
        assert_eq!(exact, 0);
    }
}
