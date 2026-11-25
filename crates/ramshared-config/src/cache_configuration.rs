// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-config/src/cache_configuration.rs
//! Root-issued native cache intent; consumers must verify its file and lifetime.
use crate::native_origin::identity_hash;
use crate::resource_profile::{
    RESOURCE_PROFILE_SCHEMA_VERSION, ResourcePlatform, ResourceProfile, ResourceTarget,
};
use ramshared_vram::{GpuAdapterIdentity, RUNTIME_FREE_BUFFER_BYTES, ReserveFloorPolicy};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const CONFIGURATION_NAME: &str = "cache-configuration.toml";
pub const MAX_CONFIGURATION_BYTES: usize = 64 * 1024;
pub fn configuration_path() -> PathBuf {
    Path::new("/var/lib/ramshared/transactions").join(CONFIGURATION_NAME)
}
pub fn lease_path() -> &'static Path {
    Path::new("/run/ramshared/cache-configuration.lock")
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CacheMemberConfiguration {
    pub adapter: GpuAdapterIdentity,
    pub capacity_bytes: u64,
    pub target_bytes: u64,
    pub reserve_bytes: u64,
    pub runtime_buffer_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CacheConfiguration {
    pub schema_version: u32,
    pub generation: u64,
    pub plan_fingerprint: String,
    pub approval_id: String,
    pub approval_issued_unix: u64,
    pub applied_unix: u64,
    pub profile_fingerprint: String,
    pub release_version: String,
    pub daemon_executable: PathBuf,
    pub daemon_sha256: String,
    pub origin: ResourceTarget,
    pub lower_sink_identity: String,
    pub required_lower_sink_bytes: u64,
    pub members: Vec<CacheMemberConfiguration>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record<T> {
    sha256: String,
    configuration: T,
}
fn record_hash(body: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"ramshared-cache-configuration-v1:");
    digest.update(body.as_bytes());
    digest
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn key(member: &CacheMemberConfiguration) -> (&str, &str) {
    (&member.adapter.backend, &member.adapter.key)
}
impl CacheConfiguration {
    pub fn validate(&self) -> Result<u64, String> {
        if self.schema_version != 1 || self.generation == 0 || self.members.is_empty() {
            return Err("cache configuration needs current schema, generation and members".into());
        }
        for hash in [
            &self.plan_fingerprint,
            &self.profile_fingerprint,
            &self.daemon_sha256,
        ] {
            if hash.len() != 64 || !hash.bytes().all(|c| c.is_ascii_hexdigit()) {
                return Err("cache configuration content identity is invalid".into());
            }
        }
        for identity in [
            &self.approval_id,
            &self.release_version,
            &self.lower_sink_identity,
        ] {
            if identity.is_empty() || identity.chars().any(char::is_control) {
                return Err("cache configuration owner identity is absent or malformed".into());
            }
        }
        if self.approval_issued_unix == 0
            || self.applied_unix < self.approval_issued_unix
            || !self.daemon_executable.to_str().is_some_and(|path| {
                path.starts_with('/')
                    && !path.contains(['\\', '\0'])
                    && path
                        .split('/')
                        .skip(1)
                        .all(|part| !part.is_empty() && !matches!(part, "." | ".."))
            })
            || self.required_lower_sink_bytes == 0
        {
            return Err("cache configuration clock, image or lower sink is invalid".into());
        }
        let ResourceTarget::LinuxFileOrigin {
            allocated_bytes,
            identity_field_hash,
            ..
        } = &self.origin
        else {
            return Err("cache configuration requires an existing native origin".into());
        };
        ResourceProfile {
            schema_version: RESOURCE_PROFILE_SCHEMA_VERSION,
            planned_caps: Default::default(),
            targets: vec![self.origin.clone()],
        }
        .validate_for(ResourcePlatform::NativeLinux)
        .map_err(|e| e.to_string())?;
        if identity_hash(&self.origin)? != *identity_field_hash {
            return Err("cache configuration origin identity differs".into());
        }
        let mut total = 0u64;
        for (index, member) in self.members.iter().enumerate() {
            let adapter = &member.adapter;
            if !adapter.is_stable_driver_identity()
                || adapter.key.to_ascii_lowercase() != adapter.key
                || adapter
                    .luid
                    .as_ref()
                    .is_some_and(|id| id.to_ascii_lowercase() != *id)
                || member.capacity_bytes == 0
                || member.target_bytes == 0
                || member.reserve_bytes
                    != ReserveFloorPolicy::sealed().configured_reserve_bytes(member.capacity_bytes)
                || member.runtime_buffer_bytes != RUNTIME_FREE_BUFFER_BYTES
                || member
                    .target_bytes
                    .checked_add(member.reserve_bytes)
                    .and_then(|n| n.checked_add(member.runtime_buffer_bytes))
                    .is_none_or(|n| n > member.capacity_bytes)
            {
                return Err(
                    "cache member identity, target or shared reserve policy is invalid".into(),
                );
            }
            if index > 0 && key(&self.members[index - 1]) >= key(member)
                || self.members[..index]
                    .iter()
                    .any(|old| !adapter.proves_independent_memory(&old.adapter))
            {
                return Err("cache configuration members are aliases or not canonical".into());
            }
            total = total
                .checked_add(member.target_bytes)
                .filter(|n| *n <= *allocated_bytes)
                .ok_or("cache targets exceed authoritative origin geometry")?;
        }
        Ok(total)
    }
    pub fn encode(&self) -> Result<String, String> {
        self.validate()?;
        let body = toml::to_string(self).map_err(|e| e.to_string())?;
        let text = toml::to_string(&Record {
            sha256: record_hash(&body),
            configuration: self,
        })
        .map_err(|e| e.to_string())?;
        if text.len() > MAX_CONFIGURATION_BYTES {
            return Err("cache configuration exceeds metadata budget".into());
        }
        Ok(text)
    }
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.is_empty() || text.len() > MAX_CONFIGURATION_BYTES {
            return Err("cache configuration exceeds metadata budget".into());
        }
        let record: Record<Self> = toml::from_str(text).map_err(|e| e.to_string())?;
        let value = record.configuration;
        value.validate()?;
        let canonical = toml::to_string(&value).map_err(|e| e.to_string())?;
        if record.sha256 != record_hash(&canonical) {
            return Err("cache configuration record is incomplete or its checksum differs".into());
        }
        Ok(value)
    }
    pub fn next_generation(
        previous: Option<&Self>,
        approval_id: &str,
        issued_unix: u64,
    ) -> Result<u64, String> {
        if approval_id.is_empty() || approval_id.chars().any(char::is_control) || issued_unix == 0 {
            return Err("cache generation requires a valid approval identity and clock".into());
        }
        if let Some(old) = previous {
            old.validate()?;
        }
        match previous {
            None => Ok(1),
            Some(old)
                if approval_id != old.approval_id && issued_unix > old.approval_issued_unix =>
            {
                old.generation
                    .checked_add(1)
                    .ok_or_else(|| "cache generation exhausted".into())
            }
            _ => Err("cache configuration requires a new, later approval".into()),
        }
    }
}

#[cfg(test)]
mod tests;
