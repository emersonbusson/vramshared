// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/tests/gpu_identity_supervisor.rs
//! Custody policy with synthetic process effects; never physical qualification.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use ramshared_winsvc::gpu_identity::supervisor::{
    ExitObservation, PipeRead, Producer, Supervisor, Tick,
};
use ramshared_winsvc::gpu_identity::{HostGpuIdentityReport, ProducerProcess, encode_report};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
struct Effects {
    exit: Option<u32>,
    empty: bool,
    stops: usize,
    drops: usize,
    stop_error: bool,
    output: Vec<u8>,
    closed: bool,
    stderr: Vec<u8>,
    identity_error: bool,
    observe_error: bool,
    read_error: bool,
    invalid_read: bool,
}
struct Process(Arc<Mutex<Effects>>);
impl Drop for Process {
    fn drop(&mut self) {
        self.0.lock().unwrap().drops += 1;
    }
}
impl Producer for Process {
    fn identity(&self) -> Result<ProducerProcess, String> {
        if self.0.lock().unwrap().identity_error {
            return Err("no process identity".into());
        }
        Ok(ProducerProcess {
            pid: 42,
            creation_time_100ns: 77,
        })
    }
    fn observe_exit(&mut self) -> Result<ExitObservation, String> {
        let effects = self.0.lock().unwrap();
        if effects.observe_error {
            return Err("no native exit observation".into());
        }
        Ok(ExitObservation {
            code: effects.exit,
            job_empty: effects.empty,
        })
    }
    fn read_stdout(&mut self, buffer: &mut [u8]) -> Result<PipeRead, String> {
        let mut effects = self.0.lock().unwrap();
        if effects.read_error {
            return Err("native pipe failed".into());
        }
        if effects.invalid_read {
            return Ok(PipeRead::Bytes(buffer.len() + 1));
        }
        if !effects.output.is_empty() {
            let count = buffer.len().min(effects.output.len());
            buffer[..count].copy_from_slice(&effects.output[..count]);
            effects.output.drain(..count);
            Ok(PipeRead::Bytes(count))
        } else if effects.closed {
            Ok(PipeRead::Closed)
        } else {
            Ok(PipeRead::Pending)
        }
    }
    fn read_stderr(&mut self, buffer: &mut [u8]) -> Result<PipeRead, String> {
        let mut effects = self.0.lock().unwrap();
        if effects.stderr.is_empty() {
            return Ok(if effects.closed {
                PipeRead::Closed
            } else {
                PipeRead::Pending
            });
        }
        let count = buffer.len().min(effects.stderr.len());
        buffer[..count].copy_from_slice(&effects.stderr[..count]);
        effects.stderr.drain(..count);
        Ok(PipeRead::Bytes(count))
    }
    fn terminate_job(&mut self) -> Result<(), String> {
        let mut effects = self.0.lock().unwrap();
        effects.stops += 1;
        if effects.stop_error {
            Err("termination refused".into())
        } else {
            Ok(())
        }
    }
}
fn report() -> HostGpuIdentityReport {
    HostGpuIdentityReport {
        schema_version: 1,
        producer: ProducerProcess {
            pid: 42,
            creation_time_100ns: 77,
        },
        started_at_unix_ms: 1010,
        elapsed_ms: 1,
        adapters: vec![],
    }
}
fn fixture() -> (Supervisor<Process>, Arc<Mutex<Effects>>, Instant) {
    (
        Supervisor::default(),
        Arc::new(Mutex::new(Effects::default())),
        Instant::now(),
    )
}

#[test]
fn supervisor_keeps_pending_native_work_and_refuses_replacement() {
    let (mut owner, effects, start) = fixture();
    owner
        .start(
            || Ok(Process(effects.clone())),
            start,
            1000,
            Duration::from_millis(100),
        )
        .unwrap();
    assert!(matches!(
        owner.tick(start + Duration::from_millis(100), 1100),
        Tick::Pending
    ));
    assert_eq!(effects.lock().unwrap().stops, 1);
    let mut spawned = false;
    assert!(
        owner
            .start(
                || {
                    spawned = true;
                    Ok(Process(effects.clone()))
                },
                start,
                1000,
                Duration::from_secs(1)
            )
            .is_err()
    );
    assert!(!spawned);
    assert!(matches!(
        owner.tick(start + Duration::from_millis(200), 1200),
        Tick::Pending
    ));
    assert_eq!(effects.lock().unwrap().stops, 1);
    assert_eq!(effects.lock().unwrap().drops, 0);
    {
        let mut effects = effects.lock().unwrap();
        effects.exit = Some(1);
        effects.empty = true;
        effects.closed = true;
    }
    assert!(matches!(
        owner.tick(start + Duration::from_millis(300), 1300),
        Tick::Completed(Err(_))
    ));
    assert_eq!(effects.lock().unwrap().drops, 1);
    assert!(!owner.busy());
    assert!(owner.snapshot(start).is_none());
}

#[test]
fn supervisor_publishes_only_matching_complete_reports_before_original_deadline() {
    let (mut owner, effects, start) = fixture();
    {
        let mut e = effects.lock().unwrap();
        e.output = encode_report(&report()).unwrap();
        e.exit = Some(0);
        e.empty = true;
        e.closed = true;
    }
    owner
        .start(
            || Ok(Process(effects.clone())),
            start,
            1000,
            Duration::from_millis(100),
        )
        .unwrap();
    assert!(matches!(
        owner.tick(start + Duration::from_millis(20), 1020),
        Tick::Running
    ));
    assert!(matches!(
        owner.tick(start + Duration::from_millis(21), 1021),
        Tick::Completed(Ok(_))
    ));
    assert_eq!(
        owner.snapshot(start + Duration::from_millis(22)),
        Some(&report())
    );
    assert!(
        owner
            .snapshot(start + Duration::from_millis(ramshared_vram::GPU_BUDGET_MAX_AGE_MS))
            .is_none()
    );
    let sample = owner
        .aged_snapshot(start + Duration::from_millis(22) + Duration::from_micros(500))
        .unwrap();
    assert_eq!(
        sample.age_ms, 23,
        "parent age is rounded up, never renewed on read"
    );
    assert_eq!(sample.report, report());
    assert_eq!(
        owner
            .aged_snapshot(start + Duration::from_millis(24))
            .unwrap()
            .age_ms,
        24
    );
    assert!(
        owner
            .aged_snapshot(start - Duration::from_millis(1))
            .is_none()
    );
    assert!(
        owner
            .aged_snapshot(start + Duration::from_millis(ramshared_vram::GPU_BUDGET_MAX_AGE_MS))
            .is_none()
    );
    assert_eq!(effects.lock().unwrap().stops, 0);
    assert_eq!(effects.lock().unwrap().drops, 1);
}

#[test]
fn supervisor_refuses_process_clock_exit_decode_and_diagnostic_changes() {
    for change in [
        "pid", "creation", "early", "future", "duration", "clock", "decode", "exit", "stderr",
    ] {
        let (mut owner, effects, start) = fixture();
        let mut report = report();
        match change {
            "pid" => report.producer.pid += 1,
            "creation" => report.producer.creation_time_100ns += 1,
            "early" => report.started_at_unix_ms = 999,
            "future" => report.started_at_unix_ms = 2000,
            "duration" => report.elapsed_ms = 50,
            _ => {}
        }
        {
            let mut e = effects.lock().unwrap();
            e.output = encode_report(&report).unwrap();
            e.output.push(b'\n');
            e.closed = true;
            e.exit = Some(0);
            e.empty = true;
            if change == "decode" {
                e.output = b"not a report".to_vec();
            }
            if change == "exit" {
                e.exit = Some(1);
            }
            if change == "stderr" {
                e.stderr = b"unexpected diagnostics".to_vec();
            }
        }
        owner
            .start(
                || Ok(Process(effects.clone())),
                start,
                1000,
                Duration::from_millis(100),
            )
            .unwrap();
        owner.tick(start + Duration::from_millis(20), 2000);
        let wall = if change == "clock" { 0 } else { 1021 };
        assert!(
            matches!(
                owner.tick(start + Duration::from_millis(21), wall),
                Tick::Completed(Err(_))
            ),
            "{change}"
        );
        assert!(
            owner.snapshot(start + Duration::from_millis(22)).is_none(),
            "{change}"
        );
    }
}

#[test]
fn supervisor_refuses_invalid_admission_before_spawn_and_clears_old_snapshot() {
    let (mut owner, effects, start) = fixture();
    for (clock, timeout) in [
        (0, Duration::from_secs(1)),
        (1000, Duration::ZERO),
        (
            1000,
            Duration::from_millis(ramshared_vram::GPU_BUDGET_MAX_AGE_MS),
        ),
    ] {
        let mut spawned = false;
        assert!(
            owner
                .start(
                    || {
                        spawned = true;
                        Ok(Process(effects.clone()))
                    },
                    start,
                    clock,
                    timeout
                )
                .is_err()
        );
        assert!(!spawned);
    }
    {
        let mut e = effects.lock().unwrap();
        e.output = encode_report(&report()).unwrap();
        e.exit = Some(0);
        e.empty = true;
        e.closed = true;
    }
    owner
        .start(
            || Ok(Process(effects.clone())),
            start,
            1000,
            Duration::from_millis(100),
        )
        .unwrap();
    owner.tick(start + Duration::from_millis(20), 1020);
    owner.tick(start + Duration::from_millis(21), 1021);
    assert!(owner.snapshot(start + Duration::from_millis(22)).is_some());
    assert!(owner.snapshot(start - Duration::from_millis(1)).is_none());
    assert!(
        owner
            .start(
                || Err("spawn refused".into()),
                start,
                1000,
                Duration::from_secs(1)
            )
            .is_err()
    );
    assert!(owner.snapshot(start + Duration::from_millis(22)).is_none());
    assert!(matches!(owner.tick(start, 1000), Tick::Idle));
}

#[test]
fn supervisor_retains_exited_child_until_its_job_and_pipes_are_settled() {
    let (mut owner, effects, start) = fixture();
    {
        let mut e = effects.lock().unwrap();
        e.exit = Some(0);
        e.output = encode_report(&report()).unwrap();
        e.stop_error = true;
    }
    owner
        .start(
            || Ok(Process(effects.clone())),
            start,
            1000,
            Duration::from_millis(100),
        )
        .unwrap();
    assert!(matches!(
        owner.tick(start + Duration::from_millis(20), 1020),
        Tick::Pending
    ));
    assert!(matches!(
        owner.tick(start + Duration::from_millis(100), 1100),
        Tick::Pending
    ));
    assert!(matches!(
        owner.tick(start + Duration::from_millis(200), 1200),
        Tick::Pending
    ));
    assert_eq!(effects.lock().unwrap().stops, 1);
    assert_eq!(effects.lock().unwrap().drops, 0);
    effects.lock().unwrap().empty = true;
    assert!(matches!(
        owner.tick(start + Duration::from_millis(201), 1201),
        Tick::Pending
    ));
    effects.lock().unwrap().closed = true;
    assert!(matches!(
        owner.tick(start + Duration::from_millis(202), 1202),
        Tick::Completed(Err(_))
    ));
    assert_eq!(effects.lock().unwrap().drops, 1);
}

#[test]
fn supervisor_bounds_output_and_preserves_native_failure_custody() {
    for change in [
        "stdout",
        "stderr",
        "read",
        "count",
        "observe",
        "identity",
        "cancel",
        "monotonic",
    ] {
        let (mut owner, effects, start) = fixture();
        {
            let mut e = effects.lock().unwrap();
            match change {
                "stdout" => e.output = vec![b'x'; 70 * 1024],
                "stderr" => e.stderr = vec![b'x'; 5000],
                "read" => e.read_error = true,
                "count" => e.invalid_read = true,
                "observe" => e.observe_error = true,
                "identity" => e.identity_error = true,
                _ => {}
            }
        }
        owner
            .start(
                || Ok(Process(effects.clone())),
                start,
                1000,
                Duration::from_secs(1),
            )
            .unwrap();
        if change == "cancel" {
            owner.cancel();
        }
        let mut result = Tick::Idle;
        for n in 1..=20 {
            let now = if change == "monotonic" {
                start - Duration::from_millis(1)
            } else {
                start + Duration::from_millis(n)
            };
            result = owner.tick(now, 1000 + n);
        }
        assert!(matches!(result, Tick::Pending), "{change}");
        assert_eq!(effects.lock().unwrap().stops, 1, "{change}");
        assert_eq!(effects.lock().unwrap().drops, 0, "{change}");
        {
            let mut e = effects.lock().unwrap();
            e.exit = Some(2);
            e.empty = true;
            e.closed = true;
            e.observe_error = false;
        }
        assert!(
            matches!(
                owner.tick(start + Duration::from_millis(30), 1030),
                Tick::Completed(Err(_))
            ),
            "{change}"
        );
        assert!(!owner.busy());
        assert!(owner.snapshot(start).is_none());
    }
}

#[test]
fn supervisor_retains_process_when_identity_effect_unwinds() {
    struct PanicIdentity(Process);
    impl Producer for PanicIdentity {
        fn identity(&self) -> Result<ProducerProcess, String> {
            panic!("injected identity unwind");
        }
        fn observe_exit(&mut self) -> Result<ExitObservation, String> {
            self.0.observe_exit()
        }
        fn read_stdout(&mut self, buffer: &mut [u8]) -> Result<PipeRead, String> {
            self.0.read_stdout(buffer)
        }
        fn read_stderr(&mut self, buffer: &mut [u8]) -> Result<PipeRead, String> {
            self.0.read_stderr(buffer)
        }
        fn terminate_job(&mut self) -> Result<(), String> {
            self.0.terminate_job()
        }
    }
    let mut owner = Supervisor::default();
    let effects = Arc::new(Mutex::new(Effects::default()));
    let start = Instant::now();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            owner
                .start(
                    || Ok(PanicIdentity(Process(effects.clone()))),
                    start,
                    1000,
                    Duration::from_secs(1),
                )
                .unwrap();
        }))
        .is_err()
    );
    assert!(owner.busy());
    assert_eq!(effects.lock().unwrap().drops, 0);
    owner.cancel();
    assert!(matches!(owner.tick(start, 1000), Tick::Pending));
    {
        let mut e = effects.lock().unwrap();
        e.exit = Some(2);
        e.empty = true;
        e.closed = true;
    }
    assert!(matches!(
        owner.tick(start + Duration::from_millis(1), 1001),
        Tick::Completed(Err(_))
    ));
    assert_eq!(effects.lock().unwrap().drops, 1);
}

#[test]
fn fresh_catalog_survives_pending_refresh_without_renewal_and_revokes_on_failure() {
    let (mut owner, effects, start) = fixture();
    {
        let mut e = effects.lock().unwrap();
        e.output = encode_report(&report()).unwrap();
        e.exit = Some(0);
        e.empty = true;
        e.closed = true;
    }
    owner
        .start(
            || Ok(Process(effects.clone())),
            start,
            1000,
            Duration::from_millis(100),
        )
        .unwrap();
    owner.tick(start + Duration::from_millis(20), 1020);
    owner.tick(start + Duration::from_millis(21), 1021);
    assert_eq!(
        owner.snapshot(start + Duration::from_millis(22)),
        Some(&report())
    );
    let next = Arc::new(Mutex::new(Effects::default()));
    owner
        .start(
            || Ok(Process(next.clone())),
            start + Duration::from_millis(2500),
            3500,
            Duration::from_millis(100),
        )
        .unwrap();
    assert_eq!(
        owner
            .aged_snapshot(start + Duration::from_millis(2500))
            .unwrap()
            .age_ms,
        2500,
        "a pending successor cannot erase a still-fresh verified observation"
    );
    assert!(matches!(
        owner.tick(start + Duration::from_millis(2501), 3501),
        Tick::Running
    ));
    assert_eq!(
        owner
            .aged_snapshot(start + Duration::from_millis(2502))
            .unwrap()
            .age_ms,
        2502
    );
    assert!(
        owner
            .aged_snapshot(start + Duration::from_millis(5000))
            .is_none(),
        "retention cannot renew original age"
    );
    next.lock().unwrap().stderr = b"native refresh failed".to_vec();
    assert!(matches!(
        owner.tick(start + Duration::from_millis(2503), 3503),
        Tick::Pending
    ));
    assert!(
        owner
            .aged_snapshot(start + Duration::from_millis(2503))
            .is_none(),
        "observed refresh failure revokes prior metadata"
    );
    assert!(
        owner.busy(),
        "native custody remains owned after metadata revocation"
    );
    {
        let mut e = next.lock().unwrap();
        e.exit = Some(1);
        e.empty = true;
        e.closed = true;
    }
    assert!(matches!(
        owner.tick(start + Duration::from_millis(2504), 3504),
        Tick::Completed(Err(_))
    ));
    assert!(!owner.busy());
    assert_eq!(next.lock().unwrap().stops, 1);
}

#[test]
fn vm_probe_uses_same_process_binding_and_original_clock() {
    use ramshared_winsvc::gpu_identity::supervisor::{ProbeSupervisor, ProbeTick};
    use ramshared_winsvc::vm_identity::{VmIdentity, VmIdentityReport, VmOsClassification};
    for mismatch in [false, true] {
        let (_, effects, start) = fixture();
        let report = VmIdentityReport {
            schema_version: 1,
            producer: ProducerProcess {
                pid: if mismatch { 43 } else { 42 },
                creation_time_100ns: 77,
            },
            started_at_unix_ms: 1000,
            elapsed_ms: 1,
            systems: vec![VmIdentity {
                system_id: "native-system".into(),
                runtime_id: [1; 16],
                os_classification: VmOsClassification::Linux,
            }],
        };
        {
            let mut e = effects.lock().unwrap();
            e.output = serde_json::to_vec(&report).unwrap();
            e.exit = Some(0);
            e.empty = true;
            e.closed = true;
        }
        let mut owner: ProbeSupervisor<Process, VmIdentityReport> = ProbeSupervisor::default();
        owner
            .start(
                || Ok(Process(effects.clone())),
                start,
                1000,
                Duration::from_millis(100),
            )
            .unwrap();
        assert!(matches!(
            owner.tick(start + Duration::from_millis(20), 1020),
            ProbeTick::Running
        ));
        let result = owner.tick(start + Duration::from_millis(21), 1021);
        if mismatch {
            assert!(matches!(result, ProbeTick::Completed(Err(_))));
            assert!(owner.snapshot(start + Duration::from_millis(22)).is_none());
        } else {
            assert!(matches!(result, ProbeTick::Completed(Ok(_))));
            let (observed, age) = owner
                .snapshot_with_age(start + Duration::from_millis(22) + Duration::from_micros(500))
                .unwrap();
            assert_eq!(observed, &report);
            assert_eq!(age, 23);
            assert!(
                owner
                    .snapshot(start + Duration::from_millis(ramshared_vram::GPU_BUDGET_MAX_AGE_MS))
                    .is_none()
            );
        }
        assert!(!owner.busy());
        assert_eq!(effects.lock().unwrap().drops, 1);
    }
}

#[test]
fn verified_native_failure_preserves_bounded_query_diagnostic() {
    let (mut owner, effects, start) = fixture();
    {
        let mut e = effects.lock().unwrap();
        e.stderr = b"VM inspection failed: HRESULT 0x80070005\n".to_vec();
        e.exit = Some(2);
        e.empty = true;
        e.closed = true;
    }
    owner
        .start(
            || Ok(Process(effects.clone())),
            start,
            1000,
            Duration::from_millis(100),
        )
        .unwrap();
    owner.tick(start + Duration::from_millis(20), 1020);
    let Tick::Completed(Err(error)) = owner.tick(start + Duration::from_millis(21), 1021) else {
        panic!("verified failure was not completed")
    };
    assert!(
        error.contains("HRESULT 0x80070005"),
        "diagnostic lost: {error}"
    );
    assert!(!owner.busy());
    assert!(owner.snapshot(start + Duration::from_millis(22)).is_none());
}
