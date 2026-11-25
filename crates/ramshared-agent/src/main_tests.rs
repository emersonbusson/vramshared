// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-agent/src/main_tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "main_tests/status_and_command.rs"]
mod status_and_command;

use super::*;

use ramshared_broker::model::{PsiSample, Slice, SliceState, TransportKind};

use ramshared_broker::protocol::{
    Msg, NbdEndpoint, PROTO_VERSION, SliceIo, TenantStatus, read_msg, write_msg,
};

use std::collections::HashMap;

use std::io::BufReader;

use std::net::{TcpListener, TcpStream};

use std::sync::mpsc;

use std::thread;

use std::time::{Duration, Instant};

pub(super) fn args(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

pub(super) fn parse_config(v: &[&str]) -> Config {
    match parse_args(&args(v)) {
        Ok(ParsedArgs::Config(config)) => Ok(config),
        Ok(ParsedArgs::Help) => Err("test expected configuration, not help".to_string()),
        Err(err) => Err(err),
    }
    .expect("arguments must parse as a configuration")
}

pub(super) fn test_config(broker: String, watchdog: Duration) -> Config {
    Config {
        broker,
        tenant: "test-tenant".to_string(),
        swap_prio: Some(-3),
        nbd_base: "/dev/ramshared-test-nbd".to_string(),
        transport: TransportKind::NbdTcp,
        watchdog,
        status_only: false,
    }
}

pub(super) fn expect_register_and_psi(reader: &mut BufReader<TcpStream>) {
    assert!(matches!(
        read_msg(reader).expect("registration must decode"),
        Some(Msg::Register {
            proto: PROTO_VERSION,
            tenant,
            transport: TransportKind::NbdTcp,
        }) if tenant == "test-tenant"
    ));
    // Avoid asserting PSI report directly because test nodes may lack PSI and it skips it.
    let _ = read_msg(reader);
}

#[test]
fn backoff_doubles_up_to_cap() {
    // doubles: 2→4→8→16→32→60(cap)→60
    assert_eq!(next_backoff(INITIAL_BACKOFF), Duration::from_secs(4));
    assert_eq!(next_backoff(Duration::from_secs(4)), Duration::from_secs(8));
    assert_eq!(
        next_backoff(Duration::from_secs(16)),
        Duration::from_secs(32)
    );
    // 32*2=64 → saturates at the cap of 60
    assert_eq!(next_backoff(Duration::from_secs(32)), MAX_BACKOFF);
    assert_eq!(next_backoff(MAX_BACKOFF), MAX_BACKOFF);
}

#[test]
fn parse_minimal_agent() {
    let c = parse_config(&["--broker", "10.0.0.1:7000", "--tenant", "wsl2"]);
    assert_eq!(c.broker, "10.0.0.1:7000");
    assert_eq!(c.tenant, "wsl2");
    assert_eq!(c.nbd_base, "/dev/nbd");
    assert!(matches!(c.transport, TransportKind::NbdTcp));
    assert_eq!(c.watchdog, Duration::from_secs(90));
    assert!(!c.status_only);
    assert!(c.swap_prio.is_none());
}

#[test]
fn parse_full_flags() {
    let c = parse_config(&[
        "--broker",
        "h:1",
        "--tenant",
        "t",
        "--swap-prio",
        "-3",
        "--nbd-base",
        "/dev/nbd",
        "--transport",
        "unix",
        "--watchdog-secs",
        "30",
    ]);
    assert_eq!(c.swap_prio, Some(-3));
    assert!(matches!(c.transport, TransportKind::NbdUnix));
    assert_eq!(c.watchdog, Duration::from_secs(30));
}

#[test]
fn status_mode_needs_no_tenant() {
    let c = parse_config(&["--broker", "h:1", "--status"]);
    assert!(c.status_only);
    assert!(c.tenant.is_empty());
}

#[test]
fn missing_broker_errors() {
    assert!(parse_args(&args(&["--tenant", "x"])).is_err());
}

#[test]
fn unknown_flag_errors() {
    assert!(parse_args(&args(&["--broker", "h:1", "--bogus"])).is_err());
}

#[test]
fn bad_transport_errors() {
    assert!(parse_args(&args(&["--broker", "h:1", "--transport", "rdma"])).is_err());
}

#[test]
fn bad_swap_prio_errors() {
    assert!(parse_args(&args(&["--broker", "h:1", "--swap-prio", "x"])).is_err());
}

#[test]
fn flag_without_value_errors() {
    assert!(parse_args(&args(&["--broker"])).is_err());
}

#[test]
fn help_is_a_parse_outcome() {
    assert!(matches!(
        parse_args(&args(&["--help"])),
        Ok(ParsedArgs::Help)
    ));
}

#[test]
fn usage_diagnostic_adds_usage_once() {
    let usage = usage();
    assert_eq!(
        usage_diagnostic("invalid input"),
        format!("invalid input\n{usage}")
    );
    assert_eq!(usage_diagnostic(&usage), usage);
}

#[test]
fn swap_on_prefers_broker_priority_without_running_swap() {
    let cfg = test_config("127.0.0.1:1".to_string(), Duration::from_secs(1));
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let mut active = HashMap::new();

    assert!(handle_msg(
        &cfg,
        Msg::SwapOn {
            slice: 7,
            export: "s7".to_string(),
            endpoint: NbdEndpoint::Tcp {
                host: "127.0.0.1".to_string(),
                port: 10809,
            },
            swap_prio: Some(-9),
        },
        &mut active,
        &cmd_tx,
    ));
    assert_eq!(
        active.get(&7),
        Some(&SliceRecord::Active {
            dev: "/dev/ramshared-test-nbd7".to_string()
        })
    );
    match cmd_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("SwapOn must be dispatched")
    {
        ExecCmd::On {
            slice,
            export,
            endpoint,
            dev,
            prio,
        } => {
            assert_eq!(slice, 7);
            assert_eq!(export, "s7");
            assert!(matches!(endpoint, NbdEndpoint::Tcp { port: 10809, .. }));
            assert_eq!(dev, "/dev/ramshared-test-nbd7");
            assert_eq!(prio, Some(-9));
        }
        ExecCmd::Off { .. } => panic!("SwapOn must not dispatch SwapOff"),
    }
}

#[test]
fn demote_all_dispatches_release_without_running_swap() {
    let cfg = test_config("127.0.0.1:1".to_string(), Duration::from_secs(1));
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let mut active = HashMap::from([
        (
            1,
            SliceRecord::Active {
                dev: "/dev/ramshared-test-nbd1".to_string(),
            },
        ),
        (
            2,
            SliceRecord::Active {
                dev: "/dev/ramshared-test-nbd2".to_string(),
            },
        ),
    ]);

    assert!(handle_msg(&cfg, Msg::DemoteAll, &mut active, &cmd_tx));
    let mut releases = [
        cmd_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first DemoteAll release must dispatch"),
        cmd_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("second DemoteAll release must dispatch"),
    ]
    .into_iter()
    .map(|cmd| match cmd {
        ExecCmd::Off { slice, dev } => (slice, dev),
        ExecCmd::On { .. } => panic!("DemoteAll must only dispatch SwapOff"),
    })
    .collect::<Vec<_>>();
    releases.sort_unstable();
    assert_eq!(
        releases,
        vec![
            (1, "/dev/ramshared-test-nbd1".to_string()),
            (2, "/dev/ramshared-test-nbd2".to_string()),
        ]
    );
    // AUD-09: dispatch is not commitment. Both devices are still owned
    // until their detach results prove absence.
    assert_eq!(
        active.get(&1),
        Some(&SliceRecord::Draining {
            dev: "/dev/ramshared-test-nbd1".to_string()
        })
    );
    assert_eq!(
        active.get(&2),
        Some(&SliceRecord::Draining {
            dev: "/dev/ramshared-test-nbd2".to_string()
        })
    );
}

#[test]
fn demote_all_stops_when_the_execution_thread_is_gone() {
    // A DemoteAll whose first send already fails must report the refusal
    // instead of looping over a dead channel — and must keep the records
    // it could not dispatch, so a later retry can still find them.
    let cfg = test_config("127.0.0.1:1".to_string(), Duration::from_secs(1));
    let (cmd_tx, cmd_rx) = mpsc::channel();
    drop(cmd_rx);
    let mut active = HashMap::from([(
        1,
        SliceRecord::Active {
            dev: "/dev/ramshared-test-nbd1".to_string(),
        },
    )]);

    assert!(
        !handle_msg(&cfg, Msg::DemoteAll, &mut active, &cmd_tx),
        "a dead execution thread must fail the dispatch"
    );
    assert_eq!(
        active.get(&1),
        Some(&SliceRecord::Draining {
            dev: "/dev/ramshared-test-nbd1".to_string()
        }),
        "AUD-09: a demotion that could not run must not forget the device"
    );
}

#[test]
fn failed_swapoff_keeps_the_device_record() {
    // AUD-09 repro: the record used to be dropped on dispatch and again
    // on the result, so a detach that failed left live swap with no
    // owner and no retry path.
    let mut active = HashMap::from([(
        1,
        SliceRecord::Draining {
            dev: "/dev/ramshared-test-nbd1".to_string(),
        },
    )]);

    let msg = commit_result(
        ExecResult::Off {
            slice: 1,
            ok: false,
            retained: true,
            detail: "swapoff: device busy".to_string(),
        },
        &mut active,
    );
    assert!(
        matches!(
            msg,
            Msg::SwapOffDone {
                slice: 1,
                ok: false,
                ..
            }
        ),
        "the broker must still be told: {msg:?}"
    );
    assert_eq!(
        active.get(&1),
        Some(&SliceRecord::Failed {
            dev: "/dev/ramshared-test-nbd1".to_string()
        }),
        "a failed swapoff must retain the device record"
    );
}

#[test]
fn legitimate_swapoff_releases_the_device_record() {
    // The other half of AUD-09: a proved absence is the only thing that
    // drops the record.
    let mut active = HashMap::from([(
        1,
        SliceRecord::Draining {
            dev: "/dev/ramshared-test-nbd1".to_string(),
        },
    )]);

    let msg = commit_result(
        ExecResult::Off {
            slice: 1,
            ok: true,
            retained: false,
            detail: "/dev/ramshared-test-nbd1".to_string(),
        },
        &mut active,
    );
    assert!(
        matches!(
            msg,
            Msg::SwapOffDone {
                slice: 1,
                ok: true,
                ..
            }
        ),
        "got {msg:?}"
    );
    assert!(active.is_empty(), "a proved absence drops the record");
}

#[test]
fn repeated_stop_is_idempotent_and_queues_one_detach() {
    // a retried `swapoff` must not queue a second detach
    // while one is in flight, and a stop after a failure must retry.
    let cfg = test_config("127.0.0.1:1".to_string(), Duration::from_secs(1));
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let mut active = HashMap::from([(
        1,
        SliceRecord::Active {
            dev: "/dev/ramshared-test-nbd1".to_string(),
        },
    )]);

    assert!(handle_msg(
        &cfg,
        Msg::SwapOff { slice: 1 },
        &mut active,
        &cmd_tx
    ));
    assert!(matches!(
        cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        ExecCmd::Off { slice: 1, .. }
    ));
    assert_eq!(
        active.get(&1),
        Some(&SliceRecord::Draining {
            dev: "/dev/ramshared-test-nbd1".to_string()
        })
    );

    // A repeat while draining dispatches nothing.
    assert!(handle_msg(
        &cfg,
        Msg::SwapOff { slice: 1 },
        &mut active,
        &cmd_tx
    ));
    assert!(
        cmd_rx.try_recv().is_err(),
        "a repeat stop must not queue a second detach"
    );

    // A failed detach leaves it retryable.
    commit_result(
        ExecResult::Off {
            slice: 1,
            ok: false,
            retained: true,
            detail: "swapoff: device busy".to_string(),
        },
        &mut active,
    );
    assert!(handle_msg(
        &cfg,
        Msg::SwapOff { slice: 1 },
        &mut active,
        &cmd_tx
    ));
    assert!(
        matches!(
            cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            ExecCmd::Off { slice: 1, .. }
        ),
        "a stop after a failure must retry the detach"
    );
}

#[test]
fn failed_swapoff_is_still_torn_down_by_cleanup_and_by_demotion() {
    // AUD-09 required: after a failed swapoff, both the end-of-session
    // cleanup and a later demotion must still reach the device.
    let cfg = test_config("127.0.0.1:1".to_string(), Duration::from_secs(1));
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let mut active = HashMap::from([(
        1,
        SliceRecord::Active {
            dev: "/dev/ramshared-test-nbd1".to_string(),
        },
    )]);
    assert!(handle_msg(
        &cfg,
        Msg::SwapOff { slice: 1 },
        &mut active,
        &cmd_tx
    ));
    drop(cmd_rx);
    commit_result(
        ExecResult::Off {
            slice: 1,
            ok: false,
            retained: true,
            detail: "swapoff: device busy".to_string(),
        },
        &mut active,
    );

    // A demotion still dispatches the retained device.
    let (cmd_tx, cmd_rx) = mpsc::channel();
    assert!(handle_msg(&cfg, Msg::DemoteAll, &mut active, &cmd_tx));
    assert!(
        matches!(
            cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            ExecCmd::Off { slice: 1, ref dev } if dev == "/dev/ramshared-test-nbd1"
        ),
        "a demotion must reach a device whose earlier swapoff failed"
    );

    // And the session teardown retries it too.
    commit_result(
        ExecResult::Off {
            slice: 1,
            ok: false,
            retained: true,
            detail: "swapoff: device busy".to_string(),
        },
        &mut active,
    );
    let mut torn: Vec<String> = Vec::new();
    cleanup_owned(&mut active, &mut |dev| {
        torn.push(dev.to_string());
        Ok(())
    });
    assert_eq!(torn, vec!["/dev/ramshared-test-nbd1".to_string()]);
}

#[test]
fn reader_loop_stops_when_the_main_loop_is_gone() {
    // The reader is a pure forwarder: when the main loop drops the channel
    // the reader must exit rather than spin on a closed socket pair.
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener must bind");
    let addr = listener
        .local_addr()
        .expect("listener address must be available");
    let writer = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("reader must connect");
        write_msg(&mut stream, &Msg::Ack).expect("Ack must write");
        // Keep the socket open long enough for the reader to observe the send.
        thread::sleep(Duration::from_millis(50));
    });

    let stream = TcpStream::connect(addr).expect("reader must connect to the fixture");
    let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
    drop(msg_rx); // main loop is gone before the first frame arrives
    reader_loop(
        BufReader::new(stream.try_clone().expect("stream must clone")),
        msg_tx,
    );
    writer.join().expect("fixture writer must finish");
}

#[test]
fn session_registers_dispatches_commands_and_stops_on_refusal() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener must bind");
    let broker = listener
        .local_addr()
        .expect("listener address must be available")
        .to_string();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("agent must connect");
        let mut reader = BufReader::new(stream.try_clone().expect("stream must clone"));
        expect_register_and_psi(&mut reader);
        write_msg(&mut stream, &Msg::Registered { tenant_id: 42 }).expect("Registered must write");
        write_msg(&mut stream, &Msg::Ack).expect("Ack must write");
        write_msg(&mut stream, &Msg::SwapOff { slice: 7 }).expect("SwapOff must write");
        write_msg(
            &mut stream,
            &Msg::LeaseGranted {
                lease: 9,
                bytes: 4096,
            },
        )
        .expect("non-command frame must write");
        write_msg(
            &mut stream,
            &Msg::Error {
                reason: "test refusal".to_string(),
            },
        )
        .expect("broker refusal must write");
    });
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (_res_tx, res_rx) = mpsc::channel();
    let cfg = test_config(broker, Duration::from_secs(1));

    let _ = session(&cfg, &cmd_tx, &res_rx);
    let _ = server.join();
    match cmd_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("broker command must dispatch")
    {
        ExecCmd::Off { slice, dev } => {
            assert_eq!(slice, 7);
            assert_eq!(dev, "/dev/ramshared-test-nbd7");
        }
        ExecCmd::On { .. } => panic!("SwapOff frame must not attach swap"),
    }
    assert!(cmd_rx.try_recv().is_err());
}

#[test]
fn session_reports_execution_results_without_running_swap() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener must bind");
    let broker = listener
        .local_addr()
        .expect("listener address must be available")
        .to_string();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("agent must connect");
        let mut reader = BufReader::new(stream.try_clone().expect("stream must clone"));
        expect_register_and_psi(&mut reader);
        assert!(matches!(
            read_msg(&mut reader).expect("SwapOn completion must decode"),
            Some(Msg::SwapOnDone {
                slice: 3,
                ok: false,
                ref detail,
            }) if detail == "test attach refusal"
        ));
        assert!(matches!(
            read_msg(&mut reader).expect("SwapOff completion must decode"),
            Some(Msg::SwapOffDone {
                slice: 3,
                ok: true,
                ref detail,
            }) if detail == "/dev/ramshared-test-nbd3"
        ));
        write_msg(
            &mut stream,
            &Msg::Error {
                reason: "fixture complete".to_string(),
            },
        )
        .expect("broker refusal must write");
    });
    let (cmd_tx, _cmd_rx) = mpsc::channel();
    let (res_tx, res_rx) = mpsc::channel();
    res_tx
        .send(ExecResult::On {
            slice: 3,
            ok: false,
            retained: false,
            detail: "test attach refusal".to_string(),
        })
        .expect("test result must queue");
    res_tx
        .send(ExecResult::Off {
            slice: 3,
            ok: true,
            retained: false,
            detail: "/dev/ramshared-test-nbd3".to_string(),
        })
        .expect("test result must queue");
    let cfg = test_config(broker, Duration::from_secs(1));

    assert!(session(&cfg, &cmd_tx, &res_rx).is_ok());
    server.join().expect("broker fixture must finish");
}

#[test]
fn session_watchdog_terminates_silent_broker_without_swap() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener must bind");
    let broker = listener
        .local_addr()
        .expect("listener address must be available")
        .to_string();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("agent must connect");
        let mut reader = BufReader::new(stream);
        expect_register_and_psi(&mut reader);
        assert!(
            read_msg(&mut reader)
                .expect("client shutdown must decode as EOF")
                .is_none()
        );
    });
    let (cmd_tx, _cmd_rx) = mpsc::channel();
    let (_res_tx, res_rx) = mpsc::channel();
    let cfg = test_config(broker, Duration::from_millis(20));
    let started = Instant::now();

    let err = session(&cfg, &cmd_tx, &res_rx).unwrap_err();
    assert!(err.to_string().starts_with("watchdog: broker silent"));
    assert!(started.elapsed() < Duration::from_secs(1));
    server.join().expect("silent broker fixture must finish");
}

/// One `StatusReply` with one tenant, one slice, and one IO counter, so the
/// three formatting loops in `run_status` all take their non-empty branch.
pub(super) fn fixture_status_reply() -> Msg {
    Msg::StatusReply {
        tenants: vec![TenantStatus {
            id: 42,
            name: "test-tenant".to_string(),
            psi: Some(PsiSample {
                avg10: 1.5,
                avg60: 0.5,
                stall_us: 7,
            }),
            slices: vec![3],
            present: true,
            bytes_served: 1024,
        }],
        slices: vec![Slice {
            id: 3,
            offset: 4096,
            len: 8192,
            tenant: Some(42),
            state: SliceState::Active,
        }],
        slice_io: vec![SliceIo {
            id: 3,
            bytes_served: 1024,
            io_count: 2,
        }],
        last_rebalance_secs: Some(11),
    }
}
