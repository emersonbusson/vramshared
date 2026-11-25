// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/workload/identity.rs
//! Boot-bound process identity, supervisor admission evidence, and hardware resume leases.

use super::{
    HOST_RESUME_LEASE_SCHEMA_VERSION, MAX_CONTROL_EVIDENCE_AGE_MS, NONCE_COUNTER,
    RECOVERY_TRANSACTION_SCHEMA_VERSION, SUPERVISOR_STATUS_SCHEMA_VERSION, write_atomic_durable,
};
use crate::supervisor::{SupervisorActionResult, SupervisorActionStatus};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OwnerIdentity {
    pub boot_id: String,
    pub pid: u32,
    pub start_time: u64,
    pub nonce: String,
}

/// Read this boot's kernel boot ID. Every boot-bound identity and the
/// host-resume lease bind to this value; it is the single reader for it.
pub(crate) fn current_boot_id() -> Result<String, String> {
    read_proc_boot_id_at(Path::new("/proc"), Path::new("sys/kernel/random/boot_id"))
}

#[cfg(test)]
fn boot_id_file(path: &Path) -> Result<std::fs::File, String> {
    crate::release_identity::open_regular_no_follow(path)
        .ok_or_else(|| "boot identity unavailable".to_string())
}

pub(super) fn read_proc_boot_id_at(root: &Path, member: &Path) -> Result<String, String> {
    read_proc_boot_id_at_with(root, member, || {})
}

pub(super) fn read_proc_boot_id_at_with(
    root: &Path,
    member: &Path,
    after_read: impl FnOnce(),
) -> Result<String, String> {
    use rustix::fs::{CWD, FileType, Mode, OFlags, ResolveFlags, fstat, openat2};
    let open_root = || {
        openat2(
            CWD,
            root,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
        )
        .map_err(|_| "proc root unavailable or redirected".to_string())
    };
    let proc_root = open_root()?;
    let original = proc_root_identity(&proc_root)?;
    let descriptor = openat2(
        &proc_root,
        member,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
        ResolveFlags::BENEATH
            | ResolveFlags::NO_XDEV
            | ResolveFlags::NO_SYMLINKS
            | ResolveFlags::NO_MAGICLINKS,
    )
    .map_err(|_| "boot identity unavailable or redirected".to_string())?;
    if FileType::from_raw_mode(
        fstat(&descriptor)
            .map_err(|_| "boot identity metadata unavailable".to_string())?
            .st_mode,
    ) != FileType::RegularFile
    {
        return Err("boot identity is not a regular file".into());
    }
    let file = std::fs::File::from(descriptor);
    if rustix::fs::fstatfs(&file)
        .map_err(|_| "boot identity filesystem unavailable".to_string())?
        .f_type
        != rustix::fs::PROC_SUPER_MAGIC
    {
        return Err("boot identity is not on procfs".into());
    }
    let id = read_boot_id_file(file)?;
    after_read();
    if proc_root_identity(&proc_root)? != original || proc_root_identity(&open_root()?)? != original
    {
        return Err("proc root changed during boot identity read".into());
    }
    Ok(id)
}

fn proc_root_identity(fd: &std::os::fd::OwnedFd) -> Result<(u32, u32, u64, u64), String> {
    use rustix::fs::{AtFlags, StatxFlags, statx};
    if rustix::fs::fstatfs(fd)
        .map_err(|_| "proc root filesystem unavailable".to_string())?
        .f_type
        != rustix::fs::PROC_SUPER_MAGIC
    {
        return Err("process root is not procfs".into());
    }
    let required = StatxFlags::INO | StatxFlags::MNT_ID;
    let stat = statx(
        fd,
        "",
        AtFlags::EMPTY_PATH | AtFlags::STATX_DONT_SYNC,
        required,
    )
    .map_err(|_| "proc root identity unavailable".to_string())?;
    if !StatxFlags::from_bits_retain(stat.stx_mask).contains(required)
        || stat.stx_ino == 0
        || stat.stx_mnt_id == 0
    {
        return Err("proc root identity unavailable".into());
    }
    Ok((
        stat.stx_dev_major,
        stat.stx_dev_minor,
        stat.stx_ino,
        stat.stx_mnt_id,
    ))
}

#[cfg(test)]
pub(super) fn read_boot_id_at(path: &Path) -> Result<String, String> {
    read_boot_id_file(boot_id_file(path)?)
}

fn read_boot_id_file(file: std::fs::File) -> Result<String, String> {
    let bytes = crate::release_identity::read_file_bytes(file, 37)
        .ok_or_else(|| "boot identity unavailable or oversized".to_string())?;
    let text = std::str::from_utf8(&bytes).map_err(|_| "boot identity is not UTF-8".to_string())?;
    let id = text.strip_suffix('\n').unwrap_or(text);
    if !valid_boot_id(id) {
        return Err("boot identity has invalid format".into());
    }
    Ok(id.to_owned())
}

pub(crate) fn valid_boot_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
        })
}

impl OwnerIdentity {
    pub(crate) fn current() -> Result<Self, String> {
        let pid = std::process::id();
        Ok(Self {
            boot_id: current_boot_id()?,
            pid,
            start_time: process_start_time(pid).ok_or("read process start time")?,
            nonce: unique_nonce(),
        })
    }
}

pub(crate) fn process_start_time(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(") ")?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

pub(super) fn unique_nonce() -> String {
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let sequence = NONCE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:x}-{:x}-{:x}", std::process::id(), time, sequence)
}

pub(super) trait OwnerProbe {
    fn is_alive(&self, owner: &OwnerIdentity) -> bool;
}

pub(super) struct SystemOwnerProbe;

impl OwnerProbe for SystemOwnerProbe {
    fn is_alive(&self, owner: &OwnerIdentity) -> bool {
        let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").ok();
        boot.as_deref().map(str::trim) == Some(owner.boot_id.as_str())
            && process_start_time(owner.pid) == Some(owner.start_time)
    }
}

pub(super) fn unix_time_ms() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .map_err(|error| format!("system clock is before the Unix epoch: {error}"))
}

pub(super) fn path_is_present(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

pub(super) fn valid_incident_id(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(crate) fn valid_systemd_invocation_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

pub(super) fn valid_distro(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

pub(super) fn identity_is_current(
    identity: &OwnerIdentity,
    boot_id: &str,
    probe: &dyn OwnerProbe,
) -> bool {
    identity.boot_id == boot_id
        && identity.pid != 0
        && identity.start_time != 0
        && !identity.nonce.is_empty()
        && identity
            .nonce
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
        && probe.is_alive(identity)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub(super) struct SupervisorStatus {
    pub(super) schema_version: u32,
    pub(super) control_state: String,
    pub(super) healthy_samples: u64,
    pub(super) action_results: Vec<SupervisorActionResult>,
    pub(super) supervisor_identity: OwnerIdentity,
    pub(super) written_at_unix_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct AdmissionState {
    pub(super) schema_version: u32,
    pub(super) admission_open: bool,
    pub(super) control_state: String,
    pub(super) supervisor_identity: OwnerIdentity,
    pub(super) written_at_unix_ms: u64,
}

/// The control state of the last published supervisor evidence, when that
/// evidence is still fresh enough to describe the present.
///
/// A replacing control plane uses this to tell two very different starts
/// apart. No record, or a record older than `MAX_CONTROL_EVIDENCE_AGE_MS`,
/// is a real reboot or a clean first start: there is nothing to resume.
/// A fresh record whose control state is not `HEALTHY` means the process
/// just replaced a supervisor that was already closing admission, and that
/// close must not be undone by the replacement opening on its first sample.
///
/// Identity is deliberately not checked here. The resuming process is by
/// definition a different owner than the one that wrote the record; the
/// question is only whether the published pressure state is still current.
pub(crate) fn last_fresh_control_state_at(path: &Path, now_ms: u64) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let status: SupervisorStatus = serde_json::from_str(&text).ok()?;
    if status.schema_version != SUPERVISOR_STATUS_SCHEMA_VERSION
        || status.written_at_unix_ms > now_ms
        || now_ms.saturating_sub(status.written_at_unix_ms) > MAX_CONTROL_EVIDENCE_AGE_MS
    {
        return None;
    }
    Some(status.control_state)
}

pub(super) fn read_current_supervisor_status_at(
    path: &Path,
    current: &OwnerIdentity,
    now_ms: u64,
    minimum_healthy_samples: u64,
    probe: &dyn OwnerProbe,
) -> Result<SupervisorStatus, String> {
    let text = fs::read_to_string(path)
        .map_err(|_| "supervisor health evidence is unavailable".to_string())?;
    let status: SupervisorStatus = serde_json::from_str(&text)
        .map_err(|_| "supervisor health evidence is malformed".to_string())?;
    if status.schema_version != SUPERVISOR_STATUS_SCHEMA_VERSION
        || status.written_at_unix_ms > now_ms
        || now_ms.saturating_sub(status.written_at_unix_ms) > MAX_CONTROL_EVIDENCE_AGE_MS
        || !identity_is_current(&status.supervisor_identity, &current.boot_id, probe)
        || status
            .action_results
            .iter()
            .any(|result| match result.status {
                SupervisorActionStatus::Succeeded => result.error.is_some(),
                SupervisorActionStatus::Failed => result.error.is_none(),
            })
    {
        return Err("supervisor health evidence is stale, foreign, or invalid".into());
    }
    if status.control_state != "HEALTHY" {
        return Err(format!(
            "workload admission is closed by control state {}",
            status.control_state
        ));
    }
    if status.healthy_samples < minimum_healthy_samples {
        return Err("supervisor health evidence has too few healthy samples".into());
    }
    Ok(status)
}

/// Who authorized heavy guest units on this boot. Unknown issuers are
/// rejected: a lease is only as good as the gate that minted it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HostResumeLeaseSource {
    /// `ramshared-host-gate.service` after a fresh sealed guardian proof.
    FreshSealedGuardianProof,
    /// `ramshared recover --resume` after the recovery window closes.
    SupervisorResume,
}

/// Hardware verdict carried by the host-resume lease.
///
/// Only the classifier's two clean outcomes can be minted; a `refused_*`
/// verdict never reaches the lease. The distinction is kept because an
/// overridden boot is auditable after the fact (`clean_override`), not
/// laundered into `clean`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HostHardwareHealth {
    /// No MCE/SIGBUS/unclean evidence in the window.
    Clean,
    /// The evidence exists but was reviewed up to `cleared_through_ms`.
    CleanOverride,
}

/// On-disk host-resume lease: the boot-ephemeral authorization that heavy
/// guest units require (pressure-incident DT-11).
///
/// Its lifetime is **this boot**, expressed by `boot_id`, not by a wall-clock
/// deadline: `/run` clears at reboot and a lease bound to another boot is
/// dead. That is why there is no expiry field. `host_gate::LeaseToken` is the
/// in-memory broker token and is not this file.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct HostResumeLease {
    pub schema_version: u32,
    pub boot_id: String,
    pub source: HostResumeLeaseSource,
    pub hardware_health: HostHardwareHealth,
}

/// Refusal text is `&'static str` so `boot` can surface it unchanged as
/// `HostPrerequisite` (NFR-6) without rewording at the boundary.
pub(crate) type HostResumeLeaseError = &'static str;

/// Verify the host-resume lease is present, well-formed, and bound to this
/// boot. This is the one validity rule for the file; every consumer uses it.
pub(crate) fn verify_host_resume_lease_at(
    path: &Path,
    current_boot_id: &str,
) -> Result<HostResumeLease, HostResumeLeaseError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err("host-resume lease is missing");
        }
        Err(_) => return Err("host-resume lease is unreadable"),
    };
    let lease: HostResumeLease = serde_json::from_str(&text)
        .map_err(|_| "host-resume lease is malformed" as HostResumeLeaseError)?;
    if lease.schema_version != HOST_RESUME_LEASE_SCHEMA_VERSION {
        return Err("host-resume lease is malformed");
    }
    if lease.boot_id != current_boot_id {
        return Err("host-resume lease is not bound to this boot");
    }
    Ok(lease)
}

/// Mint the boot-ephemeral authorization. Minting is `ramshared-host-gate.service`
/// (normal boot) or `recover --resume` (after an incident); nothing else may.
pub(crate) fn mint_host_resume_lease(
    path: &Path,
    boot_id: &str,
    source: HostResumeLeaseSource,
    hardware_health: HostHardwareHealth,
) -> Result<(), String> {
    let lease = HostResumeLease {
        schema_version: HOST_RESUME_LEASE_SCHEMA_VERSION,
        boot_id: boot_id.to_string(),
        source,
        hardware_health,
    };
    write_atomic_durable(
        path,
        format!(
            "{}\n",
            serde_json::to_string(&lease).map_err(|error| error.to_string())?
        )
        .as_bytes(),
    )
}

/// Read the lease if present. `None` means absent; `Err` means present but
/// not admissible. Callers that need it decide which of the two refuses.
pub(crate) fn read_host_resume_lease_if_present(
    path: &Path,
    current_boot_id: &str,
) -> Result<Option<HostResumeLease>, HostResumeLeaseError> {
    match verify_host_resume_lease_at(path, current_boot_id) {
        Ok(lease) => Ok(Some(lease)),
        Err("host-resume lease is missing") => Ok(None),
        Err(error) => Err(error),
    }
}

/// Read the hardware classifier's verdict for this boot.
///
/// `recover --resume` is the second minter of the host-resume lease. It must
/// not clear an MCE/SIGBUS event and must not re-classify with a cheaper rule
/// of its own, so it reads the same document the host gate reads. Absence is
/// a refusal, never an implicit clean; the document is bounded
/// and boot-bound, like the lease itself.
pub(crate) fn read_hardware_health_at(
    path: &Path,
    current_boot_id: &str,
) -> Result<HostHardwareHealth, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err("hardware health proof is missing".into());
        }
        Err(_) => return Err("hardware health proof is unreadable".into()),
    };
    if !metadata.file_type().is_file() || metadata.len() == 0 || metadata.len() > 4096 {
        return Err("hardware health proof is malformed".into());
    }
    let text = fs::read_to_string(path).map_err(|_| "hardware health proof is unreadable")?;
    let document: serde_json::Value =
        serde_json::from_str(&text).map_err(|_| "hardware health proof is malformed")?;
    if document
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        != Some(1)
        || document.get("boot_id").and_then(serde_json::Value::as_str) != Some(current_boot_id)
    {
        return Err("hardware health proof is not bound to this boot".into());
    }
    let count = |key: &str| -> Result<u64, String> {
        document
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| "hardware health proof is malformed".to_string())
    };
    let mce = count("mce_kill_count")?;
    let sigbus = count("sigbus_mceerr_count")?;
    let marker = document
        .get("unclean_marker_present")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| "hardware health proof is malformed".to_string())?;
    match document.get("verdict").and_then(serde_json::Value::as_str) {
        Some("clean") if mce == 0 && sigbus == 0 && !marker => Ok(HostHardwareHealth::Clean),
        // A document that says `clean` while carrying the events it claims to
        // have cleared is contradictory, not clean.
        Some("clean") => Err("hardware health proof contradicts its own counts".into()),
        Some("clean_override") => Ok(HostHardwareHealth::CleanOverride),
        Some(verdict @ ("refused_mce" | "refused_sigbus" | "refused_unclean")) => {
            Err(format!("hardware health is refused: {verdict}"))
        }
        _ => Err("hardware health proof is malformed".into()),
    }
}

/// Durable record of an in-flight `recover --resume`. Kept at
/// `/var/lib/ramshared/recovery-pending.json` and consumed when the resume
/// completes — this is **not** the host-resume lease, which is the short
/// boot-ephemeral document above.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct RecoveryTransaction {
    pub(super) schema_version: u32,
    pub(super) boot_id: String,
    pub(super) incident_id: String,
    pub(super) distro: String,
    pub(super) supervisor_identity: OwnerIdentity,
    pub(super) issued_at_epoch_ms: u64,
    pub(super) expires_at_epoch_ms: u64,
}

pub(super) fn validate_recovery_transaction(
    transaction: &RecoveryTransaction,
    current: &OwnerIdentity,
    supervisor: &SupervisorStatus,
    now_ms: u64,
    probe: &dyn OwnerProbe,
) -> Result<(), String> {
    if transaction.schema_version != RECOVERY_TRANSACTION_SCHEMA_VERSION
        || transaction.boot_id != current.boot_id
        || !valid_incident_id(&transaction.incident_id)
        || !valid_distro(&transaction.distro)
        || transaction.supervisor_identity != supervisor.supervisor_identity
        || !identity_is_current(&transaction.supervisor_identity, &current.boot_id, probe)
        || transaction.issued_at_epoch_ms > now_ms
        || transaction.expires_at_epoch_ms < now_ms
        || transaction
            .expires_at_epoch_ms
            .saturating_sub(transaction.issued_at_epoch_ms)
            > MAX_CONTROL_EVIDENCE_AGE_MS
        || now_ms.saturating_sub(transaction.issued_at_epoch_ms) > MAX_CONTROL_EVIDENCE_AGE_MS
    {
        return Err("recovery transaction is stale, foreign, or invalid".into());
    }
    Ok(())
}
