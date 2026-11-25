// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/budget_observer/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use ramshared_ipc::budget_observation::Purpose;
use ramshared_vram::{GpuAdapterIdentity, GpuBudgetSnapshot, GpuBudgetSource, GpuBudgetTelemetry};
fn report(pid: u32, stamp: u64) -> Report {
    Report {
        schema_version: 1,
        purpose: Purpose::NativeLinuxPlanningObservation,
        pid,
        started_unix_ms: stamp,
        elapsed_ms: 0,
        budgets: vec![GpuBudgetTelemetry::from_snapshot(
            &GpuBudgetSnapshot {
                adapter: Some(GpuAdapterIdentity {
                    backend: "cuda".into(),
                    key: format!("{:032x}", 1),
                    luid: None,
                    node_mask: None,
                }),
                total_bytes: Some(8 << 30),
                budget_bytes: 8 << 30,
                used_bytes: 8 << 30,
                source: GpuBudgetSource::DriverReported,
                sampled_at: Instant::now(),
            },
            stamp,
        )],
        unavailable_reason: None,
    }
}
fn command(report: &Report, actual_pid: bool) -> Command {
    let bytes = serde_json::to_string(report).unwrap();
    let body = if actual_pid {
        bytes.replace(&format!("\"pid\":{}", report.pid), "\"pid\":'\"$$\"'")
    } else {
        bytes
    };
    let mut c = Command::new("/bin/sh");
    c.args(["-c", &format!("printf '%s' '{body}'")]);
    c
}
fn state() -> &'static State {
    Box::leak(Box::new(State::new(NativeProcessControl)))
}
pub(super) fn protected_process(case: &str) -> bool {
    let executable = std::env::current_exe().unwrap();
    if transport::protected(&executable, rustix::process::geteuid().as_raw()).is_ok() {
        return false;
    }
    assert_ne!(
        std::env::var("RAMSHARED_OBSERVER_FIXTURE_CHILD")
            .ok()
            .as_deref(),
        Some(case),
        "fixture copy must have a protected hierarchy"
    );
    let fixture = Fixture::new();
    let copied = fixture.0.join("observer-test");
    std::fs::copy(&executable, &copied).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&copied, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut command = Command::new(&copied);
    command
        .args(["--exact", case, "--nocapture"])
        .env("RAMSHARED_OBSERVER_FIXTURE_CHILD", case);
    let output = ramshared_exec::run_capture_command(
        &mut command,
        "protected observer test process",
        Duration::from_secs(15),
        256 * 1024,
        |_| {},
    )
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
    true
}

#[test]
fn off_probe_binds_actual_child_and_keeps_exhausted_budget() {
    let state = state();
    let start = Instant::now();
    let stamp = unix_ms().unwrap();
    let output = query_once(state, &mut command(&report(1, stamp), true), start, stamp).unwrap();
    assert_ne!(output.pid, 1);
    assert_eq!(output.budgets[0].available_bytes, 0);
    *state.sample.lock().unwrap() = Some((output, start));
    assert!(state.packet().report.is_some());
    state.stop.store(true, Ordering::Release);
    assert!(
        state.packet().report.is_none(),
        "cancellation revokes even a retained sample"
    );
    state.clear();
    assert!(state.packet().report.is_none());
    state.settle();
}

#[test]
fn off_observation_provenance_does_not_fabricate_active_members_or_drop_unknowns() {
    use crate::readiness::published_members::{PublishedGpuCapacities, from_observations};
    let now = unix_ms().unwrap();
    let first = report(1, now).budgets.remove(0);
    let mut second = first.clone();
    second.adapter.as_mut().unwrap().key = format!("{:032x}", 2);
    let PublishedGpuCapacities::OffObserved(rows) =
        from_observations(&[first.clone(), second], now)
    else {
        panic!("Off observations lost provenance")
    };
    assert_eq!(rows.len(), 2);
    assert_ne!(rows[0].key, rows[1].key);
    let mut invalid = first;
    invalid.adapter = None;
    assert!(matches!(
        from_observations(&[invalid], now),
        PublishedGpuCapacities::Unavailable(_)
    ));
}
#[test]
fn off_probe_refuses_wrong_pid_and_stale_or_diagnostic_output() {
    let state = state();
    let stamp = unix_ms().unwrap();
    assert!(
        query_once(
            state,
            &mut command(&report(1, stamp), false),
            Instant::now(),
            stamp
        )
        .unwrap_err()
        .contains("child identity")
    );
    let old = stamp - GPU_BUDGET_MAX_AGE_MS;
    assert!(
        query_once(
            state,
            &mut command(&report(1, old), true),
            Instant::now(),
            stamp
        )
        .is_err()
    );
    assert!(
        query_once(
            state,
            Command::new("/bin/sh").args(["-c", "printf bad >&2"]),
            Instant::now(),
            stamp
        )
        .unwrap_err()
        .contains("diagnostics")
    );
    *state.sample.lock().unwrap() = Some((
        report(1, stamp),
        Instant::now() - Duration::from_millis(GPU_BUDGET_MAX_AGE_MS),
    ));
    assert!(state.packet().report.is_none());
    state.settle();
}
#[test]
fn off_observer_transport_proves_current_native_peer_and_bounded_packet() {
    if protected_process(
        "budget_observer::tests::off_observer_transport_proves_current_native_peer_and_bounded_packet",
    ) {
        return;
    }
    let now = unix_ms().unwrap();
    let name = format!("ramshared-test-observer-{}-{now}", std::process::id());
    let listener = transport::bind(name.as_bytes()).unwrap();
    assert!(transport::bind(name.as_bytes()).is_err());
    let server = std::thread::spawn(move || {
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    transport::send(
                        stream,
                        &Packet {
                            report: Some(report(std::process::id(), now)),
                            age_ms: 0,
                        },
                    )
                    .unwrap();
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(POLL),
                Err(e) => panic!("{e}"),
            }
        }
    });
    let packet = transport::read_fixture(
        name.as_bytes(),
        rustix::process::geteuid().as_raw(),
        &std::env::current_exe().unwrap(),
        now,
    )
    .unwrap();
    assert_eq!(packet.report.unwrap().budgets[0].available_bytes, 0);
    server.join().unwrap();
    assert!(
        transport::read_fixture(
            name.as_bytes(),
            rustix::process::geteuid().as_raw(),
            &std::env::current_exe().unwrap(),
            now
        )
        .is_err()
    );
}
#[test]
fn off_observer_does_not_start_from_an_uninstalled_or_unprivileged_cli() {
    assert!(transport::probe_image().is_err());
    assert!(off_budgets(unix_ms().unwrap()).is_none());
}

pub(super) struct Fixture(pub(super) std::path::PathBuf);
impl Fixture {
    pub(super) fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap()
            .join("local/tests/budget_observer")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn off_observer_failure_revokes_samples_and_bounds_deduplicated_diagnostics() {
    let state = state();
    *state.sample.lock().unwrap() = Some((report(1, unix_ms().unwrap()), Instant::now()));
    state.failed(&format!("query\n{}", "x".repeat(1024)));
    assert!(state.packet().report.is_none());
    let first = state.failure.lock().unwrap().clone().unwrap();
    assert_eq!(first.chars().count(), 512);
    assert!(!first.chars().any(char::is_control));
    state.failed(&format!("query\n{}", "x".repeat(1024)));
    assert_eq!(
        state.failure.lock().unwrap().as_deref(),
        Some(first.as_str())
    );
    state.failed("different failure");
    assert_eq!(
        state.failure.lock().unwrap().as_deref(),
        Some("different failure")
    );
    state.settle();
}

#[test]
fn off_actor_owns_query_and_response_threads_until_actual_shutdown() {
    if protected_process(
        "budget_observer::tests::off_actor_owns_query_and_response_threads_until_actual_shutdown",
    ) {
        return;
    }
    let fixture = Fixture::new();
    let state = state();
    let name = format!(
        "ramshared-actor-{}-{}",
        std::process::id(),
        unix_ms().unwrap()
    );
    let listener = transport::bind(name.as_bytes()).unwrap();
    let lease = fixture.0.join("probe.lock");
    let guard = start_with(state, listener, move || {
        run_queries_with(state, lease, rustix::process::geteuid().as_raw(), |held| {
            let mut cmd = command(&report(1, unix_ms().unwrap()), true);
            cmd.stdin(Stdio::from(held));
            cmd
        })
    })
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while state.packet().report.is_none() && Instant::now() < deadline {
        std::thread::sleep(POLL);
    }
    let packet = transport::read_fixture(
        name.as_bytes(),
        rustix::process::geteuid().as_raw(),
        &std::env::current_exe().unwrap(),
        unix_ms().unwrap(),
    )
    .unwrap();
    assert_eq!(packet.report.unwrap().budgets[0].available_bytes, 0);
    drop(guard);
    assert!(state.stop.load(Ordering::Acquire));
    assert!(state.packet().report.is_none());
    assert!(
        transport::bind(name.as_bytes()).is_ok(),
        "kernel endpoint must disappear after join"
    );
    assert!(
        start_with(
            state,
            transport::bind(format!("{name}-stopped").as_bytes()).unwrap(),
            || {}
        )
        .is_err()
    );
}

struct Unknown(std::sync::Arc<AtomicBool>);
impl ProcessControl for Unknown {
    fn observe_exit(&self, child: &mut std::process::Child) -> std::io::Result<bool> {
        if self.0.load(Ordering::Acquire) {
            Err(std::io::Error::other("held exit observation"))
        } else {
            NativeProcessControl.observe_exit(child)
        }
    }
    fn stop_group(&self, child: &mut std::process::Child) -> std::io::Result<()> {
        if self.0.load(Ordering::Acquire) {
            Err(std::io::Error::other("held native stop"))
        } else {
            NativeProcessControl.stop_group(child)
        }
    }
    fn reap(
        &self,
        child: &mut std::process::Child,
    ) -> std::io::Result<Option<std::process::ExitStatus>> {
        NativeProcessControl.reap(child)
    }
    fn group_absent(&self, pid: u32) -> std::io::Result<bool> {
        NativeProcessControl.group_absent(pid)
    }
}
#[test]
fn off_probe_timeout_keeps_child_lease_and_refuses_replacement() {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let fixture = Fixture::new();
    let lease = fixture.0.join("probe.lock");
    let gate = fixture.0.join("gate");
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &gate,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        0,
    )
    .unwrap();
    let input = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&gate)
        .unwrap();
    let unknown = std::sync::Arc::new(AtomicBool::new(true));
    let state = Box::leak(Box::new(State::new(Unknown(unknown.clone()))));
    struct Release {
        state: &'static State<Unknown>,
        unknown: std::sync::Arc<AtomicBool>,
        input: File,
    }
    impl Drop for Release {
        fn drop(&mut self) {
            self.unknown.store(false, Ordering::Release);
            let _ = self.input.write_all(b"release\n");
            self.state.settle();
        }
    }
    let mut release = Release {
        state,
        unknown,
        input,
    };
    let held =
        ramshared_exec::inherited_lease::acquire(&lease, rustix::process::geteuid().as_raw())
            .unwrap();
    let ready = fixture.0.join("ready");
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", "printf ready > \"$READY\"; read line < \"$GATE\""])
        .env("READY", &ready)
        .env("GATE", &gate)
        .stdin(Stdio::from(held));
    assert!(query_once(state, &mut cmd, Instant::now(), unix_ms().unwrap()).is_err());
    drop(cmd);
    assert!(ready.exists());
    assert!(
        ramshared_exec::inherited_lease::acquire(&lease, rustix::process::geteuid().as_raw())
            .is_err()
    );
    let replacement = fixture.0.join("replacement");
    let mut forbidden = Command::new("/usr/bin/touch");
    forbidden.arg(&replacement);
    assert!(matches!(
        state
            .owner
            .run_recorded(&mut forbidden, Duration::from_secs(1), 64),
        Err(ramshared_exec::retained_process::ServingCommandError::Busy)
    ));
    assert!(!replacement.exists());
    release.unknown.store(false, Ordering::Release);
    release.input.write_all(b"release\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !state.owner.reconcile() && Instant::now() < deadline {
        std::thread::sleep(POLL);
    }
    assert_eq!(
        state.owner.state(),
        ramshared_exec::retained_process::HelperState::Idle
    );
    assert!(
        ramshared_exec::inherited_lease::acquire(&lease, rustix::process::geteuid().as_raw())
            .is_ok()
    );
    state
        .owner
        .run_recorded(&mut Command::new("/bin/true"), Duration::from_secs(1), 64)
        .unwrap();
}

#[test]
fn unprivileged_observer_entrypoints_fail_closed_and_stopped_state_refuses_restart() {
    if rustix::process::geteuid().as_raw() != 0 {
        assert!(start().is_err());
    }
    let _ = off_budgets(1_700_000_000_000);
    let stopped = state();
    stopped.stop.store(true, Ordering::Release);
    let listener = transport::bind(b"ramshared-stopped-observer-test").unwrap();
    assert!(start_with(stopped, listener, || {}).is_err());

    // Cover lease failure and panic-containment error branches inside `run_queries_with`.
    let fail_state = state();
    let uid = rustix::process::geteuid().as_raw();
    let missing_parent_lease = std::env::temp_dir()
        .join(format!("ramshared-missing-lease-{}", std::process::id()))
        .join("nested")
        .join("probe.lock");
    let handle = std::thread::spawn(move || {
        run_queries_with(fail_state, missing_parent_lease, uid, |_held| {
            Command::new("/bin/true")
        });
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    while fail_state.failure.lock().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(POLL);
    }
    assert!(fail_state.failure.lock().unwrap().is_some());
    fail_state.stop.store(true, Ordering::Release);
    handle.join().unwrap();

    // Cover panic recovery and `off_budgets_with` telemetry acceptance cleanly.
    let panic_state = state();
    let fixture = Fixture::new();
    let panic_lease = fixture.0.join("panic-probe.lock");
    let panic_handle = std::thread::spawn(move || {
        run_queries_with(panic_state, panic_lease, uid, |_held| -> Command {
            panic!("simulated observer panic")
        });
    });
    panic_handle.join().unwrap();
    assert!(panic_state.stop.load(Ordering::Acquire));

    let accepted = off_budgets_with(
        1_700_000_000_000,
        || true,
        |_| {
            Some(Packet {
                report: Some(report(1, 1_700_000_000_000)),
                age_ms: 0,
            })
        },
    );
    assert_eq!(accepted.map(|b| b.len()), Some(1));
    assert!(off_budgets_with(1_700_000_000_000, || true, |_| None).is_none());
}
