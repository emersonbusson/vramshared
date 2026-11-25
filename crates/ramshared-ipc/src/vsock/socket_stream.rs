// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-ipc/src/vsock/socket_stream.rs
//! Standard socket adapters for bounded protocol exchange and platform controls.
use super::VsockStreamTrait;
use std::time::Duration;

impl VsockStreamTrait for std::net::TcpStream {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        Self::set_read_timeout(self, timeout)
    }
    fn set_write_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        Self::set_write_timeout(self, timeout)
    }
    fn shutdown_both(&self) -> std::io::Result<()> {
        self.shutdown(std::net::Shutdown::Both)
    }
}
#[cfg(unix)]
impl VsockStreamTrait for std::os::unix::net::UnixStream {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        Self::set_read_timeout(self, timeout)
    }
    fn set_write_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        Self::set_write_timeout(self, timeout)
    }
    fn shutdown_both(&self) -> std::io::Result<()> {
        self.shutdown(std::net::Shutdown::Both)
    }
}
