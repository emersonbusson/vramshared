// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/installed_image.rs
//! Installed product image identity — one predicate for "is this process the
//! installed binary?".
//!
//! **Why this module exists (DT-2 of release-coherence-and-live-refresh).**
//! `monitor.rs` answered "is the process I am talking to the binary that is
//! installed?" for one process, while nothing answered it for the fleet. A
//! second comparison written for the fleet would be a second definition of one
//! number — the exact failure DT-10 forbids. The predicate lives here so the
//! monitor, the fleet observation and the joint move all ask one question one
//! way.
//!
//! Two layers are deliberately separate:
//! - **what an installed image is** — `InstalledIdentity`, the sealed-release
//!   and direct-install readers, and the manifest seal (`verify_release_manifest`);
//! - **whether a process is running it** — `RunningInstallStatus`,
//!   `ProcessInstallOrigin`, `restart_target`.
//!
//! Rendering of those facts as strings stays beside the code that lays them
//! out; only the identity descriptions that travel with the values live here.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::UNIX_EPOCH;

use crate::release_identity::{SEALED_IDENTITY_FILES, read_release_version};

mod binary_hash;
mod manifest;
mod release_tree;
#[cfg(test)]
use binary_hash::sha256_open_file;
pub(crate) use binary_hash::{sha256_file, sha256_reader, sha256_reader_with_limit};
use manifest::{direct_install_metadata_path, direct_install_timestamp_path};
pub(crate) use manifest::{
    effective_tui_reexec_origin, is_utc_timestamp, observe_installed_input_bundle_digest,
    read_installed_identity_uncached, read_versioned_release_identity, valid_sha256,
    verify_committed_release, verify_release_manifest,
};
#[cfg(test)]
use manifest::{read_direct_install_identity, read_install_timestamp_for_executable};

pub(crate) const BUILD_GIT_SHA: &str = env!("RAMSHARED_BUILD_GIT_SHA");
pub(crate) const BUILD_TREE_STATE: &str = env!("RAMSHARED_BUILD_TREE_STATE");
pub(crate) const INSTALLED_PROVENANCE_V1: &str = "ramshared-installed-release-provenance/v1";
pub(crate) const INSTALLED_PROVENANCE_V2: &str = "ramshared-installed-release-provenance/v2";
#[cfg(test)]
pub(crate) const DIRECT_INSTALL_METADATA_V1: &str = "ramshared-direct-install-metadata/v1";
pub(crate) const DIRECT_INSTALL_METADATA_V2: &str = "ramshared-direct-install-metadata/v2";

/// Where the installer selects the active release root.
pub(crate) const PRODUCT_ROOT: &str = "/opt/ramshared";

/// Where the installer puts the direct entry points.
pub(crate) const DIRECT_INSTALL_BIN_DIR: &str = "/usr/local/bin";

/// The two installed-image entries a product program can match.
///
/// The active-release entry is the program the selected release declares —
/// whatever its file name, binary or script. The direct entry is the
/// conventional `/usr/local/bin` link for a program of that file name. No
/// program name is assumed; the pair is derived, so a new product program
/// needs no new constant.
pub(crate) fn install_entries_for_program(declared_executable: &Path) -> (PathBuf, PathBuf) {
    let file_name = declared_executable
        .file_name()
        .map(PathBuf::from)
        .unwrap_or_default();
    (
        Path::new(DIRECT_INSTALL_BIN_DIR).join(&file_name),
        declared_executable.to_path_buf(),
    )
}

/// The CLI's two installed-image entries. The CLI is the identity carrier for
/// a release; every other product program derives its own pair from
/// [`install_entries_for_program`].
pub(crate) fn cli_install_entries() -> (PathBuf, PathBuf) {
    install_entries_for_program(&Path::new(PRODUCT_ROOT).join("current/bin/ramshared"))
}

/// The executable this process is actually running.
///
/// `/proc/self/exe` is the authority: it names the pinned file, not the
/// selector a later install may have repointed. `current_exe()` follows the
/// selector and would report the *new* release for a process still executing
/// the old one — the exact inversion this campaign exists to close.
pub(crate) fn current_running_executable_path() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        fs::read_link("/proc/self/exe").ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::env::current_exe().ok()
    }
}

/// The sealed release version an executable belongs to.
///
/// A release root is the directory two levels above `bin/<program>`, and its
/// `RELEASE_VERSION` is the bundle identity (the `git describe` value) — never
/// the crate semver, which is the claim about the bytes.
pub(crate) fn release_version_for_executable(executable: &Path) -> Option<String> {
    let release_root = executable.parent()?.parent()?;
    read_release_version(release_root)
}

/// The release version this process is running.
pub(crate) fn current_release_version() -> Option<String> {
    let executable = current_running_executable_path()?;
    release_version_for_executable(&executable)
}

/// The release version the selector currently points at.
pub(crate) fn selected_release_version() -> Option<String> {
    let (_, active_release_executable) = cli_install_entries();
    release_version_for_executable(&active_release_executable)
}

pub(crate) fn format_build_identity(version: &str, commit: &str, tree_state: &str) -> String {
    format!(
        "RamShared CLI {}",
        format_identity(version, commit, tree_state)
    )
}

pub(crate) fn build_info_lines() -> String {
    let commit = full_commit_sha(BUILD_GIT_SHA).unwrap_or_else(|| "unavailable".to_string());
    let tree_state = normalized_tree_state(BUILD_TREE_STATE);
    format!(
        "version={}\nsource_commit={commit}\nsource_tree_state={tree_state}",
        env!("CARGO_PKG_VERSION")
    )
}

pub(crate) fn full_commit_sha(commit: &str) -> Option<String> {
    if commit.len() != 40 || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(commit.to_ascii_lowercase())
}

pub(crate) fn normalized_tree_state(tree_state: &str) -> &'static str {
    match tree_state {
        "clean" => "clean",
        "dirty" => "dirty",
        _ => "unavailable",
    }
}

pub(crate) fn format_identity(version: &str, commit: &str, tree_state: &str) -> String {
    let Some(commit) = short_commit_sha(commit) else {
        return format!("v{version} · source unavailable");
    };
    let state = match normalized_tree_state(tree_state) {
        "clean" => "clean",
        "dirty" => "dirty",
        _ => "state unavailable",
    };
    format!("v{version} · {commit} ({state})")
}

pub(crate) fn short_commit_sha(commit: &str) -> Option<String> {
    if commit.len() == 40 && commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Some(commit[..8].to_ascii_lowercase())
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum InstallKind {
    Direct,
    ActiveRelease,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProcessInstallOrigin {
    Direct,
    ActiveRelease,
    Both,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FileStamp {
    device: Option<u64>,
    inode: Option<u64>,
    length: u64,
    modified_ns: Option<u128>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InstallFingerprint(Vec<Option<FileStamp>>);

#[derive(Clone, Debug)]
pub(crate) struct CachedInstallIdentity {
    fingerprint: InstallFingerprint,
    identity: Option<InstalledIdentity>,
}

#[derive(Default)]
pub(crate) struct InstallationIdentityCache {
    direct: Option<CachedInstallIdentity>,
    active_release: Option<CachedInstallIdentity>,
}

impl InstallationIdentityCache {
    pub(crate) fn read(
        &mut self,
        executable: &Path,
        kind: InstallKind,
    ) -> Option<InstalledIdentity> {
        let fingerprint = install_fingerprint(executable, kind);
        let cached = match kind {
            InstallKind::Direct => &mut self.direct,
            InstallKind::ActiveRelease => &mut self.active_release,
        };
        if let Some(entry) = cached
            .as_ref()
            .filter(|entry| entry.fingerprint == fingerprint)
        {
            return entry.identity.clone();
        }

        let identity = read_installed_identity_uncached(executable, kind);
        *cached = Some(CachedInstallIdentity {
            fingerprint,
            identity: identity.clone(),
        });
        identity
    }
}

pub(crate) fn install_fingerprint(executable: &Path, kind: InstallKind) -> InstallFingerprint {
    let mut paths = vec![executable.to_path_buf()];
    match kind {
        InstallKind::Direct => {
            if let Some(daemon) = executable.parent().map(|parent| parent.join("ramsharedd")) {
                paths.push(daemon);
            }
            paths.extend(direct_install_metadata_path(executable));
            paths.extend(direct_install_timestamp_path(executable));
        }
        InstallKind::ActiveRelease => {
            if let Some(root) = executable.parent().and_then(Path::parent) {
                paths.push(root.join("bin/ramsharedd"));
                for name in SEALED_IDENTITY_FILES {
                    paths.push(root.join(name));
                }
                for name in ["SHA256SUMS", "INSTALLED_MANIFEST_SHA256"] {
                    paths.push(root.join(name));
                }
            }
        }
    }
    InstallFingerprint(paths.iter().map(|path| file_stamp(path)).collect())
}

pub(crate) fn file_stamp(path: &Path) -> Option<FileStamp> {
    let metadata = fs::metadata(path)
        .ok()
        .filter(|metadata| metadata.is_file())?;
    let modified_ns = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos());
    #[cfg(unix)]
    let (device, inode) = {
        use std::os::unix::fs::MetadataExt;
        (Some(metadata.dev()), Some(metadata.ino()))
    };
    #[cfg(not(unix))]
    let (device, inode) = (None, None);
    Some(FileStamp {
        device,
        inode,
        length: metadata.len(),
        modified_ns,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InstalledIdentity {
    pub(crate) executable: PathBuf,
    pub(crate) version: Option<String>,
    pub(crate) source_commit: Option<String>,
    pub(crate) source_tree_state: Option<String>,
    pub(crate) installed_at_utc: Option<String>,
    pub(crate) executable_sha256: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunningInstallStatus {
    InstalledDirect,
    InstalledActiveRelease,
    InstalledBoth,
    UpdatePendingDirect,
    UpdatePendingActiveRelease,
    UpdatePendingBoth,
    NotInstalled,
    Unknown,
}

impl RunningInstallStatus {
    /// The running file **is** one of the installed images.
    pub(crate) fn is_installed(self) -> bool {
        matches!(
            self,
            Self::InstalledDirect | Self::InstalledActiveRelease | Self::InstalledBoth
        )
    }

    /// The running file is on an install path of a **different** generation.
    /// This is what a repointed selector produces.
    pub(crate) fn is_update_pending(self) -> bool {
        matches!(
            self,
            Self::UpdatePendingDirect | Self::UpdatePendingActiveRelease | Self::UpdatePendingBoth
        )
    }

    /// An install exists and the running file is **not** any of them.
    ///
    /// This is the signature of a process that kept running after `current`
    /// moved: `/proc/self/exe` names a release directory that is no longer
    /// selected, so its path matches neither install and its file matches
    /// neither either. It is not "unknown" — it is a named mismatch, and
    /// reporting it as unclassifiable is how the three-generation defect
    /// stayed invisible.
    pub(crate) fn is_not_installed(self) -> bool {
        matches!(self, Self::NotInstalled)
    }

    /// No fact could be established (no running file, or no install to compare).
    pub(crate) fn is_unknown(self) -> bool {
        matches!(self, Self::Unknown)
    }

    /// The process is running something other than the installed image, and
    /// that is known. The fleet calls this `Stale`.
    pub(crate) fn is_known_stale(self) -> bool {
        self.is_update_pending() || self.is_not_installed()
    }
}

pub(crate) fn format_running_identity_line(
    version: &str,
    commit: &str,
    tree_state: &str,
    status: RunningInstallStatus,
) -> String {
    // Operator question: is the process I am talking to the binary that is
    // installed? Which named path it matches is the install block's job —
    // naming the path here only matters when the two differ, and that case is
    // already "restart pending".
    let value = match status {
        RunningInstallStatus::InstalledDirect
        | RunningInstallStatus::InstalledActiveRelease
        | RunningInstallStatus::InstalledBoth => "matches the install",
        RunningInstallStatus::UpdatePendingDirect
        | RunningInstallStatus::UpdatePendingActiveRelease
        | RunningInstallStatus::UpdatePendingBoth => "restart pending",
        RunningInstallStatus::NotInstalled => "not installed",
        RunningInstallStatus::Unknown => "unknown",
    };
    format!(
        "Running: {} · {value}",
        format_identity(version, commit, tree_state)
    )
}

#[cfg(test)]
pub(crate) fn running_install_status(
    executable: Option<&Path>,
    direct_install_executable: &Path,
    active_release_executable: &Path,
) -> RunningInstallStatus {
    let running_metadata = executable
        .and_then(|path| fs::metadata(path).ok())
        .filter(|metadata| metadata.is_file());
    let origin = executable.map_or(ProcessInstallOrigin::Unknown, |path| {
        process_install_origin_for_path(path, direct_install_executable, active_release_executable)
    });
    running_install_status_from_metadata(
        running_metadata.as_ref(),
        origin,
        direct_install_executable,
        active_release_executable,
    )
}

pub(crate) fn running_install_status_from_metadata(
    running_metadata: Option<&fs::Metadata>,
    origin: ProcessInstallOrigin,
    direct_install_executable: &Path,
    active_release_executable: &Path,
) -> RunningInstallStatus {
    let Some(running_metadata) = running_metadata else {
        return RunningInstallStatus::Unknown;
    };
    let direct_metadata = fs::metadata(direct_install_executable)
        .ok()
        .filter(|metadata| metadata.is_file());
    let active_metadata = fs::metadata(active_release_executable)
        .ok()
        .filter(|metadata| metadata.is_file());
    let direct_match = direct_metadata
        .as_ref()
        .is_some_and(|candidate| same_file_identity(running_metadata, candidate));
    let active_match = active_metadata
        .as_ref()
        .is_some_and(|candidate| same_file_identity(running_metadata, candidate));
    match (direct_match, active_match) {
        (true, true) => RunningInstallStatus::InstalledBoth,
        (true, false) => RunningInstallStatus::InstalledDirect,
        (false, true) => RunningInstallStatus::InstalledActiveRelease,
        (false, false) if origin == ProcessInstallOrigin::Direct => {
            RunningInstallStatus::UpdatePendingDirect
        }
        (false, false) if origin == ProcessInstallOrigin::ActiveRelease => {
            RunningInstallStatus::UpdatePendingActiveRelease
        }
        (false, false) if origin == ProcessInstallOrigin::Both => {
            RunningInstallStatus::UpdatePendingBoth
        }
        (false, false) if direct_metadata.is_some() || active_metadata.is_some() => {
            RunningInstallStatus::NotInstalled
        }
        (false, false) => RunningInstallStatus::Unknown,
    }
}

pub(crate) fn format_installed_identity_line(
    label: &str,
    installed: Option<&InstalledIdentity>,
) -> String {
    let Some(installed) = installed else {
        return format!("Installed {label}: not found");
    };
    format!(
        "Installed {label}: {}",
        format_installed_identity_body(installed)
    )
}

/// Whether two named installs are the same product image.
///
/// Compares the recorded source identity and the bound executable digest, not
/// the paths: a direct entry that links into a sealed release is that release's
/// binary, and the header should not present it as a second install.
pub(crate) fn installed_images_agree(left: &InstalledIdentity, right: &InstalledIdentity) -> bool {
    left.version == right.version
        && left.source_commit == right.source_commit
        && left.source_tree_state == right.source_tree_state
        && left.installed_at_utc == right.installed_at_utc
        && left.executable_sha256 == right.executable_sha256
        && left.executable_sha256.is_some()
}

/// Render the named installs of one product for the dashboard header.
///
/// When every identified install is the same image the header says so once —
/// one product, one image, the paths are how it is reached. When they differ,
/// the difference is the news, so each keeps its own line. Naming a path per
/// line is for the divergence case; on a healthy install it reads as two
/// broken half-installs beside each other.
pub(crate) fn format_installed_identity_lines(
    installs: &[(&str, Option<&InstalledIdentity>)],
) -> Vec<String> {
    let identified: Vec<(&str, &InstalledIdentity)> = installs
        .iter()
        .filter_map(|(label, installed)| installed.map(|identity| (*label, identity)))
        .collect();
    let missing: Vec<&str> = installs
        .iter()
        .filter_map(|(label, installed)| installed.is_none().then_some(*label))
        .collect();

    if identified.is_empty() {
        return vec![format!(
            "Installed: not found ({})",
            installs
                .iter()
                .map(|(label, _)| *label)
                .collect::<Vec<_>>()
                .join(", ")
        )];
    }

    let all_agree = identified
        .windows(2)
        .all(|pair| installed_images_agree(pair[0].1, pair[1].1));

    let mut lines: Vec<String> = Vec::new();
    if all_agree {
        // One image, one line. The paths are how the same binary is reached;
        // naming them here is what made a healthy install read as two
        // half-installs. A path that is absent is the news, so it is named.
        lines.push(format!(
            "Installed: {}",
            format_installed_identity_body(identified[0].1)
        ));
        if !missing.is_empty() {
            lines.push(format!("Missing: {}", missing.join(", ")));
        }
        return lines;
    }

    for (label, identity) in &identified {
        lines.push(format_installed_identity_line(label, Some(identity)));
    }
    for label in &missing {
        lines.push(format_installed_identity_line(label, None));
    }
    lines
}

/// The `version · commit (tree) · timestamp` body shared by every install line.
pub(crate) fn format_installed_identity_body(installed: &InstalledIdentity) -> String {
    let identity = match (
        installed.version.as_deref(),
        installed.source_commit.as_deref(),
        installed.source_tree_state.as_deref(),
    ) {
        (Some(version), Some(commit), Some(tree_state))
            if full_commit_sha(commit).is_some() && valid_version_identity(version) =>
        {
            format_identity(version, commit, tree_state)
        }
        _ => "identity unknown".to_string(),
    };
    let timestamp = installed
        .installed_at_utc
        .as_deref()
        .filter(|value| is_utc_timestamp(value))
        .map(|value| format!("{} {} UTC", &value[..10], &value[11..19]))
        .unwrap_or_else(|| "install time unknown".to_string());
    format!("{identity} · {timestamp}")
}

#[cfg(test)]
pub(crate) fn read_installed_identity(
    executable: &Path,
    kind: InstallKind,
) -> Option<InstalledIdentity> {
    read_installed_identity_uncached(executable, kind)
}

pub(crate) fn valid_tree_state(value: &str) -> bool {
    matches!(value, "clean" | "dirty" | "unavailable")
}

pub(crate) fn valid_version_identity(value: &str) -> bool {
    normalize_version_identity(value).is_some()
}

pub(crate) fn normalize_version_identity(value: &str) -> Option<String> {
    let version = value.strip_prefix('v').unwrap_or(value);
    if !version.is_empty()
        && version.len() <= 128
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'+' | b'-'))
        && version.as_bytes()[0].is_ascii_alphanumeric()
    {
        Some(version.to_string())
    } else {
        None
    }
}

pub(crate) fn restart_target(
    origin: ProcessInstallOrigin,
    running_metadata: Option<&fs::Metadata>,
    running_sha256: Option<&str>,
    allow_reexec: bool,
    direct_install: Option<&InstalledIdentity>,
    active_install: Option<&InstalledIdentity>,
) -> Option<PathBuf> {
    if !allow_reexec || running_metadata.is_none() {
        return None;
    }
    let direct = direct_install
        .filter(|installed| restart_required(installed, running_metadata, running_sha256));
    let active = active_install
        .filter(|installed| restart_required(installed, running_metadata, running_sha256));
    match origin {
        ProcessInstallOrigin::Direct => direct.map(|installed| installed.executable.clone()),
        ProcessInstallOrigin::ActiveRelease => active.map(|installed| installed.executable.clone()),
        ProcessInstallOrigin::Both => match (direct, active) {
            (Some(direct), None) => Some(direct.executable.clone()),
            (None, Some(active)) => Some(active.executable.clone()),
            (Some(direct), Some(active))
                if direct.executable_sha256 == active.executable_sha256
                    && installed_source_identity(direct) == installed_source_identity(active) =>
            {
                Some(active.executable.clone())
            }
            _ => None,
        },
        ProcessInstallOrigin::Unknown => None,
    }
}

pub(crate) fn restart_required(
    installed: &InstalledIdentity,
    running_metadata: Option<&fs::Metadata>,
    running_sha256: Option<&str>,
) -> bool {
    let (Some(version), Some(commit), Some(tree_state), Some(installed_sha256)) = (
        installed.version.as_deref(),
        installed.source_commit.as_deref(),
        installed.source_tree_state.as_deref(),
        installed.executable_sha256.as_deref(),
    ) else {
        return false;
    };
    if !valid_version_identity(version)
        || full_commit_sha(commit).is_none()
        || !valid_tree_state(tree_state)
        || !valid_sha256(installed_sha256)
    {
        return false;
    }
    if running_metadata.is_some_and(|running| {
        fs::metadata(&installed.executable)
            .ok()
            .is_some_and(|candidate| same_file_identity(running, &candidate))
    }) {
        return false;
    }
    if let Some(running_sha256) = running_sha256 {
        return installed_sha256 != running_sha256;
    }
    let current_sha = full_commit_sha(BUILD_GIT_SHA);
    let installed_sha = full_commit_sha(commit);
    !(version == env!("CARGO_PKG_VERSION")
        && current_sha == installed_sha
        && normalized_tree_state(tree_state) == normalized_tree_state(BUILD_TREE_STATE))
}

pub(crate) fn installed_source_identity(
    identity: &InstalledIdentity,
) -> (Option<&str>, Option<&str>, Option<&str>) {
    (
        identity.version.as_deref(),
        identity.source_commit.as_deref(),
        identity.source_tree_state.as_deref(),
    )
}

pub(crate) fn current_running_executable_metadata() -> Option<fs::Metadata> {
    #[cfg(target_os = "linux")]
    {
        fs::metadata("/proc/self/exe").ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::env::current_exe()
            .ok()
            .and_then(|path| fs::metadata(path).ok())
    }
}

pub(crate) fn current_running_executable_sha256() -> Option<String> {
    static RUNNING_EXECUTABLE_SHA256: OnceLock<Option<String>> = OnceLock::new();
    RUNNING_EXECUTABLE_SHA256
        .get_or_init(|| {
            #[cfg(target_os = "linux")]
            if let Ok(mut executable) = File::open("/proc/self/exe") {
                return sha256_reader(&mut executable);
            }
            let path = std::env::current_exe().ok()?;
            sha256_file(&path)
        })
        .clone()
}

pub(crate) fn current_process_install_origin() -> ProcessInstallOrigin {
    let (direct_executable, active_release_executable) = cli_install_entries();
    let (direct_executable, active_release_executable) = (
        direct_executable.as_path(),
        active_release_executable.as_path(),
    );
    let running_path = {
        #[cfg(target_os = "linux")]
        {
            fs::read_link("/proc/self/exe").ok()
        }
        #[cfg(not(target_os = "linux"))]
        {
            std::env::current_exe().ok()
        }
    };
    running_path.map_or(ProcessInstallOrigin::Unknown, |path| {
        process_install_origin_for_path(&path, direct_executable, active_release_executable)
    })
}

pub(crate) fn process_install_origin_for_path(
    running_path: &Path,
    direct_executable: &Path,
    active_release_executable: &Path,
) -> ProcessInstallOrigin {
    let display = running_path.to_string_lossy();
    let clean_path = Path::new(display.strip_suffix(" (deleted)").unwrap_or(&display));
    let direct_match = path_matches(clean_path, direct_executable);
    let active_match = path_matches(clean_path, active_release_executable);
    match (direct_match, active_match) {
        (true, true) => ProcessInstallOrigin::Both,
        (true, false) => ProcessInstallOrigin::Direct,
        (false, true) => ProcessInstallOrigin::ActiveRelease,
        (false, false) => ProcessInstallOrigin::Unknown,
    }
}

pub(crate) fn path_matches(left: &Path, right: &Path) -> bool {
    left == right
        || left
            .canonicalize()
            .ok()
            .zip(right.canonicalize().ok())
            .is_some_and(|(left, right)| left == right)
}

#[cfg(unix)]
pub(crate) fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
pub(crate) fn same_file_identity(_left: &fs::Metadata, _right: &fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests;
