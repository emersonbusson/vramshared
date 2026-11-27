// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_tree/rollback.rs

//! Bind rollback cleanup to the original release tree creation receipt.

use std::os::unix::fs::MetadataExt;

use super::{
    AtFlags, CustomerError, CustomerTransitionGuard, OperationOwner, OwnedTree, PackagePending,
    Path, Phase, check_empty, identity, open_directory, open_directory_no_follow, read_history,
    read_pending, recovery_refusal, statat, validate_directory, verify_nonce,
};
use crate::customer::package_publish::inventory::mount_id;

pub(in crate::customer) fn observe_owned_root(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    parent: &Path,
) -> Result<Option<OwnedTree>, CustomerError> {
    if guard.owner() != OperationOwner::Package
        || read_pending(guard.paths())?.as_ref() != Some(pending)
    {
        return Err(recovery_refusal("rollback tree transaction changed"));
    }
    guard.revalidate()?;
    let Some(name) = pending.target_release.as_deref() else {
        return Ok(None);
    };
    let Some(anchor) = open_directory_no_follow(parent, false)? else {
        return Ok(None);
    };
    validate_directory(&anchor, "rollback releases directory")?;
    let entry = match statat(&anchor, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(entry) => entry,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(_) => {
            return Err(recovery_refusal(
                "cannot inspect rollback release directory",
            ));
        }
    };
    let history = read_history(guard.paths())?
        .ok_or_else(|| recovery_refusal("release directory has no durable rollback custody"))?;
    let root = history
        .get(1)
        .or_else(|| history.first())
        .filter(|record| {
            record.pending == *pending
                && record.parent == parent
                && record.root_name == name
                && record.relative.as_os_str().is_empty()
        })
        .ok_or_else(|| recovery_refusal("release directory has no matching rollback receipt"))?;
    if root.parent_identity != identity(&anchor)?
        || root.directory_parent_identity != root.parent_identity
        || root.mount_id != mount_id(&anchor)?
    {
        return Err(recovery_refusal(
            "rollback releases directory identity changed",
        ));
    }
    // The history validator requires identity on Ready and omits it on Intent.
    // A root without confirmation must still be private, marked and empty.
    let expected = root.identity.unwrap_or((entry.st_dev, entry.st_ino));
    let path = parent.join(name);
    let directory = open_directory(&path, expected, root.mount_id)?;
    verify_nonce(&directory, &root.nonce)?;
    if root.phase == Phase::Intent {
        let mode = directory
            .metadata()
            .map_err(|_| recovery_refusal("unconfirmed release directory is unavailable"))?
            .mode();
        if mode & 0o7777 != 0o700 {
            return Err(recovery_refusal(
                "unconfirmed release directory mode changed",
            ));
        }
        check_empty(&directory)?;
    }
    guard.revalidate()?;
    Ok(Some(OwnedTree {
        root: path,
        identity: expected,
    }))
}
