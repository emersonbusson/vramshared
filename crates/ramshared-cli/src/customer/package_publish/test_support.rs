// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_publish/test_support.rs
//! Deterministic interruption hooks used only by isolated fixtures.

use super::{
    CustomerError, CustomerTransitionGuard, DeclaredBundle, PackagePending, PublicationHooks,
    publication_signature, publish_impl,
};

pub(in crate::customer) fn publish_signed_owned_with_after_link(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    tree: &super::super::package_tree::OwnedTree,
    role: &str,
    signature: &super::super::package_trust::VerifiedSignature,
    after_link: impl FnOnce() -> Result<(), CustomerError>,
) -> Result<(), CustomerError> {
    publish_impl(
        guard,
        pending,
        bundle,
        (tree.path(), tree.identity()),
        role,
        Some(tree),
        PublicationHooks {
            before_link: || Ok(()),
            after_link,
            after_record: || Ok(()),
            before_effect: || publication_signature(signature, pending),
            before_confirmation: || publication_signature(signature, pending),
        },
    )
}

pub(in crate::customer) fn publish_signed_owned_with_before_link(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    tree: &super::super::package_tree::OwnedTree,
    role: &str,
    signature: &super::super::package_trust::VerifiedSignature,
    before_link: impl FnOnce() -> Result<(), CustomerError>,
) -> Result<(), CustomerError> {
    publish_impl(
        guard,
        pending,
        bundle,
        (tree.path(), tree.identity()),
        role,
        Some(tree),
        PublicationHooks {
            before_link,
            after_link: || Ok(()),
            after_record: || Ok(()),
            before_effect: || publication_signature(signature, pending),
            before_confirmation: || publication_signature(signature, pending),
        },
    )
}

pub(in crate::customer) fn publish_owned_with_before_link(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    tree: &super::super::package_tree::OwnedTree,
    role: &str,
    before_link: impl FnOnce() -> Result<(), CustomerError>,
) -> Result<(), CustomerError> {
    publish_impl(
        guard,
        pending,
        bundle,
        (tree.path(), tree.identity()),
        role,
        Some(tree),
        PublicationHooks {
            before_link,
            after_link: || Ok(()),
            after_record: || Ok(()),
            before_effect: || Ok(()),
            before_confirmation: || Ok(()),
        },
    )
}

pub(in crate::customer) fn publish_owned_with_after_link(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    bundle: &DeclaredBundle,
    tree: &super::super::package_tree::OwnedTree,
    role: &str,
    after_link: impl FnOnce() -> Result<(), CustomerError>,
) -> Result<(), CustomerError> {
    publish_impl(
        guard,
        pending,
        bundle,
        (tree.path(), tree.identity()),
        role,
        Some(tree),
        PublicationHooks {
            before_link: || Ok(()),
            after_link,
            after_record: || Ok(()),
            before_effect: || Ok(()),
            before_confirmation: || Ok(()),
        },
    )
}
