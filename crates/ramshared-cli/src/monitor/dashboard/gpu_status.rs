// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/dashboard/gpu_status.rs
//! Presentation of GPU-only telemetry faults with verified swap protection.

use serde_json::Value;

use super::super::model::Observation;

pub(super) enum GpuPanelDelay {
    Telemetry,
    CacheWorker,
}

pub(super) fn gpu_panel_delay(observation: &Observation) -> Option<GpuPanelDelay> {
    if observation.string("overall_state") != "BLOCKED"
        || observation.string("protection_state") != "READY"
        || observation.string("origin_state") != "READY"
        || observation.string("guardian_state") != "HEALTHY"
        || observation.string("control_plane_state") != "vsock_leased"
        || observation.value("order_ok").and_then(Value::as_bool) != Some(true)
        || observation.value("ghost").and_then(Value::as_bool) != Some(false)
        || !["health_errors", "reasons"].iter().all(|key| {
            observation
                .value(key)
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
        })
    {
        return None;
    }

    let errors = observation.value("measurement_errors")?.as_array()?;
    if errors.is_empty() {
        return (observation.string("cache_state") == "UNAVAILABLE")
            .then_some(GpuPanelDelay::CacheWorker);
    }
    errors
        .iter()
        .all(|error| {
            matches!(
                error.as_str(),
                Some(
                    "gpu_budget_telemetry_invalid_or_stale"
                        | "gpu_utilization_telemetry_invalid_or_stale"
                        | "gpu_cache_telemetry_invalid_or_stale"
                )
            )
        })
        .then_some(GpuPanelDelay::Telemetry)
}
