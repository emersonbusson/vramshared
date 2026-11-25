// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/installation/driver_store/receipts.rs
use super::{DriverReceipts, DriverStoreError, PublishedDriver, RECEIPT_SCHEMA, is_oem_inf_name};
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;

/// `%SystemRoot%\INF`, where the driver store keeps the published OEM INFs.
#[cfg(windows)]
pub fn windows_inf_dir() -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    PathBuf::from(root).join("INF")
}

/// `%SystemRoot%\System32\DriverStore\FileRepository`, where imported
/// packages keep their INF, catalog, and payload together.
#[cfg(windows)]
pub fn windows_driver_repository() -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    PathBuf::from(root)
        .join("System32")
        .join("DriverStore")
        .join("FileRepository")
}

/// Parse a receipt file. Absent is `None`; every other failure is a refusal,
/// because an unreadable receipt would let uninstall forget a package.
pub fn read_receipts(path: &Path) -> Result<Option<DriverReceipts>, DriverStoreError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(DriverStoreError::Receipt(format!(
                "{}: {error}",
                path.display()
            )));
        }
    };
    let receipts: DriverReceipts = serde_json::from_slice(&bytes)
        .map_err(|error| DriverStoreError::Receipt(format!("{}: {error}", path.display())))?;
    if receipts.schema != RECEIPT_SCHEMA {
        return Err(DriverStoreError::Receipt(format!(
            "{}: schema {} is not {RECEIPT_SCHEMA}",
            path.display(),
            receipts.schema
        )));
    }
    for package in &receipts.packages {
        if !is_oem_inf_name(&package.oem_inf) {
            return Err(DriverStoreError::Receipt(format!(
                "{}: {} is not an exact oemNN.inf name",
                path.display(),
                package.oem_inf
            )));
        }
        if package.inf_sha256.len() != 64
            || !package
                .inf_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(DriverStoreError::Receipt(format!(
                "{}: {} has no SHA-256",
                path.display(),
                package.oem_inf
            )));
        }
    }
    Ok(Some(receipts))
}

/// Write the receipt so a crash cannot leave a half-written file: write a
/// sibling, then rename over the target.
pub fn write_receipts(path: &Path, receipts: &DriverReceipts) -> Result<(), DriverStoreError> {
    let bytes = serde_json::to_vec_pretty(receipts)
        .map_err(|error| DriverStoreError::Receipt(error.to_string()))?;
    let parent = path.parent().ok_or_else(|| {
        DriverStoreError::Receipt(format!("{} has no parent directory", path.display()))
    })?;
    std::fs::create_dir_all(parent)
        .map_err(|error| DriverStoreError::Receipt(format!("{}: {error}", parent.display())))?;
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, &bytes)
        .map_err(|error| DriverStoreError::Receipt(format!("{}: {error}", temporary.display())))?;
    std::fs::rename(&temporary, path)
        .map_err(|error| DriverStoreError::Receipt(format!("{}: {error}", path.display())))?;
    Ok(())
}

/// Record one published package. A repeat install of identical bytes reuses
/// the same OEM INF and does not duplicate the entry.
pub fn record_published(
    path: &Path,
    driver: &PublishedDriver,
) -> Result<DriverReceipts, DriverStoreError> {
    let mut receipts = read_receipts(path)?.unwrap_or_else(DriverReceipts::empty);
    receipts.record(driver);
    write_receipts(path, &receipts)?;
    Ok(receipts)
}

/// Remove one exact recorded package and drop only its receipt entry.
///
/// Used when an install fails after publication and the candidate package is
/// not the one the restored version runs. Removal runs before the receipt is
/// rewritten, so a failed removal leaves the name recorded for the next
/// attempt. Other entries survive: an upgrade's still-active package must not
/// be forgotten because the candidate was rolled back.
pub fn remove_one_recorded(
    path: &Path,
    oem_inf: &str,
    mut remove: impl FnMut(&str) -> Result<(), DriverStoreError>,
) -> Result<bool, DriverStoreError> {
    if !is_oem_inf_name(oem_inf) {
        return Err(DriverStoreError::Receipt(format!(
            "{oem_inf} is not an exact oemNN.inf name"
        )));
    }
    let Some(mut receipts) = read_receipts(path)? else {
        return Ok(false);
    };
    if !receipts
        .packages
        .iter()
        .any(|package| package.oem_inf == oem_inf)
    {
        return Ok(false);
    }
    remove(oem_inf)?;
    receipts
        .packages
        .retain(|package| package.oem_inf != oem_inf);
    if receipts.packages.is_empty() {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(DriverStoreError::Receipt(format!(
                    "{}: {error}",
                    path.display()
                )));
            }
        }
    } else {
        write_receipts(path, &receipts)?;
    }
    Ok(true)
}

/// Remove every recorded package, in order, through the supplied remover.
///
/// The receipt is cleared only after **every** package is gone. A failure
/// leaves the file untouched, so the exact remaining `oemNN.inf` names survive
/// for the next attempt instead of being rediscovered by a wildcard.
pub fn remove_recorded(
    path: &Path,
    mut remove: impl FnMut(&str) -> Result<(), DriverStoreError>,
) -> Result<Vec<String>, DriverStoreError> {
    let Some(mut receipts) = read_receipts(path)? else {
        return Ok(Vec::new());
    };
    let mut removed = Vec::new();
    // Persist after every package. A later failure must not leave an
    // already-removed name in the receipt: the next uninstall would try it
    // again, and an absent package must not make the retry unprovable.
    while let Some(package) = receipts.packages.first().cloned() {
        remove(&package.oem_inf)?;
        removed.push(package.oem_inf.clone());
        receipts.packages.remove(0);
        if receipts.packages.is_empty() {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(DriverStoreError::Receipt(format!(
                        "{}: {error}",
                        path.display()
                    )));
                }
            }
        } else {
            write_receipts(path, &receipts)?;
        }
    }
    Ok(removed)
}
