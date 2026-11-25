// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-vram/src/worker_telemetry.rs
//! Versioned worker-cache telemetry envelope (DT-8, RF-8).
//!
//! Physical accounting stays where it already lives: `cached_bytes` and
//! `target_bytes` are frame-header fields and are never re-derived from this
//! envelope. Logical cache bytes are **cache occupancy**, never guest or host
//! RAM, and every label that renders them says so.
//!
//! The envelope carries lengths, counters, and a bounded refusal reason only.
//! No input/output bytes, file contents, addresses, or workload labels
//! (SPEC Observability).

use serde::{Deserialize, Serialize};

use crate::{GpuBudgetTelemetry, GpuUtilizationTelemetry};

/// Envelope schema version produced and accepted by this tree.
pub const WORKER_TELEMETRY_SCHEMA_VERSION: u8 = 1;

/// Hard ceiling for one serialized envelope. Matches the existing GPU-budget
/// heartbeat payload limit; the envelope must fit inside it.
pub const MAX_WORKER_TELEMETRY_PAYLOAD_BYTES: usize = 4096;

/// Maximum age of a cache telemetry sample before a consumer must drop it.
pub const TELEMETRY_MAX_AGE_MS: u64 = 5_000;

/// Bound on the codec refusal reason. Truncated on construction so the
/// envelope cannot grow with a driver string.
pub const MAX_CODEC_REFUSAL_REASON_BYTES: usize = 64;

/// Whether the selected provider advertises a GPU cache codec (DT-2 / RF-7).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodecCapability {
    /// `VramProvider::cache_codec()` returned `None` — the worker is raw-only
    /// and compression never starts (DT-9).
    RawOnly,
    /// A codec is present and may publish compressed extents.
    Available,
}

impl CodecCapability {
    pub fn as_str(self) -> &'static str {
        match self {
            CodecCapability::RawOnly => "raw-only",
            CodecCapability::Available => "available",
        }
    }
}

/// Codec runtime state at sample time.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodecState {
    /// Compression disabled or unsupported: the raw cache is the whole product.
    RawOnly,
    /// Codec usable, no fault observed on this worker.
    Ready,
    /// At least one codec fault was observed. Raw keeps serving (DT-11).
    Faulted,
    /// The codec sub-deadline fired (DT-3).
    TimedOut,
    /// The worker is disabled and is not serving.
    Disabled,
}

impl CodecState {
    pub fn as_str(self) -> &'static str {
        match self {
            CodecState::RawOnly => "raw-only",
            CodecState::Ready => "ready",
            CodecState::Faulted => "faulted",
            CodecState::TimedOut => "timed-out",
            CodecState::Disabled => "disabled",
        }
    }
}

/// Codec capability, state, and one bounded refusal reason (no payload).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CodecTelemetry {
    pub capability: CodecCapability,
    pub state: CodecState,
    /// Bounded, truncated at construction. Never a payload or an address.
    pub refusal_reason: Option<String>,
}

impl CodecTelemetry {
    /// Builds codec telemetry with a refusal reason capped at
    /// `MAX_CODEC_REFUSAL_REASON_BYTES`.
    pub fn new(capability: CodecCapability, state: CodecState, refusal: Option<&str>) -> Self {
        Self {
            capability,
            state,
            refusal_reason: refusal.map(truncate_reason),
        }
    }
}

/// Cache occupancy and codec health for one worker heartbeat (DT-8).
///
/// Every field is a length or a counter. `logical_cached_bytes` is cache
/// occupancy in the worker's logical address space and must never be labelled
/// or aggregated as guest/host RAM.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkerCacheTelemetry {
    pub schema_version: u8,
    pub sampled_at_unix_ms: u64,
    pub codec: CodecTelemetry,
    /// Logical address-space bytes currently covered by the cache.
    pub logical_cached_bytes: u64,
    /// Physical VRAM bytes held in compressed-store slabs.
    pub physical_cache_slab_bytes: u64,
    /// Provider scratch reserved for one codec call.
    pub codec_workspace_bytes: u64,
    /// Stored compressed payload bytes.
    pub compressed_payload_bytes: u64,
    /// Stored raw payload bytes inside the compressed store.
    pub raw_payload_bytes: u64,
    /// Accounted host metadata bytes, or absent when a device codec's
    /// indices and allocator cannot all be measured.
    #[serde(default)]
    pub metadata_bytes: Option<u64>,
    /// Known vector storage in the device slab pool, excluding other metadata
    /// and allocator overhead; a lower bound, not total host memory use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_metadata_lower_bound_bytes: Option<u64>,
    /// Bytes that bypassed the codec and live in the raw chunk map.
    pub raw_bypass_bytes: u64,
    /// Checksum refusals before decode (DT-7).
    pub codec_integrity_errors: u64,
    /// Decoder failures: bad status, wrong length, or output checksum mismatch.
    pub codec_decode_errors: u64,
    /// Codec sub-deadline exceedances (DT-3).
    pub codec_timeouts: u64,
}

impl WorkerCacheTelemetry {
    /// `true` when the body is usable: a schema this reader understands.
    ///
    /// This is the malformed half of the trust decision, split out so a display
    /// can tell the two refusals apart. "The worker published something
    /// unreadable" is absence and reads `not measured`. "The worker published a
    /// real reading that is no longer current" is a level that aged and reads
    /// `stale`. Collapsing them is what turns a stale reading into a claim that
    /// the worker never spoke.
    ///
    /// Age is deliberately not consulted: a sample can be well formed and old
    /// at the same time, and that combination is the one a record must keep.
    ///
    /// The body check is the schema alone. Every other field is a length or a
    /// counter with no independent domain bound, and inventing one here would
    /// invent a policy the producer does not hold; serde already refuses a
    /// body whose enums or shapes are not this type.
    pub fn is_well_formed(&self) -> bool {
        self.schema_version == WORKER_TELEMETRY_SCHEMA_VERSION
    }

    /// `true` when this sample is still fresh at `now_unix_ms`.
    ///
    /// A `now` that precedes `sampled_at_unix_ms` is a corrupt pair, not a
    /// sample taken in the future, so `checked_sub` refuses it rather than
    /// treating it as just taken.
    pub fn is_fresh_at(&self, now_unix_ms: u64, max_age_ms: u64) -> bool {
        now_unix_ms
            .checked_sub(self.sampled_at_unix_ms)
            .is_some_and(|age| age <= max_age_ms)
    }

    /// `true` when this sample is well formed and still fresh at `now_unix_ms`.
    ///
    /// Callers that must tell "malformed" from "aged out" classify with
    /// [`Self::is_well_formed`] and [`Self::is_fresh_at`] themselves; this is
    /// the admission answer, which refuses either one.
    pub fn trusted_at(&self, now_unix_ms: u64, max_age_ms: u64) -> bool {
        self.is_well_formed() && self.is_fresh_at(now_unix_ms, max_age_ms)
    }
}

/// Versioned heartbeat payload: GPU budget telemetry, adapter busy time, and
/// the cache telemetry envelope (DT-8).
///
/// Three separate fields so no one can be read as another. Cache occupancy is
/// never adapter budget; adapter budget is never cache occupancy; and adapter
/// **busy time** is never VRAM capacity occupancy. Physical `cached_bytes` /
/// `target_bytes` stay in the frame header and are not duplicated here.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkerTelemetryEnvelope {
    /// Envelope schema version. Consumers refuse anything else.
    pub schema_version: u8,
    pub sampled_at_unix_ms: u64,
    /// Existing GPU budget telemetry (its own `schema_version`, unchanged).
    pub budget: Option<GpuBudgetTelemetry>,
    /// Adapter busy time over the driver's trailing sample window. `None` is
    /// "not measured" — never a zero-percent sample.
    pub utilization: Option<GpuUtilizationTelemetry>,
    /// Cache occupancy and codec health. Never RAM.
    pub cache: Option<WorkerCacheTelemetry>,
}

impl WorkerTelemetryEnvelope {
    /// Builds a v1 envelope. `cache` is dropped when it is malformed or stale
    /// so a consumer never sees a sample it would have to discard anyway.
    pub fn new(
        sampled_at_unix_ms: u64,
        budget: Option<GpuBudgetTelemetry>,
        utilization: Option<GpuUtilizationTelemetry>,
        cache: Option<WorkerCacheTelemetry>,
    ) -> Self {
        Self {
            schema_version: WORKER_TELEMETRY_SCHEMA_VERSION,
            sampled_at_unix_ms,
            budget,
            utilization: utilization
                .filter(|sample| sample.trusted_at(sampled_at_unix_ms, TELEMETRY_MAX_AGE_MS)),
            cache: cache.filter(|telemetry| {
                telemetry.schema_version == WORKER_TELEMETRY_SCHEMA_VERSION
                    && telemetry.is_fresh_at(sampled_at_unix_ms, TELEMETRY_MAX_AGE_MS)
            }),
        }
    }

    /// Serialized size bound. `None` when the envelope exceeds the payload
    /// ceiling — the caller must then omit it rather than truncate it.
    pub fn to_bounded_payload(&self) -> Option<Vec<u8>> {
        let payload = serde_json::to_vec(self).ok()?;
        (payload.len() <= MAX_WORKER_TELEMETRY_PAYLOAD_BYTES).then_some(payload)
    }

    /// Parses a bounded payload, refusing unknown envelope versions.
    pub fn from_bounded_payload(payload: &[u8]) -> Option<Self> {
        if payload.is_empty() || payload.len() > MAX_WORKER_TELEMETRY_PAYLOAD_BYTES {
            return None;
        }
        let envelope: Self = serde_json::from_slice(payload).ok()?;
        (envelope.schema_version == WORKER_TELEMETRY_SCHEMA_VERSION).then_some(envelope)
    }
}

fn truncate_reason(reason: &str) -> String {
    if reason.len() <= MAX_CODEC_REFUSAL_REASON_BYTES {
        return reason.to_string();
    }
    let mut end = MAX_CODEC_REFUSAL_REASON_BYTES;
    while end > 0 && !reason.is_char_boundary(end) {
        end -= 1;
    }
    reason[..end].to_string()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::{GpuAdapterIdentity, GpuBudgetSource, GpuUtilizationSnapshot};

    fn budget() -> GpuBudgetTelemetry {
        GpuBudgetTelemetry {
            schema_version: 1,
            adapter: Some(GpuAdapterIdentity {
                node_mask: None,
                backend: "test".into(),
                key: "adapter-0".into(),
                luid: None,
            }),
            total_bytes: Some(8 * 1024 * 1024 * 1024),
            budget_bytes: 8 * 1024 * 1024 * 1024,
            used_bytes: 1024,
            available_bytes: 8 * 1024 * 1024 * 1024 - 1024,
            source: GpuBudgetSource::DriverReported,
            sampled_at_unix_ms: 1_000,
        }
    }

    fn cache(sampled_at_unix_ms: u64) -> WorkerCacheTelemetry {
        WorkerCacheTelemetry {
            schema_version: 1,
            sampled_at_unix_ms,
            codec: CodecTelemetry::new(CodecCapability::Available, CodecState::Ready, None),
            logical_cached_bytes: 4096,
            physical_cache_slab_bytes: 2 * 1024 * 1024,
            codec_workspace_bytes: 0,
            compressed_payload_bytes: 128,
            raw_payload_bytes: 64,
            metadata_bytes: Some(32),
            device_metadata_lower_bound_bytes: None,
            raw_bypass_bytes: 2048,
            codec_integrity_errors: 0,
            codec_decode_errors: 0,
            codec_timeouts: 0,
        }
    }

    #[test]
    fn metadata_unknown_is_distinct_from_measured_zero_and_legacy_number() {
        let mut sample = cache(1_000);
        sample.metadata_bytes = None;
        let mut value = serde_json::to_value(&sample).unwrap();
        assert!(value["metadata_bytes"].is_null());
        assert_eq!(
            serde_json::from_value::<WorkerCacheTelemetry>(value.clone())
                .unwrap()
                .metadata_bytes,
            None
        );
        value["metadata_bytes"] = serde_json::json!(0);
        value
            .as_object_mut()
            .unwrap()
            .remove("device_metadata_lower_bound_bytes");
        assert_eq!(
            serde_json::from_value::<WorkerCacheTelemetry>(value.clone())
                .unwrap()
                .metadata_bytes,
            Some(0)
        );
        assert_eq!(
            serde_json::from_value::<WorkerCacheTelemetry>(value)
                .unwrap()
                .device_metadata_lower_bound_bytes,
            None
        );
    }

    #[test]
    fn envelope_rejects_unknown_version_or_oversize() {
        let envelope =
            WorkerTelemetryEnvelope::new(1_000, Some(budget()), None, Some(cache(1_000)));
        let payload = envelope.to_bounded_payload().expect("envelope fits");
        assert!(payload.len() <= MAX_WORKER_TELEMETRY_PAYLOAD_BYTES);

        let mut unknown = envelope.clone();
        unknown.schema_version = 99;
        let unknown_payload = serde_json::to_vec(&unknown).unwrap();
        assert!(WorkerTelemetryEnvelope::from_bounded_payload(&unknown_payload).is_none());

        let oversize = vec![0u8; MAX_WORKER_TELEMETRY_PAYLOAD_BYTES + 1];
        assert!(WorkerTelemetryEnvelope::from_bounded_payload(&oversize).is_none());
        assert!(WorkerTelemetryEnvelope::from_bounded_payload(&[]).is_none());

        assert!(WorkerTelemetryEnvelope::from_bounded_payload(&payload).is_some());
    }

    #[test]
    fn envelope_omits_stale_or_malformed_cache() {
        let fresh = WorkerTelemetryEnvelope::new(10_000, None, None, Some(cache(10_000)));
        assert!(fresh.cache.is_some());

        let stale = WorkerTelemetryEnvelope::new(
            10_000 + TELEMETRY_MAX_AGE_MS + 1,
            None,
            None,
            Some(cache(10_000)),
        );
        assert!(
            stale.cache.is_none(),
            "stale cache telemetry must be omitted"
        );

        let mut malformed = cache(10_000);
        malformed.schema_version = 7;
        let dropped = WorkerTelemetryEnvelope::new(10_000, None, None, Some(malformed));
        assert!(
            dropped.cache.is_none(),
            "unknown cache schema must be omitted"
        );
    }

    /// The trust answer has two refusals and a display must be able to tell
    /// them apart: nonsense is absence, age is a level that is no longer
    /// current. Aged still leaves a well-formed body behind, which is the body
    /// a record keeps.
    #[test]
    fn cache_trust_splits_malformed_from_aged() {
        let aged = cache(1_000);
        assert!(aged.is_well_formed(), "age does not make a body malformed");
        assert!(!aged.is_fresh_at(1_000 + TELEMETRY_MAX_AGE_MS + 1, TELEMETRY_MAX_AGE_MS));
        assert!(!aged.trusted_at(1_000 + TELEMETRY_MAX_AGE_MS + 1, TELEMETRY_MAX_AGE_MS));
        // The last instant inside the window is still current.
        assert!(aged.trusted_at(1_000 + TELEMETRY_MAX_AGE_MS, TELEMETRY_MAX_AGE_MS));

        // A future sample is a corrupt pair, not one taken just now.
        assert!(!aged.is_fresh_at(999, TELEMETRY_MAX_AGE_MS));

        let mut unknown = cache(1_000);
        unknown.schema_version = 2;
        assert!(!unknown.is_well_formed());
        assert!(
            unknown.is_fresh_at(1_000, TELEMETRY_MAX_AGE_MS),
            "freshness alone must not launder an unknown schema"
        );
        assert!(!unknown.trusted_at(1_000, TELEMETRY_MAX_AGE_MS));
    }

    #[test]
    fn envelope_keeps_utilization_absence_distinct_from_measured_idle() {
        // A measured idle sample is carried. `0` is a fact the driver reported.
        let idle = GpuUtilizationTelemetry::from_snapshot(
            &GpuUtilizationSnapshot {
                gpu_pct: 0,
                memory_pct: 0,
            },
            10_000,
        );
        let envelope = WorkerTelemetryEnvelope::new(10_000, None, Some(idle), None);
        assert_eq!(envelope.utilization, Some(idle));

        // No sample at all stays `None` through the envelope. A consumer that
        // reads `None` as `0` is wrong before it ever reaches this type.
        let absent = WorkerTelemetryEnvelope::new(10_000, None, None, None);
        assert_eq!(absent.utilization, None);

        // A sample older than the window is dropped the same way cache
        // telemetry is: a consumer must never see a sample it would discard.
        let stale =
            WorkerTelemetryEnvelope::new(10_000 + TELEMETRY_MAX_AGE_MS + 1, None, Some(idle), None);
        assert_eq!(stale.utilization, None);
    }

    #[test]
    fn refusal_reason_is_bounded_and_never_a_payload() {
        let long = "x".repeat(MAX_CODEC_REFUSAL_REASON_BYTES * 4);
        let telemetry = CodecTelemetry::new(
            CodecCapability::RawOnly,
            CodecState::Faulted,
            Some(long.as_str()),
        );
        let reason = telemetry.refusal_reason.expect("reason kept");
        assert!(reason.len() <= MAX_CODEC_REFUSAL_REASON_BYTES);
        assert_eq!(reason.len(), MAX_CODEC_REFUSAL_REASON_BYTES);
    }

    #[test]
    fn codec_labels_and_reason_truncation_are_total() {
        // Every enum variant renders a stable label.
        assert_eq!(CodecCapability::RawOnly.as_str(), "raw-only");
        assert_eq!(CodecCapability::Available.as_str(), "available");
        assert_eq!(CodecState::RawOnly.as_str(), "raw-only");
        assert_eq!(CodecState::Ready.as_str(), "ready");
        assert_eq!(CodecState::Faulted.as_str(), "faulted");
        assert_eq!(CodecState::TimedOut.as_str(), "timed-out");
        assert_eq!(CodecState::Disabled.as_str(), "disabled");

        // A short reason is kept verbatim without entering the truncation path.
        assert_eq!(truncate_reason("ok"), "ok");

        // A reason whose byte cut would split a multi-byte UTF-8 character
        // backs off to the nearest char boundary instead of panicking.
        let prefix = "a".repeat(MAX_CODEC_REFUSAL_REASON_BYTES - 1);
        let reason = format!("{prefix}é tail");
        let truncated = truncate_reason(&reason);
        assert!(truncated.len() <= MAX_CODEC_REFUSAL_REASON_BYTES);
        assert!(truncated.is_char_boundary(truncated.len()));
        assert_eq!(truncated, prefix);
    }

    #[test]
    fn logical_cache_bytes_are_never_budget_or_ram() {
        let envelope =
            WorkerTelemetryEnvelope::new(1_000, Some(budget()), None, Some(cache(1_000)));
        let cache = envelope.cache.clone().expect("cache present");
        let budget = envelope.budget.clone().expect("budget present");
        assert_ne!(
            cache.logical_cached_bytes, budget.used_bytes,
            "logical cache occupancy must stay separate from adapter budget"
        );
        assert_ne!(
            cache.logical_cached_bytes, budget.available_bytes,
            "logical cache occupancy must never be read as available budget"
        );
        // The envelope never carries a field that could be mistaken for RAM.
        let encoded = serde_json::to_string(&envelope).unwrap();
        assert!(!encoded.contains("\"ram\""));
        assert!(!encoded.contains("\"host_ram\""));
        assert!(!encoded.contains("\"guest_ram\""));
    }
}
