// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/installation/root_device.rs
//! Owned `Root\RamShared` device registration (DT-14 product-path slice).
//!
//! `install` publishes the INF (see [`crate::driver_store`]) but the INF only
//! binds to a device that already exists. The lab creates that device with
//! `devcon.exe`; a customer machine has no WDK. This module owns the
//! registration and its removal through SetupAPI.
//!
//! Ownership is decided by the device's hardware-ID list, matched exactly, not
//! by its instance id. The live host is the proof: it carries
//! `ROOT\SCSIADAPTER\0000` with hardware id `Root\RamShared`, because the
//! instance name is inherited from the first device ID, while a freshly
//! created device gets `ROOT\RAMSHARED\0000`. Matching the instance name would
//! miss the existing device and add a second adapter on the next install.
//! Removal still passes the exact observed instance id and re-checks the
//! hardware id before touching it.
//!
//! The decision table is pure and testable on any host; only
//! [`ensure_owned_root`] and [`remove_owned_root`] call Windows.

use std::fmt;
use std::path::Path;

/// The one hardware id this product owns.
pub const OWNED_HARDWARE_ID: &str = r"Root\RamShared";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GuidParts {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

/// What an INF asks the PnP manager to bind: one hardware id, one class.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootDevicePlan {
    pub hardware_id: String,
    pub class_guid: GuidParts,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedDevice {
    pub instance_id: String,
    pub hardware_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedRootDevice {
    pub instance_id: String,
    pub created: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RootDeviceError {
    /// The INF cannot become a root-device plan.
    Plan(String),
    /// A device or INF claims something this product does not own.
    Foreign(String),
    /// More than one device claims the owned hardware id.
    Multiple(Vec<String>),
    /// A SetupAPI call failed; the code is kept verbatim.
    Win32 { op: &'static str, code: u32 },
    /// The host has no PnP manager (Linux tests, packaging hosts).
    Unsupported,
    /// Registration produced the wrong device count and the partial device
    /// could not be taken back.
    Compensation { reason: String, removal: String },
}

impl fmt::Display for RootDeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plan(reason) => write!(f, "root device plan: {reason}"),
            Self::Foreign(reason) => write!(f, "root device refusal: {reason}"),
            Self::Multiple(ids) => write!(
                f,
                "root device refusal: {} devices claim {OWNED_HARDWARE_ID}: {}",
                ids.len(),
                ids.join(", ")
            ),
            Self::Win32 { op, code } => {
                write!(f, "{op} failed (Win32 error {code})")
            }
            Self::Unsupported => {
                write!(f, "root device registration is not supported on this host")
            }
            Self::Compensation { reason, removal } => write!(
                f,
                "root device registration failed: {reason}; compensation: {removal}"
            ),
        }
    }
}

impl std::error::Error for RootDeviceError {}

/// Parse the canonical `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}` GUID text.
pub fn parse_guid(text: &str) -> Option<GuidParts> {
    let body = text.trim();
    let body = body.strip_prefix('{')?.strip_suffix('}')?;
    if !body
        .bytes()
        .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        return None;
    }
    let fields: Vec<&str> = body.split('-').collect();
    let [first, second, third, fourth, fifth] = fields.as_slice() else {
        return None;
    };
    if first.len() != 8 || second.len() != 4 || third.len() != 4 {
        return None;
    }
    if fourth.len() != 4 || fifth.len() != 12 {
        return None;
    }
    let mut data4 = [0u8; 8];
    let pairs = [&fourth[0..2], &fourth[2..4]];
    let pairs: [&str; 8] = [
        pairs[0],
        pairs[1],
        &fifth[0..2],
        &fifth[2..4],
        &fifth[4..6],
        &fifth[6..8],
        &fifth[8..10],
        &fifth[10..12],
    ];
    for (slot, pair) in data4.iter_mut().zip(pairs) {
        *slot = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(GuidParts {
        data1: u32::from_str_radix(first, 16).ok()?,
        data2: u16::from_str_radix(second, 16).ok()?,
        data3: u16::from_str_radix(third, 16).ok()?,
        data4,
    })
}

/// Every `section`, `key`, `value` triple the INF declares.
fn inf_entries(inf: &str) -> Vec<(String, String, String)> {
    let mut entries = Vec::new();
    let mut section = String::new();
    for line in inf.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            section = name.trim().to_string();
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            entries.push((
                section.clone(),
                key.trim().to_string(),
                value.trim().to_string(),
            ));
        }
    }
    entries
}

/// Build the plan from INF text and prove it binds the device this product
/// owns. A foreign or ambiguous INF is refused, never adapted.
pub fn plan_from_inf(inf: &str) -> Result<RootDevicePlan, RootDeviceError> {
    let entries = inf_entries(inf);
    let class_guid = entries
        .iter()
        .find(|(section, key, _)| {
            section.eq_ignore_ascii_case("Version") && key.eq_ignore_ascii_case("ClassGuid")
        })
        .and_then(|(_, _, value)| parse_guid(value))
        .ok_or_else(|| {
            RootDeviceError::Plan("INF has no parseable [Version] ClassGuid".to_string())
        })?;

    let manufacturer = entries
        .iter()
        .find(|(section, _, _)| section.eq_ignore_ascii_case("Manufacturer"))
        .map(|(_, _, value)| value.clone())
        .ok_or_else(|| RootDeviceError::Plan("INF has no [Manufacturer] section".to_string()))?;
    let model_section = manufacturer
        .split(',')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if model_section.is_empty() {
        return Err(RootDeviceError::Plan(
            "INF [Manufacturer] value names no models section".to_string(),
        ));
    }

    let mut hardware_ids: Vec<String> = Vec::new();
    for (section, _, value) in &entries {
        let models = section.eq_ignore_ascii_case(&model_section)
            || section
                .to_ascii_lowercase()
                .starts_with(&format!("{}.", model_section.to_ascii_lowercase()));
        if !models {
            continue;
        }
        let Some((_, hardware_id)) = value.split_once(',') else {
            continue;
        };
        let hardware_id = hardware_id.trim();
        if hardware_id.is_empty() {
            continue;
        }
        if !hardware_ids
            .iter()
            .any(|known| known.eq_ignore_ascii_case(hardware_id))
        {
            hardware_ids.push(hardware_id.to_string());
        }
    }
    match hardware_ids.as_slice() {
        [] => Err(RootDeviceError::Plan(format!(
            "INF models section {model_section} declares no hardware id"
        ))),
        [only] => {
            if !only.eq_ignore_ascii_case(OWNED_HARDWARE_ID) {
                return Err(RootDeviceError::Foreign(format!(
                    "INF binds {only}, not {OWNED_HARDWARE_ID}"
                )));
            }
            Ok(RootDevicePlan {
                hardware_id: OWNED_HARDWARE_ID.to_string(),
                class_guid,
            })
        }
        many => Err(RootDeviceError::Foreign(format!(
            "INF binds several hardware ids ({}); one owned id is required",
            many.join(", ")
        ))),
    }
}

pub fn plan_from_inf_file(path: &Path) -> Result<RootDevicePlan, RootDeviceError> {
    let inf = std::fs::read_to_string(path)
        .map_err(|error| RootDeviceError::Plan(format!("{}: {error}", path.display())))?;
    plan_from_inf(&inf)
}

fn owns(device: &ObservedDevice) -> bool {
    device
        .hardware_ids
        .iter()
        .any(|id| id.eq_ignore_ascii_case(OWNED_HARDWARE_ID))
}

fn owned_devices(devices: &[ObservedDevice]) -> Vec<&ObservedDevice> {
    devices.iter().filter(|device| owns(device)).collect()
}

/// The SetupAPI surface the decision table needs. A fake implements it on
/// hosts without a PnP manager.
pub trait RootBackend {
    fn devices(&mut self) -> Result<Vec<ObservedDevice>, RootDeviceError>;
    fn create(&mut self, plan: &RootDevicePlan) -> Result<String, RootDeviceError>;
    fn remove(&mut self, instance_id: &str) -> Result<(), RootDeviceError>;
}

/// Ensure exactly one device claims the owned hardware id, creating it only
/// when none exists.
///
/// A repeat install on a machine that already carries the adapter is a no-op,
/// even when the instance id is `ROOT\SCSIADAPTER\0000`. More than one owned
/// device is refused rather than pruned: silently deleting one would hide the
/// duplicate that caused it.
pub fn ensure_owned_root_with(
    backend: &mut dyn RootBackend,
    plan: &RootDevicePlan,
) -> Result<OwnedRootDevice, RootDeviceError> {
    let devices = backend.devices()?;
    let owned = owned_devices(&devices);
    match owned.len() {
        0 => {
            let created = backend.create(plan)?;
            let after = backend.devices()?;
            let owned = owned_devices(&after);
            if owned.len() == 1 {
                return Ok(OwnedRootDevice {
                    instance_id: owned[0].instance_id.clone(),
                    created: true,
                });
            }
            let reason = format!(
                "registration produced {} owned device(s), expected exactly one",
                owned.len()
            );
            let removal = match backend.remove(&created) {
                Ok(()) => "partial device removed".to_string(),
                Err(error) => error.to_string(),
            };
            Err(RootDeviceError::Compensation { reason, removal })
        }
        1 => Ok(OwnedRootDevice {
            instance_id: owned[0].instance_id.clone(),
            created: false,
        }),
        _ => Err(RootDeviceError::Multiple(
            owned
                .iter()
                .map(|device| device.instance_id.clone())
                .collect(),
        )),
    }
}

/// The instance id of the single owned device, or `None`. Read-only: status
/// must not create a device just to report that it is missing.
pub fn observe_owned_root_with(
    backend: &mut dyn RootBackend,
) -> Result<Option<String>, RootDeviceError> {
    let devices = backend.devices()?;
    let owned = owned_devices(&devices);
    match owned.len() {
        0 => Ok(None),
        1 => Ok(Some(owned[0].instance_id.clone())),
        _ => Err(RootDeviceError::Multiple(
            owned
                .iter()
                .map(|device| device.instance_id.clone())
                .collect(),
        )),
    }
}

/// Remove the single owned device and prove it is gone. Returns `false` when
/// no owned device is present, so uninstall stays idempotent.
pub fn remove_owned_root_with(backend: &mut dyn RootBackend) -> Result<bool, RootDeviceError> {
    let devices = backend.devices()?;
    let owned = owned_devices(&devices);
    match owned.len() {
        0 => Ok(false),
        1 => {
            let instance_id = owned[0].instance_id.clone();
            backend.remove(&instance_id)?;
            let after = backend.devices()?;
            let remaining = owned_devices(&after);
            if remaining.is_empty() {
                Ok(true)
            } else {
                Err(RootDeviceError::Plan(format!(
                    "removal left {} owned device(s) behind",
                    remaining.len()
                )))
            }
        }
        _ => Err(RootDeviceError::Multiple(
            owned
                .iter()
                .map(|device| device.instance_id.clone())
                .collect(),
        )),
    }
}

/// Observe the owned device on Windows without changing it.
#[cfg(windows)]
pub fn observe_owned_root() -> Result<Option<String>, RootDeviceError> {
    observe_owned_root_with(&mut setup_api::SetupApi)
}

/// Register the owned device on Windows.
#[cfg(windows)]
pub fn ensure_owned_root(plan: &RootDevicePlan) -> Result<OwnedRootDevice, RootDeviceError> {
    ensure_owned_root_with(&mut setup_api::SetupApi, plan)
}

#[cfg(windows)]
pub fn remove_owned_root() -> Result<bool, RootDeviceError> {
    remove_owned_root_with(&mut setup_api::SetupApi)
}

#[cfg(not(windows))]
pub fn observe_owned_root() -> Result<Option<String>, RootDeviceError> {
    Err(RootDeviceError::Unsupported)
}

#[cfg(not(windows))]
pub fn ensure_owned_root(_plan: &RootDevicePlan) -> Result<OwnedRootDevice, RootDeviceError> {
    Err(RootDeviceError::Unsupported)
}

#[cfg(not(windows))]
pub fn remove_owned_root() -> Result<bool, RootDeviceError> {
    Err(RootDeviceError::Unsupported)
}

#[cfg(windows)]
#[path = "root_device/setup_api.rs"]
mod setup_api;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const REAL_INF: &str = include_str!("../../../../drivers/windows/ramshared/ramshared.inf");

    fn owned(instance: &str) -> ObservedDevice {
        ObservedDevice {
            instance_id: instance.to_string(),
            hardware_ids: vec![OWNED_HARDWARE_ID.to_string()],
        }
    }

    fn foreign(instance: &str) -> ObservedDevice {
        ObservedDevice {
            instance_id: instance.to_string(),
            hardware_ids: vec![r"Root\ForeignKey".to_string()],
        }
    }

    struct Fake {
        devices: Vec<ObservedDevice>,
        creates: usize,
        removes: Vec<String>,
        publish_created: bool,
        honor_remove: bool,
    }

    impl Fake {
        fn new(devices: Vec<ObservedDevice>) -> Self {
            Self {
                devices,
                creates: 0,
                removes: Vec::new(),
                publish_created: true,
                honor_remove: true,
            }
        }
    }

    impl RootBackend for Fake {
        fn devices(&mut self) -> Result<Vec<ObservedDevice>, RootDeviceError> {
            Ok(self.devices.clone())
        }

        fn create(&mut self, plan: &RootDevicePlan) -> Result<String, RootDeviceError> {
            self.creates += 1;
            let id = r"ROOT\RAMSHARED\0000".to_string();
            if self.publish_created {
                self.devices.push(ObservedDevice {
                    instance_id: id.clone(),
                    hardware_ids: vec![plan.hardware_id.clone()],
                });
            }
            Ok(id)
        }

        fn remove(&mut self, instance_id: &str) -> Result<(), RootDeviceError> {
            self.removes.push(instance_id.to_string());
            if self.honor_remove {
                self.devices
                    .retain(|device| !device.instance_id.eq_ignore_ascii_case(instance_id));
            }
            Ok(())
        }
    }

    fn plan() -> RootDevicePlan {
        plan_from_inf(
            "[Version]\nClassGuid = {4d36e97b-e325-11ce-bfc1-08002be10318}\n\
             [Manufacturer]\n%M% = Standard,NTamd64.10.0...16299\n\
             [Standard.NTamd64.10.0...16299]\n%Desc% = RamShared_Device, Root\\RamShared\n",
        )
        .unwrap()
    }

    #[test]
    fn plan_accepts_the_shipped_inf() {
        let plan = plan_from_inf(REAL_INF).unwrap();
        assert_eq!(plan.hardware_id, OWNED_HARDWARE_ID);
        assert_eq!(
            plan.class_guid,
            GuidParts {
                data1: 0x4d36_e97b,
                data2: 0xe325,
                data3: 0x11ce,
                data4: [0xbf, 0xc1, 0x08, 0x00, 0x2b, 0xe1, 0x03, 0x18],
            }
        );
    }

    #[test]
    fn plan_refuses_a_foreign_inf() {
        let inf = "[Version]\nClassGuid = {4d36e97b-e325-11ce-bfc1-08002be10318}\n\
                   [Manufacturer]\n%M% = Standard,NTamd64.10.0...16299\n\
                   [Standard.NTamd64.10.0...16299]\n%Desc% = Other_Device, Root\\Other\n";
        let error = plan_from_inf(inf).unwrap_err();
        assert!(matches!(error, RootDeviceError::Foreign(_)), "{error}");
        assert!(error.to_string().contains(OWNED_HARDWARE_ID), "{error}");
    }

    #[test]
    fn plan_refuses_an_inf_without_a_class_guid_or_models_section() {
        assert!(plan_from_inf("[Version]\nSignature=\"$Windows NT$\"\n").is_err());
        assert!(
            plan_from_inf("[Version]\nClassGuid = {4d36e97b-e325-11ce-bfc1-08002be10318}\n")
                .is_err()
        );
    }

    #[test]
    fn parse_guid_requires_the_canonical_form() {
        assert!(parse_guid("{4d36e97b-e325-11ce-bfc1-08002be10318}").is_some());
        for bad in [
            "4d36e97b-e325-11ce-bfc1-08002be10318",
            "{4d36e97b-e325-11ce-bfc1-08002be10318",
            "{zzzzzzzz-e325-11ce-bfc1-08002be10318}",
            "{4d36e97b-e325-11ce-bfc1-08002be1031}",
            "{4d36e97b-e325-11ce-bfc1-08002be10318-ff}",
        ] {
            assert!(parse_guid(bad).is_none(), "{bad} accepted");
        }
    }

    #[test]
    fn clean_machine_creates_one_owned_root_device() {
        let mut backend = Fake::new(Vec::new());
        let device = ensure_owned_root_with(&mut backend, &plan()).unwrap();
        assert!(device.created);
        assert_eq!(device.instance_id, r"ROOT\RAMSHARED\0000");
        assert_eq!(backend.creates, 1);
        assert_eq!(backend.devices.len(), 1);
    }

    #[test]
    fn install_repeat_keeps_one_root_adapter() {
        let mut backend = Fake::new(vec![
            owned(r"ROOT\SCSIADAPTER\0000"),
            foreign(r"ROOT\FOREIGN\0000"),
        ]);
        let device = ensure_owned_root_with(&mut backend, &plan()).unwrap();
        assert!(!device.created);
        assert_eq!(device.instance_id, r"ROOT\SCSIADAPTER\0000");
        assert_eq!(backend.creates, 0);
        assert_eq!(backend.removes.len(), 0);
    }

    #[test]
    fn foreign_device_or_oem_inf_is_never_removed() {
        let mut backend = Fake::new(vec![foreign(r"ROOT\FOREIGN\0000")]);
        assert!(!remove_owned_root_with(&mut backend).unwrap());
        assert!(backend.removes.is_empty());
        assert_eq!(backend.devices.len(), 1);
    }

    #[test]
    fn duplicate_owned_adapters_are_refused_not_pruned() {
        let mut backend = Fake::new(vec![
            owned(r"ROOT\RAMSHARED\0000"),
            owned(r"ROOT\SCSIADAPTER\0000"),
        ]);
        let error = ensure_owned_root_with(&mut backend, &plan()).unwrap_err();
        assert!(matches!(error, RootDeviceError::Multiple(_)), "{error}");
        assert_eq!(backend.removes.len(), 0);
    }

    #[test]
    fn root_registration_failure_compensates_owned_changes() {
        let mut backend = Fake::new(Vec::new());
        backend.publish_created = false;
        let error = ensure_owned_root_with(&mut backend, &plan()).unwrap_err();
        assert!(
            matches!(error, RootDeviceError::Compensation { .. }),
            "{error}"
        );
        assert_eq!(backend.removes, vec![r"ROOT\RAMSHARED\0000".to_string()]);
    }

    #[test]
    fn observation_reports_the_owned_instance_without_changing_it() {
        let mut backend = Fake::new(vec![
            owned(r"ROOT\SCSIADAPTER\0000"),
            foreign(r"ROOT\FOREIGN\0000"),
        ]);
        assert_eq!(
            observe_owned_root_with(&mut backend).unwrap(),
            Some(r"ROOT\SCSIADAPTER\0000".to_string())
        );
        assert_eq!(backend.creates, 0);
        assert!(backend.removes.is_empty());
        let mut empty = Fake::new(Vec::new());
        assert_eq!(observe_owned_root_with(&mut empty).unwrap(), None);
    }

    #[test]
    fn observation_refuses_a_duplicate_owned_adapter() {
        let mut backend = Fake::new(vec![
            owned(r"ROOT\RAMSHARED\0000"),
            owned(r"ROOT\SCSIADAPTER\0000"),
        ]);
        let error = observe_owned_root_with(&mut backend).unwrap_err();
        assert!(matches!(error, RootDeviceError::Multiple(_)), "{error}");
    }

    #[test]
    fn removal_is_verified_not_assumed() {
        let mut backend = Fake::new(vec![owned(r"ROOT\RAMSHARED\0000")]);
        backend.honor_remove = false;
        let error = remove_owned_root_with(&mut backend).unwrap_err();
        assert!(error.to_string().contains("left 1 owned device"), "{error}");
    }

    #[test]
    fn removal_reports_whether_a_device_was_present() {
        let mut backend = Fake::new(vec![owned(r"ROOT\RAMSHARED\0000")]);
        assert!(remove_owned_root_with(&mut backend).unwrap());
        assert!(backend.devices.is_empty());
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "reads the live PnP manager; run explicitly on a provisioned host"]
    fn setup_api_sees_the_live_owned_device() {
        let mut backend = setup_api::SetupApi;
        let devices = backend.devices().unwrap();
        let owned = owned_devices(&devices);
        assert_eq!(
            owned.len(),
            1,
            "expected exactly one {OWNED_HARDWARE_ID} device, saw {:?}",
            owned
                .iter()
                .map(|device| device.instance_id.clone())
                .collect::<Vec<_>>()
        );
        assert!(
            owned[0]
                .hardware_ids
                .iter()
                .any(|id| id.eq_ignore_ascii_case(OWNED_HARDWARE_ID))
        );
        assert!(
            owned[0]
                .instance_id
                .to_ascii_uppercase()
                .starts_with("ROOT\\")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn host_without_a_pnp_manager_is_unsupported() {
        let path = std::path::PathBuf::from("/nonexistent/ramshared.inf");
        assert_eq!(
            ensure_owned_root(&plan()).unwrap_err(),
            RootDeviceError::Unsupported
        );
        assert_eq!(
            remove_owned_root().unwrap_err(),
            RootDeviceError::Unsupported
        );
        assert!(plan_from_inf_file(&path).is_err());
    }
}
