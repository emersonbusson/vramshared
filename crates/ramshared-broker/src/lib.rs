// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-broker/src/lib.rs
//! ramshared-broker — protocol (JSON-lines) + model + policy of the Memory Broker arbiter.
//!
//!
//! **Pure library, testable without network/root/GPU**: model types ([`model`]), JSON-lines codec
//! ([`protocol`], DT-1), and — in ITEM-4 — the slice map and the arbiter (injected clock). The
//! plumbing (sockets, worker, IO) lives in the `ramsharedd` daemon (crate `ramshared-wsl2d`, ITEM-8).
#![forbid(unsafe_code)]

pub mod arbiter;
pub mod lease;
pub mod model;
pub mod protocol;
pub mod slices;
