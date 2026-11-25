// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/supervisor/actions.rs
//! Victim selection, runtime request/target persistence, and systemd unit actions.

use super::*;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct VictimIdentity {
    pub(super) unit: String,
    pub(super) invocation_id: String,
}

pub(super) fn select_victim(path: &Path) -> Result<Option<VictimIdentity>, String> {
    let mut candidates = workload::read_reservation_ledger(path)?
        .into_iter()
        .filter_map(|reservation| {
            let priority = discard_priority(reservation.class.as_str())?;
            let invocation_id = reservation
                .invocation_id
                .filter(|value| workload::valid_systemd_invocation_id(value))?;
            valid_scope_unit(&reservation.unit).then_some((
                priority,
                std::cmp::Reverse(reservation.memory_bytes),
                reservation.issued_ordinal,
                reservation.unit,
                invocation_id,
            ))
        })
        .collect::<Vec<_>>();
    candidates.sort();
    Ok(candidates
        .into_iter()
        .next()
        .map(|(_, _, _, unit, invocation_id)| VictimIdentity {
            unit,
            invocation_id,
        }))
}

pub(super) fn required_victim<'a>(
    victim: &'a Result<Option<VictimIdentity>, String>,
    missing_reason: &'static str,
) -> Result<&'a VictimIdentity, String> {
    match victim {
        Ok(Some(victim)) => Ok(victim),
        Ok(None) => Err(missing_reason.into()),
        Err(error) => Err(error.clone()),
    }
}

pub(super) fn write_runtime_request_at(
    parent: &Path,
    name: &str,
    value: serde_json::Value,
) -> Result<(), String> {
    invoke_runtime_write_hook(name, &value)?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let path = parent.join(name);
    let nonce = RUNTIME_FILE_NONCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(".{name}.{}-{nonce}.tmp", std::process::id()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|error| error.to_string())?;
        file.write_all(format!("{value}\n").as_bytes())
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        fs::rename(&temporary, &path).map_err(|error| error.to_string())?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| error.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub(super) fn remove_runtime_record_at(parent: &Path, name: &str) -> Result<(), String> {
    fs::remove_file(parent.join(name)).map_err(|error| error.to_string())?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| error.to_string())
}

pub(super) fn read_runtime_record_at<T>(parent: &Path, name: &str) -> Result<Option<T>, String>
where
    T: serde::de::DeserializeOwned,
{
    const MAX_RUNTIME_RECORD_BYTES: u64 = 16 * 1024;
    let path = parent.join(name);
    let opened = match rustix::fs::open(
        &path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    ) {
        Ok(opened) => opened,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => return Err(format!("open {name} without following links: {error}")),
    };
    let mut file = File::from(opened);
    let before = file
        .metadata()
        .map_err(|error| format!("inspect {name}: {error}"))?;
    if !before.is_file()
        || before.nlink() != 1
        || before.uid() != rustix::process::geteuid().as_raw()
        || before.mode() & 0o022 != 0
        || before.len() > MAX_RUNTIME_RECORD_BYTES
    {
        return Err(format!("{name} has unsafe metadata"));
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_RUNTIME_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {name}: {error}"))?;
    if bytes.len() as u64 > MAX_RUNTIME_RECORD_BYTES {
        return Err(format!("{name} exceeds its bounded size"));
    }
    let after = file
        .metadata()
        .map_err(|error| format!("reinspect {name}: {error}"))?;
    let named =
        fs::symlink_metadata(&path).map_err(|error| format!("reinspect named {name}: {error}"))?;
    if !named.is_file()
        || before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.len() != after.len()
        || before.dev() != named.dev()
        || before.ino() != named.ino()
        || before.len() != named.len()
    {
        return Err(format!("{name} identity or size changed during read"));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| format!("{name} is malformed"))
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum FrozenPhase {
    Pending,
    Applied,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct FrozenTargetRecord {
    pub(super) schema_version: u32,
    pub(super) phase: FrozenPhase,
    pub(super) unit: String,
    pub(super) invocation_id: String,
    pub(super) updated_at_unix_ms: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum EmergencyPhase {
    PendingTerm,
    Termed,
    Killed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct EmergencyTargetRecord {
    pub(super) schema_version: u32,
    pub(super) phase: EmergencyPhase,
    pub(super) unit: String,
    pub(super) invocation_id: String,
    pub(super) term_succeeded_at_unix_ms: Option<u64>,
    pub(super) updated_at_unix_ms: u64,
}

pub(super) fn validate_record_victim(
    unit: String,
    invocation_id: String,
) -> Result<VictimIdentity, String> {
    if !valid_scope_unit(&unit) || !workload::valid_systemd_invocation_id(&invocation_id) {
        return Err("durable scope target identity is invalid".into());
    }
    Ok(VictimIdentity {
        unit,
        invocation_id,
    })
}

pub(super) fn read_frozen_target(
    runtime: &Path,
) -> Result<Option<(FrozenPhase, VictimIdentity)>, String> {
    let Some(record) = read_runtime_record_at::<FrozenTargetRecord>(runtime, "frozen-scope.json")?
    else {
        return Ok(None);
    };
    if record.schema_version != 3 || record.updated_at_unix_ms == 0 {
        return Err("frozen scope evidence has an unsupported schema or timestamp".into());
    }
    Ok(Some((
        record.phase,
        validate_record_victim(record.unit, record.invocation_id)?,
    )))
}

pub(super) fn write_frozen_target(
    runtime: &Path,
    phase: FrozenPhase,
    victim: &VictimIdentity,
    now_ms: u64,
) -> Result<(), String> {
    write_runtime_request_at(
        runtime,
        "frozen-scope.json",
        serde_json::to_value(FrozenTargetRecord {
            schema_version: 3,
            phase,
            unit: victim.unit.clone(),
            invocation_id: victim.invocation_id.clone(),
            updated_at_unix_ms: now_ms,
        })
        .map_err(|error| error.to_string())?,
    )
}

pub(super) fn read_emergency_target(
    runtime: &Path,
) -> Result<Option<EmergencyTargetRecord>, String> {
    let Some(record) =
        read_runtime_record_at::<EmergencyTargetRecord>(runtime, "emergency-target.json")?
    else {
        return Ok(None);
    };
    if record.schema_version != 1
        || record.updated_at_unix_ms == 0
        || validate_record_victim(record.unit.clone(), record.invocation_id.clone()).is_err()
        || matches!(record.phase, EmergencyPhase::PendingTerm)
            && record.term_succeeded_at_unix_ms.is_some()
        || matches!(
            record.phase,
            EmergencyPhase::Termed | EmergencyPhase::Killed
        ) && record.term_succeeded_at_unix_ms.is_none()
    {
        return Err("emergency target evidence is invalid".into());
    }
    Ok(Some(record))
}

pub(super) fn emergency_record_victim(
    record: &EmergencyTargetRecord,
) -> Result<VictimIdentity, String> {
    validate_record_victim(record.unit.clone(), record.invocation_id.clone())
}

pub(super) fn write_emergency_target(
    runtime: &Path,
    phase: EmergencyPhase,
    victim: &VictimIdentity,
    term_succeeded_at_unix_ms: Option<u64>,
    now_ms: u64,
) -> Result<(), String> {
    write_runtime_request_at(
        runtime,
        "emergency-target.json",
        serde_json::to_value(EmergencyTargetRecord {
            schema_version: 1,
            phase,
            unit: victim.unit.clone(),
            invocation_id: victim.invocation_id.clone(),
            term_succeeded_at_unix_ms,
            updated_at_unix_ms: now_ms,
        })
        .map_err(|error| error.to_string())?,
    )
}

pub(super) fn unix_time_ms() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .map_err(|error| format!("system clock is before the Unix epoch: {error}"))
}

pub(super) fn valid_daemon_instance_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

pub(super) fn request_daemon_instance_at(runtime: &Path, now_ms: u64) -> Result<String, String> {
    let path = runtime.join("cache-status.json");
    // The one reader for a published status document: descriptor-bound,
    // `O_NOFOLLOW`, owner-matches-parent, neither side writable by a peer. A
    // bare `read_to_string` here takes a substituted `daemon_instance_id` from
    // anyone who can write the publication point and puts it in a critical
    // control request. Missing, malformed and untrusted are one refusal —
    // none of them is a document this caller may name.
    let value = read_status_document(&path)
        .ok_or("cache status is missing, unreadable, or not a trusted publication")?;
    let schema_version = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .ok_or("cache status schema is missing")?;
    let daemon_instance_id = value
        .get("daemon_instance_id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| valid_daemon_instance_id(value))
        .ok_or("cache status daemon instance is missing")?;
    let written_at_ms = value
        .get("written_at_unix_ms")
        .and_then(serde_json::Value::as_u64)
        .ok_or("cache status freshness is missing")?;
    if schema_version != 1
        || now_ms < written_at_ms
        || now_ms.saturating_sub(written_at_ms) > CONTROL_REQUEST_MAX_AGE_MS
    {
        return Err("cache status is stale or invalid".into());
    }
    Ok(daemon_instance_id.to_string())
}

pub(super) fn critical_request_value(
    daemon_instance_id: &str,
    issued_at_unix_ms: u64,
    target_bytes: Option<u64>,
) -> serde_json::Value {
    let mut request = serde_json::json!({
        "schema_version": 1,
        "reason": "control_pressure",
        "daemon_instance_id": daemon_instance_id,
        "issued_at_unix_ms": issued_at_unix_ms,
    });
    if let Some(target_bytes) = target_bytes {
        request["target_bytes"] = serde_json::json!(target_bytes);
    }
    request
}

pub(super) fn run_systemctl_bounded(args: &[&str]) -> Result<(), String> {
    run_systemctl_bounded_for(Path::new("systemctl"), args, Duration::from_secs(1))
}

pub(super) fn run_systemctl_bounded_for(
    command: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<(), String> {
    let mut command = Command::new(command);
    command.args(args);
    let output = bounded_process::run_capture_command(
        &mut command,
        "systemctl action",
        timeout,
        bounded_process::DEFAULT_OUTPUT_LIMIT,
        |_| {},
    )
    .map_err(|error| format!("bounded systemctl action failed: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            Err(format!("systemctl exited with {}", output.status))
        } else {
            Err(format!("systemctl exited with {}: {stderr}", output.status))
        }
    }
}

pub(super) trait UnitActionRunner {
    fn current_invocation_id(&self, unit: &str) -> Result<String, String>;
    fn run(&self, args: &[&str]) -> Result<(), String>;
}

pub(super) struct SystemUnitActionRunner;

impl UnitActionRunner for SystemUnitActionRunner {
    fn current_invocation_id(&self, unit: &str) -> Result<String, String> {
        query_unit_invocation_id(unit)
    }

    fn run(&self, args: &[&str]) -> Result<(), String> {
        run_systemctl_bounded(args)
    }
}

pub(super) fn parse_unit_invocation_id(unit: &str, output: &str) -> Result<String, String> {
    let mut id = None;
    let mut invocation_id = None;
    for line in output.lines() {
        let (name, value) = line
            .split_once('=')
            .ok_or("malformed systemd identity response")?;
        let target = match name {
            "Id" => &mut id,
            "InvocationID" => &mut invocation_id,
            _ => return Err("unexpected systemd identity field".into()),
        };
        if target.replace(value.to_string()).is_some() {
            return Err("duplicate systemd identity field".into());
        }
    }
    if id.as_deref() != Some(unit) {
        return Err("systemd unit identity changed".into());
    }
    invocation_id
        .filter(|value| workload::valid_systemd_invocation_id(value))
        .ok_or_else(|| "systemd InvocationID is missing or invalid".into())
}

pub(super) fn query_unit_invocation_id(unit: &str) -> Result<String, String> {
    if !valid_scope_unit(unit) {
        return Err("invalid managed scope unit".into());
    }
    let mut command = Command::new("systemctl");
    command.args(["show", "--property=Id", "--property=InvocationID", unit]);
    let output = bounded_process::run_capture_command(
        &mut command,
        "systemd identity query",
        SYSTEMCTL_IDENTITY_TIMEOUT,
        bounded_process::DEFAULT_OUTPUT_LIMIT,
        |_| {},
    )
    .map_err(|error| format!("bounded systemd identity query failed: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "systemd identity query exited with {}",
            output.status
        ));
    }
    let output = String::from_utf8(output.stdout)
        .map_err(|error| format!("systemd identity query returned non-UTF-8 output: {error}"))?;
    parse_unit_invocation_id(unit, &output)
}

pub(super) fn run_identity_bound_action(
    runner: &dyn UnitActionRunner,
    victim: &VictimIdentity,
    args: &[&str],
) -> Result<(), String> {
    let current = runner.current_invocation_id(&victim.unit)?;
    if current != victim.invocation_id {
        return Err("managed scope InvocationID changed; action refused".into());
    }
    runner.run(args)
}

pub(super) struct ActionPaths<'a> {
    pub(super) runtime: &'a Path,
    pub(super) ledger: &'a Path,
    pub(super) daemon_instance_id: Option<&'a str>,
    pub(super) issued_at_unix_ms: Option<u64>,
}

pub(super) fn execute_freeze(
    paths: &ActionPaths<'_>,
    selected: &Result<Option<VictimIdentity>, String>,
    runner: &dyn UnitActionRunner,
) -> Result<(), String> {
    let now_ms = paths
        .issued_at_unix_ms
        .ok_or("freeze evidence timestamp is unavailable")?;
    let existing = read_frozen_target(paths.runtime)?;
    if matches!(&existing, Some((FrozenPhase::Applied, _))) {
        return Ok(());
    }
    let victim = match existing {
        Some((FrozenPhase::Pending, victim)) => victim,
        Some((FrozenPhase::Applied, _)) => unreachable!("applied freeze returned above"),
        None => required_victim(selected, "no_discardable_scope")?.clone(),
    };
    write_frozen_target(paths.runtime, FrozenPhase::Pending, &victim, now_ms)?;
    run_identity_bound_action(runner, &victim, &["freeze", &victim.unit])?;
    write_frozen_target(paths.runtime, FrozenPhase::Applied, &victim, now_ms)
}

pub(super) fn execute_thaw(
    paths: &ActionPaths<'_>,
    runner: &dyn UnitActionRunner,
) -> Result<(), String> {
    let Some((_, victim)) = read_frozen_target(paths.runtime)? else {
        return Err("no_frozen_scope".into());
    };
    run_identity_bound_action(runner, &victim, &["thaw", &victim.unit])?;
    remove_runtime_record_at(paths.runtime, "frozen-scope.json")
}

pub(super) fn execute_term(
    paths: &ActionPaths<'_>,
    selected: &Result<Option<VictimIdentity>, String>,
    runner: &dyn UnitActionRunner,
) -> Result<(), String> {
    let now_ms = paths
        .issued_at_unix_ms
        .ok_or("TERM outcome timestamp is unavailable")?;
    let existing = read_emergency_target(paths.runtime)?;
    let victim = match existing.as_ref() {
        Some(record) if record.phase == EmergencyPhase::Termed => return Ok(()),
        Some(record) if record.phase == EmergencyPhase::Killed => {
            return Err("emergency target is already durably killed".into());
        }
        Some(record) => emergency_record_victim(record)?,
        None => required_victim(selected, "no_discardable_scope")?.clone(),
    };
    write_emergency_target(
        paths.runtime,
        EmergencyPhase::PendingTerm,
        &victim,
        None,
        now_ms,
    )?;
    run_identity_bound_action(
        runner,
        &victim,
        &["kill", "--kill-whom=all", "--signal=TERM", &victim.unit],
    )?;
    write_emergency_target(
        paths.runtime,
        EmergencyPhase::Termed,
        &victim,
        Some(now_ms),
        now_ms,
    )
}

pub(super) fn execute_kill(
    paths: &ActionPaths<'_>,
    runner: &dyn UnitActionRunner,
) -> Result<(), String> {
    let now_ms = paths
        .issued_at_unix_ms
        .ok_or("KILL outcome timestamp is unavailable")?;
    let record = read_emergency_target(paths.runtime)?
        .ok_or("no successfully terminated emergency target")?;
    if record.phase == EmergencyPhase::Killed {
        return Ok(());
    }
    if record.phase != EmergencyPhase::Termed {
        return Err("emergency target has no successful TERM outcome".into());
    }
    let term_succeeded_at = record
        .term_succeeded_at_unix_ms
        .ok_or("emergency target TERM timestamp is unavailable")?;
    if now_ms < term_succeeded_at.saturating_add(EMERGENCY_TERM_GRACE_MS) {
        return Err("emergency target TERM grace has not elapsed".into());
    }
    let victim = emergency_record_victim(&record)?;
    run_identity_bound_action(
        runner,
        &victim,
        &["kill", "--kill-whom=all", "--signal=KILL", &victim.unit],
    )?;
    write_emergency_target(
        paths.runtime,
        EmergencyPhase::Killed,
        &victim,
        Some(term_succeeded_at),
        now_ms,
    )
}

pub(super) fn execute_actions_with<F>(
    decision: &SupervisorDecision,
    paths: &ActionPaths<'_>,
    runner: &dyn UnitActionRunner,
    mut close_admission: F,
) -> Vec<SupervisorActionResult>
where
    F: FnMut() -> Result<(), String>,
{
    let victim = select_victim(paths.ledger);
    let mut outcomes = Vec::with_capacity(decision.actions.len());
    for action in &decision.actions {
        let result = match action {
            SupervisorAction::CloseAdmission => close_admission(),
            SupervisorAction::ReduceVramCache => paths
                .daemon_instance_id
                .zip(paths.issued_at_unix_ms)
                .ok_or_else(|| "daemon_instance_unavailable".to_string())
                .and_then(|(daemon_instance_id, issued_at_unix_ms)| {
                    write_runtime_request_at(
                        paths.runtime,
                        "cache-target.json",
                        critical_request_value(daemon_instance_id, issued_at_unix_ms, Some(0)),
                    )
                }),
            SupervisorAction::RequestReclaim => paths
                .daemon_instance_id
                .zip(paths.issued_at_unix_ms)
                .ok_or_else(|| "daemon_instance_unavailable".to_string())
                .and_then(|(daemon_instance_id, issued_at_unix_ms)| {
                    write_runtime_request_at(
                        paths.runtime,
                        "reclaim-request.json",
                        critical_request_value(daemon_instance_id, issued_at_unix_ms, None),
                    )
                }),
            SupervisorAction::FreezeDiscardable => execute_freeze(paths, &victim, runner),
            SupervisorAction::ThawDiscardable => execute_thaw(paths, runner),
            SupervisorAction::TerminateDiscardable => execute_term(paths, &victim, runner),
            SupervisorAction::KillDiscardable => execute_kill(paths, runner),
        };
        outcomes.push(SupervisorActionResult::from_result(*action, result));
    }
    outcomes
}
