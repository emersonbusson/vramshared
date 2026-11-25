// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/systemd_notify.rs
// Verified systemd notification client (SPEC zero-script-native-runtime
// ITEM-3, RF-4, DT-4). The environment variable is removed before any child
// worker can inherit it, and a non-root filesystem socket is refused.

#![allow(unsafe_code)]

use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug)]
pub enum NotifyError {
    Missing,
    RelativePath,
    NotASocket,
    NonRootOwner,
    Io(std::io::Error),
}

impl std::fmt::Display for NotifyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => formatter.write_str("NOTIFY_SOCKET is not set"),
            Self::RelativePath => formatter.write_str("NOTIFY_SOCKET must be absolute"),
            Self::NotASocket => formatter.write_str("NOTIFY_SOCKET is not a socket"),
            Self::NonRootOwner => formatter.write_str("NOTIFY_SOCKET is not owned by uid 0"),
            Self::Io(error) => write!(formatter, "systemd notify io: {error}"),
        }
    }
}

impl std::error::Error for NotifyError {}

impl From<std::io::Error> for NotifyError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

pub struct NotifySocket {
    socket: UnixDatagram,
    path: PathBuf,
}

impl NotifySocket {
    /// Read `NOTIFY_SOCKET`, remove it from the environment immediately, then
    /// validate and connect. Removal happens even when validation fails so a
    /// later child cannot inherit a spoofable endpoint.
    pub fn from_env() -> Result<Option<Self>, NotifyError> {
        Self::from_env_with_owner(true)
    }

    /// Test/lab variant. Production always uses [`Self::from_env`], which
    /// requires a root-owned socket.
    pub fn from_env_with_owner(require_root: bool) -> Result<Option<Self>, NotifyError> {
        let Some(value) = std::env::var_os("NOTIFY_SOCKET") else {
            return Ok(None);
        };
        // SAFETY: the daemon calls `from_env` during startup before any worker
        // thread exists. No other thread may access the environment
        // concurrently; this is the same single-threaded configuration window
        // the systemd protocol requires.
        unsafe { std::env::remove_var("NOTIFY_SOCKET") };
        let path = PathBuf::from(value);
        Self::from_path(&path, require_root).map(Some)
    }

    pub fn from_path(path: &Path, require_root: bool) -> Result<Self, NotifyError> {
        if !path.is_absolute() {
            return Err(NotifyError::RelativePath);
        }
        let metadata = std::fs::metadata(path)?;
        if !metadata.file_type().is_socket() {
            return Err(NotifyError::NotASocket);
        }
        #[cfg(unix)]
        if require_root {
            use std::os::unix::fs::MetadataExt;
            if metadata.uid() != 0 {
                return Err(NotifyError::NonRootOwner);
            }
        }
        let socket = UnixDatagram::unbound()?;
        socket.connect(path)?;
        Ok(Self {
            socket,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn send_ready(&self) -> Result<(), NotifyError> {
        self.send(b"READY=1")
    }

    pub fn send_watchdog(&self) -> Result<(), NotifyError> {
        self.send(b"WATCHDOG=1")
    }

    pub fn send_stopping(&self) -> Result<(), NotifyError> {
        self.send(b"STOPPING=1")
    }

    fn send(&self, payload: &[u8]) -> Result<(), NotifyError> {
        self.socket.send(payload)?;
        Ok(())
    }
}

/// Send `READY=1` with the daemon PID and keep the socket for watchdog ticks.
/// A missing `NOTIFY_SOCKET` is not an error: the daemon runs the same way
/// under a plain shell or a container.
pub fn notify_ready(pid: u32) -> Result<Option<Arc<NotifySocket>>, NotifyError> {
    let Some(socket) = NotifySocket::from_env()? else {
        return Ok(None);
    };
    socket.send(format!("READY=1\nMAINPID={pid}").as_bytes())?;
    Ok(Some(Arc::new(socket)))
}

/// Send one watchdog keep-alive.
pub fn notify_watchdog(socket: &NotifySocket) -> Result<(), NotifyError> {
    socket.send_watchdog()
}

/// systemd asks for a tick at half the configured `WATCHDOG_USEC` interval.
/// Absent, zero, or malformed values mean "no watchdog configured".
#[must_use]
pub fn watchdog_interval_from_usec(raw: Option<&str>) -> Option<Duration> {
    let usec = raw?.trim().parse::<u64>().ok()?;
    if usec == 0 {
        return None;
    }
    Some(Duration::from_micros(usec / 2).max(Duration::from_millis(1)))
}

/// Read `WATCHDOG_USEC` once at startup.
#[must_use]
pub fn watchdog_interval() -> Option<Duration> {
    watchdog_interval_from_usec(std::env::var("WATCHDOG_USEC").ok().as_deref())
}

/// Supervised keep-alive thread. The thread ends when the socket send fails;
/// the process itself owns the lifetime, so no join is required at shutdown.
pub fn spawn_watchdog(
    socket: Arc<NotifySocket>,
    interval: Duration,
) -> Option<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("ramsharedd-systemd-watchdog".into())
        .spawn(move || {
            loop {
                std::thread::sleep(interval);
                if notify_watchdog(&socket).is_err() {
                    break;
                }
            }
        })
        .ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "ramshared-notify-unit-{}-{id}-{name}",
            std::process::id()
        ))
    }

    #[test]
    fn notify_error_display_and_path_rejections_are_total() {
        for error in [
            NotifyError::Missing,
            NotifyError::RelativePath,
            NotifyError::NotASocket,
            NotifyError::NonRootOwner,
            NotifyError::Io(std::io::Error::other("x")),
        ] {
            assert!(!error.to_string().is_empty());
        }
        let io: NotifyError = std::io::Error::other("x").into();
        assert!(matches!(io, NotifyError::Io(_)));
        assert!(matches!(
            NotifySocket::from_path(Path::new("relative.sock"), false),
            Err(NotifyError::RelativePath)
        ));
        let missing = temp_path("missing.sock");
        assert!(matches!(
            NotifySocket::from_path(&missing, false),
            Err(NotifyError::Io(_))
        ));
        let regular = temp_path("regular");
        std::fs::write(&regular, b"x").unwrap();
        assert!(matches!(
            NotifySocket::from_path(&regular, false),
            Err(NotifyError::NotASocket)
        ));
        let _ = std::fs::remove_file(&regular);
    }

    #[test]
    fn notify_environment_is_removed_even_when_the_socket_is_invalid() {
        // SAFETY: this unit test mutates the process environment before any
        // thread is spawned; the test harness is single-threaded here.
        unsafe { std::env::remove_var("NOTIFY_SOCKET") };
        assert!(NotifySocket::from_env_with_owner(false).unwrap().is_none());
        let regular = temp_path("env-regular");
        std::fs::write(&regular, b"x").unwrap();
        // SAFETY: same single-threaded setup window as above.
        unsafe { std::env::set_var("NOTIFY_SOCKET", &regular) };
        assert!(matches!(
            NotifySocket::from_env_with_owner(false),
            Err(NotifyError::NotASocket)
        ));
        assert!(std::env::var_os("NOTIFY_SOCKET").is_none());
        let _ = std::fs::remove_file(&regular);
    }

    #[test]
    fn watchdog_interval_parses_and_notify_payloads_round_trip() {
        assert_eq!(
            watchdog_interval_from_usec(Some("1000000")),
            Some(Duration::from_millis(500))
        );
        assert_eq!(
            watchdog_interval_from_usec(Some("1")),
            Some(Duration::from_millis(1))
        );
        assert_eq!(watchdog_interval_from_usec(Some("0")), None);
        assert_eq!(watchdog_interval_from_usec(Some("nope")), None);
        assert_eq!(watchdog_interval_from_usec(None), None);

        let path = temp_path("watchdog.sock");
        let receiver = UnixDatagram::bind(&path).unwrap();
        let client = NotifySocket::from_path(&path, false).unwrap();
        notify_watchdog(&client).unwrap();
        let mut buffer = [0u8; 64];
        let read = receiver.recv(&mut buffer).unwrap();
        assert_eq!(&buffer[..read], b"WATCHDOG=1");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn watchdog_thread_ticks_until_the_socket_closes() {
        let path = temp_path("spawn-watchdog.sock");
        let receiver = UnixDatagram::bind(&path).unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let socket = Arc::new(NotifySocket::from_path(&path, false).unwrap());
        let Some(handle) = spawn_watchdog(socket, Duration::from_millis(5)) else {
            panic!("watchdog thread did not start");
        };
        let mut buffer = [0u8; 32];
        let read = receiver.recv(&mut buffer).unwrap();
        assert_eq!(&buffer[..read], b"WATCHDOG=1");
        receiver.shutdown(std::net::Shutdown::Both).unwrap();
        drop(receiver);
        let _ = std::fs::remove_file(&path);
        handle.join().unwrap();
    }

    #[test]
    fn notify_send_surfaces_a_closed_receiver() {
        let path = temp_path("closed.sock");
        let receiver = UnixDatagram::bind(&path).unwrap();
        let client = NotifySocket::from_path(&path, false).unwrap();
        assert_eq!(client.path(), path);
        // Model a concurrently forked child retaining the same socket.
        let inherited_receiver = receiver.try_clone().unwrap();
        receiver.shutdown(std::net::Shutdown::Both).unwrap();
        drop(receiver);
        let _ = std::fs::remove_file(&path);
        assert!(client.send_ready().is_err());
        drop(inherited_receiver);
    }
}
