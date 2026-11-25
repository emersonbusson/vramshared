// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/serve_pool.rs
//! The bounded serve pool (SPEC `wsl2-nbd-serving-throughput`, ITEM-2, DT-1).
//!
//! The daemon's serving loop is the *dispatcher*: it keeps the `jobs`
//! receiver and today's control handling verbatim (connection lifecycle,
//! canary, residency, demote/recovery). Only `WMsg::Job` is forwarded here:
//! a bounded queue consumed by N serve workers, each running the same inner
//! body the serialized loop ran — receive one job, `serve()`, send one
//! `Reply`. Replies stay handle-addressed and the per-connection writer
//! threads keep socket writes serialized (DT-2).
//!
//! Decisions this module owns:
//!
//! - **DT-4** — N = 1 is today's behavior, byte for byte: one worker, one
//!   FIFO queue, the same order.
//! - **DT-5** — a panicked worker marks the pool degraded. `submit` then
//!   refuses every new job ([`SubmitError::Degraded`]) and the caller answers
//!   with [`refusal_reply`]; the pool never shrinks silently.
//! - **DT-6** — the worker count is the caller's (`--serve-threads`,
//!   1..=16, default 4).
//!
//! A full queue ([`SubmitError::Full`]) is backpressure, not failure: the
//! dispatcher serves that one job inline — it is I/O-free otherwise and
//! stays responsive — and records it with [`ServePool::record_served`] so
//! the published counter stays a total.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use ramshared_block::{NBD_EIO, SharedBlockBackend, encode_simple_reply, serve};

use ramshared_wsl2d::{Job, Reply};

/// DT-6: the default worker count when `--serve-threads` is not given. Four
/// exceeds the fallback's ~7.6 k IOPS ceiling while staying inside the
/// kernel's 128-deep NBD queue.
pub const DEFAULT_SERVE_THREADS: usize = 4;

/// DT-6: the bound `--serve-threads` validates against. More threads than the
/// kernel's 128-deep NBD queue cannot help.
pub const MAX_SERVE_THREADS: usize = 16;

/// A job the pool refused before it was ever served.
pub enum SubmitError {
    /// The queue is at capacity: the caller serves this job inline.
    Full(Job),
    /// A worker died: the pool never shrinks silently (DT-5). The caller
    /// answers the job with [`refusal_reply`] and closes the connection.
    Degraded(Job),
}

/// The refusal reply for a job the pool will not serve: a named I/O error on
/// the job's own handle, with the connection closed.
pub fn refusal_reply(
    req: &ramshared_block::Request,
    credit: ramshared_wsl2d::reply_budget::ReplyCredit,
) -> Reply {
    Reply {
        reply: encode_simple_reply(NBD_EIO, req.handle),
        data: Vec::new(),
        credit,
        disconnect: true,
        silent_disconnect: req.cmd == ramshared_block::Command::Disc,
    }
}

/// The bounded serve pool: the queue, its workers, and the two published
/// counters (degradation and served jobs).
pub struct ServePool {
    queue: SyncSender<Job>,
    threads: Vec<JoinHandle<()>>,
    degraded: Arc<AtomicBool>,
    served: Arc<AtomicU64>,
}

impl ServePool {
    /// The configured worker count (DT-6). Published by the cache-status
    /// face (ITEM-3) and asserted by the pool tests.
    #[allow(dead_code)]
    pub fn threads(&self) -> usize {
        self.threads.len()
    }

    /// Whether a worker has died. Once true it never goes back: the pool
    /// refuses new jobs for the rest of the runtime (DT-5).
    pub fn is_degraded(&self) -> bool {
        self.degraded.load(Ordering::Acquire)
    }

    /// Jobs the pool served so far, plus the inline backpressure fallbacks
    /// the dispatcher recorded. Published by the cache-status face (ITEM-3)
    /// and asserted by the pool tests.
    #[allow(dead_code)]
    pub fn served(&self) -> u64 {
        self.served.load(Ordering::Relaxed)
    }

    /// Bookkeeping for a job the dispatcher served inline (queue-full
    /// backpressure) so the published counter stays a total.
    pub fn record_served(&self) {
        self.served.fetch_add(1, Ordering::Relaxed);
    }

    /// Queue one job for the workers. `Ok(())` means a worker owns it now;
    /// the job comes back inside the error otherwise.
    pub fn submit(&self, job: Job) -> Result<(), SubmitError> {
        if self.is_degraded() {
            return Err(SubmitError::Degraded(job));
        }
        match self.queue.try_send(job) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(job)) => Err(SubmitError::Full(job)),
            // Every receiver is gone: that is a dead pool, same as degraded.
            Err(TrySendError::Disconnected(job)) => Err(SubmitError::Degraded(job)),
        }
    }

    /// Stop the pool: close the queue and join every worker. Queued jobs are
    /// drained first; a degraded pool exits as soon as its workers wake.
    pub fn join(self) {
        let ServePool { queue, threads, .. } = self;
        drop(queue);
        for thread in threads {
            let _ = thread.join();
        }
    }
}

/// Spawn `workers` serve workers over a shared backend. The queue is the
/// bound (DT-1: `CHAN_CAP`); the workers share it through a mutex held only
/// across the blocking receive, never across `serve()`.
pub fn spawn_serve_pool<B: SharedBlockBackend + 'static>(
    backend: Arc<B>,
    workers: usize,
    queue_cap: usize,
) -> ServePool {
    assert!(workers >= 1, "the serve pool needs at least one worker");
    let (queue, receiver) = sync_channel::<Job>(queue_cap);
    let receiver = Arc::new(Mutex::new(receiver));
    let degraded = Arc::new(AtomicBool::new(false));
    let served = Arc::new(AtomicU64::new(0));
    let mut threads = Vec::with_capacity(workers);
    for _ in 0..workers {
        let receiver = Arc::clone(&receiver);
        let backend = Arc::clone(&backend);
        let degraded = Arc::clone(&degraded);
        let served = Arc::clone(&served);
        threads.push(std::thread::spawn(move || {
            run_worker(receiver, backend, degraded, served);
        }));
    }
    ServePool {
        queue,
        threads,
        degraded,
        served,
    }
}

fn run_worker<B: SharedBlockBackend + 'static>(
    queue: Arc<Mutex<Receiver<Job>>>,
    backend: Arc<B>,
    degraded: Arc<AtomicBool>,
    served: Arc<AtomicU64>,
) {
    // A panicked worker is the degradation signal (DT-5): the flag is set
    // from the unwind so `submit` refuses every later job. The panic message
    // still reaches the process's hook: the failure stays loud.
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        loop {
            let job = {
                let receiver = match queue.lock() {
                    Ok(receiver) => receiver,
                    // The queue mutex is held only across `recv`, which does
                    // not panic: poisoning cannot happen in practice.
                    Err(_) => return,
                };
                receiver.recv()
            };
            let job = match job {
                Ok(job) => job,
                // Dispatcher dropped the queue: normal shutdown, drained.
                Err(_) => return,
            };
            if degraded.load(Ordering::Acquire) {
                // No job is served after the pool is degraded.
                let _ = job.reply.send(refusal_reply(&job.req, job.credit));
                return;
            }
            let mut shared = &*backend;
            let out = serve(&job.req, &job.payload, &mut shared);
            publish_served_reply(
                &served,
                Reply {
                    reply: out.reply,
                    data: out.read_data,
                    credit: job.credit,
                    disconnect: out.disconnect,
                    silent_disconnect: job.req.cmd == ramshared_block::Command::Disc,
                },
                |reply| {
                    let _ = job.reply.send(reply);
                },
            );
        }
    }));
    if outcome.is_err() {
        degraded.store(true, Ordering::SeqCst);
    }
}

fn publish_served_reply(served: &AtomicU64, reply: Reply, send: impl FnOnce(Reply)) {
    served.fetch_add(1, Ordering::Relaxed);
    send(reply);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use ramshared_block::{Command, IoError, Request};
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    use ramshared_wsl2d::CHAN_CAP;

    #[test]
    fn refusal_suppresses_only_disconnect_reply() {
        let (mut job, _) = read_job(42, 0, 8);
        let read_refusal = refusal_reply(&job.req, job.credit);
        assert!(read_refusal.disconnect);
        assert!(!read_refusal.silent_disconnect);
        job.req.cmd = Command::Disc;
        job.credit = ramshared_wsl2d::reply_budget::ReplyBudget::new(1, 0)
            .admit(0)
            .unwrap();
        let disc_refusal = refusal_reply(&job.req, job.credit);
        assert!(disc_refusal.disconnect);
        assert!(disc_refusal.silent_disconnect);
    }

    #[test]
    fn served_counter_precedes_reply_publication() {
        let served = AtomicU64::new(0);
        let (job, receiver) = read_job(42, 0, 8);
        let reply = refusal_reply(&job.req, job.credit);
        publish_served_reply(&served, reply, |reply| {
            assert_eq!(
                served.load(Ordering::Relaxed),
                1,
                "a published reply must not outrun its counter"
            );
            job.reply.send(reply).unwrap();
        });
        assert!(recv_reply(&receiver, "counted reply").disconnect);
    }

    #[test]
    fn completed_service_is_counted_when_the_receiver_is_gone() {
        let served = AtomicU64::new(0);
        let (job, receiver) = read_job(42, 0, 8);
        drop(receiver);
        publish_served_reply(&served, refusal_reply(&job.req, job.credit), |reply| {
            assert!(job.reply.send(reply).is_err());
        });
        assert_eq!(served.load(Ordering::Relaxed), 1);
    }

    /// A minimal shared backend: a byte buffer with an optional poison
    /// offset that panics a worker, plus the counters the tests assert on.
    struct PoolBackend {
        bytes: Mutex<Vec<u8>>,
        poison_at: Option<u64>,
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
        served_reads: AtomicU64,
        read_delay: Duration,
    }

    impl PoolBackend {
        fn new(capacity: usize) -> Self {
            PoolBackend {
                bytes: Mutex::new(vec![0; capacity]),
                poison_at: None,
                in_flight: AtomicUsize::new(0),
                max_in_flight: AtomicUsize::new(0),
                served_reads: AtomicU64::new(0),
                read_delay: Duration::ZERO,
            }
        }
    }

    impl SharedBlockBackend for PoolBackend {
        fn size_bytes(&self) -> u64 {
            self.bytes.lock().expect("bytes").len() as u64
        }

        fn block_size(&self) -> u32 {
            8
        }

        fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<(), IoError> {
            if self.poison_at == Some(off) {
                panic!("poisoned read at {off}");
            }
            let in_flight = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(in_flight, Ordering::SeqCst);
            if !self.read_delay.is_zero() {
                std::thread::sleep(self.read_delay);
            }
            let bytes = self.bytes.lock().expect("bytes");
            let start = off as usize;
            let end = start + buf.len();
            if end > bytes.len() {
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                return Err(IoError(format!("read out of range: {start}..{end}")));
            }
            buf.copy_from_slice(&bytes[start..end]);
            self.served_reads.fetch_add(1, Ordering::SeqCst);
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        }

        fn write_at(&self, off: u64, data: &[u8]) -> Result<(), IoError> {
            let mut bytes = self.bytes.lock().expect("bytes");
            let start = off as usize;
            let end = start + data.len();
            if end > bytes.len() {
                return Err(IoError(format!("write out of range: {start}..{end}")));
            }
            bytes[start..end].copy_from_slice(data);
            Ok(())
        }

        fn flush(&self) -> Result<(), IoError> {
            Ok(())
        }
    }

    fn read_job(handle: u64, offset: u64, len: u32) -> (Job, Receiver<Reply>) {
        let (reply, receiver) = std::sync::mpsc::channel();
        (
            Job {
                export: 0,
                req: Request {
                    flags: 0,
                    cmd: Command::Read,
                    handle,
                    offset,
                    len,
                },
                payload: Vec::new(),
                credit: ramshared_wsl2d::reply_budget::ReplyBudget::new(1, 0)
                    .admit(0)
                    .unwrap(),
                reply,
            },
            receiver,
        )
    }

    fn recv_reply(receiver: &Receiver<Reply>, what: &str) -> Reply {
        receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|error| panic!("no reply for {what}: {error}"))
    }

    /// at N = 1 the pool is today's serialized loop — the same
    /// replies, byte for byte, in the same order.
    #[test]
    fn serve_pool_single_thread_matches_the_serialized_loop() {
        let pool_backend = Arc::new(PoolBackend::new(64));
        let direct_backend = PoolBackend::new(64);
        let pool = spawn_serve_pool(Arc::clone(&pool_backend), 1, CHAN_CAP);

        let mut receivers = Vec::new();
        for handle in 0..8u64 {
            let (job, receiver) = read_job(handle, 0, 8);
            let mut direct = &direct_backend;
            let expected = serve(&job.req, &job.payload, &mut direct);
            assert!(pool.submit(job).is_ok(), "job {handle} must be accepted");
            receivers.push((expected, receiver));
        }

        for (handle, (expected, receiver)) in receivers.into_iter().enumerate() {
            let reply = recv_reply(&receiver, "the serialized-equivalence job");
            assert_eq!(
                reply.reply, expected.reply,
                "N = 1 must produce today's reply bytes for handle {handle}"
            );
            assert_eq!(reply.data, expected.read_data, "read payload differs");
            assert_eq!(reply.disconnect, expected.disconnect);
        }

        assert!(!pool.is_degraded());
        assert_eq!(pool.threads(), 1);
        assert_eq!(pool.served(), 8);
        pool.join();
    }

    /// DT-6: with N = 4 the pool serves every queued job and really runs
    /// more than one `serve()` at a time.
    #[test]
    fn serve_pool_serves_all_queued_jobs_within_the_budget() {
        let backend = Arc::new(PoolBackend {
            read_delay: Duration::from_millis(1),
            ..PoolBackend::new(4096)
        });
        let pool = spawn_serve_pool(Arc::clone(&backend), 4, CHAN_CAP);

        let jobs = 32u64;
        let mut receivers = Vec::new();
        for handle in 0..jobs {
            let (job, receiver) = read_job(handle, 0, 8);
            assert!(pool.submit(job).is_ok(), "job {handle} must be accepted");
            receivers.push((handle, receiver));
        }

        for (handle, receiver) in receivers {
            let reply = recv_reply(&receiver, "a queued job");
            assert_eq!(
                u32::from_be_bytes([
                    reply.reply[4],
                    reply.reply[5],
                    reply.reply[6],
                    reply.reply[7]
                ]),
                ramshared_block::NBD_OK,
                "job {handle} must be acknowledged"
            );
            assert_eq!(reply.data.len(), 8);
        }

        assert_eq!(backend.served_reads.load(Ordering::SeqCst), jobs);
        assert!(
            backend.max_in_flight.load(Ordering::SeqCst) >= 2,
            "four workers must run more than one serve() at a time"
        );
        assert_eq!(pool.served(), jobs);
        pool.join();
    }

    /// invariant checkDT-5: a panicked worker degrades the pool, and no job
    /// is served after that — new jobs are refused, never silently dropped.
    #[test]
    fn a_panicked_serve_worker_degrades_the_pool_and_refuses_new_jobs() {
        let backend = Arc::new(PoolBackend {
            poison_at: Some(8),
            ..PoolBackend::new(64)
        });
        let pool = spawn_serve_pool(Arc::clone(&backend), 2, CHAN_CAP);

        let (poison, poison_receiver) = read_job(0, 8, 8);
        assert!(pool.submit(poison).is_ok());

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !pool.is_degraded() {
            assert!(
                std::time::Instant::now() < deadline,
                "the panicked worker must mark the pool degraded"
            );
            std::thread::sleep(Duration::from_millis(5));
        }

        // The poison job died with its worker: no reply is ever sent.
        assert!(
            poison_receiver
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "the poisoned job must not be acknowledged"
        );

        // A new job is refused with ownership returned: the caller replies
        // with the named refusal, and the backend never sees the read.
        let (valid, valid_receiver) = read_job(1, 0, 8);
        let refused = pool.submit(valid);
        let job = match refused {
            Err(SubmitError::Degraded(job)) => job,
            _ => panic!("a degraded pool must refuse new jobs"),
        };
        let _ = job.reply.send(refusal_reply(&job.req, job.credit));

        let refusal = recv_reply(&valid_receiver, "the refused job");
        assert_eq!(
            u32::from_be_bytes([
                refusal.reply[4],
                refusal.reply[5],
                refusal.reply[6],
                refusal.reply[7]
            ]),
            NBD_EIO,
            "the refusal must be a named I/O error"
        );
        assert!(refusal.disconnect, "the refusal must close the connection");
        assert_eq!(
            backend.served_reads.load(Ordering::SeqCst),
            0,
            "no job may be served after the pool is degraded"
        );
        assert_eq!(pool.served(), 0);

        // Joining is bounded: the surviving worker wakes on the closed queue.
        pool.join();
    }
}
