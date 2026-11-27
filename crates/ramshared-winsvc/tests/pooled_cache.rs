// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/tests/pooled_cache.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use ramshared_ipc::cache_wire::{
    CACHE_PAGE_BYTES, CacheFrameHeader, CacheGuestRole, CacheOpcode, CacheScope,
    DEFAULT_POOLED_CACHE_PORT, crc32c, validate_cache_data_port,
};
use ramshared_ipc::vsock::VsockPeerIdentity;
use ramshared_vram::ReserveFloorPolicy;
use ramshared_winsvc::pooled_cache::allocation::{
    AdapterObservation, AllocationRefusal, GuestCustodyState, GuestPlacementIntent, PoolAdmission,
    RolePlacement,
};
use ramshared_winsvc::pooled_cache::worker::{
    GpuWorkerBackend, OwnedGpuWorker, WATCHDOG_DRAIN_BOUND, WATCHDOG_RETIRE_BOUND, WorkerFault,
    WorkerSlotKey,
};
use ramshared_winsvc::pooled_cache::{
    EnrolledRole, MAX_AUTH_FAILURES, PRIMARY_BURST_CAP, PoolSupervisor, SupervisorError,
};

const GIB: u64 = 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;

struct MockGpuWorker {
    storage: Vec<[u8; CACHE_PAGE_BYTES]>,
    store_fault: Option<WorkerFault>,
    load_fault: Option<WorkerFault>,
    scrub_fault: Option<WorkerFault>,
    corrupt_slot_on_load: Option<usize>,
    terminated: bool,
    destroyed: bool,
}

impl MockGpuWorker {
    fn new(slots: usize) -> Self {
        Self {
            storage: vec![[0u8; CACHE_PAGE_BYTES]; slots],
            store_fault: None,
            load_fault: None,
            scrub_fault: None,
            corrupt_slot_on_load: None,
            terminated: false,
            destroyed: false,
        }
    }
}

impl GpuWorkerBackend for MockGpuWorker {
    fn store_slot(
        &mut self,
        slot: usize,
        page: &[u8; CACHE_PAGE_BYTES],
    ) -> Result<(), WorkerFault> {
        if let Some(f) = self.store_fault {
            return Err(f);
        }
        self.storage[slot] = *page;
        Ok(())
    }

    fn load_slot(
        &mut self,
        slot: usize,
        dst: &mut [u8; CACHE_PAGE_BYTES],
    ) -> Result<(), WorkerFault> {
        if let Some(f) = self.load_fault {
            return Err(f);
        }
        *dst = self.storage[slot];
        if self.corrupt_slot_on_load == Some(slot) {
            dst[0] ^= 0xFF;
        }
        Ok(())
    }

    fn scrub_slot(&mut self, slot: usize) -> Result<(), WorkerFault> {
        if let Some(f) = self.scrub_fault {
            return Err(f);
        }
        self.storage[slot].fill(0);
        Ok(())
    }

    fn terminate_process(&mut self) {
        self.terminated = true;
    }

    fn context_destroyed(&self) -> bool {
        self.destroyed
    }
}

fn fresh_obs(now: Instant) -> AdapterObservation {
    AdapterObservation {
        adapter_id: "rtx-2060-pci-07".into(),
        adapter_local_bytes: 6 * GIB,
        process_budget_bytes: 5 * GIB,
        wddm_current_usage_bytes: 512 * MIB,
        cuda_free_bytes: 4 * GIB,
        host_physical_ram_bytes: 32 * GIB,
        host_available_ram_bytes: 12 * GIB,
        staging_allowance_bytes: 64 * MIB,
        sampled_at: now,
        signed_match: true,
    }
}

fn enrolled(role: CacheGuestRole, id_byte: u8, key_byte: u8) -> EnrolledRole {
    EnrolledRole {
        role,
        key: [key_byte; 32],
        expected_runtime_id: [id_byte; 16],
        expected_origin_fingerprint: [id_byte ^ 0x55; 32],
        extent_bytes: 16 * CACHE_PAGE_BYTES as u64,
        data_port: DEFAULT_POOLED_CACHE_PORT,
        control_port: 2761,
    }
}

fn authenticate(
    sup: &mut PoolSupervisor<MockGpuWorker>,
    cfg: &EnrolledRole,
    epoch: u64,
) -> CacheScope {
    let guid = validate_cache_data_port(cfg.data_port, cfg.control_port).unwrap();
    let peer = VsockPeerIdentity {
        vm_id: cfg.expected_runtime_id,
        service_id: guid,
    };
    let g_nonce = [0x31u8; 32];
    let h_nonce = [0x41u8; 32];
    let scope = CacheScope {
        role: cfg.role,
        runtime_id: cfg.expected_runtime_id,
        service_guid: guid,
        origin_fingerprint: cfg.expected_origin_fingerprint,
        epoch,
        extent_bytes: cfg.extent_bytes,
        guest_nonce: g_nonce,
        host_nonce: h_nonce,
    };
    let gp = scope.guest_proof(&cfg.key).unwrap();
    let fp = scope.finish_proof(&cfg.key).unwrap();
    sup.authenticate_session(cfg.role, Some(peer), g_nonce, h_nonce, &gp, &fp)
        .unwrap()
}

fn sealed_pair() -> (RolePlacement, RolePlacement) {
    (
        RolePlacement {
            intent: GuestPlacementIntent::PooledSealed,
            custody: GuestCustodyState::RunningConsented { boot_gen: 1 },
        },
        RolePlacement {
            intent: GuestPlacementIntent::PooledSealed,
            custody: GuestCustodyState::RunningConsented { boot_gen: 1 },
        },
    )
}

#[test]
fn host_pool_prioritizes_primary_and_quarantines_uncertain_allocation() {
    let now = Instant::now();
    let obs = fresh_obs(now);
    let mut sup = PoolSupervisor::new(
        PoolAdmission::new(ReserveFloorPolicy::sealed(), "rtx-2060-pci-07"),
        OwnedGpuWorker::new(MockGpuWorker::new(8), 8),
    );
    let wsl_cfg = enrolled(CacheGuestRole::Wsl2, 0xA1, 0x11);
    let sec_cfg = enrolled(CacheGuestRole::SecondaryGuest, 0xB2, 0x22);
    sup.enroll(wsl_cfg.clone()).unwrap();
    sup.enroll(sec_cfg.clone()).unwrap();
    let (wp, sp) = sealed_pair();
    sup.update_placement(wp, sp, false, now).unwrap();

    let wsl_scope = authenticate(&mut sup, &wsl_cfg, 1);
    let sec_scope = authenticate(&mut sup, &sec_cfg, 1);

    // 1. Secondary can lend when idle, then WSL2 evicts secondary lending on demand (DT-8).
    let sec_page = Box::new([0x5Au8; CACHE_PAGE_BYTES]);
    let sec_crc = crc32c(&sec_page[..]);
    let sec_offer = CacheFrameHeader::new(CacheOpcode::Offer, 1, sec_scope.epoch, 0, 1).unwrap();
    sup.enqueue_offer(
        CacheGuestRole::SecondaryGuest,
        sec_offer,
        sec_page,
        sec_crc,
        now,
    )
    .unwrap();
    let r = sup.step(&obs, now).unwrap();
    assert_eq!(r.header.opcode, CacheOpcode::Accepted);
    assert_eq!(
        sup.admission().charged_for(CacheGuestRole::SecondaryGuest),
        CACHE_PAGE_BYTES as u64
    );

    let wsl_page = Box::new([0xA5u8; CACHE_PAGE_BYTES]);
    let wsl_crc = crc32c(&wsl_page[..]);
    let wsl_offer = CacheFrameHeader::new(CacheOpcode::Offer, 1, wsl_scope.epoch, 0, 1).unwrap();
    sup.enqueue_offer(
        CacheGuestRole::Wsl2,
        wsl_offer,
        wsl_page.clone(),
        wsl_crc,
        now,
    )
    .unwrap();
    let r = sup.step(&obs, now).unwrap();
    assert_eq!(r.header.opcode, CacheOpcode::Accepted);
    assert_eq!(
        sup.admission().charged_for(CacheGuestRole::SecondaryGuest),
        0,
        "WSL2 demand must reclaim secondary lending first"
    );
    assert_eq!(
        sup.admission().charged_for(CacheGuestRole::Wsl2),
        CACHE_PAGE_BYTES as u64
    );

    // 2. Bounded 4:1 WSL2 priority scheduling (at most 4 WSL2 lookups before 1 queued Secondary).
    let mut order = Vec::new();
    for corr in 2u64..7 {
        let _ = sup.enqueue_lookup(
            CacheGuestRole::Wsl2,
            CacheFrameHeader::new(CacheOpcode::Lookup, corr, wsl_scope.epoch, 0, 1).unwrap(),
            now,
        );
        let _ = sup.enqueue_lookup(
            CacheGuestRole::SecondaryGuest,
            CacheFrameHeader::new(CacheOpcode::Lookup, corr, sec_scope.epoch, 0, 1).unwrap(),
            now,
        );
        if let Some(reply) = sup.step(&obs, now) {
            order.push(reply.role);
        }
    }
    let w_corr = 7u64;
    assert_eq!(
        order[..PRIMARY_BURST_CAP as usize],
        vec![CacheGuestRole::Wsl2; PRIMARY_BURST_CAP as usize]
    );
    assert_eq!(
        order[PRIMARY_BURST_CAP as usize],
        CacheGuestRole::SecondaryGuest
    );
    // Drain any remaining queued lookup cleanly.
    while sup.step(&obs, now).is_some() {}

    // 3. Hung GPU driver on store quarantines the worker AND keeps credits charged until exit (DT-9).
    sup.worker_mut().backend_mut().store_fault = Some(WorkerFault::DriverHung);
    let hung_offer =
        CacheFrameHeader::new(CacheOpcode::Offer, w_corr, wsl_scope.epoch, 4096, 1).unwrap();
    sup.enqueue_offer(CacheGuestRole::Wsl2, hung_offer, wsl_page, wsl_crc, now)
        .unwrap();
    let busy = sup.step(&obs, now).unwrap();
    assert_eq!(busy.header.opcode, CacheOpcode::Busy);
    assert!(sup.worker().is_quarantined());
    assert!(sup.worker().backend().terminated);
    assert_eq!(
        sup.admission().quarantined_bytes(),
        2 * CACHE_PAGE_BYTES as u64,
        "both in-flight and committed bytes remain quarantined until context exit"
    );
    // Passing watchdog intervals alone NEVER releases quarantined credits while context is alive.
    assert!(!sup.poll_quarantine_reap(now + WATCHDOG_RETIRE_BOUND + WATCHDOG_DRAIN_BOUND));
    assert_eq!(
        sup.try_release_placement(),
        Err(SupervisorError::Allocation(
            AllocationRefusal::QuarantineActive
        ))
    );
    // Verified context destruction finally releases quarantined credits and placement permit.
    sup.worker_mut().backend_mut().destroyed = true;
    assert!(sup.poll_quarantine_reap(now + WATCHDOG_DRAIN_BOUND));
    assert_eq!(sup.admission().quarantined_bytes(), 0);
    assert!(sup.try_release_placement().is_ok());
}

#[test]
fn two_sessions_read_equal_offsets_and_retire_stale_completion() {
    let now = Instant::now();
    let obs = fresh_obs(now);
    let mut sup = PoolSupervisor::new(
        PoolAdmission::new(ReserveFloorPolicy::sealed(), "rtx-2060-pci-07"),
        OwnedGpuWorker::new(MockGpuWorker::new(16), 16),
    );
    let wsl_cfg = enrolled(CacheGuestRole::Wsl2, 0xC1, 0x31);
    let sec_cfg = enrolled(CacheGuestRole::SecondaryGuest, 0xD2, 0x42);
    sup.enroll(wsl_cfg.clone()).unwrap();
    sup.enroll(sec_cfg.clone()).unwrap();
    let (wp, sp) = sealed_pair();
    sup.update_placement(wp, sp, false, now).unwrap();

    let w_scope = authenticate(&mut sup, &wsl_cfg, 1);
    let s_scope = authenticate(&mut sup, &sec_cfg, 1);

    // Same offset (4096) offered by both guests with distinct payloads; neither may read the other's bytes.
    let w_bytes = Box::new([0xAAu8; CACHE_PAGE_BYTES]);
    let s_bytes = Box::new([0xBBu8; CACHE_PAGE_BYTES]);
    // Insert WSL first, then Secondary (no WSL waiting -> Secondary stores into its own slot).
    sup.enqueue_offer(
        CacheGuestRole::Wsl2,
        CacheFrameHeader::new(CacheOpcode::Offer, 1, w_scope.epoch, 4096, 5).unwrap(),
        w_bytes.clone(),
        crc32c(&w_bytes[..]),
        now,
    )
    .unwrap();
    assert_eq!(
        sup.step(&obs, now).unwrap().header.opcode,
        CacheOpcode::Accepted
    );
    // Exact duplicate offer reuses accounted slot without growing charged bytes (#17).
    sup.enqueue_offer(
        CacheGuestRole::Wsl2,
        CacheFrameHeader::new(CacheOpcode::Offer, 2, w_scope.epoch, 4096, 5).unwrap(),
        w_bytes.clone(),
        crc32c(&w_bytes[..]),
        now,
    )
    .unwrap();
    assert_eq!(
        sup.step(&obs, now).unwrap().header.opcode,
        CacheOpcode::Accepted
    );
    assert_eq!(
        sup.admission().charged_for(CacheGuestRole::Wsl2),
        CACHE_PAGE_BYTES as u64
    );

    sup.enqueue_offer(
        CacheGuestRole::SecondaryGuest,
        CacheFrameHeader::new(CacheOpcode::Offer, 1, s_scope.epoch, 4096, 5).unwrap(),
        s_bytes.clone(),
        crc32c(&s_bytes[..]),
        now,
    )
    .unwrap();
    assert_eq!(
        sup.step(&obs, now).unwrap().header.opcode,
        CacheOpcode::Accepted
    );
    assert_eq!(
        sup.worker().resident_bytes_for(CacheGuestRole::Wsl2),
        CACHE_PAGE_BYTES as u64
    );
    assert_eq!(
        sup.worker()
            .resident_bytes_for(CacheGuestRole::SecondaryGuest),
        CACHE_PAGE_BYTES as u64
    );

    // Both read offset 4096 and receive their own exact pages.
    sup.enqueue_lookup(
        CacheGuestRole::Wsl2,
        CacheFrameHeader::new(CacheOpcode::Lookup, 3, w_scope.epoch, 4096, 5).unwrap(),
        now,
    )
    .unwrap();
    let w_hit = sup.step(&obs, now).unwrap();
    assert_eq!(w_hit.header.opcode, CacheOpcode::Hit);
    assert_eq!(w_hit.page.as_deref(), Some(&*w_bytes));

    sup.enqueue_lookup(
        CacheGuestRole::SecondaryGuest,
        CacheFrameHeader::new(CacheOpcode::Lookup, 2, s_scope.epoch, 4096, 5).unwrap(),
        now,
    )
    .unwrap();
    let s_hit = sup.step(&obs, now).unwrap();
    assert_eq!(s_hit.header.opcode, CacheOpcode::Hit);
    assert_eq!(s_hit.page.as_deref(), Some(&*s_bytes));

    // Stale generation lookup misses cleanly; bad-CRC slot is invalidated as poison pill.
    sup.enqueue_lookup(
        CacheGuestRole::SecondaryGuest,
        CacheFrameHeader::new(CacheOpcode::Lookup, 3, s_scope.epoch, 4096, 6).unwrap(),
        now,
    )
    .unwrap();
    assert_eq!(
        sup.step(&obs, now).unwrap().header.opcode,
        CacheOpcode::Miss
    );

    sup.worker_mut().backend_mut().corrupt_slot_on_load = Some(1);
    sup.enqueue_lookup(
        CacheGuestRole::SecondaryGuest,
        CacheFrameHeader::new(CacheOpcode::Lookup, 4, s_scope.epoch, 4096, 5).unwrap(),
        now,
    )
    .unwrap();
    assert_eq!(
        sup.step(&obs, now).unwrap().header.opcode,
        CacheOpcode::Busy
    );

    // Replayed or stale-epoch frame retires the session while leaving the other guest unaffected.
    assert!(matches!(
        sup.enqueue_lookup(
            CacheGuestRole::SecondaryGuest,
            CacheFrameHeader::new(CacheOpcode::Lookup, 4, s_scope.epoch, 4096, 5).unwrap(),
            now,
        ),
        Err(SupervisorError::Wire(_))
    ));
    assert_eq!(sup.active_epoch(CacheGuestRole::SecondaryGuest), None);
    assert_eq!(sup.active_epoch(CacheGuestRole::Wsl2), Some(1));
    assert_eq!(
        sup.counters(CacheGuestRole::SecondaryGuest)
            .epoch_retirements,
        1
    );

    // Auth refusal rate limiting and wrong route/proof refusal.
    let bad_peer = VsockPeerIdentity {
        vm_id: [0xFF; 16],
        service_id: [0xEE; 16],
    };
    for _ in 0..MAX_AUTH_FAILURES {
        assert_eq!(
            sup.authenticate_session(
                CacheGuestRole::SecondaryGuest,
                Some(bad_peer),
                [1; 32],
                [2; 32],
                &[0; 32],
                &[0; 32],
            ),
            Err(SupervisorError::RouteMismatch)
        );
    }
    assert_eq!(
        sup.authenticate_session(
            CacheGuestRole::SecondaryGuest,
            Some(bad_peer),
            [1; 32],
            [2; 32],
            &[0; 32],
            &[0; 32],
        ),
        Err(SupervisorError::RateLimited)
    );
}

#[test]
fn pool_refuses_double_allocator_and_stale_budget() {
    let now = Instant::now();
    let mut admission = PoolAdmission::new(ReserveFloorPolicy::sealed(), "rtx-2060-pci-07");
    let (wp, sp) = sealed_pair();

    // Competing independent allocator refuses placement (DT-7 / Pass 2).
    assert_eq!(
        admission.authorize_placement(wp, sp, true),
        Err(AllocationRefusal::CompetingAllocator)
    );
    assert!(!admission.placement_permit_held());

    // Independent boot intent or unconsented running role refuses placement.
    let indep = RolePlacement {
        intent: GuestPlacementIntent::Independent,
        custody: GuestCustodyState::RunningConsented { boot_gen: 1 },
    };
    assert_eq!(
        admission.authorize_placement(indep, sp, false),
        Err(AllocationRefusal::PlacementNotAuthorized)
    );
    let unconsented = RolePlacement {
        intent: GuestPlacementIntent::PooledSealed,
        custody: GuestCustodyState::RunningUnconsented,
    };
    assert_eq!(
        admission.authorize_placement(wp, unconsented, false),
        Err(AllocationRefusal::PlacementNotAuthorized)
    );

    // Authorize legitimately, then prove stale sample (> 2s), unsigned adapter, and low host RAM refuse.
    admission.authorize_placement(wp, sp, false).unwrap();
    let stale_now = now + Duration::from_millis(2_500);
    assert_eq!(
        admission.try_charge(
            CacheGuestRole::Wsl2,
            4096,
            &fresh_obs(now),
            stale_now,
            false
        ),
        Err(AllocationRefusal::StaleObservation)
    );

    let mut bad_adapter = fresh_obs(now);
    bad_adapter.signed_match = false;
    assert_eq!(
        admission.try_charge(CacheGuestRole::Wsl2, 4096, &bad_adapter, now, false),
        Err(AllocationRefusal::AmbiguousAdapter)
    );

    let mut low_host = fresh_obs(now);
    low_host.host_available_ram_bytes = 2 * GIB;
    assert_eq!(
        admission.try_charge(CacheGuestRole::Wsl2, 4096, &low_host, now, false),
        Err(AllocationRefusal::HostReserveFloor)
    );
    // Explicit dual-VM host floor clamps at >= 4 GiB: admits at 5 GiB, still refuses below 4 GiB.
    let mut dual_vm_adm = PoolAdmission::new(ReserveFloorPolicy::sealed(), "rtx-2060-pci-07")
        .with_host_ram_floor_bytes(4 * GIB);
    dual_vm_adm.authorize_placement(wp, sp, false).unwrap();
    let mut mid_host = fresh_obs(now);
    mid_host.host_available_ram_bytes = 5 * GIB;
    assert!(
        dual_vm_adm
            .try_charge(CacheGuestRole::Wsl2, 4096, &mid_host, now, false)
            .is_ok()
    );
    assert_eq!(
        dual_vm_adm.try_charge(CacheGuestRole::Wsl2, 4096, &low_host, now, false),
        Err(AllocationRefusal::HostReserveFloor)
    );

    // Below GPU reserve floor yields ZeroHeadroom; secondary while WSL waiting yields PrimaryPriority.
    let mut low_vram = fresh_obs(now);
    low_vram.cuda_free_bytes = 1024 * MIB;
    assert_eq!(
        admission.try_charge(CacheGuestRole::Wsl2, 4096, &low_vram, now, false),
        Err(AllocationRefusal::ZeroHeadroom)
    );
    assert_eq!(
        admission.try_charge(
            CacheGuestRole::SecondaryGuest,
            4096,
            &fresh_obs(now),
            now,
            true
        ),
        Err(AllocationRefusal::PrimaryPriority)
    );
    assert_eq!(
        admission.try_charge(CacheGuestRole::Wsl2, 0, &fresh_obs(now), now, false),
        Err(AllocationRefusal::ZeroRequest)
    );
    // Secondary lending is capped at 128 MiB step.
    let lent = admission
        .try_charge(
            CacheGuestRole::SecondaryGuest,
            512 * MIB,
            &fresh_obs(now),
            now,
            false,
        )
        .unwrap();
    assert_eq!(lent, 128 * MIB);
    assert_eq!(admission.in_flight_bytes(), 128 * MIB);
}

#[test]
fn pool_refuses_off_to_incompatible_boot_and_retains_adapter_fence() {
    let now = Instant::now();
    let obs = fresh_obs(now);
    let mut sup = PoolSupervisor::new(
        PoolAdmission::new(ReserveFloorPolicy::sealed(), "rtx-2060-pci-07"),
        OwnedGpuWorker::new(MockGpuWorker::new(8), 8),
    );
    let wsl_cfg = enrolled(CacheGuestRole::Wsl2, 0xE1, 0x71);
    sup.enroll(wsl_cfg.clone()).unwrap();

    // Controlled Off with sealed boot path is admitted; Off without sealed path or IncompatibleBoot refuses.
    let wsl_on = RolePlacement {
        intent: GuestPlacementIntent::PooledSealed,
        custody: GuestCustodyState::RunningConsented { boot_gen: 2 },
    };
    let sec_controlled_off = RolePlacement {
        intent: GuestPlacementIntent::PooledSealed,
        custody: GuestCustodyState::ControlledOff {
            sealed_boot_path: true,
        },
    };
    sup.update_placement(wsl_on, sec_controlled_off, false, now)
        .unwrap();
    assert!(sup.admission().placement_permit_held());

    // Populate one WSL slot so the worker holds active memory.
    let w_scope = authenticate(&mut sup, &wsl_cfg, 1);
    let page = Box::new([0x33u8; CACHE_PAGE_BYTES]);
    sup.enqueue_offer(
        CacheGuestRole::Wsl2,
        CacheFrameHeader::new(CacheOpcode::Offer, 1, w_scope.epoch, 0, 1).unwrap(),
        page.clone(),
        crc32c(&page[..]),
        now,
    )
    .unwrap();
    assert_eq!(
        sup.step(&obs, now).unwrap().header.opcode,
        CacheOpcode::Accepted
    );

    // Secondary transitions from Off to an incompatible/unsealed boot -> refused, sessions fenced,
    // AND adapter placement permit remains held while worker process is still active (Pass 2!).
    let sec_incompat = RolePlacement {
        intent: GuestPlacementIntent::PooledSealed,
        custody: GuestCustodyState::IncompatibleBoot,
    };
    assert_eq!(
        sup.update_placement(wsl_on, sec_incompat, false, now),
        Err(SupervisorError::Allocation(
            AllocationRefusal::IncompatibleOffTransition
        ))
    );
    assert_eq!(sup.active_epoch(CacheGuestRole::Wsl2), None);
    assert!(sup.admission().placement_permit_held());
    assert_eq!(
        sup.try_release_placement(),
        Err(SupervisorError::Allocation(
            AllocationRefusal::QuarantineActive
        ))
    );

    // Saved or restored guest custody is also explicitly refused.
    let sec_saved = RolePlacement {
        intent: GuestPlacementIntent::PooledSealed,
        custody: GuestCustodyState::SavedOrRestored,
    };
    assert_eq!(
        sup.update_placement(wsl_on, sec_saved, false, now),
        Err(SupervisorError::Allocation(
            AllocationRefusal::SavedOrRestoredGuest
        ))
    );

    // Worker scrub failure keeps pending bytes quarantined and retains the adapter fence until exit.
    let key = WorkerSlotKey {
        role: CacheGuestRole::Wsl2,
        epoch: 9,
        offset_bytes: 0,
        content_gen: 1,
    };
    sup.worker_mut()
        .publish_offer(key, &page, crc32c(&page[..]), now)
        .unwrap();
    sup.worker_mut().backend_mut().scrub_fault = Some(WorkerFault::TimedOut);
    let retire = sup
        .worker_mut()
        .retire_scope(CacheGuestRole::Wsl2, Some(9), now);
    assert_eq!(retire.pending_scrub_bytes, CACHE_PAGE_BYTES as u64);
    assert!(sup.worker().is_quarantined());
    assert!(sup.admission().placement_permit_held());
    sup.worker_mut().backend_mut().destroyed = true;
    assert!(sup.poll_quarantine_reap(now));
    assert!(sup.try_release_placement().is_ok());
    assert!(!sup.admission().placement_permit_held());

    // Additional refusal & edge-case custody coverage for supervisor and owned worker.
    let mut sup2 = PoolSupervisor::new(
        PoolAdmission::new(ReserveFloorPolicy::sealed(), "rtx-2060-pci-07"),
        OwnedGpuWorker::new(MockGpuWorker::new(1), 1),
    );
    let mut bad_port_cfg = enrolled(CacheGuestRole::Wsl2, 0xA2, 0x12);
    bad_port_cfg.data_port = 2761;
    assert!(matches!(
        sup2.enroll(bad_port_cfg),
        Err(SupervisorError::Wire(_))
    ));
    assert_eq!(
        sup2.authenticate_session(
            CacheGuestRole::Wsl2,
            None,
            [1; 32],
            [2; 32],
            &[0; 32],
            &[0; 32]
        ),
        Err(SupervisorError::UnenrolledRole)
    );
    assert_eq!(
        sup2.enqueue_lookup(
            CacheGuestRole::Wsl2,
            CacheFrameHeader::new(CacheOpcode::Lookup, 1, 1, 0, 1).unwrap(),
            now,
        ),
        Err(SupervisorError::SessionInactive)
    );
    assert_eq!(
        sup2.enqueue_offer(
            CacheGuestRole::Wsl2,
            CacheFrameHeader::new(CacheOpcode::Offer, 1, 1, 0, 1).unwrap(),
            page.clone(),
            crc32c(&page[..]),
            now,
        ),
        Err(SupervisorError::SessionInactive)
    );
    let wsl2 = enrolled(CacheGuestRole::Wsl2, 0xA3, 0x13);
    let sec2 = enrolled(CacheGuestRole::SecondaryGuest, 0xB3, 0x23);
    sup2.enroll(wsl2.clone()).unwrap();
    sup2.enroll(sec2.clone()).unwrap();
    let (wp2, sp2) = sealed_pair();
    sup2.update_placement(wp2, sp2, false, now).unwrap();

    // Matching peer but wrong HMAC proof returns AuthFailed.
    let guid2 = validate_cache_data_port(wsl2.data_port, wsl2.control_port).unwrap();
    assert_eq!(
        sup2.authenticate_session(
            CacheGuestRole::Wsl2,
            Some(VsockPeerIdentity {
                vm_id: wsl2.expected_runtime_id,
                service_id: guid2,
            }),
            [1; 32],
            [2; 32],
            &[0xFF; 32],
            &[0xFF; 32],
        ),
        Err(SupervisorError::AuthFailed)
    );
    let w2 = authenticate(&mut sup2, &wsl2, 1);
    let s2 = authenticate(&mut sup2, &sec2, 1);
    // Duplicate active session is refused.
    assert_eq!(
        sup2.authenticate_session(
            CacheGuestRole::Wsl2,
            Some(VsockPeerIdentity {
                vm_id: wsl2.expected_runtime_id,
                service_id: guid2,
            }),
            [1; 32],
            [2; 32],
            &[0; 32],
            &[0; 32],
        ),
        Err(SupervisorError::DuplicateActiveSession)
    );
    // Offer queue capacity (2) rejects 3rd offer with QueueFull; non-hang CapacityExhausted frees charge.
    sup2.enqueue_offer(
        CacheGuestRole::Wsl2,
        CacheFrameHeader::new(CacheOpcode::Offer, 1, w2.epoch, 0, 1).unwrap(),
        page.clone(),
        crc32c(&page[..]),
        now,
    )
    .unwrap();
    sup2.enqueue_offer(
        CacheGuestRole::Wsl2,
        CacheFrameHeader::new(CacheOpcode::Offer, 2, w2.epoch, 4096, 1).unwrap(),
        page.clone(),
        crc32c(&page[..]),
        now,
    )
    .unwrap();
    assert_eq!(
        sup2.enqueue_offer(
            CacheGuestRole::Wsl2,
            CacheFrameHeader::new(CacheOpcode::Offer, 3, w2.epoch, 8192, 1).unwrap(),
            page.clone(),
            crc32c(&page[..]),
            now,
        ),
        Err(SupervisorError::QueueFull)
    );
    assert_eq!(
        sup2.step(&obs, now).unwrap().header.opcode,
        CacheOpcode::Accepted
    );
    // Second offer hits 1-slot CapacityExhausted -> Busy, charge released cleanly.
    assert_eq!(
        sup2.step(&obs, now).unwrap().header.opcode,
        CacheOpcode::Busy
    );
    // Stale observation during offer step -> Busy + dropped_offers.
    sup2.enqueue_offer(
        CacheGuestRole::SecondaryGuest,
        CacheFrameHeader::new(CacheOpcode::Offer, 1, s2.epoch, 0, 1).unwrap(),
        page.clone(),
        crc32c(&page[..]),
        now,
    )
    .unwrap();
    assert_eq!(
        sup2.step(&obs, now + Duration::from_secs(5))
            .unwrap()
            .header
            .opcode,
        CacheOpcode::Busy
    );
    // Wrong opcode on lookup/offer and bad CRC on offer retire sessions.
    assert!(matches!(
        sup2.enqueue_lookup(
            CacheGuestRole::SecondaryGuest,
            CacheFrameHeader::new(CacheOpcode::Offer, 2, s2.epoch, 0, 1).unwrap(),
            now,
        ),
        Err(SupervisorError::Wire(_))
    ));
    let s2_b = authenticate(&mut sup2, &sec2, 2);
    assert!(matches!(
        sup2.enqueue_offer(
            CacheGuestRole::SecondaryGuest,
            CacheFrameHeader::new(CacheOpcode::Lookup, 1, s2_b.epoch, 0, 1).unwrap(),
            page.clone(),
            crc32c(&page[..]),
            now,
        ),
        Err(SupervisorError::Wire(_))
    ));
    let s2_c = authenticate(&mut sup2, &sec2, 3);
    assert!(matches!(
        sup2.enqueue_offer(
            CacheGuestRole::SecondaryGuest,
            CacheFrameHeader::new(CacheOpcode::Offer, 1, s2_c.epoch, 0, 1).unwrap(),
            page.clone(),
            crc32c(&page[..]) ^ 1,
            now,
        ),
        Err(SupervisorError::Wire(_))
    ));
    // Lookup DriverHung quarantines worker and charged bytes.
    sup2.worker_mut().backend_mut().load_fault = Some(WorkerFault::DriverHung);
    sup2.enqueue_lookup(
        CacheGuestRole::Wsl2,
        CacheFrameHeader::new(CacheOpcode::Lookup, 4, w2.epoch, 0, 1).unwrap(),
        now,
    )
    .unwrap();
    assert_eq!(
        sup2.step(&obs, now).unwrap().header.opcode,
        CacheOpcode::Busy
    );
    assert!(sup2.worker().is_quarantined());

    // Direct worker edge-case custody checks (poll while clean, bad CRC, evict skip/scrub fault, quarantined ops).
    let mut w = OwnedGpuWorker::new(MockGpuWorker::new(3), 3);
    assert!(!w.poll_quarantine_settlement(now));
    let k_wsl = WorkerSlotKey {
        role: CacheGuestRole::Wsl2,
        epoch: 1,
        offset_bytes: 0,
        content_gen: 1,
    };
    let k_sec = WorkerSlotKey {
        role: CacheGuestRole::SecondaryGuest,
        epoch: 1,
        offset_bytes: 0,
        content_gen: 1,
    };
    assert_eq!(
        w.publish_offer(k_wsl, &page, crc32c(&page[..]) ^ 1, now),
        Err(WorkerFault::CorruptPayload)
    );
    w.publish_offer(k_wsl, &page, crc32c(&page[..]), now)
        .unwrap();
    w.publish_offer(k_sec, &page, crc32c(&page[..]), now)
        .unwrap();
    // Evict secondary skips WSL slot and empty slot; non-timeout scrub fault retains needs_scrub.
    w.backend_mut().scrub_fault = Some(WorkerFault::CorruptPayload);
    assert_eq!(
        w.evict_role_bytes(CacheGuestRole::SecondaryGuest, 8192, now),
        0
    );
    // Next publish retries scrub on dirty slot (skips on CorruptPayload) and fills slot 2, then CapacityExhausted.
    let k_wsl2 = WorkerSlotKey {
        role: CacheGuestRole::Wsl2,
        epoch: 1,
        offset_bytes: 4096,
        content_gen: 1,
    };
    w.publish_offer(k_wsl2, &page, crc32c(&page[..]), now)
        .unwrap();
    let k_wsl3 = WorkerSlotKey {
        role: CacheGuestRole::Wsl2,
        epoch: 1,
        offset_bytes: 8192,
        content_gen: 1,
    };
    assert_eq!(
        w.publish_offer(k_wsl3, &page, crc32c(&page[..]), now),
        Err(WorkerFault::CapacityExhausted)
    );
    // Scrub timeout during find_free_clean_slot quarantines the worker.
    w.backend_mut().scrub_fault = Some(WorkerFault::TimedOut);
    assert_eq!(
        w.publish_offer(k_wsl3, &page, crc32c(&page[..]), now),
        Err(WorkerFault::Quarantined)
    );
    let mut dst = [0u8; CACHE_PAGE_BYTES];
    assert_eq!(
        w.lookup_page(k_wsl, &mut dst, now),
        Err(WorkerFault::Quarantined)
    );
    assert_eq!(
        w.publish_offer(k_wsl3, &page, crc32c(&page[..]), now),
        Err(WorkerFault::Quarantined)
    );
    assert_eq!(
        w.retire_scope(CacheGuestRole::Wsl2, None, now)
            .physically_reclaimed_bytes,
        0
    );
}

#[test]
fn pooled_cuda_backend_geometry_and_observation_enforce_reserve_floor() {
    use ramshared_winsvc::pooled_cache::cuda_backend::{
        build_adapter_observation, validate_slab_geometry,
    };

    let policy = ReserveFloorPolicy::sealed();
    // 6 GiB GPU with 4 GiB free admits a 256 MiB slab (65536 slots) cleanly.
    assert_eq!(
        validate_slab_geometry(65_536, 4 * GIB, 6 * GIB, policy).unwrap(),
        256 * MIB
    );
    // Zero slots, overflow, and below-reserve free VRAM are all refused.
    assert!(validate_slab_geometry(0, 4 * GIB, 6 * GIB, policy).is_err());
    assert!(validate_slab_geometry(usize::MAX, 4 * GIB, 6 * GIB, policy).is_err());
    assert!(validate_slab_geometry(65_536, GIB, 6 * GIB, policy).is_err());

    let now = Instant::now();
    let obs = build_adapter_observation(
        "cuda-0-rtx2060",
        3 * GIB,
        6 * GIB,
        256 * MIB,
        32 * GIB,
        12 * GIB,
        now,
    );
    assert_eq!(obs.adapter_id, "cuda-0-rtx2060");
    assert_eq!(obs.cuda_free_bytes, 3 * GIB + 256 * MIB);
    assert_eq!(
        obs.wddm_current_usage_bytes,
        6 * GIB - (3 * GIB + 256 * MIB)
    );
    assert!(obs.signed_match);
}

#[cfg(unix)]
#[test]
fn pooled_server_serves_dual_guests_and_refuses_bad_routes_and_configs() {
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use ramshared_ipc::cache_wire::{
        CacheClientFinish, CacheClientHello, CacheFrameBuffer, origin_fingerprint_from_binding,
        read_host_challenge, read_into, write_client_finish, write_client_hello, write_frame_with,
    };
    use ramshared_ipc::vsock::{MockVsockStream, linux_service_guid};
    use ramshared_winsvc::pooled_cache::server::{
        PooledServerConfig, PooledStatusSnapshot, SchedulerHandle, SchedulerLoopConfig,
        build_live_supervisor, create_scheduler_channel, parse_key_bytes, run_scheduler_loop,
        serve_session_stream, write_status_atomic,
    };

    let dir = std::env::temp_dir().join(format!(
        "ramshared-pooled-srv-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    let wsl_key = [0xA1u8; 32];
    let sec_key = [0xB2u8; 32];
    let wsl_key_path = dir.join("wsl.key");
    let sec_key_path = dir.join("sec.key");
    // Exercise both raw 32-byte and 64-hex-char key loaders.
    std::fs::write(&wsl_key_path, wsl_key).unwrap();
    let sec_hex: String = sec_key.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(&sec_key_path, format!("{sec_hex}\n")).unwrap();
    let status_path = dir.join("status.json");

    // Key parser refusals (#13).
    assert!(parse_key_bytes(&[0u8; 32]).is_err());
    assert!(parse_key_bytes(b"short").is_err());
    assert!(parse_key_bytes(&[0xFF; 17]).is_err());
    assert!(parse_key_bytes(format!("{}zz", "aa".repeat(31)).as_bytes()).is_err());

    let sec_guid_str = "6b1632b8-4b03-462a-90f2-f5de50b4ff5d";
    let toml_cfg = format!(
        r#"
data_port = 2762
control_port = 2761
cuda_ordinal = 0
slot_count = 16
status_file = "{status}"

[wsl]
vm_id = "auto"
key_file = "{wsl_key}"
origin_binding = "wsl-origin-001"
extent_bytes = 65536

[secondary]
vm_id = "{sec_guid}"
key_file = "{sec_key}"
origin_binding = "sec-origin-002"
extent_bytes = 65536
"#,
        status = status_path.display(),
        wsl_key = wsl_key_path.display(),
        sec_guid = sec_guid_str,
        sec_key = sec_key_path.display(),
    );
    let cfg_path = dir.join("pooled-cache.toml");
    std::fs::write(&cfg_path, &toml_cfg).unwrap();
    let cfg = PooledServerConfig::load(&cfg_path).unwrap();
    assert!(cfg.wsl_uses_auto_vm_id());

    // Config refusal matrix (#13).
    assert!(PooledServerConfig::from_toml("data_port = 2761\n").is_err());
    assert!(PooledServerConfig::from_toml("slot_count = 0\n").is_err());
    assert!(PooledServerConfig::from_toml("cuda_ordinal = -1\n").is_err());
    assert!(
        PooledServerConfig::from_toml("host_ram_reserve_bytes = 1073741824\n").is_err(),
        "host RAM reserve floor below 4 GiB hard minimum must be refused"
    );
    assert!(PooledServerConfig::from_toml("data_port = 2762\n").is_err());
    assert!(PooledServerConfig::from_toml(&"x".repeat(70_000)).is_err());
    assert!(PooledServerConfig::load(&dir.join("missing.toml")).is_err());
    assert!(
        PooledServerConfig::from_toml(
            r#"
[secondary]
vm_id = "6b1632b8-4b03-462a-90f2-f5de50b4ff5d"
key_file = "relative.key"
origin_binding = "a"
extent_bytes = 4096
"#
        )
        .is_err()
    );
    assert!(
        PooledServerConfig::from_toml(&format!(
            r#"
[secondary]
vm_id = "6b1632b8-4b03-462a-90f2-f5de50b4ff5d"
key_file = "{}"
origin_binding = "a"
extent_bytes = 4097
"#,
            sec_key_path.display()
        ))
        .is_err()
    );
    assert!(
        PooledServerConfig::from_toml(&format!(
            r#"
[secondary]
vm_id = "6b1632b8-4b03-462a-90f2-f5de50b4ff5d"
key_file = "{}"
origin_binding = "   "
extent_bytes = 4096
"#,
            sec_key_path.display()
        ))
        .is_err()
    );
    assert!(
        PooledServerConfig::from_toml(&format!(
            r#"
[secondary]
vm_id = "auto"
key_file = "{}"
origin_binding = "a"
extent_bytes = 4096
"#,
            sec_key_path.display()
        ))
        .is_err()
    );
    let dup_guid = format!(
        r#"
data_port = 2762
[wsl]
vm_id = "{sec_guid_str}"
key_file = "{}"
origin_binding = "a"
extent_bytes = 4096
[secondary]
vm_id = "{sec_guid_str}"
key_file = "{}"
origin_binding = "b"
extent_bytes = 4096
"#,
        wsl_key_path.display(),
        sec_key_path.display()
    );
    assert!(PooledServerConfig::from_toml(&dup_guid).is_err());
    assert!(
        ramshared_winsvc::pooled_cache::server::run_hyperv_server(
            &cfg,
            Arc::new(AtomicBool::new(true))
        )
        .is_err()
    );
    assert!(
        ramshared_winsvc::pooled_cache::server::spawn_if_configured(Arc::new(AtomicBool::new(
            true
        )))
        .is_none()
    );

    let sup = build_live_supervisor(&cfg, "rtx-2060-pci-07", MockGpuWorker::new(16)).unwrap();
    let (handle, rx) = create_scheduler_channel();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_sched = Arc::clone(&stop);
    let status_clone = status_path.clone();
    let sched = std::thread::spawn(move || {
        run_scheduler_loop(
            sup,
            SchedulerLoopConfig {
                adapter_id: "rtx-2060-pci-07",
                slot_capacity: 16,
                wsl_auto_vm: true,
                status_path: Some(&status_clone),
            },
            rx,
            |_backend, now| Some(fresh_obs(now)),
            &stop_sched,
        );
    });

    let sec_vm_id = ramshared_winsvc::vm_id_resolve::parse_guid(sec_guid_str).unwrap();
    let wsl_vm_id = [0x44u8; 16];
    let svc_guid = linux_service_guid(2762);

    // 1. Refuse WSL2 pretending to have secondary's VM GUID (dynamic auto-bind collision).
    {
        let (mut cli, srv) = UnixStream::pair().unwrap();
        let h = handle.clone();
        let st = Arc::clone(&stop);
        let t = std::thread::spawn(move || {
            let mut s = MockVsockStream(srv);
            let _ = serve_session_stream(
                &mut s,
                &h,
                2762,
                &st,
                Some(VsockPeerIdentity {
                    vm_id: sec_vm_id,
                    service_id: svc_guid,
                }),
            );
        });
        let hello = CacheClientHello {
            role: CacheGuestRole::Wsl2,
            requested_epoch: 1,
            extent_bytes: 65536,
            origin_fingerprint: origin_fingerprint_from_binding("wsl-origin-001").unwrap(),
            guest_nonce: [0x12; 32],
        };
        write_client_hello(&mut cli, &hello).unwrap();
        let challenge = read_host_challenge(&mut cli).unwrap();
        assert!(!challenge.accepted);
        drop(cli);
        t.join().unwrap();
    }

    // 1b. Refuse bad extent, bad origin fingerprint, and wrong secondary route before active session.
    for (role, vm, extent, fp) in [
        (
            CacheGuestRole::SecondaryGuest,
            [0xEE; 16],
            65536u64,
            origin_fingerprint_from_binding("sec-origin-002").unwrap(),
        ),
        (
            CacheGuestRole::SecondaryGuest,
            sec_vm_id,
            4096u64,
            origin_fingerprint_from_binding("sec-origin-002").unwrap(),
        ),
        (
            CacheGuestRole::SecondaryGuest,
            sec_vm_id,
            65536u64,
            origin_fingerprint_from_binding("wrong-origin").unwrap(),
        ),
    ] {
        let (mut cli, srv) = UnixStream::pair().unwrap();
        let h = handle.clone();
        let st = Arc::clone(&stop);
        let t = std::thread::spawn(move || {
            let mut s = MockVsockStream(srv);
            let _ = serve_session_stream(
                &mut s,
                &h,
                2762,
                &st,
                Some(VsockPeerIdentity {
                    vm_id: vm,
                    service_id: svc_guid,
                }),
            );
        });
        write_client_hello(
            &mut cli,
            &CacheClientHello {
                role,
                requested_epoch: 1,
                extent_bytes: extent,
                origin_fingerprint: fp,
                guest_nonce: [0x24; 32],
            },
        )
        .unwrap();
        assert!(!read_host_challenge(&mut cli).unwrap().accepted);
        drop(cli);
        t.join().unwrap();
    }

    // 2. Refuse stream lacking native AF_HYPERV peer identity (override_peer=None on UnixStream).
    {
        let (_cli, srv) = UnixStream::pair().unwrap();
        let mut s = MockVsockStream(srv);
        assert!(serve_session_stream(&mut s, &handle, 2762, &stop, None).is_err());
    }

    // 3. Authenticate BOTH WSL2 (auto VM GUID) and Secondary concurrently, Offer + Lookup isolated pages.
    let run_guest = |role: CacheGuestRole,
                     vm_id: [u8; 16],
                     key: [u8; 32],
                     binding: &'static str,
                     fill: u8,
                     h: SchedulerHandle,
                     st: Arc<AtomicBool>| {
        let (mut cli, srv) = UnixStream::pair().unwrap();
        let srv_thread = std::thread::spawn(move || {
            let mut s = MockVsockStream(srv);
            serve_session_stream(
                &mut s,
                &h,
                2762,
                &st,
                Some(VsockPeerIdentity {
                    vm_id,
                    service_id: svc_guid,
                }),
            )
        });
        let fp = origin_fingerprint_from_binding(binding).unwrap();
        let g_nonce = [fill; 32];
        write_client_hello(
            &mut cli,
            &CacheClientHello {
                role,
                requested_epoch: 1,
                extent_bytes: 65536,
                origin_fingerprint: fp,
                guest_nonce: g_nonce,
            },
        )
        .unwrap();
        let ch = read_host_challenge(&mut cli).unwrap();
        assert!(ch.accepted);
        assert_eq!(ch.runtime_id, vm_id);
        let scope = CacheScope {
            role,
            runtime_id: ch.runtime_id,
            service_guid: svc_guid,
            origin_fingerprint: fp,
            epoch: ch.epoch,
            extent_bytes: 65536,
            guest_nonce: g_nonce,
            host_nonce: ch.host_nonce,
        };
        assert!(scope.verify_host_proof(&key, &ch.host_proof));
        write_client_finish(
            &mut cli,
            &CacheClientFinish {
                guest_proof: scope.guest_proof(&key).unwrap(),
                finish_proof: scope.finish_proof(&key).unwrap(),
            },
        )
        .unwrap();

        let page = [fill; CACHE_PAGE_BYTES];
        let mut wb = CacheFrameBuffer::new();
        let mut rb = CacheFrameBuffer::new();
        let offer = CacheFrameHeader::new(CacheOpcode::Offer, 1, ch.epoch, 4096, 1).unwrap();
        write_frame_with(&mut cli, &mut wb, offer, Some(&page)).unwrap();
        let acc = read_into(&mut cli, &mut rb).unwrap();
        assert_eq!(acc.header.opcode, CacheOpcode::Accepted);

        let lookup = CacheFrameHeader::new(CacheOpcode::Lookup, 2, ch.epoch, 4096, 1).unwrap();
        write_frame_with(&mut cli, &mut wb, lookup, None).unwrap();
        let hit = read_into(&mut cli, &mut rb).unwrap();
        assert_eq!(hit.header.opcode, CacheOpcode::Hit);
        assert_eq!(hit.page, Some(&page));

        (cli, srv_thread)
    };

    let (wsl_cli, wsl_srv) = run_guest(
        CacheGuestRole::Wsl2,
        wsl_vm_id,
        wsl_key,
        "wsl-origin-001",
        0xAA,
        handle.clone(),
        Arc::clone(&stop),
    );
    let (sec_cli, sec_srv) = run_guest(
        CacheGuestRole::SecondaryGuest,
        sec_vm_id,
        sec_key,
        "sec-origin-002",
        0xBB,
        handle.clone(),
        Arc::clone(&stop),
    );

    // Duplicate active WSL2 session is refused while live session stays intact.
    {
        let (mut cli, srv) = UnixStream::pair().unwrap();
        let h = handle.clone();
        let st = Arc::clone(&stop);
        let t = std::thread::spawn(move || {
            let mut s = MockVsockStream(srv);
            let _ = serve_session_stream(
                &mut s,
                &h,
                2762,
                &st,
                Some(VsockPeerIdentity {
                    vm_id: wsl_vm_id,
                    service_id: svc_guid,
                }),
            );
        });
        write_client_hello(
            &mut cli,
            &CacheClientHello {
                role: CacheGuestRole::Wsl2,
                requested_epoch: 1,
                extent_bytes: 65536,
                origin_fingerprint: origin_fingerprint_from_binding("wsl-origin-001").unwrap(),
                guest_nonce: [0x36; 32],
            },
        )
        .unwrap();
        assert!(!read_host_challenge(&mut cli).unwrap().accepted);
        drop(cli);
        t.join().unwrap();
    }

    let snap = handle.snapshot().unwrap();
    assert_eq!(snap.wsl_active_epoch, Some(1));
    assert_eq!(snap.secondary_active_epoch, Some(1));
    assert_eq!(snap.wsl.hits, 1);
    assert_eq!(snap.secondary.hits, 1);
    assert_eq!(snap.wsl.gpu_hit_bytes, 4096);
    assert_eq!(snap.secondary.gpu_hit_bytes, 4096);

    // Verify atomic status JSON roundtrips cleanly.
    write_status_atomic(&status_path, &snap).unwrap();
    let decoded: PooledStatusSnapshot =
        serde_json::from_str(&std::fs::read_to_string(&status_path).unwrap()).unwrap();
    assert_eq!(decoded, snap);

    // Clean disconnect retires both sessions and releases charged VRAM bytes.
    drop(wsl_cli);
    drop(sec_cli);
    wsl_srv.join().unwrap().unwrap();
    sec_srv.join().unwrap().unwrap();

    let after = handle.snapshot().unwrap();
    assert_eq!(after.wsl_active_epoch, None);
    assert_eq!(after.secondary_active_epoch, None);
    assert_eq!(after.wsl_charged_bytes, 0);
    assert_eq!(after.secondary_charged_bytes, 0);

    stop.store(true, Ordering::Release);
    handle.shutdown();
    sched.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
