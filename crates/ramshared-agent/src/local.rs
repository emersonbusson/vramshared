// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-agent/src/local.rs
//! Local loopback protocol used by DCC adapters.
//!
//! This is intentionally distinct from the broker wire protocol (DT-35). The
//! addon can ask for a lease and receive a compact evidence-backed snapshot,
//! without gaining access to swap or broker control messages.
#![forbid(unsafe_code)]

use std::io::{BufRead, Read, Write};

/// Longest accepted frame **content**. The wire form adds one newline (see
/// `write_json_line`), so a decoder must bound itself to `MAX_LINE_BYTES + 1`
/// and never allocate before that bound is known (AUD-07).
pub const MAX_LINE_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LocalMsg {
    Status,
    LeaseRequest { bytes: u64, client: String },
    LeaseRelease { lease: u32 },
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LocalReply {
    Status {
        vram_free: Option<u64>,
        vram_total: Option<u64>,
        lease: Option<u32>,
        evidence: Vec<String>,
    },
    LeaseGranted {
        lease: u32,
        bytes: u64,
    },
    LeaseDenied {
        reason: String,
    },
    Released {
        lease: u32,
    },
    Error {
        reason: String,
    },
}

pub fn write_json_line<W: Write, T: serde::Serialize>(w: &mut W, value: &T) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    if line.len() > MAX_LINE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "local message too large",
        ));
    }
    line.push(b'\n');
    w.write_all(&line)?;
    // The protocol is one message per line over a buffered socket: a reply
    // that never leaves the buffer looks like a hung peer to the reader.
    w.flush()
}

/// Reads one frame, refusing to allocate past the wire budget first.
///
/// The wire form is the content plus one newline, so the reader is capped at
/// exactly that length. Anything that fills the cap without a terminator is
/// either over the limit or a truncated frame, and neither is a message
/// (AUD-07).
pub fn read_json_line<R: BufRead, T: serde::de::DeserializeOwned>(
    r: &mut R,
) -> std::io::Result<Option<T>> {
    let mut buf = Vec::new();
    let n = r
        .take((MAX_LINE_BYTES + 1) as u64)
        .read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    let content = match buf.strip_suffix(b"\n") {
        Some(content) => content,
        // Hit the byte limit (or EOF) with no terminator: over the cap, or
        // truncated. Either way it is not a complete frame.
        None => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "local message too large or truncated",
            ));
        }
    };
    if content.len() > MAX_LINE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "local message too large",
        ));
    }
    let text = std::str::from_utf8(content).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "local message is not UTF-8",
        )
    })?;
    // `trim_end` keeps CRLF senders working, as before.
    serde_json::from_str(text.trim_end())
        .map(Some)
        .map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    use std::io::Cursor;

    /// A `BufRead` that hands out at most `step` bytes per fill, so a decoder
    /// has to reassemble a frame that arrives in pieces (AUD-07).
    struct Drip {
        data: Vec<u8>,
        pos: usize,
        step: usize,
    }

    impl Read for Drip {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let end = (self.pos + self.step).min(self.data.len());
            let n = (end - self.pos).min(out.len());
            out[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    impl BufRead for Drip {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            let end = (self.pos + self.step).min(self.data.len());
            Ok(&self.data[self.pos..end])
        }
        fn consume(&mut self, n: usize) {
            self.pos += n;
        }
    }

    /// A `LocalMsg` whose serialized form is exactly `MAX_LINE_BYTES`.
    fn max_sized_msg() -> LocalMsg {
        let prefix = r#"{"type":"lease_request","bytes":4096,"client":""#;
        let suffix = r#""}"#;
        let pad = MAX_LINE_BYTES - prefix.len() - suffix.len();
        LocalMsg::LeaseRequest {
            bytes: 4096,
            client: "A".repeat(pad),
        }
    }

    #[test]
    fn local_protocol_roundtrip() {
        let msg = LocalMsg::LeaseRequest {
            bytes: 4096,
            client: "dcc-test".into(),
        };
        let mut buf = Vec::new();
        write_json_line(&mut buf, &msg).expect("write");
        let decoded: LocalMsg = read_json_line(&mut Cursor::new(buf))
            .expect("read")
            .expect("msg");
        assert_eq!(decoded, msg);
    }

    #[test]
    fn write_json_line_too_large_errors() {
        let msg = LocalMsg::LeaseRequest {
            bytes: 4096,
            client: "A".repeat(MAX_LINE_BYTES),
        };
        let mut buf = Vec::new();
        let err = write_json_line(&mut buf, &msg).expect_err("oversized write must fail");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(buf.is_empty());
    }

    #[test]
    fn write_json_line_flushes_the_frame() {
        // A buffered socket that is never flushed looks like a hung peer.
        struct Tally {
            flushed: bool,
        }
        impl Write for Tally {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.flushed = true;
                Ok(())
            }
        }
        let mut sink = Tally { flushed: false };
        write_json_line(&mut sink, &LocalMsg::Status).expect("write");
        assert!(
            sink.flushed,
            "the frame must leave the buffer before return"
        );
    }

    #[test]
    fn read_json_line_accepts_exactly_the_maximum_content() {
        let msg = max_sized_msg();
        let mut wire = serde_json::to_vec(&msg).expect("serialize");
        assert_eq!(wire.len(), MAX_LINE_BYTES);
        wire.push(b'\n');
        assert_eq!(wire.len(), MAX_LINE_BYTES + 1);

        let mut cursor = Cursor::new(wire);
        let decoded: LocalMsg = read_json_line(&mut cursor)
            .expect("a full-length frame is valid")
            .expect("msg");
        assert_eq!(decoded, msg);
    }

    #[test]
    fn read_json_line_never_consumes_beyond_cap_plus_one() {
        // An adversarial peer streams far more than the cap. The decoder must
        // stop inside the wire budget and leave the rest unread (AUD-07).
        let mut input = vec![b'A'; MAX_LINE_BYTES * 4];
        input.push(b'\n');
        let mut cursor = Cursor::new(input);
        let err = read_json_line::<_, LocalMsg>(&mut cursor).expect_err("must reject");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            cursor.position() as usize <= MAX_LINE_BYTES + 1,
            "consumed {} bytes, budget is {}",
            cursor.position(),
            MAX_LINE_BYTES + 1
        );
    }

    #[test]
    fn agent_local_reader_rejects_unterminated_oversized_line_without_oom() {
        let mut cursor = Cursor::new(vec![b'A'; 1024 * 1024]);
        let error = read_json_line::<_, LocalMsg>(&mut cursor)
            .expect_err("an unterminated megabyte must be refused");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            cursor.position() as usize <= MAX_LINE_BYTES + 1,
            "consumed {} bytes, budget is {}",
            cursor.position(),
            MAX_LINE_BYTES + 1
        );
    }

    #[test]
    fn read_json_line_too_large_errors() {
        // Over the content cap: one byte more than the wire budget can hold.
        let mut input = vec![b'A'; MAX_LINE_BYTES + 1];
        input.push(b'\n');
        let err = read_json_line::<_, LocalMsg>(&mut Cursor::new(input))
            .expect_err("oversized read must fail");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn read_json_line_refuses_a_frame_without_its_terminator() {
        // A frame is content plus newline. Without the terminator the read
        // reached EOF or the byte limit mid-frame, and a half frame is not a
        // message (AUD-07).
        for input in [
            br#"{"type":"status"}"#.to_vec(),
            vec![b'A'; MAX_LINE_BYTES],
            vec![b'A'; MAX_LINE_BYTES + 1],
        ] {
            let err = read_json_line::<_, LocalMsg>(&mut Cursor::new(input))
                .expect_err("unterminated frame must fail");
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn read_json_line_refuses_non_utf8() {
        let input = vec![0xff, 0xfe, 0xfd, b'\n'];
        let err =
            read_json_line::<_, LocalMsg>(&mut Cursor::new(input)).expect_err("non-utf8 must fail");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn read_json_line_reassembles_a_slow_trickle() {
        let msg = LocalMsg::LeaseRequest {
            bytes: 4096,
            client: "dcc-test".into(),
        };
        let mut wire = serde_json::to_vec(&msg).expect("serialize");
        wire.push(b'\n');
        let mut drip = Drip {
            data: wire,
            pos: 0,
            step: 1,
        };
        let decoded: LocalMsg = read_json_line(&mut drip).expect("read").expect("msg");
        assert_eq!(decoded, msg);
    }
}
