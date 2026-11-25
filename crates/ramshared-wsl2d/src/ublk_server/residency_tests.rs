// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/ublk_server/residency_tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::join_tests::{FakeQueue, descriptor_bytes};
use super::residency_worker::{
    report_stuck_drain, serve_ublk_residency, serve_ublk_residency_default, spawn_residency_handle,
};
use super::*;
use crate::swap::{DEMOTE_STUCK_DETECTION_WINDOW, SwapoffOutcome};
use crate::{CANARY_BYTES, CanaryProbe, ResidencyConfig};
use ramshared_vram::VramError;
use ramshared_vram::VramMemory;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

/// **Fake VRAM backend in RAM (`Vec<u8>`): exercises the generic loop
/// [`serve_ublk_residency`] (serve + §9.4 + teardown) **without GPU/ublk/root** — safe
/// on WSL2. The real e2e with CUDA+ublk is the `#[ignore]` `dt3_vram_residency_*` (gated).
struct FakeVram(Vec<u8>, Option<Arc<AtomicU32>>);

impl FakeVram {
    fn new(len: usize) -> Self {
        Self(vec![0u8; len], None)
    }

    fn monitored(len: usize, zero_calls: Arc<AtomicU32>) -> Self {
        Self(vec![0u8; len], Some(zero_calls))
    }
}

impl VramMemory for FakeVram {
    fn len(&self) -> usize {
        self.0.len()
    }

    fn zero(&mut self) -> Result<(), VramError> {
        if let Some(calls) = &self.1 {
            calls.fetch_add(1, Ordering::Relaxed);
        }
        self.0.iter_mut().for_each(|b| *b = 0);
        Ok(())
    }

    fn read_at(&self, off: u64, dst: &mut [u8]) -> Result<(), VramError> {
        let off = off as usize;
        let end = off
            .checked_add(dst.len())
            .filter(|&e| e <= self.0.len())
            .ok_or(VramError::OutOfRange {
                off: off as u64,
                len: dst.len() as u64,
                size: self.0.len() as u64,
            })?;
        dst.copy_from_slice(&self.0[off..end]);
        Ok(())
    }

    fn write_at(&mut self, off: u64, src: &[u8]) -> Result<(), VramError> {
        let off = off as usize;
        let end = off
            .checked_add(src.len())
            .filter(|&e| e <= self.0.len())
            .ok_or(VramError::OutOfRange {
                off: off as u64,
                len: src.len() as u64,
                size: self.0.len() as u64,
            })?;
        self.0[off..end].copy_from_slice(src);
        Ok(())
    }
}

const BS: u32 = 4096;

fn read_work(tag: u16) -> ublk::IoWork {
    ublk::IoWork {
        qid: 0,
        tag,
        buffer_addr: 0,
        req: Request {
            flags: 0,
            cmd: Command::Read,
            handle: tag as u64,
            offset: 0,
            len: BS,
        },
        payload: vec![0u8; BS as usize],
    }
}

/// Directs `n` Reads through the generic loop with the given `mem_free` and returns the number of
/// DEMOTEs. `_reply_rx` remains alive until join (otherwise `reply_tx.send` fails and the loop
/// breaks early); `work_tx` is dropped to terminate the loop. Without real swap: the
/// non-existent `swap_dev` makes `swapoff` fail without side effects.
fn run_loop(cfg: ResidencyConfig, mem_free: fn() -> Option<u64>, n: u16) -> u32 {
    let (work_tx, work_rx) = mpsc::sync_channel::<ublk::IoWork>(4);
    let (reply_tx, _reply_rx) = mpsc::channel::<WorkerReply>();
    let demotes = Arc::new(AtomicU32::new(0));
    let backend = VramBackend::new(FakeVram::new((BS as usize) * 8), BS);
    let probe = CanaryProbe::new(FakeVram::new(CANARY_BYTES));
    let demotes_t = Arc::clone(&demotes);

    let worker = thread::spawn(move || {
        serve_ublk_residency(
            backend,
            probe,
            mem_free,
            work_rx,
            reply_tx,
            "/dev/ramshared-no-such-swap",
            cfg,
            demotes_t,
            |started| u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            |_| {
                let (tx, rx) = mpsc::channel();
                let _ = tx.send(SwapoffOutcome::Rejected { exit_code: Some(1) });
                rx
            },
            // A real 30 s window never elapses inside these tests.
            DEMOTE_STUCK_DETECTION_WINDOW,
            |_| {},
            || Some(()), // the fixture lifecycle proves absence
        )
    });

    for tag in 0..n {
        work_tx.send(read_work(tag)).expect("enfileira work");
    }
    drop(work_tx); // terminates the loop
    worker.join().expect("worker join").expect("serve ok");
    demotes.load(Ordering::Relaxed)
}

#[test]
fn demote_fires_when_free_below_floor() {
    // high latency_mult -> the canary §9 never fires (fake serve is ~0us); the DEMOTE
    // comes from the probe §9.4 (free=0 < free_floor). consecutive=1 -> 1 degraded sample.
    let cfg = ResidencyConfig {
        latency_mult: 4096,
        consecutive: 1,
        free_floor_bytes: 1 << 30, // 1 GiB; free=0 fica abaixo
    };
    // >=64 reads for the cadence §9.4 (CANARY_EVERY) to fire at least once.
    let demotes = run_loop(cfg, || Some(0), 80);
    assert!(
        demotes >= 1,
        "esperava >=1 DEMOTE por free<floor, obteve {demotes}"
    );
}

#[test]
fn no_demote_when_healthy() {
    // abundant free + fast serve -> no trigger (§9 nor §9.4).
    let cfg = ResidencyConfig {
        latency_mult: 4096,
        consecutive: 1,
        free_floor_bytes: 0,
    };
    let demotes = run_loop(cfg, || Some(u64::MAX), 80);
    assert_eq!(demotes, 0, "should not have DEMOTE with healthy VRAM");
}

#[test]
fn latency_demote_uses_injected_clock_and_swapoff_result() {
    let (work_tx, work_rx) = mpsc::sync_channel::<ublk::IoWork>(32);
    let (reply_tx, _reply_rx) = mpsc::channel::<WorkerReply>();
    let demotes = Arc::new(AtomicU32::new(0));
    let demotes_worker = Arc::clone(&demotes);
    let swapoff_paths = Arc::new(Mutex::new(Vec::new()));
    let observed_paths = Arc::clone(&swapoff_paths);
    let pending_swapoff = Arc::new(Mutex::new(None::<mpsc::Sender<SwapoffOutcome>>));
    let clock_swapoff = Arc::clone(&pending_swapoff);
    let backend = VramBackend::new(FakeVram::new((BS as usize) * 8), BS);
    let probe = CanaryProbe::new(FakeVram::new(CANARY_BYTES));
    let mut sample = 0usize;

    let worker = thread::spawn(move || {
        serve_ublk_residency(
            backend,
            probe,
            || Some(u64::MAX),
            work_rx,
            reply_tx,
            "/dev/ublkb-test",
            ResidencyConfig {
                latency_mult: 10,
                consecutive: 1,
                free_floor_bytes: 0,
            },
            demotes_worker,
            move |_| {
                sample += 1;
                if sample == 18
                    && let Some(tx) = clock_swapoff.lock().expect("pending swapoff mutex").take()
                {
                    tx.send(SwapoffOutcome::Drained)
                        .expect("complete fake swapoff");
                }
                match sample {
                    1..=16 => 10,
                    17 => 1_000,
                    _ => 10,
                }
            },
            move |swap_dev| {
                observed_paths
                    .lock()
                    .expect("swapoff path mutex")
                    .push(swap_dev.to_string());
                let (tx, rx) = mpsc::channel();
                *pending_swapoff.lock().expect("pending swapoff mutex") = Some(tx);
                rx
            },
            // A real 30 s window never elapses inside these tests.
            DEMOTE_STUCK_DETECTION_WINDOW,
            |_| {},
            || Some(()), // the fixture lifecycle proves absence
        )
    });

    for tag in 0..19 {
        work_tx.send(read_work(tag)).expect("queue read work");
    }
    drop(work_tx);
    worker
        .join()
        .expect("residency worker join")
        .expect("serve ok");

    assert_eq!(demotes.load(Ordering::Relaxed), 1);
    assert_eq!(
        *swapoff_paths.lock().expect("swapoff path mutex"),
        vec!["/dev/ublkb-test"]
    );
}

#[test]
fn residency_clears_a_disconnected_swapoff_observation() {
    let (work_tx, work_rx) = mpsc::sync_channel::<ublk::IoWork>(96);
    let (reply_tx, _reply_rx) = mpsc::channel::<WorkerReply>();
    let demotes = Arc::new(AtomicU32::new(0));
    let demotes_worker = Arc::clone(&demotes);
    let backend = VramBackend::new(FakeVram::new((BS as usize) * 8), BS);
    let probe = CanaryProbe::new(FakeVram::new(CANARY_BYTES));

    let worker = thread::spawn(move || {
        serve_ublk_residency(
            backend,
            probe,
            || Some(0),
            work_rx,
            reply_tx,
            "/dev/ublkb-disconnected-test",
            ResidencyConfig {
                latency_mult: 4096,
                consecutive: 1,
                free_floor_bytes: 1,
            },
            demotes_worker,
            |started| u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            |_| {
                let (_tx, rx) = mpsc::channel();
                rx
            },
            // A real 30 s window never elapses inside these tests.
            DEMOTE_STUCK_DETECTION_WINDOW,
            |_| {},
            || Some(()), // the fixture lifecycle proves absence
        )
    });

    for tag in 0..80 {
        work_tx.send(read_work(tag)).expect("queue read work");
    }
    drop(work_tx);
    worker
        .join()
        .expect("residency worker join")
        .expect("serve ok");
    assert_eq!(demotes.load(Ordering::Relaxed), 1);
}

#[test]
fn residency_ignores_non_vram_flush_for_canary_and_probe() {
    let (work_tx, work_rx) = mpsc::sync_channel::<ublk::IoWork>(1);
    let (reply_tx, reply_rx) = mpsc::channel::<WorkerReply>();
    let demotes = Arc::new(AtomicU32::new(0));
    let backend = VramBackend::new(FakeVram::new((BS as usize) * 8), BS);
    let probe = CanaryProbe::new(FakeVram::new(CANARY_BYTES));
    let mut flush = read_work(0);
    flush.req.cmd = Command::Flush;
    flush.req.len = 0;
    flush.payload.clear();
    work_tx.send(flush).expect("queue flush");
    drop(work_tx);

    serve_ublk_residency(
        backend,
        probe,
        || Some(0),
        work_rx,
        reply_tx,
        "/dev/ublkb-flush-test",
        ResidencyConfig {
            latency_mult: 1,
            consecutive: 1,
            free_floor_bytes: u64::MAX,
        },
        Arc::clone(&demotes),
        |_| 1_000_000,
        |_| panic!("flush does not request a swapoff"),
        // A real 30 s window never elapses inside these tests.
        DEMOTE_STUCK_DETECTION_WINDOW,
        |_| {},
        || Some(()), // the fixture lifecycle proves absence
    )
    .expect("flush is served without entering VRAM pressure policy");

    let replies = reply_rx.try_iter().collect::<Vec<_>>();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].result, 0);
    assert_eq!(demotes.load(Ordering::Relaxed), 0);
}

/// a transient `swapoff` rejection backs the spawn off. The
/// canary keeps firing on every degraded sample, so without the policy the
/// spawn closure would run once per sample.
#[test]
fn transient_rejection_delays_the_next_demote_spawn() {
    let (work_tx, work_rx) = mpsc::sync_channel::<ublk::IoWork>(96);
    let (reply_tx, _reply_rx) = mpsc::channel::<WorkerReply>();
    let demotes = Arc::new(AtomicU32::new(0));
    let demotes_worker = Arc::clone(&demotes);
    let backend = VramBackend::new(FakeVram::new((BS as usize) * 8), BS);
    let probe = CanaryProbe::new(FakeVram::new(CANARY_BYTES));
    let spawn_calls = Arc::new(AtomicU32::new(0));
    let spawn_calls_worker = Arc::clone(&spawn_calls);

    let worker = thread::spawn(move || {
        serve_ublk_residency(
            backend,
            probe,
            || Some(0),
            work_rx,
            reply_tx,
            "/dev/ramshared-no-such-swap",
            ResidencyConfig {
                latency_mult: 4096,
                consecutive: 1,
                free_floor_bytes: 1 << 30,
            },
            demotes_worker,
            |started| u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            move |_| {
                spawn_calls_worker.fetch_add(1, Ordering::Relaxed);
                let (tx, rx) = mpsc::channel();
                let _ = tx.send(SwapoffOutcome::Rejected { exit_code: Some(1) });
                rx
            },
            // A real 30 s window never elapses inside these tests.
            DEMOTE_STUCK_DETECTION_WINDOW,
            |_| {},
            || Some(()), // the fixture lifecycle proves absence
        )
    });

    for tag in 0..80 {
        work_tx.send(read_work(tag)).expect("queue read work");
    }
    drop(work_tx);
    worker
        .join()
        .expect("residency worker join")
        .expect("serve ok");
    assert_eq!(
        spawn_calls.load(Ordering::Relaxed),
        1,
        "a transient rejection must not re-arm the spawn on the next sample"
    );
    assert_eq!(demotes.load(Ordering::Relaxed), 1);
}

/// a deterministic `swapoff` unavailability parks the path.
/// Retrying would only spin the loop and hide the missing binary.
#[test]
fn unavailable_swapoff_parks_the_demote_spawn() {
    let (work_tx, work_rx) = mpsc::sync_channel::<ublk::IoWork>(96);
    let (reply_tx, _reply_rx) = mpsc::channel::<WorkerReply>();
    let demotes = Arc::new(AtomicU32::new(0));
    let demotes_worker = Arc::clone(&demotes);
    let backend = VramBackend::new(FakeVram::new((BS as usize) * 8), BS);
    let probe = CanaryProbe::new(FakeVram::new(CANARY_BYTES));
    let spawn_calls = Arc::new(AtomicU32::new(0));
    let spawn_calls_worker = Arc::clone(&spawn_calls);

    let worker = thread::spawn(move || {
        serve_ublk_residency(
            backend,
            probe,
            || Some(0),
            work_rx,
            reply_tx,
            "/dev/ramshared-no-such-swap",
            ResidencyConfig {
                latency_mult: 4096,
                consecutive: 1,
                free_floor_bytes: 1 << 30,
            },
            demotes_worker,
            |started| u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            move |_| {
                spawn_calls_worker.fetch_add(1, Ordering::Relaxed);
                let (tx, rx) = mpsc::channel();
                let _ = tx.send(SwapoffOutcome::Unavailable);
                rx
            },
            // A real 30 s window never elapses inside these tests.
            DEMOTE_STUCK_DETECTION_WINDOW,
            |_| {},
            || Some(()), // the fixture lifecycle proves absence
        )
    });

    for tag in 0..80 {
        work_tx.send(read_work(tag)).expect("queue read work");
    }
    drop(work_tx);
    worker
        .join()
        .expect("residency worker join")
        .expect("serve ok");
    assert_eq!(
        spawn_calls.load(Ordering::Relaxed),
        1,
        "a deterministic unavailability must park the path instead of retrying"
    );
    assert_eq!(demotes.load(Ordering::Relaxed), 1);
}

#[test]
fn residency_shutdown_preserves_memory_until_absence_and_custody_are_proven() {
    let (work_tx, work_rx) = mpsc::sync_channel::<ublk::IoWork>(1);
    let (reply_tx, _reply_rx) = mpsc::channel::<WorkerReply>();
    let (gate_tx, gate_rx) = mpsc::sync_channel(1);
    let release_allowed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker_allowed = Arc::clone(&release_allowed);
    let zero_calls = Arc::new(AtomicU32::new(0));
    let backend = VramBackend::new(
        FakeVram::monitored(BS as usize, Arc::clone(&zero_calls)),
        BS,
    );
    let probe = CanaryProbe::new(FakeVram::monitored(CANARY_BYTES, Arc::clone(&zero_calls)));
    drop(work_tx);
    let worker = thread::spawn(move || {
        serve_ublk_residency(
            backend,
            probe,
            || Some(u64::MAX),
            work_rx,
            reply_tx,
            "/dev/ublkb-unobserved-test",
            ResidencyConfig::default(),
            Arc::new(AtomicU32::new(0)),
            |_| 1,
            |_| panic!("shutdown observation must not start another drain"),
            DEMOTE_STUCK_DETECTION_WINDOW,
            |_| {},
            || {
                let _ = gate_tx.try_send(());
                worker_allowed.load(Ordering::Acquire).then_some(())
            },
        )
    });
    gate_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("shutdown gate observed");
    let finished_without_proof = worker.is_finished();
    let zeroed_without_proof = zero_calls.load(Ordering::Relaxed);
    release_allowed.store(true, Ordering::Release);
    worker
        .join()
        .expect("worker join")
        .expect("verified shutdown");
    assert!(
        !finished_without_proof,
        "missing proof released the backend owner"
    );
    assert_eq!(zeroed_without_proof, 0, "missing proof zeroed swap memory");
    assert_eq!(zero_calls.load(Ordering::Relaxed), 2);
}

/// SPEC §9.2: a `swapoff` that never reaches a terminal result is a stuck
/// drain. The loop must report it once with its age, and must neither abort
/// the child nor re-arm the spawn while the flight is pending. The window is
/// `Duration::ZERO` so the report is due on the first poll — waiting 30 s is
/// not what this test is about.
#[test]
fn stuck_swapoff_is_reported_once_without_aborting_the_flight() {
    let (work_tx, work_rx) = mpsc::sync_channel::<ublk::IoWork>(96);
    let (reply_tx, reply_rx) = mpsc::channel::<WorkerReply>();
    let demotes = Arc::new(AtomicU32::new(0));
    let demotes_worker = Arc::clone(&demotes);
    let zero_calls = Arc::new(AtomicU32::new(0));
    let backend = VramBackend::new(
        FakeVram::monitored((BS as usize) * 8, Arc::clone(&zero_calls)),
        BS,
    );
    let probe = CanaryProbe::new(FakeVram::monitored(CANARY_BYTES, Arc::clone(&zero_calls)));
    let spawn_calls = Arc::new(AtomicU32::new(0));
    let spawn_calls_worker = Arc::clone(&spawn_calls);
    let report_calls = Arc::new(AtomicU32::new(0));
    let report_calls_worker = Arc::clone(&report_calls);
    // The sender is parked here and never delivers, so the receiver stays
    // `Empty` — a genuine in-flight drain, not a vanished worker.
    let pending_swapoff = Arc::new(Mutex::new(None::<mpsc::Sender<SwapoffOutcome>>));
    let pending_worker = Arc::clone(&pending_swapoff);

    let worker = thread::spawn(move || {
        serve_ublk_residency(
            backend,
            probe,
            || Some(0),
            work_rx,
            reply_tx,
            "/dev/ublkb-stuck-test",
            ResidencyConfig {
                latency_mult: 4096,
                consecutive: 1,
                free_floor_bytes: 1 << 30,
            },
            demotes_worker,
            |started| u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            move |_| {
                spawn_calls_worker.fetch_add(1, Ordering::Relaxed);
                let (tx, rx) = mpsc::channel();
                *pending_worker.lock().expect("pending swapoff mutex") = Some(tx);
                rx
            },
            Duration::ZERO,
            move |_age_secs| {
                report_calls_worker.fetch_add(1, Ordering::Relaxed);
            },
            || Some(()), // released only after the fake flight completes
        )
    });

    for tag in 0..80 {
        work_tx.send(read_work(tag)).expect("queue read work");
    }
    for _ in 0..80 {
        reply_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("requests remain served during the pending drain");
    }
    drop(work_tx);
    // Closing the ring cannot turn the old five-second observation limit
    // into permission to zero or free the still-owned swap memory.
    thread::sleep(Duration::from_millis(5_100));
    let finished_while_pending = worker.is_finished();
    let zeroed_while_pending = zero_calls.load(Ordering::Relaxed);
    let sender = pending_swapoff
        .lock()
        .expect("pending swapoff mutex")
        .take()
        .expect("one pending drain");
    let _ = sender.send(SwapoffOutcome::Drained);
    worker
        .join()
        .expect("residency worker join")
        .expect("serve ok");

    assert!(
        !finished_while_pending,
        "pending drain released its backend owner"
    );
    assert_eq!(zeroed_while_pending, 0, "pending drain zeroed swap memory");
    assert_eq!(zero_calls.load(Ordering::Relaxed), 2);

    assert_eq!(
        spawn_calls.load(Ordering::Relaxed),
        1,
        "a stuck drain must not re-arm the spawn while the flight is pending"
    );
    assert_eq!(demotes.load(Ordering::Relaxed), 1);
    assert_eq!(
        report_calls.load(Ordering::Relaxed),
        1,
        "the stuck drain is reported exactly once per flight (edge-triggered)"
    );
}

#[test]
fn reply_channel_drop_stops_residency_worker_before_teardown() {
    let (work_tx, work_rx) = mpsc::sync_channel::<ublk::IoWork>(4);
    let (reply_tx, reply_rx) = mpsc::channel::<WorkerReply>();
    let zero_calls = Arc::new(AtomicU32::new(0));
    let backend = VramBackend::new(
        FakeVram::monitored((BS as usize) * 4, Arc::clone(&zero_calls)),
        BS,
    );
    let probe = CanaryProbe::new(FakeVram::monitored(CANARY_BYTES, Arc::clone(&zero_calls)));
    let demotes = Arc::new(AtomicU32::new(0));

    drop(reply_rx);
    work_tx.send(read_work(0)).expect("queue read before exit");
    serve_ublk_residency(
        backend,
        probe,
        || Some(u64::MAX),
        work_rx,
        reply_tx,
        "/dev/ramshared-absent-ublk-reply-drop",
        ResidencyConfig::default(),
        demotes,
        |_| 1,
        |_| mpsc::channel().1,
        DEMOTE_STUCK_DETECTION_WINDOW,
        |_| {},
        || Some(()),
    )
    .expect("dropped reply channel triggers clean worker shutdown");

    assert_eq!(zero_calls.load(Ordering::Relaxed), 2);
}

#[test]
fn spawn_residency_handle_routes_io_and_authorized_shutdown() {
    let commits = Arc::new(Mutex::new(Vec::new()));
    let queue = FakeQueue::new(
        descriptor_bytes(ublk::UBLK_IO_OP_READ, 8),
        vec![
            vec![ramshared_uring::UblkCompletion { tag: 0, result: 0 }],
            vec![ramshared_uring::UblkCompletion {
                tag: 0,
                result: ublk::UBLK_IO_RES_ABORT,
            }],
        ],
        Arc::clone(&commits),
    );
    let zero_calls = Arc::new(AtomicU32::new(0));
    let worker_zero_calls = Arc::clone(&zero_calls);
    let swap_dev = "/dev/ramshared-absent-ublk-residency-wiring".to_string();
    let swap_dev_worker = swap_dev.clone();

    let handle = spawn_residency_handle(
        queue,
        (),
        1,
        BS as usize,
        move |work_rx, reply_tx, demotes, shutdown_rx| {
            let backend = VramBackend::new(
                FakeVram::monitored((BS as usize) * 4, Arc::clone(&worker_zero_calls)),
                BS,
            );
            let probe = CanaryProbe::new(FakeVram::monitored(
                CANARY_BYTES,
                Arc::clone(&worker_zero_calls),
            ));
            report_stuck_drain(1);
            serve_ublk_residency_default(
                backend,
                probe,
                || Some(u64::MAX),
                work_rx,
                reply_tx,
                &swap_dev_worker,
                ResidencyConfig::default(),
                demotes,
                &shutdown_rx,
                |_| mpsc::channel().1,
            )
        },
    );

    let permit = Arc::new(
        crate::tier_release::try_claim_backend_shutdown_for(&swap_dev)
            .expect("isolated shutdown lease"),
    );
    handle
        .authorize_shutdown(permit)
        .expect("shutdown permit reaches residency worker");
    assert_eq!(handle.demote_count(), 0);
    handle.join().expect("residency handle joins cleanly");

    assert_eq!(*commits.lock().expect("commit mutex"), vec![(0, 4096)]);
    assert_eq!(zero_calls.load(Ordering::Relaxed), 2);
}
