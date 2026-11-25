// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/conn_backpressure_tests.rs
// Included inside conn::tests to reuse its private wire and bounded-wait fixtures.
struct CloseBudgetOnDrop(Arc<ReplyBudget>);

impl Drop for CloseBudgetOnDrop {
    fn drop(&mut self) {
        self.0.close();
    }
}

struct CloseTestSocketOnDrop(TestUnixStream);

impl Drop for CloseTestSocketOnDrop {
    fn drop(&mut self) {
        let _ = self.0.shutdown(Shutdown::Both);
    }
}

fn two_request_wire() -> Vec<u8> {
    let mut wire = export_name_handshake(EXPORT_NAME.as_bytes());
    for handle in [1, 2] {
        wire.extend_from_slice(&request_bytes(0, handle, 3, NBD_REQUEST_MAGIC));
    }
    wire
}

#[test]
fn terminal_writer_wakes_a_reader_blocked_on_reply_credit() {
    let (jobs_tx, jobs_rx) = sync_channel(4);
    let (reply_tx, reply_rx) = channel();
    let budget = Arc::new(ReplyBudget::new(1, 100));
    let _cleanup = CloseBudgetOnDrop(Arc::clone(&budget));
    let reader = spawn_reader(
        Cursor::new(two_request_wire()),
        TestWriter::new(Arc::new(WriterState::default()), WriterFailure::Never),
        one_export(4096),
        0,
        jobs_tx,
        reply_tx,
        Arc::clone(&budget),
        test_slot(),
    );
    let writer = spawn_writer(
        TestWriter::new(Arc::new(WriterState::default()), WriterFailure::Flush),
        reply_rx,
        Arc::clone(&budget),
    );
    let WMsg::Job(first) = recv_with_deadline(&jobs_rx) else {
        panic!("first job not admitted");
    };
    assert!(matches!(
        jobs_rx.recv_timeout(Duration::from_millis(50)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    first.reply.send(reply(1, &[1, 2, 3], false, first.credit)).unwrap();
    assert_only_closed(&jobs_rx);
    join_with_deadline(reader);
    join_with_deadline(writer);
    assert_eq!(budget.pending(), 0);
    assert!(budget.admit(1).is_none());
}

#[test]
fn stalled_connection_does_not_block_another_reader() {
    let (jobs_tx, jobs_rx) = sync_channel(8);
    let (reply_tx, _reply_rx) = channel();
    let budget = Arc::new(ReplyBudget::new(1, 100));
    let _cleanup = CloseBudgetOnDrop(Arc::clone(&budget));
    assert!(budget.admit(19).is_some());
    let blocked = spawn_reader(
        Cursor::new(two_request_wire()),
        TestWriter::new(Arc::new(WriterState::default()), WriterFailure::Never),
        one_export(4096),
        0,
        jobs_tx.clone(),
        reply_tx.clone(),
        Arc::clone(&budget),
        test_slot(),
    );
    let independent = spawn_test_reader(
        Cursor::new(two_request_wire()),
        TestWriter::new(Arc::new(WriterState::default()), WriterFailure::Never),
        one_export(4096),
        0,
        jobs_tx,
        reply_tx,
    );
    for _ in 0..2 {
        assert!(matches!(recv_with_deadline(&jobs_rx), WMsg::Job(_)));
    }
    assert!(matches!(recv_with_deadline(&jobs_rx), WMsg::Closed));
    join_with_deadline(independent);
    budget.close();
    assert_only_closed(&jobs_rx);
    join_with_deadline(blocked);
}

#[test]
fn unix_pipeline_preserves_connection_and_reply_bytes() {
    let (server, mut client) = TestUnixStream::pair().unwrap();
    let _socket_cleanup = CloseTestSocketOnDrop(client.try_clone().unwrap());
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let (jobs_tx, jobs_rx) = sync_channel(4);
    let (reply_tx, reply_rx) = channel();
    let budget = Arc::new(ReplyBudget::new(1, 100));
    let _cleanup = CloseBudgetOnDrop(Arc::clone(&budget));
    let writer_socket = server.try_clone().unwrap();
    let handshake_socket = server.try_clone().unwrap();
    let reader = spawn_reader(
        server,
        handshake_socket,
        one_export(4096),
        0,
        jobs_tx,
        reply_tx,
        Arc::clone(&budget),
        test_slot(),
    );
    let writer = spawn_writer(writer_socket, reply_rx, Arc::clone(&budget));
    client.write_all(&two_request_wire()).unwrap();
    let mut handshake = [0u8; 28];
    client.read_exact(&mut handshake).unwrap();
    assert_eq!(
        &handshake[..8],
        &ramshared_block::protocol::NBDMAGIC.to_be_bytes()
    );
    let WMsg::Job(first) = recv_with_deadline(&jobs_rx) else {
        panic!("first job missing");
    };
    assert!(matches!(
        jobs_rx.recv_timeout(Duration::from_millis(50)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    let mut expected = Vec::new();
    for handle in [1, 2] {
        let header = ramshared_block::encode_simple_reply(ramshared_block::NBD_OK, handle);
        expected.extend_from_slice(&header);
        expected.extend_from_slice(&[1, 2, 3]);
    }
    first
        .reply
        .send(Reply {
            reply: expected[..16].try_into().unwrap(),
            data: vec![1, 2, 3],
            credit: first.credit,
            disconnect: false,
            silent_disconnect: false,
        })
        .unwrap();
    drop(first.reply);
    let WMsg::Job(second) = recv_with_deadline(&jobs_rx) else {
        panic!("second job missing");
    };
    second
        .reply
        .send(Reply {
            reply: expected[19..35].try_into().unwrap(),
            data: vec![1, 2, 3],
            credit: second.credit,
            disconnect: false,
            silent_disconnect: false,
        })
        .unwrap();
    drop(second.reply);
    let mut actual = vec![0u8; expected.len()];
    client.read_exact(&mut actual).unwrap();
    assert_eq!(actual, expected);
    client.shutdown(Shutdown::Write).unwrap();
    assert_only_closed(&jobs_rx);
    join_with_deadline(reader);
    join_with_deadline(writer);
    assert_eq!(budget.pending(), 0);
}

#[test]
fn unix_duplicate_handles_complete_out_of_order_with_exact_credit() {
    let (server, mut client) = TestUnixStream::pair().unwrap();
    let _socket_cleanup = CloseTestSocketOnDrop(client.try_clone().unwrap());
    client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let (jobs_tx, jobs_rx) = sync_channel(4);
    let (reply_tx, reply_rx) = channel();
    let budget = Arc::new(ReplyBudget::new(3, 110));
    let _cleanup = CloseBudgetOnDrop(Arc::clone(&budget));
    let reader = spawn_reader(
        server.try_clone().unwrap(), server.try_clone().unwrap(), one_export(4096),
        0, jobs_tx, reply_tx, Arc::clone(&budget), test_slot(),
    );
    let writer = spawn_writer(server, reply_rx, Arc::clone(&budget));
    let mut wire = export_name_handshake(EXPORT_NAME.as_bytes());
    wire.extend_from_slice(&request_bytes(0, 7, 74, NBD_REQUEST_MAGIC));
    wire.extend_from_slice(&request_bytes(0, 7, 4, NBD_REQUEST_MAGIC));
    client.write_all(&wire).unwrap();
    let mut handshake = [0; 28];
    client.read_exact(&mut handshake).unwrap();
    let WMsg::Job(large) = recv_with_deadline(&jobs_rx) else { panic!("large job missing") };
    let WMsg::Job(small) = recv_with_deadline(&jobs_rx) else { panic!("small job missing") };
    assert_eq!(large.req.handle, small.req.handle);
    assert_eq!(budget.reserved_bytes(), 110);
    let header = ramshared_block::encode_simple_reply(ramshared_block::NBD_OK, 7);
    small.reply.send(Reply { reply: header, data: vec![2; 4], credit: small.credit, disconnect: false, silent_disconnect: false }).unwrap();
    drop(small.reply);
    let mut small_frame = [0; 20];
    client.read_exact(&mut small_frame).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while budget.pending() != 1 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(budget.reserved_bytes(), 90);
    assert_eq!(&small_frame[..16], &header);
    assert_eq!(&small_frame[16..], &[2; 4]);
    large.reply.send(Reply { reply: header, data: vec![1; 74], credit: large.credit, disconnect: false, silent_disconnect: false }).unwrap();
    drop(large.reply);
    let mut large_frame = [0; 90];
    client.read_exact(&mut large_frame).unwrap();
    assert_eq!(&large_frame[..16], &header);
    assert_eq!(&large_frame[16..], &[1; 74]);
    client.shutdown(Shutdown::Write).unwrap();
    assert_only_closed(&jobs_rx);
    join_with_deadline(reader);
    join_with_deadline(writer);
    assert_eq!(budget.pending(), 0);
}

#[test]
fn out_of_order_small_reply_must_leave_large_credit_reserved() {
    let state = Arc::new(WriterState::default());
    let budget = Arc::new(ReplyBudget::new(3, 110));
    let large = budget.admit(90).unwrap();
    let small = budget.admit(20).unwrap();
    let (tx, rx) = channel();
    let writer = spawn_writer(
        TestWriter::new(Arc::clone(&state), WriterFailure::Never),
        rx,
        Arc::clone(&budget),
    );
    tx.send(reply(0x11, &[0; 4], false, small)).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while budget.pending() != 1 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    let remaining = budget.reserved_bytes();
    tx.send(reply(0x22, &[0; 74], false, large)).unwrap();
    drop(tx);
    join_with_deadline(writer);
    assert_eq!(
        remaining, 90,
        "the outstanding large reply must remain fully reserved"
    );
    assert_eq!(budget.pending(), 0);
}

#[test]
fn writer_refuses_foreign_credit_before_writing() {
    let state = Arc::new(WriterState::default());
    let budget = test_budget();
    let own = budget.admit(32).unwrap();
    let foreign_budget = ReplyBudget::new(1, 32);
    let foreign = foreign_budget.admit(16).unwrap();
    let (tx, rx) = channel();
    let writer = spawn_writer(
        TestWriter::new(Arc::clone(&state), WriterFailure::Never),
        rx,
        Arc::clone(&budget),
    );
    tx.send(reply(0x11, &[], false, foreign)).unwrap();
    drop(tx);
    join_with_deadline(writer);
    assert!(state.bytes.lock().unwrap().is_empty());
    assert_eq!(state.shutdowns.load(Ordering::SeqCst), 1);
    assert_eq!(budget.pending(), 1);
    assert_eq!(budget.reserved_bytes(), 32);
    assert!(budget.admit(1).is_none());
    assert!(budget.release(own));
}

#[test]
fn writer_completes_equal_sized_replies_in_reverse_order() {
    let state = Arc::new(WriterState::default());
    let budget = Arc::new(ReplyBudget::new(2, 32));
    let first = budget.admit(16).unwrap();
    let second = budget.admit(16).unwrap();
    let (tx, rx) = channel();
    let writer = spawn_writer(
        TestWriter::new(Arc::clone(&state), WriterFailure::Never),
        rx,
        Arc::clone(&budget),
    );
    tx.send(reply(0x22, &[], false, second)).unwrap();
    tx.send(reply(0x11, &[], false, first)).unwrap();
    drop(tx);
    join_with_deadline(writer);
    let mut expected = vec![0x22; SIMPLE_REPLY_LEN];
    expected.extend_from_slice(&[0x11; SIMPLE_REPLY_LEN]);
    assert_eq!(*state.bytes.lock().unwrap(), expected);
    assert_eq!(budget.pending(), 0);
}

#[test]
fn reader_enqueue_failure_returns_only_its_own_credit() {
    let mut wire = export_name_handshake(EXPORT_NAME.as_bytes());
    wire.extend_from_slice(&request_bytes(0, 1, 4, NBD_REQUEST_MAGIC));
    let (jobs_tx, jobs_rx) = sync_channel(1);
    drop(jobs_rx);
    let (reply_tx, _reply_rx) = channel();
    let budget = Arc::new(ReplyBudget::new(2, 110));
    let earlier = budget.admit(90).unwrap();
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
    join_with_deadline(reader);
    assert_eq!(budget.pending(), 1);
    assert_eq!(budget.reserved_bytes(), 90);
    assert!(budget.release(earlier));
}
