// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-ipc/src/vsock.rs
//! vsock transport abstraction for host-guest IPC.
//!
//! Guest side: `AF_VSOCK` stream socket to `VMADDR_CID_HOST`.
//! Host side: `AF_HYPERV` stream socket on a well-known GUID.
//!

use std::io::{Read, Write};
use std::sync::atomic::AtomicBool;
#[cfg(target_os = "linux")]
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
mod socket_stream;

#[cfg(any(target_os = "linux", test))]
const MAX_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(target_os = "linux")]
const LISTEN_STOP_POLL_SLICE: Duration = Duration::from_millis(250);

#[cfg(any(target_os = "linux", test))]
fn bounded_connect_timeout(timeout: Duration) -> Duration {
    timeout.min(MAX_CONNECT_TIMEOUT)
}

/// vsock address metadata for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VsockEndpoint {
    pub cid: u32,
    pub port: u32,
    pub guid: [u8; 16],
}

/// vsock transport errors.
#[derive(Debug, PartialEq, Eq)]
pub enum VsockError {
    ConnectTimeout,
    ConnectFailed(String),
    ListenFailed(String),
    AcceptTimeout,
    AcceptFailed(String),
    IoError(String),
    Unsupported,
}

impl std::fmt::Display for VsockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConnectTimeout => write!(f, "vsock connect timed out"),
            Self::ConnectFailed(e) => write!(f, "vsock connect failed: {e}"),
            Self::ListenFailed(e) => write!(f, "vsock listen failed: {e}"),
            Self::AcceptTimeout => write!(f, "vsock accept timed out"),
            Self::AcceptFailed(e) => write!(f, "vsock accept failed: {e}"),
            Self::IoError(e) => write!(f, "vsock io error: {e}"),
            Self::Unsupported => write!(f, "vsock not supported on this platform"),
        }
    }
}

impl std::error::Error for VsockError {}

/// Kernel-observed native endpoint. Ordinary stream adapters carry no such scope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VsockPeerIdentity {
    pub vm_id: [u8; 16],
    pub service_id: [u8; 16],
}

/// A connected vsock stream (Read + Write).
pub trait VsockStreamTrait: Read + Write + Send {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()>;
    fn set_write_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()>;
    fn shutdown_both(&self) -> std::io::Result<()>;
    fn native_peer(&self) -> std::io::Result<Option<VsockPeerIdentity>> {
        Ok(None)
    }
}

/// Mock vsock stream backed by `std::os::unix::net::UnixStream` for testing.
#[cfg(unix)]
pub struct MockVsockStream(pub std::os::unix::net::UnixStream);

#[cfg(unix)]
impl Read for MockVsockStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

#[cfg(unix)]
impl Write for MockVsockStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

#[cfg(unix)]
impl VsockStreamTrait for MockVsockStream {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.0.set_read_timeout(timeout)
    }
    fn set_write_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.0.set_write_timeout(timeout)
    }
    fn shutdown_both(&self) -> std::io::Result<()> {
        use std::net::Shutdown;
        self.0.shutdown(Shutdown::Both)
    }
}

/// Connect to a vsock endpoint with bounded timeout.
///
/// On Unix (test/dev): creates a `MockVsockStream` via `UnixStream::pair` is not
/// possible for remote connect. Production uses `libc::socket(AF_VSOCK)`.
/// This function is the production entrypoint and returns `Unsupported` on
/// platforms where AF_VSOCK is not available.
pub fn connect_vsock(
    cid: u32,
    port: u32,
    timeout: Duration,
) -> Result<Box<dyn VsockStreamTrait>, VsockError> {
    #[cfg(target_os = "linux")]
    {
        connect_vsock_linux(cid, port, timeout)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (cid, port, timeout);
        Err(VsockError::Unsupported)
    }
}

/// Listen for one guest-side AF_VSOCK connection and return the accepted
/// stream. The host connector initiates; the guest remains the protocol client.
pub fn listen_vsock(port: u32, timeout: Duration) -> Result<Box<dyn VsockStreamTrait>, VsockError> {
    let never_stop = AtomicBool::new(false);
    listen_vsock_cancellable(port, Some(timeout), &never_stop, None, || {})
}

/// Listen continuously on a guest-side `AF_VSOCK` port until a host peer
/// connects, `timeout` elapses (when `Some`), or `stop` becomes `true`.
///
/// Keeping one bound socket across short poll slices eliminates the closed-port
/// blind window between retry cycles while guaranteeing prompt shutdown when
/// `stop` is raised. If `idle_after` elapses once without a peer, `on_idle` is
/// called once so callers can record a single state transition without log spam.
pub fn listen_vsock_cancellable(
    port: u32,
    timeout: Option<Duration>,
    stop: &AtomicBool,
    idle_after: Option<Duration>,
    on_idle: impl FnMut(),
) -> Result<Box<dyn VsockStreamTrait>, VsockError> {
    #[cfg(target_os = "linux")]
    {
        listen_vsock_linux(port, timeout, stop, idle_after, on_idle)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (port, timeout, stop, idle_after, on_idle);
        Err(VsockError::Unsupported)
    }
}

#[cfg(target_os = "linux")]
fn listen_vsock_linux(
    port: u32,
    timeout: Option<Duration>,
    stop: &AtomicBool,
    idle_after: Option<Duration>,
    mut on_idle: impl FnMut(),
) -> Result<Box<dyn VsockStreamTrait>, VsockError> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    if port == 0 {
        return Err(VsockError::ConnectFailed(
            "vsock port must be nonzero".into(),
        ));
    }
    if stop.load(Ordering::Acquire) {
        return Err(VsockError::AcceptTimeout);
    }
    // SAFETY: socket takes scalar arguments and returns a fresh descriptor on success.
    let raw = unsafe {
        libc::socket(
            libc::AF_VSOCK,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if raw < 0 {
        return Err(VsockError::ConnectFailed(
            std::io::Error::last_os_error().to_string(),
        ));
    }
    // SAFETY: the successful socket call transferred sole descriptor ownership.
    let listener = unsafe { OwnedFd::from_raw_fd(raw) };
    let fd = listener.as_raw_fd();
    let addr = libc::sockaddr_vm {
        svm_family: libc::AF_VSOCK as libc::sa_family_t,
        svm_reserved1: 0,
        svm_port: port,
        svm_cid: u32::MAX,
        svm_zero: [0; 4],
    };
    if unsafe {
        libc::bind(
            fd,
            (&addr as *const libc::sockaddr_vm).cast::<libc::sockaddr>(),
            std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        )
    } != 0
    {
        let error = std::io::Error::last_os_error();
        return Err(VsockError::ConnectFailed(format!(
            "vsock bind failed: {error}"
        )));
    }
    if unsafe { libc::listen(fd, 4) } != 0 {
        let error = std::io::Error::last_os_error();
        return Err(VsockError::ConnectFailed(format!(
            "vsock listen failed: {error}"
        )));
    }
    let started = Instant::now();
    let mut idle_notified = false;
    loop {
        if stop.load(Ordering::Acquire) {
            return Err(VsockError::AcceptTimeout);
        }
        let elapsed = started.elapsed();
        if !idle_notified && idle_after.is_some_and(|after| elapsed >= after) {
            idle_notified = true;
            on_idle();
        }
        let mut slice = match timeout {
            Some(limit) => {
                let remaining = limit.saturating_sub(elapsed);
                if remaining.is_zero() {
                    return Err(VsockError::ConnectTimeout);
                }
                remaining.min(LISTEN_STOP_POLL_SLICE)
            }
            None => LISTEN_STOP_POLL_SLICE,
        };
        if !idle_notified && let Some(after) = idle_after {
            let until_idle = after.saturating_sub(elapsed);
            if !until_idle.is_zero() {
                slice = slice.min(until_idle);
            }
        }
        let mut descriptor = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout_ms = slice
            .as_secs()
            .saturating_mul(1_000)
            .saturating_add(u64::from(slice.subsec_nanos().div_ceil(1_000_000)))
            .clamp(1, i32::MAX as u64) as i32;
        let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if ready > 0 {
            if stop.load(Ordering::Acquire) {
                return Err(VsockError::AcceptTimeout);
            }
            if let Some(stream) = accept_vsock_linux(&listener)? {
                return Ok(Box::new(UnixVsockStream(stream)));
            }
        }
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(VsockError::ConnectFailed(format!(
                "vsock accept poll failed: {error}"
            )));
        }
    }
}

#[cfg(target_os = "linux")]
fn accept_vsock_linux(
    listener: &std::os::fd::OwnedFd,
) -> Result<Option<std::os::unix::net::UnixStream>, VsockError> {
    use std::os::fd::{AsRawFd, FromRawFd};

    // SAFETY: the borrowed listener stays open and null address arguments omit peer output.
    let client = unsafe {
        libc::accept4(
            listener.as_raw_fd(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            libc::SOCK_CLOEXEC,
        )
    };
    if client < 0 {
        let error = std::io::Error::last_os_error();
        return match error.kind() {
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted => Ok(None),
            _ => Err(VsockError::ConnectFailed(format!(
                "vsock accept failed: {error}"
            ))),
        };
    }
    // SAFETY: `accept4` returned a fresh owned descriptor. `UnixStream` only
    // supplies stream I/O and timeout operations for the connected AF_VSOCK fd.
    let stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(client) };
    stream
        .set_nonblocking(false)
        .map_err(|error| VsockError::ConnectFailed(error.to_string()))?;
    Ok(Some(stream))
}

#[cfg(target_os = "linux")]
fn connect_vsock_linux(
    cid: u32,
    port: u32,
    timeout: Duration,
) -> Result<Box<dyn VsockStreamTrait>, VsockError> {
    use std::os::unix::io::FromRawFd;

    if port == 0 {
        return Err(VsockError::ConnectFailed(
            "vsock port must be nonzero".into(),
        ));
    }

    let fd = unsafe {
        libc::socket(
            libc::AF_VSOCK,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(VsockError::ConnectFailed(
            std::io::Error::last_os_error().to_string(),
        ));
    }

    // SAFETY: `socket` returned a fresh owned descriptor. `UnixStream` only
    // supplies generic stream I/O and timeout operations; Linux accepts any
    // connected SOCK_STREAM descriptor here, including AF_VSOCK.
    let stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
    let addr = libc::sockaddr_vm {
        svm_family: libc::AF_VSOCK as libc::sa_family_t,
        svm_reserved1: 0,
        svm_port: port,
        svm_cid: cid,
        svm_zero: [0; 4],
    };

    let result = unsafe {
        libc::connect(
            fd,
            (&addr as *const libc::sockaddr_vm).cast::<libc::sockaddr>(),
            std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        )
    };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINPROGRESS | libc::EALREADY | libc::EINTR) => {
                wait_for_connect_until(
                    bounded_connect_timeout(timeout),
                    |remaining| poll_connect_ready(fd, remaining),
                    || socket_connect_error(fd),
                )?;
            }
            _ => return Err(VsockError::ConnectFailed(error.to_string())),
        }
    }

    stream
        .set_nonblocking(false)
        .map_err(|error| VsockError::ConnectFailed(error.to_string()))?;
    Ok(Box::new(UnixVsockStream(stream)))
}

#[cfg(target_os = "linux")]
fn poll_connect_ready(fd: std::os::fd::RawFd, remaining: Duration) -> std::io::Result<bool> {
    let mut descriptor = libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    };
    let timeout_ms = remaining
        .as_secs()
        .saturating_mul(1_000)
        .saturating_add(u64::from(remaining.subsec_nanos().div_ceil(1_000_000)))
        .min(i32::MAX as u64) as i32;
    let result = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
    if result < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(result > 0)
    }
}

#[cfg(target_os = "linux")]
fn socket_connect_error(fd: std::os::fd::RawFd) -> std::io::Result<()> {
    let mut error_code: libc::c_int = 0;
    let mut length = std::mem::size_of_val(&error_code) as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            (&mut error_code as *mut libc::c_int).cast::<libc::c_void>(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if error_code != 0 {
        return Err(std::io::Error::from_raw_os_error(error_code));
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn wait_for_connect_until(
    timeout: Duration,
    mut wait_for_writable: impl FnMut(Duration) -> std::io::Result<bool>,
    mut read_socket_error: impl FnMut() -> std::io::Result<()>,
) -> Result<(), VsockError> {
    let started = Instant::now();
    loop {
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(VsockError::ConnectTimeout);
        }
        match wait_for_writable(remaining) {
            Ok(false) => return Err(VsockError::ConnectTimeout),
            Ok(true) if started.elapsed() >= timeout => return Err(VsockError::ConnectTimeout),
            Ok(true) => {
                return read_socket_error()
                    .map_err(|error| VsockError::ConnectFailed(error.to_string()));
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(VsockError::ConnectFailed(error.to_string())),
        }
    }
}

#[cfg(target_os = "linux")]
struct UnixVsockStream(std::os::unix::net::UnixStream);

#[cfg(target_os = "linux")]
impl Read for UnixVsockStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

#[cfg(target_os = "linux")]
impl Write for UnixVsockStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

#[cfg(target_os = "linux")]
impl VsockStreamTrait for UnixVsockStream {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.0.set_read_timeout(timeout)
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.0.set_write_timeout(timeout)
    }

    fn shutdown_both(&self) -> std::io::Result<()> {
        self.0.shutdown(std::net::Shutdown::Both)
    }
}

/// A boxed stream is a stream: `connect_vsock` hands back
/// `Box<dyn VsockStreamTrait>`, and the control-plane composition owns that
/// box directly.
impl VsockStreamTrait for Box<dyn VsockStreamTrait> {
    fn native_peer(&self) -> std::io::Result<Option<VsockPeerIdentity>> {
        (**self).native_peer()
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        (**self).set_read_timeout(timeout)
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        (**self).set_write_timeout(timeout)
    }

    fn shutdown_both(&self) -> std::io::Result<()> {
        (**self).shutdown_both()
    }
}

/// Bind the host side of a Hyper-V socket service.
///
/// `guid` is the service GUID in canonical UUID byte order. Hyper-V requires
/// the service to be registered in the host GuestCommunicationServices registry.
pub fn listen_hyperv(guid: [u8; 16]) -> Result<VsockListener, VsockError> {
    let port =
        service_port_from_guid(guid).map_err(|error| VsockError::ListenFailed(error.into()))?;
    #[cfg(windows)]
    {
        windows_hyperv::listen(guid, port)
    }
    #[cfg(not(windows))]
    {
        let _ = (guid, port);
        Err(VsockError::Unsupported)
    }
}

/// Connect to a Linux guest's AF_VSOCK listener through AF_HYPERV.
///
/// `vm_id` is the guest VM GUID in canonical UUID byte order; `guid` is the
/// service GUID carrying the guest port. The host connector is the mirror of
/// [`listen_hyperv`] and is used by the native host-to-guest WSL2 path.
pub fn connect_hyperv(
    vm_id: [u8; 16],
    guid: [u8; 16],
    timeout: Duration,
) -> Result<Box<dyn VsockStreamTrait>, VsockError> {
    let _port =
        service_port_from_guid(guid).map_err(|error| VsockError::ConnectFailed(error.into()))?;
    #[cfg(windows)]
    {
        windows_hyperv::connect(vm_id, guid, timeout)
    }
    #[cfg(not(windows))]
    {
        let _ = (vm_id, guid, timeout);
        Err(VsockError::Unsupported)
    }
}

/// vsock listener (host side).
pub struct VsockListener {
    endpoint: VsockEndpoint,
    #[cfg(windows)]
    inner: windows_hyperv::Listener,
}

impl VsockListener {
    /// Return the endpoint metadata used to create this listener.
    pub fn endpoint(&self) -> VsockEndpoint {
        self.endpoint
    }

    /// Accept one connection, returning `AcceptTimeout` when no peer arrives by
    /// the deadline. The returned stream owns the accepted socket.
    pub fn accept(&self, timeout: Duration) -> Result<Box<dyn VsockStreamTrait>, VsockError> {
        #[cfg(windows)]
        {
            self.inner.accept(timeout)
        }
        #[cfg(not(windows))]
        {
            let _ = timeout;
            Err(VsockError::Unsupported)
        }
    }
}

#[cfg(any(windows, test))]
fn accept_until<T>(
    timeout: Duration,
    mut try_accept: impl FnMut() -> Result<Option<T>, VsockError>,
) -> Result<T, VsockError> {
    let started = Instant::now();
    loop {
        if let Some(stream) = try_accept()? {
            return Ok(stream);
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(VsockError::AcceptTimeout);
        }
        std::thread::sleep(remaining.min(Duration::from_millis(5)));
    }
}

#[cfg(any(windows, test))]
fn guid_components(bytes: [u8; 16]) -> (u32, u16, u16, [u8; 8]) {
    (
        u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        u16::from_be_bytes([bytes[4], bytes[5]]),
        u16::from_be_bytes([bytes[6], bytes[7]]),
        [
            bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        ],
    )
}

const LINUX_VSOCK_GUID_SUFFIX: [u8; 12] = [
    0xfa, 0xcb, 0x11, 0xe6, 0xbd, 0x58, 0x64, 0x00, 0x6a, 0x79, 0x86, 0xd3,
];

/// Build the Linux guest service GUID for `port`.
///
/// Hyper-V routes a Linux guest's `AF_VSOCK` port to the host listener whose
/// service GUID carries that port in its first 32-bit field.
pub fn linux_service_guid(port: u32) -> [u8; 16] {
    let mut guid = [0u8; 16];
    guid[..4].copy_from_slice(&port.to_be_bytes());
    guid[4..].copy_from_slice(&LINUX_VSOCK_GUID_SUFFIX);
    guid
}

fn service_port_from_guid(guid: [u8; 16]) -> Result<u32, &'static str> {
    if guid[4..] != LINUX_VSOCK_GUID_SUFFIX {
        return Err("service GUID must use the Linux Hyper-V socket template");
    }
    let port = u32::from_be_bytes([guid[0], guid[1], guid[2], guid[3]]);
    if port == 0 {
        return Err("service GUID must encode a nonzero vsock port");
    }
    if port > 0x7FFF_FFFF {
        return Err("service GUID port exceeds the Linux vsock range 0..=0x7FFFFFFF");
    }
    Ok(port)
}

#[cfg(windows)]
mod windows_hyperv;

/// Detect whether vsock is available on this system.
pub fn vsock_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::FromRawFd;
        let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            false
        } else {
            // SAFETY: a successful socket call returned a fresh owned descriptor.
            drop(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) });
            true
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

#[cfg(test)]
mod tests;
