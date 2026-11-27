// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/installed_image/manifest.rs
//! Sealed-release manifest verification and installed metadata readers.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::release_identity::{
    MAX_IDENTITY_LINE_BYTES, MAX_SEALED_JSON_BYTES, MAX_SEALED_TEXT_BYTES, SEALED_IDENTITY_FILES,
    parse_identity_line, read_regular_json, read_regular_line,
};

use super::binary_hash::sha256_file;
use super::release_tree::ReleaseTree;
use super::{
    DIRECT_INSTALL_METADATA_V2, INSTALLED_PROVENANCE_V1, INSTALLED_PROVENANCE_V2, InstallKind,
    InstalledIdentity, ProcessInstallOrigin, full_commit_sha, normalize_version_identity,
    valid_tree_state,
};

pub(crate) fn read_installed_identity_uncached(
    executable: &Path,
    kind: InstallKind,
) -> Option<InstalledIdentity> {
    if !fs::metadata(executable)
        .ok()
        .is_some_and(|metadata| metadata.is_file())
    {
        return None;
    }
    match kind {
        InstallKind::ActiveRelease => Some(
            read_versioned_release_identity(executable).unwrap_or_else(|| InstalledIdentity {
                executable: executable.to_path_buf(),
                version: None,
                source_commit: None,
                source_tree_state: None,
                installed_at_utc: read_install_timestamp_for_executable(executable),
                executable_sha256: None,
            }),
        ),
        InstallKind::Direct => Some(read_direct_install_identity(executable)),
    }
}

pub(crate) fn read_versioned_release_identity(executable: &Path) -> Option<InstalledIdentity> {
    let release_root = executable.parent()?.parent()?;
    let tree = ReleaseTree::open(release_root)?;
    let manifest_hashes = verify_release_manifest_tree(&tree)?;
    let verified_line = |name: &str| {
        let expected = manifest_hashes.get(name)?;
        let bytes = tree.verified_bytes(name, expected, MAX_IDENTITY_LINE_BYTES)?;
        parse_identity_line(&bytes)
    };
    let version = verified_line("RELEASE_VERSION")?;
    let version = normalize_version_identity(&version)?;
    let commit = verified_line("SOURCE_COMMIT")?;
    let commit = full_commit_sha(&commit)?;
    let tree_state = verified_line("SOURCE_TREE_STATE")?;
    if !valid_tree_state(&tree_state) {
        return None;
    }

    let provenance = tree.verified_bytes(
        "INSTALL_PROVENANCE.json",
        manifest_hashes.get("INSTALL_PROVENANCE.json")?,
        MAX_SEALED_JSON_BYTES,
    )?;
    let record: Value = serde_json::from_slice(&provenance).ok()?;
    let schema = record.get("schema_version").and_then(Value::as_str)?;
    if !matches!(schema, INSTALLED_PROVENANCE_V1 | INSTALLED_PROVENANCE_V2)
        || record.get("source_commit").and_then(Value::as_str) != Some(commit.as_str())
        || record.get("source_tree_state").and_then(Value::as_str) != Some(tree_state.as_str())
    {
        return None;
    }
    let installed_at_utc = record
        .get("installed_at_utc")
        .and_then(Value::as_str)
        .filter(|value| is_utc_timestamp(value))
        .map(str::to_string);
    if schema == INSTALLED_PROVENANCE_V2 && installed_at_utc.is_none() {
        return None;
    }
    tree.matches_selected(release_root)
        .then_some(InstalledIdentity {
            executable: executable.to_path_buf(),
            version: Some(version),
            source_commit: Some(commit),
            source_tree_state: Some(tree_state),
            installed_at_utc,
            executable_sha256: manifest_hashes.get("bin/ramshared").cloned(),
        })
}

pub(crate) fn read_direct_install_identity(executable: &Path) -> InstalledIdentity {
    let metadata_path = direct_install_metadata_path(executable);
    let timestamp_path = direct_install_timestamp_path(executable);
    let timestamp_fallback = timestamp_path
        .as_deref()
        .and_then(read_install_timestamp_file);
    let Some(path) = metadata_path else {
        return direct_install_identity_without_record(executable, timestamp_fallback);
    };
    let Some(record) = read_regular_json(&path) else {
        return direct_install_identity_without_record(executable, timestamp_fallback);
    };
    let valid_schema =
        record.get("schema_version").and_then(Value::as_str) == Some(DIRECT_INSTALL_METADATA_V2);
    let version = record
        .get("version")
        .and_then(Value::as_str)
        .and_then(normalize_version_identity);
    let source_commit = record
        .get("source_commit")
        .and_then(Value::as_str)
        .and_then(full_commit_sha);
    let source_tree_state = record
        .get("source_tree_state")
        .and_then(Value::as_str)
        .filter(|value| valid_tree_state(value))
        .map(str::to_string);
    let installed_at_utc = record
        .get("installed_at_utc")
        .and_then(Value::as_str)
        .filter(|value| is_utc_timestamp(value))
        .map(str::to_string);
    let cli_sha256 = record
        .get("cli_sha256")
        .and_then(Value::as_str)
        .filter(|value| valid_sha256(value))
        .map(str::to_string);
    let daemon_sha256 = record
        .get("daemon_sha256")
        .and_then(Value::as_str)
        .filter(|value| valid_sha256(value))
        .map(str::to_string);
    let daemon_executable = executable.parent().map(|parent| parent.join("ramsharedd"));
    let hashes_match = cli_sha256
        .as_deref()
        .is_some_and(|expected| sha256_file(executable).as_deref() == Some(expected))
        && daemon_sha256.as_deref().is_some_and(|expected| {
            daemon_executable
                .as_deref()
                .and_then(sha256_file)
                .as_deref()
                == Some(expected)
        });
    if valid_schema
        && version.is_some()
        && source_commit.is_some()
        && source_tree_state.is_some()
        && installed_at_utc.is_some()
        && hashes_match
    {
        InstalledIdentity {
            executable: executable.to_path_buf(),
            version,
            source_commit,
            source_tree_state,
            installed_at_utc,
            executable_sha256: cli_sha256,
        }
    } else {
        InstalledIdentity {
            executable: executable.to_path_buf(),
            version: None,
            source_commit: None,
            source_tree_state: None,
            installed_at_utc: installed_at_utc.or(timestamp_fallback),
            executable_sha256: None,
        }
    }
}

/// Identity of a direct install that carries no metadata record of its own.
///
/// A direct entry that is a link into a sealed release is that release's
/// binary, and the release tree already identifies it. Reporting "identity
/// unknown" for it contradicts the one-identity model: the header would show
/// an unidentified install beside an identified one for what is the same file.
///
/// A direct record that exists but fails its own validation does **not** take
/// this path. That case stays unidentified so a hash mismatch is never
/// papered over by borrowing the release identity.
pub(crate) fn direct_install_identity_without_record(
    executable: &Path,
    timestamp_fallback: Option<String>,
) -> InstalledIdentity {
    let unresolved = || InstalledIdentity {
        executable: executable.to_path_buf(),
        version: None,
        source_commit: None,
        source_tree_state: None,
        installed_at_utc: timestamp_fallback.clone(),
        executable_sha256: None,
    };
    // `parent().parent()` in the release reader is lexical, so a link has to
    // be resolved before the release tree around the real binary is visible.
    let resolved = fs::canonicalize(executable).unwrap_or_else(|_| executable.to_path_buf());
    let Some(release) = read_versioned_release_identity(&resolved) else {
        return unresolved();
    };
    InstalledIdentity {
        executable: executable.to_path_buf(),
        version: release.version,
        source_commit: release.source_commit,
        source_tree_state: release.source_tree_state,
        installed_at_utc: release.installed_at_utc.or(timestamp_fallback),
        executable_sha256: release.executable_sha256,
    }
}

pub(crate) fn verify_release_manifest(release_root: &Path) -> Option<HashMap<String, String>> {
    let tree = ReleaseTree::open(release_root)?;
    let hashes = verify_release_manifest_tree(&tree)?;
    tree.matches_selected(release_root).then_some(hashes)
}

/// Observe the original bundle digest bound by the installed manifest and provenance.
/// This is a point-in-time consistency check, not publisher authentication.
pub(crate) fn observe_installed_input_bundle_digest(release_root: &Path) -> Option<String> {
    let tree = ReleaseTree::open(release_root)?;
    let hashes = verify_release_manifest_tree(&tree)?;
    let digest = input_bundle_digest(&tree, &hashes)?;
    tree.matches_selected(release_root)
        .then(|| digest.to_owned())
}

pub(crate) fn verify_committed_release(
    release_root: &Path,
    expected_seal: &str,
    expected_input: &str,
) -> Option<HashMap<String, String>> {
    let tree = ReleaseTree::open(release_root)?;
    if tree.line("INSTALLED_MANIFEST_SHA256")?.as_str() != expected_seal {
        return None;
    }
    let hashes = verify_release_manifest_tree(&tree)?;
    (input_bundle_digest(&tree, &hashes)? == expected_input && tree.matches_selected(release_root))
        .then_some(hashes)
}

fn input_bundle_digest<'a>(
    tree: &ReleaseTree,
    hashes: &'a HashMap<String, String>,
) -> Option<&'a str> {
    let digest = hashes.get("INPUT_BUNDLE_SHA256SUMS")?;
    tree.verified_bytes("INPUT_BUNDLE_SHA256SUMS", digest, MAX_SEALED_TEXT_BYTES)?;
    let provenance = tree.verified_bytes(
        "INSTALL_PROVENANCE.json",
        hashes.get("INSTALL_PROVENANCE.json")?,
        MAX_SEALED_JSON_BYTES,
    )?;
    let record: Value = serde_json::from_slice(&provenance).ok()?;
    if !matches!(
        record.get("schema_version").and_then(Value::as_str),
        Some(INSTALLED_PROVENANCE_V1 | INSTALLED_PROVENANCE_V2)
    ) || record
        .get("input_bundle_manifest_sha256")
        .and_then(Value::as_str)
        != Some(digest.as_str())
    {
        return None;
    }
    Some(digest)
}

pub(super) fn verify_release_manifest_tree(tree: &ReleaseTree) -> Option<HashMap<String, String>> {
    let expected_manifest_hash = tree.line("INSTALLED_MANIFEST_SHA256")?;
    if !valid_sha256(&expected_manifest_hash) {
        return None;
    }
    let manifest_bytes =
        tree.verified_bytes("SHA256SUMS", &expected_manifest_hash, MAX_SEALED_TEXT_BYTES)?;
    let manifest = std::str::from_utf8(&manifest_bytes).ok()?;
    let mut hashes = HashMap::new();
    for line in manifest.lines() {
        let bytes = line.as_bytes();
        if bytes.len() < 66 || &bytes[64..66] != b"  " {
            return None;
        }
        let digest = std::str::from_utf8(&bytes[..64]).ok()?;
        let relative_path = std::str::from_utf8(&bytes[66..]).ok()?;
        let path = relative_path.strip_prefix("./")?;
        if !valid_sha256(digest)
            || path.is_empty()
            || path
                .split('/')
                .any(|component| matches!(component, "" | "." | ".."))
            || path.chars().any(char::is_control)
            || Path::new(path).is_absolute()
            || Path::new(path)
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
            || hashes
                .insert(path.to_string(), digest.to_ascii_lowercase())
                .is_some()
        {
            return None;
        }
    }
    // Binaries plus the identity documents. The document names come from the
    // shared list so this and the install-fingerprint sweep cannot drift.
    let mut covered = vec!["bin/ramshared", "bin/ramsharedd"];
    covered.extend_from_slice(SEALED_IDENTITY_FILES);
    for path in covered {
        let expected = hashes.get(path)?;
        if tree.sha256(path).as_deref() != Some(expected.as_str()) {
            return None;
        }
    }
    Some(hashes)
}

pub(crate) fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(crate) fn direct_install_metadata_path(executable: &Path) -> Option<PathBuf> {
    let prefix = executable.parent()?.parent()?;
    Some(prefix.join("share/ramshared/INSTALL_METADATA.json"))
}

pub(crate) fn is_utc_timestamp(value: &str) -> bool {
    let bytes = value.as_bytes();
    if !(bytes.len() == 20
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'Z'
        && bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19) || byte.is_ascii_digit()
        }))
    {
        return false;
    }
    let year = value[0..4].parse::<u16>().unwrap_or(0);
    let month = value[5..7].parse::<u8>().unwrap_or(0);
    let day = value[8..10].parse::<u8>().unwrap_or(0);
    let hour = value[11..13].parse::<u8>().unwrap_or(24);
    let minute = value[14..16].parse::<u8>().unwrap_or(60);
    let second = value[17..19].parse::<u8>().unwrap_or(60);
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 400 == 0 || year % 4 == 0 && year % 100 != 0 => 29,
        2 => 28,
        _ => return false,
    };
    year > 0 && (1..=days_in_month).contains(&day) && hour < 24 && minute < 60 && second < 60
}

pub(crate) fn read_install_timestamp_for_executable(executable: &Path) -> Option<String> {
    let release_root = executable.parent()?.parent()?;
    read_install_timestamp_from_release(release_root)
        .or_else(|| read_install_timestamp_file(&direct_install_timestamp_path(executable)?))
}

pub(crate) fn direct_install_timestamp_path(executable: &Path) -> Option<PathBuf> {
    let prefix = executable.parent()?.parent()?;
    Some(prefix.join("share/ramshared/INSTALL_TIMESTAMP"))
}

pub(crate) fn read_install_timestamp_from_release(release_root: &Path) -> Option<String> {
    let path = release_root.join("INSTALL_PROVENANCE.json");
    let record = read_regular_json(&path)?;
    match record.get("schema_version").and_then(Value::as_str) {
        Some(INSTALLED_PROVENANCE_V1 | INSTALLED_PROVENANCE_V2) => {}
        _ => return None,
    }
    record
        .get("installed_at_utc")
        .and_then(Value::as_str)
        .filter(|value| is_utc_timestamp(value))
        .map(str::to_string)
}

/// The install timestamp is one sealed single-line document: same reader as
/// every other identity line, then the timestamp format is checked on top.
/// A second hand-rolled line parser is the DT-10 dual-reader defect again.
pub(crate) fn read_install_timestamp_file(path: &Path) -> Option<String> {
    read_regular_line(path).filter(|timestamp| is_utc_timestamp(timestamp))
}

/// Promotes `Unknown` only when a live TUI process is executing from a
/// sealed release directory inside `<product_root>/releases/<ver>/bin/...`.
///
/// A checkout binary or unsealed path stays `Unknown` so local builds never
/// re-exec onto `/opt/ramshared/current`, while a sealed release started before
/// or mid-transition can refresh seamlessly into the active selector.
pub(crate) fn effective_tui_reexec_origin(
    origin: ProcessInstallOrigin,
    running_path: Option<&Path>,
    product_root: &Path,
) -> ProcessInstallOrigin {
    if origin != ProcessInstallOrigin::Unknown {
        return origin;
    }
    let Some(running) = running_path else {
        return origin;
    };
    let display = running.to_string_lossy();
    let clean = Path::new(display.strip_suffix(" (deleted)").unwrap_or(&display));
    if clean
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return origin;
    }
    let Some(bin_dir) = clean.parent() else {
        return origin;
    };
    let Some(release_root) = bin_dir.parent() else {
        return origin;
    };
    let releases_root = product_root.join("releases");
    if bin_dir.file_name().and_then(|name| name.to_str()) != Some("bin")
        || release_root.parent() != Some(releases_root.as_path())
    {
        return origin;
    }
    if read_versioned_release_identity(clean).is_some() {
        ProcessInstallOrigin::ActiveRelease
    } else {
        origin
    }
}
