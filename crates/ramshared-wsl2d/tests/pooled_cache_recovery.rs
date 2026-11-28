// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/tests/pooled_cache_recovery.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use ramshared_block::isolated_origin::{
    AuthoritativeOriginBackend, POOLED_PAGE_BYTES, POOLED_STAGING_SLOTS, PooledCacheClient,
    PooledLookupReply, PooledReadState, PooledReadTelemetry,
};
use ramshared_block::{DisabledCache, FileOrigin};
use ramshared_ipc::cache_wire::{
    CacheFrameBuffer, CacheFrameHeader, CacheGuestRole, CacheOpcode, CacheScope,
    DEFAULT_POOLED_CACHE_PORT, crc32c, origin_fingerprint_from_binding, read_into,
    validate_cache_data_port, write_frame_with,
};
use ramshared_ipc::vsock::{MockVsockStream, VsockPeerIdentity, linux_service_guid};
use ramshared_vram::ReserveFloorPolicy;
use ramshared_winsvc::pooled_cache::allocation::{
    AdapterObservation, GuestCustodyState, GuestPlacementIntent, PoolAdmission, RolePlacement,
};
use ramshared_winsvc::pooled_cache::server::{
    SchedulerLoopConfig, create_scheduler_channel, run_scheduler_loop, serve_session_stream,
};
use ramshared_winsvc::pooled_cache::worker::{GpuWorkerBackend, OwnedGpuWorker, WorkerFault};
use ramshared_winsvc::pooled_cache::{EnrolledRole, PoolSupervisor};
use ramshared_wsl2d::pooled_cache::connector::{
    PooledReconnectWorker, connect_boot_pooled_with_dialer, try_reconnect_pooled_with_dialer,
};
use ramshared_wsl2d::pooled_cache::profile::{
    CACHE_PROFILE_SCHEMA_VERSION, CacheProfile, CacheProfileSelection, StagedCacheProfile,
    stage_next_boot_profile,
};
use ramshared_wsl2d::pooled_cache::{
    EffectiveCacheMode, GuestCacheSession, HostBootstrapProof, StreamPooledClient, VMADDR_CID_HOST,
};

fn unique_temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ramshared-pooled-recovery-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn sample_doc(key_ref: &str, origin_binding: &str) -> StagedCacheProfile {
    StagedCacheProfile {
        schema_version: CACHE_PROFILE_SCHEMA_VERSION,
        profile: CacheProfile::Pooled,
        role: "secondary_guest".into(),
        endpoint_version: 1,
        data_port: DEFAULT_POOLED_CACHE_PORT,
        key_ref: key_ref.into(),
        release_binding: "v1.0.6".into(),
        origin_binding: origin_binding.into(),
        generation: 1,
    }
}

fn sample_scope(epoch: u64) -> CacheScope {
    CacheScope {
        role: CacheGuestRole::SecondaryGuest,
        runtime_id: [0x21; 16],
        service_guid: validate_cache_data_port(DEFAULT_POOLED_CACHE_PORT, 2761).unwrap(),
        origin_fingerprint: [0x32; 32],
        epoch,
        extent_bytes: 16 * POOLED_PAGE_BYTES as u64,
        guest_nonce: [0x43; 32],
        host_nonce: [0x54; 32],
    }
}

struct MemWorker(Vec<[u8; POOLED_PAGE_BYTES]>);
impl GpuWorkerBackend for MemWorker {
    fn store_slot(
        &mut self,
        slot: usize,
        page: &[u8; POOLED_PAGE_BYTES],
    ) -> Result<(), WorkerFault> {
        self.0[slot] = *page;
        Ok(())
    }
    fn load_slot(
        &mut self,
        slot: usize,
        dst: &mut [u8; POOLED_PAGE_BYTES],
    ) -> Result<(), WorkerFault> {
        *dst = self.0[slot];
        Ok(())
    }
    fn scrub_slot(&mut self, slot: usize) -> Result<(), WorkerFault> {
        self.0[slot].fill(0);
        Ok(())
    }
    fn terminate_process(&mut self) {}
    fn context_destroyed(&self) -> bool {
        true
    }
}

#[test]
fn closed_stream_after_offer_failure_retires_epoch_without_false_active_or_slot_quarantine() {
    let dir = unique_temp_dir("offer-close");
    let key = [0x5Au8; 32];
    let scope = sample_scope(2);
    let proof = HostBootstrapProof {
        host_cid: VMADDR_CID_HOST,
        scope: scope.clone(),
        host_proof: scope.host_proof(&key).unwrap(),
        key,
    };
    let session = GuestCacheSession::from_boot_selection(
        Ok(CacheProfileSelection::Staged(sample_doc(
            "/etc/ramshared/pooled.key",
            "5039dca1-61a0-41ff-aa08-221f49326a1b",
        ))),
        "5039dca1-61a0-41ff-aa08-221f49326a1b",
        &["/dev/sdb"],
        Some(proof),
    )
    .unwrap();
    assert_eq!(
        session.status(None).effective_mode,
        EffectiveCacheMode::PooledActive
    );

    // Server answers the first Lookup with Miss, then drops the stream on Offer.
    let (client_sock, mut host_sock) = UnixStream::pair().unwrap();
    let server = std::thread::spawn(move || {
        let mut rbuf = CacheFrameBuffer::new();
        let mut wbuf = CacheFrameBuffer::new();
        let req = read_into(&mut host_sock, &mut rbuf).unwrap();
        assert_eq!(req.header.opcode, CacheOpcode::Lookup);
        let miss = CacheFrameHeader::new(
            CacheOpcode::Miss,
            req.header.correlation,
            req.header.epoch,
            req.header.offset_bytes,
            req.header.content_gen,
        )
        .unwrap();
        write_frame_with(&mut host_sock, &mut wbuf, miss, None).unwrap();
        // Read the Offer frame and drop the socket without replying (simulates timeout/disconnect).
        let offer = read_into(&mut host_sock, &mut rbuf).unwrap();
        assert_eq!(offer.header.opcode, CacheOpcode::Offer);
        drop(host_sock);
    });

    let wire_client = StreamPooledClient::new(scope.clone(), client_sock);
    let pooled = PooledReadState::with_budget(2, Duration::from_secs(2), wire_client);
    let origin_path = dir.join("origin.img");
    let origin_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&origin_path)
        .unwrap();
    origin_file.set_len(16 * POOLED_PAGE_BYTES as u64).unwrap();
    let backend = AuthoritativeOriginBackend::new(
        FileOrigin::from_file(origin_file),
        DisabledCache,
        16 * POOLED_PAGE_BYTES as u64,
        POOLED_PAGE_BYTES as u32,
    )
    .unwrap()
    .with_pooled_read(pooled);

    let pattern = [0xA7u8; POOLED_PAGE_BYTES];
    backend.write_at(0, &pattern).unwrap();
    let mut out = [0u8; POOLED_PAGE_BYTES];
    backend.read_at(0, &mut out).unwrap();
    assert_eq!(out, pattern);
    server.join().unwrap();

    // Offer transport failure retired the epoch immediately without quarantining any lookup slots.
    let pooled_ref = backend.pooled_read().unwrap();
    assert_eq!(pooled_ref.active_epoch(), 0);
    let tel = pooled_ref.telemetry();
    assert_eq!(tel.quarantined_slots, 0);
    assert_eq!(tel.deadline_overruns, 0);
    assert_eq!(tel.poison_retirements, 1);

    // Sync session retirement and confirm status never reports false pooled_active.
    session.retire_to_origin_only("pooled_epoch_retired");
    let st = session.status(Some(tel));
    assert_eq!(st.effective_mode, EffectiveCacheMode::PooledOriginOnly);
    assert_eq!(st.epoch, 0);
    assert_eq!(st.quarantined_slots, 0);

    // Truthfulness guard: even if a session claims PooledActive while all staging slots
    // are quarantined, status projects PooledOriginOnly instead of false pooled_active.
    let bad_active = GuestCacheSession::from_boot_selection(
        Ok(CacheProfileSelection::Staged(sample_doc(
            "/etc/ramshared/pooled.key",
            "5039dca1-61a0-41ff-aa08-221f49326a1b",
        ))),
        "5039dca1-61a0-41ff-aa08-221f49326a1b",
        &[],
        Some(HostBootstrapProof {
            host_cid: VMADDR_CID_HOST,
            scope: scope.clone(),
            host_proof: scope.host_proof(&key).unwrap(),
            key,
        }),
    )
    .unwrap();
    let quarantined_status = bad_active.status(Some(PooledReadTelemetry {
        quarantined_slots: POOLED_STAGING_SLOTS as u64,
        ..PooledReadTelemetry::default()
    }));
    assert_eq!(
        quarantined_status.effective_mode,
        EffectiveCacheMode::PooledOriginOnly
    );
    assert_eq!(
        quarantined_status.reason.as_deref(),
        Some("lookup_slots_quarantined")
    );

    // Invalid rebound proof is refused and increments peer_refusals (#13).
    assert!(!session.activate_rebound(&HostBootstrapProof {
        host_cid: 99,
        scope,
        host_proof: [0u8; 32],
        key,
    }));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pre_send_expired_deadline_skips_as_busy_without_quarantining_slot() {
    let scope = sample_scope(3);
    let (client_sock, mut host_sock) = UnixStream::pair().unwrap();
    let page = [0x5Cu8; POOLED_PAGE_BYTES];
    let server = std::thread::spawn(move || {
        let mut rbuf = CacheFrameBuffer::new();
        let mut wbuf = CacheFrameBuffer::new();
        let req = read_into(&mut host_sock, &mut rbuf).unwrap();
        assert_eq!(req.header.opcode, CacheOpcode::Lookup);
        let hit = CacheFrameHeader::new(
            CacheOpcode::Hit,
            req.header.correlation,
            req.header.epoch,
            req.header.offset_bytes,
            req.header.content_gen,
        )
        .unwrap();
        write_frame_with(&mut host_sock, &mut wbuf, hit, Some(&page)).unwrap();
    });

    let client = StreamPooledClient::new(scope, client_sock);
    let mut dst = [0u8; POOLED_PAGE_BYTES];
    let expired = Instant::now()
        .checked_sub(Duration::from_millis(1))
        .unwrap();
    assert_eq!(
        client.lookup_into(3, 0, 1, 1, expired, &mut dst),
        PooledLookupReply::Busy,
        "pre-send expired deadline must skip as Busy without closing or quarantining"
    );
    assert!(!client.is_closed());

    // Subsequent lookup with a healthy deadline still succeeds cleanly on the same stream.
    let future = Instant::now() + Duration::from_secs(2);
    assert_eq!(
        client.lookup_into(3, 0, 1, 2, future, &mut dst),
        PooledLookupReply::Hit {
            epoch: 3,
            slot_gen: 2,
            content_gen: 1,
            crc: crc32c(&page),
        }
    );
    assert_eq!(dst, page);
    server.join().unwrap();
}

#[test]
fn retired_pooled_session_rebinds_fresh_epoch_after_cooldown_and_serves_hits() {
    let dir = unique_temp_dir("reconnect-e2e");
    let key = [0x8Eu8; 32];
    let key_path = dir.join("pooled.key");
    std::fs::write(&key_path, key).unwrap();
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();

    let origin_binding = "5039dca1-61a0-41ff-aa08-221f49326a1b";
    let fp = origin_fingerprint_from_binding(origin_binding).unwrap();
    let vm_id = [0x77u8; 16];
    let extent = 16 * POOLED_PAGE_BYTES as u64;

    let mut sup = PoolSupervisor::new(
        PoolAdmission::new(ReserveFloorPolicy::sealed(), "cuda-0-test"),
        OwnedGpuWorker::new(MemWorker(vec![[0u8; POOLED_PAGE_BYTES]; 16]), 16),
    );
    sup.enroll(EnrolledRole {
        role: CacheGuestRole::SecondaryGuest,
        key,
        expected_runtime_id: vm_id,
        expected_origin_fingerprint: fp,
        extent_bytes: extent,
        data_port: DEFAULT_POOLED_CACHE_PORT,
        control_port: 2761,
    })
    .unwrap();
    let consented = RolePlacement {
        intent: GuestPlacementIntent::PooledSealed,
        custody: GuestCustodyState::RunningConsented { boot_gen: 1 },
    };
    sup.update_placement(consented, consented, false, Instant::now())
        .unwrap();

    let (handle, rx) = create_scheduler_channel();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_sched = Arc::clone(&stop);
    let sched = std::thread::spawn(move || {
        run_scheduler_loop(
            sup,
            SchedulerLoopConfig {
                adapter_id: "cuda-0-test",
                slot_capacity: 16,
                wsl_auto_vm: false,
                status_path: None,
            },
            rx,
            |_, now| {
                Some(AdapterObservation {
                    adapter_id: "cuda-0-test".into(),
                    adapter_local_bytes: 6 * 1024 * 1024 * 1024,
                    process_budget_bytes: 6 * 1024 * 1024 * 1024,
                    wddm_current_usage_bytes: 512 * 1024 * 1024,
                    cuda_free_bytes: 4 * 1024 * 1024 * 1024,
                    host_physical_ram_bytes: 32 * 1024 * 1024 * 1024,
                    host_available_ram_bytes: 12 * 1024 * 1024 * 1024,
                    staging_allowance_bytes: 16 * 1024 * 1024,
                    sampled_at: now,
                    signed_match: true,
                })
            },
            &stop_sched,
        );
    });

    let profile_path = dir.join("cache-mode.json");
    let staged = sample_doc(key_path.to_str().unwrap(), origin_binding);
    stage_next_boot_profile(&profile_path, staged.clone(), false).unwrap();

    // Start with initial live connection, verify miss->offer->hit, then retire it to simulate host drop.
    let (cli1, srv1) = UnixStream::pair().unwrap();
    let stop_peer1 = Arc::new(AtomicBool::new(false));
    let peer1 = {
        let h = handle.clone();
        let sp = Arc::clone(&stop_peer1);
        std::thread::spawn(move || {
            let mut s = MockVsockStream(srv1);
            serve_session_stream(
                &mut s,
                &h,
                DEFAULT_POOLED_CACHE_PORT,
                &sp,
                Some(VsockPeerIdentity {
                    vm_id,
                    service_id: linux_service_guid(DEFAULT_POOLED_CACHE_PORT),
                }),
            )
        })
    };

    let (session, pooled_opt) = connect_boot_pooled_with_dialer(
        Ok(CacheProfileSelection::Staged(staged)),
        origin_binding,
        &["/dev/sdb"],
        extent,
        |_| Ok(MockVsockStream(cli1)),
    )
    .unwrap();
    let initial_epoch = session.active_epoch();
    assert!(initial_epoch > 0);
    let session = Arc::new(session);
    let origin_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(dir.join("origin.img"))
        .unwrap();
    origin_file.set_len(extent).unwrap();
    let backend = AuthoritativeOriginBackend::new(
        FileOrigin::from_file(origin_file),
        DisabledCache,
        extent,
        POOLED_PAGE_BYTES as u32,
    )
    .unwrap()
    .with_pooled_read(pooled_opt.unwrap());
    let pooled_arc = backend.pooled_read_arc().unwrap();

    // Refusal checks: non-pooled, malformed profile, and independent session activation refusal (#13).
    assert!(
        try_reconnect_pooled_with_dialer(
            Ok(CacheProfileSelection::DefaultIndependent),
            origin_binding,
            extent,
            &session,
            &pooled_arc,
            |_| Err::<MockVsockStream, String>("must not dial".into()),
        )
        .is_err()
    );
    assert!(
        try_reconnect_pooled_with_dialer(
            Err(
                ramshared_wsl2d::pooled_cache::profile::ProfileError::MalformedDocument(
                    "bad json".into(),
                )
            ),
            origin_binding,
            extent,
            &session,
            &pooled_arc,
            |_| Err::<MockVsockStream, String>("must not dial".into()),
        )
        .is_err()
    );
    // Production spawn() constructor on an already-active session sleeps cleanly and joins on Drop.
    let active_watcher = PooledReconnectWorker::spawn(
        profile_path.clone(),
        origin_binding.into(),
        extent,
        Arc::clone(&session),
        Arc::clone(&pooled_arc),
    );
    std::thread::sleep(Duration::from_millis(20));
    drop(active_watcher);

    // Simulate transport drop -> retire epoch cleanly.
    stop_peer1.store(true, Ordering::Release);
    pooled_arc.retire_epoch();
    assert_eq!(pooled_arc.active_epoch(), 0);
    let _ = peer1.join();

    // Independent session refuses pooled reconnect activation even after a valid host handshake.
    let indep_session = GuestCacheSession::from_boot_selection(
        Ok(CacheProfileSelection::DefaultIndependent),
        origin_binding,
        &[],
        None,
    )
    .unwrap();
    let (cli_refuse, srv_refuse) = UnixStream::pair().unwrap();
    let stop_refuse = Arc::new(AtomicBool::new(false));
    let peer_refuse = {
        let h = handle.clone();
        let sp = Arc::clone(&stop_refuse);
        std::thread::spawn(move || {
            let mut s = MockVsockStream(srv_refuse);
            serve_session_stream(
                &mut s,
                &h,
                DEFAULT_POOLED_CACHE_PORT,
                &sp,
                Some(VsockPeerIdentity {
                    vm_id,
                    service_id: linux_service_guid(DEFAULT_POOLED_CACHE_PORT),
                }),
            )
        })
    };
    let temp_pooled = PooledReadState::new_retired();
    assert_eq!(
        try_reconnect_pooled_with_dialer(
            Ok(CacheProfileSelection::Staged(sample_doc(
                key_path.to_str().unwrap(),
                origin_binding,
            ))),
            origin_binding,
            extent,
            &indep_session,
            &temp_pooled,
            |_| Ok(MockVsockStream(cli_refuse)),
        )
        .unwrap_err(),
        "reconnect_activation_refused"
    );
    stop_refuse.store(true, Ordering::Release);
    let _ = peer_refuse.join();

    // Background reconnect worker fails once ("host restarting"), then connects cleanly on attempt 2!
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_bg = Arc::clone(&attempts);
    let peer2_holder = Arc::new(std::sync::Mutex::new(Vec::new()));
    let peer2_bg = Arc::clone(&peer2_holder);
    let h_bg = handle.clone();
    let stop_bg = Arc::clone(&stop);

    let reconnect_worker = PooledReconnectWorker::spawn_with_dialer(
        profile_path,
        origin_binding.into(),
        extent,
        Duration::from_millis(25),
        Arc::clone(&session),
        Arc::clone(&pooled_arc),
        move |is_wsl, port| {
            assert!(!is_wsl);
            assert_eq!(port, DEFAULT_POOLED_CACHE_PORT);
            let n = attempts_bg.fetch_add(1, Ordering::AcqRel);
            if n == 0 {
                return Err("host restarting".into());
            }
            let (cli2, srv2) = UnixStream::pair().map_err(|e| e.to_string())?;
            let h = h_bg.clone();
            let s_flag = Arc::clone(&stop_bg);
            let thr = std::thread::spawn(move || {
                let mut s = MockVsockStream(srv2);
                serve_session_stream(
                    &mut s,
                    &h,
                    DEFAULT_POOLED_CACHE_PORT,
                    &s_flag,
                    Some(VsockPeerIdentity {
                        vm_id,
                        service_id: linux_service_guid(DEFAULT_POOLED_CACHE_PORT),
                    }),
                )
            });
            peer2_bg.lock().unwrap().push(thr);
            Ok(MockVsockStream(cli2))
        },
    );

    // Wait up to 3 s for bounded reconnect to rebind a fresh epoch > initial_epoch.
    let wait_start = Instant::now();
    while session.active_epoch() <= initial_epoch && wait_start.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(15));
    }
    let rebound_epoch = session.active_epoch();
    assert!(
        rebound_epoch > initial_epoch,
        "expected rebound epoch > {initial_epoch}, got {rebound_epoch}"
    );
    assert_eq!(
        session.status(Some(pooled_arc.telemetry())).effective_mode,
        EffectiveCacheMode::PooledActive
    );

    // Prove live VRAM offer + hit on the rebound epoch!
    let page = [0xD4u8; POOLED_PAGE_BYTES];
    backend.write_at(4096, &page).unwrap();
    let mut r1 = [0u8; POOLED_PAGE_BYTES];
    backend.read_at(4096, &mut r1).unwrap();
    assert_eq!(r1, page);
    let mut r2 = [0u8; POOLED_PAGE_BYTES];
    backend.read_at(4096, &mut r2).unwrap();
    assert_eq!(r2, page);
    assert!(
        pooled_arc.telemetry().hits >= 1,
        "expected VRAM hit after bounded reconnect"
    );

    // Quarantine all lookup slots while session appears active: reconnect worker retires and rebinds!
    struct AlwaysTimeoutClient;
    impl PooledCacheClient for AlwaysTimeoutClient {
        fn lookup_into(
            &self,
            _epoch: u64,
            _offset: u64,
            _content_gen: u64,
            _slot_gen: u64,
            _deadline: Instant,
            _dst: &mut [u8; POOLED_PAGE_BYTES],
        ) -> PooledLookupReply {
            PooledLookupReply::TimedOut
        }
        fn offer_copy(
            &self,
            _epoch: u64,
            _offset: u64,
            _content_gen: u64,
            _page: &[u8; POOLED_PAGE_BYTES],
            _crc: u32,
        ) -> bool {
            false
        }
        fn close_poisoned(&self, _epoch: u64) {}
    }
    assert!(pooled_arc.rebind_client(rebound_epoch, AlwaysTimeoutClient));
    let mut tmp = [0u8; POOLED_PAGE_BYTES];
    backend.read_at(4096, &mut tmp).unwrap();
    backend.read_at(4096, &mut tmp).unwrap();
    let q_wait = Instant::now();
    while session.active_epoch() <= rebound_epoch && q_wait.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(15));
    }
    assert!(
        session.active_epoch() > rebound_epoch,
        "quarantined slots must trigger retirement and bounded reconnect"
    );
    assert_eq!(pooled_arc.telemetry().quarantined_slots, 0);

    drop(reconnect_worker);
    drop(backend);
    stop.store(true, Ordering::Release);
    for thr in std::mem::take(&mut *peer2_holder.lock().unwrap()) {
        let _ = thr.join();
    }
    handle.shutdown();
    sched.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
