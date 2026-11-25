// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/broker_srv/transport.rs
//! Bounded TCP session I/O loop, acceptor, outbound dispatcher, and telemetry sink for [`BrokerCore`].

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ramshared_broker::arbiter::ArbiterConfig;
use ramshared_broker::protocol::{Msg, read_msg, write_msg};
use ramshared_broker::slices::SliceMap;

use super::{BrokerCore, BrokerCoreConfig, CoreEvent, EndpointCfg, Outbound};
use crate::conn::WMsg;
use crate::residency::DemoteReason;
use crate::telemetry::{SliceIoCounters, TelemetryCore, VramGauge};

// ===================== IO Layer (DT-2/DT-24) =====================
//
// `BrokerCore` is pure; `spawn_broker` is the thin shell of threads running it: TCP acceptor,
// reader/writer per session (writer = bounded channel 64, DT-24), the core loop (`recv_timeout`
// = tick) and DEMOTE and zero-done forwarders. The core never does socket IO.
//
// The IO event channel is bounded (AUD-06): session readers block on `send`, so
// a flood of control events applies backpressure to those sessions' sockets
// instead of growing an unbounded queue in front of the core.

/// Depth of the core's IO event queue (AUD-06). The core is a pure state
/// machine and drains fast; the cap exists so session readers can outpace it
/// only by this much before their sockets apply backpressure.
pub const IO_EVENT_CAP: usize = 256;

/// Most concurrent sessions the acceptor will wire (AUD-06). Beyond this the
/// socket is closed immediately instead of spawning reader/writer threads.
pub const MAX_SESSIONS: usize = 128;

/// Broker config (DT-2: in-process in the daemon; RNF-2: `listen` already validated as non-unspecified).
pub struct BrokerConfig {
    pub listen: SocketAddr,
    pub endpoints: EndpointCfg,
    pub swap_prio: Option<i32>,
    pub arbiter: ArbiterConfig,
    pub tick: Duration,
    /// Bound on an orphaned active lease. Production uses [`DEFAULT_LEASE_TTL`].
    pub lease_ttl: Duration,
    /// Telemetry (SPEC): counters per slice + VRAM gauge (shared with the worker).
    pub slice_io: Arc<Vec<SliceIoCounters>>,
    pub vram: Arc<VramGauge>,
    pub tol_frac: f64,
    pub recon_streak: u32,
    /// Destino do JSONL de telemetria (RF-5); `None` = telemetria silenciosa.
    pub telemetry_jsonl: Option<PathBuf>,
}

/// Evento interno do IO (multiplexa registro de sessão e eventos do core).
pub(super) enum IoEvent {
    NewSession(usize, SyncSender<Msg>),
    Core(CoreEvent),
}

/// Starts the broker: acceptor + sessions + single-thread core. Returns the core handle and the
/// bound `SocketAddr` (useful with port 0 in tests). `shutdown` triggers `DemoteAll` + exit.
pub fn spawn_broker(
    cfg: BrokerConfig,
    slice_map: SliceMap,
    demote_rx: Receiver<DemoteReason>,
    jobs: SyncSender<WMsg>,
    shutdown: Arc<AtomicBool>,
) -> io::Result<(JoinHandle<()>, SocketAddr)> {
    let listener = TcpListener::bind(cfg.listen)?;
    let addr = listener.local_addr()?;
    listener.set_nonblocking(true)?;
    let (io_tx, io_rx) = mpsc::sync_channel::<IoEvent>(IO_EVENT_CAP);

    // Acceptor.
    {
        let io_tx = io_tx.clone();
        let shutdown = Arc::clone(&shutdown);
        thread::spawn(move || acceptor_loop(&listener, &io_tx, &shutdown));
    }
    // DEMOTE forwarder (canary/residency) → CoreEvent::Demote.
    {
        let io_tx = io_tx.clone();
        thread::spawn(move || {
            for reason in demote_rx.iter() {
                if io_tx
                    .send(IoEvent::Core(CoreEvent::Demote(format!("{reason:?}"))))
                    .is_err()
                {
                    break;
                }
            }
        });
    }
    // Core (single thread owner of BrokerCore). Keeps an `io_tx` for the zero-done forwarders.
    let core = BrokerCore::new(
        slice_map,
        BrokerCoreConfig {
            arbiter_cfg: cfg.arbiter,
            endpoints: cfg.endpoints,
            swap_prio: cfg.swap_prio,
            lease_ttl: cfg.lease_ttl,
            slice_io: cfg.slice_io,
            vram: cfg.vram,
            tol_frac: cfg.tol_frac,
            recon_streak: cfg.recon_streak,
        },
    );
    let tick = cfg.tick;
    let sink = cfg.telemetry_jsonl.and_then(TelemetrySink::open);
    let handle =
        thread::spawn(move || core_loop(core, &io_rx, &io_tx, &jobs, tick, &shutdown, sink));
    Ok((handle, addr))
}

fn acceptor_loop(listener: &TcpListener, io_tx: &SyncSender<IoEvent>, shutdown: &AtomicBool) {
    let mut next_sid = 0usize;
    let live = Arc::new(AtomicUsize::new(0));
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                // Session cap (AUD-06): refuse before spawning any thread.
                let cur = live.load(Ordering::SeqCst);
                if cur >= MAX_SESSIONS {
                    eprintln!(
                        "[ramsharedd] broker: session cap ({MAX_SESSIONS}) reached; refusing"
                    );
                    drop(stream);
                    continue;
                }
                live.fetch_add(1, Ordering::SeqCst);
                let _ = stream.set_nodelay(true);
                let Ok(wsock) = stream.try_clone() else {
                    live.fetch_sub(1, Ordering::SeqCst);
                    continue;
                };
                let sid = next_sid;
                next_sid += 1;
                let (wtx, wrx) = mpsc::sync_channel::<Msg>(64); // DT-24: bounded
                thread::spawn(move || session_writer(wsock, &wrx));
                // `try_send`: a full IO queue means the core is behind. Refuse
                // the session rather than block the acceptor on one flood.
                if io_tx.try_send(IoEvent::NewSession(sid, wtx)).is_err() {
                    live.fetch_sub(1, Ordering::SeqCst);
                    break; // core encerrou or queue full — either way, stop accepting
                }
                let io2 = io_tx.clone();
                let live2 = Arc::clone(&live);
                thread::spawn(move || session_reader(stream, sid, &io2, live2));
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
}

fn session_writer(mut sock: TcpStream, rx: &Receiver<Msg>) {
    for msg in rx.iter() {
        if write_msg(&mut sock, &msg).is_err() {
            break;
        }
    }
    // closed channel (CloseSession/backpressure) or error → closes the socket (the reader sees EOF).
    let _ = sock.shutdown(Shutdown::Both);
}

fn session_reader(
    sock: TcpStream,
    sid: usize,
    io_tx: &SyncSender<IoEvent>,
    live: Arc<AtomicUsize>,
) {
    let mut r = BufReader::new(sock);
    // Blocking `send` is the backpressure (AUD-06): a session that outpaces the
    // core waits here, which stalls its socket reads rather than growing the
    // IO queue without bound.
    while let Ok(Some(m)) = read_msg(&mut r) {
        if io_tx.send(IoEvent::Core(CoreEvent::Msg(sid, m))).is_err() {
            live.fetch_sub(1, Ordering::SeqCst);
            return;
        }
    }
    let _ = io_tx.send(IoEvent::Core(CoreEvent::Disconnected(sid)));
    live.fetch_sub(1, Ordering::SeqCst);
}

#[allow(clippy::too_many_arguments)] // core IO shell: channels + tick + shutdown + sink
fn core_loop(
    mut core: BrokerCore,
    io_rx: &Receiver<IoEvent>,
    io_tx: &SyncSender<IoEvent>,
    jobs: &SyncSender<WMsg>,
    tick: Duration,
    shutdown: &AtomicBool,
    mut sink: Option<TelemetrySink>,
) {
    let mut sessions: HashMap<usize, SyncSender<Msg>> = HashMap::new();
    // Wall-clock deadline for the next Tick. CRITICAL: the Arbiter's Tick MUST NOT be starved
    // by messages. Pure `recv_timeout(tick)` never expires under normal `Psi` flow
    // (~1/s per tenant) → the arbiter would never run `AssignFree`/rebalance. Here the wait shrinks
    // as messages arrive, and the Tick fires when the deadline passes, regardless of
    // message rate. (Bug caught in e2e cross-host multi-tenant; the QEMU drill passed by luck of timing.)
    let mut next_tick = Instant::now() + tick;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            let outs = core.handle(CoreEvent::Demote("shutdown".into()), Instant::now());
            dispatch(outs, &mut sessions, jobs, io_tx, &mut sink);
            break;
        }
        let wait = next_tick.saturating_duration_since(Instant::now());
        match io_rx.recv_timeout(wait) {
            Ok(IoEvent::NewSession(sid, wtx)) => {
                sessions.insert(sid, wtx);
            }
            Ok(IoEvent::Core(ev)) => {
                let outs = core.handle(ev, Instant::now());
                dispatch(outs, &mut sessions, jobs, io_tx, &mut sink);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if Instant::now() >= next_tick {
            let outs = core.handle(CoreEvent::Tick, Instant::now());
            dispatch(outs, &mut sessions, jobs, io_tx, &mut sink);
            next_tick = Instant::now() + tick;
        }
    }
}

pub(super) fn dispatch(
    outs: Vec<Outbound>,
    sessions: &mut HashMap<usize, SyncSender<Msg>>,
    jobs: &SyncSender<WMsg>,
    io_tx: &SyncSender<IoEvent>,
    sink: &mut Option<TelemetrySink>,
) {
    for o in outs {
        match o {
            Outbound::ToSession(sid, msg) => {
                // DT-24: try_send; channel full/dead → drops the session (without blocking the core).
                let dead = match sessions.get(&sid) {
                    Some(wtx) => wtx.try_send(msg).is_err(),
                    None => false,
                };
                if dead {
                    sessions.remove(&sid);
                }
            }
            Outbound::CloseSession(sid) => {
                sessions.remove(&sid);
            }
            Outbound::ZeroSlice { slice, base, len } => {
                let (dtx, drx) = mpsc::channel::<bool>();
                if jobs
                    .try_send(WMsg::ZeroExport {
                        base,
                        len,
                        done: dtx,
                    })
                    .is_ok()
                {
                    let io2 = io_tx.clone();
                    thread::spawn(move || {
                        let ok = drx.recv().unwrap_or(false);
                        let _ = io2.send(IoEvent::Core(CoreEvent::ZeroDone(slice, ok)));
                    });
                } else {
                    eprintln!(
                        "[ramsharedd] WARN jobs channel full; zeroing of s{slice} postponed (R4)"
                    );
                }
            }
            Outbound::Log(s) => eprintln!("{s}"),
            Outbound::Telemetry(core) => {
                if let Some(s) = sink.as_mut()
                    && let Err(error) = s.emit(&core)
                {
                    eprintln!("[ramsharedd] WARN telemetry disabled after write failure: {error}");
                    *sink = None;
                }
            }
        }
    }
}

/// JSONL telemetry sink (RF-5/DT-8): timestamps `t`/`branch`/`commit` on the core's `TelemetryCore` and
/// appends 1 JSON object per line. `branch`/`commit` come from env
/// (`RAMSHARED_BUILD_BRANCH`/`RAMSHARED_BUILD_COMMIT`; `None` if absent — F-v2-4).
pub(super) struct TelemetrySink {
    file: File,
    branch: Option<String>,
    commit: Option<String>,
}

impl TelemetrySink {
    pub(super) fn open(path: PathBuf) -> Option<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| eprintln!("[ramsharedd] WARN telemetria off: {path:?}: {e}"))
            .ok()?;
        Some(Self {
            file,
            branch: std::env::var("RAMSHARED_BUILD_BRANCH").ok(),
            commit: std::env::var("RAMSHARED_BUILD_COMMIT").ok(),
        })
    }

    pub(super) fn emit(&mut self, core: &TelemetryCore) -> std::io::Result<()> {
        let t = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let sample = crate::telemetry::TelemetrySample {
            t,
            branch: self.branch.clone(),
            commit: self.commit.clone(),
            core: core.clone(),
        };
        let mut line = serde_json::to_string(&sample)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        line.push('\n');
        self.file.write_all(line.as_bytes())
    }
}
