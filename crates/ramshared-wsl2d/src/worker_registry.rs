// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/worker_registry.rs
//! Process-lifetime admission for worker processes and their creating threads.
use std::io;
use std::sync::{Arc, Mutex, OnceLock};

use ramshared_vram::GpuAdapterIdentity;

use super::worker_custody::{Reservation, WorkerCustody};
use super::worker_startup::{StartOutcome, Startup, StartupAuthority, StartupCustody};

pub(super) type ReservedStart = Box<dyn FnOnce(Reservation) -> StartOutcome + Send>;

enum Scope {
    Automatic,
    Pinned(GpuAdapterIdentity),
}
impl Scope {
    fn from_request(adapter: Option<&GpuAdapterIdentity>) -> io::Result<Self> {
        let Some(adapter) = adapter else {
            return Ok(Self::Automatic);
        };
        if !adapter.is_stable_driver_identity() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid worker custody identity",
            ));
        }
        Ok(Self::Pinned(GpuAdapterIdentity {
            node_mask: adapter.node_mask,
            backend: adapter.backend.clone(),
            key: adapter.key.to_ascii_lowercase(),
            luid: adapter.luid.as_ref().map(|luid| luid.to_ascii_lowercase()),
        }))
    }

    fn conflicts(&self, other: &Self) -> bool {
        let (Self::Pinned(a), Self::Pinned(b)) = (self, other) else {
            return true;
        };
        !a.proves_independent_memory(b)
    }
}

struct Record {
    scope: Scope,
    native: Arc<WorkerCustody>,
    startup: Arc<StartupCustody>,
}
impl Record {
    fn is_quiescent(&self) -> bool {
        // The creator can still own startup resources after the child exits.
        // Observe its completion before native custody releases memory.
        self.startup.is_quiescent() && self.native.is_quiescent()
    }
}

#[derive(Default)]
pub(super) struct WorkerRegistry(Mutex<Vec<Record>>);

impl WorkerRegistry {
    pub(super) fn reserve(&self, adapter: Option<&GpuAdapterIdentity>) -> io::Result<Reservation> {
        self.admit(adapter, |record| record.native.reserve())
    }

    #[cfg(test)]
    pub(super) fn begin(
        &self,
        adapter: Option<&GpuAdapterIdentity>,
        task: ReservedStart,
    ) -> io::Result<Startup> {
        self.begin_with_host_memory(adapter, None, task)
    }

    /// Keep recovery authority and any admitted memory attached through handoff.
    pub(super) fn begin_guarded(
        &self,
        adapter: Option<&GpuAdapterIdentity>,
        task: ReservedStart,
        authority: StartupAuthority,
    ) -> io::Result<Startup> {
        self.begin_guarded_with_host_memory(adapter, None, task, authority)
    }

    fn begin_guarded_with_host_memory(
        &self,
        adapter: Option<&GpuAdapterIdentity>,
        host_memory: Option<ramshared_wsl2d::host_memory_budget::MemoryReservation>,
        task: ReservedStart,
        authority: StartupAuthority,
    ) -> io::Result<Startup> {
        self.admit(adapter, |record| {
            let authority: StartupAuthority = match host_memory.as_ref() {
                Some(memory) => {
                    let memory_is_current = memory.probe();
                    Arc::new(move || authority() && memory_is_current())
                }
                None => authority,
            };
            let reservation = record.native.reserve()?;
            let reservation = match host_memory {
                Some(memory) => reservation.with_host_memory(memory)?,
                None => reservation,
            };
            record
                .startup
                .begin_guarded(Box::new(move || task(reservation)), authority)
        })
    }

    /// Attach an already admitted memory grant before the creator can run.
    /// A rejected launch drops the grant; a spawned child retains it in native
    /// custody until its exact exit and all stop owners have settled.
    #[cfg(test)]
    pub(super) fn begin_with_host_memory(
        &self,
        adapter: Option<&GpuAdapterIdentity>,
        host_memory: Option<ramshared_wsl2d::host_memory_budget::MemoryReservation>,
        task: ReservedStart,
    ) -> io::Result<Startup> {
        self.admit(adapter, |record| {
            // Reserve before publishing a creator. A second request cannot
            // slip between startup admission and the actual child spawn.
            let reservation = record.native.reserve()?;
            let reservation = match host_memory {
                Some(memory) => reservation.with_host_memory(memory)?,
                None => reservation,
            };
            record.startup.begin(Box::new(move || task(reservation)))
        })
    }

    fn admit<T>(
        &self,
        adapter: Option<&GpuAdapterIdentity>,
        operation: impl FnOnce(&Record) -> io::Result<T>,
    ) -> io::Result<T> {
        let scope = Scope::from_request(adapter)?;
        let mut records = self.0.try_lock().map_err(|error| match error {
            std::sync::TryLockError::WouldBlock => io::Error::from(io::ErrorKind::WouldBlock),
            std::sync::TryLockError::Poisoned(_) => io::Error::other("worker registry poisoned"),
        })?;
        // An exited process with a live creator/reaper is still owned. This
        // joins only finished creators and never drops an unknown outcome.
        records.retain(|record| !record.is_quiescent());
        if records.iter().any(|record| scope.conflicts(&record.scope)) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "worker custody scope remains occupied",
            ));
        }
        records.try_reserve(1).map_err(io::Error::other)?;
        let index = records.len();
        records.push(Record {
            scope,
            native: Arc::new(WorkerCustody::default()),
            startup: Arc::new(StartupCustody::default()),
        });
        // Record custody before the effect, including any error or unwind.
        operation(&records[index])
    }
}

pub(super) fn registry() -> &'static WorkerRegistry {
    static REGISTRY: OnceLock<WorkerRegistry> = OnceLock::new();
    REGISTRY.get_or_init(WorkerRegistry::default)
}

#[cfg(test)]
mod tests;
