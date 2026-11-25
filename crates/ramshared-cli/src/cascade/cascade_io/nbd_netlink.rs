// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/nbd_netlink.rs
//! Generic Netlink `nbd` client and live peer-replacement orchestration (`GAP-REGISTER` #85).
//!
//! Uses safe `rustix::net` netlink sockets (`#![deny(unsafe_code)]`) so the product:
//! 1. Arms a finite `NBD_ATTR_DEAD_CONN_TIMEOUT` (`15 s`, never infinite `-persist`) on
//!    production NBD attaches, giving the kernel a bounded grace window instead of raising
//!    immediate `-EIO`/`SIGBUS` when a serving `ramsharedd` peer dies unexpectedly.
//! 2. Queries live kernel NBD status (`NBD_CMD_STATUS`).
//! 3. Replaces a dead `ramsharedd` peer on a still-active `/dev/nbdX` via `NBD_CMD_RECONFIGURE`
//!    after completing an authenticated, size-verified NBD newstyle handshake over Unix socket.

#[allow(unused_imports)]
use super::*;
use std::os::unix::net::UnixStream;

pub(in crate::cascade) const DEFAULT_DEAD_CONN_TIMEOUT_SECS: u64 = 15;
const NETLINK_IO_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_NETLINK_REPLY_BYTES: usize = 16 * 1024;
const NLA_F_NESTED: u16 = 1 << 15;
const NLA_HDRLEN: usize = 4;
const NLMSG_HDRLEN: usize = 16;
const GENL_HDRLEN: usize = 4;

const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLM_F_REQUEST: u16 = 1;
const NLM_F_ACK: u16 = 4;

const GENL_ID_CTRL: u16 = 0x10;
const CTRL_CMD_GETFAMILY: u8 = 3;
const CTRL_ATTR_FAMILY_ID: u16 = 1;
const CTRL_ATTR_FAMILY_NAME: u16 = 2;

const NBD_GENL_FAMILY_NAME: &[u8] = b"nbd\0";
const NBD_GENL_VERSION: u8 = 1;
const NBD_CMD_RECONFIGURE: u8 = 3;
const NBD_CMD_STATUS: u8 = 5;

const NBD_ATTR_INDEX: u16 = 1;
const NBD_ATTR_TIMEOUT: u16 = 4;
const NBD_ATTR_SOCKETS: u16 = 7;
const NBD_ATTR_DEAD_CONN_TIMEOUT: u16 = 8;
const NBD_ATTR_DEVICE_LIST: u16 = 9;
const NBD_ATTR_BACKEND_IDENTIFIER: u16 = 10;

const NBD_DEVICE_ITEM: u16 = 1;
const NBD_DEVICE_INDEX: u16 = 1;
const NBD_DEVICE_CONNECTED: u16 = 2;

const NBD_SOCK_ITEM: u16 = 1;
const NBD_SOCK_FD: u16 = 1;

const NBD_MAGIC_BANNER: &[u8; 8] = b"NBDMAGIC";
const NBD_OPT_MAGIC: u64 = 0x4948_4156_454F_5054; // "IHAVEOPT"
const NBD_FLAG_FIXED_NEWSTYLE: u16 = 1 << 0;
const NBD_FLAG_NO_ZEROES: u16 = 1 << 1;
const NBD_FLAG_C_FIXED_NEWSTYLE: u32 = 1 << 0;
const NBD_FLAG_C_NO_ZEROES: u32 = 1 << 1;
const NBD_OPT_EXPORT_NAME: u32 = 1;
const NBD_FLAG_HAS_FLAGS: u16 = 1 << 0;
const NBD_FLAG_CAN_MULTI_CONN: u16 = 1 << 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::cascade) struct NbdDeviceStatus {
    pub(in crate::cascade) index: u32,
    pub(in crate::cascade) connected: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::cascade) struct NbdHandshakeExport {
    pub(in crate::cascade) size_bytes: u64,
    pub(in crate::cascade) export_flags: u16,
}

#[derive(Clone, Debug, Default)]
pub(in crate::cascade) struct NbdReconfigureRequest<'a> {
    pub(in crate::cascade) index: u32,
    pub(in crate::cascade) timeout_secs: Option<u64>,
    pub(in crate::cascade) dead_conn_timeout_secs: Option<u64>,
    pub(in crate::cascade) backend_identifier: Option<&'a str>,
    pub(in crate::cascade) socket_fds: &'a [i32],
}

pub(in crate::cascade) trait NbdNetlinkTransport {
    fn resolve_family_id(&mut self) -> Result<u16, CascadeError>;
    fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>, CascadeError>;
}

const fn align4(len: usize) -> usize {
    (len + 3) & !3
}

struct NlMsgBuilder {
    buf: Vec<u8>,
}

impl NlMsgBuilder {
    fn new(nlmsg_type: u16, nlmsg_flags: u16, seq: u32, cmd: u8, version: u8) -> Self {
        let mut buf = Vec::with_capacity(128);
        buf.extend_from_slice(&0u32.to_ne_bytes());
        buf.extend_from_slice(&nlmsg_type.to_ne_bytes());
        buf.extend_from_slice(&nlmsg_flags.to_ne_bytes());
        buf.extend_from_slice(&seq.to_ne_bytes());
        buf.extend_from_slice(&0u32.to_ne_bytes());
        buf.push(cmd);
        buf.push(version);
        buf.extend_from_slice(&0u16.to_ne_bytes());
        Self { buf }
    }

    fn push_attr(&mut self, nla_type: u16, payload: &[u8]) -> Result<(), CascadeError> {
        let len = NLA_HDRLEN
            .checked_add(payload.len())
            .and_then(|v| u16::try_from(v).ok())
            .ok_or_else(|| {
                CascadeError::Precondition("netlink attribute exceeds u16 length".into())
            })?;
        self.buf.extend_from_slice(&len.to_ne_bytes());
        self.buf.extend_from_slice(&nla_type.to_ne_bytes());
        self.buf.extend_from_slice(payload);
        let pad = align4(payload.len()).saturating_sub(payload.len());
        self.buf.resize(self.buf.len() + pad, 0);
        Ok(())
    }

    fn push_nested<F>(&mut self, nla_type: u16, build: F) -> Result<(), CascadeError>
    where
        F: FnOnce(&mut Self) -> Result<(), CascadeError>,
    {
        let start = self.buf.len();
        self.buf.extend_from_slice(&0u16.to_ne_bytes());
        self.buf
            .extend_from_slice(&(nla_type | NLA_F_NESTED).to_ne_bytes());
        build(self)?;
        let len = self
            .buf
            .len()
            .checked_sub(start)
            .and_then(|v| u16::try_from(v).ok())
            .ok_or_else(|| CascadeError::Precondition("nested netlink attr overflow".into()))?;
        self.buf[start..start + 2].copy_from_slice(&len.to_ne_bytes());
        Ok(())
    }

    fn finish(mut self) -> Result<Vec<u8>, CascadeError> {
        let len = u32::try_from(self.buf.len())
            .map_err(|_| CascadeError::Precondition("netlink message exceeds u32 length".into()))?;
        self.buf[0..4].copy_from_slice(&len.to_ne_bytes());
        Ok(self.buf)
    }
}

struct NlMsgView<'a> {
    msg_type: u16,
    seq: u32,
    payload: &'a [u8],
}

fn read_u16_ne(bytes: &[u8], offset: usize) -> Option<u16> {
    let slice = bytes.get(offset..offset + 2)?;
    Some(u16::from_ne_bytes([slice[0], slice[1]]))
}

fn read_u32_ne(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset + 4)?;
    Some(u32::from_ne_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_i32_ne(bytes: &[u8], offset: usize) -> Option<i32> {
    let slice = bytes.get(offset..offset + 4)?;
    Some(i32::from_ne_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn parse_nlmsgs(mut bytes: &[u8]) -> Result<Vec<NlMsgView<'_>>, CascadeError> {
    let mut msgs = Vec::new();
    while bytes.len() >= NLMSG_HDRLEN {
        let len = read_u32_ne(bytes, 0).unwrap_or(0) as usize;
        if len < NLMSG_HDRLEN || len > bytes.len() {
            return Err(CascadeError::Precondition(
                "truncated netlink message header".into(),
            ));
        }
        let msg_type = read_u16_ne(bytes, 4).unwrap_or(0);
        let seq = read_u32_ne(bytes, 8).unwrap_or(0);
        let payload = &bytes[NLMSG_HDRLEN..len];
        msgs.push(NlMsgView {
            msg_type,
            seq,
            payload,
        });
        let step = align4(len).min(bytes.len());
        bytes = &bytes[step..];
    }
    Ok(msgs)
}

fn parse_attrs(mut bytes: &[u8]) -> Result<Vec<(u16, &[u8])>, CascadeError> {
    let mut attrs = Vec::new();
    while bytes.len() >= NLA_HDRLEN {
        let len = read_u16_ne(bytes, 0).unwrap_or(0) as usize;
        let kind = read_u16_ne(bytes, 2).unwrap_or(0) & !(NLA_F_NESTED | (1 << 14));
        if len < NLA_HDRLEN || len > bytes.len() {
            return Err(CascadeError::Precondition(
                "malformed netlink attribute length".into(),
            ));
        }
        attrs.push((kind, &bytes[NLA_HDRLEN..len]));
        let step = align4(len).min(bytes.len());
        bytes = &bytes[step..];
    }
    Ok(attrs)
}

pub(in crate::cascade) fn build_get_family_request(seq: u32) -> Result<Vec<u8>, CascadeError> {
    // Query commands emit a GENL reply (or NLMSG_ERROR on failure); setting
    // NLM_F_ACK queues an extra trailing NLMSG_ERROR(0) packet that would
    // desynchronize subsequent single-recv() exchanges on the same socket.
    let mut msg = NlMsgBuilder::new(GENL_ID_CTRL, NLM_F_REQUEST, seq, CTRL_CMD_GETFAMILY, 1);
    msg.push_attr(CTRL_ATTR_FAMILY_NAME, NBD_GENL_FAMILY_NAME)?;
    msg.finish()
}

pub(in crate::cascade) fn parse_get_family_id(
    reply: &[u8],
    expected_seq: u32,
) -> Result<u16, CascadeError> {
    for msg in parse_nlmsgs(reply)? {
        if msg.seq != expected_seq {
            continue;
        }
        if msg.msg_type == NLMSG_ERROR {
            let err = read_i32_ne(msg.payload, 0).unwrap_or(-libc::EINVAL);
            if err != 0 {
                return Err(CascadeError::Precondition(format!(
                    "netlink nbd family lookup failed: errno {}",
                    err.saturating_neg()
                )));
            }
            continue;
        }
        if msg.payload.len() < GENL_HDRLEN {
            continue;
        }
        for (kind, value) in parse_attrs(&msg.payload[GENL_HDRLEN..])? {
            if kind == CTRL_ATTR_FAMILY_ID && value.len() >= 2 {
                return Ok(read_u16_ne(value, 0).unwrap_or(0));
            }
        }
    }
    Err(CascadeError::Precondition(
        "netlink generic controller omitted nbd family id".into(),
    ))
}

pub(in crate::cascade) fn build_nbd_status_request(
    family_id: u16,
    seq: u32,
    index: u32,
) -> Result<Vec<u8>, CascadeError> {
    let mut msg = NlMsgBuilder::new(
        family_id,
        NLM_F_REQUEST,
        seq,
        NBD_CMD_STATUS,
        NBD_GENL_VERSION,
    );
    msg.push_attr(NBD_ATTR_INDEX, &index.to_ne_bytes())?;
    msg.finish()
}

pub(in crate::cascade) fn parse_nbd_status_response(
    reply: &[u8],
    expected_seq: u32,
    expected_index: u32,
) -> Result<Option<NbdDeviceStatus>, CascadeError> {
    for msg in parse_nlmsgs(reply)? {
        if msg.seq != expected_seq {
            continue;
        }
        if msg.msg_type == NLMSG_ERROR {
            let err = read_i32_ne(msg.payload, 0).unwrap_or(-libc::EINVAL);
            if err != 0 {
                return Err(CascadeError::Precondition(format!(
                    "netlink NBD_CMD_STATUS failed: errno {}",
                    err.saturating_neg()
                )));
            }
            continue;
        }
        if msg.msg_type == NLMSG_DONE || msg.payload.len() < GENL_HDRLEN {
            continue;
        }
        for (kind, value) in parse_attrs(&msg.payload[GENL_HDRLEN..])? {
            if kind != NBD_ATTR_DEVICE_LIST {
                continue;
            }
            for (item_kind, item) in parse_attrs(value)? {
                if item_kind != NBD_DEVICE_ITEM {
                    continue;
                }
                let mut index = None;
                let mut connected = None;
                for (field, bytes) in parse_attrs(item)? {
                    if field == NBD_DEVICE_INDEX && bytes.len() >= 4 {
                        index = read_u32_ne(bytes, 0);
                    } else if field == NBD_DEVICE_CONNECTED && !bytes.is_empty() {
                        connected = Some(bytes[0] != 0);
                    }
                }
                if index == Some(expected_index) {
                    return Ok(Some(NbdDeviceStatus {
                        index: expected_index,
                        connected: connected.unwrap_or(false),
                    }));
                }
            }
        }
    }
    Ok(None)
}

pub(in crate::cascade) fn build_nbd_reconfigure_request(
    family_id: u16,
    seq: u32,
    req: &NbdReconfigureRequest<'_>,
) -> Result<Vec<u8>, CascadeError> {
    let mut msg = NlMsgBuilder::new(
        family_id,
        NLM_F_REQUEST | NLM_F_ACK,
        seq,
        NBD_CMD_RECONFIGURE,
        NBD_GENL_VERSION,
    );
    msg.push_attr(NBD_ATTR_INDEX, &req.index.to_ne_bytes())?;
    if let Some(timeout) = req.timeout_secs {
        msg.push_attr(NBD_ATTR_TIMEOUT, &timeout.to_ne_bytes())?;
    }
    if let Some(dead_conn_timeout) = req.dead_conn_timeout_secs {
        msg.push_attr(NBD_ATTR_DEAD_CONN_TIMEOUT, &dead_conn_timeout.to_ne_bytes())?;
    }
    if let Some(identifier) = req.backend_identifier {
        let mut nul = identifier.as_bytes().to_vec();
        nul.push(0);
        msg.push_attr(NBD_ATTR_BACKEND_IDENTIFIER, &nul)?;
    }
    if !req.socket_fds.is_empty() {
        msg.push_nested(NBD_ATTR_SOCKETS, |outer| {
            for &fd in req.socket_fds {
                let raw = u32::try_from(fd)
                    .map_err(|_| CascadeError::Precondition("invalid negative socket fd".into()))?;
                outer.push_nested(NBD_SOCK_ITEM, |item| {
                    item.push_attr(NBD_SOCK_FD, &raw.to_ne_bytes())
                })?;
            }
            Ok(())
        })?;
    }
    msg.finish()
}

pub(in crate::cascade) fn parse_netlink_ack(
    reply: &[u8],
    expected_seq: u32,
) -> Result<(), CascadeError> {
    for msg in parse_nlmsgs(reply)? {
        if msg.seq != expected_seq || msg.msg_type != NLMSG_ERROR {
            continue;
        }
        let err = read_i32_ne(msg.payload, 0).ok_or_else(|| {
            CascadeError::Precondition("truncated netlink NLMSG_ERROR reply".into())
        })?;
        if err == 0 {
            return Ok(());
        }
        return Err(CascadeError::Shell {
            cmd: "nbd-netlink".into(),
            msg: format!(
                "NBD netlink request failed with errno {}",
                err.saturating_neg()
            ),
        });
    }
    Err(CascadeError::Precondition(
        "netlink reply omitted expected ACK".into(),
    ))
}

pub(in crate::cascade) fn nbd_device_index(dev_path: &str) -> Result<u32, CascadeError> {
    let resolved_buf;
    let candidate = if dev_path.starts_with("/dev/nbd") {
        dev_path
    } else if let Ok(target) = std::fs::read_link(dev_path) {
        resolved_buf = target.to_string_lossy().into_owned();
        resolved_buf.as_str()
    } else {
        dev_path
    };
    let suffix = candidate
        .strip_prefix("/dev/nbd")
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .ok_or_else(|| {
            CascadeError::Precondition(format!("invalid NBD device path: {dev_path}"))
        })?;
    suffix
        .parse::<u32>()
        .map_err(|_| CascadeError::Precondition(format!("NBD index overflow in path: {dev_path}")))
}

pub(in crate::cascade) fn handshake_nbd_stream<S: Read + Write>(
    stream: &mut S,
    export_name: &str,
) -> Result<NbdHandshakeExport, CascadeError> {
    let mut banner = [0u8; 18];
    stream
        .read_exact(&mut banner)
        .map_err(|error| CascadeError::Io(format!("read NBD handshake banner: {error}")))?;
    if &banner[0..8] != NBD_MAGIC_BANNER
        || u64::from_be_bytes([
            banner[8], banner[9], banner[10], banner[11], banner[12], banner[13], banner[14],
            banner[15],
        ]) != NBD_OPT_MAGIC
    {
        return Err(CascadeError::Precondition(
            "NBD peer sent an invalid newstyle handshake banner".into(),
        ));
    }
    let handshake_flags = u16::from_be_bytes([banner[16], banner[17]]);
    if handshake_flags & NBD_FLAG_FIXED_NEWSTYLE == 0 {
        return Err(CascadeError::Precondition(
            "NBD peer omitted NBD_FLAG_FIXED_NEWSTYLE".into(),
        ));
    }
    let client_flags = NBD_FLAG_C_FIXED_NEWSTYLE
        | if handshake_flags & NBD_FLAG_NO_ZEROES != 0 {
            NBD_FLAG_C_NO_ZEROES
        } else {
            0
        };
    stream
        .write_all(&client_flags.to_be_bytes())
        .map_err(|error| CascadeError::Io(format!("write NBD client flags: {error}")))?;
    let name_bytes = export_name.as_bytes();
    let name_len = u32::try_from(name_bytes.len())
        .map_err(|_| CascadeError::Precondition("NBD export name exceeds u32 length".into()))?;
    let mut opt = Vec::with_capacity(16 + name_bytes.len());
    opt.extend_from_slice(&NBD_OPT_MAGIC.to_be_bytes());
    opt.extend_from_slice(&NBD_OPT_EXPORT_NAME.to_be_bytes());
    opt.extend_from_slice(&name_len.to_be_bytes());
    opt.extend_from_slice(name_bytes);
    stream
        .write_all(&opt)
        .and_then(|()| stream.flush())
        .map_err(|error| CascadeError::Io(format!("write NBD_OPT_EXPORT_NAME: {error}")))?;

    let mut reply = [0u8; 10];
    stream
        .read_exact(&mut reply)
        .map_err(|error| CascadeError::Io(format!("read NBD export metadata: {error}")))?;
    let size_bytes = u64::from_be_bytes([
        reply[0], reply[1], reply[2], reply[3], reply[4], reply[5], reply[6], reply[7],
    ]);
    let export_flags = u16::from_be_bytes([reply[8], reply[9]]);
    if handshake_flags & NBD_FLAG_NO_ZEROES == 0 {
        let mut zeroes = [0u8; 124];
        stream
            .read_exact(&mut zeroes)
            .map_err(|error| CascadeError::Io(format!("read NBD handshake zeroes: {error}")))?;
    }
    if size_bytes < 4096 || size_bytes % 4096 != 0 {
        return Err(CascadeError::Precondition(format!(
            "NBD peer exported invalid size: {size_bytes}"
        )));
    }
    if export_flags & NBD_FLAG_HAS_FLAGS == 0 || export_flags & NBD_FLAG_CAN_MULTI_CONN == 0 {
        return Err(CascadeError::Precondition(
            "NBD peer omitted CAN_MULTI_CONN; live socket replacement refused".into(),
        ));
    }
    Ok(NbdHandshakeExport {
        size_bytes,
        export_flags,
    })
}

pub(in crate::cascade) fn handshake_unix_nbd_socket(
    socket_path: &Path,
    export_name: &str,
    timeout: Duration,
) -> Result<(UnixStream, NbdHandshakeExport), CascadeError> {
    let mut stream = UnixStream::connect(socket_path).map_err(|error| {
        CascadeError::Io(format!(
            "connect replacement NBD socket {}: {error}",
            socket_path.display()
        ))
    })?;
    stream
        .set_read_timeout(Some(timeout))
        .and_then(|()| stream.set_write_timeout(Some(timeout)))
        .map_err(|error| CascadeError::Io(format!("configure NBD socket timeout: {error}")))?;
    let export = handshake_nbd_stream(&mut stream, export_name)?;
    stream
        .set_read_timeout(None)
        .and_then(|()| stream.set_write_timeout(None))
        .map_err(|error| CascadeError::Io(format!("clear NBD socket timeout: {error}")))?;
    Ok((stream, export))
}

pub(super) struct KernelNbdNetlink {
    fd: OwnedFd,
    seq: u32,
}

impl KernelNbdNetlink {
    pub(super) fn open() -> Result<Self, CascadeError> {
        let fd = rustix::net::socket_with(
            rustix::net::AddressFamily::NETLINK,
            rustix::net::SocketType::RAW,
            rustix::net::SocketFlags::CLOEXEC,
            Some(rustix::net::netlink::GENERIC),
        )
        .map_err(|error| CascadeError::Io(format!("open netlink socket: {error}")))?;
        rustix::net::bind(&fd, &rustix::net::netlink::SocketAddrNetlink::new(0, 0))
            .map_err(|error| CascadeError::Io(format!("bind netlink socket: {error}")))?;
        rustix::net::sockopt::set_socket_timeout(
            &fd,
            rustix::net::sockopt::Timeout::Recv,
            Some(NETLINK_IO_TIMEOUT),
        )
        .map_err(|error| CascadeError::Io(format!("set netlink recv timeout: {error}")))?;
        rustix::net::sockopt::set_socket_timeout(
            &fd,
            rustix::net::sockopt::Timeout::Send,
            Some(NETLINK_IO_TIMEOUT),
        )
        .map_err(|error| CascadeError::Io(format!("set netlink send timeout: {error}")))?;
        Ok(Self { fd, seq: 100 })
    }

    fn next_seq(&mut self) -> u32 {
        let current = self.seq;
        self.seq = self.seq.wrapping_add(1).max(1);
        current
    }
}

impl NbdNetlinkTransport for KernelNbdNetlink {
    fn resolve_family_id(&mut self) -> Result<u16, CascadeError> {
        let seq = self.next_seq();
        let req = build_get_family_request(seq)?;
        let reply = self.exchange(&req)?;
        parse_get_family_id(&reply, seq)
    }

    fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>, CascadeError> {
        rustix::net::send(&self.fd, request, rustix::net::SendFlags::empty())
            .map_err(|error| CascadeError::Io(format!("send netlink request: {error}")))?;
        let mut buf = vec![0u8; MAX_NETLINK_REPLY_BYTES];
        let (got, _) = rustix::net::recv(&self.fd, &mut buf, rustix::net::RecvFlags::empty())
            .map_err(|error| CascadeError::Io(format!("recv netlink reply: {error}")))?;
        buf.truncate(got);
        Ok(buf)
    }
}

pub(in crate::cascade) fn arm_dead_conn_timeout_with<T: NbdNetlinkTransport>(
    transport: &mut T,
    seq: u32,
    swap_dev: &str,
    dead_conn_timeout_secs: u64,
) -> Result<(), CascadeError> {
    if dead_conn_timeout_secs == 0 {
        return Err(CascadeError::Precondition(
            "NBD dead_conn_timeout must be non-zero when armed".into(),
        ));
    }
    let index = nbd_device_index(swap_dev)?;
    let family_id = transport.resolve_family_id()?;
    let req = build_nbd_reconfigure_request(
        family_id,
        seq,
        &NbdReconfigureRequest {
            index,
            timeout_secs: None,
            dead_conn_timeout_secs: Some(dead_conn_timeout_secs),
            backend_identifier: None,
            socket_fds: &[],
        },
    )?;
    let reply = transport.exchange(&req)?;
    parse_netlink_ack(&reply, seq)
}

pub(in crate::cascade) fn arm_dead_conn_timeout(
    swap_dev: &str,
    dead_conn_timeout_secs: u64,
) -> Result<(), CascadeError> {
    let mut transport = KernelNbdNetlink::open()?;
    let seq = transport.next_seq().wrapping_add(1);
    arm_dead_conn_timeout_with(&mut transport, seq, swap_dev, dead_conn_timeout_secs)
}

pub(in crate::cascade) fn query_device_status_with<T: NbdNetlinkTransport>(
    transport: &mut T,
    seq: u32,
    swap_dev: &str,
) -> Result<NbdDeviceStatus, CascadeError> {
    let index = nbd_device_index(swap_dev)?;
    let family_id = transport.resolve_family_id()?;
    let req = build_nbd_status_request(family_id, seq, index)?;
    let reply = transport.exchange(&req)?;
    parse_nbd_status_response(&reply, seq, index)?.ok_or_else(|| {
        CascadeError::Precondition(format!("NBD netlink status omitted index {index}"))
    })
}

#[derive(Clone, Debug)]
pub(in crate::cascade) struct NbdPeerAttachOptions<'a> {
    pub(in crate::cascade) socket_path: &'a Path,
    pub(in crate::cascade) export_name: &'a str,
    pub(in crate::cascade) expected_size_bytes: Option<u64>,
    pub(in crate::cascade) dead_conn_timeout_secs: Option<u64>,
    pub(in crate::cascade) backend_identifier: Option<&'a str>,
}

pub(in crate::cascade) fn replace_peer_socket_with<T: NbdNetlinkTransport>(
    transport: &mut T,
    seq: u32,
    swap_dev: &str,
    options: &NbdPeerAttachOptions<'_>,
) -> Result<NbdHandshakeExport, CascadeError> {
    let index = nbd_device_index(swap_dev)?;
    let (stream, export) =
        handshake_unix_nbd_socket(options.socket_path, options.export_name, NETLINK_IO_TIMEOUT)?;
    if let Some(expected) = options.expected_size_bytes
        && export.size_bytes != expected
    {
        return Err(CascadeError::Precondition(format!(
            "replacement NBD export size {} disagrees with expected {expected}",
            export.size_bytes
        )));
    }
    let family_id = transport.resolve_family_id()?;
    let fds = [stream.as_raw_fd()];
    let req = build_nbd_reconfigure_request(
        family_id,
        seq,
        &NbdReconfigureRequest {
            index,
            timeout_secs: None,
            dead_conn_timeout_secs: options.dead_conn_timeout_secs,
            backend_identifier: options.backend_identifier,
            socket_fds: &fds,
        },
    )?;
    let reply = transport.exchange(&req)?;
    parse_netlink_ack(&reply, seq)?;
    drop(stream);
    Ok(export)
}

#[cfg(test)]
#[path = "nbd_netlink/tests.rs"]
pub(super) mod tests;
