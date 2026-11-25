// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/observations/telemetry_docs.rs
//! Telemetry-document, reservation-ledger, and benchmark-evidence readers.

use std::fs;
use std::path::Path;

use ramshared_vram::{GpuBudgetTelemetry, WorkerCacheTelemetry};
use serde_json::{Map, Value};

use super::super::model::GpuObservation;
use super::super::vram_reserve_fields;
use crate::workload;

const BENCHMARK_EVIDENCE_SCHEMA_V1: &str = "ramshared-evidence/v1";
const BENCHMARK_MIN_SAMPLE_COUNT: usize = 3;

/// The qualification record was read and is promotable.
const BENCHMARK_STATUS_PASS: &str = "PASS";
/// The record was read and is not promotable: legacy, dirty, incomplete or
/// forged. The evidence exists and is simply not usable as a baseline.
pub(in crate::monitor) const BENCHMARK_STATUS_AWAITING: &str = "AWAITING_QUALIFICATION";
/// The record could not be read at all. Distinct from
/// [`BENCHMARK_STATUS_AWAITING`] on purpose: "there is no evidence here" and
/// "there is evidence here and it does not qualify" are two different operator
/// answers, and collapsing them is how a missing path reads as a work in
/// progress.
pub(in crate::monitor) const BENCHMARK_STATUS_PATH_NOT_FOUND: &str = "EVIDENCE_PATH_NOT_FOUND";

pub(in crate::monitor) fn read_benchmark_qualification(
    path: &Path,
) -> (f64, f64, f64, f64, String) {
    let awaiting = || (0.0, 0.0, 0.0, 0.0, BENCHMARK_STATUS_AWAITING.to_string());
    let not_found = || {
        (
            0.0,
            0.0,
            0.0,
            0.0,
            BENCHMARK_STATUS_PATH_NOT_FOUND.to_string(),
        )
    };
    let Ok(content) = fs::read_to_string(path) else {
        return not_found();
    };
    let Ok(json) = serde_json::from_str::<Value>(&content) else {
        return awaiting();
    };
    if !is_promotable_benchmark_evidence(&json) {
        return awaiting();
    }

    let Some(speed) = benchmark_metric_summary(&json, "reclaim_speed_gbs", "GB/s", "median") else {
        return awaiting();
    };
    let Some(duration) = benchmark_metric_summary(&json, "reclaim_duration_ms", "ms", "median")
    else {
        return awaiting();
    };
    let Some(p50) = benchmark_metric_summary(&json, "p50_cycle_latency_ms", "ms", "median") else {
        return awaiting();
    };
    let Some(p99) =
        benchmark_metric_summary(&json, "p99_cycle_latency_ms", "ms", "p99_nearest_rank")
    else {
        return awaiting();
    };

    (speed, duration, p50, p99, BENCHMARK_STATUS_PASS.to_string())
}

fn is_promotable_benchmark_evidence(json: &Value) -> bool {
    let source = &json["source"];
    let workload = &json["workload"];
    let comparison = &json["comparison"];
    let lifecycle = &json["lifecycle"];
    let decision = &json["decision"];
    let artifacts = json["artifacts"].as_array();
    let refusals = lifecycle["refusals"].as_array();

    json["schema_version"].as_str() == Some(BENCHMARK_EVIDENCE_SCHEMA_V1)
        && json["run_id"]
            .as_str()
            .is_some_and(|run_id| !run_id.is_empty())
        && source["commit"]
            .as_str()
            .is_some_and(|commit| !commit.is_empty())
        && source["dirty"].as_bool() == Some(false)
        && source["dirty_entry_count"].as_u64() == Some(0)
        && json["candidate"]
            .as_object()
            .is_some_and(|candidate| !candidate.is_empty())
        && workload["runs"].as_u64().is_some_and(|runs| runs >= 3)
        && comparison["qualified"].as_bool() == Some(true)
        && lifecycle["binary_match"].as_bool() == Some(true)
        && lifecycle["legitimate"]["verdict"].as_str() == Some("PASS")
        && refusals.is_some_and(|entries| {
            !entries.is_empty()
                && entries
                    .iter()
                    .all(|entry| entry["verdict"].as_str() == Some("PASS"))
        })
        && lifecycle["cleanup"]["complete"].as_bool() == Some(true)
        && lifecycle["residue"].as_u64() == Some(0)
        && artifacts.is_some_and(|entries| !entries.is_empty())
        && decision["verdict"].as_str() == Some("PASS")
        && decision["promotable"].as_bool() == Some(true)
}

fn benchmark_metric_summary(
    json: &Value,
    metric_name: &str,
    expected_unit: &str,
    summary_key: &str,
) -> Option<f64> {
    let metric = json.get("metrics")?.get(metric_name)?;
    if metric.get("unit")?.as_str()? != expected_unit {
        return None;
    }
    let samples = metric.get("samples")?.as_array()?;
    if samples.len() < BENCHMARK_MIN_SAMPLE_COUNT
        || metric.get("n")?.as_u64()? != u64::try_from(samples.len()).ok()?
    {
        return None;
    }

    let mut values = samples
        .iter()
        .map(Value::as_f64)
        .collect::<Option<Vec<_>>>()?;
    if values
        .iter()
        .any(|value| !value.is_finite() || *value <= 0.0)
    {
        return None;
    }
    values.sort_by(f64::total_cmp);

    let recomputed = match summary_key {
        "median" => {
            let middle = values.len() / 2;
            if values.len() % 2 == 0 {
                (values[middle - 1] + values[middle]) / 2.0
            } else {
                values[middle]
            }
        }
        "p99_nearest_rank" => {
            let rank = ((values.len() as f64) * 0.99).ceil() as usize;
            values[rank.max(1).min(values.len()) - 1]
        }
        _ => return None,
    };
    let recorded = metric.get(summary_key)?.as_f64()?;
    let tolerance = recomputed.abs().max(1.0) * 1.0e-9;
    if !recorded.is_finite() || (recorded - recomputed).abs() > tolerance {
        return None;
    }

    Some(recomputed)
}

pub(in crate::monitor) fn read_reservation_totals(path: &Path) -> (u64, u64) {
    workload::read_reservation_ledger(path).map_or_else(
        |_| (0, 0),
        |reservations| {
            let reserved_bytes = reservations
                .iter()
                .map(|reservation| reservation.memory_bytes)
                .sum();
            (reservations.len() as u64, reserved_bytes)
        },
    )
}

/// Parses the published `gpu_budget` field as a **record**, never as an
/// absence marker.
///
/// A sample that merely aged out is still the sample the worker produced and
/// is returned as itself, carrying its own `sampled_at_unix_ms`. Collapsing it
/// here is what told the operator the driver never spoke when it spoke
/// seconds ago — the same defect class the cache and busy-time records
/// already refuse. The gate lives at the renderer, which spells the two
/// refusals apart: a body this reader cannot use reads as the panel's absence
/// wording, a body that is no longer current reads `stale`.
///
/// `None` is therefore only "no body this panel can show": a missing or
/// `null` field (the daemon's honest never-bound report), a field that will
/// not parse, a body [`GpuBudgetTelemetry::is_well_formed`] refuses, or an
/// adapter with no usable identity. None of those is an aged reading, and
/// none of them is a measured zero.
///
/// A measured zero budget is **kept**. The driver spoke: it reported no
/// allocatable capacity, which is a product state and not silence. What such
/// a sample cannot carry is an occupancy *ratio*, because a fullness against
/// a denominator of zero is not a quantity — that refusal lives where the
/// ratio is derived, in [`GpuObservation::used_percent_of_budget`], and it
/// refuses only the ratio. The figures the driver did publish (zero
/// budget, zero used, and any physical total beside them) are still figures.
/// Dropping the whole sample here published a measured zero as the panel's
/// absence wording, which is the conflation the aged-budget rule above
/// already refuses in the other direction.
///
/// Deliberately not [`GpuBudgetTelemetry::trusted_available_at`], which is
/// the admission collapse and wants exactly "present but not trustworthy"
/// for the paths that allocate against it. This is the display record.
pub(in crate::monitor) fn gpu_observation_from_status(
    status: &Map<String, Value>,
) -> Option<GpuObservation> {
    let telemetry =
        serde_json::from_value::<GpuBudgetTelemetry>(status.get("gpu_budget")?.clone()).ok()?;
    if !telemetry.is_well_formed() {
        return None;
    }
    let adapter = telemetry.adapter.as_ref()?;
    // An adapter this reader cannot name is absence, not an aged reading and
    // not a measured zero: the row is about *which* device the budget belongs
    // to, and an unnamed one cannot appear as a named level.
    if adapter.backend.trim().is_empty() || adapter.key.trim().is_empty() {
        return None;
    }
    // Well-formedness already pinned `available_bytes == budget - used`, so
    // this is the one headroom figure the driver published — not a
    // recomputation that could disagree with it.
    let available_bytes = telemetry.budget_bytes - telemetry.used_bytes;

    let (
        vram_reserve_enforced_mib,
        vram_reserve_source,
        vram_reserve_sealed_min_mib,
        vram_reserve_sealed_percent,
        vram_reserve_runtime_buffer_mib,
    ) = vram_reserve_fields(telemetry.total_bytes, telemetry.budget_bytes);

    Some(GpuObservation {
        adapter: adapter.clone(),
        source: telemetry.source,
        sampled_at_unix_ms: telemetry.sampled_at_unix_ms,
        // Bytes, exactly as the telemetry carries them. The whole-MiB
        // publish conversion is a view on the record and must not be
        // precomputed here: floor-dividing at construction is what collapses
        // a present sub-MiB reading into a measured zero and moves the
        // occupancy percent off the true ratio.
        total_bytes: telemetry.total_bytes,
        budget_bytes: telemetry.budget_bytes,
        used_bytes: telemetry.used_bytes,
        free_bytes: available_bytes,
        vram_reserve_enforced_mib,
        vram_reserve_source,
        vram_reserve_sealed_min_mib,
        vram_reserve_sealed_percent,
        vram_reserve_runtime_buffer_mib,
    })
}

/// Parses the published cache telemetry as a **record**, never as an absence
/// marker (DT-8).
///
/// A sample that merely aged out is still the sample the worker produced and
/// is returned as itself. Collapsing it here is what told the operator the
/// worker never spoke when it spoke seconds ago — the same defect class the
/// budget and the busy-time records already refuse. The gate lives at the
/// renderer, which spells the two refusals apart: a body this reader cannot
/// use reads `not measured`, a body that is no longer current reads `stale`.
///
/// Deliberately not [`crate::published_budget::trusted_cache_field`], which is
/// the admission collapse and wants exactly "present but not trustworthy" for
/// the alarm built on it.
pub(in crate::monitor) fn cache_telemetry_record(
    value: Option<&Value>,
) -> Option<WorkerCacheTelemetry> {
    let field = value?;
    if field.is_null() {
        return None;
    }
    serde_json::from_value::<WorkerCacheTelemetry>(field.clone()).ok()
}
