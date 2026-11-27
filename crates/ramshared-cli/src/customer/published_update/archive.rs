// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/published_update/archive.rs
//! Bounded extraction into a private newly created directory.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions, create_dir_all};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use flate2::read::GzDecoder;

const MAX_EXPANDED_BYTES: u64 = 512 * 1024 * 1024;
const MAX_FILES: usize = 1024;
const MAX_PATH_BYTES: usize = 512;
const MAX_DEPTH: usize = 12;

fn reject() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid published release archive",
    )
}

fn package_path(bytes: &[u8], root: &str, directory: bool) -> Option<String> {
    let path = std::str::from_utf8(bytes).ok()?;
    let path = if directory {
        path.trim_end_matches('/')
    } else {
        path
    };
    if path.len() > MAX_PATH_BYTES {
        return None;
    }
    let parts: Vec<_> = path.split('/').collect();
    if parts.len() > MAX_DEPTH
        || parts.first().copied() != Some(root)
        || parts.iter().any(|part| {
            part.is_empty()
                || matches!(*part, "." | "..")
                || !part
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b".-_".contains(&c))
        })
        || (parts.len() == 1 && !directory)
        || (parts.len() > 1 && parts[1] != "release")
        || (parts.len() == 2 && !directory)
    {
        return None;
    }
    Some(path.to_owned())
}

pub(super) fn extract(input: File, stage: &Path, release_root: &str) -> io::Result<()> {
    let mut archive = tar::Archive::new(GzDecoder::new(input).take(MAX_EXPANDED_BYTES + 1));
    let mut paths = BTreeSet::new();
    let mut directories = BTreeSet::new();
    for member in archive.entries()? {
        let mut entry = member?;
        let kind = entry.header().entry_type();
        if (!kind.is_file() && !kind.is_dir()) || entry.link_name()?.is_some() {
            return Err(reject());
        }
        let path =
            package_path(&entry.path_bytes(), release_root, kind.is_dir()).ok_or_else(reject)?;
        if paths.len() >= MAX_FILES || !paths.insert(path.clone()) {
            return Err(reject());
        }
        let destination = stage.join(path);
        let directory = if kind.is_dir() {
            if entry.size() != 0 {
                return Err(reject());
            }
            destination.as_path()
        } else {
            destination.parent().ok_or_else(reject)?
        };
        create_dir_all(directory)?;
        directories.extend(
            directory
                .ancestors()
                .take_while(|parent| *parent != stage)
                .map(Path::to_path_buf),
        );
        if kind.is_file() {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&destination)?;
            if io::copy(&mut entry, &mut file)? != entry.size() {
                return Err(reject());
            }
            file.sync_all()?;
        }
    }
    let mut remaining = archive.into_inner();
    io::copy(&mut remaining, &mut io::sink())?;
    if paths.is_empty() || remaining.limit() == 0 {
        return Err(reject());
    }
    for directory in directories.into_iter().rev() {
        File::open(directory)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
