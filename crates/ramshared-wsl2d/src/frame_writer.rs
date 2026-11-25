// SPDX-License-Identifier: GPL-2.0-only
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/frame_writer.rs
// Cursor-advancing NBD frame writer (SPEC zero-script-native-runtime ITEM-1,
// AUD-04/AUD-05). Extracted from `conn.rs` so the transport loop has one
// byte-progress authority and a bounded retry budget.

use std::io::{self, Write};
use std::time::Duration;

pub const MAX_ATTEMPTS: u32 = 5;

/// Sleeps one backoff step with the historical jitter, then widens the gap.
pub(crate) fn backoff_step(backoff: &mut Duration) {
    const MAX_BACKOFF: Duration = Duration::from_millis(500);
    let jitter = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
        % 20) as u64;
    let jitter_millis = jitter * backoff.as_millis() as u64 / 100;
    std::thread::sleep(*backoff + Duration::from_millis(jitter_millis));
    *backoff = (*backoff * 2).min(MAX_BACKOFF);
}

/// Writes one reply frame -- header then payload -- then flushes it.
///
/// The accepted-byte cursor advances on every short write, `Interrupted` and
/// progress; a non-transient error or `WriteZero` is terminal so the caller
/// tears the connection down instead of framing the next reply.
pub(crate) fn write_frame<W: Write>(
    writer: &mut W,
    header: &[u8],
    payload: &[u8],
) -> io::Result<()> {
    let total = header.len() + payload.len();
    let mut sent = 0;
    let mut attempt = 0_u32;
    let mut backoff = Duration::from_millis(10);
    while sent < total {
        let (chunk, from) = if sent < header.len() {
            (header, sent)
        } else {
            (payload, sent - header.len())
        };
        match writer.write(&chunk[from..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "writer accepted zero bytes",
                ));
            }
            Ok(n) => {
                sent += n;
                attempt = 0;
                backoff = Duration::from_millis(10);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                attempt += 1;
                if attempt > MAX_ATTEMPTS {
                    return Err(error);
                }
                backoff_step(&mut backoff);
            }
            Err(error) => return Err(error),
        }
    }
    loop {
        match writer.flush() {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                attempt += 1;
                if attempt > MAX_ATTEMPTS {
                    return Err(error);
                }
                backoff_step(&mut backoff);
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    struct MockWriter {
        bytes: Vec<u8>,
        short: usize,
        fail_at: Option<usize>,
        writes: usize,
    }

    impl MockWriter {
        fn new(short: usize, fail_at: Option<usize>) -> Self {
            Self {
                bytes: Vec::new(),
                short,
                fail_at,
                writes: 0,
            }
        }
    }

    impl Write for MockWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.fail_at == Some(self.writes) {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "fatal"));
            }
            self.writes += 1;
            let take = bytes.len().min(self.short.max(1));
            self.bytes.extend_from_slice(&bytes[..take]);
            Ok(take)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn nbd_frame_writer_advances_cursor_on_partial_write_and_aborts_on_fatal() {
        let header = [0xA5u8; 16];
        let payload = [0x5Au8; 8];
        let mut writer = MockWriter::new(13, None);
        write_frame(&mut writer, &header, &payload).unwrap();
        let mut expected = header.to_vec();
        expected.extend_from_slice(&payload);
        assert_eq!(writer.bytes, expected);

        let mut fatal = MockWriter::new(usize::MAX, Some(1));
        let error = write_frame(&mut fatal, &header, &payload).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(fatal.bytes, header.to_vec());
    }

    #[test]
    fn frame_writer_refuses_zero_progress_and_transient_exhaustion() {
        struct ZeroWriter;
        impl Write for ZeroWriter {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Ok(0)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert_eq!(
            write_frame(&mut ZeroWriter, b"h", b"p").unwrap_err().kind(),
            io::ErrorKind::WriteZero
        );

        struct BlockedWriter;
        impl Write for BlockedWriter {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::WouldBlock, "blocked"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert_eq!(
            write_frame(&mut BlockedWriter, b"h", b"p")
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
    }
}
