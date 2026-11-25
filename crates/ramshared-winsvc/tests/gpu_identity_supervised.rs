// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/tests/gpu_identity_supervised.rs
//! Real Windows supervisor/sampler control. No context, allocation or swap.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(windows)]
#[test]
#[ignore = "read-only: installed native GPU driver and exact built product required"]
fn public_supervised_identity_matches_native_child_and_repeats() {
    use ramshared_winsvc::gpu_identity::supervisor::{Producer, Supervisor, Tick};
    use ramshared_winsvc::gpu_identity::{encode_report, process::NativeProcess, sampler};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
    fn wall() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }
    let product = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("ramshared-winsvc.exe");
    let mut owner = Supervisor::default();
    let mut native_identity = None;
    let start = Instant::now();
    owner
        .start(
            || {
                let process = NativeProcess::spawn_identity(&product)?;
                native_identity = Some(process.identity()?);
                Ok(process)
            },
            start,
            wall(),
            Duration::from_secs(2),
        )
        .unwrap();
    let report = loop {
        match owner.tick(Instant::now(), wall()) {
            Tick::Completed(report) => break report.unwrap(),
            Tick::Idle => panic!("native process vanished before verification"),
            _ => std::thread::sleep(Duration::from_millis(10)),
        }
        // Keep polling the same owner; the native policy owns termination.
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "native custody remains pending"
        );
    };
    assert_eq!(Some(report.producer.clone()), native_identity);
    assert!(!owner.busy());
    assert!(!report.adapters.is_empty());
    assert_eq!(owner.snapshot(Instant::now()), Some(&report));
    eprintln!(
        "NATIVE_SUPERVISOR_CHILD_OK pid={} creation={} exited=true job_empty=true",
        report.producer.pid, report.producer.creation_time_100ns
    );

    assert!(sampler::start(std::path::PathBuf::from("relative.exe")).is_err());
    sampler::start(product.clone()).unwrap();
    struct Stop;
    impl Drop for Stop {
        fn drop(&mut self) {
            let _ = sampler::stop_and_wait();
        }
    }
    let guard = Stop;
    assert!(
        sampler::start(product)
            .unwrap_err()
            .contains("already started")
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut first = None;
    let mut repeated_age_reads = 0;
    let mut previous_age: Option<(ramshared_winsvc::gpu_identity::ProducerProcess, u64)> = None;
    let second = loop {
        if let Some(observation) = sampler::aged_observation() {
            observation.validate().unwrap();
            if let Some((producer, age)) = &previous_age
                && *producer == observation.report.producer
            {
                assert!(observation.age_ms > *age, "reading renewed parent age");
                repeated_age_reads += 1;
            }
            previous_age = Some((observation.report.producer.clone(), observation.age_ms));
            let report = observation.report;
            assert!(!report.adapters.is_empty());
            if let Some(first) = &first {
                let first: &ramshared_winsvc::gpu_identity::HostGpuIdentityReport = first;
                if report.producer != first.producer {
                    assert!(report.started_at_unix_ms > first.started_at_unix_ms);
                    break report;
                }
                assert_eq!(report, *first, "reading renewed an existing observation");
            } else {
                first = Some(report);
            }
        }
        assert!(
            Instant::now() < deadline,
            "sampler did not complete two native observations: {}",
            sampler::diagnostic()
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    eprintln!(
        "NATIVE_IDENTITY_REPORT {}",
        String::from_utf8(encode_report(&second).unwrap()).unwrap()
    );
    assert!(
        repeated_age_reads > 0,
        "original age control was not exercised"
    );
    sampler::stop_and_wait().unwrap();
    drop(guard);
    assert!(sampler::observation().is_none());
    assert!(sampler::aged_observation().is_none());
    eprintln!(
        "NATIVE_SAMPLER_AGE_OK original_parent_clock=true stopped=true repeated_reads={repeated_age_reads}"
    );
    assert!(sampler::diagnostic().contains("verified empty custody"));
    eprintln!("NATIVE_SAMPLER_OK distinct_children=2 duplicate_start_refused=true stopped=true");
}
