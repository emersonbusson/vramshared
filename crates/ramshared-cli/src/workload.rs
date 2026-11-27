// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/workload.rs
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use crate::bounded_process;
use crate::supervisor::valid_scope_unit;

mod identity;
mod ledger;
mod ledger_lock;
mod recovery;
mod systemd;

pub use identity::OwnerIdentity;
#[allow(unused_imports)]
use identity::{
    AdmissionState, OwnerProbe, RecoveryTransaction, SupervisorStatus, SystemOwnerProbe,
    identity_is_current, path_is_present, read_current_supervisor_status_at, unique_nonce,
    unix_time_ms, valid_distro, valid_incident_id, validate_recovery_transaction,
};
#[cfg(test)]
#[allow(unused_imports)]
pub(crate) use identity::{HostHardwareHealth, HostResumeLease, HostResumeLeaseError};
#[allow(unused_imports)]
pub(crate) use identity::{
    HostResumeLeaseSource, current_boot_id, last_fresh_control_state_at, mint_host_resume_lease,
    process_start_time, read_hardware_health_at, read_host_resume_lease_if_present, valid_boot_id,
    valid_systemd_invocation_id, verify_host_resume_lease_at,
};
#[cfg(test)]
use ledger::acquire_reservation_at;
#[cfg(test)]
#[allow(unused_imports)]
pub(crate) use ledger::{LEDGER_BUSY_SIGNATURE, publish_admission_state_at};
#[allow(unused_imports)]
use ledger::{
    admit_reservation_at, bind_reservation_invocation_while_locked, release_reservation_at,
    validate_ledger,
};
pub(crate) use ledger::{
    is_ledger_busy, publish_admission_state_at_with_transient_retry, read_reservation_ledger,
};
pub use ledger_lock::Reservation;
#[allow(unused_imports)]
use ledger_lock::{
    LedgerLock, LedgerPaths, ReservationLedger, acquire_lock, named_identity,
    open_or_create_regular, quarantine_snapshot, validate_regular,
};
pub use recovery::{recover_resume, recover_status};
#[allow(unused_imports)]
use systemd::{ScopePresence, ScopeStatusAdapter, run_with};

pub(super) const GIB_BYTES: u64 = 1024 * 1024 * 1024;
pub(super) const MIB_BYTES: u64 = 1024 * 1024;
pub(super) const DEFAULT_LEDGER_DIR: &str = "/run/ramshared/admission";
pub(super) const SAFE_MODE_FILE: &str = "/var/lib/ramshared/safe-mode.json";
pub(super) const RESUME_LEASE_FILE: &str = "/run/ramshared/host-resume-lease.json";
pub(super) const RECOVERY_PENDING_FILE: &str = "/var/lib/ramshared/recovery-pending.json";
pub(super) const SUPERVISOR_STATUS_SCHEMA_VERSION: u32 = 3;
pub(super) const SAFE_MODE_GATE_SCHEMA_VERSION: u32 = 1;
pub(super) const HOST_RESUME_LEASE_SCHEMA_VERSION: u32 = 2;
pub(super) const RECOVERY_TRANSACTION_SCHEMA_VERSION: u32 = 2;
pub(super) const ADMISSION_STATE_SCHEMA_VERSION: u32 = 1;
pub(crate) const RESERVATION_LEDGER_SCHEMA_VERSION: u32 = 3;
pub(super) const MAX_LEDGER_BYTES: u64 = 1024 * 1024;
pub(super) const MAX_CONTROL_EVIDENCE_AGE_MS: u64 = 15_000;
/// How long the identity bind may take to observe a start `InvocationID`.
/// This budget is about the control surface (systemd answering), never about
/// the workload: `acknowledge_start` does not wait on the runner child, so a
/// command that runs for hours is not racing this deadline.
pub(super) const SCOPE_START_ACK_TIMEOUT: Duration = Duration::from_secs(2);
pub(super) const SCOPE_STATUS_QUERY_TIMEOUT: Duration = Duration::from_secs(2);
pub(super) const SCOPE_STATUS_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Race allowance after the runner child has already exited, for systemd to
/// report the scope terminal. The workload is over at this point; this is only
/// how long we tolerate a lagging unit-status projection before treating the
/// scope as uncontained.
pub(super) const SCOPE_TERMINAL_PROOF_TIMEOUT: Duration = Duration::from_secs(2);

/// A canonical synthetic `InvocationID` for status fixtures: 32 hex characters
/// in the shape systemd mints, and never a live identity.
#[cfg(test)]
pub(super) const FIXTURE_INVOCATION_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
pub(super) static NONCE_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(super) fn write_atomic_durable(path: &Path, contents: &[u8]) -> Result<(), String> {
    let parent = path.parent().ok_or("atomic file parent missing")?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temporary = parent.join(format!(".ramshared-{}.tmp", unique_nonce()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| error.to_string())?;
        file.write_all(contents)
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        fs::rename(&temporary, path).map_err(|error| error.to_string())?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| error.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkloadBudget {
    pub control_plane_reserve_bytes: u64,
    pub memory_high_bytes: u64,
    pub memory_max_bytes: u64,
}

pub fn calculate_budget(
    memory_total_bytes: u64,
    _memory_available_bytes: u64,
) -> Result<WorkloadBudget, String> {
    if memory_total_bytes == 0 {
        return Err("invalid memory snapshot".into());
    }
    let reserve = (memory_total_bytes.div_ceil(4)).max(4 * GIB_BYTES);
    let memory_max = memory_total_bytes.saturating_sub(reserve);
    if memory_max < GIB_BYTES {
        return Err(format!(
            "less than 1 GiB remains after the {} MiB control-plane reserve",
            reserve >> 20
        ));
    }
    let high_gap = (memory_total_bytes.div_ceil(10)).max(GIB_BYTES);
    Ok(WorkloadBudget {
        control_plane_reserve_bytes: reserve,
        memory_high_bytes: memory_max.saturating_sub(high_gap),
        memory_max_bytes: memory_max,
    })
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkloadClass {
    Interactive,
    Build,
    BrowserTest,
    Batch,
}

impl WorkloadClass {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "interactive" => Some(Self::Interactive),
            "build" => Some(Self::Build),
            "browser-test" => Some(Self::BrowserTest),
            "batch" => Some(Self::Batch),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Build => "build",
            Self::BrowserTest => "browser-test",
            Self::Batch => "batch",
        }
    }

    fn default_memory_mib(self) -> u64 {
        match self {
            Self::Interactive => 2 * 1024,
            Self::Build => 6 * 1024,
            Self::BrowserTest => 4 * 1024,
            Self::Batch => 8 * 1024,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkloadRequest {
    pub class: WorkloadClass,
    pub memory_max_bytes: u64,
    pub command: Vec<String>,
}

pub fn parse_run_args(args: &[String]) -> Result<WorkloadRequest, String> {
    let mut class = None;
    let mut requested_mib = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--class" => {
                index += 1;
                class = Some(
                    args.get(index)
                        .and_then(|value| WorkloadClass::parse(value))
                        .ok_or("--class must be interactive|build|browser-test|batch")?,
                );
            }
            "--memory-max" => {
                index += 1;
                requested_mib = Some(
                    args.get(index)
                        .ok_or("--memory-max requires MiB")?
                        .parse::<u64>()
                        .ok()
                        .filter(|value| *value > 0)
                        .ok_or("--memory-max must be a positive MiB value")?,
                );
            }
            "--" => {
                let class = class.ok_or("--class is required")?;
                let command = args[index + 1..].to_vec();
                if command.is_empty() {
                    return Err("a command is required after --".into());
                }
                let memory_mib = requested_mib.unwrap_or_else(|| class.default_memory_mib());
                return Ok(WorkloadRequest {
                    class,
                    memory_max_bytes: memory_mib
                        .checked_mul(MIB_BYTES)
                        .ok_or("--memory-max overflow")?,
                    command,
                });
            }
            _ => {
                return Err(
                    "usage: ramshared run --class <class> [--memory-max MiB] -- <command>".into(),
                );
            }
        }
        index += 1;
    }
    Err("usage: ramshared run --class <class> [--memory-max MiB] -- <command>".into())
}

pub fn run(args: &[String]) -> Result<(), String> {
    let request = parse_run_args(args)?;
    if !rustix::process::geteuid().is_root() {
        return Err(
            "workload admission requires root privileges to access the reservation ledger".into(),
        );
    }
    let meminfo = fs::read_to_string("/proc/meminfo").map_err(|error| error.to_string())?;
    run_with(
        request,
        &meminfo,
        &systemd::SystemScopeRunner,
        Path::new(DEFAULT_LEDGER_DIR),
        Path::new(SAFE_MODE_FILE),
        Path::new(RESUME_LEASE_FILE),
        Path::new("/run/ramshared/supervisor-state.json"),
    )
}

pub fn session(args: &[String]) -> Result<(), String> {
    let mut session_args = args.to_vec();
    if !session_args.iter().any(|item| item == "--") {
        session_args.push("--".into());
        session_args.push(std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()));
    }
    run(&session_args)
}

#[cfg(test)]
#[path = "workload/tests.rs"]
mod tests;
