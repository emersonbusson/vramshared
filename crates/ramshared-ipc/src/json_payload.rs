// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-ipc/src/json_payload.rs
//! Refuse JSON overflow during serialization, before copying excess bytes.
use crate::{FrameError, checked_payload_length};
use serde::Serialize;
use std::io::{self, Write};

struct BoundedJson {
    bytes: Vec<u8>,
    cap: usize,
    oversized: Option<usize>,
    overflow: bool,
}
impl Write for BoundedJson {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(total) = self.bytes.len().checked_add(bytes.len()) else {
            self.overflow = true;
            return Err(io::Error::other("JSON payload length overflow"));
        };
        if total > self.cap {
            self.oversized = Some(total);
            return Err(io::Error::other("JSON payload exceeds its cap"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
pub(crate) fn encode<T: Serialize>(message: &T, cap: u32) -> Result<Vec<u8>, FrameError> {
    let mut writer = BoundedJson {
        bytes: Vec::with_capacity(cap as usize),
        cap: cap as usize,
        oversized: None,
        overflow: false,
    };
    let result = serde_json::to_writer(&mut writer, message);
    if writer.overflow {
        return Err(FrameError::PayloadLengthOverflow);
    }
    if let Some(size) = writer.oversized {
        return Err(FrameError::PayloadExceedsCap(checked_payload_length(size)?));
    }
    result.map_err(|error| FrameError::PayloadDeserialization(error.to_string()))?;
    writer
        .flush()
        .map_err(|error| FrameError::PayloadDeserialization(error.to_string()))?;
    Ok(writer.bytes)
}
