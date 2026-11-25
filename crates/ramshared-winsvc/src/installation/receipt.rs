// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/installation/receipt.rs
//! Durable install receipt and native reconciliation (DT-14/15).
//!
//! The receipt is the product's own statement of what it installed: version,
//! commit, the exact root-device instance, the exact OEM INF and whether
//! Windows asked for a restart. `status` reconciles it against the live host
//! instead of assuming that "the files are present" means "the new driver is
//! loaded".
//!
//! The restart bit is the reason this module exists. A driver upgrade can
//! return success with `NeedReboot`: the package is staged, the old driver is
//! still bound, and calling that a successful reload is false. Reconciliation
//! reports `RestartPending` until a later boot, which is observed as a boot
//! epoch (wall clock minus uptime) greater than the one recorded at install.
//!
//! The decision table is pure; only [`observe`] reads Windows.

use crate::installation::driver_store::is_oem_inf_name;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::Path;

pub const RECEIPT_SCHEMA: u32 = 1;

/// What the product installed, in the product's own words.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallReceipt {
    pub schema: u32,
    pub version: String,
    pub commit: String,
    /// The exact owned device instance, for example `ROOT\SCSIADAPTER\0000`.
    pub root_instance: String,
    /// The exact published package, for example `oem38.inf`.
    pub oem_inf: String,
    /// The driver-store call asked for a restart before the driver is active.
    pub reboot_required: bool,
    /// Wall clock minus uptime at install time; a larger value means a reboot.
    pub boot_epoch_unix: u64,
}

/// The live facts reconciliation needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeSnapshot {
    pub root_instance: Option<String>,
    pub oem_inf_present: bool,
    pub boot_epoch_unix: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallState {
    /// The recorded device and package are present and no restart is pending.
    Active,
    /// Everything is present, but Windows asked for a restart before the
    /// published driver is loaded. This is not a successful reload.
    RestartPending,
    /// The recorded device or package is missing; the product cannot claim
    /// the tuple it installed.
    RecoveryRequired,
}

impl InstallState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::RestartPending => "restart_pending",
            Self::RecoveryRequired => "recovery_required",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReceiptError {
    Io(String),
    Invalid(String),
}

impl fmt::Display for ReceiptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(reason) => write!(f, "install receipt: {reason}"),
            Self::Invalid(reason) => write!(f, "install receipt: {reason}"),
        }
    }
}

impl std::error::Error for ReceiptError {}

/// Wall clock minus uptime, never negative.
pub fn boot_epoch_from(now_unix: u64, uptime_secs: u64) -> u64 {
    now_unix.saturating_sub(uptime_secs)
}

/// The one reconciliation rule.
///
/// A missing device or package is recovery work, not a pending restart: there
/// is nothing to wait for. A pending restart is only satisfied by a boot that
/// happened after the install recorded its boot epoch.
pub fn reconcile(receipt: &InstallReceipt, observed: &NativeSnapshot) -> InstallState {
    if observed.root_instance.as_deref() != Some(receipt.root_instance.as_str()) {
        return InstallState::RecoveryRequired;
    }
    if !observed.oem_inf_present {
        return InstallState::RecoveryRequired;
    }
    if receipt.reboot_required && observed.boot_epoch_unix <= receipt.boot_epoch_unix {
        return InstallState::RestartPending;
    }
    InstallState::Active
}

/// Reconcile only if the receipt names the manifest that is active now.
///
/// A crash between the active-manifest switch and the receipt write can leave
/// an older receipt naming a package that is still present. Reconciling that
/// receipt against the host would report `active` for a tuple the product no
/// longer runs, so the identity check comes first.
pub fn reconcile_active(
    receipt: &InstallReceipt,
    observed: &NativeSnapshot,
    version: &str,
    commit: &str,
) -> InstallState {
    if receipt.version != version || !receipt.commit.eq_ignore_ascii_case(commit) {
        return InstallState::RecoveryRequired;
    }
    reconcile(receipt, observed)
}

/// Observe the live host. The receipt names the package; the device is found
/// by the product's exact hardware-ID ownership, never by an instance prefix.
#[cfg(windows)]
pub fn observe(oem_inf: &str) -> NativeSnapshot {
    NativeSnapshot {
        root_instance: crate::installation::root_device::observe_owned_root()
            .ok()
            .flatten(),
        oem_inf_present: crate::installation::driver_store::windows_inf_dir()
            .join(oem_inf)
            .is_file(),
        boot_epoch_unix: current_boot_epoch(),
    }
}

/// The current boot epoch on Windows.
#[cfg(windows)]
pub fn current_boot_epoch() -> u64 {
    use windows_sys::Win32::System::SystemInformation::GetTickCount64;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    boot_epoch_from(now, unsafe { GetTickCount64() } / 1000)
}

pub fn read(path: &Path) -> Result<Option<InstallReceipt>, ReceiptError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ReceiptError::Io(format!("{}: {error}", path.display()))),
    };
    let receipt: InstallReceipt = serde_json::from_slice(&bytes)
        .map_err(|error| ReceiptError::Invalid(format!("{}: {error}", path.display())))?;
    if receipt.schema != RECEIPT_SCHEMA {
        return Err(ReceiptError::Invalid(format!(
            "{}: schema {} is not {RECEIPT_SCHEMA}",
            path.display(),
            receipt.schema
        )));
    }
    if receipt.version.is_empty() || receipt.commit.is_empty() {
        return Err(ReceiptError::Invalid(format!(
            "{}: version and commit are required",
            path.display()
        )));
    }
    if receipt.root_instance.is_empty() {
        return Err(ReceiptError::Invalid(format!(
            "{}: root instance is required",
            path.display()
        )));
    }
    if !is_oem_inf_name(&receipt.oem_inf) {
        return Err(ReceiptError::Invalid(format!(
            "{}: {} is not an exact oemNN.inf name",
            path.display(),
            receipt.oem_inf
        )));
    }
    Ok(Some(receipt))
}

/// Write the receipt so a crash cannot leave a half-written file: write a
/// sibling, then rename over the target.
pub fn write(path: &Path, receipt: &InstallReceipt) -> Result<(), ReceiptError> {
    let bytes = serde_json::to_vec_pretty(receipt)
        .map_err(|error| ReceiptError::Invalid(error.to_string()))?;
    let parent = path
        .parent()
        .ok_or_else(|| ReceiptError::Io(format!("{} has no parent directory", path.display())))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| ReceiptError::Io(format!("{}: {error}", parent.display())))?;
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, &bytes)
        .map_err(|error| ReceiptError::Io(format!("{}: {error}", temporary.display())))?;
    std::fs::rename(&temporary, path)
        .map_err(|error| ReceiptError::Io(format!("{}: {error}", path.display())))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn receipt(reboot_required: bool, boot_epoch_unix: u64) -> InstallReceipt {
        InstallReceipt {
            schema: RECEIPT_SCHEMA,
            version: "0.15.0".into(),
            commit: "abcdef1234567890".into(),
            root_instance: r"ROOT\SCSIADAPTER\0000".into(),
            oem_inf: "oem38.inf".into(),
            reboot_required,
            boot_epoch_unix,
        }
    }

    fn observed(boot_epoch_unix: u64) -> NativeSnapshot {
        NativeSnapshot {
            root_instance: Some(r"ROOT\SCSIADAPTER\0000".into()),
            oem_inf_present: true,
            boot_epoch_unix,
        }
    }

    fn receipt_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ramshared-install-receipt-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn driver_upgrade_reboot_required_is_not_successful_reload() {
        let receipt = receipt(true, 1_000);
        let state = reconcile(&receipt, &observed(1_000));
        assert_eq!(state, InstallState::RestartPending);
        assert_ne!(
            state,
            InstallState::Active,
            "a pending restart must never read as a successful reload"
        );
        assert_eq!(state.as_str(), "restart_pending");
    }

    #[test]
    fn restart_pending_clears_after_a_later_boot() {
        let receipt = receipt(true, 1_000);
        assert_eq!(
            reconcile(&receipt, &observed(1_001)),
            InstallState::Active,
            "a boot after the recorded epoch satisfies the pending restart"
        );
    }

    #[test]
    fn complete_install_without_a_reboot_is_active() {
        assert_eq!(
            reconcile(&receipt(false, 1_000), &observed(1_000)),
            InstallState::Active
        );
    }

    #[test]
    fn receipt_from_a_superseded_version_is_recovery_required() {
        let receipt = receipt(false, 1_000);
        assert_eq!(
            reconcile_active(&receipt, &observed(1_000), "0.15.0", "abcdef1234567890"),
            InstallState::Active
        );
        assert_eq!(
            reconcile_active(&receipt, &observed(1_000), "0.15.1", "abcdef1234567890"),
            InstallState::RecoveryRequired,
            "a receipt for another version cannot describe the active tuple"
        );
        assert_eq!(
            reconcile_active(&receipt, &observed(1_000), "0.15.0", "ABCDEF1234567890"),
            InstallState::Active,
            "commit hex case is not identity"
        );
    }

    #[test]
    fn missing_or_changed_device_requires_recovery() {
        let receipt = receipt(false, 1_000);
        let mut snapshot = observed(1_000);
        snapshot.root_instance = None;
        assert_eq!(
            reconcile(&receipt, &snapshot),
            InstallState::RecoveryRequired
        );
        snapshot.root_instance = Some(r"ROOT\RAMSHARED\0000".into());
        assert_eq!(
            reconcile(&receipt, &snapshot),
            InstallState::RecoveryRequired,
            "a different instance is not the recorded device"
        );
    }

    #[test]
    fn missing_oem_inf_requires_recovery() {
        let mut snapshot = observed(1_000);
        snapshot.oem_inf_present = false;
        assert_eq!(
            reconcile(&receipt(false, 1_000), &snapshot),
            InstallState::RecoveryRequired
        );
    }

    #[test]
    fn boot_epoch_never_underflows() {
        assert_eq!(boot_epoch_from(1_000, 400), 600);
        assert_eq!(boot_epoch_from(100, 400), 0);
    }

    #[test]
    fn every_install_state_has_a_stable_label() {
        assert_eq!(InstallState::Active.as_str(), "active");
        assert_eq!(InstallState::RestartPending.as_str(), "restart_pending");
        assert_eq!(InstallState::RecoveryRequired.as_str(), "recovery_required");
    }

    #[test]
    fn receipt_errors_name_the_reason() {
        let io = ReceiptError::Io("x: denied".into());
        let invalid = ReceiptError::Invalid("x: schema 9".into());
        assert_eq!(io.to_string(), "install receipt: x: denied");
        assert_eq!(invalid.to_string(), "install receipt: x: schema 9");
        let boxed: Box<dyn std::error::Error> = Box::new(io);
        assert!(boxed.to_string().contains("denied"));
    }

    #[test]
    fn receipt_round_trips_through_its_file() {
        let dir = receipt_dir("round-trip");
        let path = dir.join("install-receipt.json");
        let receipt = receipt(true, 1_000);
        write(&path, &receipt).unwrap();
        assert_eq!(read(&path).unwrap(), Some(receipt));
        assert_eq!(read(&dir.join("absent.json")).unwrap(), None);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "observes the live host; run explicitly on a provisioned machine"]
    fn live_observation_reconciles_the_installed_tuple() {
        let observed = observe("oem38.inf");
        let instance = observed
            .root_instance
            .clone()
            .expect("the owned root device must be present");
        assert!(observed.oem_inf_present, "oem38.inf must be published");
        let receipt = InstallReceipt {
            schema: RECEIPT_SCHEMA,
            version: "0.15.0".into(),
            commit: "abcdef1234567890".into(),
            root_instance: instance,
            oem_inf: "oem38.inf".into(),
            reboot_required: false,
            boot_epoch_unix: observed.boot_epoch_unix,
        };
        assert_eq!(reconcile(&receipt, &observed), InstallState::Active);
        eprintln!(
            "live tuple: instance={:?} oem38={} boot_epoch={}",
            observed.root_instance, observed.oem_inf_present, observed.boot_epoch_unix
        );
    }

    #[test]
    fn receipt_file_refuses_a_foreign_schema_or_an_unexact_name() {
        let dir = receipt_dir("strict");
        let path = dir.join("install-receipt.json");
        std::fs::write(&path, br#"{"schema":9,"version":"1","commit":"c","root_instance":"r","oem_inf":"oem1.inf","reboot_required":false,"boot_epoch_unix":1}"#).unwrap();
        assert!(matches!(read(&path).unwrap_err(), ReceiptError::Invalid(_)));
        std::fs::write(&path, br#"{"schema":1,"version":"1","commit":"c","root_instance":"r","oem_inf":"../oem1.inf","reboot_required":false,"boot_epoch_unix":1}"#).unwrap();
        assert!(matches!(read(&path).unwrap_err(), ReceiptError::Invalid(_)));
        std::fs::write(&path, br#"{"schema":1,"version":"1","commit":"c","root_instance":"","oem_inf":"oem1.inf","reboot_required":false,"boot_epoch_unix":1}"#).unwrap();
        assert!(matches!(read(&path).unwrap_err(), ReceiptError::Invalid(_)));
        std::fs::write(&path, br#"{"schema":1,"version":"1","commit":"c","root_instance":"r","oem_inf":"oem1.inf","reboot_required":false,"boot_epoch_unix":1,"extra":true}"#).unwrap();
        assert!(matches!(read(&path).unwrap_err(), ReceiptError::Invalid(_)));
        std::fs::write(&path, br#"{"schema":1,"version":"","commit":"","root_instance":"r","oem_inf":"oem1.inf","reboot_required":false,"boot_epoch_unix":1}"#).unwrap();
        assert!(matches!(read(&path).unwrap_err(), ReceiptError::Invalid(_)));
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, b"a file, not a directory").unwrap();
        assert!(matches!(
            write(&blocker.join("install-receipt.json"), &receipt(false, 1)).unwrap_err(),
            ReceiptError::Io(_)
        ));
    }
}
