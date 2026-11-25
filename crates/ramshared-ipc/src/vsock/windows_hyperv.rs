// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-ipc/src/vsock/windows_hyperv.rs
//! Native Hyper-V socket effects and kernel-observed endpoint scope.
mod stream;

use super::*;
use std::mem::MaybeUninit;
use std::sync::Arc;
use stream::HypervStream;
use windows_sys::Win32::Networking::WinSock::{
    AF_HYPERV, FIONBIO, INVALID_SOCKET, SOCK_STREAM, SOCKET, SOMAXCONN, WSACleanup, WSADATA,
    WSAEINTR, WSAEWOULDBLOCK, WSAGetLastError, WSAStartup, accept, bind, closesocket,
    connect as winsock_connect, getpeername, ioctlsocket, listen as winsock_listen, setsockopt,
    socket,
};
use windows_sys::Win32::System::Hypervisor::{
    HV_PROTOCOL_RAW, HVSOCKET_CONNECT_TIMEOUT, HVSOCKET_CONNECT_TIMEOUT_MAX, SOCKADDR_HV,
};
use windows_sys::core::GUID;

const _: () = assert!(std::mem::size_of::<SOCKADDR_HV>() == 36);

fn canonical_guid(guid: GUID) -> [u8; 16] {
    let mut bytes = [0; 16];
    bytes[..4].copy_from_slice(&guid.data1.to_be_bytes());
    bytes[4..6].copy_from_slice(&guid.data2.to_be_bytes());
    bytes[6..8].copy_from_slice(&guid.data3.to_be_bytes());
    bytes[8..].copy_from_slice(&guid.data4);
    bytes
}
fn observed_peer(socket: SOCKET) -> Result<VsockPeerIdentity, VsockError> {
    let mut address = MaybeUninit::<SOCKADDR_HV>::zeroed();
    let mut length = std::mem::size_of::<SOCKADDR_HV>() as i32;
    // SAFETY: owned connected socket, writable exact SDK output and actual length.
    if unsafe { getpeername(socket, address.as_mut_ptr().cast(), &mut length) } != 0 {
        // SAFETY: thread-local error immediately after this failed Winsock call.
        return Err(VsockError::IoError(format!(
            "native peer query failed with Winsock error {}",
            // SAFETY: thread-local error from the immediately preceding Winsock call.
            unsafe { WSAGetLastError() }
        )));
    }
    if length != std::mem::size_of::<SOCKADDR_HV>() as i32 {
        return Err(VsockError::IoError(
            "native peer address length mismatch".into(),
        ));
    }
    // SAFETY: successful call returned the exact all-bit-pattern-valid SDK layout.
    let address = unsafe { address.assume_init() };
    let peer = VsockPeerIdentity {
        vm_id: canonical_guid(address.VmId),
        service_id: canonical_guid(address.ServiceId),
    };
    if address.Family != AF_HYPERV || address.Reserved != 0 || peer.vm_id == [0; 16] {
        return Err(VsockError::IoError(
            "native peer family, flags or VM scope unavailable".into(),
        ));
    }
    Ok(peer)
}
fn connected_stream(
    socket: SOCKET,
    runtime: Arc<WinsockRuntime>,
    expected: Option<VsockPeerIdentity>,
) -> Result<Box<dyn VsockStreamTrait>, VsockError> {
    let peer = match observed_peer(socket) {
        Ok(peer) => peer,
        Err(err) => {
            // SAFETY: still uniquely owned; no stream accepted this handle.
            unsafe { closesocket(socket) };
            return Err(err);
        }
    };
    if expected.is_some_and(|expected| peer != expected) {
        // SAFETY: still uniquely owned; no stream accepted this handle.
        unsafe { closesocket(socket) };
        return Err(VsockError::IoError(
            "native peer differs from requested VM/service endpoint".into(),
        ));
    }
    HypervStream::new(socket, runtime, peer)
        .map(|stream| Box::new(stream) as Box<dyn VsockStreamTrait>)
}
struct WinsockRuntime;

impl WinsockRuntime {
    fn start() -> Result<Arc<Self>, VsockError> {
        let mut data = MaybeUninit::<WSADATA>::uninit();
        // SAFETY: exact writable SDK output; successful startup is owned by this runtime.
        let result = unsafe { WSAStartup(0x0202, data.as_mut_ptr()) };
        if result != 0 {
            return Err(VsockError::ListenFailed(format!(
                "WSAStartup failed with error {result}"
            )));
        }
        Ok(Arc::new(Self))
    }
}

impl Drop for WinsockRuntime {
    fn drop(&mut self) {
        // SAFETY: this owner balances its single successful WSAStartup call.
        let _ = unsafe { WSACleanup() };
    }
}

pub fn connect(
    vm_id: [u8; 16],
    guid: [u8; 16],
    timeout: Duration,
) -> Result<Box<dyn VsockStreamTrait>, VsockError> {
    let runtime = WinsockRuntime::start()?;
    // SAFETY: fixed native socket family/type/protocol; handle checked before use.
    let socket = unsafe { socket(AF_HYPERV as i32, SOCK_STREAM, HV_PROTOCOL_RAW as i32) };
    if socket == INVALID_SOCKET {
        return Err(VsockError::ConnectFailed(format!(
            "socket(AF_HYPERV) failed with Winsock error {}",
            // SAFETY: thread-local error from the immediately preceding Winsock call.
            unsafe { WSAGetLastError() }
        )));
    }
    let timeout_ms = timeout
        .as_millis()
        .clamp(1, u128::from(HVSOCKET_CONNECT_TIMEOUT_MAX)) as u32;
    let timeout_ptr = std::ptr::addr_of!(timeout_ms).cast::<u8>();
    // SAFETY: owned socket, live u32 option buffer and exact byte length.
    if unsafe {
        setsockopt(
            socket,
            HV_PROTOCOL_RAW as i32,
            HVSOCKET_CONNECT_TIMEOUT as i32,
            timeout_ptr,
            std::mem::size_of::<u32>() as i32,
        )
    } != 0
    {
        // SAFETY: thread-local error captured before cleanup changes it.
        let error = unsafe { WSAGetLastError() };
        // SAFETY: still uniquely owned; no stream accepted this handle.
        unsafe { closesocket(socket) };
        return Err(VsockError::ConnectFailed(format!(
            "HVSOCKET_CONNECT_TIMEOUT failed with Winsock error {error}"
        )));
    }
    let (v1, v2, v3, v4) = guid_components(vm_id);
    let (s1, s2, s3, s4) = guid_components(guid);
    let address = SOCKADDR_HV {
        Family: AF_HYPERV,
        Reserved: 0,
        VmId: GUID {
            data1: v1,
            data2: v2,
            data3: v3,
            data4: v4,
        },
        ServiceId: GUID {
            data1: s1,
            data2: s2,
            data3: s3,
            data4: s4,
        },
    };
    // SAFETY: owned socket and borrowed exact SDK address for this synchronous call.
    let connected = unsafe {
        winsock_connect(
            socket,
            (&address as *const SOCKADDR_HV).cast(),
            std::mem::size_of::<SOCKADDR_HV>() as i32,
        )
    };
    if connected != 0 {
        // SAFETY: thread-local error captured before cleanup changes it.
        let error = unsafe { WSAGetLastError() };
        // SAFETY: still uniquely owned; no stream accepted this handle.
        unsafe { closesocket(socket) };
        return Err(VsockError::ConnectFailed(format!(
            "connect(AF_HYPERV) failed with Winsock error {error}"
        )));
    }
    connected_stream(
        socket,
        runtime,
        Some(VsockPeerIdentity {
            vm_id,
            service_id: guid,
        }),
    )
}

pub struct Listener {
    socket: SOCKET,
    runtime: Arc<WinsockRuntime>,
}

impl Drop for Listener {
    fn drop(&mut self) {
        // SAFETY: the listener owns this handle and never transfers it.
        let _ = unsafe { closesocket(self.socket) };
    }
}

pub fn listen(guid: [u8; 16], port: u32) -> Result<VsockListener, VsockError> {
    let runtime = WinsockRuntime::start()?;
    // SAFETY: fixed native socket family/type/protocol; handle checked before use.
    let socket = unsafe { socket(AF_HYPERV as i32, SOCK_STREAM, HV_PROTOCOL_RAW as i32) };
    if socket == INVALID_SOCKET {
        return Err(VsockError::ListenFailed(format!(
            "socket(AF_HYPERV) failed with Winsock error {}",
            // SAFETY: thread-local error from the immediately preceding Winsock call.
            unsafe { WSAGetLastError() }
        )));
    }

    let (data1, data2, data3, data4) = guid_components(guid);
    let address = SOCKADDR_HV {
        Family: AF_HYPERV,
        Reserved: 0,
        VmId: GUID::default(),
        ServiceId: GUID {
            data1,
            data2,
            data3,
            data4,
        },
    };
    // SAFETY: owned socket and borrowed exact SDK bind address.
    let bound = unsafe {
        bind(
            socket,
            (&address as *const SOCKADDR_HV).cast(),
            std::mem::size_of::<SOCKADDR_HV>() as i32,
        )
    };
    if bound != 0 {
        // SAFETY: thread-local error captured before cleanup changes it.
        let error = unsafe { WSAGetLastError() };
        // SAFETY: still uniquely owned; no stream accepted this handle.
        unsafe { closesocket(socket) };
        return Err(VsockError::ListenFailed(format!(
            "bind(AF_HYPERV) failed with Winsock error {error}"
        )));
    }
    // SAFETY: this socket is successfully bound and remains owned here.
    if unsafe { winsock_listen(socket, SOMAXCONN as i32) } != 0 {
        // SAFETY: thread-local error captured before cleanup changes it.
        let error = unsafe { WSAGetLastError() };
        // SAFETY: still uniquely owned; no stream accepted this handle.
        unsafe { closesocket(socket) };
        return Err(VsockError::ListenFailed(format!(
            "listen(AF_HYPERV) failed with Winsock error {error}"
        )));
    }

    let mut nonblocking = 1_u32;
    // SAFETY: owned socket and writable exact SDK nonblocking-mode argument.
    if unsafe { ioctlsocket(socket, FIONBIO, &mut nonblocking) } != 0 {
        // SAFETY: thread-local error captured before cleanup changes it.
        let error = unsafe { WSAGetLastError() };
        // SAFETY: still uniquely owned; no stream accepted this handle.
        unsafe { closesocket(socket) };
        return Err(VsockError::ListenFailed(format!(
            "nonblocking AF_HYPERV setup failed with Winsock error {error}"
        )));
    }

    Ok(VsockListener {
        endpoint: VsockEndpoint { cid: 0, port, guid },
        inner: Listener { socket, runtime },
    })
}

impl Listener {
    pub fn accept(&self, timeout: Duration) -> Result<Box<dyn VsockStreamTrait>, VsockError> {
        let runtime = Arc::clone(&self.runtime);
        accept_until(timeout, || {
            // SAFETY: live nonblocking listener; no optional address output requested.
            let client = unsafe { accept(self.socket, std::ptr::null_mut(), std::ptr::null_mut()) };
            if client == INVALID_SOCKET {
                // SAFETY: thread-local error captured before cleanup changes it.
                let error = unsafe { WSAGetLastError() };
                return if error == WSAEWOULDBLOCK || error == WSAEINTR {
                    Ok(None)
                } else {
                    Err(VsockError::AcceptFailed(format!(
                        "accept(AF_HYPERV) failed with Winsock error {error}"
                    )))
                };
            }

            connected_stream(client, Arc::clone(&runtime), None).map(Some)
        })
    }
}
