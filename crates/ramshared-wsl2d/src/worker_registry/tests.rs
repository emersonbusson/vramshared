// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/worker_registry/tests.rs
//! Real native child custody with explicitly synthetic driver identities.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::worker_custody::{ChildWorkerProcess, WorkerProcessControl, WorkerResources};
use std::process::{ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

fn adapter(key: &str) -> GpuAdapterIdentity {
    GpuAdapterIdentity {
        node_mask: None,
        backend: "cuda".into(),
        key: key.into(),
        luid: None,
    }
}
fn first() -> GpuAdapterIdentity {
    adapter("00112233445566778899aabbccddeeff")
}
fn second() -> GpuAdapterIdentity {
    adapter("ffeeddccbbaa99887766554433221100")
}

struct NativeChild {
    input: Option<ChildStdin>,
    process: ChildWorkerProcess,
    resources: WorkerResources,
}
impl NativeChild {
    fn spawn(reservation: Reservation) -> Self {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "read line"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take();
        let resources = reservation.bind(child);
        Self {
            input,
            process: ChildWorkerProcess::from_resources(resources.clone()),
            resources,
        }
    }
    fn finish(&mut self) {
        drop(self.input.take());
        let deadline = Instant::now() + Duration::from_secs(3);
        while !matches!(self.process.try_wait(), Ok(true)) {
            assert!(
                Instant::now() < deadline,
                "native fixture child did not exit"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
impl Drop for NativeChild {
    fn drop(&mut self) {
        self.finish();
    }
}

#[test]
fn independent_adapter_processes_hold_distinct_custody() {
    let registry = WorkerRegistry::default();
    let mut a = NativeChild::spawn(registry.reserve(Some(&first())).unwrap());
    let mut b = NativeChild::spawn(
        registry
            .reserve(Some(&second()))
            .expect("independent adapter lost its process slot"),
    );
    assert_ne!(a.resources.pid, b.resources.pid);
    assert!(matches!(a.process.try_wait(), Ok(false)));
    assert!(matches!(b.process.try_wait(), Ok(false)));
    assert!(registry.reserve(Some(&first())).is_err());
    assert!(registry.reserve(Some(&second())).is_err());
    a.finish();
    b.finish();
}

fn refusal(registry: &WorkerRegistry, identity: Option<&GpuAdapterIdentity>) -> io::ErrorKind {
    registry
        .reserve(identity)
        .err()
        .expect("scope must be refused")
        .kind()
}

#[test]
fn adapter_exit_does_not_release_healthy_peer() {
    let registry = WorkerRegistry::default();
    let mut a = NativeChild::spawn(registry.reserve(Some(&first())).unwrap());
    let mut b = NativeChild::spawn(registry.reserve(Some(&second())).unwrap());
    a.finish();
    let replacement = registry.reserve(Some(&first())).unwrap();
    assert_eq!(
        refusal(&registry, Some(&second())),
        io::ErrorKind::WouldBlock
    );
    assert!(matches!(b.process.try_wait(), Ok(false)));
    assert_eq!(refusal(&registry, None), io::ErrorKind::WouldBlock);
    drop(replacement);
    b.finish();
    drop(registry.reserve(None).unwrap());
    assert_eq!(
        registry.0.lock().unwrap().len(),
        1,
        "old quiescent records were retained"
    );
}

#[test]
fn automatic_and_alias_scopes_refuse_overlap() {
    let registry = WorkerRegistry::default();
    let automatic = registry.reserve(None).unwrap();
    assert_eq!(
        refusal(&registry, Some(&first())),
        io::ErrorKind::WouldBlock
    );
    drop(automatic);
    let pinned = registry.reserve(Some(&first())).unwrap();
    assert_eq!(refusal(&registry, None), io::ErrorKind::WouldBlock);
    let mut uppercase = first();
    uppercase.key.make_ascii_uppercase();
    assert_eq!(
        refusal(&registry, Some(&uppercase)),
        io::ErrorKind::WouldBlock
    );
    assert_eq!(
        uppercase.key, "00112233445566778899AABBCCDDEEFF",
        "custody changed wire identity"
    );
    drop(pinned);
    drop(registry.reserve(Some(&uppercase)).unwrap());

    let mut a = first();
    a.luid = Some("ABCDEF12:00000001".into());
    let pinned = registry.reserve(Some(&a)).unwrap();
    let mut b = second();
    b.backend = "vulkan".into();
    b.luid = Some("abcdef12:00000001".into());
    assert_eq!(refusal(&registry, Some(&b)), io::ErrorKind::WouldBlock);
    b.luid = Some("00000000:00000002".into());
    drop(
        registry
            .reserve(Some(&b))
            .expect("distinct observed LUIDs prove independent adapters"),
    );
    b.luid = None;
    assert_eq!(refusal(&registry, Some(&b)), io::ErrorKind::WouldBlock);
    let mut changed_luid = a.clone();
    changed_luid.luid = Some("00000000:00000003".into());
    assert_eq!(
        refusal(&registry, Some(&changed_luid)),
        io::ErrorKind::WouldBlock
    );
    drop(pinned);
    drop(registry.reserve(Some(&b)).unwrap());
}

#[test]
fn invalid_identity_does_not_reserve_native_or_creator_custody() {
    let registry = WorkerRegistry::default();
    for identity in [
        adapter("00000000000000000000000000000000"),
        GpuAdapterIdentity {
            backend: "unknown".into(),
            ..first()
        },
        GpuAdapterIdentity {
            luid: Some("invalid".into()),
            ..first()
        },
    ] {
        assert_eq!(
            refusal(&registry, Some(&identity)),
            io::ErrorKind::InvalidInput
        );
    }
    assert!(registry.0.lock().unwrap().is_empty());
    drop(registry.reserve(None).unwrap());
}

fn refused_start() -> ReservedStart {
    Box::new(|reservation| {
        drop(reservation);
        StartOutcome {
            result: Err("independent task completed".into()),
            creator_child: None,
        }
    })
}

fn poll_failure(startup: &mut Startup) -> String {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match startup.poll() {
            Err(error) => return error,
            Ok(None) => assert!(Instant::now() < deadline, "fixture startup did not report"),
            Ok(Some(_)) => panic!("refusal fixture returned a worker"),
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn held_startup_does_not_block_independent_adapter() {
    let registry = WorkerRegistry::default();
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let mut a = registry
        .begin(
            Some(&first()),
            Box::new(move |reservation| {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
                drop(reservation);
                StartOutcome {
                    result: Err("held task completed".into()),
                    creator_child: None,
                }
            }),
        )
        .unwrap();
    entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(matches!(a.poll(), Ok(None)));
    assert_eq!(
        refusal(&registry, Some(&first())),
        io::ErrorKind::WouldBlock
    );
    assert_eq!(refusal(&registry, None), io::ErrorKind::WouldBlock);
    let mut b = registry.begin(Some(&second()), refused_start()).unwrap();
    assert_eq!(poll_failure(&mut b), "independent task completed");
    assert!(
        matches!(a.poll(), Ok(None)),
        "first startup was not held during peer completion"
    );
    release_tx.send(()).unwrap();
    assert_eq!(poll_failure(&mut a), "held task completed");
}

#[test]
fn unknown_creator_fences_its_adapter_but_not_an_independent_peer() {
    let registry = WorkerRegistry::default();
    let mut a = registry
        .begin(
            Some(&first()),
            Box::new(|_reservation| panic!("manufactured startup panic")),
        )
        .unwrap();
    assert!(poll_failure(&mut a).contains("disconnected"));
    assert!(!a.is_quiescent(), "a panic became a confirmed outcome");
    assert_eq!(
        refusal(&registry, Some(&first())),
        io::ErrorKind::WouldBlock
    );
    assert_eq!(refusal(&registry, None), io::ErrorKind::WouldBlock);
    let mut b = registry.begin(Some(&second()), refused_start()).unwrap();
    assert_eq!(poll_failure(&mut b), "independent task completed");
    assert!(!a.is_quiescent());
}

#[test]
fn unknown_native_observation_keeps_scope_and_allows_independent_peer() {
    let registry = WorkerRegistry::default();
    let a = NativeChild::spawn(registry.reserve(Some(&first())).unwrap());
    let held = a.resources.child.lock().unwrap();
    assert_eq!(
        refusal(&registry, Some(&first())),
        io::ErrorKind::WouldBlock
    );
    let b = NativeChild::spawn(registry.reserve(Some(&second())).unwrap());
    assert_eq!(refusal(&registry, None), io::ErrorKind::WouldBlock);
    assert_ne!(a.resources.pid, b.resources.pid);
    drop(held);
    drop(a);
    drop(b);
}

#[test]
fn registry_contention_never_waits_and_poison_refuses_admission() {
    let registry = Arc::new(WorkerRegistry::default());
    let held = registry.0.lock().unwrap();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let contender = Arc::clone(&registry);
    let thread = std::thread::spawn(move || {
        tx.send(contender.reserve(None).err().map(|error| error.kind()))
            .unwrap();
    });
    let immediate = rx.recv_timeout(Duration::from_millis(250));
    // Release before asserting/joining, so a blocking implementation fails
    // observably without deadlocking the entire test process.
    drop(held);
    thread.join().unwrap();
    assert_eq!(immediate.unwrap(), Some(io::ErrorKind::WouldBlock));
    let _ = std::panic::catch_unwind(|| {
        let _held = registry.0.lock().unwrap();
        panic!("manufactured registry poison");
    });
    assert_eq!(refusal(&registry, None), io::ErrorKind::Other);
}

#[test]
fn exited_process_does_not_release_live_creator_scope() {
    let registry = WorkerRegistry::default();
    let (exited_tx, exited_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let mut startup = registry
        .begin(
            Some(&first()),
            Box::new(move |reservation| {
                let child = Command::new("/bin/sh")
                    .args(["-c", "exit 0"])
                    .spawn()
                    .unwrap();
                let resources = reservation.bind(child);
                let mut process = ChildWorkerProcess::from_resources(resources);
                let deadline = Instant::now() + Duration::from_secs(3);
                while !matches!(process.try_wait(), Ok(true)) {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(1));
                }
                assert!(process.stop_is_quiescent());
                exited_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
                StartOutcome {
                    result: Err("exited child with held creator".into()),
                    creator_child: Some(process),
                }
            }),
        )
        .unwrap();
    exited_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let before_release = refusal(&registry, Some(&first()));
    assert_eq!(refusal(&registry, None), io::ErrorKind::WouldBlock);
    drop(registry.reserve(Some(&second())).unwrap());
    release_tx.send(()).unwrap();
    assert_eq!(poll_failure(&mut startup), "exited child with held creator");
    let deadline = Instant::now() + Duration::from_secs(3);
    while !startup.is_quiescent() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(before_release, io::ErrorKind::WouldBlock);
    drop(registry.reserve(Some(&first())).unwrap());
}

#[test]
fn cancelled_startup_keeps_reserved_scope_until_creator_finishes() {
    let registry = WorkerRegistry::default();
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let mut startup = registry
        .begin(
            Some(&first()),
            Box::new(move |reservation| {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
                drop(reservation);
                StartOutcome {
                    result: Err("cancelled held task".into()),
                    creator_child: None,
                }
            }),
        )
        .unwrap();
    entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    startup.cancel();
    assert!(!startup.is_quiescent());
    assert_eq!(
        refusal(&registry, Some(&first())),
        io::ErrorKind::WouldBlock
    );
    drop(registry.reserve(Some(&second())).unwrap());
    release_tx.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !startup.is_quiescent() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    drop(registry.reserve(None).unwrap());
}

#[test]
fn cancelled_creator_retains_host_memory_until_its_child_is_gone() {
    use ramshared_wsl2d::host_memory_budget::{
        HostScope, MemoryObservation, MemorySample, SharedMemoryReservations,
    };
    let pool = SharedMemoryReservations::default();
    let now = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    assert!(pool.refresh(&MemoryObservation {
        guest: MemorySample {
            total_bytes: 200,
            available_bytes: 100,
            sampled_at_unix_ms: now
        },
        host: HostScope::Native,
        cgroups: Some(&[ramshared_wsl2d::host_memory_budget::CgroupSample {
            identity: ramshared_wsl2d::host_memory_budget::CgroupIdentity {
                device: 1,
                inode: 1
            },
            limit_bytes: None,
            current_bytes: 0,
            sampled_at_unix_ms: now,
        }]),
        guest_reserve_bytes: 0,
        host_reserve_bytes: 0,
        max_age_ms: 10_000,
    }));
    let registry = WorkerRegistry::default();
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let permit = pool.try_reserve(100).unwrap();
    let mut startup = registry
        .begin_with_host_memory(
            Some(&first()),
            Some(permit),
            Box::new(move |reservation| {
                let mut child = Command::new("/bin/sh")
                    .args(["-c", "read line"])
                    .stdin(Stdio::piped())
                    .spawn()
                    .unwrap();
                let input = child.stdin.take().unwrap();
                let process = ChildWorkerProcess::from_resources(reservation.bind(child));
                entered_tx.send((input, process.clone())).unwrap();
                release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
                StartOutcome {
                    result: Err("cancelled creator".into()),
                    creator_child: Some(process),
                }
            }),
        )
        .unwrap();
    let (input, mut child) = entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    startup.cancel();
    assert!(pool.try_reserve(1).is_none());
    assert!(!startup.is_quiescent());
    drop(input);
    let deadline = Instant::now() + Duration::from_secs(3);
    while !child.try_wait().unwrap() {
        assert!(Instant::now() < deadline, "child did not exit");
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(registry.reserve(Some(&second())).unwrap());
    assert!(
        pool.try_reserve(1).is_none(),
        "the live creator still owns the reservation"
    );
    release_tx.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !startup.is_quiescent() {
        assert!(Instant::now() < deadline, "creator did not settle");
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(registry.reserve(Some(&second())).unwrap());
    assert!(pool.try_reserve(100).is_some());
}

#[test]
fn failed_empty_admission_releases_only_its_own_reservation() {
    let registry = WorkerRegistry::default();
    let result: io::Result<()> = registry.admit(Some(&first()), |record| {
        let _reservation = record.native.reserve()?;
        Err(io::Error::other("manufactured empty startup refusal"))
    });
    assert!(result.is_err());
    drop(registry.reserve(None).unwrap());
    assert_eq!(registry.0.lock().unwrap().len(), 1);
}

#[test]
fn uuid_and_luid_only_keys_need_positive_physical_correspondence() {
    let registry = WorkerRegistry::default();
    let mut child = NativeChild::spawn(registry.reserve(Some(&first())).unwrap());
    let fallback = GpuAdapterIdentity {
        node_mask: Some(1),
        backend: "cuda".into(),
        key: "luid:12345678:00000001".into(),
        luid: Some("12345678:00000001".into()),
    };
    let unproved = registry.reserve(Some(&fallback)).is_ok();
    child.finish();
    assert!(
        !unproved,
        "UUID and LUID-only keys were treated as independent without correspondence"
    );
    let mut known = first();
    known.luid = Some("87654321:00000001".into());
    let mut child = NativeChild::spawn(registry.reserve(Some(&known)).unwrap());
    let proved = registry.reserve(Some(&fallback)).is_ok();
    child.finish();
    assert!(proved, "distinct known LUIDs lost their independent scopes");
}

#[test]
fn native_custody_preserves_observed_node_scope() {
    let mut request = first();
    request.luid = Some("AABBCCDD:00000001".into());
    request.node_mask = Some(2);
    let Scope::Pinned(normalized) = Scope::from_request(Some(&request)).unwrap() else {
        panic!("explicit identity lost");
    };
    assert_eq!(normalized.node_mask, Some(2));
    assert_eq!(normalized.luid.as_deref(), Some("aabbccdd:00000001"));
}

#[test]
fn guarded_registry_never_runs_a_task_after_authority_refusal() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let registry = WorkerRegistry::default();
    let executed = Arc::new(AtomicBool::new(false));
    let spy = Arc::clone(&executed);
    let mut startup = registry
        .begin_guarded(
            Some(&first()),
            Box::new(move |_| {
                spy.store(true, Ordering::Release);
                StartOutcome {
                    result: Err("unexpected task".into()),
                    creator_child: None,
                }
            }),
            Arc::new(|| false),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match startup.poll() {
            Err(_) => break,
            Ok(None) => assert!(Instant::now() < deadline),
            Ok(Some(_)) => panic!("refused authority produced a worker"),
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    while !startup.is_quiescent() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(!executed.load(Ordering::Acquire));
    assert!(registry.reserve(Some(&first())).is_ok());
}

#[test]
fn guarded_registry_refuses_handoff_when_memory_authority_is_revoked() {
    use ramshared_wsl2d::host_memory_budget::{
        HostScope, MemoryObservation, MemorySample, SharedMemoryReservations,
    };
    let now = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    let mut sample = MemoryObservation {
        guest: MemorySample {
            total_bytes: 200,
            available_bytes: 100,
            sampled_at_unix_ms: now,
        },
        host: HostScope::Native,
        cgroups: Some(&[ramshared_wsl2d::host_memory_budget::CgroupSample {
            identity: ramshared_wsl2d::host_memory_budget::CgroupIdentity {
                device: 1,
                inode: 1,
            },
            limit_bytes: None,
            current_bytes: 0,
            sampled_at_unix_ms: now,
        }]),
        guest_reserve_bytes: 0,
        host_reserve_bytes: 0,
        max_age_ms: 10_000,
    };
    let pool = SharedMemoryReservations::default();
    assert!(pool.refresh(&sample));
    let permit = pool.try_reserve(100).unwrap();
    let registry = WorkerRegistry::default();
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let mut startup = registry
        .begin_guarded_with_host_memory(
            Some(&first()),
            Some(permit),
            Box::new(move |reservation| {
                let _reservation = reservation;
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
                StartOutcome {
                    result: Err("fixture complete".into()),
                    creator_child: None,
                }
            }),
            Arc::new(|| true),
        )
        .unwrap();
    entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    sample.cgroups = Some(&[]);
    assert!(!pool.refresh(&sample));
    let refused = startup.poll().is_err();
    release_tx.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !startup.is_quiescent() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(refused, "revoked memory must cancel pending worker handoff");
}
