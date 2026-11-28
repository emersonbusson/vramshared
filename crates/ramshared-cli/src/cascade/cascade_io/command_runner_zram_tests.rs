// SPDX-License-Identifier: MIT
// Author: Emerson Busson
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Email: emersonbusson@gmail.com
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/command_runner_zram_tests.rs

use std::cell::{Cell, RefCell};

use super::*;

fn io_failure(errno: i32) -> ZramError {
    ZramError::Io(std::io::Error::from_raw_os_error(errno))
}

#[test]
fn busy_zram_reset_revalidates_before_retrying() {
    let events = RefCell::new(Vec::new());
    let attempts = Cell::new(0);
    let result = retry_busy_zram_reset(
        || {
            events.borrow_mut().push("reset");
            let count = attempts.get();
            attempts.set(count + 1);
            if count == 0 {
                Err(io_failure(libc::EBUSY))
            } else {
                Ok(())
            }
        },
        || {
            events.borrow_mut().push("revalidate");
            Ok(())
        },
        || events.borrow_mut().push("wait"),
    );
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(*events.borrow(), ["reset", "wait", "revalidate", "reset"]);
}

#[test]
fn busy_zram_reset_stops_at_the_attempt_limit_and_other_errors_never_retry() {
    for (errno, expected_attempts) in [(libc::EBUSY, 5), (libc::EPERM, 1)] {
        let attempts = Cell::new(0);
        let revalidations = Cell::new(0);
        let error = retry_busy_zram_reset(
            || {
                attempts.set(attempts.get() + 1);
                Err(io_failure(errno))
            },
            || {
                revalidations.set(revalidations.get() + 1);
                Ok(())
            },
            || {},
        );
        assert!(matches!(error, Err(CascadeError::Shell { .. })));
        assert_eq!(attempts.get(), expected_attempts);
        assert_eq!(revalidations.get(), expected_attempts - 1);
    }
}

#[test]
fn busy_zram_reset_aborts_on_lost_authority() {
    let attempts = Cell::new(0);
    let result = retry_busy_zram_reset(
        || {
            attempts.set(attempts.get() + 1);
            Err(io_failure(libc::EBUSY))
        },
        || Err(CascadeError::Precondition("device identity changed".into())),
        || {},
    );
    assert!(matches!(result, Err(CascadeError::Precondition(_))));
    assert_eq!(attempts.get(), 1);
}
