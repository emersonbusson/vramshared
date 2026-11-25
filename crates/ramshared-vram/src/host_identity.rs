// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-vram/src/host_identity.rs
//! Bounded host GPU metadata model; serialized observations are not allocation authority.
use crate::{GPU_BUDGET_MAX_AGE_MS, GpuAdapterIdentity};
use serde::{Deserialize, Serialize};

pub const MAX_IDENTITY_ADAPTERS: usize = 64;
pub const MAX_IDENTITY_REPORT_BYTES: usize = 64 * 1024;
pub const INTERFACE_UTF16_CAPACITY: usize = 260;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProducerProcess {
    pub pid: u32,
    pub creation_time_100ns: u64,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostGpuIdentity {
    pub ordinal: u32,
    pub allocator: GpuAdapterIdentity,
    pub interface_id: String,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostGpuIdentityReport {
    pub schema_version: u8,
    pub producer: ProducerProcess,
    pub started_at_unix_ms: u64,
    pub elapsed_ms: u64,
    pub adapters: Vec<HostGpuIdentity>,
}

/// Read-only native effects. Driver calls belong to an isolated producer process.
/// Injected observations and serialized reports cannot mint allocation authority.
pub trait IdentitySource {
    fn device_count(&mut self) -> Result<i32, String>;
    fn adapter_identity(&mut self, ordinal: u32) -> Result<GpuAdapterIdentity, String>;
    fn adapter_interface(&mut self, identity: &GpuAdapterIdentity) -> Result<String, String>;
}

fn validate_allocator(identity: &GpuAdapterIdentity) -> Result<(), String> {
    if identity.backend != "cuda"
        || !identity.is_stable_driver_identity()
        || identity.nvidia_uuid().is_none()
        || identity.wddm_physical_adapter_index().is_none()
        || identity.key != identity.key.to_ascii_lowercase()
        || identity
            .luid
            .as_deref()
            .is_none_or(|luid| luid != luid.to_ascii_lowercase())
    {
        return Err(
            "GPU identity requires a canonical CUDA UUID, host LUID and single node".into(),
        );
    }
    Ok(())
}

pub fn validate_interface_id(interface_id: &str) -> Result<(), String> {
    if interface_id.is_empty()
        || interface_id.encode_utf16().count() >= INTERFACE_UTF16_CAPACITY
        || interface_id.chars().any(char::is_control)
    {
        return Err("GPU interface identity is empty, malformed or exceeds the SDK bound".into());
    }
    Ok(())
}

pub fn decode_interface_id_utf16(raw: &[u16; INTERFACE_UTF16_CAPACITY]) -> Result<String, String> {
    let nul = raw
        .iter()
        .position(|&unit| unit == 0)
        .ok_or_else(|| "vGPU interface identifier lacks a NUL terminator".to_string())?;
    let decoded = String::from_utf16(&raw[..nul])
        .map_err(|_| "vGPU interface identifier is invalid UTF-16".to_string())?;
    validate_interface_id(&decoded)?;
    Ok(decoded)
}

fn validate_row(row: &HostGpuIdentity, previous: &[HostGpuIdentity]) -> Result<(), String> {
    validate_allocator(&row.allocator)?;
    validate_interface_id(&row.interface_id)?;
    if previous.iter().any(|old| {
        old.allocator.key == row.allocator.key
            || old.allocator.luid == row.allocator.luid
            || old.interface_id.eq_ignore_ascii_case(&row.interface_id)
    }) {
        return Err(
            "GPU identity is duplicate or partition/linked-adapter correspondence is ambiguous"
                .into(),
        );
    }
    Ok(())
}

pub fn collect_identities(
    source: &mut impl IdentitySource,
) -> Result<Vec<HostGpuIdentity>, String> {
    let count = usize::try_from(source.device_count()?).map_err(|_| "negative native GPU count")?;
    if count > MAX_IDENTITY_ADAPTERS {
        return Err("native GPU count exceeds the bounded inventory".into());
    }
    let mut rows = Vec::with_capacity(count);
    for ordinal in 0..count {
        let allocator = source.adapter_identity(ordinal as u32)?;
        validate_allocator(&allocator)?;
        let interface_id = source.adapter_interface(&allocator)?;
        let row = HostGpuIdentity {
            ordinal: ordinal as u32,
            allocator,
            interface_id,
        };
        validate_row(&row, &rows)?;
        rows.push(row);
    }
    Ok(rows)
}

impl HostGpuIdentityReport {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1
            || self.producer.pid == 0
            || self.producer.creation_time_100ns == 0
            || self.started_at_unix_ms == 0
            || self.elapsed_ms >= GPU_BUDGET_MAX_AGE_MS
            || self.adapters.len() > MAX_IDENTITY_ADAPTERS
        {
            return Err("GPU identity report has invalid version, process, time or count".into());
        }
        for (index, row) in self.adapters.iter().enumerate() {
            if row.ordinal as usize != index {
                return Err("GPU identity report ordinal changed".into());
            }
            validate_row(row, &self.adapters[..index])?;
        }
        Ok(())
    }

    pub fn find_wsl_host_adapter(
        &self,
        allocator: &GpuAdapterIdentity,
        physical_adapter_index: u32,
    ) -> Result<Option<&HostGpuIdentity>, String> {
        self.validate()?;
        if !allocator.is_stable_driver_identity()
            || allocator.wddm_physical_adapter_index() != Some(physical_adapter_index)
        {
            return Err(
                "WSL guest correspondence requires a single-node stable adapter identity".into(),
            );
        }
        let mut matches = self.adapters.iter().filter(|row| {
            row.allocator.backend == allocator.backend
                && row.allocator.key.eq_ignore_ascii_case(&allocator.key)
                && row.allocator.wddm_physical_adapter_index() == Some(physical_adapter_index)
                && row.allocator.node_mask == allocator.node_mask
        });
        let first = matches.next();
        if matches.next().is_some() {
            return Err("host GPU identity catalog has ambiguous matching adapters".into());
        }
        Ok(first)
    }

    pub fn verify_wsl_guest_adapter(
        &self,
        allocator: &GpuAdapterIdentity,
        guest_interface_id: &str,
        physical_adapter_index: u32,
    ) -> Result<&HostGpuIdentity, String> {
        validate_interface_id(guest_interface_id)?;
        let row = self
            .find_wsl_host_adapter(allocator, physical_adapter_index)?
            .ok_or_else(|| {
                "host GPU identity catalog has no matching adapter for WSL guest".to_string()
            })?;
        if !row.interface_id.eq_ignore_ascii_case(guest_interface_id) {
            return Err(
                "WSL guest vGPU interface does not match the authenticated host adapter".into(),
            );
        }
        Ok(row)
    }
}

pub fn encode_report(report: &HostGpuIdentityReport) -> Result<Vec<u8>, String> {
    report.validate()?;
    let encoded = serde_json::to_vec(report).map_err(|error| error.to_string())?;
    if encoded.len() > MAX_IDENTITY_REPORT_BYTES {
        return Err("GPU identity report exceeds its byte bound".into());
    }
    Ok(encoded)
}

pub fn decode_report(encoded: &[u8]) -> Result<HostGpuIdentityReport, String> {
    if encoded.len() > MAX_IDENTITY_REPORT_BYTES {
        return Err("GPU identity report exceeds its byte bound".into());
    }
    let report: HostGpuIdentityReport =
        serde_json::from_slice(encoded).map_err(|error| error.to_string())?;
    report.validate()?;
    Ok(report)
}

/// Original parent-measured age, never a receipt or reader timestamp.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostGpuObservation {
    pub report: HostGpuIdentityReport,
    pub age_ms: u64,
}
impl HostGpuObservation {
    pub fn validate(&self) -> Result<(), String> {
        self.report.validate()?;
        if self.age_ms < self.report.elapsed_ms || self.age_ms >= GPU_BUDGET_MAX_AGE_MS {
            return Err("GPU observation age is invalid or expired".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn cuda_identity(hex: u8, node_mask: Option<u32>, luid: Option<&str>) -> GpuAdapterIdentity {
        GpuAdapterIdentity {
            node_mask,
            backend: "cuda".into(),
            key: format!("{:032x}", hex),
            luid: luid.map(str::to_string),
        }
    }

    fn sample_report() -> HostGpuIdentityReport {
        HostGpuIdentityReport {
            schema_version: 1,
            producer: ProducerProcess {
                pid: 42,
                creation_time_100ns: 77,
            },
            started_at_unix_ms: 1000,
            elapsed_ms: 5,
            adapters: vec![HostGpuIdentity {
                ordinal: 0,
                allocator: cuda_identity(1, Some(1), Some("00000000:00011287")),
                interface_id: r"\\?\PCI#VEN_10DE&DEV_1F08&SUBSYS_0000#{1111}".into(),
            }],
        }
    }

    #[test]
    fn utf16_interface_decoder_validates_nul_surrogates_and_control_chars() {
        let mut raw = [u16::MAX; INTERFACE_UTF16_CAPACITY];
        assert!(decode_interface_id_utf16(&raw).is_err());

        raw[0] = 0;
        assert!(decode_interface_id_utf16(&raw).is_err());

        raw[0] = b'a' as u16;
        raw[1] = 0xD800; // unpaired UTF-16 surrogate
        raw[2] = 0;
        assert!(decode_interface_id_utf16(&raw).is_err());

        raw[1] = b'\n' as u16;
        assert!(decode_interface_id_utf16(&raw).is_err());

        let expected = r"\\?\PCI#VEN_10DE#iface";
        let encoded: Vec<u16> = expected.encode_utf16().collect();
        raw = [u16::MAX; INTERFACE_UTF16_CAPACITY];
        raw[..encoded.len()].copy_from_slice(&encoded);
        raw[encoded.len()] = 0;
        assert_eq!(decode_interface_id_utf16(&raw).unwrap(), expected);
    }

    #[test]
    fn wsl_guest_correspondence_requires_matching_backend_uuid_node_and_interface() {
        let report = sample_report();
        let guest_cuda = cuda_identity(1, Some(1), Some("00000000:01541119"));
        let matched = report
            .verify_wsl_guest_adapter(
                &guest_cuda,
                r"\\?\pci#ven_10de&dev_1f08&subsys_0000#{1111}",
                0,
            )
            .unwrap();
        assert_eq!(matched.ordinal, 0);

        // Unmapped UUID, missing LUID, wrong node, wrong interface, or cross-API Vulkan must all fail.
        assert!(
            report
                .find_wsl_host_adapter(&cuda_identity(2, Some(1), Some("00000000:01541119")), 0)
                .unwrap()
                .is_none()
        );
        assert!(
            report
                .find_wsl_host_adapter(&cuda_identity(2, Some(1), None), 0)
                .is_err()
        );
        assert!(
            report
                .verify_wsl_guest_adapter(
                    &cuda_identity(2, Some(1), Some("00000000:01541119")),
                    &matched.interface_id,
                    0,
                )
                .is_err()
        );
        assert!(
            report
                .verify_wsl_guest_adapter(&guest_cuda, &matched.interface_id, 1)
                .is_err()
        );
        assert!(
            report
                .verify_wsl_guest_adapter(&guest_cuda, r"\\?\other", 0)
                .is_err()
        );

        let mut vulkan = guest_cuda.clone();
        vulkan.backend = "vulkan".into();
        assert!(
            report
                .verify_wsl_guest_adapter(&vulkan, &matched.interface_id, 0)
                .is_err()
        );
    }
}
