// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/broker_srv/reclaim_lease.rs
//! Lease request/release, zeroing retries, rebalance ticks, and telemetry projection for [`BrokerCore`].

use std::sync::atomic::Ordering;
use std::time::Instant;

use ramshared_broker::arbiter::{Action, TenantView};
use ramshared_broker::lease::{LeaseDecision, LeaseDeny};
use ramshared_broker::model::{Slice, SliceId, SliceState, TenantId, TransportKind};
use ramshared_broker::protocol::{Msg, SliceIo, TenantStatus};

use super::{BrokerCore, Outbound, SwapAckDir, SwapAckState, ZERO_RETRY_ERROR, ZERO_RETRY_GRACE};
use crate::canary_probe::CANARY_BYTES;
use crate::telemetry::{ReconcileFlag, ReconcileInput, TelemetryCore, reconcile, vram_outros};

impl BrokerCore {
    pub(super) fn on_lease_request(&mut self, sid: usize, bytes: u64, out: &mut Vec<Outbound>) {
        let Some(&holder) = self.sessions.get(&sid) else {
            out.push(Outbound::ToSession(
                sid,
                Msg::Error {
                    reason: "lease antes de register".into(),
                },
            ));
            out.push(Outbound::CloseSession(sid));
            return;
        };
        match self.lease_book.begin_request(holder, bytes) {
            LeaseDecision::Pending(_) => out.push(Outbound::Log(format!(
                "[ramsharedd] lease pedido holder={holder} bytes={bytes} (grant no próximo tick)"
            ))),
            LeaseDecision::Denied(reason) => out.push(Outbound::ToSession(
                sid,
                Msg::LeaseDenied {
                    reason: match reason {
                        LeaseDeny::ZeroBytes => "bytes_zero",
                        LeaseDeny::OverCapacity => "acima_da_capacidade",
                        _ => "lease_em_andamento",
                    }
                    .into(),
                },
            )),
        }
    }

    pub(super) fn on_lease_release(&mut self, sid: usize, lease_id: u32, out: &mut Vec<Outbound>) {
        let Some(&holder) = self.sessions.get(&sid) else {
            out.push(Outbound::ToSession(
                sid,
                Msg::Error {
                    reason: "lease release antes de register".into(),
                },
            ));
            out.push(Outbound::CloseSession(sid));
            return;
        };
        match self.lease_book.release(holder, lease_id) {
            Ok(true) => {}
            Ok(false) | Err(LeaseDeny::WrongLease) => {
                // Already released. A release is a retryable command, so the
                // idempotent case is a success the client is still waiting on
                // (invariant checkAUD-08): silence here hung the host bridge.
                out.push(Outbound::Log(format!(
                    "[ramsharedd] lease {lease_id} release already settled holder={holder}"
                )));
                out.push(Outbound::ToSession(sid, Msg::Ack));
                return;
            }
            Err(LeaseDeny::WrongHolder) => {
                out.push(Outbound::ToSession(
                    sid,
                    Msg::Error {
                        reason: "holder de lease incorreto".into(),
                    },
                ));
                out.push(Outbound::CloseSession(sid));
                return;
            }
            Err(_) => return,
        }
        let leased: Vec<SliceId> = self
            .slice_map
            .slices()
            .iter()
            .filter(|s| s.state == SliceState::Leased)
            .map(|s| s.id)
            .collect();
        for slice in leased {
            let _ = self.slice_map.unlease(slice); // Leased → Free (round-robin re-leases)
        }
        out.push(Outbound::Log(format!(
            "[ramsharedd] lease {lease_id} liberado; slices devolvidas ao tier de swap"
        )));
        // A release is a request: it has to be answered. `Ack` is the settled
        // reply the host bridge turns into `Released { lease }` (AUD-08).
        out.push(Outbound::ToSession(sid, Msg::Ack));
    }

    pub(super) fn on_disconnect(&mut self, sid: usize, out: &mut Vec<Outbound>) {
        if let Some(id) = self.sessions.remove(&sid) {
            // A dropped connection is a dropped generation: work armed for it
            // can never be acknowledged by that session again (AUD-01).
            self.swap_ack.retain(|_, armed| armed.sid != sid);
            if let Some(t) = self.tenants.get_mut(&id) {
                t.present = false;
                t.sid = None;
            }
            out.push(Outbound::Log(format!(
                "[ramsharedd] tenant {id} desconectou; slices congeladas (DT-20)"
            )));
            // A requester is cancelled. An active lease remains reserved until
            // its monotonic deadline, so a transient disconnect cannot cause
            // premature reuse of the slices.
            let disconnected = self.lease_book.disconnect(id);
            if let Some(lease) = disconnected.retained_active {
                out.push(Outbound::Log(format!(
                    "[ramsharedd] lease {} retained after holder disconnect until expiry",
                    lease.id
                )));
            }
        }
    }

    pub(super) fn release_slices(&mut self, lease_id: u32, out: &mut Vec<Outbound>) {
        let leased: Vec<SliceId> = self
            .slice_map
            .slices()
            .iter()
            .filter(|s| s.state == SliceState::Leased)
            .map(|s| s.id)
            .collect();
        for slice in leased {
            let _ = self.slice_map.unlease(slice);
        }
        out.push(Outbound::Log(format!(
            "[ramsharedd] lease {lease_id} liberado; slices devolvidas ao tier de swap"
        )));
    }

    pub(super) fn on_zero_done(&mut self, slice: SliceId, ok: bool, out: &mut Vec<Outbound>) {
        if !ok {
            // retry zeroing on the next movement event; for now log and remain Draining (R4).
            out.push(Outbound::Log(format!(
                "[ramsharedd] zero FALHOU slice=s{slice}; fica Draining (retry)"
            )));
            if let Some(s) = self.slice_map.get(slice) {
                out.push(Outbound::ZeroSlice {
                    slice,
                    base: s.offset,
                    len: s.len,
                });
            }
            return;
        }
        self.pending_zero.remove(&slice); // confirmed zero ends the retry (R4)
        self.swap_ack.remove(&slice); // the zero moves the lifecycle on (AUD-01)
        if self.slice_map.release(slice).is_err() {
            return; // was not Draining; ignore
        }
        // Movement in flight: the cleaned slice goes to the destination (SwapOn).
        if let Some(dest) = self.pending_dest.remove(&slice)
            && self.slice_map.assign(slice, dest).is_ok()
        {
            self.emit_swapon(slice, dest, out);
        }
    }

    pub(super) fn on_demote(&mut self, reason: &str, out: &mut Vec<Outbound>) {
        self.demotes_total = self.demotes_total.saturating_add(1);
        self.last_demote_reason = Some(reason.to_string());
        out.push(Outbound::Log(format!(
            "[ramsharedd] DemoteAll reason={reason}"
        )));
        let present: Vec<TenantId> = self
            .tenants
            .iter()
            .filter(|(_, t)| t.present)
            .map(|(id, _)| *id)
            .collect();
        for id in present {
            self.to_tenant(id, Msg::DemoteAll, out);
        }
    }

    /// Dispatches `SwapOn` and arms the acknowledgement for the exact session
    /// that received it (AUD-01). Nothing is armed when nothing is delivered.
    pub(super) fn emit_swapon(
        &mut self,
        slice: SliceId,
        tenant: TenantId,
        out: &mut Vec<Outbound>,
    ) {
        let transport = match self.tenants.get(&tenant) {
            Some(t) => t.transport,
            None => return,
        };
        let Some(endpoint) = self.endpoint_for(transport) else {
            out.push(Outbound::Log(format!(
                "[ramsharedd] ERRO transporte indisponível p/ tenant {tenant}"
            )));
            return;
        };
        let Some(sid) = self.tenants.get(&tenant).and_then(|t| t.sid) else {
            out.push(Outbound::Log(format!(
                "[ramsharedd] WARN tenant {tenant} ausente; msg descartada"
            )));
            return;
        };
        self.swap_ack.insert(
            slice,
            SwapAckState {
                dir: SwapAckDir::On,
                tenant,
                sid,
            },
        );
        out.push(Outbound::ToSession(
            sid,
            Msg::SwapOn {
                slice,
                export: format!("s{slice}"),
                endpoint,
                swap_prio: self.swap_prio,
            },
        ));
    }

    /// Dispatches `SwapOff` and arms the acknowledgement for the exact session
    /// that received it (AUD-01). Nothing is armed when nothing is delivered.
    pub(super) fn emit_swapoff(
        &mut self,
        slice: SliceId,
        tenant: TenantId,
        out: &mut Vec<Outbound>,
    ) {
        let Some(sid) = self.tenants.get(&tenant).and_then(|t| t.sid) else {
            out.push(Outbound::Log(format!(
                "[ramsharedd] WARN tenant {tenant} ausente; msg descartada"
            )));
            return;
        };
        self.swap_ack.insert(
            slice,
            SwapAckState {
                dir: SwapAckDir::Off,
                tenant,
                sid,
            },
        );
        out.push(Outbound::ToSession(sid, Msg::SwapOff { slice }));
    }

    pub(super) fn status_reply(&self) -> Msg {
        let tenants = self
            .tenants
            .iter()
            .map(|(id, t)| TenantStatus {
                id: *id,
                name: t.name.clone(),
                psi: t.psi,
                slices: self.slices_of(*id),
                present: t.present,
                bytes_served: self
                    .slices_of(*id)
                    .iter()
                    .filter_map(|s| self.slice_io.get(*s as usize))
                    .map(|c| c.bytes_served.load(Ordering::Relaxed))
                    .fold(0_u64, u64::saturating_add),
            })
            .collect();
        let slice_io = self
            .slice_map
            .slices()
            .iter()
            .filter_map(|s| {
                self.slice_io.get(s.id as usize).map(|c| SliceIo {
                    id: s.id,
                    bytes_served: c.bytes_served.load(Ordering::Relaxed),
                    io_count: c.io_count.load(Ordering::Relaxed),
                })
            })
            .collect();
        Msg::StatusReply {
            tenants,
            slices: self.slice_map.slices().to_vec(),
            slice_io,
            last_rebalance_secs: None,
        }
    }

    pub(super) fn on_tick(&mut self, now: Instant, out: &mut Vec<Outbound>) {
        if let Some(lease) = self.lease_book.expire(now) {
            out.push(Outbound::Log(format!(
                "[ramsharedd] lease {} expired; returning slices to the allocator",
                lease.id
            )));
            self.release_slices(lease.id, out);
        }
        self.tick_arbiter(now, out);
        self.retry_stuck_zeroing(out);
        self.emit_telemetry(out);
    }

    pub(super) fn tick_arbiter(&mut self, now: Instant, out: &mut Vec<Outbound>) {
        // DT-20: only present tenants; only Free slices or slices with a present owner.
        // WinDrive / lease-only transports are excluded from swap round-robin (DT-7).
        let present: Vec<TenantView> = self
            .tenants
            .iter()
            .filter(|(_, t)| {
                t.present
                    && !matches!(
                        t.transport,
                        TransportKind::WinDrive | TransportKind::DccAgent
                    )
            })
            .map(|(id, t)| TenantView {
                id: *id,
                psi: t.psi,
                slices: self.slices_of(*id).len() as u16,
            })
            .collect();
        let visible: Vec<Slice> = self
            .slice_map
            .slices()
            .iter()
            .filter(|s| {
                s.tenant.is_none()
                    || s.tenant
                        .is_some_and(|t| self.tenants.get(&t).is_some_and(|ts| ts.present))
            })
            .cloned()
            .collect();

        let pending_lease = self
            .lease_book
            .pending()
            .map(|pending| (pending.holder, pending.requested_bytes));
        let actions = match self.arbiter.tick(now, &present, &visible, pending_lease) {
            Ok(acts) => acts,
            Err(e) => {
                out.push(Outbound::Log(format!(
                    "[ramsharedd] arbiter tick error: {e}"
                )));
                Vec::new()
            }
        };
        for action in actions {
            match action {
                Action::AssignFree { slice, to } => {
                    if self.slice_map.assign(slice, to).is_ok() {
                        out.push(Outbound::Log(format!(
                            "[ramsharedd] arbiter assign slice=s{slice} to={to}"
                        )));
                        self.emit_swapon(slice, to, out);
                    }
                }
                Action::MoveSlice { slice, from, to } | Action::RevertMove { slice, from, to } => {
                    if self.slice_map.drain(slice).is_ok() {
                        self.pending_dest.insert(slice, to);
                        self.last_rebalance = Some(now);
                        // A pressure figure is named only when one was measured.
                        // `psi10=0.0` would claim an idle nobody observed.
                        let pf = present
                            .iter()
                            .find(|t| t.id == from)
                            .and_then(|t| t.psi)
                            .map_or_else(
                                || "unmeasured".to_string(),
                                |s| format!("{:.1}", s.avg10),
                            );
                        let pt = present
                            .iter()
                            .find(|t| t.id == to)
                            .and_then(|t| t.psi)
                            .map_or_else(
                                || "unmeasured".to_string(),
                                |s| format!("{:.1}", s.avg10),
                            );
                        out.push(Outbound::Log(format!(
                            "[ramsharedd] arbiter move slice=s{slice} from={from}(psi10={pf}) to={to}(psi10={pt})"
                        )));
                        self.emit_swapoff(slice, from, out);
                    }
                }
                Action::RevokeForLease { slice, from, .. } => {
                    // Revokes for the lease: drain+SwapOff; on ZeroDone becomes Free (no pending_dest),
                    // and the arbiter counts it for the lease on the next tick (DT-8/R2).
                    if self.slice_map.drain(slice).is_ok() {
                        out.push(Outbound::Log(format!(
                            "[ramsharedd] revoke-for-lease slice=s{slice} from={from}"
                        )));
                        self.emit_swapoff(slice, from, out);
                    }
                }
                Action::GrantLease { holder, slices } => {
                    if self.lease_ttl.is_zero() || now.checked_add(self.lease_ttl).is_none() {
                        let _ = self.lease_book.cancel_pending(holder);
                        out.push(Outbound::Log(
                            "[ramsharedd] refusing lease grant with invalid lease TTL".into(),
                        ));
                        self.to_tenant(
                            holder,
                            Msg::LeaseDenied {
                                reason: "lease_ttl_invalid".into(),
                            },
                            out,
                        );
                        continue;
                    }

                    let Some(granted) = slices.iter().try_fold(0_u64, |total, slice| {
                        self.slice_map
                            .get(*slice)
                            .filter(|current| current.state == SliceState::Free)
                            .and_then(|current| total.checked_add(current.len))
                    }) else {
                        out.push(Outbound::Log(
                            "[ramsharedd] refusing lease grant with unavailable slices".into(),
                        ));
                        continue;
                    };

                    let Ok(lease) = self.lease_book.grant_pending(granted, now, self.lease_ttl)
                    else {
                        out.push(Outbound::Log(
                            "[ramsharedd] lease grant diverged from LeaseBook".into(),
                        ));
                        continue;
                    };

                    let mut leased = Vec::with_capacity(slices.len());
                    let mut transition_failed = false;
                    for slice in &slices {
                        if self.slice_map.lease(*slice).is_ok() {
                            leased.push(*slice);
                        } else {
                            transition_failed = true;
                            break;
                        }
                    }
                    if transition_failed {
                        for leased_slice in leased {
                            let _ = self.slice_map.unlease(leased_slice);
                        }
                        let _ = self.lease_book.release(holder, lease.id);
                        out.push(Outbound::Log(
                            "[ramsharedd] rolled back incomplete lease slice transition".into(),
                        ));
                        continue;
                    }
                    out.push(Outbound::Log(format!(
                        "[ramsharedd] lease {} concedido holder={holder} slices={slices:?}",
                        lease.id
                    )));
                    self.to_tenant(
                        holder,
                        Msg::LeaseGranted {
                            lease: lease.id,
                            bytes: granted,
                        },
                        out,
                    );
                }
            }
        }
    }

    pub(super) fn retry_stuck_zeroing(&mut self, out: &mut Vec<Outbound>) {
        // R4: retries zeroing of stuck slices (if `try_send(ZeroExport)` failed, `ZeroDone` doesn't arrive).
        // Grace period of 1 tick for zero in flight; above that, re-emits; ERROR after N
        // ticks without confirmation. Only touches slices in `pending_zero` (already swapped-off; safe to re-zero).
        let stuck: Vec<SliceId> = self.pending_zero.keys().copied().collect();
        for slice in stuck {
            let count = match self.pending_zero.get_mut(&slice) {
                Some(n) => {
                    *n += 1;
                    *n
                }
                None => continue,
            };
            if count == ZERO_RETRY_ERROR {
                out.push(Outbound::Log(format!(
                    "[ramsharedd] ERRO: zero de s{slice} sem confirmar em {ZERO_RETRY_ERROR} ticks (slice presa em Draining, R4); re-tentando"
                )));
            }
            if count > ZERO_RETRY_GRACE
                && let Some(s) = self.slice_map.get(slice)
            {
                let (base, len) = (s.offset, s.len);
                out.push(Outbound::ZeroSlice { slice, base, len });
            }
        }
    }

    /// Reconciliation per tick (RF-4/RF-5): occupancy invariant + hysteresis (DT-12) → emits
    /// `Outbound::Telemetry`. Observer: does not touch the arbiter/SliceMap.
    pub(super) fn emit_telemetry(&mut self, out: &mut Vec<Outbound>) {
        let alloc_active: u64 = self
            .slice_map
            .slices()
            .iter()
            .filter(|s| matches!(s.state, SliceState::Active | SliceState::Draining))
            .map(|s| s.len)
            .fold(0_u64, u64::saturating_add);
        let occupied: u64 = self
            .tenants
            .values()
            .map(|t| t.occupied_bytes)
            .fold(0_u64, u64::saturating_add);
        // Σ diskstats (cumulative in bytes) of tenants reporting `mem` (RF-2); `None` if nobody
        // reports. The consumer derives the rate by the difference between samples (`t` field of the line).
        let page_io: Option<u64> = self.tenants.values().any(|t| t.mem.is_some()).then(|| {
            self.tenants
                .values()
                .filter_map(|t| t.mem.as_ref())
                .map(|m| m.diskstats_io)
                .fold(0_u64, u64::saturating_add)
        });
        let free = self.vram.free.load(Ordering::Relaxed);
        let total = self.vram.total.load(Ordering::Relaxed);
        let has_vram = total > 0; // F-v2-6: RAM sentinel (no GPU) → vram_* = None
        let vram_alloc_daemon = alloc_active.saturating_add(CANARY_BYTES as u64);
        let vram_total_used = has_vram.then(|| total.saturating_sub(free));
        let vram_others = vram_total_used.map(|u| vram_outros(u, vram_alloc_daemon));
        let demotes_delta = self
            .demotes_total
            .saturating_sub(self.demotes_at_last_sample);
        self.demotes_at_last_sample = self.demotes_total;
        let stuck_draining = self.pending_zero.values().any(|n| *n >= ZERO_RETRY_ERROR);
        let any_present = self.tenants.values().any(|t| t.present);
        let inp = ReconcileInput {
            alloc_active_bytes: alloc_active,
            occupied_swap_bytes: occupied,
            stuck_draining,
            demotes_delta,
            any_source_missing: !has_vram || !any_present,
        };
        let (delta, candidate) = reconcile(&inp, self.tol_frac);
        // Hysteresis (DT-12) for SUSTAINED flags (Unaccounted/StuckSlice/Partial): confirms after
        // `recon_streak` consecutive identical ticks. `Eviction` is a canary EVENT (DT-6;
        // `demotes_delta` is per-tick, lasts 1 tick) → immediate confirmation; otherwise hysteresis would swallow
        // a transient eviction (1-2 DEMOTEs) and the eviction signal would never appear (bug C1).
        if candidate == self.recon_flag {
            self.recon_count = self.recon_count.saturating_add(1);
        } else {
            self.recon_flag = candidate;
            self.recon_count = 1;
        }
        let confirmed = match candidate {
            ReconcileFlag::Eviction => ReconcileFlag::Eviction, // evento: sem histerese
            ReconcileFlag::None => ReconcileFlag::None,
            sustained if self.recon_count >= self.recon_streak => sustained,
            _ => ReconcileFlag::None,
        };
        if confirmed != ReconcileFlag::None {
            out.push(Outbound::Log(format!(
                "[ramsharedd] reconcile flag={confirmed:?} delta={delta:.3} reason={:?}",
                self.last_demote_reason
            )));
        }
        out.push(Outbound::Telemetry(TelemetryCore {
            tenant: None,
            slice: None,
            swap_used: occupied,
            alloc_active,
            page_io_s: page_io,
            vram_alloc_daemon,
            vram_total_used,
            vram_outros: vram_others,
            canario_demotes: self.demotes_total,
            demote_reason: self.last_demote_reason.clone(),
            reconcile_delta: delta,
            flag: confirmed,
        }));
    }
}
