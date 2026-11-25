// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/detached_nbd.rs
//! Kernel boundary proof that a detached NBD block device is idle, zero-sized, and unheld.
//!
//! Extracted from `cascade_io.rs` to keep detached-NBD evidence validation isolated.

use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DetachedNbdObservation {
    pub(super) path: String,
    pub(super) dev_t: String,
    pub(super) sysfs_path: String,
    pub(super) sysfs_dev_t: String,
}

#[derive(Clone, Debug)]
pub(super) struct DetachedNbdEvidence {
    pub(super) named_is_symlink: bool,
    pub(super) named_is_block_device: bool,
    pub(super) dev_t: String,
    pub(super) sysfs_path: String,
    pub(super) sysfs_dev_t: String,
    pub(super) owner_pid: Option<u32>,
    pub(super) size_sectors: u64,
    pub(super) has_holders: bool,
    pub(super) named_after_is_symlink: bool,
    pub(super) named_after_is_block_device: bool,
    pub(super) named_after_dev_t: String,
}

#[derive(Clone, Debug)]
pub(super) struct DetachedNbdNode {
    pub(super) is_symlink: bool,
    pub(super) is_block_device: bool,
    pub(super) dev_t: String,
}

pub(super) fn validate_detached_nbd_evidence(
    path: String,
    evidence: DetachedNbdEvidence,
) -> Result<DetachedNbdObservation, CascadeError> {
    if evidence.named_is_symlink || !evidence.named_is_block_device {
        return Err(CascadeError::Precondition(
            "detached NBD target is not an exact block-device node".into(),
        ));
    }
    if evidence.sysfs_dev_t != evidence.dev_t {
        return Err(CascadeError::Precondition(
            "detached NBD node and sysfs dev_t disagree".into(),
        ));
    }
    if evidence.owner_pid.is_some_and(|pid| pid != 0) {
        return Err(CascadeError::UnsafeContainment(format!(
            "NBD target {path} still has a kernel owner PID"
        )));
    }
    if evidence.size_sectors != 0 {
        return Err(CascadeError::UnsafeContainment(format!(
            "NBD target {path} still exports {} sectors",
            evidence.size_sectors
        )));
    }
    if evidence.has_holders {
        return Err(CascadeError::UnsafeContainment(format!(
            "NBD target {path} still has a kernel holder"
        )));
    }
    if evidence.named_after_is_symlink
        || !evidence.named_after_is_block_device
        || evidence.named_after_dev_t != evidence.dev_t
    {
        return Err(CascadeError::Precondition(
            "detached NBD target identity changed during absence proof".into(),
        ));
    }
    Ok(DetachedNbdObservation {
        path,
        dev_t: evidence.dev_t,
        sysfs_path: evidence.sysfs_path,
        sysfs_dev_t: evidence.sysfs_dev_t,
    })
}

pub(super) fn prove_detached_nbd_sysfs<F>(
    path: String,
    named: DetachedNbdNode,
    sysfs_class_block: &Path,
    mut restat_named: F,
) -> Result<DetachedNbdObservation, CascadeError>
where
    F: FnMut() -> Result<DetachedNbdNode, CascadeError>,
{
    if named.is_symlink || !named.is_block_device {
        return Err(CascadeError::Precondition(
            "detached NBD target is not an exact block-device node".into(),
        ));
    }
    let basename = Path::new(&path)
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| CascadeError::Precondition("detached NBD name is invalid".into()))?;
    let sysfs = fs::canonicalize(sysfs_class_block.join(basename)).map_err(|error| {
        CascadeError::Precondition(format!("resolve detached NBD sysfs identity: {error}"))
    })?;
    let sysfs_dev_t =
        parse_sysfs_device_number(&fs::read_to_string(sysfs.join("dev")).map_err(|error| {
            CascadeError::Precondition(format!("read detached NBD sysfs dev_t: {error}"))
        })?)?;
    let owner_pid = match fs::read_to_string(sysfs.join("pid")) {
        Ok(value) => {
            let value = value.trim();
            (!value.is_empty())
                .then(|| value.parse::<u32>())
                .transpose()
                .map_err(|_| {
                    CascadeError::Precondition("detached NBD owner PID is malformed".into())
                })?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(CascadeError::Precondition(format!(
                "read detached NBD owner PID: {error}"
            )));
        }
    };
    let size_sectors = fs::read_to_string(sysfs.join("size"))
        .map_err(|error| CascadeError::Precondition(format!("read detached NBD size: {error}")))?
        .trim()
        .parse::<u64>()
        .map_err(|_| CascadeError::Precondition("detached NBD size is malformed".into()))?;
    let mut holders = fs::read_dir(sysfs.join("holders")).map_err(|error| {
        CascadeError::Precondition(format!("enumerate detached NBD holders: {error}"))
    })?;
    let has_holders = holders
        .next()
        .transpose()
        .map_err(|error| CascadeError::Precondition(format!("read detached NBD holder: {error}")))?
        .is_some();
    let named_after = restat_named()?;
    validate_detached_nbd_evidence(
        path,
        DetachedNbdEvidence {
            named_is_symlink: named.is_symlink,
            named_is_block_device: named.is_block_device,
            dev_t: named.dev_t,
            sysfs_path: sysfs.to_string_lossy().into_owned(),
            sysfs_dev_t,
            owner_pid,
            size_sectors,
            has_holders,
            named_after_is_symlink: named_after.is_symlink,
            named_after_is_block_device: named_after.is_block_device,
            named_after_dev_t: named_after.dev_t,
        },
    )
}

/// Proves that one exact NBD target is detached at the kernel boundary.
///
/// A missing entry in the live-device enumeration is not enough: a timeout can
/// race an attach effect. The target node, sysfs dev_t, owner PID, exported
/// size, holders, and a fresh strict `/proc/swaps` snapshot must all agree.
pub(super) fn observe_exact_detached_nbd(
    path: &str,
) -> Result<DetachedNbdObservation, CascadeError> {
    let path = canonicalize_swap_path(path);
    validate_nbd_swap_device(&path)?;
    let swaps = read_swaps()?;
    if swaps.iter().any(|entry| entry.canonical_path() == path) {
        return Err(CascadeError::UnsafeContainment(format!(
            "NBD target {path} is still active in a fresh strict /proc/swaps snapshot"
        )));
    }
    #[cfg(test)]
    {
        let observed = observe_bound_device(&path, ManagedDeviceKind::Nbd)?;
        validate_detached_nbd_evidence(
            path,
            DetachedNbdEvidence {
                named_is_symlink: false,
                named_is_block_device: true,
                dev_t: observed.dev_t.clone(),
                sysfs_path: observed.sysfs_path,
                sysfs_dev_t: observed.sysfs_dev_t,
                owner_pid: None,
                size_sectors: 0,
                has_holders: false,
                named_after_is_symlink: false,
                named_after_is_block_device: true,
                named_after_dev_t: observed.dev_t,
            },
        )
    }
    #[cfg(not(test))]
    {
        let named = fs::symlink_metadata(&path).map_err(|error| {
            CascadeError::Precondition(format!("stat exact detached NBD target: {error}"))
        })?;
        let path_for_restat = path.clone();
        prove_detached_nbd_sysfs(
            path,
            DetachedNbdNode {
                is_symlink: named.file_type().is_symlink(),
                is_block_device: named.file_type().is_block_device(),
                dev_t: linux_device_number(named.rdev()),
            },
            Path::new("/sys/class/block"),
            || {
                let named_after = fs::symlink_metadata(&path_for_restat).map_err(|error| {
                    CascadeError::Precondition(format!("restat exact detached NBD target: {error}"))
                })?;
                Ok(DetachedNbdNode {
                    is_symlink: named_after.file_type().is_symlink(),
                    is_block_device: named_after.file_type().is_block_device(),
                    dev_t: linux_device_number(named_after.rdev()),
                })
            },
        )
    }
}
