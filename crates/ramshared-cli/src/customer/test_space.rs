// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/customer/test_space.rs
//! Per-test ceiling for exercising space loss without filling a shared disk.

use std::cell::Cell;

thread_local! {
    static FREE_SPACE_CEILING: Cell<Option<u64>> = const { Cell::new(None) };
}

/// Clear the ceiling even if a fixture panics.
pub(super) struct SpaceCeiling;

impl SpaceCeiling {
    pub(super) fn new() -> Self {
        FREE_SPACE_CEILING.with(|ceiling| ceiling.set(None));
        Self
    }

    pub(super) fn set(&self, bytes: Option<u64>) {
        FREE_SPACE_CEILING.with(|ceiling| ceiling.set(bytes));
    }
}

impl Drop for SpaceCeiling {
    fn drop(&mut self) {
        FREE_SPACE_CEILING.with(|ceiling| ceiling.set(None));
    }
}

pub(super) fn limit(observed: u64) -> u64 {
    FREE_SPACE_CEILING.with(|ceiling| ceiling.get().map_or(observed, |cap| observed.min(cap)))
}

#[test]
fn ceiling_is_thread_local_and_cleared_on_drop() {
    let ceiling = SpaceCeiling::new();
    ceiling.set(Some(0));
    assert_eq!(limit(100), 0);
    assert_eq!(std::thread::spawn(|| limit(100)).join().ok(), Some(100));
    drop(ceiling);
    assert_eq!(limit(100), 100);
}
