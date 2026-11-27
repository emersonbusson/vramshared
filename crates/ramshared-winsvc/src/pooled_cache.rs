// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/pooled_cache.rs
//! Dual-guest pooled VRAM read-cache supervisor, bounded queues, and scheduling.
//! SPEC: the protocol specification §DT-3, §DT-8, §DT-10, §DT-11.

pub mod allocation;
pub mod cuda_backend;
pub mod server;
pub mod worker;

use std::collections::VecDeque;
use std::time::Instant;

use ramshared_ipc::cache_wire::{
    CACHE_PAGE_BYTES, CacheFrameHeader, CacheGuestRole, CacheOpcode, CacheScope, CacheWireError,
    crc32c, validate_cache_data_port,
};
use ramshared_ipc::vsock::VsockPeerIdentity;

use crate::vm_id_resolve::verify_connected_route_peer;
use allocation::{AdapterObservation, AllocationRefusal, PoolAdmission, RolePlacement};
use worker::{GpuWorkerBackend, OwnedGpuWorker, StoreOutcome, WorkerFault, WorkerSlotKey};

pub const PRIMARY_BURST_CAP: u8 = 4;
pub const PER_ROLE_LOOKUP_CAP: usize = 2;
pub const PER_ROLE_OFFER_CAP: usize = 2;
pub const MAX_AUTH_FAILURES: u32 = 5;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnrolledRole {
    pub role: CacheGuestRole,
    pub key: [u8; 32],
    pub expected_runtime_id: [u8; 16],
    pub expected_origin_fingerprint: [u8; 32],
    pub extent_bytes: u64,
    pub data_port: u32,
    pub control_port: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RoleCounters {
    pub hits: u64,
    pub misses: u64,
    pub busy: u64,
    pub accepted_offers: u64,
    pub dropped_offers: u64,
    pub auth_refusals: u64,
    pub frame_refusals: u64,
    pub epoch_retirements: u64,
    pub gpu_hit_bytes: u64,
}

#[derive(Debug, Eq, PartialEq)]
pub enum SupervisorError {
    UnenrolledRole,
    RateLimited,
    RouteMismatch,
    AuthFailed,
    DuplicateActiveSession,
    SessionInactive,
    QueueFull,
    Wire(CacheWireError),
    Allocation(AllocationRefusal),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueuedLookup {
    pub header: CacheFrameHeader,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueuedOffer {
    pub header: CacheFrameHeader,
    pub page: Box<[u8; CACHE_PAGE_BYTES]>,
    pub crc: u32,
}

#[derive(Debug, Eq, PartialEq)]
pub struct ScheduledReply {
    pub role: CacheGuestRole,
    pub header: CacheFrameHeader,
    pub page: Option<Box<[u8; CACHE_PAGE_BYTES]>>,
}

struct RoleState {
    enrolled: Option<EnrolledRole>,
    active_scope: Option<CacheScope>,
    last_correlation: u64,
    next_epoch: u64,
    failed_auths: u32,
    lookups: VecDeque<QueuedLookup>,
    offers: VecDeque<QueuedOffer>,
    counters: RoleCounters,
}

impl Default for RoleState {
    fn default() -> Self {
        Self {
            enrolled: None,
            active_scope: None,
            last_correlation: 0,
            next_epoch: 1,
            failed_auths: 0,
            lookups: VecDeque::with_capacity(PER_ROLE_LOOKUP_CAP),
            offers: VecDeque::with_capacity(PER_ROLE_OFFER_CAP),
            counters: RoleCounters::default(),
        }
    }
}

pub struct PoolSupervisor<B: GpuWorkerBackend> {
    wsl: RoleState,
    secondary: RoleState,
    admission: PoolAdmission,
    worker: OwnedGpuWorker<B>,
    primary_burst_used: u8,
}

impl<B: GpuWorkerBackend> PoolSupervisor<B> {
    pub fn new(admission: PoolAdmission, worker: OwnedGpuWorker<B>) -> Self {
        Self {
            wsl: RoleState::default(),
            secondary: RoleState::default(),
            admission,
            worker,
            primary_burst_used: 0,
        }
    }

    pub fn admission(&self) -> &PoolAdmission {
        &self.admission
    }

    pub fn worker(&self) -> &OwnedGpuWorker<B> {
        &self.worker
    }

    pub fn worker_mut(&mut self) -> &mut OwnedGpuWorker<B> {
        &mut self.worker
    }

    pub fn counters(&self, role: CacheGuestRole) -> RoleCounters {
        self.role_state(role).counters
    }

    pub fn active_epoch(&self, role: CacheGuestRole) -> Option<u64> {
        self.role_state(role).active_scope.as_ref().map(|s| s.epoch)
    }

    pub fn enrolled(&self, role: CacheGuestRole) -> Option<&EnrolledRole> {
        self.role_state(role).enrolled.as_ref()
    }

    pub fn next_epoch(&self, role: CacheGuestRole) -> u64 {
        self.role_state(role).next_epoch
    }

    /// Associate a dynamically observed WSL2 runtime ID when no WSL2 session is active
    /// and the candidate does not collide with the enrolled secondary guest VM GUID (DT-3).
    pub fn bind_dynamic_wsl_runtime_id(
        &mut self,
        runtime_id: [u8; 16],
    ) -> Result<(), SupervisorError> {
        if runtime_id == [0; 16] {
            return Err(SupervisorError::RouteMismatch);
        }
        if self.wsl.active_scope.is_some() {
            return Err(SupervisorError::DuplicateActiveSession);
        }
        if self
            .secondary
            .enrolled
            .as_ref()
            .is_some_and(|sec| sec.expected_runtime_id == runtime_id)
        {
            return Err(SupervisorError::RouteMismatch);
        }
        let Some(cfg) = self.wsl.enrolled.as_mut() else {
            return Err(SupervisorError::UnenrolledRole);
        };
        cfg.expected_runtime_id = runtime_id;
        Ok(())
    }

    pub fn enroll(&mut self, config: EnrolledRole) -> Result<[u8; 16], SupervisorError> {
        let guid = validate_cache_data_port(config.data_port, config.control_port)
            .map_err(SupervisorError::Wire)?;
        let role = config.role;
        self.role_state_mut(role).enrolled = Some(config);
        Ok(guid)
    }

    pub fn update_placement(
        &mut self,
        wsl: RolePlacement,
        secondary: RolePlacement,
        competing_allocator_active: bool,
        now: Instant,
    ) -> Result<(), SupervisorError> {
        if let Err(refusal) =
            self.admission
                .authorize_placement(wsl, secondary, competing_allocator_active)
        {
            // Keep existing adapter fence intact while fencing both roles to origin-only.
            self.retire_session(CacheGuestRole::Wsl2, now);
            self.retire_session(CacheGuestRole::SecondaryGuest, now);
            return Err(SupervisorError::Allocation(refusal));
        }
        Ok(())
    }

    pub fn try_release_placement(&mut self) -> Result<(), SupervisorError> {
        self.admission
            .try_release_placement(self.worker.is_process_active())
            .map_err(SupervisorError::Allocation)
    }

    /// Authenticate one native route and bind a monotonic cache epoch without replacing live peers.
    pub fn authenticate_session(
        &mut self,
        role: CacheGuestRole,
        observed_peer: Option<VsockPeerIdentity>,
        guest_nonce: [u8; 32],
        host_nonce: [u8; 32],
        guest_proof: &[u8; 32],
        finish_proof: &[u8; 32],
    ) -> Result<CacheScope, SupervisorError> {
        let state = self.role_state_mut(role);
        let Some(cfg) = state.enrolled.clone() else {
            return Err(SupervisorError::UnenrolledRole);
        };
        if state.failed_auths >= MAX_AUTH_FAILURES {
            state.counters.auth_refusals = state.counters.auth_refusals.saturating_add(1);
            return Err(SupervisorError::RateLimited);
        }
        if state.active_scope.is_some() {
            state.counters.auth_refusals = state.counters.auth_refusals.saturating_add(1);
            return Err(SupervisorError::DuplicateActiveSession);
        }
        let peer =
            verify_connected_route_peer(cfg.expected_runtime_id, cfg.data_port, observed_peer)
                .map_err(|_| {
                    state.failed_auths = state.failed_auths.saturating_add(1);
                    state.counters.auth_refusals = state.counters.auth_refusals.saturating_add(1);
                    SupervisorError::RouteMismatch
                })?;
        let scope = CacheScope {
            role,
            runtime_id: peer.vm_id,
            service_guid: peer.service_id,
            origin_fingerprint: cfg.expected_origin_fingerprint,
            epoch: state.next_epoch,
            extent_bytes: cfg.extent_bytes,
            guest_nonce,
            host_nonce,
        };
        if !scope.verify_guest_proof(&cfg.key, guest_proof)
            || !scope.verify_finish_proof(&cfg.key, finish_proof)
        {
            state.failed_auths = state.failed_auths.saturating_add(1);
            state.counters.auth_refusals = state.counters.auth_refusals.saturating_add(1);
            return Err(SupervisorError::AuthFailed);
        }
        state.failed_auths = 0;
        state.last_correlation = 0;
        state.next_epoch = state.next_epoch.saturating_add(1);
        state.active_scope = Some(scope.clone());
        Ok(scope)
    }

    pub fn retire_session(&mut self, role: CacheGuestRole, now: Instant) {
        let had_epoch = {
            let state = self.role_state_mut(role);
            state.lookups.clear();
            state.offers.clear();
            state.last_correlation = 0;
            state.active_scope.take().map(|s| s.epoch)
        };
        if let Some(epoch) = had_epoch {
            let summary = self.worker.retire_scope(role, Some(epoch), now);
            self.admission
                .release_verified(role, summary.physically_reclaimed_bytes);
            if summary.pending_scrub_bytes > 0 {
                match role {
                    CacheGuestRole::Wsl2 => {
                        self.admission
                            .quarantine_uncertain(0, summary.pending_scrub_bytes, 0);
                    }
                    CacheGuestRole::SecondaryGuest => {
                        self.admission
                            .quarantine_uncertain(0, 0, summary.pending_scrub_bytes);
                    }
                }
            }
            let c = &mut self.role_state_mut(role).counters;
            c.epoch_retirements = c.epoch_retirements.saturating_add(1);
        }
    }

    pub fn enqueue_lookup(
        &mut self,
        role: CacheGuestRole,
        header: CacheFrameHeader,
        now: Instant,
    ) -> Result<(), SupervisorError> {
        let res = {
            let state = self.role_state_mut(role);
            let Some(scope) = state.active_scope.as_ref() else {
                return Err(SupervisorError::SessionInactive);
            };
            if header.opcode != CacheOpcode::Lookup {
                Err(CacheWireError::UnsolicitedOrMismatchedReply)
            } else {
                scope.admit_request(&header, &mut state.last_correlation)
            }
        };
        if let Err(err) = res {
            self.role_state_mut(role).counters.frame_refusals += 1;
            self.retire_session(role, now);
            return Err(SupervisorError::Wire(err));
        }
        let state = self.role_state_mut(role);
        if state.lookups.len() >= PER_ROLE_LOOKUP_CAP {
            state.counters.busy += 1;
            return Err(SupervisorError::QueueFull);
        }
        state.lookups.push_back(QueuedLookup { header });
        Ok(())
    }

    pub fn enqueue_offer(
        &mut self,
        role: CacheGuestRole,
        header: CacheFrameHeader,
        page: Box<[u8; CACHE_PAGE_BYTES]>,
        crc: u32,
        now: Instant,
    ) -> Result<(), SupervisorError> {
        let res = {
            let state = self.role_state_mut(role);
            let Some(scope) = state.active_scope.as_ref() else {
                return Err(SupervisorError::SessionInactive);
            };
            if header.opcode != CacheOpcode::Offer {
                Err(CacheWireError::UnsolicitedOrMismatchedReply)
            } else if crc32c(&page[..]) != crc {
                Err(CacheWireError::CrcMismatch {
                    expected: crc32c(&page[..]),
                    got: crc,
                })
            } else {
                scope.admit_request(&header, &mut state.last_correlation)
            }
        };
        if let Err(err) = res {
            self.role_state_mut(role).counters.frame_refusals += 1;
            self.retire_session(role, now);
            return Err(SupervisorError::Wire(err));
        }
        let state = self.role_state_mut(role);
        if state.offers.len() >= PER_ROLE_OFFER_CAP {
            state.counters.dropped_offers += 1;
            return Err(SupervisorError::QueueFull);
        }
        state.offers.push_back(QueuedOffer { header, page, crc });
        Ok(())
    }

    /// Execute one bounded scheduling step: lookups beat offers; at most 4 WSL2 per 1 Secondary.
    pub fn step(&mut self, obs: &AdapterObservation, now: Instant) -> Option<ScheduledReply> {
        if let Some(role) = self.pick_next_lookup_role() {
            let req = self.role_state_mut(role).lookups.pop_front()?;
            return Some(self.execute_lookup(role, req.header, now));
        }
        if let Some(role) = self.pick_next_offer_role() {
            let req = self.role_state_mut(role).offers.pop_front()?;
            return Some(self.execute_offer(role, req, obs, now));
        }
        None
    }

    pub fn poll_quarantine_reap(&mut self, now: Instant) -> bool {
        if self.worker.poll_quarantine_settlement(now) {
            let q = self.admission.quarantined_bytes();
            self.admission.release_quarantined_after_exit(q);
            return true;
        }
        false
    }

    fn pick_next_lookup_role(&mut self) -> Option<CacheGuestRole> {
        let wsl_ready = !self.wsl.lookups.is_empty();
        let sec_ready = !self.secondary.lookups.is_empty();
        match (wsl_ready, sec_ready) {
            (true, false) => {
                self.primary_burst_used = self
                    .primary_burst_used
                    .saturating_add(1)
                    .min(PRIMARY_BURST_CAP);
                Some(CacheGuestRole::Wsl2)
            }
            (false, true) => {
                self.primary_burst_used = 0;
                Some(CacheGuestRole::SecondaryGuest)
            }
            (true, true) => {
                if self.primary_burst_used < PRIMARY_BURST_CAP {
                    self.primary_burst_used += 1;
                    Some(CacheGuestRole::Wsl2)
                } else {
                    self.primary_burst_used = 0;
                    Some(CacheGuestRole::SecondaryGuest)
                }
            }
            (false, false) => None,
        }
    }

    fn pick_next_offer_role(&self) -> Option<CacheGuestRole> {
        if !self.wsl.offers.is_empty() {
            Some(CacheGuestRole::Wsl2)
        } else if !self.secondary.offers.is_empty() {
            Some(CacheGuestRole::SecondaryGuest)
        } else {
            None
        }
    }

    fn execute_lookup(
        &mut self,
        role: CacheGuestRole,
        req: CacheFrameHeader,
        now: Instant,
    ) -> ScheduledReply {
        let key = WorkerSlotKey {
            role,
            epoch: req.epoch,
            offset_bytes: req.offset_bytes,
            content_gen: req.content_gen,
        };
        let mut buf = Box::new([0u8; CACHE_PAGE_BYTES]);
        match self.worker.lookup_page(key, &mut buf, now) {
            Ok(Some(_)) => {
                let c = &mut self.role_state_mut(role).counters;
                c.hits += 1;
                c.gpu_hit_bytes = c.gpu_hit_bytes.saturating_add(CACHE_PAGE_BYTES as u64);
                ScheduledReply {
                    role,
                    header: CacheFrameHeader {
                        opcode: CacheOpcode::Hit,
                        payload_len: CacheOpcode::Hit.expected_payload_len(),
                        ..req
                    },
                    page: Some(buf),
                }
            }
            Ok(None) => {
                self.role_state_mut(role).counters.misses += 1;
                ScheduledReply {
                    role,
                    header: CacheFrameHeader {
                        opcode: CacheOpcode::Miss,
                        payload_len: 0,
                        ..req
                    },
                    page: None,
                }
            }
            Err(fault) => {
                if matches!(fault, WorkerFault::TimedOut | WorkerFault::DriverHung) {
                    let w = self.admission.charged_for(CacheGuestRole::Wsl2);
                    let s = self.admission.charged_for(CacheGuestRole::SecondaryGuest);
                    self.admission.quarantine_uncertain(0, w, s);
                }
                self.role_state_mut(role).counters.busy += 1;
                ScheduledReply {
                    role,
                    header: CacheFrameHeader {
                        opcode: CacheOpcode::Busy,
                        payload_len: 0,
                        ..req
                    },
                    page: None,
                }
            }
        }
    }

    fn execute_offer(
        &mut self,
        role: CacheGuestRole,
        req: QueuedOffer,
        obs: &AdapterObservation,
        now: Instant,
    ) -> ScheduledReply {
        let wsl_waiting = !self.wsl.lookups.is_empty() || !self.wsl.offers.is_empty();
        // Evict secondary lending before growing WSL2 when WSL2 asks for capacity (DT-8).
        if role == CacheGuestRole::Wsl2
            && self.admission.charged_for(CacheGuestRole::SecondaryGuest) > 0
        {
            let reclaimed = self.worker.evict_role_bytes(
                CacheGuestRole::SecondaryGuest,
                CACHE_PAGE_BYTES as u64,
                now,
            );
            self.admission
                .release_verified(CacheGuestRole::SecondaryGuest, reclaimed);
        }
        let charged =
            match self
                .admission
                .try_charge(role, CACHE_PAGE_BYTES as u64, obs, now, wsl_waiting)
            {
                Ok(bytes) => bytes,
                Err(_) => {
                    self.role_state_mut(role).counters.dropped_offers += 1;
                    return ScheduledReply {
                        role,
                        header: CacheFrameHeader {
                            opcode: CacheOpcode::Busy,
                            payload_len: 0,
                            ..req.header
                        },
                        page: None,
                    };
                }
            };
        let key = WorkerSlotKey {
            role,
            epoch: req.header.epoch,
            offset_bytes: req.header.offset_bytes,
            content_gen: req.header.content_gen,
        };
        match self.worker.publish_offer(key, &req.page, req.crc, now) {
            Ok(StoreOutcome::StoredNew { .. }) => {
                self.admission.commit_charged(role, charged);
                self.role_state_mut(role).counters.accepted_offers += 1;
                ScheduledReply {
                    role,
                    header: CacheFrameHeader {
                        opcode: CacheOpcode::Accepted,
                        payload_len: 0,
                        ..req.header
                    },
                    page: None,
                }
            }
            Ok(StoreOutcome::ReusedExisting { .. }) => {
                self.admission.commit_charged(role, charged);
                self.admission.release_verified(role, charged);
                self.role_state_mut(role).counters.accepted_offers += 1;
                ScheduledReply {
                    role,
                    header: CacheFrameHeader {
                        opcode: CacheOpcode::Accepted,
                        payload_len: 0,
                        ..req.header
                    },
                    page: None,
                }
            }
            Err(WorkerFault::TimedOut | WorkerFault::DriverHung) => {
                let w = self.admission.charged_for(CacheGuestRole::Wsl2);
                let s = self.admission.charged_for(CacheGuestRole::SecondaryGuest);
                self.admission.quarantine_uncertain(charged, w, s);
                self.role_state_mut(role).counters.dropped_offers += 1;
                ScheduledReply {
                    role,
                    header: CacheFrameHeader {
                        opcode: CacheOpcode::Busy,
                        payload_len: 0,
                        ..req.header
                    },
                    page: None,
                }
            }
            Err(_) => {
                self.admission.commit_charged(role, charged);
                self.admission.release_verified(role, charged);
                self.role_state_mut(role).counters.dropped_offers += 1;
                ScheduledReply {
                    role,
                    header: CacheFrameHeader {
                        opcode: CacheOpcode::Busy,
                        payload_len: 0,
                        ..req.header
                    },
                    page: None,
                }
            }
        }
    }

    fn role_state(&self, role: CacheGuestRole) -> &RoleState {
        match role {
            CacheGuestRole::Wsl2 => &self.wsl,
            CacheGuestRole::SecondaryGuest => &self.secondary,
        }
    }

    fn role_state_mut(&mut self, role: CacheGuestRole) -> &mut RoleState {
        match role {
            CacheGuestRole::Wsl2 => &mut self.wsl,
            CacheGuestRole::SecondaryGuest => &mut self.secondary,
        }
    }
}
