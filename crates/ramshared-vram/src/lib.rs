// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-vram/src/lib.rs
//! `ramshared-vram` — VRAM backend abstraction (RF-G1, preparation for P3).
//!
//! Separates the VRAM **control plane** (lifecycle + allocation + wipe + free-floor) from
//! the concrete backend (CUDA and Vulkan). The **data plane** (block I/O) is
//! already abstracted by `ramshared_block::BlockBackend`; this crate handles VRAM-specific operations.
//!
//! Safe Rust only, completely driver-agnostic. Concrete CUDA and Vulkan implementations live in
//! `ramshared-cuda` and `ramshared-vulkan`.
//!
#![forbid(unsafe_code)]

use std::fmt;
use std::time::Instant;

#[cfg(test)]
use std::time::Duration;

mod adapter_identity;
pub mod codec;
pub mod device_codec;
mod frame_lru;
mod gpu_budget;
pub mod host_identity;
/// The single configured-reserve authority for every VRAM admission surface.
///
pub mod reserve_policy;
mod slab_backing;
pub mod slab_pool;
pub mod worker_telemetry;
pub use adapter_identity::{GpuAdapterIdentity, format_luid};
pub use codec::{
    CodecAlignments, CodecChunkResult, CodecId, CodecStatus, FakeCodec, GpuCacheCodec,
    VramOutputReservation, VramSpan, crc32,
};
pub use device_codec::{
    CODEC_SUBDEADLINE, CodecError, CompressedPageMeta, GpuDeviceCodec, MAX_BATCH_PAGES,
    MAX_COMPRESSED_LEN, PAGE_BYTES, PageDecodeStatus, ReferenceLz4Codec,
};
pub use gpu_budget::{
    GPU_BUDGET_MAX_AGE_MS, GpuBudgetSnapshot, GpuBudgetSource, GpuBudgetTelemetry,
    GpuUtilizationSnapshot, GpuUtilizationTelemetry,
};
pub use reserve_policy::{
    RUNTIME_FREE_BUFFER_BYTES, ReserveFloorEnv, ReserveFloorError, ReserveFloorPolicy,
    ReserveFloorSource, SEALED_PERCENT_SAFETY_FLOOR, SEALED_RESERVE_MIN_MIB,
    SEALED_RESERVE_PERCENT, enforced_free_floor_from_configured,
};
pub use slab_backing::HostSlabBacking;
pub use slab_pool::{
    FRAME_BYTES, MAX_SLOTS_PER_FRAME, SLOT_ALIGNMENT_BYTES, SlabClass, SlabError, SlotEncoding,
    VramSlabPool, VramSlabTicket,
};
pub use worker_telemetry::{
    CodecCapability, CodecState, CodecTelemetry, MAX_CODEC_REFUSAL_REASON_BYTES,
    MAX_WORKER_TELEMETRY_PAYLOAD_BYTES, TELEMETRY_MAX_AGE_MS, WORKER_TELEMETRY_SCHEMA_VERSION,
    WorkerCacheTelemetry, WorkerTelemetryEnvelope,
};

/// VRAM operation error (mapped from the backend-specific error, e.g., `CudaError`).
#[derive(Debug)]
pub enum VramError {
    /// Backend provider failure: initialization/driver/allocation error.
    Provider(String),
    /// Attempted access out of the allocated memory range.
    OutOfRange { off: u64, len: u64, size: u64 },
    /// Allocation failed due to out-of-memory.
    OutOfMemory,
    /// Invalid alignment for VRAM operation.
    InvalidAlignment,
    /// VRAM operation failed because resource is busy.
    Busy,
}

impl fmt::Display for VramError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VramError::Provider(m) => write!(f, "vram provider: {m}"),
            VramError::OutOfRange { off, len, size } => {
                write!(f, "vram out-of-range: off={off} len={len} size={size}")
            }
            VramError::OutOfMemory => write!(f, "vram out of memory"),
            VramError::InvalidAlignment => write!(f, "vram invalid alignment"),
            VramError::Busy => write!(f, "vram busy"),
        }
    }
}

impl std::error::Error for VramError {}

/// An allocated VRAM memory region. Synchronous operations (wipe/zeroing is blocking, DT-17/§11).
///
/// **Thread Affinity:** The implementation can be thread-local (CUDA is). It must be used on the
/// same thread that allocated it. This is why the daemon handles all VRAM I/O on a single thread.
/// The trait does NOT require `Send`.
pub trait VramMemory {
    /// Size of the region in bytes.
    fn len(&self) -> usize;
    /// Returns `true` if the region has 0 bytes.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Fills the entire region with zeroes (secure wipe + synchronize). DT-17/§11.
    fn zero(&mut self) -> Result<(), VramError>;
    /// Reads `dst.len()` bytes starting at `off`.
    fn read_at(&self, off: u64, dst: &mut [u8]) -> Result<(), VramError>;
    /// Writes `src` bytes starting at `off`.
    fn write_at(&mut self, off: u64, src: &[u8]) -> Result<(), VramError>;
}

/// VRAM Provider (representing an initialized thread-affinity context): Allocates regions and reports capacity metrics.
///
/// The driver lifecycle (driver load, device selection, and context creation) is the responsibility
/// of the concrete backend constructor (e.g., `Cuda::load()` + `create_context()`), as it differs per
/// backend; the daemon receives an initialized provider and communicates solely via this trait.
pub trait VramProvider {
    /// Type of the allocated region (GAT: borrows `&self`, preserving thread affinity without `Arc`).
    type Mem<'p>: VramMemory
    where
        Self: 'p;

    /// Allocates `bytes` of VRAM. The region is released when dropped (RAII).
    fn alloc(&self, bytes: usize) -> Result<Self::Mem<'_>, VramError>;

    /// Returns free and total VRAM capacities in bytes (used by the residency canary — DT-3/9/11).
    fn mem_info(&self) -> Result<(u64, u64), VramError>;

    /// Optional GPU cache codec capability (DT-2 / RF-7).
    ///
    /// Returns `None` unless the provider's optional runtime is loaded **and**
    /// the selected adapter supports it. The worker calls this only when
    /// compression is explicitly enabled (DT-9); an absent capability means
    /// raw-only operation. Adding compression methods to raw `VramMemory` is
    /// deliberately avoided — the capability is a separate opt-in surface.
    fn cache_codec(&self) -> Option<&dyn GpuCacheCodec<Self::Mem<'_>>> {
        None
    }

    /// Returns adapter-bound admission telemetry. The default is deliberately marked as a local
    /// estimate because legacy providers cannot prove adapter identity or external usage.
    fn budget_snapshot(&self) -> Result<GpuBudgetSnapshot, VramError> {
        let (available, total) = self.mem_info()?;
        Ok(GpuBudgetSnapshot {
            adapter: None,
            total_bytes: Some(total),
            budget_bytes: total,
            used_bytes: total.saturating_sub(available),
            source: GpuBudgetSource::ProviderLocalEstimate,
            sampled_at: Instant::now(),
        })
    }

    /// Adapter busy time over the driver's trailing sample window.
    ///
    /// `None` is **not measured** and must publish as `null`, never as a zero
    /// percent. It is the default because most providers have no driver
    /// utilization source, and a provider that cannot answer is not the same
    /// fact as an idle adapter. Providers that can measure override this and
    /// refuse out-of-range values rather than clamping them into a fabricated
    /// figure.
    fn utilization_snapshot(&self) -> Option<GpuUtilizationSnapshot> {
        None
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    #[test]
    fn budget_freshness_refuses_unknown_epoch() {
        assert!(!GpuBudgetTelemetry::sample_is_fresh_at(0, 0, 5000));
        assert!(!GpuBudgetTelemetry::sample_is_fresh_at(0, 1000, 5000));
        assert!(!GpuBudgetTelemetry::sample_is_fresh_at(1, 0, 5000));
        assert!(GpuBudgetTelemetry::sample_is_fresh_at(1, 1, 5000));
    }

    #[test]
    fn budget_admission_requires_identity_and_driver_budget() {
        let budget = GpuBudgetSnapshot {
            adapter: Some(GpuAdapterIdentity {
                node_mask: None,
                backend: "vulkan".into(),
                key: "uuid:test-adapter".into(),
                luid: None,
            }),
            total_bytes: Some(8_000),
            budget_bytes: 6_000,
            used_bytes: 2_000,
            source: GpuBudgetSource::DriverReported,
            sampled_at: Instant::now(),
        };
        assert_eq!(budget.available_bytes(), 4_000);
        assert!(budget.can_admit(4_000));
        assert!(!budget.can_admit(4_001));
        assert!(budget.can_admit_at(
            4_000,
            budget.sampled_at + std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(5)
        ));
        assert!(!budget.can_admit_at(
            1,
            budget.sampled_at + std::time::Duration::from_secs(6),
            std::time::Duration::from_secs(5)
        ));
        assert!(!budget.can_admit_at(
            1,
            budget.sampled_at - std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(5)
        ));

        let unknown = GpuBudgetSnapshot {
            adapter: None,
            source: GpuBudgetSource::ProviderLocalEstimate,
            sampled_at: Instant::now(),
            ..budget
        };
        assert!(!unknown.can_admit(1));

        let estimated = GpuBudgetSnapshot {
            adapter: budget.adapter.clone(),
            source: GpuBudgetSource::ProviderLocalEstimate,
            ..budget
        };
        assert!(!estimated.can_admit(1));

        let budget_only = GpuBudgetSnapshot {
            total_bytes: None,
            ..budget.clone()
        };
        assert!(budget_only.can_admit(4_000));
        let malformed = GpuBudgetSnapshot {
            total_bytes: Some(3_000),
            ..budget
        };
        assert!(!malformed.can_admit(1));
    }

    #[test]
    fn budget_admission_window_is_the_product_window() {
        let budget = GpuBudgetSnapshot {
            adapter: Some(GpuAdapterIdentity {
                node_mask: None,
                backend: "vulkan".into(),
                key: "uuid:test-adapter".into(),
                luid: None,
            }),
            total_bytes: Some(8_000),
            budget_bytes: 6_000,
            used_bytes: 2_000,
            source: GpuBudgetSource::DriverReported,
            sampled_at: Instant::now(),
        };

        // Boundary with the clock supplied, so the pair is exact: admitted at
        // the product window, refused one millisecond past it. This is
        // `can_admit_at`'s contract at the window the product uses, and it is
        // deterministic -- no wall-clock scheduling can move either side.
        let window = Duration::from_millis(GPU_BUDGET_MAX_AGE_MS);
        assert!(budget.can_admit_at(1, budget.sampled_at + window, window));
        assert!(!budget.can_admit_at(
            1,
            budget.sampled_at + window + Duration::from_millis(1),
            window
        ));

        // `can_admit` is that same call with the product window and the live
        // clock, and it refuses a sample the window has already outlived. A
        // bare literal wider than `GPU_BUDGET_MAX_AGE_MS` would still admit
        // this one, which is the drift the constant's own doc names. A window
        // narrower than the constant is fail-closed and is the direction the
        // fresh-sample assertions in the test above already reject.
        let aged = GpuBudgetSnapshot {
            sampled_at: Instant::now()
                .checked_sub(Duration::from_millis(GPU_BUDGET_MAX_AGE_MS + 1))
                .expect("monotonic clock predates the product freshness window"),
            ..budget
        };
        assert!(!aged.can_admit(1));
    }

    #[test]
    fn budget_available_saturates_when_driver_usage_exceeds_budget() {
        let snapshot = GpuBudgetSnapshot {
            adapter: None,
            total_bytes: None,
            budget_bytes: 1,
            used_bytes: 2,
            source: GpuBudgetSource::ProviderLocalEstimate,
            sampled_at: Instant::now(),
        };
        assert_eq!(snapshot.available_bytes(), 0);
    }

    #[test]
    fn safe_cache_target_respects_actual_allocation_free_floor() {
        const MIB: u64 = 1024 * 1024;
        let mut budget = GpuBudgetSnapshot {
            adapter: None,
            total_bytes: Some(6144 * MIB),
            budget_bytes: 6144 * MIB,
            used_bytes: 0,
            source: GpuBudgetSource::ProviderLocalEstimate,
            sampled_at: Instant::now(),
        };
        for (available, requested, expected) in [
            (6144 * MIB, u64::MAX, 3456 * MIB),
            (6144 * MIB, 1024 * MIB, 1024 * MIB),
            (800 * MIB, u64::MAX, 0),
            (2688 * MIB, u64::MAX, 0),
            (2688 * MIB + 1, u64::MAX, 1),
        ] {
            budget.used_bytes = budget.budget_bytes - available;
            assert_eq!(
                budget.safe_target_bytes(requested, 2048 * MIB, 640 * MIB),
                expected,
                "available={available} requested={requested}"
            );
        }
        assert_eq!(budget.safe_target_bytes(u64::MAX, u64::MAX, 1), 0);
    }

    #[test]
    fn budget_target_caps_at_the_reserve_share_and_keeps_runtime_free() {
        let budget = GpuBudgetSnapshot {
            adapter: Some(GpuAdapterIdentity {
                node_mask: None,
                backend: "cuda".into(),
                key: "gpu:test".into(),
                luid: None,
            }),
            total_bytes: Some(8_000),
            budget_bytes: 6_000,
            used_bytes: 1_000,
            source: GpuBudgetSource::DriverReported,
            sampled_at: Instant::now(),
        };

        // capacity = 6_000, available = 5_000, reserve = 1_200 and
        // runtime = 500. The same 1_700-byte floor binds all admission paths.
        assert_eq!(budget.safe_target_bytes(10_000, 1_000, 500), 3_300);
        assert_eq!(budget.required_free_bytes(1_000, 500), 1_700);
    }

    /// Positive targets must preserve both free-floor components. A snapshot
    /// already below the floor must admit no additional allocation.
    #[test]
    fn safe_target_keeps_the_floor_for_trusted_snapshots() {
        let mib = 1024 * 1024;
        let configured = 1_000;
        let runtime = 500;
        // (total, budget, used). Trusted means budget <= total when total is known.
        let cases: [(u64, u64, u64); 6] = [
            (8_000, 6_000, 1_000),
            (6_000, 6_000, 0),
            (6_000, 6_000, 5_000),
            (6_144 * mib, 4_270 * mib, 1_549 * mib),
            (48 * 1024 * mib, 40 * 1024 * mib, 1024 * mib),
            (6_000, 4_000, 3_000),
        ];
        for (total, budget, used) in cases {
            let snapshot = GpuBudgetSnapshot {
                adapter: Some(GpuAdapterIdentity {
                    node_mask: None,
                    backend: "cuda".into(),
                    key: "gpu:test".into(),
                    luid: None,
                }),
                total_bytes: Some(total),
                budget_bytes: budget,
                used_bytes: used,
                source: GpuBudgetSource::DriverReported,
                sampled_at: Instant::now(),
            };
            let capacity = snapshot
                .total_bytes
                .unwrap_or(snapshot.budget_bytes)
                .min(snapshot.budget_bytes);
            let floor = snapshot.required_free_bytes(configured, runtime);
            for requested in [1, 1_000, 10_000, 1 << 30, u64::MAX] {
                let target = snapshot.safe_target_bytes(requested, configured, runtime);
                let free_after = capacity.saturating_sub(used).saturating_sub(target);
                assert!(
                    target == 0 || free_after >= floor,
                    "target {target} leaves {free_after} of capacity free, below floor {floor} \
                     (total={total} budget={budget} used={used} requested={requested})"
                );
            }
        }
    }

    /// Numeric clamping is conservative even for an inconsistent snapshot;
    /// production admission still rejects such telemetry before this helper.
    #[test]
    fn safe_target_clamps_inconsistent_numeric_geometry() {
        let snapshot = GpuBudgetSnapshot {
            adapter: None,
            total_bytes: Some(3_000),
            budget_bytes: 8_000,
            used_bytes: 2_000,
            source: GpuBudgetSource::ProviderLocalEstimate,
            sampled_at: Instant::now(),
        };
        assert_eq!(snapshot.safe_target_bytes(u64::MAX, 500, 500), 0);
        let unknown_total = GpuBudgetSnapshot {
            total_bytes: None,
            ..snapshot
        };
        assert_eq!(unknown_total.safe_target_bytes(u64::MAX, 500, 500), 3_900);
    }

    #[test]
    fn adapter_identity_matches_cross_api_only_through_shared_luid() {
        let cuda = GpuAdapterIdentity {
            node_mask: Some(1),
            backend: "cuda".into(),
            key: "cuda-uuid".into(),
            luid: Some("aabbccdd:00001122".into()),
        };
        let dxg = GpuAdapterIdentity {
            node_mask: Some(1),
            backend: "dxg".into(),
            key: "aabbccdd:00001122".into(),
            luid: Some("aabbccdd:00001122".into()),
        };
        let other = GpuAdapterIdentity {
            node_mask: Some(1),
            backend: "cuda".into(),
            key: "other-uuid".into(),
            luid: Some("00000001:00000002".into()),
        };
        let missing_luid = GpuAdapterIdentity {
            node_mask: Some(1),
            backend: "dxg".into(),
            key: "cuda-uuid".into(),
            luid: None,
        };
        let same_backend = GpuAdapterIdentity {
            node_mask: Some(1),
            backend: "cuda".into(),
            key: "cuda-uuid".into(),
            luid: Some("ffffffff:ffffffff".into()),
        };
        assert!(cuda.matches_physical_adapter(&dxg));
        assert!(!cuda.matches_physical_adapter(&other));
        assert!(!cuda.matches_physical_adapter(&missing_luid));
        assert!(cuda.matches_physical_adapter(&same_backend));
    }

    #[test]
    fn luid_format_matches_windows_high_low_display_order() {
        assert_eq!(
            format_luid([0x22, 0x11, 0, 0, 0xdd, 0xcc, 0xbb, 0xaa]),
            Some("aabbccdd:00001122".into())
        );
        assert_eq!(format_luid([0; 8]), None);
    }

    #[test]
    fn published_budget_is_serializable_and_admission_expires() {
        let now_ms = 1_000_000;
        let snapshot = GpuBudgetSnapshot {
            adapter: Some(GpuAdapterIdentity {
                node_mask: None,
                backend: "vulkan".into(),
                key: "uuid:abcd".into(),
                luid: Some("aabbccdd:00001122".into()),
            }),
            total_bytes: Some(8_000),
            budget_bytes: 6_000,
            used_bytes: 2_000,
            source: GpuBudgetSource::DriverReported,
            sampled_at: Instant::now(),
        };
        let telemetry = GpuBudgetTelemetry::from_snapshot(&snapshot, now_ms);
        assert_eq!(telemetry.available_bytes, 4_000);
        assert_eq!(
            telemetry.trusted_available_at(now_ms + 4_000, GPU_BUDGET_MAX_AGE_MS),
            Some(4_000)
        );
        assert_eq!(
            telemetry
                .trusted_available_at(now_ms + GPU_BUDGET_MAX_AGE_MS + 1, GPU_BUDGET_MAX_AGE_MS),
            None
        );
        assert_eq!(
            telemetry.trusted_available_at(now_ms - 1, GPU_BUDGET_MAX_AGE_MS),
            None
        );

        let encoded = serde_json::to_vec(&telemetry).expect("budget telemetry serializes");
        let decoded: GpuBudgetTelemetry =
            serde_json::from_slice(&encoded).expect("budget telemetry deserializes");
        assert_eq!(decoded, telemetry);
    }

    #[test]
    fn from_published_status_reads_the_budget_field() {
        // The published document carries the sample under `gpu_budget`. The
        // deserializer is the one that knows that shape, so every reader —
        // cascade and readiness alike — reduces a document through it.
        let status = serde_json::json!({
            "ok": true,
            "gpu_budget": {
                "schema_version": 1,
                "adapter": { "backend": "cuda", "key": "0", "luid": null },
                "total_bytes": 8_000,
                "budget_bytes": 6_000,
                "used_bytes": 2_000,
                "available_bytes": 4_000,
                "source": "driver_reported",
                "sampled_at_unix_ms": 1_000,
            }
        });
        let budget = GpuBudgetTelemetry::from_published_status(&status)
            .expect("a well-formed published sample parses");
        assert_eq!(budget.budget_bytes, 6_000);
        assert_eq!(budget.used_bytes, 2_000);
        assert_eq!(
            budget.trusted_available_at(1_000, GPU_BUDGET_MAX_AGE_MS),
            Some(4_000)
        );
    }

    #[test]
    fn from_published_status_treats_missing_and_null_as_never_bound() {
        // `gpu_budget: null` is the daemon's honest "no budget bound" report,
        // not a stale sample. Missing and null are the same never-bound answer:
        // distinguishing them into `stale` would raise an alarm on a daemon
        // that is running exactly as configured.
        let missing = serde_json::json!({ "ok": true });
        assert!(GpuBudgetTelemetry::from_published_status(&missing).is_none());

        let never_bound = serde_json::json!({ "ok": true, "gpu_budget": null });
        assert!(GpuBudgetTelemetry::from_published_status(&never_bound).is_none());
    }

    #[test]
    fn from_published_status_refuses_a_malformed_sample() {
        // A field that will not parse is no sample. Fabricating one would be a
        // second budget producer by the back door (DT-6).
        let wrong_type = serde_json::json!({ "gpu_budget": "not-an-object" });
        assert!(GpuBudgetTelemetry::from_published_status(&wrong_type).is_none());

        let incomplete = serde_json::json!({
            "gpu_budget": { "schema_version": 1, "budget_bytes": 6_000 }
        });
        assert!(GpuBudgetTelemetry::from_published_status(&incomplete).is_none());
    }

    #[test]
    fn gpu_budget_window_is_the_predicate_boundary() {
        // The product window is not a documentation number: it is the exact
        // age at which `trusted_available_at` turns a well-formed sample down.
        // Every reader in every crate names this one constant, so pinning it
        // here pins the whole chain.
        assert_eq!(GPU_BUDGET_MAX_AGE_MS, 5_000);
        let snapshot = GpuBudgetSnapshot {
            adapter: Some(GpuAdapterIdentity {
                node_mask: None,
                backend: "test".into(),
                key: "key".into(),
                luid: None,
            }),
            total_bytes: Some(8_000),
            budget_bytes: 6_000,
            used_bytes: 2_000,
            source: GpuBudgetSource::DriverReported,
            sampled_at: Instant::now(),
        };
        let telemetry = GpuBudgetTelemetry::from_snapshot(&snapshot, 1_000);
        assert_eq!(
            telemetry.trusted_available_at(1_000 + GPU_BUDGET_MAX_AGE_MS, GPU_BUDGET_MAX_AGE_MS),
            Some(4_000),
            "a sample exactly at the window is still trusted"
        );
        assert_eq!(
            telemetry
                .trusted_available_at(1_000 + GPU_BUDGET_MAX_AGE_MS + 1, GPU_BUDGET_MAX_AGE_MS),
            None,
            "a sample one millisecond past the window is refused"
        );
    }

    #[test]
    fn budget_trust_splits_malformed_from_aged() {
        let sample = GpuBudgetTelemetry {
            schema_version: 1,
            adapter: Some(GpuAdapterIdentity {
                node_mask: None,
                backend: "cuda".into(),
                key: "gpu-0".into(),
                luid: None,
            }),
            total_bytes: Some(8_000),
            budget_bytes: 6_000,
            used_bytes: 2_000,
            available_bytes: 4_000,
            source: GpuBudgetSource::DriverReported,
            sampled_at_unix_ms: 1_000,
        };
        // Well formed is about the body, never the clock. An aged sample is
        // still a real reading, which is exactly the case a record must keep
        // instead of erasing to the absence marker.
        assert!(sample.is_well_formed());
        assert!(!sample.is_fresh_at(1_000 + GPU_BUDGET_MAX_AGE_MS + 1, GPU_BUDGET_MAX_AGE_MS));
        assert!(sample.is_well_formed(), "age does not unmake the body");
        assert!(sample.is_fresh_at(1_000, GPU_BUDGET_MAX_AGE_MS));

        // Malformed is refused regardless of age, and it is not the same fact
        // as aged out: one is the driver speaking nonsense, the other is the
        // driver going quiet after a real reading.
        let torn = GpuBudgetTelemetry {
            available_bytes: 5_000,
            ..sample.clone()
        };
        assert!(!torn.is_well_formed());
        assert!(
            torn.is_fresh_at(1_000, GPU_BUDGET_MAX_AGE_MS),
            "freshness alone does not launder a torn figure"
        );

        let overspent = GpuBudgetTelemetry {
            used_bytes: 9_000,
            available_bytes: 0,
            ..sample.clone()
        };
        assert!(!overspent.is_well_formed());
        assert!(overspent.is_fresh_at(1_000, GPU_BUDGET_MAX_AGE_MS));

        let over_budget = GpuBudgetTelemetry {
            total_bytes: Some(1_000),
            ..sample.clone()
        };
        assert!(!over_budget.is_well_formed());

        let wrong_version = GpuBudgetTelemetry {
            schema_version: 2,
            ..sample.clone()
        };
        assert!(!wrong_version.is_well_formed());
        assert!(wrong_version.is_fresh_at(1_000, GPU_BUDGET_MAX_AGE_MS));

        // The admission answer still refuses either one, so widening the
        // split cannot widen what gets trusted.
        assert!(
            torn.trusted_available_at(1_000, GPU_BUDGET_MAX_AGE_MS)
                .is_none()
        );
        assert!(
            overspent
                .trusted_available_at(1_000, GPU_BUDGET_MAX_AGE_MS)
                .is_none()
        );
        assert!(
            wrong_version
                .trusted_available_at(1_000, GPU_BUDGET_MAX_AGE_MS)
                .is_none()
        );
        assert!(
            sample
                .trusted_available_at(1_000 + GPU_BUDGET_MAX_AGE_MS + 1, GPU_BUDGET_MAX_AGE_MS)
                .is_none()
        );
    }

    /// Source is admission, not arithmetic. An internally consistent estimate
    /// is a well-formed sample whose numbers add up; what it is not is the
    /// driver's report, and that is why the admission answer turns it down
    /// while the body check does not.
    ///
    /// The two answers must move independently. Folding the source into
    /// [`GpuBudgetTelemetry::is_well_formed`] made an estimate look exactly
    /// like a torn figure, so the only face a display could give it was the
    /// absence wording — "the provider never spoke" — for a provider that
    /// published a real, if less authoritative, number.
    #[test]
    fn budget_admission_refuses_an_estimate_that_is_well_formed() {
        let driver = GpuBudgetTelemetry {
            schema_version: 1,
            adapter: Some(GpuAdapterIdentity {
                node_mask: None,
                backend: "cuda".into(),
                key: "gpu-0".into(),
                luid: None,
            }),
            total_bytes: Some(8_000),
            budget_bytes: 6_000,
            used_bytes: 2_000,
            available_bytes: 4_000,
            source: GpuBudgetSource::DriverReported,
            sampled_at_unix_ms: 1_000,
        };
        let estimate = GpuBudgetTelemetry {
            source: GpuBudgetSource::ProviderLocalEstimate,
            ..driver.clone()
        };

        // The body is consistent in both, so both are well formed. This is
        // the widening that lets a display see the record at all.
        assert!(driver.is_well_formed());
        assert!(
            estimate.is_well_formed(),
            "an internally consistent estimate is a well-formed body"
        );

        // And neither the body check nor the admission answer confuses the
        // two: only the driver's own report may be allocated against.
        assert_eq!(
            driver.trusted_available_at(1_000, GPU_BUDGET_MAX_AGE_MS),
            Some(4_000)
        );
        assert!(
            estimate
                .trusted_available_at(1_000, GPU_BUDGET_MAX_AGE_MS)
                .is_none(),
            "admission refuses a source that is not the driver's report"
        );
    }

    #[test]
    fn utilization_absence_is_not_measured_idle() {
        // Missing and `null` are the same honest "not measured" answer, and
        // neither is a zero-percent sample. A dashboard that renders `None` as
        // `0%` claims an observation nobody took.
        let missing = serde_json::json!({ "ok": true });
        assert!(GpuUtilizationTelemetry::from_published_status(&missing).is_none());
        let not_measured = serde_json::json!({ "ok": true, "gpu_utilization": null });
        assert!(GpuUtilizationTelemetry::from_published_status(&not_measured).is_none());

        // A measured idle sample is a different fact and does parse.
        let idle = serde_json::json!({
            "gpu_utilization": {
                "schema_version": 1,
                "gpu_pct": 0,
                "memory_pct": 0,
                "sampled_at_unix_ms": 1_000
            }
        });
        let sample = GpuUtilizationTelemetry::from_published_status(&idle).expect("idle parses");
        assert_eq!(sample.gpu_pct, 0);
        assert!(sample.trusted_at(1_000, GPU_BUDGET_MAX_AGE_MS));
    }

    #[test]
    fn utilization_trust_refuses_stale_and_out_of_range() {
        let sample = GpuUtilizationTelemetry {
            schema_version: 1,
            gpu_pct: 37,
            memory_pct: 82,
            sampled_at_unix_ms: 1_000,
        };
        // Freshness is the same window the budget sample uses: the two are
        // taken at one heartbeat and must not outlive each other.
        assert!(sample.trusted_at(1_000 + GPU_BUDGET_MAX_AGE_MS, GPU_BUDGET_MAX_AGE_MS));
        assert!(!sample.trusted_at(1_000 + GPU_BUDGET_MAX_AGE_MS + 1, GPU_BUDGET_MAX_AGE_MS));

        // A percent above 100 is malformed and is never trusted. Clamping it
        // into range would turn a bad driver reply into a fabricated figure.
        let overflow = GpuUtilizationTelemetry {
            gpu_pct: 101,
            ..sample
        };
        assert!(!overflow.trusted_at(1_000, GPU_BUDGET_MAX_AGE_MS));

        let wrong_version = GpuUtilizationTelemetry {
            schema_version: 2,
            ..sample
        };
        assert!(!wrong_version.trusted_at(1_000, GPU_BUDGET_MAX_AGE_MS));
    }

    #[test]
    fn utilization_trust_splits_malformed_from_aged() {
        let sample = GpuUtilizationTelemetry {
            schema_version: 1,
            gpu_pct: 37,
            memory_pct: 82,
            sampled_at_unix_ms: 1_000,
        };
        // Well formed is about the body, never the clock. An aged sample is
        // still a real reading, which is exactly the case a record must keep
        // instead of erasing to the absence marker.
        assert!(sample.is_well_formed());
        assert!(!sample.is_fresh_at(1_000 + GPU_BUDGET_MAX_AGE_MS + 1, GPU_BUDGET_MAX_AGE_MS));
        assert!(sample.is_well_formed(), "age does not unmake the body");
        assert!(sample.is_fresh_at(1_000, GPU_BUDGET_MAX_AGE_MS));

        // Malformed is refused regardless of age, and it is not the same fact
        // as aged out: one is the driver speaking nonsense, the other is the
        // driver going quiet after a real reading.
        let overflow = GpuUtilizationTelemetry {
            gpu_pct: 101,
            ..sample
        };
        assert!(!overflow.is_well_formed());
        assert!(
            overflow.is_fresh_at(1_000, GPU_BUDGET_MAX_AGE_MS),
            "freshness alone does not launder a bad percent"
        );

        let wrong_version = GpuUtilizationTelemetry {
            schema_version: 2,
            ..sample
        };
        assert!(!wrong_version.is_well_formed());
        assert!(wrong_version.is_fresh_at(1_000, GPU_BUDGET_MAX_AGE_MS));

        // The admission answer still refuses either one, so widening the
        // split cannot widen what gets trusted.
        assert!(!overflow.trusted_at(1_000, GPU_BUDGET_MAX_AGE_MS));
        assert!(!wrong_version.trusted_at(1_000, GPU_BUDGET_MAX_AGE_MS));
        assert!(!sample.trusted_at(1_000 + GPU_BUDGET_MAX_AGE_MS + 1, GPU_BUDGET_MAX_AGE_MS));
    }

    #[test]
    fn test_vram_error_display() {
        assert_eq!(
            VramError::Provider("test".to_string()).to_string(),
            "vram provider: test"
        );
        assert_eq!(
            VramError::OutOfRange {
                off: 0,
                len: 10,
                size: 5
            }
            .to_string(),
            "vram out-of-range: off=0 len=10 size=5"
        );
        assert_eq!(VramError::OutOfMemory.to_string(), "vram out of memory");
        assert_eq!(
            VramError::InvalidAlignment.to_string(),
            "vram invalid alignment"
        );
        assert_eq!(VramError::Busy.to_string(), "vram busy");
    }
}
