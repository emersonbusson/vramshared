// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/gpu_cache_worker/tests/device_timeout.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::super::*;
use super::codec_fixtures::*;
use super::*;

#[test]
fn device_codec_timeout_stops_worker_and_origin_read_remains_valid() {
    const MIB: u64 = 1024 * 1024;
    let provider = CodecProvider::new(8 * 1024 * MIB, 8 * 1024 * MIB);
    let config = GpuWorkerConfig {
        target_bytes: 32 * MIB,
        chunk_bytes: (2 * MIB) as usize,
        reserve_floor_bytes: 1024 * MIB,
        compression_enabled: true,
    };
    let mut direct = GpuCacheWorker::new_with_device_codec(
        &provider,
        config,
        Some(|_, _, _| {
            let mut codec = ramshared_vram::device_codec::ReferenceLz4Codec::new();
            codec.set_force_deadline(true);
            Ok(Box::new(codec) as Box<dyn ramshared_vram::device_codec::GpuDeviceCodec>)
        }),
    );
    direct.handle_update(0, &[0x44; 4096]);
    assert!(direct.is_disabled());
    assert_eq!(direct.codec_timeouts(), 1);
    assert_eq!(direct.handle_read(0, 4096), None);
    drop(direct);
    let (client_socket, worker_socket) = UnixStream::pair().unwrap();
    let (finished_tx, finished_rx) = std::sync::mpsc::sync_channel(1);
    let handle = std::thread::spawn(move || {
        let result =
            run_gpu_worker_loop_with_device_codec(worker_socket, provider, config, |_, _, _| {
                let mut codec = ramshared_vram::device_codec::ReferenceLz4Codec::new();
                codec.set_force_deadline(true);
                Ok(Box::new(codec) as Box<dyn ramshared_vram::device_codec::GpuDeviceCodec>)
            });
        finished_tx.send(result).unwrap();
    });
    let mut client = IpcCacheClient::with_pacing(
        client_socket,
        Duration::from_secs(2),
        32 * MIB,
        PacingPolicy::immediate(),
    );
    client.perform_handshake().unwrap();
    assert_eq!(client.update(0, &[0x44; 4096]), CacheMutation::Accepted);
    assert!(
        finished_rx
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .is_ok()
    );
    let path =
        std::env::temp_dir().join(format!("ramshared-timeout-origin-{}", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    std::io::Write::write_all(&mut file, &[0x5a; 4096]).unwrap();
    let mut origin = crate::AuthoritativeOriginBackend::new_with_admission(
        crate::FileOrigin::from_file(file),
        client,
        4096,
        4096,
        crate::AllowCacheAdmission,
    )
    .unwrap();
    let mut data = [0; 4096];
    crate::BlockBackend::read_at(&mut origin, 0, &mut data).unwrap();
    assert_eq!(data, [0x5a; 4096]);
    drop(origin);
    std::fs::remove_file(path).unwrap();
    handle.join().unwrap();
}

#[test]
fn device_codec_read_timeout_stops_worker_without_returning_partial_data() {
    use ramshared_vram::device_codec::{CodecError, GpuDeviceCodec, PAGE_BYTES, PageDecodeStatus};
    use ramshared_vram::slab_pool::{VramSlabPool, VramSlabTicket};
    struct ReadTimeout(ramshared_vram::device_codec::ReferenceLz4Codec);
    impl GpuDeviceCodec for ReadTimeout {
        fn backend_name(&self) -> &'static str {
            "read-timeout-test"
        }
        fn compress_and_pack_batch(
            &mut self,
            pages: &[[u8; PAGE_BYTES]],
            pool: &mut VramSlabPool,
            out: &mut [Option<VramSlabTicket>],
        ) -> Result<(), CodecError> {
            self.0.compress_and_pack_batch(pages, pool, out)
        }
        fn verify_and_unpack_batch(
            &mut self,
            _: &[VramSlabTicket],
            _: &mut [[u8; PAGE_BYTES]],
            _: &mut [PageDecodeStatus],
        ) -> Result<(), CodecError> {
            Err(CodecError::DeadlineExceeded)
        }
    }
    const MIB: u64 = 1024 * 1024;
    let provider = CodecProvider::new(8 * 1024 * MIB, 8 * 1024 * MIB);
    let mut worker = GpuCacheWorker::new_with_device_codec(
        &provider,
        GpuWorkerConfig {
            target_bytes: 32 * MIB,
            chunk_bytes: (2 * MIB) as usize,
            reserve_floor_bytes: 1024 * MIB,
            compression_enabled: true,
        },
        Some(|_, _, _| {
            Ok(Box::new(ReadTimeout(
                ramshared_vram::device_codec::ReferenceLz4Codec::new(),
            )) as Box<dyn GpuDeviceCodec>)
        }),
    );
    worker.handle_update(0, &[0x44; 4096]);
    assert!(!worker.is_disabled());
    assert_eq!(worker.handle_read(0, 4096), None);
    assert!(worker.is_disabled());
    assert_eq!(worker.codec_timeouts(), 1);
}
