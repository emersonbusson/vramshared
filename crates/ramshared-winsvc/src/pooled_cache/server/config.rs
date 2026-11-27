// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/pooled_cache/server/config.rs
//! Configuration parsing, key decoding, and atomic status snapshots for the pooled cache server.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use ramshared_ipc::cache_wire::{
    CACHE_PAGE_BYTES, CacheGuestRole, DEFAULT_POOLED_CACHE_PORT, origin_fingerprint_from_binding,
    validate_cache_data_port,
};
use serde::{Deserialize, Serialize};

use super::super::allocation::MIN_HOST_RAM_RESERVE_BYTES;
use super::super::{EnrolledRole, RoleCounters};
use crate::vm_id_resolve::parse_guid;
use crate::vsock_lease::{KeyProtection, read_key_file_with_protection};

pub const DEFAULT_POOLED_CONFIG_PATH: &str =
    r"C:\ProgramData\RamShared\control-plane\pooled-cache.toml";
pub const DEFAULT_POOLED_STATUS_PATH: &str =
    r"C:\ProgramData\RamShared\control-plane\pooled-cache-status.json";
pub const MAX_POOLED_SLOTS: usize = 1_048_576;
pub const MAX_POOLED_CONFIG_BYTES: usize = 64 * 1024;
const DEFAULT_CONTROL_PORT: u32 = 2761;
const DEFAULT_SLOT_COUNT: usize = 65_536;

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleEndpointConfig {
    /// Canonical Hyper-V VM GUID, or `"auto"` (allowed for `wsl` only).
    pub vm_id: String,
    pub key_file: PathBuf,
    #[serde(default)]
    pub key_protection: KeyProtection,
    pub origin_binding: String,
    pub extent_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PooledServerConfig {
    #[serde(default = "default_data_port")]
    pub data_port: u32,
    #[serde(default = "default_control_port")]
    pub control_port: u32,
    #[serde(default)]
    pub cuda_ordinal: i32,
    #[serde(default = "default_slot_count")]
    pub slot_count: usize,
    #[serde(default)]
    pub status_file: Option<PathBuf>,
    /// Optional explicit host RAM reserve floor (must be >= 4 GiB; default is max(4 GiB, 25% RAM)).
    #[serde(default)]
    pub host_ram_reserve_bytes: Option<u64>,
    #[serde(default)]
    pub wsl: Option<RoleEndpointConfig>,
    #[serde(default)]
    pub secondary: Option<RoleEndpointConfig>,
}

const fn default_data_port() -> u32 {
    DEFAULT_POOLED_CACHE_PORT
}
const fn default_control_port() -> u32 {
    DEFAULT_CONTROL_PORT
}
const fn default_slot_count() -> usize {
    DEFAULT_SLOT_COUNT
}

impl PooledServerConfig {
    pub fn from_toml(text: &str) -> Result<Self, String> {
        if text.len() > MAX_POOLED_CONFIG_BYTES {
            return Err("pooled cache config exceeds maximum size".into());
        }
        let cfg: Self =
            toml::from_str(text).map_err(|e| format!("invalid pooled cache config: {e}"))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        Self::from_toml(&text)
    }

    pub fn validate(&self) -> Result<(), String> {
        validate_cache_data_port(self.data_port, self.control_port)
            .map_err(|e| format!("invalid pooled data port: {e}"))?;
        if self.slot_count == 0 || self.slot_count > MAX_POOLED_SLOTS {
            return Err(format!(
                "slot_count {} outside 1..={MAX_POOLED_SLOTS}",
                self.slot_count
            ));
        }
        if self.cuda_ordinal < 0 {
            return Err("cuda_ordinal must be non-negative".into());
        }
        if let Some(floor) = self.host_ram_reserve_bytes
            && floor < MIN_HOST_RAM_RESERVE_BYTES
        {
            return Err(format!(
                "host_ram_reserve_bytes ({floor}) below hard floor ({MIN_HOST_RAM_RESERVE_BYTES})"
            ));
        }
        if self.wsl.is_none() && self.secondary.is_none() {
            return Err("at least one guest role must be enrolled".into());
        }
        let mut wsl_guid = None;
        if let Some(wsl) = &self.wsl {
            validate_role_config(wsl, true)?;
            if !wsl.vm_id.trim().eq_ignore_ascii_case("auto") {
                wsl_guid = Some(parse_guid(&wsl.vm_id).map_err(|e| e.to_string())?);
            }
        }
        if let Some(sec) = &self.secondary {
            validate_role_config(sec, false)?;
            let sec_guid = parse_guid(&sec.vm_id).map_err(|e| e.to_string())?;
            if wsl_guid == Some(sec_guid) {
                return Err("wsl and secondary roles must have distinct VM GUIDs".into());
            }
        }
        Ok(())
    }

    pub fn wsl_uses_auto_vm_id(&self) -> bool {
        self.wsl
            .as_ref()
            .is_some_and(|w| w.vm_id.trim().eq_ignore_ascii_case("auto"))
    }

    /// Resolve enrolled roles and load their 256-bit keys from protected storage.
    pub fn build_enrolled_roles(&self) -> Result<Vec<EnrolledRole>, String> {
        self.validate()?;
        let mut out = Vec::with_capacity(2);
        if let Some(wsl) = &self.wsl {
            out.push(resolve_role(
                CacheGuestRole::Wsl2,
                wsl,
                self.data_port,
                self.control_port,
            )?);
        }
        if let Some(sec) = &self.secondary {
            out.push(resolve_role(
                CacheGuestRole::SecondaryGuest,
                sec,
                self.data_port,
                self.control_port,
            )?);
        }
        Ok(out)
    }
}

fn validate_role_config(role: &RoleEndpointConfig, allow_auto_vm: bool) -> Result<(), String> {
    if !role.key_file.is_absolute() {
        return Err("role key_file must be an absolute path".into());
    }
    if role.extent_bytes == 0 || !role.extent_bytes.is_multiple_of(CACHE_PAGE_BYTES as u64) {
        return Err("role extent_bytes must be a positive multiple of 4096".into());
    }
    origin_fingerprint_from_binding(&role.origin_binding)
        .map_err(|e| format!("invalid origin_binding: {e}"))?;
    let trimmed = role.vm_id.trim();
    if !(allow_auto_vm && trimmed.eq_ignore_ascii_case("auto")) {
        parse_guid(trimmed).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn resolve_role(
    role: CacheGuestRole,
    cfg: &RoleEndpointConfig,
    data_port: u32,
    control_port: u32,
) -> Result<EnrolledRole, String> {
    let raw = read_key_file_with_protection(&cfg.key_file, cfg.key_protection)
        .map_err(|e| e.to_string())?;
    let key = parse_key_bytes(&raw)?;
    let expected_runtime_id = if cfg.vm_id.trim().eq_ignore_ascii_case("auto") {
        // Placeholder until first authenticated WSL2 peer binds a distinct runtime ID.
        [0xFF; 16]
    } else {
        parse_guid(&cfg.vm_id).map_err(|e| e.to_string())?
    };
    let expected_origin_fingerprint = origin_fingerprint_from_binding(&cfg.origin_binding)
        .map_err(|e| format!("origin fingerprint failed: {e}"))?;
    Ok(EnrolledRole {
        role,
        key,
        expected_runtime_id,
        expected_origin_fingerprint,
        extent_bytes: cfg.extent_bytes,
        data_port,
        control_port,
    })
}

/// Parse a 256-bit secret from either 32 raw bytes or 64 trimmed ASCII hex characters.
pub fn parse_key_bytes(raw: &[u8]) -> Result<[u8; 32], String> {
    let key = if raw.len() == 32 {
        let mut out = [0u8; 32];
        out.copy_from_slice(raw);
        out
    } else {
        let text = std::str::from_utf8(raw)
            .map_err(|_| "pooled key must be 32 raw bytes or 64 ASCII hex chars".to_string())?
            .trim();
        if text.len() != 64 {
            return Err(format!(
                "pooled key length {} is neither 32 raw bytes nor 64 hex chars",
                raw.len()
            ));
        }
        let mut out = [0u8; 32];
        for (idx, chunk) in text.as_bytes().as_chunks::<2>().0.iter().enumerate() {
            let pair = std::str::from_utf8(chunk).map_err(|_| "invalid hex utf8".to_string())?;
            out[idx] = u8::from_str_radix(pair, 16)
                .map_err(|_| "invalid hex digit in pooled key".to_string())?;
        }
        out
    };
    if key == [0u8; 32] {
        return Err("all-zero pooled key is refused".into());
    }
    Ok(key)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub struct RoleStatusCounters {
    pub hits: u64,
    pub misses: u64,
    pub busy: u64,
    pub accepted_offers: u64,
    pub dropped_offers: u64,
    pub auth_refusals: u64,
    pub frame_refusals: u64,
    pub epoch_retirements: u64,
    pub gpu_hit_bytes: u64,
}

impl From<RoleCounters> for RoleStatusCounters {
    fn from(c: RoleCounters) -> Self {
        Self {
            hits: c.hits,
            misses: c.misses,
            busy: c.busy,
            accepted_offers: c.accepted_offers,
            dropped_offers: c.dropped_offers,
            auth_refusals: c.auth_refusals,
            frame_refusals: c.frame_refusals,
            epoch_retirements: c.epoch_retirements,
            gpu_hit_bytes: c.gpu_hit_bytes,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct PooledStatusSnapshot {
    pub schema_version: u32,
    pub written_at_unix_ms: u64,
    pub adapter_id: String,
    pub slot_capacity: usize,
    pub slab_bytes: u64,
    pub placement_permit_held: bool,
    pub worker_quarantined: bool,
    pub wsl_active_epoch: Option<u64>,
    pub secondary_active_epoch: Option<u64>,
    pub wsl_charged_bytes: u64,
    pub secondary_charged_bytes: u64,
    pub in_flight_bytes: u64,
    pub quarantined_bytes: u64,
    pub host_physical_ram_bytes: u64,
    pub host_available_ram_bytes: u64,
    pub cuda_free_bytes: u64,
    pub wsl: RoleStatusCounters,
    pub secondary: RoleStatusCounters,
}

/// Write status JSON atomically via unique sibling staging file + rename.
pub fn write_status_atomic(path: &Path, snap: &PooledStatusSnapshot) -> Result<(), String> {
    static NEXT_SEQ: AtomicUsize = AtomicUsize::new(0);
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| "status path has no parent".to_string())?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let payload = serde_json::to_vec_pretty(snap).map_err(|e| e.to_string())?;
    let seq = NEXT_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".pooled-status.{}.{seq}.tmp", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|e| e.to_string())?;
    if let Err(e) = file.write_all(&payload).and_then(|()| file.sync_data()) {
        drop(file);
        let _ = std::fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    drop(file);
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    Ok(())
}
