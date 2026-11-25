// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/installation/driver_store.rs
//! Driver-store publication and its exact receipt (DT-14/15).
//!
//! The lab published the signed INF with `pnputil /add-driver` and parsed
//! console text to tell success from failure. This module publishes through
//! `DiInstallDriverW` and records *which* OEM INF the driver store created, so
//! uninstall can remove that exact package and nothing else.
//!
//! The receipt is not cosmetic. `DiInstallDriverW` returns no package name,
//! and `SetupUninstallOEMInfW` deletes by name. Before this module the product
//! could publish an INF and then had no supported way to name it again: a
//! rollback left the OEM INF in the store forever. The store copies the signed
//! INF verbatim (its catalog covers the INF hash), so the published package is
//! found by the SHA-256 of the INF this product shipped.
//!
//! Two or more matching packages are refused rather than guessed: removing the
//! wrong `oemNN.inf` is exactly the wildcard behaviour this product forbids.
//! The lookup is read-only and runs without elevation; publication and removal
//! need it.

use crate::package::{ArtifactRole, ProductManifestV1};
use crate::package_builder::inf_references;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::path::{Path, PathBuf};

/// Schema of the receipt file that survives between install and uninstall.
pub const RECEIPT_SCHEMA: u32 = 1;

/// What a package asks the driver store to publish.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriverStorePlan {
    /// The staged INF, already hash-verified by the install transaction.
    pub inf_path: PathBuf,
    /// The driver binary the INF must reference.
    pub sys_path: PathBuf,
    /// Uppercase SHA-256 of the INF. The driver store copies the signed INF
    /// verbatim, so this is how the published `oemNN.inf` is named again.
    pub inf_sha256: String,
    /// Uppercase SHA-256 of the driver binary. The store keeps the payload
    /// beside the published INF, so this is how a package that the host calls
    /// "already imported" is proved to hold *these* bytes and not an older
    /// build that reused the same INF version.
    pub sys_sha256: String,
}

/// One driver package this product published, named exactly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishedDriver {
    /// The OEM INF file name the store created, for example `oem38.inf`.
    pub oem_inf: String,
    /// Uppercase SHA-256 of the INF that produced it.
    pub inf_sha256: String,
    /// `DiInstallDriverW` asked for a restart before the driver is active.
    pub reboot_required: bool,
}

/// The durable receipt: every package this product published and has not yet
/// removed. A list, not one entry, because an upgrade publishes a new INF
/// before the old one is retired.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriverReceipts {
    pub schema: u32,
    pub packages: Vec<PublishedDriver>,
}

impl DriverReceipts {
    pub fn empty() -> Self {
        Self {
            schema: RECEIPT_SCHEMA,
            packages: Vec::new(),
        }
    }

    /// Add a package once, keyed by the INF hash. A repeated install of the
    /// same bytes reuses the same OEM INF, so it must not grow the receipt.
    pub fn record(&mut self, driver: &PublishedDriver) {
        if self
            .packages
            .iter()
            .any(|known| known.inf_sha256.eq_ignore_ascii_case(&driver.inf_sha256))
        {
            return;
        }
        self.packages.push(driver.clone());
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DriverStoreError {
    /// The package cannot name a publishable INF.
    Plan(String),
    /// The host refused the call because the process is not elevated.
    ElevationRequired { code: u32 },
    /// Any other Win32 failure, kept as the raw code.
    Os { code: u32 },
    /// The store accepted the INF but the receipt cannot name the package.
    Unnamed(String),
    /// More than one published INF matches this product's bytes.
    Ambiguous(Vec<String>),
    /// The store already holds this exact INF but its payload is not the
    /// binary this package shipped. The host will never update the device to
    /// bytes it believes are already present, so the package must carry a
    /// newer `DriverVer` instead of silently keeping the older binary.
    PayloadDrift {
        oem_inf: String,
        expected: String,
        found: String,
    },
    /// The store's payload could not be located or read.
    StorePayload(String),
    /// A receipt file is unreadable or does not match its schema.
    Receipt(String),
    /// This host has no driver store.
    Unsupported,
}

impl fmt::Display for DriverStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plan(reason) => write!(f, "driver store plan: {reason}"),
            Self::ElevationRequired { code } => write!(
                f,
                "driver store publication requires an elevated process (Win32 error {code})"
            ),
            Self::Os { code } => write!(f, "driver store call failed (Win32 error {code})"),
            Self::Unnamed(reason) => write!(f, "driver store receipt: {reason}"),
            Self::Ambiguous(names) => write!(
                f,
                "driver store receipt: {} published INFs match this product: {}",
                names.len(),
                names.join(", ")
            ),
            Self::PayloadDrift {
                oem_inf,
                expected,
                found,
            } => write!(
                f,
                "driver store already holds {oem_inf} for this INF with a different driver binary (package {expected}, store {found}); the package must bump DriverVer to supersede it"
            ),
            Self::StorePayload(reason) => write!(f, "driver store payload: {reason}"),
            Self::Receipt(reason) => write!(f, "driver store receipt: {reason}"),
            Self::Unsupported => write!(f, "driver store is not supported on this host"),
        }
    }
}

impl std::error::Error for DriverStoreError {}

impl DriverStoreError {
    /// Map a Win32 error from the driver-store calls to a refusal that says
    /// what the operator can do about it.
    pub fn from_win32(code: u32) -> Self {
        match code {
            // ERROR_ACCESS_DENIED / ERROR_PRIVILEGE_NOT_HELD.
            5 | 1314 => Self::ElevationRequired { code },
            other => Self::Os { code: other },
        }
    }
}

fn sha256_upper(bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(bytes);
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect()
}

/// The payload of a receipt error without its repeated Display prefix.
#[cfg(windows)]
fn receipt_reason(error: DriverStoreError) -> String {
    match error {
        DriverStoreError::Receipt(reason) => reason,
        other => other.to_string(),
    }
}

/// Resolve the INF and the driver binary the install transaction should
/// publish, and prove the INF actually names that binary.
pub fn plan_publication(
    root: &Path,
    manifest: &ProductManifestV1,
) -> Result<DriverStorePlan, DriverStoreError> {
    let artifact = |role: ArtifactRole| -> Result<PathBuf, DriverStoreError> {
        let relative = &manifest
            .artifact(role)
            .map_err(|error| DriverStoreError::Plan(error.to_string()))?
            .relative_path;
        let path = root.join(relative);
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| DriverStoreError::Plan(format!("{}: {error}", path.display())))?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            return Err(DriverStoreError::Plan(format!(
                "{} is not a regular file",
                path.display()
            )));
        }
        Ok(path)
    };
    let inf_path = artifact(ArtifactRole::DriverInf)?;
    let sys_path = artifact(ArtifactRole::DriverSys)?;
    let inf = std::fs::read(&inf_path)
        .map_err(|error| DriverStoreError::Plan(format!("{}: {error}", inf_path.display())))?;
    let inf_sha256 = sha256_upper(&inf);
    let inf = String::from_utf8(inf)
        .map_err(|_| DriverStoreError::Plan(format!("{} is not UTF-8 text", inf_path.display())))?;
    let sys_name = sys_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| DriverStoreError::Plan("driver binary has no usable name".to_string()))?;
    if !inf_references(&inf, sys_name) {
        return Err(DriverStoreError::Plan(format!(
            "{} does not list {sys_name} under [SourceDisksFiles]",
            inf_path.display()
        )));
    }
    let sys = std::fs::read(&sys_path)
        .map_err(|error| DriverStoreError::Plan(format!("{}: {error}", sys_path.display())))?;
    let sys_sha256 = sha256_upper(&sys);
    Ok(DriverStorePlan {
        inf_path,
        sys_path,
        inf_sha256,
        sys_sha256,
    })
}

/// An INF file the store published, with the hash of its bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredInf {
    pub name: String,
    pub sha256: String,
}

/// The exact OEM INF whose bytes are this product's INF, if the store holds
/// it.
///
/// More than one match means the store holds two packages with identical
/// bytes; removal cannot pick one, so that is a refusal, never a wildcard.
/// Zero matches is `None`: either publication has not happened or every
/// package is already gone, and both are legitimate for a repeat uninstall.
pub fn reconcile(
    stored: &[StoredInf],
    inf_sha256: &str,
) -> Result<Option<String>, DriverStoreError> {
    let mut matches: Vec<&StoredInf> = stored
        .iter()
        .filter(|candidate| candidate.sha256.eq_ignore_ascii_case(inf_sha256))
        .collect();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(matches.remove(0).name.clone())),
        _ => Err(DriverStoreError::Ambiguous(
            matches
                .iter()
                .map(|candidate| candidate.name.clone())
                .collect(),
        )),
    }
}

/// Name the package a publication just created, using the store snapshot taken
/// before its effect.
///
/// A store that gained exactly one `oem*.inf` whose bytes match the shipped
/// INF has named this call even when an older package holds identical bytes;
/// that is precisely the case the hash lookup alone must refuse. Every other
/// shape falls back to the hash lookup, which returns `None` for zero matches
/// and refuses several. A new INF with foreign bytes is never attributed.
pub fn name_published(
    before: &[String],
    after: &[StoredInf],
    inf_sha256: &str,
) -> Result<String, DriverStoreError> {
    let added: Vec<&StoredInf> = after
        .iter()
        .filter(|candidate| {
            !before
                .iter()
                .any(|name| name.eq_ignore_ascii_case(&candidate.name))
        })
        .collect();
    let mut added_matches = added
        .iter()
        .copied()
        .filter(|candidate| candidate.sha256.eq_ignore_ascii_case(inf_sha256));
    match (added_matches.next(), added_matches.next()) {
        (Some(created), None) => Ok(created.name.clone()),
        (Some(first), Some(second)) => {
            let mut names = vec![first.name.clone(), second.name.clone()];
            names.extend(added_matches.map(|candidate| candidate.name.clone()));
            Err(DriverStoreError::Ambiguous(names))
        }
        (None, _) => reconcile(after, inf_sha256)?.ok_or_else(|| {
            DriverStoreError::Unnamed(if added.is_empty() {
                "no published INF matches the shipped INF hash".to_string()
            } else {
                format!(
                    "the store gained {} new INF(s) but none match the shipped INF hash",
                    added.len()
                )
            })
        }),
    }
}

/// The store's `FileRepository` package directories for one INF file name.
///
/// Imported packages keep their payload beside the INF under
/// `%SystemRoot%\System32\DriverStore\FileRepository\<inf-name>_<arch>_<hash>\`.
/// The directory suffix is not stable across reimports, so packages are
/// selected by the exact bytes of their INF, not by their directory name.
pub fn store_packages(repository: &Path, inf_name: &str) -> Result<Vec<PathBuf>, DriverStoreError> {
    let prefix = format!("{inf_name}_");
    let entries = std::fs::read_dir(repository).map_err(|error| {
        DriverStoreError::StorePayload(format!("{}: {error}", repository.display()))
    })?;
    let mut packages = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            DriverStoreError::StorePayload(format!("{}: {error}", repository.display()))
        })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(&prefix) {
            continue;
        }
        let file_type = entry.file_type().map_err(|error| {
            DriverStoreError::StorePayload(format!("{}: {error}", entry.path().display()))
        })?;
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        packages.push(entry.path());
    }
    packages.sort();
    Ok(packages)
}

/// The one store package whose INF bytes are exactly this package's INF.
///
/// Zero is a refusal: a host that answered "already imported" must still be
/// able to name the package that holds these bytes. More than one refuses for
/// the same reason removal refuses an ambiguous store: nothing here guesses.
pub fn locate_store_payload(
    packages: &[PathBuf],
    inf_name: &str,
    inf_sha256: &str,
) -> Result<PathBuf, DriverStoreError> {
    let mut matches = Vec::new();
    for package in packages {
        let inf = package.join(inf_name);
        match std::fs::read(&inf) {
            Ok(bytes) if sha256_upper(&bytes).eq_ignore_ascii_case(inf_sha256) => {
                matches.push(package.clone());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(DriverStoreError::StorePayload(format!(
                    "{}: {error}",
                    inf.display()
                )));
            }
        }
    }
    match matches.len() {
        0 => Err(DriverStoreError::StorePayload(format!(
            "no store package holds the shipped INF bytes ({inf_sha256})"
        ))),
        1 => Ok(matches.remove(0)),
        _ => Err(DriverStoreError::Ambiguous(
            matches
                .iter()
                .filter_map(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .collect(),
        )),
    }
}

/// Decide a publication the host answered with `ERROR_NO_MORE_ITEMS`.
///
/// The caller has already found the one published INF whose bytes are this
/// package's INF and read the payload that sits beside it in the store.
/// Matching bytes mean the package is published and no device needed an
/// update, so the install is complete without a reboot. Different bytes are
/// a refusal: the device is using an older binary under the same `DriverVer`,
/// and recording this package would attribute bytes the store does not hold.
pub fn confirm_published(
    plan: &DriverStorePlan,
    oem_inf: &str,
    store_sys_sha256: &str,
) -> Result<PublishedDriver, DriverStoreError> {
    if !plan.sys_sha256.eq_ignore_ascii_case(store_sys_sha256) {
        return Err(DriverStoreError::PayloadDrift {
            oem_inf: oem_inf.to_string(),
            expected: plan.sys_sha256.clone(),
            found: store_sys_sha256.to_ascii_uppercase(),
        });
    }
    Ok(PublishedDriver {
        oem_inf: oem_inf.to_string(),
        inf_sha256: plan.inf_sha256.clone(),
        reboot_required: false,
    })
}

/// The `oemNN.inf` names the store holds, without hashing their bytes.
///
/// Publication snapshots this before its effect so a new package can still be
/// named when the post-install hash lookup is ambiguous. It is also the last
/// refusal point that costs nothing: if the directory cannot be listed now,
/// a package created afterwards could never be named again.
pub fn oem_inf_names(inf_dir: &Path) -> Result<Vec<String>, DriverStoreError> {
    let entries = std::fs::read_dir(inf_dir)
        .map_err(|error| DriverStoreError::Receipt(format!("{}: {error}", inf_dir.display())))?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            DriverStoreError::Receipt(format!("{}: {error}", inf_dir.display()))
        })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !is_oem_inf_name(name) {
            continue;
        }
        let file_type = entry.file_type().map_err(|error| {
            DriverStoreError::Receipt(format!("{}: {error}", entry.path().display()))
        })?;
        if file_type.is_file() && !file_type.is_symlink() {
            names.push(name.to_string());
        }
    }
    Ok(names)
}

/// Read `oem*.inf` under `inf_dir` and hash each regular file. Symlinks and
/// directories are skipped: the receipt must name a real store entry.
pub fn stored_infs(inf_dir: &Path) -> Result<Vec<StoredInf>, DriverStoreError> {
    let entries = std::fs::read_dir(inf_dir)
        .map_err(|error| DriverStoreError::Receipt(format!("{}: {error}", inf_dir.display())))?;
    let mut stored = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            DriverStoreError::Receipt(format!("{}: {error}", inf_dir.display()))
        })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !is_oem_inf_name(name) {
            continue;
        }
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| DriverStoreError::Receipt(format!("{}: {error}", path.display())))?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            continue;
        }
        let bytes = std::fs::read(&path)
            .map_err(|error| DriverStoreError::Receipt(format!("{}: {error}", path.display())))?;
        stored.push(StoredInf {
            name: name.to_string(),
            sha256: sha256_upper(&bytes),
        });
    }
    Ok(stored)
}

/// `oem<digits>.inf`, the only shape the driver store creates. Anything else
/// (a path, a wildcard, a catalog) is refused before it reaches the API.
pub fn is_oem_inf_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let Some(body) = lower.strip_prefix("oem") else {
        return false;
    };
    let Some(digits) = body.strip_suffix(".inf") else {
        return false;
    };
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

/// Publish the planned INF through the Windows driver store and name the
/// package it created.
#[cfg(windows)]
pub fn publish(plan: &DriverStorePlan) -> Result<PublishedDriver, DriverStoreError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::DiInstallDriverW;
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::core::BOOL;

    // Snapshot before the effect. If the store cannot be inventoried now, the
    // package this call would create could never be named afterwards, so the
    // refusal costs nothing. The snapshot also names a new package when an
    // older copy with identical bytes makes the hash lookup ambiguous.
    let inf_dir = windows_inf_dir();
    let before = oem_inf_names(&inf_dir).map_err(|error| {
        DriverStoreError::Receipt(format!(
            "refuse publication before any effect: {}",
            receipt_reason(error)
        ))
    })?;
    let inf: Vec<u16> = plan
        .inf_path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut reboot: BOOL = 0;
    let installed = unsafe { DiInstallDriverW(std::ptr::null_mut(), inf.as_ptr(), 0, &mut reboot) };
    if installed == 0 {
        let code = unsafe { GetLastError() };
        // ERROR_NO_MORE_ITEMS: the host already imported this INF and no
        // device needed an update. That is a completed publication when the
        // store holds this package's exact payload, and a refusal when it
        // holds an older binary under the same DriverVer. A repeat install of
        // the same package must not fail here.
        if code == ERROR_NO_MORE_ITEMS {
            return confirm_store_payload(plan, &inf_dir);
        }
        return Err(DriverStoreError::from_win32(code));
    }
    // From here the package exists. A failed re-read must say so instead of
    // surfacing a bare directory error that reads as if nothing happened.
    let stored = stored_infs(&inf_dir).map_err(|error| {
        DriverStoreError::Unnamed(format!(
            "the INF was published but the store could not be re-read to name it: {}",
            receipt_reason(error)
        ))
    })?;
    let oem_inf = name_published(&before, &stored, &plan.inf_sha256)?;
    Ok(PublishedDriver {
        oem_inf,
        inf_sha256: plan.inf_sha256.clone(),
        reboot_required: reboot != 0,
    })
}

/// ERROR_NO_MORE_ITEMS: the host reported that the package is already
/// imported and that no device needed an update.
#[cfg(windows)]
const ERROR_NO_MORE_ITEMS: u32 = 259;

#[cfg(windows)]
fn confirm_store_payload(
    plan: &DriverStorePlan,
    inf_dir: &Path,
) -> Result<PublishedDriver, DriverStoreError> {
    let oem_inf = reconcile(&stored_infs(inf_dir)?, &plan.inf_sha256)?.ok_or_else(|| {
        DriverStoreError::Unnamed(
            "the host reported the package as already imported but no published INF matches the shipped bytes"
                .to_string(),
        )
    })?;
    let inf_name = plan
        .inf_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| DriverStoreError::StorePayload("INF has no usable name".into()))?;
    let sys_name = plan
        .sys_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| DriverStoreError::StorePayload("driver binary has no usable name".into()))?;
    let repository = windows_driver_repository();
    let packages = store_packages(&repository, inf_name)?;
    let package = locate_store_payload(&packages, inf_name, &plan.inf_sha256)?;
    let store_sys = package.join(sys_name);
    let bytes = std::fs::read(&store_sys).map_err(|error| {
        DriverStoreError::StorePayload(format!("{}: {error}", store_sys.display()))
    })?;
    confirm_published(plan, &oem_inf, &sha256_upper(&bytes))
}

#[cfg(not(windows))]
pub fn publish(_plan: &DriverStorePlan) -> Result<PublishedDriver, DriverStoreError> {
    Err(DriverStoreError::Unsupported)
}

/// Remove the package this product shipped for a manifest that predates
/// receipts. A clean store is `None`; an ambiguous store is a refusal.
#[cfg(windows)]
pub fn remove_unrecorded(
    inf_dir: &Path,
    inf_sha256: &str,
) -> Result<Option<String>, DriverStoreError> {
    let Some(oem_inf) = reconcile(&stored_infs(inf_dir)?, inf_sha256)? else {
        return Ok(None);
    };
    remove(&oem_inf)?;
    Ok(Some(oem_inf))
}

#[cfg(not(windows))]
pub fn remove_unrecorded(
    _inf_dir: &Path,
    _inf_sha256: &str,
) -> Result<Option<String>, DriverStoreError> {
    Err(DriverStoreError::Unsupported)
}

/// Remove exactly the OEM INF named in the receipt. Flags are zero: a package
/// still in use makes the call fail, and that failure must survive as a
/// receipt rather than be forced away.
#[cfg(windows)]
pub fn remove(oem_inf: &str) -> Result<(), DriverStoreError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::SetupUninstallOEMInfW;
    use windows_sys::Win32::Foundation::GetLastError;

    if !is_oem_inf_name(oem_inf) {
        return Err(DriverStoreError::Plan(format!(
            "refuse to remove {oem_inf}: not an exact oemNN.inf name"
        )));
    }
    let name: Vec<u16> = std::ffi::OsStr::new(oem_inf)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let removed = unsafe { SetupUninstallOEMInfW(name.as_ptr(), 0, std::ptr::null_mut()) };
    let code = unsafe { GetLastError() };
    uninstall_result(removed, code)
}

/// The postcondition `remove` promises is "this name is not in the store".
/// A package that is already absent satisfies it, so a retry after a partial
/// uninstall (or a crash between removal and the receipt rewrite) converges
/// instead of failing forever on an idempotent verb.
#[cfg(windows)]
fn uninstall_result(removed: i32, code: u32) -> Result<(), DriverStoreError> {
    // ERROR_FILE_NOT_FOUND / ERROR_NOT_FOUND.
    const ERROR_FILE_NOT_FOUND: u32 = 2;
    const ERROR_NOT_FOUND: u32 = 1168;
    if removed != 0 || code == ERROR_FILE_NOT_FOUND || code == ERROR_NOT_FOUND {
        return Ok(());
    }
    Err(DriverStoreError::from_win32(code))
}

#[cfg(not(windows))]
pub fn remove(_oem_inf: &str) -> Result<(), DriverStoreError> {
    Err(DriverStoreError::Unsupported)
}

mod receipts;
pub use receipts::*;

#[cfg(test)]
mod tests;
