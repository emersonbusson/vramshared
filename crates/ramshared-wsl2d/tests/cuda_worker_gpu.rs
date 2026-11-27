// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/tests/cuda_worker_gpu.rs
//! Environment-bound CUDA GPU worker checks (device codec and small-target raw cache).
#![allow(clippy::expect_used, clippy::unwrap_used)]

use ramshared_block::{Command, serve};
use ramshared_cuda::Cuda;

#[test]
#[ignore = "requires a functional CUDA GPU and fresh device budget"]
fn cuda_device_codec_serves_normal_worker_io_and_reports_physical_allocation() {
    use ramshared_block::ipc_cache_client::PacingPolicy;
    use ramshared_block::{
        BestEffortCache, CacheMutation, CacheRead, GpuWorkerConfig, IpcCacheClient,
    };
    use ramshared_vram::device_codec::GpuDeviceCodec;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    const MIB: u64 = 1024 * 1024;
    let (socket, peer) = UnixStream::pair().unwrap();
    let worker = std::thread::spawn(move || {
        let cuda = Cuda::load().expect("CUDA library");
        let device = cuda.device(0).expect("CUDA device");
        let ctx = cuda.create_context(&device).expect("CUDA context");
        ramshared_block::run_gpu_worker_loop_with_device_codec(
            peer,
            ctx,
            GpuWorkerConfig {
                target_bytes: 128 * MIB,
                chunk_bytes: (2 * MIB) as usize,
                reserve_floor_bytes: 2 * 1024 * MIB,
                compression_enabled: true,
            },
            |ctx, available, frames| {
                ramshared_cuda::CudaPtxLz4Codec::new_context_codec(ctx, available, frames)
                    .map(|codec| Box::new(codec) as Box<dyn GpuDeviceCodec + '_>)
                    .map_err(|error| format!("{error:?}"))
            },
        )
    });
    let mut client = IpcCacheClient::with_pacing(
        socket,
        Duration::from_secs(2),
        128 * MIB,
        PacingPolicy::immediate(),
    );
    client.perform_handshake().expect("worker handshake");
    assert!(client.target_bytes() > 0, "driver budget must admit cache");
    let bytes = vec![0x5a; 8192];
    assert_eq!(client.update(0, &bytes), CacheMutation::Accepted);
    let mut actual = vec![0; bytes.len()];
    assert_eq!(client.read(0, &mut actual), CacheRead::Hit);
    assert_eq!(actual, bytes);
    let replacement = vec![0xa7; 4096];
    assert_eq!(client.update(0, &replacement), CacheMutation::Accepted);
    assert_eq!(client.read(0, &mut actual), CacheRead::Hit);
    assert_eq!(&actual[..4096], replacement);
    assert_eq!(&actual[4096..], &bytes[4096..]);
    let before_raw = client.refresh_cached_bytes().expect("device occupancy");
    let partial = vec![0x3c; 31];
    assert_eq!(client.update(32769, &partial), CacheMutation::Accepted);
    let mut partial_actual = vec![0; partial.len()];
    assert_eq!(client.read(32769, &mut partial_actual), CacheRead::Hit);
    assert_eq!(partial_actual, partial);
    let occupancy = client.refresh_cached_bytes().expect("physical occupancy");
    assert!(occupancy > 0 && occupancy <= client.target_bytes());
    assert_eq!(occupancy, before_raw + 2 * MIB);
    let telemetry = client.cache_telemetry().expect("codec telemetry");
    let activity = telemetry
        .gpu_codec_activity
        .expect("hardware codec activity");
    assert!(activity.compression_age_ms.unwrap() < 5_000);
    assert!(activity.decompression_age_ms.unwrap() < 5_000);
    assert!(telemetry.compressed_payload_bytes > 0);
    assert!(telemetry.compressed_payload_bytes < telemetry.logical_cached_bytes);
    assert!(telemetry.physical_cache_slab_bytes > 0);
    assert!(telemetry.codec_workspace_bytes > 0);
    assert!(telemetry.metadata_bytes.is_none());
    assert!(telemetry.device_metadata_lower_bound_bytes.unwrap() > 0);
    assert!(occupancy >= telemetry.physical_cache_slab_bytes + telemetry.codec_workspace_bytes);
    // Exercise the ordinary NBD request dispatcher against an authoritative
    // file and the same live GPU worker, without arming host swap.
    let path = std::env::temp_dir().join(format!("ramshared-gpu-origin-{}", std::process::id()));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    file.set_len(16384).unwrap();
    let backend = ramshared_block::AuthoritativeOriginBackend::new_with_admission(
        ramshared_block::FileOrigin::from_file(file),
        client,
        16384,
        4096,
        ramshared_block::AllowCacheAdmission,
    )
    .unwrap();
    let (mut nbd_client, mut nbd_server) = UnixStream::pair().unwrap();
    nbd_client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    nbd_server
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let server = std::thread::spawn(move || {
        use std::io::{Read, Write};
        let mut reader = nbd_server.try_clone().unwrap();
        let exports = [ramshared_block::handshake::Export::production(16384)];
        ramshared_block::server_handshake(&mut reader, &mut nbd_server, &exports, 1).unwrap();
        let mut backend = backend;
        for _ in 0..2 {
            let mut header = [0u8; 28];
            reader.read_exact(&mut header).unwrap();
            let request = ramshared_block::parse_request(&header).unwrap();
            let mut data = if request.cmd == Command::Write {
                vec![0; request.len as usize]
            } else {
                Vec::new()
            };
            if !data.is_empty() {
                reader.read_exact(&mut data).unwrap();
            }
            let result = serve(&request, &data, &mut backend);
            nbd_server.write_all(&result.reply).unwrap();
            nbd_server.write_all(&result.read_data).unwrap();
        }
        backend
    });
    use std::io::{Read, Write};
    let mut greeting = [0u8; 18];
    nbd_client.read_exact(&mut greeting).unwrap();
    assert_eq!(
        &greeting[..8],
        &ramshared_block::protocol::NBDMAGIC.to_be_bytes()
    );
    nbd_client.write_all(&2u32.to_be_bytes()).unwrap();
    nbd_client
        .write_all(&ramshared_block::protocol::IHAVEOPT.to_be_bytes())
        .unwrap();
    nbd_client
        .write_all(&ramshared_block::handshake::NBD_OPT_EXPORT_NAME.to_be_bytes())
        .unwrap();
    nbd_client.write_all(&7u32.to_be_bytes()).unwrap();
    nbd_client.write_all(b"default").unwrap();
    let mut export = [0u8; 10];
    nbd_client.read_exact(&mut export).unwrap();
    assert_eq!(&export[..8], &16384u64.to_be_bytes());
    let payload = vec![0x31; 4096];
    let mut header = [0u8; 28];
    header[..4].copy_from_slice(&ramshared_block::protocol::NBD_REQUEST_MAGIC.to_be_bytes());
    header[6..8].copy_from_slice(&1u16.to_be_bytes());
    header[8..16].copy_from_slice(&10u64.to_be_bytes());
    header[16..24].copy_from_slice(&8192u64.to_be_bytes());
    header[24..28].copy_from_slice(&4096u32.to_be_bytes());
    nbd_client.write_all(&header).unwrap();
    nbd_client.write_all(&payload).unwrap();
    let mut reply = [0u8; 16];
    nbd_client.read_exact(&mut reply).unwrap();
    assert_eq!(
        &reply[..4],
        &ramshared_block::protocol::NBD_SIMPLE_REPLY_MAGIC.to_be_bytes()
    );
    assert_eq!(u32::from_be_bytes(reply[4..8].try_into().unwrap()), 0);
    assert_eq!(&reply[8..], &10u64.to_be_bytes());
    header[6..8].copy_from_slice(&0u16.to_be_bytes());
    header[8..16].copy_from_slice(&11u64.to_be_bytes());
    nbd_client.write_all(&header).unwrap();
    nbd_client.read_exact(&mut reply).unwrap();
    assert_eq!(
        &reply[..4],
        &ramshared_block::protocol::NBD_SIMPLE_REPLY_MAGIC.to_be_bytes()
    );
    assert_eq!(u32::from_be_bytes(reply[4..8].try_into().unwrap()), 0);
    assert_eq!(&reply[8..], &11u64.to_be_bytes());
    let mut read = vec![0; 4096];
    nbd_client.read_exact(&mut read).unwrap();
    assert_eq!(read, payload);
    let backend = server.join().unwrap();
    assert!(backend.telemetry().cache_hits > 0);
    let on_disk = std::fs::read(&path).unwrap();
    assert_eq!(&on_disk[8192..12288], payload);
    assert_eq!(
        backend.release_cache().unwrap(),
        ramshared_block::CacheReleaseOutcome::Revoked
    );
    drop(backend);
    worker.join().unwrap().expect("worker exit");
    std::fs::remove_file(path).unwrap();
}

#[test]
#[ignore = "requires a functional CUDA GPU and fresh device budget"]
fn cuda_raw_cache_works_when_target_is_smaller_than_routing_chunk() {
    use ramshared_block::ipc_cache_client::PacingPolicy;
    use ramshared_block::{
        BestEffortCache, CacheMutation, CacheRead, GpuWorkerConfig, IpcCacheClient,
    };
    use std::{os::unix::net::UnixStream, time::Duration};
    const MIB: u64 = 1024 * 1024;
    let (socket, peer) = UnixStream::pair().unwrap();
    let worker = std::thread::spawn(move || {
        let cuda = Cuda::load().expect("CUDA library");
        let device = cuda.device(0).expect("CUDA device");
        let ctx = cuda.create_context(&device).expect("CUDA context");
        ramshared_block::run_gpu_worker_loop(
            peer,
            ctx,
            GpuWorkerConfig {
                target_bytes: 16 * MIB,
                chunk_bytes: (128 * MIB) as usize,
                reserve_floor_bytes: 2 * 1024 * MIB,
                compression_enabled: false,
            },
        )
    });
    let mut client = IpcCacheClient::with_pacing(
        socket,
        Duration::from_secs(2),
        16 * MIB,
        PacingPolicy::immediate(),
    );
    client.perform_handshake().unwrap();
    assert_eq!(
        client.target_bytes(),
        16 * MIB,
        "insufficient device headroom for this test"
    );
    let data = vec![0x6c; 4096];
    assert_eq!(client.update(0, &data), CacheMutation::Accepted);
    let mut actual = vec![0; data.len()];
    assert_eq!(client.read(0, &mut actual), CacheRead::Hit);
    assert_eq!(actual, data);
    assert_eq!(client.refresh_cached_bytes().unwrap(), 16 * MIB);
    assert_eq!(client.disable(), CacheMutation::Accepted);
    worker.join().unwrap().unwrap();
}
