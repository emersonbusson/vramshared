// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/broker_runtime.rs
//! Broker control-plane composition, listener ownership, and bounded worker lifecycle.

use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ramshared_block::protocol::{
    NBD_FLAG_CAN_MULTI_CONN, NBD_FLAG_HAS_FLAGS, NBD_FLAG_SEND_FLUSH, NBD_FLAG_SEND_FUA,
};
use ramshared_block::{BlockBackend, Command, serve};
use ramshared_broker::arbiter::ArbiterConfig;
use ramshared_broker::lease::DEFAULT_LEASE_TTL;
use ramshared_broker::slices::SliceMap;
use ramshared_wsl2d::broker_srv::{BrokerConfig, EndpointCfg, spawn_broker};
use ramshared_wsl2d::{
    CHAN_CAP, DemoteReason, RamBackend, Reply, SliceIoCounters, SliceView, VramGauge, WMsg,
    spawn_acceptor,
};

use super::shutdown_signals::{SHUTDOWN, ShutdownBridge, install_shutdown_signals};
use super::{
    BLOCK_SIZE, OwnedUnixSocketPath, apply_listen_backlog, bind_owned_unix_listener, zero_window,
};

/// Reconciliation tolerance (DT-7, provisional — calibrate at P0).
const RECON_TOL_FRAC: f64 = 0.10;
/// Consecutive ticks to confirm a reconciliation flag (hysteresis DT-12).
const RECON_STREAK: u32 = 3;

/// The worker-owned half of broker startup. Keeping the `Receiver` owned (not
/// borrowed) makes the data-plane runnable in a bounded test thread without
/// pretending that `Receiver` is `Sync`.
pub(super) struct BrokerWorkerRuntime {
    pub(super) geom: Vec<(u64, u64)>,
    pub(super) jobs_rx: std::sync::mpsc::Receiver<WMsg>,
    pub(super) demote_tx: std::sync::mpsc::Sender<DemoteReason>,
    pub(super) shutdown: std::sync::Arc<AtomicBool>,
    /// A full worker queue cannot be awakened by a detached blocking sender:
    /// the worker clears this pending bit when its top-of-iteration terminal
    /// check preempts the queued work.
    pub(super) shutdown_wake_pending: std::sync::Arc<AtomicBool>,
    pub(super) shutdown_wake_tx: std::sync::mpsc::SyncSender<WMsg>,
    /// IO counters per slice (telemetry RF-1): worker increments, broker reads in `Status`.
    pub slice_io: std::sync::Arc<Vec<SliceIoCounters>>,
    /// VRAM Gauge (RF-3): the residency closure publishes free/total; broker reads on tick.
    #[cfg(test)]
    pub vram: std::sync::Arc<VramGauge>,
}

/// Result of the nonblocking broker-worker shutdown wake (DT-50). A full queue
/// is safe because it proves the receiver has pending work; the worker checks
/// the terminal flag before receiving the next message. A disconnected queue
/// is already terminal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BrokerShutdownWake {
    Queued,
    QueueFull,
    Disconnected,
}

/// Paired terminal flag and explicit worker-channel wake. `request` stores the
/// flag first and never blocks, so signal mirroring and rollback paths cannot
/// deadlock behind a full bounded queue.
#[derive(Clone)]
pub(super) struct BrokerShutdown {
    pub(super) flag: std::sync::Arc<AtomicBool>,
    pub(super) wake_tx: std::sync::mpsc::SyncSender<WMsg>,
    pub(super) wake_pending: std::sync::Arc<AtomicBool>,
}

impl BrokerShutdown {
    pub(super) fn new(
        flag: std::sync::Arc<AtomicBool>,
        wake_tx: std::sync::mpsc::SyncSender<WMsg>,
    ) -> Self {
        Self {
            flag,
            wake_tx,
            wake_pending: std::sync::Arc::new(AtomicBool::new(false)),
        }
    }

    pub(super) fn request(&self) -> BrokerShutdownWake {
        self.flag.store(true, Ordering::SeqCst);
        match self.wake_tx.try_send(WMsg::Shutdown) {
            Ok(()) => BrokerShutdownWake::Queued,
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                // Never detach a blocking `send`: if no receiver remains it
                // would leak a test/daemon thread indefinitely. A full queue
                // already guarantees the worker is runnable; its independent
                // top-of-iteration flag check preempts all queued work.
                self.wake_pending.store(true, Ordering::SeqCst);
                BrokerShutdownWake::QueueFull
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => BrokerShutdownWake::Disconnected,
        }
    }
}

/// Broker control-plane parts retained by the daemon shell until the worker has
/// stopped. Splitting this value transfers the non-`Sync` receiver exactly once
/// to the worker and keeps the broker join handle and stoppable signal bridge for
/// orderly cleanup.
pub(super) struct BrokerRuntime {
    pub(super) shutdown_bridge: Option<ShutdownBridge>,
    pub(super) worker: BrokerWorkerRuntime,
    pub(super) broker: std::thread::JoinHandle<()>,
    pub(super) shutdown: BrokerShutdown,
    pub(super) socket: Option<OwnedUnixSocketPath>,
}

impl BrokerRuntime {
    pub(super) fn into_parts(
        self,
    ) -> (
        BrokerWorkerRuntime,
        std::thread::JoinHandle<()>,
        BrokerShutdown,
        Option<OwnedUnixSocketPath>,
        Option<ShutdownBridge>,
    ) {
        (
            self.worker,
            self.broker,
            self.shutdown,
            self.socket,
            self.shutdown_bridge,
        )
    }
}

/// Joins the broker on a dedicated observer so an early broker panic can wake
/// and stop the worker before the lifecycle attempts cleanup. The observer is
/// itself owned and joined; no detached cleanup thread survives a return.
pub(super) struct BrokerJoinMonitor {
    result_rx: std::sync::mpsc::Receiver<std::thread::Result<()>>,
    observer: Option<std::thread::JoinHandle<()>>,
    shutdown: BrokerShutdown,
    complete: bool,
}

impl BrokerJoinMonitor {
    pub(super) fn start(broker: std::thread::JoinHandle<()>, shutdown: BrokerShutdown) -> Self {
        let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
        let observer_shutdown = shutdown.clone();
        let observer = std::thread::spawn(move || {
            let result = broker.join();
            let _ = observer_shutdown.request();
            let _ = result_tx.send(result);
        });
        Self {
            result_rx,
            observer: Some(observer),
            shutdown,
            complete: false,
        }
    }

    pub(super) fn finish(mut self) -> Result<(), Box<dyn std::error::Error>> {
        let result = self
            .result_rx
            .recv()
            .map_err(|_| "broker result observer disconnected")?;
        let observer = self
            .observer
            .take()
            .ok_or("broker result observer is missing")?;
        if observer.join().is_err() {
            return Err("broker result observer panicked".into());
        }
        self.complete = true;
        match result {
            Ok(()) => Ok(()),
            Err(payload) => {
                let detail = payload
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("non-string panic payload");
                Err(format!("broker control-plane thread panicked: {detail}").into())
            }
        }
    }
}

impl Drop for BrokerJoinMonitor {
    fn drop(&mut self) {
        if self.complete {
            return;
        }
        let _ = self.shutdown.request();
        if let Some(observer) = self.observer.take() {
            let _ = observer.join();
        }
    }
}

/// Builds the exact broker control-plane configuration with an explicit core
/// poll interval. Production retains the two-second contract; a bounded local
/// harness can use a short interval solely to prove shutdown/cleanup without
/// starting a GPU, swap device, or NBD client.
pub(super) fn build_broker_config_with_tick(
    slices: u16,
    sock: &str,
    advertise_tcp: Option<(String, u16)>,
    arbiter_addr: std::net::SocketAddr,
    telemetry_jsonl: Option<std::path::PathBuf>,
    tick: Duration,
) -> BrokerConfig {
    let slice_io = std::sync::Arc::new(
        (0..slices)
            .map(|_| SliceIoCounters::default())
            .collect::<Vec<_>>(),
    );
    let vram = std::sync::Arc::new(VramGauge::default());
    BrokerConfig {
        listen: arbiter_addr,
        endpoints: EndpointCfg {
            nbd_unix: Some(sock.to_string()),
            nbd_tcp: advertise_tcp,
        },
        swap_prio: None,
        arbiter: ArbiterConfig::default(),
        tick,
        lease_ttl: DEFAULT_LEASE_TTL,
        slice_io,
        vram,
        tol_frac: RECON_TOL_FRAC,
        recon_streak: RECON_STREAK,
        telemetry_jsonl,
    }
}

/// The listeners owned by one broker-start attempt. The Unix pathname is cleaned
/// only when it was successfully bound by this attempt; an existing regular
/// file or socket is never replaced (DT-17 safe cleanup).
pub(super) struct BrokerListeners {
    pub(super) unix: UnixListener,
    pub(super) tcp: Option<std::net::TcpListener>,
    pub(super) socket: OwnedUnixSocketPath,
}

impl BrokerListeners {
    pub(super) fn cleanup(self) {
        drop(self);
    }

    pub(super) fn into_parts(
        self,
    ) -> (
        UnixListener,
        Option<std::net::TcpListener>,
        OwnedUnixSocketPath,
    ) {
        (self.unix, self.tcp, self.socket)
    }
}

/// Binds the safe socket portion of broker startup. If the optional TCP bind
/// fails after Unix binding succeeded, it closes and removes only that newly
/// created Unix socket before returning the refusal.
pub(super) fn bind_broker_listeners(
    path: &Path,
    listen_nbd_addr: Option<std::net::SocketAddr>,
) -> std::io::Result<BrokerListeners> {
    let (unix, socket) = bind_owned_unix_listener(path)?;
    let tcp = match listen_nbd_addr {
        Some(addr) => {
            let listener = std::net::TcpListener::bind(addr)?;
            apply_listen_backlog(&listener)?;
            Some(listener)
        }
        None => None,
    };
    Ok(BrokerListeners { unix, tcp, socket })
}

/// Starts the data-plane acceptors after the broker control plane has bound.
/// Keeping this side-effect behind a narrow interface lets the daemon prove its
/// listener ownership and bounded shutdown with a recording harness, while the
/// production implementation still uses the real protocol acceptors.
pub(super) trait BrokerAcceptorStarter {
    fn start(
        &mut self,
        unix: UnixListener,
        tcp: Option<std::net::TcpListener>,
        exports: std::sync::Arc<Vec<ramshared_block::handshake::Export>>,
        tx_flags: u16,
        jobs_tx: std::sync::mpsc::SyncSender<WMsg>,
    ) -> Result<(), Box<dyn std::error::Error>>;
}

pub(super) struct ProductionBrokerAcceptorStarter;

impl BrokerAcceptorStarter for ProductionBrokerAcceptorStarter {
    fn start(
        &mut self,
        unix: UnixListener,
        tcp: Option<std::net::TcpListener>,
        exports: std::sync::Arc<Vec<ramshared_block::handshake::Export>>,
        tx_flags: u16,
        jobs_tx: std::sync::mpsc::SyncSender<WMsg>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let _ = spawn_acceptor(
            unix,
            std::sync::Arc::clone(&exports),
            tx_flags,
            jobs_tx.clone(),
        );
        if let Some(tcp) = tcp {
            let addr = tcp.local_addr()?;
            eprintln!("[ramsharedd] NBD TCP listener at {addr}");
            let _ = ramshared_wsl2d::conn::spawn_acceptor_tcp(tcp, exports, tx_flags, jobs_tx);
        }
        Ok(())
    }
}

/// Starts the broker control-plane with checked signal registration before any
/// listener or acceptor setup and retains an owned stoppable shutdown bridge.
#[allow(clippy::too_many_arguments)]
pub(super) fn broker_setup_with_runtime(
    slices: u16,
    slice_bytes: u64,
    sock: &str,
    listen_nbd_addr: Option<std::net::SocketAddr>,
    advertise_tcp: Option<(String, u16)>,
    arbiter_addr: std::net::SocketAddr,
    telemetry_jsonl: Option<std::path::PathBuf>,
    broker_tick: Duration,
    bridge_poll: Duration,
    shutdown_source: &'static AtomicBool,
    install_signals: impl FnOnce() -> Result<(), Box<dyn std::error::Error>>,
    acceptors: &mut dyn BrokerAcceptorStarter,
) -> Result<BrokerRuntime, Box<dyn std::error::Error>> {
    install_signals()?;
    let shutdown = std::sync::Arc::new(AtomicBool::new(false));
    let mut runtime = broker_setup_with_acceptors(
        slices,
        slice_bytes,
        sock,
        listen_nbd_addr,
        advertise_tcp,
        arbiter_addr,
        telemetry_jsonl,
        shutdown,
        broker_tick,
        acceptors,
    )?;
    let bridge_shutdown = runtime.shutdown.clone();
    runtime.shutdown_bridge = Some(ShutdownBridge::spawn(
        shutdown_source,
        bridge_poll,
        move || {
            let _ = bridge_shutdown.request();
        },
    ));
    Ok(runtime)
}

/// Starts the broker control-plane (independent of backend): slices map + geometry +
/// NBD exports ("s0".."sN"), acceptors (always Unix; TCP if `--listen-nbd`) feeding the
/// SAME `jobs` worker channel, the arbiter (`spawn_broker`, sharing `jobs` for the
/// hygiene `ZeroExport` DT-17 and consuming the DEMOTE channel) and the `SHUTDOWN` bridge
/// (signal handler only touches the async-signal-safe static variable → mirrored in `Arc`).
#[allow(clippy::too_many_arguments)] // control-plane setup: geometry, network, and telemetry
pub(super) fn broker_setup(
    slices: u16,
    slice_bytes: u64,
    sock: &str,
    listen_nbd_addr: Option<std::net::SocketAddr>,
    advertise_tcp: Option<(String, u16)>,
    arbiter_addr: std::net::SocketAddr,
    telemetry_jsonl: Option<std::path::PathBuf>,
) -> Result<BrokerRuntime, Box<dyn std::error::Error>> {
    let mut acceptors = ProductionBrokerAcceptorStarter;
    broker_setup_with_runtime(
        slices,
        slice_bytes,
        sock,
        listen_nbd_addr,
        advertise_tcp,
        arbiter_addr,
        telemetry_jsonl,
        Duration::from_secs(2),
        Duration::from_millis(100),
        &SHUTDOWN,
        install_shutdown_signals,
        &mut acceptors,
    )
}

/// The bounded, injectable broker startup core. Production passes the signal
/// bridge and real protocol acceptors; local tests pass an explicit shutdown
/// flag and recording starter, while still binding only temporary Unix and
/// loopback TCP sockets.
#[allow(clippy::too_many_arguments)]
pub(super) fn broker_setup_with_acceptors(
    slices: u16,
    slice_bytes: u64,
    sock: &str,
    listen_nbd_addr: Option<std::net::SocketAddr>,
    advertise_tcp: Option<(String, u16)>,
    arbiter_addr: std::net::SocketAddr,
    telemetry_jsonl: Option<std::path::PathBuf>,
    shutdown: std::sync::Arc<AtomicBool>,
    broker_tick: Duration,
    acceptors: &mut dyn BrokerAcceptorStarter,
) -> Result<BrokerRuntime, Box<dyn std::error::Error>> {
    // Slices map: export index (resolved by handshake) == geometry index == exports index
    // ("s{id}" names identical to those emitted by the broker in SwapOn).
    let slice_map = SliceMap::new(slices, slice_bytes, u64::from(slices) * slice_bytes)
        .map_err(|e| format!("invalid slice geometry: {:?}", e))?;
    let geom: Vec<(u64, u64)> = slice_map
        .slices()
        .iter()
        .map(|s| (s.offset, s.len))
        .collect();
    let exports = std::sync::Arc::new(
        slice_map
            .exports()
            .into_iter()
            .map(|(name, size)| ramshared_block::handshake::Export { name, size })
            .collect::<Vec<_>>(),
    );

    let tx_flags =
        NBD_FLAG_HAS_FLAGS | NBD_FLAG_SEND_FLUSH | NBD_FLAG_SEND_FUA | NBD_FLAG_CAN_MULTI_CONN;
    let (jobs_tx, jobs_rx) = std::sync::mpsc::sync_channel::<WMsg>(CHAN_CAP);
    let broker_shutdown = BrokerShutdown::new(std::sync::Arc::clone(&shutdown), jobs_tx.clone());
    let (demote_tx, demote_rx) = std::sync::mpsc::channel::<DemoteReason>();

    let listeners = bind_broker_listeners(Path::new(sock), listen_nbd_addr)?;

    let bcfg = build_broker_config_with_tick(
        slices,
        sock,
        advertise_tcp,
        arbiter_addr,
        telemetry_jsonl,
        broker_tick,
    );
    let slice_io = std::sync::Arc::clone(&bcfg.slice_io);
    #[cfg(test)]
    let vram = std::sync::Arc::clone(&bcfg.vram);
    let (broker, broker_addr) = match spawn_broker(
        bcfg,
        slice_map,
        demote_rx,
        jobs_tx.clone(),
        std::sync::Arc::clone(&shutdown),
    ) {
        Ok(result) => result,
        Err(error) => {
            listeners.cleanup();
            return Err(error.into());
        }
    };
    let (unix, tcp, socket) = listeners.into_parts();
    eprintln!("[ramsharedd] NBD Unix listener at {sock}");
    if let Err(error) = acceptors.start(unix, tcp, exports, tx_flags, jobs_tx.clone()) {
        let _ = broker_shutdown.request();
        let _ = broker.join();
        drop(socket);
        return Err(error);
    }
    eprintln!("[ramsharedd] broker arbiter at {broker_addr}");
    drop(jobs_tx); // clones (acceptors + broker) keep the channel; worker owns the rx

    Ok(BrokerRuntime {
        shutdown_bridge: None,
        worker: BrokerWorkerRuntime {
            geom,
            jobs_rx,
            demote_tx,
            shutdown,
            shutdown_wake_pending: std::sync::Arc::clone(&broker_shutdown.wake_pending),
            shutdown_wake_tx: broker_shutdown.wake_tx.clone(),
            slice_io,
            #[cfg(test)]
            vram,
        },
        broker,
        shutdown: broker_shutdown,
        socket: Some(socket),
    })
}

/// Bounded broker worker loop. Production uses a 500 ms shutdown poll; tests
/// inject a shorter positive interval to prove that an idle worker cannot wait
/// indefinitely after its explicit stop signal.
pub(super) fn serve_broker_jobs_with_poll<B: BlockBackend>(
    backend: B,
    rt: BrokerWorkerRuntime,
    residency: impl FnMut(u64) -> Option<DemoteReason>,
    poll_interval: Duration,
) -> B {
    serve_broker_jobs_with_poll_and_reply_hook(backend, rt, residency, poll_interval, || {})
}

/// Worker core with an injected post-publication hook. The hook lets tests
/// freeze the worker immediately after a reply becomes observable and prove
/// that all completion state was published first.
pub(super) fn serve_broker_jobs_with_poll_and_reply_hook<B: BlockBackend>(
    backend: B,
    rt: BrokerWorkerRuntime,
    residency: impl FnMut(u64) -> Option<DemoteReason>,
    poll_interval: Duration,
    reply_published: impl FnMut(),
) -> B {
    serve_broker_jobs_with_poll_heartbeat_and_reply_hook(
        backend,
        rt,
        residency,
        || None,
        poll_interval,
        Duration::from_secs(1),
        reply_published,
    )
}

pub(super) fn serve_broker_jobs_with_poll_heartbeat_and_reply_hook<B: BlockBackend>(
    mut backend: B,
    rt: BrokerWorkerRuntime,
    mut residency: impl FnMut(u64) -> Option<DemoteReason>,
    mut heartbeat: impl FnMut() -> Option<DemoteReason>,
    poll_interval: Duration,
    heartbeat_interval: Duration,
    mut reply_published: impl FnMut(),
) -> B {
    let poll_interval = if poll_interval.is_zero() {
        Duration::from_millis(1)
    } else {
        poll_interval
    };
    let heartbeat_interval = if heartbeat_interval.is_zero() {
        Duration::from_millis(1)
    } else {
        heartbeat_interval
    };
    let mut demoted = false;
    let mut consecutive_io_errors: u32 = 0;
    const MAX_CONSECUTIVE_IO_ERRORS: u32 = 3;
    let mut last_heartbeat = std::time::Instant::now();
    eprintln!("[ramsharedd] serving (single worker; multi-slice broker)");
    loop {
        if rt.shutdown.load(Ordering::SeqCst) {
            rt.shutdown_wake_pending.store(false, Ordering::SeqCst);
            break;
        }
        let msg = match rt.jobs_rx.recv_timeout(poll_interval) {
            Ok(m) => m,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if rt.shutdown.load(Ordering::SeqCst) {
                    break; // DT-28: stop only after SIGINT or SIGTERM.
                }
                if last_heartbeat.elapsed() >= heartbeat_interval && !demoted {
                    last_heartbeat = std::time::Instant::now();
                    if let Some(reason) = heartbeat() {
                        eprintln!(
                            "[ramsharedd] DEMOTE ({reason:?}) heartbeat probe -> broker DemoteAll"
                        );
                        let _ = rt.demote_tx.send(reason);
                        demoted = true;
                    }
                }
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let job = match msg {
            // DT-28: NBD connections coming and going do NOT terminate the daemon (the broker persists).
            WMsg::Opened | WMsg::Closed => None,
            WMsg::Shutdown => break,
            WMsg::Job(job) => Some(job),
            WMsg::ZeroExport { base, len, done } => {
                let ok = zero_window(&mut backend, base, len).is_ok();
                let _ = done.send(ok);
                None
            }
        };

        if let Some(job) = job {
            let touches = matches!(job.req.cmd, Command::Read | Command::Write);
            // Export geometry (handshake already resolved name→index). Defensive fallback: entire
            // backend (should not happen — every Job carries a valid export).
            let (base, len) = rt
                .geom
                .get(job.export)
                .copied()
                .unwrap_or((0, backend.size_bytes()));
            let t0 = std::time::Instant::now();
            let out = {
                let mut view = SliceView::new(&mut backend, base, len);
                serve(&job.req, &job.payload, &mut view)
            };
            let lat_us = t0.elapsed().as_micros() as u64;

            // Circuit breaker: rapid consecutive NBD_EIO replies immediately trip demote
            // rather than waiting for 64-request cadence canary sampling.
            let reply_errno =
                u32::from_be_bytes([out.reply[4], out.reply[5], out.reply[6], out.reply[7]]);
            let is_io_error = touches && reply_errno == ramshared_block::protocol::NBD_EIO;
            if is_io_error {
                consecutive_io_errors = consecutive_io_errors.saturating_add(1);
                if consecutive_io_errors >= MAX_CONSECUTIVE_IO_ERRORS && !demoted {
                    eprintln!(
                        "[ramsharedd] DEMOTE (IoErrorBurst) consecutive_errors={consecutive_io_errors} -> broker DemoteAll"
                    );
                    let _ = rt.demote_tx.send(DemoteReason::IoErrorBurst);
                    demoted = true;
                }
            } else if touches && reply_errno == ramshared_block::protocol::NBD_OK {
                consecutive_io_errors = 0;
            }

            // Telemetry RF-1: a reply is the completion barrier. Publish the
            // counters first so every observer that receives the reply also
            // observes the completed IO accounting.
            if touches && let Some(c) = rt.slice_io.get(job.export) {
                c.bytes_served
                    .fetch_add(u64::from(job.req.len), Ordering::Relaxed);
                c.io_count.fetch_add(1, Ordering::Relaxed);
            }
            if job
                .reply
                .send(Reply {
                    reply: out.reply,
                    data: out.read_data,
                    credit: job.credit,
                    disconnect: out.disconnect,
                    silent_disconnect: job.req.cmd == Command::Disc,
                })
                .is_ok()
            {
                reply_published();
            }

            if touches
                && !demoted
                && let Some(reason) = residency(lat_us)
            {
                eprintln!("[ramsharedd] DEMOTE ({reason:?}) lat={lat_us}us -> broker DemoteAll");
                let _ = rt.demote_tx.send(reason);
                demoted = true;
            }
        }

        if rt.shutdown_wake_pending.swap(false, Ordering::SeqCst) {
            match rt.shutdown_wake_tx.try_send(WMsg::Shutdown) {
                Ok(()) => {}
                Err(std::sync::mpsc::TrySendError::Full(_)) => {
                    rt.shutdown_wake_pending.store(true, Ordering::SeqCst);
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break,
            }
        }
    }
    backend
}

/// RAM broker path (without GPU): same control-plane, backend in heap. Exists to validate the
/// arbitration + swap lifecycle in **QEMU** (ITEM-11), where there is no GPU. Without residency
/// (RAM does not suffer eviction). `Cuda::load()` is never called → runs without libcuda.
pub(super) fn run_broker_ram(
    slice_bytes: u64,
    slices: u16,
    sock: String,
    listen_nbd_addr: Option<std::net::SocketAddr>,
    advertise_tcp: Option<(String, u16)>,
    arbiter_addr: std::net::SocketAddr,
    telemetry_jsonl: Option<std::path::PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let setup_sock = sock.clone();
    run_broker_ram_with_setup(
        slice_bytes,
        slices,
        sock,
        move || {
            broker_setup(
                slices,
                slice_bytes,
                &setup_sock,
                listen_nbd_addr,
                advertise_tcp,
                arbiter_addr,
                telemetry_jsonl,
            )
        },
        Duration::from_millis(500),
    )
}

/// Heap-RAM broker lifecycle using the same bounded worker and cleanup order as
/// the VRAM path. The setup boundary is injectable for a no-driver local proof.
pub(super) fn run_broker_ram_with_setup<S>(
    slice_bytes: u64,
    slices: u16,
    _sock: String,
    setup: S,
    worker_poll: Duration,
) -> Result<(), Box<dyn std::error::Error>>
where
    S: FnOnce() -> Result<BrokerRuntime, Box<dyn std::error::Error>>,
{
    let total = (slices as u64)
        .checked_mul(slice_bytes)
        .ok_or("--slices * --slice-mb: overflow")?;
    let backend = RamBackend::new(total as usize);
    eprintln!(
        "[ramsharedd] broker RAM (without GPU): {slices} slices x {} MiB = {} MiB, block_size={BLOCK_SIZE}",
        slice_bytes >> 20,
        total >> 20
    );

    let rt = setup()?;
    let (worker, broker, shutdown, socket, shutdown_bridge) = rt.into_parts();
    let broker_monitor = BrokerJoinMonitor::start(broker, shutdown);
    let _ = serve_broker_jobs_with_poll(backend, worker, |_| None, worker_poll); // RAM: no residency

    let broker_result = broker_monitor.finish();
    drop(shutdown_bridge);
    drop(socket);
    broker_result?;
    eprintln!("[ramsharedd] broker RAM stopped");
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn broker_setup_signal_failure_refuses_before_socket_bind_or_acceptors() {
        struct ForbiddenAcceptors;
        impl BrokerAcceptorStarter for ForbiddenAcceptors {
            fn start(
                &mut self,
                _: UnixListener,
                _: Option<std::net::TcpListener>,
                _: std::sync::Arc<Vec<ramshared_block::handshake::Export>>,
                _: u16,
                _: std::sync::mpsc::SyncSender<WMsg>,
            ) -> Result<(), Box<dyn std::error::Error>> {
                panic!("acceptor must never run after signal installation failure")
            }
        }

        static NEVER_SIGNALED: AtomicBool = AtomicBool::new(false);
        let sock = std::env::temp_dir().join(format!(
            "ramshared-broker-signal-fail-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&sock);
        let mut acceptors = ForbiddenAcceptors;
        let result = broker_setup_with_runtime(
            1,
            4096,
            sock.to_string_lossy().as_ref(),
            None,
            None,
            "127.0.0.1:0".parse().expect("valid loopback addr"),
            None,
            Duration::from_millis(20),
            Duration::from_millis(5),
            &NEVER_SIGNALED,
            || Err("injected SIGTERM registration error".into()),
            &mut acceptors,
        );
        let err = match result {
            Ok(_) => panic!("signal registration error must refuse broker setup"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("SIGTERM registration"));
        assert!(!sock.exists(), "socket must not exist when signals fail");
    }

    #[test]
    fn broker_setup_with_runtime_wakes_on_signal_flag_and_joins_bridge() {
        struct RecordingAcceptors {
            started: std::sync::Arc<AtomicBool>,
        }
        impl BrokerAcceptorStarter for RecordingAcceptors {
            fn start(
                &mut self,
                _unix: UnixListener,
                tcp: Option<std::net::TcpListener>,
                _exports: std::sync::Arc<Vec<ramshared_block::handshake::Export>>,
                _tx_flags: u16,
                _jobs_tx: std::sync::mpsc::SyncSender<WMsg>,
            ) -> Result<(), Box<dyn std::error::Error>> {
                assert!(tcp.is_some(), "loopback TCP listener should be bound");
                self.started.store(true, Ordering::SeqCst);
                Ok(())
            }
        }

        static BROKER_TEST_SHUTDOWN: AtomicBool = AtomicBool::new(false);
        BROKER_TEST_SHUTDOWN.store(false, Ordering::SeqCst);
        let sock = std::env::temp_dir().join(format!(
            "ramshared-broker-bridge-join-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&sock);
        let started = std::sync::Arc::new(AtomicBool::new(false));
        let mut acceptors = RecordingAcceptors {
            started: std::sync::Arc::clone(&started),
        };
        let runtime = broker_setup_with_runtime(
            1,
            4096,
            sock.to_string_lossy().as_ref(),
            Some("127.0.0.1:0".parse().expect("valid tcp addr")),
            None,
            "127.0.0.1:0".parse().expect("valid arbiter addr"),
            None,
            Duration::from_millis(10),
            Duration::from_millis(5),
            &BROKER_TEST_SHUTDOWN,
            || Ok(()),
            &mut acceptors,
        )
        .expect("broker setup with injected signal installer succeeds");
        assert!(started.load(Ordering::SeqCst));
        assert!(runtime.shutdown_bridge.is_some());

        let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
        let sock_str = sock.to_string_lossy().into_owned();
        let runner = std::thread::spawn(move || {
            let res = run_broker_ram_with_setup(
                4096,
                1,
                sock_str,
                || Ok(runtime),
                Duration::from_millis(5),
            )
            .map_err(|error| error.to_string());
            let _ = result_tx.send(res);
        });
        BROKER_TEST_SHUTDOWN.store(true, Ordering::SeqCst);
        let outcome = result_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("broker lifecycle must exit promptly after signal flag flip");
        runner.join().expect("broker runner thread joins");
        BROKER_TEST_SHUTDOWN.store(false, Ordering::SeqCst);
        assert_eq!(outcome, Ok(()));
        assert!(!sock.exists(), "owned unix socket cleaned after stop");

        // Also prove ProductionBrokerAcceptorStarter handles Unix + TCP listeners cleanly.
        let prod_sock = std::env::temp_dir().join(format!(
            "ramshared-prod-acceptor-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&prod_sock);
        let listeners = bind_broker_listeners(
            &prod_sock,
            Some("127.0.0.1:0".parse().expect("valid tcp addr")),
        )
        .expect("bind unix + tcp listeners");
        let (unix, tcp, socket_guard) = listeners.into_parts();
        let (jobs_tx, _jobs_rx) = std::sync::mpsc::sync_channel(CHAN_CAP);
        let exports = std::sync::Arc::new(vec![ramshared_block::handshake::Export {
            name: "s0".into(),
            size: 4096,
        }]);
        let mut starter = ProductionBrokerAcceptorStarter;
        starter
            .start(unix, tcp, exports, NBD_FLAG_HAS_FLAGS, jobs_tx)
            .expect("production acceptor starts unix and tcp listeners");
        drop(socket_guard);
    }

    #[test]
    fn broker_worker_handles_control_messages_residency_and_disconnect() {
        let (jobs_tx, jobs_rx) = std::sync::mpsc::sync_channel(CHAN_CAP);
        let shutdown = std::sync::Arc::new(AtomicBool::new(false));
        let wake_pending = std::sync::Arc::new(AtomicBool::new(false));
        let (demote_tx, _demote_rx) = std::sync::mpsc::channel();
        let mut backend = RamBackend::new(4096);
        backend.write_at(0, &[0x5a; 4096]).expect("seed backend");

        let (zero_done_tx, zero_done_rx) = std::sync::mpsc::channel();
        jobs_tx.send(WMsg::Opened).expect("send opened");
        jobs_tx.send(WMsg::Closed).expect("send closed");
        jobs_tx
            .send(WMsg::ZeroExport {
                base: 0,
                len: 4096,
                done: zero_done_tx,
            })
            .expect("send zero export");
        drop(jobs_tx.clone());

        let (wake_tx, _wake_rx) = std::sync::mpsc::sync_channel(1);
        let worker_rt = BrokerWorkerRuntime {
            geom: vec![(0, 4096)],
            jobs_rx,
            demote_tx,
            shutdown,
            shutdown_wake_pending: wake_pending,
            shutdown_wake_tx: wake_tx,
            slice_io: std::sync::Arc::new(vec![SliceIoCounters::default()]),
            vram: std::sync::Arc::new(VramGauge::default()),
        };

        // Disconnect worker channel immediately after messages are queued.
        drop(jobs_tx);
        let mut backend = serve_broker_jobs_with_poll_heartbeat_and_reply_hook(
            backend,
            worker_rt,
            |_| Some(DemoteReason::Latency),
            || None,
            Duration::ZERO,
            Duration::ZERO,
            || {},
        );
        assert_eq!(zero_done_rx.recv(), Ok(true));
        let mut readback = [0xffu8; 16];
        backend
            .read_at(0, &mut readback)
            .expect("read zeroed window");
        assert_eq!(readback, [0u8; 16]);
    }
}
