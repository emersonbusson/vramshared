// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-ipc/tests/native_peer.rs
//! Forwarding observed native endpoints must not invent scope on ordinary streams.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use ramshared_ipc::vsock::{VsockPeerIdentity, VsockStreamTrait};
use std::io::{self, Read, Write};
use std::time::Duration;
struct Scoped;
impl Read for Scoped {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Ok(0)
    }
}
impl Write for Scoped {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl VsockStreamTrait for Scoped {
    fn set_read_timeout(&self, _: Option<Duration>) -> io::Result<()> {
        Ok(())
    }
    fn set_write_timeout(&self, _: Option<Duration>) -> io::Result<()> {
        Ok(())
    }
    fn shutdown_both(&self) -> io::Result<()> {
        Ok(())
    }
    fn native_peer(&self) -> io::Result<Option<VsockPeerIdentity>> {
        Ok(Some(VsockPeerIdentity {
            vm_id: [1; 16],
            service_id: [2; 16],
        }))
    }
}
#[test]
fn boxed_stream_preserves_native_endpoint_observation() {
    let stream: Box<dyn VsockStreamTrait> = Box::new(Scoped);
    assert_eq!(
        stream.native_peer().unwrap(),
        Some(VsockPeerIdentity {
            vm_id: [1; 16],
            service_id: [2; 16]
        })
    );
}

#[test]
fn ordinary_tcp_stream_does_not_invent_native_vm_scope() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    assert_eq!(client.native_peer().unwrap(), None);
    assert_eq!(server.native_peer().unwrap(), None);
}

#[cfg(windows)]
#[test]
#[ignore = "read-only: connected current WSL guest listener and exact native VM ID"]
fn native_kernel_peer_matches_current_guest_endpoint() {
    use ramshared_ipc::vsock::{connect_hyperv, linux_service_guid};
    let vm = std::env::var("RAMSHARED_NATIVE_EXPECTED_VM_ID").unwrap();
    let canonical = vm.trim().replace('-', "");
    let vm_id = u128::from_str_radix(&canonical, 16).unwrap().to_be_bytes();
    assert_ne!(vm_id, [0; 16]);
    let port: u32 = std::env::var("RAMSHARED_NATIVE_EXPECTED_PORT")
        .unwrap()
        .parse()
        .unwrap();
    assert!(port > 0 && port <= i32::MAX as u32);
    let expected = VsockPeerIdentity {
        vm_id,
        service_id: linux_service_guid(port),
    };
    let mut stream = connect_hyperv(vm_id, expected.service_id, Duration::from_secs(2)).unwrap();
    assert_eq!(stream.native_peer().unwrap(), Some(expected));
    stream
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let mut idle = [0; 1];
    for _ in 0..3 {
        assert!(matches!(
            stream.read(&mut idle),
            Err(error) if error.kind() == io::ErrorKind::TimedOut
        ));
    }
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream.write_all(b"PING").unwrap();
    let mut response = [0; 4];
    stream.read_exact(&mut response).unwrap();
    assert_eq!(&response, b"PONG");
    stream.shutdown_both().unwrap();
    eprintln!("NATIVE_KERNEL_PEER_OK observed_vm=true observed_service=true roundtrip=true");
}
