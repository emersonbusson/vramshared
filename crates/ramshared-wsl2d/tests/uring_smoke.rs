// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/tests/uring_smoke.rs
#![allow(clippy::unwrap_used, clippy::expect_used)] // teste: unwrap/expect é idiomático

use ramshared_wsl2d::uring_smoke;

#[test]
fn creates_io_uring_and_enters_without_work() {
    let report = uring_smoke::run(2).expect("io_uring smoke");

    assert_eq!(report.entries, 2);
    assert_eq!(report.submitted, 0);
}
