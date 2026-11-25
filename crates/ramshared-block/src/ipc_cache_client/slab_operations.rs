// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/ipc_cache_client/slab_operations.rs
// Client-side IPC roundtrips for compressed VRAM slab batch operations and
// worker slab-bridge telemetry (`MSG_SLAB_*`).

use ramshared_vram::device_codec::{CodecError, PAGE_BYTES, PageDecodeStatus};
use ramshared_vram::slab_pool::VramSlabTicket;

use crate::gpu_cache_worker::{
    FRAME_HEADER_LEN, FrameHeader, MAX_IPC_PAYLOAD_BYTES, MSG_SLAB_COMPRESS_BATCH_REQ,
    MSG_SLAB_COMPRESS_BATCH_RESP, MSG_SLAB_RECOVER_BATCH_REQ, MSG_SLAB_RECOVER_BATCH_RESP,
    MSG_SLAB_TELEMETRY_REQ, MSG_SLAB_TELEMETRY_RESP, STATUS_ERROR, STATUS_MISS, STATUS_OK,
    decode_compress_batch_resp, decode_recover_batch_resp, encode_compress_batch_req,
    encode_recover_batch_req,
};
use crate::gpu_worker_slab_bridge::SlabBridgeTelemetry;
use crate::origin_cache::CacheState;

use super::{IpcCacheClient, deadline_after, read_exact_until, try_drain_pending, write_all_until};

fn map_worker_codec_error(payload: &[u8]) -> CodecError {
    let text = String::from_utf8_lossy(payload).into_owned();
    if text.contains("DeadlineExceeded") {
        CodecError::DeadlineExceeded
    } else if text.contains("InvalidBatchLength") {
        CodecError::InvalidBatchLength
    } else {
        CodecError::BackendFailure(text)
    }
}

impl IpcCacheClient {
    fn roundtrip_slab(
        &mut self,
        req_type: u8,
        expected_resp_type: u8,
        aux: u32,
        req_payload: &[u8],
    ) -> Result<(FrameHeader, Vec<u8>), String> {
        if self.state != CacheState::Active {
            return Err("ipc cache client is not active".into());
        }
        match self.flush_deferred_invalidations() {
            Ok(true) => {}
            Ok(false) => return Err("ipc cache client invalidations pending".into()),
            Err(()) => return Err("ipc cache client invalidation failed".into()),
        }
        match try_drain_pending(&mut self.socket, &mut self.pending_frame) {
            Ok(true) => {}
            Ok(false) => {
                return Err("ipc cache client mutation stream busy".into());
            }
            Err(_) => {
                self.fail("slab rpc mutation drain failed");
                return Err("mutation drain failed".into());
            }
        }
        let deadline = deadline_after(self.read_timeout).map_err(|e| {
            self.fail("slab rpc deadline setup failed");
            e.to_string()
        })?;
        self.seq = self.seq.saturating_add(1);
        let req = FrameHeader {
            msg_type: req_type,
            status: STATUS_OK,
            correlation_id: self.seq,
            offset: 0,
            payload_len: req_payload.len() as u32,
            aux,
        };
        if write_all_until(&mut self.socket, &req.encode(), deadline).is_err()
            || (!req_payload.is_empty()
                && write_all_until(&mut self.socket, req_payload, deadline).is_err())
        {
            self.fail("slab rpc write failed");
            return Err("slab rpc write failed".into());
        }

        let mut hdr_buf = [0u8; FRAME_HEADER_LEN];
        if read_exact_until(&mut self.socket, &mut hdr_buf, deadline).is_err() {
            self.fail("slab rpc response timed out");
            return Err("slab rpc response timed out".into());
        }
        let Some(resp) = FrameHeader::decode(&hdr_buf) else {
            self.fail("slab rpc header malformed");
            return Err("slab rpc header malformed".into());
        };
        if resp.msg_type != expected_resp_type || resp.correlation_id != self.seq {
            self.fail("slab rpc identity mismatched");
            return Err("slab rpc identity mismatched".into());
        }
        if resp.payload_len as usize > MAX_IPC_PAYLOAD_BYTES {
            self.fail("slab rpc payload exceeded limit");
            return Err("slab rpc payload exceeded limit".into());
        }
        let mut resp_payload = vec![0u8; resp.payload_len as usize];
        if !resp_payload.is_empty()
            && read_exact_until(&mut self.socket, &mut resp_payload, deadline).is_err()
        {
            self.fail("slab rpc payload truncated");
            return Err("slab rpc payload truncated".into());
        }
        self.clear_unacked_mutation_window();
        Ok((resp, resp_payload))
    }

    /// Requests batch compression inside the isolated GPU worker process and
    /// returns the resident VRAM slab tickets assigned to each page.
    pub fn slab_compress_batch(
        &mut self,
        pages: &[[u8; PAGE_BYTES]],
    ) -> Result<Vec<Option<VramSlabTicket>>, CodecError> {
        let (aux, payload) = encode_compress_batch_req(pages)?;
        let (resp, resp_payload) = self
            .roundtrip_slab(
                MSG_SLAB_COMPRESS_BATCH_REQ,
                MSG_SLAB_COMPRESS_BATCH_RESP,
                aux,
                &payload,
            )
            .map_err(CodecError::BackendFailure)?;
        if resp.status == STATUS_ERROR {
            return Err(map_worker_codec_error(&resp_payload));
        }
        if resp.status != STATUS_OK || resp.aux as usize != pages.len() {
            self.fail("slab compress batch response invalid");
            return Err(CodecError::BackendFailure(
                "slab compress batch response invalid".into(),
            ));
        }
        decode_compress_batch_resp(pages.len(), &resp_payload).ok_or_else(|| {
            self.fail("slab compress batch payload malformed");
            CodecError::BackendFailure("slab compress batch payload malformed".into())
        })
    }

    /// Verifies and unpacks `tickets` inside the isolated GPU worker process.
    /// Per-page decode outcomes are reported in the returned status vector.
    pub fn slab_recover_batch(
        &mut self,
        tickets: &[VramSlabTicket],
    ) -> Result<(Vec<PageDecodeStatus>, Vec<[u8; PAGE_BYTES]>), CodecError> {
        let (aux, payload) = encode_recover_batch_req(tickets)?;
        let (resp, resp_payload) = self
            .roundtrip_slab(
                MSG_SLAB_RECOVER_BATCH_REQ,
                MSG_SLAB_RECOVER_BATCH_RESP,
                aux,
                &payload,
            )
            .map_err(CodecError::BackendFailure)?;
        if resp.status == STATUS_ERROR {
            return Err(map_worker_codec_error(&resp_payload));
        }
        if resp.status != STATUS_OK || resp.aux as usize != tickets.len() {
            self.fail("slab recover batch response invalid");
            return Err(CodecError::BackendFailure(
                "slab recover batch response invalid".into(),
            ));
        }
        decode_recover_batch_resp(tickets.len(), &resp_payload).ok_or_else(|| {
            self.fail("slab recover batch payload malformed");
            CodecError::BackendFailure("slab recover batch payload malformed".into())
        })
    }

    /// Queries the worker's compressed VRAM slab bridge telemetry.
    pub fn slab_telemetry(&mut self) -> Result<Option<SlabBridgeTelemetry>, String> {
        let (resp, resp_payload) =
            self.roundtrip_slab(MSG_SLAB_TELEMETRY_REQ, MSG_SLAB_TELEMETRY_RESP, 0, &[])?;
        match resp.status {
            STATUS_OK => {
                let telemetry: SlabBridgeTelemetry = serde_json::from_slice(&resp_payload)
                    .map_err(|e| {
                        self.fail("slab telemetry JSON malformed");
                        e.to_string()
                    })?;
                Ok(Some(telemetry))
            }
            STATUS_MISS => Ok(None),
            STATUS_ERROR => Err(String::from_utf8_lossy(&resp_payload).into_owned()),
            _ => {
                self.fail("slab telemetry status invalid");
                Err("slab telemetry status invalid".into())
            }
        }
    }
}
