// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-block/src/isolated_origin/range_lock.rs
//! Bounded overlap coordination for the authoritative origin and cache.
//!
//! Locks are acquired in a fixed order. Hash collisions only serialize extra
//! ranges; they cannot let overlapping byte ranges pass independently.
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::IoError;

const STRIPES: usize = 256;
const STRIPE_BYTES: u64 = 64 * 1024;

pub(super) struct RangeLocks([RwLock<()>; STRIPES]);

impl Default for RangeLocks {
    fn default() -> Self {
        Self(std::array::from_fn(|_| RwLock::new(())))
    }
}

impl RangeLocks {
    fn touched(offset: u64, len: usize) -> [bool; STRIPES] {
        let mut selected = [false; STRIPES];
        if len == 0 {
            return selected;
        }
        let first = offset / STRIPE_BYTES;
        let last = offset.saturating_add((len as u64).saturating_sub(1)) / STRIPE_BYTES;
        let count = last
            .saturating_sub(first)
            .saturating_add(1)
            .min(STRIPES as u64);
        for step in 0..count {
            selected[((first + step) % STRIPES as u64) as usize] = true;
        }
        selected
    }

    pub(super) fn read(
        &self,
        offset: u64,
        len: usize,
    ) -> Result<Vec<RwLockReadGuard<'_, ()>>, IoError> {
        let mut guards = Vec::new();
        for (lock, touched) in self.0.iter().zip(Self::touched(offset, len)) {
            if touched {
                guards.push(
                    lock.read()
                        .map_err(|_| IoError("origin range lock poisoned".into()))?,
                );
            }
        }
        Ok(guards)
    }

    pub(super) fn write(
        &self,
        offset: u64,
        len: usize,
    ) -> Result<Vec<RwLockWriteGuard<'_, ()>>, IoError> {
        let mut guards = Vec::new();
        for (lock, touched) in self.0.iter().zip(Self::touched(offset, len)) {
            if touched {
                guards.push(
                    lock.write()
                        .map_err(|_| IoError("origin range lock poisoned".into()))?,
                );
            }
        }
        Ok(guards)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn stripes_cover_boundaries_wraparound_and_long_ranges() {
        assert_eq!(RangeLocks::touched(0, 0).iter().filter(|x| **x).count(), 0);
        let crossing = RangeLocks::touched(STRIPE_BYTES - 1, 2);
        assert!(crossing[0] && crossing[1]);
        assert_eq!(crossing.iter().filter(|x| **x).count(), 2);
        let wrap = RangeLocks::touched(
            STRIPE_BYTES * (STRIPES as u64 - 1),
            (STRIPE_BYTES + 1) as usize,
        );
        assert!(wrap[STRIPES - 1] && wrap[0]);
        assert!(
            RangeLocks::touched(1, STRIPES * STRIPE_BYTES as usize)
                .iter()
                .all(|x| *x)
        );
    }

    #[test]
    fn poisoned_range_is_refused() {
        let locks = std::sync::Arc::new(RangeLocks::default());
        let worker = std::sync::Arc::clone(&locks);
        let thread = std::thread::spawn(move || {
            let _guard = worker.write(0, 1).unwrap();
            panic!("poison fixture");
        });
        assert!(thread.join().is_err());
        assert!(locks.read(0, 1).is_err());
        assert!(locks.write(0, 1).is_err());
        assert!(locks.write(STRIPE_BYTES, 1).is_ok());
    }
}
