// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-vram/src/device_codec/lz4.rs
//! Deterministic raw LZ4 blocks with strict page-sized decode bounds.

use super::PAGE_BYTES;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Lz4DecodeError;

pub type Lz4DecodeResult = Result<(), Lz4DecodeError>;

/// Deterministic LZ4 block encoder. Returns `None` when the encoded form is
/// not smaller than the 3072-byte compression cutoff.
pub fn lz4_compress_block(src: &[u8]) -> Option<Vec<u8>> {
    const HASH_LOG: u32 = 12;
    const HASH_SIZE: usize = 1 << HASH_LOG;
    const MIN_MATCH: usize = 4;
    if src.len() < 13 || src.len() > PAGE_BYTES {
        return None;
    }
    let mut table = vec![0usize; HASH_SIZE];
    let mut out = Vec::with_capacity(src.len());
    let mut anchor = 0usize;
    let mut pos = 0usize;
    while pos + 12 <= src.len() {
        let sequence = u32::from_le_bytes([src[pos], src[pos + 1], src[pos + 2], src[pos + 3]]);
        let hash = (sequence.wrapping_mul(2_654_435_761) >> (32 - HASH_LOG)) as usize;
        let candidate = table[hash];
        table[hash] = pos + 1;
        if candidate == 0 {
            pos += 1;
            continue;
        }
        let match_pos = candidate - 1;
        let offset = pos.saturating_sub(match_pos);
        if offset == 0
            || offset > u16::MAX as usize
            || src[match_pos..match_pos + MIN_MATCH] != src[pos..pos + MIN_MATCH]
        {
            pos += 1;
            continue;
        }
        let mut match_len = MIN_MATCH;
        while pos + match_len < src.len() - 5 && src[match_pos + match_len] == src[pos + match_len]
        {
            match_len += 1;
        }
        let literal_len = pos - anchor;
        emit_sequence(&mut out, &src[anchor..pos], literal_len, match_len, offset);
        pos += match_len;
        anchor = pos;
    }
    let literal_len = src.len() - anchor;
    emit_last_literals(&mut out, &src[anchor..], literal_len);
    if out.len() >= 3072 { None } else { Some(out) }
}

fn emit_sequence(
    out: &mut Vec<u8>,
    literals: &[u8],
    literal_len: usize,
    match_len: usize,
    offset: usize,
) {
    let token = ((literal_len.min(15) as u8) << 4) | (match_len.saturating_sub(4).min(15) as u8);
    out.push(token);
    emit_length(out, literal_len);
    out.extend_from_slice(literals);
    out.extend_from_slice(&(offset as u16).to_le_bytes());
    emit_length(out, match_len - 4);
}

fn emit_last_literals(out: &mut Vec<u8>, literals: &[u8], literal_len: usize) {
    out.push((literal_len.min(15) as u8) << 4);
    emit_length(out, literal_len);
    out.extend_from_slice(literals);
}

fn emit_length(out: &mut Vec<u8>, len: usize) {
    if len < 15 {
        return;
    }
    let mut remaining = len - 15;
    while remaining >= 255 {
        out.push(255);
        remaining -= 255;
    }
    out.push(remaining as u8);
}

/// Strict LZ4 block decoder. The output must be exactly 4096 bytes.
pub fn lz4_decompress_block(src: &[u8], dst: &mut [u8; PAGE_BYTES]) -> Lz4DecodeResult {
    let mut si = 0usize;
    let mut di = 0usize;
    while si < src.len() {
        let token = src[si];
        si += 1;
        let mut literal_len = usize::from(token >> 4);
        if literal_len == 15 {
            loop {
                let byte = *src.get(si).ok_or(Lz4DecodeError)?;
                si += 1;
                literal_len = literal_len
                    .checked_add(usize::from(byte))
                    .ok_or(Lz4DecodeError)?;
                if byte != 255 {
                    break;
                }
            }
        }
        let literal_end = si.checked_add(literal_len).ok_or(Lz4DecodeError)?;
        let copy_end = di.checked_add(literal_len).ok_or(Lz4DecodeError)?;
        if literal_end > src.len() || copy_end > dst.len() {
            return Err(Lz4DecodeError);
        }
        dst[di..copy_end].copy_from_slice(&src[si..literal_end]);
        si = literal_end;
        di = copy_end;
        if si == src.len() {
            break;
        }
        if si + 2 > src.len() {
            return Err(Lz4DecodeError);
        }
        let offset = usize::from(u16::from_le_bytes([src[si], src[si + 1]]));
        si += 2;
        if offset == 0 || offset > di {
            return Err(Lz4DecodeError);
        }
        let mut match_len = usize::from(token & 0x0F) + 4;
        if token & 0x0F == 15 {
            loop {
                let byte = *src.get(si).ok_or(Lz4DecodeError)?;
                si += 1;
                match_len = match_len
                    .checked_add(usize::from(byte))
                    .ok_or(Lz4DecodeError)?;
                if byte != 255 {
                    break;
                }
            }
        }
        let match_end = di.checked_add(match_len).ok_or(Lz4DecodeError)?;
        if match_end > dst.len() {
            return Err(Lz4DecodeError);
        }
        for index in 0..match_len {
            dst[di + index] = dst[di - offset + index];
        }
        di = match_end;
    }
    if di == dst.len() {
        Ok(())
    } else {
        Err(Lz4DecodeError)
    }
}
