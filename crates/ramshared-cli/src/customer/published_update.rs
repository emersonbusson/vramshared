// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/published_update.rs
//! Retain authenticated public bundles for the existing installer transaction.

use std::fs::{File, create_dir, rename};
use std::io::{self, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use super::install_fs::ensure_dir_no_follow;
use super::package_source::DeclaredBundle;
use super::package_trust::{verify_bundle_from_policy, verify_bundle_with_keyring};
use super::{CustomerError, CustomerReason, CustomerState};
use crate::installed_image::{sha256_reader, valid_sha256};

mod archive;
mod catalog;
mod http;
mod lease;
mod progress;

use catalog::Candidate;

pub(super) fn cache_root(product_root: &Path) -> PathBuf {
    product_root.join("published-bundles")
}

pub(super) fn ensure_cached_patch(
    cache: &Path,
    lock_root: &Path,
    policy_path: &Path,
    keyring: &Path,
    running: &str,
    previous_manifest: &str,
    local_previous_exists: bool,
) -> Result<Option<PathBuf>, CustomerError> {
    if super::release_ordering::stable_release_series(running).is_none() {
        return Ok(None);
    }
    let Some(_fetch_lease) = lease::admit(lock_root)? else {
        return Ok(None);
    };
    let Some(candidate) = http::discover(running)
        .ok_or_else(|| unavailable("cannot read published release catalog"))?
    else {
        return Ok(None);
    };
    let directory = ensure_dir_no_follow(cache, 0o700, "published bundle directory")?;
    if !local_previous_exists {
        let stored = cache
            .join(format!("ramshared-linux-{running}"))
            .join("release");
        if bundle_manifest(&stored, running, policy_path).as_deref() != Some(previous_manifest) {
            let predecessor = http::by_tag(running).ok_or_else(|| {
                unavailable("signed package for the installed release is unavailable")
            })?;
            retain(
                &predecessor,
                cache,
                policy_path,
                keyring,
                Some(previous_manifest),
                false,
            )?;
        }
    }
    let bundle = retain(&candidate, cache, policy_path, keyring, None, true)?;
    directory
        .sync_all()
        .map_err(|_| unavailable("cannot sync published bundle directory"))?;
    Ok(Some(bundle))
}

fn unavailable(detail: &str) -> CustomerError {
    super::install_fs::fs_error(
        CustomerReason::ObservationUnavailable,
        detail,
        CustomerState::InstalledOff,
    )
}

fn untrusted() -> CustomerError {
    super::install_fs::fs_error(
        CustomerReason::AuthorizationRequired,
        "published release could not be authenticated",
        CustomerState::InstalledOff,
    )
}

fn bundle_manifest(bundle: &Path, tag: &str, policy: &Path) -> Option<String> {
    let signature = verify_bundle_from_policy(bundle, policy, 0)?;
    let declared = DeclaredBundle::open_verified(bundle, &signature)?;
    let raw = declared.member("RELEASE_VERSION", 4096)?;
    if crate::release_identity::parse_identity_line(&raw).as_deref() != Some(tag) {
        return None;
    }
    Some(signature.manifest_sha256().into())
}

fn checksum_for(body: &[u8], name: &str) -> Option<String> {
    let line = std::str::from_utf8(body).ok()?.strip_suffix('\n')?;
    let (digest, asset) = line.split_once("  ")?;
    (valid_sha256(digest) && asset == name).then(|| digest.to_owned())
}

fn authenticate_outer(archive: &mut File, expected: &str) -> io::Result<()> {
    if sha256_reader(archive).as_deref() != Some(expected) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid archive digest",
        ));
    }
    archive.seek(SeekFrom::Start(0))?;
    Ok(())
}

fn retain(
    candidate: &Candidate,
    cache: &Path,
    policy: &Path,
    keyring: &Path,
    expected_manifest: Option<&str>,
    observe_download: bool,
) -> Result<PathBuf, CustomerError> {
    let root = format!("ramshared-linux-{}", candidate.tag);
    let destination = cache.join(&root);
    let installed = destination.join("release");
    if destination.symlink_metadata().is_ok() {
        let manifest = bundle_manifest(&installed, &candidate.tag, policy).ok_or_else(untrusted)?;
        if expected_manifest.is_some_and(|expected| expected != manifest) {
            return Err(untrusted());
        }
        return Ok(installed);
    }

    let stage = tempfile::Builder::new()
        .prefix(".published-")
        .tempdir_in(cache)
        .map_err(|_| unavailable("cannot stage published release"))?;
    let outer = stage.path().join("outer");
    create_dir(&outer).map_err(|_| unavailable("cannot stage published signature"))?;
    http::download(
        &candidate.checksum,
        &candidate.tag,
        &outer.join("SHA256SUMS"),
    )
    .and_then(|()| {
        http::download(
            &candidate.signature,
            &candidate.tag,
            &outer.join("SHA256SUMS.asc"),
        )
    })
    .map_err(|_| unavailable("cannot download published checksums"))?;
    let proof = verify_bundle_with_keyring(&outer, keyring, 0).ok_or_else(untrusted)?;
    let body = proof.verified_manifest().ok_or_else(untrusted)?;
    let expected = checksum_for(&body, &candidate.archive.name).ok_or_else(untrusted)?;

    let path = stage.path().join("archive");
    {
        let _progress = observe_download.then(|| {
            progress::TransferProgress::start(&path, &candidate.tag, candidate.archive.size)
        });
        http::download(&candidate.archive, &candidate.tag, &path)
            .map_err(|_| unavailable("cannot download published release archive"))?;
    }
    let mut file = File::open(&path).map_err(|_| unavailable("cannot open downloaded archive"))?;
    authenticate_outer(&mut file, &expected).map_err(|_| untrusted())?;
    archive::extract(file, stage.path(), &root).map_err(|_| untrusted())?;

    let verified = stage.path().join(&root);
    let manifest =
        bundle_manifest(&verified.join("release"), &candidate.tag, policy).ok_or_else(untrusted)?;
    if expected_manifest.is_some_and(|expected| expected != manifest) {
        return Err(untrusted());
    }
    rename(&verified, &destination).map_err(|_| unavailable("cannot retain verified bundle"))?;
    Ok(installed)
}

#[cfg(test)]
mod tests;
