// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/native_sampler/tests.rs
//! Real native threads, including TLS cleanup, exercise sampler custody itself.
#![allow(clippy::unwrap_used)]
use super::*;
use crate::gpu_identity::HostGpuIdentityReport;
use crate::thread_custody::{OwnedThread, ThreadState};
use std::sync::mpsc;
type Sampler = NativeSampler<HostGpuIdentityReport>;
struct SamplerGuard(&'static Sampler);
impl Drop for SamplerGuard {
    fn drop(&mut self) {
        let _ = self.0.stop_and_wait();
    }
}
fn fresh() -> &'static Sampler {
    Box::leak(Box::new(Sampler::new()))
}

#[test]
fn native_sampler_stopped_owner_refuses_late_start() {
    let sampler = fresh();
    let _ = sampler.stop_and_wait();
    let late = sampler.start(std::env::current_exe().unwrap(), ProbeKind::Vm);
    assert!(late.is_err(), "a stopped sampler accepted a new thread");
}

#[test]
fn native_sampler_stop_requires_thread_exit_after_custody_settlement() {
    use std::cell::RefCell;
    struct Cleanup(mpsc::Sender<()>, mpsc::Receiver<()>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.0.send(());
            let _ = self.1.recv_timeout(Duration::from_secs(3));
        }
    }
    thread_local! { static CLEANUP: RefCell<Option<Cleanup>> = const { RefCell::new(None) }; }
    let sampler = fresh();
    struct Stop(&'static Sampler);
    impl Drop for Stop {
        fn drop(&mut self) {
            let _ = self.0.stop_and_wait();
        }
    }
    let _stop = Stop(sampler);
    let (entered, ready) = mpsc::channel();
    let (release, blocked) = mpsc::channel();
    sampler
        .launch(|| {
            OwnedThread::spawn("sampler-tls-test", move |_| {
                CLEANUP.with(|slot| *slot.borrow_mut() = Some(Cleanup(entered, blocked)));
                Ok(())
            })
        })
        .unwrap();
    struct Release(mpsc::Sender<()>);
    impl Drop for Release {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }
    let release = Release(release);
    ready.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(!sampler.owner().lock().unwrap().busy());
    let before = Instant::now();
    assert_eq!(
        sampler.stop(Duration::from_millis(20)).unwrap(),
        ThreadState::Pending
    );
    assert!(before.elapsed() < Duration::from_secs(1));
    assert!(!sampler.stopped());
    assert!(sampler.observation().is_none());
    assert!(
        sampler
            .launch(|| panic!("duplicate launch executed"))
            .is_err()
    );
    let _ = release.0.send(());
    sampler.stop_and_wait().unwrap();
    assert!(sampler.stopped());
}

#[test]
fn native_sampler_failure_retains_failure_after_join() {
    let sampler = fresh();
    let _guard = SamplerGuard(sampler);
    sampler
        .launch(|| {
            OwnedThread::spawn("sampler-panic-test", move |_| {
                sampler.run_scoped(|| panic!("test-only actor panic"))
            })
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !sampler.stop_requested.load(Ordering::Acquire) && Instant::now() < deadline {
        std::thread::sleep(POLL);
    }
    assert!(
        sampler.stop_requested.load(Ordering::Acquire),
        "actor failure did not independently revoke publication"
    );
    assert!(sampler.observation().is_none());
    assert!(sampler.stop_and_wait().unwrap_err().contains("panicked"));
    assert!(sampler.stopped());
    assert!(sampler.stop_and_wait().is_err());
    assert!(sampler.observation().is_none());
}

#[test]
#[ignore = "read-only: exact built native product and process/job custody"]
fn native_sampler_panic_recovers_retained_child_custody() {
    use crate::gpu_identity::supervisor::Producer;
    let sampler = fresh();
    let _guard = SamplerGuard(sampler);
    let product = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("ramshared-winsvc.exe");
    let (ready, started) = mpsc::channel();
    let (finished, settled) = mpsc::channel();
    sampler
        .launch(|| {
            OwnedThread::spawn("sampler-child-panic-test", move |_| {
                let result = sampler.run_scoped(|| {
                    let mut owner = sampler.owner().lock().unwrap();
                    let mut actual = None;
                    owner
                        .start(
                            || {
                                let process = NativeProcess::spawn_vm_identity(&product)?;
                                actual = Some(process.identity()?);
                                Ok(process)
                            },
                            Instant::now(),
                            unix_ms(),
                            QUERY_BUDGET,
                        )
                        .unwrap();
                    assert!(actual.unwrap().pid > 0);
                    ready.send(()).unwrap();
                    panic!("test-only panic with process custody retained");
                });
                let _ = finished.send(());
                result
            })
        })
        .unwrap();
    started.recv_timeout(Duration::from_secs(3)).unwrap();
    settled.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(sampler.stop_requested.load(Ordering::Acquire));
    assert!(sampler.observation().is_none());
    assert!(sampler.stop_and_wait().unwrap_err().contains("panicked"));
    assert!(
        !sampler
            .owner()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .busy()
    );
    assert!(sampler.stopped());
    assert!(sampler.observation().is_none());
    eprintln!(
        "NATIVE_SAMPLER_PANIC_CLEANUP_OK thread_joined=true child_custody_empty=true failure_preserved=true"
    );
}

#[test]
fn storage_sampler_refuses_invalid_path_before_starting_actor() {
    let sampler = NativeSampler::<crate::host_storage::HostStorageReport>::new();
    let sampler = Box::leak(Box::new(sampler));
    let error = sampler
        .start(
            std::env::current_exe().unwrap(),
            ProbeKind::Storage("relative.vhdx".into()),
        )
        .unwrap_err();
    assert!(error.contains("absolute"));
    assert!(sampler.task.lock().unwrap().is_none());
}

#[test]
fn published_observation_remains_available_while_owner_lock_is_held() {
    use crate::gpu_identity::{HostGpuIdentity, ProducerProcess};

    let sampler = fresh();
    let start = Instant::now();
    let expires = start + Duration::from_millis(GPU_BUDGET_MAX_AGE_MS);
    let report = HostGpuIdentityReport {
        schema_version: 1,
        producer: ProducerProcess {
            pid: 77,
            creation_time_100ns: 99,
        },
        started_at_unix_ms: 1_000,
        elapsed_ms: 12,
        adapters: vec![HostGpuIdentity {
            ordinal: 0,
            allocator: ramshared_vram::GpuAdapterIdentity {
                backend: "cuda".into(),
                key: format!("{:032x}", 1),
                luid: Some("00000000:00011287".into()),
                node_mask: Some(1),
            },
            interface_id: "iface-0".into(),
        }],
    };
    sampler.publish_snapshot(Some((report.clone(), start, expires)));

    let _held = sampler.owner().lock().unwrap();
    let (observed, age) = sampler.observation_with_age().unwrap();
    assert_eq!(observed, report);
    assert!(age < GPU_BUDGET_MAX_AGE_MS);

    sampler.stop_requested.store(true, Ordering::Release);
    assert!(sampler.observation_with_age().is_none());
    sampler.stop_requested.store(false, Ordering::Release);
    sampler.publish_snapshot(Some((
        report, start, start, // expired: now >= expires
    )));
    assert!(sampler.observation_with_age().is_none());
}

#[test]
fn refresh_cadence_stays_well_within_budget_max_age_and_retry_is_bounded() {
    assert_eq!(
        REFRESH_CADENCE,
        Duration::from_millis(GPU_BUDGET_MAX_AGE_MS / 5)
    );
    assert!(REFRESH_CADENCE < QUERY_BUDGET);
    assert!(REFRESH_CADENCE * 3 <= Duration::from_millis(GPU_BUDGET_MAX_AGE_MS));
    assert!(POLL < REFRESH_CADENCE);
    assert!(IDLE_REFRESH_CADENCE > REFRESH_CADENCE);
    assert!(IDLE_REFRESH_CADENCE < Duration::from_millis(GPU_BUDGET_MAX_AGE_MS));

    let sampler = fresh();
    assert_eq!(sampler.refresh_cadence(), REFRESH_CADENCE);
    sampler.set_idle(true);
    assert_eq!(sampler.refresh_cadence(), IDLE_REFRESH_CADENCE);
    sampler.set_idle(false);
    assert_eq!(sampler.refresh_cadence(), REFRESH_CADENCE);
    sampler.set_paused(true);
    assert!(sampler.paused.load(Ordering::Acquire));
    sampler.set_paused(false);
    assert!(!sampler.paused.load(Ordering::Acquire));
}
