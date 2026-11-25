// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_prepare.rs
//! Guarded preparation and recovery of a release before activation.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;

use super::CustomerError;
use super::package_publish::{inventory, publication_journal_present};
use super::package_source::DeclaredBundle;
use super::package_stage_journal::{inventory as stage, stage_journal_present};
use super::package_tree::{self, OwnedTree, tree_journal_present};
use super::package_trust::VerifiedSignature;
use super::transition::{
    CustomerTransitionGuard, OperationOwner, PackageAction, PackagePending, read_pending,
    recovery_refusal,
};

pub(super) struct PreparePaths<'a> {
    pub stage: &'a Path,
    pub parent: &'a Path,
    pub parent_identity: (u64, u64),
    pub name: &'a str,
    pub reserve_bytes: u64,
}

pub(super) struct PreparedRelease {
    pub tree: OwnedTree,
    pub members: BTreeMap<String, File>,
}

fn evidence(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    signature: &VerifiedSignature,
) -> Result<(bool, bool, bool), CustomerError> {
    if super::package_selector::journal_present(guard.paths())? {
        return Err(recovery_refusal(
            "package selector requires reconciliation before preparation",
        ));
    }
    if guard.owner() != OperationOwner::Package
        || pending.action == PackageAction::Remove
        || pending.new_hash != bundle.manifest_sha256()
        || pending.new_hash != signature.manifest_sha256()
        || signature.revalidate().is_none()
        || read_pending(guard.paths())?.as_ref() != Some(pending)
    {
        return Err(recovery_refusal(
            "prepare transaction identity does not match",
        ));
    }
    let target = pending
        .target_release
        .as_deref()
        .ok_or_else(|| recovery_refusal("prepare target identity is absent"))?;
    let identity = bundle
        .member(
            "RELEASE_VERSION",
            crate::release_identity::MAX_IDENTITY_LINE_BYTES,
        )
        .and_then(|bytes| crate::release_identity::parse_identity_line(&bytes));
    if identity.as_deref() != Some(target) || signature.revalidate().is_none() {
        return Err(recovery_refusal("prepare target identity changed"));
    }
    guard.revalidate()?;
    let result = (
        stage_journal_present(guard.paths())?,
        tree_journal_present(guard.paths())?,
        publication_journal_present(guard.paths())?,
    );
    if (!result.0 && (result.1 || result.2)) || (!result.1 && result.2) {
        return Err(recovery_refusal(
            "prepare journals have an impossible sequence",
        ));
    }
    guard.revalidate()?;
    if signature.revalidate().is_none() || read_pending(guard.paths())?.as_ref() != Some(pending) {
        return Err(recovery_refusal("prepare transaction changed"));
    }
    Ok(result)
}

/// Prepare using the guard retained by admission, without releasing it.
pub(super) fn prepare_admitted(
    transaction: &super::package_transition::AdmittedPackage<'_>,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    signature: &VerifiedSignature,
    paths: &PreparePaths<'_>,
) -> Result<PreparedRelease, CustomerError> {
    prepare_admitted_with_hooks(transaction, pending, bundle, signature, paths, || {}, || {})
}

pub(super) fn prepare_admitted_with_hooks(
    transaction: &super::package_transition::AdmittedPackage<'_>,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    signature: &VerifiedSignature,
    paths: &PreparePaths<'_>,
    after_stage: impl FnOnce(),
    after_tree: impl FnOnce(),
) -> Result<PreparedRelease, CustomerError> {
    if transaction.plan().action != pending.action
        || !transaction.admits_source(bundle)
        || transaction.candidate_release().is_none()
        || transaction.candidate_release() != pending.target_release.as_deref()
    {
        return Err(recovery_refusal(
            "admitted action or source changed before preparation",
        ));
    }
    prepare(
        transaction.guard(),
        pending,
        bundle,
        signature,
        paths,
        after_stage,
        after_tree,
    )
}

/// Begin only when no previous package effect is recorded.
fn prepare(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    signature: &VerifiedSignature,
    paths: &PreparePaths<'_>,
    after_stage: impl FnOnce(),
    after_tree: impl FnOnce(),
) -> Result<PreparedRelease, CustomerError> {
    if evidence(guard, pending, bundle, signature)? != (false, false, false) {
        return Err(recovery_refusal("prepare history already exists"));
    }
    drop(stage::stage_all_members(
        guard,
        pending,
        bundle,
        paths.stage,
        paths.reserve_bytes,
    )?);
    after_stage();
    if evidence(guard, pending, bundle, signature)? != (true, false, false) {
        return Err(recovery_refusal("prepare history changed after staging"));
    }
    let tree = package_tree::create_tree(
        guard,
        pending,
        bundle,
        paths.parent,
        paths.parent_identity,
        paths.name,
        paths.reserve_bytes,
    )?;
    after_tree();
    if evidence(guard, pending, bundle, signature)? != (true, true, false) {
        return Err(recovery_refusal(
            "prepare history changed before publication",
        ));
    }
    let members = inventory::publish_owned_all(
        guard,
        pending,
        bundle,
        &tree,
        signature,
        paths.reserve_bytes,
    )?;
    Ok(PreparedRelease { tree, members })
}

/// Resume only the phase established by the durable evidence.
pub(super) fn resume(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    signature: &VerifiedSignature,
    paths: &PreparePaths<'_>,
) -> Result<PreparedRelease, CustomerError> {
    let (staged, tree_exists, published) = evidence(guard, pending, bundle, signature)?;
    if !staged {
        return Err(recovery_refusal("prepare history is absent"));
    }
    super::package_stage_journal::require_stage_source(guard.paths(), bundle)?;
    if !tree_exists {
        drop(stage::resume_staged_inventory(
            guard,
            pending,
            bundle,
            paths.stage,
            paths.reserve_bytes,
        )?);
        if evidence(guard, pending, bundle, signature)? != (true, false, false) {
            return Err(recovery_refusal(
                "prepare history changed during stage recovery",
            ));
        }
    }
    let tree = if published {
        package_tree::reopen_owned_tree(
            guard,
            pending,
            bundle,
            paths.parent,
            paths.parent_identity,
            paths.name,
        )?
    } else if tree_exists {
        package_tree::resume_tree(
            guard,
            pending,
            bundle,
            paths.parent,
            paths.parent_identity,
            paths.name,
            paths.reserve_bytes,
        )?
    } else {
        package_tree::create_tree(
            guard,
            pending,
            bundle,
            paths.parent,
            paths.parent_identity,
            paths.name,
            paths.reserve_bytes,
        )?
    };
    if evidence(guard, pending, bundle, signature)? != (true, true, published) {
        return Err(recovery_refusal(
            "prepare history changed before publication recovery",
        ));
    }
    let members = if published {
        inventory::resume_owned_all(
            guard,
            pending,
            bundle,
            &tree,
            signature,
            paths.reserve_bytes,
        )?
    } else {
        inventory::publish_owned_all(
            guard,
            pending,
            bundle,
            &tree,
            signature,
            paths.reserve_bytes,
        )?
    };
    Ok(PreparedRelease { tree, members })
}
