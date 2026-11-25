// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/origin_runtime/tests.rs
//! Source-selection components; hardware/platform authority is explicitly synthetic.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::native_fixture::Fixture;
use crate::native_origin::{AdmittedOrigin, NativeOriginRequest};
use ramshared_block::OriginStorage;
use ramshared_block::native_origin::{NativeOriginVolume, OriginMount};

fn held_file(fixture: &Fixture) -> FileOrigin {
    let mount = &fixture.devices[0].mounts[0];
    let volume = NativeOriginVolume::open(
        &OriginMount {
            mountpoint: mount.mountpoint.clone(),
            mount_id: mount.mount_id,
            major_minor: mount.major_minor.clone(),
        },
        fixture.uid,
    )
    .unwrap();
    FileOrigin::from_file(volume.verify_for_io(&fixture.manifest.target).unwrap())
}

fn request() -> NativeOriginRequest {
    NativeOriginRequest {
        manifest: ramshared_config::native_origin::default_native_origin_path(),
        expected_bytes: None,
    }
}

/// Uses the production worker launcher. The Rust test executable rejects its
/// internal worker arguments before loading a GPU driver, producing real EOF.
#[derive(Default)]
struct InitialWorkerStarter {
    initial: Option<crate::worker_startup::InitialStartResult>,
    replacement_requests: u32,
}
impl NbdRuntimeStarter for InitialWorkerStarter {
    fn spawn_isolated_gpu_worker(
        &mut self,
        params: &WorkerSpawnParams,
    ) -> crate::worker_startup::InitialStartResult {
        self.initial
            .take()
            .unwrap_or_else(|| crate::worker_launcher::spawn(params))
    }
    fn begin_isolated_worker_start(
        &mut self,
        _: &WorkerSpawnParams,
    ) -> std::io::Result<crate::worker_startup::Startup> {
        self.replacement_requests += 1;
        Err(std::io::ErrorKind::WouldBlock.into())
    }
    fn lock_memory(&mut self, _: bool, _: bool) -> Result<(), Box<dyn std::error::Error>> {
        panic!("origin construction must not lock memory")
    }
    fn start_acceptor(
        &mut self,
        _: std::os::unix::net::UnixListener,
        _: Arc<Vec<ramshared_block::handshake::Export>>,
        _: u16,
        _: std::sync::mpsc::SyncSender<crate::WMsg>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        panic!("origin construction must not accept connections")
    }
    fn nbd_used_kb(&mut self, _: &str) -> u64 {
        panic!("origin construction must not inspect swap")
    }
    fn try_claim_shutdown(&mut self, _: &str) -> Option<crate::ShutdownPermit> {
        panic!("origin construction must not claim NBD shutdown")
    }
    fn publish_demote(&mut self, _: &ramshared_wsl2d::DemoteStatusFile) {
        panic!("origin construction must not publish a demote")
    }
    fn elapsed_us(&mut self, _: std::time::Instant) -> u64 {
        panic!("origin construction must not measure a demote")
    }
    fn spawn_swapoff(&mut self, _: &str) -> std::sync::mpsc::Receiver<crate::SwapoffOutcome> {
        panic!("origin construction must not swapoff")
    }
    fn spawn_recovery_activation(
        &mut self,
        _: &str,
        _: i16,
    ) -> Result<std::sync::mpsc::Receiver<bool>, Box<dyn std::error::Error>> {
        panic!("origin construction must not activate swap")
    }
}

fn initial_runtime(fixture: &Fixture, starter: &mut InitialWorkerStarter) -> OriginRuntime {
    build(
        OpenedOrigin::Wsl(
            held_file(fixture),
            SealedOrigin {
                logical_capacity_mib: 1,
                partuuid: "synthetic-wsl-seal".into(),
                origin_vhdx: "synthetic-host-origin".into(),
            },
        ),
        8192,
        0,
        starter,
    )
    .unwrap()
}

#[test]
fn failed_initial_worker_is_retained_through_shutdown() {
    use std::time::Duration;

    let fixture = Fixture::new();
    let mut runtime = initial_runtime(&fixture, &mut InitialWorkerStarter::default());
    let worker = runtime.workers.iter_mut().next().unwrap();
    assert!(
        worker.supervisor.is_some(),
        "failed initial worker lost runtime shutdown ownership"
    );
    assert!(worker.supervisor.as_ref().unwrap().pid() > 0);
    assert!(
        worker
            .unavailable_reason
            .as_ref()
            .unwrap()
            .contains("worker handshake failed")
    );
    assert_eq!(
        runtime.backend.cache_state(),
        crate::OriginCacheState::Unavailable
    );
    runtime.backend.write_at(0, &[91; 4096]).unwrap();
    let mut bytes = [0; 4096];
    runtime.backend.read_at(0, &mut bytes).unwrap();
    assert_eq!(bytes, [91; 4096]);
    assert!(crate::worker_recovery::shutdown_worker_runtime(
        worker,
        Duration::from_secs(3),
        Duration::from_millis(10)
    ));
    assert!(crate::worker_recovery::gpu_worker_has_exited(worker));
    drop(runtime);
    drop(held_file(&fixture));
}

#[test]
fn unconfirmed_initial_failure_refuses_replacement_and_shutdown() {
    use crate::worker_custody::{ChildWorkerProcess, WorkerCustody};
    use crate::worker_recovery::{recover_isolated_gpu_worker_if_failed, shutdown_worker_runtime};
    use std::process::{Command, Stdio};
    use std::time::Duration;

    let fixture = Fixture::new();
    let mut child = Command::new("/bin/sh")
        .args(["-c", "read line"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let input = child.stdin.take().unwrap();
    let custody = Arc::new(WorkerCustody::default());
    let resources = custody.reserve().unwrap().bind(child);
    // Native child observation is held: a timeout/error cannot prove exit.
    // Guard and stdin RAII also release the child if an assertion fails.
    let observation = resources.child.lock().unwrap();
    let initial = crate::worker_startup::StartOutcome {
        result: Err("manufactured failed handshake after native child spawn".into()),
        creator_child: Some(ChildWorkerProcess::from_resources(resources.clone())),
    }
    .into_initial_result();
    let mut starter = InitialWorkerStarter {
        initial: Some(initial),
        replacement_requests: 0,
    };
    let mut runtime = initial_runtime(&fixture, &mut starter);
    let worker = runtime.workers.iter_mut().next().unwrap();
    let supervisor = worker.supervisor.as_ref().unwrap();
    assert_eq!(supervisor.pid(), resources.pid);
    let Some(crate::WorkerChildHandle::Process(process)) = supervisor.child.as_ref() else {
        panic!("exact native failure owner was replaced")
    };
    assert!(Arc::ptr_eq(&process.child, &resources.child));
    for _ in 0..3 {
        recover_isolated_gpu_worker_if_failed(&mut starter, &runtime.backend, worker, true);
    }
    assert_eq!(starter.replacement_requests, 0);
    assert_eq!(worker.budget.respawns_since_healthy, 0);
    assert!(
        worker
            .unavailable_reason
            .as_ref()
            .unwrap()
            .contains("manufactured failed handshake")
    );
    assert!(!shutdown_worker_runtime(
        worker,
        Duration::ZERO,
        Duration::from_millis(10)
    ));
    assert_eq!(worker.supervisor.as_ref().unwrap().pid(), resources.pid);
    assert!(custody.reserve().is_err());
    runtime.backend.write_at(4096, &[73; 4096]).unwrap();
    let mut bytes = [0; 4096];
    runtime.backend.read_at(4096, &mut bytes).unwrap();
    assert_eq!(bytes, [73; 4096]);

    drop(observation);
    drop(input);
    assert!(shutdown_worker_runtime(
        worker,
        Duration::from_secs(3),
        Duration::from_millis(10)
    ));
    assert!(resources.exited.load(std::sync::atomic::Ordering::Acquire));
    assert!(
        !resources
            .stop_active
            .load(std::sync::atomic::Ordering::Acquire)
    );
    assert!(
        !resources
            .reaper_active
            .load(std::sync::atomic::Ordering::Acquire)
    );
    assert!(custody.reserve().is_ok());
    drop(runtime);
    drop(held_file(&fixture));
}

#[test]
fn initial_failure_before_spawn_has_no_child_owner() {
    use std::time::Duration;

    let fixture = Fixture::new();
    let failure = crate::worker_startup::StartOutcome {
        result: Err("manufactured refusal before child creation".into()),
        creator_child: None,
    }
    .into_initial_result();
    let mut runtime = initial_runtime(
        &fixture,
        &mut InitialWorkerStarter {
            initial: Some(failure),
            replacement_requests: 0,
        },
    );
    let worker = runtime.workers.iter_mut().next().unwrap();
    assert!(worker.supervisor.is_none());
    assert_eq!(
        worker.unavailable_reason.as_deref(),
        Some(
            "isolated GPU cache worker startup failed: manufactured refusal before child creation"
        )
    );
    assert!(crate::worker_recovery::shutdown_worker_runtime(
        worker,
        Duration::ZERO,
        Duration::ZERO
    ));
    drop(runtime);
    drop(held_file(&fixture));
}

#[test]
fn unavailable_initial_reply_preserves_requested_adapter() {
    use ramshared_block::gpu_cache_worker::{FrameHeader, MSG_HANDSHAKE_RESP, STATUS_OK};
    use std::io::Write;
    let (socket, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
    peer.write_all(
        &FrameHeader {
            msg_type: MSG_HANDSHAKE_RESP,
            status: STATUS_OK,
            correlation_id: 1,
            offset: 0,
            payload_len: 0,
            aux: 0,
        }
        .encode(),
    )
    .unwrap();
    let mut client = crate::IpcCacheClient::new(socket, std::time::Duration::from_millis(50), 4096);
    client.perform_handshake().unwrap();
    let expected = ramshared_vram::GpuAdapterIdentity {
        backend: "cuda".into(),
        key: format!("{:032x}", 7),
        luid: None,
        node_mask: None,
    };
    let mut params = WorkerSpawnParams {
        generation: None,
        target_bytes: 4096,
        chunk_bytes: 4096,
        reserve_floor: 1024,
        expected_adapter: Some(expected.clone()),
        host_catalog: None,
    };
    let mut starter = InitialWorkerStarter {
        initial: Some(Ok((
            client,
            crate::IsolatedWorkerSupervisor { child: None },
        ))),
        replacement_requests: 0,
    };
    let (cache, supervisor, reason) = initial_worker(&mut starter, &mut params, 8192);
    assert_eq!(cache.state(), crate::OriginCacheState::Unavailable);
    assert_eq!(params.expected_adapter, Some(expected));
    assert_eq!(params.target_bytes, 4096);
    assert!(supervisor.is_some());
    assert!(reason.is_none());
}

#[test]
fn origin_selection_refuses_mixed_sources_before_open_and_preserves_absence() {
    let native = request();
    for wsl in [None, Some("must-not-open")] {
        let result = open_selected(
            wsl,
            wsl.map(|_| &native),
            8192,
            |_, _| panic!("no WSL admission for absent or mixed sources"),
            |_| panic!("no native admission for absent or mixed sources"),
        );
        if wsl.is_some() {
            assert!(result.is_err());
        } else {
            assert!(result.unwrap().is_none());
        }
    }
    assert!(open_configured(None, None, 8192).unwrap().is_none());
    assert!(open_configured(Some("refused"), Some(&request()), 8192).is_err());
}

#[test]
fn origin_selection_transfers_native_descriptor_and_manifest_capacity() {
    let fixture = Fixture::new();
    let (opened, size) = open_selected(
        None,
        Some(&request()),
        16384,
        |_, _| panic!("native selection must not read a WSL seal"),
        |request| {
            assert_eq!(request.expected_bytes, None);
            Ok(AdmittedOrigin {
                file: held_file(&fixture),
                logical_bytes: 8192,
                manifest: fixture.manifest.clone(),
                cache_session: None,
            })
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        size, 8192,
        "factory preserves admitted geometry over caller size"
    );
    assert!(opened.is_native());
    let OpenedOrigin::Native(admitted) = opened else {
        panic!("native provenance lost")
    };
    let file = admitted.file;
    let mut data = [0; 4096];
    file.read_exact_at(0, &mut data).unwrap();
    assert_eq!(data, [17; 4096]);
    drop(file);
    drop(held_file(&fixture));
}

#[test]
fn origin_selection_preserves_wsl_provenance_and_propagates_admission_errors() {
    let fixture = Fixture::new();
    let sealed = SealedOrigin {
        logical_capacity_mib: 1,
        partuuid: "synthetic-wsl-seal".into(),
        origin_vhdx: "synthetic-host-origin".into(),
    };
    let (opened, size) = open_selected(
        Some("synthetic-manifest"),
        None,
        8192,
        |path, size| {
            assert_eq!((path, size), ("synthetic-manifest", 8192));
            Ok((held_file(&fixture), sealed))
        },
        |_| panic!("WSL selection must not open native data"),
    )
    .unwrap()
    .unwrap();
    assert_eq!(size, 8192);
    assert!(!opened.is_native());
    let OpenedOrigin::Wsl(file, seal) = opened else {
        panic!("WSL provenance lost")
    };
    assert_eq!(seal.partuuid, "synthetic-wsl-seal");
    let mut data = [0; 4096];
    file.read_exact_at(4096, &mut data).unwrap();
    assert_eq!(data, [17; 4096]);
    drop(file);
    let failure = open_selected(
        Some("refused"),
        None,
        8192,
        |_, _| Err("WSL admission refused".into()),
        |_| panic!("must not fall through to native"),
    );
    assert!(
        failure
            .err()
            .unwrap()
            .to_string()
            .contains("WSL admission refused")
    );
    let failure = open_selected(
        None,
        Some(&request()),
        8192,
        |_, _| panic!("must not fall through to WSL"),
        |_| Err("native admission refused".into()),
    );
    assert!(
        failure
            .err()
            .unwrap()
            .to_string()
            .contains("native admission refused")
    );
    assert!(open_configured(Some("/tmp/never-a-canonical-wsl-seal"), None, 8192).is_err());
    let invalid = NativeOriginRequest {
        manifest: "/tmp/never-a-canonical-native-seal".into(),
        expected_bytes: None,
    };
    assert!(open_configured(None, Some(&invalid), 8192).is_err());
}

#[test]
fn origin_selection_matrix_never_falls_through_after_admission_failure() {
    use std::cell::Cell;
    let fixture = Fixture::new();
    let native_request = request();
    // One provider pair exercises the entire selection/error matrix. A failed
    // provider may never trigger the other source or leak its writable lease.
    for (wsl, native, fail, expected_calls, expected_native) in [
        (None, None, false, (0, 0), None),
        (Some("wsl"), Some(&native_request), false, (0, 0), None),
        (Some("wsl"), None, false, (1, 0), Some(false)),
        (None, Some(&native_request), false, (0, 1), Some(true)),
        (Some("wsl"), None, true, (1, 0), None),
        (None, Some(&native_request), true, (0, 1), None),
    ] {
        let calls = (Cell::new(0), Cell::new(0));
        let result = open_selected(
            wsl,
            native,
            8192,
            |_, _| {
                calls.0.set(calls.0.get() + 1);
                if fail {
                    return Err("WSL admission failed".into());
                }
                Ok((
                    held_file(&fixture),
                    SealedOrigin {
                        logical_capacity_mib: 1,
                        partuuid: "synthetic-wsl-seal".into(),
                        origin_vhdx: "synthetic-host-origin".into(),
                    },
                ))
            },
            |_| {
                calls.1.set(calls.1.get() + 1);
                if fail {
                    return Err("native admission failed".into());
                }
                Ok(AdmittedOrigin {
                    file: held_file(&fixture),
                    logical_bytes: 8192,
                    manifest: fixture.manifest.clone(),
                    cache_session: None,
                })
            },
        );
        assert_eq!((calls.0.get(), calls.1.get()), expected_calls);
        if let Some(native) = expected_native {
            let (origin, bytes) = result.unwrap().unwrap();
            assert_eq!(origin.is_native(), native);
            assert_eq!(bytes, 8192);
            drop(origin);
        } else if wsl.is_none() && native.is_none() {
            assert!(result.unwrap().is_none());
        } else {
            assert!(result.is_err());
        }
        drop(held_file(&fixture));
    }
}

#[test]
fn wsl_startup_accepts_bounded_catalog_wait_without_blocking_origin() {
    let fixture = Fixture::new();
    let mut starter = InitialWorkerStarter {
        initial: Some(Err("synthetic fallback".into())),
        replacement_requests: 0,
    };
    let runtime = build_with_catalog_wait(
        OpenedOrigin::Wsl(
            held_file(&fixture),
            SealedOrigin {
                logical_capacity_mib: 1,
                partuuid: "synthetic-wsl-seal".into(),
                origin_vhdx: "synthetic-host-origin".into(),
            },
        ),
        8192,
        0,
        &mut starter,
        std::time::Duration::from_millis(10),
    )
    .expect("origin startup completes when catalog is absent within bounded grace");
    assert_eq!(
        runtime.backend.origin_state(),
        ramshared_block::OriginState::Ready
    );
}
