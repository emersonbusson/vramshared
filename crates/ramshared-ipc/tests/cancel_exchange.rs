// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-ipc/tests/cancel_exchange.rs
//! Cancellation retains the original transaction deadline and exact-stream refusal.
#![allow(clippy::unwrap_used)]
use ramshared_ipc::deadline::DeadlineIo;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
fn pair() -> (std::net::TcpStream, std::net::TcpStream) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    (client, listener.accept().unwrap().0)
}
#[test]
fn cancelled_exchange_refuses_before_io_and_interrupts_silent_peer() {
    let (mut client, mut server) = pair();
    let stop = AtomicBool::new(true);
    assert!(DeadlineIo::new_cancellable(&mut client, Duration::from_secs(5), &stop).is_err());
    server
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    assert_eq!(server.read(&mut [0; 1]).unwrap(), 0);
    let (mut client, _server) = pair();
    let stop = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&stop);
    let worker = std::thread::spawn(move || {
        let mut exchange =
            DeadlineIo::new_cancellable(&mut client, Duration::from_secs(5), &signal).unwrap();
        exchange.read_exact(&mut [0; 4]).unwrap_err()
    });
    std::thread::sleep(Duration::from_millis(50));
    let requested = Instant::now();
    stop.store(true, Ordering::Release);
    assert_eq!(
        worker.join().unwrap().kind(),
        std::io::ErrorKind::ConnectionAborted
    );
    assert!(requested.elapsed() < Duration::from_secs(1));
}
#[test]
fn cancellation_polling_preserves_fragment_progress_and_absolute_deadline() {
    let (mut client, mut server) = pair();
    let writer = std::thread::spawn(move || {
        for byte in b"DATA" {
            server.write_all(&[*byte]).unwrap();
            std::thread::sleep(Duration::from_millis(35));
        }
    });
    let stop = AtomicBool::new(false);
    let mut exchange =
        DeadlineIo::new_cancellable(&mut client, Duration::from_secs(1), &stop).unwrap();
    let mut bytes = [0; 4];
    exchange.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"DATA");
    exchange.finish().unwrap();
    writer.join().unwrap();
    let (mut client, _server) = pair();
    let before = Instant::now();
    let mut exchange =
        DeadlineIo::new_cancellable(&mut client, Duration::from_millis(140), &stop).unwrap();
    assert!(exchange.read_exact(&mut [0; 1]).is_err());
    assert!(before.elapsed() < Duration::from_secs(1));
}
