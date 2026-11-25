// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/nbd_runtime/recovery_cadence.rs
//! Recovery polling cadence, independent of NBD job rate.

use std::time::{Duration, Instant};

const RECOVERY_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Default)]
pub(super) struct RecoveryCadence {
    next: Option<Instant>,
}

impl RecoveryCadence {
    pub(super) fn due_at(&mut self, now: Instant) -> bool {
        if self.next.is_some_and(|next| now < next) {
            return false;
        }
        let Some(next) = now.checked_add(RECOVERY_INTERVAL) else {
            return false;
        };
        self.next = Some(next);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::RecoveryCadence;
    use std::time::{Duration, Instant};

    #[test]
    fn recovery_checks_are_bounded_even_when_jobs_arrive_continuously() {
        let mut cadence = RecoveryCadence::default();
        let start = Instant::now();
        assert!(cadence.due_at(start));
        for _ in 0..100 {
            assert!(!cadence.due_at(start + Duration::from_millis(249)));
        }
        assert!(cadence.due_at(start + Duration::from_millis(250)));
        assert!(!cadence.due_at(start + Duration::from_millis(250)));
    }

    #[test]
    fn a_backward_observation_never_accelerates_recovery() {
        let mut cadence = RecoveryCadence::default();
        let start = Instant::now();
        assert!(cadence.due_at(start + Duration::from_secs(1)));
        assert!(!cadence.due_at(start));
        assert!(cadence.due_at(start + Duration::from_millis(1250)));
    }
}
