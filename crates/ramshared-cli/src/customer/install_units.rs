// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/install_units.rs
//! Transactional systemd unit placement, hash-bound legacy backup, and rollback.

use std::ffi::OsStr;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use rustix::fs::{AtFlags, FileType, statat, unlinkat};
use rustix::process::getpid;

use super::install_fs::{
    Identity, check_legacy_root_absent_or_dir, digest, ensure_dir_no_follow,
    ensure_immutable_backup, finish_atomic_regular_in_dir, fs_error as unit_error,
    place_atomic_regular_in_dir, read_bounded_regular_at, verify_backed_up_regular,
    verify_regular_digest_in_dir, write_atomic_staging_in_dir,
};
use super::transition_custody::{open_directory_no_follow, validate_directory};
use super::{CustomerError, CustomerReason, CustomerState};
use crate::bounded_process::run_capture_command;
use crate::cascade::boot_policy_effect::{
    CASCADE_UNIT, HEALTH_UNIT, HOST_GATE_UNIT, SUPERVISOR_UNIT,
};
use crate::installed_image::valid_sha256;

mod active_slice;
pub(super) mod update;

pub(super) use active_slice::check_slice_before_drain;
use active_slice::preserves_active_workloads_slice;

pub(super) const WORKLOADS_SLICE_UNIT: &str = "ramshared-workloads.slice";
pub(super) const MANAGED_UNITS: [&str; 5] = [
    HOST_GATE_UNIT,
    SUPERVISOR_UNIT,
    CASCADE_UNIT,
    HEALTH_UNIT,
    WORKLOADS_SLICE_UNIT,
];
pub(super) const MAX_UNIT_FILE_BYTES: usize = 64 * 1024;
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LegacyUnitApproval {
    pub(crate) unit: Option<String>,
    pub(crate) sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct UnitRuntimeStatus {
    pub(super) active_state: String,
    pub(super) file_state: String,
}

pub(super) trait UnitHost {
    fn query_unit(&self, unit: &str) -> Result<UnitRuntimeStatus, CustomerError>;
    fn daemon_reload(&mut self) -> Result<(), CustomerError>;
    fn sync_selected_release(&mut self) -> Result<(), CustomerError>;
    fn enable_update_schedule(&mut self) -> Result<(), CustomerError> {
        Ok(())
    }
}

pub(super) struct SystemUnitHost;

fn run_systemctl_capture(
    args: &[&str],
    label: &str,
    reason: CustomerReason,
    state: CustomerState,
    err_prefix: &str,
) -> Result<Vec<u8>, CustomerError> {
    let mut cmd = Command::new(resolve_systemctl_bin());
    cmd.args(args);
    let output = run_capture_command(&mut cmd, label, SYSTEMCTL_TIMEOUT, 4096, |_| {})
        .map_err(|e| unit_error(reason, format!("{err_prefix}: {e}"), state))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        return Err(unit_error(
            reason,
            if detail.is_empty() {
                format!("{err_prefix}: systemctl exited nonzero")
            } else {
                format!("{err_prefix}: {detail}")
            },
            state,
        ));
    }
    Ok(output.stdout)
}

impl UnitHost for SystemUnitHost {
    fn query_unit(&self, unit: &str) -> Result<UnitRuntimeStatus, CustomerError> {
        let stdout = run_systemctl_capture(
            &[
                "show",
                "--all",
                "--property=ActiveState",
                "--property=UnitFileState",
                "--no-pager",
                "--",
                unit,
            ],
            "installer systemd unit status",
            CustomerReason::ObservationUnavailable,
            CustomerState::UnknownBlocked,
            &format!("NBD_INSTALL_REASON=PRODUCT_UNIT_ACTIVITY_UNKNOWN ({unit})"),
        )?;
        let text = std::str::from_utf8(&stdout).map_err(|_| {
            unit_error(
                CustomerReason::ObservationUnavailable,
                format!(
                    "NBD_INSTALL_REASON=PRODUCT_UNIT_ACTIVITY_UNKNOWN ({unit}): non-UTF-8 output"
                ),
                CustomerState::UnknownBlocked,
            )
        })?;
        parse_unit_status_output(text, unit)
    }

    fn daemon_reload(&mut self) -> Result<(), CustomerError> {
        run_systemctl_capture(
            &["daemon-reload"],
            "installer systemctl daemon-reload",
            CustomerReason::RecoveryRequired,
            CustomerState::RecoveryRequired,
            "NBD_INSTALL_REASON=SYSTEMD_DAEMON_RELOAD_FAILED",
        )
        .map(|_| ())
    }

    fn enable_update_schedule(&mut self) -> Result<(), CustomerError> {
        update::enable()
    }

    fn sync_selected_release(&mut self) -> Result<(), CustomerError> {
        if !Path::new("/run/systemd/system").exists() {
            return Ok(());
        }
        crate::release_coherence::sync_selected_release().ok_or_else(|| {
            unit_error(
                CustomerReason::RecoveryRequired,
                "NBD_INSTALL_REASON=RELEASE_SYNC_FAILED: post-switch release coherence sync failed",
                CustomerState::RecoveryRequired,
            )
        })?;
        Ok(())
    }
}

pub(super) struct UnitInstallHooks<'a> {
    pub after_legacy_backup: &'a mut dyn FnMut() -> Result<(), CustomerError>,
    pub after_unit_stage: &'a mut dyn FnMut(&str) -> Result<(), CustomerError>,
    pub after_unit_link: &'a mut dyn FnMut(&str) -> Result<(), CustomerError>,
}

#[derive(Clone)]
enum PriorUnitState {
    Absent,
    OwnedPrior {
        bytes: Vec<u8>,
    },
    Legacy {
        bytes: Vec<u8>,
        hash: String,
        backup_path: PathBuf,
        backup_identity: Identity,
    },
}

struct PlacedUnit {
    unit: &'static str,
    installed_identity: Identity,
    prior: PriorUnitState,
}

pub(super) struct UnitInstallCustody {
    systemd_dir: PathBuf,
    placed: Vec<PlacedUnit>,
    daemon_reloaded: bool,
}

impl UnitInstallCustody {
    pub(super) fn reload_and_sync(&mut self, host: &mut dyn UnitHost) -> Result<(), CustomerError> {
        host.daemon_reload()?;
        self.daemon_reloaded = true;
        host.sync_selected_release()
    }

    pub(super) fn rollback(&mut self, host: &mut dyn UnitHost) -> Result<(), CustomerError> {
        let dir = open_directory_no_follow(&self.systemd_dir, false)?.ok_or_else(|| {
            unit_error(
                CustomerReason::RecoveryRequired,
                "NBD_INSTALL_ROLLBACK=UNIT_RESTORE_FAILED: systemd dir missing",
                CustomerState::RecoveryRequired,
            )
        })?;
        validate_directory(&dir, "systemd unit directory")?;
        for item in self.placed.iter().rev() {
            rollback_one_unit(&dir, item)?;
        }
        self.placed.clear();
        if self.daemon_reloaded {
            host.daemon_reload()?;
            self.daemon_reloaded = false;
        }
        Ok(())
    }
}

pub(crate) fn parse_legacy_unit_approval(raw: &str) -> Result<LegacyUnitApproval, CustomerError> {
    let trimmed = raw.trim();
    if let Some((unit, digest_hex)) = trimmed.split_once(':') {
        if !(MANAGED_UNITS.contains(&unit) || update::UPDATE_UNITS.contains(&unit))
            || !valid_sha256(digest_hex)
        {
            return Err(unit_error(
                CustomerReason::UnsupportedSurface,
                format!("NBD_INSTALL_REASON=LEGACY_UNIT_APPROVAL_INVALID ({raw})"),
                CustomerState::InstalledOff,
            ));
        }
        return Ok(LegacyUnitApproval {
            unit: Some(unit.to_owned()),
            sha256: digest_hex.to_ascii_lowercase(),
        });
    }
    if !valid_sha256(trimmed) {
        return Err(unit_error(
            CustomerReason::UnsupportedSurface,
            format!("NBD_INSTALL_REASON=LEGACY_UNIT_APPROVAL_INVALID ({raw})"),
            CustomerState::InstalledOff,
        ));
    }
    Ok(LegacyUnitApproval {
        unit: None,
        sha256: trimmed.to_ascii_lowercase(),
    })
}

fn check_units_inactive_and_disabled(
    host: &dyn UnitHost,
    allow_active_slice: bool,
) -> Result<(), CustomerError> {
    for unit in MANAGED_UNITS {
        let status = host.query_unit(unit)?;
        match status.active_state.as_str() {
            "inactive" | "failed" => {}
            "active"
                if unit == WORKLOADS_SLICE_UNIT
                    && status.file_state == "static"
                    && allow_active_slice => {}
            "active" | "reloading" | "activating" | "deactivating" => {
                return Err(unit_error(
                    CustomerReason::ActiveSession,
                    format!(
                        "NBD_INSTALL_REASON=PRODUCT_UNIT_ACTIVE ({unit}: {})",
                        status.active_state
                    ),
                    CustomerState::Active,
                ));
            }
            _ => {
                return Err(unit_error(
                    CustomerReason::ObservationUnavailable,
                    format!(
                        "NBD_INSTALL_REASON=PRODUCT_UNIT_ACTIVITY_UNKNOWN ({unit}: {})",
                        status.active_state
                    ),
                    CustomerState::UnknownBlocked,
                ));
            }
        }
        match status.file_state.as_str() {
            "" | "disabled" | "masked" | "masked-runtime" | "static" | "indirect" | "generated"
            | "transient" | "not-found" | "bad" => {}
            "enabled" | "enabled-runtime" | "linked" | "linked-runtime" | "alias" => {
                return Err(unit_error(
                    CustomerReason::IdentityChanged,
                    format!(
                        "NBD_INSTALL_REASON=PRODUCT_UNIT_ENABLED ({unit}: {})",
                        status.file_state
                    ),
                    CustomerState::InstalledOff,
                ));
            }
            _ => {
                return Err(unit_error(
                    CustomerReason::ObservationUnavailable,
                    format!(
                        "NBD_INSTALL_REASON=PRODUCT_UNIT_ENABLEMENT_UNKNOWN ({unit}: {})",
                        status.file_state
                    ),
                    CustomerState::UnknownBlocked,
                ));
            }
        }
    }
    Ok(())
}

struct PlannedUnit {
    unit: &'static str,
    new_bytes: Vec<u8>,
    prior: PriorUnitSpec,
}

enum PriorUnitSpec {
    Absent,
    OwnedPrior(Vec<u8>),
    Legacy { bytes: Vec<u8>, hash: String },
}

pub(super) fn check_install_units(
    host: &dyn UnitHost,
    release_root: &Path,
    prior_release_root: Option<&Path>,
    product_root: &Path,
    systemd_dir: &Path,
    approvals: &[LegacyUnitApproval],
) -> Result<(), CustomerError> {
    let plans = plan_units(
        release_root,
        prior_release_root,
        product_root,
        systemd_dir,
        approvals,
    )?;
    check_units_inactive_and_disabled(host, preserves_active_workloads_slice(&plans))
}

pub(super) fn validate_unit_plan(
    release_root: &Path,
    prior_release_root: Option<&Path>,
    product_root: &Path,
    systemd_dir: &Path,
    approvals: &[LegacyUnitApproval],
) -> Result<(), CustomerError> {
    let _ = plan_units(
        release_root,
        prior_release_root,
        product_root,
        systemd_dir,
        approvals,
    )?;
    Ok(())
}

pub(super) fn inspect_and_stage_units(
    release_root: &Path,
    prior_release_root: Option<&Path>,
    product_root: &Path,
    systemd_dir: &Path,
    approvals: &[LegacyUnitApproval],
    host: &mut dyn UnitHost,
) -> Result<UnitInstallCustody, CustomerError> {
    let mut noop1 = || Ok(());
    let mut noop2 = |_: &str| Ok(());
    let mut noop3 = |_: &str| Ok(());
    let mut hooks = UnitInstallHooks {
        after_legacy_backup: &mut noop1,
        after_unit_stage: &mut noop2,
        after_unit_link: &mut noop3,
    };
    inspect_and_stage_units_with_hooks(
        release_root,
        prior_release_root,
        product_root,
        systemd_dir,
        approvals,
        host,
        &mut hooks,
    )
}

pub(super) fn inspect_and_stage_units_with_hooks(
    release_root: &Path,
    prior_release_root: Option<&Path>,
    product_root: &Path,
    systemd_dir: &Path,
    approvals: &[LegacyUnitApproval],
    host: &mut dyn UnitHost,
    hooks: &mut UnitInstallHooks<'_>,
) -> Result<UnitInstallCustody, CustomerError> {
    let plans = plan_units(
        release_root,
        prior_release_root,
        product_root,
        systemd_dir,
        approvals,
    )?;
    check_units_inactive_and_disabled(host, preserves_active_workloads_slice(&plans))?;
    let needs_legacy_dir = plans
        .iter()
        .any(|p| matches!(p.prior, PriorUnitSpec::Legacy { .. }));
    let legacy_dir = if needs_legacy_dir {
        Some(ensure_dir_no_follow(
            &product_root.join("legacy-units"),
            0o755,
            "NBD_INSTALL_REASON=LEGACY_BACKUP_ROOT_INVALID",
        )?)
    } else {
        None
    };

    let sys_fd =
        ensure_dir_no_follow(systemd_dir, 0o755, "NBD_INSTALL_REASON=SYSTEMD_DIR_INVALID")?;
    let mut custody = UnitInstallCustody {
        systemd_dir: systemd_dir.to_path_buf(),
        placed: Vec::with_capacity(plans.len()),
        daemon_reloaded: false,
    };
    let result = apply_planned_units(
        &sys_fd,
        legacy_dir.as_ref(),
        product_root,
        &plans,
        &mut custody,
        hooks,
    );
    match result {
        Ok(()) => Ok(custody),
        Err(err) => {
            if let Err(rb_err) = custody.rollback(host) {
                return Err(unit_error(
                    CustomerReason::RecoveryRequired,
                    format!(
                        "{}; rollback refused ({}); NBD_INSTALL_ROLLBACK=RELEASE_RETAINED",
                        err.detail, rb_err.detail
                    ),
                    CustomerState::RecoveryRequired,
                ));
            }
            Err(err)
        }
    }
}

fn apply_planned_units(
    sys_fd: &File,
    legacy_dir: Option<&File>,
    product_root: &Path,
    plans: &[PlannedUnit],
    custody: &mut UnitInstallCustody,
    hooks: &mut UnitInstallHooks<'_>,
) -> Result<(), CustomerError> {
    for item in plans {
        let prior_state = match &item.prior {
            PriorUnitSpec::Absent => PriorUnitState::Absent,
            PriorUnitSpec::OwnedPrior(bytes) => PriorUnitState::OwnedPrior {
                bytes: bytes.clone(),
            },
            PriorUnitSpec::Legacy { bytes, hash } => {
                let dir = legacy_dir.ok_or_else(|| {
                    unit_error(
                        CustomerReason::RecoveryRequired,
                        "NBD_INSTALL_REASON=LEGACY_BACKUP_ROOT_INVALID",
                        CustomerState::RecoveryRequired,
                    )
                })?;
                let bak_name = format!("{}.{hash}.bak", item.unit);
                let bak_id =
                    ensure_immutable_backup(dir, &bak_name, bytes, hash, MAX_UNIT_FILE_BYTES)?;
                (hooks.after_legacy_backup)()?;
                PriorUnitState::Legacy {
                    bytes: bytes.clone(),
                    hash: hash.clone(),
                    backup_path: product_root.join("legacy-units").join(bak_name),
                    backup_identity: bak_id,
                }
            }
        };
        let staging_name = format!(".{}.{}.staging", item.unit, getpid().as_raw_pid());
        let staged_id = write_atomic_staging_in_dir(sys_fd, &staging_name, &item.new_bytes, 0o644)?;
        (hooks.after_unit_stage)(item.unit)?;
        finish_atomic_regular_in_dir(
            sys_fd,
            item.unit,
            &staging_name,
            staged_id,
            0o644,
            &format!("NBD_INSTALL_REASON=UNIT_LINK_FAILED ({})", item.unit),
            "cannot sync systemd directory after unit install",
        )?;
        let live_id = verify_regular_digest_in_dir(
            sys_fd,
            OsStr::new(item.unit),
            &digest(&item.new_bytes),
            MAX_UNIT_FILE_BYTES,
        )?;
        if live_id != staged_id {
            return Err(unit_error(
                CustomerReason::RecoveryRequired,
                format!("NBD_INSTALL_REASON=UNIT_IDENTITY_MISMATCH ({})", item.unit),
                CustomerState::RecoveryRequired,
            ));
        }
        custody.placed.push(PlacedUnit {
            unit: item.unit,
            installed_identity: live_id,
            prior: prior_state,
        });
        (hooks.after_unit_link)(item.unit)?;
    }
    Ok(())
}

fn plan_units(
    release_root: &Path,
    prior_release_root: Option<&Path>,
    product_root: &Path,
    systemd_dir: &Path,
    approvals: &[LegacyUnitApproval],
) -> Result<Vec<PlannedUnit>, CustomerError> {
    let release_units_dir = open_directory_no_follow(&release_root.join("systemd"), false)?
        .ok_or_else(|| {
            unit_error(
                CustomerReason::UnsupportedSurface,
                "NBD_INSTALL_REASON=RELEASE_LAYOUT_INVALID: missing systemd unit directory",
                CustomerState::InstalledOff,
            )
        })?;
    validate_directory(&release_units_dir, "release systemd directory")?;

    let prior_units_dir = prior_release_root.and_then(|prior| {
        open_directory_no_follow(&prior.join("systemd"), false)
            .ok()
            .flatten()
    });
    let sys_dir_opt = open_directory_no_follow(systemd_dir, false)?;
    if systemd_dir.exists() && sys_dir_opt.is_none() {
        return Err(unit_error(
            CustomerReason::UnsupportedSurface,
            "NBD_INSTALL_REASON=SYSTEMD_DIR_INVALID",
            CustomerState::InstalledOff,
        ));
    }

    check_legacy_root_absent_or_dir(product_root)?;

    let update_group = update::group_for_install(&release_units_dir, prior_units_dir.as_ref())?;
    let mut out = Vec::with_capacity(MANAGED_UNITS.len() + update_group.len());
    for unit in MANAGED_UNITS
        .into_iter()
        .chain(update_group.iter().copied())
    {
        let new_bytes =
            read_bounded_regular_at(&release_units_dir, OsStr::new(unit), MAX_UNIT_FILE_BYTES)
                .ok_or_else(|| {
                    unit_error(
                        CustomerReason::UnsupportedSurface,
                        format!("NBD_INSTALL_REASON=RELEASE_LAYOUT_INVALID: missing {unit}"),
                        CustomerState::InstalledOff,
                    )
                })?;
        let prior = match &sys_dir_opt {
            None => PriorUnitSpec::Absent,
            Some(sys_dir) => {
                inspect_existing_unit(sys_dir, unit, prior_units_dir.as_ref(), approvals)?
            }
        };
        out.push(PlannedUnit {
            unit,
            new_bytes,
            prior,
        });
    }
    Ok(out)
}

fn legacy_unit_code(
    unit: &str,
    cascade_code: &'static str,
    aux_code: &'static str,
) -> &'static str {
    if unit == CASCADE_UNIT {
        cascade_code
    } else {
        aux_code
    }
}

fn inspect_existing_unit(
    sys_dir: &File,
    unit: &str,
    prior_units_dir: Option<&File>,
    approvals: &[LegacyUnitApproval],
) -> Result<PriorUnitSpec, CustomerError> {
    let st = match statat(sys_dir, unit, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => st,
        Err(rustix::io::Errno::NOENT) => return Ok(PriorUnitSpec::Absent),
        Err(_) => {
            return Err(unit_error(
                CustomerReason::ObservationUnavailable,
                format!("cannot stat installed unit {unit}"),
                CustomerState::UnknownBlocked,
            ));
        }
    };
    if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
        let code = legacy_unit_code(
            unit,
            "LEGACY_UNIT_APPROVAL_MISSING",
            "AUXILIARY_UNIT_CONFLICT",
        );
        return Err(unit_error(
            CustomerReason::IdentityChanged,
            format!("NBD_INSTALL_REASON={code}"),
            CustomerState::InstalledOff,
        ));
    }
    let current_bytes = read_bounded_regular_at(sys_dir, OsStr::new(unit), MAX_UNIT_FILE_BYTES)
        .ok_or_else(|| {
            unit_error(
                CustomerReason::ObservationUnavailable,
                format!("cannot read existing unit {unit}"),
                CustomerState::UnknownBlocked,
            )
        })?;
    if let Some(prior_dir) = prior_units_dir
        && let Some(prior_bytes) =
            read_bounded_regular_at(prior_dir, OsStr::new(unit), MAX_UNIT_FILE_BYTES)
        && prior_bytes == current_bytes
    {
        return Ok(PriorUnitSpec::OwnedPrior(current_bytes));
    }
    let old_hash = digest(&current_bytes);
    let approved = approvals.iter().find(|a| match &a.unit {
        Some(target) => target == unit,
        None => unit == CASCADE_UNIT,
    });
    let Some(approval) = approved else {
        let code = legacy_unit_code(
            unit,
            "LEGACY_UNIT_APPROVAL_MISSING",
            "AUXILIARY_UNIT_CONFLICT",
        );
        return Err(unit_error(
            CustomerReason::AuthorizationRequired,
            format!("NBD_INSTALL_REASON={code} ({unit} sha256={old_hash})"),
            CustomerState::InstalledOff,
        ));
    };
    if approval.sha256 != old_hash {
        let code = legacy_unit_code(
            unit,
            "LEGACY_UNIT_HASH_MISMATCH",
            "LEGACY_AUXILIARY_UNIT_HASH_MISMATCH",
        );
        return Err(unit_error(
            CustomerReason::IdentityChanged,
            format!(
                "NBD_INSTALL_REASON={code} ({unit}: expected {}, live {old_hash})",
                approval.sha256
            ),
            CustomerState::InstalledOff,
        ));
    }
    Ok(PriorUnitSpec::Legacy {
        bytes: current_bytes,
        hash: old_hash,
    })
}

fn rollback_one_unit(sys_dir: &File, item: &PlacedUnit) -> Result<(), CustomerError> {
    let st = statat(sys_dir, item.unit, AtFlags::SYMLINK_NOFOLLOW).map_err(|_| {
        unit_error(
            CustomerReason::RecoveryRequired,
            rollback_fail_tag(item.unit, &item.prior),
            CustomerState::RecoveryRequired,
        )
    })?;
    if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile
        || (st.st_dev, st.st_ino) != item.installed_identity
    {
        return Err(unit_error(
            CustomerReason::RecoveryRequired,
            rollback_fail_tag(item.unit, &item.prior),
            CustomerState::RecoveryRequired,
        ));
    }
    match &item.prior {
        PriorUnitState::Absent => {
            unlinkat(sys_dir, item.unit, AtFlags::empty()).map_err(|_| {
                unit_error(
                    CustomerReason::RecoveryRequired,
                    rollback_fail_tag(item.unit, &item.prior),
                    CustomerState::RecoveryRequired,
                )
            })?;
            sys_dir.sync_all().map_err(|_| {
                unit_error(
                    CustomerReason::RecoveryRequired,
                    "cannot sync systemd dir on rollback",
                    CustomerState::RecoveryRequired,
                )
            })
        }
        PriorUnitState::OwnedPrior { bytes } => replace_unit_bytes(sys_dir, item.unit, bytes),
        PriorUnitState::Legacy {
            bytes,
            hash,
            backup_path,
            backup_identity,
        } => {
            verify_backed_up_regular(
                backup_path,
                hash,
                *backup_identity,
                MAX_UNIT_FILE_BYTES,
                "NBD_INSTALL_ROLLBACK=LEGACY_UNIT_RESTORE_FAILED",
            )?;
            replace_unit_bytes(sys_dir, item.unit, bytes)
        }
    }
}

fn replace_unit_bytes(sys_dir: &File, unit: &str, bytes: &[u8]) -> Result<(), CustomerError> {
    let rb_staging = format!(".{unit}.rollback.{}.staging", getpid().as_raw_pid());
    place_atomic_regular_in_dir(
        sys_dir,
        unit,
        &rb_staging,
        bytes,
        0o644,
        &format!("NBD_INSTALL_ROLLBACK=UNIT_RESTORE_FAILED ({unit})"),
        "cannot sync systemd dir after restore",
    )
    .map(|_| ())
}

fn rollback_fail_tag(unit: &str, prior: &PriorUnitState) -> String {
    match prior {
        PriorUnitState::Legacy { .. } => {
            format!("NBD_INSTALL_ROLLBACK=LEGACY_UNIT_RESTORE_FAILED ({unit})")
        }
        _ if unit == CASCADE_UNIT => {
            format!("NBD_INSTALL_ROLLBACK=UNIT_RESTORE_FAILED ({unit})")
        }
        _ => format!("NBD_INSTALL_ROLLBACK=AUXILIARY_UNIT_RESTORE_FAILED ({unit})"),
    }
}

pub(super) fn parse_unit_status_output(
    text: &str,
    unit: &str,
) -> Result<UnitRuntimeStatus, CustomerError> {
    let mut active = None;
    let mut file_state = None;
    for line in text.lines() {
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        match key {
            "ActiveState" if active.is_none() => active = Some(val.to_owned()),
            "UnitFileState" if file_state.is_none() => file_state = Some(val.to_owned()),
            "ActiveState" | "UnitFileState" => {
                return Err(unit_error(
                    CustomerReason::ObservationUnavailable,
                    format!("NBD_INSTALL_REASON=PRODUCT_UNIT_ACTIVITY_UNKNOWN ({unit})"),
                    CustomerState::UnknownBlocked,
                ));
            }
            _ => {}
        }
    }
    let Some(active_state) = active else {
        return Err(unit_error(
            CustomerReason::ObservationUnavailable,
            format!("NBD_INSTALL_REASON=PRODUCT_UNIT_ACTIVITY_UNKNOWN ({unit})"),
            CustomerState::UnknownBlocked,
        ));
    };
    let Some(file_state) = file_state else {
        return Err(unit_error(
            CustomerReason::ObservationUnavailable,
            format!("NBD_INSTALL_REASON=PRODUCT_UNIT_ENABLEMENT_UNKNOWN ({unit})"),
            CustomerState::UnknownBlocked,
        ));
    };
    Ok(UnitRuntimeStatus {
        active_state,
        file_state,
    })
}

pub(super) fn resolve_systemctl_bin() -> &'static str {
    for candidate in ["/usr/bin/systemctl", "/bin/systemctl"] {
        if Path::new(candidate).exists() {
            return candidate;
        }
    }
    "systemctl"
}

#[cfg(test)]
mod tests;
