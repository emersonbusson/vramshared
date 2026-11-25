// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-vram/src/adapter_identity.rs
//! Stable driver identity and proven profile correspondence, independent of capacity.
use serde::{Deserialize, Serialize};

/// Identity of the adapter used by one concrete provider instance.
/// `key` must be stable for the physical adapter when the backend exposes such an identifier.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GpuAdapterIdentity {
    pub backend: String,
    pub key: String,
    /// Windows adapter LUID, normalized as `high:low`, when exposed by the backend.
    pub luid: Option<String>,
    /// Native Windows linked-adapter node mask; absent when unreported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_mask: Option<u32>,
}

impl GpuAdapterIdentity {
    /// Whether two API providers refer to the same physical adapter.
    ///
    /// Keys are backend-specific; cross-API correlation requires a shared LUID and observed physical node.
    pub fn matches_physical_adapter(&self, other: &Self) -> bool {
        if self.backend == other.backend {
            self.key == other.key && self.node_mask == other.node_mask
        } else {
            matches!((&self.luid, &other.luid), (Some(left), Some(right)) if left == right)
                && self.wddm_physical_adapter_index().is_some()
                && self.wddm_physical_adapter_index() == other.wddm_physical_adapter_index()
        }
    }
}

/// Formats the Windows LUID byte layout used by CUDA and Vulkan as `high:low`.
/// Zero is reserved as “not available” by these provider APIs.
pub fn format_luid(bytes: [u8; 8]) -> Option<String> {
    let low = u32::from_le_bytes(bytes[..4].try_into().ok()?);
    let high = u32::from_le_bytes(bytes[4..].try_into().ok()?);
    (low != 0 || high != 0).then(|| format!("{high:08x}:{low:08x}"))
}

fn canonical_luid(luid: &str) -> bool {
    luid.len() == 17
        && luid.as_bytes()[8] == b':'
        && luid
            .bytes()
            .enumerate()
            .all(|(i, byte)| i == 8 || byte.is_ascii_hexdigit())
        && luid.bytes().any(|byte| byte != b'0' && byte != b':')
}

impl GpuAdapterIdentity {
    /// Physical index in the linked Windows adapter, from a single observed bit.
    /// Missing or invalid scope is not physical node zero.
    pub fn wddm_physical_adapter_index(&self) -> Option<u32> {
        self.luid.as_deref().filter(|luid| canonical_luid(luid))?;
        self.node_mask
            .filter(|mask| mask.is_power_of_two())
            .map(u32::trailing_zeros)
    }

    /// True only when stable observations prove independent memory scopes.
    /// Different APIs require distinct known Windows LUIDs; UUID shape alone
    /// cannot prove that they describe different physical devices.
    pub fn proves_independent_memory(&self, other: &Self) -> bool {
        if !self.is_stable_driver_identity() || !other.is_stable_driver_identity() {
            return false;
        }
        if self.backend == other.backend && self.key.eq_ignore_ascii_case(&other.key) {
            return false;
        }
        match (&self.luid, &other.luid) {
            (Some(a), Some(b)) => !a.eq_ignore_ascii_case(b),
            // Only distinct UUIDs share a unique namespace within one API.
            // A UUID and a LUID-only fallback might name the same device.
            _ => self.backend == other.backend && self.key.len() == 32 && other.key.len() == 32,
        }
    }

    /// A stable UUID or explicit Windows LUID from a supported driver backend.
    /// Shape is not proof that an adapter exists or has allocation authority.
    pub fn is_stable_driver_identity(&self) -> bool {
        if !matches!(self.backend.as_str(), "cuda" | "vulkan")
            || self
                .luid
                .as_deref()
                .is_some_and(|luid| !canonical_luid(luid))
            || self
                .node_mask
                .is_some_and(|mask| !mask.is_power_of_two() || self.luid.is_none())
        {
            return false;
        }
        let uuid = self.key.len() == 32
            && self.key.bytes().all(|byte| byte.is_ascii_hexdigit())
            && self.key.bytes().any(|byte| byte != b'0');
        let luid = self.luid.as_deref().is_some_and(|luid| {
            self.key.strip_prefix("luid:") == Some(luid) && canonical_luid(luid)
        });
        uuid || luid
    }

    /// Complete CUDA/NVML driver UUID. LUID-only and other APIs need separate
    /// correspondence proof and cannot use this namespace.
    pub fn nvidia_uuid(&self) -> Option<String> {
        if self.backend != "cuda" || self.key.len() != 32 || !self.is_stable_driver_identity() {
            return None;
        }
        let key = self.key.to_ascii_lowercase();
        Some(format!(
            "GPU-{}-{}-{}-{}-{}",
            &key[..8],
            &key[8..12],
            &key[12..16],
            &key[16..20],
            &key[20..]
        ))
    }

    /// CUDA and NVIDIA-SMI expose the same driver UUID in two formats.
    /// Cross-API UUID correspondence is not inferred from equal-looking bytes.
    pub fn resource_profile_key(&self) -> Option<String> {
        Some(format!(
            "nvidia-uuid:{}",
            self.nvidia_uuid()?.to_ascii_lowercase()
        ))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    fn adapter() -> GpuAdapterIdentity {
        GpuAdapterIdentity {
            node_mask: None,
            backend: "cuda".into(),
            key: "00112233445566778899AABBCCDDEEFF".into(),
            luid: None,
        }
    }
    #[test]
    fn stable_adapter_identity_rejects_ambiguous_keys() {
        assert!(adapter().is_stable_driver_identity());
        let mut sample = adapter();
        sample.backend = "vulkan".into();
        assert!(sample.is_stable_driver_identity());
        sample.luid = Some("12345678:00000001".into());
        sample.key = "luid:12345678:00000001".into();
        assert!(sample.is_stable_driver_identity());
        assert!(sample.resource_profile_key().is_none());
        for key in [
            "",
            "0",
            "00000000000000000000000000000000",
            "00112233445566778899aabbccddeefg",
            "luid:00000000:00000000",
            "luid:12345678:00000002",
            "éééééééééééééééé",
        ] {
            let mut bad = sample.clone();
            bad.key = key.into();
            assert!(!bad.is_stable_driver_identity());
        }
        for luid in [
            "",
            "00000000:00000000",
            "12345678-00000001",
            "gggggggg:00000001",
        ] {
            let mut bad = adapter();
            bad.luid = Some(luid.into());
            assert!(!bad.is_stable_driver_identity());
        }
        let mut bad = adapter();
        bad.backend = "unknown".into();
        assert!(!bad.is_stable_driver_identity());
    }
    #[test]
    fn nvidia_uuid_requires_full_cuda_identity_and_keeps_profile_format() {
        assert_eq!(
            adapter().nvidia_uuid().unwrap(),
            "GPU-00112233-4455-6677-8899-aabbccddeeff"
        );
        for key in [
            "",
            "short",
            "00000000000000000000000000000000",
            "00112233445566778899aabbccddeefg",
            "luid:12345678:00000001",
        ] {
            let mut bad = adapter();
            bad.key = key.into();
            assert!(bad.nvidia_uuid().is_none());
        }
        let mut other = adapter();
        other.backend = "vulkan".into();
        assert!(other.nvidia_uuid().is_none());
    }

    #[test]
    fn adapter_profile_key_preserves_backend_correspondence() {
        assert_eq!(
            adapter().resource_profile_key().unwrap(),
            "nvidia-uuid:gpu-00112233-4455-6677-8899-aabbccddeeff"
        );
        let mut bad = adapter();
        bad.backend = "vulkan".into();
        assert!(bad.resource_profile_key().is_none());
        bad = adapter();
        bad.key = "00000000000000000000000000000000".into();
        assert!(bad.resource_profile_key().is_none());
        bad = adapter();
        bad.luid = Some("bad".into());
        assert!(bad.resource_profile_key().is_none());
    }
}
