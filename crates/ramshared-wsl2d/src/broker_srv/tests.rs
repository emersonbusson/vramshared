// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/broker_srv/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
pub(super) use super::transport::TelemetrySink;
use super::*;
pub(super) use ramshared_broker::model::SliceState;
pub(super) use std::path::PathBuf;
pub(super) use std::sync::atomic::Ordering;
pub(super) use std::sync::mpsc;

pub(super) fn core(k: u16) -> BrokerCore {
    core_streak(k, 1)
}

/// Like `core`, but with configurable `recon_streak` (tests the actual production hysteresis).
pub(super) fn core_streak(k: u16, recon_streak: u32) -> BrokerCore {
    core_with_lease_ttl(k, recon_streak, Duration::from_secs(30))
}

pub(super) fn core_with_lease_ttl(k: u16, recon_streak: u32, lease_ttl: Duration) -> BrokerCore {
    let cfg = ArbiterConfig {
        streak: 1, // move already on the 1st tick above delta (tests)
        ..ArbiterConfig::default()
    };
    BrokerCore::new(
        SliceMap::new(k, 64 * 1024 * 1024, u64::from(k) * 64 * 1024 * 1024).unwrap(),
        BrokerCoreConfig {
            arbiter_cfg: cfg,
            endpoints: EndpointCfg {
                nbd_unix: Some("/run/x.sock".into()),
                nbd_tcp: None,
            },
            swap_prio: None,
            slice_io: Arc::new((0..k).map(|_| SliceIoCounters::default()).collect()),
            vram: Arc::new(VramGauge::default()),
            tol_frac: 0.10,
            recon_streak,
            lease_ttl,
        },
    )
}

pub(super) fn reg(c: &mut BrokerCore, sid: usize, name: &str) -> Vec<Outbound> {
    c.handle(
        CoreEvent::Msg(
            sid,
            Msg::Register {
                proto: PROTO_VERSION,
                tenant: name.into(),
                transport: TransportKind::NbdUnix,
            },
        ),
        Instant::now(),
    )
}

pub(super) fn psi(c: &mut BrokerCore, sid: usize, avg10: f32) -> Vec<Outbound> {
    c.handle(
        CoreEvent::Msg(
            sid,
            Msg::Psi {
                sample: PsiSample {
                    avg10,
                    avg60: avg10,
                    stall_us: 0,
                },
                swaps: vec![],
                mem: None,
            },
        ),
        Instant::now(),
    )
}

/// Arms the outstanding `SwapOff` exactly as a move does: the slice goes to
/// `Draining` and the command is delivered to the owner's live session.
///
/// Tests must go through this rather than injecting `SwapOffDone` into a
/// slice that was never asked to swap off — that injection is the defect
/// AUD-01 closes.
pub(super) fn arm_swap_off(c: &mut BrokerCore, slice: SliceId, owner: TenantId) -> Vec<Outbound> {
    c.slice_map.drain(slice).unwrap();
    let mut out = Vec::new();
    c.emit_swapoff(slice, owner, &mut out);
    out
}

#[test]
fn register_assigns_stable_id_and_acks_psi() {
    let mut c = core(2);
    let o = reg(&mut c, 10, "wsl2");
    assert!(o.contains(&Outbound::ToSession(10, Msg::Registered { tenant_id: 1 })));
    let o = psi(&mut c, 10, 0.0);
    assert!(o.contains(&Outbound::ToSession(10, Msg::Ack)));
}

#[test]
fn duplicate_register_is_rejected() {
    let mut c = core(2);
    reg(&mut c, 10, "wsl2");
    let o = reg(&mut c, 11, "wsl2"); // same name, another live connection
    assert!(o.iter().any(|x| matches!(x, Outbound::CloseSession(11))));
    assert!(o.iter().any(
        |x| matches!(x, Outbound::ToSession(11, Msg::Error { reason }) if reason == "tenant_duplicado")
    ));
}

/// AUD-02: a second `Register` on the *same* connection must not re-bind
/// that peer to another tenant name. Before this, one session could take
/// an identity per message and reach whatever slices that identity owned.
#[test]
fn a_second_register_on_one_session_is_refused() {
    let mut c = core(2);
    let o = reg(&mut c, 10, "wsl2");
    assert!(o.contains(&Outbound::ToSession(10, Msg::Registered { tenant_id: 1 })));

    let o = reg(&mut c, 10, "outro-tenant"); // same session, different name
    assert!(o.iter().any(|x| matches!(x, Outbound::CloseSession(10))));
    assert!(o.iter().any(
        |x| matches!(x, Outbound::ToSession(10, Msg::Error { reason }) if reason == "sessao_ja_registrada")
    ));
    assert!(
        !c.sessions.contains_key(&10) || c.sessions.get(&10) == Some(&1),
        "the session must not be re-bound to the second name: {:?}",
        c.sessions
    );
}

/// AUD-02: the tenant name is an identity label. An empty name would alias
/// every such peer onto one tenant record.
#[test]
fn an_empty_tenant_name_is_refused() {
    let mut c = core(2);
    let o = reg(&mut c, 10, "");
    assert!(o.iter().any(|x| matches!(x, Outbound::CloseSession(10))));
    assert!(o.iter().any(
        |x| matches!(x, Outbound::ToSession(10, Msg::Error { reason }) if reason.starts_with("nome_de_tenant_invalido"))
    ));
    assert!(c.name_to_id.is_empty(), "no tenant record may be created");
}

/// AUD-02: an unbounded name is an allocation path through the identity
/// check; the label is capped at [`MAX_TENANT_NAME_BYTES`].
#[test]
fn an_over_long_tenant_name_is_refused() {
    let mut c = core(2);
    let long = "x".repeat(MAX_TENANT_NAME_BYTES + 1);
    let o = reg(&mut c, 10, &long);
    assert!(o.iter().any(|x| matches!(x, Outbound::CloseSession(10))));
    assert!(o.iter().any(
        |x| matches!(x, Outbound::ToSession(10, Msg::Error { reason }) if reason.starts_with("nome_de_tenant_invalido"))
    ));
    assert!(c.name_to_id.is_empty(), "no tenant record may be created");

    // Exactly at the cap is still accepted — the bound is inclusive.
    let exact = "y".repeat(MAX_TENANT_NAME_BYTES);
    let o = reg(&mut c, 11, &exact);
    assert!(o.contains(&Outbound::ToSession(11, Msg::Registered { tenant_id: 1 })));
}

/// AUD-02 required test: distinct tenant names must not grow the control
/// plane without limit. The table is live state, not a directory.
#[test]
fn distinct_tenant_names_are_bounded() {
    let mut c = core(2);
    for i in 0..MAX_TENANTS {
        let name = format!("t{i}");
        let o = reg(&mut c, i + 1, &name);
        assert!(
            o.contains(&Outbound::ToSession(
                i + 1,
                Msg::Registered {
                    tenant_id: (i + 1) as u32
                }
            )),
            "tenant {name} must register"
        );
    }
    assert_eq!(c.name_to_id.len(), MAX_TENANTS);

    let o = reg(&mut c, 999, "overflow");
    assert!(o.iter().any(|x| matches!(x, Outbound::CloseSession(999))));
    assert!(o.iter().any(
        |x| matches!(x, Outbound::ToSession(999, Msg::Error { reason }) if reason.starts_with("tenants_esgotados"))
    ));
    assert_eq!(
        c.name_to_id.len(),
        MAX_TENANTS,
        "the refused name must not be recorded"
    );
}

/// AUD-02 required test: bounding the table and refusing a second Register
/// must not break the legitimate path — a peer that disconnects and comes
/// back on a new connection under the same name is a reconnect, not an
/// impersonation attempt.
#[test]
fn a_reconnect_after_disconnect_registers_again() {
    let mut c = core(2);
    let o = reg(&mut c, 10, "wsl2");
    assert!(o.contains(&Outbound::ToSession(10, Msg::Registered { tenant_id: 1 })));

    c.handle(CoreEvent::Disconnected(10), Instant::now());

    let o = reg(&mut c, 11, "wsl2"); // new connection, same name
    assert!(
        o.contains(&Outbound::ToSession(11, Msg::Registered { tenant_id: 1 })),
        "a reconnect must keep its stable id: {o:?}"
    );
}

#[test]
fn proto_mismatch_rejected() {
    let mut c = core(1);
    let o = c.handle(
        CoreEvent::Msg(
            10,
            Msg::Register {
                proto: 999,
                tenant: "x".into(),
                transport: TransportKind::NbdUnix,
            },
        ),
        Instant::now(),
    );
    assert!(o.iter().any(|x| matches!(x, Outbound::CloseSession(10))));
}

#[test]
fn psi_before_register_closes() {
    let mut c = core(1);
    let o = psi(&mut c, 7, 0.0);
    assert!(o.iter().any(|x| matches!(x, Outbound::CloseSession(7))));
}

#[test]
fn tick_round_robin_assigns_free_slices() {
    let mut c = core(2);
    reg(&mut c, 10, "a");
    reg(&mut c, 20, "b");
    psi(&mut c, 10, 0.0);
    psi(&mut c, 20, 0.0);
    let o = c.handle(CoreEvent::Tick, Instant::now());
    // 2 Free slices → SwapOn for each tenant (round-robin: s0→a(1), s1→b(2))
    let swapons: Vec<_> = o
        .iter()
        .filter(|x| matches!(x, Outbound::ToSession(_, Msg::SwapOn { .. })))
        .collect();
    assert_eq!(swapons.len(), 2);
    assert_eq!(c.slice_map.get(0).unwrap().state, SliceState::Active);
    assert_eq!(c.slice_map.get(1).unwrap().state, SliceState::Active);
}

#[test]
fn broker_swapoff_done_rejects_foreign_or_stale_session() {
    let mut c = core(2);
    reg(&mut c, 10, "owner");
    reg(&mut c, 20, "foreign");
    psi(&mut c, 10, 0.0);
    c.handle(CoreEvent::Tick, Instant::now());
    let armed = arm_swap_off(&mut c, 0, 1);
    assert!(
        armed
            .iter()
            .any(|x| matches!(x, Outbound::ToSession(10, Msg::SwapOff { slice: 0 })))
    );
    let foreign = c.handle(
        CoreEvent::Msg(
            20,
            Msg::SwapOffDone {
                slice: 0,
                ok: true,
                detail: String::new(),
            },
        ),
        Instant::now(),
    );
    assert!(
        !foreign
            .iter()
            .any(|x| matches!(x, Outbound::ZeroSlice { .. }))
    );
    assert!(
        foreign
            .iter()
            .any(|x| matches!(x, Outbound::CloseSession(20)))
    );
    assert_eq!(c.slice_map.get(0).unwrap().state, SliceState::Draining);

    c.handle(CoreEvent::Disconnected(10), Instant::now());
    reg(&mut c, 30, "owner");
    let stale = c.handle(
        CoreEvent::Msg(
            30,
            Msg::SwapOffDone {
                slice: 0,
                ok: true,
                detail: String::new(),
            },
        ),
        Instant::now(),
    );
    assert!(
        !stale
            .iter()
            .any(|x| matches!(x, Outbound::ZeroSlice { .. }))
    );
    assert_eq!(c.slice_map.get(0).unwrap().state, SliceState::Draining);
}

#[test]
fn swapoff_done_triggers_zero_then_release() {
    let mut c = core(1);
    reg(&mut c, 10, "a");
    psi(&mut c, 10, 0.0);
    c.handle(CoreEvent::Tick, Instant::now()); // assign s0 → a (Active)
    let armed = arm_swap_off(&mut c, 0, 1); // start of move (Active→Draining)
    assert!(
        armed
            .iter()
            .any(|x| matches!(x, Outbound::ToSession(10, Msg::SwapOff { slice: 0 })))
    );
    let o = c.handle(
        CoreEvent::Msg(
            10,
            Msg::SwapOffDone {
                slice: 0,
                ok: true,
                detail: String::new(),
            },
        ),
        Instant::now(),
    );
    assert!(o.iter().any(|x| matches!(
        x,
        Outbound::ZeroSlice { slice: 0, base: 0, len } if *len == 64 * 1024 * 1024
    )));
    // A failed zero keeps the slice draining and schedules another exact zero attempt.
    let failed_zero = c.handle(CoreEvent::ZeroDone(0, false), Instant::now());
    assert!(failed_zero.iter().any(|x| matches!(
        x,
        Outbound::ZeroSlice { slice: 0, base: 0, len } if *len == 64 * 1024 * 1024
    )));
    assert_eq!(c.slice_map.get(0).unwrap().state, SliceState::Draining);
    // ZeroDone → release → Free
    c.handle(CoreEvent::ZeroDone(0, true), Instant::now());
    assert_eq!(c.slice_map.get(0).unwrap().state, SliceState::Free);
}

/// AUD-01 repro: an unregistered session must not be able to zero a live
/// Active slice by claiming a swapoff. Before the ack contract this
/// produced `ZeroSlice` for a slice that was never drained.
#[test]
fn unregistered_ack_cannot_zero_a_live_active_slice() {
    let mut c = core(1);
    reg(&mut c, 10, "a");
    psi(&mut c, 10, 0.0);
    c.handle(CoreEvent::Tick, Instant::now()); // s0 → a (Active)
    assert_eq!(c.slice_map.get(0).unwrap().state, SliceState::Active);
    let o = c.handle(
        CoreEvent::Msg(
            999,
            Msg::SwapOffDone {
                slice: 0,
                ok: true,
                detail: String::new(),
            },
        ),
        Instant::now(),
    );
    assert!(
        !o.iter().any(|x| matches!(x, Outbound::ZeroSlice { .. })),
        "an unregistered session must not arm a zero"
    );
    assert!(o.iter().any(|x| matches!(x, Outbound::CloseSession(999))));
    assert_eq!(c.slice_map.get(0).unwrap().state, SliceState::Active);
    assert_eq!(c.slice_map.get(0).unwrap().tenant, Some(1));
}

/// A registered session of another tenant cannot complete — or destroy —
/// a slice it does not own.
#[test]
fn foreign_session_cannot_ack_another_tenants_slice() {
    let mut c = core(2);
    reg(&mut c, 10, "owner");
    reg(&mut c, 20, "attacker");
    psi(&mut c, 10, 0.0);
    psi(&mut c, 20, 0.0);
    c.handle(CoreEvent::Tick, Instant::now()); // s0→owner(1), s1→attacker(2)
    let armed = arm_swap_off(&mut c, 0, 1);
    assert!(
        armed
            .iter()
            .any(|x| matches!(x, Outbound::ToSession(10, Msg::SwapOff { slice: 0 })))
    );
    let o = c.handle(
        CoreEvent::Msg(
            20,
            Msg::SwapOffDone {
                slice: 0,
                ok: true,
                detail: String::new(),
            },
        ),
        Instant::now(),
    );
    assert!(!o.iter().any(|x| matches!(x, Outbound::ZeroSlice { .. })));
    assert!(o.iter().any(|x| matches!(x, Outbound::CloseSession(20))));
    assert_eq!(c.slice_map.get(0).unwrap().state, SliceState::Draining);
    assert_eq!(c.slice_map.get(0).unwrap().tenant, Some(1));
    assert!(!c.pending_zero.contains_key(&0));
}

/// `SwapOffDone { ok: true }` on a slice that is still serving swap is a
/// request to zero live data. The owner never received `SwapOff`.
#[test]
fn unsolicited_swapoff_ack_on_an_active_slice_is_refused() {
    let mut c = core(1);
    reg(&mut c, 10, "a");
    psi(&mut c, 10, 0.0);
    c.handle(CoreEvent::Tick, Instant::now()); // s0 Active and armed On
    let o = c.handle(
        CoreEvent::Msg(
            10,
            Msg::SwapOffDone {
                slice: 0,
                ok: true,
                detail: String::new(),
            },
        ),
        Instant::now(),
    );
    assert!(!o.iter().any(|x| matches!(x, Outbound::ZeroSlice { .. })));
    assert!(o.iter().any(|x| matches!(x, Outbound::CloseSession(10))));
    assert_eq!(c.slice_map.get(0).unwrap().state, SliceState::Active);
    assert_eq!(c.slice_map.get(0).unwrap().tenant, Some(1));
}

/// A foreign `SwapOnDone { ok: false }` used to drain and release a live
/// slice. Ownership is now checked before the effect.
#[test]
fn foreign_swapon_failure_cannot_release_a_live_slice() {
    let mut c = core(2);
    reg(&mut c, 10, "owner");
    reg(&mut c, 20, "attacker");
    psi(&mut c, 10, 0.0);
    psi(&mut c, 20, 0.0);
    c.handle(CoreEvent::Tick, Instant::now()); // s0→owner(1)
    assert_eq!(c.slice_map.get(0).unwrap().state, SliceState::Active);
    let o = c.handle(
        CoreEvent::Msg(
            20,
            Msg::SwapOnDone {
                slice: 0,
                ok: false,
                detail: "lie".into(),
            },
        ),
        Instant::now(),
    );
    assert!(o.iter().any(|x| matches!(x, Outbound::CloseSession(20))));
    assert_eq!(c.slice_map.get(0).unwrap().state, SliceState::Active);
    assert_eq!(c.slice_map.get(0).unwrap().tenant, Some(1));
}

/// The owner's own `SwapOnDone { ok: false }` still returns the slice
/// without a zero pass — nothing was written on a failed mount.
#[test]
fn owners_swapon_failure_still_releases_its_slice() {
    let mut c = core(1);
    reg(&mut c, 10, "a");
    psi(&mut c, 10, 0.0);
    c.handle(CoreEvent::Tick, Instant::now()); // s0 → a, armed On
    let o = c.handle(
        CoreEvent::Msg(
            10,
            Msg::SwapOnDone {
                slice: 0,
                ok: false,
                detail: "mount refused".into(),
            },
        ),
        Instant::now(),
    );
    assert!(!o.iter().any(|x| matches!(x, Outbound::CloseSession(_))));
    assert_eq!(c.slice_map.get(0).unwrap().state, SliceState::Free);
    assert_eq!(c.slice_map.get(0).unwrap().tenant, None);
}

/// A reconnect is a new generation: work armed for the dropped session can
/// never be acknowledged by its replacement (AUD-01).
#[test]
fn stale_swapoff_ack_after_reconnect_is_refused() {
    let mut c = core(1);
    reg(&mut c, 10, "a");
    psi(&mut c, 10, 0.0);
    c.handle(CoreEvent::Tick, Instant::now());
    arm_swap_off(&mut c, 0, 1); // armed for sid 10
    c.handle(CoreEvent::Disconnected(10), Instant::now());
    reg(&mut c, 11, "a"); // reconnect: same tenant, new session
    let o = c.handle(
        CoreEvent::Msg(
            11,
            Msg::SwapOffDone {
                slice: 0,
                ok: true,
                detail: String::new(),
            },
        ),
        Instant::now(),
    );
    assert!(
        !o.iter().any(|x| matches!(x, Outbound::ZeroSlice { .. })),
        "a reconnected session did not earn the completion"
    );
    assert!(o.iter().any(|x| matches!(x, Outbound::CloseSession(11))));
    assert_eq!(c.slice_map.get(0).unwrap().state, SliceState::Draining);
}

/// A late failure after a confirmed mount must not release a slice that is
/// already serving swap.
#[test]
fn late_swapon_failure_after_success_is_refused() {
    let mut c = core(1);
    reg(&mut c, 10, "a");
    psi(&mut c, 10, 0.0);
    c.handle(CoreEvent::Tick, Instant::now()); // armed On for sid 10
    c.handle(
        CoreEvent::Msg(
            10,
            Msg::SwapOnDone {
                slice: 0,
                ok: true,
                detail: String::new(),
            },
        ),
        Instant::now(),
    );
    assert_eq!(c.slice_map.get(0).unwrap().state, SliceState::Active);
    let o = c.handle(
        CoreEvent::Msg(
            10,
            Msg::SwapOnDone {
                slice: 0,
                ok: false,
                detail: String::new(),
            },
        ),
        Instant::now(),
    );
    assert!(o.iter().any(|x| matches!(x, Outbound::CloseSession(10))));
    assert_eq!(c.slice_map.get(0).unwrap().state, SliceState::Active);
    assert_eq!(c.slice_map.get(0).unwrap().tenant, Some(1));
}

/// A retransmitted completion is idempotent: no second
/// effect, no state change, and the owner is not closed for retrying.
#[test]
fn duplicate_completion_is_idempotent() {
    let mut c = core(1);
    reg(&mut c, 10, "a");
    psi(&mut c, 10, 0.0);
    c.handle(CoreEvent::Tick, Instant::now());
    arm_swap_off(&mut c, 0, 1);
    let ack = || {
        CoreEvent::Msg(
            10,
            Msg::SwapOffDone {
                slice: 0,
                ok: true,
                detail: String::new(),
            },
        )
    };
    let first = c.handle(ack(), Instant::now());
    assert_eq!(
        first
            .iter()
            .filter(|x| matches!(x, Outbound::ZeroSlice { .. }))
            .count(),
        1
    );
    let second = c.handle(ack(), Instant::now());
    assert!(
        !second
            .iter()
            .any(|x| matches!(x, Outbound::ZeroSlice { .. })),
        "a retransmit must not re-arm the zero"
    );
    assert!(
        !second
            .iter()
            .any(|x| matches!(x, Outbound::CloseSession(_))),
        "a retransmit from the owner is not unauthorized"
    );
    assert_eq!(c.slice_map.get(0).unwrap().state, SliceState::Draining);
    assert_eq!(c.pending_zero.get(&0).copied(), Some(0));
}

/// Same contract on the swapon side: a retransmitted success is a no-op.
#[test]
fn duplicate_swapon_completion_is_idempotent() {
    let mut c = core(1);
    reg(&mut c, 10, "a");
    psi(&mut c, 10, 0.0);
    c.handle(CoreEvent::Tick, Instant::now());
    let ack = || {
        CoreEvent::Msg(
            10,
            Msg::SwapOnDone {
                slice: 0,
                ok: true,
                detail: String::new(),
            },
        )
    };
    c.handle(ack(), Instant::now());
    let second = c.handle(ack(), Instant::now());
    assert!(
        !second
            .iter()
            .any(|x| matches!(x, Outbound::CloseSession(_)))
    );
    assert_eq!(c.slice_map.get(0).unwrap().state, SliceState::Active);
    assert_eq!(c.slice_map.get(0).unwrap().tenant, Some(1));
}

#[test]
fn stuck_draining_zero_is_retried_on_tick() {
    // R4: if zeroing is not confirmed (channel full → no ZeroDone), the tick re-emits ZeroExport
    // after grace and escalates to ERROR; once confirmed, stops retrying.
    let mut c = core(1);
    reg(&mut c, 10, "a");
    psi(&mut c, 10, 0.0);
    c.handle(CoreEvent::Tick, Instant::now()); // assign s0 (Active)
    arm_swap_off(&mut c, 0, 1); // Draining + SwapOff dispatched
    c.handle(
        CoreEvent::Msg(
            10,
            Msg::SwapOffDone {
                slice: 0,
                ok: true,
                detail: String::new(),
            },
        ),
        Instant::now(),
    );
    // WITHOUT ZeroDone (simulates full channel). Tick 1 = grace → does not re-emit.
    let t1 = c.handle(CoreEvent::Tick, Instant::now());
    assert!(
        !t1.iter()
            .any(|x| matches!(x, Outbound::ZeroSlice { slice: 0, .. })),
        "carência: sem retry no 1º tick"
    );
    // Tick 2 → re-emits the zero.
    let t2 = c.handle(CoreEvent::Tick, Instant::now());
    assert!(
        t2.iter()
            .any(|x| matches!(x, Outbound::ZeroSlice { slice: 0, .. })),
        "retry do zero no 2º tick"
    );
    // More ticks → escalates to ERROR (R4).
    let mut saw_error = false;
    for _ in 0..ZERO_RETRY_ERROR {
        let o = c.handle(CoreEvent::Tick, Instant::now());
        if o.iter()
            .any(|x| matches!(x, Outbound::Log(s) if s.contains("R4")))
        {
            saw_error = true;
        }
    }
    assert!(
        saw_error,
        "must escalate to ERROR (R4) after N unconfirmed ticks"
    );
    // Zero confirms → Free + stops retrying.
    c.handle(CoreEvent::ZeroDone(0, true), Instant::now());
    assert_eq!(c.slice_map.get(0).unwrap().state, SliceState::Free);
    let after = c.handle(CoreEvent::Tick, Instant::now());
    assert!(
        !after
            .iter()
            .any(|x| matches!(x, Outbound::ZeroSlice { slice: 0, .. })),
        "after confirmation: no more retries"
    );
}

#[test]
fn move_drains_donor_then_swapon_dest_after_zero() {
    let mut c = core(2);
    reg(&mut c, 10, "donor");
    reg(&mut c, 20, "recv");
    psi(&mut c, 10, 0.0);
    psi(&mut c, 20, 0.0);
    c.handle(CoreEvent::Tick, Instant::now()); // assign s0→donor(1), s1→recv(2)
    // imbalance: recv (2) highly pressured, donor (1) idle, no Free slices → move
    psi(&mut c, 10, 0.0);
    psi(&mut c, 20, 50.0);
    let o = c.handle(CoreEvent::Tick, Instant::now());
    // move de uma slice do donor(1) p/ recv(2): SwapOff p/ a sessão do donor (10)
    assert!(
        o.iter()
            .any(|x| matches!(x, Outbound::ToSession(10, Msg::SwapOff { .. })))
    );
    let moved = c
        .slice_map
        .slices()
        .iter()
        .find(|s| s.state == SliceState::Draining)
        .map(|s| s.id)
        .expect("uma slice drenando");
    // SwapOffDone → zero → ZeroDone → SwapOn for recv (session 20)
    c.handle(
        CoreEvent::Msg(
            10,
            Msg::SwapOffDone {
                slice: moved,
                ok: true,
                detail: String::new(),
            },
        ),
        Instant::now(),
    );
    let o = c.handle(CoreEvent::ZeroDone(moved, true), Instant::now());
    assert!(o.iter().any(|x| matches!(
        x,
        Outbound::ToSession(20, Msg::SwapOn { slice, .. }) if *slice == moved
    )));
    assert_eq!(c.slice_map.get(moved).unwrap().tenant, Some(2));
}

#[test]
fn disconnect_marks_absent_and_freezes() {
    let mut c = core(1);
    reg(&mut c, 10, "a");
    psi(&mut c, 10, 0.0);
    c.handle(CoreEvent::Tick, Instant::now()); // s0 → a (Active)
    c.handle(CoreEvent::Disconnected(10), Instant::now());
    // slice remains Active (frozen), owner absent
    assert_eq!(c.slice_map.get(0).unwrap().state, SliceState::Active);
    // tick does not touch the absent one's slice (does not become Free nor reassigned)
    let o = c.handle(CoreEvent::Tick, Instant::now());
    assert!(
        !o.iter()
            .any(|x| matches!(x, Outbound::ToSession(_, Msg::SwapOn { .. })))
    );
    assert_eq!(c.slice_map.get(0).unwrap().state, SliceState::Active);
}

#[test]
fn demote_broadcasts_to_present() {
    let mut c = core(1);
    reg(&mut c, 10, "a");
    let o = c.handle(CoreEvent::Demote("latency".into()), Instant::now());
    assert!(o.contains(&Outbound::ToSession(10, Msg::DemoteAll)));
}

#[test]
fn status_reply_lists_tenants_and_slices() {
    let mut c = core(2);
    reg(&mut c, 10, "a");
    let o = c.handle(CoreEvent::Msg(10, Msg::Status), Instant::now());
    assert!(o.iter().any(|x| matches!(
        x,
        Outbound::ToSession(10, Msg::StatusReply { tenants, slices, .. })
            if tenants.len() == 1 && slices.len() == 2
    )));
}

#[test]
fn a_registered_tenant_publishes_absence_not_a_fabricated_idle() {
    // The broker must not invent the figure a tenant never sent. The fold
    // this replaces seeded `PsiSample::default()` on register, whose
    // avg10 of 0.0 was published as a measured idle and drove donor
    // selection and lease revocation.
    let mut c = core(2);
    reg(&mut c, 10, "a");
    match c.status_reply() {
        Msg::StatusReply { tenants, .. } => {
            assert_eq!(tenants.len(), 1);
            assert_eq!(
                tenants[0].psi, None,
                "a tenant that has never reported has no pressure figure"
            );
        }
        other => panic!("expected StatusReply, got {other:?}"),
    }
}

#[test]
fn status_reply_includes_slice_io() {
    // RF-1: StatusReply exposes counters per slice (read from the shared Arc).
    let c = core(2);
    c.slice_io[0].bytes_served.store(4096, Ordering::Relaxed);
    c.slice_io[0].io_count.store(1, Ordering::Relaxed);
    match c.status_reply() {
        Msg::StatusReply { slice_io, .. } => {
            assert_eq!(slice_io.len(), 2);
            let s0 = slice_io.iter().find(|s| s.id == 0).expect("slice 0");
            assert_eq!(s0.bytes_served, 4096);
            assert_eq!(s0.io_count, 1);
        }
        other => panic!("esperava StatusReply, veio {other:?}"),
    }
}

#[path = "tests/telemetry_and_lease.rs"]
mod telemetry_and_lease;
