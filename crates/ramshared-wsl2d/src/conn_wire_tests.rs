// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/conn_wire_tests.rs
// Included inside conn::tests to reuse its private wire and writer fixtures.
#[test]
fn writer_writes_header_data_and_flushes() {
    let state = Arc::new(WriterState::default());
    let (tx, rx) = channel();
    let (writer, budget) = spawn_test_writer(
        TestWriter::new(Arc::clone(&state), WriterFailure::Never),
        rx,
    );
    tx.send(admitted_reply(&budget, 0x11, &[0x22, 0x33], false))
        .unwrap();
    drop(tx);
    join_with_deadline(writer);

    let mut expected = vec![0x11; SIMPLE_REPLY_LEN];
    expected.extend_from_slice(&[0x22, 0x33]);
    assert_eq!(*state.bytes.lock().unwrap(), expected);
    assert_eq!(state.flushes.load(Ordering::SeqCst), 1);
}

#[test]
fn disconnect_waits_for_earlier_reply_without_emitting_a_reply() {
    let state = Arc::new(WriterState::default());
    let (tx, rx) = channel();
    let budget = test_budget();
    let first = budget.admit(SIMPLE_REPLY_LEN + 1).unwrap();
    let second = budget.admit(SIMPLE_REPLY_LEN + 1).unwrap();
    let disc = budget.admit(SIMPLE_REPLY_LEN).unwrap();
    let writer = spawn_writer(
        TestWriter::new(Arc::clone(&state), WriterFailure::Never),
        rx,
        Arc::clone(&budget),
    );
    let mut disconnect = reply(0x44, &[], true, disc);
    disconnect.silent_disconnect = true;
    tx.send(disconnect).unwrap();
    tx.send(reply(0x11, &[0x22], false, first)).unwrap();
    tx.send(reply(0x33, &[0x44], false, second)).unwrap();
    drop(tx);
    join_with_deadline(writer);
    let mut expected = vec![0x11; SIMPLE_REPLY_LEN];
    expected.push(0x22);
    expected.extend_from_slice(&[0x33; SIMPLE_REPLY_LEN]);
    expected.push(0x44);
    assert_eq!(*state.bytes.lock().unwrap(), expected);
    assert_eq!(budget.pending(), 0);
    assert_eq!(state.shutdowns.load(Ordering::SeqCst), 1);
}

#[test]
fn reader_never_dispatches_requests_after_disconnect() {
    let mut wire = export_name_handshake(EXPORT_NAME.as_bytes());
    wire.extend_from_slice(&request_bytes(2, 11, 0, NBD_REQUEST_MAGIC));
    wire.extend_from_slice(&request_bytes(0, 12, 1, NBD_REQUEST_MAGIC));
    let (jobs_tx, jobs_rx) = sync_channel(4);
    let (reply_tx, _reply_rx) = channel();
    let budget = test_budget();
    let writer_guard = budget.writer_guard();
    let reader = spawn_reader(
        Cursor::new(wire),
        TestWriter::new(Arc::new(WriterState::default()), WriterFailure::Never),
        one_export(4096),
        0,
        jobs_tx,
        reply_tx,
        budget,
        test_slot(),
    );
    assert!(
        matches!(recv_with_deadline(&jobs_rx), WMsg::Job(job) if job.req.cmd == Command::Disc)
    );
    drop(writer_guard);
    join_with_deadline(reader);
    assert!(matches!(recv_with_deadline(&jobs_rx), WMsg::Closed));
}

#[test]
fn disconnect_reader_waits_for_writer_before_closing_liveness() {
    let mut wire = export_name_handshake(EXPORT_NAME.as_bytes());
    wire.extend_from_slice(&request_bytes(2, 11, 0, NBD_REQUEST_MAGIC));
    wire.extend_from_slice(&request_bytes(0, 12, 1, NBD_REQUEST_MAGIC));
    let (jobs_tx, jobs_rx) = sync_channel(4);
    let (reply_tx, _reply_rx) = channel();
    let budget = test_budget();
    let writer_guard = budget.writer_guard();
    let reader = spawn_reader(
        Cursor::new(wire),
        TestWriter::new(Arc::new(WriterState::default()), WriterFailure::Never),
        one_export(4096),
        0,
        jobs_tx,
        reply_tx,
        budget,
        test_slot(),
    );
    assert!(
        matches!(recv_with_deadline(&jobs_rx), WMsg::Job(job) if job.req.cmd == Command::Disc)
    );
    let early = jobs_rx.recv_timeout(Duration::from_millis(100));
    drop(writer_guard);
    join_with_deadline(reader);
    assert!(
        matches!(early, Err(std::sync::mpsc::RecvTimeoutError::Timeout)),
        "liveness closed before the writer finished"
    );
    assert!(matches!(recv_with_deadline(&jobs_rx), WMsg::Closed));
}

#[test]
fn unix_disconnect_drains_inflight_reply_then_closes_without_disc_reply() {
    let (server, mut client) = TestUnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let writer = server.try_clone().unwrap();
    let hs_writer = server.try_clone().unwrap();
    let (jobs_tx, jobs_rx) = sync_channel(4);
    let live = Arc::new(AtomicUsize::new(0));
    assert!(wire_conn(
        server,
        writer,
        hs_writer,
        &one_export(4096),
        0,
        &jobs_tx,
        &live
    ));
    assert!(matches!(recv_with_deadline(&jobs_rx), WMsg::Opened));
    client
        .write_all(&export_name_handshake(EXPORT_NAME.as_bytes()))
        .unwrap();
    let mut handshake = [0; 28];
    client.read_exact(&mut handshake).unwrap();
    client
        .write_all(&request_bytes(0, 11, 1, NBD_REQUEST_MAGIC))
        .unwrap();
    client
        .write_all(&request_bytes(2, 12, 0, NBD_REQUEST_MAGIC))
        .unwrap();
    let WMsg::Job(read) = recv_with_deadline(&jobs_rx) else {
        panic!("missing read job")
    };
    let WMsg::Job(disc) = recv_with_deadline(&jobs_rx) else {
        panic!("missing disconnect job")
    };
    assert_eq!(read.req.cmd, Command::Read);
    assert_eq!(disc.req.cmd, Command::Disc);
    let mut terminal = reply(0x44, &[], true, disc.credit);
    terminal.silent_disconnect = true;
    disc.reply.send(terminal).unwrap();
    read.reply
        .send(reply(0x11, &[0x22], false, read.credit))
        .unwrap();
    drop(read.reply);
    drop(disc.reply);
    let mut actual = [0; SIMPLE_REPLY_LEN + 1];
    client.read_exact(&mut actual).unwrap();
    assert_eq!(&actual[..SIMPLE_REPLY_LEN], &[0x11; SIMPLE_REPLY_LEN]);
    assert_eq!(actual[SIMPLE_REPLY_LEN], 0x22);
    assert_eq!(client.read(&mut [0]).unwrap(), 0);
    assert!(matches!(recv_with_deadline(&jobs_rx), WMsg::Closed));
}

#[test]
fn writer_retries_on_transient_error() {
    let state = Arc::new(WriterState::default());
    let (tx, rx) = channel();
    let (writer, budget) = spawn_test_writer(
        TestWriter::new(Arc::clone(&state), WriterFailure::Transient(0, 2)), // Fail twice, then succeed
        rx,
    );
    let start = std::time::Instant::now();
    tx.send(admitted_reply(&budget, 0xBB, &[0xCC, 0xDD], false))
        .unwrap();
    drop(tx);
    join_with_deadline(writer);
    let elapsed = start.elapsed();

    let mut expected = vec![0xBB; SIMPLE_REPLY_LEN];
    expected.extend_from_slice(&[0xCC, 0xDD]);
    assert_eq!(*state.bytes.lock().unwrap(), expected);
    assert_eq!(state.flushes.load(Ordering::SeqCst), 1);

    assert!(
        elapsed >= Duration::from_millis(30),
        "writer should have backed off"
    );
}

#[test]
fn writer_stops_on_disconnect_or_io_error() {
    let disconnect_state = Arc::new(WriterState::default());
    let (disconnect_tx, disconnect_rx) = channel();
    let (disconnect_writer, disconnect_budget) = spawn_test_writer(
        TestWriter::new(Arc::clone(&disconnect_state), WriterFailure::Never),
        disconnect_rx,
    );
    let first = admitted_reply(&disconnect_budget, 0x44, &[0x55], true);
    let second = admitted_reply(&disconnect_budget, 0x66, &[0x77], false);
    disconnect_tx.send(first).unwrap();
    let _ = disconnect_tx.send(second);
    drop(disconnect_tx);
    join_with_deadline(disconnect_writer);
    let mut disconnect_expected = vec![0x44; SIMPLE_REPLY_LEN];
    disconnect_expected.push(0x55);
    assert_eq!(*disconnect_state.bytes.lock().unwrap(), disconnect_expected);
    assert_eq!(disconnect_state.flushes.load(Ordering::SeqCst), 1);
    assert_eq!(
        disconnect_state.shutdowns.load(Ordering::SeqCst),
        1,
        "a clean disconnect shuts the connection down"
    );

    let write_error_state = Arc::new(WriterState::default());
    let (write_error_tx, write_error_rx) = channel();
    let (write_error_writer, write_error_budget) = spawn_test_writer(
        TestWriter::new(Arc::clone(&write_error_state), WriterFailure::WriteAt(1)),
        write_error_rx,
    );
    write_error_tx
        .send(admitted_reply(&write_error_budget, 0x88, &[0x99], false))
        .unwrap();
    drop(write_error_tx);
    join_with_deadline(write_error_writer);
    assert_eq!(
        *write_error_state.bytes.lock().unwrap(),
        vec![0x88; SIMPLE_REPLY_LEN]
    );
    assert_eq!(write_error_state.flushes.load(Ordering::SeqCst), 0);
    assert_eq!(
        write_error_state.shutdowns.load(Ordering::SeqCst),
        1,
        "a terminal write error shuts the connection down"
    );

    let flush_error_state = Arc::new(WriterState::default());
    let (flush_error_tx, flush_error_rx) = channel();
    let (flush_error_writer, flush_error_budget) = spawn_test_writer(
        TestWriter::new(Arc::clone(&flush_error_state), WriterFailure::Flush),
        flush_error_rx,
    );
    flush_error_tx
        .send(admitted_reply(&flush_error_budget, 0xaa, &[], false))
        .unwrap();
    drop(flush_error_tx);
    join_with_deadline(flush_error_writer);
    assert_eq!(
        *flush_error_state.bytes.lock().unwrap(),
        vec![0xaa; SIMPLE_REPLY_LEN]
    );
    assert_eq!(flush_error_state.flushes.load(Ordering::SeqCst), 1);
    assert_eq!(flush_error_state.shutdowns.load(Ordering::SeqCst), 1);
}

/// AUD-04: a `WouldBlock` after the header landed must resume the payload,
/// not restart the frame. The output is byte-identical to one frame.
#[test]
fn writer_resumes_after_a_block_between_header_and_payload() {
    let state = Arc::new(WriterState::default());
    let (tx, rx) = channel();
    let (writer, budget) = spawn_test_writer(
        TestWriter::new(
            Arc::clone(&state),
            WriterFailure::BlockAfter(SIMPLE_REPLY_LEN, 1),
        ),
        rx,
    );
    tx.send(admitted_reply(&budget, 0x10, &[0x20, 0x30], false))
        .unwrap();
    drop(tx);
    join_with_deadline(writer);

    let mut expected = vec![0x10; SIMPLE_REPLY_LEN];
    expected.extend_from_slice(&[0x20, 0x30]);
    assert_eq!(
        *state.bytes.lock().unwrap(),
        expected,
        "a retry must resume the frame, never replay the header"
    );
}

/// AUD-04: a block before any byte was accepted is the case the old retry
/// loop handled. It must still produce exactly one frame.
#[test]
fn writer_resumes_after_a_block_before_any_byte() {
    let state = Arc::new(WriterState::default());
    let (tx, rx) = channel();
    let (writer, budget) = spawn_test_writer(
        TestWriter::new(Arc::clone(&state), WriterFailure::BlockAfter(0, 2)),
        rx,
    );
    tx.send(admitted_reply(&budget, 0xA0, &[0xB0], false))
        .unwrap();
    drop(tx);
    join_with_deadline(writer);

    let mut expected = vec![0xA0; SIMPLE_REPLY_LEN];
    expected.push(0xB0);
    assert_eq!(*state.bytes.lock().unwrap(), expected);
}

/// AUD-04: short writes advance the resume offset. A header and a payload
/// delivered one byte at a time are still exactly one frame.
#[test]
fn writer_reassembles_short_writes_into_one_frame() {
    let state = Arc::new(WriterState::default());
    let (tx, rx) = channel();
    let (writer, budget) = spawn_test_writer(
        TestWriter::new(Arc::clone(&state), WriterFailure::Short(1)),
        rx,
    );
    tx.send(admitted_reply(&budget, 0xC0, &[0xD0, 0xD1, 0xD2], false))
        .unwrap();
    drop(tx);
    join_with_deadline(writer);

    let mut expected = vec![0xC0; SIMPLE_REPLY_LEN];
    expected.extend_from_slice(&[0xD0, 0xD1, 0xD2]);
    assert_eq!(
        *state.bytes.lock().unwrap(),
        expected,
        "short writes must not re-send the bytes already accepted"
    );
}

/// AUD-04: `Interrupted` is retriable and must not advance or replay.
#[test]
fn writer_retries_interrupted_without_replaying() {
    let state = Arc::new(WriterState::default());
    let (tx, rx) = channel();
    let (writer, budget) = spawn_test_writer(
        TestWriter::new(Arc::clone(&state), WriterFailure::InterruptedAt(1)),
        rx,
    );
    tx.send(admitted_reply(&budget, 0xE0, &[0xF0, 0xF1], false))
        .unwrap();
    drop(tx);
    join_with_deadline(writer);

    let mut expected = vec![0xE0; SIMPLE_REPLY_LEN];
    expected.extend_from_slice(&[0xF0, 0xF1]);
    assert_eq!(*state.bytes.lock().unwrap(), expected);
}

/// AUD-05: a terminal error ends the writer. The next queued reply must
/// never reach a socket that already refused this one.
#[test]
fn writer_stops_after_a_fatal_error_before_the_next_reply() {
    let state = Arc::new(WriterState::default());
    let (tx, rx) = channel();
    let (writer, budget) = spawn_test_writer(
        // Header of reply 1 lands; its payload dies terminally.
        TestWriter::new(Arc::clone(&state), WriterFailure::WriteAt(1)),
        rx,
    );
    let first = admitted_reply(&budget, 0x11, &[0x22], false);
    let second = admitted_reply(&budget, 0x33, &[0x44], false);
    tx.send(first).unwrap();
    tx.send(second).unwrap();
    drop(tx);
    join_with_deadline(writer);

    assert_eq!(
        *state.bytes.lock().unwrap(),
        vec![0x11; SIMPLE_REPLY_LEN],
        "a terminal error must not be followed by another frame"
    );
    assert_eq!(state.shutdowns.load(Ordering::SeqCst), 1);
}

/// AUD-05: retry exhaustion is terminal in the same way.
#[test]
fn writer_stops_after_retry_exhaustion_before_the_next_reply() {
    let state = Arc::new(WriterState::default());
    let (tx, rx) = channel();
    let (writer, budget) = spawn_test_writer(
        // More consecutive transient failures than the retry budget allows.
        TestWriter::new(Arc::clone(&state), WriterFailure::Transient(0, 99)),
        rx,
    );
    let first = admitted_reply(&budget, 0x55, &[0x66], false);
    let second = admitted_reply(&budget, 0x77, &[0x88], false);
    tx.send(first).unwrap();
    tx.send(second).unwrap();
    drop(tx);
    join_with_deadline(writer);

    assert!(
        state.bytes.lock().unwrap().is_empty(),
        "a fully blocked sink accepts nothing"
    );
    assert_eq!(state.shutdowns.load(Ordering::SeqCst), 1);
}

#[test]
fn reader_enqueues_write_payload_then_closed() {
    let mut wire = export_name_handshake(EXPORT_NAME.as_bytes());
    wire.extend_from_slice(&request_bytes(1, 0x0102_0304, 3, NBD_REQUEST_MAGIC));
    wire.extend_from_slice(&[7, 8, 9]);
    let (jobs_tx, jobs_rx) = sync_channel(2);
    let (reply_tx, _reply_rx) = channel();
    let reader = spawn_test_reader(
        Cursor::new(wire),
        TestWriter::new(Arc::new(WriterState::default()), WriterFailure::Never),
        one_export(4096),
        0,
        jobs_tx,
        reply_tx,
    );
    join_with_deadline(reader);

    match recv_with_deadline(&jobs_rx) {
        WMsg::Job(job) => {
            assert_eq!(job.export, 0);
            assert_eq!(job.req.cmd, Command::Write);
            assert_eq!(job.req.handle, 0x0102_0304);
            assert_eq!(job.payload, vec![7, 8, 9]);
        }
        _ => panic!("reader must enqueue the negotiated write job"),
    }
    assert_only_closed(&jobs_rx);
}

#[test]
fn reader_refusal_and_eof_emit_closed() {
    let (refusal_tx, refusal_rx) = sync_channel(1);
    let (reply_tx, _reply_rx) = channel();
    let refusal_reader = spawn_test_reader(
        Cursor::new(export_name_handshake(b"missing")),
        TestWriter::new(Arc::new(WriterState::default()), WriterFailure::Never),
        one_export(4096),
        0,
        refusal_tx,
        reply_tx,
    );
    join_with_deadline(refusal_reader);
    assert_only_closed(&refusal_rx);

    let (eof_tx, eof_rx) = sync_channel(1);
    let (reply_tx, _reply_rx) = channel();
    let eof_reader = spawn_test_reader(
        Cursor::new(export_name_handshake(EXPORT_NAME.as_bytes())),
        TestWriter::new(Arc::new(WriterState::default()), WriterFailure::Never),
        one_export(4096),
        0,
        eof_tx,
        reply_tx,
    );
    join_with_deadline(eof_reader);
    assert_only_closed(&eof_rx);
}

#[test]
fn reader_malformed_and_oversized_write_emit_closed() {
    let mut malformed_wire = export_name_handshake(EXPORT_NAME.as_bytes());
    malformed_wire.extend_from_slice(&request_bytes(0, 1, 0, 0));
    let (malformed_tx, malformed_rx) = sync_channel(1);
    let (reply_tx, _reply_rx) = channel();
    let malformed_reader = spawn_test_reader(
        Cursor::new(malformed_wire),
        TestWriter::new(Arc::new(WriterState::default()), WriterFailure::Never),
        one_export(4096),
        0,
        malformed_tx,
        reply_tx,
    );
    join_with_deadline(malformed_reader);
    assert_only_closed(&malformed_rx);

    let mut oversized_wire = export_name_handshake(EXPORT_NAME.as_bytes());
    oversized_wire.extend_from_slice(&request_bytes(1, 2, 4097, NBD_REQUEST_MAGIC));
    let (oversized_tx, oversized_rx) = sync_channel(1);
    let (reply_tx, _reply_rx) = channel();
    let oversized_reader = spawn_test_reader(
        Cursor::new(oversized_wire),
        TestWriter::new(Arc::new(WriterState::default()), WriterFailure::Never),
        one_export(4096),
        0,
        oversized_tx,
        reply_tx,
    );
    join_with_deadline(oversized_reader);
    assert_only_closed(&oversized_rx);

    let mut over_ipc_limit_wire = export_name_handshake(EXPORT_NAME.as_bytes());
    // 16 MiB + 1
    over_ipc_limit_wire.extend_from_slice(&request_bytes(
        1,
        2,
        16 * 1024 * 1024 + 1,
        NBD_REQUEST_MAGIC,
    ));
    let (over_ipc_tx, over_ipc_rx) = sync_channel(1);
    let (reply_tx, _reply_rx) = channel();
    let over_ipc_reader = spawn_test_reader(
        Cursor::new(over_ipc_limit_wire),
        TestWriter::new(Arc::new(WriterState::default()), WriterFailure::Never),
        one_export(32 * 1024 * 1024), // Export size is 32 MiB, so it doesn't fail the export size check
        0,
        over_ipc_tx,
        reply_tx,
    );
    join_with_deadline(over_ipc_reader);
    assert_only_closed(&over_ipc_rx);
}

#[test]
fn reader_stops_when_worker_is_closed() {
    let mut wire = export_name_handshake(EXPORT_NAME.as_bytes());
    wire.extend_from_slice(&request_bytes(0, 3, 0, NBD_REQUEST_MAGIC));
    let (jobs_tx, jobs_rx) = sync_channel(1);
    drop(jobs_rx);
    let (reply_tx, _reply_rx) = channel();
    let reader = spawn_test_reader(
        Cursor::new(wire),
        TestWriter::new(Arc::new(WriterState::default()), WriterFailure::Never),
        one_export(4096),
        0,
        jobs_tx,
        reply_tx,
    );
    join_with_deadline(reader);
}

#[test]
fn wire_conn_balances_opened_and_closed() {
    let (server, mut client) = TestUnixStream::pair().unwrap();
    let writer = server.try_clone().unwrap();
    let hs_writer = server.try_clone().unwrap();
    let (jobs_tx, jobs_rx) = sync_channel(2);
    let exports = one_export(4096);
    let live = Arc::new(AtomicUsize::new(0));
    assert!(wire_conn(
        server, writer, hs_writer, &exports, 0, &jobs_tx, &live
    ));
    assert!(matches!(recv_with_deadline(&jobs_rx), WMsg::Opened));

    client
        .write_all(&export_name_handshake(EXPORT_NAME.as_bytes()))
        .unwrap();
    client.shutdown(Shutdown::Write).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut handshake_reply = [0u8; 28];
    client.read_exact(&mut handshake_reply).unwrap();
    assert_eq!(
        &handshake_reply[0..8],
        &ramshared_block::protocol::NBDMAGIC.to_be_bytes()
    );
    assert_only_closed(&jobs_rx);
}

#[test]
fn acceptors_stop_when_worker_is_closed() {
    let unix_path = socket_path("acceptor");
    let _ = std::fs::remove_file(&unix_path);
    let unix_listener = TestUnixListener::bind(&unix_path).unwrap();
    let (unix_jobs_tx, unix_jobs_rx) = sync_channel(1);
    drop(unix_jobs_rx);
    let unix_acceptor = spawn_acceptor(unix_listener, one_export(4096), 0, unix_jobs_tx);
    let unix_client = TestUnixStream::connect(&unix_path).unwrap();
    join_with_deadline(unix_acceptor);
    drop(unix_client);
    std::fs::remove_file(&unix_path).unwrap();

    let tcp_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let tcp_addr = tcp_listener.local_addr().unwrap();
    let (tcp_jobs_tx, tcp_jobs_rx) = sync_channel(1);
    drop(tcp_jobs_rx);
    let tcp_acceptor = spawn_acceptor_tcp(tcp_listener, one_export(4096), 0, tcp_jobs_tx);
    let tcp_client = TcpStream::connect(tcp_addr).unwrap();
    join_with_deadline(tcp_acceptor);
    drop(tcp_client);
}

