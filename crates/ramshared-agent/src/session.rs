// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-agent/src/session.rs
//! Broker session loop, slice command dispatch, and reconnection policy for `ramshared-agent`.

use std::collections::HashMap;
use std::io::BufReader;
use std::net::TcpStream;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use ramshared_agent::watchdog::{Watchdog, WatchdogError};
use ramshared_agent::{psi, socket, swap};
use ramshared_broker::model::SliceId;
use ramshared_broker::protocol::{Msg, NbdEndpoint, PROTO_VERSION, TenantMem, read_msg, write_msg};

use crate::cli::Config;

/// Transmission rate of `Psi` (low-rate control-plane, ~1 msg/s).
pub(super) const PSI_PERIOD: Duration = Duration::from_secs(1);
/// Poll slice of the main loop (responsiveness of the timer/exec without busy-loop).
pub(super) const POLL_SLICE: Duration = Duration::from_millis(200);
/// Reconnection backoff to the broker: starts at [`INITIAL_BACKOFF`] and doubles up to [`MAX_BACKOFF`]
/// while connection fails (broker down) — avoids reconnection thrashing; resets after a
/// productive session (≥ [`PRODUCTIVE_SESSION`], i.e., actually connected and ran).
pub(super) const INITIAL_BACKOFF: Duration = Duration::from_secs(2);
pub(super) const MAX_BACKOFF: Duration = Duration::from_secs(60);
pub(super) const PRODUCTIVE_SESSION: Duration = Duration::from_secs(10);

/// Next backoff (doubles with cap). Pure/testable.
pub(super) fn next_backoff(cur: Duration) -> Duration {
    (cur * 2).min(MAX_BACKOFF)
}

/// Command from the main loop to the execution thread.
#[derive(Debug)]
pub(super) enum ExecCmd {
    On {
        slice: SliceId,
        export: String,
        endpoint: NbdEndpoint,
        dev: String,
        prio: Option<i32>,
    },
    Off {
        slice: SliceId,
        dev: String,
    },
}

/// Result returned by the execution thread to the main loop.
///
/// `retained` carries the runner's cleanup authority (AUD-10/AUD-11): `true`
/// means the device may still hold a connection and/or swap even though the
/// effect failed, so the ownership map must keep it.
#[derive(Debug)]
pub(super) enum ExecResult {
    On {
        slice: SliceId,
        ok: bool,
        retained: bool,
        detail: String,
    },
    Off {
        slice: SliceId,
        ok: bool,
        retained: bool,
        detail: String,
    },
}

/// Spawns the execution thread (lives for the entire process duration) and runs the session loop with reconnection.
pub(super) fn run_agent(cfg: &Config) -> Result<(), Box<dyn std::error::Error>> {
    let (cmd_tx, cmd_rx) = mpsc::channel::<ExecCmd>();
    let (res_tx, res_rx) = mpsc::channel::<ExecResult>();
    let _exec = thread::spawn(move || exec_loop(cmd_rx, res_tx));

    eprintln!(
        "[agent] tenant={} broker={} transport={:?} watchdog={}s",
        cfg.tenant,
        cfg.broker,
        cfg.transport,
        cfg.watchdog.as_secs()
    );

    let mut backoff = INITIAL_BACKOFF;
    loop {
        let t0 = Instant::now();
        let result = session(cfg, &cmd_tx, &res_rx);
        let ran = t0.elapsed();
        match result {
            Ok(()) => eprintln!("[agent] session closed (EOF); reconnecting in {backoff:?}…"),
            Err(e) => eprintln!("[agent] session failed: {e}; reconnecting in {backoff:?}"),
        }
        thread::sleep(backoff);
        // Productive session (connected + ran) → goes back to minimum; quick failure (broker down) → grows.
        backoff = if ran >= PRODUCTIVE_SESSION {
            INITIAL_BACKOFF
        } else {
            next_backoff(backoff)
        };
    }
}

/// What the agent still owns for one slice (AUD-09).
///
/// Dispatch never commits. This record is the session's cleanup authority, and
/// it is dropped only when a swap absence is proved by a successful detach —
/// not when a `swapoff` is merely queued. A failed detach leaves the device
/// here so a retry or the end-of-session teardown can still find it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum SliceRecord {
    /// Swap is attached (or is being attached) on `dev`.
    Active { dev: String },
    /// A `swapoff` is in flight for `dev`. The device is still owned until
    /// its result proves absence.
    Draining { dev: String },
    /// `swapoff` failed for `dev`. It may still hold swap, so the record and
    /// the failure stay visible.
    Failed { dev: String },
}

impl SliceRecord {
    fn dev(&self) -> &str {
        match self {
            Self::Active { dev } | Self::Draining { dev } | Self::Failed { dev } => dev,
        }
    }

    /// A repeat `swapoff` is idempotent while one is already in flight
    ///: it must not queue a second detach.
    fn is_draining(&self) -> bool {
        matches!(self, Self::Draining { .. })
    }
}

/// A TCP session: connects, registers, and runs the loop until EOF/error/watchdog. On exit, performs
/// best-effort `swapoff` of still owned slices (dead broker ⇒ dead NBD).
pub(super) fn session(
    cfg: &Config,
    cmd_tx: &Sender<ExecCmd>,
    res_rx: &Receiver<ExecResult>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Absolute connect deadline and an armed write side (AUD-11). Reads are
    // deliberately left unbounded: the watchdog is the silence authority, and
    // a mid-frame read timeout would desync the framed protocol. On teardown
    // `shutdown(Both)` below is what unblocks the reader thread.
    let stream = socket::connect_bounded(&cfg.broker)?;
    let mut w = stream.try_clone()?;
    let reader = BufReader::new(stream);

    // reader thread: socket → Msg channel; exits (drops sender) on EOF/error.
    let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
    let reader_handle = thread::spawn(move || reader_loop(reader, msg_tx));

    write_msg(
        &mut w,
        &Msg::Register {
            proto: PROTO_VERSION,
            tenant: cfg.tenant.clone(),
            transport: cfg.transport,
        },
    )?;

    let mut active: HashMap<SliceId, SliceRecord> = HashMap::new();
    let mut wd = Watchdog::new_clamped(cfg.watchdog, Instant::now());
    let mut next_psi = Instant::now();
    let mut session_err: Option<Box<dyn std::error::Error>> = None;

    loop {
        let now = Instant::now();

        // (1) PSI heartbeat at cadence. Error reading /proc is transient: log and continue.
        if now >= next_psi {
            let (psi_res, swaps_res) = (psi::read_psi(), psi::read_swaps());

            match (psi_res, swaps_res) {
                (Ok(sample), Ok(swaps)) => {
                    // RF-2: memory telemetry (cgroup swap + diskstats of mounted nbds, DT-10/11).
                    let mem = Some(TenantMem {
                        swap_current: psi::read_memcg_swap(),
                        diskstats_io: active
                            .values()
                            .filter_map(|r| psi::read_diskstats(r.dev()))
                            .sum(),
                    });
                    if let Err(e) = write_msg(&mut w, &Msg::Psi { sample, swaps, mem }) {
                        session_err = Some(e.into());
                        break;
                    }
                }
                (s, sw) => {
                    // Transient /proc failure: skip the cycle. Never fabricate telemetry —
                    // a dummy PSI would make the broker budget on invented numbers.
                    eprintln!(
                        "[agent] PSI unreadable (psi={:?} swaps={:?}); skipping cycle",
                        s.err(),
                        sw.err()
                    );
                }
            }
            next_psi = now + PSI_PERIOD;
        }

        // (2) drains results from exec → Done back to the broker (single writer = this thread).
        while let Ok(res) = res_rx.try_recv() {
            let done = commit_result(res, &mut active);
            if let Err(e) = write_msg(&mut w, &done) {
                session_err = Some(e.into());
                break;
            }
        }
        if session_err.is_some() {
            break;
        }

        // (3) waits for a message from the broker (with a short slice to keep timer/exec alive).
        match msg_rx.recv_timeout(POLL_SLICE) {
            Ok(msg) => {
                wd.touch(Instant::now());
                if !handle_msg(cfg, msg, &mut active, cmd_tx) {
                    break; // broker sent Error / requested shutdown
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                session_err = Some(WatchdogError::SupervisorTerminated.into());
                break; // reader exited (EOF/socket error)
            }
        }

        // (4) watchdog: silent broker beyond the deadline ⇒ dead session.
        if let Err(e) = wd.check(Instant::now()) {
            session_err = Some(e.into());
            break;
        }
    }

    // Cleanup: every device the session still owns gets another teardown
    // attempt (best-effort; broker reconciles on re-register). A failed
    // `swapoff` earlier in the session is still owed this retry (AUD-09).
    cleanup_owned(&mut active, &mut swap::detach_swap);
    let _ = w.shutdown(std::net::Shutdown::Both);
    let _ = reader_handle.join();

    match session_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Handles a message from the broker. Returns `false` if the session should terminate.
/// Commits one execution result into the ownership map and builds the broker
/// report (AUD-09).
///
/// Commitment is separated from dispatch and from the socket write so that
/// "a failed detach keeps the device" is testable on its own: only a proved
/// swap absence drops the record.
pub(super) fn commit_result(res: ExecResult, active: &mut HashMap<SliceId, SliceRecord>) -> Msg {
    match res {
        ExecResult::On {
            slice,
            ok,
            retained,
            detail,
        } => {
            if ok {
                // Dispatch already recorded `Active`; swap is now on.
            } else if retained {
                // The effect failed but may have left the device in use —
                // `swapon` can complete before a contained helper is killed
                // (AUD-11). Keep the record so teardown can still find it.
                if let Some(record) = active.get_mut(&slice) {
                    let dev = record.dev().to_string();
                    eprintln!("[agent] swapon s{slice} ({dev}) failed; device retained: {detail}");
                    *record = SliceRecord::Failed { dev };
                }
            } else {
                // Refused without taking the effect: nothing to clean up.
                active.remove(&slice);
            }
            Msg::SwapOnDone { slice, ok, detail }
        }
        ExecResult::Off {
            slice,
            ok,
            retained,
            detail,
        } => {
            if ok {
                active.remove(&slice);
            } else if retained {
                if let Some(record) = active.get_mut(&slice) {
                    let dev = record.dev().to_string();
                    eprintln!("[agent] swapoff s{slice} ({dev}) failed; device retained: {detail}");
                    *record = SliceRecord::Failed { dev };
                }
            } else {
                // Failed but verifiably left nothing behind.
                active.remove(&slice);
            }
            Msg::SwapOffDone { slice, ok, detail }
        }
    }
}

/// Best-effort teardown of everything the session still owns (AUD-09).
///
/// A `swapoff` that failed earlier in the session is still owed this retry:
/// the record is the cleanup authority, so it is the devices still in the map
/// that are torn down, not the ones a dispatch happened to mention.
pub(super) fn cleanup_owned(
    active: &mut HashMap<SliceId, SliceRecord>,
    detach: &mut impl FnMut(&str) -> Result<(), swap::SwapFailure>,
) {
    for (slice, record) in active.drain() {
        let dev = record.dev().to_string();
        if let Err(f) = detach(&dev) {
            eprintln!(
                "[agent] cleanup swapoff s{slice} ({dev}) failed: {f} (retained={})",
                f.retained
            );
        }
    }
}

pub(super) fn handle_msg(
    cfg: &Config,
    msg: Msg,
    active: &mut HashMap<SliceId, SliceRecord>,
    cmd_tx: &Sender<ExecCmd>,
) -> bool {
    match msg {
        Msg::Registered { tenant_id } => {
            eprintln!("[agent] registered: tenant_id={tenant_id}");
            true
        }
        Msg::Ack => true, // heartbeat (already touched the watchdog)
        Msg::SwapOn {
            slice,
            export,
            endpoint,
            swap_prio,
        } => {
            let dev = format!("{}{}", cfg.nbd_base, slice);
            active.insert(slice, SliceRecord::Active { dev: dev.clone() });
            let prio = swap_prio.or(cfg.swap_prio); // DT-7: broker is authoritative; CLI is fallback
            cmd_tx
                .send(ExecCmd::On {
                    slice,
                    export,
                    endpoint,
                    dev,
                    prio,
                })
                .is_ok()
        }
        Msg::SwapOff { slice } => {
            // Dispatch does not commit (AUD-09): the record stays until the
            // result proves absence. A repeat while one is already in flight
            // is idempotent and queues nothing.
            let known = active
                .get(&slice)
                .map(|r| (r.dev().to_string(), r.is_draining()));
            match known {
                Some((_, true)) => true,
                Some((dev, false)) => {
                    active.insert(slice, SliceRecord::Draining { dev: dev.clone() });
                    cmd_tx.send(ExecCmd::Off { slice, dev }).is_ok()
                }
                // Unknown slice: derive the path so the broker still gets an
                // answer, but do not invent an ownership record for it.
                None => {
                    let dev = format!("{}{}", cfg.nbd_base, slice);
                    cmd_tx.send(ExecCmd::Off { slice, dev }).is_ok()
                }
            }
        }
        Msg::DemoteAll => {
            // Marking is not draining (AUD-09): each record stays until its
            // own result proves absence, so a failed one is still retryable
            // after a demotion that did not finish.
            let pending: Vec<(SliceId, String)> = active
                .iter_mut()
                .filter_map(|(&slice, record)| {
                    if record.is_draining() {
                        return None;
                    }
                    let dev = record.dev().to_string();
                    *record = SliceRecord::Draining { dev: dev.clone() };
                    Some((slice, dev))
                })
                .collect();
            eprintln!("[agent] DemoteAll: releasing {} slice(s)", pending.len());
            for (slice, dev) in pending {
                if cmd_tx.send(ExecCmd::Off { slice, dev }).is_err() {
                    return false;
                }
            }
            true
        }
        Msg::Error { reason } => {
            eprintln!("[agent] broker refused the session: {reason}");
            false
        }
        other => {
            eprintln!("[agent] ignored message: {other:?}");
            true
        }
    }
}

/// Maps one [`ExecCmd`] onto one [`ExecResult`] through the supplied attach/detach
/// runners. Pure with respect to the host: the runners own every mutation, so the
/// mapping and its failure text are unit-testable without touching swap (DT-27).
pub(super) fn execute_command<Attach, Detach>(
    cmd: ExecCmd,
    attach: &mut Attach,
    detach: &mut Detach,
) -> ExecResult
where
    Attach: FnMut(&NbdEndpoint, &str, &str, Option<i32>) -> Result<(), swap::SwapFailure>,
    Detach: FnMut(&str) -> Result<(), swap::SwapFailure>,
{
    match cmd {
        ExecCmd::On {
            slice,
            export,
            endpoint,
            dev,
            prio,
        } => {
            let (ok, retained, detail) = match attach(&endpoint, &export, &dev, prio) {
                Ok(()) => (true, false, dev),
                Err(f) => (false, f.retained, f.to_string()),
            };
            ExecResult::On {
                slice,
                ok,
                retained,
                detail,
            }
        }
        ExecCmd::Off { slice, dev } => {
            let (ok, retained, detail) = match detach(&dev) {
                Ok(()) => (true, false, dev),
                Err(f) => (false, f.retained, f.to_string()),
            };
            ExecResult::Off {
                slice,
                ok,
                retained,
                detail,
            }
        }
    }
}

/// Execution thread loop: runs attach/detach (blocking) and returns the result.
pub(super) fn exec_loop(cmd_rx: Receiver<ExecCmd>, res_tx: Sender<ExecResult>) {
    for cmd in cmd_rx.iter() {
        let res = execute_command(cmd, &mut swap::attach_swap, &mut swap::detach_swap);
        if res_tx.send(res).is_err() {
            break; // main loop is gone; nothing to do
        }
    }
}

/// Reader thread loop: forwards each `Msg` to the main loop; exits on EOF/error (dropping the
/// sender, which the main loop detects as `Disconnected`).
pub(super) fn reader_loop(mut reader: BufReader<TcpStream>, msg_tx: Sender<Msg>) {
    loop {
        match read_msg(&mut reader) {
            Ok(Some(msg)) => {
                if msg_tx.send(msg).is_err() {
                    break;
                }
            }
            Ok(None) => break, // clean EOF
            Err(e) => {
                eprintln!("[agent] socket read error: {e}");
                break;
            }
        }
    }
}
