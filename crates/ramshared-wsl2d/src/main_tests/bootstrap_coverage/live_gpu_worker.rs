// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/main_tests/bootstrap_coverage/live_gpu_worker.rs
use super::*;

#[test]
fn isolated_gpu_worker_entry_with_covers_error_unavailable_and_probe_paths() {
    struct MockGpuHost {
        sock: Option<UnixStream>,
        fail_dup: bool,
        fail_bind: bool,
        fail_admit: bool,
        probe_hw: bool,
        forward_dxg: bool,
        forward_catalog: bool,
    }

    impl GpuWorkerEntryHost for MockGpuHost {
        fn duplicate_socket(&mut self, _: i32) -> std::io::Result<UnixStream> {
            if self.fail_dup {
                return Err(std::io::Error::other("dup failed"));
            }
            Ok(self.sock.take().expect("socket available once"))
        }

        fn bind_parent(&mut self, _: &UnixStream) -> std::io::Result<()> {
            if self.fail_bind {
                return Err(std::io::Error::other("bind failed"));
            }
            Ok(())
        }

        fn admit(&mut self, _: &gpu_worker_request::WorkerRequest) -> Result<(), String> {
            if self.fail_admit {
                return Err("admit failed".into());
            }
            Ok(())
        }

        fn claim_catalog(
            &mut self,
            catalog_fd: Option<i32>,
            worker_socket: &UnixStream,
        ) -> std::io::Result<Option<Arc<dyn ramshared_wsl2d::host_catalog::HostGpuCatalogProvider>>>
        {
            if self.forward_catalog {
                ProductionGpuWorkerEntryHost.claim_catalog(catalog_fd, worker_socket)
            } else {
                Ok(None)
            }
        }

        fn probe_hardware(&self) -> bool {
            self.probe_hw
        }

        fn open_dxg(&self, req: BudgetRequest) -> Result<DxgBudgetProvider, DxgError> {
            if self.forward_dxg {
                ProductionGpuWorkerEntryHost.open_dxg(req)
            } else {
                Err(DxgError::Malformed("synthetic test dxg refusal"))
            }
        }

        fn open_dxg_target(
            &self,
            target: ramshared_wsl2d::gpu_budget::DxgOpenTarget<'_>,
        ) -> Result<DxgBudgetProvider, DxgError> {
            if self.forward_dxg {
                ProductionGpuWorkerEntryHost.open_dxg_target(target)
            } else {
                Err(DxgError::Malformed("synthetic test dxg target refusal"))
            }
        }
    }

    let args: Vec<String> = ["--fd", "3", "--reserve-floor", "0"]
        .into_iter()
        .map(str::to_string)
        .collect();

    // 1. Parse / dup / bind / admit failures.
    assert!(run_isolated_gpu_worker_entry(&[]).is_err());
    for (fail_dup, fail_bind, fail_admit) in [
        (true, false, false),
        (false, true, false),
        (false, false, true),
    ] {
        let (worker_sock, peer_sock) = UnixStream::pair().expect("pair");
        drop(peer_sock);
        let mut host = MockGpuHost {
            sock: Some(worker_sock),
            fail_dup,
            fail_bind,
            fail_admit,
            probe_hw: false,
            forward_dxg: false,
            forward_catalog: false,
        };
        assert!(run_isolated_gpu_worker_entry_with(&args, &mut host).is_err());
    }

    // 2. probe_hw = false drops straight into UnavailableVramProvider loop and exits on EOF.
    let (worker_sock, peer_sock) = UnixStream::pair().expect("pair");
    drop(peer_sock);
    let mut no_hw = MockGpuHost {
        sock: Some(worker_sock),
        fail_dup: false,
        fail_bind: false,
        fail_admit: false,
        probe_hw: false,
        forward_dxg: false,
        forward_catalog: false,
    };
    run_isolated_gpu_worker_entry_with(&args, &mut no_hw)
        .expect("unavailable worker exits cleanly on peer EOF");

    // 3. probe_hw = true exercises real CUDA/Vulkan loops while fail-closed DxgError keeps it safe.
    let (worker_sock, peer_sock) = UnixStream::pair().expect("pair");
    drop(peer_sock);
    let mut with_hw = MockGpuHost {
        sock: Some(worker_sock),
        fail_dup: false,
        fail_bind: false,
        fail_admit: false,
        probe_hw: true,
        forward_dxg: false,
        forward_catalog: false,
    };
    run_isolated_gpu_worker_entry_with(&args, &mut with_hw)
        .expect("fail-closed WDDM probe falls back to UnavailableVramProvider cleanly");

    // 3b. Exercise live WDDM/GPU selection revalidation with immediate peer EOF.
    let (worker_sock, peer_sock) = UnixStream::pair().expect("pair");
    drop(peer_sock);
    let mut with_real_dxg = MockGpuHost {
        sock: Some(worker_sock),
        fail_dup: false,
        fail_bind: false,
        fail_admit: false,
        probe_hw: true,
        forward_dxg: true,
        forward_catalog: false,
    };
    let _ = run_isolated_gpu_worker_entry_with(&args, &mut with_real_dxg);

    // 4. ProductionGpuWorkerEntryHost direct adapter checks (no device mutation).
    let mut prod_host = ProductionGpuWorkerEntryHost;
    assert!(prod_host.probe_hardware());
    assert!(prod_host.duplicate_socket(-1).is_err());
    let (s1, _s2) = UnixStream::pair().expect("pair");
    let _ = prod_host.bind_parent(&s1);
    let mut bad_req = parse_isolated_gpu_worker_args(&args).expect("parse req");
    bad_req.generation = Some((999_999, 1));
    assert!(prod_host.admit(&bad_req).is_err());
    let _ = prod_host.open_dxg(BudgetRequest {
        adapter: None,
        physical_adapter_index: None,
    });
    assert!(prod_host.claim_catalog(None, &s1).unwrap().is_none());
    let cat_stream = ramshared_wsl2d::host_catalog::SharedHostCatalog::new()
        .subscribe()
        .expect("subscribe");
    let claimed = prod_host
        .claim_catalog(Some(std::os::fd::AsRawFd::as_raw_fd(&cat_stream)), &s1)
        .expect("claim catalog");
    assert!(claimed.is_some());
    assert!(
        prod_host
            .open_dxg_target(ramshared_wsl2d::gpu_budget::DxgOpenTarget::Interface {
                interface_id: r"\\?\PCI#NONEXISTENT#0000",
                physical_adapter_index: 0,
            })
            .is_err()
    );
}

#[test]
#[ignore = "requires an explicitly scheduled live WSL2 GPU qualification"]
fn isolated_gpu_worker_entry_live_wsl_host_catalog_e2e() {
    use ramshared_block::{CacheMutation, CacheRead, CacheState, IpcCacheClient};
    use ramshared_cuda::Cuda;
    use ramshared_ipc::gpu_catalog::{CatalogSession, CatalogTiming};
    use ramshared_ipc::{HandshakeTranscript, HeartbeatAck, HostChallenge, IPC_VERSION_4};
    use ramshared_vram::host_identity::{
        HostGpuIdentity, HostGpuIdentityReport, HostGpuObservation, ProducerProcess,
    };
    use ramshared_wsl2d::gpu_budget::DxgOpenTarget;
    use ramshared_wsl2d::host_catalog::SharedHostCatalog;
    use std::os::fd::{AsFd, AsRawFd};

    struct LiveMockGpuHost {
        sock: Option<UnixStream>,
    }
    impl GpuWorkerEntryHost for LiveMockGpuHost {
        fn duplicate_socket(&mut self, _: i32) -> std::io::Result<UnixStream> {
            Ok(self.sock.take().expect("socket available once"))
        }
        fn bind_parent(&mut self, _: &UnixStream) -> std::io::Result<()> {
            Ok(())
        }
        fn admit(&mut self, request: &gpu_worker_request::WorkerRequest) -> Result<(), String> {
            ProductionGpuWorkerEntryHost.admit(request)
        }
        fn claim_catalog(
            &mut self,
            catalog_fd: Option<i32>,
            worker_socket: &UnixStream,
        ) -> std::io::Result<Option<Arc<dyn ramshared_wsl2d::host_catalog::HostGpuCatalogProvider>>>
        {
            ProductionGpuWorkerEntryHost.claim_catalog(catalog_fd, worker_socket)
        }
        fn open_dxg(&self, req: BudgetRequest) -> Result<DxgBudgetProvider, DxgError> {
            ProductionGpuWorkerEntryHost.open_dxg(req)
        }
        fn open_dxg_target(
            &self,
            target: DxgOpenTarget<'_>,
        ) -> Result<DxgBudgetProvider, DxgError> {
            ProductionGpuWorkerEntryHost.open_dxg_target(target)
        }
    }

    let Ok(cuda) = Cuda::load() else {
        eprintln!("LIVE_WSL2_CATALOG_WORKER_E2E=SKIP reason=cuda_unavailable");
        return;
    };
    let Ok(device) = cuda.device(0) else {
        eprintln!("LIVE_WSL2_CATALOG_WORKER_E2E=SKIP reason=no_cuda_device");
        return;
    };
    let Some(mut cuda_id) = device.adapter_identity() else {
        eprintln!("LIVE_WSL2_CATALOG_WORKER_E2E=SKIP reason=missing_cuda_identity");
        return;
    };
    cuda_id.backend = cuda_id.backend.to_ascii_lowercase();
    cuda_id.key = cuda_id.key.to_ascii_lowercase();
    cuda_id.luid = cuda_id.luid.as_deref().map(str::to_ascii_lowercase);
    let Some(phys_idx) = cuda_id.wddm_physical_adapter_index() else {
        eprintln!("LIVE_WSL2_CATALOG_WORKER_E2E=SKIP reason=non_single_node_cuda");
        return;
    };
    let Ok(dxg) = DxgBudgetProvider::open(None) else {
        eprintln!("LIVE_WSL2_CATALOG_WORKER_E2E=SKIP reason=dxg_unavailable");
        return;
    };
    let Ok(Some(interface_id)) = dxg.adapter_interface() else {
        eprintln!("LIVE_WSL2_CATALOG_WORKER_E2E=SKIP reason=vgpu_interface_unavailable");
        return;
    };
    let Ok(dxg_snap) = ramshared_dxg::GpuBudgetProvider::snapshot(&dxg) else {
        eprintln!("LIVE_WSL2_CATALOG_WORKER_E2E=SKIP reason=dxg_snapshot_failed");
        return;
    };
    if dxg_snap.adapter_namespace != ramshared_dxg::AdapterLuidNamespace::WslGuest {
        eprintln!("LIVE_WSL2_CATALOG_WORKER_E2E=SKIP reason=not_wsl_guest_dxg");
        return;
    }

    let report = HostGpuIdentityReport {
        schema_version: 1,
        producer: ProducerProcess {
            pid: std::process::id().max(1),
            creation_time_100ns: 1_234_567,
        },
        started_at_unix_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_millis() as u64,
        elapsed_ms: 1,
        adapters: vec![HostGpuIdentity {
            ordinal: 0,
            allocator: cuda_id.clone(),
            interface_id,
        }],
    };
    if report.validate().is_err() {
        eprintln!("LIVE_WSL2_CATALOG_WORKER_E2E=SKIP reason=identity_not_single_node_nvidia");
        return;
    }

    let secret = b"live-wsl2-e2e-test-secret-32-by!";
    let transcript = HandshakeTranscript {
        min_version: IPC_VERSION_4,
        max_version: IPC_VERSION_4,
        boot_id: "boot-live-wsl2".into(),
        distro_id: "ubuntu".into(),
        guest_nonce: [0x11; 32],
    };
    let challenge = HostChallenge {
        accepted_version: IPC_VERSION_4,
        heartbeat_secs: 2,
        lease_timeout_secs: 6,
        host_nonce: [0x22; 32],
    };
    let mut hs = CatalogSession::new(secret, &transcript, &challenge, 7).expect("hs");
    let mut gs = CatalogSession::new(secret, &transcript, &challenge, 7).expect("gs");
    let ack = HeartbeatAck {
        timestamp_ms: 6000,
        lease_remaining_ms: 10_000,
        host_memory: None,
    };
    let encoded = hs
        .seal(
            secret,
            &ack,
            Some(HostGpuObservation { report, age_ms: 10 }),
        )
        .expect("seal");
    let now = Instant::now();
    let entry = gs
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
        .expect("open")
        .expect("entry");

    let catalog = SharedHostCatalog::new();
    catalog.publish(Some(&entry));
    let reader = catalog.subscribe().expect("subscribe");
    let inherited_cat = rustix::io::fcntl_dupfd_cloexec(reader.as_fd(), 3).expect("dup cat");
    let cat_raw = inherited_cat.as_raw_fd();
    let expected_json = serde_json::to_string(&cuda_id).expect("json");

    let (client_sock, worker_sock) = UnixStream::pair().expect("pair");
    let args: Vec<String> = [
        "--fd".to_string(),
        "3".to_string(),
        "--target-bytes".to_string(),
        (4 * 1024 * 1024).to_string(),
        "--chunk-bytes".to_string(),
        4096.to_string(),
        "--reserve-floor".to_string(),
        (2u64 * 1024 * 1024 * 1024).to_string(),
        "--adapter-identity".to_string(),
        expected_json,
        ramshared_wsl2d::host_catalog::CATALOG_FD_ARG.to_string(),
        cat_raw.to_string(),
    ]
    .to_vec();

    let worker = std::thread::spawn(move || {
        let _keep_cat = inherited_cat;
        let _keep_reader = reader;
        let mut live_host = LiveMockGpuHost {
            sock: Some(worker_sock),
        };
        run_isolated_gpu_worker_entry_with(&args, &mut live_host).map_err(|e| e.to_string())
    });

    let mut client = IpcCacheClient::with_pacing(
        client_sock,
        Duration::from_secs(10),
        4 * 1024 * 1024,
        ramshared_block::ipc_cache_client::PacingPolicy::immediate(),
    );
    client
        .expect_adapter(cuda_id.clone())
        .expect("expect adapter");
    client.perform_handshake().expect("handshake");
    assert_eq!(client.state(), CacheState::Active);
    assert_eq!(client.target_bytes(), 4 * 1024 * 1024);
    assert_eq!(client.bound_adapter_identity(), Some(&cuda_id));
    client.refresh_cached_bytes().expect("initial heartbeat");
    assert!(client.gpu_budget_telemetry().is_some());

    let pattern = vec![0xa5u8; 4096];
    assert_eq!(client.promote(0, &pattern), CacheMutation::Accepted);
    let mut readback = vec![0u8; 4096];
    assert_eq!(client.read(0, &mut readback), CacheRead::Hit);
    assert_eq!(readback, pattern);

    // Revoking the shared host catalog must cause WddmBudgetGuard to fail closed on the next heartbeat.
    catalog.revoke();
    assert!(
        client.refresh_cached_bytes().is_err(),
        "heartbeat must fail closed after host catalog revocation"
    );
    drop(client);
    worker
        .join()
        .expect("worker join")
        .expect("worker clean EOF");
    let _ = phys_idx;
    eprintln!("LIVE_WSL2_CATALOG_WORKER_E2E=PASS");
}

#[test]
#[ignore = "requires an explicitly scheduled live CUDA hardware qualification"]
fn isolated_gpu_worker_entry_live_rtx2060_slab_compression_ipc_e2e() {
    use ramshared_block::{CacheState, IpcCacheClient};
    use ramshared_cuda::Cuda;
    use ramshared_dxg::GpuBudgetProvider;
    use ramshared_ipc::gpu_catalog::{CatalogSession, CatalogTiming};
    use ramshared_ipc::{HandshakeTranscript, HeartbeatAck, HostChallenge, IPC_VERSION_4};
    use ramshared_vram::host_identity::{
        HostGpuIdentity, HostGpuIdentityReport, HostGpuObservation, ProducerProcess,
    };
    use ramshared_vram::{PAGE_BYTES, PageDecodeStatus, SlotEncoding};
    use ramshared_wsl2d::gpu_budget::DxgOpenTarget;
    use ramshared_wsl2d::host_catalog::SharedHostCatalog;
    use std::os::fd::{AsFd, AsRawFd};

    struct LiveMockGpuHost {
        sock: Option<UnixStream>,
    }
    impl GpuWorkerEntryHost for LiveMockGpuHost {
        fn duplicate_socket(&mut self, _: i32) -> std::io::Result<UnixStream> {
            Ok(self.sock.take().expect("socket available once"))
        }
        fn bind_parent(&mut self, _: &UnixStream) -> std::io::Result<()> {
            Ok(())
        }
        fn admit(&mut self, request: &gpu_worker_request::WorkerRequest) -> Result<(), String> {
            ProductionGpuWorkerEntryHost.admit(request)
        }
        fn claim_catalog(
            &mut self,
            catalog_fd: Option<i32>,
            worker_socket: &UnixStream,
        ) -> std::io::Result<Option<Arc<dyn ramshared_wsl2d::host_catalog::HostGpuCatalogProvider>>>
        {
            ProductionGpuWorkerEntryHost.claim_catalog(catalog_fd, worker_socket)
        }
        fn open_dxg(&self, req: BudgetRequest) -> Result<DxgBudgetProvider, DxgError> {
            ProductionGpuWorkerEntryHost.open_dxg(req)
        }
        fn open_dxg_target(
            &self,
            target: DxgOpenTarget<'_>,
        ) -> Result<DxgBudgetProvider, DxgError> {
            ProductionGpuWorkerEntryHost.open_dxg_target(target)
        }
    }

    let Ok(cuda) = Cuda::load() else {
        eprintln!("LIVE_RTX2060_SLAB_IPC_E2E=SKIP reason=cuda_unavailable");
        return;
    };
    let Ok(device) = cuda.device(0) else {
        eprintln!("LIVE_RTX2060_SLAB_IPC_E2E=SKIP reason=no_cuda_device");
        return;
    };
    let Some(mut cuda_id) = device.adapter_identity() else {
        eprintln!("LIVE_RTX2060_SLAB_IPC_E2E=SKIP reason=missing_cuda_identity");
        return;
    };
    cuda_id.backend = cuda_id.backend.to_ascii_lowercase();
    cuda_id.key = cuda_id.key.to_ascii_lowercase();
    cuda_id.luid = cuda_id.luid.as_deref().map(str::to_ascii_lowercase);
    if cuda_id.wddm_physical_adapter_index().is_none() {
        eprintln!("LIVE_RTX2060_SLAB_IPC_E2E=SKIP reason=non_single_node_cuda");
        return;
    }
    let Ok(dxg) = DxgBudgetProvider::open(None) else {
        eprintln!("LIVE_RTX2060_SLAB_IPC_E2E=SKIP reason=dxg_unavailable");
        return;
    };
    let Ok(Some(interface_id)) = dxg.adapter_interface() else {
        eprintln!("LIVE_RTX2060_SLAB_IPC_E2E=SKIP reason=vgpu_interface_unavailable");
        return;
    };
    let Ok(dxg_snap) = dxg.snapshot() else {
        eprintln!("LIVE_RTX2060_SLAB_IPC_E2E=SKIP reason=dxg_snapshot_failed");
        return;
    };
    if dxg_snap.adapter_namespace != ramshared_dxg::AdapterLuidNamespace::WslGuest {
        eprintln!("LIVE_RTX2060_SLAB_IPC_E2E=SKIP reason=not_wsl_guest_dxg");
        return;
    }

    let report = HostGpuIdentityReport {
        schema_version: 1,
        producer: ProducerProcess {
            pid: std::process::id().max(1),
            creation_time_100ns: 1_234_568,
        },
        started_at_unix_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_millis() as u64,
        elapsed_ms: 1,
        adapters: vec![HostGpuIdentity {
            ordinal: 0,
            allocator: cuda_id.clone(),
            interface_id,
        }],
    };
    if report.validate().is_err() {
        eprintln!("LIVE_RTX2060_SLAB_IPC_E2E=SKIP reason=identity_not_single_node_nvidia");
        return;
    }

    let secret = b"live-rtx2060-slab-e2e-secret-32!";
    let transcript = HandshakeTranscript {
        min_version: IPC_VERSION_4,
        max_version: IPC_VERSION_4,
        boot_id: "boot-live-slab".into(),
        distro_id: "ubuntu".into(),
        guest_nonce: [0x33; 32],
    };
    let challenge = HostChallenge {
        accepted_version: IPC_VERSION_4,
        heartbeat_secs: 2,
        lease_timeout_secs: 6,
        host_nonce: [0x44; 32],
    };
    let mut hs = CatalogSession::new(secret, &transcript, &challenge, 7).expect("hs");
    let mut gs = CatalogSession::new(secret, &transcript, &challenge, 7).expect("gs");
    let ack = HeartbeatAck {
        timestamp_ms: 7000,
        lease_remaining_ms: 10_000,
        host_memory: None,
    };
    let encoded = hs
        .seal(
            secret,
            &ack,
            Some(HostGpuObservation { report, age_ms: 10 }),
        )
        .expect("seal");
    let now = Instant::now();
    let entry = gs
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
        .expect("open")
        .expect("entry");

    let catalog = SharedHostCatalog::new();
    catalog.publish(Some(&entry));
    let reader = catalog.subscribe().expect("subscribe");
    let inherited_cat = rustix::io::fcntl_dupfd_cloexec(reader.as_fd(), 3).expect("dup cat");
    let cat_raw = inherited_cat.as_raw_fd();
    let expected_json = serde_json::to_string(&cuda_id).expect("json");
    let reserve_floor: u64 = 1024 * 1024 * 1024;
    let target_bytes: u64 = 4 * 1024 * 1024 * 1024;

    let (client_sock, worker_sock) = UnixStream::pair().expect("pair");
    let args: Vec<String> = vec![
        "--fd".into(),
        "3".into(),
        "--target-bytes".into(),
        target_bytes.to_string(),
        "--chunk-bytes".into(),
        "4096".into(),
        "--reserve-floor".into(),
        reserve_floor.to_string(),
        "--compression".into(),
        "--adapter-identity".into(),
        expected_json,
        ramshared_wsl2d::host_catalog::CATALOG_FD_ARG.into(),
        cat_raw.to_string(),
    ];

    let worker = std::thread::spawn(move || {
        let _keep_cat = inherited_cat;
        let _keep_reader = reader;
        let mut live_host = LiveMockGpuHost {
            sock: Some(worker_sock),
        };
        run_isolated_gpu_worker_entry_with(&args, &mut live_host).map_err(|e| e.to_string())
    });

    let mut client = IpcCacheClient::with_pacing(
        client_sock,
        Duration::from_secs(10),
        target_bytes,
        ramshared_block::ipc_cache_client::PacingPolicy::immediate(),
    );
    client
        .expect_adapter(cuda_id.clone())
        .expect("expect adapter");
    client.perform_handshake().expect("handshake");
    assert_eq!(client.state(), CacheState::Active);

    let tele = client
        .slab_telemetry()
        .expect("slab telemetry")
        .expect("CUDA PTX slab bridge must be admitted when --compression is enabled");
    assert_eq!(tele.backend.as_deref(), Some("cuda-ptx-gpu"));

    let mut compressible = [0u8; PAGE_BYTES];
    for (i, b) in compressible.iter_mut().enumerate() {
        *b = ((i / 64) as u8).wrapping_mul(3);
    }
    let mut high_entropy = [0u8; PAGE_BYTES];
    let mut prng = 0xfeed_face_cafe_beef_u64;
    for b in &mut high_entropy {
        prng ^= prng << 13;
        prng ^= prng >> 7;
        prng ^= prng << 17;
        *b = (prng >> 24) as u8;
    }

    // Fill > 1 B512 slab frame (64 KiB physical) alongside 1 Raw4096 frame
    // so effective density cleanly exceeds 1.0x across the two active frames.
    let mut batch = vec![[0u8; PAGE_BYTES]; 31];
    batch.extend(std::iter::repeat_n(compressible, 32));
    batch.push(high_entropy);
    let tickets_opt = client
        .slab_compress_batch(&batch)
        .expect("GPU PTX compress batch over IPC");
    let t0 = tickets_opt[0].expect("zero page ticket");
    let t31 = tickets_opt[31].expect("compressible page ticket");
    let t63 = tickets_opt[63].expect("raw page ticket");
    assert_eq!(t0.encoding, SlotEncoding::Lz4);
    assert_eq!(t31.encoding, SlotEncoding::Lz4);
    assert_eq!(t63.encoding, SlotEncoding::Raw);

    let after = client
        .slab_telemetry()
        .expect("slab telemetry after compress")
        .expect("telemetry present");
    assert_eq!(after.logical_pages_cached, Some(64));
    assert!(
        after.effective_density_ratio.unwrap_or(0.0) >= 2.0,
        "expected >=2.0x density for 63 B512 + 1 Raw4096 across 2 frames: {:?}",
        after
    );

    let (statuses, recovered) = client
        .slab_recover_batch(&[t0, t31, t63])
        .expect("GPU PTX recover batch over IPC");
    assert_eq!(
        statuses,
        vec![
            PageDecodeStatus::Ok,
            PageDecodeStatus::Ok,
            PageDecodeStatus::Ok
        ]
    );
    assert_eq!(recovered[0], [0u8; PAGE_BYTES]);
    assert_eq!(recovered[1], compressible);
    assert_eq!(recovered[2], high_entropy);

    // Keep the same real GPU worker and exercise it through the authoritative
    // file-origin path, using a private ordinary file rather than a swap device.
    let origin_path = std::env::temp_dir().join(format!(
        "ramshared-codec-origin-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let origin_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&origin_path)
        .expect("private origin fixture");
    origin_file.set_len(8192).unwrap();
    let backend = ramshared_block::AuthoritativeOriginBackend::new_with_admission(
        ramshared_block::FileOrigin::from_file(origin_file),
        client,
        8192,
        PAGE_BYTES as u32,
        ramshared_block::AllowCacheAdmission,
    )
    .expect("file origin and GPU cache");
    backend.write_at(0, &compressible).expect("origin write");
    backend
        .write_at(PAGE_BYTES as u64, &high_entropy)
        .expect("origin raw write");
    backend.flush().expect("origin flush");
    let mut observed = [0; PAGE_BYTES];
    backend.read_at(0, &mut observed).expect("compressed read");
    assert_eq!(observed, compressible);
    backend
        .read_at(PAGE_BYTES as u64, &mut observed)
        .expect("raw read");
    assert_eq!(observed, high_entropy);
    backend.write_at(512, &[0x9a; 128]).expect("partial write");
    backend.flush().expect("partial write flush");
    let mut replaced = compressible;
    replaced[512..640].fill(0x9a);
    backend.read_at(0, &mut observed).expect("updated read");
    assert_eq!(observed, replaced);
    let stored = std::fs::read(&origin_path).expect("read private origin");
    assert_eq!(&stored[..PAGE_BYTES], &replaced);
    assert_eq!(&stored[PAGE_BYTES..], &high_entropy);
    assert!(
        backend.telemetry().cache_hits > 0,
        "real GPU cache must serve origin reads"
    );
    let origin_slab = backend
        .try_control_cache(|cache| cache.slab_telemetry())
        .expect("cache control available")
        .expect("slab telemetry query")
        .expect("device slab telemetry");
    assert!(
        origin_slab.logical_pages_cached.unwrap_or(0) > 64,
        "origin writes must populate the GPU slab: {origin_slab:?}"
    );
    drop(backend);
    std::fs::remove_file(origin_path).expect("remove private origin");
    worker
        .join()
        .expect("worker join")
        .expect("worker clean EOF");
    eprintln!("LIVE_RTX2060_SLAB_IPC_E2E=PASS");
}
