// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-ipc/src/cache_wire.rs
//! Bounded binary data-lane codec and mutual proof for pooled Hyper-V VRAM cache.
//! SPEC: the protocol specification §DT-3, §DT-4.

use std::io::{Read, Write};

pub mod handshake;
pub use handshake::{
    CACHE_CHALLENGE_LEN, CACHE_CHALLENGE_MAGIC, CACHE_FINISH_LEN, CACHE_FINISH_MAGIC,
    CACHE_HELLO_LEN, CACHE_HELLO_MAGIC, CacheClientFinish, CacheClientHello, CacheHostChallenge,
    origin_fingerprint_from_binding, read_client_finish, read_client_hello, read_host_challenge,
    write_client_finish, write_client_hello, write_host_challenge,
};

use crate::FrameError;
use crate::frame_io::read_exact_checked;
use crate::handshake::{compute_hmac, constant_time_eq};
use crate::vsock::linux_service_guid;

pub const CACHE_WIRE_MAGIC: [u8; 4] = *b"RSCP";
pub const CACHE_WIRE_VERSION: u8 = 1;
pub const CACHE_HEADER_LEN: usize = 48;
pub const CACHE_PAGE_BYTES: usize = 4096;
pub const CACHE_CRC_BYTES: usize = 4;
pub const CACHE_PAYLOAD_BYTES: usize = CACHE_PAGE_BYTES + CACHE_CRC_BYTES;
pub const CACHE_MAX_FRAME_BYTES: usize = CACHE_HEADER_LEN + CACHE_PAYLOAD_BYTES;
pub const DEFAULT_POOLED_CACHE_PORT: u32 = 2762;
const LABELS: [&[u8]; 3] = [
    b"ramshared-pooled-cache-guest-v1",
    b"ramshared-pooled-cache-host-v1",
    b"ramshared-pooled-cache-finish-v1",
];

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum CacheGuestRole {
    Wsl2 = 1,
    SecondaryGuest = 2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CacheOpcode {
    Lookup = 1,
    Offer = 2,
    Hit = 129,
    Miss = 130,
    Accepted = 131,
    Busy = 132,
}

impl CacheOpcode {
    pub const fn from_byte(raw: u8) -> Option<Self> {
        match raw {
            1 => Some(Self::Lookup),
            2 => Some(Self::Offer),
            129 => Some(Self::Hit),
            130 => Some(Self::Miss),
            131 => Some(Self::Accepted),
            132 => Some(Self::Busy),
            _ => None,
        }
    }
    pub const fn expected_payload_len(self) -> u32 {
        match self {
            Self::Offer | Self::Hit => CACHE_PAYLOAD_BYTES as u32,
            Self::Lookup | Self::Miss | Self::Accepted | Self::Busy => 0,
        }
    }
    pub const fn accepts_reply(self, reply: Self) -> bool {
        matches!(
            (self, reply),
            (Self::Lookup, Self::Hit | Self::Miss | Self::Busy)
                | (Self::Offer, Self::Accepted | Self::Busy)
        )
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum CacheWireError {
    InvalidPort,
    PortCollision,
    InvalidScope(&'static str),
    InvalidMagic([u8; 4]),
    UnsupportedVersion(u8),
    UnknownOpcode(u8),
    NonZeroReserved,
    InvalidPayloadLength { expected: u32, got: u32 },
    InvalidCorrelation,
    InvalidEpoch,
    UnalignedOrOverflowOffset(u64),
    OutOfExtent(u64),
    CrcMismatch { expected: u32, got: u32 },
    UnsolicitedOrMismatchedReply,
    Frame(FrameError),
}
impl std::fmt::Display for CacheWireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for CacheWireError {}

/// Validate a pooled data-lane port and return its Linux Hyper-V service GUID.
pub fn validate_cache_data_port(port: u32, control_port: u32) -> Result<[u8; 16], CacheWireError> {
    if port == 0 || port > 0x7FFF_FFFF {
        return Err(CacheWireError::InvalidPort);
    }
    if port == control_port {
        return Err(CacheWireError::PortCollision);
    }
    Ok(linux_service_guid(port))
}

/// Deterministic Castagnoli CRC32C over `bytes`.
#[must_use]
pub fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0x82F6_3B78 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

fn le_u64(raw: &[u8]) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&raw[..8]);
    u64::from_le_bytes(bytes)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CacheFrameHeader {
    pub version: u8,
    pub opcode: CacheOpcode,
    pub payload_len: u32,
    pub correlation: u64,
    pub epoch: u64,
    pub offset_bytes: u64,
    pub content_gen: u64,
}

impl CacheFrameHeader {
    pub fn new(
        opcode: CacheOpcode,
        correlation: u64,
        epoch: u64,
        offset_bytes: u64,
        content_gen: u64,
    ) -> Result<Self, CacheWireError> {
        let header = Self {
            version: CACHE_WIRE_VERSION,
            opcode,
            payload_len: opcode.expected_payload_len(),
            correlation,
            epoch,
            offset_bytes,
            content_gen,
        };
        header.validate()?;
        Ok(header)
    }

    pub fn validate(&self) -> Result<(), CacheWireError> {
        if self.version != CACHE_WIRE_VERSION {
            return Err(CacheWireError::UnsupportedVersion(self.version));
        }
        let expected = self.opcode.expected_payload_len();
        if self.payload_len != expected {
            return Err(CacheWireError::InvalidPayloadLength {
                expected,
                got: self.payload_len,
            });
        }
        if self.correlation == 0 || self.correlation == u64::MAX {
            return Err(CacheWireError::InvalidCorrelation);
        }
        if self.epoch == 0 {
            return Err(CacheWireError::InvalidEpoch);
        }
        if !self.offset_bytes.is_multiple_of(CACHE_PAGE_BYTES as u64)
            || self
                .offset_bytes
                .checked_add(CACHE_PAGE_BYTES as u64)
                .is_none()
        {
            return Err(CacheWireError::UnalignedOrOverflowOffset(self.offset_bytes));
        }
        Ok(())
    }

    pub fn encode(&self) -> [u8; CACHE_HEADER_LEN] {
        let mut out = [0u8; CACHE_HEADER_LEN];
        out[0..4].copy_from_slice(&CACHE_WIRE_MAGIC);
        out[4] = self.version;
        out[5] = self.opcode as u8;
        out[8..12].copy_from_slice(&self.payload_len.to_le_bytes());
        out[16..24].copy_from_slice(&self.correlation.to_le_bytes());
        out[24..32].copy_from_slice(&self.epoch.to_le_bytes());
        out[32..40].copy_from_slice(&self.offset_bytes.to_le_bytes());
        out[40..48].copy_from_slice(&self.content_gen.to_le_bytes());
        out
    }

    pub fn decode(raw: &[u8; CACHE_HEADER_LEN]) -> Result<Self, CacheWireError> {
        let magic = [raw[0], raw[1], raw[2], raw[3]];
        if magic != CACHE_WIRE_MAGIC {
            return Err(CacheWireError::InvalidMagic(magic));
        }
        if raw[4] != CACHE_WIRE_VERSION {
            return Err(CacheWireError::UnsupportedVersion(raw[4]));
        }
        let opcode = CacheOpcode::from_byte(raw[5]).ok_or(CacheWireError::UnknownOpcode(raw[5]))?;
        if raw[6] != 0 || raw[7] != 0 || raw[12..16] != [0; 4] {
            return Err(CacheWireError::NonZeroReserved);
        }
        let header = Self {
            version: raw[4],
            opcode,
            payload_len: u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]),
            correlation: le_u64(&raw[16..24]),
            epoch: le_u64(&raw[24..32]),
            offset_bytes: le_u64(&raw[32..40]),
            content_gen: le_u64(&raw[40..48]),
        };
        header.validate()?;
        Ok(header)
    }
}

/// Fixed pre-allocated buffer for zero-heap steady-state cache frame I/O.
pub struct CacheFrameBuffer {
    bytes: [u8; CACHE_MAX_FRAME_BYTES],
}
impl Default for CacheFrameBuffer {
    fn default() -> Self {
        Self::new()
    }
}
impl CacheFrameBuffer {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            bytes: [0u8; CACHE_MAX_FRAME_BYTES],
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct CacheFrame<'a> {
    pub header: CacheFrameHeader,
    pub page: Option<&'a [u8; CACHE_PAGE_BYTES]>,
    pub crc: Option<u32>,
}

/// Read one length- and checksum-validated frame directly into `buf`.
pub fn read_into<'a, R: Read>(
    reader: &mut R,
    buf: &'a mut CacheFrameBuffer,
) -> Result<CacheFrame<'a>, CacheWireError> {
    let mut header_raw = [0u8; CACHE_HEADER_LEN];
    read_exact_checked(reader, &mut header_raw).map_err(CacheWireError::Frame)?;
    let header = CacheFrameHeader::decode(&header_raw)?;
    buf.bytes[..CACHE_HEADER_LEN].copy_from_slice(&header_raw);
    if header.payload_len == 0 {
        return Ok(CacheFrame {
            header,
            page: None,
            crc: None,
        });
    }
    let body = &mut buf.bytes[CACHE_HEADER_LEN..CACHE_MAX_FRAME_BYTES];
    read_exact_checked(reader, body).map_err(CacheWireError::Frame)?;
    let (page_bytes, crc_bytes) = body.split_at(CACHE_PAGE_BYTES);
    let got_crc = u32::from_le_bytes([crc_bytes[0], crc_bytes[1], crc_bytes[2], crc_bytes[3]]);
    let expected_crc = crc32c(page_bytes);
    if got_crc != expected_crc {
        return Err(CacheWireError::CrcMismatch {
            expected: expected_crc,
            got: got_crc,
        });
    }
    let page: &[u8; CACHE_PAGE_BYTES] =
        page_bytes
            .try_into()
            .map_err(|_| CacheWireError::InvalidPayloadLength {
                expected: CACHE_PAYLOAD_BYTES as u32,
                got: header.payload_len,
            })?;
    Ok(CacheFrame {
        header,
        page: Some(page),
        crc: Some(got_crc),
    })
}

/// Encode and write one validated cache frame using `buf` without heap allocation.
pub fn write_frame_with<W: Write>(
    writer: &mut W,
    buf: &mut CacheFrameBuffer,
    header: CacheFrameHeader,
    page: Option<&[u8; CACHE_PAGE_BYTES]>,
) -> Result<(), CacheWireError> {
    header.validate()?;
    buf.bytes[..CACHE_HEADER_LEN].copy_from_slice(&header.encode());
    let total = match (header.payload_len, page) {
        (0, None) => CACHE_HEADER_LEN,
        (len, Some(page_bytes)) if len == CACHE_PAYLOAD_BYTES as u32 => {
            buf.bytes[CACHE_HEADER_LEN..CACHE_HEADER_LEN + CACHE_PAGE_BYTES]
                .copy_from_slice(page_bytes);
            buf.bytes[CACHE_HEADER_LEN + CACHE_PAGE_BYTES..CACHE_MAX_FRAME_BYTES]
                .copy_from_slice(&crc32c(page_bytes).to_le_bytes());
            CACHE_MAX_FRAME_BYTES
        }
        (expected, _) => {
            return Err(CacheWireError::InvalidPayloadLength {
                expected,
                got: page.map_or(0, |_| CACHE_PAYLOAD_BYTES as u32),
            });
        }
    };
    writer
        .write_all(&buf.bytes[..total])
        .and_then(|()| writer.flush())
        .map_err(|e| CacheWireError::Frame(FrameError::StreamIo(e.to_string())))
}

/// Authenticated scope binding role, native route, origin identity, nonces and epoch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CacheScope {
    pub role: CacheGuestRole,
    pub runtime_id: [u8; 16],
    pub service_guid: [u8; 16],
    pub origin_fingerprint: [u8; 32],
    pub epoch: u64,
    pub extent_bytes: u64,
    pub guest_nonce: [u8; 32],
    pub host_nonce: [u8; 32],
}

impl CacheScope {
    pub fn validate(&self) -> Result<(), CacheWireError> {
        if self.runtime_id == [0; 16] || self.service_guid == [0; 16] {
            return Err(CacheWireError::InvalidScope("zero route identity"));
        }
        if self.origin_fingerprint == [0; 32] {
            return Err(CacheWireError::InvalidScope("zero origin fingerprint"));
        }
        if self.epoch == 0 {
            return Err(CacheWireError::InvalidEpoch);
        }
        if self.extent_bytes == 0 || !self.extent_bytes.is_multiple_of(CACHE_PAGE_BYTES as u64) {
            return Err(CacheWireError::InvalidScope("invalid extent geometry"));
        }
        if self.guest_nonce == [0; 32] || self.host_nonce == [0; 32] {
            return Err(CacheWireError::InvalidScope("nonce not fresh"));
        }
        Ok(())
    }

    fn mac(&self, key: &[u8], label_idx: usize) -> Result<[u8; 32], CacheWireError> {
        if key.len() < 32 {
            return Err(CacheWireError::InvalidScope("key shorter than 256 bits"));
        }
        self.validate()?;
        let label = LABELS[label_idx];
        let mut data = Vec::with_capacity(label.len() + 146);
        data.extend_from_slice(label);
        data.extend_from_slice(&[CACHE_WIRE_VERSION, self.role as u8]);
        for part in [
            self.runtime_id.as_slice(),
            &self.service_guid,
            &self.origin_fingerprint,
            &self.epoch.to_le_bytes(),
            &self.extent_bytes.to_le_bytes(),
            &self.guest_nonce,
            &self.host_nonce,
        ] {
            data.extend_from_slice(part);
        }
        compute_hmac(key, &data)
            .as_slice()
            .try_into()
            .map_err(|_| CacheWireError::InvalidScope("unexpected mac length"))
    }

    pub fn guest_proof(&self, key: &[u8]) -> Result<[u8; 32], CacheWireError> {
        self.mac(key, 0)
    }
    pub fn host_proof(&self, key: &[u8]) -> Result<[u8; 32], CacheWireError> {
        self.mac(key, 1)
    }
    pub fn finish_proof(&self, key: &[u8]) -> Result<[u8; 32], CacheWireError> {
        self.mac(key, 2)
    }
    pub fn verify_guest_proof(&self, key: &[u8], proof: &[u8; 32]) -> bool {
        self.guest_proof(key)
            .is_ok_and(|e| constant_time_eq(&e, proof))
    }
    pub fn verify_host_proof(&self, key: &[u8], proof: &[u8; 32]) -> bool {
        self.host_proof(key)
            .is_ok_and(|e| constant_time_eq(&e, proof))
    }
    pub fn verify_finish_proof(&self, key: &[u8], proof: &[u8; 32]) -> bool {
        self.finish_proof(key)
            .is_ok_and(|e| constant_time_eq(&e, proof))
    }

    pub fn admit_request(
        &self,
        hdr: &CacheFrameHeader,
        last_corr: &mut u64,
    ) -> Result<(), CacheWireError> {
        hdr.validate()?;
        if !matches!(hdr.opcode, CacheOpcode::Lookup | CacheOpcode::Offer) {
            return Err(CacheWireError::UnsolicitedOrMismatchedReply);
        }
        if hdr.epoch != self.epoch {
            return Err(CacheWireError::InvalidEpoch);
        }
        if hdr.correlation <= *last_corr {
            return Err(CacheWireError::InvalidCorrelation);
        }
        let end = hdr
            .offset_bytes
            .checked_add(CACHE_PAGE_BYTES as u64)
            .ok_or(CacheWireError::UnalignedOrOverflowOffset(hdr.offset_bytes))?;
        if end > self.extent_bytes {
            return Err(CacheWireError::OutOfExtent(hdr.offset_bytes));
        }
        *last_corr = hdr.correlation;
        Ok(())
    }

    pub fn admit_reply(
        &self,
        req: &CacheFrameHeader,
        rep: &CacheFrameHeader,
    ) -> Result<(), CacheWireError> {
        rep.validate()?;
        if !req.opcode.accepts_reply(rep.opcode)
            || (
                rep.epoch,
                rep.correlation,
                rep.offset_bytes,
                rep.content_gen,
            ) != (
                self.epoch,
                req.correlation,
                req.offset_bytes,
                req.content_gen,
            )
            || req.epoch != self.epoch
        {
            return Err(CacheWireError::UnsolicitedOrMismatchedReply);
        }
        Ok(())
    }
}
