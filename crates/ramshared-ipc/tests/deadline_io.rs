// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-ipc/tests/deadline_io.rs
//! Absolute exchange budgets; injected streams confer no host authority.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use ramshared_ipc::deadline::DeadlineIo;
use ramshared_ipc::vsock::VsockStreamTrait;
use std::cell::{Cell, RefCell};
use std::io::{self, Cursor, Read, Write};
use std::time::{Duration, Instant};

struct Stream {
    input: Cursor<Vec<u8>>,
    output: Vec<u8>,
    chunk: usize,
    delay: Duration,
    setup_error: bool,
    reads: usize,
    writes: usize,
    shutdowns: Cell<usize>,
    waits: RefCell<Vec<Duration>>,
    interrupt: bool,
    write_error: bool,
    flush_error: bool,
}
impl Default for Stream {
    fn default() -> Self {
        Self {
            input: Cursor::new((0..64).collect()),
            output: vec![],
            chunk: 3,
            delay: Duration::ZERO,
            setup_error: false,
            reads: 0,
            writes: 0,
            shutdowns: Cell::new(0),
            waits: RefCell::new(vec![]),
            interrupt: false,
            write_error: false,
            flush_error: false,
        }
    }
}
impl Read for Stream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.reads += 1;
        if self.interrupt {
            self.interrupt = false;
            return Err(io::ErrorKind::Interrupted.into());
        }
        std::thread::sleep(self.delay);
        let len = buffer.len().min(self.chunk);
        self.input.read(&mut buffer[..len])
    }
}
impl Write for Stream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.writes += 1;
        if self.write_error {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let len = bytes.len().min(self.chunk);
        self.output.extend_from_slice(&bytes[..len]);
        Ok(len)
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.flush_error {
            Err(io::ErrorKind::BrokenPipe.into())
        } else {
            Ok(())
        }
    }
}
impl VsockStreamTrait for Stream {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        if self.setup_error {
            return Err(io::Error::other("required timeout unavailable"));
        }
        self.waits.borrow_mut().push(timeout.unwrap());
        Ok(())
    }
    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(timeout)
    }
    fn shutdown_both(&self) -> io::Result<()> {
        self.shutdowns.set(self.shutdowns.get() + 1);
        Ok(())
    }
}
#[test]
fn deadline_setup_failure_prevents_any_io() {
    let mut stream = Stream {
        setup_error: true,
        ..Stream::default()
    };
    assert!(DeadlineIo::new(&mut stream, Duration::from_secs(1)).is_err());
    assert_eq!((stream.reads, stream.writes), (0, 0));
    assert_eq!(stream.shutdowns.get(), 1);
}
#[test]
fn deadline_exchange_preserves_fragments_and_refuses_dribbling() {
    let mut stream = Stream {
        interrupt: true,
        ..Stream::default()
    };
    let mut exchange = DeadlineIo::new(&mut stream, Duration::from_secs(1)).unwrap();
    let mut data = [0; 12];
    exchange.read_exact(&mut data).unwrap();
    exchange.write_all(b"complete frame").unwrap();
    exchange.flush().unwrap();
    exchange.finish().unwrap();
    assert_eq!(data, std::array::from_fn(|index| index as u8));
    assert_eq!(stream.output, b"complete frame");
    assert_eq!(stream.shutdowns.get(), 0);
    assert!(
        stream
            .waits
            .borrow()
            .windows(2)
            .all(|waits| waits[1] <= waits[0])
    );

    let mut stream = Stream {
        chunk: 1,
        delay: Duration::from_millis(5),
        ..Stream::default()
    };
    let start = Instant::now();
    let mut exchange = DeadlineIo::new(&mut stream, Duration::from_millis(20)).unwrap();
    exchange.shorten(Duration::from_secs(1)).unwrap();
    assert!(exchange.read_exact(&mut data).is_err());
    assert!(exchange.write_all(b"must not restart").is_err());
    drop(exchange);
    assert!(start.elapsed() < Duration::from_millis(500));
    assert!(stream.input.position() < 12);
    assert_eq!(stream.writes, 0);
    assert_eq!(stream.shutdowns.get(), 1);
}

#[test]
fn deadline_real_tcp_round_trip_uses_required_socket_waits() {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let peer = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut exchange = DeadlineIo::new(&mut stream, Duration::from_secs(2)).unwrap();
        let mut bytes = [0; 4];
        exchange.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"ping");
        exchange.write_all(b"pong").unwrap();
        exchange.finish().unwrap();
    });
    let mut stream = std::net::TcpStream::connect(address).unwrap();
    let mut exchange = DeadlineIo::new(&mut stream, Duration::from_secs(2)).unwrap();
    exchange.write_all(b"ping").unwrap();
    let mut response = [0; 4];
    exchange.read_exact(&mut response).unwrap();
    exchange.finish().unwrap();
    assert_eq!(&response, b"pong");
    stream.shutdown_both().unwrap();
    peer.join().unwrap();
}
#[test]
fn deadline_refuses_invalid_budget_write_flush_and_abandoned_exchange() {
    for budget in [Duration::ZERO, Duration::MAX] {
        let mut stream = Stream::default();
        assert!(DeadlineIo::new(&mut stream, budget).is_err());
        assert_eq!((stream.reads, stream.writes), (0, 0));
    }
    for failure in ["write", "flush", "abandoned"] {
        let mut stream = Stream {
            write_error: failure == "write",
            flush_error: failure == "flush",
            ..Stream::default()
        };
        let mut exchange = DeadlineIo::new(&mut stream, Duration::from_secs(1)).unwrap();
        match failure {
            "write" => assert!(exchange.write_all(b"frame").is_err()),
            "flush" => assert!(exchange.flush().is_err()),
            _ => {}
        }
        drop(exchange);
        assert_eq!(stream.shutdowns.get(), 1);
    }
}
