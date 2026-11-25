// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/uring_smoke.rs
//! Minimal io_uring smoke test for Phase B.
//!
//! This module validates `io_uring_setup` + `io_uring_enter` without a ublk device, without
//! opening `/dev/ublk-control`, and without touching swap. The goal is to test the runtime
//! gate before the first real ublk loop.

pub use ramshared_uring::SmokeReport;

pub fn run(entries: u32) -> std::io::Result<SmokeReport> {
    ramshared_uring::smoke(entries)
}
