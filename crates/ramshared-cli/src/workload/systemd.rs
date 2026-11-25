// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/workload/systemd.rs
//! Managed systemd scope lifecycle, exact property queries, and runner containment.

#[cfg(test)]
use super::FIXTURE_INVOCATION_ID;
use super::{
    OwnerIdentity, SCOPE_START_ACK_TIMEOUT, SCOPE_STATUS_POLL_INTERVAL, SCOPE_STATUS_QUERY_TIMEOUT,
    SCOPE_TERMINAL_PROOF_TIMEOUT, SystemOwnerProbe, WorkloadBudget, WorkloadRequest,
    admit_reservation_at, bind_reservation_invocation_while_locked, bounded_process,
    calculate_budget, release_reservation_at, valid_systemd_invocation_id,
};
use std::path::Path;
use std::process::{Child, Command};
use std::sync::Arc;

pub(super) fn parse_meminfo(text: &str) -> Result<u64, String> {
    let total_kib = text.lines().find_map(|line| {
        let (name, rest) = line.split_once(':')?;
        (name == "MemTotal")
            .then(|| rest.split_whitespace().next()?.parse::<u64>().ok())
            .flatten()
    });
    total_kib
        .map(|value| value.saturating_mul(1024))
        .ok_or_else(|| "MemTotal missing from /proc/meminfo".into())
}

pub(super) fn build_runner_args(
    unit: &str,
    request: &WorkloadRequest,
    budget: WorkloadBudget,
) -> Vec<String> {
    vec![
        "--scope".into(),
        "--quiet".into(),
        "--unit".into(),
        unit.trim_end_matches(".scope").into(),
        "--property".into(),
        "Slice=ramshared-workloads.slice".into(),
        "--property".into(),
        format!(
            "MemoryHigh={}",
            request.memory_max_bytes.min(budget.memory_high_bytes)
        ),
        "--property".into(),
        format!("MemoryMax={}", request.memory_max_bytes),
        "--property".into(),
        "TasksMax=8192".into(),
        "--setenv".into(),
        format!("RAMSHARED_WORKLOAD_CLASS={}", request.class.as_str()),
        "--".into(),
    ]
}

pub(super) enum ScopeCompletion {
    /// The runner child reached its own terminal state and the scope is
    /// proven gone or terminal. The inner result is the **workload's**
    /// exit/error semantics and is safe to release from the admission ledger.
    Terminal(Result<Option<i32>, String>),
    /// The scope may still exist but its exact terminal state could not be
    /// proven. Keep the reservation and its forensic evidence fail-closed.
    UnsafeContainment(String),
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ScopeStart {
    /// systemd-run accepted the scope. `Some(id)` is the start identity bound
    /// to the reservation. `None` means the workload finished inside the
    /// identity window and no `InvocationID` was observable — which is only
    /// ever concluded after the runner child itself has already exited, since
    /// systemd answers `not-found` identically before the start job lands and
    /// after the unit is collected. The reservation is released by owner
    /// identity and no later action will target the unit.
    Acknowledged(Option<String>),
    /// A control acknowledgement or its identity snapshot is inconclusive.
    /// The unit may exist, so ledger evidence must survive for containment.
    UnsafeContainment(String),
}

/// What a `.scope` unit actually reports.
///
/// A scope is a cgroup of the operator's processes. It has **no** main
/// process, so the service properties `ExecMainCode` / `ExecMainStatus` /
/// `MainPID` are not merely absent on this host — they are not scope
/// properties at all. Asking a scope for them is a category error, and the
/// unit-status projection simply omits them.
///
/// The workload's exit status therefore cannot come from the unit. It comes
/// from the runner child (`systemd-run --scope`), whose own exit status is the
/// command's. This type only proves identity and lifecycle.
#[derive(Debug)]
pub(super) struct ScopeStatus {
    pub(super) id: String,
    pub(super) invocation_id: String,
    pub(super) load_state: String,
    pub(super) active_state: String,
    /// The scope's process-presence observation (`running` or `dead`). This is
    /// the only projection field that says whether the cgroup still holds
    /// processes; `ActiveState` alone only says how far the unit's own state
    /// machine has moved.
    pub(super) sub_state: String,
    /// systemd's scope **lifecycle** result (`success`, `failure`, `signal`,
    /// `timeout`, `resources`, …). It is not the workload's exit code: a scope
    /// whose command exited 7 still reports `Result=success`.
    pub(super) result: String,
}

/// Lifecycle of the unit itself, separate from the workload's exit status.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ScopeLifecycle {
    /// The scope still holds processes; the workload is not over.
    Running,
    /// The scope reached `inactive`/`failed`. `result` is systemd's lifecycle
    /// result, not the workload's exit code.
    Terminal { result: String },
}

/// Whether a scope's cgroup still holds processes.
///
/// For a scope, `SubState` is this observation and nothing else: `running`
/// means processes are present and `dead` means the cgroup is empty. systemd
/// can lag its own unit-state transition after the last process exits, so an
/// `active` scope with a `dead` sub-state is an empty cgroup whose unit has
/// not flipped yet — contained, not a leaked workload.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ScopePresence {
    /// The cgroup still holds processes.
    Occupied,
    /// The cgroup is empty; the unit state may still be catching up.
    Empty,
}

impl ScopeStatus {
    #[cfg(test)]
    pub(super) fn terminal_success(unit: &str) -> Self {
        Self {
            id: unit.to_string(),
            invocation_id: FIXTURE_INVOCATION_ID.into(),
            load_state: "loaded".into(),
            active_state: "inactive".into(),
            sub_state: "dead".into(),
            result: "success".into(),
        }
    }

    #[cfg(test)]
    pub(super) fn active(unit: &str) -> Self {
        Self {
            id: unit.to_string(),
            invocation_id: FIXTURE_INVOCATION_ID.into(),
            load_state: "loaded".into(),
            active_state: "active".into(),
            sub_state: "running".into(),
            result: "success".into(),
        }
    }

    /// True when the unit reports a bound start identity.
    pub(super) fn has_invocation_id(&self) -> bool {
        !self.invocation_id.is_empty()
    }

    pub(super) fn lifecycle(&self) -> Result<ScopeLifecycle, String> {
        match self.load_state.as_str() {
            // A unit systemd already collected: nothing is running in it.
            "not-found" => {
                return Ok(ScopeLifecycle::Terminal {
                    result: self.result.clone(),
                });
            }
            "loaded" => {}
            state => return Err(format!("scope load state is {state}")),
        }
        match self.active_state.as_str() {
            "active" | "activating" | "reloading" | "deactivating" => Ok(ScopeLifecycle::Running),
            "inactive" | "failed" => Ok(ScopeLifecycle::Terminal {
                result: self.result.clone(),
            }),
            state => Err(format!("scope active state is {state}")),
        }
    }

    pub(super) fn presence(&self) -> Result<ScopePresence, String> {
        match self.sub_state.as_str() {
            "running" => Ok(ScopePresence::Occupied),
            "dead" => Ok(ScopePresence::Empty),
            state => Err(format!("scope sub state is {state}")),
        }
    }

    /// True when an empty cgroup in this unit state is a finished workload
    /// rather than a start still in flight.
    ///
    /// `activating` and `reloading` are start/reload jobs in progress: their
    /// cgroup can be empty for a moment while the workload is about to land,
    /// so an empty cgroup there is never accepted as containment.
    pub(super) fn empty_cgroup_is_contained(&self) -> bool {
        matches!(self.active_state.as_str(), "active" | "deactivating")
    }

    /// The single "this unit holds no processes and will not start another"
    /// answer.
    ///
    /// Vacant means: the unit is terminal or absent, or the unit is still in a
    /// post-start state and its cgroup is already empty. A scope whose cgroup
    /// still holds processes is not vacant, and `activating`/`reloading` with
    /// an empty cgroup is a start in flight, so it is not vacant either. An
    /// unknown projection is never read as vacant.
    ///
    /// Two call sites share this predicate because they are asking the same
    /// question about the same object: the post-reap containment proof asks
    /// whether the finished workload has left the unit empty, and the
    /// verified-dead reservation recovery asks whether a row whose owner is
    /// dead still represents any processes. Neither may answer from a deadline
    /// or from owner liveness alone.
    pub(super) fn is_process_vacant(&self) -> Result<bool, String> {
        match self.lifecycle()? {
            ScopeLifecycle::Terminal { .. } => Ok(true),
            ScopeLifecycle::Running => {
                if !self.empty_cgroup_is_contained() {
                    return Ok(false);
                }
                Ok(self.presence()? == ScopePresence::Empty)
            }
        }
    }
}

pub(super) trait ScopeStatusAdapter: Send + Sync {
    fn query(&self, unit: &str) -> Result<ScopeStatus, String>;
}

pub(super) struct SystemScopeStatusAdapter;

impl ScopeStatusAdapter for SystemScopeStatusAdapter {
    fn query(&self, unit: &str) -> Result<ScopeStatus, String> {
        let output = run_scope_status_command(unit)?;
        parse_scope_status(unit, &output)
    }
}

/// The operator-facing shape of a failed scope-status query.
///
/// The tool's own reason is what an operator needs, so stderr rides along when
/// it has one; `run_capture_command` already bounded the capture. An empty or
/// whitespace-only reason falls back to the status alone. A killed query has
/// no exit code and is reported as `signal`.
///
/// Pure so the contract is testable without a control plane.
pub(super) fn scope_status_query_failure(status_code: Option<i32>, stderr: &[u8]) -> String {
    let reason = String::from_utf8_lossy(stderr).trim().to_string();
    let status = status_code.map_or_else(|| "signal".into(), |code| code.to_string());
    if reason.is_empty() {
        format!("exact scope status query failed: {status}")
    } else {
        format!("exact scope status query failed: {status}: {reason}")
    }
}

pub(super) fn run_scope_status_command(unit: &str) -> Result<String, String> {
    let mut status = Command::new("systemctl");
    status
        .arg("show")
        .arg("--property=Id")
        .arg("--property=InvocationID")
        .arg("--property=LoadState")
        .arg("--property=ActiveState")
        .arg("--property=SubState")
        .arg("--property=Result")
        .arg(unit);
    let output = bounded_process::run_capture_command(
        &mut status,
        "exact scope status query",
        SCOPE_STATUS_QUERY_TIMEOUT,
        bounded_process::DEFAULT_OUTPUT_LIMIT,
        |_| {},
    )
    .map_err(|error| format!("exact scope status query custody failed: {error}"))?;
    if !output.status.success() {
        return Err(scope_status_query_failure(
            output.status.code(),
            &output.stderr,
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|error| format!("exact scope status query returned non-UTF-8 output: {error}"))
}

pub(super) fn parse_scope_status(unit: &str, output: &str) -> Result<ScopeStatus, String> {
    let mut id = None;
    let mut invocation_id = None;
    let mut load_state = None;
    let mut active_state = None;
    let mut sub_state = None;
    let mut result = None;
    for line in output.lines() {
        let (name, value) = line
            .split_once('=')
            .ok_or("malformed exact scope status response")?;
        let slot = match name {
            "Id" => &mut id,
            "InvocationID" => &mut invocation_id,
            "LoadState" => &mut load_state,
            "ActiveState" => &mut active_state,
            "SubState" => &mut sub_state,
            "Result" => &mut result,
            _ => return Err(format!("unexpected exact scope status field {name}")),
        };
        if slot.is_some() {
            return Err(format!("duplicate exact scope status field {name}"));
        }
        *slot = Some(value.to_string());
    }
    let id = id.ok_or("exact scope status omitted Id")?;
    if id != unit {
        return Err(format!(
            "exact scope status referred to {id}, expected {unit}"
        ));
    }
    // An empty `InvocationID` is a real value, not an omission: systemd clears
    // it once the unit leaves the active state, so a scope that already
    // finished reports `InvocationID=` and the caller decides what that means.
    let invocation_id = invocation_id.ok_or("exact scope status omitted InvocationID")?;
    if !invocation_id.is_empty() && !valid_systemd_invocation_id(&invocation_id) {
        return Err(format!(
            "exact scope status reported an invalid InvocationID {invocation_id}"
        ));
    }
    Ok(ScopeStatus {
        id,
        invocation_id,
        load_state: load_state.ok_or("exact scope status omitted LoadState")?,
        active_state: active_state.ok_or("exact scope status omitted ActiveState")?,
        sub_state: sub_state.ok_or("exact scope status omitted SubState")?,
        result: result.ok_or("exact scope status omitted Result")?,
    })
}

pub(super) trait ScopeExecution {
    /// Prove `systemd-run` accepted the scope while admission remains closed
    /// to concurrent transitions. This deliberately never waits on the runner
    /// child: the workload owns its own lifetime, and a start acknowledgement
    /// that waited for the command would kill everything longer than the ack
    /// budget.
    fn acknowledge_start(&mut self) -> ScopeStart;

    /// Wait for the runner child — the workload — and report its exit status,
    /// then prove the scope is terminal before the ledger may release.
    fn wait_for_completion(&mut self) -> ScopeCompletion;
}

pub(super) trait ScopeRunner {
    fn launch(
        &self,
        unit: &str,
        runner_args: &[String],
        command: &[String],
    ) -> Result<Box<dyn ScopeExecution>, String>;
}

pub(super) struct SystemScopeRunner;

/// A managed workload run under `systemd-run --scope`.
///
/// The runner child **is** the workload: `systemd-run --scope` forks the
/// command into the new scope's cgroup and waits for it, so the child's own
/// exit status is the command's. The unit-status projection only proves
/// identity and lifecycle — it cannot report an exit code for a scope.
pub(super) struct SystemScopeExecution {
    pub(super) runner: Child,
    pub(super) unit: String,
    pub(super) status: Arc<dyn ScopeStatusAdapter>,
    pub(super) invocation_id: Option<String>,
    pub(super) reaped: bool,
}

impl SystemScopeExecution {
    pub(super) fn with_status_adapter(
        runner: Child,
        unit: String,
        status: Arc<dyn ScopeStatusAdapter>,
    ) -> Self {
        Self {
            runner,
            unit,
            status,
            invocation_id: None,
            reaped: false,
        }
    }

    /// Poll the unit-status projection until the scope reports a bound start
    /// identity, the unit is already gone, or the control budget expires.
    pub(super) fn bind_start_identity(&mut self) -> ScopeStart {
        let deadline = std::time::Instant::now() + SCOPE_START_ACK_TIMEOUT;
        loop {
            let scope_status = match self.status.query(&self.unit) {
                Ok(scope_status) => scope_status,
                // We could not ask. That is never an answer about the unit:
                // a missing or failing control plane is not a missing unit,
                // and reading it as one would acknowledge a scope start with
                // no identity behind it. Absence is decided below, from the
                // projection's own `LoadState`.
                Err(error) => {
                    return ScopeStart::UnsafeContainment(format!(
                        "managed scope identity is unknown; reservation retained: {error}"
                    ));
                }
            };
            if scope_status.id != self.unit {
                return ScopeStart::UnsafeContainment(format!(
                    "managed scope identity is not bound to {}; reservation retained",
                    self.unit
                ));
            }
            if scope_status.has_invocation_id() {
                self.invocation_id = Some(scope_status.invocation_id.clone());
                return ScopeStart::Acknowledged(Some(scope_status.invocation_id));
            }
            match scope_status.lifecycle() {
                // A terminal unit with no start identity: either the workload
                // finished inside the identity window, or the start job has
                // not landed yet. systemd answers `not-found` for both with an
                // identical projection (`LoadState=not-found`,
                // `InvocationID=`), so only the runner child separates them —
                // while it is alive the scope is still ours to start or run.
                Ok(ScopeLifecycle::Terminal { .. }) => {
                    match bounded_process::child_has_exited(&mut self.runner) {
                        // Already over: systemd cleared the start identity and
                        // the runner child's exit status stays authoritative.
                        // No later action will target the unit.
                        Ok(true) => return ScopeStart::Acknowledged(None),
                        // Still ours. Keep polling for the identity.
                        Ok(false) => {}
                        // An unknown wait state is never read as "finished":
                        // fall through to the deadline, which retains.
                        Err(_) => {}
                    }
                }
                Ok(ScopeLifecycle::Running) => {}
                Err(error) => {
                    return ScopeStart::UnsafeContainment(format!(
                        "managed scope identity is invalid; reservation retained: {error}"
                    ));
                }
            }
            if std::time::Instant::now() >= deadline {
                return ScopeStart::UnsafeContainment(format!(
                    "managed scope identity for {} was never observed; reservation retained",
                    self.unit
                ));
            }
            std::thread::sleep(SCOPE_STATUS_POLL_INTERVAL);
        }
    }

    /// After the runner child is reaped, the scope must be contained before
    /// the ledger may release. Anything still holding processes means the
    /// workload left the group alive and the reservation stays fail-closed.
    pub(super) fn prove_scope_terminal(&mut self) -> Result<(), String> {
        let deadline = std::time::Instant::now() + SCOPE_TERMINAL_PROOF_TIMEOUT;
        let mut last_observation;
        loop {
            let scope_status = match self.status.query(&self.unit) {
                Ok(scope_status) => scope_status,
                // We could not ask, so we do not know. The reservation is
                // retained: a collected unit is proved terminal from the
                // projection's own `LoadState` below, never from a transport
                // failure that happens to mention a missing something.
                Err(error) => {
                    return Err(format!(
                        "exact terminal state for {} is unknown: {error}",
                        self.unit
                    ));
                }
            };
            if scope_status.id != self.unit {
                return Err(format!(
                    "exact terminal state changed from {} to {}",
                    self.unit, scope_status.id
                ));
            }
            if let Some(expected) = &self.invocation_id
                && expected != &scope_status.invocation_id
                && scope_status.has_invocation_id()
            {
                return Err(format!("scope {} invocation identity changed", self.unit));
            }
            last_observation = format!(
                "load={} active={} sub={} result={}",
                scope_status.load_state,
                scope_status.active_state,
                scope_status.sub_state,
                scope_status.result
            );
            match scope_status.is_process_vacant() {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(error) => {
                    return Err(format!(
                        "exact terminal state for {} is invalid: {error}",
                        self.unit
                    ));
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!(
                    "scope {} is not contained after the runner exited: {}",
                    self.unit, last_observation
                ));
            }
            std::thread::sleep(SCOPE_STATUS_POLL_INTERVAL);
        }
    }
}

impl Drop for SystemScopeExecution {
    /// Containment on the way out. If the caller never proved completion the
    /// group is still ours, and dropping it unreaped would strand both the
    /// processes and the zombie. A group already reaped by
    /// `wait_for_completion` is left alone.
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        let _ = bounded_process::terminate_group_and_reap(&mut self.runner, "managed scope runner");
    }
}

impl ScopeExecution for SystemScopeExecution {
    fn acknowledge_start(&mut self) -> ScopeStart {
        self.bind_start_identity()
    }

    fn wait_for_completion(&mut self) -> ScopeCompletion {
        let exit = match bounded_process::wait_grouped_child_unbounded(
            &mut self.runner,
            "managed scope runner",
        ) {
            Ok(status) => status,
            Err(error) => {
                return ScopeCompletion::UnsafeContainment(format!(
                    "managed workload runner custody failed; reservation retained: {error}"
                ));
            }
        };
        self.reaped = true;
        if let Err(error) = self.prove_scope_terminal() {
            return ScopeCompletion::UnsafeContainment(format!("{error}; reservation retained"));
        }
        let workload_exit = match exit.code() {
            Some(code) => Ok(Some(code)),
            None => Ok(None),
        };
        ScopeCompletion::Terminal(workload_exit)
    }
}

impl ScopeRunner for SystemScopeRunner {
    fn launch(
        &self,
        unit: &str,
        runner_args: &[String],
        command: &[String],
    ) -> Result<Box<dyn ScopeExecution>, String> {
        let terminator = runner_args
            .iter()
            .position(|argument| argument == "--")
            .ok_or("managed scope arguments omit the command terminator")?;
        let mut systemd_run = Command::new("systemd-run");
        systemd_run
            .args(&runner_args[..terminator])
            .arg("--no-block")
            .arg("--")
            .args(command);
        let acknowledgement = bounded_process::configure_process_group(&mut systemd_run)
            .spawn()
            .map_err(|error| format!("failed to start managed systemd scope: {error}"))?;
        Ok(Box::new(SystemScopeExecution::with_status_adapter(
            acknowledgement,
            unit.to_string(),
            Arc::new(SystemScopeStatusAdapter),
        )))
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run_with(
    request: WorkloadRequest,
    meminfo: &str,
    runner: &dyn ScopeRunner,
    ledger_root: &Path,
    safe_mode: &Path,
    resume_lease: &Path,
    supervisor_state: &Path,
) -> Result<(), String> {
    let owner = OwnerIdentity::current()?;
    let budget = calculate_budget(parse_meminfo(meminfo)?, 0)?;
    let (mut reservation, admission_lock) = admit_reservation_at(
        ledger_root,
        safe_mode,
        resume_lease,
        supervisor_state,
        owner,
        request.class,
        request.memory_max_bytes,
        budget.memory_max_bytes,
        &SystemOwnerProbe,
        &SystemScopeStatusAdapter,
    )?;
    let runner_args = build_runner_args(&reservation.unit, &request, budget);
    let mut admission_lock = Some(admission_lock);
    let (result, release_reservation) = match runner.launch(
        &reservation.unit,
        &runner_args,
        &request.command,
    ) {
        Ok(mut execution) => match execution.acknowledge_start() {
            ScopeStart::Acknowledged(maybe_invocation_id) => {
                if let Some(invocation_id) = maybe_invocation_id {
                    let Some(lock) = admission_lock.as_ref() else {
                        return Err(
                            "managed scope identity lock disappeared before binding; reservation retained"
                                .into(),
                        );
                    };
                    reservation = match bind_reservation_invocation_while_locked(
                        lock,
                        &reservation,
                        &invocation_id,
                    ) {
                        Ok(reservation) => reservation,
                        Err(error) => {
                            return Err(format!(
                                "managed scope identity was not persisted; reservation retained: {error}"
                            ));
                        }
                    };
                }
                drop(admission_lock.take());
                match execution.wait_for_completion() {
                    ScopeCompletion::Terminal(result) => (result, true),
                    ScopeCompletion::UnsafeContainment(error) => (Err(error), false),
                }
            }
            ScopeStart::UnsafeContainment(error) => (Err(error), false),
        },
        Err(error) => (Err(error), true),
    };
    let result = match result {
        Ok(Some(0)) => Ok(()),
        Ok(Some(code)) => Err(format!("managed workload exited with status {code}")),
        Ok(None) => Err("managed workload exited with a signal".into()),
        Err(error) => Err(error),
    };
    drop(admission_lock);
    if release_reservation {
        let release = release_reservation_at(ledger_root, &reservation, &SystemOwnerProbe);
        result.and(release.map(|_| ()))
    } else {
        result
    }
}
