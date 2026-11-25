// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/lib.rs
//! ramshared-block — NBD fixed-newstyle protocol + I/O model of the VRAM tier.
//!
//! SPEC: `SPECv3-WSL2.md` §8 (I/O atomicity) and §10.1 (NBD backend).
//! Also hosts [`VramBackend`] (windows-swap-driver ITEM-2 / DT-6).
//!
//! Protocol and cache policy are testable without root. Linux origin and
//! emergency backings own descriptor-bound files and retain failed allocations.
//! Device attachment, swap activation, platform and volume selection belong to
//! the lifecycle callers; this crate does not grant that authority.
#![forbid(unsafe_code)]

pub mod cache_group;
pub mod compressed_cache;
pub mod elastic_cache;
#[cfg(unix)]
pub mod gpu_cache_worker;
pub mod gpu_worker_slab_bridge;
pub mod handshake;
pub mod inflight;
#[cfg(unix)]
pub mod ipc_cache_client;
pub mod isolated_origin;
#[cfg(target_os = "linux")]
pub mod memory_observation;
#[cfg(target_os = "linux")]
pub mod native_origin;
pub mod origin_cache;
#[cfg(target_os = "linux")]
pub mod platform;
pub mod protocol;
pub mod request;
pub mod sparse_vram;
#[cfg(target_os = "linux")]
pub mod storage_inventory;
#[cfg(target_os = "linux")]
pub mod swap_absorber;
pub mod vhd_identity;
pub mod vram_backend;

pub use elastic_cache::{
    ELASTIC_CHUNK_BYTES, ElasticCacheConfig, ElasticExtentTable, ElasticVramCache,
};
#[cfg(unix)]
pub use gpu_cache_worker::{
    FRAME_HEADER_LEN, FrameHeader, GpuCacheWorker, GpuWorkerConfig, MSG_SLAB_COMPRESS_BATCH_REQ,
    MSG_SLAB_COMPRESS_BATCH_RESP, MSG_SLAB_RECOVER_BATCH_REQ, MSG_SLAB_RECOVER_BATCH_RESP,
    MSG_SLAB_TELEMETRY_REQ, MSG_SLAB_TELEMETRY_RESP, WORKER_FRAME_READ_TIMEOUT,
    run_gpu_worker_loop, run_gpu_worker_loop_with_device_codec,
    run_gpu_worker_loop_with_frame_read_timeout,
};
pub use gpu_worker_slab_bridge::{
    BRIDGE_FRAME_BYTES, BRIDGE_STAGING_BYTES, GpuWorkerSlabBridge, SlabBridgeConfig,
    SlabBridgeTelemetry,
};
pub use handshake::{HandshakeError, server_handshake};
pub use inflight::Inflight;
#[cfg(unix)]
pub use ipc_cache_client::{DEFAULT_READ_TIMEOUT, IpcCacheClient};
pub use isolated_origin::{
    AllowCacheAdmission, AuthoritativeOriginBackend, BestEffortCache, BoundedCache,
    BoundedCacheClient, CacheAdmission, CacheMutation, CacheRead, CacheReleaseOutcome,
    DisabledCache, IsolatedCacheControl, IsolatedCacheRequest, IsolatedCacheWorker,
    RefuseCacheAdmission, isolated_cache_channel,
};
pub use origin_cache::{
    CachePolicyOutcome, CacheState, CacheTelemetry, FileOrigin, GpuSample,
    ORIGIN_CACHE_CHUNK_BYTES, OriginCacheIo, OriginState, OriginStorage, WriteThroughCacheBackend,
    physical_target_bytes,
};
pub use protocol::{
    Command, NBD_EACCES, NBD_EINVAL, NBD_EIO, NBD_EPERM, NBD_ERANGE, NBD_OK, ProtocolError,
    Request, encode_simple_reply, parse_request,
};
/// Runtime headroom admitted above the configured reserve.
///
/// A reserve-policy literal, not worker machinery, so it is re-exported
/// **ungated**: platform-neutral admission code and every target must reach
/// the single authority without going through the Unix-socket worker module.
pub use ramshared_vram::RUNTIME_FREE_BUFFER_BYTES;
pub use request::{BlockBackend, IoError, ServeOutcome, SharedBlockBackend, WriteOptions, serve};
pub use sparse_vram::{
    CommitBudgetGate, DEFAULT_CHUNK_MIB, SparseVramBackend, chunk_bytes_from_env,
    commit_cap_bytes_from_env, idle_free_secs_from_env, safe_commit_cap, sealed_reserve_policy,
    sealed_reserve_policy_from_env,
};
pub use vram_backend::VramBackend;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lib_exports_and_constants_are_consistent() {
        assert_eq!(ORIGIN_CACHE_CHUNK_BYTES, 128 * 1024 * 1024);
        assert_eq!(DEFAULT_CHUNK_MIB, 128);
    }
}
