// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/supervisor_status.rs
//! Strict shape validation for the supervisor snapshot consumed by cascade health.

use super::SUPERVISOR_ACTION_ERROR_MAX_BYTES;
use serde_json::{Map, Value};

type Status = Map<String, Value>;

pub(super) fn supervisor_status_shape_is_valid(value: &Value) -> bool {
    let Some(status) = value.as_object() else {
        return false;
    };
    if !valid_envelope(status)
        || !valid_daemon_identity(status)
        || !valid_supervisor_identity(status)
    {
        return false;
    }
    let Some(state) = status.get("control_state").and_then(Value::as_str) else {
        return false;
    };
    let Some(actions) = ordered_actions(status) else {
        return false;
    };
    valid_state_actions(state, &actions)
}

fn valid_envelope(status: &Status) -> bool {
    const KEYS: &[&str] = &[
        "schema_version",
        "control_state",
        "healthy_samples",
        "action_results",
        "daemon_instance_id",
        "supervisor_identity",
        "written_at_unix_ms",
    ];
    status.len() == KEYS.len()
        && KEYS.iter().all(|key| status.contains_key(*key))
        && status.get("schema_version").and_then(Value::as_u64) == Some(3)
        && status
            .get("healthy_samples")
            .and_then(Value::as_u64)
            .is_some_and(|samples| samples <= u64::from(u32::MAX))
        && status
            .get("written_at_unix_ms")
            .and_then(Value::as_u64)
            .is_some()
}

fn valid_daemon_identity(status: &Status) -> bool {
    match status.get("daemon_instance_id") {
        Some(Value::Null) => true,
        Some(Value::String(value)) => {
            !value.is_empty()
                && value.len() <= 128
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        }
        _ => false,
    }
}

fn valid_supervisor_identity(status: &Status) -> bool {
    let Some(identity) = status.get("supervisor_identity").and_then(Value::as_object) else {
        return false;
    };
    const KEYS: &[&str] = &["boot_id", "pid", "start_time", "nonce"];
    identity.len() == KEYS.len()
        && KEYS.iter().all(|key| identity.contains_key(*key))
        && ["boot_id", "nonce"].iter().all(|key| {
            identity
                .get(*key)
                .and_then(Value::as_str)
                .is_some_and(|value| !value.is_empty())
        })
        && identity
            .get("pid")
            .and_then(Value::as_u64)
            .is_some_and(|pid| pid > 0 && pid <= u64::from(u32::MAX))
        && identity
            .get("start_time")
            .and_then(Value::as_u64)
            .is_some_and(|time| time > 0)
}

fn ordered_actions(status: &Status) -> Option<Vec<&str>> {
    let results = status.get("action_results")?.as_array()?;
    if results.len() > 7 {
        return None;
    }
    let mut actions = Vec::with_capacity(results.len());
    for result in results {
        let result = result.as_object()?;
        const KEYS: &[&str] = &["action", "status", "error"];
        if result.len() != KEYS.len() || KEYS.iter().any(|key| !result.contains_key(*key)) {
            return None;
        }
        let action = result.get("action")?.as_str()?;
        if !matches!(
            action,
            "close_admission"
                | "reduce_vram_cache"
                | "freeze_discardable"
                | "thaw_discardable"
                | "request_reclaim"
                | "terminate_discardable"
                | "kill_discardable"
        ) || actions.contains(&action)
        {
            return None;
        }
        let valid = match result.get("status").and_then(Value::as_str) {
            Some("succeeded") => result.get("error").is_some_and(Value::is_null),
            Some("failed") => result
                .get("error")
                .and_then(Value::as_str)
                .is_some_and(|error| {
                    !error.is_empty() && error.len() <= SUPERVISOR_ACTION_ERROR_MAX_BYTES
                }),
            _ => false,
        };
        if !valid {
            return None;
        }
        actions.push(action);
    }
    Some(actions)
}

fn valid_state_actions(state: &str, actions: &[&str]) -> bool {
    match state {
        "HEALTHY" | "SAFE_MODE" => actions.is_empty(),
        "GUARDED" => matches!(
            actions,
            ["close_admission"] | ["thaw_discardable", "close_admission"]
        ),
        "CRITICAL" => matches!(
            actions,
            ["close_admission", "freeze_discardable", "request_reclaim"]
                | ["close_admission", "request_reclaim"]
                | [
                    "close_admission",
                    "reduce_vram_cache",
                    "freeze_discardable",
                    "request_reclaim"
                ]
                | ["close_admission", "reduce_vram_cache", "request_reclaim"]
        ),
        "EMERGENCY" => matches!(
            actions,
            ["close_admission", "reduce_vram_cache", "request_reclaim"]
                | [
                    "close_admission",
                    "reduce_vram_cache",
                    "request_reclaim",
                    "terminate_discardable"
                ]
                | [
                    "close_admission",
                    "reduce_vram_cache",
                    "request_reclaim",
                    "kill_discardable"
                ]
                | [
                    "thaw_discardable",
                    "close_admission",
                    "reduce_vram_cache",
                    "request_reclaim"
                ]
                | [
                    "thaw_discardable",
                    "close_admission",
                    "reduce_vram_cache",
                    "request_reclaim",
                    "terminate_discardable"
                ]
                | [
                    "thaw_discardable",
                    "close_admission",
                    "reduce_vram_cache",
                    "request_reclaim",
                    "kill_discardable"
                ]
        ),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use serde_json::json;

    fn repeated_critical() -> serde_json::Value {
        json!({
            "schema_version": 3, "control_state": "CRITICAL", "healthy_samples": 0,
            "daemon_instance_id": "42-123", "written_at_unix_ms": 1000,
            "supervisor_identity": {"boot_id": "fixture-boot", "pid": 42, "start_time": 100, "nonce": "fixture-nonce"},
            "action_results": [
                {"action": "close_admission", "status": "succeeded", "error": null},
                {"action": "reduce_vram_cache", "status": "succeeded", "error": null},
                {"action": "request_reclaim", "status": "succeeded", "error": null}
            ]
        })
    }

    #[test]
    fn repeated_critical_frame_without_reissuing_applied_freeze_is_valid() {
        assert!(supervisor_status_shape_is_valid(&repeated_critical()));
    }

    #[test]
    fn all_supported_state_sequences_remain_valid_and_ordered() {
        for (state, actions) in [
            ("HEALTHY", vec![]),
            ("SAFE_MODE", vec![]),
            ("GUARDED", vec!["close_admission"]),
            ("GUARDED", vec!["thaw_discardable", "close_admission"]),
            (
                "CRITICAL",
                vec![
                    "close_admission",
                    "reduce_vram_cache",
                    "freeze_discardable",
                    "request_reclaim",
                ],
            ),
            (
                "EMERGENCY",
                vec!["close_admission", "reduce_vram_cache", "request_reclaim"],
            ),
            (
                "EMERGENCY",
                vec![
                    "close_admission",
                    "reduce_vram_cache",
                    "request_reclaim",
                    "terminate_discardable",
                ],
            ),
            (
                "EMERGENCY",
                vec![
                    "close_admission",
                    "reduce_vram_cache",
                    "request_reclaim",
                    "kill_discardable",
                ],
            ),
            (
                "EMERGENCY",
                vec![
                    "thaw_discardable",
                    "close_admission",
                    "reduce_vram_cache",
                    "request_reclaim",
                ],
            ),
            (
                "EMERGENCY",
                vec![
                    "thaw_discardable",
                    "close_admission",
                    "reduce_vram_cache",
                    "request_reclaim",
                    "terminate_discardable",
                ],
            ),
            (
                "EMERGENCY",
                vec![
                    "thaw_discardable",
                    "close_admission",
                    "reduce_vram_cache",
                    "request_reclaim",
                    "kill_discardable",
                ],
            ),
        ] {
            let mut frame = repeated_critical();
            frame["control_state"] = json!(state);
            frame["action_results"] = json!(
                actions
                    .iter()
                    .map(|action| json!({"action":action,"status":"succeeded","error":null}))
                    .collect::<Vec<_>>()
            );
            assert!(supervisor_status_shape_is_valid(&frame), "{state}");
            frame["action_results"] =
                json!([{"action":"request_reclaim","status":"succeeded","error":null}]);
            assert!(
                !supervisor_status_shape_is_valid(&frame),
                "incomplete {state}"
            );
        }
    }

    #[test]
    fn malformed_envelope_identity_and_outcomes_are_refused() {
        assert!(!supervisor_status_shape_is_valid(&Value::Null));
        for (key, invalid) in [
            ("schema_version", json!(true)),
            ("healthy_samples", json!(u64::MAX)),
            ("written_at_unix_ms", json!("1000")),
            ("daemon_instance_id", json!("foreign/path")),
            ("daemon_instance_id", json!(false)),
            ("supervisor_identity", Value::Null),
            ("control_state", json!(false)),
            ("control_state", json!("UNKNOWN")),
            ("action_results", Value::Null),
            ("action_results", json!([null])),
        ] {
            let mut frame = repeated_critical();
            frame[key] = invalid;
            assert!(!supervisor_status_shape_is_valid(&frame), "{key}");
        }
        let mut frame = repeated_critical();
        frame["daemon_instance_id"] = Value::Null;
        assert!(supervisor_status_shape_is_valid(&frame));
        for (key, invalid) in [
            ("pid", json!(0)),
            ("start_time", json!(0)),
            ("nonce", json!("")),
            ("boot_id", Value::Null),
        ] {
            let mut frame = repeated_critical();
            frame["supervisor_identity"][key] = invalid;
            assert!(!supervisor_status_shape_is_valid(&frame));
        }
        for key in ["action", "status", "error"] {
            let mut frame = repeated_critical();
            frame["action_results"][0]
                .as_object_mut()
                .unwrap()
                .remove(key);
            assert!(!supervisor_status_shape_is_valid(&frame));
        }
        let mut frame = repeated_critical();
        frame["action_results"] = json!(vec![
            json!({"action":"close_admission","status":"succeeded","error":null});
            8
        ]);
        assert!(!supervisor_status_shape_is_valid(&frame));
        let mut frame = repeated_critical();
        frame["action_results"][0]["status"] = json!("pending");
        assert!(!supervisor_status_shape_is_valid(&frame));
    }
}
