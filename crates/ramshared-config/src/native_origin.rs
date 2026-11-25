// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-config/src/native_origin.rs
//! Durable identity of a prepared native file origin; never a serving grant.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::resource_profile::{
    MAX_RESOURCE_PROFILE_BYTES, PlannedTierCaps, RESOURCE_PROFILE_SCHEMA_VERSION, ResourcePlatform,
    ResourceProfile, ResourceTarget,
};

pub const NATIVE_ORIGIN_MANIFEST_NAME: &str = "native-origin.toml";
/// The native receipt shares the canonical resource-profile directory.
pub fn default_native_origin_path() -> std::path::PathBuf {
    std::path::Path::new(crate::resource_profile::DEFAULT_RESOURCE_PROFILE_PATH)
        .with_file_name(NATIVE_ORIGIN_MANIFEST_NAME)
}

const MANIFEST_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeOriginManifest {
    pub schema_version: u32,
    pub plan_fingerprint: String,
    pub approval_id: String,
    pub profile_fingerprint: String,
    pub release_version: String,
    pub target: ResourceTarget,
}

/// Hash identity fields only. The framing/domain preserve the existing native
/// observer's v1 bytes; mutable file contents are deliberately excluded.
pub fn identity_hash(target: &ResourceTarget) -> Result<String, String> {
    let ResourceTarget::LinuxFileOrigin {
        filesystem_uuid,
        device_identity,
        managed_relative_path,
        inode,
        allocated_bytes,
        ..
    } = target
    else {
        return Err("an existing native file-origin identity is required".into());
    };
    let mut digest = Sha256::new();
    digest.update(b"ramshared-native-file-origin-v1:");
    for value in [
        filesystem_uuid.clone(),
        device_identity.clone(),
        managed_relative_path.clone(),
        inode.to_string(),
        allocated_bytes.to_string(),
    ] {
        digest.update(value.len().to_string().as_bytes());
        digest.update(b":");
        digest.update(value.as_bytes());
    }
    Ok(digest
        .finalize()
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect())
}

/// Native origin roles use one confined relative namespace, independently of
/// the current mount pathname. This is validation, never path traversal.
pub fn validate_managed_path(path: &str) -> Result<(), String> {
    if !path.starts_with("origin/")
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || path.contains(['\\', '\0'])
    {
        return Err(
            "native origin path must be confined beneath the managed origin directory".into(),
        );
    }
    Ok(())
}

impl NativeOriginManifest {
    /// Compare creation geometry only. Callers validate the manifest and
    /// observe the current descriptor separately; this is not ownership proof.
    pub fn matches_request(&self, request: &ResourceTarget) -> bool {
        match (request, &self.target) {
            (
                ResourceTarget::LinuxFileOriginRequest {
                    filesystem_uuid: a,
                    device_identity: b,
                    managed_relative_path: c,
                    allocated_bytes: d,
                },
                ResourceTarget::LinuxFileOrigin {
                    filesystem_uuid: e,
                    device_identity: f,
                    managed_relative_path: g,
                    allocated_bytes: h,
                    ..
                },
            ) => a == e && b == f && c == g && d == h,
            _ => false,
        }
    }

    pub fn new(
        plan_fingerprint: String,
        approval_id: String,
        profile_fingerprint: String,
        release_version: String,
        target: ResourceTarget,
    ) -> Result<Self, String> {
        let manifest = Self {
            schema_version: MANIFEST_SCHEMA_VERSION,
            plan_fingerprint,
            approval_id,
            profile_fingerprint,
            release_version,
            target,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != MANIFEST_SCHEMA_VERSION {
            return Err("unsupported native-origin manifest schema".into());
        }
        for hash in [&self.plan_fingerprint, &self.profile_fingerprint] {
            if hash.len() != 64 || !hash.bytes().all(|value| value.is_ascii_hexdigit()) {
                return Err("native-origin plan/profile hash is invalid".into());
            }
        }
        if self.approval_id.is_empty() || self.release_version.is_empty() {
            return Err("native-origin owner/release identity is absent".into());
        }
        let ResourceTarget::LinuxFileOrigin {
            identity_field_hash,
            managed_relative_path,
            ..
        } = &self.target
        else {
            return Err("native-origin manifest needs a sealed native file target".into());
        };
        validate_managed_path(managed_relative_path)?;
        ResourceProfile {
            schema_version: RESOURCE_PROFILE_SCHEMA_VERSION,
            planned_caps: PlannedTierCaps::default(),
            targets: vec![self.target.clone()],
        }
        .validate_for(ResourcePlatform::NativeLinux)
        .map_err(|error| error.to_string())?;
        if identity_hash(&self.target)? != *identity_field_hash {
            return Err("native-origin identity-field hash differs from its target".into());
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<String, String> {
        self.validate()?;
        let body = toml::to_string(self).map_err(|error| error.to_string())?;
        if body.len() > MAX_RESOURCE_PROFILE_BYTES {
            return Err("native-origin manifest exceeds the shared size limit".into());
        }
        Ok(body)
    }

    pub fn parse(body: &str) -> Result<Self, String> {
        if body.len() > MAX_RESOURCE_PROFILE_BYTES {
            return Err("native-origin manifest exceeds the shared size limit".into());
        }
        let manifest: Self = toml::from_str(body).map_err(|error| error.to_string())?;
        manifest.validate()?;
        Ok(manifest)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn native_manifest_request_correspondence_is_exact() {
        let target = ResourceTarget::LinuxFileOrigin {
            filesystem_uuid: "filesystem".into(),
            device_identity: "device".into(),
            managed_relative_path: "origin/data.img".into(),
            inode: 42,
            allocated_bytes: 8192,
            identity_field_hash: String::new(),
        };
        let mut sealed = target;
        let hash = identity_hash(&sealed).unwrap();
        if let ResourceTarget::LinuxFileOrigin {
            identity_field_hash,
            ..
        } = &mut sealed
        {
            *identity_field_hash = hash;
        }
        let manifest = NativeOriginManifest::new(
            "a".repeat(64),
            "approval".into(),
            "b".repeat(64),
            "creator-release".into(),
            sealed,
        )
        .unwrap();
        let request = ResourceTarget::LinuxFileOriginRequest {
            filesystem_uuid: "filesystem".into(),
            device_identity: "device".into(),
            managed_relative_path: "origin/data.img".into(),
            allocated_bytes: 8192,
        };
        assert!(manifest.matches_request(&request));
        for index in 0..4 {
            let mut changed = request.clone();
            if let ResourceTarget::LinuxFileOriginRequest {
                filesystem_uuid,
                device_identity,
                managed_relative_path,
                allocated_bytes,
            } = &mut changed
            {
                match index {
                    0 => filesystem_uuid.push_str("-changed"),
                    1 => device_identity.push_str("-changed"),
                    2 => managed_relative_path.push_str("-changed"),
                    _ => *allocated_bytes += 1,
                }
            }
            assert!(!manifest.matches_request(&changed));
        }
        assert!(!manifest.matches_request(&manifest.target));
    }

    fn manifest() -> NativeOriginManifest {
        let mut target = ResourceTarget::LinuxFileOrigin {
            filesystem_uuid: "filesystem".into(),
            device_identity: "device".into(),
            managed_relative_path: "origin/test.img".into(),
            inode: 9,
            allocated_bytes: 4096,
            identity_field_hash: String::new(),
        };
        let hash = identity_hash(&target).unwrap();
        if let ResourceTarget::LinuxFileOrigin {
            identity_field_hash,
            ..
        } = &mut target
        {
            *identity_field_hash = hash;
        }
        NativeOriginManifest::new(
            "a".repeat(64),
            "approval".into(),
            "b".repeat(64),
            "v0.15.0".into(),
            target,
        )
        .unwrap()
    }

    #[test]
    fn native_manifest_roundtrip_and_identity_refusals() {
        let value = manifest();
        let body = value.encode().unwrap();
        assert_eq!(NativeOriginManifest::parse(&body).unwrap(), value);
        for mutate in 0..7 {
            let mut invalid = value.clone();
            match mutate {
                0 => invalid.schema_version += 1,
                1 => invalid.plan_fingerprint.clear(),
                2 => invalid.profile_fingerprint = "x".repeat(64),
                3 => invalid.approval_id.clear(),
                4 => invalid.release_version.clear(),
                5 => {
                    if let ResourceTarget::LinuxFileOrigin { inode, .. } = &mut invalid.target {
                        *inode += 1;
                    }
                }
                _ => {
                    invalid.target = ResourceTarget::LinuxFileOriginRequest {
                        filesystem_uuid: "fs".into(),
                        device_identity: "dev".into(),
                        managed_relative_path: "origin/request.img".into(),
                        allocated_bytes: 4096,
                    }
                }
            }
            assert!(invalid.encode().is_err());
            assert!(NativeOriginManifest::parse(&toml::to_string(&invalid).unwrap()).is_err());
        }
        assert!(NativeOriginManifest::parse(&("extra = true\n".to_string() + &body)).is_err());
        assert!(NativeOriginManifest::parse(&"x".repeat(MAX_RESOURCE_PROFILE_BYTES + 1)).is_err());
    }
    #[test]
    fn native_manifest_rejects_invalid_namespace_and_unbounded_encoding() {
        for path in [
            "origin",
            "origin/../foreign",
            "origin//file",
            "origin/./file",
            "swap/file",
            "origin/a\\b",
            "origin/a\0b",
        ] {
            assert!(validate_managed_path(path).is_err());
        }
        let mut value = manifest();
        value.approval_id = "x".repeat(MAX_RESOURCE_PROFILE_BYTES);
        assert!(value.encode().is_err());
        let mut value = manifest();
        if let ResourceTarget::LinuxFileOrigin {
            managed_relative_path,
            identity_field_hash,
            ..
        } = &mut value.target
        {
            *managed_relative_path = "swap/foreign".into();
            identity_field_hash.clear();
        }
        assert!(value.validate().is_err());
    }
}
