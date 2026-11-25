// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_publish/reconciliation.rs
//! Read-only identity for a missing, ambiguously published member.

use rustix::fs::{AtFlags, statat};
use sha2::{Digest, Sha256};

use super::{
    CustomerError, CustomerTransitionGuard, DeclaredBundle, PackagePending, Phase, directory,
    open_published_record, publication_signature, read_history, recovery_refusal, revalidate_tree,
    target, verify_custody, verify_inventory, verify_recorded_stage,
};
use crate::customer::{package_tree::OwnedTree, package_trust::VerifiedSignature};

fn fingerprint_evidence(publication: &[u8], stage: &[u8], tree: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"ramshared-missing-publication-observation-v2\0");
    for evidence in [publication, stage, tree] {
        digest.update((evidence.len() as u64).to_be_bytes());
        digest.update(evidence);
    }
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn fingerprint_with_trust(publication: &[u8], stage: &[u8], tree: &[u8], trust: &[u8]) -> String {
    let evidence = fingerprint_evidence(publication, stage, tree);
    let mut digest = Sha256::new();
    digest.update(b"ramshared-missing-publication-observation-v3\0");
    for value in [evidence.as_bytes(), trust] {
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value);
    }
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A point-in-time reference, never an approval or a permit to mutate.
pub(in crate::customer) struct MissingObservation {
    fingerprint: String,
    destination: std::path::PathBuf,
    role: String,
    manifest_sha256: String,
}

impl MissingObservation {
    pub(in crate::customer) fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub(in crate::customer) fn refusal_detail(&self) -> String {
        format!(
            "prepared publication target {:?} for member {:?} is absent; effect may have occurred; manifest {}; review reference {}; this is not an approval",
            self.destination,
            self.role,
            self.manifest_sha256,
            self.fingerprint()
        )
    }
}

/// Identify an absent prepared target only after revalidating its retained evidence.
pub(in crate::customer) fn observe_missing(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    tree: &OwnedTree,
    signature: &VerifiedSignature,
) -> Result<Option<MissingObservation>, CustomerError> {
    observe_missing_inner(guard, pending, bundle, tree, signature, || {})
}

#[cfg(test)]
pub(in crate::customer) fn observe_missing_with_hook(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    tree: &OwnedTree,
    signature: &VerifiedSignature,
    after_prior: impl FnOnce(),
) -> Result<Option<MissingObservation>, CustomerError> {
    observe_missing_inner(guard, pending, bundle, tree, signature, after_prior)
}

fn observe_missing_inner(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    tree: &OwnedTree,
    signature: &VerifiedSignature,
    after_prior: impl FnOnce(),
) -> Result<Option<MissingObservation>, CustomerError> {
    let Some(history) = read_history(guard.paths())? else {
        return Ok(None);
    };
    let Some(last) = history
        .last()
        .filter(|record| record.phase == Phase::Prepared)
    else {
        return Ok(None);
    };
    if last.pending != *pending || last.root != tree.path() || last.root_identity != tree.identity()
    {
        return Err(recovery_refusal(
            "reconciliation transaction identity changed",
        ));
    }
    let (parent_path, leaf) = target(&last.root, &last.destination)?;
    directory(&last.root, last.root_identity)?;
    let parent = directory(&parent_path, last.parent_identity)?;
    match statat(&parent, &leaf, AtFlags::SYMLINK_NOFOLLOW) {
        Err(error) if error == rustix::io::Errno::NOENT => (),
        Ok(_) => return Ok(None),
        Err(_) => return Err(recovery_refusal("reconciliation target is unavailable")),
    }
    verify_custody(guard, pending, bundle)?;
    publication_signature(signature, pending)?;
    let trust = signature
        .trust_reference()
        .ok_or_else(|| recovery_refusal("reconciliation trust identity is unavailable"))?;
    revalidate_tree(guard, pending, bundle, Some(tree))?;
    let slots = verify_inventory(guard, pending, bundle)?;
    for record in history
        .iter()
        .filter(|record| record.phase == Phase::Prepared)
    {
        verify_recorded_stage(record, &slots)?;
        let (destination, mode, digest) = bundle
            .publication_member(&record.role)
            .ok_or_else(|| recovery_refusal("reconciliation member is not declared"))?;
        if (destination, mode, digest)
            != (
                record.destination.clone(),
                record.mode,
                record.digest.clone(),
            )
        {
            return Err(recovery_refusal("reconciliation declaration changed"));
        }
    }
    for record in history
        .iter()
        .filter(|record| record.phase == Phase::Published)
    {
        drop(open_published_record(record, bundle)?);
    }
    after_prior();
    let encoded = serde_json::to_vec(&history)
        .map_err(|_| recovery_refusal("cannot encode reconciliation observation"))?;
    let stage_evidence =
        crate::customer::package_stage_journal::reconciliation_evidence(guard.paths())?;
    let tree_evidence = crate::customer::package_tree::reconciliation_evidence(guard.paths())?;
    let fingerprint =
        fingerprint_with_trust(&encoded, &stage_evidence, &tree_evidence, trust.as_bytes());
    verify_custody(guard, pending, bundle)?;
    publication_signature(signature, pending)?;
    if signature.trust_reference().as_deref() != Some(trust.as_str()) {
        return Err(recovery_refusal("reconciliation trust identity changed"));
    }
    revalidate_tree(guard, pending, bundle, Some(tree))?;
    directory(&parent_path, last.parent_identity)?;
    if read_history(guard.paths())?.as_ref() != Some(&history)
        || crate::customer::package_stage_journal::reconciliation_evidence(guard.paths())?
            != stage_evidence
        || crate::customer::package_tree::reconciliation_evidence(guard.paths())? != tree_evidence
        || slots.values().any(|slot| slot.revalidate().is_none())
        || !matches!(
            statat(&parent, &leaf, AtFlags::SYMLINK_NOFOLLOW),
            Err(rustix::io::Errno::NOENT)
        )
    {
        return Err(recovery_refusal("reconciliation observation changed"));
    }
    for record in history
        .iter()
        .filter(|record| record.phase == Phase::Published)
    {
        drop(open_published_record(record, bundle)?);
    }
    verify_custody(guard, pending, bundle)?;
    if read_history(guard.paths())?.as_ref() != Some(&history) {
        return Err(recovery_refusal("reconciliation history changed"));
    }
    Ok(Some(MissingObservation {
        fingerprint,
        destination: last.root.join(&last.destination),
        role: last.role.clone(),
        manifest_sha256: pending.new_hash.clone(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observation_identity_binds_each_ordered_evidence_document() {
        let baseline = fingerprint_evidence(b"publication", b"stage", b"tree");
        assert_eq!(
            baseline,
            fingerprint_evidence(b"publication", b"stage", b"tree")
        );
        assert_ne!(
            baseline,
            fingerprint_evidence(b"publication!", b"stage", b"tree")
        );
        assert_ne!(
            baseline,
            fingerprint_evidence(b"publication", b"stage!", b"tree")
        );
        assert_ne!(
            baseline,
            fingerprint_evidence(b"publication", b"stage", b"tree!")
        );
        assert_ne!(
            fingerprint_evidence(b"ab", b"c", b"d"),
            fingerprint_evidence(b"a", b"bc", b"d")
        );
    }

    #[test]
    fn observation_identity_changes_with_trust_authority() {
        assert_ne!(
            fingerprint_with_trust(b"publication", b"stage", b"tree", b"authority-a"),
            fingerprint_with_trust(b"publication", b"stage", b"tree", b"authority-b")
        );
    }
}
