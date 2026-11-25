// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-integrity/src/lib.rs
//! ramshared-integrity — Block integrity verification (SPEC §8.1, §14.2).
//!
//! For `--debug-checksum` mode: Fast non-cryptographic hashing + pre-allocated
//! checksum table indexed by block number (detects corruption/torn reads in VRAM)
//! and reproducible patterns for `test-integrity`. Pure logic, no root required.
#![forbid(unsafe_code)]

pub mod hash;
pub mod pattern;

pub use hash::{ChecksumMismatchError, ChecksumTable, block_hash};
pub use pattern::{IntegrityError, Pattern, fill_block, verify_block};
