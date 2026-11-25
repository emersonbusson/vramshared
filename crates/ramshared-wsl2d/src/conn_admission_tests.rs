// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/conn_admission_tests.rs
// Included inside conn::tests to reuse its private admission and handshake fixtures.
#[test]
fn test_worker_guard_clause_skips_non_jobs() {
    let (jobs_tx, jobs_rx) = sync_channel::<WMsg>(5);
    let (reply_tx, _reply_rx) = channel::<Reply>();

    let worker = std::thread::spawn(move || {
        let mut served = 0u32;
        for m in jobs_rx.iter() {
            let WMsg::Job(job) = m else {
                continue;
            }; // Guard clause
            let _ = job.reply.send(Reply {
                reply: [0u8; SIMPLE_REPLY_LEN],
                data: Vec::new(),
                credit: job.credit,
                disconnect: false,
                silent_disconnect: false,
            });
            served += 1;
            if served >= 2 {
                break;
            }
        }
        served
    });

    // Send non-job messages first; the worker should skip them without crashing or hanging.
    jobs_tx.send(WMsg::Opened).unwrap();
    jobs_tx.send(WMsg::Closed).unwrap();

    // Send actual jobs
    for _ in 0..2 {
        jobs_tx
            .send(WMsg::Job(Job {
                export: 0,
                req: dummy_req(),
                payload: Vec::new(),
                credit: fixture_credit(),
                reply: reply_tx.clone(),
            }))
            .unwrap();
    }

    assert_eq!(
        worker.join().unwrap(),
        2,
        "worker skipped non-jobs correctly using let-else guard clause"
    );
}

// DT-7 / DT-18: unbounded replica — worker progresses even with the writer stopped.
// If the replica were bounded and the writer did not drain, the worker would block →
// Jobs channel would fill up → reader would block → deadlock (this test would hang).
#[test]
fn slow_writer_does_not_deadlock() {
    let (jobs_tx, jobs_rx) = sync_channel::<WMsg>(2); // small Jobs channel
    let (reply_tx, reply_rx) = channel::<Reply>(); // UNBOUNDED replica (DT-7)
    let _stalled_writer = reply_rx; // holds without draining (simulates a hung socket)

    let worker = std::thread::spawn(move || {
        let mut served = 0u32;
        for m in jobs_rx.iter() {
            let WMsg::Job(job) = m else {
                continue;
            }; // Guard clause
            // worker never blocks: unbounded replica
            let _ = job.reply.send(Reply {
                reply: [0u8; SIMPLE_REPLY_LEN],
                data: Vec::new(),
                credit: job.credit,
                disconnect: false,
                silent_disconnect: false,
            });
            served += 1;
            if served >= 10 {
                break;
            }
        }
        served
    });

    for _ in 0..10 {
        jobs_tx
            .send(WMsg::Job(Job {
                export: 0,
                req: dummy_req(),
                payload: Vec::new(),
                credit: fixture_credit(),
                reply: reply_tx.clone(),
            }))
            .unwrap();
    }
    assert_eq!(
        worker.join().unwrap(),
        10,
        "worker processed every job without deadlock"
    );
}

// ===================== AUD-06: reply budget, caps, handshake deadline =====================

#[test]
fn a_reply_budget_refuses_past_its_count() {
    let budget = ReplyBudget::new(2, usize::MAX);
    assert!(budget.admit(1).is_some(), "first reservation fits");
    assert!(budget.admit(1).is_some(), "second reservation fits");
    assert!(budget.admit(1).is_none(), "third must be refused");
    assert_eq!(budget.pending(), 2, "nothing beyond the cap is held");
}

#[test]
fn a_reply_budget_refuses_past_its_bytes() {
    let budget = ReplyBudget::new(usize::MAX, 100);
    assert!(budget.admit(60).is_some(), "first reservation fits");
    assert!(
        budget.admit(50).is_none(),
        "the second does not fit in 100 bytes"
    );
    assert_eq!(
        budget.reserved_bytes(),
        60,
        "the refused call reserved nothing"
    );
}

#[test]
fn a_written_reply_returns_its_reservation() {
    let budget = ReplyBudget::new(2, usize::MAX);
    let first = budget.admit(10).unwrap();
    let second = budget.admit(20).unwrap();
    assert!(budget.release(first));
    assert_eq!(budget.pending(), 1, "one reply was written");
    assert_eq!(
        budget.reserved_bytes(),
        20,
        "the matching reservation is the one returned"
    );
    assert!(budget.release(second));
    assert_eq!(budget.reserved_bytes(), 0, "the budget is empty again");
}

#[test]
fn a_failed_enqueue_returns_its_reservation() {
    let budget = ReplyBudget::new(2, usize::MAX);
    let first = budget.admit(10).unwrap();
    let second = budget.admit(20).unwrap();
    assert!(budget.release(second)); // the second enqueue failed
    assert_eq!(budget.pending(), 1, "the failed enqueue is not held");
    assert_eq!(
        budget.reserved_bytes(),
        10,
        "only the delivered job is reserved"
    );
    assert!(budget.release(first));
    assert_eq!(budget.reserved_bytes(), 0);
}

/// A full pipeline waits for credit without closing an established peer.
#[test]
fn a_pipelining_peer_waits_for_reply_credit_without_disconnect() {
    let mut wire = export_name_handshake(EXPORT_NAME.as_bytes());
    wire.extend_from_slice(&request_bytes(0, 1, 3, NBD_REQUEST_MAGIC));
    wire.extend_from_slice(&request_bytes(0, 2, 3, NBD_REQUEST_MAGIC));
    let (jobs_tx, jobs_rx) = sync_channel(4);
    let (reply_tx, _reply_rx) = channel();
    let budget = Arc::new(ReplyBudget::new(1, usize::MAX));
    let reader = spawn_reader(
        Cursor::new(wire),
        TestWriter::new(Arc::new(WriterState::default()), WriterFailure::Never),
        one_export(4096),
        0,
        jobs_tx,
        reply_tx,
        Arc::clone(&budget),
        test_slot(),
    );
    let WMsg::Job(first) = recv_with_deadline(&jobs_rx) else {
        panic!("first job missing")
    };
    let next = jobs_rx.recv_timeout(Duration::from_millis(100));
    assert!(budget.release(first.credit)); // also releases a waiting reader on failure
    assert!(
        matches!(next, Err(std::sync::mpsc::RecvTimeoutError::Timeout)),
        "a full budget must wait without closing the connection"
    );
    assert!(matches!(recv_with_deadline(&jobs_rx), WMsg::Job(_)));
    assert_only_closed(&jobs_rx);
    join_with_deadline(reader);
}

/// AUD-06 required test: many idle handshakes must not create unbounded
/// connection threads. `wire_conn` refuses beyond the cap before spawning.
#[test]
fn the_connection_cap_refuses_beyond_the_limit() {
    let live = Arc::new(AtomicUsize::new(MAX_CONNECTIONS));
    let (jobs_tx, jobs_rx) = sync_channel(2);
    let (server, _client) = TestUnixStream::pair().unwrap();
    let writer = server.try_clone().unwrap();
    let hs_writer = server.try_clone().unwrap();
    assert!(
        wire_conn(
            server,
            writer,
            hs_writer,
            &one_export(4096),
            0,
            &jobs_tx,
            &live
        ),
        "the acceptor keeps running after a refusal"
    );
    // A refused connection is never opened, so the worker must see nothing
    // at all — neither Opened nor Closed.
    assert!(
        matches!(
            jobs_rx.try_recv(),
            Err(TryRecvError::Empty | TryRecvError::Disconnected)
        ),
        "a refused connection must not reach the worker"
    );
    assert_eq!(
        live.load(Ordering::SeqCst),
        MAX_CONNECTIONS,
        "a refused connection must not consume a slot"
    );
}

/// AUD-06 required test: an idle peer must not hold a connection thread
/// past the handshake deadline. A probe stream records the deadline the
/// reader arms and then reports the timeout the real socket would return.
#[test]
fn an_idle_handshake_is_bounded_by_the_deadline() {
    struct IdlePeer {
        deadlines: Arc<Mutex<Vec<Option<Duration>>>>,
    }
    impl Read for IdlePeer {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "handshake read deadline",
            ))
        }
    }
    impl ReadDeadline for IdlePeer {
        fn set_read_deadline(&self, deadline: Option<Duration>) -> io::Result<()> {
            self.deadlines.lock().unwrap().push(deadline);
            Ok(())
        }
    }

    let deadlines = Arc::new(Mutex::new(Vec::new()));
    let (jobs_tx, jobs_rx) = sync_channel(1);
    let (reply_tx, _reply_rx) = channel();
    let started = std::time::Instant::now();
    let reader = spawn_reader(
        IdlePeer {
            deadlines: Arc::clone(&deadlines),
        },
        TestWriter::new(Arc::new(WriterState::default()), WriterFailure::Never),
        one_export(4096),
        0,
        jobs_tx,
        reply_tx,
        test_budget(),
        test_slot(),
    );
    join_with_deadline(reader);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "an idle handshake must not hold the connection thread"
    );
    assert_only_closed(&jobs_rx);
    let armed = deadlines.lock().unwrap();
    assert_eq!(
        armed.first(),
        Some(&Some(HANDSHAKE_TIMEOUT)),
        "the handshake must be armed with HANDSHAKE_TIMEOUT, got {armed:?}"
    );
}

/// The deadline is cleared for the request phase: a swap connection is
/// long-lived and idle time there is normal (AUD-06).
#[test]
fn the_request_phase_is_not_deadline_bounded() {
    struct IdleAfterHandshake {
        phase: std::cell::Cell<usize>,
        deadlines: Arc<Mutex<Vec<Option<Duration>>>>,
    }
    impl Read for IdleAfterHandshake {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.phase.get() == 0 {
                // Feed a complete handshake, then sit idle.
                let wire = export_name_handshake(EXPORT_NAME.as_bytes());
                let n = wire.len().min(buf.len());
                buf[..n].copy_from_slice(&wire[..n]);
                self.phase.set(1);
                Ok(n)
            } else {
                Err(io::Error::new(io::ErrorKind::TimedOut, "idle"))
            }
        }
    }
    impl ReadDeadline for IdleAfterHandshake {
        fn set_read_deadline(&self, deadline: Option<Duration>) -> io::Result<()> {
            self.deadlines.lock().unwrap().push(deadline);
            Ok(())
        }
    }

    let deadlines = Arc::new(Mutex::new(Vec::new()));
    let (jobs_tx, _jobs_rx) = sync_channel(1);
    let (reply_tx, _reply_rx) = channel();
    let reader = spawn_reader(
        IdleAfterHandshake {
            phase: std::cell::Cell::new(0),
            deadlines: Arc::clone(&deadlines),
        },
        TestWriter::new(Arc::new(WriterState::default()), WriterFailure::Never),
        one_export(4096),
        0,
        jobs_tx,
        reply_tx,
        test_budget(),
        test_slot(),
    );
    join_with_deadline(reader);
    let armed = deadlines.lock().unwrap();
    assert_eq!(
        armed.first(),
        Some(&Some(HANDSHAKE_TIMEOUT)),
        "the handshake must start deadline-bounded, got {armed:?}"
    );
    assert_eq!(
        armed.get(1),
        Some(&None),
        "the deadline must be cleared after the handshake, got {armed:?}"
    );
}

/// AUD-06 required test: other tenants keep working while one connection
/// is over budget. A rejected admission closes only that connection.
#[test]
fn an_over_budget_connection_does_not_disturb_the_worker() {
    // Connection A is over budget and closes; connection B is admitted and
    // completes. Both share the worker channel.
    let (jobs_tx, jobs_rx) = sync_channel(8);

    let mut wire_a = export_name_handshake(EXPORT_NAME.as_bytes());
    wire_a.extend_from_slice(&request_bytes(0, 0xA, 1, NBD_REQUEST_MAGIC));
    let (reply_a_tx, _reply_a_rx) = channel();
    let reader_a = spawn_reader(
        Cursor::new(wire_a),
        TestWriter::new(Arc::new(WriterState::default()), WriterFailure::Never),
        one_export(4096),
        0,
        jobs_tx.clone(),
        reply_a_tx,
        Arc::new(ReplyBudget::new(0, 0)), // nothing fits
        test_slot(),
    );

    let mut wire_b = export_name_handshake(EXPORT_NAME.as_bytes());
    wire_b.extend_from_slice(&request_bytes(0, 0xB, 1, NBD_REQUEST_MAGIC));
    let (reply_b_tx, _reply_b_rx) = channel();
    let reader_b = spawn_reader(
        Cursor::new(wire_b),
        TestWriter::new(Arc::new(WriterState::default()), WriterFailure::Never),
        one_export(4096),
        0,
        jobs_tx.clone(),
        reply_b_tx,
        test_budget(),
        test_slot(),
    );

    join_with_deadline(reader_a);
    join_with_deadline(reader_b);
    drop(jobs_tx);

    let mut admitted = 0;
    let mut closed = 0;
    while let Ok(msg) = jobs_rx.try_recv() {
        match msg {
            WMsg::Job(_) => admitted += 1,
            WMsg::Closed => closed += 1,
            _ => {}
        }
    }
    assert_eq!(admitted, 1, "only connection B's job is admitted");
    assert_eq!(
        closed, 2,
        "both connections still balance Opened with Closed"
    );
}
