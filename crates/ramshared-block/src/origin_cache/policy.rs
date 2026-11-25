// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/origin_cache/policy.rs
//! Residency target validation and telemetry state projections for the origin cache.

use std::time::{Duration, Instant};

use ramshared_vram::{GpuAdapterIdentity, GpuBudgetSource};

/// Maximum age of a GPU sample before it is refused (DT-6).
///
/// The number is [`ramshared_vram::GPU_BUDGET_MAX_AGE_MS`] — the freshness
/// authority beside [`ramshared_vram::GpuBudgetTelemetry::trusted_available_at`],
/// which this crate already depends on. Restated as a `Duration` only because
/// the cache's own timestamps are `Instant`-relative.
pub const SAMPLE_MAX_AGE: Duration = Duration::from_millis(ramshared_vram::GPU_BUDGET_MAX_AGE_MS);

/// One adapter-bound driver budget observation.
///
/// Carries its own provenance (DT-12). **Not `Copy`**: `GpuAdapterIdentity`
/// owns `String` fields. The producer builds this; `observe_gpu` only forwards
/// it; [`physical_target_bytes`] is the single validator (DT-6).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GpuSample {
    /// Allocation budget for the adapter, in bytes.
    pub budget_bytes: u64,
    /// VRAM used by processes other than this worker, in bytes.
    pub external_usage_bytes: u64,
    /// Total physical device memory, in bytes.
    pub total_vram_bytes: u64,
    /// When the producer observed this sample (DT-12: `Instant`, one clock).
    pub sampled_at: Instant,
    /// Where the numbers came from. Only [`GpuBudgetSource::DriverReported`]
    /// is admitted.
    pub source: GpuBudgetSource,
    /// The adapter the numbers describe. Missing identity refuses admission.
    pub adapter: Option<GpuAdapterIdentity>,
}

/// Physical cache target for a logical size, or `0` when the sample must not
/// be trusted (DT-6).
///
/// One ownership chain, no second owner: the producer carries provenance,
/// `observe_gpu` forwards, and **this function validates and refuses**. It
/// never stamps provenance — never `Instant::now()`, never
/// `GpuBudgetSource::DriverReported` — so a staleness check can actually fail.
///
/// After the checks it returns
/// `safe_target_bytes(logical_bytes, configured_reserve_bytes,
/// runtime_headroom_bytes)` over the numeric mapping `budget_bytes` →
/// `budget_bytes`, `external_usage_bytes` → `used_bytes`, `total_vram_bytes` →
/// `total_bytes`. `safe_target_bytes` is arithmetic only and supplies the
/// arithmetic **after** these checks.
pub fn physical_target_bytes(
    logical_bytes: u64,
    sample: Option<&GpuSample>,
    configured_reserve_bytes: u64,
    runtime_headroom_bytes: u64,
    now: Instant,
) -> u64 {
    let Some(sample) = sample else {
        return 0;
    };
    // Provenance refusal. Each of these is a real, testable failure.
    if sample.source != GpuBudgetSource::DriverReported {
        return 0;
    }
    if sample.adapter.is_none() {
        return 0;
    }
    let age = match now.checked_duration_since(sample.sampled_at) {
        Some(age) => age,
        // A sample from the future is not trustworthy either.
        None => return 0,
    };
    if age > SAMPLE_MAX_AGE {
        return 0;
    }
    // Consistency refusal.
    if sample.external_usage_bytes > sample.budget_bytes {
        return 0;
    }
    if sample.budget_bytes > sample.total_vram_bytes {
        return 0;
    }

    // Numeric mapping onto the shared helper. No inline reserve math remains.
    let snapshot = ramshared_vram::GpuBudgetSnapshot {
        adapter: sample.adapter.clone(),
        total_bytes: Some(sample.total_vram_bytes),
        budget_bytes: sample.budget_bytes,
        used_bytes: sample.external_usage_bytes,
        source: sample.source,
        sampled_at: sample.sampled_at,
    };
    snapshot.safe_target_bytes(
        logical_bytes,
        configured_reserve_bytes,
        runtime_headroom_bytes,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OriginState {
    Off,
    Ready,
    Degraded,
    Failed,
}

impl OriginState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "OFF",
            Self::Ready => "READY",
            Self::Degraded => "DEGRADED",
            Self::Failed => "FAILED",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CacheState {
    Off,
    Active,
    Restricted,
    Unavailable,
    Stuck,
}

impl CacheState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "OFF",
            Self::Active => "ACTIVE",
            Self::Restricted => "RESTRICTED",
            Self::Unavailable => "UNAVAILABLE",
            Self::Stuck => "STUCK",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CacheTelemetry {
    pub origin_written_bytes: u64,
    pub origin_syncs: u64,
    pub batched_writes: u64,
    pub cache_read_bytes: u64,
    pub cache_hits: u64,
    pub fallback_reads: u64,
    pub invalidations: u64,
    pub promotion_refusals: u64,
    pub releases: u64,
    pub allocation_failures: u64,
    pub cache_read_failures: u64,
    pub cache_write_failures: u64,
    pub valid_blocks: u64,
}

/// Published origin-plane I/O accounting.
///
/// [`CacheTelemetry`] is the write-through cache's internal counter set. This
/// is the subset of it that the daemon's cache-status document used to drop on
/// the floor. The four counters that already carry a top-level key on that
/// document — `ssd_origin_written_kib`, `cache_fallback_reads`,
/// `cache_invalidations`, `cache_releases` — keep those names and are **not**
/// repeated here, so one fact has one published name.
///
/// There is deliberately no `cache_write_bytes`: write-through traffic is
/// already accounted as `ssd_origin_written_kib`, and a second byte counter
/// for the same writes would be a second number for one fact.
///
/// `cache_read_bytes` is the one byte volume and keeps the source name and
/// unit so it matches the benchmark taxonomy's cache-hit metric. Every other
/// field is a count. `valid_blocks` is a **gauge**, not a counter: it is the
/// number of cache blocks valid in this sample and falls when chunks are
/// invalidated, so it must never be differenced like the rest.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct OriginCacheIo {
    /// Successful `sync_data` calls that made authoritative-origin writes durable.
    pub origin_syncs: u64,
    /// Writes acknowledged without an immediate durability sync (non-FUA).
    pub batched_writes: u64,
    /// Cache-hit traffic: bytes served from the cache rather than the origin.
    pub cache_read_bytes: u64,
    /// Cache reads that failed and invalidated the affected chunk.
    pub cache_read_failures: u64,
    /// Cache writes that failed and invalidated the affected chunk.
    pub cache_write_failures: u64,
    /// Refused promotion segments, or unknown when the worker cannot report them.
    pub promotion_refusals: Option<u64>,
    /// Cache chunk allocations that failed, or unknown for an isolated worker.
    pub allocation_failures: Option<u64>,
    /// **Gauge:** valid blocks in this sample, or unknown for an isolated worker.
    pub valid_blocks: Option<u64>,
}

impl From<CacheTelemetry> for OriginCacheIo {
    /// Project the measured counters onto their published names.
    ///
    /// One-to-one on purpose: a field the backend counted is a field the
    /// document publishes, and a field the type does not have is a counter the
    /// product does not measure.
    fn from(telemetry: CacheTelemetry) -> Self {
        Self {
            origin_syncs: telemetry.origin_syncs,
            batched_writes: telemetry.batched_writes,
            cache_read_bytes: telemetry.cache_read_bytes,
            cache_read_failures: telemetry.cache_read_failures,
            cache_write_failures: telemetry.cache_write_failures,
            promotion_refusals: Some(telemetry.promotion_refusals),
            allocation_failures: Some(telemetry.allocation_failures),
            valid_blocks: Some(telemetry.valid_blocks),
        }
    }
}

impl OriginCacheIo {
    /// The isolated origin counts its own I/O, but does not observe worker
    /// promotion refusals, allocation failures or the number of valid worker blocks.
    pub fn from_isolated(telemetry: CacheTelemetry) -> Self {
        Self {
            promotion_refusals: None,
            allocation_failures: None,
            valid_blocks: None,
            ..Self::from(telemetry)
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CachePolicyOutcome {
    pub target_bytes: u64,
    pub allocated_bytes: u64,
    pub released_bytes: u64,
}
