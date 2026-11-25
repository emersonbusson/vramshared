// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/sealed_origin.rs
//! Sealed origin configuration parsing, daemon instance validation, and guardian identity health verification.
//!
//! Extracted from `cascade/mod.rs` to keep host-sealed origin and guardian trust verification isolated.

use super::*;
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SealedOriginConfig {
    pub(super) host_manifest_sha256: String,
    pub(super) configuration_sha256: String,
    pub(super) origin_path: String,
    pub(super) partuuid: String,
    pub(super) ptuuid: String,
    pub(super) partition_dev_t: String,
    pub(super) parent_dev_t: String,
    pub(super) expected_swap_uuid: String,
    pub(super) logical_capacity_mib: u64,
    pub(super) physical_cache_cap_mib: u64,
}

pub(super) fn canonical_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(super) fn canonical_device_number(value: &str) -> bool {
    let Some((major, minor)) = value.split_once(':') else {
        return false;
    };
    major.parse::<u64>().is_ok()
        && minor.parse::<u64>().is_ok()
        && !major.starts_with('+')
        && !minor.starts_with('+')
}

pub(super) fn parse_sealed_origin_config(text: &str) -> Result<SealedOriginConfig, CascadeError> {
    let mut values = std::collections::BTreeMap::new();
    for line in text.lines() {
        if line.is_empty() {
            return Err(CascadeError::Precondition(
                "sealed origin manifest contains an empty line".into(),
            ));
        }
        let (key, value) = line.split_once('=').ok_or_else(|| {
            CascadeError::Precondition("sealed origin manifest contains a malformed line".into())
        })?;
        if key.trim() != key || value.trim() != value || key.is_empty() || value.is_empty() {
            return Err(CascadeError::Precondition(
                "sealed origin manifest is not canonical".into(),
            ));
        }
        if values.insert(key, value).is_some() {
            return Err(CascadeError::Precondition(format!(
                "sealed origin manifest repeats key {key}"
            )));
        }
    }
    const KEYS: &[&str] = &[
        "schema_version",
        "host_manifest_sha256",
        "configuration_sha256",
        "origin_path",
        "partuuid",
        "ptuuid",
        "partition_dev_t",
        "parent_dev_t",
        "expected_swap_uuid",
        "swap_type",
        "logical_capacity_mib",
        "physical_cache_cap_mib",
    ];
    if values.len() != KEYS.len() || KEYS.iter().any(|key| !values.contains_key(key)) {
        return Err(CascadeError::Precondition(
            "sealed origin manifest schema is incomplete or contains unknown keys".into(),
        ));
    }
    if values["schema_version"] != "3" || values["swap_type"] != "swap" {
        return Err(CascadeError::Precondition(
            "sealed origin manifest schema or swap type is invalid".into(),
        ));
    }
    let origin_path = values["origin_path"].to_string();
    let partuuid = origin_partuuid(&origin_path)?.to_ascii_lowercase();
    if !values["partuuid"].eq_ignore_ascii_case(&partuuid)
        || !canonical_origin_uuid(values["ptuuid"])
        || !canonical_origin_uuid(values["expected_swap_uuid"])
        || !canonical_device_number(values["partition_dev_t"])
        || !canonical_device_number(values["parent_dev_t"])
        || !canonical_sha256(values["host_manifest_sha256"])
        || !canonical_sha256(values["configuration_sha256"])
    {
        return Err(CascadeError::Precondition(
            "sealed origin manifest identity or hash is invalid".into(),
        ));
    }
    let logical_capacity_mib = values["logical_capacity_mib"]
        .parse::<u64>()
        .map_err(|_| CascadeError::Precondition("sealed logical capacity is invalid".into()))?;
    validate_origin_logical_capacity(logical_capacity_mib)?;
    let physical_cache_cap_mib = values["physical_cache_cap_mib"]
        .parse::<u64>()
        .map_err(|_| CascadeError::Precondition("sealed physical cache cap is invalid".into()))?;
    if physical_cache_cap_mib < DEFAULT_PHYSICAL_CACHE_CAP_MIB
        || physical_cache_cap_mib > logical_capacity_mib
    {
        return Err(CascadeError::Precondition(
            "sealed physical cache cap is outside product policy".into(),
        ));
    }
    Ok(SealedOriginConfig {
        host_manifest_sha256: values["host_manifest_sha256"].to_ascii_lowercase(),
        configuration_sha256: values["configuration_sha256"].to_ascii_lowercase(),
        origin_path,
        partuuid,
        ptuuid: values["ptuuid"].to_ascii_lowercase(),
        partition_dev_t: values["partition_dev_t"].to_string(),
        parent_dev_t: values["parent_dev_t"].to_string(),
        expected_swap_uuid: values["expected_swap_uuid"].to_ascii_lowercase(),
        logical_capacity_mib,
        physical_cache_cap_mib,
    })
}

#[cfg(test)]
pub(super) fn sealed_origin_test_fixture() -> String {
    "schema_version=3\n\
host_manifest_sha256=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n\
configuration_sha256=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n\
origin_path=/dev/disk/by-partuuid/11111111-2222-4333-8444-555555555555\n\
partuuid=11111111-2222-4333-8444-555555555555\n\
ptuuid=aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee\n\
partition_dev_t=8:33\n\
parent_dev_t=8:32\n\
expected_swap_uuid=99999999-8888-4777-8666-555555555555\n\
swap_type=swap\n\
logical_capacity_mib=4096\n\
physical_cache_cap_mib=1024"
        .into()
}

/// Whether the held descriptor is a sealed origin manifest.
///
/// Pure over the `fstat` fields so every rule is testable without minting a
/// root-owned fixture. The production reader runs this against the descriptor
/// it holds for the whole read, never against a path re-stat. The rules are
/// the ones this reader has always enforced: a regular file (not a symlink),
/// root-owned, not group- or world-writable, non-empty, and at most
/// [`SEALED_ORIGIN_CONFIG_MAX_BYTES`] bytes.
pub(super) fn sealed_origin_document_trust(
    file_is_regular: bool,
    file_mode: u32,
    file_uid: u32,
    file_len: u64,
) -> Result<(), &'static str> {
    if !file_is_regular {
        return Err("sealed origin manifest must be a regular file");
    }
    if file_uid != 0 {
        return Err("sealed origin manifest must be root-owned");
    }
    if file_mode & 0o022 != 0 {
        return Err("sealed origin manifest must not be group- or world-writable");
    }
    if file_len == 0 {
        return Err("sealed origin manifest must not be empty");
    }
    if file_len > SEALED_ORIGIN_CONFIG_MAX_BYTES {
        return Err("sealed origin manifest exceeds the size bound");
    }
    Ok(())
}

/// Read and parse the sealed origin manifest at `path`.
///
/// The parent directory is opened `O_DIRECTORY|O_NOFOLLOW` and the manifest is
/// opened **relative to that descriptor**, so the file this reader stats is
/// provably a child of the directory it is about to trust. Every predicate
/// runs on that held descriptor's `fstat`; the bytes are read from the same
/// descriptor. No path re-lookup sits between the check and the bytes
///.
pub(super) fn read_sealed_origin_config_from(
    path: &Path,
) -> Result<SealedOriginConfig, CascadeError> {
    use rustix::fs::{FileType, Mode, OFlags, fstat, open, openat};

    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    let parent = parent.unwrap_or_else(|| Path::new("."));
    let name = path.file_name().ok_or_else(|| {
        CascadeError::Io("stat sealed origin manifest: path has no file name".into())
    })?;

    let publication_point = open(
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| CascadeError::Io(format!("stat sealed origin manifest: {error}")))?;
    let file = openat(
        &publication_point,
        name,
        // `NONBLOCK` so a fifo planted at the path cannot hang the open: the
        // type check below refuses it, and the flag is a no-op for a regular
        // file's reads.
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|error| {
        // A symlink is not a sealed manifest, and the open is the check that
        // refuses it. Keep the user-visible contract of the pre-descriptor
        // reader: a redirect is the same refusal as any other untrusted shape.
        if error == rustix::io::Errno::LOOP {
            CascadeError::Precondition(
                "sealed origin manifest is not a bounded root-owned regular file".into(),
            )
        } else {
            CascadeError::Io(format!("stat sealed origin manifest: {error}"))
        }
    })?;

    let file_stat = fstat(&file)
        .map_err(|error| CascadeError::Io(format!("stat sealed origin manifest: {error}")))?;
    let file_len = u64::try_from(file_stat.st_size).map_err(|_| {
        CascadeError::Precondition(
            "sealed origin manifest is not a bounded root-owned regular file".into(),
        )
    })?;
    sealed_origin_document_trust(
        FileType::from_raw_mode(file_stat.st_mode) == FileType::RegularFile,
        file_stat.st_mode,
        file_stat.st_uid,
        file_len,
    )
    .map_err(|_| {
        CascadeError::Precondition(
            "sealed origin manifest is not a bounded root-owned regular file".into(),
        )
    })?;

    // The size bound is enforced on the bytes themselves, so a file that grows
    // between `fstat` and the read cannot smuggle more than the bound.
    let mut text = String::new();
    let mut limited = std::io::Read::take(
        std::fs::File::from(file),
        SEALED_ORIGIN_CONFIG_MAX_BYTES + 1,
    );
    std::io::Read::read_to_string(&mut limited, &mut text)
        .map_err(|error| CascadeError::Io(format!("read sealed origin manifest: {error}")))?;
    if u64::try_from(text.len()).unwrap_or(u64::MAX) > SEALED_ORIGIN_CONFIG_MAX_BYTES {
        return Err(CascadeError::Precondition(
            "sealed origin manifest is not a bounded root-owned regular file".into(),
        ));
    }
    parse_sealed_origin_config(&text)
}

pub(super) fn read_sealed_origin_config() -> Result<SealedOriginConfig, CascadeError> {
    #[cfg(test)]
    {
        let configured = TEST_ORIGIN_CONFIG.with(|cell| cell.borrow().clone());
        let text = match configured {
            Some(Ok(text)) => text,
            Some(Err(message)) => return Err(CascadeError::Io(message)),
            None => sealed_origin_test_fixture(),
        };
        parse_sealed_origin_config(&text)
    }
    #[cfg(not(test))]
    {
        read_sealed_origin_config_from(Path::new(ORIGIN_CONFIG_FILE))
    }
}

pub(super) fn origin_partuuid(path: &str) -> Result<&str, CascadeError> {
    let uuid = path.strip_prefix("/dev/disk/by-partuuid/").ok_or_else(|| {
        CascadeError::Precondition(
            "origin must use the sealed /dev/disk/by-partuuid/<uuid> identity".into(),
        )
    })?;
    let valid = uuid.len() == 36
        && uuid.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        });
    if !valid {
        return Err(CascadeError::Precondition(
            "origin PARTUUID identity is invalid or aliases the WSL fallback swap".into(),
        ));
    }
    Ok(uuid)
}

pub(super) fn canonical_origin_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

pub(super) fn validate_origin_logical_capacity(value_mib: u64) -> Result<(), CascadeError> {
    if (1024..=24 * 1024).contains(&value_mib) {
        Ok(())
    } else {
        Err(CascadeError::Precondition(
            "origin logical capacity must be between 1024 and 24576 MiB".into(),
        ))
    }
}

pub(super) fn unix_time_ms() -> Option<u64> {
    use std::time::UNIX_EPOCH;

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
}

pub(super) fn process_start_ticks(stat: &str) -> Option<&str> {
    stat.rsplit_once(") ")?.1.split_whitespace().nth(19)
}

pub(super) fn daemon_instance_id_from_pid(pid: u32) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let start_ticks = process_start_ticks(&stat)?;
    (!start_ticks.is_empty()).then(|| format!("{pid}-{start_ticks}"))
}

pub(super) fn cache_status_shape_is_valid(value: &serde_json::Value) -> bool {
    value
        .get("ok")
        .and_then(serde_json::Value::as_bool)
        .is_some()
        && matches!(
            value
                .get("origin_state")
                .and_then(serde_json::Value::as_str),
            Some("OFF") | Some("READY") | Some("DEGRADED") | Some("FAILED")
        )
        && matches!(
            value.get("cache_state").and_then(serde_json::Value::as_str),
            Some("OFF") | Some("ACTIVE") | Some("RESTRICTED") | Some("UNAVAILABLE") | Some("STUCK")
        )
}

pub(super) fn cache_status_matches_current_daemon(
    value: &serde_json::Value,
    daemon_instance_id: &str,
    now_unix_ms: u64,
) -> bool {
    value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        == Some(1)
        && value
            .get("daemon_instance_id")
            .and_then(serde_json::Value::as_str)
            == Some(daemon_instance_id)
        && value
            .get("written_at_unix_ms")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|written_at| {
                now_unix_ms >= written_at
                    && now_unix_ms.saturating_sub(written_at) <= CONTROL_STATUS_MAX_AGE_MS
            })
}

/// Is this cache-status document the current daemon's live one?
///
/// Shape, identity and age in one answer. The cascade's snapshot builder and
/// the readiness run both gate the document through here before any field is
/// reduced, so no caller restates a clause and gets it half right. A document
/// that fails is refused **whole** — never field by field — because a field of
/// a foreign or expired document is not that field of the live one. That is
/// the boundary the honesty rule draws: a per-field collapse inside an
/// admitted document is the bug, a document-level refusal is the policy.
///
/// The clock is passed in rather than read here so the rule is a pure
/// decision and a test can pin every arm without owning a daemon or a clock.
pub(crate) fn cache_status_is_current_daemon(
    value: &serde_json::Value,
    daemon_instance_id: &str,
    now_unix_ms: u64,
) -> bool {
    cache_status_shape_is_valid(value)
        && cache_status_matches_current_daemon(value, daemon_instance_id, now_unix_ms)
}

pub(super) fn supervisor_status_matches_current_daemon(
    value: &serde_json::Value,
    daemon_instance_id: &str,
    now_unix_ms: u64,
) -> bool {
    value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        == Some(3)
        && value
            .get("daemon_instance_id")
            .and_then(serde_json::Value::as_str)
            == Some(daemon_instance_id)
        && value
            .get("written_at_unix_ms")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|written_at| {
                now_unix_ms >= written_at
                    && now_unix_ms.saturating_sub(written_at) <= CONTROL_STATUS_MAX_AGE_MS
            })
}

#[cfg(test)]
pub(super) fn control_plane_status_is_current(
    cache_status: &serde_json::Value,
    supervisor_status: &serde_json::Value,
    daemon_instance_id: &str,
    now_unix_ms: u64,
) -> bool {
    let (cache_current, supervisor_current) = control_plane_status_freshness(
        Some(cache_status),
        Some(supervisor_status),
        daemon_instance_id,
        now_unix_ms,
    );
    cache_current && supervisor_current
}

pub(super) fn control_plane_status_freshness(
    cache_status: Option<&serde_json::Value>,
    supervisor_status: Option<&serde_json::Value>,
    daemon_instance_id: &str,
    now_unix_ms: u64,
) -> (bool, bool) {
    let cache_current = cache_status.is_some_and(|status| {
        cache_status_is_current_daemon(status, daemon_instance_id, now_unix_ms)
    });
    let supervisor_current = supervisor_status.is_some_and(|status| {
        supervisor_status_shape_is_valid(status)
            && supervisor_status_matches_current_daemon(status, daemon_instance_id, now_unix_ms)
    });

    (cache_current, supervisor_current)
}

/// Is a guardian health proof current enough to trust?
///
/// The contract is "the proof is **not older than** `max_age`", which is a
/// lower bound on the timestamp. A mtime a little ahead of the local clock is
/// therefore the freshest possible proof — the two clocks simply disagree —
/// not a stale one. Folding that case into staleness is what made a just-written
/// proof report `guardian_state_stale`: the publisher rewrites the file every
/// few seconds, so the ramp-step check lands nearest a write exactly when the
/// local clock is behind.
///
/// A mtime further ahead than `max_age` cannot be a real write from a publisher
/// that refreshes inside the same window, so it is not trusted either. An
/// unreadable mtime is handled by the caller and stays fail-closed.
pub(super) fn proof_is_fresh(modified: SystemTime, now: SystemTime, max_age: Duration) -> bool {
    match now.duration_since(modified) {
        Ok(age) => age <= max_age,
        Err(ahead) => ahead.duration() <= max_age,
    }
}

pub(super) fn guardian_state_from_files(
    safe_mode: &Path,
    health: &Path,
    max_age: Duration,
    expected: &sealed_identity::SealedIdentity,
) -> (GuardianState, Option<String>) {
    // Presence of the marker is the gate. Validate content only
    // when readable; an unreadable marker is still a marker (fail-safe #16).
    match fs::symlink_metadata(safe_mode) {
        Ok(_) => {
            return match fs::read_to_string(safe_mode) {
                Ok(text) if serde_json::from_str::<serde_json::Value>(&text).is_ok() => {
                    (GuardianState::SafeMode, None)
                }
                Ok(_) => (GuardianState::Blocked, Some("safe_mode_invalid".into())),
                Err(_) => (GuardianState::SafeMode, None),
            };
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return (GuardianState::Blocked, Some("safe_mode_unreadable".into())),
    }
    let text = match fs::read_to_string(health) {
        Ok(text) => text,
        Err(_) => {
            return (
                GuardianState::Blocked,
                Some("guardian_state_unavailable".into()),
            );
        }
    };
    let fresh = fs::metadata(health)
        .and_then(|metadata| metadata.modified())
        .ok()
        .is_some_and(|modified| proof_is_fresh(modified, SystemTime::now(), max_age));
    if !fresh {
        return (GuardianState::Blocked, Some("guardian_state_stale".into()));
    }
    let json = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let value = match serde_json::from_str::<serde_json::Value>(json) {
        Ok(value) => value,
        Err(_) => {
            return (
                GuardianState::Blocked,
                Some("guardian_state_invalid".into()),
            );
        }
    };
    // The proof is accepted only for the sealed `(distro, user_sid)` pair.
    // A proof naming another user or another distro is someone else's proof
    // (AUD-40); a proof missing either half is ambiguous and refused.
    if !sealed_identity::health_belongs_to(&value, expected) {
        return (
            GuardianState::Blocked,
            Some("guardian_state_identity_mismatch".into()),
        );
    }
    match value.get("state").and_then(serde_json::Value::as_str) {
        Some("HEALTHY") => (GuardianState::Healthy, None),
        Some("SAFE_MODE") => (GuardianState::SafeMode, None),
        _ => (
            GuardianState::Blocked,
            Some("guardian_reported_blocked".into()),
        ),
    }
}

/// Resolve the sealed identity and read **its** guardian state.
///
/// Fail closed when no identity is sealed: an unsealed identity is not
/// authorization to fall back to a default distro name (AUD-40). The sealed
/// config names the distro; the health proof is located from that name and is
/// accepted only for that pair.
pub(super) fn sealed_guardian_state_from(
    config: &Path,
    state_dir: &Path,
    safe_mode: &Path,
) -> (GuardianState, Option<String>) {
    let identity = match sealed_identity::read_sealed_identity(config) {
        Ok(identity) => identity,
        Err(error) => return (GuardianState::Blocked, Some(error.as_str().into())),
    };
    let health = identity.health_path(state_dir);
    guardian_state_from_files(safe_mode, &health, Duration::from_secs(15), &identity)
}

/// Production guardian state, bound to the host-sealed identity.
pub(super) fn sealed_guardian_state() -> (GuardianState, Option<String>) {
    sealed_guardian_state_from(
        Path::new(sealed_identity::GUARDIAN_CONFIG_FILE),
        Path::new(sealed_identity::GUARDIAN_STATE_DIR),
        Path::new(SAFE_MODE_FILE),
    )
}

/// Sealed distro label for a user-facing recovery action.
///
/// Returns a neutral phrase when no identity is sealed, so a recovery message
/// never names a distro that was not selected (AUD-40).
pub(super) fn sealed_distro_or_unknown() -> String {
    sealed_identity::read_sealed_identity(Path::new(sealed_identity::GUARDIAN_CONFIG_FILE))
        .map(|identity| identity.distro)
        .unwrap_or_else(|_| "the selected distro".to_string())
}
