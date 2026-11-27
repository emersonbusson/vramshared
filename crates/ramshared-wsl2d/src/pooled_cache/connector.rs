// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/pooled_cache/connector.rs
//! Guest-side RSCP v1 bootstrap connector over `AF_VSOCK` to the Windows host authority.
//! SPEC: the protocol specification §DT-1, §DT-3, §DT-6.

use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::time::Duration;

use ramshared_block::isolated_origin::PooledReadState;
use ramshared_ipc::cache_wire::{
    CacheClientFinish, CacheClientHello, CacheGuestRole, CacheScope,
    origin_fingerprint_from_binding, read_host_challenge, write_client_finish, write_client_hello,
};
use ramshared_ipc::random_nonce;
use ramshared_ipc::vsock::{VsockStreamTrait, connect_vsock, linux_service_guid, listen_vsock};

use super::profile::{
    CacheProfile, CacheProfileSelection, ProfileError, StagedCacheProfile, load_boot_profile,
};
use super::{
    GuestBootError, GuestCacheSession, HostBootstrapProof, StreamPooledClient, VMADDR_CID_HOST,
    verify_exclusive_origin,
};

const MAX_KEY_FILE_BYTES: u64 = 256;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
/// Hard socket timeout above the 2 ms DT-6 soft deadline so a single first-page
/// GPU warmup or scheduling jitter tick triggers clean SSD fallback via the 2 ms
/// `PooledReadState` deadline without permanently poisoning framing, while a truly
/// stalled host still closes and retires cleanly within 20 ms.
const STEADY_SOCKET_TIMEOUT: Duration = Duration::from_millis(20);

/// Load a 256-bit secret key without following symlinks and enforcing strict file mode.
pub fn read_pooled_key(path: &Path) -> Result<[u8; 32], String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| format!("open pooled key {}: {e}", path.display()))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("stat pooled key {}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err("pooled key is not a regular file".into());
    }
    if meta.mode() & 0o077 != 0 {
        return Err("pooled key must not be group- or world-accessible (require 0600)".into());
    }
    if meta.len() == 0 || meta.len() > MAX_KEY_FILE_BYTES {
        return Err("pooled key file length out of bounds".into());
    }
    let mut raw = Vec::with_capacity(meta.len() as usize);
    file.take(MAX_KEY_FILE_BYTES)
        .read_to_end(&mut raw)
        .map_err(|e| format!("read pooled key: {e}"))?;
    parse_key_bytes(&raw)
}

/// Parse 32 raw bytes or 64 trimmed ASCII hex characters into a non-zero 256-bit key.
pub fn parse_key_bytes(raw: &[u8]) -> Result<[u8; 32], String> {
    let key = if raw.len() == 32 {
        let mut out = [0u8; 32];
        out.copy_from_slice(raw);
        out
    } else {
        let text = std::str::from_utf8(raw)
            .map_err(|_| "pooled key must be 32 raw bytes or 64 ASCII hex chars".to_string())?
            .trim();
        if text.len() != 64 {
            return Err("pooled key length is neither 32 raw bytes nor 64 hex chars".into());
        }
        let mut out = [0u8; 32];
        for (idx, chunk) in text.as_bytes().as_chunks::<2>().0.iter().enumerate() {
            let pair = std::str::from_utf8(chunk).map_err(|_| "invalid hex utf8".to_string())?;
            out[idx] = u8::from_str_radix(pair, 16)
                .map_err(|_| "invalid hex digit in pooled key".to_string())?;
        }
        out
    };
    if key == [0u8; 32] {
        return Err("all-zero pooled key is refused".into());
    }
    Ok(key)
}

/// Execute the 3-message RSCP v1 mutual HMAC handshake on `stream` and wrap it for data I/O.
pub fn handshake_and_wrap<S: VsockStreamTrait + 'static>(
    mut stream: S,
    role: CacheGuestRole,
    data_port: u32,
    key: [u8; 32],
    origin_fingerprint: [u8; 32],
    extent_bytes: u64,
    guest_nonce: [u8; 32],
) -> Result<(HostBootstrapProof, StreamPooledClient), String> {
    let _ = stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT));
    let hello = CacheClientHello {
        role,
        requested_epoch: 1,
        extent_bytes,
        origin_fingerprint,
        guest_nonce,
    };
    write_client_hello(&mut stream, &hello).map_err(|e| format!("write client hello: {e}"))?;
    let challenge =
        read_host_challenge(&mut stream).map_err(|e| format!("read host challenge: {e}"))?;
    if !challenge.accepted
        || challenge.epoch == 0
        || challenge.runtime_id == [0u8; 16]
        || challenge.host_nonce == [0u8; 32]
    {
        return Err("host refused pooled cache session".into());
    }
    let scope = CacheScope {
        role,
        runtime_id: challenge.runtime_id,
        service_guid: linux_service_guid(data_port),
        origin_fingerprint,
        epoch: challenge.epoch,
        extent_bytes,
        guest_nonce,
        host_nonce: challenge.host_nonce,
    };
    if !scope.verify_host_proof(&key, &challenge.host_proof) {
        return Err("host bootstrap HMAC proof mismatch".into());
    }
    let guest_proof = scope
        .guest_proof(&key)
        .map_err(|e| format!("guest_proof failed: {e}"))?;
    let finish_proof = scope
        .finish_proof(&key)
        .map_err(|e| format!("finish_proof failed: {e}"))?;
    write_client_finish(
        &mut stream,
        &CacheClientFinish {
            guest_proof,
            finish_proof,
        },
    )
    .map_err(|e| format!("write client finish: {e}"))?;
    let _ = stream.set_read_timeout(Some(STEADY_SOCKET_TIMEOUT));
    let _ = stream.set_write_timeout(Some(STEADY_SOCKET_TIMEOUT));
    let proof = HostBootstrapProof {
        host_cid: VMADDR_CID_HOST,
        scope: scope.clone(),
        host_proof: challenge.host_proof,
        key,
    };
    Ok((proof, StreamPooledClient::new(scope, stream)))
}

/// Resolve the staged boot profile and, when `Pooled`, attempt live `AF_VSOCK` bootstrap.
///
/// Any host absence, key error, binding mismatch, or auth refusal falls back cleanly to
/// `PooledOriginOnly` while retaining the authoritative local origin.
pub fn connect_boot_pooled_session(
    profile_path: &Path,
    actual_origin_binding: &str,
    active_swaps_and_roots: &[&str],
    extent_bytes: u64,
) -> Result<(GuestCacheSession, Option<PooledReadState>), GuestBootError> {
    let selection = load_boot_profile(profile_path);
    let is_wsl = matches!(
        &selection,
        Ok(CacheProfileSelection::Staged(doc)) if doc.role == "wsl2"
    );
    connect_boot_pooled_with_dialer(
        selection,
        actual_origin_binding,
        active_swaps_and_roots,
        extent_bytes,
        |port| {
            if is_wsl {
                listen_vsock(port, CONNECT_TIMEOUT).map_err(|e| e.to_string())
            } else {
                connect_vsock(VMADDR_CID_HOST, port, CONNECT_TIMEOUT).map_err(|e| e.to_string())
            }
        },
    )
}

/// Injectable dialer variant for hermetic unit and end-to-end stream testing.
pub fn connect_boot_pooled_with_dialer<S, D>(
    selection: Result<CacheProfileSelection, ProfileError>,
    actual_origin_binding: &str,
    active_swaps_and_roots: &[&str],
    extent_bytes: u64,
    dial: D,
) -> Result<(GuestCacheSession, Option<PooledReadState>), GuestBootError>
where
    S: VsockStreamTrait + 'static,
    D: FnOnce(u32) -> Result<S, String>,
{
    verify_exclusive_origin(actual_origin_binding, active_swaps_and_roots)?;
    let doc = match &selection {
        Ok(CacheProfileSelection::Staged(doc)) if doc.profile == CacheProfile::Pooled => {
            doc.clone()
        }
        _ => {
            let session = GuestCacheSession::from_boot_selection(
                selection,
                actual_origin_binding,
                active_swaps_and_roots,
                None,
            )?;
            return Ok((session, None));
        }
    };
    match attempt_pooled_connect(&doc, actual_origin_binding, extent_bytes, dial) {
        Ok((proof, client)) => {
            let epoch = proof.scope.epoch;
            let session = GuestCacheSession::from_boot_selection(
                selection,
                actual_origin_binding,
                active_swaps_and_roots,
                Some(proof),
            )?;
            if session.active_epoch() == epoch {
                Ok((session, Some(PooledReadState::new(epoch, client))))
            } else {
                Ok((session, None))
            }
        }
        Err(reason) => {
            let session = GuestCacheSession::from_boot_selection(
                selection,
                actual_origin_binding,
                active_swaps_and_roots,
                None,
            )?;
            session.retire_to_origin_only(&reason);
            Ok((session, None))
        }
    }
}

fn attempt_pooled_connect<S, D>(
    doc: &StagedCacheProfile,
    actual_origin_binding: &str,
    extent_bytes: u64,
    dial: D,
) -> Result<(HostBootstrapProof, StreamPooledClient), String>
where
    S: VsockStreamTrait + 'static,
    D: FnOnce(u32) -> Result<S, String>,
{
    let role = doc.validate().map_err(|e| format!("profile_invalid:{e}"))?;
    if !doc
        .origin_binding
        .trim()
        .eq_ignore_ascii_case(actual_origin_binding.trim())
    {
        return Err("origin_binding_mismatch".into());
    }
    let key = read_pooled_key(Path::new(&doc.key_ref))?;
    let fp = origin_fingerprint_from_binding(actual_origin_binding)
        .map_err(|e| format!("origin_fingerprint:{e}"))?;
    let nonce = random_nonce().map_err(|e| format!("nonce:{e}"))?;
    let stream = dial(doc.data_port)?;
    handshake_and_wrap(stream, role, doc.data_port, key, fp, extent_bytes, nonce)
}
