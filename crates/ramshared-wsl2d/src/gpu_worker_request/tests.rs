// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/gpu_worker_request/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
fn daemon_argv(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| arg.to_string()).collect()
}

#[test]
fn isolated_gpu_worker_arguments_accept_bounded_values() {
    let args = daemon_argv(&[
        "--fd",
        "42",
        "--target-bytes",
        "2147483648",
        "--chunk-bytes",
        "1048576",
        "--reserve-floor",
        "2147483648",
    ]);
    let request = parse_isolated_gpu_worker_args(&args).unwrap();
    let (fd, config) = (request.fd, request.config);
    assert_eq!(fd, 42);
    assert_eq!(config.target_bytes, 2 * 1024 * 1024 * 1024);
    assert_eq!(config.chunk_bytes, 1024 * 1024);
    assert_eq!(config.reserve_floor_bytes, 2 * 1024 * 1024 * 1024);

    // `--reserve-floor 0` is a legal explicit value (DT-9 case 3): the
    // shared helper still applies `capacity.div_ceil(5)`. Presence is what
    // the parser requires, not a non-zero floor.
    let request =
        parse_isolated_gpu_worker_args(&daemon_argv(&["--fd", "7", "--reserve-floor", "0"]))
            .unwrap();
    let (zero_fd, zero) = (request.fd, request.config);
    assert_eq!(zero_fd, 7);
    assert_eq!(zero.reserve_floor_bytes, 0);
    assert_eq!(zero.target_bytes, GpuWorkerConfig::default().target_bytes);
    assert_eq!(zero.chunk_bytes, GpuWorkerConfig::default().chunk_bytes);
}

/// RF-1 / RF-5: a missing `--reserve-floor` must fail closed instead of
/// falling back to `GpuWorkerConfig::default()` and undercutting the
/// sealed floor (invariant check— the refusal is the evidence).
#[test]
fn isolated_gpu_worker_arguments_require_reserve_floor() {
    let missing = parse_isolated_gpu_worker_args(&daemon_argv(&["--fd", "7"]));
    assert_eq!(
        missing.unwrap_err(),
        "missing --reserve-floor for isolated gpu worker"
    );
    let missing_after_sizing = parse_isolated_gpu_worker_args(&daemon_argv(&[
        "--fd",
        "7",
        "--target-bytes",
        "2147483648",
        "--chunk-bytes",
        "1048576",
    ]));
    assert_eq!(
        missing_after_sizing.unwrap_err(),
        "missing --reserve-floor for isolated gpu worker"
    );
}

#[test]
fn isolated_gpu_worker_arguments_refuse_ambiguous_or_invalid_values() {
    for args in [
        daemon_argv(&[]),
        daemon_argv(&["--fd"]),
        daemon_argv(&["--fd", "not-a-descriptor"]),
        daemon_argv(&["--fd", "-1"]),
        daemon_argv(&["--fd", "7", "--fd", "8"]),
        daemon_argv(&["--fd", "7", "--target-bytes", "0"]),
        daemon_argv(&["--fd", "7", "--target-bytes", "18446744073709551616"]),
        daemon_argv(&["--fd", "7", "--chunk-bytes", "0"]),
        daemon_argv(&["--fd", "7", "--chunk-bytes", "18446744073709551616"]),
        daemon_argv(&["--fd", "7", "--reserve-floor", "bad"]),
        daemon_argv(&["--fd", "7", "--reserve-floor"]),
        daemon_argv(&["--fd", "7", "--reserve-floor", "1", "--reserve-floor", "2"]),
        daemon_argv(&["--fd", "7", "--unknown", "1"]),
    ] {
        assert!(
            parse_isolated_gpu_worker_args(&args).is_err(),
            "worker args should fail closed: {args:?}"
        );
    }
}

fn pinned_adapter() -> GpuAdapterIdentity {
    GpuAdapterIdentity {
        node_mask: Some(1),
        backend: "cuda".into(),
        key: "a".repeat(32),
        luid: Some("12345678:00000001".into()),
    }
}

#[test]
fn worker_request_roundtrip_preserves_identity_and_limits() {
    let params = WorkerSpawnParams {
        generation: None,
        target_bytes: 96 << 30,
        chunk_bytes: 64 << 20,
        reserve_floor: 20 << 30,
        expected_adapter: Some(pinned_adapter()),
        host_catalog: None,
    };
    let args = params.arguments(42).unwrap();
    let request = parse_isolated_gpu_worker_args(&args).unwrap();
    assert_eq!(request.fd, 42);
    assert_eq!(request.config.target_bytes, params.target_bytes);
    assert_eq!(request.config.chunk_bytes as u64, params.chunk_bytes);
    assert_eq!(request.config.reserve_floor_bytes, params.reserve_floor);
    assert!(
        request.config.compression_enabled,
        "tier workers request device compression by default"
    );
    assert_eq!(request.expected_adapter, params.expected_adapter);
    let automatic = WorkerSpawnParams {
        generation: None,
        expected_adapter: None,
        ..params.clone()
    };
    assert!(
        parse_isolated_gpu_worker_args(&automatic.arguments(7).unwrap())
            .unwrap()
            .expected_adapter
            .is_none()
    );
    let mut invalid = params;
    invalid.expected_adapter.as_mut().unwrap().key = "0".into();
    assert!(invalid.arguments(7).is_err());
}

#[test]
fn worker_request_rejects_unknown_duplicate_and_malformed_identity() {
    for text in [
        "{",
        "null",
        "{}",
        "{\"backend\":\"cuda\",\"key\":\"0\",\"luid\":null}",
        "{\"backend\":\"cuda\",\"key\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",\"luid\":null,\"foreign\":true}",
        &"x".repeat(MAX_ADAPTER_IDENTITY_BYTES + 1),
    ] {
        let args = daemon_argv(&[
            "--fd",
            "7",
            "--reserve-floor",
            "0",
            "--adapter-identity",
            text,
        ]);
        assert!(parse_isolated_gpu_worker_args(&args).is_err(), "{text}");
    }
    let pin = serde_json::to_string(&pinned_adapter()).unwrap();
    assert!(
        parse_isolated_gpu_worker_args(&daemon_argv(&[
            "--fd",
            "7",
            "--reserve-floor",
            "0",
            "--adapter-identity",
            &pin,
            "--adapter-identity",
            &pin
        ]))
        .is_err()
    );
    assert!(parse_isolated_gpu_worker_args(&vec!["--fd".into(); 11]).is_err());
}

#[test]
fn inherited_worker_socket_requires_an_existing_connected_unix_stream() {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    assert!(duplicate_inherited_socket(-1).is_err());
    let file = std::fs::File::open("/dev/null").unwrap();
    assert!(duplicate_inherited_socket(file.as_raw_fd()).is_err());
    assert!(
        file.metadata().is_ok(),
        "refusal retains caller's descriptor"
    );
    let datagram = std::os::unix::net::UnixDatagram::unbound().unwrap();
    assert!(duplicate_inherited_socket(datagram.as_raw_fd()).is_err());
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (_peer, _) = listener.accept().unwrap();
    assert!(duplicate_inherited_socket(client.as_raw_fd()).is_err());
    let (original, mut peer) = UnixStream::pair().unwrap();
    let mut taken = duplicate_inherited_socket(original.as_raw_fd()).unwrap();
    assert!(
        rustix::io::fcntl_getfd(&original)
            .unwrap()
            .contains(rustix::io::FdFlags::CLOEXEC)
    );
    assert!(
        original.peer_addr().is_ok(),
        "the borrowed input remains valid"
    );
    peer.write_all(&[73; 16]).unwrap();
    let mut read = [0; 16];
    taken.read_exact(&mut read).unwrap();
    assert_eq!(read, [73; 16]);
}

fn startup_client(
    sample: Option<ramshared_vram::GpuBudgetTelemetry>,
    target: u64,
    expected: Option<GpuAdapterIdentity>,
) -> (ramshared_block::IpcCacheClient, std::thread::JoinHandle<()>) {
    use ramshared_block::gpu_cache_worker::*;
    use std::io::{Read, Write};
    let (socket, mut peer) = UnixStream::pair().unwrap();
    let worker = std::thread::spawn(move || {
        let mut raw = [0; FRAME_HEADER_LEN];
        peer.read_exact(&mut raw).unwrap();
        let request = FrameHeader::decode(&raw).unwrap();
        assert_eq!(request.msg_type, MSG_HANDSHAKE_REQ);
        let reply = FrameHeader {
            msg_type: MSG_HANDSHAKE_RESP,
            status: STATUS_OK,
            correlation_id: request.correlation_id,
            offset: target,
            payload_len: 0,
            aux: 0,
        };
        peer.write_all(&reply.encode()).unwrap();
        if target == 0 {
            return;
        }
        peer.read_exact(&mut raw).unwrap();
        let request = FrameHeader::decode(&raw).unwrap();
        assert_eq!(request.msg_type, MSG_HEARTBEAT_REQ);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let payload = ramshared_vram::WorkerTelemetryEnvelope::new(now, sample, None, None)
            .to_bounded_payload()
            .unwrap();
        let reply = FrameHeader {
            msg_type: MSG_HEARTBEAT_RESP,
            status: STATUS_OK,
            correlation_id: request.correlation_id,
            offset: target,
            payload_len: payload.len() as u32,
            aux: 0,
        };
        peer.write_all(&reply.encode()).unwrap();
        peer.write_all(&payload).unwrap();
    });
    let mut client =
        ramshared_block::IpcCacheClient::new(socket, std::time::Duration::from_millis(200), target);
    if let Some(expected) = expected {
        client.expect_adapter(expected).unwrap();
    }
    client.perform_handshake().unwrap();
    (client, worker)
}

#[test]
fn worker_startup_requires_fresh_driver_identity_before_exposure() {
    use ramshared_block::BestEffortCache;
    use ramshared_vram::{GpuBudgetSnapshot, GpuBudgetSource, GpuBudgetTelemetry};
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let sample = GpuBudgetTelemetry::from_snapshot(
        &GpuBudgetSnapshot {
            adapter: Some(pinned_adapter()),
            total_bytes: Some(96 << 30),
            budget_bytes: 96 << 30,
            used_bytes: 20 << 30,
            source: GpuBudgetSource::DriverReported,
            sampled_at: std::time::Instant::now(),
        },
        now,
    );
    let (mut client, worker) = startup_client(Some(sample.clone()), 1 << 20, None);
    complete_startup_binding(&mut client).unwrap();
    assert_eq!(client.bound_adapter_identity(), Some(&pinned_adapter()));
    worker.join().unwrap();
    let mut stale = sample.clone();
    stale.sampled_at_unix_ms = 1;
    let mut estimate = sample.clone();
    estimate.source = GpuBudgetSource::ProviderLocalEstimate;
    let mut unstable = sample.clone();
    unstable.adapter.as_mut().unwrap().key = "0".into();
    for sample in [None, Some(stale), Some(estimate), Some(unstable)] {
        let (mut client, worker) = startup_client(sample, 1 << 20, None);
        assert!(complete_startup_binding(&mut client).is_err());
        worker.join().unwrap();
    }
    let mut other = pinned_adapter();
    other.key = "b".repeat(32);
    let (mut client, worker) = startup_client(Some(sample), 1 << 20, Some(other.clone()));
    assert!(complete_startup_binding(&mut client).is_err());
    assert_eq!(client.state(), ramshared_block::CacheState::Unavailable);
    assert_eq!(client.bound_adapter_identity(), Some(&other));
    worker.join().unwrap();
    let (mut client, worker) = startup_client(None, 0, None);
    complete_startup_binding(&mut client).unwrap();
    assert!(client.bound_adapter_identity().is_none());
    worker.join().unwrap();
}

#[test]
fn worker_parent_binding_refuses_orphans_reparenting_and_failed_signal_setup() {
    use std::cell::Cell;
    let creator = rustix::process::getpid();
    let calls = Cell::new(0);
    assert!(
        verify_parent_binding(
            creator,
            || None,
            || {
                calls.set(1);
                Ok(())
            }
        )
        .is_err()
    );
    assert_eq!(calls.get(), 0);
    assert!(
        verify_parent_binding(
            creator,
            || Some(creator),
            || Err(io::Error::from(io::ErrorKind::PermissionDenied))
        )
        .is_err()
    );
    assert!(
        verify_parent_binding(
            creator,
            || if calls.replace(calls.get() + 1) == 0 {
                Some(creator)
            } else {
                None
            },
            || Ok(())
        )
        .is_err()
    );
    assert_eq!(calls.get(), 2);
    verify_parent_binding(creator, || Some(creator), || Ok(())).unwrap();
    // This socket was created by this test process, not its real parent. The
    // production worker must refuse it without altering process signal state.
    let (socket, _peer) = UnixStream::pair().unwrap();
    assert!(bind_parent_lifetime(&socket).is_err());
}

#[test]
fn worker_parent_death_signal_uses_the_real_kernel_and_restores_test_state() {
    struct Restore(i32);
    impl Drop for Restore {
        fn drop(&mut self) {
            // SAFETY: restore this test process's prior scalar signal setting.
            assert_eq!(
                unsafe {
                    libc::prctl(
                        libc::PR_SET_PDEATHSIG,
                        self.0 as libc::c_ulong,
                        0 as libc::c_ulong,
                        0 as libc::c_ulong,
                        0 as libc::c_ulong,
                    )
                },
                0
            );
        }
    }
    fn current() -> i32 {
        let mut signal = 0i32;
        // SAFETY: GET_PDEATHSIG writes one int into this live aligned value.
        assert_eq!(
            unsafe {
                libc::prctl(
                    libc::PR_GET_PDEATHSIG,
                    &mut signal as *mut i32,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                )
            },
            0
        );
        signal
    }
    let _restore = Restore(current());
    arm_parent_death().unwrap();
    assert_eq!(current(), libc::SIGTERM);
}

#[test]
fn pinned_worker_filters_other_backends_and_cuda_devices_before_context_creation() {
    let mut request = WorkerRequest {
        generation: None,
        fd: 7,
        config: GpuWorkerConfig::default(),
        expected_adapter: Some(pinned_adapter()),
        catalog_fd: None,
    };
    assert!(request.allows_backend("cuda"));
    assert!(!request.allows_backend("vulkan"));
    assert!(request.accepts_adapter(Some(&pinned_adapter())));
    assert!(!request.accepts_adapter(None));
    let mut foreign = pinned_adapter();
    foreign.key = "b".repeat(32);
    assert!(!request.accepts_adapter(Some(&foreign)));
    let mut changed = pinned_adapter();
    changed.luid = None;
    assert!(!request.accepts_adapter(Some(&changed)));
    request.expected_adapter = None;
    assert!(request.allows_backend("cuda"));
    assert!(request.allows_backend("vulkan"));
    assert!(request.accepts_adapter(None));
}

#[test]
fn worker_launch_socket_uses_child_stdin_without_parent_descriptor_leak() {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    let (mut client, socket) = std::os::unix::net::UnixStream::pair().unwrap();
    let parent_descriptor = socket.as_raw_fd();
    let mut command = std::process::Command::new("/bin/sh");
    command
        .args(["-c", "read value; printf '%s' \"$value\""])
        .stdout(std::process::Stdio::piped());
    let child = spawn_socket_child(&mut command, socket).unwrap();
    // Command still owns the parent's socket while a separate exec occurs.
    // Clearing CLOEXEC globally would let this sibling observe that socket.
    let result = std::process::Command::new("/bin/sh")
        .args([
            "-c",
            &format!("test ! -e /proc/self/fd/{parent_descriptor}"),
        ])
        .status()
        .unwrap();
    client.write_all(b"socket-stdin\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        result.success(),
        "worker socket leaked into an unrelated exec"
    );
    assert!(output.status.success());
    assert_eq!(output.stdout, b"socket-stdin");
}

#[test]
fn queued_worker_binding_must_still_be_fresh_at_handoff() {
    use ramshared_vram::{GpuBudgetSnapshot, GpuBudgetSource, GpuBudgetTelemetry};
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let sample = GpuBudgetTelemetry::from_snapshot(
        &GpuBudgetSnapshot {
            adapter: Some(pinned_adapter()),
            total_bytes: Some(96 << 30),
            budget_bytes: 96 << 30,
            used_bytes: 0,
            source: GpuBudgetSource::DriverReported,
            sampled_at: std::time::Instant::now(),
        },
        now,
    );
    let (mut client, worker) = startup_client(Some(sample), 1 << 20, None);
    complete_startup_binding(&mut client).unwrap();
    worker.join().unwrap();
    assert!(validate_startup_binding_at(&client, now).is_ok());
    assert!(
        validate_startup_binding_at(&client, now + ramshared_vram::GPU_BUDGET_MAX_AGE_MS + 1)
            .is_err()
    );
    assert!(validate_startup_binding_at(&client, 0).is_err());
}

#[test]
fn failed_socket_child_reports_eof_after_command_alias_is_dropped() {
    use std::io::Read;
    let (mut client, socket) = std::os::unix::net::UnixStream::pair().unwrap();
    let mut command = std::process::Command::new("/bin/true");
    let mut child = spawn_socket_child(&mut command, socket).unwrap();
    drop(command);
    client
        .set_read_timeout(Some(std::time::Duration::from_millis(250)))
        .unwrap();
    assert_eq!(client.read(&mut [0]).unwrap(), 0);
    assert!(child.wait().unwrap().success());
}

fn sample_host_catalog_entry(
    producer_pid: u32,
) -> ramshared_ipc::gpu_catalog::AuthenticatedCatalog {
    use ramshared_ipc::gpu_catalog::{CatalogSession, CatalogTiming};
    use ramshared_ipc::{HandshakeTranscript, HeartbeatAck, HostChallenge, IPC_VERSION_4};
    use ramshared_vram::host_identity::{
        HostGpuIdentity, HostGpuIdentityReport, HostGpuObservation, ProducerProcess,
    };
    let report = HostGpuIdentityReport {
        schema_version: 1,
        producer: ProducerProcess {
            pid: producer_pid,
            creation_time_100ns: 77,
        },
        started_at_unix_ms: 1_700_000_000_000,
        elapsed_ms: 5,
        adapters: vec![HostGpuIdentity {
            ordinal: 0,
            allocator: GpuAdapterIdentity {
                node_mask: Some(1),
                backend: "cuda".into(),
                key: format!("{:032x}", 1u8),
                luid: Some("11111111:22222222".into()),
            },
            interface_id: r"\\?\PCI#VEN_10DE&DEV_1F08#iface".into(),
        }],
    };
    let secret = b"worker-request-test-secret-32-by";
    let transcript = HandshakeTranscript {
        min_version: IPC_VERSION_4,
        max_version: IPC_VERSION_4,
        boot_id: "boot-req".into(),
        distro_id: "ubuntu".into(),
        guest_nonce: [0x11; 32],
    };
    let challenge = HostChallenge {
        accepted_version: IPC_VERSION_4,
        heartbeat_secs: 2,
        lease_timeout_secs: 6,
        host_nonce: [0x22; 32],
    };
    let mut host = CatalogSession::new(secret, &transcript, &challenge, 7).unwrap();
    let mut guest = CatalogSession::new(secret, &transcript, &challenge, 7).unwrap();
    let ack = HeartbeatAck {
        timestamp_ms: 6000,
        lease_remaining_ms: 6000,
        host_memory: None,
    };
    let encoded = host
        .seal(
            secret,
            &ack,
            Some(HostGpuObservation { report, age_ms: 10 }),
        )
        .unwrap();
    let now = std::time::Instant::now();
    guest
        .open(
            secret,
            &ack,
            &encoded,
            4,
            CatalogTiming {
                started: now,
                received: now,
            },
        )
        .unwrap()
        .unwrap()
}

#[test]
fn isolated_gpu_worker_arguments_validate_catalog_fd_and_bounds() {
    let request = parse_isolated_gpu_worker_args(&daemon_argv(&[
        "--fd",
        "7",
        "--reserve-floor",
        "0",
        "--catalog-fd",
        "9",
    ]))
    .unwrap();
    assert_eq!(request.fd, 7);
    assert_eq!(request.catalog_fd, Some(9));

    let adapter_json = serde_json::to_string(&pinned_adapter()).unwrap();
    for bad in [
        daemon_argv(&["--fd", "7", "--reserve-floor", "0", "--catalog-fd"]),
        daemon_argv(&["--fd", "7", "--reserve-floor", "0", "--catalog-fd", "bad"]),
        daemon_argv(&["--fd", "7", "--reserve-floor", "0", "--catalog-fd", "2"]),
        daemon_argv(&[
            "--fd",
            "7",
            "--reserve-floor",
            "0",
            "--catalog-fd",
            "9",
            "--catalog-fd",
            "10",
        ]),
        daemon_argv(&["--fd", "7", "--reserve-floor", "0", "--catalog-fd", "7"]),
        daemon_argv(&[
            "--fd",
            "7",
            "--reserve-floor",
            "0",
            "--adapter-identity",
            &adapter_json,
            "--configuration-fd",
            "9",
            "--configuration-generation",
            "1",
            "--catalog-fd",
            "9",
        ]),
        daemon_argv(&[
            "--fd",
            "7",
            "--target-bytes",
            "1",
            "--chunk-bytes",
            "1",
            "--reserve-floor",
            "0",
            "--adapter-identity",
            &adapter_json,
            "--configuration-fd",
            "8",
            "--configuration-generation",
            "1",
            "--catalog-fd",
            "9",
            "--extra",
            "x",
        ]),
    ] {
        assert!(
            parse_isolated_gpu_worker_args(&bad).is_err(),
            "catalog args should fail closed: {bad:?}"
        );
    }
}

#[test]
fn inherit_catalog_stream_delivers_fd_without_leaking_to_unrelated_exec() {
    use ramshared_wsl2d::host_catalog::SharedHostCatalog;
    use std::os::fd::AsRawFd;

    let catalog = SharedHostCatalog::new();
    let entry_inherit = sample_host_catalog_entry(101);
    catalog.publish(Some(&entry_inherit));
    let mut command = std::process::Command::new("/bin/sh");
    command.args([
        "-c",
        "test \"$1\" = '--catalog-fd' && test -S \"/proc/self/fd/$2\"",
        "probe",
    ]);
    let kept = inherit_catalog_stream(&mut command, &catalog).unwrap();
    let parent_fd = kept.as_raw_fd();
    let args: Vec<String> = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        &args[3..],
        &[
            ramshared_wsl2d::host_catalog::CATALOG_FD_ARG.to_string(),
            parent_fd.to_string()
        ]
    );
    let sibling = std::process::Command::new("/bin/sh")
        .args(["-c", &format!("test ! -e /proc/self/fd/{parent_fd}")])
        .status()
        .unwrap();
    assert!(
        sibling.success(),
        "inherited catalog fd leaked across unrelated exec"
    );
    let child_status = command.status().unwrap();
    drop(kept);
    assert!(
        child_status.success(),
        "child did not inherit readable catalog fd via inherit_catalog_stream"
    );
}

#[test]
fn claim_inherited_catalog_reads_parent_report_and_rejects_bad_fd() {
    use ramshared_wsl2d::host_catalog::SharedHostCatalog;
    use std::os::fd::AsRawFd;

    let catalog = SharedHostCatalog::new();
    let entry_claim = sample_host_catalog_entry(202);
    catalog.publish(Some(&entry_claim));
    let (_worker_parent, worker_child) = UnixStream::pair().unwrap();
    let catalog_reader = catalog.subscribe().unwrap();
    let claimed = claim_inherited_catalog(catalog_reader.as_raw_fd(), &worker_child).unwrap();
    assert_eq!(claimed.current_report().unwrap().producer.pid, 202);
    assert!(claim_inherited_catalog(-1, &worker_child).is_err());
    drop(catalog_reader);
}
