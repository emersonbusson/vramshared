// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-ipc/src/vsock/windows_hyperv/stream.rs
//! Non-blocking `AF_HYPERV` stream wrapper using `select()` for bounded I/O.
//!
//! Windows `hvsocket.sys` aborts (`WSAECONNABORTED`, 10053) a blocking `AF_HYPERV`
//! socket when a `SO_RCVTIMEO`/`SO_SNDTIMEO` IRP expires during idle waits such as
//! `DeadlineIo::STOP_POLL` (50 ms). Keeping the connected socket in `FIONBIO = 1`
//! mode and bounding waits with Winsock `select()` returns `ErrorKind::TimedOut`
//! cleanly without tearing down the VMBus channel.

use super::WinsockRuntime;
use crate::vsock::{VsockError, VsockPeerIdentity, VsockStreamTrait};
use std::io::{self, ErrorKind, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows_sys::Win32::Networking::WinSock::{
    FD_SET, FIONBIO, SD_BOTH, SOCKET, TIMEVAL, WSAEINTR, WSAEWOULDBLOCK, WSAGetLastError,
    closesocket, ioctlsocket, recv, select, send, shutdown,
};

pub(super) struct HypervStream {
    socket: SOCKET,
    read_timeout: Mutex<Option<Duration>>,
    write_timeout: Mutex<Option<Duration>>,
    _runtime: Arc<WinsockRuntime>,
    peer: VsockPeerIdentity,
}

impl HypervStream {
    pub(super) fn new(
        socket: SOCKET,
        runtime: Arc<WinsockRuntime>,
        peer: VsockPeerIdentity,
    ) -> Result<Self, VsockError> {
        let mut nonblocking: u32 = 1;
        // SAFETY: `socket` is uniquely owned here and `nonblocking` is a valid live u32.
        if unsafe { ioctlsocket(socket, FIONBIO, &mut nonblocking) } != 0 {
            let error = unsafe { WSAGetLastError() };
            // SAFETY: still uniquely owned; ownership never moved into `Self`.
            unsafe { closesocket(socket) };
            return Err(VsockError::IoError(format!(
                "nonblocking socket setup failed with Winsock error {error}"
            )));
        }
        Ok(Self {
            socket,
            read_timeout: Mutex::new(None),
            write_timeout: Mutex::new(None),
            _runtime: runtime,
            peer,
        })
    }

    fn timeout_value(slot: &Mutex<Option<Duration>>) -> io::Result<Option<Duration>> {
        slot.lock()
            .map(|guard| *guard)
            .map_err(|_| io::Error::other("vsock timeout lock poisoned"))
    }

    fn set_timeout_value(
        slot: &Mutex<Option<Duration>>,
        timeout: Option<Duration>,
    ) -> io::Result<()> {
        if matches!(timeout, Some(d) if d.is_zero()) {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "cannot set a 0 duration timeout",
            ));
        }
        let mut guard = slot
            .lock()
            .map_err(|_| io::Error::other("vsock timeout lock poisoned"))?;
        *guard = timeout;
        Ok(())
    }

    fn wait_ready(&self, write: bool, deadline: Option<Instant>) -> io::Result<()> {
        loop {
            let tv;
            let timeout_ptr = match deadline {
                Some(end) => {
                    let Some(rem) = end.checked_duration_since(Instant::now()) else {
                        return Err(io::Error::new(ErrorKind::TimedOut, "timed out"));
                    };
                    if rem.is_zero() {
                        return Err(io::Error::new(ErrorKind::TimedOut, "timed out"));
                    }
                    tv = duration_to_timeval(rem);
                    std::ptr::addr_of!(tv)
                }
                None => std::ptr::null(),
            };
            let mut fds = FD_SET {
                fd_count: 1,
                fd_array: [0; 64],
            };
            fds.fd_array[0] = self.socket;
            let (readfds, writefds) = if write {
                (std::ptr::null_mut(), std::ptr::addr_of_mut!(fds))
            } else {
                (std::ptr::addr_of_mut!(fds), std::ptr::null_mut())
            };
            // SAFETY: valid single-entry `FD_SET` and optional `TIMEVAL` pointer.
            let ready = unsafe { select(0, readfds, writefds, std::ptr::null_mut(), timeout_ptr) };
            if ready > 0 {
                return Ok(());
            }
            if ready == 0 {
                return Err(io::Error::new(ErrorKind::TimedOut, "timed out"));
            }
            let err = unsafe { WSAGetLastError() };
            if err != WSAEINTR {
                return Err(io::Error::from_raw_os_error(err));
            }
        }
    }
}

impl Drop for HypervStream {
    fn drop(&mut self) {
        // SAFETY: `self.socket` is uniquely owned by this `HypervStream`.
        let _ = unsafe { closesocket(self.socket) };
    }
}

impl Read for HypervStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let len = buf.len().min(i32::MAX as usize) as i32;
        let deadline = deadline_from_timeout(Self::timeout_value(&self.read_timeout)?)?;
        loop {
            // SAFETY: valid mutable slice of `len` bytes on an open socket.
            let n = unsafe { recv(self.socket, buf.as_mut_ptr(), len, 0) };
            if n >= 0 {
                return Ok(n as usize);
            }
            let err = unsafe { WSAGetLastError() };
            if err == WSAEINTR {
                continue;
            }
            if err != WSAEWOULDBLOCK {
                return Err(io::Error::from_raw_os_error(err));
            }
            self.wait_ready(false, deadline)?;
        }
    }
}

impl Write for HypervStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let len = buf.len().min(i32::MAX as usize) as i32;
        let deadline = deadline_from_timeout(Self::timeout_value(&self.write_timeout)?)?;
        loop {
            // SAFETY: valid shared slice of `len` bytes on an open socket.
            let n = unsafe { send(self.socket, buf.as_ptr(), len, 0) };
            if n >= 0 {
                return Ok(n as usize);
            }
            let err = unsafe { WSAGetLastError() };
            if err == WSAEINTR {
                continue;
            }
            if err != WSAEWOULDBLOCK {
                return Err(io::Error::from_raw_os_error(err));
            }
            self.wait_ready(true, deadline)?;
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl VsockStreamTrait for HypervStream {
    fn native_peer(&self) -> io::Result<Option<VsockPeerIdentity>> {
        Ok(Some(self.peer))
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        Self::set_timeout_value(&self.read_timeout, timeout)
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        Self::set_timeout_value(&self.write_timeout, timeout)
    }

    fn shutdown_both(&self) -> io::Result<()> {
        // SAFETY: `self.socket` remains open until `Drop`.
        if unsafe { shutdown(self.socket, SD_BOTH) } == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }))
        }
    }
}

fn deadline_from_timeout(timeout: Option<Duration>) -> io::Result<Option<Instant>> {
    timeout
        .map(|duration| {
            Instant::now().checked_add(duration).ok_or_else(|| {
                io::Error::new(ErrorKind::InvalidInput, "vsock timeout is too large")
            })
        })
        .transpose()
}

fn duration_to_timeval(duration: Duration) -> TIMEVAL {
    let tv_sec = duration.as_secs().min(i32::MAX as u64) as i32;
    let tv_usec = if duration.as_secs() > i32::MAX as u64 {
        999_999
    } else {
        duration.subsec_micros() as i32
    };
    TIMEVAL {
        tv_sec,
        tv_usec: if tv_sec == 0 && tv_usec == 0 {
            1
        } else {
            tv_usec
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::os::windows::io::IntoRawSocket;

    #[test]
    fn excessive_timeout_refuses_without_panicking() {
        assert!(matches!(
            deadline_from_timeout(Some(Duration::MAX)),
            Err(error) if error.kind() == ErrorKind::InvalidInput
        ));
        assert!(matches!(deadline_from_timeout(None), Ok(None)));
    }

    #[test]
    fn read_timeout_preserves_socket_for_subsequent_io() -> io::Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let client = TcpStream::connect(listener.local_addr()?)?;
        let (mut server, _) = listener.accept()?;
        let runtime = WinsockRuntime::start().map_err(io::Error::other)?;
        let mut stream = HypervStream::new(
            client.into_raw_socket() as SOCKET,
            runtime,
            VsockPeerIdentity {
                vm_id: [1; 16],
                service_id: [2; 16],
            },
        )
        .map_err(io::Error::other)?;
        stream.set_read_timeout(Some(Duration::from_millis(10)))?;
        let mut byte = [0];
        assert!(
            matches!(stream.read(&mut byte), Err(error) if error.kind() == ErrorKind::TimedOut)
        );
        server.write_all(b"x")?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.read_exact(&mut byte)?;
        assert_eq!(byte, *b"x");
        stream.write_all(b"y")?;
        server.read_exact(&mut byte)?;
        assert_eq!(byte, *b"y");
        assert!(
            matches!(stream.set_read_timeout(Some(Duration::ZERO)), Err(error) if error.kind() == ErrorKind::InvalidInput)
        );
        Ok(())
    }
}
