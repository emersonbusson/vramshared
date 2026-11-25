// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/workload/tests.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

pub(super) use super::identity::*;

pub(super) use super::ledger::*;

pub(super) use super::ledger_lock::*;

pub(super) use super::recovery::*;

pub(super) use super::systemd::*;

pub(super) use super::*;

pub(super) use std::cell::RefCell;

pub(super) use std::os::unix::fs::PermissionsExt;

pub(super) use std::path::PathBuf;

pub(super) use std::process::{Command, Stdio};

pub(super) use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicUsize, Ordering as AtomicOrdering},
};

pub(super) use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(super) const GIB: u64 = 1024 * 1024 * 1024;

// `FIXTURE_INVOCATION_ID` lives at module scope so the `#[cfg(test)]`
// `ScopeStatus` constructors share it with the assertions below.
pub(super) static TEMP: AtomicUsize = AtomicUsize::new(0);

pub(super) struct FakeProbe {
    pub(super) dead_pid: Option<u32>,
}

/// A scope that still holds processes: verified-dead recovery must not
/// fire, so an existing admission test keeps exactly the rows it wrote.
pub(super) struct OccupiedScopeStatus;

impl ScopeStatusAdapter for OccupiedScopeStatus {
    fn query(&self, unit: &str) -> Result<ScopeStatus, String> {
        Ok(ScopeStatus {
            id: unit.to_string(),
            invocation_id: String::new(),
            load_state: "loaded".into(),
            active_state: "active".into(),
            sub_state: "running".into(),
            result: "success".into(),
        })
    }
}

/// A collected scope: no processes and none coming. This is the only
/// answer under which a dead owner's row may be recovered.
pub(super) struct VacantScopeStatus;

impl ScopeStatusAdapter for VacantScopeStatus {
    fn query(&self, unit: &str) -> Result<ScopeStatus, String> {
        Ok(ScopeStatus {
            id: unit.to_string(),
            invocation_id: String::new(),
            load_state: "not-found".into(),
            active_state: "inactive".into(),
            sub_state: "dead".into(),
            result: "success".into(),
        })
    }
}

/// A unit whose state cannot be observed at all.
///
/// An unknown observation is never proof of absence. Verified-dead recovery
/// keeps the row, because dropping it on a failed query would free budget
/// for memory that may still be held.
pub(super) struct UnknownScopeStatus;

impl ScopeStatusAdapter for UnknownScopeStatus {
    fn query(&self, _unit: &str) -> Result<ScopeStatus, String> {
        Err("scope status is unavailable".into())
    }
}

pub(super) struct FakeRunner {
    pub(super) result: Result<Option<i32>, String>,
    pub(super) calls: RefCell<Vec<(Vec<String>, Vec<String>)>>,
}

pub(super) struct TransitionBlockingRunner {
    pub(super) ledger_root: Arc<PathBuf>,
    pub(super) supervisor: Arc<OwnerIdentity>,
    pub(super) calls: RefCell<usize>,
}

pub(super) struct FakeExecution {
    pub(super) result: Result<Option<i32>, String>,
}

pub(super) struct TransitionBlockingExecution {
    pub(super) ledger_root: Arc<PathBuf>,
    pub(super) supervisor: Arc<OwnerIdentity>,
}

pub(super) struct StartPublishingRunner {
    pub(super) ledger_root: Arc<PathBuf>,
    pub(super) supervisor: Arc<OwnerIdentity>,
    pub(super) start_calls: RefCell<usize>,
    pub(super) wait_calls: Arc<AtomicUsize>,
}

pub(super) struct StartPublishingExecution {
    pub(super) ledger_root: Arc<PathBuf>,
    pub(super) supervisor: Arc<OwnerIdentity>,
    pub(super) wait_calls: Arc<AtomicUsize>,
}

pub(super) struct BlockingScopeStatusAdapter {
    pub(super) state: Mutex<BlockingScopeStatus>,
    pub(super) changed: Condvar,
}

pub(super) struct BlockingScopeStatus {
    pub(super) terminal_poll_started: bool,
    pub(super) terminal: bool,
    pub(super) queries: usize,
    pub(super) units: Vec<String>,
}

pub(super) struct SystemExecutionRunner {
    pub(super) status: Arc<dyn ScopeStatusAdapter>,
    pub(super) launches: Arc<AtomicUsize>,
}

pub(super) struct ChangingScopeIdentityAdapter {
    pub(super) queries: AtomicUsize,
}

pub(super) struct ErroringScopeStatusAdapter {
    pub(super) queries: AtomicUsize,
}

impl ScopeRunner for FakeRunner {
    fn launch(
        &self,
        _unit: &str,
        runner_args: &[String],
        command: &[String],
    ) -> Result<Box<dyn ScopeExecution>, String> {
        self.calls
            .borrow_mut()
            .push((runner_args.to_vec(), command.to_vec()));
        Ok(Box::new(FakeExecution {
            result: self.result.clone(),
        }))
    }
}

impl ScopeExecution for FakeExecution {
    fn acknowledge_start(&mut self) -> ScopeStart {
        ScopeStart::Acknowledged(Some(FIXTURE_INVOCATION_ID.into()))
    }

    fn wait_for_completion(&mut self) -> ScopeCompletion {
        ScopeCompletion::Terminal(std::mem::replace(&mut self.result, Ok(None)))
    }
}

impl ScopeRunner for TransitionBlockingRunner {
    fn launch(
        &self,
        _unit: &str,
        _runner_args: &[String],
        _command: &[String],
    ) -> Result<Box<dyn ScopeExecution>, String> {
        *self.calls.borrow_mut() += 1;
        Ok(Box::new(TransitionBlockingExecution {
            ledger_root: Arc::clone(&self.ledger_root),
            supervisor: Arc::clone(&self.supervisor),
        }))
    }
}

impl ScopeExecution for TransitionBlockingExecution {
    fn acknowledge_start(&mut self) -> ScopeStart {
        assert!(
            LedgerPaths::new(&self.ledger_root).transition.exists(),
            "workload start must retain the admission transition guard"
        );
        let error =
            publish_admission_state_at(&self.ledger_root, &self.supervisor, "GUARDED", now_ms())
                .unwrap_err();
        assert!(
            error.contains("transition is busy"),
            "a close-admission transition interleaved with workload start: {error}"
        );
        ScopeStart::Acknowledged(Some(FIXTURE_INVOCATION_ID.into()))
    }

    fn wait_for_completion(&mut self) -> ScopeCompletion {
        ScopeCompletion::Terminal(Ok(Some(0)))
    }
}

impl ScopeRunner for StartPublishingRunner {
    fn launch(
        &self,
        _unit: &str,
        _runner_args: &[String],
        _command: &[String],
    ) -> Result<Box<dyn ScopeExecution>, String> {
        *self.start_calls.borrow_mut() += 1;
        Ok(Box::new(StartPublishingExecution {
            ledger_root: Arc::clone(&self.ledger_root),
            supervisor: Arc::clone(&self.supervisor),
            wait_calls: Arc::clone(&self.wait_calls),
        }))
    }
}

impl ScopeStatusAdapter for BlockingScopeStatusAdapter {
    fn query(&self, unit: &str) -> Result<ScopeStatus, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "fixture status lock poisoned")?;
        state.queries += 1;
        state.units.push(unit.to_string());
        self.changed.notify_all();
        if state.queries == 1 {
            return Ok(ScopeStatus::active(unit));
        }
        state.terminal_poll_started = true;
        self.changed.notify_all();
        while !state.terminal {
            state = self
                .changed
                .wait(state)
                .map_err(|_| "fixture status lock poisoned")?;
        }
        Ok(ScopeStatus::terminal_success(unit))
    }
}

impl ScopeStatusAdapter for ChangingScopeIdentityAdapter {
    fn query(&self, unit: &str) -> Result<ScopeStatus, String> {
        if self.queries.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
            return Ok(ScopeStatus::active(unit));
        }
        Ok(ScopeStatus {
            id: unit.to_string(),
            invocation_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            load_state: "loaded".into(),
            active_state: "inactive".into(),
            sub_state: "dead".into(),
            result: "success".into(),
        })
    }
}

impl ScopeStatusAdapter for ErroringScopeStatusAdapter {
    fn query(&self, unit: &str) -> Result<ScopeStatus, String> {
        if self.queries.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
            Ok(ScopeStatus::active(unit))
        } else {
            Err("fixture bounded status query failed".into())
        }
    }
}

impl BlockingScopeStatusAdapter {
    fn wait_until_terminal_poll(&self) {
        let state = self.state.lock().unwrap();
        let (state, _) = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(1), |state| {
                !state.terminal_poll_started
            })
            .unwrap();
        assert!(
            state.terminal_poll_started,
            "SystemScopeExecution never started its terminal status poll"
        );
    }

    fn allow_terminal(&self) {
        let mut state = self.state.lock().unwrap();
        state.terminal = true;
        self.changed.notify_all();
    }
}

impl ScopeRunner for SystemExecutionRunner {
    fn launch(
        &self,
        unit: &str,
        runner_args: &[String],
        command: &[String],
    ) -> Result<Box<dyn ScopeExecution>, String> {
        self.launches.fetch_add(1, AtomicOrdering::SeqCst);
        assert!(
            !runner_args.iter().any(|argument| argument == "--collect"),
            "terminal status evidence requires systemd to retain the exact scope"
        );
        assert_eq!(command, [String::from("/bin/true")]);
        let mut command = Command::new("/bin/true");
        let acknowledgement = bounded_process::configure_process_group(&mut command)
            .spawn()
            .map_err(|error| error.to_string())?;
        Ok(Box::new(SystemScopeExecution::with_status_adapter(
            acknowledgement,
            unit.to_string(),
            self.status.clone(),
        )))
    }
}

impl ScopeExecution for StartPublishingExecution {
    fn acknowledge_start(&mut self) -> ScopeStart {
        assert!(
            LedgerPaths::new(&self.ledger_root).transition.exists(),
            "launch acknowledgement must remain serialized with admission"
        );
        ScopeStart::Acknowledged(Some(FIXTURE_INVOCATION_ID.into()))
    }

    fn wait_for_completion(&mut self) -> ScopeCompletion {
        self.wait_calls.fetch_add(1, AtomicOrdering::SeqCst);
        let reservations = load_ledger(&LedgerPaths::new(&self.ledger_root).ledger)
            .unwrap_or_else(|error| panic!("read identity-bound ledger: {error}"))
            .reservations;
        assert_eq!(reservations.len(), 1);
        assert_eq!(
            reservations[0].invocation_id.as_deref(),
            Some(FIXTURE_INVOCATION_ID),
            "InvocationID must be durable before the admission transition is released"
        );
        match publish_admission_state_at(&self.ledger_root, &self.supervisor, "GUARDED", now_ms()) {
            Ok(()) => ScopeCompletion::Terminal(Ok(Some(0))),
            Err(error) => ScopeCompletion::Terminal(Err(format!(
                "supervisor transition remained blocked by workload: {error}"
            ))),
        }
    }
}

impl OwnerProbe for FakeProbe {
    fn is_alive(&self, owner: &OwnerIdentity) -> bool {
        self.dead_pid != Some(owner.pid)
    }
}

pub(super) fn owner(pid: u32, start_time: u64) -> OwnerIdentity {
    OwnerIdentity {
        boot_id: "boot-a".into(),
        pid,
        start_time,
        nonce: unique_nonce(),
    }
}

/// A fresh ledger authority directory with **no** reservation document.
///
/// The document is product state: DT-2 makes the authority self-initialize
/// it under the lock. The fixture creates only the directory, so a test
/// that reaches admission exercises the real initializer instead of a
/// fixture that had been papering over its absence.
pub(super) fn fixture() -> PathBuf {
    let parent = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    for _ in 0..1024 {
        let number = TEMP.fetch_add(1, AtomicOrdering::SeqCst);
        let path = parent.join(format!("ramshared-ledger-{}-{number}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => return path,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!("create isolated workload fixture: {error}"),
        }
    }
    panic!("exhausted isolated workload fixture names");
}

pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

pub(super) fn healthy_supervisor_status(
    owner: &OwnerIdentity,
    written_at_unix_ms: u64,
) -> serde_json::Value {
    serde_json::json!({
        "schema_version": 3,
        "control_state": "HEALTHY",
        "healthy_samples": 60,
        "action_results": [],
        "supervisor_identity": owner,
        "written_at_unix_ms": written_at_unix_ms,
    })
}

pub(super) fn current_healthy_supervisor_status() -> serde_json::Value {
    let owner = OwnerIdentity::current().unwrap();
    healthy_supervisor_status(&owner, now_ms())
}

/// Exactly the document `ramshared-host-gate.sh` mints. Keeping the
/// fixture byte-identical to the producer is what stops a fourth shape
/// from being invented in a test and silently passing.
pub(super) fn valid_host_resume_lease(boot_id: &str) -> serde_json::Value {
    serde_json::json!({
        "schema_version": 2,
        "boot_id": boot_id,
        "source": "fresh_sealed_guardian_proof",
        "hardware_health": "clean",
    })
}

pub(super) fn valid_hardware_health(boot_id: &str) -> serde_json::Value {
    serde_json::json!({
        "schema_version": 1,
        "boot_id": boot_id,
        "mce_kill_count": 0,
        "sigbus_mceerr_count": 0,
        "unclean_marker_present": false,
        "verdict": "clean",
    })
}

pub(super) fn valid_recovery_transaction(owner: &OwnerIdentity) -> serde_json::Value {
    let now = now_ms();
    serde_json::json!({
        "schema_version": 2,
        "boot_id": owner.boot_id,
        "incident_id": "0123456789abcdef0123456789abcdef",
        "distro": "Ubuntu-24.04",
        "supervisor_identity": owner,
        "issued_at_epoch_ms": now,
        "expires_at_epoch_ms": now + 15_000,
    })
}

pub(super) fn publish_open_admission_state(root: &Path, owner: &OwnerIdentity) {
    fs::create_dir_all(root).unwrap();
    publish_admission_state_at(root, owner, "HEALTHY", now_ms())
        .unwrap_or_else(|error| panic!("publish open admission state: {error}"));
}

pub(super) fn run_isolated_admission_fixture() {
    let root = fixture();
    let owner = OwnerIdentity::current().unwrap();
    fs::write(
        root.join("supervisor.json"),
        healthy_supervisor_status(&owner, now_ms()).to_string(),
    )
    .unwrap();
    fs::write(
        root.join("lease.json"),
        valid_host_resume_lease(&owner.boot_id).to_string(),
    )
    .unwrap();
    let ledger_root = root.join("ledger");
    publish_open_admission_state(&ledger_root, &owner);
    let runner = FakeRunner {
        result: Ok(Some(0)),
        calls: RefCell::new(Vec::new()),
    };
    run_with(
        WorkloadRequest {
            class: WorkloadClass::Interactive,
            memory_max_bytes: GIB,
            command: vec!["/bin/true".into()],
        },
        "MemTotal: 16777216 kB\n",
        &runner,
        &ledger_root,
        &root.join("safe"),
        &root.join("lease.json"),
        &root.join("supervisor.json"),
    )
    .unwrap();
    assert_eq!(runner.calls.borrow().len(), 1);
    assert!(
        load_ledger(&LedgerPaths::new(&ledger_root).ledger)
            .unwrap()
            .reservations
            .is_empty()
    );
    fs::remove_dir_all(root).unwrap();
}

pub(super) fn lock_refused(paths: &LedgerPaths) -> bool {
    match acquire_lock(paths, &owner(10, 1), &FakeProbe { dead_pid: None }) {
        Ok(guard) => {
            drop(guard);
            false
        }
        Err(_) => true,
    }
}

/// The rows verified-dead recovery dropped, read back from its evidence.
///
/// The quarantine directory also holds transition-owner crash evidence,
/// which is a different mechanism with its own name prefix. Counting
/// `reservation-*` names is what makes "recovered exactly once" mean the
/// recovery snapshot and not whatever else shares the directory.
pub(super) fn reservation_snapshots(root: &Path) -> Vec<Reservation> {
    let quarantine = LedgerPaths::new(root).quarantine;
    let mut recovered = Vec::new();
    for entry in fs::read_dir(&quarantine).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("reservation-") || !name.ends_with(".json") {
            continue;
        }
        recovered.push(serde_json::from_slice(&fs::read(entry.path()).unwrap()).unwrap());
    }
    recovered
}

pub(super) fn recovery_fixture(root: &Path) -> RecoveryPaths {
    RecoveryPaths {
        supervisor_state: root.join("supervisor.json"),
        boot_id: root.join("boot-id"),
        guest_gate: root.join("guest-safe.json"),
        host_gate_dir: root.join("host-safe"),
        pending: root.join("recovery-pending.json"),
        resume_lease: root.join("resume-lease.json"),
        hardware_health: root.join("hardware-health.json"),
    }
}

pub(super) fn seed_recovery(paths: &RecoveryPaths, with_host_gate: bool) {
    let owner = OwnerIdentity::current().unwrap();
    fs::write(
        &paths.supervisor_state,
        format!("{}\n", healthy_supervisor_status(&owner, now_ms())),
    )
    .unwrap();
    fs::write(&paths.boot_id, format!("{}\n", owner.boot_id)).unwrap();
    fs::write(
        &paths.hardware_health,
        format!("{}\n", valid_hardware_health(&owner.boot_id)),
    )
    .unwrap();
    let guest_gate = serde_json::json!({
        "schema_version": 1,
        "incident_id": "0123456789abcdef0123456789abcdef",
        "distro": "Ubuntu-24.04",
        "boot_id": owner.boot_id,
    });
    fs::write(&paths.guest_gate, format!("{guest_gate}\n")).unwrap();
    if with_host_gate {
        let host_gate = serde_json::json!({
            "schema_version": 1,
            "incident_id": "0123456789abcdef0123456789abcdef",
            "distro": "Ubuntu-24.04",
            "prior_boot_id": "prior-boot",
            "reason": "guardian_proven_guest_inaccessible",
            "timestamp_utc": "2026-08-22T00:00:00Z",
        });
        fs::create_dir_all(&paths.host_gate_dir).unwrap();
        fs::write(
            paths.host_gate_dir.join("Ubuntu-24.04.json"),
            format!("{host_gate}\n"),
        )
        .unwrap();
    } else {
        let pending = valid_recovery_transaction(&owner);
        fs::write(&paths.pending, format!("{pending}\n")).unwrap();
    }
}

#[path = "tests/identity_and_ledger_lock.rs"]
mod identity_and_ledger_lock;
#[path = "tests/recovery_and_cleanup.rs"]
mod recovery_and_cleanup;
#[path = "tests/scope_admission_and_custody.rs"]
mod scope_admission_and_custody;
#[path = "tests/systemd_scope_lifecycle.rs"]
mod systemd_scope_lifecycle;
#[path = "tests/workload_extra.rs"]
mod workload_extra;
