// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/gpu_worker_slab_bridge.rs
// Bridge between the isolated GPU worker and the VRAM slab codec
// (SPEC gpu-vram-hardware-compression ITEM-4, RF-6/RF-8, NFR-2/NFR-5).

#[cfg(any(unix, test))]
use ramshared_vram::GpuCodecActivity;
use ramshared_vram::device_codec::{CodecError, GpuDeviceCodec, PAGE_BYTES, PageDecodeStatus};
use ramshared_vram::slab_pool::{SlotEncoding, VramSlabPool, VramSlabTicket};
use serde::{Deserialize, Serialize};
use std::time::Instant;

pub const BRIDGE_STAGING_BYTES: u64 = 776 * 1024;
pub const BRIDGE_FRAME_BYTES: u64 = 65_536;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SlabBridgeConfig {
    pub staging_bytes: u64,
    pub frame_bytes: u64,
}

impl SlabBridgeConfig {
    pub const fn sealed() -> Self {
        Self {
            staging_bytes: BRIDGE_STAGING_BYTES,
            frame_bytes: BRIDGE_FRAME_BYTES,
        }
    }

    /// Subtract the reserve floor, the staging arena and one frame before the
    /// slab pool computes its frame count. A budget that cannot hold both is
    /// refused, never rounded down into a silent under-allocation.
    pub fn frame_count(
        &self,
        available_bytes: u64,
        reserve_floor_bytes: u64,
    ) -> Result<u32, String> {
        if self.frame_bytes == 0 {
            return Err("frame size must be nonzero".to_string());
        }
        let required = reserve_floor_bytes
            .checked_add(self.staging_bytes)
            .and_then(|bytes| bytes.checked_add(self.frame_bytes))
            .ok_or_else(|| "vram budget overflow".to_string())?;
        let usable = available_bytes
            .checked_sub(required)
            .ok_or_else(|| "insufficient vram for staging arena and one frame".to_string())?;
        u32::try_from(usable / self.frame_bytes)
            .map_err(|_| "vram frame count exceeds supported range".to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SlabBridgeTelemetry {
    pub backend: Option<String>,
    pub active_frames: Option<u64>,
    pub logical_pages_cached: Option<u64>,
    pub effective_density_ratio: Option<f64>,
    pub predecode_crc_mismatches: Option<u64>,
    pub corrupt_lz4_fallbacks: Option<u64>,
    pub deadline_poison_events: Option<u64>,
}

#[derive(Default)]
struct GpuCodecCompletions {
    compression: Option<Instant>,
    decompression: Option<Instant>,
}

pub struct GpuWorkerSlabBridge<'a> {
    pool: VramSlabPool,
    codec: Option<Box<dyn GpuDeviceCodec + 'a>>,
    crc_mismatches: u64,
    corrupt_lz4: u64,
    deadline_poison: u64,
    completions: Option<GpuCodecCompletions>,
}

impl<'a> GpuWorkerSlabBridge<'a> {
    pub fn new(pool: VramSlabPool, codec: Option<Box<dyn GpuDeviceCodec + 'a>>) -> Self {
        let completions = codec
            .as_ref()
            .filter(|codec| codec.uses_gpu_lz4())
            .map(|_| GpuCodecCompletions::default());
        Self {
            pool,
            codec,
            completions,
            crc_mismatches: 0,
            corrupt_lz4: 0,
            deadline_poison: 0,
        }
    }

    pub fn pool(&self) -> &VramSlabPool {
        &self.pool
    }

    pub fn begin_eviction_observation(&mut self) {
        self.pool.begin_eviction_observation();
    }

    pub fn finish_eviction_observation(&mut self) -> Vec<u32> {
        self.pool.finish_eviction_observation()
    }

    /// Physical device allocations, or unknown when the codec cannot report them.
    pub fn device_allocation_bytes(&self) -> Option<(u64, u64)> {
        self.codec.as_ref()?.device_allocation_bytes()
    }

    pub fn compress_batch(
        &mut self,
        pages: &[[u8; PAGE_BYTES]],
        out_tickets: &mut [Option<VramSlabTicket>],
    ) -> Result<(), CodecError> {
        let codec = self
            .codec
            .as_mut()
            .ok_or_else(|| CodecError::BackendFailure("gpu codec is disabled".to_string()))?;
        codec.compress_and_pack_batch(pages, &mut self.pool, out_tickets)?;
        if let Some(completions) = &mut self.completions
            && out_tickets
                .iter()
                .take(pages.len())
                .flatten()
                .any(|ticket| ticket.encoding == SlotEncoding::Lz4)
        {
            completions.compression = Some(Instant::now());
        }
        Ok(())
    }

    /// Verify and unpack a batch. A page that fails CRC or LZ4 decoding is
    /// transparently replaced from the authoritative origin; valid siblings
    /// remain VRAM hits. The status vector keeps the refusal reason so the
    /// caller can invalidate exactly the failed tickets.
    pub fn recover_batch<F>(
        &mut self,
        tickets: &[VramSlabTicket],
        out_pages: &mut [[u8; PAGE_BYTES]],
        out_status: &mut [PageDecodeStatus],
        mut read_origin: F,
    ) -> Result<(), CodecError>
    where
        F: FnMut(usize) -> Result<[u8; PAGE_BYTES], String>,
    {
        let codec = self
            .codec
            .as_mut()
            .ok_or_else(|| CodecError::BackendFailure("gpu codec is disabled".to_string()))?;
        match codec.verify_and_unpack_batch(tickets, out_pages, out_status) {
            Ok(()) => {}
            Err(CodecError::DeadlineExceeded) => {
                self.deadline_poison = self.deadline_poison.saturating_add(1);
                return Err(CodecError::DeadlineExceeded);
            }
            Err(error) => return Err(error),
        }
        if let Some(completions) = &mut self.completions
            && tickets
                .iter()
                .zip(out_status.iter())
                .any(|(ticket, status)| {
                    ticket.encoding == SlotEncoding::Lz4 && *status == PageDecodeStatus::Ok
                })
        {
            completions.decompression = Some(Instant::now());
        }
        for (index, status) in out_status.iter().enumerate() {
            match status {
                PageDecodeStatus::Ok => {
                    let _ = self.pool.lookup_and_touch(&tickets[index]);
                }
                PageDecodeStatus::PreDecodeCrcMismatch => {
                    self.invalidate_ticket(&tickets[index]);
                    self.crc_mismatches = self.crc_mismatches.saturating_add(1);
                    out_pages[index] = read_origin(index).map_err(CodecError::BackendFailure)?;
                }
                PageDecodeStatus::CorruptLz4Stream => {
                    self.invalidate_ticket(&tickets[index]);
                    self.corrupt_lz4 = self.corrupt_lz4.saturating_add(1);
                    out_pages[index] = read_origin(index).map_err(CodecError::BackendFailure)?;
                }
                PageDecodeStatus::InvalidTicket => {
                    self.invalidate_ticket(&tickets[index]);
                    out_pages[index] = read_origin(index).map_err(CodecError::BackendFailure)?;
                }
                PageDecodeStatus::SkippedOrIncompressible => {
                    out_pages[index] = read_origin(index).map_err(CodecError::BackendFailure)?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn invalidate_ticket(&mut self, ticket: &VramSlabTicket) {
        let _ = self.pool.invalidate(ticket);
        if let Some(codec) = self.codec.as_mut() {
            codec.invalidate(ticket);
        }
    }

    /// Evict the oldest active slab frame so caller-owned host metadata can reclaim.
    pub fn evict_coldest_frame(&mut self) -> Option<u32> {
        self.pool.evict_lru_frame()
    }

    /// Remove a pruned frame slot from the backend codec index without touching `pool`.
    #[cfg(any(unix, test))]
    pub(crate) fn invalidate_codec_ticket(&mut self, ticket: &VramSlabTicket) {
        if let Some(codec) = self.codec.as_mut() {
            codec.invalidate(ticket);
        }
    }

    #[cfg(any(unix, test))]
    pub(crate) fn gpu_codec_activity(&self) -> Option<GpuCodecActivity> {
        let completions = self.completions.as_ref()?;
        let now = Instant::now();
        let age_ms = |at: Instant| u64::try_from(now.checked_duration_since(at)?.as_millis()).ok();
        Some(GpuCodecActivity {
            compression_age_ms: completions.compression.and_then(age_ms),
            decompression_age_ms: completions.decompression.and_then(age_ms),
        })
    }

    pub fn telemetry(&self) -> SlabBridgeTelemetry {
        match self.codec.as_ref() {
            None => SlabBridgeTelemetry {
                backend: None,
                active_frames: None,
                logical_pages_cached: None,
                effective_density_ratio: None,
                predecode_crc_mismatches: None,
                corrupt_lz4_fallbacks: None,
                deadline_poison_events: None,
            },
            Some(codec) => SlabBridgeTelemetry {
                backend: Some(codec.backend_name().to_string()),
                active_frames: Some(self.pool.active_frames()),
                logical_pages_cached: Some(self.pool.logical_pages_cached()),
                effective_density_ratio: self.pool.effective_density_ratio(),
                predecode_crc_mismatches: Some(self.crc_mismatches),
                corrupt_lz4_fallbacks: Some(self.corrupt_lz4),
                deadline_poison_events: Some(self.deadline_poison),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use ramshared_vram::device_codec::ReferenceLz4Codec;

    fn page(seed: u8) -> [u8; PAGE_BYTES] {
        let mut page = [0u8; PAGE_BYTES];
        for (index, byte) in page.iter_mut().enumerate() {
            *byte = if index % 64 < 32 {
                seed
            } else {
                (index / 64) as u8
            };
        }
        page
    }

    #[test]
    fn tracks_successful_lz4_work_independently() {
        let mut codec = ReferenceLz4Codec::new();
        codec.set_deadline(std::time::Duration::from_secs(30));
        let mut bridge =
            GpuWorkerSlabBridge::new(VramSlabPool::new(1).unwrap(), Some(Box::new(codec)));
        assert!(bridge.gpu_codec_activity().is_none());
        // The fixture executes the same ticket contract with a CPU codec.
        bridge.completions = Some(GpuCodecCompletions::default());
        assert_eq!(
            bridge.gpu_codec_activity().unwrap().compression_age_ms,
            None
        );
        let mut tickets = [None];
        bridge.compress_batch(&[page(63)], &mut tickets).unwrap();
        let ticket = tickets[0].unwrap();
        assert_eq!(ticket.encoding, SlotEncoding::Lz4);
        let activity = bridge.gpu_codec_activity().unwrap();
        assert!(activity.compression_age_ms.is_some());
        assert_eq!(activity.decompression_age_ms, None);

        let mut out = [[0u8; PAGE_BYTES]];
        let mut status = [PageDecodeStatus::InvalidTicket];
        bridge
            .recover_batch(&[ticket], &mut out, &mut status, |_| {
                panic!("unexpected fallback")
            })
            .unwrap();
        assert_eq!(out, [page(63)]);
        assert!(
            bridge
                .gpu_codec_activity()
                .unwrap()
                .decompression_age_ms
                .is_some()
        );

        bridge.completions.as_mut().unwrap().decompression = None;
        bridge.invalidate_ticket(&ticket);
        bridge
            .recover_batch(&[ticket], &mut out, &mut status, |_| Ok(page(9)))
            .unwrap();
        assert_eq!(out, [page(9)]);
        assert_eq!(
            bridge.gpu_codec_activity().unwrap().decompression_age_ms,
            None
        );
    }

    #[test]
    fn unmeasured_device_allocations_are_unknown() {
        let bridge = GpuWorkerSlabBridge::new(
            VramSlabPool::new(1).unwrap(),
            Some(Box::new(ReferenceLz4Codec::new())),
        );
        assert_eq!(bridge.device_allocation_bytes(), None);
    }

    #[test]
    fn worker_partial_batch_crc_mismatch_recovers_single_page_from_origin() {
        let pages: Vec<[u8; PAGE_BYTES]> = (0..64).map(|index| page(index as u8)).collect();
        let mut pool = VramSlabPool::new(64).unwrap();
        let mut codec = ReferenceLz4Codec::new();
        codec.set_deadline(std::time::Duration::from_secs(30));
        let mut tickets = vec![None; pages.len()];
        codec
            .compress_and_pack_batch(&pages, &mut pool, &mut tickets)
            .unwrap();
        let corrupt = tickets[17].unwrap();
        codec.payload_mut(&corrupt).unwrap()[0] ^= 0x40;
        let mut bridge = GpuWorkerSlabBridge::new(pool, Some(Box::new(codec)));
        let mut decoded = vec![[0u8; PAGE_BYTES]; pages.len()];
        let mut status = vec![PageDecodeStatus::Ok; pages.len()];
        let mut origin_reads = 0;
        bridge
            .recover_batch(
                &tickets
                    .iter()
                    .map(|ticket| ticket.unwrap())
                    .collect::<Vec<_>>(),
                &mut decoded,
                &mut status,
                |_| {
                    origin_reads += 1;
                    Ok([0xEE; PAGE_BYTES])
                },
            )
            .unwrap();
        assert_eq!(origin_reads, 1);
        assert_eq!(status[17], PageDecodeStatus::PreDecodeCrcMismatch);
        assert!(bridge.pool().lookup(&corrupt).is_err());
        assert_eq!(bridge.pool().logical_pages_cached(), 63);
        assert_eq!(decoded[17], [0xEE; PAGE_BYTES]);
        for index in 0..pages.len() {
            if index != 17 {
                assert_eq!(decoded[index], pages[index]);
                assert_eq!(status[index], PageDecodeStatus::Ok);
            }
        }
        assert_eq!(bridge.telemetry().predecode_crc_mismatches, Some(1));
    }

    enum MockFailure {
        None,
        Deadline,
        Other,
    }

    struct StatusCodec {
        status: PageDecodeStatus,
        failure: MockFailure,
    }

    impl GpuDeviceCodec for StatusCodec {
        fn backend_name(&self) -> &'static str {
            "mock"
        }

        fn compress_and_pack_batch(
            &mut self,
            _pages: &[[u8; PAGE_BYTES]],
            _slab_alloc: &mut VramSlabPool,
            _out_tickets: &mut [Option<VramSlabTicket>],
        ) -> Result<(), CodecError> {
            Ok(())
        }

        fn verify_and_unpack_batch(
            &mut self,
            _tickets: &[VramSlabTicket],
            _out_pages: &mut [[u8; PAGE_BYTES]],
            out_status: &mut [PageDecodeStatus],
        ) -> Result<(), CodecError> {
            match self.failure {
                MockFailure::None => {}
                MockFailure::Deadline => return Err(CodecError::DeadlineExceeded),
                MockFailure::Other => return Err(CodecError::InvalidBatchLength),
            }
            for status in out_status {
                *status = self.status;
            }
            Ok(())
        }
    }

    fn one_ticket(pool: &mut VramSlabPool) -> VramSlabTicket {
        pool.alloc_sub_slot(ramshared_vram::slab_pool::SlabClass::B512)
            .unwrap()
    }

    #[test]
    fn worker_slab_bridge_refuses_disabled_and_falls_back_each_status() {
        let pool = VramSlabPool::new(1).unwrap();
        let mut disabled = GpuWorkerSlabBridge::new(pool, None);
        assert_eq!(disabled.pool().frame_count(), 1);
        assert!(disabled.compress_batch(&[], &mut []).is_err());
        assert!(
            disabled
                .recover_batch(&[], &mut [], &mut [], |_| Ok([0u8; PAGE_BYTES]))
                .is_err()
        );

        for (status, failure) in [
            (PageDecodeStatus::CorruptLz4Stream, MockFailure::None),
            (PageDecodeStatus::SkippedOrIncompressible, MockFailure::None),
            (PageDecodeStatus::Ok, MockFailure::Deadline),
            (PageDecodeStatus::Ok, MockFailure::Other),
        ] {
            let mut pool = VramSlabPool::new(1).unwrap();
            let ticket = one_ticket(&mut pool);
            let mut bridge =
                GpuWorkerSlabBridge::new(pool, Some(Box::new(StatusCodec { status, failure })));
            let mut pages = [[0u8; PAGE_BYTES]; 1];
            let mut statuses = [PageDecodeStatus::Ok; 1];
            let result = bridge.recover_batch(&[ticket], &mut pages, &mut statuses, |_| {
                Ok([0xEE; PAGE_BYTES])
            });
            match status {
                PageDecodeStatus::Ok => assert!(result.is_err()),
                _ => {
                    result.unwrap();
                    assert_eq!(pages[0], [0xEE; PAGE_BYTES]);
                }
            }
        }
    }

    #[test]
    fn worker_reserve_floor_preaccounts_staging_arena() {
        let config = SlabBridgeConfig::sealed();
        assert!(config.frame_count(BRIDGE_STAGING_BYTES, 0).is_err());
        assert_eq!(
            config
                .frame_count(BRIDGE_STAGING_BYTES + BRIDGE_FRAME_BYTES * 3, 0)
                .unwrap(),
            2
        );
        assert_eq!(
            config
                .frame_count(
                    BRIDGE_STAGING_BYTES + BRIDGE_FRAME_BYTES * 5,
                    BRIDGE_FRAME_BYTES
                )
                .unwrap(),
            3
        );
        assert!(config.frame_count(u64::MAX, u64::MAX).is_err());
        for gib in [6u64, 32, 96, 192, 256] {
            let bytes = gib * 1024 * 1024 * 1024;
            let expected = (bytes - BRIDGE_STAGING_BYTES - BRIDGE_FRAME_BYTES) / BRIDGE_FRAME_BYTES;
            assert_eq!(u64::from(config.frame_count(bytes, 0).unwrap()), expected);
        }
        assert!(config.frame_count(u64::MAX, 0).is_err());
        assert!(
            SlabBridgeConfig {
                staging_bytes: 0,
                frame_bytes: 0
            }
            .frame_count(1, 0)
            .is_err()
        );
    }

    #[test]
    fn worker_telemetry_emits_some_when_active_and_none_when_disabled() {
        let disabled = GpuWorkerSlabBridge::new(VramSlabPool::new(1).unwrap(), None);
        let value = serde_json::to_value(disabled.telemetry()).unwrap();
        assert!(value["backend"].is_null());
        assert!(value["active_frames"].is_null());
        assert!(value["effective_density_ratio"].is_null());

        let active = GpuWorkerSlabBridge::new(
            VramSlabPool::new(1).unwrap(),
            Some(Box::new(ReferenceLz4Codec::new())),
        );
        let telemetry = active.telemetry();
        assert_eq!(telemetry.backend.as_deref(), Some("reference-lz4"));
        assert_eq!(telemetry.active_frames, Some(0));
        assert_eq!(telemetry.logical_pages_cached, Some(0));
        assert_eq!(telemetry.predecode_crc_mismatches, Some(0));
    }
}
