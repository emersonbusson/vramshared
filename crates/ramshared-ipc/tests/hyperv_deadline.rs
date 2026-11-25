// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-ipc/tests/hyperv_deadline.rs
//! Native option probe only; no connection, registration, guest or storage change.
#![allow(clippy::unwrap_used)]
#[cfg(windows)]
#[test]
#[ignore = "read-only: requires native Windows Hyper-V socket provider"]
fn native_hyperv_socket_supports_required_timeout_options() {
    use std::os::windows::io::{FromRawSocket, RawSocket};
    use std::time::Duration;
    use windows_sys::Win32::Networking::WinSock::{
        AF_HYPERV, INVALID_SOCKET, SOCK_STREAM, WSACleanup, WSADATA, WSAStartup, socket,
    };
    // SDK protocol discriminant; the generated binding omits HV_PROTOCOL_RAW.
    const HV_PROTOCOL_RAW: i32 = 1;
    const _: () = assert!(std::mem::size_of::<WSADATA>() == 408);
    let mut data = WSADATA::default();
    // SAFETY: exact SDK output lives through initialization; own one reference.
    assert_eq!(unsafe { WSAStartup(0x0202, &mut data) }, 0);
    struct Runtime;
    impl Drop for Runtime {
        fn drop(&mut self) {
            // SAFETY: one successful WSAStartup above.
            unsafe {
                WSACleanup();
            }
        }
    }
    let _runtime = Runtime;
    // SAFETY: documented Hyper-V family/type/protocol, no pointer arguments.
    let handle = unsafe { socket(i32::from(AF_HYPERV), SOCK_STREAM, HV_PROTOCOL_RAW) };
    assert_ne!(
        handle, INVALID_SOCKET,
        "Hyper-V socket provider is unavailable"
    );
    // SAFETY: uniquely own this newly created socket; standard socket operations
    // use Winsock regardless of address family. Drop closes it before cleanup.
    let stream = unsafe { std::net::TcpStream::from_raw_socket(handle as RawSocket) };
    stream
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    eprintln!("NATIVE_HYPERV_TIMEOUT_OPTIONS_OK connected=false");
}
