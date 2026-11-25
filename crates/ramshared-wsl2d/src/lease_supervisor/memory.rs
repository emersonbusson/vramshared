// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/lease_supervisor/memory.rs
//! Host memory observations retain the exact publishing session generation.
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use ramshared_ipc::{HostMemoryTelemetry, gpu_catalog::AuthenticatedHostMemory};

use crate::host_gate::{ControlPlaneAuthority, SessionAuthority};

#[derive(Clone)]
pub struct SessionHostMemory {
    session: SessionAuthority,
    sample: AuthenticatedHostMemory,
}

impl SessionHostMemory {
    pub(super) fn new(session: SessionAuthority, sample: AuthenticatedHostMemory) -> Self {
        Self { session, sample }
    }

    /// A point-in-time observation, not a memory reservation or effect permit.
    pub fn snapshot(
        &self,
        expected: &Arc<Mutex<ControlPlaneAuthority>>,
    ) -> Option<HostMemoryTelemetry> {
        self.session.observe_current(expected, |authority| {
            let now_ms = u64::try_from(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .ok()?
                    .as_millis(),
            )
            .ok()?;
            authority.admit_cache(now_ms).ok()?;
            self.sample.sample(Instant::now(), now_ms).cloned()
        })?
    }
}
