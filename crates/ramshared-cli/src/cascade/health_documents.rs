// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/health_documents.rs
//! Strict readers for the two runtime health documents (SPEC
//! wsl2-transparent-tier-service ITEM-7, DT-5/DT-10).
//!
//! `status --json` is often read after the daemon is gone: a file survives
//! it and needs no new IPC. That is also why these readers refuse anything
//! they cannot prove — a symlink, a directory, a body over 4 KiB, an unknown
//! schema, or a missing key. "Absent" is `None` and publishes `null`; it is
//! never coerced into a measured zero.

use super::boot;
use super::posture::Posture;
use serde_json::{Map, Value};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// DT-10: the pump's only status surface.
pub const PUMP_HEALTH_PATH: &str = "/run/ramshared/pump-health.json";

/// DT-5: the hardware classifier's document.
pub const HARDWARE_HEALTH_PATH: &str = "/run/ramshared/hardware-health.json";

/// DT-5/DT-10: both documents are bounded to 4 KiB.
pub const HEALTH_DOCUMENT_MAX_BYTES: usize = 4096;

/// The approval directory that owns the release-scoped activation token.
const APPROVAL_DIR: &str = "/var/lib/ramshared/approvals";

/// The release selector whose `RELEASE_VERSION` names the installed release.
const RELEASE_SELECTOR: &str = "/opt/ramshared/current";

/// A refused or unreadable health document. The message is for the
/// `health_errors` fragment; the status document keeps rendering.
#[derive(Debug, PartialEq, Eq)]
pub enum HealthError {
    /// The file exists but cannot be read.
    Io(String),
    /// The path is not a bounded regular file (symlink, directory, ...).
    Refused(&'static str),
    /// The body is not the declared schema.
    Malformed(&'static str),
}

impl fmt::Display for HealthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HealthError::Io(message) => write!(f, "unreadable: {message}"),
            HealthError::Refused(reason) => write!(f, "refused: {reason}"),
            HealthError::Malformed(reason) => write!(f, "malformed: {reason}"),
        }
    }
}

impl std::error::Error for HealthError {}

/// The pump states, pinned by SPEC Observability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PumpState {
    Bound,
    TierLost,
    PumpStopped,
    Degraded,
}

impl PumpState {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "bound" => Some(Self::Bound),
            "tier_lost" => Some(Self::TierLost),
            "pump_stopped" => Some(Self::PumpStopped),
            "degraded" => Some(Self::Degraded),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bound => "bound",
            Self::TierLost => "tier_lost",
            Self::PumpStopped => "pump_stopped",
            Self::Degraded => "degraded",
        }
    }
}

/// Schema 1 of `pump-health.json` as the daemon writes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PumpHealth {
    pub state: PumpState,
    pub last_cycle_mib: Option<u64>,
    pub effective_mib_per_s: Option<u32>,
    pub p99_cycle_ms: Option<u64>,
    pub consecutive_slow_cycles: u8,
}

/// The pinned hardware verdicts (DT-5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HardwareVerdict {
    Clean,
    CleanOverride,
    RefusedMce,
    RefusedSigbus,
    RefusedUnclean,
}

impl HardwareVerdict {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "clean" => Some(Self::Clean),
            "clean_override" => Some(Self::CleanOverride),
            "refused_mce" => Some(Self::RefusedMce),
            "refused_sigbus" => Some(Self::RefusedSigbus),
            "refused_unclean" => Some(Self::RefusedUnclean),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::CleanOverride => "clean_override",
            Self::RefusedMce => "refused_mce",
            Self::RefusedSigbus => "refused_sigbus",
            Self::RefusedUnclean => "refused_unclean",
        }
    }
}

/// Schema 1 of `hardware-health.json` as the classifier writes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HardwareHealth {
    pub boot_id: String,
    pub boots_scanned: u64,
    pub mce_kill_count: u64,
    pub sigbus_mceerr_count: u64,
    pub unclean_marker_present: bool,
    /// Audit-only on Windows: the boot-time classifier cannot read WHEA, so
    /// its own document says `null`. Never a measured zero.
    pub whea_event_count: Option<u64>,
    pub journal: String,
    pub verdict: HardwareVerdict,
}

/// The enrolled posture and why it is or is not publishable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatusPosture {
    pub posture: Option<Posture>,
    pub reason: &'static str,
}

impl StatusPosture {
    fn missing(reason: &'static str) -> Self {
        StatusPosture {
            posture: None,
            reason,
        }
    }
}

/// Read one bounded health document. `Ok(None)` is the only absent case;
/// every other unprovable path is an error that the status fragment reports.
pub fn read_bounded_document(path: &Path) -> Result<Option<String>, HealthError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(HealthError::Io(error.to_string())),
    };
    if metadata.file_type().is_symlink() {
        return Err(HealthError::Refused("symlinked health document"));
    }
    if !metadata.is_file() {
        return Err(HealthError::Refused(
            "health document is not a regular file",
        ));
    }
    if metadata.len() == 0 || metadata.len() > HEALTH_DOCUMENT_MAX_BYTES as u64 {
        return Err(HealthError::Refused("health document exceeds its bound"));
    }
    let body = fs::read_to_string(path).map_err(|error| HealthError::Io(error.to_string()))?;
    if body.len() > HEALTH_DOCUMENT_MAX_BYTES {
        return Err(HealthError::Refused("health document exceeds its bound"));
    }
    Ok(Some(body))
}

fn object(body: &str, what: &'static str) -> Result<Map<String, Value>, HealthError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|_| HealthError::Malformed("health document is not JSON"))?;
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(HealthError::Malformed(what)),
    }
}

fn exact_keys(map: &Map<String, Value>, keys: &[&str]) -> bool {
    map.len() == keys.len() && keys.iter().all(|key| map.contains_key(*key))
}

fn schema_is_v1(map: &Map<String, Value>) -> bool {
    map.get("schema_version").and_then(Value::as_u64) == Some(1)
}

fn required_u64(map: &Map<String, Value>, key: &'static str) -> Result<u64, HealthError> {
    map.get(key)
        .and_then(Value::as_u64)
        .ok_or(HealthError::Malformed(key))
}

fn optional_u64(map: &Map<String, Value>, key: &'static str) -> Result<Option<u64>, HealthError> {
    match map.get(key) {
        Some(Value::Null) => Ok(None),
        Some(value) => value.as_u64().map(Some).ok_or(HealthError::Malformed(key)),
        None => Err(HealthError::Malformed(key)),
    }
}

fn optional_u32(map: &Map<String, Value>, key: &'static str) -> Result<Option<u32>, HealthError> {
    optional_u64(map, key)?.map_or(Ok(None), |value| {
        u32::try_from(value)
            .map(Some)
            .map_err(|_| HealthError::Malformed(key))
    })
}

fn required_bool(map: &Map<String, Value>, key: &'static str) -> Result<bool, HealthError> {
    map.get(key)
        .and_then(Value::as_bool)
        .ok_or(HealthError::Malformed(key))
}

fn required_string<'a>(
    map: &'a Map<String, Value>,
    key: &'static str,
) -> Result<&'a str, HealthError> {
    map.get(key)
        .and_then(Value::as_str)
        .ok_or(HealthError::Malformed(key))
}

const PUMP_KEYS: &[&str] = &[
    "schema_version",
    "state",
    "last_cycle_mib",
    "effective_mib_per_s",
    "p99_cycle_ms",
    "consecutive_slow_cycles",
];

/// DT-10: read `pump-health.json`. Absent is `Ok(None)`, never a zeroed
/// measurement.
pub fn read_pump_health(path: &Path) -> Result<Option<PumpHealth>, HealthError> {
    let Some(body) = read_bounded_document(path)? else {
        return Ok(None);
    };
    let map = object(&body, "pump health is not an object")?;
    if !exact_keys(&map, PUMP_KEYS) {
        return Err(HealthError::Malformed(
            "pump health has missing or extra keys",
        ));
    }
    if !schema_is_v1(&map) {
        return Err(HealthError::Malformed("pump health schema is not 1"));
    }
    let state = PumpState::parse(required_string(&map, "state")?)
        .ok_or(HealthError::Malformed("pump health state is unknown"))?;
    let last_cycle_mib = optional_u64(&map, "last_cycle_mib")?;
    let effective_mib_per_s = optional_u32(&map, "effective_mib_per_s")?;
    let p99_cycle_ms = optional_u64(&map, "p99_cycle_ms")?;
    let consecutive_slow_cycles = u8::try_from(required_u64(&map, "consecutive_slow_cycles")?)
        .map_err(|_| HealthError::Malformed("consecutive_slow_cycles"))?;
    Ok(Some(PumpHealth {
        state,
        last_cycle_mib,
        effective_mib_per_s,
        p99_cycle_ms,
        consecutive_slow_cycles,
    }))
}

const HARDWARE_KEYS: &[&str] = &[
    "schema_version",
    "generated_at_ms",
    "boot_id",
    "window",
    "boots_scanned",
    "mce_kill_count",
    "sigbus_mceerr_count",
    "unclean_marker_present",
    "whea_event_count",
    "journal",
    "verdict",
];

/// DT-5: read `hardware-health.json`. The verdict is pinned; WHEA is
/// nullable by design.
pub fn read_hardware_health(path: &Path) -> Result<Option<HardwareHealth>, HealthError> {
    let Some(body) = read_bounded_document(path)? else {
        return Ok(None);
    };
    let map = object(&body, "hardware health is not an object")?;
    if !exact_keys(&map, HARDWARE_KEYS) {
        return Err(HealthError::Malformed(
            "hardware health has missing or extra keys",
        ));
    }
    if !schema_is_v1(&map) {
        return Err(HealthError::Malformed("hardware health schema is not 1"));
    }
    let _generated_at_ms = required_u64(&map, "generated_at_ms")?;
    let boot_id = required_string(&map, "boot_id")?;
    if boot_id.is_empty()
        || boot_id.len() > 64
        || !boot_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(HealthError::Malformed("hardware health boot_id"));
    }
    let _window = required_string(&map, "window")?;
    let boots_scanned = required_u64(&map, "boots_scanned")?;
    let mce_kill_count = required_u64(&map, "mce_kill_count")?;
    let sigbus_mceerr_count = required_u64(&map, "sigbus_mceerr_count")?;
    let unclean_marker_present = required_bool(&map, "unclean_marker_present")?;
    let whea_event_count = optional_u64(&map, "whea_event_count")?;
    let journal = required_string(&map, "journal")?;
    if journal != "available" && journal != "unavailable" {
        return Err(HealthError::Malformed("hardware health journal"));
    }
    let verdict = HardwareVerdict::parse(required_string(&map, "verdict")?)
        .ok_or(HealthError::Malformed("hardware health verdict is unknown"))?;
    Ok(Some(HardwareHealth {
        boot_id: boot_id.to_string(),
        boots_scanned,
        mce_kill_count,
        sigbus_mceerr_count,
        unclean_marker_present,
        whea_event_count,
        journal: journal.to_string(),
        verdict,
    }))
}

/// Parse the posture bound to `release` from a token body. Pure: the caller
/// owns trust and I/O.
pub fn parse_posture_binding(raw: &str, release: &str) -> Result<Posture, HealthError> {
    let approval = boot::parse_scoped_approval(raw)
        .map_err(|_| HealthError::Malformed("activation token is not an approval"))?;
    if !approval.matches_release(release) {
        return Err(HealthError::Malformed(
            "activation token names another release",
        ));
    }
    Ok(approval.posture)
}

/// Classify a token read that failed *after* its metadata passed the trust
/// checks. The token is minted `0400` root-owned (SPEC DT-2), so an ordinary
/// user reading the status document gets `EACCES` on a token that is
/// perfectly trustworthy; reporting `token_untrusted` there would send the
/// operator to re-enroll instead of re-running the read as root. A body that
/// is present but unreadable as text is the only remaining untrusted case.
fn token_read_failure(error: &io::Error) -> &'static str {
    match error.kind() {
        io::ErrorKind::NotFound => "token_missing",
        io::ErrorKind::PermissionDenied => "token_unreadable",
        _ => "token_untrusted",
    }
}

/// Resolve the posture enrolled for the installed release. Display-only:
/// activation still proves the full token trust itself. A missing selector,
/// version, or token reports its reason and publishes `null`.
pub fn status_posture(selector: &Path, approval_dir: &Path) -> StatusPosture {
    let release_dir = match fs::read_link(selector) {
        Ok(target) => {
            if target.is_absolute() {
                target
            } else {
                selector
                    .parent()
                    .unwrap_or_else(|| Path::new("/"))
                    .join(target)
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return StatusPosture::missing("token_missing");
        }
        Err(_) => return StatusPosture::missing("selector_unreadable"),
    };
    let version = match fs::read_to_string(release_dir.join("RELEASE_VERSION")) {
        Ok(version) => version.trim().to_string(),
        Err(_) => return StatusPosture::missing("release_unreadable"),
    };
    if version.is_empty() || version.len() > 128 {
        return StatusPosture::missing("release_unreadable");
    }
    let token_path: PathBuf = approval_dir.join(format!("activate-{version}.token"));
    let metadata = match fs::symlink_metadata(&token_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return StatusPosture::missing("token_missing");
        }
        // No metadata means no trust evidence at all: the token cannot be
        // inspected (a missing parent component, `EACCES`, `ENOTDIR`). That
        // is not the same refusal as metadata that failed the checks below.
        Err(_) => return StatusPosture::missing("token_unreadable"),
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() == 0
        || metadata.len() > 512
    {
        return StatusPosture::missing("token_untrusted");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return StatusPosture::missing("token_untrusted");
        }
    }
    let body = match fs::read_to_string(&token_path) {
        Ok(body) => body,
        Err(error) => return StatusPosture::missing(token_read_failure(&error)),
    };
    match parse_posture_binding(&body, &version) {
        Ok(posture) => StatusPosture {
            posture: Some(posture),
            reason: "token_binding",
        },
        Err(HealthError::Malformed(reason)) => StatusPosture::missing(reason),
        Err(_) => StatusPosture::missing("token_untrusted"),
    }
}

/// Render the additive `status --json` fragments (ITEM-7). Absent documents
/// and unmeasured values publish `null`; a refused document is listed in
/// `health_errors` so it is neither silent nor treated as a measured zero.
pub fn render_json_fragments(
    pump: Option<&PumpHealth>,
    hardware: Option<&HardwareHealth>,
    posture: &StatusPosture,
    errors: &[String],
) -> Result<String, HealthError> {
    let posture_value = posture.posture.map_or(Value::Null, |posture| {
        Value::String(posture.as_status().into())
    });
    let pump_state = pump.map_or(Value::Null, |pump| {
        Value::String(pump.state.as_str().into())
    });
    let hardware_health = hardware.map_or(Value::Null, |hardware| {
        Value::String(hardware.verdict.as_str().into())
    });
    let fragment = serde_json::json!({
        "posture": posture_value,
        "posture_reason": posture.reason,
        "tier_state": pump_state,
        "spill_mib_per_cycle": pump.and_then(|pump| pump.last_cycle_mib),
        "writeback_mib_per_s": pump
            .and_then(|pump| pump.effective_mib_per_s)
            .map_or(Value::Null, Value::from),
        "writeback_p99_ms": pump.and_then(|pump| pump.p99_cycle_ms),
        "consecutive_slow_cycles": pump.map(|pump| pump.consecutive_slow_cycles),
        "hardware_health": hardware_health,
        "mce_kill_count": hardware.map(|hardware| hardware.mce_kill_count),
        "sigbus_mceerr_count": hardware.map(|hardware| hardware.sigbus_mceerr_count),
        "whea_event_count": hardware.and_then(|hardware| hardware.whea_event_count),
        "health_errors": errors,
    });
    fragment
        .to_string()
        .strip_prefix('{')
        .and_then(|body| body.strip_suffix('}'))
        .map(str::to_string)
        .ok_or(HealthError::Malformed("status fragment is not an object"))
}

/// The two fixed document paths, resolved once for the status command.
pub fn runtime_paths() -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    (
        PathBuf::from(PUMP_HEALTH_PATH),
        PathBuf::from(HARDWARE_HEALTH_PATH),
        PathBuf::from(RELEASE_SELECTOR),
        PathBuf::from(APPROVAL_DIR),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ramshared-health-documents-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn pump_body(last: &str) -> String {
        format!(
            "{{\"schema_version\":1,\"state\":\"bound\",\"last_cycle_mib\":{last},\
             \"effective_mib_per_s\":{last},\"p99_cycle_ms\":1000,\
             \"consecutive_slow_cycles\":0}}"
        )
    }

    fn hardware_body(verdict: &str) -> String {
        format!(
            "{{\"schema_version\":1,\"generated_at_ms\":1,\
             \"boot_id\":\"01234567-89ab-cdef-0123-456789abcdef\",\
             \"window\":\"8 boots / 7 days\",\"boots_scanned\":2,\
             \"mce_kill_count\":0,\"sigbus_mceerr_count\":0,\
             \"unclean_marker_present\":false,\"whea_event_count\":null,\
             \"journal\":\"available\",\"verdict\":\"{verdict}\"}}"
        )
    }

    /// a missing pump document is absent, and the rendered
    /// fragment says null rather than a measured zero.
    #[test]
    fn an_absent_pump_health_file_is_not_zero() {
        let dir = temp_dir("absent");
        let path = dir.join("pump-health.json");
        assert_eq!(read_pump_health(&path), Ok(None));
        let posture = StatusPosture::missing("token_missing");
        let fragments = render_json_fragments(None, None, &posture, &[]).unwrap();
        assert!(fragments.contains("\"tier_state\":null"), "{fragments}");
        assert!(
            fragments.contains("\"spill_mib_per_cycle\":null"),
            "{fragments}"
        );
        assert!(
            fragments.contains("\"writeback_p99_ms\":null"),
            "{fragments}"
        );
        assert!(
            fragments.contains("\"writeback_mib_per_s\":null"),
            "{fragments}"
        );
        assert!(!fragments.contains("\"spill_mib_per_cycle\":0"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// malformed, over-bounded, non-regular, unknown
    /// schema, and extra-key documents are refused rather than partially
    /// trusted.
    #[test]
    fn a_malformed_health_document_is_refused() {
        let dir = temp_dir("malformed");
        let path = dir.join("pump-health.json");

        fs::write(&path, "{\"schema_version\":1,\"state\":\"bound\"}").unwrap();
        assert!(matches!(
            read_pump_health(&path),
            Err(HealthError::Malformed(_))
        ));

        fs::write(&path, pump_body("0").replace("\"bound\"", "\"happy\"")).unwrap();
        assert!(matches!(
            read_pump_health(&path),
            Err(HealthError::Malformed(_))
        ));

        fs::write(
            &path,
            pump_body("0").replace("\"schema_version\":1", "\"schema_version\":2"),
        )
        .unwrap();
        assert!(matches!(
            read_pump_health(&path),
            Err(HealthError::Malformed(_))
        ));

        // Extra keys are a different producer, not this schema.
        fs::write(&path, pump_body("0").replace("}", ",\"spilled_mib\":7}")).unwrap();
        assert!(matches!(
            read_pump_health(&path),
            Err(HealthError::Malformed(_))
        ));

        // An absent measurement is null; a quoted value is not a number.
        fs::write(&path, pump_body("\"many\"")).unwrap();
        assert!(matches!(
            read_pump_health(&path),
            Err(HealthError::Malformed(_))
        ));

        // The bound is enforced before parsing.
        fs::write(&path, " ".repeat(HEALTH_DOCUMENT_MAX_BYTES + 1)).unwrap();
        assert!(matches!(
            read_pump_health(&path),
            Err(HealthError::Refused(_))
        ));

        let hardware = dir.join("hardware-health.json");
        fs::write(&hardware, hardware_body("unclean")).unwrap();
        assert!(matches!(
            read_hardware_health(&hardware),
            Err(HealthError::Malformed(_))
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    /// a symlink is refused, not followed — even when it
    /// points at a perfectly valid document.
    #[test]
    fn a_symlinked_health_document_is_refused() {
        let dir = temp_dir("symlink");
        let target = dir.join("valid-pump.json");
        fs::write(&target, pump_body("32")).unwrap();
        let link = dir.join("pump-health.json");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, &link).unwrap();
            assert_eq!(
                read_pump_health(&link),
                Err(HealthError::Refused("symlinked health document"))
            );
            let hardware_link = dir.join("hardware-health.json");
            let hardware_target = dir.join("valid-hardware.json");
            fs::write(&hardware_target, hardware_body("clean")).unwrap();
            std::os::unix::fs::symlink(&hardware_target, &hardware_link).unwrap();
            assert_eq!(
                read_hardware_health(&hardware_link),
                Err(HealthError::Refused("symlinked health document"))
            );
        }
        // A directory at the path is also not a document.
        assert!(matches!(
            read_pump_health(&dir),
            Err(HealthError::Refused(_))
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_valid_pump_document_preserves_absence_and_measurement() {
        let dir = temp_dir("valid-pump");
        let path = dir.join("pump-health.json");
        fs::write(&path, pump_body("32")).unwrap();
        let health = read_pump_health(&path).unwrap().unwrap();
        assert_eq!(health.state, PumpState::Bound);
        assert_eq!(health.last_cycle_mib, Some(32));
        assert_eq!(health.effective_mib_per_s, Some(32));
        assert_eq!(health.p99_cycle_ms, Some(1000));

        // Explicit null is "not measured"; a dropped key is schema drift and
        // is refused rather than silently read as zero.
        let explicit_null = pump_body("null");
        fs::write(&path, &explicit_null).unwrap();
        let health = read_pump_health(&path).unwrap().unwrap();
        assert_eq!(health.last_cycle_mib, None);
        assert_eq!(health.effective_mib_per_s, None);

        let key_absent = explicit_null.replace(",\"effective_mib_per_s\":null", "");
        assert!(!key_absent.contains("effective_mib_per_s"), "{key_absent}");
        fs::write(&path, &key_absent).unwrap();
        assert_eq!(
            read_pump_health(&path),
            Err(HealthError::Malformed(
                "pump health has missing or extra keys"
            ))
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_valid_hardware_document_carries_the_counts_and_a_nullable_whea() {
        let dir = temp_dir("valid-hardware");
        let path = dir.join("hardware-health.json");
        fs::write(&path, hardware_body("clean")).unwrap();
        let health = read_hardware_health(&path).unwrap().unwrap();
        assert_eq!(health.verdict, HardwareVerdict::Clean);
        assert_eq!(health.whea_event_count, None);
        assert_eq!(health.mce_kill_count, 0);
        assert_eq!(health.journal, "available");

        fs::write(
            &path,
            hardware_body("refused_mce")
                .replace("\"whea_event_count\":null", "\"whea_event_count\":3"),
        )
        .unwrap();
        let health = read_hardware_health(&path).unwrap().unwrap();
        assert_eq!(health.verdict, HardwareVerdict::RefusedMce);
        assert_eq!(health.whea_event_count, Some(3));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_fragments_publish_the_measured_values_and_the_reasons() {
        let pump = PumpHealth {
            state: PumpState::Degraded,
            last_cycle_mib: Some(31),
            effective_mib_per_s: Some(4),
            p99_cycle_ms: Some(12_000),
            consecutive_slow_cycles: 3,
        };
        let hardware = HardwareHealth {
            boot_id: "01234567-89ab-cdef-0123-456789abcdef".into(),
            boots_scanned: 2,
            mce_kill_count: 0,
            sigbus_mceerr_count: 0,
            unclean_marker_present: false,
            whea_event_count: None,
            journal: "available".into(),
            verdict: HardwareVerdict::Clean,
        };
        let posture = StatusPosture {
            posture: Some(Posture::Writeback),
            reason: "token_binding",
        };
        let fragments = render_json_fragments(Some(&pump), Some(&hardware), &posture, &[]).unwrap();
        assert!(
            fragments.contains("\"posture\":\"writeback\""),
            "{fragments}"
        );
        assert!(
            fragments.contains("\"posture_reason\":\"token_binding\""),
            "{fragments}"
        );
        assert!(
            fragments.contains("\"tier_state\":\"degraded\""),
            "{fragments}"
        );
        assert!(
            fragments.contains("\"spill_mib_per_cycle\":31"),
            "{fragments}"
        );
        assert!(
            fragments.contains("\"writeback_p99_ms\":12000"),
            "{fragments}"
        );
        assert!(
            fragments.contains("\"consecutive_slow_cycles\":3"),
            "{fragments}"
        );
        assert!(
            fragments.contains("\"hardware_health\":\"clean\""),
            "{fragments}"
        );
        assert!(
            fragments.contains("\"whea_event_count\":null"),
            "{fragments}"
        );
    }

    #[test]
    fn a_refused_document_is_reported_in_the_fragment() {
        let posture = StatusPosture::missing("token_missing");
        let fragments = render_json_fragments(
            None,
            None,
            &posture,
            &["pump-health: refused: symlinked health document".to_string()],
        )
        .unwrap();
        assert!(
            fragments.contains("\"health_errors\":[\"pump-health:"),
            "{fragments}"
        );
        assert!(fragments.contains("\"posture\":null"), "{fragments}");
        assert!(
            fragments.contains("\"posture_reason\":\"token_missing\""),
            "{fragments}"
        );
    }

    #[test]
    fn a_token_binding_reports_the_posture_and_refuses_a_foreign_release() {
        assert_eq!(
            parse_posture_binding("activate:0.15.0:posture=writeback", "0.15.0"),
            Ok(Posture::Writeback)
        );
        assert_eq!(
            parse_posture_binding("activate:0.15.0:posture=swap", "0.15.0"),
            Ok(Posture::Swap)
        );
        assert!(matches!(
            parse_posture_binding("activate:0.15.0:posture=tiered", "0.15.0"),
            Err(HealthError::Malformed(_))
        ));
        assert!(matches!(
            parse_posture_binding("activate:0.16.0:posture=writeback", "0.15.0"),
            Err(HealthError::Malformed(_))
        ));
    }

    /// DT-2: the token is `0400` root-owned, so a non-root reader cannot even
    /// obtain its metadata. That is an observation failure, not a trust
    /// refusal: the operator must re-read as root, not re-enroll.
    #[cfg(unix)]
    #[test]
    fn a_token_the_reader_cannot_inspect_is_unreadable_not_untrusted() {
        let dir = temp_dir("posture-uninspectable");
        let release = dir.join("releases/v0.15.0");
        fs::create_dir_all(&release).unwrap();
        fs::write(release.join("RELEASE_VERSION"), "0.15.0\n").unwrap();
        let selector = dir.join("current");
        std::os::unix::fs::symlink("releases/v0.15.0", &selector).unwrap();

        // A regular file where the approval directory should be makes the
        // token path uninspectable (`ENOTDIR`) for every uid, root included.
        let approvals = dir.join("approvals");
        fs::write(&approvals, "").unwrap();
        let posture = status_posture(&selector, &approvals);
        assert_eq!(posture.posture, None);
        assert_eq!(posture.reason, "token_unreadable");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The real non-root case has valid metadata and a denied read; a
    /// vanished or non-text body keeps its own reason.
    #[test]
    fn a_permission_denied_token_read_is_unreadable_not_untrusted() {
        assert_eq!(
            token_read_failure(&io::Error::from(io::ErrorKind::PermissionDenied)),
            "token_unreadable"
        );
        assert_eq!(
            token_read_failure(&io::Error::from(io::ErrorKind::NotFound)),
            "token_missing"
        );
        assert_eq!(
            token_read_failure(&io::Error::from(io::ErrorKind::InvalidData)),
            "token_untrusted"
        );
    }
}
