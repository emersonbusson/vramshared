// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/gpu_cache_worker/slab_ipc.rs
// Framed IPC wire helpers and worker dispatch handlers for compressed VRAM
// slab batch operations (`MSG_SLAB_*`). Extracted out of `gpu_cache_worker.rs`
// to keep codec/slab IPC logic outside the oversized main worker module.

use std::io::Write;
use std::os::unix::net::UnixStream;

use ramshared_vram::VramProvider;
use ramshared_vram::device_codec::{CodecError, MAX_BATCH_PAGES, PAGE_BYTES, PageDecodeStatus};
use ramshared_vram::slab_pool::{SlabClass, SlotEncoding, VramSlabTicket};

use crate::gpu_worker_slab_bridge::SlabBridgeTelemetry;

use super::{FrameHeader, GpuCacheWorker, GpuWorkerConfig, STATUS_ERROR, STATUS_MISS, STATUS_OK};

pub const MSG_SLAB_COMPRESS_BATCH_REQ: u8 = 12;
pub const MSG_SLAB_COMPRESS_BATCH_RESP: u8 = 13;
pub const MSG_SLAB_RECOVER_BATCH_REQ: u8 = 14;
pub const MSG_SLAB_RECOVER_BATCH_RESP: u8 = 15;
pub const MSG_SLAB_TELEMETRY_REQ: u8 = 16;
pub const MSG_SLAB_TELEMETRY_RESP: u8 = 17;

pub const SLAB_TICKET_WIRE_LEN: usize = 24;
const COMPRESS_SLOT_WIRE_LEN: usize = 1 + SLAB_TICKET_WIRE_LEN;
const RECOVER_SLOT_WIRE_LEN: usize = 1 + PAGE_BYTES;

fn slab_class_from_u8(v: u8) -> Option<SlabClass> {
    match v {
        0 => Some(SlabClass::B512),
        1 => Some(SlabClass::B1024),
        2 => Some(SlabClass::B2048),
        3 => Some(SlabClass::B3072),
        4 => Some(SlabClass::Raw4096),
        _ => None,
    }
}

fn slot_encoding_from_u8(v: u8) -> Option<SlotEncoding> {
    match v {
        0 => Some(SlotEncoding::Lz4),
        1 => Some(SlotEncoding::Raw),
        _ => None,
    }
}

fn page_status_to_u8(status: PageDecodeStatus) -> u8 {
    match status {
        PageDecodeStatus::Ok => 0,
        PageDecodeStatus::PreDecodeCrcMismatch => 1,
        PageDecodeStatus::CorruptLz4Stream => 2,
        PageDecodeStatus::InvalidTicket => 3,
        PageDecodeStatus::SkippedOrIncompressible => 4,
    }
}

fn page_status_from_u8(v: u8) -> Option<PageDecodeStatus> {
    match v {
        0 => Some(PageDecodeStatus::Ok),
        1 => Some(PageDecodeStatus::PreDecodeCrcMismatch),
        2 => Some(PageDecodeStatus::CorruptLz4Stream),
        3 => Some(PageDecodeStatus::InvalidTicket),
        4 => Some(PageDecodeStatus::SkippedOrIncompressible),
        _ => None,
    }
}

pub fn encode_ticket(ticket: &VramSlabTicket, out: &mut [u8]) {
    debug_assert_eq!(out.len(), SLAB_TICKET_WIRE_LEN);
    out[0..4].copy_from_slice(&ticket.frame_idx.to_le_bytes());
    out[4] = ticket.sub_slot;
    out[5] = ticket.class as u8;
    out[6] = ticket.encoding as u8;
    out[7] = 0;
    out[8..10].copy_from_slice(&ticket.payload_len.to_le_bytes());
    out[10..14].copy_from_slice(&ticket.payload_crc32.to_le_bytes());
    out[14..22].copy_from_slice(&ticket.epoch.to_le_bytes());
    out[22..24].copy_from_slice(&[0, 0]);
}

pub fn decode_ticket(bytes: &[u8]) -> Option<VramSlabTicket> {
    if bytes.len() != SLAB_TICKET_WIRE_LEN || bytes[7] != 0 || bytes[22] != 0 || bytes[23] != 0 {
        return None;
    }
    let frame_idx = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let sub_slot = bytes[4];
    let class = slab_class_from_u8(bytes[5])?;
    let encoding = slot_encoding_from_u8(bytes[6])?;
    let payload_len = u16::from_le_bytes([bytes[8], bytes[9]]);
    let payload_crc32 = u32::from_le_bytes([bytes[10], bytes[11], bytes[12], bytes[13]]);
    let epoch = u64::from_le_bytes([
        bytes[14], bytes[15], bytes[16], bytes[17], bytes[18], bytes[19], bytes[20], bytes[21],
    ]);
    Some(VramSlabTicket {
        frame_idx,
        sub_slot,
        class,
        encoding,
        payload_len,
        payload_crc32,
        epoch,
    })
}

pub fn encode_compress_batch_req(pages: &[[u8; PAGE_BYTES]]) -> Result<(u32, Vec<u8>), CodecError> {
    if pages.len() > MAX_BATCH_PAGES {
        return Err(CodecError::InvalidBatchLength);
    }
    let mut payload = Vec::with_capacity(pages.len() * PAGE_BYTES);
    for page in pages {
        payload.extend_from_slice(page);
    }
    Ok((pages.len() as u32, payload))
}

pub fn encode_recover_batch_req(tickets: &[VramSlabTicket]) -> Result<(u32, Vec<u8>), CodecError> {
    if tickets.len() > MAX_BATCH_PAGES {
        return Err(CodecError::InvalidBatchLength);
    }
    let mut payload = vec![0u8; tickets.len() * SLAB_TICKET_WIRE_LEN];
    for (idx, ticket) in tickets.iter().enumerate() {
        let start = idx * SLAB_TICKET_WIRE_LEN;
        encode_ticket(ticket, &mut payload[start..start + SLAB_TICKET_WIRE_LEN]);
    }
    Ok((tickets.len() as u32, payload))
}

pub fn decode_compress_batch_resp(
    count: usize,
    payload: &[u8],
) -> Option<Vec<Option<VramSlabTicket>>> {
    if count > MAX_BATCH_PAGES || payload.len() != count * COMPRESS_SLOT_WIRE_LEN {
        return None;
    }
    let mut out = Vec::with_capacity(count);
    for chunk in payload.as_chunks::<COMPRESS_SLOT_WIRE_LEN>().0 {
        match chunk[0] {
            0 => {
                if chunk[1..].iter().any(|b| *b != 0) {
                    return None;
                }
                out.push(None);
            }
            1 => out.push(Some(decode_ticket(&chunk[1..])?)),
            _ => return None,
        }
    }
    Some(out)
}

pub fn decode_recover_batch_resp(
    count: usize,
    payload: &[u8],
) -> Option<(Vec<PageDecodeStatus>, Vec<[u8; PAGE_BYTES]>)> {
    if count > MAX_BATCH_PAGES || payload.len() != count * RECOVER_SLOT_WIRE_LEN {
        return None;
    }
    let mut statuses = Vec::with_capacity(count);
    let mut pages = Vec::with_capacity(count);
    for chunk in payload.as_chunks::<RECOVER_SLOT_WIRE_LEN>().0 {
        statuses.push(page_status_from_u8(chunk[0])?);
        let mut page = [0u8; PAGE_BYTES];
        page.copy_from_slice(&chunk[1..]);
        pages.push(page);
    }
    Some((statuses, pages))
}

fn write_slab_reply(
    socket: &mut UnixStream,
    msg_type: u8,
    status: u8,
    correlation_id: u64,
    aux: u32,
    payload: &[u8],
) -> Result<(), String> {
    let hdr = FrameHeader {
        msg_type,
        status,
        correlation_id,
        offset: 0,
        payload_len: payload.len() as u32,
        aux,
    };
    socket
        .write_all(&hdr.encode())
        .map_err(|e| format!("worker write slab resp header error: {e}"))?;
    if !payload.is_empty() {
        socket
            .write_all(payload)
            .map_err(|e| format!("worker write slab resp payload error: {e}"))?;
    }
    Ok(())
}

fn handle_compress_req<P: VramProvider>(
    worker: &mut GpuCacheWorker<'_, P>,
    hdr: &FrameHeader,
    payload: &[u8],
    socket: &mut UnixStream,
) -> Result<(), String> {
    let count = hdr.aux as usize;
    if count > MAX_BATCH_PAGES || payload.len() != count * PAGE_BYTES || worker.is_disabled() {
        return write_slab_reply(
            socket,
            MSG_SLAB_COMPRESS_BATCH_RESP,
            STATUS_ERROR,
            hdr.correlation_id,
            0,
            b"invalid slab compress batch request or disabled worker",
        );
    }
    let mut pages = Vec::with_capacity(count);
    for chunk in payload.as_chunks::<PAGE_BYTES>().0 {
        pages.push(*chunk);
    }
    let mut tickets = vec![None; count];
    match worker.compress_pages_into_slabs(&pages, &mut tickets) {
        Ok(()) => {
            let mut out = vec![0u8; count * COMPRESS_SLOT_WIRE_LEN];
            for (idx, maybe_ticket) in tickets.iter().enumerate() {
                if let Some(ticket) = maybe_ticket {
                    let base = idx * COMPRESS_SLOT_WIRE_LEN;
                    out[base] = 1;
                    encode_ticket(ticket, &mut out[base + 1..base + COMPRESS_SLOT_WIRE_LEN]);
                }
            }
            write_slab_reply(
                socket,
                MSG_SLAB_COMPRESS_BATCH_RESP,
                STATUS_OK,
                hdr.correlation_id,
                count as u32,
                &out,
            )
        }
        Err(err) => write_slab_reply(
            socket,
            MSG_SLAB_COMPRESS_BATCH_RESP,
            STATUS_ERROR,
            hdr.correlation_id,
            0,
            err.as_bytes(),
        ),
    }
}

fn handle_recover_req<P: VramProvider>(
    worker: &mut GpuCacheWorker<'_, P>,
    hdr: &FrameHeader,
    payload: &[u8],
    socket: &mut UnixStream,
) -> Result<(), String> {
    let count = hdr.aux as usize;
    if count > MAX_BATCH_PAGES
        || payload.len() != count * SLAB_TICKET_WIRE_LEN
        || worker.is_disabled()
    {
        return write_slab_reply(
            socket,
            MSG_SLAB_RECOVER_BATCH_RESP,
            STATUS_ERROR,
            hdr.correlation_id,
            0,
            b"invalid slab recover batch request or disabled worker",
        );
    }
    let mut tickets = Vec::with_capacity(count);
    for chunk in payload.as_chunks::<SLAB_TICKET_WIRE_LEN>().0 {
        let Some(ticket) = decode_ticket(chunk) else {
            return write_slab_reply(
                socket,
                MSG_SLAB_RECOVER_BATCH_RESP,
                STATUS_ERROR,
                hdr.correlation_id,
                0,
                b"malformed VramSlabTicket in recover batch request",
            );
        };
        tickets.push(ticket);
    }
    let mut pages = vec![[0u8; PAGE_BYTES]; count];
    let mut statuses = vec![PageDecodeStatus::InvalidTicket; count];
    match worker.recover_pages_from_slabs(&tickets, &mut pages, &mut statuses, |_| {
        Ok([0u8; PAGE_BYTES])
    }) {
        Ok(()) => {
            let mut out = Vec::with_capacity(count * RECOVER_SLOT_WIRE_LEN);
            for idx in 0..count {
                out.push(page_status_to_u8(statuses[idx]));
                out.extend_from_slice(&pages[idx]);
            }
            write_slab_reply(
                socket,
                MSG_SLAB_RECOVER_BATCH_RESP,
                STATUS_OK,
                hdr.correlation_id,
                count as u32,
                &out,
            )
        }
        Err(err) => write_slab_reply(
            socket,
            MSG_SLAB_RECOVER_BATCH_RESP,
            STATUS_ERROR,
            hdr.correlation_id,
            0,
            err.as_bytes(),
        ),
    }
}

fn handle_telemetry_req<P: VramProvider>(
    worker: &GpuCacheWorker<'_, P>,
    hdr: &FrameHeader,
    payload: &[u8],
    socket: &mut UnixStream,
) -> Result<(), String> {
    if !payload.is_empty() || worker.is_disabled() {
        return write_slab_reply(
            socket,
            MSG_SLAB_TELEMETRY_RESP,
            STATUS_ERROR,
            hdr.correlation_id,
            0,
            b"invalid slab telemetry request or disabled worker",
        );
    }
    let (status, telemetry) = match worker.slab_bridge() {
        Some(bridge) => (STATUS_OK, bridge.telemetry()),
        None => (
            STATUS_MISS,
            SlabBridgeTelemetry {
                backend: None,
                active_frames: None,
                logical_pages_cached: None,
                effective_density_ratio: None,
                predecode_crc_mismatches: None,
                corrupt_lz4_fallbacks: None,
                deadline_poison_events: None,
            },
        ),
    };
    let encoded = serde_json::to_vec(&telemetry).unwrap_or_default();
    write_slab_reply(
        socket,
        MSG_SLAB_TELEMETRY_RESP,
        status,
        hdr.correlation_id,
        0,
        &encoded,
    )
}

pub fn run_gpu_worker_loop_with_device_codec<P, F>(
    socket: UnixStream,
    provider: P,
    config: GpuWorkerConfig,
    make_codec: F,
) -> Result<(), String>
where
    P: VramProvider,
    F: for<'p> FnOnce(
        &'p P,
        u64,
        u32,
    )
        -> Result<Box<dyn ramshared_vram::device_codec::GpuDeviceCodec + 'p>, String>,
{
    let worker = GpuCacheWorker::new_with_device_codec(&provider, config, Some(make_codec));
    super::run_gpu_worker_loop_with_worker(socket, worker, super::WORKER_FRAME_READ_TIMEOUT)
}

pub(super) fn handle_slab_frame<P: VramProvider>(
    worker: &mut GpuCacheWorker<'_, P>,
    hdr: &FrameHeader,
    payload: &[u8],
    socket: &mut UnixStream,
) -> Result<bool, String> {
    match hdr.msg_type {
        MSG_SLAB_COMPRESS_BATCH_REQ => {
            handle_compress_req(worker, hdr, payload, socket)?;
            Ok(true)
        }
        MSG_SLAB_RECOVER_BATCH_REQ => {
            handle_recover_req(worker, hdr, payload, socket)?;
            Ok(true)
        }
        MSG_SLAB_TELEMETRY_REQ => {
            handle_telemetry_req(worker, hdr, payload, socket)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}
