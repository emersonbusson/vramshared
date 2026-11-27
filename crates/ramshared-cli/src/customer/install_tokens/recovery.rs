// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/install_tokens/recovery.rs

//! Idempotent approval and enrollment effects for a proven package commit.

use std::ffi::OsStr;
use std::fs::File;
use std::path::Path;

use rustix::fs::{AtFlags, statat};

use super::super::transition::recovery_refusal;
use super::*;

fn owned_token(dir: &File, name: &str, body: &str) -> Result<bool, CustomerError> {
    let flags = AtFlags::SYMLINK_NOFOLLOW;
    let before = match statat(dir, name, flags) {
        Ok(stat) => stat,
        Err(rustix::io::Errno::NOENT) => return Ok(false),
        Err(error) => {
            return Err(recovery_refusal(format!(
                "cannot observe token {name}: {error}"
            )));
        }
    };
    if before.st_mode & 0o7777 != 0o400
        || read_bounded_regular_at(dir, OsStr::new(name), MAX_TOKEN_BYTES).as_deref()
            != Some(body.as_bytes())
    {
        return Err(recovery_refusal(format!(
            "token {name} differs from the package plan"
        )));
    }
    let after = statat(dir, name, flags)
        .map_err(|_| recovery_refusal(format!("token {name} disappeared")))?;
    let identity = (before.st_dev, before.st_ino);
    if (after.st_dev, after.st_ino) != identity {
        return Err(recovery_refusal(format!("token {name} was replaced")));
    }
    Ok(true)
}

pub(in crate::customer) fn recover_activation_approval(
    dir: &Path,
    release: &str,
    posture: Posture,
) -> Result<(), CustomerError> {
    if !valid_release_token_segment(release) {
        return Err(recovery_refusal("invalid release identity for approval"));
    }
    let token_dir = ensure_dir_no_follow(dir, 0o755, "cannot open recovery approval directory")?;
    let token = format!("activate-{release}.token");
    if !owned_token(&token_dir, &token, &approval_body(release, posture))? {
        publish_activation_approval(dir, release, posture)?;
    }
    Ok(())
}

pub(in crate::customer) fn recover_enrollment(
    host: &impl BootPolicyHost,
    approvals: &Path,
    release: &str,
) -> Result<(), CustomerError> {
    if !valid_release_token_segment(release) {
        return Err(recovery_refusal("invalid release identity for enrollment"));
    }
    let dir = ensure_dir_no_follow(approvals, 0o755, "cannot open recovery approval directory")?;
    let name = format!("wsl2-nbd-ack-{release}.token");
    let enrolled = if owned_token(&dir, &name, &acknowledgment_body(release))? {
        let policy = observe_boot_policy(host)?;
        let health = host
            .unit_is_enabled(HEALTH_UNIT)
            .map_err(recovery_refusal)?;
        (policy == BootPolicyObservation::On && health)
            || enable_enrolled_boot_or_rollback(host, release, policy, health, None)?
    } else {
        matches!(
            apply_default_enrollment(host, approvals, release)?,
            EnrollmentOutcome::Enrolled { .. }
        )
    };
    if !enrolled {
        return Err(recovery_refusal("package enrollment could not be restored"));
    }
    Ok(())
}
