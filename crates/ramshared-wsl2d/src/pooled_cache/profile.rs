// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/pooled_cache/profile.rs
//! Sealed next-boot cache profile parsing, validation, and atomic staging.
//! SPEC: the protocol specification §DT-1, §DT-11.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;

use ramshared_ipc::cache_wire::{
    CACHE_WIRE_VERSION, CacheGuestRole, DEFAULT_POOLED_CACHE_PORT, validate_cache_data_port,
};
use serde::{Deserialize, Serialize};

pub const CACHE_PROFILE_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_CACHE_PROFILE_PATH: &str = "/etc/ramshared/cache-mode.json";
pub const MAX_CACHE_PROFILE_BYTES: usize = 8 * 1024;
const CONTROL_PORT: u32 = 2761;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheProfile {
    #[default]
    Independent,
    Pooled,
}

impl CacheProfile {
    pub fn parse(raw: &str) -> Result<Self, ProfileError> {
        match raw.trim() {
            "independent" => Ok(Self::Independent),
            "pooled" => Ok(Self::Pooled),
            _ => Err(ProfileError::InvalidMode),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Independent => "independent",
            Self::Pooled => "pooled",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProfileError {
    InvalidMode,
    MalformedDocument(String),
    UnsupportedVersion(u32),
    ActiveSwitchRefused,
    StaleBinding,
    Io(String),
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for ProfileError {}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StagedCacheProfile {
    pub schema_version: u32,
    pub profile: CacheProfile,
    pub role: String,
    pub endpoint_version: u8,
    pub data_port: u32,
    pub key_ref: String,
    pub release_binding: String,
    pub origin_binding: String,
    pub generation: u64,
}

impl StagedCacheProfile {
    pub fn validate(&self) -> Result<CacheGuestRole, ProfileError> {
        if self.schema_version != CACHE_PROFILE_SCHEMA_VERSION {
            return Err(ProfileError::UnsupportedVersion(self.schema_version));
        }
        if self.endpoint_version != CACHE_WIRE_VERSION {
            return Err(ProfileError::MalformedDocument(
                "unsupported endpoint version".into(),
            ));
        }
        let role = match self.role.as_str() {
            "wsl2" => CacheGuestRole::Wsl2,
            "secondary_guest" => CacheGuestRole::SecondaryGuest,
            _ => return Err(ProfileError::MalformedDocument("unknown guest role".into())),
        };
        validate_cache_data_port(self.data_port, CONTROL_PORT).map_err(|_| {
            ProfileError::MalformedDocument("invalid or colliding data port".into())
        })?;
        if self.key_ref.trim().is_empty()
            || self.release_binding.trim().is_empty()
            || self.origin_binding.trim().is_empty()
            || self.generation == 0
        {
            return Err(ProfileError::MalformedDocument(
                "incomplete binding metadata".into(),
            ));
        }
        Ok(role)
    }

    pub fn same_intent(&self, other: &Self) -> bool {
        self.profile == other.profile
            && self.role == other.role
            && self.endpoint_version == other.endpoint_version
            && self.data_port == other.data_port
            && self.key_ref == other.key_ref
            && self.release_binding == other.release_binding
            && self.origin_binding == other.origin_binding
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CacheProfileSelection {
    DefaultIndependent,
    Staged(StagedCacheProfile),
}

impl CacheProfileSelection {
    pub fn profile(&self) -> CacheProfile {
        match self {
            Self::DefaultIndependent => CacheProfile::Independent,
            Self::Staged(doc) => doc.profile,
        }
    }
}

pub fn parse_staged_profile(raw: &str) -> Result<StagedCacheProfile, ProfileError> {
    if raw.len() > MAX_CACHE_PROFILE_BYTES {
        return Err(ProfileError::MalformedDocument(
            "profile document exceeds bound".into(),
        ));
    }
    let doc: StagedCacheProfile =
        serde_json::from_str(raw).map_err(|e| ProfileError::MalformedDocument(e.to_string()))?;
    let _ = doc.validate()?;
    Ok(doc)
}

fn io_err(err: impl std::fmt::Display) -> ProfileError {
    ProfileError::Io(err.to_string())
}

/// Read boot profile from `path`. Absence defaults to `Independent`; malformed refuses.
pub fn load_boot_profile(path: &Path) -> Result<CacheProfileSelection, ProfileError> {
    match fs::read_to_string(path) {
        Ok(raw) => Ok(CacheProfileSelection::Staged(parse_staged_profile(&raw)?)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Ok(CacheProfileSelection::DefaultIndependent)
        }
        Err(err) => Err(io_err(err)),
    }
}

/// Stage next-boot intent idempotently via write -> sync_data -> rename -> dir sync.
/// Refuses active in-place profile switching while an origin is serving.
pub fn stage_next_boot_profile(
    path: &Path,
    mut candidate: StagedCacheProfile,
    apply_immediately_while_active: bool,
) -> Result<StagedCacheProfile, ProfileError> {
    if apply_immediately_while_active {
        return Err(ProfileError::ActiveSwitchRefused);
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| io_err("profile path has no parent"))?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| io_err("profile path has no utf-8 filename"))?;
    if candidate.data_port == 0 {
        candidate.data_port = DEFAULT_POOLED_CACHE_PORT;
    }
    candidate.generation = candidate.generation.max(1);
    let _ = candidate.validate()?;
    let existing = match load_boot_profile(path)? {
        CacheProfileSelection::Staged(doc) => Some(doc),
        CacheProfileSelection::DefaultIndependent => None,
    };
    if let Some(prev) = existing {
        if prev.same_intent(&candidate) {
            return Ok(prev);
        }
        candidate.generation = prev.generation.saturating_add(1);
    }
    commit_profile_atomic(parent, file_name, path, &candidate)?;
    Ok(candidate)
}

fn commit_profile_atomic(
    parent: &Path,
    file_name: &str,
    path: &Path,
    doc: &StagedCacheProfile,
) -> Result<(), ProfileError> {
    let payload = serde_json::to_vec_pretty(doc)
        .map_err(|e| ProfileError::MalformedDocument(e.to_string()))?;
    let tmp_path = parent.join(format!(".{file_name}.tmp.{}", std::process::id()));
    let mut tmp = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp_path)
        .map_err(io_err)?;
    let write_res = tmp
        .write_all(&payload)
        .and_then(|()| tmp.sync_data())
        .and_then(|()| {
            drop(tmp);
            fs::rename(&tmp_path, path)
        })
        .and_then(|()| File::open(parent)?.sync_all());
    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp_path);
        return Err(io_err(err));
    }
    Ok(())
}
