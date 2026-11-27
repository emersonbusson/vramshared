// SPDX-License-Identifier: MIT
// Copyright (c) 2024-2026 Emerson Busson. All rights reserved.
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/monitor/tui/update_wake.rs
//! One nonblocking local scheduler notification on installed interactive launch.

use std::io::IsTerminal;
use std::path::Path;

use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType, connect, socket_with};

use crate::installed_image::ProcessInstallOrigin;
use crate::release_coherence::PREVIOUS_RELEASE_HINT_ENV;

use crate::customer::install_transaction::SOCKET_PATH;

pub(super) fn notify(origin: ProcessInstallOrigin, carried_state: bool) {
    let resumed = carried_state || std::env::var_os(PREVIOUS_RELEASE_HINT_ENV).is_some();
    let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if permitted(origin, tty, resumed) {
        let _ = connect_once(Path::new(SOCKET_PATH));
    }
}

fn permitted(origin: ProcessInstallOrigin, interactive: bool, resumed: bool) -> bool {
    interactive
        && !resumed
        && matches!(
            origin,
            ProcessInstallOrigin::ActiveRelease | ProcessInstallOrigin::Both
        )
}

fn connect_once(path: &Path) -> rustix::io::Result<()> {
    let address = SocketAddrUnix::new(path)?;
    let socket = socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
        None,
    )?;
    connect(&socket, &address)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn wake_is_only_sent_for_the_first_interactive_installed_process() {
        assert!(permitted(ProcessInstallOrigin::ActiveRelease, true, false));
        assert!(permitted(ProcessInstallOrigin::Both, true, false));
        assert!(!permitted(ProcessInstallOrigin::Direct, true, false));
        assert!(!permitted(ProcessInstallOrigin::Unknown, true, false));
        assert!(!permitted(
            ProcessInstallOrigin::ActiveRelease,
            false,
            false
        ));
        assert!(!permitted(ProcessInstallOrigin::ActiveRelease, true, true));
    }

    #[test]
    fn wake_connects_without_sending_content_and_tolerates_missing_endpoint() {
        use std::io::Read;
        use std::os::unix::net::UnixListener;

        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("wake.sock");
        assert!(connect_once(&socket).is_err());
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        connect_once(&socket).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        assert_eq!(peer.read(&mut [0u8]).unwrap(), 0);
    }
}
