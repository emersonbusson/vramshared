// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/package_hook.rs
//! Native DT-6 package-lifecycle admission gate (`ramshared package check-quiescent`).

use std::io::Write;
use std::path::{Path, PathBuf};

use rustix::process::geteuid;

use super::install_fs::fs_error;
use super::install_units::{MANAGED_UNITS, SystemUnitHost, UnitHost, UnitRuntimeStatus};
use super::package::{MAX_PAYLOAD_MANIFEST_BYTES, PayloadManifest, RETIRED_LIFECYCLE_UNIT};
use super::package_admission::{LifecycleOwnership, LifecycleSnapshot, admit_package};
use super::package_prepare::PreparePaths;
use super::package_publish::publication_journal_present;
use super::package_selector::journal_present as selector_journal_present;
use super::package_stage_journal::stage_journal_present;
use super::package_tree::tree_journal_present;
use super::plan::ReleaseIdentity;
use super::transition::{
    OperationOwner, PackageAction, TransitionPaths, acquire_transition, read_pending,
};
use super::{CustomerError, CustomerReason, CustomerState};
use crate::cascade::boot_policy_effect::CASCADE_UNIT;
use crate::installed_image::verify_release_manifest;
use crate::release_identity::read_release_version;

const HOOK_CANDIDATE_PATHS: [&str; 2] = [
    "/usr/lib/ramshared/package-lifecycle-hook.sh",
    "/var/lib/dpkg/info/ramshared.prerm",
];

const VENDOR_SYSTEMD_DIRS: [&str; 2] = ["/usr/lib/systemd/system", "/lib/systemd/system"];
const BUILD_IDENTITY_PATH: &str = "/usr/share/doc/ramshared/BUILD-IDENTITY";
const MAX_BUILD_IDENTITY_BYTES: u64 = 4096;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PackageCheckRequest {
    pub(super) action: PackageAction,
    pub(super) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ObservedHostPackageState {
    pub(super) snapshot: LifecycleSnapshot,
    pub(super) release: ReleaseIdentity,
}

pub(super) trait PackageHost {
    fn transitions(&self) -> &TransitionPaths;
    fn require_root(&self) -> Result<(), CustomerError>;
    fn observe(&self) -> ObservedHostPackageState;
}

pub(super) struct SystemPackageHost {
    transitions: TransitionPaths,
    product_root: PathBuf,
    systemd_dirs: Vec<PathBuf>,
    build_identity_path: PathBuf,
}

impl SystemPackageHost {
    fn production() -> Self {
        let mut systemd_dirs = vec![PathBuf::from("/etc/systemd/system")];
        systemd_dirs.extend(VENDOR_SYSTEMD_DIRS.iter().map(PathBuf::from));
        Self {
            transitions: TransitionPaths::production(),
            product_root: PathBuf::from(crate::installed_image::PRODUCT_ROOT),
            systemd_dirs,
            build_identity_path: PathBuf::from(BUILD_IDENTITY_PATH),
        }
    }
}

impl PackageHost for SystemPackageHost {
    fn transitions(&self) -> &TransitionPaths {
        &self.transitions
    }

    fn require_root(&self) -> Result<(), CustomerError> {
        if geteuid().as_raw() != 0 {
            return Err(fs_error(
                CustomerReason::AuthorizationRequired,
                "package check-quiescent requires root privileges",
                CustomerState::InstalledOff,
            ));
        }
        Ok(())
    }

    fn observe(&self) -> ObservedHostPackageState {
        observe_live_package_state_in(
            &self.transitions,
            &self.product_root,
            &self.systemd_dirs,
            &self.build_identity_path,
            &SystemUnitHost,
            &HOOK_CANDIDATE_PATHS,
            Path::new("/proc/swaps"),
            Path::new("/run/ramshared"),
        )
    }
}

/// Top-level `ramshared package` dispatch.
pub(crate) fn run(args: &[String], stdout: &mut dyn Write, stderr: &mut dyn Write) -> u8 {
    let host = SystemPackageHost::production();
    run_with(args, &host, stdout, stderr)
}

pub(super) fn run_with(
    args: &[String],
    host: &dyn PackageHost,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> u8 {
    let req = match parse_package_args(args) {
        Ok(req) => req,
        Err(err) => {
            let _ = writeln!(stderr, "{err}");
            return err.exit_code();
        }
    };
    if let Err(err) = host.require_root() {
        let _ = writeln!(stderr, "{err}");
        return err.exit_code();
    }
    match execute_check_quiescent(&req, host) {
        Ok((obs, plan)) => {
            if req.json {
                let doc = serde_json::json!({
                    "action": plan.action.as_str(),
                    "state": plan.observed_state.as_str(),
                    "quiescent": true,
                    "old_version": plan.old_version,
                    "guarded_hooks": obs.snapshot.guarded_hooks,
                    "live_tier_untouched": true,
                });
                let _ = writeln!(stdout, "{doc}");
            } else {
                let _ = writeln!(
                    stdout,
                    "quiescent: action={} state={} version={}",
                    plan.action.as_str(),
                    plan.observed_state.as_str(),
                    plan.old_version
                );
            }
            0
        }
        Err(err) => {
            let _ = writeln!(stderr, "{err}");
            err.exit_code()
        }
    }
}

pub(super) fn parse_package_args(args: &[String]) -> Result<PackageCheckRequest, CustomerError> {
    let bad = |detail: &str| {
        fs_error(
            CustomerReason::UnsupportedSurface,
            detail,
            CustomerState::InstalledOff,
        )
    };
    let Some(verb) = args.first() else {
        return Err(bad("expected `ramshared package check-quiescent`"));
    };
    if verb != "check-quiescent" {
        return Err(bad(&format!("unsupported package subcommand: {verb}")));
    }
    let mut action: Option<PackageAction> = None;
    let mut json = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--action" if action.is_none() => {
                let Some(val) = args.get(i + 1) else {
                    return Err(bad("--action requires `upgrade` or `remove`"));
                };
                action = Some(match val.as_str() {
                    "upgrade" => PackageAction::Upgrade,
                    "remove" => PackageAction::Remove,
                    other => {
                        return Err(bad(&format!(
                            "unsupported package action for check-quiescent: {other}"
                        )));
                    }
                });
                i += 1;
            }
            "--action" => return Err(bad("duplicate --action option")),
            "--json" if !json => json = true,
            "--json" => return Err(bad("duplicate --json option")),
            other => return Err(bad(&format!("unsupported package option: {other}"))),
        }
        i += 1;
    }
    let Some(action) = action else {
        return Err(bad("--action <upgrade|remove> is required"));
    };
    Ok(PackageCheckRequest { action, json })
}

fn execute_check_quiescent(
    req: &PackageCheckRequest,
    host: &dyn PackageHost,
) -> Result<
    (
        ObservedHostPackageState,
        super::package_admission::PackagePlan,
    ),
    CustomerError,
> {
    let first = host.observe();
    let _ = admit_package(req.action, &first.snapshot, &first.release)?;
    let _guard = acquire_transition(OperationOwner::Package, host.transitions())?;
    let locked = host.observe();
    if locked != first {
        return Err(fs_error(
            CustomerReason::IdentityChanged,
            "package host observation changed while acquiring transition guard",
            CustomerState::InstalledOff,
        ));
    }
    let plan = admit_package(req.action, &locked.snapshot, &locked.release)?;
    // Re-touch PreparePaths field names to preserve shared layout compilation check.
    let _ = std::marker::PhantomData::<PreparePaths<'_>>;
    Ok((locked, plan))
}

fn blocked_state(
    state: CustomerState,
    owner: LifecycleOwnership,
    release: &str,
) -> ObservedHostPackageState {
    ObservedHostPackageState {
        snapshot: LifecycleSnapshot {
            state,
            owner,
            guarded_hooks: false,
        },
        release: ReleaseIdentity::new(release),
    }
}

#[cfg(test)]
pub(super) fn observe_live_package_state(
    transitions: &TransitionPaths,
    product_root: &Path,
    systemd_dir: &Path,
    unit_host: &dyn UnitHost,
    hook_paths: &[&str],
    swaps_path: &Path,
    runtime_dir: &Path,
) -> ObservedHostPackageState {
    observe_live_package_state_in(
        transitions,
        product_root,
        &[systemd_dir.to_path_buf()],
        &product_root.join("__no_build_identity__"),
        unit_host,
        hook_paths,
        swaps_path,
        runtime_dir,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn observe_live_package_state_in(
    transitions: &TransitionPaths,
    product_root: &Path,
    systemd_dirs: &[PathBuf],
    build_identity_path: &Path,
    unit_host: &dyn UnitHost,
    hook_paths: &[&str],
    swaps_path: &Path,
    runtime_dir: &Path,
) -> ObservedHostPackageState {
    if any_recovery_journal_present(transitions) {
        return blocked_state(
            CustomerState::RecoveryRequired,
            LifecycleOwnership::Owned,
            "unknown",
        );
    }

    let unit_state = inspect_managed_units(unit_host);
    let swaps_text = match std::fs::read_to_string(swaps_path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => {
            return blocked_state(
                CustomerState::UnknownBlocked,
                LifecycleOwnership::Unobserved,
                "unknown",
            );
        }
    };
    let pid_alive = daemon_pid_alive(&runtime_dir.join("ramsharedd.pid"));
    let runtime_markers =
        runtime_dir.join("zram-dev").exists() || runtime_dir.join("swap-dev").exists();
    let owned_swap_active = swaps_text.as_deref().is_some_and(|text| {
        text.lines()
            .skip(1)
            .any(|line| line.contains("/dev/nbd") || line.contains("/dev/zram"))
    });

    if pid_alive || runtime_markers {
        return ObservedHostPackageState {
            snapshot: LifecycleSnapshot {
                state: CustomerState::Active,
                owner: LifecycleOwnership::Owned,
                guarded_hooks: detect_guarded_hooks(
                    product_root,
                    systemd_dirs,
                    build_identity_path,
                    hook_paths,
                ),
            },
            release: ReleaseIdentity::new(
                read_release_version(&product_root.join("current"))
                    .or_else(|| read_build_identity_version(build_identity_path))
                    .unwrap_or_else(|| "unknown".into()),
            ),
        };
    }
    if owned_swap_active {
        return blocked_state(
            CustomerState::InstalledOff,
            LifecycleOwnership::Foreign,
            "unknown",
        );
    }
    observe_quiescent_or_unit_state(
        unit_state,
        product_root,
        systemd_dirs,
        build_identity_path,
        hook_paths,
    )
}

fn observe_quiescent_or_unit_state(
    unit_state: Result<(), CustomerError>,
    product_root: &Path,
    systemd_dirs: &[PathBuf],
    build_identity_path: &Path,
    hook_paths: &[&str],
) -> ObservedHostPackageState {
    if let Err(err) = unit_state {
        let (state, owner) = match err.reason {
            CustomerReason::ActiveSession => (CustomerState::Active, LifecycleOwnership::Owned),
            _ => (
                CustomerState::UnknownBlocked,
                LifecycleOwnership::Unobserved,
            ),
        };
        return blocked_state(
            state,
            owner,
            &read_release_version(&product_root.join("current"))
                .or_else(|| read_build_identity_version(build_identity_path))
                .unwrap_or_else(|| "unknown".into()),
        );
    }

    let selector = product_root.join("current");
    let selector_present = std::fs::symlink_metadata(&selector).is_ok();
    let unit_installed = systemd_dirs
        .iter()
        .any(|dir| MANAGED_UNITS.iter().any(|u| dir.join(u).exists()));
    let retired_unit_present = systemd_dirs
        .iter()
        .any(|dir| dir.join(RETIRED_LIFECYCLE_UNIT).exists());

    if !selector_present && !unit_installed && !retired_unit_present {
        return ObservedHostPackageState {
            snapshot: LifecycleSnapshot {
                state: CustomerState::Absent,
                owner: LifecycleOwnership::Owned,
                guarded_hooks: detect_guarded_hooks(
                    product_root,
                    systemd_dirs,
                    build_identity_path,
                    hook_paths,
                ),
            },
            release: ReleaseIdentity::new("absent"),
        };
    }

    let guarded = !retired_unit_present
        && detect_guarded_hooks(product_root, systemd_dirs, build_identity_path, hook_paths);
    let version = read_release_version(&selector)
        .or_else(|| read_build_identity_version(build_identity_path))
        .unwrap_or_else(|| {
            if guarded && unit_installed {
                env!("CARGO_PKG_VERSION").to_string()
            } else {
                "unknown".into()
            }
        });
    ObservedHostPackageState {
        snapshot: LifecycleSnapshot {
            state: CustomerState::InstalledOff,
            owner: LifecycleOwnership::Owned,
            guarded_hooks: guarded,
        },
        release: ReleaseIdentity::new(&version),
    }
}

fn any_recovery_journal_present(paths: &TransitionPaths) -> bool {
    !matches!(read_pending(paths), Ok(None))
        || !matches!(stage_journal_present(paths), Ok(false))
        || !matches!(publication_journal_present(paths), Ok(false))
        || !matches!(tree_journal_present(paths), Ok(false))
        || !matches!(selector_journal_present(paths), Ok(false))
}

fn inspect_managed_units(unit_host: &dyn UnitHost) -> Result<(), CustomerError> {
    for unit in [CASCADE_UNIT, MANAGED_UNITS[1], MANAGED_UNITS[2]] {
        let status: UnitRuntimeStatus = unit_host.query_unit(unit)?;
        match status.active_state.as_str() {
            "inactive" | "failed" | "" => {}
            "active" | "reloading" | "activating" | "deactivating" => {
                return Err(fs_error(
                    CustomerReason::ActiveSession,
                    format!("active unit {unit}"),
                    CustomerState::Active,
                ));
            }
            _ => {
                return Err(fs_error(
                    CustomerReason::ObservationUnavailable,
                    format!("unknown unit state {unit}"),
                    CustomerState::UnknownBlocked,
                ));
            }
        }
    }
    Ok(())
}

fn daemon_pid_alive(pid_path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(pid_path) else {
        return false;
    };
    let Ok(pid) = text.trim().parse::<u32>() else {
        return false;
    };
    pid > 0 && Path::new(&format!("/proc/{pid}")).exists()
}

fn read_build_identity_version(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.file_type().is_file() || meta.len() == 0 || meta.len() > MAX_BUILD_IDENTITY_BYTES {
        return None;
    }
    let body = std::fs::read_to_string(path).ok()?;
    let mut package_version = None;
    let mut has_cli_sha = false;
    let mut has_daemon_sha = false;
    for line in body.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "package_version" if !value.is_empty() && value.len() <= 64 => {
                package_version = Some(value.to_string());
            }
            "cli_sha256" if value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()) => {
                has_cli_sha = true;
            }
            "daemon_sha256"
                if value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()) =>
            {
                has_daemon_sha = true;
            }
            _ => {}
        }
    }
    (has_cli_sha && has_daemon_sha).then_some(package_version?)
}

fn detect_guarded_hooks(
    product_root: &Path,
    systemd_dirs: &[PathBuf],
    build_identity_path: &Path,
    hook_paths: &[&str],
) -> bool {
    if systemd_dirs
        .iter()
        .any(|dir| dir.join(RETIRED_LIFECYCLE_UNIT).exists())
    {
        return false;
    }
    let mut saw_hook = false;
    for path_str in hook_paths {
        let path = Path::new(path_str);
        if !path.exists() {
            continue;
        }
        saw_hook = true;
        let Ok(body) = std::fs::read_to_string(path) else {
            return false;
        };
        if !body.contains("check-quiescent") || body.contains("swapoff") {
            return false;
        }
    }
    if saw_hook {
        return true;
    }
    if read_build_identity_version(build_identity_path).is_some()
        && systemd_dirs
            .iter()
            .any(|dir| MANAGED_UNITS.iter().any(|u| dir.join(u).exists()))
    {
        return true;
    }
    let selector = product_root.join("current");
    if selector.exists() {
        let manifest_path = selector.join("release-manifest.json");
        let Ok(bytes) = std::fs::read(&manifest_path) else {
            return false;
        };
        return verify_release_manifest(&selector).is_some()
            && bytes.len() <= MAX_PAYLOAD_MANIFEST_BYTES
            && PayloadManifest::parse(&bytes).is_ok();
    }
    false
}

#[cfg(test)]
mod tests;
