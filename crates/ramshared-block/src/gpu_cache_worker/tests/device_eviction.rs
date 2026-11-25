// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/gpu_cache_worker/tests/device_eviction.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::super::*;
use super::codec_fixtures::*;
use ramshared_vram::device_codec::{CodecError, GpuDeviceCodec, PAGE_BYTES, PageDecodeStatus};
use ramshared_vram::slab_pool::{VramSlabPool, VramSlabTicket};

struct FailAfterEviction(ramshared_vram::device_codec::ReferenceLz4Codec);
impl GpuDeviceCodec for FailAfterEviction {
    fn backend_name(&self) -> &'static str {
        "eviction-failure-test"
    }
    fn compress_and_pack_batch(
        &mut self,
        pages: &[[u8; PAGE_BYTES]],
        pool: &mut VramSlabPool,
        out: &mut [Option<VramSlabTicket>],
    ) -> Result<(), CodecError> {
        let before = pool.evictions();
        self.0.compress_and_pack_batch(pages, pool, out)?;
        if pool.evictions() > before {
            Err(CodecError::BackendFailure("injected after eviction".into()))
        } else {
            Ok(())
        }
    }
    fn verify_and_unpack_batch(
        &mut self,
        tickets: &[VramSlabTicket],
        out: &mut [[u8; PAGE_BYTES]],
        status: &mut [PageDecodeStatus],
    ) -> Result<(), CodecError> {
        self.0.verify_and_unpack_batch(tickets, out, status)
    }
    fn invalidate(&mut self, ticket: &VramSlabTicket) {
        self.0.invalidate(ticket);
    }
}

#[test]
fn failed_batch_prunes_preexisting_entries_evicted_by_that_batch() {
    const MIB: u64 = 1024 * 1024;
    let provider = CodecProvider::new(8 * 1024 * MIB, 8 * 1024 * MIB);
    let config = GpuWorkerConfig {
        target_bytes: MIB,
        chunk_bytes: (2 * MIB) as usize,
        reserve_floor_bytes: 1024 * MIB,
        compression_enabled: true,
    };
    let mut worker = GpuCacheWorker::new_with_device_codec(
        &provider,
        config,
        Some(|_, _, _| {
            let mut codec = ramshared_vram::device_codec::ReferenceLz4Codec::new();
            codec.set_deadline(std::time::Duration::from_secs(10));
            Ok(Box::new(FailAfterEviction(codec)) as Box<dyn GpuDeviceCodec>)
        }),
    );
    let frames = worker.slab_bridge().unwrap().pool().frame_count() as usize;
    for page in 0..=frames * 128 {
        worker.handle_update((page * PAGE_BYTES) as u64, &[0x41; PAGE_BYTES]);
    }
    let pool = worker.slab_bridge().unwrap().pool();
    assert!(pool.evictions() > 0);
    assert!(
        worker
            .slab_pages
            .values()
            .all(|ticket| pool.lookup(ticket).is_ok())
    );
    assert!(worker.handle_read(0, PAGE_BYTES).is_none());
    assert_eq!(worker.slab_pages.len(), worker.slab_frame_pages.len());
}

#[test]
fn batch_that_evicts_its_own_tickets_never_publishes_stale_entries() {
    const MIB: u64 = 1024 * 1024;
    let provider = CodecProvider::new(8 * 1024 * MIB, 8 * 1024 * MIB);
    let config = GpuWorkerConfig {
        target_bytes: MIB,
        chunk_bytes: (2 * MIB) as usize,
        reserve_floor_bytes: 1024 * MIB,
        compression_enabled: true,
    };
    let mut worker = GpuCacheWorker::new_with_device_codec(
        &provider,
        config,
        Some(|_, _, _| {
            let mut codec = ramshared_vram::device_codec::ReferenceLz4Codec::new();
            codec.set_deadline(std::time::Duration::from_secs(10));
            Ok(Box::new(codec) as Box<dyn GpuDeviceCodec>)
        }),
    );
    let mut state = 0x123456789abcdefu64;
    let mut bytes = vec![0; 64 * PAGE_BYTES];
    for byte in &mut bytes {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = (state >> 24) as u8;
    }
    worker.handle_update(0, &bytes);
    assert!(worker.slab_bridge().unwrap().pool().evictions() > 0);
    assert!(worker.slab_pages.is_empty());
    assert!(worker.slab_frame_pages.is_empty());
    assert!(worker.handle_read(0, PAGE_BYTES).is_none());
}

#[test]
fn slab_rpc_eviction_prunes_normal_io_index() {
    const MIB: u64 = 1024 * 1024;
    let provider = CodecProvider::new(8 * 1024 * MIB, 8 * 1024 * MIB);
    let config = GpuWorkerConfig {
        target_bytes: MIB,
        chunk_bytes: (2 * MIB) as usize,
        reserve_floor_bytes: 1024 * MIB,
        compression_enabled: true,
    };
    let mut worker = GpuCacheWorker::new_with_device_codec(
        &provider,
        config,
        Some(|_, _, _| {
            let mut codec = ramshared_vram::device_codec::ReferenceLz4Codec::new();
            codec.set_deadline(std::time::Duration::from_secs(10));
            Ok(Box::new(codec) as Box<dyn GpuDeviceCodec>)
        }),
    );
    let frames = worker.slab_bridge().unwrap().pool().frame_count() as usize;
    for page in 0..frames * 128 {
        worker.handle_update((page * PAGE_BYTES) as u64, &[0x41; PAGE_BYTES]);
    }
    let mut ticket = [None];
    worker
        .compress_pages_into_slabs(&[[0x44; PAGE_BYTES]], &mut ticket)
        .unwrap();
    assert!(ticket[0].is_some());
    let pool = worker.slab_bridge().unwrap().pool();
    assert!(pool.evictions() > 0);
    assert!(
        worker
            .slab_pages
            .values()
            .all(|ticket| pool.lookup(ticket).is_ok())
    );
    assert_eq!(worker.slab_pages.len(), worker.slab_frame_pages.len());
    assert!(worker.handle_read(0, PAGE_BYTES).is_none());
}
