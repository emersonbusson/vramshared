// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/install_tokens.rs
//! Transactional activation approval tokens, optional boot enrollment, and safe stale cleanup.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, Dir, FileType, statat, unlinkat};
use rustix::process::getpid;

use super::install_fs::{
    Identity, ensure_dir_no_follow, fs_error as token_error, place_atomic_regular_in_dir,
    read_bounded_regular_at, verify_and_unlink_regular_in_dir,
};
use super::service::{BootPolicy, BootPolicyPlan};
use super::transition_custody::open_directory_no_follow;
use super::{CustomerError, CustomerReason, CustomerState};
use crate::cascade::boot::{parse_scoped_approval, parse_wsl2_acceptance};
use crate::cascade::boot_policy_effect::{
    BootPolicyHost, BootPolicyObservation, CASCADE_UNIT, HEALTH_UNIT, HOST_GATE_UNIT,
    SUPERVISOR_UNIT, apply_boot_policy, observe_boot_policy,
};
use crate::cascade::posture::Posture;
use crate::readiness::Decision;

const MAX_TOKEN_BYTES: usize = 512;

mod recovery;
pub(super) use recovery::{recover_activation_approval, recover_enrollment};

pub(super) struct ApprovalTokenCustody {
    approval_dir: PathBuf,
    token_name: String,
    token_identity: Identity,
}

impl ApprovalTokenCustody {
    pub(super) fn rollback(&mut self) -> Result<(), CustomerError> {
        let dir = open_directory_no_follow(&self.approval_dir, false)?.ok_or_else(|| {
            token_error(
                CustomerReason::RecoveryRequired,
                "NBD_INSTALL_ROLLBACK=APPROVAL_RESTORE_FAILED: approval dir missing",
                CustomerState::RecoveryRequired,
            )
        })?;
        verify_and_unlink_regular_in_dir(
            &dir,
            self.token_name.as_str(),
            self.token_identity,
            "NBD_INSTALL_ROLLBACK=APPROVAL_RESTORE_FAILED: approval token replaced",
            "NBD_INSTALL_ROLLBACK=APPROVAL_RESTORE_FAILED",
            "cannot sync approval dir after token rollback",
        )
        .map(|_| ())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum EnrollmentOutcome {
    Enrolled { warnings: Vec<String> },
    Warned { warning: &'static str },
}

pub(super) fn valid_release_token_segment(release: &str) -> bool {
    !release.is_empty()
        && release.len() <= 128
        && release
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && !matches!(release, "." | "..")
}

pub(super) fn publish_activation_approval(
    approval_dir: &Path,
    release: &str,
    posture: Posture,
) -> Result<ApprovalTokenCustody, CustomerError> {
    if !valid_release_token_segment(release) {
        return Err(token_error(
            CustomerReason::UnsupportedSurface,
            "NBD_INSTALL_REASON=RELEASE_VERSION_INVALID",
            CustomerState::InstalledOff,
        ));
    }
    let body = approval_body(release, posture);
    let token_name = format!("activate-{release}.token");
    let dir = ensure_dir_no_follow(
        approval_dir,
        0o755,
        "NBD_INSTALL_REASON=APPROVAL_DIR_INVALID",
    )?;
    if statat(&dir, token_name.as_str(), AtFlags::SYMLINK_NOFOLLOW).is_ok() {
        return Err(token_error(
            CustomerReason::IdentityChanged,
            format!("NBD_INSTALL_REASON=APPROVAL_ALREADY_EXISTS ({token_name})"),
            CustomerState::InstalledOff,
        ));
    }
    let staging = format!(".{token_name}.{}.staging", getpid().as_raw_pid());
    let token_id = place_atomic_regular_in_dir(
        &dir,
        &token_name,
        &staging,
        body.as_bytes(),
        0o400,
        &format!("cannot publish token {token_name}"),
        "cannot sync approval directory",
    )?;
    Ok(ApprovalTokenCustody {
        approval_dir: approval_dir.to_path_buf(),
        token_name,
        token_identity: token_id,
    })
}

fn approval_body(release: &str, posture: Posture) -> String {
    let mode = match posture {
        Posture::Writeback => "writeback",
        Posture::Swap => "swap",
    };
    format!("activate:{release}:posture={mode}\n")
}

fn acknowledgment_body(release: &str) -> String {
    format!("accept-wsl2-nbd:{release}\n")
}

pub(super) fn cleanup_owned_prior_approvals(
    approval_dir: &Path,
    current_release: &str,
) -> Result<(), CustomerError> {
    cleanup_owned_tokens(
        approval_dir,
        "activate-",
        &format!("activate-{current_release}.token"),
        |body| parse_scoped_approval(body).is_ok(),
        "NBD_INSTALL_WARNING=stale-approval-cleanup-failed",
    )
}

pub(super) fn apply_default_enrollment(
    host: &impl BootPolicyHost,
    approval_dir: &Path,
    release: &str,
) -> Result<EnrollmentOutcome, CustomerError> {
    if !valid_release_token_segment(release) {
        return Err(token_error(
            CustomerReason::UnsupportedSurface,
            "NBD_INSTALL_REASON=RELEASE_VERSION_INVALID",
            CustomerState::InstalledOff,
        ));
    }
    let prior_policy = observe_boot_policy(host)?;
    let prior_health = host.unit_is_enabled(HEALTH_UNIT).map_err(|detail| {
        token_error(
            CustomerReason::ObservationUnavailable,
            format!("cannot observe {HEALTH_UNIT}: {detail}"),
            CustomerState::UnknownBlocked,
        )
    })?;

    let dir = ensure_dir_no_follow(
        approval_dir,
        0o755,
        "NBD_INSTALL_REASON=APPROVAL_DIR_INVALID",
    )?;
    let ack_name = format!("wsl2-nbd-ack-{release}.token");
    if statat(&dir, ack_name.as_str(), AtFlags::SYMLINK_NOFOLLOW).is_ok() {
        return Ok(EnrollmentOutcome::Warned {
            warning: "enrollment-acknowledgment",
        });
    }
    let ack_body = acknowledgment_body(release);
    let staging = format!(".{ack_name}.{}.staging", getpid().as_raw_pid());
    let ack_id = place_atomic_regular_in_dir(
        &dir,
        &ack_name,
        &staging,
        ack_body.as_bytes(),
        0o400,
        &format!("cannot publish token {ack_name}"),
        "cannot sync approval directory",
    )?;

    let acknowledgment = NewEnrollmentAcknowledgment {
        approval_dir,
        name: &ack_name,
        identity: ack_id,
    };
    if !enable_enrolled_boot_or_rollback(
        host,
        release,
        prior_policy,
        prior_health,
        Some(acknowledgment),
    )? {
        return Ok(EnrollmentOutcome::Warned {
            warning: "enrollment-boot-policy",
        });
    }

    let mut warnings = Vec::new();
    if cleanup_stale_ack_tokens(approval_dir, &ack_name).is_err() {
        warnings.push("stale-acknowledgment-cleanup-failed".to_owned());
    }
    Ok(EnrollmentOutcome::Enrolled { warnings })
}

struct NewEnrollmentAcknowledgment<'a> {
    approval_dir: &'a Path,
    name: &'a str,
    identity: Identity,
}

fn enable_enrolled_boot_or_rollback(
    host: &impl BootPolicyHost,
    release: &str,
    prior_policy: BootPolicyObservation,
    prior_health: bool,
    new_acknowledgment: Option<NewEnrollmentAcknowledgment<'_>>,
) -> Result<bool, CustomerError> {
    let plan = BootPolicyPlan {
        policy: BootPolicy::On,
        release_version: release.to_owned(),
        grant_id: Some(format!("install-enroll-{release}")),
        readiness: Decision::Ready,
        live_tier_untouched: true,
    };
    if let Err(err) = apply_boot_policy(&plan, host).and_then(|()| {
        host.enable_unit(HEALTH_UNIT).map_err(|detail| {
            token_error(
                CustomerReason::RecoveryRequired,
                format!("cannot enable {HEALTH_UNIT}: {detail}"),
                CustomerState::RecoveryRequired,
            )
        })
    }) {
        rollback_enrollment(host, prior_policy, prior_health, new_acknowledgment).map_err(
            |rb_err| {
                token_error(
                    CustomerReason::RecoveryRequired,
                    format!(
                        "{}; {}; NBD_INSTALL_ROLLBACK=ENROLLMENT_RESTORE_FAILED",
                        err.detail, rb_err.detail
                    ),
                    CustomerState::RecoveryRequired,
                )
            },
        )?;
        return Ok(false);
    }
    Ok(true)
}

fn cleanup_stale_ack_tokens(approval_dir: &Path, keep_name: &str) -> Result<(), CustomerError> {
    cleanup_owned_tokens(
        approval_dir,
        "wsl2-nbd-ack-",
        keep_name,
        |body| {
            let trimmed = body.trim();
            trimmed.strip_prefix("accept-wsl2-nbd:").is_some_and(|rel| {
                valid_release_token_segment(rel) && parse_wsl2_acceptance(trimmed, rel).is_ok()
            })
        },
        "stale-acknowledgment-cleanup-failed",
    )
}

fn rollback_enrollment(
    host: &impl BootPolicyHost,
    prior_policy: BootPolicyObservation,
    prior_health: bool,
    new_acknowledgment: Option<NewEnrollmentAcknowledgment<'_>>,
) -> Result<(), CustomerError> {
    restore_boot_set(host, prior_policy)?;
    if !prior_health {
        host.disable_unit(HEALTH_UNIT).map_err(|detail| {
            token_error(
                CustomerReason::RecoveryRequired,
                format!("cannot restore boot state for {HEALTH_UNIT}: {detail}"),
                CustomerState::RecoveryRequired,
            )
        })?;
    }
    let Some(acknowledgment) = new_acknowledgment else {
        return Ok(());
    };
    let dir = open_directory_no_follow(acknowledgment.approval_dir, false)?.ok_or_else(|| {
        token_error(
            CustomerReason::RecoveryRequired,
            "approval dir disappeared during enrollment rollback",
            CustomerState::RecoveryRequired,
        )
    })?;
    verify_and_unlink_regular_in_dir(
        &dir,
        acknowledgment.name,
        acknowledgment.identity,
        "acknowledgment token replaced before rollback",
        "cannot unlink acknowledgment token on rollback",
        "cannot sync approval dir after enrollment rollback",
    )
    .map(|_| ())
}

fn restore_boot_set(
    host: &impl BootPolicyHost,
    prior: BootPolicyObservation,
) -> Result<(), CustomerError> {
    let bits = prior.enablement();
    let pairs = [
        (CASCADE_UNIT, bits.cascade_enabled),
        (SUPERVISOR_UNIT, bits.supervisor_enabled),
        (HOST_GATE_UNIT, bits.host_gate_enabled),
    ];
    for (unit, should_enable) in pairs {
        let res = if should_enable {
            host.enable_unit(unit)
        } else {
            host.disable_unit(unit)
        };
        res.map_err(|detail| {
            token_error(
                CustomerReason::RecoveryRequired,
                format!("cannot restore boot state for {unit}: {detail}"),
                CustomerState::RecoveryRequired,
            )
        })?;
    }
    Ok(())
}

fn cleanup_err(tag: &str) -> CustomerError {
    token_error(
        CustomerReason::RecoveryRequired,
        tag,
        CustomerState::InstalledOff,
    )
}

fn cleanup_owned_tokens(
    approval_dir: &Path,
    prefix: &str,
    keep_name: &str,
    is_owned_body: impl Fn(&str) -> bool,
    err_tag: &str,
) -> Result<(), CustomerError> {
    let Some(dir) = open_directory_no_follow(approval_dir, false)? else {
        return Ok(());
    };
    let mut reader = Dir::read_from(&dir).map_err(|_| cleanup_err(err_tag))?;
    let mut candidates = Vec::new();
    while let Some(item) = reader.read() {
        let item = item.map_err(|_| cleanup_err(err_tag))?;
        let Ok(name) = item.file_name().to_str() else {
            continue;
        };
        if name == keep_name || !name.starts_with(prefix) || !name.ends_with(".token") {
            continue;
        }
        candidates.push(name.to_owned());
    }
    for name in candidates {
        let Ok(st) = statat(&dir, name.as_str(), AtFlags::SYMLINK_NOFOLLOW) else {
            continue;
        };
        if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
            continue;
        }
        let Some(bytes) = read_bounded_regular_at(&dir, OsStr::new(&name), MAX_TOKEN_BYTES) else {
            continue;
        };
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        if !is_owned_body(text) {
            continue;
        }
        unlinkat(&dir, name.as_str(), AtFlags::empty()).map_err(|_| cleanup_err(err_tag))?;
    }
    dir.sync_all().map_err(|_| cleanup_err(err_tag))
}

#[cfg(test)]
mod tests;
