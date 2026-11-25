// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/shutdown_signals.rs
//! Checked asynchronous shutdown signal installation and joinable bridge.

use core::ffi::c_int;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ramshared_wsl2d::WMsg;

pub(super) const SIGINT: c_int = libc::SIGINT;
pub(super) const SIGTERM: c_int = libc::SIGTERM;

/// Process-wide shutdown flag written only from [`handle_shutdown`].
pub(super) static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Async-signal-safe handler that records the shutdown request in [`SHUTDOWN`].
pub(super) extern "C" fn handle_shutdown(_sig: c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SignalHandlerTarget {
    Shutdown,
    Previous(libc::sighandler_t),
}

fn install_posix_signal(
    signum: c_int,
    target: SignalHandlerTarget,
) -> Result<libc::sighandler_t, std::io::Error> {
    let raw = match target {
        SignalHandlerTarget::Shutdown => handle_shutdown as *const () as libc::sighandler_t,
        SignalHandlerTarget::Previous(previous) => previous,
    };
    // SAFETY: valid POSIX signal numbers and async-signal-safe handler or prior displacement.
    let previous = unsafe { libc::signal(signum, raw) };
    if previous == libc::SIG_ERR {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(previous)
    }
}

/// Registers `SIGINT` and `SIGTERM` handlers before listeners or kernel devices exist.
/// If `SIGTERM` registration fails after `SIGINT` succeeded, restores the prior `SIGINT` handler.
pub(super) fn install_shutdown_signals_with<F>(
    mut register: F,
) -> Result<(), Box<dyn std::error::Error>>
where
    F: FnMut(c_int, SignalHandlerTarget) -> Result<libc::sighandler_t, std::io::Error>,
{
    let previous_sigint = register(SIGINT, SignalHandlerTarget::Shutdown)?;
    if let Err(error) = register(SIGTERM, SignalHandlerTarget::Shutdown) {
        let _ = register(SIGINT, SignalHandlerTarget::Previous(previous_sigint));
        return Err(error.into());
    }
    Ok(())
}

/// Installs the process-wide `SIGINT` and `SIGTERM` handlers and fails on `SIG_ERR`.
pub(super) fn install_shutdown_signals() -> Result<(), Box<dyn std::error::Error>> {
    install_shutdown_signals_with(install_posix_signal)
}

/// Stoppable, joined polling bridge mirroring a static shutdown flag into an explicit wake action.
pub(super) struct ShutdownBridge {
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl ShutdownBridge {
    pub(super) fn spawn(
        shutdown: &'static AtomicBool,
        poll_interval: Duration,
        on_shutdown: impl FnOnce() + Send + 'static,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            while !shutdown.load(Ordering::SeqCst) && !worker_stop.load(Ordering::SeqCst) {
                std::thread::sleep(poll_interval);
            }
            if shutdown.load(Ordering::SeqCst) {
                on_shutdown();
            }
        });
        Self {
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for ShutdownBridge {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub(super) type NbdShutdownBridge = ShutdownBridge;

pub(super) fn spawn_nbd_shutdown_bridge(
    shutdown: &'static AtomicBool,
    jobs_tx: std::sync::mpsc::SyncSender<WMsg>,
    poll_interval: Duration,
) -> NbdShutdownBridge {
    ShutdownBridge::spawn(shutdown, poll_interval, move || {
        let _ = jobs_tx.try_send(WMsg::Shutdown);
    })
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    #[test]
    fn install_shutdown_signals_checks_sigint_and_restores_on_sigterm_error() {
        let mut calls = Vec::new();
        let err = install_shutdown_signals_with(|sig, target| {
            calls.push((sig, target));
            Err(Error::from(ErrorKind::PermissionDenied))
        })
        .expect_err("SIGINT failure must return early");
        assert!(err.to_string().contains("permission denied"));
        assert_eq!(calls, vec![(SIGINT, SignalHandlerTarget::Shutdown)]);

        let mut restore_calls = Vec::new();
        let err = install_shutdown_signals_with(|sig, target| {
            restore_calls.push((sig, target));
            if sig == SIGTERM && matches!(target, SignalHandlerTarget::Shutdown) {
                Err(Error::from(ErrorKind::InvalidInput))
            } else {
                Ok(42)
            }
        })
        .expect_err("SIGTERM failure must fail closed and restore SIGINT");
        assert!(err.to_string().contains("invalid input"));
        assert_eq!(
            restore_calls,
            vec![
                (SIGINT, SignalHandlerTarget::Shutdown),
                (SIGTERM, SignalHandlerTarget::Shutdown),
                (SIGINT, SignalHandlerTarget::Previous(42)),
            ]
        );
    }

    #[test]
    fn install_shutdown_signals_succeeds_and_bridge_joins_without_signal() {
        let mut registered = Vec::new();
        install_shutdown_signals_with(|sig, target| {
            registered.push((sig, target));
            Ok(0)
        })
        .expect("injected registration succeeds");
        assert_eq!(
            registered,
            vec![
                (SIGINT, SignalHandlerTarget::Shutdown),
                (SIGTERM, SignalHandlerTarget::Shutdown),
            ]
        );

        static IDLE_FLAG: AtomicBool = AtomicBool::new(false);
        IDLE_FLAG.store(false, Ordering::SeqCst);
        let settled = Arc::new(AtomicBool::new(false));
        let callback_ran = Arc::new(AtomicBool::new(false));
        let settled_probe = Arc::clone(&settled);
        let callback_probe = Arc::clone(&callback_ran);
        let bridge = ShutdownBridge::spawn(&IDLE_FLAG, Duration::from_millis(5), move || {
            callback_probe.store(true, Ordering::SeqCst);
        });
        drop(bridge);
        settled_probe.store(true, Ordering::SeqCst);
        assert!(settled.load(Ordering::SeqCst));
        assert!(
            !callback_ran.load(Ordering::SeqCst),
            "stopping bridge without signal must not emit shutdown"
        );
    }

    #[test]
    fn shutdown_bridge_runs_callback_once_flag_flips() {
        static WAKE_FLAG: AtomicBool = AtomicBool::new(false);
        WAKE_FLAG.store(false, Ordering::SeqCst);
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let bridge = ShutdownBridge::spawn(&WAKE_FLAG, Duration::from_millis(5), move || {
            let _ = tx.send(true);
        });
        WAKE_FLAG.store(true, Ordering::SeqCst);
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)), Ok(true));
        drop(bridge);
        WAKE_FLAG.store(false, Ordering::SeqCst);
    }
}
