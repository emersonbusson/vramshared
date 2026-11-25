// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/conn.rs
//! Multi-connection NBD connection (§9.4 / H1): dedicated reader and writer per connection,
//! connected to the **single CUDA worker** (in `main`) via channels. The reader drains the socket
//! and enqueues `Job`s; the worker processes them (CUDA affinity) and returns `Reply`s via
//! the connection's **unbounded** replica channel; the writer writes to the socket.
//!
//! `Opened` comes from the acceptor (before spawning the reader), `Closed` comes from the reader (upon exit) —
//! the worker counts `live` connections and reports the transition to zero. The simple NBD
//! runtime remains quiescent at zero until explicit shutdown so it can accept a later generation.

use std::io::{BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel};
use std::thread::JoinHandle;
use std::time::Duration;

use ramshared_block::handshake::Export;
use ramshared_block::protocol::SIMPLE_REPLY_LEN;
use ramshared_block::{Command, Request, parse_request, protocol::REQUEST_LEN, server_handshake};

use crate::frame_writer::write_frame;

/// Capacity of the worker message channel (`WMsg`): the **single** point of backpressure.
/// The replica channel per connection is unbounded (DT-7), so the worker never blocks when
/// responding — only the readers apply backpressure when enqueuing `Job`s. What bounds
/// that unbounded replica channel is [`ReplyBudget`] admission (AUD-06).
pub const CHAN_CAP: usize = 64;

/// Most replies one connection may have in flight before admission refuses (AUD-06).
/// NBD pipelines, but a peer that submits work and stops consuming replies must not
/// turn the replica channel into a memory reservation.
pub const MAX_PENDING_REPLIES: usize = 32;

/// Most bytes of in-flight reply payload one connection may hold (AUD-06). Together
/// with [`MAX_PENDING_REPLIES`] this is the connection's reply-memory ceiling; the
/// global ceiling is [`MAX_CONNECTIONS`] × this.
pub const MAX_PENDING_REPLY_BYTES: usize = 8 * 1024 * 1024;

/// Handshake deadline (AUD-06). A peer that connects and says nothing must not hold a
/// connection thread. Cleared for the request phase: a swap connection is long-lived
/// and idle time there is normal.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Most concurrent connections an acceptor will wire (AUD-06). Beyond this the
/// acceptor closes the socket immediately instead of spawning reader/writer threads.
pub const MAX_CONNECTIONS: usize = 32;

pub use crate::reply_budget::ReplyBudget;
use crate::reply_budget::ReplyCredit;

/// Read-deadline control on a connection stream (AUD-06).
///
/// The acceptor arms [`HANDSHAKE_TIMEOUT`] before the handshake so an idle peer
/// cannot hold a connection thread; the reader clears it once the handshake is
/// done, because the request phase is long-lived and must tolerate idle time.
/// In-memory test streams keep the default no-op.
pub trait ReadDeadline {
    fn set_read_deadline(&self, _deadline: Option<Duration>) -> std::io::Result<()> {
        Ok(())
    }
}

impl ReadDeadline for TcpStream {
    fn set_read_deadline(&self, deadline: Option<Duration>) -> std::io::Result<()> {
        self.set_read_timeout(deadline)
    }
}

impl ReadDeadline for UnixStream {
    fn set_read_deadline(&self, deadline: Option<Duration>) -> std::io::Result<()> {
        self.set_read_timeout(deadline)
    }
}

/// In-memory streams have no deadline to arm; a `Cursor` is already bounded by
/// the data it holds (AUD-06).
impl<T> ReadDeadline for std::io::Cursor<T> {}

/// One connection's slot against the acceptor's connection cap (AUD-06).
///
/// Held by the reader thread, which is the connection's longest-lived half: the
/// writer ends when the reader drops the reply sender. Dropping the slot is what
/// lets the acceptor wire another connection. The type is `pub` because it is
/// the handle `spawn_reader` takes to keep that connection accounted for.
pub struct ConnectionSlot {
    live: Arc<AtomicUsize>,
}

impl ConnectionSlot {
    /// Takes one slot against `max`, or `None` when the acceptor is at its cap.
    /// The counter is only ever moved by `try_acquire` and `Drop`, so the CAS
    /// loop cannot overshoot `max` under concurrent accepts.
    pub fn try_acquire(live: &Arc<AtomicUsize>, max: usize) -> Option<Self> {
        let mut cur = live.load(Ordering::SeqCst);
        loop {
            if cur >= max {
                return None;
            }
            match live.compare_exchange(cur, cur + 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => {
                    return Some(Self {
                        live: Arc::clone(live),
                    });
                }
                Err(actual) => cur = actual,
            }
        }
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A request to be processed by the CUDA worker, with the replica route of the source connection.
/// The canary latency is measured in the worker around `serve()` (serve-only, DT-16
/// revised): measuring the wait time in the queue caused false positives for DEMOTE under normal load.
pub struct Job {
    /// Index of the export (slice) negotiated in the handshake — which window the worker serves (RF-L1).
    pub export: usize,
    pub req: Request,
    pub payload: Vec<u8>,
    pub credit: ReplyCredit,
    pub reply: Sender<Reply>,
}

/// Outcome of `serve()` to be written to the connection socket. `reply` is the 16-byte
/// NBD header (fixed `Copy` array, without allocation in the hot path — DT-8).
pub struct Reply {
    pub reply: [u8; SIMPLE_REPLY_LEN],
    pub data: Vec<u8>,
    pub credit: ReplyCredit,
    pub disconnect: bool,
    pub silent_disconnect: bool,
}

/// Worker channel message (DT-15). `Opened`/`Closed` control connection
/// accounting; `Job` is work; `ZeroExport` is the broker slice cleanup
/// (DT-17); and `Shutdown` is the internal broker-worker wake (DT-50). These
/// control variants are never exposed as NBD wire values.
pub enum WMsg {
    Opened,
    Job(Job),
    Closed,
    Shutdown,
    ZeroExport {
        base: u64,
        len: u64,
        done: Sender<bool>,
    },
}

/// Count of live connections in the worker (DT-15). `Opened` (from acceptor) always precedes
/// `Closed` (from reader) per connection, so `live` stays balanced. Reaching zero is a
/// transition signal; the owning runtime decides whether zero is terminal. Pure logic
/// (testable without GPU/sockets).
#[derive(Default)]
pub struct LiveCount {
    live: u32,
    opened: bool,
}

impl LiveCount {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_open(&mut self) {
        self.live += 1;
        self.opened = true;
    }

    /// Registers the closing of a connection; returns `true` once when **all** open
    /// connections have closed. An unbalanced
    /// `Closed` is ignored so it cannot emit a second terminal transition.
    pub fn on_close(&mut self) -> bool {
        if self.live == 0 {
            return false;
        }
        self.live -= 1;
        self.live == 0 && self.opened
    }

    pub fn live(&self) -> u32 {
        self.live
    }
}

/// A writable connection half that can also tear the whole connection down.
///
/// The writer owns only the write half, but a terminal write error has to
/// reach the reader as well: shutting the shared socket down is what turns a
/// dead writer into the reader's EOF, which then balances `Opened` with
/// `Closed` and leaves the block backend and the other tenants alone (AUD-05).
pub trait ConnWrite: Write {
    /// Best-effort teardown of the whole connection. Default is a no-op.
    fn shutdown_conn(&mut self) {}
}

impl ConnWrite for std::net::TcpStream {
    fn shutdown_conn(&mut self) {
        let _ = self.shutdown(std::net::Shutdown::Both);
    }
}

impl ConnWrite for UnixStream {
    fn shutdown_conn(&mut self) {
        let _ = self.shutdown(std::net::Shutdown::Both);
    }
}

/// Writer thread: drains `Reply`s and writes to the socket. Replies can go out of
/// order (each carries the NBD `handle`).
///
/// Terminates on socket error, on `disconnect`, or when the channel closes. A
/// terminal write failure ends the writer **immediately** — the next queued
/// reply must not be framed onto a socket that already refused this one
/// (AUD-05). The socket is shut down in both terminal cases so the reader sees
/// EOF and can balance `Opened` with `Closed`.
///
/// Every consumed reply releases one [`ReplyBudget`] reservation (AUD-06), so a
/// peer that stops reading stops being admitted new work instead of filling
/// memory.
pub fn spawn_writer<S: ConnWrite + Send + 'static>(
    mut stream: S,
    replies: Receiver<Reply>,
    budget: Arc<ReplyBudget>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let _writer_custody = budget.writer_guard();
        let mut draining = false;
        for r in replies.iter() {
            if !budget.contains(&r.credit) {
                budget.close();
                stream.shutdown_conn();
                return;
            }
            if r.silent_disconnect {
                budget.close();
                drop(r.data);
                if !budget.release(r.credit) {
                    stream.shutdown_conn();
                    return;
                }
                draining = true;
                if budget.pending() == 0 {
                    stream.shutdown_conn();
                    return;
                }
                continue;
            }
            let result = write_frame(&mut stream, &r.reply, &r.data);
            drop(r.data);
            if result.is_err() || r.disconnect {
                budget.close();
            }
            if !budget.release(r.credit) {
                budget.close();
                stream.shutdown_conn();
                return;
            }
            if let Err(e) = result {
                eprintln!("[ramsharedd] conn: fatal write error: {e}");
                stream.shutdown_conn();
                return;
            }
            if r.disconnect || (draining && budget.pending() == 0) {
                stream.shutdown_conn();
                return;
            }
        }
    })
}

/// Reader thread (generic over stream — Unix or TCP, RF-L2): handshake on its own thread
/// (DT-15 — error confined to the connection), negotiates export by name (RF-L1) and enqueues `Job`s with
/// the export index. `hs_writer` is the write handle (clone made by the acceptor) used only during the
/// handshake. Upon exiting (EOF/error/handshake failure) sends `WMsg::Closed` to balance `Opened`.
///
/// The stream is armed with [`HANDSHAKE_TIMEOUT`] around the handshake and the
/// deadline is cleared for the request phase (AUD-06). Each job is admitted
/// against `budget` before it is enqueued; temporary saturation waits in this
/// reader without closing the peer or growing the replica channel.
// Explicit reader boundary: stream + handshake writer + export table + budget
// + connection slot are each owned by this thread, so they cannot be bundled
// without inventing a second struct that only exists to satisfy an arity lint.
#[allow(clippy::too_many_arguments)]
pub fn spawn_reader<S, W2>(
    stream: S,
    mut hs_writer: W2,
    exports: Arc<Vec<Export>>,
    tx_flags: u16,
    jobs: SyncSender<WMsg>,
    reply_tx: Sender<Reply>,
    budget: Arc<ReplyBudget>,
    _slot: ConnectionSlot,
) -> JoinHandle<()>
where
    S: Read + ReadDeadline + Send + 'static,
    W2: Write + Send + 'static,
{
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        // Bound the handshake (AUD-06): a peer that connects and says nothing
        // must not hold this thread. The deadline is cleared for the request
        // phase, where a swap connection is long-lived and may sit idle.
        let _ = reader.get_ref().set_read_deadline(Some(HANDSHAKE_TIMEOUT));
        let Ok(idx) = server_handshake(&mut reader, &mut hs_writer, &exports, tx_flags)
            .inspect_err(|e| {
                eprintln!("[ramsharedd] conn: handshake failed: {e}");
                let _ = jobs.send(WMsg::Closed);
            })
        else {
            return;
        };
        let _ = reader.get_ref().set_read_deadline(None);

        drop(hs_writer); // handshake completed; from here on only the writer thread writes replies.
        let export_size = exports[idx].size; // anti-DoS based on negotiated export (RF-L1)

        let mut hdr = [0u8; REQUEST_LEN];
        let mut disconnect_seen = false;
        loop {
            if reader.read_exact(&mut hdr).is_err() {
                break; // EOF or socket error
            }
            let req = match parse_request(&hdr) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("[ramsharedd] conn: malformed request: {e}; disconnecting");
                    break;
                }
            };
            // Anti-DoS: physical upper bound for IPC buffers (16 MiB) to prevent memory exhaustion.
            if req.len > 16 * 1024 * 1024 {
                eprintln!(
                    "[ramsharedd] conn: request len {} exceeds physical IPC buffer bound (16 MiB); disconnecting",
                    req.len
                );
                break;
            }

            // Anti-DoS: a WRITE can never exceed the negotiated export (prevents allocating gigabytes).
            if req.cmd == Command::Write && req.len as u64 > export_size {
                eprintln!(
                    "[ramsharedd] conn: WRITE len {} exceeds export; disconnecting",
                    req.len
                );
                break;
            }

            let mut payload = Vec::new();
            if req.cmd == Command::Write {
                payload.resize(req.len as usize, 0);
                if reader.read_exact(&mut payload).is_err() {
                    break;
                }
            }

            // Admission (AUD-06): the reply for a READ carries up to `req.len`
            // bytes, and every reply carries its 16-byte header. A connection
            // that cannot fit the reply is over its budget and is closed.
            let reply_bytes = SIMPLE_REPLY_LEN
                + if req.cmd == Command::Read {
                    req.len as usize
                } else {
                    0
                };
            let Some(credit) = budget.admit_wait(reply_bytes) else {
                eprintln!(
                    "[ramsharedd] conn: reply admission closed or impossible (pending={} bytes={}); disconnecting",
                    budget.pending(),
                    budget.reserved_bytes()
                );
                break;
            };

            let job = Job {
                export: idx,
                req,
                payload,
                credit,
                reply: reply_tx.clone(),
            };
            disconnect_seen = job.req.cmd == Command::Disc;
            if let Err(error) = jobs.send(WMsg::Job(job)) {
                match error.0 {
                    WMsg::Job(job) => {
                        if !budget.release(job.credit) {
                            budget.close();
                        }
                    }
                    _ => budget.close(),
                }
                break; // worker terminated
            }
            if disconnect_seen {
                break;
            }
        }
        drop(reply_tx);
        if disconnect_seen {
            budget.wait_writer_finished();
        }
        let _ = jobs.send(WMsg::Closed);
    })
}

/// Wires an accepted connection to the worker: `WMsg::Opened` **before** spawning the reader (balances
/// `live`, DT-15), **unbounded** replica channel (DT-7), writer + reader. Generic over the
/// handles (Unix/TCP). Returns `false` if the worker terminated (the acceptor should stop).
///
/// A connection that does not fit under `live`/[`MAX_CONNECTIONS`] is refused
/// here, before any thread is spawned (AUD-06).
fn wire_conn<RS, WS>(
    rstream: RS,
    wstream: WS,
    hs_writer: WS,
    exports: &Arc<Vec<Export>>,
    tx_flags: u16,
    jobs: &SyncSender<WMsg>,
    live: &Arc<AtomicUsize>,
) -> bool
where
    RS: Read + ReadDeadline + Send + 'static,
    WS: ConnWrite + Send + 'static,
{
    let Some(slot) = ConnectionSlot::try_acquire(live, MAX_CONNECTIONS) else {
        eprintln!("[ramsharedd] conn: connection cap ({MAX_CONNECTIONS}) reached; refusing");
        return true; // the acceptor keeps serving the connections already wired
    };
    if jobs.send(WMsg::Opened).is_err() {
        return false; // worker terminated
    }
    let (reply_tx, reply_rx) = channel::<Reply>(); // unbounded (DT-7); bounded by ReplyBudget
    let budget = Arc::new(ReplyBudget::new(
        MAX_PENDING_REPLIES,
        MAX_PENDING_REPLY_BYTES,
    ));
    spawn_writer(wstream, reply_rx, Arc::clone(&budget));
    spawn_reader(
        rstream,
        hs_writer,
        Arc::clone(exports),
        tx_flags,
        jobs.clone(),
        reply_tx,
        budget,
        slot,
    );
    true
}

/// **Unix** Acceptor: accepts connections in a loop (N-agnostic) and wires each to the worker, negotiating
/// the export by name via `exports` (RF-L1). Each connection needs 2 clones of the stream (writer +
/// handshake) in addition to the read handle. Connections beyond [`MAX_CONNECTIONS`] are refused.
pub fn spawn_acceptor(
    listener: UnixListener,
    exports: Arc<Vec<Export>>,
    tx_flags: u16,
    jobs: SyncSender<WMsg>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let live = Arc::new(AtomicUsize::new(0));
        loop {
            let stream = match listener.accept() {
                Ok((s, _)) => s,
                Err(e) => {
                    eprintln!("[ramsharedd] accept failed: {e}");
                    break;
                }
            };

            let Ok(wstream) = stream.try_clone() else {
                eprintln!("[ramsharedd] try_clone (unix) wstream failed; skipping connection");
                continue;
            };

            let Ok(hs_writer) = stream.try_clone() else {
                eprintln!("[ramsharedd] try_clone (unix) hs_writer failed; skipping connection");
                continue;
            };

            if !wire_conn(stream, wstream, hs_writer, &exports, tx_flags, &jobs, &live) {
                break;
            }
        }
    })
}

/// **TCP** Acceptor (RF-L2): same design as Unix over `TcpListener`, feeding the SAME
/// `jobs` channel (one or more serving workers behind the dispatcher —
/// `serve_pool.rs`). `TCP_NODELAY` per connection (swap latency).
/// Connections beyond [`MAX_CONNECTIONS`] are refused.
pub fn spawn_acceptor_tcp(
    listener: TcpListener,
    exports: Arc<Vec<Export>>,
    tx_flags: u16,
    jobs: SyncSender<WMsg>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let live = Arc::new(AtomicUsize::new(0));
        loop {
            let stream = match listener.accept() {
                Ok((s, _)) => s,
                Err(e) => {
                    eprintln!("[ramsharedd] TCP accept failed: {e}");
                    break;
                }
            };

            let _ = stream.set_nodelay(true); // TCP_NODELAY: swap latency

            let Ok(wstream) = stream.try_clone() else {
                eprintln!("[ramsharedd] try_clone (tcp) wstream failed; skipping connection");
                continue;
            };

            let Ok(hs_writer) = stream.try_clone() else {
                eprintln!("[ramsharedd] try_clone (tcp) hs_writer failed; skipping connection");
                continue;
            };

            if !wire_conn(stream, wstream, hs_writer, &exports, tx_flags, &jobs, &live) {
                break;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    include!("conn_backpressure_tests.rs");
    include!("conn_wire_tests.rs");
    include!("conn_admission_tests.rs");
    use ramshared_block::handshake::NBD_OPT_EXPORT_NAME;
    use ramshared_block::protocol::{IHAVEOPT, NBD_REQUEST_MAGIC};
    use std::io::{self, Cursor};
    use std::net::{Shutdown, TcpStream};
    use std::os::unix::net::{UnixListener as TestUnixListener, UnixStream as TestUnixStream};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{TryRecvError, sync_channel};
    use std::time::Duration;

    static SOCKET_COUNTER: AtomicUsize = AtomicUsize::new(0);

    #[derive(Default)]
    struct WriterState {
        bytes: Mutex<Vec<u8>>,
        writes: AtomicUsize,
        flushes: AtomicUsize,
        blocks: AtomicUsize,
        shutdowns: AtomicUsize,
    }

    #[derive(Clone, Copy)]
    enum WriterFailure {
        Never,
        /// Terminal `BrokenPipe` on the `n`th `write` call (0-based).
        WriteAt(usize),
        Flush,
        /// `WouldBlock` on `write` calls in `[fail_at, fail_at+fail_count)`.
        Transient(usize, usize), // (fail_at, fail_count)
        /// Accepts at most `n` bytes per `write` (short writes, AUD-04).
        Short(usize),
        /// `WouldBlock` on writes that start at or after `total` accepted
        /// bytes, at most `fail_count` times; then accepts. Pins the resume
        /// offset at the header/payload transition (AUD-04).
        BlockAfter(usize, usize),
        /// `Interrupted` on the `n`th `write` call (AUD-04).
        InterruptedAt(usize),
    }

    struct TestWriter {
        state: Arc<WriterState>,
        failure: WriterFailure,
    }

    impl TestWriter {
        fn new(state: Arc<WriterState>, failure: WriterFailure) -> Self {
            Self { state, failure }
        }
    }

    impl Write for TestWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let write_index = self.state.writes.fetch_add(1, Ordering::SeqCst);
            if let WriterFailure::WriteAt(expected) = self.failure
                && write_index == expected
            {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "test write failure",
                ));
            }
            if let WriterFailure::InterruptedAt(expected) = self.failure
                && write_index == expected
            {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "test interrupted",
                ));
            }
            if let WriterFailure::Transient(fail_at, fail_count) = self.failure
                && write_index >= fail_at
                && write_index < fail_at + fail_count
            {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "test transient failure",
                ));
            }
            if let WriterFailure::BlockAfter(total, fail_count) = self.failure {
                let accepted = self.state.bytes.lock().unwrap().len();
                if accepted >= total && self.state.blocks.load(Ordering::SeqCst) < fail_count {
                    self.state.blocks.fetch_add(1, Ordering::SeqCst);
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "test transient block",
                    ));
                }
            }
            let mut take = bytes.len();
            if let WriterFailure::Short(n) = self.failure {
                take = take.min(n.max(1));
            }
            self.state
                .bytes
                .lock()
                .unwrap()
                .extend_from_slice(&bytes[..take]);
            Ok(take)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.state.flushes.fetch_add(1, Ordering::SeqCst);
            if matches!(self.failure, WriterFailure::Flush) {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "test flush failure",
                ));
            }
            Ok(())
        }
    }

    impl ConnWrite for TestWriter {
        fn shutdown_conn(&mut self) {
            self.state.shutdowns.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn join_with_deadline(handle: JoinHandle<()>) {
        let (done_tx, done_rx) = sync_channel(1);
        std::thread::spawn(move || {
            let _ = done_tx.send(handle.join().is_ok());
        });
        assert!(
            done_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap_or(false),
            "connection thread must terminate within the bounded test deadline"
        );
    }

    fn recv_with_deadline<T>(rx: &Receiver<T>) -> T {
        rx.recv_timeout(Duration::from_secs(1)).unwrap_or_else(|_| {
            panic!("connection message must arrive within the bounded test deadline")
        })
    }

    fn assert_only_closed(rx: &Receiver<WMsg>) {
        assert!(matches!(recv_with_deadline(rx), WMsg::Closed));
        assert!(matches!(
            rx.try_recv(),
            Err(TryRecvError::Empty | TryRecvError::Disconnected)
        ));
    }

    /// The only export the test tables carry. Every handshake that means to
    /// succeed must name it: an empty name is refused (AUD-03), so a `b""`
    /// probe closes before the code under test runs.
    const EXPORT_NAME: &str = "default";

    fn one_export(size: u64) -> Arc<Vec<Export>> {
        Arc::new(vec![Export {
            name: EXPORT_NAME.to_string(),
            size,
        }])
    }

    /// Production-sized budget for tests that do not exercise admission (AUD-06).
    fn test_budget() -> Arc<ReplyBudget> {
        Arc::new(ReplyBudget::new(
            MAX_PENDING_REPLIES,
            MAX_PENDING_REPLY_BYTES,
        ))
    }

    fn test_slot() -> ConnectionSlot {
        let live = Arc::new(AtomicUsize::new(0));
        ConnectionSlot::try_acquire(&live, MAX_CONNECTIONS).expect("fresh counter has room")
    }

    /// `spawn_writer` with the production budget; the connection slot is not
    /// the writer's concern.
    fn spawn_test_writer<S: ConnWrite + Send + 'static>(
        stream: S,
        replies: Receiver<Reply>,
    ) -> (JoinHandle<()>, Arc<ReplyBudget>) {
        let budget = test_budget();
        (spawn_writer(stream, replies, Arc::clone(&budget)), budget)
    }

    /// `spawn_reader` with a fresh budget and slot. Tests that exercise
    /// admission pass their own.
    fn spawn_test_reader<S, W2>(
        stream: S,
        hs_writer: W2,
        exports: Arc<Vec<Export>>,
        tx_flags: u16,
        jobs: SyncSender<WMsg>,
        reply_tx: Sender<Reply>,
    ) -> JoinHandle<()>
    where
        S: Read + ReadDeadline + Send + 'static,
        W2: Write + Send + 'static,
    {
        spawn_reader(
            stream,
            hs_writer,
            exports,
            tx_flags,
            jobs,
            reply_tx,
            test_budget(),
            test_slot(),
        )
    }

    fn export_name_handshake(name: &[u8]) -> Vec<u8> {
        let mut wire = Vec::new();
        wire.extend_from_slice(&(1u32 << 1).to_be_bytes()); // NBD_FLAG_C_NO_ZEROES
        wire.extend_from_slice(&IHAVEOPT.to_be_bytes());
        wire.extend_from_slice(&NBD_OPT_EXPORT_NAME.to_be_bytes());
        wire.extend_from_slice(&(name.len() as u32).to_be_bytes());
        wire.extend_from_slice(name);
        wire
    }

    fn request_bytes(command: u16, handle: u64, len: u32, magic: u32) -> [u8; REQUEST_LEN] {
        let mut wire = [0u8; REQUEST_LEN];
        wire[0..4].copy_from_slice(&magic.to_be_bytes());
        wire[4..6].copy_from_slice(&0u16.to_be_bytes());
        wire[6..8].copy_from_slice(&command.to_be_bytes());
        wire[8..16].copy_from_slice(&handle.to_be_bytes());
        wire[16..24].copy_from_slice(&0u64.to_be_bytes());
        wire[24..28].copy_from_slice(&len.to_be_bytes());
        wire
    }

    fn fixture_credit() -> ReplyCredit {
        ReplyBudget::new(1, 0).admit(0).unwrap()
    }

    fn reply(header: u8, data: &[u8], disconnect: bool, credit: ReplyCredit) -> Reply {
        Reply {
            reply: [header; SIMPLE_REPLY_LEN],
            data: data.to_vec(),
            credit,
            disconnect,
            silent_disconnect: false,
        }
    }

    fn admitted_reply(budget: &ReplyBudget, header: u8, data: &[u8], disconnect: bool) -> Reply {
        reply(
            header,
            data,
            disconnect,
            budget.admit(SIMPLE_REPLY_LEN + data.len()).unwrap(),
        )
    }

    fn socket_path(label: &str) -> PathBuf {
        let suffix = SOCKET_COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "ramshared-conn-{label}-{}-{suffix}.sock",
            std::process::id()
        ))
    }

    fn dummy_req() -> Request {
        Request {
            flags: 0,
            cmd: Command::Read,
            handle: 1,
            offset: 0,
            len: 0,
        }
    }

    #[test]
    fn job_reply_roundtrip() {
        let (tx, _rx) = channel::<Reply>();
        let job = Job {
            export: 0,
            req: dummy_req(),
            payload: vec![1, 2, 3],
            credit: fixture_credit(),
            reply: tx,
        };
        assert_eq!(job.req.handle, 1);
        assert_eq!(job.payload, vec![1, 2, 3]);
        let rep = Reply {
            reply: [0u8; SIMPLE_REPLY_LEN],
            data: vec![9, 8, 7],
            credit: fixture_credit(),
            disconnect: false,
            silent_disconnect: false,
        };
        assert_eq!(rep.data, vec![9, 8, 7]);
        assert!(!rep.disconnect);
    }

    #[test]
    fn chan_cap_is_bounded() {
        let (tx, _rx) = sync_channel::<u8>(2);
        assert!(tx.try_send(1).is_ok());
        assert!(tx.try_send(2).is_ok());
        assert!(
            tx.try_send(3).is_err(),
            "must reject beyond the cap (backpressure)"
        );
    }

    // DT-18 / F-3/F-5: deterministic zero-live transition.
    #[test]
    fn live_count_reports_zero_after_all_closed() {
        let mut lc = LiveCount::new();
        lc.on_open(); // live=1
        lc.on_open(); // live=2
        assert!(!lc.on_close(), "live=1 still"); // live=1
        assert!(lc.on_close(), "live=0 emits the quiescent transition"); // live=0
    }

    // DT-18 / F-6: failed handshake = immediate Opened (acceptor) + Closed (reader); balanced.
    #[test]
    fn live_count_balanced_open_then_close() {
        let mut lc = LiveCount::new();
        lc.on_open();
        assert!(lc.on_close(), "1 connection opened and closed -> zero live");
    }

    #[test]
    fn live_count_never_stops_before_any_open() {
        let mut lc = LiveCount::new();
        assert!(
            !lc.on_close(),
            "without Opened does not transition spuriously"
        );
        assert_eq!(lc.live(), 0);
    }

    #[test]
    fn live_count_refuses_duplicate_closed() {
        let mut lc = LiveCount::new();
        lc.on_open();
        assert!(lc.on_close(), "the balanced close reaches zero live");
        assert!(
            !lc.on_close(),
            "a duplicate Closed must not emit another zero-live transition"
        );
        assert_eq!(lc.live(), 0);
    }
}
