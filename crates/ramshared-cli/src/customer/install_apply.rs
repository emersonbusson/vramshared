// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/install_apply.rs
//! Verified package apply, sealing, selector publication, and rollback custody.

use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::install_args::{InstallRequest, utc_timestamp_now};
use super::install_drain::InstallerLock;
use super::install_fs::{ensure_dir_no_follow, fs_error, read_bounded_regular_at};
use super::install_tokens::{
    ApprovalTokenCustody, EnrollmentOutcome, apply_default_enrollment,
    cleanup_owned_prior_approvals, publish_activation_approval,
};
use super::install_transaction::{INSTALL_RESERVE_BYTES, InstallEnvironment, resolve_bundle_dir};
use super::install_units::{
    SystemUnitHost, UnitHost, UnitInstallCustody, check_install_units, inspect_and_stage_units,
};
use super::package::{
    MAX_PAYLOAD_MANIFEST_BYTES, PayloadIdentity, PayloadManifest, RETIRED_LIFECYCLE_UNIT,
};
use super::package_admission::{LifecycleOwnership, LifecycleSnapshot};
use super::package_prepare::{PreparePaths, prepare_admitted};
use super::package_publish::clear_publication_journal;
use super::package_selector::{
    PreviousEvidence, PreviousTransformation, SelectorEffectRecord, SelectorRecoveryAction,
    SinkWitness, clear_selector_journals, finalize_and_seal_release, inspect_effect_journal,
    publish_sealed_selector, reconcile_selector, record_prepared_intent,
    remove_owned_unselected_release,
};
use super::package_source::stream::{MemoryObservation, MemoryRequirement};
use super::package_source::{DeclaredBundle, MEMBER_LIMIT, StreamVerifiedBundle};
use super::package_stage_journal::{cleanup_staged_members, clear_stage_journal};
use super::package_transition::{PackageObservation, begin_verified_package_held};
use super::package_tree::clear_tree_journal;
use super::package_trust::{
    VerifiedSignature, verify_bundle, verify_bundle_from_policy, verify_bundle_with_keyring,
};
use super::plan::ReleaseIdentity;
use super::release_ordering::{ReleaseOrder, find_previous_bundle, release_is_newer};
use super::transition::{CustomerTransitionGuard, PackageAction, PackagePending, clear_pending};
use super::transition_custody::open_directory_no_follow;
use super::{CustomerError, CustomerReason, CustomerState};
use crate::cascade::boot_policy_effect::{BootPolicyHost, CASCADE_UNIT};
use crate::cascade::posture::Posture;
use crate::installed_image::{observe_installed_input_bundle_digest, sha256_reader};
use crate::operation_progress::{OperationProgressGuard, PACKAGE_COMMIT, PACKAGE_VERIFY};
use crate::release_identity::{MAX_SEALED_TEXT_BYTES, read_release_version};

mod drain_verification;
use drain_verification::{resolve_prior_identity, verify_quiesced_install};
pub(super) use drain_verification::{verify_committed_request, verify_install};

const MAX_TOTAL_BUNDLE_BYTES: u64 = 512 * 1024 * 1024;
const STREAM_WORK_BYTES: u64 = 1024 * 1024;
const STREAM_GUEST_RESERVE_BYTES: u64 = 16 * 1024 * 1024;
const STREAM_HOST_RESERVE_BYTES: u64 = 16 * 1024 * 1024;
const STREAM_MAX_AGE_MS: u64 = 5_000;

struct ApplyInputs<'a> {
    approved_ver: &'a str,
    approved_sha: &'a str,
    lower_sink_raw: &'a str,
    posture: Posture,
}

pub(super) struct VerifiedCandidate {
    bundle_dir: PathBuf,
    signature: VerifiedSignature,
    declared: DeclaredBundle,
    sink: SinkWitness,
}

struct PriorInstallState {
    prior_root: Option<PathBuf>,
    prior_identity: Option<(u64, u64)>,
    action: PackageAction,
    old_release: String,
    old_hash: String,
    prev_evidence_bundle: Option<(VerifiedSignature, Vec<u8>, Option<SinkWitness>)>,
}

struct CommitCustodyState {
    unit_custody: Option<UnitInstallCustody>,
    token_custody: Option<ApprovalTokenCustody>,
    prepared_release_loc: Option<(PathBuf, (u64, u64))>,
}

pub(super) struct CommittedInstall {
    pub(super) release: String,
    pub(super) selector: PathBuf,
    pub(super) posture: Posture,
    pub(super) enroll_outcome: Option<EnrollmentOutcome>,
}

fn install_err(reason: CustomerReason, detail: impl Into<String>) -> CustomerError {
    fs_error(reason, detail, CustomerState::InstalledOff)
}

fn recovery_err(detail: impl Into<String>) -> CustomerError {
    fs_error(
        CustomerReason::RecoveryRequired,
        detail,
        CustomerState::RecoveryRequired,
    )
}

pub(super) fn verify_candidate_trust(
    bundle_dir: &Path,
    req: &InstallRequest,
    env: &InstallEnvironment,
) -> Result<VerifiedSignature, CustomerError> {
    if let Some(keyring) = req.keyring.as_deref() {
        return verify_bundle_with_keyring(bundle_dir, keyring, env.owner_uid).ok_or_else(|| {
            install_err(
                CustomerReason::AuthorizationRequired,
                "NBD_INSTALL_REASON=SIGNATURE_VERIFICATION_FAILED",
            )
        });
    }
    let verified = if req.policy.is_none()
        && env.require_root
        && env.owner_uid == 0
        && env.default_policy == Path::new("/etc/ramshared/auto-update.conf")
    {
        verify_bundle(bundle_dir)
    } else {
        let policy_path = req.policy.as_deref().unwrap_or(&env.default_policy);
        verify_bundle_from_policy(bundle_dir, policy_path, env.owner_uid)
    };
    verified.ok_or_else(|| {
        install_err(
            CustomerReason::AuthorizationRequired,
            "NBD_INSTALL_REASON=SIGNATURE_VERIFICATION_FAILED",
        )
    })
}

pub(super) fn verify_declared_payload_identity(
    bundle_dir: &Path,
    manifest_bytes: &[u8],
) -> Result<(), CustomerError> {
    let manifest = PayloadManifest::parse(manifest_bytes)?;
    let cli_bytes = std::fs::read(bundle_dir.join("bin/ramshared")).map_err(|_| {
        install_err(
            CustomerReason::IdentityChanged,
            "NBD_INSTALL_REASON=PAYLOAD_BINARY_UNREADABLE (bin/ramshared)",
        )
    })?;
    let daemon_bytes = std::fs::read(bundle_dir.join("bin/ramsharedd")).map_err(|_| {
        install_err(
            CustomerReason::IdentityChanged,
            "NBD_INSTALL_REASON=PAYLOAD_BINARY_UNREADABLE (bin/ramsharedd)",
        )
    })?;
    let cli_sha256 = sha256_reader(&mut cli_bytes.as_slice()).unwrap_or_default();
    let daemon_sha256 = sha256_reader(&mut daemon_bytes.as_slice()).unwrap_or_default();
    manifest.verify_member_bytes("ramshared", &cli_bytes)?;
    manifest.verify_member_bytes("ramsharedd", &daemon_bytes)?;
    manifest.verify_identity(&PayloadIdentity {
        architecture: manifest.architecture,
        product_version: manifest.product.version.clone(),
        cli_sha256,
        daemon_sha256,
    })
}

fn memory_policy() -> MemoryRequirement {
    MemoryRequirement {
        work_bytes: STREAM_WORK_BYTES,
        guest_reserve_bytes: STREAM_GUEST_RESERVE_BYTES,
        host_reserve_bytes: STREAM_HOST_RESERVE_BYTES,
        max_age_ms: STREAM_MAX_AGE_MS,
    }
}

fn resolve_previous_evidence(
    req: &InstallRequest,
    env: &InstallEnvironment,
    bundle_root: &Path,
    prior_root: &Path,
    old_hash: &str,
) -> Result<(VerifiedSignature, Vec<u8>, Option<SinkWitness>), CustomerError> {
    let prev_bundle_dir = match req.previous_bundle.as_deref() {
        Some(path) => path.to_path_buf(),
        None => {
            let search_root = bundle_root
                .parent()
                .and_then(Path::parent)
                .unwrap_or(&env.default_bundle_root);
            let policy_path = req.policy.as_deref().unwrap_or(&env.default_policy);
            find_previous_bundle(search_root, policy_path, env.owner_uid, old_hash)?.ok_or_else(
                || {
                    install_err(
                        CustomerReason::AuthorizationRequired,
                        "NBD_INSTALL_REASON=PREVIOUS_BUNDLE_UNAVAILABLE",
                    )
                },
            )?
        }
    };
    let prev_sig = verify_candidate_trust(&prev_bundle_dir, req, env)?;
    if prev_sig.manifest_sha256() != old_hash {
        return Err(install_err(
            CustomerReason::IdentityChanged,
            "NBD_INSTALL_REASON=PREVIOUS_BUNDLE_HASH_MISMATCH",
        ));
    }
    let prev_declared = DeclaredBundle::open(&prev_bundle_dir, old_hash).ok_or_else(|| {
        install_err(
            CustomerReason::IdentityChanged,
            "NBD_INSTALL_REASON=PREVIOUS_BUNDLE_INVALID",
        )
    })?;
    let template = prev_declared
        .member("cascade.conf.example", MAX_SEALED_TEXT_BYTES)
        .ok_or_else(|| {
            install_err(
                CustomerReason::IdentityChanged,
                "NBD_INSTALL_REASON=PREVIOUS_TEMPLATE_MISSING",
            )
        })?;
    let prior_sink = read_prior_sink(prior_root)?;
    Ok((prev_sig, template, prior_sink))
}

fn read_prior_sink(prior_root: &Path) -> Result<Option<SinkWitness>, CustomerError> {
    let Some(dir) = open_directory_no_follow(prior_root, false)? else {
        return Ok(None);
    };
    let Some(bytes) = read_bounded_regular_at(
        &dir,
        OsStr::new("INSTALL_PROVENANCE.json"),
        MAX_PAYLOAD_MANIFEST_BYTES,
    ) else {
        return Ok(None);
    };
    let Ok(val) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Ok(None);
    };
    let Some(sink_path) = val
        .get("lower_sink")
        .and_then(|s| s.get("canonical_path"))
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(None);
    };
    SinkWitness::observe(sink_path)
        .ok_or_else(|| {
            install_err(
                CustomerReason::IdentityChanged,
                "NBD_INSTALL_REASON=PRIOR_SINK_UNAVAILABLE",
            )
        })
        .map(Some)
}

fn validate_apply_inputs(req: &InstallRequest) -> Result<ApplyInputs<'_>, CustomerError> {
    let approved_ver = req.approved_version.as_deref().ok_or_else(|| {
        install_err(
            CustomerReason::UnsupportedSurface,
            "NBD_INSTALL_REASON=APPROVAL_REQUIRED",
        )
    })?;
    let approved_sha = req.approved_manifest_sha256.as_deref().ok_or_else(|| {
        install_err(
            CustomerReason::UnsupportedSurface,
            "NBD_INSTALL_REASON=APPROVED_MANIFEST_SHA256_MISSING",
        )
    })?;
    let lower_sink_raw = req.lower_sink.as_deref().ok_or_else(|| {
        install_err(
            CustomerReason::UnsupportedSurface,
            "NBD_INSTALL_REASON=LOWER_SINK_MISSING",
        )
    })?;
    let posture = req.posture.ok_or_else(|| {
        install_err(
            CustomerReason::UnsupportedSurface,
            "NBD_INSTALL_REASON=POSTURE_MISSING",
        )
    })?;
    if let Some(target) = req.target_release.as_deref()
        && target != approved_ver
    {
        return Err(install_err(
            CustomerReason::IdentityChanged,
            "NBD_INSTALL_REASON=TARGET_RELEASE_MISMATCH",
        ));
    }
    Ok(ApplyInputs {
        approved_ver,
        approved_sha,
        lower_sink_raw,
        posture,
    })
}

fn open_verified_candidate(
    req: &InstallRequest,
    env: &InstallEnvironment,
    inputs: &ApplyInputs<'_>,
    preverified_sig: Option<VerifiedSignature>,
) -> Result<VerifiedCandidate, CustomerError> {
    let bundle_dir = resolve_bundle_dir(req.bundle_root.as_deref(), &env.default_bundle_root)?;
    let signature = match preverified_sig {
        Some(sig) => sig,
        None => verify_candidate_trust(&bundle_dir, req, env)?,
    };
    if signature.manifest_sha256() != inputs.approved_sha {
        return Err(install_err(
            CustomerReason::IdentityChanged,
            "NBD_INSTALL_REASON=APPROVED_MANIFEST_SHA256_MISMATCH",
        ));
    }
    let declared = DeclaredBundle::open_verified(&bundle_dir, &signature).ok_or_else(|| {
        install_err(
            CustomerReason::IdentityChanged,
            "NBD_INSTALL_REASON=BUNDLE_MANIFEST_INVALID",
        )
    })?;
    let declared_release = declared
        .member("RELEASE_VERSION", MAX_SEALED_TEXT_BYTES)
        .and_then(|bytes| crate::release_identity::parse_identity_line(&bytes));
    if declared_release.as_deref() != Some(inputs.approved_ver) {
        return Err(install_err(
            CustomerReason::IdentityChanged,
            "NBD_INSTALL_REASON=SIGNED_RELEASE_VERSION_MISMATCH",
        ));
    }
    let release_manifest_bytes = declared
        .member("release-manifest.json", MAX_PAYLOAD_MANIFEST_BYTES)
        .ok_or_else(|| {
            install_err(
                CustomerReason::UnsupportedSurface,
                "NBD_INSTALL_REASON=RELEASE_LAYOUT_INVALID",
            )
        })?;
    verify_declared_payload_identity(&bundle_dir, &release_manifest_bytes)?;
    let sink = SinkWitness::observe(inputs.lower_sink_raw).ok_or_else(|| {
        install_err(
            CustomerReason::UnsupportedSurface,
            format!(
                "NBD_INSTALL_REASON=LOWER_SINK_INVALID ({})",
                inputs.lower_sink_raw
            ),
        )
    })?;
    Ok(VerifiedCandidate {
        bundle_dir,
        signature,
        declared,
        sink,
    })
}

fn inspect_prior_install_state(
    req: &InstallRequest,
    env: &InstallEnvironment,
    bundle_dir: &Path,
    selector: &Path,
    approved_ver: &str,
) -> Result<PriorInstallState, CustomerError> {
    let prior_loc = resolve_prior_identity(selector)?;
    let (prior_root, prior_identity) = match prior_loc {
        Some(loc) => (Some(loc.root), Some(loc.identity)),
        None => (None, None),
    };
    let Some(prev_path) = prior_root.as_deref() else {
        if let Some(running) = req.running.as_deref()
            && let ReleaseOrder::Older | ReleaseOrder::Equal =
                release_is_newer(approved_ver, running)
        {
            return Err(install_err(
                CustomerReason::IdentityChanged,
                "NBD_INSTALL_REASON=DOWNGRADE_REFUSED",
            ));
        }
        return Ok(PriorInstallState {
            prior_root: None,
            prior_identity: None,
            action: PackageAction::Install,
            old_release: approved_ver.to_owned(),
            old_hash: "0".repeat(64),
            prev_evidence_bundle: None,
        });
    };
    let cur_ver = read_release_version(prev_path).ok_or_else(|| {
        install_err(
            CustomerReason::IdentityChanged,
            "NBD_INSTALL_REASON=INSTALLED_RELEASE_IDENTITY_INVALID",
        )
    })?;
    if req
        .running
        .as_ref()
        .is_some_and(|running| running != &cur_ver)
    {
        return Err(install_err(
            CustomerReason::IdentityChanged,
            "NBD_INSTALL_REASON=RUNNING_RELEASE_MISMATCH",
        ));
    }
    if let ReleaseOrder::Older | ReleaseOrder::Equal = release_is_newer(approved_ver, &cur_ver) {
        return Err(install_err(
            CustomerReason::IdentityChanged,
            format!("NBD_INSTALL_REASON=DOWNGRADE_REFUSED ({approved_ver} <= {cur_ver})"),
        ));
    }
    let prev_digest = observe_installed_input_bundle_digest(selector).ok_or_else(|| {
        install_err(
            CustomerReason::IdentityChanged,
            "NBD_INSTALL_REASON=INSTALLED_MANIFEST_DIGEST_MISSING",
        )
    })?;
    let ev = resolve_previous_evidence(req, env, bundle_dir, prev_path, &prev_digest)?;
    Ok(PriorInstallState {
        prior_root,
        prior_identity,
        action: PackageAction::Upgrade,
        old_release: cur_ver,
        old_hash: prev_digest,
        prev_evidence_bundle: Some(ev),
    })
}

#[allow(clippy::too_many_arguments)]
fn commit_release_custody(
    req: &InstallRequest,
    env: &InstallEnvironment,
    inputs: &ApplyInputs<'_>,
    candidate: &VerifiedCandidate,
    prior: &PriorInstallState,
    admitted: &super::package_transition::AdmittedPackage,
    pending: &PackagePending,
    prep_paths: &PreparePaths<'_>,
    selector: &Path,
    unit_host: &mut dyn UnitHost,
    boot_host: &impl BootPolicyHost,
    custody: &mut CommitCustodyState,
) -> Result<(Option<EnrollmentOutcome>, SelectorEffectRecord), CustomerError> {
    let prepared = prepare_admitted(
        admitted,
        pending,
        &candidate.declared,
        &candidate.signature,
        prep_paths,
    )?;
    if let Some(progress) = prep_paths.progress {
        progress.update_package(PACKAGE_COMMIT, inputs.approved_ver, None);
    }
    custody.prepared_release_loc =
        Some((prepared.tree.path().to_path_buf(), prepared.tree.identity()));
    let prev_ev = prior
        .prev_evidence_bundle
        .as_ref()
        .map(|(sig, tmpl, prev_sink)| PreviousEvidence {
            signature: sig,
            transformation: prev_sink.as_ref().map(|w| PreviousTransformation {
                role: "cascade.conf.example",
                template: tmpl.as_slice(),
                sink: w,
            }),
        });
    let intent = record_prepared_intent(
        admitted,
        pending,
        &candidate.declared,
        &prepared,
        &candidate.signature,
        prev_ev.as_ref(),
        selector,
    )?;
    let ts = env.utc_override.clone().unwrap_or_else(utc_timestamp_now);
    let sealed = finalize_and_seal_release(&intent, &candidate.sink, &ts)?;
    custody.unit_custody = Some(inspect_and_stage_units(
        sealed.root_path(),
        prior.prior_root.as_deref(),
        &env.product_root,
        &env.systemd_dir,
        &req.legacy_unit_approvals,
        unit_host,
    )?);
    let effect = publish_sealed_selector(&intent, &sealed)?;
    if let Some(uc) = custody.unit_custody.as_mut() {
        uc.reload_and_sync(unit_host)?;
    }
    custody.token_custody = Some(publish_activation_approval(
        &env.approval_dir,
        inputs.approved_ver,
        inputs.posture,
    )?);
    let enrollment = if req.enroll_default {
        Some(apply_default_enrollment(
            boot_host,
            &env.approval_dir,
            inputs.approved_ver,
        )?)
    } else {
        None
    };
    Ok((enrollment, effect))
}

#[allow(clippy::too_many_arguments)]
fn admit_apply_transaction<'a>(
    req: &InstallRequest,
    env: &InstallEnvironment,
    inputs: &ApplyInputs<'_>,
    candidate: &'a VerifiedCandidate,
    streamed: &'a StreamVerifiedBundle<'a>,
    prior: &PriorInstallState,
    unit_host: &mut dyn UnitHost,
    memory_obs: &mut dyn FnMut() -> Option<MemoryObservation>,
) -> Result<
    (
        super::package_transition::AdmittedPackage<'a>,
        PackagePending,
        (u64, u64),
    ),
    CustomerError,
> {
    let check_units = |host: &dyn UnitHost| {
        check_install_units(
            host,
            &candidate.bundle_dir,
            prior.prior_root.as_deref(),
            &env.product_root,
            &env.systemd_dir,
            &req.legacy_unit_approvals,
        )
    };
    check_units(unit_host)?;

    // Create the public selector's parent before its children. Implicit
    // ancestors of the confined installer paths are created owner-only.
    ensure_dir_no_follow(
        &env.product_root,
        0o755,
        "NBD_INSTALL_REASON=PRODUCT_ROOT_INVALID",
    )?;
    let releases_dir = ensure_dir_no_follow(
        &env.releases_dir(),
        0o755,
        "NBD_INSTALL_REASON=PRODUCT_ROOT_INVALID",
    )?;
    let _stage_dir = ensure_dir_no_follow(
        &env.stage_dir(),
        0o755,
        "NBD_INSTALL_REASON=STAGE_DIR_INVALID",
    )?;
    let releases_meta = releases_dir.metadata().map_err(|_| {
        fs_error(
            CustomerReason::ObservationUnavailable,
            "cannot inspect releases directory",
            CustomerState::UnknownBlocked,
        )
    })?;
    let pending = PackagePending {
        target_release: Some(inputs.approved_ver.to_owned()),
        action: prior.action,
        old_hash: prior.old_hash.clone(),
        new_hash: inputs.approved_sha.to_owned(),
        issued_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
    };
    let guarded_hooks = !env.systemd_dir.join(RETIRED_LIFECYCLE_UNIT).exists()
        || env.systemd_dir.join(CASCADE_UNIT).exists();
    let initial_state = if prior.prior_root.is_some() {
        CustomerState::InstalledOff
    } else {
        CustomerState::Absent
    };
    let mut observe = || {
        if env.require_root {
            verify_quiesced_install(env, prior, || check_units(unit_host))?;
        }
        Ok(PackageObservation {
            snapshot: LifecycleSnapshot {
                state: initial_state,
                owner: LifecycleOwnership::Owned,
                guarded_hooks,
            },
            release: ReleaseIdentity::new(&prior.old_release),
            old_hash: prior.old_hash.clone(),
            new_hash: inputs.approved_sha.to_owned(),
        })
    };
    let admitted = begin_verified_package_held(
        &env.transition,
        &pending,
        Some(streamed),
        Some(&candidate.signature),
        &mut observe,
        memory_obs,
    )?;
    Ok((
        admitted,
        pending,
        (releases_meta.dev(), releases_meta.ino()),
    ))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn execute_verified_apply(
    req: &InstallRequest,
    env: &InstallEnvironment,
    candidate: VerifiedCandidate,
    installer_lock: Option<&InstallerLock>,
    unit_host: &mut dyn UnitHost,
    boot_host: &impl BootPolicyHost,
    memory_obs: &mut dyn FnMut() -> Option<MemoryObservation>,
    stdout: &mut dyn Write,
) -> Result<CommittedInstall, CustomerError> {
    let inputs = validate_apply_inputs(req)?;
    let progress = installer_lock
        .filter(|_| env.require_root)
        .map(|_| OperationProgressGuard::start_package_install());
    if let Some(progress) = &progress {
        progress.update_package(PACKAGE_VERIFY, inputs.approved_ver, None);
    }
    let requirement = memory_policy();
    let streamed = candidate
        .declared
        .verify_streamed(
            MEMBER_LIMIT as u64,
            MAX_TOTAL_BUNDLE_BYTES,
            &requirement,
            memory_obs,
        )
        .ok_or_else(|| {
            fs_error(
                CustomerReason::ObservationUnavailable,
                "NBD_INSTALL_REASON=BUNDLE_STREAM_VERIFICATION_FAILED",
                CustomerState::InstalledOff,
            )
        })?;
    let selector = env.selector_path();
    let prior = inspect_prior_install_state(
        req,
        env,
        &candidate.bundle_dir,
        &selector,
        inputs.approved_ver,
    )?;
    if let Some(lock) = installer_lock {
        lock.record_package_plan(req.enroll_default)?;
    }
    let (admitted, pending, releases_id) = admit_apply_transaction(
        req, env, &inputs, &candidate, &streamed, &prior, unit_host, memory_obs,
    )?;

    let stage_root = env.stage_dir();
    let prep_paths = PreparePaths {
        stage: &stage_root,
        parent: &env.releases_dir(),
        parent_identity: releases_id,
        name: inputs.approved_ver,
        reserve_bytes: INSTALL_RESERVE_BYTES,
        progress: progress.as_ref(),
    };
    let mut custody = CommitCustodyState {
        unit_custody: None,
        token_custody: None,
        prepared_release_loc: None,
    };

    match commit_release_custody(
        req,
        env,
        &inputs,
        &candidate,
        &prior,
        &admitted,
        &pending,
        &prep_paths,
        &selector,
        unit_host,
        boot_host,
        &mut custody,
    ) {
        Ok((enroll_outcome, effect)) => {
            if let Some(lock) = installer_lock {
                let enrolled = matches!(enroll_outcome, Some(EnrollmentOutcome::Enrolled { .. }));
                lock.record_committed(admitted.guard(), &effect, enrolled)
                    .map_err(|error| {
                        recovery_err(format!("cannot retain committed runtime: {error}"))
                    })?;
            }
            let committed = finish_committed_apply(
                env,
                &prior,
                &selector,
                &stage_root,
                &admitted,
                &pending,
                inputs.approved_ver,
                inputs.posture,
                enroll_outcome,
                stdout,
            )?;
            if env.require_root
                && let Err(err) = super::install_units::update::arm_if_enrolled(
                    &env.releases_dir().join(inputs.approved_ver),
                    &env.default_policy,
                    env.owner_uid,
                    unit_host,
                )
            {
                let _ = writeln!(
                    stdout,
                    "NBD_INSTALL_WARNING=update-schedule: {}",
                    err.detail
                );
            }
            Ok(committed)
        }
        Err(err) => {
            match rollback_failed_apply(
                admitted.guard(),
                &pending,
                unit_host,
                custody.token_custody.as_mut(),
                custody.unit_custody.as_mut(),
                custody.prepared_release_loc,
                &selector,
                &stage_root,
            ) {
                Ok(()) => Err(CustomerError {
                    state: CustomerState::InstalledOff,
                    ..err
                }),
                Err(recovery_err) => Err(fs_error(
                    CustomerReason::RecoveryRequired,
                    format!("{}; {}", err.detail, recovery_err.detail),
                    CustomerState::RecoveryRequired,
                )),
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn finish_committed_apply(
    env: &InstallEnvironment,
    prior: &PriorInstallState,
    selector: &Path,
    stage_root: &Path,
    admitted: &super::package_transition::AdmittedPackage,
    pending: &PackagePending,
    approved_ver: &str,
    posture: Posture,
    enroll_outcome: Option<EnrollmentOutcome>,
    stdout: &mut dyn Write,
) -> Result<CommittedInstall, CustomerError> {
    if let (Some(prev_path), Some(prev_id)) = (prior.prior_root.as_deref(), prior.prior_identity)
        && let Err(err) = remove_owned_unselected_release(prev_path, prev_id, selector)
    {
        let _ = writeln!(
            stdout,
            "NBD_INSTALL_WARNING=prior-release-cleanup-failed: {err}"
        );
    }
    if let Err(err) = cleanup_owned_prior_approvals(&env.approval_dir, approved_ver) {
        let _ = writeln!(stdout, "{err}");
    }
    cleanup_staged_members(admitted.guard(), pending, stage_root)?;
    clear_all_package_journals(admitted.guard(), pending)?;
    let _ = std::marker::PhantomData::<SystemUnitHost>;
    Ok(CommittedInstall {
        release: approved_ver.to_owned(),
        selector: selector.to_path_buf(),
        posture,
        enroll_outcome,
    })
}

#[allow(clippy::too_many_arguments)]
fn rollback_failed_apply(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
    unit_host: &mut dyn UnitHost,
    token_custody: Option<&mut ApprovalTokenCustody>,
    unit_custody: Option<&mut UnitInstallCustody>,
    prepared_release_loc: Option<(PathBuf, (u64, u64))>,
    selector: &Path,
    stage_root: &Path,
) -> Result<(), CustomerError> {
    if let Some(tokens) = token_custody {
        tokens.rollback()?;
    }
    if let Some(units) = unit_custody {
        units.rollback(unit_host)?;
    }
    if inspect_effect_journal(guard.paths())?.is_some() {
        let _ = reconcile_selector(guard, pending, SelectorRecoveryAction::Rollback, None, None)?;
    }
    if let Some((path, id)) = prepared_release_loc {
        remove_owned_unselected_release(&path, id, selector)?;
    }
    cleanup_staged_members(guard, pending, stage_root)?;
    clear_all_package_journals(guard, pending)
}

pub(super) fn clear_all_package_journals(
    guard: &CustomerTransitionGuard,
    pending: &PackagePending,
) -> Result<(), CustomerError> {
    clear_selector_journals(guard, pending)?;
    clear_publication_journal(guard, pending)?;
    clear_tree_journal(guard, pending)?;
    clear_stage_journal(guard, pending)?;
    clear_pending(guard, pending)
}
