// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-ipc/src/vsock/tests.rs
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::time::Instant;

#[test]
fn vsock_connect_finishes_within_deadline() {
    // CID 2 (host) with a high port — should fail or timeout on most systems.
    // On systems with vsock and no listener, connect returns ECONNREFUSED.
    // On systems without vsock, connect returns ENODEV/EINVAL.
    // On systems with a listener (unlikely on random port), connect succeeds.
    let start = Instant::now();
    let result = connect_vsock(2, 50000, Duration::from_millis(200));
    let elapsed = start.elapsed();

    // Must complete within the bounded window regardless of outcome.
    assert!(
        elapsed < Duration::from_millis(500),
        "connect timeout must be bounded: {elapsed:?}"
    );
    // If it failed, the error must be typed. If it succeeded, the stream must be usable.
    match result {
        Err(VsockError::ConnectTimeout) => {}
        Err(VsockError::ConnectFailed(_)) => {}
        Err(VsockError::Unsupported) => {}
        Ok(_) => {
            // Connection succeeded — this is acceptable on systems with vsock
            // and a listening service on the target port.
        }
        Err(e) => panic!("unexpected error type: {e}"),
    }
}

#[cfg(target_os = "linux")]
#[test]
fn vsock_listen_rejects_zero_port() {
    assert!(matches!(
        listen_vsock(0, Duration::from_millis(10)),
        Err(VsockError::ConnectFailed(_))
    ));
}

#[cfg(target_os = "linux")]
#[test]
fn vsock_listen_reports_a_bound_port() {
    let port = 50002;
    let first = std::thread::spawn(move || listen_vsock(port, Duration::from_millis(400)));
    std::thread::sleep(Duration::from_millis(100));
    let second = listen_vsock(port, Duration::from_millis(100));
    assert!(matches!(
        second,
        Err(VsockError::ConnectFailed(_)) | Err(VsockError::Unsupported)
    ));
    let _ = first.join();
}

#[cfg(not(windows))]
#[test]
fn connect_hyperv_is_unsupported_off_windows() {
    assert!(matches!(
        connect_hyperv(
            [0u8; 16],
            linux_service_guid(2761),
            Duration::from_millis(1)
        ),
        Err(VsockError::Unsupported)
    ));
}

#[cfg(target_os = "linux")]
#[test]
fn vsock_listen_finishes_within_deadline() {
    let start = Instant::now();
    let result = listen_vsock(50001, Duration::from_millis(200));
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(600),
        "accept timeout must be bounded: {elapsed:?}"
    );
    assert!(matches!(
        result,
        Err(VsockError::ConnectTimeout)
            | Err(VsockError::ConnectFailed(_))
            | Err(VsockError::Unsupported)
    ));
}

#[cfg(target_os = "linux")]
#[test]
fn vsock_listen_cancellable_stops_promptly_and_fires_idle_once() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let already_stopped = AtomicBool::new(true);
    assert!(matches!(
        listen_vsock_cancellable(50003, None, &already_stopped, None, || {}),
        Err(VsockError::AcceptTimeout)
    ));

    let stop = Arc::new(AtomicBool::new(false));
    let stop_trigger = Arc::clone(&stop);
    let idle_hits = Arc::new(AtomicUsize::new(0));
    let idle_hits_cb = Arc::clone(&idle_hits);
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(120));
        stop_trigger.store(true, Ordering::Release);
    });
    let started = Instant::now();
    let result = listen_vsock_cancellable(
        50004,
        None,
        &stop,
        Some(Duration::from_millis(20)),
        move || {
            idle_hits_cb.fetch_add(1, Ordering::SeqCst);
        },
    );
    stopper.join().unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(900),
        "cancellable listener must stop promptly instead of stalling: {elapsed:?}"
    );
    match result {
        Err(VsockError::AcceptTimeout) => {
            assert_eq!(
                idle_hits.load(Ordering::SeqCst),
                1,
                "idle transition callback must fire at most once"
            );
        }
        Err(VsockError::ConnectFailed(_)) | Err(VsockError::Unsupported) => {}
        Err(other) => panic!("unexpected cancellable listen error: {other}"),
        Ok(_) => panic!("unexpected connection on ephemeral test port"),
    }
}

#[cfg(target_os = "linux")]
#[test]
fn vsock_connect_rejects_zero_port_before_socket_io() {
    let result = connect_vsock(2, 0, Duration::from_millis(100));
    assert!(matches!(
        result,
        Err(VsockError::ConnectFailed(message)) if message.contains("nonzero")
    ));
}

#[test]
fn connect_wait_enforces_deadline_when_waiter_returns_late() {
    let started = Instant::now();
    let result = wait_for_connect_until(
        Duration::from_millis(20),
        |_| {
            std::thread::sleep(Duration::from_millis(35));
            Ok(true)
        },
        || Ok(()),
    );

    assert!(matches!(result, Err(VsockError::ConnectTimeout)));
    assert!(started.elapsed() < Duration::from_millis(250));
}

#[test]
fn connect_wait_reports_socket_error_after_writable() {
    let result = wait_for_connect_until(
        Duration::from_millis(50),
        |_| Ok(true),
        || Err(std::io::Error::from_raw_os_error(111)),
    );

    assert!(matches!(result, Err(VsockError::ConnectFailed(_))));
}

#[test]
fn connect_wait_accepts_success_before_deadline() {
    let result = wait_for_connect_until(Duration::from_millis(50), |_| Ok(true), || Ok(()));

    assert!(result.is_ok());
}

#[test]
fn connect_wait_retries_interrupted_poll_and_times_out_when_not_writable() {
    let attempts = std::cell::Cell::new(0);
    let result = wait_for_connect_until(
        Duration::from_secs(1),
        |_| {
            if attempts.replace(attempts.get() + 1) == 0 {
                Err(std::io::Error::from(std::io::ErrorKind::Interrupted))
            } else {
                Ok(true)
            }
        },
        || Ok(()),
    );
    assert!(result.is_ok());
    assert_eq!(attempts.get(), 2);

    let result = wait_for_connect_until(
        Duration::from_secs(1),
        |_| Ok(false),
        || panic!("socket error must not be queried before the socket is writable"),
    );
    assert!(matches!(result, Err(VsockError::ConnectTimeout)));
}

#[test]
fn connect_timeout_is_capped_at_spec_limit() {
    assert_eq!(
        bounded_connect_timeout(Duration::MAX),
        Duration::from_secs(5)
    );
    assert_eq!(
        bounded_connect_timeout(Duration::from_millis(125)),
        Duration::from_millis(125)
    );
}

#[test]
#[cfg(unix)]
fn vsock_stream_read_timeout_is_bounded() {
    let (client, _server) = std::os::unix::net::UnixStream::pair().unwrap();
    let mut stream = MockVsockStream(client);
    stream
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();

    let start = Instant::now();
    let mut buf = [0u8; 16];
    let result = stream.read(&mut buf);
    let elapsed = start.elapsed();

    assert!(result.is_err(), "read on empty socket must timeout");
    assert!(
        elapsed < Duration::from_millis(500),
        "read timeout must be bounded: {elapsed:?}"
    );
}

#[test]
#[cfg(unix)]
fn vsock_disconnect_detected_within_interval() {
    let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
    let mut stream = MockVsockStream(client);
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();

    // Drop the server side to simulate disconnect.
    drop(server);

    let start = Instant::now();
    let mut buf = [0u8; 16];
    let result = stream.read(&mut buf);
    let elapsed = start.elapsed();

    // Read on disconnected socket returns EOF (Ok(0)) or error.
    match result {
        Ok(0) => {}  // EOF = disconnect detected
        Err(_) => {} // error = disconnect detected
        Ok(n) => panic!("unexpected read of {n} bytes after disconnect"),
    }
    assert!(
        elapsed < Duration::from_millis(500),
        "disconnect detection must be bounded: {elapsed:?}"
    );
}

#[test]
fn vsock_available_detects_platform() {
    // On Linux CI without /dev/vsock, this returns false.
    // On a real WSL2 host with vsock, this returns true.
    let _ = vsock_available(); // no assert — platform-dependent
}

#[test]
fn vsock_error_display_is_informative() {
    let cases = [
        (VsockError::ConnectTimeout, "vsock connect timed out"),
        (
            VsockError::ConnectFailed("fixture".into()),
            "vsock connect failed: fixture",
        ),
        (
            VsockError::ListenFailed("fixture".into()),
            "vsock listen failed: fixture",
        ),
        (VsockError::AcceptTimeout, "vsock accept timed out"),
        (
            VsockError::AcceptFailed("fixture".into()),
            "vsock accept failed: fixture",
        ),
        (
            VsockError::IoError("fixture".into()),
            "vsock io error: fixture",
        ),
        (
            VsockError::Unsupported,
            "vsock not supported on this platform",
        ),
    ];

    for (error, expected) in cases {
        assert_eq!(error.to_string(), expected);
    }
}

#[test]
#[cfg(unix)]
fn mock_vsock_stream_write_and_flush() {
    let (client, _server) = std::os::unix::net::UnixStream::pair().unwrap();
    let mut stream = MockVsockStream(client);
    let data = b"hello vsock";
    let n = stream.write(data).unwrap();
    assert_eq!(n, data.len());
    assert!(stream.flush().is_ok());
}

#[test]
#[cfg(unix)]
fn mock_vsock_stream_trait_methods() {
    let (client, _server) = std::os::unix::net::UnixStream::pair().unwrap();
    let stream = MockVsockStream(client);
    assert!(
        stream
            .set_read_timeout(Some(Duration::from_millis(50)))
            .is_ok()
    );
    assert!(
        stream
            .set_write_timeout(Some(Duration::from_millis(50)))
            .is_ok()
    );
    assert!(stream.shutdown_both().is_ok());
}

#[cfg(not(windows))]
#[test]
fn listen_hyperv_returns_unsupported_off_windows() {
    let result = listen_hyperv([
        0, 0, 0x0a, 0xc9, 0xfa, 0xcb, 0x11, 0xe6, 0xbd, 0x58, 0x64, 0x00, 0x6a, 0x79, 0x86, 0xd3,
    ]);
    assert!(matches!(result, Err(VsockError::Unsupported)));
}

#[test]
fn vsock_accept_timeout_is_bounded() {
    let started = Instant::now();
    let result = accept_until(Duration::from_millis(20), || {
        Ok::<_, VsockError>(None::<()>)
    });
    assert!(matches!(result, Err(VsockError::AcceptTimeout)));
    assert!(started.elapsed() < Duration::from_millis(250));
}

#[test]
fn vsock_accept_returns_first_connection() {
    let mut next = Some(42_u8);
    let result = accept_until(Duration::from_millis(50), || Ok(next.take()));
    assert_eq!(result, Ok(42));
}

#[test]
fn vsock_accept_propagates_socket_error() {
    let result = accept_until(Duration::from_millis(50), || {
        Err::<Option<()>, _>(VsockError::AcceptFailed("injected".into()))
    });
    assert!(matches!(result, Err(VsockError::AcceptFailed(message)) if message == "injected"));
}

#[test]
fn hyperv_guid_uses_canonical_uuid_byte_order() {
    let bytes = [
        0x00, 0x00, 0x0a, 0xc9, 0xfa, 0xcb, 0x11, 0xe6, 0xbd, 0x58, 0x64, 0x00, 0x6a, 0x79, 0x86,
        0xd3,
    ];
    assert_eq!(
        guid_components(bytes),
        (
            2761,
            0xfacb,
            0x11e6,
            [0xbd, 0x58, 0x64, 0x00, 0x6a, 0x79, 0x86, 0xd3]
        )
    );
}

#[test]
fn linux_service_guid_round_trips_the_port() {
    for port in [1u32, 2761, 0x7FFF_FFFF] {
        assert_eq!(service_port_from_guid(linux_service_guid(port)), Ok(port));
    }
    assert!(service_port_from_guid(linux_service_guid(0)).is_err());
    assert!(service_port_from_guid(linux_service_guid(0x8000_0000)).is_err());
}

#[test]
fn hyperv_linux_service_guid_requires_the_port_template() {
    let valid = [
        0x00, 0x00, 0x0a, 0xc9, 0xfa, 0xcb, 0x11, 0xe6, 0xbd, 0x58, 0x64, 0x00, 0x6a, 0x79, 0x86,
        0xd3,
    ];
    assert_eq!(service_port_from_guid(valid), Ok(2761));

    let mut wrong_template = valid;
    wrong_template[4] = 0;
    assert!(service_port_from_guid(wrong_template).is_err());

    let no_port = [
        0, 0, 0, 0, 0xfa, 0xcb, 0x11, 0xe6, 0xbd, 0x58, 0x64, 0x00, 0x6a, 0x79, 0x86, 0xd3,
    ];
    assert!(service_port_from_guid(no_port).is_err());
}

#[test]
fn vsock_endpoint_fields_are_accessible() {
    let ep = VsockEndpoint {
        cid: 3,
        port: 5000,
        guid: [1; 16],
    };
    assert_eq!(ep.cid, 3);
    assert_eq!(ep.port, 5000);
    assert_eq!(ep.guid, [1; 16]);
}

#[cfg(target_os = "linux")]
#[test]
fn unix_vsock_stream_forwards_io_timeouts_and_shutdown() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    let (client, mut peer) = UnixStream::pair().unwrap();
    let mut stream = UnixVsockStream(client);
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    peer.set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();

    stream.write_all(b"request").unwrap();
    stream.flush().unwrap();
    let mut request = [0; 7];
    peer.read_exact(&mut request).unwrap();
    assert_eq!(&request, b"request");

    peer.write_all(b"reply").unwrap();
    let mut reply = [0; 5];
    stream.read_exact(&mut reply).unwrap();
    assert_eq!(&reply, b"reply");

    stream.shutdown_both().unwrap();
    let mut trailing = [0; 1];
    assert_eq!(peer.read(&mut trailing).unwrap(), 0);
}

#[cfg(target_os = "linux")]
#[test]
fn nonblocking_accept_retries_empty_queue_and_returns_blocking_stream() {
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::os::unix::net::{UnixListener, UnixStream};

    let dir = std::env::temp_dir().join(format!("ramshared-vsock-accept-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("listener");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let fd: OwnedFd = listener.into();
    assert!(accept_vsock_linux(&fd).unwrap().is_none());

    let _client = UnixStream::connect(&path).unwrap();
    let stream = accept_vsock_linux(&fd).unwrap().unwrap();
    // SAFETY: the stream owns the descriptor throughout the query.
    let flags = unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(flags & libc::O_NONBLOCK, 0);
    std::fs::remove_file(path).unwrap();
    std::fs::remove_dir(dir).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn socket_connect_error_rejects_an_invalid_descriptor() {
    assert!(socket_connect_error(-1).is_err());
}

#[cfg(not(windows))]
#[test]
fn listener_exposes_its_endpoint_and_refuses_accept_off_windows() {
    let endpoint = VsockEndpoint {
        cid: 0,
        port: 5000,
        guid: [7; 16],
    };
    let listener = VsockListener { endpoint };
    assert_eq!(listener.endpoint(), endpoint);
    assert!(matches!(
        listener.accept(Duration::ZERO),
        Err(VsockError::Unsupported)
    ));
}
