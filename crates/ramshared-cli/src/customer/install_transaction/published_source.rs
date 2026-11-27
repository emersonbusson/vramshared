// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/install_transaction/published_source.rs
//! Bridge bounded published discovery into existing installer custody.

use std::io::Write;
use std::path::{Path, PathBuf};

use super::{InstallEnvironment, InstallRequest, InstallerLock, LifecycleHost, installer_lock};
use crate::customer::published_update::{cache_root, ensure_cached_patch};
use crate::customer::release_ordering::{
    AutoUpdatePolicy, find_bundle_by_manifest_sha256, find_previous_bundle,
    read_auto_update_policy, read_enrolled_posture, stable_release_series,
};
use crate::customer::{CustomerError, CustomerReason, CustomerState, install_fs::fs_error};
use crate::installed_image::observe_installed_input_bundle_digest;
use crate::release_identity::read_release_version;

pub(super) struct FetchedPatch {
    observed_release: String,
    observed_manifest: String,
    bundle: PathBuf,
}

impl FetchedPatch {
    pub(super) fn for_running(self, running: Option<&str>, selector: &Path) -> Option<PathBuf> {
        (running == Some(self.observed_release.as_str())
            && observe_installed_input_bundle_digest(selector).as_deref()
                == Some(self.observed_manifest.as_str()))
        .then_some(self.bundle)
    }
}

pub(super) fn enabled(req: &InstallRequest, policy: Option<&AutoUpdatePolicy>) -> bool {
    req.bundle_root.is_none() && policy.is_some_and(|policy| policy.enabled && policy.published)
}

pub(super) fn fetch_outside_install_lock(
    req: &InstallRequest,
    env: &InstallEnvironment,
    lifecycle: &dyn LifecycleHost,
    lock: &mut Option<InstallerLock>,
) -> Result<Option<FetchedPatch>, CustomerError> {
    // The local fixture environment has no production fetch authorization.
    if !env.require_root || req.bundle_root.is_some() || lock.is_none() {
        return Ok(None);
    }
    let policy_path = req.policy.as_deref().unwrap_or(&env.default_policy);
    let policy = read_auto_update_policy(policy_path, Some(env.owner_uid))?;
    let Some(policy) = policy.filter(|policy| enabled(req, Some(policy))) else {
        return Ok(None);
    };
    let selector = env.selector_path();
    let Some(running) = read_release_version(&selector) else {
        return Ok(None);
    };
    if stable_release_series(&running).is_none()
        || read_enrolled_posture(&env.approval_dir, &running)?.is_none()
    {
        return Ok(None);
    }
    let keyring = policy.release_keyring.as_deref().ok_or_else(|| {
        fs_error(
            CustomerReason::AuthorizationRequired,
            "published updates need an enrolled release keyring",
            CustomerState::InstalledOff,
        )
    })?;
    let previous_manifest = observe_installed_input_bundle_digest(&selector).ok_or_else(|| {
        fs_error(
            CustomerReason::IdentityChanged,
            "installed release does not have a verifiable input bundle",
            CustomerState::InstalledOff,
        )
    })?;
    let local_root = policy
        .bundle_root
        .as_deref()
        .unwrap_or(&env.default_bundle_root);
    let local_previous_exists =
        find_bundle_by_manifest_sha256(local_root, &previous_manifest).is_some();
    drop(lock.take());
    let result = ensure_cached_patch(
        &cache_root(&env.product_root),
        &env.transition.lock_root,
        policy_path,
        keyring,
        &running,
        &previous_manifest,
        local_previous_exists,
    );
    *lock = installer_lock(env, lifecycle)?;
    let refreshed = read_auto_update_policy(policy_path, Some(env.owner_uid))?;
    let unchanged = refreshed.as_ref() == Some(&policy);
    Ok(result?.filter(|_| unchanged).map(|bundle| FetchedPatch {
        observed_release: running,
        observed_manifest: previous_manifest,
        bundle,
    }))
}

pub(super) fn find_drain_bundle(
    env: &InstallEnvironment,
    local_root: &Path,
    policy: &Path,
    manifest: &str,
) -> Result<Option<PathBuf>, CustomerError> {
    Ok(
        find_previous_bundle(local_root, policy, env.owner_uid, manifest)?
            .or_else(|| find_bundle_by_manifest_sha256(&cache_root(&env.product_root), manifest)),
    )
}

pub(super) fn write_no_prepared_patch(json: bool, out: &mut dyn Write) {
    if json {
        let _ = writeln!(
            out,
            "{}",
            serde_json::json!({"state":"no_prepared_patch","source":"github"})
        );
    } else {
        let _ = writeln!(out, "UPDATER_STATE=NO_PREPARED_PATCH");
    }
}
