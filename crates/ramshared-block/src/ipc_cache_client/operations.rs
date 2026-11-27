// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/ipc_cache_client/operations.rs
//! Absolute deadlines for cache reads, telemetry and release on one IPC stream.
use super::*;
use crate::BoundedCache;

fn capped_deadline(deadline: Instant, local_timeout: Duration) -> io::Result<Instant> {
    Ok(deadline.min(deadline_after(local_timeout)?))
}

pub(super) fn read_response_failure(kind: ErrorKind) -> &'static str {
    match kind {
        // On Linux, SO_RCVTIMEO expiry on a blocking socket is WouldBlock.
        ErrorKind::TimedOut | ErrorKind::WouldBlock => "timed out",
        ErrorKind::UnexpectedEof
        | ErrorKind::ConnectionReset
        | ErrorKind::ConnectionAborted
        | ErrorKind::BrokenPipe => "peer disconnected",
        _ => "I/O failed",
    }
}

impl IpcCacheClient {
    fn fail_read_response(&mut self, part: &str, error: &io::Error) {
        let failure = read_response_failure(error.kind());
        self.fail(format_args!("read response {part} {failure}: {error}"));
    }
}

impl BoundedCache for IpcCacheClient {
    fn read_until(&mut self, offset: u64, destination: &mut [u8], deadline: Instant) -> CacheRead {
        if Instant::now() >= deadline {
            return CacheRead::Miss;
        }
        if self.state != CacheState::Active {
            return CacheRead::Miss;
        }
        if destination.len() > MAX_IPC_PAYLOAD_BYTES {
            return CacheRead::Miss;
        }
        match self.flush_deferred_invalidations() {
            Ok(true) => {}
            Ok(false) => return CacheRead::Miss,
            Err(()) => return CacheRead::Failed,
        }
        // A read issued while a mutation frame is still draining would be
        // concatenated into that incomplete frame and corrupt both (DT-10).
        // Origin is authoritative, so a miss is the correct answer and the
        // cache stays alive for the next quiet window.
        match try_drain_pending(&mut self.socket, &mut self.pending_frame) {
            Ok(true) => {}
            Ok(false) => return CacheRead::Miss,
            Err(_) => {
                self.fail("mutation frame drain failed");
                return CacheRead::Failed;
            }
        }
        // A read queued behind a write-mirror burst waits for the whole
        // backlog and would miss NFR-1's 50 ms bound, which fail-closes the
        // cache. Origin is authoritative, so a deferred read is a miss — the
        // correct answer — and the cache stays alive for the next quiet
        // window. Promotes from the read path itself never set this gate.
        if self.write_mirrors_are_draining() {
            return CacheRead::Miss;
        }
        self.seq = self.seq.saturating_add(1);
        let req = FrameHeader {
            msg_type: MSG_READ_REQ,
            status: STATUS_OK,
            correlation_id: self.seq,
            offset,
            payload_len: 0,
            aux: destination.len() as u32,
        };

        let deadline = match capped_deadline(deadline, self.read_timeout) {
            Ok(deadline) => deadline,
            Err(_) => {
                self.fail("read deadline setup failed");
                return CacheRead::Failed;
            }
        };
        if write_all_until(&mut self.socket, &req.encode(), deadline).is_err() {
            self.fail("read request write failed");
            return CacheRead::Failed;
        }

        let mut hdr_buf = [0u8; FRAME_HEADER_LEN];
        if let Err(error) = read_exact_until(&mut self.socket, &mut hdr_buf, deadline) {
            self.fail_read_response("header", &error);
            return CacheRead::Failed;
        }

        let Some(resp) = FrameHeader::decode(&hdr_buf) else {
            self.fail("read response header malformed");
            return CacheRead::Failed;
        };
        // AUD-34: a read response answers *this* read. It echoes the requested
        // offset — a frame for a different offset is an answer to a question
        // that was not asked, and its bytes are not this range.
        if resp.msg_type != MSG_READ_RESP
            || resp.correlation_id != self.seq
            || resp.offset != offset
        {
            self.fail("read response identity mismatched");
            return CacheRead::Failed;
        }
        // Sync authoritative cached_bytes from the worker (aux carries KiB).
        self.clear_unacked_mutation_window();
        self.observe_cached_kib(resp.aux);

        match resp.status {
            STATUS_OK => {
                if resp.payload_len as usize != destination.len() {
                    self.fail("read response payload length mismatched");
                    return CacheRead::Failed;
                }
                if let Err(error) = read_exact_until(&mut self.socket, destination, deadline) {
                    self.fail_read_response("payload", &error);
                    return CacheRead::Failed;
                }
                CacheRead::Hit
            }
            STATUS_MISS => {
                // A MISS is empty by contract. A payload here would stay on
                // the stream and be concatenated into the next frame; the
                // length cannot be trusted well enough to drain it, so the
                // session is revoked instead of resynchronising.
                if resp.payload_len != 0 {
                    self.fail("read miss carried a payload");
                    return CacheRead::Failed;
                }
                CacheRead::Miss
            }
            _ => {
                self.fail("read response status failed");
                CacheRead::Failed
            }
        }
    }

    fn disable_until(&mut self, deadline: Instant) -> CacheMutation {
        if matches!(self.state, CacheState::Off | CacheState::Unavailable) {
            return CacheMutation::Skipped;
        }
        // A failed disable write leaves `pending_frame` holding bytes that
        // were already (partly) transmitted. Re-sending that buffer would
        // replay them from byte zero and desynchronise the stream, so `Stuck`
        // is terminal for this socket: no second disable is ever attempted
        // (AUD-34).
        if self.state == CacheState::Stuck {
            return CacheMutation::Failed;
        }
        if Instant::now() >= deadline {
            self.clear_held_telemetry();
            self.state = CacheState::Stuck;
            return CacheMutation::Failed;
        }
        // Every path below leaves `Active`, and `refresh_cached_bytes` never
        // heartbeats again once the cache is not `Active`. The held samples
        // would otherwise freeze at their last live value and be republished
        // on every status write for the rest of the daemon's life.
        self.clear_held_telemetry();
        self.seq = self.seq.saturating_add(1);
        let req = FrameHeader {
            msg_type: MSG_DISABLE_REQ,
            status: STATUS_OK,
            correlation_id: self.seq,
            offset: 0,
            payload_len: 0,
            aux: 0,
        };

        let deadline = match capped_deadline(deadline, DISABLE_TIMEOUT) {
            Ok(deadline) => deadline,
            Err(_) => {
                self.state = CacheState::Stuck;
                return CacheMutation::Failed;
            }
        };
        // Teardown must observe mutation order: append the disable frame behind
        // anything still queued and flush the whole backlog (DT-10).
        self.pending_frame.extend_from_slice(&req.encode());
        if write_all_until(&mut self.socket, &self.pending_frame, deadline).is_err() {
            self.state = CacheState::Stuck;
            return CacheMutation::Failed;
        }
        self.pending_frame.clear();

        let mut hdr_buf = [0u8; FRAME_HEADER_LEN];
        if read_exact_until(&mut self.socket, &mut hdr_buf, deadline).is_err() {
            self.state = CacheState::Stuck;
            return CacheMutation::Failed;
        }

        let Some(resp) = FrameHeader::decode(&hdr_buf) else {
            self.state = CacheState::Stuck;
            return CacheMutation::Failed;
        };
        // AUD-34: DISABLE acknowledges exactly one operation and the worker
        // answers with the empty/zeroed contract. An acknowledgement that does
        // not match this request — wrong correlation, a carried offset, a
        // payload, or occupancy — is not a release of this resource, and must
        // never be reported as one.
        if resp.msg_type == MSG_DISABLE_RESP
            && resp.status == STATUS_OK
            && resp.correlation_id == self.seq
            && resp.offset == 0
            && resp.payload_len == 0
            && resp.aux == 0
        {
            self.state = CacheState::Off;
            self.deferred_invalidations.clear();
            self.cached_bytes = 0;
            CacheMutation::Accepted
        } else {
            self.state = CacheState::Stuck;
            CacheMutation::Failed
        }
    }

    fn refresh_until(&mut self, deadline: Instant) -> Result<u64, &'static str> {
        if self.state != CacheState::Active {
            return Err("GPU cache worker is unavailable");
        }
        if Instant::now() >= deadline {
            return Err("GPU cache worker shared refresh deadline expired");
        }
        match self.flush_deferred_invalidations() {
            Ok(true) => {}
            Ok(false) => return self.observed_cached_bytes(),
            Err(()) => return Err("GPU cache deferred invalidation failed"),
        }
        // A heartbeat request written mid-frame would corrupt the stream
        // (DT-10), and telemetry is optional: skip the round-trip and report
        // the last confirmed sample while the queue drains.
        match try_drain_pending(&mut self.socket, &mut self.pending_frame) {
            Ok(true) => {}
            Ok(false) => return self.observed_cached_bytes(),
            Err(_) => {
                self.fail("mutation frame drain failed");
                return Err("GPU cache worker mutation backlog could not drain");
            }
        }
        // Telemetry is optional and must never sit in a mutation backlog
        // (DT-1). Skip the round-trip and report the last confirmed sample:
        // occupancy only moves when the worker answers, and a skipped beat is
        // cheaper than the fail-closed revocation a 50 ms timeout used to
        // trigger under write load.
        let heartbeat_overdue = self
            .last_heartbeat_at
            .is_some_and(|at| at.elapsed() >= self.pacing.min_heartbeat_interval.saturating_mul(2));
        if !self.heartbeat_is_due()
            || (self.mutations_are_draining()
                && (!heartbeat_overdue || self.write_mirrors_are_draining()))
        {
            return self.observed_cached_bytes();
        }
        let deadline = match capped_deadline(deadline, self.read_timeout) {
            Ok(deadline) => deadline,
            Err(_) => {
                self.fail("heartbeat deadline setup failed");
                return Err("GPU cache worker heartbeat deadline could not be configured");
            }
        };
        self.seq = self.seq.saturating_add(1);
        let req = FrameHeader {
            msg_type: MSG_HEARTBEAT_REQ,
            status: STATUS_OK,
            correlation_id: self.seq,
            offset: 0,
            payload_len: 0,
            aux: 0,
        };
        let mut buf = [0u8; FRAME_HEADER_LEN];
        if write_all_until(&mut self.socket, &req.encode(), deadline).is_err()
            || read_exact_until(&mut self.socket, &mut buf, deadline).is_err()
        {
            // Fail closed, deliberately. This is one ordered frame stream
            // (DT-10): a partial heartbeat write leaves the stream mid-frame,
            // and a late response would be read as the next message. Skipping
            // the beat would desynchronise the protocol; tearing the cache
            // down is the only outcome that cannot serve stale bytes.
            self.fail("heartbeat I/O failed");
            return Err("GPU cache worker heartbeat timed out");
        }
        let Some(resp) = FrameHeader::decode(&buf) else {
            self.fail("heartbeat response header malformed");
            return Err("GPU cache worker heartbeat header malformed");
        };
        // A well-formed refusal is not a protocol identity mismatch. Keep
        // failing closed, but preserve the distinction for recovery diagnosis.
        if resp.msg_type == MSG_HEARTBEAT_RESP
            && resp.correlation_id == self.seq
            && resp.status == STATUS_ERROR
            && resp.offset == self.target_bytes
            && resp.payload_len == 0
        {
            self.fail("heartbeat worker refused the request");
            return Err("GPU cache worker heartbeat refused");
        }
        if resp.msg_type != MSG_HEARTBEAT_RESP
            || resp.correlation_id != self.seq
            || resp.status != STATUS_OK
            || resp.offset != self.target_bytes
        {
            self.fail("heartbeat response mismatched");
            return Err("GPU cache worker heartbeat mismatched");
        }
        // Physical occupancy stays in the frame header fields and is never
        // re-derived from the envelope (DT-8).
        self.clear_unacked_mutation_window();
        self.observe_cached_kib(resp.aux);
        if resp.payload_len > MAX_GPU_BUDGET_PAYLOAD_BYTES {
            self.fail("heartbeat worker telemetry exceeded its size limit");
            return Err("GPU cache worker heartbeat telemetry exceeded its limit");
        }
        if resp.payload_len == 0 {
            self.clear_held_telemetry();
        } else {
            let mut payload = vec![0; resp.payload_len as usize];
            if read_exact_until(&mut self.socket, &mut payload, deadline).is_err() {
                self.fail("heartbeat worker telemetry was truncated");
                return Err("GPU cache worker heartbeat telemetry was truncated");
            }
            // Unknown envelope versions and malformed samples are **omitted**,
            // not accepted. An identity change from a previously bound worker
            // is a separate ownership failure and revokes the client.
            match WorkerTelemetryEnvelope::from_bounded_payload(&payload) {
                Some(envelope) => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .ok()
                        .and_then(|time| u64::try_from(time.as_millis()).ok());
                    if let Err(reason) = self.adapter_binding.observe(envelope.budget.as_ref(), now)
                    {
                        self.fail(reason);
                        return Err(reason);
                    }
                    self.gpu_budget = envelope
                        .budget
                        .filter(|telemetry| telemetry.schema_version == 1);
                    self.gpu_utilization = envelope.utilization;
                    self.cache_telemetry = envelope.cache;
                }
                None => self.clear_held_telemetry(),
            }
        }
        self.last_heartbeat_at = Some(Instant::now());
        self.observed_cached_bytes()
    }
}
