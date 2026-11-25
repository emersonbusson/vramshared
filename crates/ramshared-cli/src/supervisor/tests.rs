// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/supervisor/tests.rs
// Tests assert panics and unwrap injected fixtures on purpose. The crate
// denies both lints for production code; the deny must not leak into the
// test module that is supposed to exercise those paths.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

pub(super) use std::cell::RefCell;

pub(super) use std::os::unix::fs::PermissionsExt;

pub(super) use std::sync::atomic::{AtomicUsize, Ordering};

pub(super) const GIB: u64 = 1024 * 1024 * 1024;

pub(super) static TEMP: AtomicUsize = AtomicUsize::new(0);

pub(super) struct RuntimeWriteHookGuard;

impl RuntimeWriteHookGuard {
    fn install(hook: RuntimeWriteHook) -> Self {
        RUNTIME_WRITE_HOOK.with(|slot| {
            assert!(
                slot.borrow().is_none(),
                "runtime write hook already installed"
            );
            *slot.borrow_mut() = Some(hook);
        });
        Self
    }
}

impl Drop for RuntimeWriteHookGuard {
    fn drop(&mut self) {
        RUNTIME_WRITE_HOOK.with(|slot| *slot.borrow_mut() = None);
    }
}

pub(super) struct FakeUnitRunner {
    pub(super) calls: RefCell<Vec<Vec<String>>>,
    pub(super) fail: bool,
}

pub(super) struct StaleIdentityRunner {
    pub(super) calls: RefCell<Vec<Vec<String>>>,
    pub(super) identity_queries: RefCell<usize>,
}

impl UnitActionRunner for FakeUnitRunner {
    fn current_invocation_id(&self, _unit: &str) -> Result<String, String> {
        Ok("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into())
    }

    fn run(&self, args: &[&str]) -> Result<(), String> {
        self.calls
            .borrow_mut()
            .push(args.iter().map(|value| (*value).to_string()).collect());
        if self.fail {
            Err("fixture systemctl refusal".into())
        } else {
            Ok(())
        }
    }
}

impl UnitActionRunner for StaleIdentityRunner {
    fn current_invocation_id(&self, _unit: &str) -> Result<String, String> {
        *self.identity_queries.borrow_mut() += 1;
        Ok("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into())
    }

    fn run(&self, args: &[&str]) -> Result<(), String> {
        self.calls
            .borrow_mut()
            .push(args.iter().map(|value| (*value).to_string()).collect());
        Ok(())
    }
}

pub(super) fn fixture() -> std::path::PathBuf {
    let parent = if std::path::Path::new("/dev/shm").is_dir() {
        std::path::PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    for _ in 0..1024 {
        let number = TEMP.fetch_add(1, Ordering::SeqCst);
        let path = parent.join(format!(
            "ramshared-supervisor-{}-{number}",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => return path,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!("create isolated supervisor fixture: {error}"),
        }
    }
    panic!("exhausted isolated supervisor fixture names");
}

pub(super) fn run_isolated_guarded_transition_fixture() {
    let root = fixture();
    let state_path = root.join("supervisor-state.json");
    let ledger_root = root.join("admission");
    let identity = OwnerIdentity::current().unwrap();
    let decision = SupervisorDecision {
        state: SupervisorState::Guarded,
        actions: vec![SupervisorAction::CloseAdmission],
        healthy_samples: 0,
    };
    let runner = FakeUnitRunner {
        calls: RefCell::new(Vec::new()),
        fail: false,
    };
    let actions = apply_decision_with(
        &state_path,
        &ledger_root,
        &decision,
        &ActionPaths {
            runtime: &root.join("runtime"),
            ledger: &ledger_root.join("reservations.json"),
            daemon_instance_id: Some("fixture-daemon"),
            issued_at_unix_ms: Some(1_000),
        },
        &runner,
        &identity,
        1_000,
    )
    .unwrap();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].action, SupervisorAction::CloseAdmission);
    assert_eq!(actions[0].status, SupervisorActionStatus::Succeeded);
    let gate: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(ledger_root.join("admission-state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(gate["admission_open"], false);
    assert!(runner.calls.borrow().is_empty());
    fs::remove_dir_all(root).unwrap();
}

pub(super) struct TestDir {
    pub(super) path: std::path::PathBuf,
}

impl TestDir {
    fn new() -> Self {
        Self { path: fixture() }
    }

    fn program(&self, name: &str, source: &str) -> std::path::PathBuf {
        let path = self.path.join(name);
        // Publish through a temporary name and rename. A freshly written
        // script that is exec'd in place can surface ETXTBSY ("Text file
        // busy") when parallel fixtures race the kernel's write-side
        // deny-write-access check on spawn. Rename publishes a path whose
        // write handle is already closed.
        let staging = self.path.join(format!("{name}.staging"));
        fs::write(&staging, source).unwrap();
        let mut permissions = fs::metadata(&staging).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&staging, permissions).unwrap();
        fs::rename(&staging, &path).unwrap();
        path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub(super) fn ledger_fixture(rows: &[(&str, u64, &str)]) -> String {
    let reservations = rows
        .iter()
        .enumerate()
        .map(|(index, (class, memory_bytes, unit))| {
            serde_json::json!({
                "id": format!("fixture-{index}"),
                "owner": {
                    "boot_id": "fixture-boot",
                    "pid": index as u32 + 1,
                    "start_time": index as u64 + 1,
                    "nonce": format!("fixture-{index}"),
                },
                "class": class,
                "memory_bytes": memory_bytes,
                "unit": unit,
                "invocation_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "issued_ordinal": index as u64 + 1,
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "schema_version": workload::RESERVATION_LEDGER_SCHEMA_VERSION,
        "next_ordinal": reservations.len() as u64 + 1,
        "reservations": reservations,
    })
    .to_string()
}

pub(super) fn sample(
    available_gib: u64,
    reserve_gib: u64,
    psi: f64,
    delay: u64,
    time: u64,
) -> PressureSample {
    PressureSample {
        memory_available_bytes: available_gib * GIB,
        control_reserve_bytes: reserve_gib * GIB,
        psi_full_avg10: psi,
        sample_delay_ms: delay,
        elapsed_ms: time,
    }
}

pub(super) struct FreezeEvidenceRunner {
    pub(super) frozen_path: std::path::PathBuf,
    pub(super) saw_pending: RefCell<bool>,
}

impl UnitActionRunner for FreezeEvidenceRunner {
    fn current_invocation_id(&self, _unit: &str) -> Result<String, String> {
        Ok("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into())
    }

    fn run(&self, _args: &[&str]) -> Result<(), String> {
        let pending = fs::read_to_string(&self.frozen_path)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .is_some_and(|value| {
                value["phase"] == "pending"
                    && value["unit"] == "ramshared-batch-fixture.scope"
                    && value["invocation_id"] == "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            });
        *self.saw_pending.borrow_mut() = pending;
        Err("injected freeze refusal".into())
    }
}

pub(super) struct HugeErrorRunner;

impl UnitActionRunner for HugeErrorRunner {
    fn current_invocation_id(&self, _unit: &str) -> Result<String, String> {
        Ok("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into())
    }

    fn run(&self, _args: &[&str]) -> Result<(), String> {
        Err(format!(
            "{}\n\0\u{001b}\u{0085}tail",
            "x".repeat(1024 * 1024)
        ))
    }
}

#[path = "tests/pressure_and_host_gate.rs"]
mod pressure_and_host_gate;
#[path = "tests/recovery_suite.rs"]
mod recovery_suite;
#[path = "tests/shutdown_and_reconcile.rs"]
mod shutdown_and_reconcile;
#[path = "tests/unit_transitions_and_lease.rs"]
mod unit_transitions_and_lease;
