// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/tests/control_plane_deadlines.rs
//! Serving timeout refusal; no key provisioning, driver calls or storage effects.
#![allow(clippy::unwrap_used)]
use ramshared_ipc::vsock::VsockStreamTrait;
use ramshared_winsvc::vsock_lease::{LeasePolicy, serve_connection};
use std::cell::Cell;
use std::io::{self, Read, Write};
use std::time::Duration;
#[derive(Default)]
struct RefusedTimeout {
    reads: usize,
    writes: usize,
    closed: Cell<bool>,
}
impl Read for RefusedTimeout {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        self.reads += 1;
        Ok(0)
    }
}
impl Write for RefusedTimeout {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.writes += 1;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl VsockStreamTrait for RefusedTimeout {
    fn set_read_timeout(&self, _: Option<Duration>) -> io::Result<()> {
        Err(io::Error::other("socket timeouts unavailable"))
    }
    fn set_write_timeout(&self, _: Option<Duration>) -> io::Result<()> {
        Err(io::Error::other("socket timeouts unavailable"))
    }
    fn shutdown_both(&self) -> io::Result<()> {
        self.closed.set(true);
        Ok(())
    }
}
#[test]
fn host_timeout_setup_failure_never_serves_handshake() {
    let mut stream = RefusedTimeout::default();
    assert!(
        serve_connection(
            &mut stream,
            b"test-only-secret",
            LeasePolicy::new(5, 15).unwrap(),
            || 0,
            ramshared_winsvc::vsock_lease::HostSamples::default
        )
        .is_err()
    );
    assert_eq!(
        stream.reads, 0,
        "timeout refusal must precede any host protocol IO"
    );
    assert_eq!(stream.writes, 0);
    assert!(stream.closed.get());
}
