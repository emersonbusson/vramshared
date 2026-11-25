// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-vram/src/gpu_budget.rs
//! Adapter-bound budget, utilization and strict telemetry trust.
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::GpuAdapterIdentity;

/// Reliability of the capacity data returned by a provider.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuBudgetSource {
    /// The driver supplies the budget; usage scope follows the backend API and is not
    /// necessarily a device-wide count of other applications' allocations.
    DriverReported,
    /// Only allocations known to this provider instance are included.
    ProviderLocalEstimate,
}

/// A memory budget tied to the adapter selected by a concrete GPU provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GpuBudgetSnapshot {
    pub adapter: Option<GpuAdapterIdentity>,
    /// Physical capacity when the backend reports it (WDDM budget queries may omit it).
    pub total_bytes: Option<u64>,
    pub budget_bytes: u64,
    pub used_bytes: u64,
    pub source: GpuBudgetSource,
    pub sampled_at: Instant,
}

/// Product freshness window for a driver-reported budget sample, in
/// milliseconds (DT-6).
///
/// Every reader of a [`GpuBudgetTelemetry`] — the daemon publisher, the
/// physical cache, the CLI cascade and the monitor — holds this one number.
/// It lives beside [`GpuBudgetTelemetry::trusted_available_at`] because that
/// is the predicate it governs, and every crate that needs the window already
/// depends on this one, so no reader has to restate the value. A bare literal
/// at a call site is how a window drifts away from the publisher that stamps
/// the sample.
pub const GPU_BUDGET_MAX_AGE_MS: u64 = 5_000;

/// Wall-clock representation of an adapter-bound GPU budget for IPC and status files.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GpuBudgetTelemetry {
    pub schema_version: u8,
    pub adapter: Option<GpuAdapterIdentity>,
    pub total_bytes: Option<u64>,
    pub budget_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    pub source: GpuBudgetSource,
    pub sampled_at_unix_ms: u64,
}

impl GpuBudgetTelemetry {
    pub fn from_snapshot(snapshot: &GpuBudgetSnapshot, sampled_at_unix_ms: u64) -> Self {
        Self {
            schema_version: 1,
            adapter: snapshot.adapter.clone(),
            total_bytes: snapshot.total_bytes,
            budget_bytes: snapshot.budget_bytes,
            used_bytes: snapshot.used_bytes,
            available_bytes: snapshot.available_bytes(),
            source: snapshot.source,
            sampled_at_unix_ms,
        }
    }

    /// Parse the `gpu_budget` field of a published status document.
    ///
    /// This is the one deserializer of a published sample. A missing key and a
    /// JSON `null` are the same honest **never-bound** report and both yield
    /// `None`; so does a field that will not parse. Nothing is invented — a
    /// caller that sees `None` has no sample, not a zero-sized one.
    ///
    /// Trust is decided separately, by [`Self::trusted_available_at`], so a
    /// caller can tell "never bound" from "bound but untrusted".
    pub fn from_published_status(status: &serde_json::Value) -> Option<Self> {
        serde_json::from_value(status.get("gpu_budget")?.clone()).ok()
    }

    /// The malformed half of the trust decision, split out so a display can
    /// tell the two refusals apart. "The driver published something
    /// unreadable" is absence and reads as the panel's absence wording. "The
    /// driver published a real reading that is no longer current" is a level
    /// that aged and reads `stale`. Collapsing them is what turns a stale
    /// reading into a claim that the driver never spoke.
    ///
    /// Age is deliberately not consulted: a sample can be well formed and old
    /// at the same time, and that combination is the one a record must keep.
    /// A body whose `used_bytes` exceeds its own `budget_bytes`, or whose
    /// `available_bytes` disagrees with the difference, is internally
    /// inconsistent and is refused here rather than reconciled into a figure
    /// the driver never published.
    ///
    /// [`GpuBudgetSource`] is deliberately **not** consulted either. It is not
    /// a property of the body's arithmetic: an internally consistent estimate
    /// is a well-formed sample that is simply an estimate. Whether this reader
    /// will *act* on an estimate is admission, and lives in
    /// [`Self::trusted_available_at`] beside freshness. Folding the two here
    /// is what made an estimate indistinguishable from a torn figure, so a
    /// display could only render it as the absence wording — telling an
    /// operator the provider never spoke when it published a real, if less
    /// authoritative, number. A display that wants to name the source already
    /// has it: it is a field on this record, and the well-formed answer is
    /// what lets that record exist at all.
    pub fn is_well_formed(&self) -> bool {
        let Some(available) = self.budget_bytes.checked_sub(self.used_bytes) else {
            return false;
        };
        self.schema_version == 1
            && self.adapter.is_some()
            && self
                .total_bytes
                .is_none_or(|total| self.budget_bytes <= total)
            && self.available_bytes == available
    }

    /// The one freshness formula for a wall-clock sample stamp.
    ///
    /// Shared so a derived record that carries only the stamp — the CLI
    /// monitor's budget observation — cannot drift from the admission answer
    /// on what "current" means. [`Self::is_fresh_at`] is this over the
    /// sample's own stamp.
    ///
    /// Zero timestamps mean unavailable clocks and are never current.
    /// A `now` that precedes `sampled_at_unix_ms` is a corrupt pair, not a
    /// sample taken in the future, so `checked_sub` refuses it rather than
    /// treating it as just taken.
    pub fn sample_is_fresh_at(sampled_at_unix_ms: u64, now_unix_ms: u64, max_age_ms: u64) -> bool {
        sampled_at_unix_ms != 0
            && now_unix_ms != 0
            && now_unix_ms
                .checked_sub(sampled_at_unix_ms)
                .is_some_and(|age| age <= max_age_ms)
    }

    /// `true` when this sample is still fresh at `now_unix_ms`.
    ///
    /// `max_age_ms` is the caller's freshness window. The product window is
    /// [`GPU_BUDGET_MAX_AGE_MS`]; passing anything else is a deliberate
    /// tighter test bound, not a second policy.
    pub fn is_fresh_at(&self, now_unix_ms: u64, max_age_ms: u64) -> bool {
        Self::sample_is_fresh_at(self.sampled_at_unix_ms, now_unix_ms, max_age_ms)
    }

    /// Returns headroom only for a valid, fresh, adapter-bound driver budget.
    ///
    /// This is the admission answer, and it refuses all three refusals:
    /// a malformed body, an aged sample, and a body that is not the driver's
    /// own report. The third is the reason this method is where the source
    /// check lives — [`Self::is_well_formed`] is consistency and answers the
    /// same question for an estimate as for a driver reading, so the decision
    /// to act on only one of them belongs here, beside freshness, where the
    /// answer is "may I allocate against this?". Callers that must tell
    /// "malformed" from "aged out" classify with [`Self::is_well_formed`] and
    /// [`Self::is_fresh_at`] themselves; callers that must name an estimate as
    /// an estimate read [`Self::source`].
    ///
    /// The shape is [`GpuBudgetSnapshot::can_admit_at`]'s, which has gated on
    /// the source since it was written.
    pub fn trusted_available_at(&self, now_unix_ms: u64, max_age_ms: u64) -> Option<u64> {
        let available = self.budget_bytes.checked_sub(self.used_bytes)?;
        (self.source == GpuBudgetSource::DriverReported
            && self.is_well_formed()
            && self.is_fresh_at(now_unix_ms, max_age_ms))
        .then_some(available)
    }
}

/// Adapter busy time over the driver's trailing sample window, in whole
/// percents. Unstamped: the caller owns the wall clock, exactly as it does
/// for a [`GpuBudgetSnapshot`].
///
/// This is **not** a capacity measurement. `gpu_pct` is the percent of the
/// window with at least one kernel executing; `memory_pct` is device-memory
/// **busy time** over the same window. Neither is VRAM occupancy, and neither
/// may share a meter with [`GpuBudgetTelemetry::used_bytes`].
///
/// The values are whole percents in `0..=100`. A returned zero is measured
/// idle over the window. Absence of the whole sample is a different fact and
/// publishes as `null`, never as a zero.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GpuUtilizationSnapshot {
    pub gpu_pct: u8,
    pub memory_pct: u8,
}

/// Published shape of an adapter busy-time sample: the snapshot plus the wall
/// clock of the instant it was taken.
///
/// Sibling to [`GpuBudgetTelemetry`], never nested inside it. Budget answers
/// "how much VRAM is in use"; this answers "how busy is the adapter". Reading
/// one as the other is the defect class the worker envelope keeps apart.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GpuUtilizationTelemetry {
    pub schema_version: u8,
    /// Percent of the sample window with at least one kernel executing.
    pub gpu_pct: u8,
    /// Percent of the sample window with device-memory traffic. Busy time,
    /// not VRAM capacity occupancy.
    pub memory_pct: u8,
    pub sampled_at_unix_ms: u64,
}

impl GpuUtilizationTelemetry {
    /// Stamps a provider sample with the caller's wall clock. The provider
    /// deliberately does not read a clock: one sample point, one adapter
    /// identity, one timestamp, taken by whoever collects the pair.
    pub fn from_snapshot(snapshot: &GpuUtilizationSnapshot, sampled_at_unix_ms: u64) -> Self {
        Self {
            schema_version: 1,
            gpu_pct: snapshot.gpu_pct,
            memory_pct: snapshot.memory_pct,
            sampled_at_unix_ms,
        }
    }

    /// Parse the `gpu_utilization` field of a published status document.
    ///
    /// A missing key and a JSON `null` are the same honest **not measured**
    /// report and both yield `None`; so does a field that will not parse. The
    /// caller sees `None` as absence, never as a zero-percent sample.
    pub fn from_published_status(status: &serde_json::Value) -> Option<Self> {
        serde_json::from_value(status.get("gpu_utilization")?.clone()).ok()
    }

    /// `true` when the body is usable: a known schema and percentages inside
    /// their domain.
    ///
    /// This is the malformed half of the trust decision, split out so a display
    /// can tell the two refusals apart. "The driver published something
    /// unreadable" is absence and reads `not measured`. "The driver published a
    /// real reading that is no longer current" is a level that aged and reads
    /// `stale`. Collapsing them is what turns a stale reading into a claim that
    /// the driver never spoke.
    ///
    /// Age is deliberately not consulted: a sample can be well formed and old
    /// at the same time, and that combination is the one a record must keep.
    pub fn is_well_formed(&self) -> bool {
        self.schema_version == 1 && self.gpu_pct <= 100 && self.memory_pct <= 100
    }

    /// `true` when this sample is still fresh at `now_unix_ms`.
    ///
    /// `max_age_ms` is the caller's freshness window. The product window is
    /// [`GPU_BUDGET_MAX_AGE_MS`], because the sample is taken at the same
    /// heartbeat as the budget and must not outlive it; passing anything else
    /// is a deliberate tighter test bound, not a second policy.
    ///
    /// A `now` that precedes `sampled_at_unix_ms` is a corrupt pair, not a
    /// sample taken in the future, so `checked_sub` refuses it rather than
    /// treating it as just taken.
    /// Zero clocks are unavailable, as in the shared budget freshness rule.
    pub fn is_fresh_at(&self, now_unix_ms: u64, max_age_ms: u64) -> bool {
        GpuBudgetTelemetry::sample_is_fresh_at(self.sampled_at_unix_ms, now_unix_ms, max_age_ms)
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

impl GpuBudgetSnapshot {
    /// Available bytes within the current allocation budget, clamped against malformed telemetry.
    pub fn available_bytes(&self) -> u64 {
        self.budget_bytes.saturating_sub(self.used_bytes)
    }

    /// Largest additional allocation that leaves the shared reserve and runtime
    /// headroom free within the measured capacity. Existing usage is deducted
    /// before applying the same free floor as physical allocation admission.
    /// Callers must validate source, identity, freshness and budget consistency;
    /// arithmetic alone cannot grant allocation authority.
    pub fn safe_target_bytes(
        &self,
        requested_bytes: u64,
        configured_reserve_bytes: u64,
        runtime_headroom_bytes: u64,
    ) -> u64 {
        let capacity = self
            .total_bytes
            .unwrap_or(self.budget_bytes)
            .min(self.budget_bytes);
        requested_bytes.min(capacity.saturating_sub(self.used_bytes).saturating_sub(
            self.required_free_bytes(configured_reserve_bytes, runtime_headroom_bytes),
        ))
    }

    /// Strict free floor shared by cache targets, physical allocations, broker
    /// admission and customer plans: configured/capacity reserve plus runtime
    /// headroom. A target must not spend either component.
    pub fn required_free_bytes(
        &self,
        configured_reserve_bytes: u64,
        runtime_headroom_bytes: u64,
    ) -> u64 {
        let capacity = self
            .total_bytes
            .unwrap_or(self.budget_bytes)
            .min(self.budget_bytes);
        configured_reserve_bytes
            .max(capacity.div_ceil(5))
            .saturating_add(runtime_headroom_bytes)
    }

    /// Automatic admission requires a stable adapter identity and a driver-reported budget.
    /// Backend APIs define the scope and precision of the budget and usage values.
    ///
    /// The freshness window is [`GPU_BUDGET_MAX_AGE_MS`] — the one product
    /// window, converted to the [`Duration`] the `Instant` clock is read in.
    /// It is not a second policy. A bare literal here is exactly how a window
    /// drifts away from the publisher that stamps the sample, which is why the
    /// constant lives beside this predicate and why this wrapper has no window
    /// of its own to choose.
    pub fn can_admit(&self, required_bytes: u64) -> bool {
        self.can_admit_at(
            required_bytes,
            Instant::now(),
            Duration::from_millis(GPU_BUDGET_MAX_AGE_MS),
        )
    }

    /// Checks admission against a caller-supplied clock and freshness window.
    ///
    /// `max_age` is the caller's freshness window. The product window is
    /// [`GPU_BUDGET_MAX_AGE_MS`] and reaches this through [`Self::can_admit`];
    /// passing anything else is a deliberate tighter test bound, not a second
    /// policy.
    pub fn can_admit_at(&self, required_bytes: u64, now: Instant, max_age: Duration) -> bool {
        self.adapter.is_some()
            && self.source == GpuBudgetSource::DriverReported
            && now
                .checked_duration_since(self.sampled_at)
                .is_some_and(|age| age <= max_age)
            && self
                .total_bytes
                .is_none_or(|total| self.budget_bytes <= total)
            && self.available_bytes() >= required_bytes
    }
}
