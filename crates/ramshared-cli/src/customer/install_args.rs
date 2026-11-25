// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/install_args.rs
//! Strict argv parser and structured status/plan formatters for `ramshared install`.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::install_fs::fs_error;
use super::install_lifecycle::ActivationOutcome;
use super::install_tokens::{EnrollmentOutcome, valid_release_token_segment};
use super::install_units::{LegacyUnitApproval, parse_legacy_unit_approval};
use super::package_selector::{SelectorRecoveryAction, SinkWitness};
use super::release_ordering::AutoUpdatePolicy;
use super::{CustomerError, CustomerReason, CustomerState};
use crate::cascade::posture::{Posture, parse_binding};
use crate::installed_image::valid_sha256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AutoUpdateMode {
    Status,
    Check,
    Apply,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum InstallMode {
    Plan,
    Apply,
    AutoUpdate(AutoUpdateMode),
    Reconcile(SelectorRecoveryAction),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct InstallRequest {
    pub(super) mode: InstallMode,
    pub(super) approved_version: Option<String>,
    pub(super) approved_manifest_sha256: Option<String>,
    pub(super) lower_sink: Option<String>,
    pub(super) posture: Option<Posture>,
    pub(super) enroll_default: bool,
    pub(super) no_enroll: bool,
    pub(super) no_activate: bool,
    pub(super) bundle_root: Option<PathBuf>,
    pub(super) target_release: Option<String>,
    pub(super) running: Option<String>,
    pub(super) previous_bundle: Option<PathBuf>,
    pub(super) keyring: Option<PathBuf>,
    pub(super) policy: Option<PathBuf>,
    pub(super) legacy_unit_approvals: Vec<LegacyUnitApproval>,
    pub(super) json: bool,
}

fn bad_arg(detail: impl Into<String>) -> CustomerError {
    fs_error(
        CustomerReason::UnsupportedSurface,
        detail,
        CustomerState::InstalledOff,
    )
}

fn take_value<'a>(
    args: &'a [String],
    index: &mut usize,
    flag: &str,
) -> Result<&'a str, CustomerError> {
    let Some(val) = args.get(*index + 1) else {
        return Err(bad_arg(format!(
            "NBD_INSTALL_REASON=MISSING_VALUE ({flag})"
        )));
    };
    *index += 1;
    Ok(val.as_str())
}

fn set_once<T>(slot: &mut Option<T>, flag: &str, value: T) -> Result<(), CustomerError> {
    if slot.is_some() {
        return Err(bad_arg(format!(
            "NBD_INSTALL_REASON=DUPLICATE_OPTION ({flag})"
        )));
    }
    *slot = Some(value);
    Ok(())
}

fn valid_safe_abs_path(raw: &str, flag: &str) -> Result<PathBuf, CustomerError> {
    let p = Path::new(raw);
    if raw.is_empty()
        || !p.is_absolute()
        || p.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::CurDir | Component::Prefix(_)
            )
        })
    {
        return Err(bad_arg(format!(
            "NBD_INSTALL_REASON=UNSAFE_PATH ({flag}: {raw})"
        )));
    }
    Ok(p.to_path_buf())
}

#[derive(Default)]
struct ArgState {
    plan_flag: bool,
    auto_flag: bool,
    check_flag: bool,
    status_flag: bool,
    reconcile_rollback: bool,
    reconcile_complete: bool,
    enroll_default: bool,
    no_enroll: bool,
    no_activate: bool,
    json: bool,
    approved_version: Option<String>,
    approved_manifest_sha256: Option<String>,
    lower_sink: Option<String>,
    posture: Option<Posture>,
    bundle_root: Option<PathBuf>,
    target_release: Option<String>,
    running: Option<String>,
    previous_bundle: Option<PathBuf>,
    keyring: Option<PathBuf>,
    policy: Option<PathBuf>,
    legacy_unit_approvals: Vec<LegacyUnitApproval>,
    seen_legacy_targets: BTreeSet<String>,
}

fn set_flag(slot: &mut bool, flag: &str) -> Result<(), CustomerError> {
    if *slot {
        return Err(bad_arg(format!(
            "NBD_INSTALL_REASON=DUPLICATE_OPTION ({flag})"
        )));
    }
    *slot = true;
    Ok(())
}

impl ArgState {
    fn try_bool_flag(&mut self, arg: &str) -> Result<bool, CustomerError> {
        let slot = match arg {
            "--plan" => &mut self.plan_flag,
            "--auto-update" => &mut self.auto_flag,
            "--check" => &mut self.check_flag,
            "--status" => &mut self.status_flag,
            "--reconcile-rollback" => &mut self.reconcile_rollback,
            "--reconcile-complete" => &mut self.reconcile_complete,
            "--enroll-default" => &mut self.enroll_default,
            "--no-enroll" => &mut self.no_enroll,
            "--no-activate" => &mut self.no_activate,
            "--json" => &mut self.json,
            _ => return Ok(false),
        };
        set_flag(slot, arg)?;
        Ok(true)
    }

    fn apply_option(&mut self, args: &[String], index: &mut usize) -> Result<(), CustomerError> {
        let flag = args[*index].as_str();
        if self.try_bool_flag(flag)? {
            return Ok(());
        }
        let known_value = matches!(
            flag,
            "--approve-nbd-product-install"
                | "--approve-manifest-sha256"
                | "--lower-sink"
                | "--posture"
                | "--bundle-root"
                | "--target-release"
                | "--running"
                | "--previous-bundle"
                | "--keyring"
                | "--policy"
                | "--approve-legacy-unit-replacement"
                | "--approve-legacy-auxiliary-unit-replacement"
        );
        if !known_value {
            return Err(bad_arg(format!(
                "NBD_INSTALL_REASON=UNSUPPORTED_OPTION ({flag})"
            )));
        }
        let val = take_value(args, index, flag)?;
        match flag {
            "--approve-nbd-product-install" => {
                if !valid_release_token_segment(val) {
                    return Err(bad_arg("NBD_INSTALL_REASON=RELEASE_VERSION_INVALID"));
                }
                set_once(&mut self.approved_version, flag, val.to_owned())?;
            }
            "--approve-manifest-sha256" => {
                if !valid_sha256(val) {
                    return Err(bad_arg(
                        "NBD_INSTALL_REASON=APPROVED_MANIFEST_SHA256_INVALID",
                    ));
                }
                set_once(
                    &mut self.approved_manifest_sha256,
                    flag,
                    val.to_ascii_lowercase(),
                )?;
            }
            "--lower-sink" => {
                let _ = valid_safe_abs_path(val, flag)?;
                set_once(&mut self.lower_sink, flag, val.to_owned())?;
            }
            "--posture" => {
                let parsed = parse_binding(val)
                    .map_err(|e| bad_arg(format!("NBD_INSTALL_REASON=POSTURE_INVALID: {e}")))?;
                set_once(&mut self.posture, flag, parsed)?;
            }
            "--bundle-root" => {
                set_once(&mut self.bundle_root, flag, valid_safe_abs_path(val, flag)?)?
            }
            "--target-release" | "--running" => {
                if !valid_release_token_segment(val) {
                    return Err(bad_arg("NBD_INSTALL_REASON=RELEASE_VERSION_INVALID"));
                }
                let target = if flag == "--target-release" {
                    &mut self.target_release
                } else {
                    &mut self.running
                };
                set_once(target, flag, val.to_owned())?;
            }
            "--previous-bundle" => {
                set_once(
                    &mut self.previous_bundle,
                    flag,
                    valid_safe_abs_path(val, flag)?,
                )?;
            }
            "--keyring" => set_once(&mut self.keyring, flag, valid_safe_abs_path(val, flag)?)?,
            "--policy" => set_once(&mut self.policy, flag, valid_safe_abs_path(val, flag)?)?,
            "--approve-legacy-unit-replacement" | "--approve-legacy-auxiliary-unit-replacement" => {
                let parsed = parse_legacy_unit_approval(val)?;
                if flag == "--approve-legacy-auxiliary-unit-replacement" && parsed.unit.is_none() {
                    return Err(bad_arg(
                        "NBD_INSTALL_REASON=LEGACY_AUXILIARY_APPROVAL_NEEDS_UNIT",
                    ));
                }
                let key = parsed.unit.clone().unwrap_or_default();
                if !self.seen_legacy_targets.insert(key) {
                    return Err(bad_arg(format!(
                        "NBD_INSTALL_REASON=DUPLICATE_OPTION ({flag})"
                    )));
                }
                self.legacy_unit_approvals.push(parsed);
            }
            other => {
                return Err(bad_arg(format!(
                    "NBD_INSTALL_REASON=UNSUPPORTED_OPTION ({other})"
                )));
            }
        }
        Ok(())
    }

    fn finish(self) -> Result<InstallRequest, CustomerError> {
        if self.enroll_default && self.no_enroll {
            return Err(bad_arg("NBD_INSTALL_REASON=ENROLL_CONFLICT"));
        }
        let any_auto = self.auto_flag || self.check_flag || self.status_flag;
        let any_reconcile = self.reconcile_rollback || self.reconcile_complete;
        let auto_modes = u8::from(self.check_flag) + u8::from(self.status_flag);
        if auto_modes > 1
            || (self.reconcile_rollback && self.reconcile_complete)
            || (self.plan_flag && (self.approved_version.is_some() || any_auto || any_reconcile))
            || (self.approved_version.is_some() && (any_auto || any_reconcile))
            || (any_reconcile && any_auto)
        {
            return Err(bad_arg("NBD_INSTALL_REASON=CONFLICTING_MODES"));
        }
        let mode = if self.reconcile_rollback {
            InstallMode::Reconcile(SelectorRecoveryAction::Rollback)
        } else if self.reconcile_complete {
            InstallMode::Reconcile(SelectorRecoveryAction::Complete)
        } else if self.status_flag {
            InstallMode::AutoUpdate(AutoUpdateMode::Status)
        } else if self.check_flag {
            InstallMode::AutoUpdate(AutoUpdateMode::Check)
        } else if self.auto_flag {
            InstallMode::AutoUpdate(AutoUpdateMode::Apply)
        } else if self.approved_version.is_some() {
            InstallMode::Apply
        } else {
            InstallMode::Plan
        };
        if matches!(mode, InstallMode::Apply) {
            if self.approved_manifest_sha256.is_none() {
                return Err(bad_arg(
                    "NBD_INSTALL_REASON=APPROVED_MANIFEST_SHA256_MISSING",
                ));
            }
            if self.lower_sink.is_none() {
                return Err(bad_arg("NBD_INSTALL_REASON=LOWER_SINK_MISSING"));
            }
            if self.posture.is_none() {
                return Err(bad_arg("NBD_INSTALL_REASON=POSTURE_MISSING"));
            }
        }
        Ok(InstallRequest {
            mode,
            approved_version: self.approved_version,
            approved_manifest_sha256: self.approved_manifest_sha256,
            lower_sink: self.lower_sink,
            posture: self.posture,
            enroll_default: self.enroll_default,
            no_enroll: self.no_enroll,
            no_activate: self.no_activate,
            bundle_root: self.bundle_root,
            target_release: self.target_release,
            running: self.running,
            previous_bundle: self.previous_bundle,
            keyring: self.keyring,
            policy: self.policy,
            legacy_unit_approvals: self.legacy_unit_approvals,
            json: self.json,
        })
    }
}

pub(super) fn parse_install_args(args: &[String]) -> Result<InstallRequest, CustomerError> {
    let mut state = ArgState::default();
    let mut i = 0;
    while i < args.len() {
        state.apply_option(args, &mut i)?;
        i += 1;
    }
    state.finish()
}

pub(super) fn write_plan_output(
    release: &str,
    target: &Path,
    selector: &Path,
    running: Option<&str>,
    sink: Option<&SinkWitness>,
    json: bool,
    stdout: &mut dyn Write,
) {
    if json {
        let doc = serde_json::json!({
            "state": "plan",
            "release": release,
            "target": target.display().to_string(),
            "selector": selector.display().to_string(),
            "running": running,
            "lower_sink_bound": sink.is_some(),
        });
        let _ = writeln!(stdout, "{doc}");
        return;
    }
    let _ = writeln!(stdout, "NBD_INSTALL_STATE=PLAN");
    let _ = writeln!(stdout, "NBD_INSTALL_RELEASE={release}");
    let _ = writeln!(stdout, "NBD_INSTALL_TARGET={}", target.display());
    let _ = writeln!(stdout, "NBD_INSTALL_SELECTOR={}", selector.display());
}

pub(super) fn write_auto_update_status(
    policy_path: &Path,
    policy: Option<&AutoUpdatePolicy>,
    running: Option<&str>,
    posture: Option<Posture>,
    newest_bundle: Option<&Path>,
    json: bool,
    stdout: &mut dyn Write,
) {
    let posture_str = posture.map(|p| match p {
        Posture::Writeback => "writeback",
        Posture::Swap => "swap",
    });
    if json {
        let doc = serde_json::json!({
            "policy": policy_path.display().to_string(),
            "enabled": policy.map(|p| p.enabled),
            "lower_sink": policy.and_then(|p| p.lower_sink.as_ref()).map(|p| p.display().to_string()),
            "bundle_root": policy.and_then(|p| p.bundle_root.as_ref()).map(|p| p.display().to_string()),
            "release_keyring": policy.and_then(|p| p.release_keyring.as_ref()).map(|p| p.display().to_string()),
            "running": running,
            "posture": posture_str,
            "newest_bundle": newest_bundle.map(|p| p.display().to_string()),
        });
        let _ = writeln!(stdout, "{doc}");
        return;
    }
    let _ = writeln!(stdout, "policy: {}", policy_path.display());
    if let Some(p) = policy {
        let _ = writeln!(stdout, "enabled: {}", u8::from(p.enabled));
        let _ = writeln!(
            stdout,
            "lower_sink: {}",
            p.lower_sink
                .as_ref()
                .map_or_else(|| "absent".to_owned(), |v| v.display().to_string())
        );
        let _ = writeln!(
            stdout,
            "bundle_root: {}",
            p.bundle_root
                .as_ref()
                .map_or_else(|| "absent".to_owned(), |v| v.display().to_string())
        );
        let _ = writeln!(
            stdout,
            "release_keyring: {}",
            p.release_keyring
                .as_ref()
                .map_or_else(|| "absent".to_owned(), |v| v.display().to_string())
        );
    } else {
        let _ = writeln!(stdout, "enabled: no policy file — auto-update is refused");
    }
    let _ = writeln!(
        stdout,
        "running: {}",
        running.unwrap_or("no installed release")
    );
    let _ = writeln!(stdout, "posture: {}", posture_str.unwrap_or("unavailable"));
    let _ = writeln!(
        stdout,
        "newest bundle: {}",
        newest_bundle.map_or_else(|| "none".to_owned(), |p| p.display().to_string())
    );
}

pub(super) fn write_commit_output(
    release: &str,
    selector: &Path,
    posture: Posture,
    enroll: Option<&EnrollmentOutcome>,
    activation: &ActivationOutcome,
    json: bool,
    stdout: &mut dyn Write,
) {
    let enabled = matches!(enroll, Some(EnrollmentOutcome::Enrolled { .. }));
    let posture_str = match posture {
        Posture::Writeback => "writeback",
        Posture::Swap => "swap",
    };
    if json {
        let activation_json = match activation {
            ActivationOutcome::Off => serde_json::json!({ "state": "off" }),
            ActivationOutcome::Active => serde_json::json!({ "state": "active" }),
            ActivationOutcome::Refused { unit, detail } => serde_json::json!({
                "state": "refused",
                "unit": unit,
                "detail": detail,
            }),
        };
        let warnings: Vec<&str> = match enroll {
            Some(EnrollmentOutcome::Warned { warning }) => vec![*warning],
            Some(EnrollmentOutcome::Enrolled { warnings }) => {
                warnings.iter().map(String::as_str).collect()
            }
            None => Vec::new(),
        };
        let doc = serde_json::json!({
            "state": "installed",
            "release": release,
            "selector": selector.display().to_string(),
            "posture": posture_str,
            "enabled": enabled,
            "activation": activation_json,
            "warnings": warnings,
        });
        let _ = writeln!(stdout, "{doc}");
        return;
    }
    if let Some(EnrollmentOutcome::Warned { warning }) = enroll {
        let _ = writeln!(stdout, "NBD_INSTALL_WARNING={warning}");
    }
    if let Some(EnrollmentOutcome::Enrolled { warnings }) = enroll {
        for warning in warnings {
            let _ = writeln!(stdout, "NBD_INSTALL_WARNING={warning}");
        }
    }
    let _ = writeln!(stdout, "NBD_INSTALL_STATE=INSTALLED");
    let _ = writeln!(stdout, "NBD_INSTALL_RELEASE={release}");
    let _ = writeln!(stdout, "NBD_INSTALL_SELECTOR={}", selector.display());
    let _ = writeln!(stdout, "NBD_INSTALL_POSTURE={posture_str}");
    let _ = writeln!(stdout, "NBD_INSTALL_ENABLED={}", u8::from(enabled));
    match activation {
        ActivationOutcome::Off => {
            let _ = writeln!(stdout, "NBD_INSTALL_ACTIVATION=OFF");
        }
        ActivationOutcome::Active => {
            let _ = writeln!(stdout, "NBD_INSTALL_ACTIVATION=ACTIVE");
        }
        ActivationOutcome::Refused { unit, detail } => {
            let _ = writeln!(stdout, "NBD_INSTALL_ACTIVATION=REFUSED ({unit}: {detail})");
        }
    }
}

pub(super) fn utc_timestamp_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format_utc_timestamp(secs)
}

pub(super) fn format_utc_timestamp(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let rem = unix_secs % 86_400;
    let hour = rem / 3_600;
    let minute = (rem % 3_600) / 60;
    let second = rem % 60;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}
