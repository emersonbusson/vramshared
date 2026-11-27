// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/release_ordering.rs
//! Release identity ordering, bounded newest-bundle discovery, policy, and posture tokens.

use std::cmp::Ordering;
use std::ffi::OsStr;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, fstat, openat, statat};
use rustix::process::geteuid;

use super::install_fs::{fs_error, read_bounded_regular_at};
use super::install_tokens::valid_release_token_segment;
use super::transition_custody::open_directory_no_follow;
use super::{CustomerError, CustomerReason, CustomerState};
use crate::cascade::boot::parse_scoped_approval;
use crate::cascade::posture::Posture;
use crate::installed_image::sha256_reader;
use crate::release_identity::{MAX_SEALED_TEXT_BYTES, read_release_version};

const MAX_BUNDLE_SCAN_ENTRIES: usize = 4096;
const MAX_POLICY_BYTES: usize = 64 * 1024;
const MAX_TOKEN_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReleaseOrder {
    Newer,
    Older,
    Equal,
    Unknown,
}

impl ReleaseOrder {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Newer => "newer",
            Self::Older => "older",
            Self::Equal => "equal",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct AutoUpdatePolicy {
    pub(super) enabled: bool,
    pub(super) lower_sink: Option<PathBuf>,
    pub(super) bundle_root: Option<PathBuf>,
    pub(super) release_keyring: Option<PathBuf>,
}

fn is_canonical_uint(seg: &str) -> bool {
    !seg.is_empty()
        && seg.bytes().all(|b| b.is_ascii_digit())
        && !(seg.len() > 1 && seg.starts_with('0'))
}

fn uint_key(value: &str) -> (usize, &str) {
    (value.len(), value)
}

struct ReleaseParts<'a> {
    version: [&'a str; 3],
    rc: Option<&'a str>,
    count: &'a str,
    dirty: bool,
}

impl<'a> ReleaseParts<'a> {
    fn cmp(&self, other: &Self) -> Ordering {
        // Stable releases sort after every RC with the same version numbers.
        let rc_key = |rc: Option<&'a str>| (rc.is_none(), rc.map(uint_key));
        self.version
            .map(uint_key)
            .cmp(&other.version.map(uint_key))
            .then_with(|| rc_key(self.rc).cmp(&rc_key(other.rc)))
            .then_with(|| uint_key(self.count).cmp(&uint_key(other.count)))
    }
}

/// Parse `vMAJOR.MINOR.PATCH[-rc.N][-COUNT-gHASH][-dirty|+dirty]`.
fn parse_release_parts(value: &str) -> Option<ReleaseParts<'_>> {
    if value.is_empty() || value.len() > 128 {
        return None;
    }
    let (core, is_dirty) = if let Some(stripped) = value.strip_suffix("-dirty") {
        (stripped, true)
    } else if let Some(stripped) = value.strip_suffix("+dirty") {
        (stripped, true)
    } else {
        (value, false)
    };
    let (base, count) = if let Some((prefix, hash)) = core.rsplit_once("-g") {
        if !(4..=64).contains(&hash.len()) || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let (base, count) = prefix.rsplit_once('-')?;
        if !is_canonical_uint(count) {
            return None;
        }
        (base, count)
    } else {
        (core, "0")
    };
    let (base, rc) = match base.split_once("-rc.") {
        Some((tag, rc)) if is_canonical_uint(rc) => (tag, Some(rc)),
        Some(_) => return None,
        None => (base, None),
    };
    let tag_nums = base.strip_prefix('v')?;
    let mut parts = tag_nums.split('.');
    let major = parts.next()?;
    let minor = parts.next()?;
    let patch = parts.next()?;
    if parts.next().is_some()
        || !is_canonical_uint(major)
        || !is_canonical_uint(minor)
        || !is_canonical_uint(patch)
    {
        return None;
    }
    Some(ReleaseParts {
        version: [major, minor, patch],
        rc,
        count,
        dirty: is_dirty,
    })
}

/// Strictly compares `candidate` vs `running` (`release_is_newer` single authority).
/// Supports stable tags, `-rc.N` tags and `git describe` identities derived from
/// either kind of tag. Refuses dirty candidates and malformed inputs.
pub(crate) fn release_is_newer(candidate: &str, running: &str) -> ReleaseOrder {
    let Some(candidate) = parse_release_parts(candidate) else {
        return ReleaseOrder::Unknown;
    };
    if candidate.dirty {
        return ReleaseOrder::Unknown;
    }
    let Some(running) = parse_release_parts(running) else {
        return ReleaseOrder::Unknown;
    };
    match candidate.cmp(&running) {
        Ordering::Greater => ReleaseOrder::Newer,
        Ordering::Less => ReleaseOrder::Older,
        Ordering::Equal => ReleaseOrder::Equal,
    }
}

/// Select the newest `<bundle_root>/*/release` directory without following symlinks.
pub(super) fn find_newest_bundle(bundle_root: &Path) -> Result<Option<PathBuf>, CustomerError> {
    let Some(root_dir) = open_directory_no_follow(bundle_root, false)? else {
        return Ok(None);
    };
    let mut entries = Dir::read_from(&root_dir).map_err(|_| {
        fs_error(
            CustomerReason::ObservationUnavailable,
            "NBD_INSTALL_REASON=BUNDLE_ROOT_UNREADABLE",
            CustomerState::UnknownBlocked,
        )
    })?;
    let mut best: Option<((i64, u64), PathBuf)> = None;
    let mut count = 0usize;
    while let Some(item) = entries.read() {
        let item = item.map_err(|_| {
            fs_error(
                CustomerReason::ObservationUnavailable,
                "NBD_INSTALL_REASON=BUNDLE_ROOT_UNREADABLE",
                CustomerState::UnknownBlocked,
            )
        })?;
        let Ok(name) = item.file_name().to_str() else {
            continue;
        };
        if matches!(name, "." | "..") {
            continue;
        }
        count += 1;
        if count > MAX_BUNDLE_SCAN_ENTRIES {
            return Err(fs_error(
                CustomerReason::UnsupportedSurface,
                "NBD_INSTALL_REASON=BUNDLE_ROOT_TOO_LARGE",
                CustomerState::InstalledOff,
            ));
        }
        let Ok(child_dir) = openat(
            &root_dir,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) else {
            continue;
        };
        let child_file = File::from(child_dir);
        let Ok(st) = statat(&child_file, "release", AtFlags::SYMLINK_NOFOLLOW) else {
            continue;
        };
        if rustix::fs::FileType::from_raw_mode(st.st_mode) != rustix::fs::FileType::Directory {
            continue;
        }
        let stamp = (st.st_mtime, st.st_mtime_nsec);
        let candidate_path = bundle_root.join(name).join("release");
        let replace = match &best {
            None => true,
            Some((best_stamp, best_path)) => {
                stamp > *best_stamp || (stamp == *best_stamp && candidate_path < *best_path)
            }
        };
        if replace {
            best = Some((stamp, candidate_path));
        }
    }
    Ok(best.map(|(_, p)| p))
}

/// Look for the selected input digest in candidate staging and the policy archive.
pub(super) fn find_previous_bundle(
    search_root: &Path,
    policy_path: &Path,
    owner_uid: u32,
    expected_manifest_sha256: &str,
) -> Result<Option<PathBuf>, CustomerError> {
    if let Some(path) = find_bundle_by_manifest_sha256(search_root, expected_manifest_sha256) {
        return Ok(Some(path));
    }
    Ok(read_auto_update_policy(policy_path, Some(owner_uid))?
        .and_then(|policy| policy.bundle_root)
        .filter(|root| root != search_root)
        .and_then(|root| find_bundle_by_manifest_sha256(&root, expected_manifest_sha256)))
}

/// Locate a signed bundle under `bundle_root` matching `expected_manifest_sha256`.
pub(super) fn find_bundle_by_manifest_sha256(
    bundle_root: &Path,
    expected_manifest_sha256: &str,
) -> Option<PathBuf> {
    let root_dir = open_directory_no_follow(bundle_root, false).ok()??;
    let mut entries = Dir::read_from(&root_dir).ok()?;
    let mut matches = Vec::new();
    let mut scanned = 0usize;
    while let Some(Ok(item)) = entries.read() {
        let Ok(name) = item.file_name().to_str() else {
            continue;
        };
        if matches!(name, "." | "..") {
            continue;
        }
        scanned += 1;
        if scanned > MAX_BUNDLE_SCAN_ENTRIES {
            return None;
        }
        let candidate = bundle_root.join(name).join("release");
        let Ok(Some(rel_dir)) = open_directory_no_follow(&candidate, false) else {
            continue;
        };
        if let Some(manifest_bytes) =
            read_bounded_regular_at(&rel_dir, OsStr::new("SHA256SUMS"), MAX_SEALED_TEXT_BYTES)
            && sha256_reader(&mut manifest_bytes.as_slice()).as_deref()
                == Some(expected_manifest_sha256)
            && read_release_version(&candidate).is_some()
        {
            matches.push(candidate);
        }
    }
    matches.sort();
    matches.into_iter().next()
}

pub(super) fn read_enrolled_posture(
    approval_dir: &Path,
    release: &str,
) -> Result<Option<Posture>, CustomerError> {
    if !valid_release_token_segment(release) {
        return Err(fs_error(
            CustomerReason::UnsupportedSurface,
            "NBD_INSTALL_REASON=RELEASE_VERSION_INVALID",
            CustomerState::InstalledOff,
        ));
    }
    let Some(dir) = open_directory_no_follow(approval_dir, false)? else {
        return Ok(None);
    };
    let name = format!("activate-{release}.token");
    let Some(bytes) = read_bounded_regular_at(&dir, OsStr::new(&name), MAX_TOKEN_BYTES) else {
        return Ok(None);
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Ok(None);
    };
    let Ok(approval) = parse_scoped_approval(text) else {
        return Ok(None);
    };
    if approval.release != release {
        return Ok(None);
    }
    Ok(Some(approval.posture))
}

fn validate_safe_absolute(raw: &str, key: &str) -> Result<PathBuf, CustomerError> {
    let path = Path::new(raw);
    if raw.is_empty()
        || !path.is_absolute()
        || path.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::CurDir | Component::Prefix(_)
            )
        })
    {
        return Err(fs_error(
            CustomerReason::UnsupportedSurface,
            format!("NBD_INSTALL_REASON=POLICY_VALUE_INVALID ({key})"),
            CustomerState::InstalledOff,
        ));
    }
    Ok(path.to_path_buf())
}

pub(super) fn parse_auto_update_policy(bytes: &[u8]) -> Result<AutoUpdatePolicy, CustomerError> {
    let text = std::str::from_utf8(bytes).map_err(|_| {
        fs_error(
            CustomerReason::UnsupportedSurface,
            "NBD_INSTALL_REASON=POLICY_NOT_UTF8",
            CustomerState::InstalledOff,
        )
    })?;
    if text.chars().any(|c| c.is_control() && c != '\n') {
        return Err(fs_error(
            CustomerReason::UnsupportedSurface,
            "NBD_INSTALL_REASON=POLICY_CONTROL_CHAR",
            CustomerState::InstalledOff,
        ));
    }
    let mut enabled = None;
    let mut lower_sink = None;
    let mut bundle_root = None;
    let mut release_keyring = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        match key {
            "enabled" if enabled.is_none() => enabled = Some(val == "1"),
            "lower_sink" if lower_sink.is_none() => {
                lower_sink = Some(validate_safe_absolute(val, "lower_sink")?);
            }
            "bundle_root" if bundle_root.is_none() => {
                bundle_root = Some(validate_safe_absolute(val, "bundle_root")?);
            }
            "release_keyring" if release_keyring.is_none() => {
                release_keyring = Some(validate_safe_absolute(val, "release_keyring")?);
            }
            "enabled" | "lower_sink" | "bundle_root" | "release_keyring" => {
                return Err(fs_error(
                    CustomerReason::UnsupportedSurface,
                    format!("NBD_INSTALL_REASON=POLICY_DUPLICATE_KEY ({key})"),
                    CustomerState::InstalledOff,
                ));
            }
            _ => {}
        }
    }
    Ok(AutoUpdatePolicy {
        enabled: enabled.unwrap_or(false),
        lower_sink,
        bundle_root,
        release_keyring,
    })
}

fn open_verified_policy_file(
    path: &Path,
    expected_owner: Option<u32>,
) -> Result<Option<File>, CustomerError> {
    let invalid = || {
        fs_error(
            CustomerReason::UnsupportedSurface,
            "NBD_INSTALL_REASON=POLICY_PATH_INVALID",
            CustomerState::InstalledOff,
        )
    };
    if !path.is_absolute() {
        return Err(invalid());
    }
    let parent_path = path.parent().ok_or_else(invalid)?;
    let leaf = path.file_name().ok_or_else(invalid)?;
    check_ancestor_ownership(parent_path, expected_owner)?;
    let Some(dir) = open_directory_no_follow(parent_path, false)? else {
        return Ok(None);
    };
    let file = match openat(
        &dir,
        leaf,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => File::from(fd),
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(_) => {
            return Err(fs_error(
                CustomerReason::UnsupportedSurface,
                "NBD_INSTALL_REASON=UNTRUSTED_POLICY",
                CustomerState::InstalledOff,
            ));
        }
    };
    let stat = fstat(&file).map_err(|_| {
        fs_error(
            CustomerReason::ObservationUnavailable,
            "cannot inspect auto-update policy metadata",
            CustomerState::UnknownBlocked,
        )
    })?;
    let owner = expected_owner.unwrap_or_else(|| geteuid().as_raw());
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_nlink != 1
        || stat.st_uid != owner
        || (stat.st_mode & 0o022) != 0
        || stat.st_size < 0
        || stat.st_size > MAX_POLICY_BYTES as i64
    {
        return Err(fs_error(
            CustomerReason::UnsupportedSurface,
            "NBD_INSTALL_REASON=UNTRUSTED_POLICY",
            CustomerState::InstalledOff,
        ));
    }
    Ok(Some(file))
}

pub(super) fn read_auto_update_policy(
    path: &Path,
    expected_owner: Option<u32>,
) -> Result<Option<AutoUpdatePolicy>, CustomerError> {
    let Some(file) = open_verified_policy_file(path, expected_owner)? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    file.take(MAX_POLICY_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            fs_error(
                CustomerReason::ObservationUnavailable,
                "cannot read auto-update policy",
                CustomerState::UnknownBlocked,
            )
        })?;
    if bytes.len() > MAX_POLICY_BYTES {
        return Err(fs_error(
            CustomerReason::UnsupportedSurface,
            "NBD_INSTALL_REASON=POLICY_TOO_LARGE",
            CustomerState::InstalledOff,
        ));
    }
    parse_auto_update_policy(&bytes).map(Some)
}

fn check_ancestor_ownership(path: &Path, expected_owner: Option<u32>) -> Result<(), CustomerError> {
    let Some(owner) = expected_owner else {
        return Ok(());
    };
    let mut acc = PathBuf::new();
    for comp in path.components() {
        acc.push(comp);
        if acc == Path::new("/") {
            continue;
        }
        let md = match std::fs::symlink_metadata(&acc) {
            Ok(md) => md,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) => {
                return Err(fs_error(
                    CustomerReason::UnsupportedSurface,
                    "NBD_INSTALL_REASON=UNTRUSTED_POLICY",
                    CustomerState::InstalledOff,
                ));
            }
        };
        if md.file_type().is_symlink() || !md.is_dir() || (md.uid() != 0 && md.uid() != owner) {
            return Err(fs_error(
                CustomerReason::UnsupportedSurface,
                "NBD_INSTALL_REASON=UNTRUSTED_POLICY",
                CustomerState::InstalledOff,
            ));
        }
        if (md.mode() & 0o022) != 0 {
            let sticky_non_parent = acc != path && md.uid() == 0 && (md.mode() & 0o1000) != 0;
            if !sticky_non_parent {
                return Err(fs_error(
                    CustomerReason::UnsupportedSurface,
                    "NBD_INSTALL_REASON=UNTRUSTED_POLICY",
                    CustomerState::InstalledOff,
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
