// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-winsvc/src/watchdog/wsl_scm_watchdog.rs
// Native SCM watchdog (SPEC zero-script-native-runtime ITEM-4, RF-6, DT-6).
// The loop is a std::thread inside ramshared-winsvc; it never spawns
// powershell.exe, pwsh.exe or wscript.exe. The live AF_HYPERV probe is a
// transport adapter supplied by the SCM service.

use crate::thread_custody::OwnedThread;
use std::io;
use std::time::Duration;

pub const WATCHDOG_INTERVAL: Duration = Duration::from_secs(5);
pub const WATCHDOG_FAILURE_THRESHOLD: u32 = 3;
#[cfg(windows)]
pub mod native;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WatchdogHealth {
    Healthy,
    Disconnected,
    Failed,
}

pub trait WatchdogTransport: Send + 'static {
    fn probe(&mut self) -> Result<(), String>;
}

pub struct WslScmWatchdog<T: WatchdogTransport> {
    transport: T,
    health: WatchdogHealth,
    consecutive_failures: u32,
    ticks: u64,
}

impl<T: WatchdogTransport> WslScmWatchdog<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            health: WatchdogHealth::Healthy,
            consecutive_failures: 0,
            ticks: 0,
        }
    }

    pub fn health(&self) -> WatchdogHealth {
        self.health
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    pub fn ticks(&self) -> u64 {
        self.ticks
    }

    /// One reconciliation tick. A transient probe failure does not trip the
    /// breaker until the configured consecutive threshold is reached.
    pub fn tick(&mut self) -> WatchdogHealth {
        self.ticks = self.ticks.saturating_add(1);
        match self.transport.probe() {
            Ok(()) => {
                self.consecutive_failures = 0;
                self.health = WatchdogHealth::Healthy;
            }
            Err(_) => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                if self.consecutive_failures >= WATCHDOG_FAILURE_THRESHOLD {
                    self.health = WatchdogHealth::Disconnected;
                }
            }
        }
        self.health
    }
}

/// The watchdog stays active through a reversible storage stop intent. A blocked
/// native probe retains its owned thread until it actually returns and joins.
pub fn spawn_watchdog<T: WatchdogTransport>(
    transport: T,
    interval: Duration,
) -> io::Result<OwnedThread> {
    if interval.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "watchdog interval must be positive",
        ));
    }
    OwnedThread::spawn("ramshared-scm-watchdog", move |stop| {
        let mut watchdog = WslScmWatchdog::new(transport);
        while !stop.is_requested() {
            let _ = watchdog.tick();
            stop.wait(interval.min(WATCHDOG_INTERVAL));
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    struct MockTransport {
        results: Vec<Result<(), String>>,
    }

    impl WatchdogTransport for MockTransport {
        fn probe(&mut self) -> Result<(), String> {
            if self.results.is_empty() {
                Ok(())
            } else {
                self.results.remove(0)
            }
        }
    }

    #[test]
    fn wsl_scm_watchdog_reconciles_without_spawning_interpreters() {
        let transport = MockTransport {
            results: vec![
                Ok(()),
                Err("timeout".into()),
                Err("timeout".into()),
                Err("timeout".into()),
            ],
        };
        let mut watchdog = WslScmWatchdog::new(transport);
        assert_eq!(watchdog.health(), WatchdogHealth::Healthy);
        assert_eq!(watchdog.tick(), WatchdogHealth::Healthy);
        assert_eq!(watchdog.tick(), WatchdogHealth::Healthy);
        assert_eq!(watchdog.tick(), WatchdogHealth::Healthy);
        assert_eq!(watchdog.tick(), WatchdogHealth::Disconnected);
        assert_eq!(watchdog.consecutive_failures(), 3);
        assert_eq!(watchdog.ticks(), 4);

        let source = include_str!("wsl_scm_watchdog.rs");
        for forbidden in [
            "Command::new(\"powershell.exe\")",
            "Command::new(\"pwsh.exe\")",
            "Command::new(\"wscript.exe\")",
        ] {
            assert!(!source.contains(forbidden));
        }
    }

    #[test]
    fn watchdog_recovers_after_a_successful_tick() {
        let transport = MockTransport {
            results: vec![Err("x".into()), Err("x".into()), Ok(())],
        };
        let mut watchdog = WslScmWatchdog::new(transport);
        assert_eq!(watchdog.health(), WatchdogHealth::Healthy);
        assert_eq!(watchdog.tick(), WatchdogHealth::Healthy);
        assert_eq!(watchdog.tick(), WatchdogHealth::Healthy);
        assert_eq!(watchdog.tick(), WatchdogHealth::Healthy);
        assert_eq!(watchdog.consecutive_failures(), 0);
    }
}
#[cfg(test)]
mod spawn_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn watchdog_spawn_exits_on_the_stop_token() {
        struct OkTransport;
        impl WatchdogTransport for OkTransport {
            fn probe(&mut self) -> Result<(), String> {
                Ok(())
            }
        }
        let mut task = spawn_watchdog(OkTransport, Duration::from_millis(1)).unwrap();
        assert_eq!(
            task.wait_for_stop(Duration::from_secs(1)).unwrap(),
            crate::thread_custody::ThreadState::Stopped
        );
    }
}
