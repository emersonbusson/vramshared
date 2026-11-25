// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/vm_id_resolve.rs
//! Native runtime routing, independent of display labels and peer authentication.
use super::vsock_lease::LeaseServerError;
use crate::vm_identity::{VmIdentityReport, VmOsClassification};
use ramshared_ipc::vsock::{VsockPeerIdentity, linux_service_guid};

pub fn parse_guid(value: &str) -> Result<[u8; 16], LeaseServerError> {
    let text = value.trim();
    let hex = match text.strip_prefix('{') {
        Some(inner) => inner
            .strip_suffix('}')
            .ok_or_else(|| LeaseServerError::Config("invalid VM GUID delimiters".into()))?,
        None => text,
    };
    let invalid = || LeaseServerError::Config(format!("invalid VM GUID: {value}"));
    let parts: Vec<&str> = hex.split('-').collect();
    if parts.len() != 5
        || [8, 4, 4, 4, 12]
            .iter()
            .zip(&parts)
            .any(|(&len, p)| p.len() != len || !p.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(invalid());
    }
    let d1 = u32::from_str_radix(parts[0], 16).map_err(|_| invalid())?;
    let d2 = u16::from_str_radix(parts[1], 16).map_err(|_| invalid())?;
    let d3 = u16::from_str_radix(parts[2], 16).map_err(|_| invalid())?;
    let d4a = u16::from_str_radix(parts[3], 16).map_err(|_| invalid())?;
    let d4b = u64::from_str_radix(parts[4], 16).map_err(|_| invalid())?;
    let mut bytes = [0u8; 16];
    bytes[..4].copy_from_slice(&d1.to_be_bytes());
    bytes[4..6].copy_from_slice(&d2.to_be_bytes());
    bytes[6..8].copy_from_slice(&d3.to_be_bytes());
    bytes[8..10].copy_from_slice(&d4a.to_be_bytes());
    bytes[10..16].copy_from_slice(&d4b.to_be_bytes()[2..8]);
    if bytes == [0; 16] {
        return Err(LeaseServerError::Config(
            "zero VM GUID is not a guest runtime".into(),
        ));
    }
    Ok(bytes)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuestRouteCandidate {
    pub runtime_id: [u8; 16],
    pub os_classification: VmOsClassification,
}
impl GuestRouteCandidate {
    pub const fn requires_native_peer_proof(self) -> bool {
        matches!(self.os_classification, VmOsClassification::Omitted)
    }
}

/// Requires an explicit native Linux OS classification; omitted OS refuses.
pub fn select_guest_vm_guid(report: &VmIdentityReport) -> Result<[u8; 16], LeaseServerError> {
    let candidate = select_connect_candidate(report)?;
    if candidate.os_classification != VmOsClassification::Linux {
        return Err(LeaseServerError::Config(
            "native VM OS type is unavailable; requires endpoint-verified routing".into(),
        ));
    }
    Ok(candidate.runtime_id)
}

/// Select all validated running ComputeCore candidates (`Linux` before `Omitted`).
pub fn select_connect_candidates(
    report: &VmIdentityReport,
) -> Result<Vec<GuestRouteCandidate>, LeaseServerError> {
    report.validate().map_err(LeaseServerError::Config)?;
    if report.systems.is_empty() {
        return Err(LeaseServerError::Config(
            "no running native Linux VM route is available".into(),
        ));
    }
    let mut out = Vec::with_capacity(report.systems.len());
    for wanted in [VmOsClassification::Linux, VmOsClassification::Omitted] {
        out.extend(
            report
                .systems
                .iter()
                .filter(|s| s.os_classification == wanted)
                .map(|s| GuestRouteCandidate {
                    runtime_id: s.runtime_id,
                    os_classification: s.os_classification,
                }),
        );
    }
    Ok(out)
}

/// Select the sole running ComputeCore candidate; absent OS/labels grant no authority.
pub fn select_connect_candidate(
    report: &VmIdentityReport,
) -> Result<GuestRouteCandidate, LeaseServerError> {
    match select_connect_candidates(report)?.as_slice() {
        [only] => Ok(*only),
        _ => Err(LeaseServerError::Config(
            "multiple native VM routes require explicit selection".into(),
        )),
    }
}

/// Require kernel-observed AF_HYPERV peer equality on the expected VM/service route.
pub fn verify_connected_route_peer(
    expected_vm_id: [u8; 16],
    port: u32,
    observed_peer: Option<VsockPeerIdentity>,
) -> Result<VsockPeerIdentity, LeaseServerError> {
    if !(1..=0x7FFF_FFFF).contains(&port) {
        return Err(LeaseServerError::Config(
            "vsock port must be in 1..=0x7FFFFFFF".into(),
        ));
    }
    let expected = VsockPeerIdentity {
        vm_id: expected_vm_id,
        service_id: linux_service_guid(port),
    };
    let peer = observed_peer.ok_or(LeaseServerError::Refused(
        "connected stream lacks native AF_HYPERV peer identity",
    ))?;
    if peer != expected {
        return Err(LeaseServerError::Refused(
            "connected native peer differs from expected VM/service route",
        ));
    }
    Ok(peer)
}

#[cfg(windows)]
fn current_vm_observation() -> Result<VmIdentityReport, LeaseServerError> {
    crate::vm_identity::sampler::observation().ok_or_else(|| {
        let detail = crate::vm_identity::sampler::diagnostic()
            .map(|error| format!("; last observation failure: {error}"))
            .unwrap_or_default();
        LeaseServerError::Config(format!(
            "fresh supervised native VM observation is unavailable{detail}"
        ))
    })
}

#[cfg(windows)]
pub fn resolve_guest_vm_guid() -> Result<[u8; 16], LeaseServerError> {
    select_guest_vm_guid(&current_vm_observation()?)
}

#[cfg(windows)]
pub fn resolve_connect_candidate() -> Result<GuestRouteCandidate, LeaseServerError> {
    select_connect_candidate(&current_vm_observation()?)
}

#[cfg(windows)]
pub fn resolve_connect_candidates() -> Result<Vec<GuestRouteCandidate>, LeaseServerError> {
    select_connect_candidates(&current_vm_observation()?)
}
