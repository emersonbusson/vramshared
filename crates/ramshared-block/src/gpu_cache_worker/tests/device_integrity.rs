// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/gpu_cache_worker/tests/device_integrity.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::super::*;
use super::codec_fixtures::*;
use ramshared_vram::device_codec::{CodecError, GpuDeviceCodec, PAGE_BYTES, PageDecodeStatus};
use ramshared_vram::slab_pool::{VramSlabPool, VramSlabTicket};

struct CorruptRead {
    codec: ramshared_vram::device_codec::ReferenceLz4Codec,
    status: PageDecodeStatus,
}
impl GpuDeviceCodec for CorruptRead {
    fn backend_name(&self) -> &'static str {
        "corrupt-test"
    }
    fn compress_and_pack_batch(
        &mut self,
        pages: &[[u8; PAGE_BYTES]],
        pool: &mut VramSlabPool,
        out: &mut [Option<VramSlabTicket>],
    ) -> Result<(), CodecError> {
        self.codec.compress_and_pack_batch(pages, pool, out)
    }
    fn verify_and_unpack_batch(
        &mut self,
        _: &[VramSlabTicket],
        _: &mut [[u8; PAGE_BYTES]],
        status: &mut [PageDecodeStatus],
    ) -> Result<(), CodecError> {
        status.fill(self.status);
        Ok(())
    }
}

#[test]
fn device_integrity_and_decode_faults_reach_worker_telemetry() {
    const MIB: u64 = 1024 * 1024;
    for (failure, integrity, decode) in [
        (PageDecodeStatus::PreDecodeCrcMismatch, 1, 0),
        (PageDecodeStatus::CorruptLz4Stream, 0, 1),
        (PageDecodeStatus::InvalidTicket, 0, 0),
    ] {
        let provider = CodecProvider::new(8 * 1024 * MIB, 8 * 1024 * MIB);
        let config = GpuWorkerConfig {
            target_bytes: 32 * MIB,
            chunk_bytes: (2 * MIB) as usize,
            reserve_floor_bytes: 1024 * MIB,
            compression_enabled: true,
        };
        let mut worker = GpuCacheWorker::new_with_device_codec(
            &provider,
            config,
            Some(|_, _, _| {
                Ok(Box::new(CorruptRead {
                    codec: ramshared_vram::device_codec::ReferenceLz4Codec::new(),
                    status: failure,
                }) as Box<dyn GpuDeviceCodec>)
            }),
        );
        worker.handle_update(0, &[0x5a; PAGE_BYTES]);
        assert_eq!(worker.handle_read(0, PAGE_BYTES), None);
        let sample = worker.cache_telemetry(1);
        assert_eq!(sample.codec_integrity_errors, integrity);
        assert_eq!(sample.codec_decode_errors, decode);
        assert_eq!(
            sample.codec.state,
            if integrity + decode > 0 {
                ramshared_vram::CodecState::Faulted
            } else {
                ramshared_vram::CodecState::Ready
            }
        );
    }
}
