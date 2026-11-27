// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-ipc/src/frame_io.rs
// Length-checked frame transport.

use crate::{
    FRAME_HEADER_LEN, FrameError, MAX_PAYLOAD_LEN, MSG_MAX, VsockFrameHeader,
    checked_payload_length,
};

// Length-checked frame transport. The declared payload length is validated
// against the caller's cap *before* any allocation, so a hostile peer cannot
// make the reader reserve an arbitrary buffer (security and benchmark guidelines:
// "Protocol frames: length-prefixed or length-checked; reject oversized
// payloads").

/// Write one length-checked frame: `FRAME_HEADER_LEN` header bytes followed by
/// exactly `payload.len()` payload bytes.
///
/// The header is validated in its own encoding direction by constructing it
/// through [`VsockFrameHeader::new`], so a caller cannot emit a magic, version
/// or message type the peer would reject.
pub fn write_frame<W: std::io::Write>(
    w: &mut W,
    msg_type: u8,
    payload: &[u8],
    correlation_id: u64,
) -> Result<(), FrameError> {
    if msg_type == 0 || msg_type > MSG_MAX {
        return Err(FrameError::UnknownMessageType(msg_type));
    }
    let len = checked_payload_length(payload.len())?;
    if len > MAX_PAYLOAD_LEN {
        return Err(FrameError::PayloadTooLarge(len));
    }
    let header = VsockFrameHeader::new(msg_type, len, correlation_id);
    w.write_all(&header.encode())
        .map_err(|e| FrameError::StreamIo(e.to_string()))?;
    if !payload.is_empty() {
        w.write_all(payload)
            .map_err(|e| FrameError::StreamIo(e.to_string()))?;
    }
    w.flush().map_err(|e| FrameError::StreamIo(e.to_string()))?;
    Ok(())
}

/// Read one length-checked frame.
///
/// `max_payload` is the caller's per-message cap (for example
/// [`MAX_CONTROL_PAYLOAD`] for control messages, [`MAX_MANIFEST_PAYLOAD`] for
/// manifests). A declared length above either that cap or
/// [`MAX_PAYLOAD_LEN`] is refused before a single payload byte is read or
/// allocated. A short body is reported as [`FrameError::TruncatedFrame`] and
/// never silently accepted.
pub fn read_frame<R: std::io::Read>(
    r: &mut R,
    max_payload: u32,
) -> Result<(u8, Vec<u8>, u64), FrameError> {
    let cap = max_payload.min(MAX_PAYLOAD_LEN);
    let mut header_bytes = [0u8; FRAME_HEADER_LEN];
    read_exact_checked(r, &mut header_bytes)?;
    let header = VsockFrameHeader::decode(&header_bytes)?;
    if header.payload_len > cap {
        return Err(FrameError::PayloadTooLarge(header.payload_len));
    }
    let mut payload = vec![0u8; header.payload_len as usize];
    if !payload.is_empty() {
        read_exact_checked(r, &mut payload)?;
    }
    Ok((header.msg_type(), payload, header.correlation_id))
}

/// `read_exact` that distinguishes "peer closed early" from other stream
/// failures, so a truncated frame is never mistaken for a clean empty body.
///
/// Reused by both legacy framed control I/O and fixed-buffer pooled cache I/O
/// (SPEC: the protocol specification §DT-4).
pub(crate) fn read_exact_checked<R: std::io::Read>(
    r: &mut R,
    buf: &mut [u8],
) -> Result<(), FrameError> {
    let mut filled = 0usize;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => {
                return Err(FrameError::TruncatedFrame {
                    expected: buf.len(),
                    got: filled,
                });
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(FrameError::StreamIo(e.to_string())),
        }
    }
    Ok(())
}
