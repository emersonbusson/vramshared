// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/resource_config/draft.rs
//! Interactive/streamed draft creation for unapplied resource profiles.

use super::linux_storage::{self, linux_target_free_bytes};
use super::{
    BYTES_PER_MIB, DRAFT_LINUX_ORIGIN_PATH, DRAFT_LINUX_SWAP_PATH, DraftStorageIdentity,
    DraftTargetRole, DraftVolumeCandidate, InventoryError, MAX_DRAFT_LINE_BYTES, MAX_DRAFT_TARGETS,
    PlannedTierCaps, RESOURCE_PROFILE_SCHEMA_VERSION, ResourcePlatform, ResourceProfile,
    ResourceSnapshot, ResourceTarget, RuntimePlatform, StorageVolumeIdentity, build_resource_plan,
    collect_snapshot, format_gib, profile_platform, render_plan_text, render_text, sample_is_fresh,
    unix_millis, windows_volume_eligibility,
};
use std::collections::HashSet;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

pub(super) fn draft_volume_candidates(
    snapshot: &ResourceSnapshot,
    now_unix_ms: u64,
) -> Vec<DraftVolumeCandidate> {
    match snapshot.platform {
        RuntimePlatform::NativeLinux => {
            let mut candidates = Vec::new();
            let mut seen = HashSet::new();
            for device in &snapshot.block_devices {
                let (Some(filesystem_uuid), Some(device_identity)) = (
                    device.uuid.as_deref(),
                    linux_storage::backing_device_identity(device),
                ) else {
                    continue;
                };
                let identity = StorageVolumeIdentity::Linux {
                    filesystem_uuid: filesystem_uuid.into(),
                    device_identity: device_identity.into(),
                };
                let Ok(observation) = linux_target_free_bytes(
                    snapshot,
                    filesystem_uuid,
                    device_identity,
                    now_unix_ms,
                ) else {
                    continue;
                };
                if !seen.insert(identity) {
                    continue;
                }
                let Some(mount) = device
                    .mounts
                    .iter()
                    .find(|mount| Some(mount.mount_id) == observation.mount_id)
                else {
                    continue;
                };
                let Some(observed_unix_ms) = mount.capacity_observed_unix_ms else {
                    continue;
                };
                let mountpoint = mount.mountpoint.as_str();
                candidates.push(DraftVolumeCandidate {
                    short_display: format!(
                        "{} at {}",
                        device.model.as_deref().unwrap_or(&device.name),
                        mountpoint
                    ),
                    display: format!(
                        "{} at {} — filesystem {}, device {}",
                        device.model.as_deref().unwrap_or(&device.name),
                        mountpoint,
                        filesystem_uuid,
                        device_identity
                    ),
                    free_bytes: observation.free_bytes,
                    observed_unix_ms,
                    storage: DraftStorageIdentity::NativeLinux {
                        filesystem_uuid: filesystem_uuid.into(),
                        device_identity: device_identity.into(),
                    },
                });
            }
            candidates
        }
        RuntimePlatform::Wsl2 => {
            let Some(windows) = snapshot.windows.as_ref() else {
                return Vec::new();
            };
            if !sample_is_fresh(windows.observed_unix_ms, now_unix_ms) {
                return Vec::new();
            }
            windows
                .volumes
                .iter()
                .filter_map(|volume| {
                    let volume_id = volume.volume_id.as_deref()?;
                    let matching_id_count = windows
                        .volumes
                        .iter()
                        .filter(|candidate| {
                            candidate
                                .volume_id
                                .as_deref()
                                .is_some_and(|observed| observed.eq_ignore_ascii_case(volume_id))
                        })
                        .count();
                    let free_bytes = windows_volume_eligibility(volume, matching_id_count).ok()?;
                    if !draft_wsl_target_path_is_valid(volume_id, volume.drive_letter.as_deref()) {
                        return None;
                    }
                    let short_display = format!(
                        "{} {} ({})",
                        volume.drive_letter.as_deref().map_or_else(
                            || "no drive letter".into(),
                            |letter| format!("{letter}:")
                        ),
                        volume.label.as_deref().unwrap_or("unlabeled"),
                        volume
                            .file_system
                            .as_deref()
                            .unwrap_or("filesystem unknown")
                    );
                    Some(DraftVolumeCandidate {
                        display: format!("{short_display} — volume {volume_id}"),
                        short_display,
                        free_bytes,
                        observed_unix_ms: windows.observed_unix_ms?,
                        storage: DraftStorageIdentity::Wsl2 {
                            volume_id: volume_id.into(),
                            drive_letter: volume.drive_letter.clone(),
                        },
                    })
                })
                .collect()
        }
    }
}

pub(super) fn draft_windows_path(
    volume_id: &str,
    drive_letter: Option<&str>,
    role: DraftTargetRole,
) -> Result<String, InventoryError> {
    let leaf = match role {
        DraftTargetRole::Emergency => "emergency.img",
        DraftTargetRole::FallbackSwap => "fallback-swap.vhdx",
        DraftTargetRole::RamSharedOrigin => "origin.vhdx",
    };
    let path = if let Some(letter) = drive_letter {
        let letter = letter.trim_end_matches(':');
        if letter.len() != 1 || !letter.as_bytes()[0].is_ascii_alphabetic() {
            return Err(InventoryError(
                "selected Windows volume has an invalid drive-letter display value".into(),
            ));
        }
        format!("{}:\\wsl\\ramshared\\{leaf}", letter.to_ascii_uppercase())
    } else {
        format!(
            "{}\\wsl\\ramshared\\{leaf}",
            volume_id.trim_end_matches(['\\', '/'])
        )
    };
    Ok(path)
}

pub(super) fn draft_wsl_target_path_is_valid(volume_id: &str, drive_letter: Option<&str>) -> bool {
    let Ok(path) = draft_windows_path(volume_id, drive_letter, DraftTargetRole::FallbackSwap)
    else {
        return false;
    };
    let profile = ResourceProfile {
        schema_version: RESOURCE_PROFILE_SCHEMA_VERSION,
        planned_caps: PlannedTierCaps::default(),
        targets: vec![ResourceTarget::WslFallback {
            windows_volume_id: volume_id.into(),
            path,
            bytes: 1,
        }],
    };
    profile.validate_for(ResourcePlatform::Wsl2).is_ok()
}

pub(super) fn materialize_draft_target(
    candidate: &DraftVolumeCandidate,
    role: DraftTargetRole,
    bytes: u64,
) -> Result<ResourceTarget, InventoryError> {
    match &candidate.storage {
        DraftStorageIdentity::NativeLinux {
            filesystem_uuid,
            device_identity,
        } => Ok(match role {
            DraftTargetRole::Emergency => ResourceTarget::LinuxEmergencySwapfile {
                filesystem_uuid: filesystem_uuid.clone(),
                device_identity: device_identity.clone(),
                managed_relative_path: "swap/ramshared-emergency.swap".into(),
                bytes,
                priority: -1,
            },
            DraftTargetRole::FallbackSwap => ResourceTarget::LinuxSwapfile {
                filesystem_uuid: filesystem_uuid.clone(),
                device_identity: device_identity.clone(),
                managed_relative_path: DRAFT_LINUX_SWAP_PATH.into(),
                bytes,
                priority: -1,
            },
            DraftTargetRole::RamSharedOrigin => ResourceTarget::LinuxFileOriginRequest {
                filesystem_uuid: filesystem_uuid.clone(),
                device_identity: device_identity.clone(),
                managed_relative_path: DRAFT_LINUX_ORIGIN_PATH.into(),
                allocated_bytes: bytes,
            },
        }),
        DraftStorageIdentity::Wsl2 {
            volume_id,
            drive_letter,
        } => {
            let path = draft_windows_path(volume_id, drive_letter.as_deref(), role)?;
            Ok(match role {
                DraftTargetRole::Emergency => ResourceTarget::WslEmergencyStorage {
                    windows_volume_id: volume_id.clone(),
                    path,
                    bytes,
                },
                DraftTargetRole::FallbackSwap => ResourceTarget::WslFallback {
                    windows_volume_id: volume_id.clone(),
                    path,
                    bytes,
                },
                DraftTargetRole::RamSharedOrigin => ResourceTarget::WslOrigin {
                    windows_volume_id: volume_id.clone(),
                    path,
                    allocated_bytes: bytes,
                },
            })
        }
    }
}

pub(super) fn parse_draft_size_mib(value: &str) -> Result<u64, InventoryError> {
    let mib = value
        .trim()
        .parse::<u64>()
        .map_err(|_| InventoryError("size must be a positive integer MiB value".into()))?;
    if mib == 0 {
        return Err(InventoryError("size must be greater than zero".into()));
    }
    mib.checked_mul(BYTES_PER_MIB)
        .ok_or_else(|| InventoryError("size exceeds the supported byte range".into()))
}

pub(super) fn parse_optional_draft_cap_mib(value: &str) -> Result<Option<u64>, InventoryError> {
    if value.trim().is_empty() {
        return Ok(None);
    }
    parse_draft_size_mib(value)
        .map(Some)
        .map_err(|error| InventoryError(format!("invalid draft planned cap: {error}")))
}

pub(super) fn read_draft_line(
    input: &mut dyn Read,
    output: &mut dyn Write,
    prompt: &str,
) -> Result<String, InventoryError> {
    write!(output, "{prompt}")
        .map_err(|error| InventoryError(format!("cannot write config prompt: {error}")))?;
    output
        .flush()
        .map_err(|error| InventoryError(format!("cannot flush config prompt: {error}")))?;
    let mut bytes = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        let read = input
            .read(&mut byte)
            .map_err(|error| InventoryError(format!("cannot read config input: {error}")))?;
        if read == 0 {
            return Err(InventoryError(
                "end of input before draft confirmation".into(),
            ));
        }
        match byte[0] {
            b'\n' => break,
            b'\r' => continue,
            value if bytes.len() < MAX_DRAFT_LINE_BYTES => bytes.push(value),
            _ => return Err(InventoryError("config input line exceeds 128 bytes".into())),
        }
    }
    String::from_utf8(bytes)
        .map_err(|error| InventoryError(format!("config input is not UTF-8: {error}")))
}

pub(super) fn resolve_user_draft_path(path: &Path) -> Result<PathBuf, InventoryError> {
    crate::artifact_file::inspect_new_output(path)
        .map_err(|error| InventoryError(error.to_string()))
}

pub(super) fn save_profile_draft(
    path: &Path,
    profile_text: &str,
) -> Result<PathBuf, InventoryError> {
    crate::artifact_file::write_new(path, profile_text.as_bytes())
        .map_err(|error| InventoryError(error.to_string()))
}

pub(super) fn display_draft_candidates(
    snapshot: &ResourceSnapshot,
    candidates: &[DraftVolumeCandidate],
    output: &mut dyn Write,
) -> Result<(), InventoryError> {
    write!(output, "{}", render_text(snapshot))
        .map_err(|error| InventoryError(format!("cannot display resource inventory: {error}")))?;
    if candidates.is_empty() {
        return Err(InventoryError(
            "no fresh, uniquely identified eligible storage volume is available for a draft target"
                .into(),
        ));
    }
    writeln!(output, "Eligible draft targets:")
        .map_err(|error| InventoryError(format!("cannot display draft candidates: {error}")))?;
    for (index, candidate) in candidates.iter().enumerate() {
        writeln!(
            output,
            "  {}. {} — free {}",
            index + 1,
            candidate.display,
            format_gib(Some(candidate.free_bytes))
        )
        .map_err(|error| InventoryError(format!("cannot display draft candidates: {error}")))?;
    }
    Ok(())
}

pub(super) fn add_draft_target(
    snapshot: &ResourceSnapshot,
    candidates: &[DraftVolumeCandidate],
    profile: &mut ResourceProfile,
    input: &mut dyn Read,
    output: &mut dyn Write,
) -> Result<bool, InventoryError> {
    let role_text = read_draft_line(
        input,
        output,
        "Add target [swap | origin | emergency | done]: ",
    )?;
    let role = match role_text.trim().to_ascii_lowercase().as_str() {
        "swap" => DraftTargetRole::FallbackSwap,
        "origin" => DraftTargetRole::RamSharedOrigin,
        "emergency" => DraftTargetRole::Emergency,
        "done" => return Ok(false),
        _ => {
            return Err(InventoryError(
                "target role must be swap, origin, emergency, or done".into(),
            ));
        }
    };
    let volume_text = read_draft_line(input, output, "Eligible volume number: ")?;
    let volume_index = volume_text
        .trim()
        .parse::<usize>()
        .ok()
        .and_then(|index| index.checked_sub(1))
        .filter(|index| *index < candidates.len())
        .ok_or_else(|| InventoryError("selected volume number is not in the list".into()))?;
    let size_text = read_draft_line(input, output, "Target size (MiB): ")?;
    let bytes = parse_draft_size_mib(&size_text)?;
    profile.targets.push(materialize_draft_target(
        &candidates[volume_index],
        role,
        bytes,
    )?);
    let profile_text = profile
        .to_toml()
        .map_err(|error| InventoryError(format!("cannot encode draft profile: {error}")))?;
    let plan = match build_resource_plan(snapshot, Some(&profile_text)) {
        Ok(plan) if plan.status == "ready_for_review" => plan,
        Ok(plan) => {
            profile.targets.pop();
            writeln!(
                output,
                "Target refused; it was not added:\n{}",
                render_plan_text(&plan)
            )
            .map_err(|error| InventoryError(format!("cannot display target refusal: {error}")))?;
            return Ok(true);
        }
        Err(error) => {
            profile.targets.pop();
            writeln!(output, "Target refused: {error}").map_err(|write_error| {
                InventoryError(format!("cannot display target refusal: {write_error}"))
            })?;
            return Ok(true);
        }
    };
    let selected = plan
        .targets
        .last()
        .map(|target| target.kind)
        .unwrap_or("target");
    writeln!(
        output,
        "Added {selected}; current plan remains read-only and apply-disabled."
    )
    .map_err(|error| InventoryError(format!("cannot display draft result: {error}")))?;
    Ok(true)
}

pub(super) fn collect_draft_profile(
    snapshot: &ResourceSnapshot,
    candidates: &[DraftVolumeCandidate],
    input: &mut dyn Read,
    output: &mut dyn Write,
) -> Result<ResourceProfile, InventoryError> {
    let mut profile = ResourceProfile {
        schema_version: RESOURCE_PROFILE_SCHEMA_VERSION,
        planned_caps: PlannedTierCaps::default(),
        targets: Vec::new(),
    };
    loop {
        if profile.targets.len() == MAX_DRAFT_TARGETS {
            writeln!(output, "Maximum of {MAX_DRAFT_TARGETS} targets reached.")
                .map_err(|error| InventoryError(format!("cannot write draft result: {error}")))?;
            break;
        }
        if !add_draft_target(snapshot, candidates, &mut profile, input, output)? {
            break;
        }
    }
    if profile.targets.is_empty() {
        return Err(InventoryError("no draft targets were selected".into()));
    }
    let zram_text = read_draft_line(
        input,
        output,
        "ZRAM planned cap (MiB, unenforced draft; blank leaves it unset): ",
    )?;
    profile.planned_caps.zram_bytes = parse_optional_draft_cap_mib(&zram_text)?;

    let origin_text = read_draft_line(
        input,
        output,
        "SSD origin planned cap (MiB, unenforced draft; blank leaves it unset): ",
    )?;
    profile.planned_caps.origin_bytes = parse_optional_draft_cap_mib(&origin_text)?;

    writeln!(
        output,
        "VRAM planned cap unavailable: no validated adapter budget was sampled; this wizard will not accept a manual adapter ID."
    )
    .map_err(|error| InventoryError(format!("cannot explain VRAM cap availability: {error}")))?;
    writeln!(
        output,
        "Planned caps are unenforced draft policy only; no RAM, GPU, swap, origin, or disk capacity is reserved or limited by this plan."
    )
    .map_err(|error| InventoryError(format!("cannot explain planned cap behavior: {error}")))?;

    profile
        .validate_for(profile_platform(snapshot.platform))
        .map_err(|error| InventoryError(format!("draft profile is invalid: {error}")))?;
    Ok(profile)
}

pub(super) fn save_reviewed_draft(
    snapshot: &ResourceSnapshot,
    profile: &ResourceProfile,
    output_path: &Path,
    input: &mut dyn Read,
    output: &mut dyn Write,
) -> Result<(), InventoryError> {
    let profile_text = profile
        .to_toml()
        .map_err(|error| InventoryError(format!("cannot encode draft profile: {error}")))?;
    let plan = build_resource_plan(snapshot, Some(&profile_text))?;
    if plan.status != "ready_for_review" || plan.apply_enabled || plan.writes_performed {
        return Err(InventoryError(
            "final draft plan is blocked or unexpectedly permits mutation".into(),
        ));
    }
    writeln!(output, "{}", render_plan_text(&plan))
        .map_err(|error| InventoryError(format!("cannot display final draft plan: {error}")))?;
    writeln!(
        output,
        "Draft only: no swap, origin, GPU, .wslconfig, or running tier was changed."
    )
    .map_err(|error| InventoryError(format!("cannot display draft boundary: {error}")))?;
    let confirmation = read_draft_line(
        input,
        output,
        &format!("Type SAVE to create new draft {}: ", output_path.display()),
    )?;
    if confirmation != "SAVE" {
        writeln!(output, "Draft canceled; no file was written.")
            .map_err(|error| InventoryError(format!("cannot display cancellation: {error}")))?;
        return Ok(());
    }
    let saved_path = save_profile_draft(output_path, &profile_text)?;
    writeln!(output, "Validated draft saved at {}.", saved_path.display())
        .map_err(|error| InventoryError(format!("cannot report saved draft: {error}")))
}

pub(super) fn run_draft_wizard_with_io(
    snapshot: &ResourceSnapshot,
    output_path: &Path,
    input: &mut dyn Read,
    output: &mut dyn Write,
) -> Result<(), InventoryError> {
    let candidates = draft_volume_candidates(snapshot, unix_millis());
    display_draft_candidates(snapshot, &candidates, output)?;
    let profile = collect_draft_profile(snapshot, &candidates, input, output)?;
    save_reviewed_draft(snapshot, &profile, output_path, input, output)
}

pub(super) fn run_draft_wizard(
    output_path: &str,
    output: &mut dyn Write,
) -> Result<(), InventoryError> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(InventoryError(
            "config draft needs a terminal on stdin and stdout; no profile was written".into(),
        ));
    }
    let snapshot = collect_snapshot()?;
    let mut input = io::stdin().lock();
    run_draft_wizard_with_io(&snapshot, Path::new(output_path), &mut input, output)
}
