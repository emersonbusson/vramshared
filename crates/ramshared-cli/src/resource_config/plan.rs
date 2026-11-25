// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/plan.rs
//! Profile loading and read-only resource plan evaluation.

use super::{
    InventoryError, MAX_RESOURCE_PROFILE_BYTES, PlannedPathIdentity, PlannedTarget,
    PlannedTierCaps, RESOURCE_PROFILE_SCHEMA_VERSION, ResourcePlan, ResourcePlatform,
    ResourceProfile, ResourceSnapshot, ResourceTarget, RuntimePlatform, StorageObservation,
    StorageVolumeIdentity, WindowsVolume, format_cap_mib, format_gib, linux_storage,
    prepared_origin, sample_is_fresh, unix_millis,
};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt::Write as FmtWrite;
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

/// Read one configured profile as raw text.
///
/// `pub(crate)` so [`crate::customer::setup_command`] reads the named profile
/// through this one loader — the same no-symlink, size-bounded, root-owned
/// discipline `config plan` already uses — instead of opening a second path.
pub(crate) fn load_profile_text(
    path: &Path,
    require_root_owned: bool,
) -> Result<Option<String>, InventoryError> {
    if require_root_owned {
        let Some(parent) = path.parent() else {
            return Err(InventoryError(
                "profile path has no parent directory".into(),
            ));
        };
        match fs::symlink_metadata(parent) {
            Ok(metadata)
                if metadata.is_dir()
                    && !metadata.file_type().is_symlink()
                    && metadata.uid() == 0
                    && metadata.mode() & 0o022 == 0 => {}
            Ok(_) => {
                return Err(InventoryError(
                    "system profile directory must be root-owned and not group/world writable"
                        .into(),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(InventoryError(format!(
                    "cannot inspect system profile directory: {error}"
                )));
            }
        }
    }

    let path_metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(InventoryError(format!(
                "cannot inspect profile file: {error}"
            )));
        }
    };
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        return Err(InventoryError(
            "profile input must be a regular, non-symlink file".into(),
        ));
    }
    if path_metadata.len() > MAX_RESOURCE_PROFILE_BYTES as u64 {
        return Err(InventoryError(
            "profile input exceeds the 64 KiB limit".into(),
        ));
    }

    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| InventoryError(format!("cannot open profile file: {error}")))?;
    let opened_metadata = file
        .metadata()
        .map_err(|error| InventoryError(format!("cannot inspect open profile: {error}")))?;
    if !opened_metadata.is_file()
        || opened_metadata.dev() != path_metadata.dev()
        || opened_metadata.ino() != path_metadata.ino()
    {
        return Err(InventoryError(
            "profile file changed while it was being opened".into(),
        ));
    }
    if require_root_owned
        && (opened_metadata.uid() != 0
            || opened_metadata.mode() & 0o7777 != 0o600
            || opened_metadata.nlink() != 1)
    {
        return Err(InventoryError(
            "system profile must be a single-link root-owned file with mode 0600".into(),
        ));
    }

    let mut content = Vec::with_capacity(path_metadata.len() as usize);
    file.take((MAX_RESOURCE_PROFILE_BYTES + 1) as u64)
        .read_to_end(&mut content)
        .map_err(|error| InventoryError(format!("cannot read profile file: {error}")))?;
    if content.len() > MAX_RESOURCE_PROFILE_BYTES {
        return Err(InventoryError(
            "profile input exceeds the 64 KiB limit".into(),
        ));
    }
    String::from_utf8(content)
        .map(Some)
        .map_err(|error| InventoryError(format!("profile input is not UTF-8: {error}")))
}

pub(super) fn profile_platform(platform: RuntimePlatform) -> ResourcePlatform {
    match platform {
        RuntimePlatform::NativeLinux => ResourcePlatform::NativeLinux,
        RuntimePlatform::Wsl2 => ResourcePlatform::Wsl2,
    }
}

pub(super) fn resource_volume_identity(target: &ResourceTarget) -> StorageVolumeIdentity {
    match target {
        ResourceTarget::LinuxEmergencySwapfile {
            filesystem_uuid,
            device_identity,
            ..
        }
        | ResourceTarget::LinuxSwapfile {
            filesystem_uuid,
            device_identity,
            ..
        }
        | ResourceTarget::LinuxFileOrigin {
            filesystem_uuid,
            device_identity,
            ..
        }
        | ResourceTarget::LinuxFileOriginRequest {
            filesystem_uuid,
            device_identity,
            ..
        } => StorageVolumeIdentity::Linux {
            filesystem_uuid: filesystem_uuid.clone(),
            device_identity: device_identity.clone(),
        },
        ResourceTarget::WslEmergencyStorage {
            windows_volume_id, ..
        }
        | ResourceTarget::WslFallback {
            windows_volume_id, ..
        }
        | ResourceTarget::WslOrigin {
            windows_volume_id, ..
        } => StorageVolumeIdentity::Windows {
            volume_id: windows_volume_id.to_lowercase(),
        },
    }
}

pub(super) fn target_kind_path_and_size(target: &ResourceTarget) -> (&'static str, String, u64) {
    match target {
        ResourceTarget::LinuxSwapfile {
            managed_relative_path,
            bytes,
            ..
        } => ("linux_swapfile", managed_relative_path.clone(), *bytes),
        ResourceTarget::LinuxFileOrigin {
            managed_relative_path,
            allocated_bytes,
            ..
        } => (
            "linux_file_origin",
            managed_relative_path.clone(),
            *allocated_bytes,
        ),
        ResourceTarget::LinuxFileOriginRequest {
            managed_relative_path,
            allocated_bytes,
            ..
        } => (
            "linux_file_origin_request",
            managed_relative_path.clone(),
            *allocated_bytes,
        ),
        ResourceTarget::LinuxEmergencySwapfile {
            managed_relative_path,
            bytes,
            ..
        } => (
            "linux_emergency_swapfile",
            managed_relative_path.clone(),
            *bytes,
        ),
        ResourceTarget::WslEmergencyStorage { path, bytes, .. } => {
            ("wsl_emergency_storage", path.clone(), *bytes)
        }
        ResourceTarget::WslFallback { path, bytes, .. } => ("wsl_fallback", path.clone(), *bytes),
        ResourceTarget::WslOrigin {
            path,
            allocated_bytes,
            ..
        } => ("wsl_origin", path.clone(), *allocated_bytes),
    }
}

pub(super) fn volume_identity_text(identity: &StorageVolumeIdentity) -> String {
    match identity {
        StorageVolumeIdentity::Linux {
            filesystem_uuid,
            device_identity,
        } => format!("filesystem:{filesystem_uuid};device:{device_identity}"),
        StorageVolumeIdentity::Windows { volume_id } => volume_id.clone(),
    }
}

pub(super) fn windows_target_free_bytes(
    snapshot: &ResourceSnapshot,
    windows_volume_id: &str,
    target_path: &str,
    now_unix_ms: u64,
) -> Result<StorageObservation, (&'static str, String)> {
    if snapshot.platform != RuntimePlatform::Wsl2 {
        return Err((
            "identity_unavailable",
            "Windows volume targets are only available under WSL2".into(),
        ));
    }
    let Some(windows) = snapshot.windows.as_ref() else {
        return Err((
            "identity_unavailable",
            "Windows host volume inventory is unavailable".into(),
        ));
    };
    if !sample_is_fresh(windows.observed_unix_ms, now_unix_ms) {
        return Err((
            "stale_sample",
            "Windows volume sample is missing or stale".into(),
        ));
    }
    let matches = windows
        .volumes
        .iter()
        .filter(|volume| {
            volume
                .volume_id
                .as_deref()
                .is_some_and(|observed| observed.eq_ignore_ascii_case(windows_volume_id))
        })
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err((
            "identity_unavailable",
            "Windows volume identity is absent or ambiguous".into(),
        ));
    }
    let volume = matches[0];
    if !windows_path_belongs_to_volume(target_path, volume) {
        return Err((
            "identity_unavailable",
            "selected path does not resolve under the bound Windows volume".into(),
        ));
    }
    let free = windows_volume_eligibility(volume, matches.len())
        .map_err(|(code, reason)| (code, reason.to_owned()))?;
    Ok(StorageObservation {
        free_bytes: free,
        mount_id: None,
    })
}

pub(super) fn windows_volume_eligibility(
    volume: &WindowsVolume,
    matching_id_count: usize,
) -> Result<u64, (&'static str, &'static str)> {
    if !volume.drive_type.eq_ignore_ascii_case("Fixed") {
        return Err(("target_ineligible", "volume is not fixed"));
    }
    if !volume
        .volume_id
        .as_deref()
        .is_some_and(|identity| !identity.trim().is_empty())
    {
        return Err((
            "identity_unavailable",
            "stable volume identity is unavailable",
        ));
    }
    if matching_id_count != 1 {
        return Err(("identity_unavailable", "volume identity is ambiguous"));
    }
    let Some(filesystem) = volume.file_system.as_deref() else {
        return Err(("target_ineligible", "filesystem is unavailable"));
    };
    if !filesystem.eq_ignore_ascii_case("NTFS") && !filesystem.eq_ignore_ascii_case("ReFS") {
        return Err(("target_ineligible", "filesystem is not NTFS/ReFS"));
    }
    let (Some(total), Some(free)) = (volume.size_bytes, volume.free_bytes) else {
        return Err(("capacity_unavailable", "volume capacity is unavailable"));
    };
    if free > total {
        return Err((
            "inconsistent_sample",
            "reported free capacity exceeds total capacity",
        ));
    }
    Ok(free)
}

pub(super) fn windows_path_belongs_to_volume(path: &str, volume: &WindowsVolume) -> bool {
    windows_path_relative_to_volume(path, volume).is_some()
}

pub(super) fn windows_path_relative_to_volume(
    path: &str,
    volume: &WindowsVolume,
) -> Option<String> {
    let normalized_path = path.to_lowercase();
    let drive_relative = volume.drive_letter.as_deref().and_then(|drive| {
        let drive = drive.trim_end_matches(':').to_lowercase();
        normalized_path.strip_prefix(&format!("{drive}:\\"))
    });
    let volume_relative = volume.volume_id.as_deref().and_then(|identity| {
        let prefix = format!("{}\\", identity.trim_end_matches('\\').to_lowercase());
        normalized_path.strip_prefix(&prefix)
    });
    drive_relative.or(volume_relative).map(str::to_owned)
}

pub(super) fn planned_path_identity(
    snapshot: &ResourceSnapshot,
    target: &ResourceTarget,
) -> Option<PlannedPathIdentity> {
    match target {
        ResourceTarget::LinuxEmergencySwapfile {
            filesystem_uuid,
            device_identity,
            managed_relative_path,
            ..
        }
        | ResourceTarget::LinuxSwapfile {
            filesystem_uuid,
            device_identity,
            managed_relative_path,
            ..
        }
        | ResourceTarget::LinuxFileOrigin {
            filesystem_uuid,
            device_identity,
            managed_relative_path,
            ..
        }
        | ResourceTarget::LinuxFileOriginRequest {
            filesystem_uuid,
            device_identity,
            managed_relative_path,
            ..
        } => Some(PlannedPathIdentity::Linux {
            filesystem_uuid: filesystem_uuid.clone(),
            device_identity: device_identity.clone(),
            relative_path: managed_relative_path.clone(),
        }),
        ResourceTarget::WslEmergencyStorage {
            windows_volume_id,
            path,
            ..
        }
        | ResourceTarget::WslFallback {
            windows_volume_id,
            path,
            ..
        }
        | ResourceTarget::WslOrigin {
            windows_volume_id,
            path,
            ..
        } => {
            let windows = snapshot.windows.as_ref()?;
            let mut matches = windows.volumes.iter().filter(|volume| {
                volume
                    .volume_id
                    .as_deref()
                    .is_some_and(|observed| observed.eq_ignore_ascii_case(windows_volume_id))
            });
            let volume = matches.next()?;
            if matches.next().is_some() {
                return None;
            }
            let relative_path = windows_path_relative_to_volume(path, volume)?;
            Some(PlannedPathIdentity::Windows {
                volume_id: windows_volume_id
                    .trim_end_matches('\\')
                    .trim_end_matches('/')
                    .to_lowercase(),
                relative_path: relative_path.to_lowercase(),
            })
        }
    }
}

pub(super) fn unconfigured_resource_plan(snapshot: &ResourceSnapshot) -> ResourcePlan {
    ResourcePlan {
        schema_version: RESOURCE_PROFILE_SCHEMA_VERSION,
        platform: snapshot.platform,
        observed_unix_ms: snapshot.observed_unix_ms,
        profile_state: "not_configured",
        profile_sha256: None,
        unenforced_planned_caps: None,
        targets: Vec::new(),
        gpu_budget_status: snapshot.gpu_budget_status.clone(),
        warnings: snapshot.warnings.clone(),
        writes_performed: false,
        apply_enabled: false,
        status: "not_configured",
    }
}

pub(super) fn target_free_bytes(
    snapshot: &ResourceSnapshot,
    target: &ResourceTarget,
    now_unix_ms: u64,
) -> Result<StorageObservation, (&'static str, String)> {
    match target {
        ResourceTarget::LinuxEmergencySwapfile { .. }
        | ResourceTarget::LinuxSwapfile { .. }
        | ResourceTarget::LinuxFileOrigin { .. }
        | ResourceTarget::LinuxFileOriginRequest { .. } => {
            linux_storage::observe_target(snapshot, target, now_unix_ms)
        }
        ResourceTarget::WslEmergencyStorage {
            windows_volume_id,
            path,
            ..
        }
        | ResourceTarget::WslFallback {
            windows_volume_id,
            path,
            ..
        }
        | ResourceTarget::WslOrigin {
            windows_volume_id,
            path,
            ..
        } => windows_target_free_bytes(snapshot, windows_volume_id, path, now_unix_ms),
    }
}

pub(super) fn plan_target(
    snapshot: &ResourceSnapshot,
    target: &ResourceTarget,
    required_free_bytes: u64,
    now_unix_ms: u64,
    duplicate_path: bool,
) -> PlannedTarget {
    let volume_identity = resource_volume_identity(target);
    let (kind, path, requested_bytes) = target_kind_path_and_size(target);
    let assessment = if duplicate_path {
        Err((
            "duplicate_target_path",
            "target resolves to the same managed path as another profile entry".into(),
        ))
    } else {
        target_free_bytes(snapshot, target, now_unix_ms)
    };
    let (observed_free_bytes, observed_mount_id, status, reason) = match assessment {
        Ok(observation) if observation.free_bytes >= required_free_bytes => (
            Some(observation.free_bytes),
            observation.mount_id,
            "storage_ready",
            None,
        ),
        Ok(observation) => (
            Some(observation.free_bytes),
            observation.mount_id,
            "insufficient_space",
            Some("available free space is below the combined target and reserve".into()),
        ),
        Err((status, reason)) => (None, None, status, Some(reason)),
    };
    PlannedTarget {
        kind,
        volume_identity: volume_identity_text(&volume_identity),
        path,
        observed_mount_id,
        requested_bytes,
        required_free_bytes,
        observed_free_bytes,
        status,
        reason,
    }
}

pub(super) fn plan_status(targets: &[PlannedTarget]) -> &'static str {
    if targets.is_empty() {
        "profile_loaded_no_storage_targets"
    } else if targets
        .iter()
        .all(|target| target.status == "storage_ready")
    {
        "ready_for_review"
    } else {
        "blocked"
    }
}

pub(super) fn profile_warnings(snapshot: &ResourceSnapshot, caps: &PlannedTierCaps) -> Vec<String> {
    let mut warnings = snapshot.warnings.clone();
    if caps.zram_bytes.is_some() || !caps.vram_bytes.is_empty() || caps.origin_bytes.is_some() {
        warnings.push(
            "planned draft caps are unenforced; this plan does not sample their owning runtime budgets, limit any tier, or authorize increases".into(),
        );
    }
    warnings
}

/// The one fingerprint of resource-profile bytes.
///
/// `pub(crate)` so [`crate::customer::plan`] pins the same digest this module
/// already records on a plan rather than re-implementing the hash.
pub(crate) fn profile_digest(profile_text: &str) -> String {
    Sha256::digest(profile_text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(super) fn build_resource_plan(
    snapshot: &ResourceSnapshot,
    profile_text: Option<&str>,
) -> Result<ResourcePlan, InventoryError> {
    let Some(profile_text) = profile_text else {
        return Ok(unconfigured_resource_plan(snapshot));
    };
    let profile = ResourceProfile::parse(profile_text)
        .map_err(|error| InventoryError(format!("resource profile is invalid: {error}")))?;
    profile
        .validate_for(profile_platform(snapshot.platform))
        .map_err(|error| InventoryError(format!("resource profile is not valid here: {error}")))?;
    let now_unix_ms = unix_millis();
    let profile = prepared_origin::project(snapshot, profile, now_unix_ms)?;
    let required_by_volume = profile
        .required_free_bytes_by_volume()
        .map_err(|error| InventoryError(format!("cannot calculate profile capacity: {error}")))?;
    let mut targets = Vec::with_capacity(profile.targets.len());
    let mut planned_paths = HashSet::new();

    for target in &profile.targets {
        let volume_identity = resource_volume_identity(target);
        let required_free_bytes = required_by_volume
            .get(&volume_identity)
            .copied()
            .ok_or_else(|| InventoryError("profile target has no volume requirement".into()))?;
        let duplicate_path = planned_path_identity(snapshot, target)
            .is_some_and(|identity| !planned_paths.insert(identity));
        targets.push(plan_target(
            snapshot,
            target,
            required_free_bytes,
            now_unix_ms,
            duplicate_path,
        ));
    }

    let status = plan_status(&targets);
    let profile_sha256 = profile_digest(profile_text);
    let warnings = profile_warnings(snapshot, &profile.planned_caps);
    Ok(ResourcePlan {
        schema_version: RESOURCE_PROFILE_SCHEMA_VERSION,
        platform: snapshot.platform,
        observed_unix_ms: snapshot.observed_unix_ms,
        profile_state: "validated",
        profile_sha256: Some(profile_sha256),
        unenforced_planned_caps: Some(profile.planned_caps),
        targets,
        gpu_budget_status: snapshot.gpu_budget_status.clone(),
        warnings,
        writes_performed: false,
        apply_enabled: false,
        status,
    })
}

pub(super) fn render_plan_text(plan: &ResourcePlan) -> String {
    let mut output = String::new();
    let _ = writeln!(output, "RamShared read-only resource plan: {}", plan.status);
    let _ = writeln!(output, "Profile: {}", plan.profile_state);
    if let Some(digest) = &plan.profile_sha256 {
        let _ = writeln!(output, "Profile SHA-256: {digest}");
    }
    if let Some(caps) = &plan.unenforced_planned_caps {
        let _ = writeln!(
            output,
            "Unenforced planned draft caps (profile policy only; not reservations or limits):"
        );
        let _ = writeln!(
            output,
            "  ZRAM planned cap (unenforced): {}",
            format_cap_mib(caps.zram_bytes)
        );
        let _ = writeln!(
            output,
            "  SSD origin planned cap (unenforced): {}",
            format_cap_mib(caps.origin_bytes)
        );
        if caps.vram_bytes.is_empty() {
            let _ = writeln!(output, "  VRAM planned caps (unenforced): not configured");
        } else {
            for (adapter_id, bytes) in &caps.vram_bytes {
                let _ = writeln!(
                    output,
                    "  VRAM planned cap (unenforced) for {adapter_id}: {}",
                    format_cap_mib(Some(*bytes))
                );
            }
        }
    }
    for target in &plan.targets {
        let _ = writeln!(
            output,
            "{} {} — {} (needs {}, observed free {})",
            target.kind,
            target.path,
            target.status,
            format_gib(Some(target.required_free_bytes)),
            format_gib(target.observed_free_bytes)
        );
        let _ = writeln!(output, "  volume identity: {}", target.volume_identity);
        if let Some(mount_id) = target.observed_mount_id {
            let _ = writeln!(output, "  current mount ID (ephemeral): {mount_id}");
        }
        if let Some(reason) = &target.reason {
            let _ = writeln!(output, "  reason: {reason}");
        }
    }
    for warning in &plan.warnings {
        let _ = writeln!(output, "Warning: {warning}");
    }
    let _ = writeln!(
        output,
        "Read-only plan. No settings, swap, origin, disk, GPU, or running tier were changed."
    );
    let _ = writeln!(output, "Apply enabled: {}", plan.apply_enabled);
    output
}

pub(super) fn render_plan_json(plan: &ResourcePlan) -> Result<String, InventoryError> {
    serde_json::to_string(plan)
        .map_err(|error| InventoryError(format!("cannot serialize resource plan: {error}")))
}
