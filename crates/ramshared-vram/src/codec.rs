// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-vram/src/codec.rs
//! Optional GPU cache codec contract (DT-2, DT-4, DT-7).
//!
//! Compression is an optional accelerator on top of a working raw cache. The
//! contract is provider-typed: it compresses into **provider-owned** VRAM
//! slabs and workspace, so a host-side codec cannot implement it and is never
//! asked to (the NFR-6b CPU control arm is a measurement harness outside this
//! trait). A provider that cannot verify a checksum before decode reports no
//! codec capability and stays raw-only.
//!
//! Every offset-plus-length calculation is checked before any provider call
//! (RF-6). Bounds here are the first refusal point; they are not a substitute
//! for the worker's physical-budget admission.

use core::fmt;

use crate::{VramError, VramMemory};

/// Identifies one codec implementation and its wire format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CodecId {
    /// Provider-specific raw storage: no compression applied.
    Raw,
    /// Deterministic in-test codec. Never a production capability.
    Fake,
    /// nvCOMP LZ4, dynamically loaded (ITEM-3).
    NvcompLz4,
}

impl CodecId {
    /// Stable short label for telemetry and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            CodecId::Raw => "raw",
            CodecId::Fake => "fake",
            CodecId::NvcompLz4 => "nvcomp-lz4",
        }
    }
}

impl fmt::Display for CodecId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Per-item outcome of one provider codec operation (DT-7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodecStatus {
    /// Item completed with the exact expected lengths.
    Ok,
    /// Item is incompressible or larger than the reserved output.
    Incompressible,
    /// Stored payload checksum mismatched **before** decode.
    ChecksumMismatch,
    /// Provider reported an error for this item only.
    Failed,
    /// Operation did not complete within its sub-deadline (DT-3, DT-11).
    TimedOut,
}

impl CodecStatus {
    /// `true` only for a fully verified success.
    pub fn is_ok(self) -> bool {
        self == CodecStatus::Ok
    }
}

impl fmt::Display for CodecStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CodecStatus::Ok => "ok",
            CodecStatus::Incompressible => "incompressible",
            CodecStatus::ChecksumMismatch => "checksum-mismatch",
            CodecStatus::Failed => "failed",
            CodecStatus::TimedOut => "timed-out",
        })
    }
}

/// One allocator-owned region inside a backing slab (DT-5).
///
/// `stored_len` is the exact payload length the codec reported;
/// `allocation_len` is the allocator-rounded physical length the slab owns.
/// Both are carried so every offset-plus-length calculation is checked against
/// the *physical* region before a provider call (RF-6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VramSpan {
    /// Byte offset of this span inside its backing slab.
    pub offset: u64,
    /// Exact payload length produced or consumed.
    pub stored_len: usize,
    /// Allocator-owned physical length of the region.
    pub allocation_len: usize,
}

impl VramSpan {
    /// Builds a span, refusing any length that exceeds its allocation.
    pub fn new(offset: u64, stored_len: usize, allocation_len: usize) -> Result<Self, VramError> {
        if stored_len > allocation_len {
            return Err(VramError::OutOfRange {
                off: offset,
                len: stored_len as u64,
                size: allocation_len as u64,
            });
        }
        let end = offset
            .checked_add(allocation_len as u64)
            .ok_or(VramError::OutOfRange {
                off: offset,
                len: allocation_len as u64,
                size: u64::MAX,
            })?;
        let _ = end;
        Ok(Self {
            offset,
            stored_len,
            allocation_len,
        })
    }

    /// Exclusive end offset of the **allocation**, not the payload.
    pub fn alloc_end(&self) -> Result<u64, VramError> {
        self.offset
            .checked_add(self.allocation_len as u64)
            .ok_or(VramError::OutOfRange {
                off: self.offset,
                len: self.allocation_len as u64,
                size: u64::MAX,
            })
    }

    /// Exclusive end offset of the **stored payload**.
    pub fn stored_end(&self) -> Result<u64, VramError> {
        self.offset
            .checked_add(self.stored_len as u64)
            .ok_or(VramError::OutOfRange {
                off: self.offset,
                len: self.stored_len as u64,
                size: u64::MAX,
            })
    }

    /// Refuses any span that does not fit inside `slab_len`.
    pub fn check_within(&self, slab_len: usize) -> Result<(), VramError> {
        let end = self.alloc_end()?;
        if end > slab_len as u64 {
            return Err(VramError::OutOfRange {
                off: self.offset,
                len: self.allocation_len as u64,
                size: slab_len as u64,
            });
        }
        Ok(())
    }
}

/// A writable reservation inside a backing slab, for one compressed output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VramOutputReservation {
    /// Byte offset of the reservation inside its backing slab.
    pub offset: u64,
    /// Writable payload capacity the worker accepts.
    pub capacity: usize,
    /// Allocator-owned physical length of the region.
    pub allocation_len: usize,
}

impl VramOutputReservation {
    /// Builds a reservation, refusing a capacity above its allocation.
    pub fn new(offset: u64, capacity: usize, allocation_len: usize) -> Result<Self, VramError> {
        if capacity > allocation_len {
            return Err(VramError::OutOfRange {
                off: offset,
                len: capacity as u64,
                size: allocation_len as u64,
            });
        }
        Ok(Self {
            offset,
            capacity,
            allocation_len,
        })
    }

    /// Refuses any reservation that does not fit inside `slab_len`.
    pub fn check_within(&self, slab_len: usize) -> Result<(), VramError> {
        let end =
            self.offset
                .checked_add(self.allocation_len as u64)
                .ok_or(VramError::OutOfRange {
                    off: self.offset,
                    len: self.allocation_len as u64,
                    size: u64::MAX,
                })?;
        if end > slab_len as u64 {
            return Err(VramError::OutOfRange {
                off: self.offset,
                len: self.allocation_len as u64,
                size: slab_len as u64,
            });
        }
        Ok(())
    }
}

/// Required byte alignments for each buffer class (DT-4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodecAlignments {
    /// Alignment of a host compression input.
    pub compression_input: usize,
    /// Alignment of a device compression output.
    pub compression_output: usize,
    /// Alignment of a device decompression input.
    pub decompression_input: usize,
    /// Alignment of a host decompression output.
    pub decompression_output: usize,
    /// Alignment of the provider workspace buffer.
    pub workspace: usize,
}

impl CodecAlignments {
    /// The conservative all-1 alignment, for codecs with no constraint.
    pub const fn byte() -> Self {
        Self {
            compression_input: 1,
            compression_output: 1,
            decompression_input: 1,
            decompression_output: 1,
            workspace: 1,
        }
    }

    /// Refuses zero or non-power-of-two alignments (DT-4).
    pub fn validate(self) -> Result<(), VramError> {
        for align in [
            self.compression_input,
            self.compression_output,
            self.decompression_input,
            self.decompression_output,
            self.workspace,
        ] {
            if align == 0 || !align.is_power_of_two() {
                return Err(VramError::InvalidAlignment);
            }
        }
        Ok(())
    }
}

/// Per-item result of one compression (DT-7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodecChunkResult {
    /// Per-item status.
    pub status: CodecStatus,
    /// Exact encoded payload length. Must be `<=` the reservation capacity.
    pub encoded_len: usize,
}

/// Optional provider-owned GPU cache codec (DT-2).
///
/// Implemented per provider and receiving provider-owned memory, bounded host
/// inputs/outputs, and queried scratch. A host-side codec cannot implement this
/// trait: it needs `VramMemory` slabs. The measurement-only CPU control arm
/// lives outside this contract.
pub trait GpuCacheCodec<M: VramMemory> {
    /// Which codec this is.
    fn codec_id(&self) -> CodecId;

    /// Required alignments for each buffer class (DT-4).
    fn required_alignments(&self) -> CodecAlignments;

    /// Maximum encoded length for one logical input (DT-4).
    ///
    /// Called **before** any output is reserved, so an item whose worst case
    /// exceeds the reservation is refused without an allocation.
    fn max_encoded_len(&self, logical_len: usize) -> Result<usize, VramError>;

    /// Workspace bytes for one batch (DT-4).
    fn workspace_bytes(
        &self,
        item_count: usize,
        max_logical_len: usize,
    ) -> Result<usize, VramError>;

    /// Compresses one batch into reserved slab regions.
    ///
    /// The worker accepts only a reported `encoded_len <= reservation.capacity`.
    fn compress_batch_into(
        &self,
        inputs: &[&[u8]],
        slab: &mut M,
        outputs: &[VramOutputReservation],
        workspace: &mut M,
    ) -> Result<Vec<CodecChunkResult>, VramError>;

    /// Provider-side checksum of the **stored** payloads (DT-7).
    ///
    /// Computed on the provider while the bytes remain in VRAM, so a mismatch
    /// can refuse the entry without ever invoking the decoder. CRC32 detects
    /// accidental corruption, not malicious modification.
    fn checksum_batch(
        &self,
        slab: &M,
        inputs: &[VramSpan],
        workspace: &mut M,
    ) -> Result<Vec<u32>, VramError>;

    /// Decodes one batch from slab regions into private host buffers (DT-7).
    ///
    /// The caller has already refused any entry whose stored checksum
    /// mismatched. Per-item status and exact output length are checked by the
    /// caller after this returns.
    fn decompress_batch_from(
        &self,
        slab: &M,
        inputs: &[VramSpan],
        logical_lengths: &[usize],
        outputs: &mut [Vec<u8>],
        workspace: &mut M,
    ) -> Result<Vec<CodecStatus>, VramError>;
}

/// Deterministic in-test codec (ITEM-1). Never a production capability.
///
/// A real, reversible byte-level RLE so both product branches are reachable
/// from one implementation: repetitive data shrinks and is stored compressed,
/// random data grows and is reported `Incompressible` so the caller keeps the
/// raw representation. Wire format is self-describing and bounded, so
/// `max_encoded_len` is exact and a reservation is never overrun.
///
/// Selected only through `CodecId::Fake`. No production provider reports this
/// capability; `VramProvider::cache_codec` stays `None` everywhere in the
/// shipping tree.
pub struct FakeCodec {
    compress_invocations: core::sync::atomic::AtomicUsize,
    decode_invocations: core::sync::atomic::AtomicUsize,
}

impl Default for FakeCodec {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeCodec {
    /// A fresh codec with zeroed invocation counters.
    pub const fn new() -> Self {
        Self {
            compress_invocations: core::sync::atomic::AtomicUsize::new(0),
            decode_invocations: core::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// How many times `compress_batch_into` entered the item loop.
    pub fn compress_invocations(&self) -> usize {
        self.compress_invocations
            .load(core::sync::atomic::Ordering::SeqCst)
    }

    /// How many times `decompress_batch_from` entered the item loop.
    ///
    /// A checksum refusal must leave this at its previous value: the decoder is
    /// never invoked for an entry whose stored payload already failed (DT-7).
    pub fn decode_invocations(&self) -> usize {
        self.decode_invocations
            .load(core::sync::atomic::Ordering::SeqCst)
    }

    /// Resets both counters (test fixture hygiene).
    pub fn reset_counters(&self) {
        self.compress_invocations
            .store(0, core::sync::atomic::Ordering::SeqCst);
        self.decode_invocations
            .store(0, core::sync::atomic::Ordering::SeqCst);
    }

    /// Worst-case RLE expansion of `len` bytes.
    ///
    /// Every 255 input bytes become at most a 257-byte literal record, plus one
    /// trailing record for the remainder.
    fn worst_case_encoded_len(logical_len: usize) -> Result<usize, VramError> {
        let full = logical_len / 255;
        let rest = logical_len % 255;
        let mut total = full
            .checked_mul(257)
            .and_then(|bytes| bytes.checked_add(2))
            .ok_or(VramError::InvalidAlignment)?;
        if rest > 0 {
            total = total
                .checked_add(rest)
                .and_then(|bytes| bytes.checked_add(2))
                .ok_or(VramError::InvalidAlignment)?;
        }
        Ok(total)
    }
}

/// One RLE record: `0x00, n, v` (repeat) or `0x01, n, b0..bn-1` (literal).
const RLE_REPEAT: u8 = 0x00;
const RLE_LITERAL: u8 = 0x01;

fn rle_encode(input: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut i = 0usize;
    let mut w = 0usize;
    while i < input.len() {
        let value = input[i];
        let mut run = 1usize;
        while i + run < input.len() && input[i + run] == value && run < 255 {
            run += 1;
        }
        if run >= 3 {
            if w + 3 > out.len() {
                return None;
            }
            out[w] = RLE_REPEAT;
            out[w + 1] = run as u8;
            out[w + 2] = value;
            w += 3;
            i += run;
        } else {
            // Accumulate a literal run, stopping before a run of 3+ equals.
            let start = i;
            let mut lit = 0usize;
            while i < input.len() && lit < 255 {
                let v = input[i];
                let mut ahead = 1usize;
                while i + ahead < input.len() && input[i + ahead] == v && ahead < 3 {
                    ahead += 1;
                }
                if ahead >= 3 && lit > 0 {
                    break;
                }
                if ahead >= 3 && lit == 0 {
                    break;
                }
                i += 1;
                lit += 1;
            }
            if lit == 0 {
                // Defensive: the loop above must always consume at least one
                // byte or make progress on a repeat. Fall back to a single
                // literal so `rle_encode` can never spin forever.
                lit = 1;
                i = start + 1;
            }
            if w + 2 + lit > out.len() {
                return None;
            }
            out[w] = RLE_LITERAL;
            out[w + 1] = lit as u8;
            out[w + 2..w + 2 + lit].copy_from_slice(&input[start..start + lit]);
            w += 2 + lit;
        }
    }
    Some(w)
}

fn rle_decode(input: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut r = 0usize;
    let mut w = 0usize;
    while r < input.len() {
        let tag = input[r];
        match tag {
            RLE_REPEAT => {
                if r + 3 > input.len() {
                    return None;
                }
                let n = input[r + 1] as usize;
                let value = input[r + 2];
                if n == 0 || w + n > out.len() {
                    return None;
                }
                out[w..w + n].fill(value);
                w += n;
                r += 3;
            }
            RLE_LITERAL => {
                if r + 2 > input.len() {
                    return None;
                }
                let n = input[r + 1] as usize;
                if n == 0 || r + 2 + n > input.len() || w + n > out.len() {
                    return None;
                }
                out[w..w + n].copy_from_slice(&input[r + 2..r + 2 + n]);
                w += n;
                r += 2 + n;
            }
            _ => return None,
        }
    }
    Some(w)
}

impl<M: VramMemory> GpuCacheCodec<M> for FakeCodec {
    fn codec_id(&self) -> CodecId {
        CodecId::Fake
    }

    fn required_alignments(&self) -> CodecAlignments {
        CodecAlignments::byte()
    }

    fn max_encoded_len(&self, logical_len: usize) -> Result<usize, VramError> {
        Self::worst_case_encoded_len(logical_len)
    }

    fn workspace_bytes(
        &self,
        _item_count: usize,
        _max_logical_len: usize,
    ) -> Result<usize, VramError> {
        Ok(0)
    }

    fn compress_batch_into(
        &self,
        inputs: &[&[u8]],
        slab: &mut M,
        outputs: &[VramOutputReservation],
        workspace: &mut M,
    ) -> Result<Vec<CodecChunkResult>, VramError> {
        let _ = workspace;
        if inputs.len() != outputs.len() {
            return Err(VramError::InvalidAlignment);
        }
        let mut results = Vec::with_capacity(inputs.len());
        // One shared scratch buffer sized to the worst case of the largest
        // item, so a reservation is never written beyond its capacity.
        let mut worst = 0usize;
        for input in inputs {
            worst = worst.max(Self::worst_case_encoded_len(input.len())?);
        }
        let mut scratch = vec![0u8; worst];
        for (input, out) in inputs.iter().zip(outputs.iter()) {
            self.compress_invocations
                .fetch_add(1, core::sync::atomic::Ordering::SeqCst);
            out.check_within(slab.len())?;
            let encoded = match rle_encode(input, &mut scratch) {
                Some(len) if len <= out.capacity => len,
                // Grew past the reservation: keep raw (DT-10 usefulness gate).
                _ => {
                    results.push(CodecChunkResult {
                        status: CodecStatus::Incompressible,
                        encoded_len: 0,
                    });
                    continue;
                }
            };
            slab.write_at(out.offset, &scratch[..encoded])?;
            results.push(CodecChunkResult {
                status: CodecStatus::Ok,
                encoded_len: encoded,
            });
        }
        Ok(results)
    }

    fn checksum_batch(
        &self,
        slab: &M,
        inputs: &[VramSpan],
        workspace: &mut M,
    ) -> Result<Vec<u32>, VramError> {
        let _ = workspace;
        let mut out = Vec::with_capacity(inputs.len());
        let mut buf = Vec::new();
        for span in inputs {
            span.check_within(slab.len())?;
            buf.resize(span.stored_len, 0u8);
            slab.read_at(span.offset, &mut buf)?;
            out.push(crc32(&buf, 0));
        }
        Ok(out)
    }

    fn decompress_batch_from(
        &self,
        slab: &M,
        inputs: &[VramSpan],
        logical_lengths: &[usize],
        outputs: &mut [Vec<u8>],
        workspace: &mut M,
    ) -> Result<Vec<CodecStatus>, VramError> {
        let _ = workspace;
        if inputs.len() != logical_lengths.len() || inputs.len() != outputs.len() {
            return Err(VramError::InvalidAlignment);
        }
        let mut statuses = Vec::with_capacity(inputs.len());
        for ((span, &logical_len), output) in inputs
            .iter()
            .zip(logical_lengths.iter())
            .zip(outputs.iter_mut())
        {
            self.decode_invocations
                .fetch_add(1, core::sync::atomic::Ordering::SeqCst);
            span.check_within(slab.len())?;
            let mut stored = vec![0u8; span.stored_len];
            slab.read_at(span.offset, &mut stored)?;
            output.resize(logical_len, 0u8);
            match rle_decode(&stored, output) {
                Some(written) if written == logical_len => {
                    statuses.push(CodecStatus::Ok);
                }
                _ => {
                    output.clear();
                    statuses.push(CodecStatus::Failed);
                }
            }
        }
        Ok(statuses)
    }
}

/// CRC32 (IEEE 802.3, reflected) over `bytes`, seeded with `crc`.
///
/// Used for both the original-bytes checksum and the stored-payload checksum
/// (DT-7). Detects accidental corruption, not malicious modification.
pub fn crc32(bytes: &[u8], crc: u32) -> u32 {
    let mut reg = !crc;
    for &b in bytes {
        reg ^= u32::from(b);
        for _ in 0..8 {
            let mask = (reg & 1).wrapping_neg();
            reg = (reg >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !reg
}

#[cfg(test)]
mod tests;
