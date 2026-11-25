// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/tests/vm_identity_supervised.rs
//! Explicit native HCS observation. Routing evidence never grants GPU allocation.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(windows)]
fn built_product_executable() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_ramshared-winsvc"))
}

#[cfg(windows)]
#[test]
#[ignore = "read-only: native ComputeCore enumeration permissions and exact built product"]
fn native_vm_probe_binds_actual_child_and_repeated_sampler_observations() {
    use ramshared_winsvc::gpu_identity::process::NativeProcess;
    use ramshared_winsvc::gpu_identity::supervisor::{ProbeSupervisor, ProbeTick, Producer};
    use ramshared_winsvc::vm_identity::{VmIdentityReport, sampler};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
    fn wall() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }
    let product = built_product_executable();
    let expected = ramshared_winsvc::vm_id_resolve::parse_guid(
        &std::env::var("RAMSHARED_NATIVE_EXPECTED_VM_ID")
            .expect("explicit native probe requires the current guest VM ID"),
    )
    .unwrap();
    let mut owner: ProbeSupervisor<NativeProcess, VmIdentityReport> = ProbeSupervisor::default();
    let mut identity = None;
    let start = Instant::now();
    owner
        .start(
            || {
                let process = NativeProcess::spawn_vm_identity(&product)?;
                identity = Some(process.identity()?);
                Ok(process)
            },
            start,
            wall(),
            Duration::from_secs(2),
        )
        .unwrap();
    let report = loop {
        match owner.tick(Instant::now(), wall()) {
            ProbeTick::Completed(report) => {
                break report.expect("native HCS prerequisite/query must succeed explicitly");
            }
            ProbeTick::Idle => panic!("custody vanished"),
            _ => std::thread::sleep(Duration::from_millis(10)),
        }
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "native VM custody remains pending"
        );
    };
    assert!(
        report
            .systems
            .iter()
            .any(|system| system.runtime_id == expected),
        "native HCS did not observe the actual current guest runtime"
    );
    let candidate = ramshared_winsvc::vm_id_resolve::select_connect_candidate(&report)
        .expect("unique running guest route candidate must resolve");
    assert_eq!(candidate.runtime_id, expected);
    assert_eq!(Some(report.producer.clone()), identity);
    assert!(!owner.busy());
    eprintln!(
        "NATIVE_VM_PROBE_OK systems={} process_verified=true exited=true job_empty=true",
        report.systems.len()
    );
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
    loop {
        if let Some(observed) = sampler::observation() {
            observed.validate().unwrap();
            if let Some(original) = &first {
                let original: &VmIdentityReport = original;
                if observed.producer != original.producer {
                    break;
                }
            } else {
                first = Some(observed);
            }
        }
        assert!(
            Instant::now() < deadline,
            "two native VM observations were not completed"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    sampler::stop_and_wait().unwrap();
    drop(guard);
    assert!(sampler::observation().is_none());
    eprintln!("NATIVE_VM_SAMPLER_OK distinct_children=2 duplicate_start_refused=true stopped=true");
}

#[cfg(windows)]
#[test]
#[ignore = "read-only: Windows token denied ComputeCore enumeration; refusal control only"]
fn native_vm_permission_failure_is_observable_without_authority_or_abandoned_custody() {
    use ramshared_winsvc::vm_identity::sampler;
    use std::time::{Duration, Instant};
    let product = built_product_executable();
    sampler::start(product).unwrap();
    struct Stop;
    impl Drop for Stop {
        fn drop(&mut self) {
            let _ = sampler::stop_and_wait();
        }
    }
    let stop = Stop;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            sampler::observation().is_none(),
            "refusal control requires a denied native token"
        );
        if let Some(failure) = sampler::diagnostic() {
            assert!(
                failure.contains("0x8037011b"),
                "expected native access refusal, observed: {failure}"
            );
            let refusal = ramshared_winsvc::vm_id_resolve::resolve_guest_vm_guid()
                .unwrap_err()
                .to_string();
            assert!(refusal.contains("0x8037011b"));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "native failure diagnostic was lost"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    sampler::stop_and_wait().unwrap();
    drop(stop);
    assert!(sampler::observation().is_none());
    eprintln!("NATIVE_VM_REFUSAL_OK access_denied=true authority=false stopped=true");
}
