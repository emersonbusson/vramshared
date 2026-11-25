// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/root_status_cache.rs
//! Root-published read-only status view for unprivileged `status` and `top` readers.
//!
//! Root-owned `ramshared-cascade-health.service` already observes `/proc/<pid>/exe`,
//! `/run/ramshared/swap-dev` (`0600`), and `/var/lib/ramshared/approvals/activate-*.token`
//! (`0400`) once per second. An unprivileged operator running `ramshared top` receives
//! `EACCES` on those three surfaces and would otherwise stay permanently `GUARDED`
//! (`release=unknown`) even while the live root monitor reports `HEALTHY`.
//!
//! This module publishes ONLY non-secret status words into `/run/ramshared/root-status-view.json`
//! (`0644 root:root`) and verifies descriptor ownership (`uid == 0`, non-link, non-FIFO,
//! not group/world writable), bounded size, freshness (`<= 10s`), current release selector,
//! and current `ramsharedd` instance identity before admitting a fallback read. Root readers
//! NEVER consume this cache; any mismatch, staleness, or untrusted file fails closed.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cascade::health_documents::StatusPosture;
use crate::cascade::posture::Posture;
use crate::cascade::snapshot::device_basename;
use crate::cascade::{SWAP_DEV_FILE, current_daemon_instance_id, unix_time_ms};
use crate::release_coherence::{ReleaseCoherence, selected_release_version};

/// World-readable (`0644 root:root`) non-secret observation summary published by root monitor.
pub(crate) const ROOT_STATUS_VIEW_FILE: &str = "/run/ramshared/root-status-view.json";

/// Maximum accepted sample age before unprivileged readers fall back to fail-closed `Unknown`.
pub(crate) const ROOT_STATUS_VIEW_MAX_AGE_MS: u64 = 10_000;

/// Small forward clock-skew tolerance; future-dated documents beyond this are refused.
const FUTURE_SKEW_TOLERANCE_MS: u64 = 1_000;

/// Hard byte ceiling for the status view document.
const ROOT_STATUS_VIEW_MAX_BYTES: u64 = 2048;

const SCHEMA_VERSION: u8 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RootStatusView {
    pub(crate) schema_version: u8,
    pub(crate) published_at_unix_ms: u64,
    pub(crate) selected_release: Option<String>,
    pub(crate) daemon_instance_id: Option<String>,
    pub(crate) release_coherence: String,
    pub(crate) tier_device: Option<String>,
    pub(crate) posture: Option<String>,
    pub(crate) posture_reason: String,
}

fn current_uid() -> Option<u32> {
    fs::metadata("/proc/self").ok().map(|meta| meta.uid())
}

fn parse_release_coherence(raw: &str) -> Option<ReleaseCoherence> {
    match raw {
        "coherent" => Some(ReleaseCoherence::Coherent),
        "stale" => Some(ReleaseCoherence::Stale),
        "unknown" => Some(ReleaseCoherence::Unknown),
        _ => None,
    }
}

fn parse_status_posture(raw: &str) -> Option<Posture> {
    match raw {
        "writeback" => Some(Posture::Writeback),
        "lab_swap" => Some(Posture::Swap),
        _ => None,
    }
}

fn is_safe_device_basename(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !matches!(name, "." | "..")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn validate_view_fields(view: &RootStatusView) -> Result<(), &'static str> {
    if view.schema_version != SCHEMA_VERSION {
        return Err("unsupported root status view schema");
    }
    if view.published_at_unix_ms == 0 {
        return Err("missing publish timestamp");
    }
    if parse_release_coherence(&view.release_coherence).is_none() {
        return Err("invalid release_coherence value");
    }
    if let Some(device) = view.tier_device.as_deref()
        && !is_safe_device_basename(device)
    {
        return Err("invalid tier_device basename");
    }
    if let Some(posture) = view.posture.as_deref()
        && parse_status_posture(posture).is_none()
    {
        return Err("invalid posture value");
    }
    if view.posture_reason.is_empty() || view.posture_reason.len() > 64 {
        return Err("invalid posture_reason");
    }
    Ok(())
}

/// Atomically publish one validated root status view (`0644`, `O_CREAT|O_EXCL` temp -> rename)
/// after verifying the parent directory is owned by `expected_dir_uid` and not group/world writable.
pub(crate) fn publish_root_status_view_at(
    path: &Path,
    expected_dir_uid: u32,
    view: &RootStatusView,
) -> Result<(), String> {
    validate_view_fields(view).map_err(|reason| reason.to_string())?;
    let parent = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .ok_or_else(|| "root status view path has no parent directory".to_string())?;
    let parent_meta =
        fs::symlink_metadata(parent).map_err(|error| format!("stat parent dir: {error}"))?;
    if parent_meta.file_type().is_symlink() || !parent_meta.is_dir() {
        return Err("root status view parent is not a real directory".to_string());
    }
    if parent_meta.uid() != expected_dir_uid || parent_meta.mode() & 0o022 != 0 {
        return Err("root status view parent has untrusted ownership or mode".to_string());
    }
    let body = serde_json::to_vec(view).map_err(|error| error.to_string())?;
    if body.len() as u64 >= ROOT_STATUS_VIEW_MAX_BYTES {
        return Err("root status view exceeds size ceiling".to_string());
    }
    let leaf = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| "root status view has invalid filename".to_string())?;
    let temporary = parent.join(format!(".{leaf}.{}.tmp", std::process::id()));
    let mut created = false;
    let written = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&temporary)
            .map_err(|error| error.to_string())?;
        created = true;
        file.write_all(&body).map_err(|error| error.to_string())?;
        file.write_all(b"\n").map_err(|error| error.to_string())?;
        file.set_permissions(fs::Permissions::from_mode(0o644))
            .map_err(|error| error.to_string())?;
        fs::rename(&temporary, path).map_err(|error| error.to_string())
    })();
    if written.is_err() && created {
        let _ = fs::remove_file(&temporary);
    }
    written
}

/// Construct a validated [`RootStatusView`] from one root monitor observation map.
pub(crate) fn view_from_observation(
    status: &BTreeMap<String, Value>,
    published_at_unix_ms: u64,
    selected_release: Option<String>,
    daemon_instance_id: Option<String>,
    swap_dev_path: &Path,
) -> Option<RootStatusView> {
    if published_at_unix_ms == 0 {
        return None;
    }
    let release_coherence = status
        .get("release_coherence")
        .and_then(Value::as_str)
        .filter(|value| parse_release_coherence(value).is_some())?;
    let posture = status
        .get("posture")
        .and_then(Value::as_str)
        .filter(|value| parse_status_posture(value).is_some())
        .map(str::to_string);
    let posture_reason = status
        .get("posture_reason")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let tier_device = fs::read_to_string(swap_dev_path)
        .ok()
        .map(|text| device_basename(&text))
        .filter(|name| is_safe_device_basename(name));
    let view = RootStatusView {
        schema_version: SCHEMA_VERSION,
        published_at_unix_ms,
        selected_release,
        daemon_instance_id,
        release_coherence: release_coherence.to_string(),
        tier_device,
        posture,
        posture_reason,
    };
    validate_view_fields(&view).ok()?;
    Some(view)
}

/// Publish the live root-owned monitor observation to `/run/ramshared/root-status-view.json`.
/// No-op when not running as root (`uid != 0`).
pub(crate) fn publish_from_observation(
    status: &BTreeMap<String, Value>,
    published_at_unix_ms: u64,
) {
    if current_uid() != Some(0) {
        return;
    }
    let Ok(selected_release) = selected_release_version() else {
        return;
    };
    let Some(view) = view_from_observation(
        status,
        published_at_unix_ms,
        selected_release,
        current_daemon_instance_id(),
        Path::new(SWAP_DEV_FILE),
    ) else {
        return;
    };
    let _ = publish_root_status_view_at(Path::new(ROOT_STATUS_VIEW_FILE), 0, &view);
}

/// Open, `fstat`-verify, parse, and bind one root status view against freshness,
/// selected release, and live daemon instance identity.
pub(crate) fn read_verified_root_status_view_at(
    path: &Path,
    expected_uid: u32,
    now_unix_ms: u64,
    expected_selected_release: &Option<String>,
    expected_daemon_instance_id: &Option<String>,
) -> Option<RootStatusView> {
    let file = crate::release_identity::open_regular_no_follow(path)?;
    let meta = file.metadata().ok()?;
    if meta.uid() != expected_uid
        || meta.mode() & 0o022 != 0
        || meta.len() == 0
        || meta.len() > ROOT_STATUS_VIEW_MAX_BYTES
    {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(ROOT_STATUS_VIEW_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.is_empty() || bytes.len() as u64 > ROOT_STATUS_VIEW_MAX_BYTES {
        return None;
    }
    let view: RootStatusView = serde_json::from_slice(&bytes).ok()?;
    validate_view_fields(&view).ok()?;
    if view.published_at_unix_ms > now_unix_ms.saturating_add(FUTURE_SKEW_TOLERANCE_MS) {
        return None;
    }
    if now_unix_ms.saturating_sub(view.published_at_unix_ms) > ROOT_STATUS_VIEW_MAX_AGE_MS {
        return None;
    }
    if &view.selected_release != expected_selected_release
        || &view.daemon_instance_id != expected_daemon_instance_id
    {
        return None;
    }
    Some(view)
}

fn read_verified_live_view() -> Option<RootStatusView> {
    if current_uid() == Some(0) {
        return None;
    }
    let selected_release = selected_release_version().ok()?;
    let daemon_instance_id = current_daemon_instance_id();
    let now_unix_ms = unix_time_ms()?;
    read_verified_root_status_view_at(
        Path::new(ROOT_STATUS_VIEW_FILE),
        0,
        now_unix_ms,
        &selected_release,
        &daemon_instance_id,
    )
}

/// Verified root-observed release coherence for unprivileged readers whose direct
/// `/proc/<pid>/exe` inspection returned `Unknown`.
pub(crate) fn verified_release_coherence() -> Option<ReleaseCoherence> {
    let view = read_verified_live_view()?;
    parse_release_coherence(&view.release_coherence)
}

/// Verified root-observed managed tier device basename (`nbd0`, `ublkb0`, ...) for
/// unprivileged readers blocked by `EACCES` on `/run/ramshared/swap-dev`.
pub(crate) fn verified_tier_device() -> Option<String> {
    read_verified_live_view()?.tier_device
}

/// Project a verified root status view into an operator posture summary only
/// when the root observer proved `token_binding`.
pub(crate) fn posture_from_view(view: &RootStatusView) -> Option<StatusPosture> {
    if view.posture_reason != "token_binding" {
        return None;
    }
    let posture = parse_status_posture(view.posture.as_deref()?)?;
    Some(StatusPosture {
        posture: Some(posture),
        reason: "root_monitor_view",
    })
}

/// Verified root-observed posture for unprivileged readers blocked by `EACCES` on
/// the `0400` release activation token.
pub(crate) fn verified_posture() -> Option<StatusPosture> {
    posture_from_view(&read_verified_live_view()?)
}

#[cfg(test)]
#[path = "root_status_cache/tests.rs"]
mod tests;
