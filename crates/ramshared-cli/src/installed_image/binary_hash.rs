// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/installed_image/binary_hash.rs
//! Byte-bounded SHA-256 for installed executable identity checks.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::release_identity::open_regular_no_follow;

/// Upper bound for one installed CLI, daemon, or running process image.
pub(super) const MAX_INSTALLED_EXECUTABLE_BYTES: u64 = 256 * 1024 * 1024;

pub(crate) fn sha256_file(path: &Path) -> Option<String> {
    let mut file = open_regular_no_follow(path)?;
    sha256_open_file(&mut file, MAX_INSTALLED_EXECUTABLE_BYTES)
}

/// Hash a descriptor only when its observed size fits the supplied byte cap.
/// The reader also enforces the cap in case the file grows after this check.
pub(super) fn sha256_open_file(file: &mut File, limit: u64) -> Option<String> {
    if file.metadata().ok()?.len() > limit {
        return None;
    }
    sha256_reader_with_limit(file, limit)
}

/// Hash at most the shared installed-executable limit from an arbitrary reader.
pub(crate) fn sha256_reader(reader: &mut impl Read) -> Option<String> {
    sha256_reader_with_limit(reader, MAX_INSTALLED_EXECUTABLE_BYTES)
}

/// Stream a digest while consuming no more than `limit + 1` bytes.
pub(crate) fn sha256_reader_with_limit(reader: &mut impl Read, limit: u64) -> Option<String> {
    let mut hasher = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    let mut remaining = limit;

    loop {
        if remaining == 0 {
            let mut overflow = [0];
            if reader.read(&mut overflow).ok()? != 0 {
                return None;
            }
            break;
        }

        let capacity = usize::try_from(remaining.min(buffer.len() as u64)).ok()?;
        let count = reader.read(&mut buffer[..capacity]).ok()?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        remaining = remaining.checked_sub(u64::try_from(count).ok()?)?;
    }

    let digest = hasher.finalize();
    let mut encoded = String::with_capacity(64);
    for byte in digest.iter() {
        use std::fmt::Write as FmtWrite;
        write!(&mut encoded, "{byte:02x}").ok()?;
    }
    Some(encoded)
}
