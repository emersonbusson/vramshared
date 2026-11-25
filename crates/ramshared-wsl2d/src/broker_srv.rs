// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/broker_srv.rs
//! `broker_srv` — **pure** core of the broker (decision/state), testable without threads/sockets/GPU.
//!
//! Same discipline as the arbiter: [`BrokerCore`] receives [`CoreEvent`]s and returns [`Outbound`]s
//! that the IO layer ([`spawn_broker`]) executes — send `Msg` to the session, close session, request
//! the worker to zero a slice (DT-17), or log (RF-B4).
//!
//! (DT-18/20/22), reconciliation (DT-9/21), rebalancing with hygiene (DT-17), DemoteAll and **revocable
//! lease (RF-B3/DT-19)**. The IO layer ([`spawn_broker`]) runs the core over TCP (DT-2/24);
//! only the wiring in the daemon's `run_nbd` is missing (`--slices`/`--arbiter-listen`).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ramshared_broker::arbiter::{Arbiter, ArbiterConfig};
use ramshared_broker::lease::LeaseBook;
use ramshared_broker::model::{PsiSample, SliceId, SliceState, TenantId, TransportKind};
use ramshared_broker::protocol::{Msg, NbdEndpoint, PROTO_VERSION, SwapEntry, TenantMem};
use ramshared_broker::slices::SliceMap;

use crate::telemetry::{ReconcileFlag, SliceIoCounters, TelemetryCore, VramGauge};

mod reclaim_lease;
mod transport;

#[cfg(test)]
use transport::IoEvent;
pub use transport::{BrokerConfig, IO_EVENT_CAP, MAX_SESSIONS, spawn_broker};

/// NBD endpoints that agents receive in `SwapOn`, chosen by the transport (DT-25).
#[derive(Clone, Debug, Default)]
pub struct EndpointCfg {
    pub nbd_unix: Option<String>,
    pub nbd_tcp: Option<(String, u16)>,
}

/// Input event for the core (produced by the IO layer). `sid` = connection ID (session).
#[derive(Clone, Debug, PartialEq)]
pub enum CoreEvent {
    Msg(usize, Msg),
    Disconnected(usize),
    ZeroDone(SliceId, bool),
    Demote(String),
    Tick,
}

/// Output action executed by the IO layer.
#[derive(Clone, Debug, PartialEq)]
pub enum Outbound {
    ToSession(usize, Msg),
    CloseSession(usize),
    ZeroSlice {
        slice: SliceId,
        base: u64,
        len: u64,
    },
    Log(String),
    /// Reconciled telemetry sample (RF-5); the IO layer timestamps `t`/`branch`/`commit`.
    Telemetry(TelemetryCore),
}

/// Direction of a swap acknowledgement (audit AUD-01).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SwapAckDir {
    On,
    Off,
}

/// Outstanding swap work a session is allowed to acknowledge.
///
/// Armed only when `SwapOn`/`SwapOff` is actually delivered to a live session,
/// and keyed to that exact session id. The connection id is the operation
/// generation: a completion replayed after a reconnect cannot match the entry
/// that was armed for the connection that received the command (AUD-01).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SwapAckState {
    dir: SwapAckDir,
    tenant: TenantId,
    sid: usize,
}

/// Verdict for a swap acknowledgement (AUD-01).
enum SwapAckVerdict {
    /// Matches the armed operation: apply the effect.
    Armed,
    /// The owner is re-sending a completion already applied: no state change.
    Duplicate,
    /// Stale, foreign, unregistered or unsolicited: refuse and close.
    Unauthorized(&'static str),
}

#[derive(Clone, Debug)]
struct TenantState {
    name: String,
    transport: TransportKind,
    present: bool,
    sid: Option<usize>,
    /// Last pressure sample, or `None` when this tenant has never reported
    /// one. `Some(0.0)` is a measured idle; `None` is not a reading of zero.
    /// The broker must not invent the figure a tenant never sent.
    psi: Option<PsiSample>,
    reconciled: bool,
    /// Last memory telemetry of the tenant (RF-2); `None` if the agent does not report cgroup/diskstats.
    mem: Option<TenantMem>,
    /// Occupied swap (bytes) in the `Active` slices of this tenant, derived in `Psi` (DT-10/F-v2-2).
    occupied_bytes: u64,
}

/// Named configuration for the pure broker core.
pub struct BrokerCoreConfig {
    pub arbiter_cfg: ArbiterConfig,
    pub endpoints: EndpointCfg,
    pub swap_prio: Option<i32>,
    /// Bound on an orphaned active lease. A holder's authenticated Psi heartbeat renews it.
    pub lease_ttl: Duration,
    pub slice_io: Arc<Vec<SliceIoCounters>>,
    pub vram: Arc<VramGauge>,
    pub tol_frac: f64,
    pub recon_streak: u32,
}

/// Broker core: sole owner of `SliceMap` + `Arbiter` + session table (without locks; the
/// IO layer runs this in a single thread). `BTreeMap` by `TenantId` gives stable iteration
/// (deterministic round-robin).
pub struct BrokerCore {
    slice_map: SliceMap,
    arbiter: Arbiter,
    endpoints: EndpointCfg,
    swap_prio: Option<i32>,
    lease_ttl: Duration,
    tenants: BTreeMap<TenantId, TenantState>,
    sessions: HashMap<usize, TenantId>, // live connection → tenant
    name_to_id: HashMap<String, TenantId>, // stable ID by name (DT-22)
    next_tenant: TenantId,
    pending_dest: HashMap<SliceId, TenantId>, // destination of a MoveSlice in flight (post-zero)
    lease_book: LeaseBook,
    last_rebalance: Option<Instant>,
    // Telemetria/reconciliação (SPEC broker-telemetry-reconciliation).
    slice_io: Arc<Vec<SliceIoCounters>>, // counters per slice (data-plane writes, RF-1/DT-1)
    vram: Arc<VramGauge>,                // VRAM gauge published by the worker (RF-3/DT-5)
    demotes_total: u64,                  // accumulated canary DEMOTEs (RF-4)
    last_demote_reason: Option<String>,
    demotes_at_last_sample: u64, // baseline for the `demotes_delta` per tick
    recon_flag: ReconcileFlag,   // candidate flag (hysteresis DT-12)
    recon_count: u32,            // consecutive ticks with the same candidate flag
    tol_frac: f64,               // reconciliation tolerance (DT-7)
    recon_streak: u32,           // ticks to confirm the flag (DT-12)
    // R4: slices post-`SwapOffDone` waiting for zero confirmation (`ZeroDone`), with number of ticks
    // without confirmation. If `try_send(ZeroExport)` failed (channel full), `ZeroDone` does not arrive → the tick
    // re-emits zero (retry) until confirmed; escalates to ERROR after N. Only slices HERE have already been
    // swapped-off by the tenant (safe to re-zero); slices in Draining awaiting SwapOffDone are NOT included.
    pending_zero: HashMap<SliceId, u32>,
    // AUD-01: swap work actually dispatched to a session, keyed by the exact
    // connection that received it. An acknowledgement that does not match is
    // stale, foreign or unsolicited and must not mutate state.
    swap_ack: HashMap<SliceId, SwapAckState>,
}

/// Grace period (in ticks) before the 1st zero retry — gives time for `ZeroDone` in flight. Above this, the
/// tick re-emits `ZeroExport`; in [`ZERO_RETRY_ERROR`] ticks without confirmation, logs ERROR (R4).
const ZERO_RETRY_GRACE: u32 = 1;
const ZERO_RETRY_ERROR: u32 = 5;

/// Longest accepted tenant name, in bytes (AUD-02). A tenant name is an
/// identity label, not a data field; bounding it keeps the `name_to_id` key
/// bounded and refuses a peer that tries to smuggle a large string into the
/// control plane through an identity.
const MAX_TENANT_NAME_BYTES: usize = 64;

/// Most tenant records the broker will ever hold (AUD-02). The live control
/// plane is not a directory: a peer that registers distinct names must not be
/// able to grow `tenants`/`name_to_id` without limit.
const MAX_TENANTS: usize = 64;

/// Maps only an exact NBD block-device identity to its slice (DT-10/DT-21).
fn dev_to_slice(dev: &str) -> Option<SliceId> {
    let dev = dev.trim();
    let base = if let Some(base) = dev.strip_prefix("/dev/") {
        base
    } else if let Some(base) = dev.strip_prefix('/') {
        base
    } else if !dev.contains('/') {
        dev
    } else {
        return None;
    };
    let slice = base.strip_prefix("nbd")?;
    if slice.is_empty() || !slice.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    slice.parse().ok()
}

impl BrokerCore {
    pub fn new(slice_map: SliceMap, cfg: BrokerCoreConfig) -> Self {
        let lease_capacity = slice_map.total_bytes();
        Self {
            slice_map,
            arbiter: Arbiter::new(cfg.arbiter_cfg),
            endpoints: cfg.endpoints,
            swap_prio: cfg.swap_prio,
            lease_ttl: cfg.lease_ttl,
            tenants: BTreeMap::new(),
            sessions: HashMap::new(),
            name_to_id: HashMap::new(),
            next_tenant: 1,
            pending_dest: HashMap::new(),
            lease_book: LeaseBook::new(lease_capacity),
            last_rebalance: None,
            slice_io: cfg.slice_io,
            vram: cfg.vram,
            demotes_total: 0,
            last_demote_reason: None,
            demotes_at_last_sample: 0,
            recon_flag: ReconcileFlag::None,
            recon_count: 0,
            tol_frac: cfg.tol_frac,
            recon_streak: cfg.recon_streak,
            pending_zero: HashMap::new(),
            swap_ack: HashMap::new(),
        }
    }

    pub fn handle(&mut self, ev: CoreEvent, now: Instant) -> Vec<Outbound> {
        let mut out = Vec::new();
        match ev {
            CoreEvent::Msg(sid, msg) => self.on_msg(sid, msg, now, &mut out),
            CoreEvent::Disconnected(sid) => self.on_disconnect(sid, &mut out),
            CoreEvent::ZeroDone(slice, ok) => self.on_zero_done(slice, ok, &mut out),
            CoreEvent::Demote(reason) => self.on_demote(&reason, &mut out),
            CoreEvent::Tick => self.on_tick(now, &mut out),
        }
        out
    }

    fn endpoint_for(&self, transport: TransportKind) -> Option<NbdEndpoint> {
        match transport {
            TransportKind::NbdUnix => self
                .endpoints
                .nbd_unix
                .clone()
                .map(|path| NbdEndpoint::Unix { path }),
            TransportKind::NbdTcp => self
                .endpoints
                .nbd_tcp
                .clone()
                .map(|(host, port)| NbdEndpoint::Tcp { host, port }),
            // WinDrive is lease-only (SPEC windows-swap-driver DT-7); no NBD endpoint.
            TransportKind::WinDrive | TransportKind::DccAgent => None,
        }
    }

    /// Sends `msg` to the tenant's live session (or logs if absent — DT-20).
    fn to_tenant(&self, tenant: TenantId, msg: Msg, out: &mut Vec<Outbound>) {
        match self.tenants.get(&tenant).and_then(|t| t.sid) {
            Some(sid) => out.push(Outbound::ToSession(sid, msg)),
            None => out.push(Outbound::Log(format!(
                "[ramsharedd] WARN tenant {tenant} ausente; msg descartada"
            ))),
        }
    }

    fn slices_of(&self, tenant: TenantId) -> Vec<SliceId> {
        self.slice_map
            .slices()
            .iter()
            .filter(|s| s.tenant == Some(tenant))
            .map(|s| s.id)
            .collect()
    }

    fn on_msg(&mut self, sid: usize, msg: Msg, now: Instant, out: &mut Vec<Outbound>) {
        match msg {
            Msg::Register {
                proto,
                tenant,
                transport,
            } => self.on_register(sid, proto, tenant, transport, out),
            Msg::Psi { sample, swaps, mem } => self.on_psi(sid, sample, swaps, mem, now, out),
            Msg::SwapOnDone { slice, ok, detail } => {
                self.on_swap_on_done(sid, slice, ok, detail, out)
            }
            Msg::SwapOffDone { slice, ok, detail } => {
                self.on_swap_off_done(sid, slice, ok, detail, out)
            }
            Msg::Status => out.push(Outbound::ToSession(sid, self.status_reply())),
            Msg::LeaseRequest { bytes } => self.on_lease_request(sid, bytes, out),
            Msg::LeaseRelease { lease } => self.on_lease_release(sid, lease, out),
            other => {
                // Broker→agent messages never arrive here; unexpected shape → close.
                out.push(Outbound::ToSession(
                    sid,
                    Msg::Error {
                        reason: format!("mensagem inesperada do agente: {other:?}"),
                    },
                ));
                out.push(Outbound::CloseSession(sid));
            }
        }
    }

    /// Authorizes a swap acknowledgement before any effect (AUD-01).
    ///
    /// The contract is identity, ownership and exactly one outstanding
    /// operation: the session must be registered, the slice must belong to that
    /// tenant, and the operation being acknowledged must be the one this broker
    /// dispatched to **that** session. Anything else is refused without
    /// touching the slice. The only exception is a re-send of a completion that
    /// is already in force, which is idempotent and must not
    /// close a well-behaved owner — in particular a late `SwapOnDone { ok: false }`
    /// after a successful mount is **not** such a re-send and is refused.
    fn authorize_swap_ack(
        &self,
        sid: usize,
        slice: SliceId,
        dir: SwapAckDir,
        ok: bool,
    ) -> SwapAckVerdict {
        let Some(&tenant) = self.sessions.get(&sid) else {
            return SwapAckVerdict::Unauthorized("ack_swap_before_register");
        };
        let Some(s) = self.slice_map.get(slice) else {
            return SwapAckVerdict::Unauthorized("ack_swap_unknown_slice");
        };
        if s.tenant != Some(tenant) {
            return SwapAckVerdict::Unauthorized("ack_swap_foreign_slice");
        }
        match self.swap_ack.get(&slice) {
            Some(armed) if armed.dir == dir && armed.tenant == tenant && armed.sid == sid => {
                SwapAckVerdict::Armed
            }
            Some(_) => SwapAckVerdict::Unauthorized("ack_swap_wrong_operation"),
            None => {
                // No armed work. A completed successful acknowledgement may be
                // re-sent; a failure cannot, because applying it would undo a
                // transition that already happened.
                let settled = ok
                    && match dir {
                        SwapAckDir::On => s.state == SliceState::Active,
                        SwapAckDir::Off => {
                            s.state == SliceState::Draining
                                && self.pending_zero.contains_key(&slice)
                        }
                    };
                if settled {
                    SwapAckVerdict::Duplicate
                } else {
                    SwapAckVerdict::Unauthorized("ack_swap_unsolicited")
                }
            }
        }
    }

    fn refuse_swap_ack(sid: usize, reason: &'static str, slice: SliceId, out: &mut Vec<Outbound>) {
        out.push(Outbound::Log(format!(
            "[ramsharedd] ERROR {reason} sid={sid} slice=s{slice}; session closed"
        )));
        out.push(Outbound::CloseSession(sid));
    }

    fn on_swap_on_done(
        &mut self,
        sid: usize,
        slice: SliceId,
        ok: bool,
        detail: String,
        out: &mut Vec<Outbound>,
    ) {
        match self.authorize_swap_ack(sid, slice, SwapAckDir::On, ok) {
            SwapAckVerdict::Unauthorized(reason) => {
                Self::refuse_swap_ack(sid, reason, slice, out);
                return;
            }
            SwapAckVerdict::Duplicate => {
                out.push(Outbound::Log(format!(
                    "[ramsharedd] swapon ok slice=s{slice} (duplicate; ignored)"
                )));
                return;
            }
            SwapAckVerdict::Armed => {}
        }
        self.swap_ack.remove(&slice);
        if ok {
            out.push(Outbound::Log(format!(
                "[ramsharedd] swapon ok slice=s{slice}"
            )));
        } else {
            // Mount failed: nothing was written, so the slice returns directly
            // without a zero pass.
            let _ = self
                .slice_map
                .drain(slice)
                .and_then(|()| self.slice_map.release(slice));
            self.pending_dest.remove(&slice);
            out.push(Outbound::Log(format!(
                "[ramsharedd] swapon failed slice=s{slice} ({detail}); slice released"
            )));
        }
    }

    fn on_swap_off_done(
        &mut self,
        sid: usize,
        slice: SliceId,
        ok: bool,
        detail: String,
        out: &mut Vec<Outbound>,
    ) {
        match self.authorize_swap_ack(sid, slice, SwapAckDir::Off, ok) {
            SwapAckVerdict::Unauthorized(reason) => {
                Self::refuse_swap_ack(sid, reason, slice, out);
                return;
            }
            SwapAckVerdict::Duplicate => {
                out.push(Outbound::Log(format!(
                    "[ramsharedd] swapoff ok slice=s{slice} (duplicate; ignored)"
                )));
                return;
            }
            SwapAckVerdict::Armed => {}
        }
        if !ok {
            // The swapoff is still outstanding: keep it armed so the owner can
            // retry, and so a later success can complete it. The slice stays
            // Draining.
            out.push(Outbound::Log(format!(
                "[ramsharedd] swapoff failed slice=s{slice} ({detail}); stays Draining"
            )));
            return;
        }
        // Hygiene (DT-17): zeroes before release. ZeroDone → release. The
        // tenant already did swapoff → safe to zero; enters pending_zero for
        // the tick retry (R4).
        self.swap_ack.remove(&slice);
        let Some(s) = self.slice_map.get(slice) else {
            return;
        };
        let (base, len) = (s.offset, s.len);
        self.pending_zero.entry(slice).or_insert(0);
        out.push(Outbound::ZeroSlice { slice, base, len });
    }

    fn on_register(
        &mut self,
        sid: usize,
        proto: u32,
        name: String,
        transport: TransportKind,
        out: &mut Vec<Outbound>,
    ) {
        if proto != PROTO_VERSION {
            out.push(Outbound::ToSession(
                sid,
                Msg::Error {
                    reason: format!("proto {proto} != {PROTO_VERSION}"),
                },
            ));
            out.push(Outbound::CloseSession(sid));
            return;
        }
        // AUD-02: a session registers exactly once. A second Register would
        // re-bind the same TCP peer to a different tenant name and hand it that
        // identity's slices, so it is refused and the session is closed.
        if self.sessions.contains_key(&sid) {
            out.push(Outbound::ToSession(
                sid,
                Msg::Error {
                    reason: "sessao_ja_registrada".into(),
                },
            ));
            out.push(Outbound::CloseSession(sid));
            return;
        }
        // AUD-02: the name is an identity label, not a data field. An empty
        // name would alias every such peer onto one tenant, and an unbounded
        // name is an allocation path through the identity check.
        if name.is_empty() || name.len() > MAX_TENANT_NAME_BYTES {
            out.push(Outbound::ToSession(
                sid,
                Msg::Error {
                    reason: format!(
                        "nome_de_tenant_invalido len={} max={MAX_TENANT_NAME_BYTES}",
                        name.len()
                    ),
                },
            ));
            out.push(Outbound::CloseSession(sid));
            return;
        }
        // AUD-02: distinct names must not grow the tenant table without limit.
        if !self.name_to_id.contains_key(&name) && self.name_to_id.len() >= MAX_TENANTS {
            out.push(Outbound::ToSession(
                sid,
                Msg::Error {
                    reason: format!("tenants_esgotados max={MAX_TENANTS}"),
                },
            ));
            out.push(Outbound::CloseSession(sid));
            return;
        }
        let id = *self
            .name_to_id
            .entry(name.clone())
            .or_insert(self.next_tenant);
        if id == self.next_tenant {
            self.next_tenant += 1;
        }
        // DT-22: tenant already has a different live session → duplicate.
        if let Some(t) = self.tenants.get(&id)
            && t.present
            && t.sid.is_some()
            && t.sid != Some(sid)
        {
            out.push(Outbound::ToSession(
                sid,
                Msg::Error {
                    reason: "tenant_duplicado".into(),
                },
            ));
            out.push(Outbound::CloseSession(sid));
            return;
        }
        self.tenants.insert(
            id,
            TenantState {
                name: name.clone(),
                transport,
                present: true,
                sid: Some(sid),
                // A reconnecting tenant keeps the sample it actually sent.
                // A brand-new one has none: `None`, not a fabricated idle.
                psi: self.tenants.get(&id).and_then(|t| t.psi),
                reconciled: false,
                mem: None,
                occupied_bytes: 0,
            },
        );
        self.sessions.insert(sid, id);
        out.push(Outbound::Log(format!(
            "[ramsharedd] tenant registrado name={name} id={id} transport={transport:?}"
        )));
        out.push(Outbound::ToSession(sid, Msg::Registered { tenant_id: id }));
    }

    fn on_psi(
        &mut self,
        sid: usize,
        sample: PsiSample,
        swaps: Vec<SwapEntry>,
        mem: Option<TenantMem>,
        now: Instant,
        out: &mut Vec<Outbound>,
    ) {
        let Some(&id) = self.sessions.get(&sid) else {
            out.push(Outbound::ToSession(
                sid,
                Msg::Error {
                    reason: "psi antes de register".into(),
                },
            ));
            out.push(Outbound::CloseSession(sid));
            return;
        };
        if let Some(t) = self.tenants.get_mut(&id) {
            t.psi = Some(sample);
            t.mem = mem;
            // Reconciliation (DT-9/21): on the 1st Psi, re-adopts slices that the agent already has mounted.
            if !t.reconciled {
                t.reconciled = true;
                for e in &swaps {
                    if let Some(slice) = dev_to_slice(&e.dev)
                        && self
                            .slice_map
                            .get(slice)
                            .is_some_and(|s| s.state == SliceState::Free)
                        && self.slice_map.assign(slice, id).is_ok()
                    {
                        out.push(Outbound::Log(format!(
                            "[ramsharedd] reconciliado slice=s{slice} -> tenant {id} (já montada)"
                        )));
                    }
                }
            }
        }
        // occupied (DT-10/F-v2-2): Σ used_kb*1024 of swaps that match Active slices of this tenant.
        let occupied: u64 = swaps
            .iter()
            .filter_map(|e| dev_to_slice(&e.dev).map(|sl| (sl, e.used_kb)))
            .filter(|(sl, _)| {
                self.slice_map
                    .get(*sl)
                    .is_some_and(|s| s.state == SliceState::Active && s.tenant == Some(id))
            })
            .map(|(_, kb)| kb.saturating_mul(1024))
            .fold(0_u64, u64::saturating_add);
        if let Some(t) = self.tenants.get_mut(&id) {
            t.occupied_bytes = occupied;
        }
        self.lease_book.renew_active(id, now, self.lease_ttl);
        out.push(Outbound::ToSession(sid, Msg::Ack)); // DT-18: heartbeat
    }
}

#[cfg(test)]
mod tests;
