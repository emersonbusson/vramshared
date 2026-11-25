// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/tests/systemd_notify.rs
// Integration coverage for the verified systemd notification client.
#![allow(unsafe_code)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::os::unix::net::UnixDatagram;
use std::time::Duration;

use ramshared_wsl2d::systemd_notify::{NotifyError, NotifySocket};

fn temp_socket(name: &str) -> (std::path::PathBuf, UnixDatagram) {
    let path = std::env::temp_dir().join(format!(
        "ramshared-notify-{}-{name}.sock",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let receiver = UnixDatagram::bind(&path).expect("bind notify socket");
    receiver
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    (path, receiver)
}

#[test]
fn systemd_notify_validates_socket_and_strips_env_for_children() {
    let (path, receiver) = temp_socket("env");
    // SAFETY: this integration test is the only test in this binary and it
    // mutates the process environment before spawning any thread or child.
    unsafe { std::env::set_var("NOTIFY_SOCKET", &path) };
    let client = NotifySocket::from_env_with_owner(false)
        .expect("environment read")
        .expect("socket present");
    assert!(std::env::var_os("NOTIFY_SOCKET").is_none());
    assert_eq!(client.path(), path);

    client.send_ready().expect("ready");
    client.send_watchdog().expect("watchdog");
    client.send_stopping().expect("stopping");

    let mut buffer = [0u8; 32];
    let first = receiver.recv(&mut buffer).expect("ready datagram");
    assert_eq!(&buffer[..first], b"READY=1");
    let second = receiver.recv(&mut buffer).expect("watchdog datagram");
    assert_eq!(&buffer[..second], b"WATCHDOG=1");
    let third = receiver.recv(&mut buffer).expect("stopping datagram");
    assert_eq!(&buffer[..third], b"STOPPING=1");

    if unsafe { libc::geteuid() } != 0 {
        assert!(matches!(
            NotifySocket::from_path(&path, true),
            Err(NotifyError::NonRootOwner)
        ));
    }
    assert!(matches!(
        NotifySocket::from_path(std::path::Path::new("relative.sock"), false),
        Err(NotifyError::RelativePath)
    ));
    let _ = std::fs::remove_file(&path);
}
