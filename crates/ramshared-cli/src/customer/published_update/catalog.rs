// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/published_update/catalog.rs
//! Public metadata selection. Package authorization comes from OpenPGP.

use serde::Deserialize;

use crate::customer::release_ordering::{ReleaseOrder, release_is_newer, stable_release_series};

pub(super) const REPOSITORY: &str = "emersonbusson/vramshared";
pub(super) const PAGE_SIZE: usize = 8;
pub(super) const MAX_PAGES: usize = 4;
pub(super) const METADATA_LIMIT: usize = 512 * 1024;
pub(super) const BUNDLE_LIMIT: u64 = 32 * 1024 * 1024;
const CHECKSUM_LIMIT: u64 = 4096;
const SIGNATURE_LIMIT: u64 = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Candidate {
    pub(super) tag: String,
    pub(super) archive: Asset,
    pub(super) checksum: Asset,
    pub(super) signature: Asset,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Asset {
    pub(super) name: String,
    pub(super) size: u64,
}

impl Asset {
    pub(super) fn url(&self, tag: &str) -> String {
        format!(
            "https://github.com/{REPOSITORY}/releases/download/{tag}/{}",
            self.name
        )
    }
}

#[derive(Deserialize)]
pub(super) struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<AssetEntry>,
}

#[derive(Deserialize)]
struct AssetEntry {
    name: String,
    state: String,
    size: u64,
    browser_download_url: String,
}

pub(super) fn parse_page(bytes: &[u8]) -> Option<Vec<Release>> {
    if bytes.len() > METADATA_LIMIT {
        return None;
    }
    let releases: Vec<Release> = serde_json::from_slice(bytes).ok()?;
    (releases.len() <= PAGE_SIZE).then_some(releases)
}

impl Release {
    fn asset(&self, name: String, limit: u64) -> Option<Asset> {
        let mut matches = self.assets.iter().filter(|entry| entry.name == name);
        let entry = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        let asset = Asset {
            name,
            size: entry.size,
        };
        (entry.state == "uploaded"
            && (1..=limit).contains(&entry.size)
            && entry.browser_download_url == asset.url(&self.tag_name))
        .then_some(asset)
    }

    pub(super) fn candidate(&self, tag: &str) -> Option<Candidate> {
        if self.draft
            || self.prerelease
            || self.tag_name != tag
            || stable_release_series(tag).is_none()
        {
            return None;
        }
        let name = format!("ramshared-linux-{tag}.signed.tar.gz");
        Some(Candidate {
            tag: tag.into(),
            checksum: self.asset(format!("{name}.sha256"), CHECKSUM_LIMIT)?,
            signature: self.asset(format!("{name}.sha256.asc"), SIGNATURE_LIMIT)?,
            archive: self.asset(name, BUNDLE_LIMIT)?,
        })
    }
}

pub(super) fn select_patch(releases: &[Release], running: &str) -> Option<Candidate> {
    let series = stable_release_series(running)?;
    let mut best: Option<Candidate> = None;
    for release in releases {
        let tag = release.tag_name.as_str();
        if stable_release_series(tag) != Some(series)
            || release_is_newer(tag, running) != ReleaseOrder::Newer
        {
            continue;
        }
        let Some(candidate) = release.candidate(tag) else {
            continue;
        };
        if best
            .as_ref()
            .is_none_or(|prior| release_is_newer(tag, &prior.tag) == ReleaseOrder::Newer)
        {
            best = Some(candidate);
        }
    }
    best
}

#[cfg(test)]
mod tests;
