// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-agent/src/lib.rs
//! RamShared agent (tenant) library: pressure collection (PSI), swap execution
//! over NBD, and session watchdog. The pure logic (parsing `/proc`, assembling argv,
//! watchdog window) lives here and is covered by unit tests; `main.rs` only
//! wires these pieces together with sockets/threads (DT-27).
//!
#![forbid(unsafe_code)]

pub mod explain;
pub mod local;
pub mod psi;
pub mod socket;
pub mod swap;
pub mod watchdog;
pub mod win_mem;
