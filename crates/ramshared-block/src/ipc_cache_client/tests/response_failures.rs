// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/ipc_cache_client/tests/response_failures.rs

use super::*;
use crate::{AllowCacheAdmission, AuthoritativeOriginBackend, FileOrigin};

#[test]
fn response_diagnostics_distinguish_deadline_disconnect_and_other_errors() {
    use super::super::operations::read_response_failure;

    for (kind, diagnostic) in [
        (ErrorKind::TimedOut, "timed out"),
        (ErrorKind::WouldBlock, "timed out"),
        (ErrorKind::UnexpectedEof, "peer disconnected"),
        (ErrorKind::ConnectionReset, "peer disconnected"),
        (ErrorKind::ConnectionAborted, "peer disconnected"),
        (ErrorKind::BrokenPipe, "peer disconnected"),
        (ErrorKind::InvalidInput, "I/O failed"),
    ] {
        assert_eq!(read_response_failure(kind), diagnostic);
    }
}

#[test]
fn closed_read_response_header_falls_back_to_origin() {
    assert_closed_read_uses_origin(false);
}

#[test]
fn closed_read_response_payload_falls_back_to_origin() {
    assert_closed_read_uses_origin(true);
}

fn assert_closed_read_uses_origin(send_header: bool) {
    let (client_socket, mut worker_socket) = UnixStream::pair().unwrap();
    worker_socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let worker = std::thread::spawn(move || {
        let mut bytes = [0; FRAME_HEADER_LEN];
        worker_socket.read_exact(&mut bytes).unwrap();
        let request = FrameHeader::decode(&bytes).unwrap();
        assert_eq!(request.msg_type, MSG_READ_REQ);
        if send_header {
            let response = FrameHeader {
                msg_type: MSG_READ_RESP,
                status: STATUS_OK,
                correlation_id: request.correlation_id,
                offset: request.offset,
                payload_len: request.aux,
                aux: 1,
            };
            worker_socket.write_all(&response.encode()).unwrap();
            worker_socket.write_all(&[0x99; 32]).unwrap();
        } else {
            worker_socket.write_all(&[0; FRAME_HEADER_LEN / 2]).unwrap();
        }
        worker_socket.shutdown(std::net::Shutdown::Write).unwrap();
    });

    let expected = [0x42; 4096];
    let path = std::env::temp_dir().join(format!(
        "ramshared-cache-response-{}-{send_header}",
        std::process::id(),
    ));
    let mut origin = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    origin.write_all(&expected).unwrap();
    origin.sync_data().unwrap();
    let backend = AuthoritativeOriginBackend::new_with_admission(
        FileOrigin::from_file(origin),
        IpcCacheClient::new(client_socket, Duration::from_secs(2), 4096),
        4096,
        4096,
        AllowCacheAdmission,
    )
    .unwrap();

    let mut destination = [0; 4096];
    backend.read_at(0, &mut destination).unwrap();
    worker.join().unwrap();
    assert_eq!(destination, expected);
    assert_eq!(backend.cache_state(), CacheState::Unavailable);
    assert_eq!(backend.telemetry().cache_read_failures, 1);
    drop(backend);
    std::fs::remove_file(path).unwrap();
}
