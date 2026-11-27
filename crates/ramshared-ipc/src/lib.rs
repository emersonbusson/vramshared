// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-ipc/src/lib.rs
//! Shared host-guest IPC protocol for RamShared vsock control plane.
//!
//! Binary framed protocol with magic `0x52414D53` ("RAMS"), versioned headers,
//! bounded payloads, and HMAC-authenticated handshake.
//!

pub mod budget_observation;
pub mod cache_wire;
pub mod deadline;
mod frame_io;
pub mod gpu_catalog;
mod handshake;
mod json_payload;
pub mod storage_proof;
pub mod storage_transport;
pub mod vsock;
pub use frame_io::{read_frame, write_frame};
pub use handshake::*;

use serde::{Deserialize, Serialize};

pub const IPC_MAGIC: u32 = 0x52414D53;
pub const IPC_VERSION_3: u32 = 3;
pub const IPC_VERSION_4: u32 = 4;
/// Reserved for the storage-capable protocol; not advertised until both
/// endpoints implement its complete exchange.
pub const IPC_VERSION_5: u32 = 5;
/// Adds activity to the signed catalog body; the frame layout is unchanged.
pub const IPC_VERSION_6: u32 = 6;
// Catalog negotiation with bounded per-process activity. Frame versions stay unchanged.
pub const IPC_VERSION_7: u32 = 7;
pub const IPC_MAX_VERSION: u32 = IPC_VERSION_7;
pub const IPC_MIN_VERSION: u32 = 2;
pub const MAX_PAYLOAD_LEN: u32 = 1024 * 1024;
pub const MAX_CONTROL_PAYLOAD: u32 = 4 * 1024;
pub const MAX_MANIFEST_PAYLOAD: u32 = 64 * 1024;
pub const MAX_GPU_CATALOG_PAYLOAD: u32 = 64 * 1024;
pub const FRAME_HEADER_LEN: usize = 24;

pub const MSG_HANDSHAKE: u8 = 1;
pub const MSG_HANDSHAKE_ACK: u8 = 2;
pub const MSG_HEARTBEAT: u8 = 3;
pub const MSG_HEARTBEAT_ACK: u8 = 4;
pub const MSG_LEASE_REQUEST: u8 = 5;
pub const MSG_LEASE_GRANTED: u8 = 6;
pub const MSG_LEASE_DENIED: u8 = 7;
pub const MSG_LEASE_RELEASE: u8 = 8;
pub const MSG_ORIGIN_MANIFEST: u8 = 9;
pub const MSG_ORIGIN_MANIFEST_ACK: u8 = 10;
pub const MSG_SAFE_MODE_GATE: u8 = 11;
pub const MSG_SAFE_MODE_GATE_ACK: u8 = 12;
pub const MSG_GUARDIAN_HEALTH: u8 = 13;
pub const MSG_GUARDIAN_HEALTH_ACK: u8 = 14;
pub const MSG_VHDX_ATTACH: u8 = 15;
pub const MSG_VHDX_ATTACH_ACK: u8 = 16;
pub const MSG_VHDX_DETACH: u8 = 17;
pub const MSG_VHDX_DETACH_ACK: u8 = 18;
pub const MSG_TELEMETRY: u8 = 19;
pub const MSG_SHUTDOWN: u8 = 20;
pub const MSG_SHUTDOWN_ACK: u8 = 21;
pub const MSG_HANDSHAKE_FINISH: u8 = 22;
pub const MSG_GPU_CATALOG: u8 = 23;
pub const MSG_STORAGE_REQUEST: u8 = 24;
pub const MSG_STORAGE_RESPONSE: u8 = 25;
pub const MSG_MAX: u8 = MSG_STORAGE_RESPONSE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VsockFrameHeader {
    pub magic: u32,
    pub version: u32,
    pub payload_len: u32,
    pub flags: u32,
    pub correlation_id: u64,
}

impl VsockFrameHeader {
    pub fn new(msg_type: u8, payload_len: u32, correlation_id: u64) -> Self {
        Self {
            magic: IPC_MAGIC,
            version: if msg_type == MSG_GPU_CATALOG {
                IPC_VERSION_4
            } else if matches!(msg_type, MSG_STORAGE_REQUEST | MSG_STORAGE_RESPONSE) {
                IPC_VERSION_5
            } else {
                IPC_VERSION_3
            },
            payload_len,
            flags: msg_type as u32,
            correlation_id,
        }
    }

    pub fn msg_type(&self) -> u8 {
        (self.flags & 0xFF) as u8
    }

    pub fn encode(&self) -> [u8; FRAME_HEADER_LEN] {
        let mut buf = [0u8; FRAME_HEADER_LEN];
        buf[0..4].copy_from_slice(&self.magic.to_le_bytes());
        buf[4..8].copy_from_slice(&self.version.to_le_bytes());
        buf[8..12].copy_from_slice(&self.payload_len.to_le_bytes());
        buf[12..16].copy_from_slice(&self.flags.to_le_bytes());
        buf[16..24].copy_from_slice(&self.correlation_id.to_le_bytes());
        buf
    }

    pub fn decode(buf: &[u8; FRAME_HEADER_LEN]) -> Result<Self, FrameError> {
        let mut b4 = [0u8; 4];
        b4.copy_from_slice(&buf[0..4]);
        let magic = u32::from_le_bytes(b4);
        if magic != IPC_MAGIC {
            return Err(FrameError::InvalidMagic(magic));
        }
        b4.copy_from_slice(&buf[4..8]);
        let version = u32::from_le_bytes(b4);
        // Version 6 is a negotiated payload dialect. Wire headers keep
        // their established message-specific versions.
        if !(IPC_MIN_VERSION..=IPC_VERSION_5).contains(&version) {
            return Err(FrameError::UnsupportedVersion(version));
        }
        b4.copy_from_slice(&buf[8..12]);
        let payload_len = u32::from_le_bytes(b4);
        if payload_len > MAX_PAYLOAD_LEN {
            return Err(FrameError::PayloadTooLarge(payload_len));
        }
        b4.copy_from_slice(&buf[12..16]);
        let flags = u32::from_le_bytes(b4);
        if flags & !0xff != 0 {
            return Err(FrameError::UnknownFlags(flags));
        }
        let msg_type = (flags & 0xFF) as u8;
        if msg_type == 0 || msg_type > MSG_MAX {
            return Err(FrameError::UnknownMessageType(msg_type));
        }
        if msg_type == MSG_GPU_CATALOG && !matches!(version, IPC_VERSION_4 | IPC_VERSION_5) {
            return Err(FrameError::UnsupportedVersion(version));
        }
        if matches!(msg_type, MSG_STORAGE_REQUEST | MSG_STORAGE_RESPONSE)
            && version != IPC_VERSION_5
        {
            return Err(FrameError::UnsupportedVersion(version));
        }
        let mut b8 = [0u8; 8];
        b8.copy_from_slice(&buf[16..24]);
        let correlation_id = u64::from_le_bytes(b8);
        Ok(Self {
            magic,
            version,
            payload_len,
            flags,
            correlation_id,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    InvalidMagic(u32),
    UnsupportedVersion(u32),
    PayloadTooLarge(u32),
    UnknownMessageType(u8),
    UnknownFlags(u32),
    PayloadLengthOverflow,
    PayloadDeserialization(String),
    PayloadExceedsCap(u32),
    /// The stream failed mid-frame. Retried only when the caller classifies the
    /// underlying signature as transient.
    StreamIo(String),
    /// The peer closed before the declared frame length arrived.
    TruncatedFrame {
        expected: usize,
        got: usize,
    },
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidMagic(m) => write!(f, "invalid magic: {m:#010X}"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported version: {v}"),
            Self::PayloadTooLarge(l) => write!(f, "payload too large: {l} bytes"),
            Self::UnknownMessageType(t) => write!(f, "unknown message type: {t}"),
            Self::UnknownFlags(flags) => write!(f, "unknown frame flags: {flags:#010x}"),
            Self::PayloadLengthOverflow => write!(f, "payload length cannot be represented"),
            Self::PayloadDeserialization(e) => write!(f, "payload deserialization failed: {e}"),
            Self::PayloadExceedsCap(l) => {
                write!(f, "payload exceeds its message byte cap: {l} bytes")
            }
            Self::StreamIo(e) => write!(f, "frame stream i/o failed: {e}"),
            Self::TruncatedFrame { expected, got } => {
                write!(f, "truncated frame: expected {expected} bytes, got {got}")
            }
        }
    }
}

impl std::error::Error for FrameError {}

// --- Message payload types ---

/// Guest `Handshake` (message type 1).
///
/// `boot_id` and `distro_id` are claims/metadata only — they are not
/// authenticated identity by themselves. Identity comes from the
/// role-separated HMAC transcript (DT-3).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Handshake {
    pub min_version: u32,
    pub max_version: u32,
    pub boot_id: String,
    pub distro_id: String,
    pub guest_nonce: [u8; 32],
    pub guest_proof: [u8; 32],
}

/// Host `HandshakeAck` (message type 2).
///
/// Carries the host challenge: a fresh nonce plus a host proof over both
/// nonces and the complete transcript. The guest accepts authority only
/// after this proof validates.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HandshakeAck {
    pub accepted_version: u32,
    pub heartbeat_secs: u64,
    pub lease_timeout_secs: u64,
    pub host_nonce: [u8; 32],
    pub host_proof: [u8; 32],
}

/// Guest `HandshakeFinish` (message type 22).
///
/// Proves the same transcript back to the host. No manifest or lease is
/// sent until the host validates this proof.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HandshakeFinish {
    pub guest_finish_proof: [u8; 32],
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Heartbeat {
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostMemoryTelemetry {
    pub sampled_at_unix_ms: u64,
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub commit_total_bytes: u64,
    pub commit_limit_bytes: u64,
    pub page_size_bytes: u64,
    /// `vmmemWSL` working set when the host can observe it. Never a fabricated
    /// zero: `None` means the host did not measure it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vmmem_wsl_working_set_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HeartbeatAck {
    pub timestamp_ms: u64,
    pub lease_remaining_ms: u64,
    /// Host memory sampled on the authenticated heartbeat. Older hosts omit
    /// the field; the guest keeps `None` rather than inventing a reading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_memory: Option<HostMemoryTelemetry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LeaseRequest {
    pub nonce: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LeaseGranted {
    pub lease_id: u32,
    pub deadline_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LeaseDenied {
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LeaseRelease {
    pub lease_id: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OriginManifestPayload {
    pub sha256_hex: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OriginManifestAck {
    pub sealed: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SafeModeGate {
    pub boot_id: String,
    pub safe_mode: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SafeModeGateAck {
    pub accepted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GuardianHealth {
    pub timestamp_ms: u64,
    pub healthy: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GuardianHealthAck {
    pub accepted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VhdxAttachRequest {
    pub path: String,
    pub partuuid: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VhdxAttachAck {
    pub attached: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VhdxDetachRequest {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VhdxDetachAck {
    pub detached: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Telemetry {
    pub control_plane_state: String,
    pub heartbeat_rtt_us: u64,
    pub lease_remaining_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Shutdown {
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShutdownAck {
    pub acknowledged: bool,
}

// --- Serialization helpers ---

pub(crate) fn checked_payload_length(bytes: usize) -> Result<u32, FrameError> {
    u32::try_from(bytes).map_err(|_| FrameError::PayloadLengthOverflow)
}

pub fn encode_control<T: Serialize>(msg: &T) -> Result<Vec<u8>, FrameError> {
    json_payload::encode(msg, MAX_CONTROL_PAYLOAD)
}

pub fn decode_control<T: for<'de> Deserialize<'de>>(payload: &[u8]) -> Result<T, FrameError> {
    let len = checked_payload_length(payload.len())?;
    if len > MAX_CONTROL_PAYLOAD {
        return Err(FrameError::PayloadExceedsCap(len));
    }
    serde_json::from_slice(payload).map_err(|e| FrameError::PayloadDeserialization(e.to_string()))
}

pub fn encode_manifest(m: &OriginManifestPayload) -> Result<Vec<u8>, FrameError> {
    let total = 2_usize
        .checked_add(m.sha256_hex.len())
        .and_then(|len| len.checked_add(m.data.len()))
        .ok_or(FrameError::PayloadLengthOverflow)?;
    let len = checked_payload_length(total)?;
    if len > MAX_MANIFEST_PAYLOAD {
        return Err(FrameError::PayloadExceedsCap(len));
    }
    let mut buf = Vec::with_capacity(total);
    buf.extend_from_slice(&(m.sha256_hex.len() as u16).to_le_bytes());
    buf.extend_from_slice(m.sha256_hex.as_bytes());
    buf.extend_from_slice(&m.data);
    Ok(buf)
}

pub fn decode_manifest(payload: &[u8]) -> Result<OriginManifestPayload, FrameError> {
    let len = checked_payload_length(payload.len())?;
    if len > MAX_MANIFEST_PAYLOAD {
        return Err(FrameError::PayloadExceedsCap(len));
    }
    if payload.len() < 2 {
        return Err(FrameError::PayloadDeserialization(
            "manifest payload too short".into(),
        ));
    }
    let mut hl = [0u8; 2];
    hl.copy_from_slice(&payload[0..2]);
    let hex_len = u16::from_le_bytes(hl) as usize;
    if payload.len() < 2 + hex_len {
        return Err(FrameError::PayloadDeserialization(
            "manifest hex length exceeds payload".into(),
        ));
    }
    let sha256_hex = std::str::from_utf8(&payload[2..2 + hex_len])
        .map_err(|e| FrameError::PayloadDeserialization(e.to_string()))?
        .to_string();
    let data = payload[2 + hex_len..].to_vec();
    Ok(OriginManifestPayload { sha256_hex, data })
}

// --- Stream framing ---
//
#[cfg(test)]
mod tests;
