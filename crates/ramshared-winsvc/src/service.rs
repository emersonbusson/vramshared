// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/service.rs
//! Provision helpers and Gate A/B teardown (SPEC DT-8 / DT-9 / ITEM-5).
//!
//! Pure sequencing with injectable backends so Linux unit tests cover pagefile gates.
//! Product phase ownership lives in [`crate::runtime`]; this module keeps the
//! co-residency path and authoritative two-gate stop frontier.

use crate::broker_tenant::{BrokerTenant, BrokerTenantError, LeaseState};
use crate::config::WinDriveConfig;
use crate::runtime::{RuntimeError, RuntimeErrorClass};

/// Read-only identity observed for a mounted volume before teardown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedVolumeIdentity {
    pub letter: char,
    pub vendor: String,
    pub product: String,
    pub serial: String,
    pub size_bytes: u64,
}

/// Parse the exact product prefix emitted by Windows storage surfaces.
///
/// `Get-Disk` may append the standard `SCSI Disk Device` class suffix to the
/// INQUIRY vendor/product pair. No other suffix is accepted.
pub fn parse_product_friendly_name(name: &str) -> Result<(String, String), String> {
    let fields: Vec<&str> = name.split_whitespace().collect();
    let prefix_matches = fields.len() >= 2
        && fields[0].eq_ignore_ascii_case("RAMSHARE")
        && fields[1].eq_ignore_ascii_case("VRAMDISK");
    let suffix_matches = fields.len() == 2
        || (fields.len() == 5
            && fields[2].eq_ignore_ascii_case("SCSI")
            && fields[3].eq_ignore_ascii_case("Disk")
            && fields[4].eq_ignore_ascii_case("Device"));
    if !prefix_matches || !suffix_matches {
        return Err("unexpected product friendly name".into());
    }
    Ok(("RAMSHARE".into(), "VRAMDISK".into()))
}

/// Exact product identity required to select a teardown volume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeardownTarget {
    letter: char,
    serial: String,
    size_bytes: u64,
}

impl TeardownTarget {
    pub fn new(letter: char, serial: impl Into<String>, size_bytes: u64) -> Result<Self, String> {
        let letter = letter.to_ascii_uppercase();
        let serial = serial.into();
        if !('D'..='Z').contains(&letter) {
            return Err("teardown letter must be D..=Z".into());
        }
        if serial.len() != 16 || !serial.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("teardown serial must be 16 hexadecimal characters".into());
        }
        if size_bytes == 0 {
            return Err("teardown size must be non-zero".into());
        }
        Ok(Self {
            letter,
            serial,
            size_bytes,
        })
    }

    pub fn verify_unique(&self, observed: &[ObservedVolumeIdentity]) -> Result<char, String> {
        let mut matches = observed.iter().filter(|identity| {
            identity.letter.to_ascii_uppercase() == self.letter
                && identity.vendor.trim() == "RAMSHARE"
                && identity.product.trim() == "VRAMDISK"
                && identity.serial.eq_ignore_ascii_case(&self.serial)
                && identity.size_bytes == self.size_bytes
        });
        let Some(first) = matches.next() else {
            return Err("product volume identity missing or mismatched".into());
        };
        if matches.next().is_some() {
            return Err("product volume identity is ambiguous".into());
        }
        Ok(first.letter.to_ascii_uppercase())
    }
}

/// Teardown sub-phases for structured logging (SPEC observability).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TeardownPhase {
    Identity,
    GateA,
    Drain,
    VolumeLock,
    GateB,
    FlushDismount,
    Unregister,
    Destroy,
    Unlock,
    Wipe,
    Release,
    ResumeOnline,
}

/// High-level service state (no live handles on Linux).
#[derive(Clone, Debug, Default)]
pub struct ServiceState {
    pub lease: Option<LeaseState>,
    pub disk_created: bool,
    pub registered_queue: bool,
    /// Online after successful provision (storage-only product never sets pagefile hot).
    pub online: bool,
}

/// Result of a provision attempt.
#[derive(Debug, PartialEq)]
pub enum ProvisionError {
    Broker(BrokerTenantError),
    Config(String),
    Disk(String),
    PagefileSafety(String),
    Runtime(RuntimeError),
}

impl std::fmt::Display for ProvisionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProvisionError::Broker(e) => write!(f, "broker: {e}"),
            ProvisionError::Config(s) => write!(f, "config: {s}"),
            ProvisionError::Disk(s) => write!(f, "disk: {s}"),
            ProvisionError::PagefileSafety(s) => write!(f, "pagefile safety: {s}"),
            ProvisionError::Runtime(e) => write!(f, "runtime: {e}"),
        }
    }
}

impl std::error::Error for ProvisionError {}

/// Injectable free-VRAM probe (DT-20 / co-residency).
pub trait FreeVram {
    fn free_bytes(&self) -> u64;
}

/// Injectable disk control (IOCTL CREATE/DESTROY/REGISTER).
pub trait DiskControl {
    fn create_disk(&mut self, size_bytes: u64, block_size: u32) -> Result<(), String>;
    fn destroy_disk(&mut self) -> Result<(), String>;
    fn register_queue(&mut self) -> Result<(), String>;
    fn unregister_queue(&mut self) -> Result<(), String>;
}

/// Wipe VRAM after destroy.
pub trait WipeVram {
    fn zero(&mut self) -> Result<(), String>;
}

/// Authoritative OS pagefile / volume queries (DT-8).
pub trait PagefileGates {
    /// Read-only exact identity check. Any mismatch or ambiguity is unsafe.
    fn verify_volume_identity(&self, letter: char) -> Result<(), String>;
    /// Gate A/B: list active pagefile identities. Err = query unsafe (fail-closed).
    fn active_pagefiles(&self) -> Result<Vec<String>, String>;
    /// RAW product disks have no filesystem volume to lock or dismount.
    fn has_mounted_volume(&self) -> bool {
        true
    }
    /// Exclusive volume lock. Err = lock failure.
    fn lock_volume(&mut self, letter: char) -> Result<(), String>;
    fn unlock_volume(&mut self) -> Result<(), String>;
    fn flush_and_dismount(&mut self) -> Result<(), String>;
    fn volume_locked(&self) -> bool;
}

/// Apply co-residency gate then CREATE + REGISTER (unit-testable path).
pub fn provision_after_lease(
    cfg: &WinDriveConfig,
    state: &mut ServiceState,
    lease: LeaseState,
    free: &dyn FreeVram,
    disk: &mut dyn DiskControl,
    tenant: &mut BrokerTenant,
) -> Result<(), ProvisionError> {
    state.lease = Some(lease.clone());
    if let Err(e) = tenant.coresidence_gate(free.free_bytes(), cfg.size_bytes) {
        tenant.clear_lease();
        state.lease = None;
        return Err(ProvisionError::Broker(e));
    }
    disk.create_disk(cfg.size_bytes, cfg.block_size)
        .map_err(ProvisionError::Disk)?;
    state.disk_created = true;
    disk.register_queue().map_err(ProvisionError::Disk)?;
    state.registered_queue = true;
    state.online = true;
    Ok(())
}

/// Exact identity → Gate A → drain → lock → Gate B → flush/dismount → unregister → destroy → unlock → wipe.
///
/// On Gate A failure / Gate B failure / lock failure: resume Online, no destructive effects (code 7).
pub fn teardown_storage_only(
    cfg: &WinDriveConfig,
    state: &mut ServiceState,
    disk: &mut dyn DiskControl,
    wipe: &mut dyn WipeVram,
    gates: &mut dyn PagefileGates,
    phases: &mut Vec<TeardownPhase>,
) -> Result<(), ProvisionError> {
    if !state.online && !state.disk_created && !state.registered_queue {
        return Ok(());
    }

    phases.push(TeardownPhase::Identity);
    gates
        .verify_volume_identity(cfg.volume_letter)
        .map_err(|e| ProvisionError::PagefileSafety(format!("volume_identity: {e}")))?;

    // Gate A — before any runtime mutation past Online stop intent.
    phases.push(TeardownPhase::GateA);
    let pagefiles = gates
        .active_pagefiles()
        .map_err(|e| ProvisionError::PagefileSafety(format!("gate_a_query: {e}")))?;
    if !pagefiles.is_empty() {
        phases.push(TeardownPhase::ResumeOnline);
        return Err(ProvisionError::PagefileSafety(format!(
            "gate_a_active: {}",
            pagefiles.join(",")
        )));
    }

    phases.push(TeardownPhase::Drain);
    // Drain is caller/runtime responsibility for live I/O; marker only here.

    if gates.has_mounted_volume() {
        phases.push(TeardownPhase::VolumeLock);
        if let Err(e) = gates.lock_volume(cfg.volume_letter) {
            phases.push(TeardownPhase::ResumeOnline);
            return Err(ProvisionError::PagefileSafety(format!("volume_lock: {e}")));
        }
    }

    phases.push(TeardownPhase::GateB);
    match gates.active_pagefiles() {
        Ok(pf) if pf.is_empty() => {}
        Ok(pf) => {
            let _ = gates.unlock_volume();
            phases.push(TeardownPhase::ResumeOnline);
            return Err(ProvisionError::PagefileSafety(format!(
                "gate_b_active: {}",
                pf.join(",")
            )));
        }
        Err(e) => {
            let _ = gates.unlock_volume();
            phases.push(TeardownPhase::ResumeOnline);
            return Err(ProvisionError::PagefileSafety(format!("gate_b_query: {e}")));
        }
    }

    if gates.has_mounted_volume() {
        phases.push(TeardownPhase::FlushDismount);
        if let Err(e) = gates.flush_and_dismount() {
            let _ = gates.unlock_volume();
            phases.push(TeardownPhase::ResumeOnline);
            return Err(ProvisionError::PagefileSafety(format!(
                "flush_dismount: {e}"
            )));
        }
    }

    // Destructive frontier — only while volume remains locked.
    if state.registered_queue {
        phases.push(TeardownPhase::Unregister);
        disk.unregister_queue().map_err(ProvisionError::Disk)?;
        state.registered_queue = false;
    }
    if state.disk_created {
        phases.push(TeardownPhase::Destroy);
        disk.destroy_disk().map_err(ProvisionError::Disk)?;
        state.disk_created = false;
    }

    if gates.has_mounted_volume() {
        phases.push(TeardownPhase::Unlock);
        gates
            .unlock_volume()
            .map_err(|e| ProvisionError::Disk(format!("unlock: {e}")))?;
    }

    phases.push(TeardownPhase::Wipe);
    wipe.zero().map_err(ProvisionError::Disk)?;

    phases.push(TeardownPhase::Release);
    // LeaseRelease is caller's responsibility on the broker stream after this returns.
    state.lease = None;
    state.online = false;
    Ok(())
}

/// Map pagefile safety refusal to runtime code 7.
pub fn pagefile_refusal_to_runtime(err: &ProvisionError) -> Option<RuntimeError> {
    match err {
        ProvisionError::PagefileSafety(s) => Some(RuntimeError::new(
            RuntimeErrorClass::PagefileSafety,
            7,
            s.clone(),
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
