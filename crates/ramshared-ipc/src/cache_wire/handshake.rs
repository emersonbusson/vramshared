// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-ipc/src/cache_wire/handshake.rs
//! Fixed-size binary RSCP v1 mutual HMAC bootstrap frames.

use std::io::{Read, Write};

use super::{CACHE_PAGE_BYTES, CACHE_WIRE_VERSION, CacheGuestRole, CacheWireError};
use crate::FrameError;
use crate::frame_io::read_exact_checked;
use crate::handshake::compute_hmac;

pub const CACHE_HELLO_MAGIC: [u8; 4] = *b"RSCH";
pub const CACHE_CHALLENGE_MAGIC: [u8; 4] = *b"RSHC";
pub const CACHE_FINISH_MAGIC: [u8; 4] = *b"RSCF";

pub const CACHE_HELLO_LEN: usize = 88;
pub const CACHE_CHALLENGE_LEN: usize = 96;
pub const CACHE_FINISH_LEN: usize = 72;

const ORIGIN_BINDING_DOMAIN: &[u8] = b"ramshared-pooled-origin-binding-v1";

/// Derive the canonical 32-byte origin fingerprint from a normalized binding string.
pub fn origin_fingerprint_from_binding(origin_binding: &str) -> Result<[u8; 32], CacheWireError> {
    let normalized = origin_binding.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return Err(CacheWireError::InvalidScope("empty origin binding"));
    }
    compute_hmac(ORIGIN_BINDING_DOMAIN, normalized.as_bytes())
        .as_slice()
        .try_into()
        .map_err(|_| CacheWireError::InvalidScope("unexpected fingerprint length"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CacheClientHello {
    pub role: CacheGuestRole,
    pub requested_epoch: u64,
    pub extent_bytes: u64,
    pub origin_fingerprint: [u8; 32],
    pub guest_nonce: [u8; 32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CacheHostChallenge {
    pub accepted: bool,
    pub epoch: u64,
    pub runtime_id: [u8; 16],
    pub host_nonce: [u8; 32],
    pub host_proof: [u8; 32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CacheClientFinish {
    pub guest_proof: [u8; 32],
    pub finish_proof: [u8; 32],
}

impl CacheClientHello {
    pub fn validate(&self) -> Result<(), CacheWireError> {
        if self.extent_bytes == 0 || !self.extent_bytes.is_multiple_of(CACHE_PAGE_BYTES as u64) {
            return Err(CacheWireError::InvalidScope("invalid hello extent"));
        }
        if self.origin_fingerprint == [0; 32] || self.guest_nonce == [0; 32] {
            return Err(CacheWireError::InvalidScope(
                "zero fingerprint or guest nonce",
            ));
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<[u8; CACHE_HELLO_LEN], CacheWireError> {
        self.validate()?;
        let mut out = [0u8; CACHE_HELLO_LEN];
        out[0..4].copy_from_slice(&CACHE_HELLO_MAGIC);
        out[4] = CACHE_WIRE_VERSION;
        out[5] = self.role as u8;
        out[8..16].copy_from_slice(&self.requested_epoch.to_le_bytes());
        out[16..24].copy_from_slice(&self.extent_bytes.to_le_bytes());
        out[24..56].copy_from_slice(&self.origin_fingerprint);
        out[56..88].copy_from_slice(&self.guest_nonce);
        Ok(out)
    }

    pub fn decode(raw: &[u8; CACHE_HELLO_LEN]) -> Result<Self, CacheWireError> {
        if raw[0..4] != CACHE_HELLO_MAGIC {
            return Err(CacheWireError::InvalidMagic([
                raw[0], raw[1], raw[2], raw[3],
            ]));
        }
        if raw[4] != CACHE_WIRE_VERSION {
            return Err(CacheWireError::UnsupportedVersion(raw[4]));
        }
        if raw[6] != 0 || raw[7] != 0 {
            return Err(CacheWireError::NonZeroReserved);
        }
        let role = match raw[5] {
            1 => CacheGuestRole::Wsl2,
            2 => CacheGuestRole::SecondaryGuest,
            other => return Err(CacheWireError::UnknownOpcode(other)),
        };
        let requested_epoch = u64::from_le_bytes([
            raw[8], raw[9], raw[10], raw[11], raw[12], raw[13], raw[14], raw[15],
        ]);
        let extent_bytes = u64::from_le_bytes([
            raw[16], raw[17], raw[18], raw[19], raw[20], raw[21], raw[22], raw[23],
        ]);
        let mut origin_fingerprint = [0u8; 32];
        origin_fingerprint.copy_from_slice(&raw[24..56]);
        let mut guest_nonce = [0u8; 32];
        guest_nonce.copy_from_slice(&raw[56..88]);
        let hello = Self {
            role,
            requested_epoch,
            extent_bytes,
            origin_fingerprint,
            guest_nonce,
        };
        hello.validate()?;
        Ok(hello)
    }
}

impl CacheHostChallenge {
    pub fn rejected() -> Self {
        Self {
            accepted: false,
            epoch: 0,
            runtime_id: [0u8; 16],
            host_nonce: [0u8; 32],
            host_proof: [0u8; 32],
        }
    }

    pub fn encode(&self) -> [u8; CACHE_CHALLENGE_LEN] {
        let mut out = [0u8; CACHE_CHALLENGE_LEN];
        out[0..4].copy_from_slice(&CACHE_CHALLENGE_MAGIC);
        out[4] = CACHE_WIRE_VERSION;
        out[5] = u8::from(!self.accepted);
        out[8..16].copy_from_slice(&self.epoch.to_le_bytes());
        out[16..32].copy_from_slice(&self.runtime_id);
        out[32..64].copy_from_slice(&self.host_nonce);
        out[64..96].copy_from_slice(&self.host_proof);
        out
    }

    pub fn decode(raw: &[u8; CACHE_CHALLENGE_LEN]) -> Result<Self, CacheWireError> {
        if raw[0..4] != CACHE_CHALLENGE_MAGIC {
            return Err(CacheWireError::InvalidMagic([
                raw[0], raw[1], raw[2], raw[3],
            ]));
        }
        if raw[4] != CACHE_WIRE_VERSION {
            return Err(CacheWireError::UnsupportedVersion(raw[4]));
        }
        if raw[6] != 0 || raw[7] != 0 {
            return Err(CacheWireError::NonZeroReserved);
        }
        let accepted = match raw[5] {
            0 => true,
            1 => false,
            other => return Err(CacheWireError::UnknownOpcode(other)),
        };
        let epoch = u64::from_le_bytes([
            raw[8], raw[9], raw[10], raw[11], raw[12], raw[13], raw[14], raw[15],
        ]);
        let mut runtime_id = [0u8; 16];
        runtime_id.copy_from_slice(&raw[16..32]);
        let mut host_nonce = [0u8; 32];
        host_nonce.copy_from_slice(&raw[32..64]);
        let mut host_proof = [0u8; 32];
        host_proof.copy_from_slice(&raw[64..96]);
        Ok(Self {
            accepted,
            epoch,
            runtime_id,
            host_nonce,
            host_proof,
        })
    }
}

impl CacheClientFinish {
    pub fn encode(&self) -> [u8; CACHE_FINISH_LEN] {
        let mut out = [0u8; CACHE_FINISH_LEN];
        out[0..4].copy_from_slice(&CACHE_FINISH_MAGIC);
        out[4] = CACHE_WIRE_VERSION;
        out[8..40].copy_from_slice(&self.guest_proof);
        out[40..72].copy_from_slice(&self.finish_proof);
        out
    }

    pub fn decode(raw: &[u8; CACHE_FINISH_LEN]) -> Result<Self, CacheWireError> {
        if raw[0..4] != CACHE_FINISH_MAGIC {
            return Err(CacheWireError::InvalidMagic([
                raw[0], raw[1], raw[2], raw[3],
            ]));
        }
        if raw[4] != CACHE_WIRE_VERSION {
            return Err(CacheWireError::UnsupportedVersion(raw[4]));
        }
        if raw[5] != 0 || raw[6] != 0 || raw[7] != 0 {
            return Err(CacheWireError::NonZeroReserved);
        }
        let mut guest_proof = [0u8; 32];
        guest_proof.copy_from_slice(&raw[8..40]);
        let mut finish_proof = [0u8; 32];
        finish_proof.copy_from_slice(&raw[40..72]);
        Ok(Self {
            guest_proof,
            finish_proof,
        })
    }
}

pub fn write_client_hello<W: Write>(
    writer: &mut W,
    hello: &CacheClientHello,
) -> Result<(), CacheWireError> {
    let raw = hello.encode()?;
    writer
        .write_all(&raw)
        .and_then(|()| writer.flush())
        .map_err(|e| CacheWireError::Frame(FrameError::StreamIo(e.to_string())))
}

pub fn read_client_hello<R: Read>(reader: &mut R) -> Result<CacheClientHello, CacheWireError> {
    let mut raw = [0u8; CACHE_HELLO_LEN];
    read_exact_checked(reader, &mut raw).map_err(CacheWireError::Frame)?;
    CacheClientHello::decode(&raw)
}

pub fn write_host_challenge<W: Write>(
    writer: &mut W,
    challenge: &CacheHostChallenge,
) -> Result<(), CacheWireError> {
    let raw = challenge.encode();
    writer
        .write_all(&raw)
        .and_then(|()| writer.flush())
        .map_err(|e| CacheWireError::Frame(FrameError::StreamIo(e.to_string())))
}

pub fn read_host_challenge<R: Read>(reader: &mut R) -> Result<CacheHostChallenge, CacheWireError> {
    let mut raw = [0u8; CACHE_CHALLENGE_LEN];
    read_exact_checked(reader, &mut raw).map_err(CacheWireError::Frame)?;
    CacheHostChallenge::decode(&raw)
}

pub fn write_client_finish<W: Write>(
    writer: &mut W,
    finish: &CacheClientFinish,
) -> Result<(), CacheWireError> {
    let raw = finish.encode();
    writer
        .write_all(&raw)
        .and_then(|()| writer.flush())
        .map_err(|e| CacheWireError::Frame(FrameError::StreamIo(e.to_string())))
}

pub fn read_client_finish<R: Read>(reader: &mut R) -> Result<CacheClientFinish, CacheWireError> {
    let mut raw = [0u8; CACHE_FINISH_LEN];
    read_exact_checked(reader, &mut raw).map_err(CacheWireError::Frame)?;
    CacheClientFinish::decode(&raw)
}
