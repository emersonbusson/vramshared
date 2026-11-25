// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-agent/src/socket.rs
//! Bounded TCP setup shared by the agent and the host bridge (AUD-11).
//!
//! A connect that can block forever is a foreign call the in-loop watchdog
//! cannot cancel, and a write to a peer that stopped reading can hang the
//! single writer the same way. Both are bounded here, at socket setup, so no
//! caller can forget them.
//!
//! Reads on the long-lived session socket are deliberately **not** bounded by a
//! socket timeout: the session watchdog is the silence authority, and a read
//! timeout that fires mid-frame would desync the length-prefixed protocol. A
//! one-shot caller (`--status`) sets its own read deadline.

use std::io;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Absolute deadline for establishing a broker connection (AUD-11).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Write deadline for agent-originated frames (AUD-11). A peer that accepts
/// and then never reads must fail the write, not hold the writer.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Connects to `addr` under [`CONNECT_TIMEOUT`] and arms its write side under
/// [`WRITE_TIMEOUT`].
pub fn connect_bounded(addr: &str) -> io::Result<TcpStream> {
    connect_bounded_with(addr, CONNECT_TIMEOUT, WRITE_TIMEOUT)
}

/// [`connect_bounded`] with explicit budgets. Split out so the bound itself is
/// testable without waiting on the production constants.
///
/// Every resolved address is tried under `connect_timeout`; the first success
/// wins and the last failure is reported. Name resolution is the system
/// resolver's own bound and is not repeated here.
pub fn connect_bounded_with(
    addr: &str,
    connect_timeout: Duration,
    write_timeout: Duration,
) -> io::Result<TcpStream> {
    let addrs = addr.to_socket_addrs()?;
    connect_resolved(addrs, connect_timeout, write_timeout, addr)
}

/// [`connect_bounded_with`] against an already-resolved address list. Split out
/// so the empty-list and all-failed outcomes are testable without steering the
/// system resolver.
///
/// `addr_label` is only what a refusal names; it is never resolved or opened.
fn connect_resolved(
    addrs: impl IntoIterator<Item = SocketAddr>,
    connect_timeout: Duration,
    write_timeout: Duration,
    addr_label: &str,
) -> io::Result<TcpStream> {
    let mut last_err: Option<io::Error> = None;
    for sock_addr in addrs {
        match TcpStream::connect_timeout(&sock_addr, connect_timeout) {
            Ok(stream) => {
                stream.set_write_timeout(Some(write_timeout))?;
                return Ok(stream);
            }
            Err(error) => last_err = Some(error),
        }
    }
    // Exhausted without a stream. The loop stores a failure for every address
    // it tried, so `last_err` is unset exactly when the list was empty — and
    // that is an unresolvable name, not a connection that refused us.
    Err(last_err.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("'{addr_label}' resolved to no addresses"),
        )
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::time::Instant;

    /// AUD-11 required test: a connection blackhole must not hang the caller.
    /// The blackhole is a reserved (RFC 5737) address that never answers. The
    /// contract under test is not "it times out" but "it returns inside the
    /// budget" — an immediate network error satisfies that just as well.
    #[test]
    fn a_connection_blackhole_returns_inside_the_budget() {
        let budget = Duration::from_millis(200);
        let started = Instant::now();
        let result = connect_bounded_with("192.0.2.1:1", budget, budget);
        let elapsed = started.elapsed();
        assert!(result.is_err(), "a blackhole must not succeed");
        assert!(
            elapsed < budget * 10,
            "connect was not bounded by {budget:?}: took {elapsed:?}"
        );
    }

    /// AUD-11 required test: a non-reading peer must fail the write, not hold
    /// the writer. The peer accepts and then never reads, so the send buffer
    /// fills and the write deadline has to be what ends the call.
    #[test]
    fn a_non_reading_peer_fails_the_write_instead_of_holding_it() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let peer = thread::spawn(move || {
            let (held, _) = listener.accept().expect("accept");
            // Never read. Hold the socket open so the writer sees a live peer.
            thread::sleep(Duration::from_secs(5));
            drop(held);
        });

        let mut stream = connect_bounded_with(
            &addr.to_string(),
            Duration::from_secs(2),
            Duration::from_millis(200),
        )
        .expect("connect to the fixture");
        let started = Instant::now();
        let mut outcome = Ok(());
        // Far more than any socket buffer: the write must hit its deadline.
        for _ in 0..4096 {
            match stream.write_all(&[0u8; 64 * 1024]) {
                Ok(()) => {}
                Err(e) => {
                    outcome = Err(e);
                    break;
                }
            }
        }
        let elapsed = started.elapsed();
        let error = outcome.expect_err("a non-reading peer must eventually fail the write");
        assert!(
            matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ),
            "expected a write deadline, got {error:?}"
        );
        assert!(
            elapsed < Duration::from_secs(4),
            "the write deadline did not end the call: took {elapsed:?}"
        );
        let _ = peer.join();
    }

    /// The successful path must arm the write side: that is the half of the
    /// contract a caller cannot see from `Ok(stream)`.
    #[test]
    fn a_connected_stream_carries_the_write_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let peer = thread::spawn(move || {
            let (mut held, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 16];
            let _ = held.read(&mut buf);
        });
        let stream = connect_bounded_with(
            &addr.to_string(),
            Duration::from_secs(2),
            Duration::from_secs(7),
        )
        .expect("connect");
        assert_eq!(
            stream.write_timeout().expect("write_timeout"),
            Some(Duration::from_secs(7)),
            "connect_bounded must arm the write side"
        );
        drop(stream);
        let _ = peer.join();
    }

    #[test]
    fn an_unresolvable_address_is_refused_without_hanging() {
        let budget = Duration::from_millis(200);
        let started = Instant::now();
        let result = connect_bounded_with("ramshared.invalid:1", budget, budget);
        assert!(result.is_err(), "an unresolvable name must not succeed");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "resolution was not bounded: {:?}",
            started.elapsed()
        );
    }

    /// An empty resolved list is an unresolvable name, never a fabricated
    /// connection failure: the two refusals have to stay distinguishable.
    #[test]
    fn an_empty_address_list_is_refused_as_unresolvable() {
        let error = connect_resolved(
            [],
            Duration::from_millis(200),
            Duration::from_millis(200),
            "ramshared-empty:1",
        )
        .expect_err("an empty address list must not succeed");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(
            error.to_string().contains("ramshared-empty:1"),
            "the refusal must name the address it was asked for: {error}"
        );
    }

    /// The first resolved address is not the only one. A dead first hop must
    /// fall through to the next rather than report the name unresolvable.
    #[test]
    fn a_refused_first_address_falls_through_to_the_next() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let live = listener.local_addr().expect("local_addr");
        let dead = SocketAddr::new(live.ip(), 1);
        let peer = thread::spawn(move || {
            let (mut held, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 16];
            let _ = held.read(&mut buf);
        });

        let stream = connect_resolved(
            [dead, live],
            Duration::from_secs(2),
            Duration::from_secs(7),
            "ramshared-fallthrough:1",
        )
        .expect("the live address must win after the dead one refuses");
        assert_eq!(
            stream.write_timeout().expect("write_timeout"),
            Some(Duration::from_secs(7)),
            "the winning stream still carries the write deadline"
        );
        drop(stream);
        let _ = peer.join();
    }

    /// Every address failing is a connection refusal, not an unresolvable
    /// name. The reported error is the last attempt's, so the caller sees why
    /// the peer was unreachable rather than a resolution claim that is false.
    #[test]
    fn every_refused_address_reports_the_last_failure() {
        let closed: SocketAddr = "127.0.0.1:1".parse().expect("literal address");
        let error = connect_resolved(
            [closed, closed],
            Duration::from_millis(500),
            Duration::from_millis(500),
            "ramshared-all-refused:1",
        )
        .expect_err("a closed port must refuse the connection");
        assert_ne!(
            error.kind(),
            io::ErrorKind::InvalidInput,
            "a refusal is not an unresolvable name"
        );
        assert!(
            !error.to_string().contains("resolved to no addresses"),
            "a connection failure must not masquerade as a resolution failure: {error}"
        );
    }

    /// The production entry point has to arm the write side too — that is the
    /// half of the contract a caller cannot see from `Ok(stream)`.
    #[test]
    fn the_production_entry_point_arms_the_write_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let peer = thread::spawn(move || {
            let (mut held, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 16];
            let _ = held.read(&mut buf);
        });
        let stream = connect_bounded(&addr.to_string()).expect("connect");
        assert_eq!(
            stream.write_timeout().expect("write_timeout"),
            Some(WRITE_TIMEOUT),
            "connect_bounded must arm the write side under the production budget"
        );
        drop(stream);
        let _ = peer.join();
    }
}
