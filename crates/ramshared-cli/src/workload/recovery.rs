// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/workload/recovery.rs
//! Operator-initiated safe-mode recovery (`recover --status` / `recover --resume`).

use super::{
    HostResumeLeaseSource, MAX_CONTROL_EVIDENCE_AGE_MS, OwnerIdentity, RECOVERY_PENDING_FILE,
    RECOVERY_TRANSACTION_SCHEMA_VERSION, RESUME_LEASE_FILE, RecoveryTransaction, SAFE_MODE_FILE,
    SAFE_MODE_GATE_SCHEMA_VERSION, SystemOwnerProbe, mint_host_resume_lease,
    read_current_supervisor_status_at, read_hardware_health_at, unix_time_ms, valid_distro,
    valid_incident_id, validate_recovery_transaction, write_atomic_durable,
};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

pub(super) fn recover_status_at(safe_mode: &Path, resume_lease: &Path) -> Result<String, String> {
    serde_json::to_string(&serde_json::json!({
        "schema_version": 4,
        "safe_mode": safe_mode.is_file(),
        "resume_lease": resume_lease.is_file()
    }))
    .map_err(|error| error.to_string())
}

pub fn recover_status() -> Result<String, String> {
    recover_status_at(Path::new(SAFE_MODE_FILE), Path::new(RESUME_LEASE_FILE))
}

#[derive(Debug, Deserialize)]
pub(super) struct GuestSafeModeGate {
    pub(super) schema_version: u32,
    pub(super) incident_id: String,
    pub(super) distro: String,
    pub(super) boot_id: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct HostSafeModeGate {
    pub(super) schema_version: u32,
    pub(super) incident_id: String,
    pub(super) distro: String,
    pub(super) prior_boot_id: String,
    pub(super) reason: String,
    pub(super) timestamp_utc: String,
}

pub(super) struct RecoveryPaths {
    pub(super) supervisor_state: PathBuf,
    pub(super) boot_id: PathBuf,
    pub(super) guest_gate: PathBuf,
    pub(super) host_gate_dir: PathBuf,
    pub(super) pending: PathBuf,
    pub(super) resume_lease: PathBuf,
    pub(super) hardware_health: PathBuf,
}

pub(super) fn recover_resume_at(paths: &RecoveryPaths) -> Result<(), String> {
    let current = OwnerIdentity::current()?;
    let boot_id = fs::read_to_string(&paths.boot_id).map_err(|error| error.to_string())?;
    let boot_id = boot_id.trim();
    if boot_id != current.boot_id {
        return Err("recovery boot identity is not current".into());
    }
    // DT-6: `recover --resume` may clear `unclean_session`; it may never clear
    // an MCE/SIGBUS event. The classifier's verdict is the only authority, and
    // a refusal (or an absent document) leaves the hardware gate closed.
    let hardware_health = read_hardware_health_at(&paths.hardware_health, boot_id)?;
    let now_ms = unix_time_ms()?;
    let supervisor = read_current_supervisor_status_at(
        &paths.supervisor_state,
        &current,
        now_ms,
        60,
        &SystemOwnerProbe,
    )
    .map_err(|error| {
        if error == "supervisor health evidence is unavailable"
            || error == "supervisor health evidence is malformed"
        {
            error
        } else {
            "recover requires 60 consecutive current healthy supervisor samples".to_string()
        }
    })?;
    let guest_gate_text = fs::read_to_string(&paths.guest_gate)
        .map_err(|_| "guest safe-mode marker is unavailable".to_string())?;
    let guest_gate: GuestSafeModeGate = serde_json::from_str(&guest_gate_text)
        .map_err(|_| "guest safe-mode marker is malformed".to_string())?;
    if guest_gate.schema_version != SAFE_MODE_GATE_SCHEMA_VERSION
        || !valid_incident_id(&guest_gate.incident_id)
        || !valid_distro(&guest_gate.distro)
    {
        return Err("guest safe-mode identity is invalid".into());
    }
    if guest_gate.boot_id != boot_id {
        return Err("safe-mode marker does not belong to the current boot".into());
    }
    let host_gate_path = paths
        .host_gate_dir
        .join(format!("{}.json", guest_gate.distro));
    let pending_path = &paths.pending;
    let host_gate = match fs::read_to_string(&host_gate_path) {
        Ok(text) => Some(
            serde_json::from_str::<HostSafeModeGate>(&text)
                .map_err(|_| "Windows safe-mode gate is malformed".to_string())?,
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return Err("Windows safe-mode gate is unavailable".into()),
    };
    let pending = match fs::read_to_string(pending_path) {
        Ok(text) => Some(
            serde_json::from_str::<RecoveryTransaction>(&text)
                .map_err(|_| "recovery transaction is malformed".to_string())?,
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return Err("recovery transaction is unavailable".into()),
    };
    // The transaction is the durable resume record; the lease is the short
    // boot-ephemeral authorization that heavy units actually read. They are
    // different documents and never share a shape.
    let new_transaction = || RecoveryTransaction {
        schema_version: RECOVERY_TRANSACTION_SCHEMA_VERSION,
        boot_id: boot_id.to_string(),
        incident_id: guest_gate.incident_id.clone(),
        distro: guest_gate.distro.clone(),
        supervisor_identity: supervisor.supervisor_identity.clone(),
        issued_at_epoch_ms: now_ms,
        expires_at_epoch_ms: now_ms.saturating_add(MAX_CONTROL_EVIDENCE_AGE_MS),
    };
    if let Some(record) = host_gate.as_ref() {
        if record.schema_version != SAFE_MODE_GATE_SCHEMA_VERSION
            || record.incident_id != guest_gate.incident_id
            || record.distro != guest_gate.distro
            || record.prior_boot_id.is_empty()
            || record.prior_boot_id == boot_id
            || record.reason.is_empty()
            || record.timestamp_utc.is_empty()
        {
            return Err("Windows and guest safe-mode identities do not match".into());
        }
        let transaction = new_transaction();
        write_atomic_durable(
            pending_path,
            format!(
                "{}\n",
                serde_json::to_string(&transaction).map_err(|error| error.to_string())?
            )
            .as_bytes(),
        )?;
    } else {
        let Some(transaction) = pending.as_ref() else {
            return Err(
                "matching Windows safe-mode gate or recovery transaction is unavailable".into(),
            );
        };
        validate_recovery_transaction(
            transaction,
            &current,
            &supervisor,
            now_ms,
            &SystemOwnerProbe,
        )?;
        if transaction.incident_id != guest_gate.incident_id
            || transaction.distro != guest_gate.distro
            || transaction.boot_id != boot_id
        {
            return Err("recovery transaction identity is invalid".into());
        }
    }
    mint_host_resume_lease(
        &paths.resume_lease,
        boot_id,
        HostResumeLeaseSource::SupervisorResume,
        hardware_health,
    )?;
    if host_gate.is_some() {
        fs::remove_file(&host_gate_path)
            .map_err(|error| format!("remove matching Windows safe-mode gate: {error}"))?;
    }
    fs::remove_file(&paths.guest_gate)
        .map_err(|error| format!("remove guest safe-mode gate: {error}"))?;
    fs::remove_file(pending_path).map_err(|error| error.to_string())
}

pub fn recover_resume() -> Result<(), String> {
    recover_resume_at(&RecoveryPaths {
        supervisor_state: PathBuf::from("/run/ramshared/supervisor-state.json"),
        boot_id: PathBuf::from("/proc/sys/kernel/random/boot_id"),
        guest_gate: PathBuf::from(SAFE_MODE_FILE),
        host_gate_dir: PathBuf::from("/mnt/c/ProgramData/RamShared/safe-mode"),
        pending: PathBuf::from(RECOVERY_PENDING_FILE),
        resume_lease: PathBuf::from(RESUME_LEASE_FILE),
        hardware_health: PathBuf::from("/run/ramshared/hardware-health.json"),
    })
}
