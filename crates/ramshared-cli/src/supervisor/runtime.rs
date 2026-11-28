// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/supervisor/runtime.rs
//! Pressure sampling loop, status publication, and decision application.

use super::*;

pub(super) fn parse_meminfo(text: &str) -> Option<(u64, u64)> {
    let value = |name: &str| {
        text.lines().find_map(|line| {
            let (key, rest) = line.split_once(':')?;
            (key == name)
                .then(|| rest.split_whitespace().next()?.parse::<u64>().ok())
                .flatten()
        })
    };
    Some((value("MemTotal")? * 1024, value("MemAvailable")? * 1024))
}

pub(crate) fn parse_psi_full_avg10(text: &str) -> Option<f64> {
    let mut full_rows = 0;
    let mut result = None;
    for line in text.lines().filter(|line| line.starts_with("full ")) {
        full_rows += 1;
        let mut avg10_count = 0;
        for field in line.split_whitespace() {
            let Some(value) = field.strip_prefix("avg10=") else {
                continue;
            };
            avg10_count += 1;
            let value = value.parse::<f64>().ok()?;
            if !value.is_finite() || !(0.0..=100.0).contains(&value) {
                return None;
            }
            result = Some(value);
        }
        if avg10_count != 1 {
            return None;
        }
    }
    if full_rows == 1 { result } else { None }
}

pub(super) fn publish_at(
    path: &Path,
    decision: &SupervisorDecision,
    action_results: &[SupervisorActionResult],
    daemon_instance_id: Option<&str>,
    supervisor_identity: &OwnerIdentity,
    written_at_unix_ms: u64,
) -> Result<(), String> {
    let parent = path.parent().ok_or("supervisor state parent missing")?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temporary = parent.join(format!(".supervisor-{}.tmp", std::process::id()));
    let value = serde_json::json!({
        "schema_version": 3,
        "control_state": decision.state,
        "healthy_samples": decision.healthy_samples,
        "cache_recovery_samples": decision.cache_recovery_samples,
        "action_results": action_results,
        "daemon_instance_id": daemon_instance_id,
        "supervisor_identity": supervisor_identity,
        "written_at_unix_ms": written_at_unix_ms,
    });
    fs::write(&temporary, format!("{value}\n")).map_err(|error| error.to_string())?;
    fs::rename(temporary, path).map_err(|error| error.to_string())
}

#[derive(Debug)]
pub(super) struct DecisionApplyError {
    pub(super) detail: String,
    pub(super) action_results: Vec<SupervisorActionResult>,
}

impl fmt::Display for DecisionApplyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for DecisionApplyError {}

#[allow(clippy::too_many_arguments)]
pub(super) fn apply_decision_with(
    state_path: &Path,
    ledger_root: &Path,
    decision: &SupervisorDecision,
    action_paths: &ActionPaths<'_>,
    runner: &dyn UnitActionRunner,
    supervisor_identity: &OwnerIdentity,
    written_at_unix_ms: u64,
) -> Result<Vec<SupervisorActionResult>, DecisionApplyError> {
    if decision.state == SupervisorState::Healthy {
        let action_results = execute_actions_with(decision, action_paths, runner, || {
            Err("CloseAdmission is invalid in a healthy decision".into())
        });
        publish_at(
            state_path,
            decision,
            &action_results,
            action_paths.daemon_instance_id,
            supervisor_identity,
            written_at_unix_ms,
        )
        .map_err(|detail| DecisionApplyError {
            detail,
            action_results: action_results.clone(),
        })?;
        match workload::publish_admission_state_at_with_transient_retry(
            ledger_root,
            supervisor_identity,
            decision.state.as_str(),
            written_at_unix_ms,
        ) {
            Ok(()) => {}
            Err(detail) if ledger_failure_ends_control_plane(&detail) => {
                return Err(DecisionApplyError {
                    detail,
                    action_results: action_results.clone(),
                });
            }
            // Transient busy while reopening. The published control state is
            // already `HEALTHY` (gate 1), the previous admission record stays
            // as it was — fail-closed when it said closed — and a stale open
            // record is refused by the 15 s evidence-age bound. The next
            // sample re-publishes; ending the process here would only
            // crash-loop systemd over a lock another actor is about to drop.
            Err(_) => {}
        }
        return Ok(action_results);
    }

    // Close admission exactly when its action is executed. A refusal remains
    // that action's definitive typed result; later pressure actions still run
    // and every outcome is published in execution order.
    //
    // A transient ledger lock contention is retried here rather than treated
    // as fatal: a supervisor that exits on `EWOULDBLOCK` crash-loops under
    // ordinary reservation churn. The retry is bounded and only matches the
    // busy signature, so a still-busy lock after the budget is recorded as
    // this action's definitive failure — never as a synthetic success. What
    // that recorded failure must not do is end the control plane: see
    // `ledger_failure_ends_control_plane`.
    let action_results = execute_actions_with(decision, action_paths, runner, || {
        workload::publish_admission_state_at_with_transient_retry(
            ledger_root,
            supervisor_identity,
            decision.state.as_str(),
            written_at_unix_ms,
        )
    });
    let state_result = publish_at(
        state_path,
        decision,
        &action_results,
        action_paths.daemon_instance_id,
        supervisor_identity,
        written_at_unix_ms,
    );
    if let Err(state) = state_result {
        return Err(DecisionApplyError {
            detail: format!(
                "pressure actions ran but their definitive state publication failed: {state}"
            ),
            action_results,
        });
    }
    if let Some(close) = action_results.iter().find(|result| {
        result.action == SupervisorAction::CloseAdmission
            && result.status == SupervisorActionStatus::Failed
    }) {
        let detail = close
            .error
            .as_deref()
            .unwrap_or("canonical admission close failed without an error detail");
        if ledger_failure_ends_control_plane(detail) {
            return Err(DecisionApplyError {
                detail: format!(
                    "admission close failed after pressure actions were recorded: {detail}"
                ),
                action_results,
            });
        }
        // Transient busy past the intra-action budget. It is already
        // published above as this action's definitive `failed` result, so
        // nothing is concealed and nothing is reported as a synthetic
        // success. Admission is already closed by the published non-HEALTHY
        // control state — the first admission gate refuses on it — and the
        // next sample re-issues CloseAdmission while the state is still
        // Guarded, Critical or Emergency. Terminating here would only let
        // systemd restart the process into a fresh `Healthy` default and
        // undo the very close that just failed to persist.
    }
    Ok(action_results)
}

pub(super) fn collect_sample(start: Instant, previous: Instant) -> Result<PressureSample, String> {
    let meminfo = fs::read_to_string("/proc/meminfo").map_err(|error| error.to_string())?;
    let pressure =
        fs::read_to_string("/proc/pressure/memory").map_err(|error| error.to_string())?;
    let (total, available) = parse_meminfo(&meminfo).ok_or("invalid /proc/meminfo")?;
    Ok(PressureSample {
        memory_available_bytes: available,
        control_reserve_bytes: memory_policy::control_reserve_bytes(total),
        psi_full_avg10: parse_psi_full_avg10(&pressure).ok_or("invalid memory PSI")?,
        sample_delay_ms: previous.elapsed().as_millis().saturating_sub(1_000) as u64,
        elapsed_ms: start.elapsed().as_millis() as u64,
    })
}

pub(super) fn run_cycle_with<F>(
    runtime: &Path,
    ledger_root: &Path,
    state_path: &Path,
    supervisor: &mut Supervisor,
    supervisor_identity: &OwnerIdentity,
    runner: &dyn UnitActionRunner,
    sample: F,
) -> Result<(), String>
where
    F: FnOnce() -> Result<(PressureSample, u64), String>,
{
    let (sample, written_at_unix_ms) = sample()?;
    supervisor.recover_action_state(runtime, written_at_unix_ms, sample.elapsed_ms)?;
    let decision = supervisor.observe(sample);
    let daemon_instance_id = request_daemon_instance_at(runtime, written_at_unix_ms).ok();
    let application = apply_decision_with(
        state_path,
        ledger_root,
        &decision,
        &ActionPaths {
            runtime,
            ledger: &ledger_root.join("reservations.json"),
            daemon_instance_id: daemon_instance_id.as_deref(),
            issued_at_unix_ms: Some(written_at_unix_ms),
        },
        runner,
        supervisor_identity,
        written_at_unix_ms,
    );
    let (action_results, application_error) = match application {
        Ok(action_results) => (action_results, None),
        Err(error) => (error.action_results, Some(error.detail)),
    };
    supervisor.commit_action_results(&action_results, sample.elapsed_ms);
    if let Some(error) = application_error {
        return Err(error);
    }
    Ok(())
}

pub fn run(args: &[String]) -> Result<(), String> {
    let once = match args {
        [] => false,
        [option] if option == "--once" => true,
        _ => return Err("usage: ramshared supervise [--once]".into()),
    };
    let _budget_observer = if once {
        None
    } else {
        match crate::budget_observer::start() {
            Ok(observer) => Some(observer),
            Err(error) => {
                eprintln!("[ramshared] Off budget observer unavailable: {error}");
                None
            }
        }
    };
    let start = Instant::now();
    let mut previous = Instant::now();
    let mut supervisor = Supervisor::resuming_from_prior_state(
        Path::new("/run/ramshared/supervisor-state.json"),
        unix_time_ms()?,
    );
    let supervisor_identity = OwnerIdentity::current()?;
    loop {
        run_cycle_with(
            Path::new("/run/ramshared"),
            Path::new("/run/ramshared/admission"),
            Path::new("/run/ramshared/supervisor-state.json"),
            &mut supervisor,
            &supervisor_identity,
            &SystemUnitActionRunner,
            || {
                let sample = collect_sample(start, previous)?;
                previous = Instant::now();
                let written_at_unix_ms = unix_time_ms()?;
                Ok((sample, written_at_unix_ms))
            },
        )?;
        if once {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}
