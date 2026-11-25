// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/control_requests.rs
//! Control-plane vsock lease attempts, daemon instance identity, and reclaim request parsing.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ramshared_block::{CacheState as OriginCacheState, OriginState as DurableOriginState};
use ramshared_wsl2d::control_plane::{NegotiatedCadence, VsockControlPlane};
use ramshared_wsl2d::host_gate::{SealedOrigin, SessionAuthority};

pub(crate) const CACHE_TARGET_REQUEST_PATH: &str = "/run/ramshared/cache-target.json";
pub(crate) const RECLAIM_REQUEST_PATH: &str = "/run/ramshared/reclaim-request.json";
pub(crate) const DEFAULT_VSOCK_CONFIG_PATH: &str = "/etc/ramshared/vsock.toml";
pub(crate) const CONTROL_REQUEST_MAX_AGE_MS: u64 = 15_000;
const MAX_VSOCK_CONFIG_BYTES: u64 = 4096;

/// Environment/sysfs/config inputs for [`attempt_vsock_lease_with`].
pub(crate) struct VsockLeaseInputs {
    pub(crate) port_raw: Option<String>,
    pub(crate) key_file: Option<String>,
    pub(crate) mode: Option<String>,
    pub(crate) boot_id_path: PathBuf,
    pub(crate) os_release_path: PathBuf,
}

impl VsockLeaseInputs {
    pub(crate) fn from_env() -> Self {
        Self::from_env_with_config(Path::new(DEFAULT_VSOCK_CONFIG_PATH))
    }

    pub(crate) fn from_env_with_config(config_path: &Path) -> Self {
        let mut port_raw = std::env::var("RAMSHARED_VSOCK_PORT")
            .ok()
            .filter(|v| !v.is_empty());
        let mut key_file = std::env::var("RAMSHARED_VSOCK_KEY_FILE")
            .ok()
            .filter(|v| !v.is_empty());
        let mut mode = std::env::var("RAMSHARED_VSOCK_MODE")
            .ok()
            .filter(|v| !v.is_empty());
        if (port_raw.is_none() || key_file.is_none() || mode.is_none())
            && let Some((cfg_port, cfg_key, cfg_mode)) = read_verified_vsock_config(config_path)
        {
            if port_raw.is_none() {
                port_raw = cfg_port;
            }
            if key_file.is_none() {
                key_file = cfg_key;
            }
            if mode.is_none() {
                mode = cfg_mode;
            }
        }
        Self {
            port_raw,
            key_file,
            mode,
            boot_id_path: PathBuf::from("/proc/sys/kernel/random/boot_id"),
            os_release_path: PathBuf::from("/etc/os-release"),
        }
    }
}

pub(crate) fn parse_vsock_toml(
    text: &str,
) -> Option<(Option<String>, Option<String>, Option<String>)> {
    let mut in_vsock = false;
    let mut port = None;
    let mut key_file = None;
    let mut mode = None;
    for raw_line in text.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            in_vsock = line[1..line.len() - 1].trim() == "vsock";
            if !in_vsock {
                return None;
            }
            continue;
        }
        if !in_vsock {
            return None;
        }
        let (key, val) = line.split_once('=')?;
        let key = key.trim();
        let val = val.trim();
        match key {
            "port" => {
                let parsed: u32 = val.parse().ok()?;
                if !(1..=0x7FFF_FFFF).contains(&parsed) || port.is_some() {
                    return None;
                }
                port = Some(parsed.to_string());
            }
            "key_file" => {
                let unquoted = strip_toml_string(val)?;
                if !unquoted.starts_with('/') || key_file.is_some() {
                    return None;
                }
                key_file = Some(unquoted.to_string());
            }
            "mode" => {
                let unquoted = strip_toml_string(val)?;
                if unquoted.is_empty() || mode.is_some() {
                    return None;
                }
                mode = Some(unquoted.to_string());
            }
            _ => return None,
        }
    }
    in_vsock.then_some((port, key_file, mode))
}

fn strip_toml_string(raw: &str) -> Option<&str> {
    raw.strip_prefix('"')?.strip_suffix('"')
}

pub(crate) fn read_verified_vsock_config(
    path: &Path,
) -> Option<(Option<String>, Option<String>, Option<String>)> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.file_type().is_file() || meta.len() > MAX_VSOCK_CONFIG_BYTES {
        return None;
    }
    let euid = rustix::process::geteuid().as_raw();
    if (meta.uid() != 0 && meta.uid() != euid) || (meta.mode() & 0o077) != 0 {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    parse_vsock_toml(&text)
}

/// One bounded attempt to reach the Windows peer and mint a lease.
///
/// `RAMSHARED_VSOCK_PORT` and `RAMSHARED_VSOCK_KEY_FILE` are the operator's
/// provisioning surface (the key is a root-only file, never a constant).
/// Any absence or failure returns `None` — never a lease.
pub(crate) fn attempt_vsock_lease(
    sealed: &SealedOrigin,
    authority_mirror: SessionAuthority,
) -> Option<(
    VsockControlPlane<Box<dyn ramshared_ipc::vsock::VsockStreamTrait>>,
    NegotiatedCadence,
)> {
    attempt_vsock_lease_with(
        sealed,
        authority_mirror,
        VsockLeaseInputs::from_env(),
        |listen_mode, port| {
            const VMADDR_CID_HOST: u32 = 2;
            if listen_mode {
                ramshared_ipc::vsock::listen_vsock(port, Duration::from_secs(15))
            } else {
                ramshared_ipc::vsock::connect_vsock(VMADDR_CID_HOST, port, Duration::from_secs(5))
            }
        },
        unix_time_ms,
    )
}

pub(crate) fn attempt_vsock_lease_with(
    sealed: &SealedOrigin,
    authority_mirror: SessionAuthority,
    inputs: VsockLeaseInputs,
    open_stream: impl FnOnce(
        bool,
        u32,
    ) -> Result<
        Box<dyn ramshared_ipc::vsock::VsockStreamTrait>,
        ramshared_ipc::vsock::VsockError,
    >,
    now_ms: impl FnOnce() -> Option<u64>,
) -> Option<(
    VsockControlPlane<Box<dyn ramshared_ipc::vsock::VsockStreamTrait>>,
    NegotiatedCadence,
)> {
    let port: u32 = inputs.port_raw?.parse().ok()?;
    if !(1..=0x7FFF_FFFF).contains(&port) {
        return None;
    }
    let key_path = inputs.key_file?;
    let secret = std::fs::read(key_path).ok()?;
    if secret.is_empty() || secret.len() > 4096 {
        return None;
    }
    let boot_id = std::fs::read_to_string(&inputs.boot_id_path)
        .ok()?
        .trim()
        .to_string();
    if boot_id.is_empty() {
        return None;
    }
    let distro_id = std::fs::read_to_string(&inputs.os_release_path)
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("ID=").map(str::to_string))
        })
        .unwrap_or_else(|| "unknown".to_string());
    let mode = inputs.mode.unwrap_or_default();
    let listen_mode = mode.eq_ignore_ascii_case("listen")
        || mode.eq_ignore_ascii_case("guest_listener")
        || mode.eq_ignore_ascii_case("guest-listen");
    let stream = match open_stream(listen_mode, port) {
        Ok(stream) => stream,
        Err(error) => {
            if listen_mode {
                eprintln!("[ramsharedd] the vsock accept window failed: {error}");
            }
            return None;
        }
    };
    let mut plane = VsockControlPlane::new(
        stream,
        secret,
        boot_id,
        distro_id,
        sealed.clone(),
        Duration::from_secs(5),
    )
    .with_authority_mirror(authority_mirror);
    if let Err(error) = plane.perform_handshake(now_ms()?) {
        eprintln!("[ramsharedd] the vsock handshake failed: {error}");
        return None;
    }
    let cadence = plane.cadence()?;
    Some((plane, cadence))
}

pub(crate) fn origin_cache_runtime_ok(origin: DurableOriginState, cache: OriginCacheState) -> bool {
    origin == DurableOriginState::Ready && cache != OriginCacheState::Stuck
}

pub(crate) fn unix_time_ms() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
}

fn process_start_ticks(stat: &str) -> Option<&str> {
    stat.rsplit_once(") ")?.1.split_whitespace().nth(19)
}

pub(crate) fn daemon_instance_id() -> Option<String> {
    let pid = std::process::id();
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let start_ticks = process_start_ticks(&stat)?;
    (pid > 0 && !start_ticks.is_empty() && start_ticks.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| format!("{pid}-{start_ticks}"))
}

fn valid_daemon_instance_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

pub(crate) fn require_origin_daemon_identity(
    origin_mode: bool,
    identity: impl FnOnce() -> Option<String>,
) -> Result<Option<String>, String> {
    if !origin_mode {
        return Ok(None);
    }
    let identity = identity()
        .filter(|value| valid_daemon_instance_id(value))
        .ok_or_else(|| {
            "origin-cache startup requires a valid daemon instance identity".to_string()
        })?;
    Ok(Some(identity))
}

fn control_request_matches(
    value: &serde_json::Value,
    daemon_instance_id: &str,
    now_unix_ms: u64,
) -> bool {
    value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        == Some(1)
        && value.get("reason").and_then(serde_json::Value::as_str) == Some("control_pressure")
        && value
            .get("daemon_instance_id")
            .and_then(serde_json::Value::as_str)
            == Some(daemon_instance_id)
        && value
            .get("issued_at_unix_ms")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|issued_at| {
                now_unix_ms >= issued_at
                    && now_unix_ms.saturating_sub(issued_at) <= CONTROL_REQUEST_MAX_AGE_MS
            })
}

pub(crate) fn consume_critical_cache_request(
    value: &serde_json::Value,
    daemon_instance_id: &str,
    now_unix_ms: u64,
) -> Option<u64> {
    control_request_matches(value, daemon_instance_id, now_unix_ms)
        .then(|| {
            value
                .get("target_bytes")
                .and_then(serde_json::Value::as_u64)
        })
        .flatten()
        .filter(|target_bytes| *target_bytes == 0)
}

fn current_control_request_at(
    path: &Path,
    daemon_instance_id: &str,
    now_unix_ms: u64,
) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(path).ok()?;
    let value = serde_json::from_str(&text).ok()?;
    control_request_matches(&value, daemon_instance_id, now_unix_ms).then_some(value)
}

pub(crate) fn critical_cache_reclaim_requested_at(
    cache_target_path: &Path,
    reclaim_request_path: &Path,
    daemon_instance_id: &str,
    now_unix_ms: u64,
) -> bool {
    current_control_request_at(cache_target_path, daemon_instance_id, now_unix_ms)
        .as_ref()
        .and_then(|value| consume_critical_cache_request(value, daemon_instance_id, now_unix_ms))
        == Some(0)
        || current_control_request_at(reclaim_request_path, daemon_instance_id, now_unix_ms)
            .is_some()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn vsock_toml_parser_and_file_verification_fail_closed() {
        assert_eq!(
            parse_vsock_toml(
                "# guest control plane\n[vsock]\nport = 2761\nkey_file = \"/etc/ramshared/vsock.key\"\nmode = \"listen\"\n"
            ),
            Some((
                Some("2761".into()),
                Some("/etc/ramshared/vsock.key".into()),
                Some("listen".into())
            ))
        );
        assert!(parse_vsock_toml("port = 2761\n").is_none());
        assert!(parse_vsock_toml("[other]\nport = 2761\n").is_none());
        assert!(parse_vsock_toml("[vsock]\nport = 0\n").is_none());
        assert!(parse_vsock_toml("[vsock]\nkey_file = \"relative/key\"\n").is_none());
        assert!(parse_vsock_toml("[vsock]\nunknown = 1\n").is_none());

        let base = std::env::temp_dir().join(format!("ramshared-vsock-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let cfg = base.join("vsock.toml");
        std::fs::write(
            &cfg,
            "[vsock]\nport = 2761\nkey_file = \"/etc/ramshared/vsock.key\"\nmode = \"listen\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            read_verified_vsock_config(&cfg).is_none(),
            "world/group readable config must be refused"
        );
        std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o600)).unwrap();
        let loaded = VsockLeaseInputs::from_env_with_config(&cfg);
        assert_eq!(loaded.port_raw.as_deref(), Some("2761"));
        assert_eq!(loaded.key_file.as_deref(), Some("/etc/ramshared/vsock.key"));
        assert_eq!(loaded.mode.as_deref(), Some("listen"));

        let link = base.join("link.toml");
        std::os::unix::fs::symlink(&cfg, &link).unwrap();
        assert!(read_verified_vsock_config(&link).is_none());
        let _ = std::fs::remove_dir_all(&base);
    }
}
