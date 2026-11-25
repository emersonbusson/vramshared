// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/worker_custody/host_memory.rs
//! Hold host-memory admission until the exact child and its stop owners settle.
use std::io;
use std::sync::{Arc, Mutex};

use ramshared_wsl2d::host_memory_budget::MemoryReservation;

#[derive(Clone, Default)]
pub(super) struct HostMemoryCustody(Arc<Mutex<Option<MemoryReservation>>>);

impl HostMemoryCustody {
    pub(super) fn new(reservation: Option<MemoryReservation>) -> Self {
        Self(Arc::new(Mutex::new(reservation)))
    }

    /// Refuse to release if child identity, exit or stop custody is uncertain.
    pub(super) fn release_if_settled(
        &self,
        identity_lost: bool,
        exited: bool,
        stop_active: bool,
        reaper_active: bool,
    ) -> io::Result<bool> {
        if identity_lost || !exited || stop_active || reaper_active {
            return Ok(false);
        }
        let mut reservation = self
            .0
            .try_lock()
            .map_err(|_| io::Error::other("worker memory custody is busy or poisoned"))?;
        drop(reservation.take());
        Ok(true)
    }
}
