// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-agent/src/main_tests/status_and_command.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::super::*;
use super::{args, expect_register_and_psi, fixture_status_reply, test_config};
use ramshared_agent::swap;
use ramshared_broker::model::TransportKind;
use ramshared_broker::protocol::{Msg, NbdEndpoint, write_msg};
use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

#[test]
fn run_status_prints_a_complete_status_reply() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener must bind");
    let broker = listener
        .local_addr()
        .expect("listener address must be available")
        .to_string();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("status client must connect");
        let mut reader = BufReader::new(stream.try_clone().expect("stream must clone"));
        assert!(matches!(
            read_msg(&mut reader).expect("status request must decode"),
            Some(Msg::Status)
        ));
        write_msg(&mut stream, &fixture_status_reply()).expect("StatusReply must write");
    });
    let cfg = Config {
        broker,
        tenant: String::new(),
        swap_prio: None,
        nbd_base: "/dev/nbd".to_string(),
        transport: TransportKind::NbdTcp,
        watchdog: Duration::from_secs(1),
        status_only: true,
    };
    run_status(&cfg).expect("a complete StatusReply must print and succeed");
    server.join().expect("status fixture must finish");
}

#[test]
fn run_status_reports_broker_rejection() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener must bind");
    let broker = listener
        .local_addr()
        .expect("listener address must be available")
        .to_string();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("status client must connect");
        let mut reader = BufReader::new(stream.try_clone().expect("stream must clone"));
        let _ = read_msg(&mut reader);
        write_msg(
            &mut stream,
            &Msg::Error {
                reason: "status refused".to_string(),
            },
        )
        .expect("broker refusal must write");
    });
    let cfg = Config {
        broker,
        tenant: String::new(),
        swap_prio: None,
        nbd_base: "/dev/nbd".to_string(),
        transport: TransportKind::NbdTcp,
        watchdog: Duration::from_secs(1),
        status_only: true,
    };
    let err = run_status(&cfg).expect_err("a broker Error must fail the status query");
    assert!(
        err.to_string().contains("status refused"),
        "the refusal reason must reach the operator: {err}"
    );
    server.join().expect("status fixture must finish");
}

#[test]
fn run_status_fails_when_no_status_reply_arrives() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener must bind");
    let broker = listener
        .local_addr()
        .expect("listener address must be available")
        .to_string();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("status client must connect");
        let mut reader = BufReader::new(stream.try_clone().expect("stream must clone"));
        let _ = read_msg(&mut reader);
        // A non-StatusReply frame is skipped; a clean close then ends the
        // poll loop without ever producing a status.
        write_msg(&mut stream, &Msg::Ack).expect("non-status frame must write");
    });
    let cfg = Config {
        broker,
        tenant: String::new(),
        swap_prio: None,
        nbd_base: "/dev/nbd".to_string(),
        transport: TransportKind::NbdTcp,
        watchdog: Duration::from_secs(1),
        status_only: true,
    };
    let err = run_status(&cfg).expect_err("no StatusReply must fail the status query");
    assert!(
        err.to_string().contains("did not return StatusReply"),
        "unexpected error: {err}"
    );
    server.join().expect("status fixture must finish");
}

#[test]
fn execute_command_maps_attach_and_detach_outcomes() {
    let endpoint = NbdEndpoint::Unix {
        path: "/run/b.sock".to_string(),
    };
    let mut ok_attach = |_: &NbdEndpoint, _: &str, _: &str, _: Option<i32>| Ok(());
    let mut ok_detach = |_: &str| Ok(());

    match execute_command(
        ExecCmd::On {
            slice: 2,
            export: "s2".to_string(),
            endpoint: endpoint.clone(),
            dev: "/dev/nbd2".to_string(),
            prio: Some(-3),
        },
        &mut ok_attach,
        &mut ok_detach,
    ) {
        ExecResult::On {
            slice,
            ok,
            retained,
            detail,
        } => {
            assert_eq!(slice, 2);
            assert!(ok);
            assert!(!retained);
            assert_eq!(detail, "/dev/nbd2");
        }
        other => panic!("On must yield On, got {other:?}"),
    }

    match execute_command(
        ExecCmd::Off {
            slice: 2,
            dev: "/dev/nbd2".to_string(),
        },
        &mut ok_attach,
        &mut ok_detach,
    ) {
        ExecResult::Off {
            slice,
            ok,
            retained,
            detail,
        } => {
            assert_eq!(slice, 2);
            assert!(ok);
            assert!(!retained);
            assert_eq!(detail, "/dev/nbd2");
        }
        other => panic!("Off must yield Off, got {other:?}"),
    }

    let mut fail_attach = |_: &NbdEndpoint, _: &str, _: &str, _: Option<i32>| {
        Err(swap::SwapFailure {
            error: swap::SwapError::DiskFull,
            retained: false,
        })
    };
    match execute_command(
        ExecCmd::On {
            slice: 5,
            export: "s5".to_string(),
            endpoint: endpoint.clone(),
            dev: "/dev/nbd5".to_string(),
            prio: None,
        },
        &mut fail_attach,
        &mut ok_detach,
    ) {
        ExecResult::On {
            slice,
            ok,
            retained,
            detail,
        } => {
            assert_eq!(slice, 5);
            assert!(!ok);
            assert!(!retained, "a refusal that left nothing is not retained");
            assert_eq!(detail, "disk full (ENOSPC)");
        }
        other => panic!("a failed attach must yield On with ok=false, got {other:?}"),
    }

    let mut fail_detach = |_: &str| {
        Err(swap::SwapFailure {
            error: swap::SwapError::PermissionDenied,
            retained: true,
        })
    };
    match execute_command(
        ExecCmd::Off {
            slice: 5,
            dev: "/dev/nbd5".to_string(),
        },
        &mut ok_attach,
        &mut fail_detach,
    ) {
        ExecResult::Off {
            slice,
            ok,
            retained,
            detail,
        } => {
            assert_eq!(slice, 5);
            assert!(!ok);
            assert!(retained, "a failed swapoff keeps the device");
            assert_eq!(detail, "permission denied (EACCES)");
        }
        other => panic!("a failed detach must yield Off with ok=false, got {other:?}"),
    }
}

/// AUD-11: a `swapon` that was contained is ambiguous — the kernel may
/// already have taken the effect — so the record must survive as `Failed`
/// and the map must not drop it.
#[test]
fn a_contained_swapon_keeps_the_device_record() {
    let mut active = HashMap::from([(
        4,
        SliceRecord::Active {
            dev: "/dev/ramshared-test-nbd4".to_string(),
        },
    )]);

    let msg = commit_result(
        ExecResult::On {
            slice: 4,
            ok: false,
            retained: true,
            detail: "swapon: timed out after 30000 ms; device retained with unknown swap state"
                .to_string(),
        },
        &mut active,
    );
    assert!(
        matches!(
            msg,
            Msg::SwapOnDone {
                slice: 4,
                ok: false,
                ..
            }
        ),
        "got {msg:?}"
    );
    assert_eq!(
        active.get(&4),
        Some(&SliceRecord::Failed {
            dev: "/dev/ramshared-test-nbd4".to_string()
        }),
        "a contained swapon must leave the device owned"
    );
}

/// The other half of AUD-11: a refusal that verifiably left nothing behind
/// drops the record — retained is the only thing that keeps it.
#[test]
fn a_refused_swapon_that_left_nothing_drops_the_record() {
    let mut active = HashMap::from([(
        6,
        SliceRecord::Active {
            dev: "/dev/ramshared-test-nbd6".to_string(),
        },
    )]);

    commit_result(
        ExecResult::On {
            slice: 6,
            ok: false,
            retained: false,
            detail: "nbd-client: refused".to_string(),
        },
        &mut active,
    );
    assert!(
        active.is_empty(),
        "a refusal that left nothing behind must not keep a record: {active:?}"
    );
}

#[test]
fn run_refuses_agent_mode_without_root() {
    // DT-26: swap requires privilege; the refusal names the observed euid so
    // the operator sees the number, not just an adjective. Tests run without
    // root, so this exercises the real early-return path.
    let err = run(&args(&["--broker", "127.0.0.1:1", "--tenant", "t"]))
        .expect_err("agent mode without root must refuse");
    match err {
        CliExit::Runtime(message) => {
            assert!(
                message.contains("root is required for swap"),
                "unexpected refusal: {message}"
            );
            assert!(
                message.contains("euid="),
                "the refusal must name the observed euid: {message}"
            );
        }
        other => panic!("expected a runtime refusal, got {other:?}"),
    }
}

#[test]
fn run_requires_tenant_in_agent_mode() {
    let err = run(&args(&["--broker", "127.0.0.1:1"]))
        .expect_err("agent mode without --tenant must refuse");
    match err {
        CliExit::Usage(message) => assert!(
            message.contains("--tenant is required"),
            "unexpected usage refusal: {message}"
        ),
        other => panic!("expected a usage refusal, got {other:?}"),
    }
}

#[test]
fn session_disconnects_on_peer_eof_and_reader_rejects_malformed_frame() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut r = BufReader::new(stream.try_clone().unwrap());
        expect_register_and_psi(&mut r);
        drop(r);
        drop(stream);
    });

    let cfg = test_config(addr.to_string(), Duration::from_secs(5));
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (res_tx, res_rx) = mpsc::channel();
    let exec_handle = thread::spawn(move || exec_loop(cmd_rx, res_tx));

    let err = session(&cfg, &cmd_tx, &res_rx).expect_err("peer EOF must surface disconnect error");
    assert!(
        err.to_string().contains("supervisor") || err.to_string().contains("terminated"),
        "unexpected EOF disconnect error: {err}"
    );
    server.join().unwrap();
    drop(cmd_tx);
    exec_handle.join().unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let writer = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.write_all(b"{not-valid-json\n").unwrap();
        stream.flush().unwrap();
    });
    let stream = TcpStream::connect(addr).unwrap();
    let (msg_tx, msg_rx) = mpsc::channel();
    reader_loop(BufReader::new(stream), msg_tx);
    writer.join().unwrap();
    assert!(
        msg_rx.try_recv().is_err(),
        "malformed JSON must terminate reader without forwarding a frame"
    );
}
