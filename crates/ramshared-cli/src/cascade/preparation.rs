// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/preparation.rs
//! Conservative, read-only preparation admission.

use super::*;

/// Read-only composition of the existing lifecycle verdict for preparation.
pub(crate) fn preparation_is_off() -> Result<bool, CascadeError> {
    use std::os::unix::fs::MetadataExt;
    let (daemon, _) = crate::customer::cache_configuration::installed_daemon()
        .map_err(|error| CascadeError::Io(format!("observe selected daemon: {error}")))?;
    let daemon_file = fs::File::open(&daemon)
        .map_err(|error| CascadeError::Io(format!("open selected daemon: {error}")))?;
    let daemon_metadata = daemon_file
        .metadata()
        .map_err(|error| CascadeError::Io(format!("observe selected daemon identity: {error}")))?;
    if !daemon_metadata.is_file() {
        return Err(CascadeError::Io(
            "selected daemon is not a regular file".into(),
        ));
    }
    let entries = read_swaps()?;
    let snapshot = build_cascade_snapshot(&entries);
    let proc_root = Path::new("/proc");
    let procfs = super::preparation_process::open_verified_procfs(proc_root)
        .map_err(|error| CascadeError::Io(format!("verify process root: {error}")))?;
    super::preparation_process::verify_procfs_path(proc_root, &procfs)
        .map_err(|error| CascadeError::Io(format!("verify process root: {error}")))?;
    let result = preparation_from_sources(
        &snapshot,
        &[
            Path::new(PID_FILE),
            Path::new(SOCK),
            Path::new(ZRAM_DEV_FILE),
            Path::new(SWAP_DEV_FILE),
        ],
        proc_root,
        Some((daemon_metadata.dev(), daemon_metadata.ino())),
        PreparationUnits {
            units: &[
                super::boot_policy_effect::CASCADE_UNIT,
                super::boot_policy_effect::HEALTH_UNIT,
                super::boot_policy_effect::HOST_GATE_UNIT,
                super::boot_policy_effect::SUPERVISOR_UNIT,
            ],
            uncooperative_units: &[
                crate::customer::package::RETIRED_LIFECYCLE_UNIT,
                "ramshared-auto-update.timer",
                "ramshared-auto-update.service",
                "ramshared-vram-tier.service",
                "ramshared-vram.service",
            ],
            observe_unit: crate::release_coherence::unit_is_quiescent,
            observe_mask: crate::release_coherence::unit_is_masked_or_absent,
        },
    );
    super::preparation_process::verify_procfs_path(proc_root, &procfs)
        .map_err(|error| CascadeError::Io(format!("recheck process root: {error}")))?;
    result
}

pub(super) struct PreparationUnits<'a> {
    pub(super) units: &'a [&'a str],
    pub(super) uncooperative_units: &'a [&'a str],
    pub(super) observe_unit: fn(&str) -> Option<bool>,
    pub(super) observe_mask: fn(&str) -> Option<bool>,
}

pub(super) fn preparation_from_sources(
    snapshot: &CascadeSnapshot,
    records: &[&Path],
    proc_root: &Path,
    daemon_identity: Option<(u64, u64)>,
    units: PreparationUnits<'_>,
) -> Result<bool, CascadeError> {
    preparation_gate(
        snapshot,
        &|| preparation_records_absent(records),
        &|| {
            super::preparation_process::possible_daemon_present_with_identity(
                proc_root,
                daemon_identity,
            )
            .map_err(|error| CascadeError::Io(format!("observe preparation processes: {error}")))
        },
        &|| {
            Ok(
                preparation_units_quiescent(units.units, units.observe_unit)?
                    && preparation_units_quiescent(units.uncooperative_units, units.observe_mask)?,
            )
        },
    )
}

pub(super) fn preparation_gate(
    snapshot: &CascadeSnapshot,
    records_absent: &dyn Fn() -> Result<bool, CascadeError>,
    daemon_present: &dyn Fn() -> Result<bool, CascadeError>,
    units_quiescent: &dyn Fn() -> Result<bool, CascadeError>,
) -> Result<bool, CascadeError> {
    Ok(preparation_snapshot_is_off(snapshot)
        && records_absent()?
        && !daemon_present()?
        && units_quiescent()?)
}

pub(super) fn preparation_units_quiescent(
    units: &[&str],
    mut observe: impl FnMut(&str) -> Option<bool>,
) -> Result<bool, CascadeError> {
    for unit in units {
        match observe(unit) {
            Some(true) => {}
            Some(false) => return Ok(false),
            None => return Err(CascadeError::Io(format!("unit state unavailable: {unit}"))),
        }
    }
    Ok(true)
}

pub(super) fn preparation_records_absent(paths: &[&Path]) -> Result<bool, CascadeError> {
    for path in paths {
        match fs::symlink_metadata(path) {
            Ok(_) => return Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(CascadeError::Io(format!(
                    "observe preparation record {}: {error}",
                    path.display()
                )));
            }
        }
    }
    Ok(true)
}

pub(super) fn preparation_snapshot_is_off(snapshot: &CascadeSnapshot) -> bool {
    !snapshot.daemon_alive
        && !snapshot.zram.present
        && !snapshot.vram.present
        && !snapshot.topology.writeback_target_is_tier
        && snapshot.measurement_errors.is_empty()
        && derive_lifecycle(snapshot).phase == lifecycle::CascadePhase::Off
}
