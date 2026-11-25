// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/tests/gpu_write_pacing.rs
//! Live CUDA validation of bounded write pacing.
#![allow(clippy::expect_used, clippy::unwrap_used)]
use ramshared_cuda::Cuda;

#[test]
#[ignore = "requires a functional CUDA GPU and fresh device budget"]
fn cuda_cache_keeps_hot_reads_available_during_write_bursts() {
    use ramshared_block::{
        BestEffortCache, CacheMutation, CacheRead, GpuWorkerConfig, IpcCacheClient,
    };
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    const MIB: u64 = 1024 * 1024;
    let (socket, peer) = UnixStream::pair().unwrap();
    let worker = std::thread::spawn(move || {
        let cuda = Cuda::load().unwrap();
        let device = cuda.device(0).unwrap();
        let ctx = cuda.create_context(&device).unwrap();
        ramshared_block::run_gpu_worker_loop(
            peer,
            ctx,
            GpuWorkerConfig {
                target_bytes: 32 * MIB,
                chunk_bytes: (2 * MIB) as usize,
                reserve_floor_bytes: 2 * 1024 * MIB,
                compression_enabled: false,
            },
        )
        .unwrap();
    });
    let mut client = IpcCacheClient::new(socket, Duration::from_millis(100), 32 * MIB);
    client.perform_handshake().unwrap();
    assert!(client.target_bytes() > 0);
    assert_eq!(client.update(0, &[1; 4096]), CacheMutation::Accepted);
    assert_eq!(client.update(0, &[2; 4096]), CacheMutation::Skipped);
    std::thread::sleep(
        ramshared_block::ipc_cache_client::READ_DRAIN_GRACE + Duration::from_millis(15),
    );
    let mut page = [0u8; 4096];
    assert_eq!(client.read(0, &mut page), CacheRead::Miss);
    assert_eq!(client.promote(0, &[2; 4096]), CacheMutation::Accepted);
    let mut hits = 0;
    let mut skipped_mirrors = 0;
    for index in 0..200 {
        match client.update(8192 + index * 4096, &[3; 4096]) {
            CacheMutation::Skipped => skipped_mirrors += 1,
            CacheMutation::Accepted => {}
            CacheMutation::Failed => panic!("GPU worker failed during sustained writes"),
        }
        match client.read(0, &mut page) {
            CacheRead::Hit => {
                hits += 1;
                assert_eq!(page, [2; 4096]);
            }
            CacheRead::Miss => {}
            CacheRead::Failed => panic!("GPU cache read failed during sustained writes"),
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(hits > 30, "GPU read windows did not make progress: {hits}");
    assert!(
        skipped_mirrors > 30,
        "write pacing did not engage: {skipped_mirrors}"
    );
    eprintln!("CUDA_WRITE_BURST_TEST hits={hits} skipped_mirrors={skipped_mirrors}");
    assert_eq!(client.disable(), CacheMutation::Accepted);
    worker.join().unwrap();
}
