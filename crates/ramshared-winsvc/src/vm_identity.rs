// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/vm_identity.rs
//! Bounded native VM routing observations. Labels confer no peer/GPU authority.
use crate::gpu_identity::ProducerProcess;
use crate::vm_id_resolve::parse_guid;
use ramshared_vram::GPU_BUDGET_MAX_AGE_MS;
use serde::{Deserialize, Serialize};

#[cfg(windows)]
mod api;
#[cfg(windows)]
mod native;
#[cfg(windows)]
pub mod sampler;
pub const MAX_VM_REPORT_BYTES: usize = 64 * 1024;
pub const MAX_VM_SYSTEMS: usize = 64;
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum VmOsClassification {
    /// Native ComputeCore explicitly reported `RuntimeOsType == "Linux"`.
    Linux,
    /// Native ComputeCore omitted `RuntimeOsType` (observed on Windows 11 WSL2
    /// utility VMs). Never grants OS or guest authority on its own; requires
    /// kernel AF_HYPERV peer verification and mutual HMAC authentication.
    Omitted,
}
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VmIdentity {
    pub system_id: String,
    pub runtime_id: [u8; 16],
    pub os_classification: VmOsClassification,
}
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VmIdentityReport {
    pub schema_version: u8,
    pub producer: ProducerProcess,
    pub started_at_unix_ms: u64,
    pub elapsed_ms: u64,
    pub systems: Vec<VmIdentity>,
}
fn validate_systems(systems: &[VmIdentity]) -> Result<(), String> {
    if systems.len() > MAX_VM_SYSTEMS {
        return Err("VM inventory exceeds its count bound".into());
    }
    for (index, system) in systems.iter().enumerate() {
        if system.system_id.is_empty()
            || system.system_id.len() > 256
            || system.system_id.chars().any(char::is_control)
            || system.runtime_id == [0; 16]
        {
            return Err("VM system/runtime identity is malformed".into());
        }
        if systems[..index]
            .iter()
            .any(|old| old.runtime_id == system.runtime_id || old.system_id == system.system_id)
        {
            return Err("VM inventory has duplicate system/runtime identity".into());
        }
    }
    Ok(())
}
impl VmIdentityReport {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1
            || self.producer.pid == 0
            || self.producer.creation_time_100ns == 0
            || self.started_at_unix_ms == 0
            || self.elapsed_ms >= GPU_BUDGET_MAX_AGE_MS
        {
            return Err("VM report process, version or time is invalid".into());
        }
        validate_systems(&self.systems)
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Properties {
    id: Option<String>,
    runtime_id: Option<String>,
    system_type: String,
    runtime_os_type: Option<String>,
    state: Option<String>,
    stopped: Option<bool>,
}
/// Decode the native API's bounded schema projection, never CLI/display text.
pub fn project_systems(bytes: &[u8]) -> Result<Vec<VmIdentity>, String> {
    if bytes.len() > MAX_VM_REPORT_BYTES {
        return Err("native VM properties exceed their byte bound".into());
    }
    let rows: Vec<Properties> = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    if rows.len() > MAX_VM_SYSTEMS {
        return Err("native VM properties exceed their count bound".into());
    }
    let mut systems = Vec::new();
    for row in rows {
        if row.system_type != "VirtualMachine" {
            continue;
        }
        // HCS omits State for some created VMs. A route always requires
        // an explicit Running state, including for an unclassified guest.
        if row.state.as_deref() != Some("Running") {
            continue;
        }
        if row.stopped == Some(true) {
            return Err("native VM reports contradictory running/stopped state".into());
        }
        let os_classification = match row.runtime_os_type.as_deref() {
            Some("Linux") => VmOsClassification::Linux,
            Some(other) if other.trim().is_empty() => {
                return Err("native VM OS type is malformed".into());
            }
            Some(_) => continue,
            None => VmOsClassification::Omitted,
        };
        let runtime_id = parse_guid(
            &row.runtime_id
                .ok_or("native VM runtime ID is unavailable")?,
        )
        .map_err(|error| error.to_string())?;
        systems.push(VmIdentity {
            system_id: row.id.ok_or("native VM system ID is unavailable")?,
            runtime_id,
            os_classification,
        });
    }
    validate_systems(&systems)?;
    Ok(systems)
}
pub fn decode_report(bytes: &[u8]) -> Result<VmIdentityReport, String> {
    if bytes.len() > MAX_VM_REPORT_BYTES {
        return Err("VM report exceeds its byte bound".into());
    }
    let report: VmIdentityReport =
        serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    report.validate()?;
    Ok(report)
}
pub fn inspect_command() -> i32 {
    #[cfg(windows)]
    let result = native::inspect().and_then(|report| {
        use std::io::Write;
        report.validate()?;
        let bytes = serde_json::to_vec(&report).map_err(|error| error.to_string())?;
        if bytes.len() > MAX_VM_REPORT_BYTES {
            return Err("VM report exceeds its byte bound".into());
        }
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(&bytes)
            .and_then(|()| stdout.write_all(b"\n"))
            .map_err(|error| error.to_string())
    });
    #[cfg(not(windows))]
    let result: Result<(), String> = Err("native VM inspection requires Windows".into());
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("VM inspection failed: {error}");
            2
        }
    }
}

impl crate::gpu_identity::supervisor::ProbeReport for VmIdentityReport {
    fn decode(bytes: &[u8]) -> Result<Self, String> {
        decode_report(bytes)
    }
    fn producer(&self) -> &ProducerProcess {
        &self.producer
    }
    fn window(&self) -> (u64, u64) {
        (self.started_at_unix_ms, self.elapsed_ms)
    }
}
