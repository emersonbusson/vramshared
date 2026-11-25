// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/nbd_netlink/tests.rs
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;
use std::collections::VecDeque;
use std::io::Cursor;

pub(in crate::cascade::cascade_io) struct FakeNetlinkTransport {
    family_id: Result<u16, CascadeError>,
    replies: VecDeque<Result<Vec<u8>, CascadeError>>,
    sent: Vec<Vec<u8>>,
}

impl FakeNetlinkTransport {
    pub(in crate::cascade::cascade_io) fn with_replies(
        family_id: u16,
        replies: Vec<Vec<u8>>,
    ) -> Self {
        Self {
            family_id: Ok(family_id),
            replies: replies.into_iter().map(Ok).collect(),
            sent: Vec::new(),
        }
    }
}

impl NbdNetlinkTransport for FakeNetlinkTransport {
    fn resolve_family_id(&mut self) -> Result<u16, CascadeError> {
        self.family_id
            .as_ref()
            .copied()
            .map_err(|e| CascadeError::Precondition(e.to_string()))
    }

    fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>, CascadeError> {
        self.sent.push(request.to_vec());
        self.replies
            .pop_front()
            .unwrap_or_else(|| Err(CascadeError::Precondition("no fake netlink reply".into())))
    }
}

pub(in crate::cascade::cascade_io) fn finish_nbd_status_reply(
    family_id: u16,
    seq: u32,
    index: u32,
    connected: u8,
) -> Vec<u8> {
    let mut msg = NlMsgBuilder::new(family_id, 0, seq, NBD_CMD_STATUS, NBD_GENL_VERSION);
    msg.push_nested(NBD_ATTR_DEVICE_LIST, |list| {
        list.push_nested(NBD_DEVICE_ITEM, |item| {
            item.push_attr(NBD_DEVICE_INDEX, &index.to_ne_bytes())?;
            item.push_attr(NBD_DEVICE_CONNECTED, &[connected])
        })
    })
    .unwrap();
    msg.finish().unwrap()
}

pub(in crate::cascade::cascade_io) fn run_fake_nbd_handshake_server(
    sock_path: &Path,
    export_size: u64,
) -> std::thread::JoinHandle<()> {
    let listener = std::os::unix::net::UnixListener::bind(sock_path).unwrap();
    std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let mut init = Vec::new();
        init.extend_from_slice(NBD_MAGIC_BANNER);
        init.extend_from_slice(&NBD_OPT_MAGIC.to_be_bytes());
        init.extend_from_slice(&(NBD_FLAG_FIXED_NEWSTYLE | NBD_FLAG_NO_ZEROES).to_be_bytes());
        conn.write_all(&init).unwrap();
        let mut req = [0u8; 4 + 8 + 4 + 4 + PRODUCTION_EXPORT_NAME.len()];
        let _ = conn.read_exact(&mut req);
        let mut reply = Vec::new();
        reply.extend_from_slice(&export_size.to_be_bytes());
        reply.extend_from_slice(&(NBD_FLAG_HAS_FLAGS | NBD_FLAG_CAN_MULTI_CONN).to_be_bytes());
        let _ = conn.write_all(&reply);
    })
}

fn finish_genl_reply(msg_type: u16, seq: u32, cmd: u8, attrs: &[u8]) -> Vec<u8> {
    let mut msg = NlMsgBuilder::new(msg_type, 0, seq, cmd, NBD_GENL_VERSION);
    msg.buf.extend_from_slice(attrs);
    msg.finish().unwrap()
}

pub(in crate::cascade::cascade_io) fn netlink_error_msg(seq: u32, errno: i32) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&20u32.to_ne_bytes());
    buf.extend_from_slice(&NLMSG_ERROR.to_ne_bytes());
    buf.extend_from_slice(&0u16.to_ne_bytes());
    buf.extend_from_slice(&seq.to_ne_bytes());
    buf.extend_from_slice(&0u32.to_ne_bytes());
    buf.extend_from_slice(&errno.to_ne_bytes());
    buf
}

fn fake_handshake_bytes(
    magic: &[u8; 8],
    handshake_flags: u16,
    export_size: u64,
    export_flags: u16,
) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(magic);
    bytes.extend_from_slice(&NBD_OPT_MAGIC.to_be_bytes());
    bytes.extend_from_slice(&handshake_flags.to_be_bytes());
    bytes.extend_from_slice(&export_size.to_be_bytes());
    bytes.extend_from_slice(&export_flags.to_be_bytes());
    if handshake_flags & NBD_FLAG_NO_ZEROES == 0 {
        bytes.extend_from_slice(&[0u8; 124]);
    }
    bytes
}

struct ReadWritePair {
    reader: Cursor<Vec<u8>>,
    written: Vec<u8>,
}

impl Read for ReadWritePair {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.reader.read(buf)
    }
}

impl Write for ReadWritePair {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.written.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn get_family_and_status_wire_roundtrip_and_refusals() {
    let req = build_get_family_request(11).unwrap();
    assert!(req.len() >= NLMSG_HDRLEN + GENL_HDRLEN);
    assert_eq!(read_u16_ne(&req, 6), Some(NLM_F_REQUEST));
    let status_req = build_nbd_status_request(42, 7, 0).unwrap();
    assert_eq!(read_u16_ne(&status_req, 6), Some(NLM_F_REQUEST));

    let mut attrs = NlMsgBuilder::new(GENL_ID_CTRL, 0, 11, CTRL_CMD_GETFAMILY, 1);
    attrs
        .push_attr(CTRL_ATTR_FAMILY_ID, &42u16.to_ne_bytes())
        .unwrap();
    let family_reply = attrs.finish().unwrap();
    assert_eq!(parse_get_family_id(&family_reply, 11).unwrap(), 42);
    assert!(parse_get_family_id(&netlink_error_msg(11, -libc::ENOENT), 11).is_err());
    assert!(parse_get_family_id(&netlink_error_msg(99, -libc::ENOENT), 11).is_err());

    let mut status_msg = NlMsgBuilder::new(42, 0, 7, NBD_CMD_STATUS, NBD_GENL_VERSION);
    status_msg
        .push_nested(NBD_ATTR_DEVICE_LIST, |list| {
            list.push_nested(NBD_DEVICE_ITEM, |item| {
                item.push_attr(NBD_DEVICE_INDEX, &0u32.to_ne_bytes())?;
                item.push_attr(NBD_DEVICE_CONNECTED, &[1u8])
            })
        })
        .unwrap();
    let status_reply = status_msg.finish().unwrap();
    let mut fake = FakeNetlinkTransport::with_replies(42, vec![status_reply]);
    let status = query_device_status_with(&mut fake, 7, "/dev/nbd0").unwrap();
    assert_eq!(
        status,
        NbdDeviceStatus {
            index: 0,
            connected: true
        }
    );
    assert!(parse_nbd_status_response(&netlink_error_msg(7, -libc::ENODEV), 7, 0).is_err());
    assert_eq!(
        parse_nbd_status_response(&finish_genl_reply(42, 7, NBD_CMD_STATUS, &[]), 7, 0).unwrap(),
        None
    );
}

#[test]
fn reconfigure_request_builds_nested_sockets_and_validates_ack() {
    let req = build_nbd_reconfigure_request(
        33,
        5,
        &NbdReconfigureRequest {
            index: 0,
            timeout_secs: Some(30),
            dead_conn_timeout_secs: Some(15),
            backend_identifier: Some("ramshared-origin"),
            socket_fds: &[9],
        },
    )
    .unwrap();
    let msgs = parse_nlmsgs(&req).unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].msg_type, 33);
    assert_eq!(msgs[0].seq, 5);
    let attrs = parse_attrs(&msgs[0].payload[GENL_HDRLEN..]).unwrap();
    assert!(attrs.iter().any(|(k, _)| *k == NBD_ATTR_DEAD_CONN_TIMEOUT));
    assert!(attrs.iter().any(|(k, _)| *k == NBD_ATTR_BACKEND_IDENTIFIER));
    assert!(attrs.iter().any(|(k, _)| *k == NBD_ATTR_SOCKETS));

    assert!(parse_netlink_ack(&netlink_error_msg(5, 0), 5).is_ok());
    assert!(parse_netlink_ack(&netlink_error_msg(5, -libc::EINVAL), 5).is_err());
    assert!(parse_netlink_ack(&[], 5).is_err());

    let mut fake = FakeNetlinkTransport::with_replies(33, vec![netlink_error_msg(5, 0)]);
    arm_dead_conn_timeout_with(&mut fake, 5, "/dev/nbd0", 15).unwrap();
    assert_eq!(fake.sent.len(), 1);
    assert!(arm_dead_conn_timeout_with(&mut fake, 5, "/dev/nbd0", 0).is_err());
}

#[test]
fn device_index_and_nbd_handshake_validation_rejects_bad_peers() {
    assert_eq!(nbd_device_index("/dev/nbd0").unwrap(), 0);
    assert_eq!(nbd_device_index("/dev/nbd12").unwrap(), 12);
    for bad in ["/dev/zram0", "/dev/nbd", "/dev/nbd0p1", "nbd0"] {
        assert!(nbd_device_index(bad).is_err(), "{bad}");
    }
    #[cfg(unix)]
    {
        let root = std::env::temp_dir().join(format!("nbd-idx-symlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let ok_link = root.join("fd3");
        std::os::unix::fs::symlink("/dev/nbd4", &ok_link).unwrap();
        assert_eq!(nbd_device_index(ok_link.to_str().unwrap()).unwrap(), 4);
        let bad_link = root.join("fd4");
        std::os::unix::fs::symlink("/dev/zram0", &bad_link).unwrap();
        assert!(nbd_device_index(bad_link.to_str().unwrap()).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    let ok_flags = NBD_FLAG_HAS_FLAGS | NBD_FLAG_CAN_MULTI_CONN;
    let mut pair = ReadWritePair {
        reader: Cursor::new(fake_handshake_bytes(
            NBD_MAGIC_BANNER,
            NBD_FLAG_FIXED_NEWSTYLE | NBD_FLAG_NO_ZEROES,
            64 * 1024 * 1024,
            ok_flags,
        )),
        written: Vec::new(),
    };
    let export = handshake_nbd_stream(&mut pair, PRODUCTION_EXPORT_NAME).unwrap();
    assert_eq!(export.size_bytes, 64 * 1024 * 1024);
    assert!(pair.written.ends_with(PRODUCTION_EXPORT_NAME.as_bytes()));

    let mut with_zeroes = ReadWritePair {
        reader: Cursor::new(fake_handshake_bytes(
            NBD_MAGIC_BANNER,
            NBD_FLAG_FIXED_NEWSTYLE,
            8192,
            ok_flags,
        )),
        written: Vec::new(),
    };
    assert_eq!(
        handshake_nbd_stream(&mut with_zeroes, PRODUCTION_EXPORT_NAME)
            .unwrap()
            .size_bytes,
        8192
    );

    for bad_stream in [
        fake_handshake_bytes(
            b"BADMAGIC",
            NBD_FLAG_FIXED_NEWSTYLE | NBD_FLAG_NO_ZEROES,
            4096,
            ok_flags,
        ),
        fake_handshake_bytes(NBD_MAGIC_BANNER, NBD_FLAG_NO_ZEROES, 4096, ok_flags),
        fake_handshake_bytes(
            NBD_MAGIC_BANNER,
            NBD_FLAG_FIXED_NEWSTYLE | NBD_FLAG_NO_ZEROES,
            4097,
            ok_flags,
        ),
        fake_handshake_bytes(
            NBD_MAGIC_BANNER,
            NBD_FLAG_FIXED_NEWSTYLE | NBD_FLAG_NO_ZEROES,
            4096,
            NBD_FLAG_HAS_FLAGS,
        ),
    ] {
        let mut bad_pair = ReadWritePair {
            reader: Cursor::new(bad_stream),
            written: Vec::new(),
        };
        assert!(handshake_nbd_stream(&mut bad_pair, PRODUCTION_EXPORT_NAME).is_err());
    }
}

#[test]
fn replace_peer_socket_over_unix_pair_validates_size_and_attaches_fd() {
    use std::os::unix::net::UnixListener;

    let dir = std::env::temp_dir().join(format!("ramshared-nbd-nl-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let sock_path = dir.join("peer.sock");
    let listener = UnixListener::bind(&sock_path).unwrap();

    let server = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let mut init = Vec::new();
        init.extend_from_slice(NBD_MAGIC_BANNER);
        init.extend_from_slice(&NBD_OPT_MAGIC.to_be_bytes());
        init.extend_from_slice(&(NBD_FLAG_FIXED_NEWSTYLE | NBD_FLAG_NO_ZEROES).to_be_bytes());
        conn.write_all(&init).unwrap();
        let mut req = [0u8; 4 + 8 + 4 + 4 + PRODUCTION_EXPORT_NAME.len()];
        conn.read_exact(&mut req).unwrap();
        let mut reply = Vec::new();
        reply.extend_from_slice(&(16u64 * 1024 * 1024).to_be_bytes());
        reply.extend_from_slice(&(NBD_FLAG_HAS_FLAGS | NBD_FLAG_CAN_MULTI_CONN).to_be_bytes());
        conn.write_all(&reply).unwrap();
    });

    let mut fake = FakeNetlinkTransport::with_replies(44, vec![netlink_error_msg(9, 0)]);
    let export = replace_peer_socket_with(
        &mut fake,
        9,
        "/dev/nbd0",
        &NbdPeerAttachOptions {
            socket_path: &sock_path,
            export_name: PRODUCTION_EXPORT_NAME,
            expected_size_bytes: Some(16 * 1024 * 1024),
            dead_conn_timeout_secs: Some(15),
            backend_identifier: Some("origin-id"),
        },
    )
    .unwrap();
    assert_eq!(export.size_bytes, 16 * 1024 * 1024);
    assert_eq!(fake.sent.len(), 1);
    server.join().unwrap();

    let server_mismatch = {
        let listener2 = UnixListener::bind(dir.join("peer2.sock")).unwrap();
        let handle = std::thread::spawn(move || {
            let (mut conn, _) = listener2.accept().unwrap();
            let mut init = Vec::new();
            init.extend_from_slice(NBD_MAGIC_BANNER);
            init.extend_from_slice(&NBD_OPT_MAGIC.to_be_bytes());
            init.extend_from_slice(&(NBD_FLAG_FIXED_NEWSTYLE | NBD_FLAG_NO_ZEROES).to_be_bytes());
            conn.write_all(&init).unwrap();
            let mut req = [0u8; 4 + 8 + 4 + 4 + PRODUCTION_EXPORT_NAME.len()];
            let _ = conn.read_exact(&mut req);
            let mut reply = Vec::new();
            reply.extend_from_slice(&(8u64 * 1024 * 1024).to_be_bytes());
            reply.extend_from_slice(&(NBD_FLAG_HAS_FLAGS | NBD_FLAG_CAN_MULTI_CONN).to_be_bytes());
            let _ = conn.write_all(&reply);
        });
        let mut fake2 = FakeNetlinkTransport::with_replies(44, vec![netlink_error_msg(10, 0)]);
        let err = replace_peer_socket_with(
            &mut fake2,
            10,
            "/dev/nbd0",
            &NbdPeerAttachOptions {
                socket_path: &dir.join("peer2.sock"),
                export_name: PRODUCTION_EXPORT_NAME,
                expected_size_bytes: Some(16 * 1024 * 1024),
                dead_conn_timeout_secs: None,
                backend_identifier: None,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("disagrees with expected"));
        assert!(fake2.sent.is_empty());
        handle
    };
    server_mismatch.join().unwrap();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn wire_error_and_kernel_family_probe_paths() {
    let status_reply = finish_nbd_status_reply(42, 7, 3, 0);
    let mut fake_missing = FakeNetlinkTransport::with_replies(42, vec![status_reply]);
    assert!(query_device_status_with(&mut fake_missing, 7, "/dev/nbd0").is_err());

    assert!(parse_nlmsgs(&[4, 0, 0, 0]).is_ok());
    let mut bad_attr = vec![0u8; NLMSG_HDRLEN + GENL_HDRLEN];
    let bad_attr_len = (bad_attr.len() + 4) as u32;
    bad_attr[0..4].copy_from_slice(&bad_attr_len.to_ne_bytes());
    bad_attr.extend_from_slice(&2u16.to_ne_bytes());
    bad_attr.extend_from_slice(&1u16.to_ne_bytes());
    assert!(parse_attrs(&bad_attr[NLMSG_HDRLEN + GENL_HDRLEN..]).is_err());

    if let Ok(mut real) = KernelNbdNetlink::open() {
        let _ = real.resolve_family_id();
    }
}
