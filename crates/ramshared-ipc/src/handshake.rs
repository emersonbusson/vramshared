// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-ipc/src/handshake.rs
// HMAC, constant-time comparison, nonces and the mutual handshake proofs.

use crate::{IPC_MAX_VERSION, IPC_MIN_VERSION};

/// Compare two byte strings without an early exit on the first mismatch.
///
/// The fold touches every byte of the shorter input; a length mismatch is
/// rejected up front because the length is public protocol metadata.
#[must_use]
pub fn constant_time_eq(lhs: &[u8], rhs: &[u8]) -> bool {
    if lhs.len() != rhs.len() {
        return false;
    }
    lhs.iter()
        .zip(rhs.iter())
        .fold(0u8, |acc, (&a, &b)| acc | (a ^ b))
        == 0
}

/// Compute HMAC-SHA256 using SHA-256 (manual implementation to avoid hmac crate version conflicts).
pub fn compute_hmac(secret: &[u8], data: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};

    const BLOCK_SIZE: usize = 64;
    let mut key = [0u8; BLOCK_SIZE];
    if secret.len() > BLOCK_SIZE {
        let mut hasher = Sha256::new();
        hasher.update(secret);
        let hash = hasher.finalize();
        key[..32].copy_from_slice(&hash);
    } else {
        key[..secret.len()].copy_from_slice(secret);
    }

    let mut ipad = [0x36u8; BLOCK_SIZE];
    let mut opad = [0x5cu8; BLOCK_SIZE];
    for i in 0..BLOCK_SIZE {
        ipad[i] ^= key[i];
        opad[i] ^= key[i];
    }

    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(data);
    let inner_hash = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_hash);
    outer.finalize().to_vec()
}

pub fn verify_hmac(secret: &[u8], data: &[u8], expected: &[u8]) -> bool {
    let computed = compute_hmac(secret, data);
    if computed.len() != expected.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in computed.iter().zip(expected.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

// --- Role-separated handshake transcript (DT-3) ---

/// Domain-separation label for the guest's opening proof.
pub const PROOF_LABEL_GUEST: &[u8] = b"ramshared-vsock-guest-proof-v1";
/// Domain-separation label for the host's challenge proof.
pub const PROOF_LABEL_HOST: &[u8] = b"ramshared-vsock-host-proof-v1";
/// Domain-separation label for the guest's finish proof.
pub const PROOF_LABEL_FINISH: &[u8] = b"ramshared-vsock-finish-proof-v1";

/// Errors from nonce generation and transcript assembly.
#[derive(Debug, PartialEq, Eq)]
pub enum HandshakeError {
    NonceGenerationFailed,
    NonceNotFresh,
    EmptyIdentityClaim,
    /// A role-separated proof did not validate. Distinguished from a framing
    /// error so a caller can fail closed on authenticity without treating the
    /// peer as merely malformed.
    ProofMismatch,
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonceGenerationFailed => write!(f, "OS CSPRNG nonce generation failed"),
            Self::NonceNotFresh => write!(f, "handshake nonce is all-zero (not fresh)"),
            Self::EmptyIdentityClaim => write!(f, "handshake identity claim is empty"),
            Self::ProofMismatch => write!(f, "handshake proof mismatch"),
        }
    }
}

impl std::error::Error for HandshakeError {}

/// Guest claims that enter the authenticated transcript.
///
/// `boot_id` and `distro_id` are metadata; the proof binds them but does not
/// make them an identity oracle on their own (DT-3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandshakeTranscript {
    pub min_version: u32,
    pub max_version: u32,
    pub boot_id: String,
    pub distro_id: String,
    pub guest_nonce: [u8; 32],
}

/// Host challenge that enters the authenticated transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostChallenge {
    pub accepted_version: u32,
    pub heartbeat_secs: u64,
    pub lease_timeout_secs: u64,
    pub host_nonce: [u8; 32],
}

/// Fill a 32-byte nonce from the OS CSPRNG (`getrandom(2)` on Unix,
/// `SystemFunction036` on Windows). An all-zero result is refused: it means
/// the CSPRNG did not produce a fresh value.
pub fn random_nonce() -> Result<[u8; 32], HandshakeError> {
    let mut nonce = [0u8; 32];
    #[cfg(target_os = "linux")]
    {
        let mut filled = 0usize;
        while filled < nonce.len() {
            // SAFETY: `nonce[filled..]` is a valid mutable byte slice of the
            // requested length; `getrandom` writes at most that many bytes.
            let n = unsafe {
                libc::getrandom(
                    nonce[filled..].as_mut_ptr().cast::<libc::c_void>(),
                    nonce.len() - filled,
                    0,
                )
            };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(HandshakeError::NonceGenerationFailed);
            }
            if n == 0 {
                return Err(HandshakeError::NonceGenerationFailed);
            }
            filled += n as usize;
        }
    }
    #[cfg(windows)]
    {
        // SystemFunction036 (RtlGenRandom) — the OS CSPRNG used by advapi32.
        #[link(name = "advapi32")]
        unsafe extern "system" {
            fn SystemFunction036(
                random_buffer: *mut core::ffi::c_void,
                random_buffer_length: u32,
            ) -> u8;
        }
        // SAFETY: `nonce` is a valid 32-byte buffer; the API writes exactly
        // `random_buffer_length` bytes and returns non-zero on success.
        let ok = unsafe { SystemFunction036(nonce.as_mut_ptr().cast(), nonce.len() as u32) };
        if ok == 0 {
            return Err(HandshakeError::NonceGenerationFailed);
        }
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        return Err(HandshakeError::NonceGenerationFailed);
    }
    if nonce == [0u8; 32] {
        return Err(HandshakeError::NonceNotFresh);
    }
    Ok(nonce)
}

fn proof32(secret: &[u8], data: &[u8]) -> [u8; 32] {
    let mac = compute_hmac(secret, data);
    let mut out = [0u8; 32];
    let n = mac.len().min(32);
    out[..n].copy_from_slice(&mac[..n]);
    out
}

fn append_u32(dst: &mut Vec<u8>, v: u32) {
    dst.extend_from_slice(&v.to_le_bytes());
}

fn append_u64(dst: &mut Vec<u8>, v: u64) {
    dst.extend_from_slice(&v.to_le_bytes());
}

fn append_str(dst: &mut Vec<u8>, s: &str) {
    append_u32(dst, s.len() as u32);
    dst.extend_from_slice(s.as_bytes());
}

fn guest_transcript_bytes(t: &HandshakeTranscript) -> Result<Vec<u8>, HandshakeError> {
    if t.boot_id.is_empty() || t.distro_id.is_empty() {
        return Err(HandshakeError::EmptyIdentityClaim);
    }
    if t.guest_nonce == [0u8; 32] {
        return Err(HandshakeError::NonceNotFresh);
    }
    let mut buf = Vec::with_capacity(16 + t.boot_id.len() + t.distro_id.len() + 32);
    append_u32(&mut buf, t.min_version);
    append_u32(&mut buf, t.max_version);
    append_str(&mut buf, &t.boot_id);
    append_str(&mut buf, &t.distro_id);
    buf.extend_from_slice(&t.guest_nonce);
    Ok(buf)
}

fn host_transcript_bytes(
    t: &HandshakeTranscript,
    c: &HostChallenge,
) -> Result<Vec<u8>, HandshakeError> {
    if c.host_nonce == [0u8; 32] {
        return Err(HandshakeError::NonceNotFresh);
    }
    let mut buf = guest_transcript_bytes(t)?;
    buf.extend_from_slice(&c.host_nonce);
    append_u32(&mut buf, c.accepted_version);
    append_u64(&mut buf, c.heartbeat_secs);
    append_u64(&mut buf, c.lease_timeout_secs);
    Ok(buf)
}

/// Guest opening proof: HMAC over its claims and its fresh nonce, under the
/// guest role label. Cannot validate as a host or finish proof.
pub fn guest_proof(secret: &[u8], t: &HandshakeTranscript) -> Result<[u8; 32], HandshakeError> {
    let mut data = PROOF_LABEL_GUEST.to_vec();
    data.extend_from_slice(&guest_transcript_bytes(t)?);
    Ok(proof32(secret, &data))
}

/// Host proof: HMAC over both nonces and the complete transcript, under the
/// host role label. The guest accepts authority only when this validates.
pub fn host_proof(
    secret: &[u8],
    t: &HandshakeTranscript,
    c: &HostChallenge,
) -> Result<[u8; 32], HandshakeError> {
    let mut data = PROOF_LABEL_HOST.to_vec();
    data.extend_from_slice(&host_transcript_bytes(t, c)?);
    Ok(proof32(secret, &data))
}

/// Guest finish proof: proves the same transcript back to the host. Binding
/// both fresh nonces prevents replay of a previously captured transcript.
pub fn guest_finish_proof(
    secret: &[u8],
    t: &HandshakeTranscript,
    c: &HostChallenge,
) -> Result<[u8; 32], HandshakeError> {
    let mut data = PROOF_LABEL_FINISH.to_vec();
    data.extend_from_slice(&host_transcript_bytes(t, c)?);
    Ok(proof32(secret, &data))
}

/// Constant-time equality over two 32-byte proofs.
fn ct_eq32(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for i in 0..32 {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Constant-time validation of the guest opening proof.
pub fn verify_guest_proof(secret: &[u8], t: &HandshakeTranscript, proof: &[u8; 32]) -> bool {
    match guest_proof(secret, t) {
        Ok(expected) => ct_eq32(&expected, proof),
        Err(_) => false,
    }
}

/// Constant-time validation of the host challenge proof.
pub fn verify_host_proof(
    secret: &[u8],
    t: &HandshakeTranscript,
    c: &HostChallenge,
    proof: &[u8; 32],
) -> bool {
    match host_proof(secret, t, c) {
        Ok(expected) => ct_eq32(&expected, proof),
        Err(_) => false,
    }
}

/// Constant-time validation of the guest finish proof.
pub fn verify_guest_finish_proof(
    secret: &[u8],
    t: &HandshakeTranscript,
    c: &HostChallenge,
    proof: &[u8; 32],
) -> bool {
    match guest_finish_proof(secret, t, c) {
        Ok(expected) => ct_eq32(&expected, proof),
        Err(_) => false,
    }
}

pub fn negotiate_version(guest_min: u32, guest_max: u32) -> Option<u32> {
    let lo = guest_min.max(IPC_MIN_VERSION);
    let hi = guest_max.min(IPC_MAX_VERSION);
    if lo <= hi { Some(hi) } else { None }
}
