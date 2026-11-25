// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/origin_identity.rs
//! Sealed WSL2 block-origin manifest policy and measured identity comparison.
//!
//! This owner verifies metadata; it does not open devices, admit native file
//! origins, activate swap or acquire cache authority.

use std::path::Path;

use ramshared_block::sparse_vram::DEFAULT_CHUNK_MIB;
use ramshared_vram::{SEALED_RESERVE_MIN_MIB, SEALED_RESERVE_PERCENT};

use super::manifest_file::read_bounded_manifest_file;
use super::{GIB, HOST_ORIGIN_MANIFEST_PATH, ORIGIN_MANIFEST_PATH};

const MIB: u64 = 1024 * 1024;
pub(super) const MIN_ORIGIN_LOGICAL_SIZE: u64 = GIB;
pub(super) const MAX_ORIGIN_LOGICAL_SIZE: u64 = 24 * GIB;
// Schema-3 fixed-container contract, including room for partition metadata.
const MIN_CONTAINER_BYTES: u64 = 5 * GIB;
const MAX_CONTAINER_BYTES: u64 = 64 * GIB;
const PARTITION_HEADROOM_MIB: u64 = 1024;

fn validate_partuuid_path(path: &str) -> Result<&str, String> {
    let partuuid = path
        .strip_prefix("/dev/disk/by-partuuid/")
        .ok_or_else(|| "origin must use /dev/disk/by-partuuid/<uuid>".to_string())?;
    let bytes = partuuid.as_bytes();
    let valid = bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        });
    if !valid {
        return Err("origin PARTUUID must use canonical 8-4-4-4-12 hexadecimal syntax".into());
    }
    Ok(partuuid)
}

pub(super) fn canonical_guid(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let valid = bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        });
    valid.then(|| value.to_ascii_lowercase())
}

fn canonical_sha256(value: &str) -> Option<String> {
    (value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| value.to_ascii_lowercase())
}

pub(super) fn canonical_device_number(value: &str) -> Option<String> {
    let (major, minor) = value.split_once(':')?;
    let major = major.parse::<u64>().ok()?;
    let minor = minor.parse::<u64>().ok()?;
    Some(format!("{major}:{minor}"))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SealedOriginManifest {
    host_manifest_sha256: String,
    configuration_sha256: String,
    pub(super) origin_path: String,
    pub(super) partuuid: String,
    ptuuid: String,
    partition_dev_t: String,
    parent_dev_t: String,
    expected_swap_uuid: String,
    pub(super) logical_capacity_mib: u64,
    pub(super) physical_cache_cap_mib: u64,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct HostOriginManifest {
    schema_version: u32,
    origin_vhdx: String,
    fixed_size_bytes: u64,
    logical_capacity_mib: u64,
    physical_cache_cap_mib: u64,
    chunk_mib: u64,
    gpu_reserve_min_mib: u64,
    gpu_reserve_percent: u64,
    partuuid: String,
    disk_guid: String,
    expected_swap_uuid: String,
    ownership_proof_schema: u32,
    existing_wsl_swap_vhdx: String,
    configuration_sha256: String,
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn windows_absolute_drive_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 4
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
}

fn host_configuration_text(manifest: &HostOriginManifest) -> String {
    format!(
        "schema=3\n\
         origin_vhdx={}\n\
         fixed_size_bytes={}\n\
         logical_capacity_mib={}\n\
         physical_cache_cap_mib={}\n\
         chunk_mib={}\n\
         gpu_reserve_min_mib={}\n\
         gpu_reserve_percent={}\n\
         partuuid={}\n\
         disk_guid={}\n\
         expected_swap_uuid={}\n\
         ownership_proof_schema={}\n\
         existing_wsl_swap_vhdx={}\n",
        manifest.origin_vhdx,
        manifest.fixed_size_bytes,
        manifest.logical_capacity_mib,
        manifest.physical_cache_cap_mib,
        manifest.chunk_mib,
        manifest.gpu_reserve_min_mib,
        manifest.gpu_reserve_percent,
        manifest.partuuid,
        manifest.disk_guid,
        manifest.expected_swap_uuid,
        manifest.ownership_proof_schema,
        manifest.existing_wsl_swap_vhdx,
    )
}

fn validate_host_origin_manifest_bytes(
    sealed: &SealedOriginManifest,
    bytes: &[u8],
) -> Result<(), String> {
    if sha256_hex(bytes) != sealed.host_manifest_sha256 {
        return Err("host origin manifest SHA-256 differs from the sealed guest hash".into());
    }
    let json_bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    let host: HostOriginManifest = serde_json::from_slice(json_bytes)
        .map_err(|error| format!("host origin manifest JSON is invalid: {error}"))?;
    let host_partuuid = canonical_guid(&host.partuuid)
        .ok_or_else(|| "host origin manifest PARTUUID is invalid".to_string())?;
    let host_disk_guid = canonical_guid(&host.disk_guid)
        .ok_or_else(|| "host origin manifest disk GUID is invalid".to_string())?;
    let host_swap_uuid = canonical_guid(&host.expected_swap_uuid)
        .ok_or_else(|| "host origin manifest swap UUID is invalid".to_string())?;
    let host_configuration_sha256 = canonical_sha256(&host.configuration_sha256)
        .ok_or_else(|| "host origin manifest configuration SHA-256 is invalid".to_string())?;
    let required_container_bytes = host
        .logical_capacity_mib
        .checked_add(PARTITION_HEADROOM_MIB)
        .and_then(|mib| mib.checked_mul(MIB))
        .ok_or("host origin capacity and partition headroom overflow bytes")?;
    if host.schema_version != 3
        || host.ownership_proof_schema != 1
        || host.fixed_size_bytes < MIN_CONTAINER_BYTES
        || host.fixed_size_bytes > MAX_CONTAINER_BYTES
        || !host.fixed_size_bytes.is_multiple_of(GIB)
        || host.fixed_size_bytes < required_container_bytes
        || host.chunk_mib != DEFAULT_CHUNK_MIB
        || host.gpu_reserve_min_mib != SEALED_RESERVE_MIN_MIB
        || host.gpu_reserve_percent != SEALED_RESERVE_PERCENT
        || !windows_absolute_drive_path(&host.origin_vhdx)
        || !windows_absolute_drive_path(&host.existing_wsl_swap_vhdx)
        || host_partuuid != sealed.partuuid
        || host_disk_guid != sealed.ptuuid
        || host_swap_uuid != sealed.expected_swap_uuid
        || host.logical_capacity_mib != sealed.logical_capacity_mib
        || host.physical_cache_cap_mib != sealed.physical_cache_cap_mib
    {
        return Err("host and guest origin manifests disagree on sealed policy or identity".into());
    }
    let computed_configuration_sha256 = sha256_hex(host_configuration_text(&host).as_bytes());
    if computed_configuration_sha256 != host_configuration_sha256
        || computed_configuration_sha256 != sealed.configuration_sha256
    {
        return Err("host origin configuration SHA-256 is not enforced end to end".into());
    }
    Ok(())
}

fn read_host_origin_manifest_bytes(path: &str) -> Result<Vec<u8>, String> {
    if path != HOST_ORIGIN_MANIFEST_PATH {
        return Err(format!(
            "host origin manifest must use {HOST_ORIGIN_MANIFEST_PATH}"
        ));
    }
    read_bounded_manifest_file(Path::new(path), false)
}

fn parse_sealed_origin_manifest(text: &str) -> Result<SealedOriginManifest, String> {
    let mut values = std::collections::BTreeMap::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| "origin manifest contains a malformed line".to_string())?;
        if key.trim() != key || value.trim() != value || key.is_empty() || value.is_empty() {
            return Err(
                "origin manifest contains non-canonical whitespace or an empty value".into(),
            );
        }
        if values.insert(key, value).is_some() {
            return Err(format!("origin manifest repeats key {key}"));
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
        return Err("origin manifest schema is incomplete or contains unknown keys".into());
    }
    if values["schema_version"] != "3" || values["swap_type"] != "swap" {
        return Err("origin manifest schema or swap type is invalid".into());
    }
    let partuuid = canonical_guid(values["partuuid"])
        .ok_or_else(|| "origin manifest PARTUUID is invalid".to_string())?;
    let ptuuid = canonical_guid(values["ptuuid"])
        .ok_or_else(|| "origin manifest PTUUID is invalid".to_string())?;
    let expected_swap_uuid = canonical_guid(values["expected_swap_uuid"])
        .ok_or_else(|| "origin manifest swap UUID is invalid".to_string())?;
    let origin_path = values["origin_path"].to_string();
    if validate_partuuid_path(&origin_path)?.to_ascii_lowercase() != partuuid {
        return Err("origin manifest path and PARTUUID disagree".into());
    }
    let logical_capacity_mib = values["logical_capacity_mib"]
        .parse::<u64>()
        .map_err(|_| "origin manifest logical capacity is invalid")?;
    if !(MIN_ORIGIN_LOGICAL_SIZE / MIB..=MAX_ORIGIN_LOGICAL_SIZE / MIB)
        .contains(&logical_capacity_mib)
    {
        return Err("origin manifest capacities are outside the product policy".into());
    }
    let physical_cache_cap_mib = parse_physical_cache_cap(
        Some(values["physical_cache_cap_mib"]),
        logical_capacity_mib.saturating_mul(1024 * 1024),
    )?
    .div_ceil(1024 * 1024);
    Ok(SealedOriginManifest {
        host_manifest_sha256: canonical_sha256(values["host_manifest_sha256"])
            .ok_or_else(|| "origin host manifest SHA-256 is invalid".to_string())?,
        configuration_sha256: canonical_sha256(values["configuration_sha256"])
            .ok_or_else(|| "origin configuration SHA-256 is invalid".to_string())?,
        origin_path,
        partuuid,
        ptuuid,
        partition_dev_t: canonical_device_number(values["partition_dev_t"])
            .ok_or_else(|| "origin partition dev_t is invalid".to_string())?,
        parent_dev_t: canonical_device_number(values["parent_dev_t"])
            .ok_or_else(|| "origin parent dev_t is invalid".to_string())?,
        expected_swap_uuid,
        logical_capacity_mib,
        physical_cache_cap_mib,
    })
}

pub(super) fn read_sealed_origin_manifest(path: &str) -> Result<SealedOriginManifest, String> {
    if path != ORIGIN_MANIFEST_PATH {
        return Err(format!(
            "origin manifest must be the sealed {ORIGIN_MANIFEST_PATH} path"
        ));
    }
    let bytes = read_bounded_manifest_file(Path::new(path), true)?;
    let text = String::from_utf8(bytes)
        .map_err(|_| "origin manifest is not canonical UTF-8".to_string())?;
    let sealed = parse_sealed_origin_manifest(&text)?;
    let host_bytes = read_host_origin_manifest_bytes(HOST_ORIGIN_MANIFEST_PATH)?;
    validate_host_origin_manifest_bytes(&sealed, &host_bytes)?;
    Ok(sealed)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct OriginIdentityObservation {
    pub(super) partuuid: String,
    pub(super) ptuuid: String,
    pub(super) path_dev_t: String,
    pub(super) fd_dev_t: String,
    pub(super) parent_dev_t: String,
    pub(super) swap_uuid: String,
    pub(super) swap_type: String,
    pub(super) origin_size: u64,
    pub(super) critical_dev_ts: Vec<String>,
}

pub(super) fn validate_origin_manifest_identity(
    manifest: &SealedOriginManifest,
    observation: &OriginIdentityObservation,
    logical_size: u64,
) -> Result<(), String> {
    if logical_size != manifest.logical_capacity_mib.saturating_mul(1024 * 1024) {
        return Err("daemon logical capacity differs from the sealed origin manifest".into());
    }
    if observation.origin_size < logical_size {
        return Err(format!(
            "origin is smaller than logical capacity: {} < {logical_size}",
            observation.origin_size
        ));
    }
    if observation.partuuid != manifest.partuuid
        || observation.ptuuid != manifest.ptuuid
        || observation.path_dev_t != manifest.partition_dev_t
        || observation.fd_dev_t != manifest.partition_dev_t
        || observation.parent_dev_t != manifest.parent_dev_t
        || observation.swap_uuid != manifest.expected_swap_uuid
        || observation.swap_type != "swap"
    {
        return Err("opened origin identity differs from the sealed manifest".into());
    }
    if observation
        .critical_dev_ts
        .iter()
        .any(|device| device == &observation.fd_dev_t || device == &observation.parent_dev_t)
    {
        return Err(
            "origin aliases the current root, active swap, or one of their parent devices".into(),
        );
    }
    Ok(())
}

fn parse_physical_cache_cap(value_mib: Option<&str>, logical_bytes: u64) -> Result<u64, String> {
    let value = value_mib.unwrap_or("1024");
    let mib = value
        .parse::<u64>()
        .map_err(|_| "physical cache cap must be an integer MiB value")?;
    let bytes = mib
        .checked_mul(1024 * 1024)
        .ok_or("physical cache cap overflows bytes")?;
    if mib < 1024 || bytes > logical_bytes {
        return Err("physical cache cap must be between 1024 MiB and logical capacity".into());
    }
    Ok(bytes)
}

#[cfg(test)]
impl SealedOriginManifest {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn test_fixture(
        origin_path: String,
        partuuid: String,
        ptuuid: String,
        partition_dev_t: String,
        parent_dev_t: String,
        expected_swap_uuid: String,
        logical_capacity_mib: u64,
        physical_cache_cap_mib: u64,
    ) -> Self {
        Self {
            host_manifest_sha256: "a".repeat(64),
            configuration_sha256: "b".repeat(64),
            origin_path,
            partuuid,
            ptuuid,
            partition_dev_t,
            parent_dev_t,
            expected_swap_uuid,
            logical_capacity_mib,
            physical_cache_cap_mib,
        }
    }
}

#[cfg(test)]
mod tests;
