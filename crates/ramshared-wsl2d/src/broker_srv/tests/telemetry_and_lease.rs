// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/broker_srv/tests/telemetry_and_lease.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::super::*;
use super::*;

#[test]
fn on_tick_emits_telemetry() {
    // RF-5: every tick emits a telemetry sample.
    let mut c = core(1);
    let o = c.handle(CoreEvent::Tick, Instant::now());
    assert!(o.iter().any(|x| matches!(x, Outbound::Telemetry(_))));
}

#[test]
fn eviction_flag_after_demote() {
    // RF-4/DT-6: a canary DEMOTE → Eviction flag on the next tick (streak=1 in helper).
    let mut c = core(1);
    reg(&mut c, 10, "a");
    c.vram.total.store(1 << 30, Ordering::Relaxed); // has_vram (otherwise Partial)
    c.vram.free.store(1 << 29, Ordering::Relaxed);
    c.handle(CoreEvent::Demote("latency".into()), Instant::now());
    let o = c.handle(CoreEvent::Tick, Instant::now());
    let flag = o
        .iter()
        .find_map(|x| match x {
            Outbound::Telemetry(t) => Some(t.flag),
            _ => None,
        })
        .expect("telemetry on tick");
    assert_eq!(flag, ReconcileFlag::Eviction);
}

#[test]
fn unaccounted_when_occupied_exceeds_alloc() {
    // RF-4: occupied > borrowed + tol → Unaccounted (no demote, with VRAM).
    let mut c = core(1); // 1 slice of 64 MiB
    reg(&mut c, 10, "a");
    let id = *c.tenants.keys().next().expect("tenant");
    c.slice_map.assign(0, id).expect("assign slice 0"); // Free→Active (64 MiB borrowed)
    c.tenants.get_mut(&id).expect("tenant").occupied_bytes = 200 * 1024 * 1024; // 200 MiB > 64
    c.vram.total.store(1 << 30, Ordering::Relaxed); // has_vram
    let o = c.handle(CoreEvent::Tick, Instant::now());
    let flag = o
        .iter()
        .find_map(|x| match x {
            Outbound::Telemetry(t) => Some(t.flag),
            _ => None,
        })
        .expect("telemetry on tick");
    assert_eq!(flag, ReconcileFlag::Unaccounted);
}

fn tick_flag(c: &mut BrokerCore) -> ReconcileFlag {
    c.handle(CoreEvent::Tick, Instant::now())
        .iter()
        .find_map(|x| match x {
            Outbound::Telemetry(t) => Some(t.flag),
            _ => None,
        })
        .expect("telemetry on tick")
}

#[test]
fn eviction_confirmed_immediately_despite_streak() {
    // C1: Eviction is an EVENT (canary) → confirms in 1 tick even with recon_streak=3 (production).
    // Without the fix, the hysteresis would swallow transient eviction.
    let mut c = core_streak(1, 3);
    reg(&mut c, 10, "a");
    c.vram.total.store(1 << 30, Ordering::Relaxed); // has_vram (otherwise Partial)
    c.vram.free.store(1 << 29, Ordering::Relaxed);
    c.handle(CoreEvent::Demote("latency".into()), Instant::now());
    assert_eq!(tick_flag(&mut c), ReconcileFlag::Eviction);
}

#[test]
fn unaccounted_respects_streak() {
    // RF-4/DT-12: SUSTAINED flag (Unaccounted) only confirms after `recon_streak` ticks.
    let mut c = core_streak(1, 2);
    reg(&mut c, 10, "a");
    let id = *c.tenants.keys().next().expect("tenant");
    c.slice_map.assign(0, id).expect("assign");
    c.tenants.get_mut(&id).expect("tenant").occupied_bytes = 200 * 1024 * 1024;
    c.vram.total.store(1 << 30, Ordering::Relaxed);
    assert_eq!(
        tick_flag(&mut c),
        ReconcileFlag::None,
        "first tick: streak not reached yet"
    );
    assert_eq!(
        tick_flag(&mut c),
        ReconcileFlag::Unaccounted,
        "second tick: confirms"
    );
}

#[test]
fn telemetry_sink_writes_jsonl_line() {
    // RF-5/DT-8: the sink writes 1 JSON object per line (write-in-file, in-process).
    let path = std::env::temp_dir().join(format!("ramshared-telem-{}.jsonl", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let mut sink = TelemetrySink::open(path.clone()).expect("abre sink");
        let tc = TelemetryCore {
            tenant: None,
            slice: None,
            swap_used: 7,
            alloc_active: 8,
            page_io_s: None,
            vram_alloc_daemon: 9,
            vram_total_used: None,
            vram_outros: None,
            canario_demotes: 0,
            demote_reason: None,
            reconcile_delta: 0.0,
            flag: ReconcileFlag::None,
        };
        sink.emit(&tc).expect("first telemetry row");
        sink.emit(&tc).expect("second telemetry row");
    }
    let content = std::fs::read_to_string(&path).expect("reads the jsonl");
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 2, "one line per emit");
    let v: serde_json::Value = serde_json::from_str(lines[0]).expect("valid JSON");
    assert_eq!(v["swap_used"], 7);
    assert!(v.get("t").is_some(), "timestamp is present");
    let _ = std::fs::remove_file(&path);
}

#[cfg(target_os = "linux")]
#[test]
fn telemetry_sink_surfaces_write_failure() {
    let mut sink = TelemetrySink::open(PathBuf::from("/dev/full")).expect("open /dev/full");
    let tc = TelemetryCore {
        tenant: None,
        slice: None,
        swap_used: 0,
        alloc_active: 0,
        page_io_s: None,
        vram_alloc_daemon: 0,
        vram_total_used: None,
        vram_outros: None,
        canario_demotes: 0,
        demote_reason: None,
        reconcile_delta: 0.0,
        flag: ReconcileFlag::Partial,
    };
    assert!(sink.emit(&tc).is_err());
}

#[test]
fn dev_to_slice_requires_exact_nbd_identity() {
    assert_eq!(dev_to_slice("/dev/nbd5"), Some(5));
    assert_eq!(dev_to_slice("/dev/nbd0"), Some(0));
    assert_eq!(dev_to_slice("/nbd7"), Some(7));
    assert_eq!(dev_to_slice("nbd9"), Some(9));
    assert_eq!(dev_to_slice("tmp/nbd5"), None);
    assert_eq!(dev_to_slice("/dev/sda"), None);
    assert_eq!(dev_to_slice("/dev/sda5"), None);
    assert_eq!(dev_to_slice("/tmp/nbd5"), None);
    assert_eq!(dev_to_slice("/dev/nbd5-backup"), None);
    assert_eq!(dev_to_slice("/dev/nbd5\\040(deleted)"), None);
}

const SLICE: u64 = 64 * 1024 * 1024;

fn lease_req(c: &mut BrokerCore, sid: usize, bytes: u64) -> Vec<Outbound> {
    c.handle(
        CoreEvent::Msg(sid, Msg::LeaseRequest { bytes }),
        Instant::now(),
    )
}

fn n_leased(c: &BrokerCore) -> usize {
    c.slice_map
        .slices()
        .iter()
        .filter(|s| s.state == SliceState::Leased)
        .count()
}

fn register_at(c: &mut BrokerCore, sid: usize, name: &str, now: Instant) {
    c.handle(
        CoreEvent::Msg(
            sid,
            Msg::Register {
                proto: PROTO_VERSION,
                tenant: name.into(),
                transport: TransportKind::DccAgent,
            },
        ),
        now,
    );
}

fn heartbeat_at(c: &mut BrokerCore, sid: usize, now: Instant) {
    c.handle(
        CoreEvent::Msg(
            sid,
            Msg::Psi {
                sample: PsiSample::default(),
                swaps: vec![],
                mem: None,
            },
        ),
        now,
    );
}

#[test]
fn lease_granted_from_free_slices() {
    let mut c = core(2);
    reg(&mut c, 10, "dcc");
    psi(&mut c, 10, 0.0);
    let o = lease_req(&mut c, 10, SLICE); // 1 slice
    assert!(
        !o.iter()
            .any(|x| matches!(x, Outbound::ToSession(_, Msg::LeaseDenied { .. })))
    );
    let o = c.handle(CoreEvent::Tick, Instant::now());
    assert!(
        o.iter()
            .any(|x| matches!(x, Outbound::ToSession(10, Msg::LeaseGranted { .. })))
    );
    assert_eq!(n_leased(&c), 1);
}

#[test]
fn lease_denied_when_in_progress() {
    let mut c = core(2);
    reg(&mut c, 10, "dcc");
    psi(&mut c, 10, 0.0);
    lease_req(&mut c, 10, SLICE);
    let o = lease_req(&mut c, 10, SLICE);
    assert!(o.iter().any(
        |x| matches!(x, Outbound::ToSession(10, Msg::LeaseDenied { reason }) if reason == "lease_em_andamento")
    ));
}

#[test]
fn lease_denied_over_capacity() {
    let mut c = core(2); // total = 128 MiB
    reg(&mut c, 10, "dcc");
    psi(&mut c, 10, 0.0);
    let o = lease_req(&mut c, 10, 200 * 1024 * 1024);
    assert!(o.iter().any(
        |x| matches!(x, Outbound::ToSession(10, Msg::LeaseDenied { reason }) if reason == "acima_da_capacidade")
    ));
}

#[test]
fn lease_release_returns_slices() {
    let mut c = core(2);
    reg(&mut c, 10, "dcc");
    psi(&mut c, 10, 0.0);
    lease_req(&mut c, 10, SLICE);
    c.handle(CoreEvent::Tick, Instant::now()); // GrantLease (lease id 1)
    assert_eq!(n_leased(&c), 1);
    c.handle(
        CoreEvent::Msg(10, Msg::LeaseRelease { lease: 1 }),
        Instant::now(),
    );
    assert_eq!(n_leased(&c), 0); // devolvida ao tier de swap
}

#[test]
fn lease_release_is_acknowledged_even_when_already_settled() {
    // AUD-08: a release is a request and has to be answered. The host
    // bridge turns this Ack into `Released { lease }`; silence hung it.
    let mut c = core(2);
    reg(&mut c, 10, "dcc");
    psi(&mut c, 10, 0.0);
    lease_req(&mut c, 10, SLICE);
    c.handle(CoreEvent::Tick, Instant::now()); // GrantLease (lease id 1)

    let out = c.handle(
        CoreEvent::Msg(10, Msg::LeaseRelease { lease: 1 }),
        Instant::now(),
    );
    assert!(
        out.contains(&Outbound::ToSession(10, Msg::Ack)),
        "a settled release must be acknowledged: {out:?}"
    );
    assert_eq!(n_leased(&c), 0);

    // The retry of an already-settled release is idempotent and still
    // answered — never left silent.
    let out = c.handle(
        CoreEvent::Msg(10, Msg::LeaseRelease { lease: 1 }),
        Instant::now(),
    );
    assert!(
        out.contains(&Outbound::ToSession(10, Msg::Ack)),
        "an idempotent release must be acknowledged: {out:?}"
    );
    assert_eq!(n_leased(&c), 0);
}

#[test]
fn disconnect_holds_active_lease_until_deadline() {
    let ttl = Duration::from_secs(3);
    let started = Instant::now();
    let mut c = core_with_lease_ttl(1, 1, ttl);
    register_at(&mut c, 10, "holder", started);
    heartbeat_at(&mut c, 10, started);
    c.handle(
        CoreEvent::Msg(10, Msg::LeaseRequest { bytes: SLICE }),
        started,
    );
    c.handle(CoreEvent::Tick, started);
    assert_eq!(n_leased(&c), 1);

    c.handle(
        CoreEvent::Disconnected(10),
        started + Duration::from_secs(1),
    );
    assert_eq!(n_leased(&c), 1, "disconnect must not reclaim immediately");

    c.handle(CoreEvent::Tick, started + Duration::from_secs(2));
    assert_eq!(n_leased(&c), 1, "pre-deadline tick retains the lease");
    c.handle(CoreEvent::Tick, started + ttl);
    assert_eq!(n_leased(&c), 0, "deadline tick returns the lease slices");
}

#[test]
fn holder_heartbeat_renews_active_lease() {
    let ttl = Duration::from_secs(3);
    let started = Instant::now();
    let mut c = core_with_lease_ttl(1, 1, ttl);
    register_at(&mut c, 10, "holder", started);
    heartbeat_at(&mut c, 10, started);
    c.handle(
        CoreEvent::Msg(10, Msg::LeaseRequest { bytes: SLICE }),
        started,
    );
    c.handle(CoreEvent::Tick, started);
    assert_eq!(n_leased(&c), 1);

    heartbeat_at(&mut c, 10, started + Duration::from_secs(2));
    c.handle(CoreEvent::Tick, started + ttl);
    assert_eq!(n_leased(&c), 1, "holder heartbeat extends the deadline");
    c.handle(CoreEvent::Tick, started + Duration::from_secs(5));
    assert_eq!(n_leased(&c), 0, "renewed deadline eventually expires");
}

#[test]
fn foreign_tenant_cannot_release_lease() {
    let mut c = core(2);
    reg(&mut c, 10, "holder");
    reg(&mut c, 20, "foreign");
    lease_req(&mut c, 10, SLICE);
    c.handle(CoreEvent::Tick, Instant::now());

    let out = c.handle(
        CoreEvent::Msg(20, Msg::LeaseRelease { lease: 1 }),
        Instant::now(),
    );

    assert_eq!(n_leased(&c), 1);
    assert!(out.contains(&Outbound::CloseSession(20)));
    assert!(out.iter().any(
        |effect| matches!(effect, Outbound::ToSession(20, Msg::Error { reason }) if reason.contains("holder"))
    ));
}

#[test]
fn shared_lease_book_preserves_linux_wire_effects() {
    let mut c = core(2);
    reg(&mut c, 10, "holder");

    lease_req(&mut c, 10, SLICE);
    let grant = c.handle(CoreEvent::Tick, Instant::now());
    assert!(grant.contains(&Outbound::ToSession(
        10,
        Msg::LeaseGranted {
            lease: 1,
            bytes: SLICE,
        },
    )));

    c.handle(
        CoreEvent::Msg(10, Msg::LeaseRelease { lease: 1 }),
        Instant::now(),
    );
    lease_req(&mut c, 10, SLICE);
    let second = c.handle(CoreEvent::Tick, Instant::now());
    assert!(second.contains(&Outbound::ToSession(
        10,
        Msg::LeaseGranted {
            lease: 2,
            bytes: SLICE,
        },
    )));
}

#[test]
fn lease_revokes_active_then_grants_after_zero() {
    // DT-8/R2: lease drena uma slice Active (de outro tenant), zera, e concede.
    let mut c = core(1);
    reg(&mut c, 10, "swap"); // id 1
    reg(&mut c, 20, "dcc"); // id 2
    psi(&mut c, 10, 0.0);
    psi(&mut c, 20, 0.0);
    c.handle(CoreEvent::Tick, Instant::now()); // s0 → swap(1) Active (round-robin)
    assert_eq!(c.slice_map.get(0).unwrap().tenant, Some(1));
    // dcc(20) requests a lease that needs the slice → revokes it from swap(1)
    lease_req(&mut c, 20, SLICE);
    let o = c.handle(CoreEvent::Tick, Instant::now());
    assert!(
        o.iter()
            .any(|x| matches!(x, Outbound::ToSession(10, Msg::SwapOff { slice: 0 })))
    );
    assert!(
        !o.iter()
            .any(|x| matches!(x, Outbound::ToSession(_, Msg::LeaseGranted { .. })))
    ); // not yet
    // swapoff confirms → zero → release → Free
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
    c.handle(CoreEvent::ZeroDone(0, true), Instant::now());
    assert_eq!(c.slice_map.get(0).unwrap().state, SliceState::Free);
    // next tick: enough Free capacity now exists → GrantLease for dcc(20)
    let o = c.handle(CoreEvent::Tick, Instant::now());
    assert!(
        o.iter()
            .any(|x| matches!(x, Outbound::ToSession(20, Msg::LeaseGranted { .. })))
    );
    assert_eq!(c.slice_map.get(0).unwrap().state, SliceState::Leased);
}

#[test]
fn pending_lease_is_cancelled_when_holder_disconnects() {
    let mut c = core(2);
    reg(&mut c, 10, "dcc");
    psi(&mut c, 10, 0.0);
    lease_req(&mut c, 10, SLICE);
    c.handle(CoreEvent::Disconnected(10), Instant::now());
    c.handle(CoreEvent::Tick, Instant::now());
    assert_eq!(n_leased(&c), 0, "disconnect cancels an ungranted request");
}

fn reg_transport(
    c: &mut BrokerCore,
    sid: usize,
    name: &str,
    transport: TransportKind,
) -> Vec<Outbound> {
    c.handle(
        CoreEvent::Msg(
            sid,
            Msg::Register {
                proto: PROTO_VERSION,
                tenant: name.into(),
                transport,
            },
        ),
        Instant::now(),
    )
}

/// SPEC windows-swap-driver DT-7: WinDrive never receives SwapOn (lease-only).
#[test]
fn windrive_does_not_receive_swap() {
    let mut c = core(2);
    reg_transport(&mut c, 10, "swap", TransportKind::NbdUnix);
    reg_transport(&mut c, 20, "windrive", TransportKind::WinDrive);
    psi(&mut c, 10, 0.0);
    psi(&mut c, 20, 0.0);
    let o = c.handle(CoreEvent::Tick, Instant::now());
    let swapons: Vec<_> = o
        .iter()
        .filter_map(|x| match x {
            Outbound::ToSession(sid, Msg::SwapOn { .. }) => Some(*sid),
            _ => None,
        })
        .collect();
    // Only the NbdUnix tenant is in the swap round-robin; both slices go to sid 10.
    assert!(
        swapons.iter().all(|&sid| sid == 10),
        "WinDrive must not receive SwapOn; got {swapons:?}"
    );
    assert!(
        !swapons.contains(&20),
        "WinDrive tenant must be excluded from present"
    );
}

/// SPEC windows-swap-driver DT-7: WinDrive can acquire a lease (revokes swap if needed).
#[test]
fn windrive_can_lease() {
    let mut c = core(1);
    reg_transport(&mut c, 10, "swap", TransportKind::NbdUnix);
    reg_transport(&mut c, 20, "windrive", TransportKind::WinDrive);
    psi(&mut c, 10, 0.0);
    psi(&mut c, 20, 0.0);
    c.handle(CoreEvent::Tick, Instant::now()); // s0 → swap Active
    assert_eq!(c.slice_map.get(0).unwrap().tenant, Some(1));
    lease_req(&mut c, 20, SLICE);
    let o = c.handle(CoreEvent::Tick, Instant::now());
    assert!(
        o.iter()
            .any(|x| matches!(x, Outbound::ToSession(10, Msg::SwapOff { slice: 0 })))
    );
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
    c.handle(CoreEvent::ZeroDone(0, true), Instant::now());
    let o = c.handle(CoreEvent::Tick, Instant::now());
    assert!(
        o.iter()
            .any(|x| matches!(x, Outbound::ToSession(20, Msg::LeaseGranted { .. }))),
        "WinDrive must receive LeaseGranted"
    );
    assert_eq!(c.slice_map.get(0).unwrap().state, SliceState::Leased);
}

/// AUD-06: the IO event queue is bounded, so a control-event flood cannot
/// grow resident memory in front of the core. A full queue refuses the
/// next event instead of accepting it.
#[test]
fn a_full_io_event_queue_refuses_instead_of_growing() {
    let (tx, rx) = mpsc::sync_channel::<IoEvent>(2);
    assert!(
        tx.try_send(IoEvent::Core(CoreEvent::Demote("a".into())))
            .is_ok(),
        "the first event fits"
    );
    assert!(
        tx.try_send(IoEvent::Core(CoreEvent::Demote("b".into())))
            .is_ok(),
        "the second event fits"
    );
    assert!(
        tx.try_send(IoEvent::Core(CoreEvent::Demote("c".into())))
            .is_err(),
        "the third must be refused — the queue is bounded"
    );
    drop(rx);
}

#[test]
fn transport_dispatch_and_sink_open_handle_backpressure_and_failures() {
    use super::transport::{TelemetrySink, dispatch};
    use std::collections::HashMap;

    assert!(
        TelemetrySink::open(PathBuf::from("/nonexistent/ramshared-test/sink.jsonl")).is_none(),
        "opening sink under a missing directory must fail closed to None"
    );

    let (dead_tx, dead_rx) = mpsc::sync_channel::<Msg>(1);
    drop(dead_rx);
    let (live_tx, _live_rx) = mpsc::sync_channel::<Msg>(1);
    let mut sessions = HashMap::from([(1_usize, dead_tx), (2_usize, live_tx)]);

    let (jobs_tx, jobs_rx) = mpsc::sync_channel::<crate::conn::WMsg>(1);
    let (io_tx, io_rx) = mpsc::sync_channel::<IoEvent>(4);

    #[cfg(target_os = "linux")]
    let mut sink = TelemetrySink::open(PathBuf::from("/dev/full"));
    #[cfg(not(target_os = "linux"))]
    let mut sink = None;

    let tc = TelemetryCore {
        tenant: None,
        slice: None,
        swap_used: 0,
        alloc_active: 0,
        page_io_s: None,
        vram_alloc_daemon: 0,
        vram_total_used: None,
        vram_outros: None,
        canario_demotes: 0,
        demote_reason: None,
        reconcile_delta: 0.0,
        flag: ReconcileFlag::None,
    };

    dispatch(
        vec![
            Outbound::ToSession(1, Msg::Ack),
            Outbound::ToSession(99, Msg::Ack),
            Outbound::CloseSession(2),
            Outbound::ZeroSlice {
                slice: 1,
                base: 4096,
                len: 4096,
            },
            Outbound::ZeroSlice {
                slice: 2,
                base: 8192,
                len: 4096,
            },
            Outbound::Log("diagnostic line".into()),
            Outbound::Telemetry(tc),
        ],
        &mut sessions,
        &jobs_tx,
        &io_tx,
        &mut sink,
    );

    assert!(
        sessions.is_empty(),
        "dead and closed sessions must be pruned"
    );
    #[cfg(target_os = "linux")]
    assert!(
        sink.is_none(),
        "telemetry sink write failure must disable further emission"
    );

    let job = jobs_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("ZeroExport queued");
    drop(job);
    let ev = io_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("ZeroDone event emitted");
    assert!(matches!(ev, IoEvent::Core(CoreEvent::ZeroDone(1, false))));
}
