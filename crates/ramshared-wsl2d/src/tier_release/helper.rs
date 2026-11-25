// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/tier_release/helper.rs
//! Trusted helper resolution and bounded, non-cancelling child observation.

use super::HelperOutcome;
use std::path::Path;
use std::process::Child;
use std::time::{Duration, Instant};

pub(super) fn system_helper_path(command: &str) -> Option<&str> {
    match command {
        "fallocate" => Some("/usr/bin/fallocate"),
        "dd" => Some("/usr/bin/dd"),
        "mkswap" => Some("/usr/sbin/mkswap"),
        "swapon" => Some("/usr/sbin/swapon"),
        "swapoff" => Some("/usr/sbin/swapoff"),
        // Explicit paths are private call-site inputs, never CLI/env overrides.
        path if Path::new(path).is_absolute() => Some(path),
        _ => None,
    }
}

pub(super) trait ChildObservation {
    fn observe(&mut self) -> std::io::Result<Option<bool>>;
}

impl ChildObservation for Child {
    fn observe(&mut self) -> std::io::Result<Option<bool>> {
        self.try_wait()
            .map(|status| status.map(|status| status.success()))
    }
}

/// Observe a helper, leaving its owned handle with the caller on uncertainty.
/// No signal or wait can prove cancellation of an uninterruptible kernel task.
pub(super) fn observe_helper_until(
    child: &mut impl ChildObservation,
    deadline: Instant,
) -> HelperOutcome {
    loop {
        match child.observe() {
            Ok(Some(true)) => return HelperOutcome::Success,
            Ok(Some(false)) => return HelperOutcome::Failed,
            Err(error) => {
                eprintln!("[ramsharedd] tier helper observation failed; child retained: {error}");
                return HelperOutcome::Uncertain;
            }
            Ok(None) => {}
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            eprintln!(
                "[ramsharedd] tier helper deadline expired; operation remains owned and unproven"
            );
            return HelperOutcome::Uncertain;
        }
        std::thread::sleep(remaining.min(Duration::from_millis(20)));
    }
}
