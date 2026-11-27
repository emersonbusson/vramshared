// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-exec/src/systemd_wake.rs
//! Confined ownership of the single update notification listener.

use std::io;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::path::Path;

const LISTEN_FD: libc::c_int = 3;
const MAX_PENDING: usize = 16;

/// Discard the bounded pending connections for this process's exact listener.
/// The timer also starts the service when there is no activation descriptor.
pub fn consume(expected_socket: &Path) -> io::Result<()> {
    let pid = std::process::id().to_string();
    if std::env::var("LISTEN_PID").as_deref() != Ok(pid.as_str()) {
        return Ok(());
    }
    if std::env::var("LISTEN_FDS").as_deref() != Ok("1")
        || std::env::var("LISTEN_FDNAMES").as_deref() != Ok("update-wake")
    {
        return Err(invalid());
    }
    let listener = UnixListener::from(duplicate_inherited()?);
    if listener.local_addr()?.as_pathname() != Some(expected_socket) {
        return Err(invalid());
    }
    // SAFETY: fcntl takes scalar arguments. The process was delegated this
    // exact listener and keeps it open until exit.
    if unsafe { libc::fcntl(LISTEN_FD, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    drain(&listener, expected_socket)
}

fn duplicate_inherited() -> io::Result<OwnedFd> {
    // SAFETY: fcntl validates its scalar descriptor argument. A successful
    // F_DUPFD_CLOEXEC returns a newly owned, close-on-exec descriptor.
    let raw = unsafe { libc::fcntl(LISTEN_FD, libc::F_DUPFD_CLOEXEC, LISTEN_FD + 1) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: raw is the distinct descriptor returned by the successful fcntl.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "unexpected updater activation listener",
    )
}

fn drain(listener: &UnixListener, expected_socket: &Path) -> io::Result<()> {
    if listener.local_addr()?.as_pathname() != Some(expected_socket) {
        return Err(invalid());
    }
    listener.set_nonblocking(true)?;
    for _ in 0..MAX_PENDING {
        match listener.accept() {
            Ok((peer, _)) => drop(peer),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn mismatched_listener_leaves_the_connection_queued() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wake.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let _peer = UnixStream::connect(&path).unwrap();
        assert!(drain(&listener, &tmp.path().join("wrong.sock")).is_err());
        listener.set_nonblocking(true).unwrap();
        assert!(listener.accept().is_ok());
    }

    #[test]
    fn matched_listener_drains_without_a_message_and_leaves_no_wait() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("wake.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let _peer = UnixStream::connect(&path).unwrap();
        drain(&listener, &path).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drain(&listener, &path).unwrap();
    }
}
