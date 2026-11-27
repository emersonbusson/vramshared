// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/published_update/http.rs
//! Bounded HTTPS reads of public release resources.

use std::io;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use ramshared_exec::bounded_process::run_capture_command;
use rustix::process::{Resource, getrlimit};

use super::catalog::{self, Asset, Candidate, Release};

fn curl(url: &str, asset_limit: Option<u64>) -> Command {
    let mut cmd = if let Some(size) = asset_limit {
        let limit = getrlimit(Resource::Fsize).current.unwrap_or(size).min(size);
        let mut limited = Command::new("/usr/bin/prlimit");
        limited
            .arg(format!("--fsize={limit}:{limit}"))
            .args(["--", "/usr/bin/curl"]);
        limited
    } else {
        Command::new("/usr/bin/curl")
    };
    cmd.env_clear().stdin(Stdio::null()).args([
        "--disable",
        "--fail",
        "--silent",
        "--show-error",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--connect-timeout",
        "5",
        "--url",
        url,
    ]);
    cmd
}

fn read_catalog(path: &str) -> Option<Vec<u8>> {
    let url = format!(
        "https://api.github.com/repos/{}/releases{path}",
        catalog::REPOSITORY
    );
    let mut cmd = curl(&url, None);
    cmd.args([
        "--header",
        "Accept: application/vnd.github+json",
        "--compressed",
    ]);
    let result = run_capture_command(
        &mut cmd,
        "release catalog",
        Duration::from_secs(12),
        catalog::METADATA_LIMIT,
        |_| {},
    )
    .ok()?;
    result.status.success().then_some(result.stdout)
}

pub(super) fn discover(running: &str) -> Option<Option<Candidate>> {
    let mut releases = Vec::new();
    for page in 1..=catalog::MAX_PAGES {
        let query = format!("?per_page={}&page={page}", catalog::PAGE_SIZE);
        let batch = catalog::parse_page(&read_catalog(&query)?)?;
        let final_page = batch.len() < catalog::PAGE_SIZE;
        releases.extend(batch);
        if final_page {
            break;
        }
    }
    Some(catalog::select_patch(&releases, running))
}

pub(super) fn by_tag(tag: &str) -> Option<Candidate> {
    crate::customer::release_ordering::stable_release_series(tag)?;
    let bytes = read_catalog(&format!("/tags/{tag}"))?;
    let release: Release = serde_json::from_slice(&bytes).ok()?;
    release.candidate(tag)
}

pub(super) fn download(asset: &Asset, tag: &str, destination: &Path) -> io::Result<()> {
    let mut cmd = curl(&asset.url(tag), Some(asset.size));
    cmd.args(["--location", "--max-redirs", "2", "--max-filesize"])
        .arg(asset.size.to_string())
        .arg("--output")
        .arg(destination);

    let result = run_capture_command(
        &mut cmd,
        "release asset",
        Duration::from_secs(45),
        8192,
        |_| {},
    )
    .map_err(io::Error::other)?;
    let metadata = destination.symlink_metadata()?;
    if !result.status.success() || !metadata.is_file() || metadata.len() != asset.size {
        return Err(io::Error::other(
            "published release asset transfer was incomplete",
        ));
    }
    Ok(())
}
