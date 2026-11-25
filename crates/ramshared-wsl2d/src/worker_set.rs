// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-wsl2d/src/worker_set.rs
//! Runtime ownership and lifecycle fan-out for immutable cache members.
use std::sync::Arc;
use std::time::Duration;

use ramshared_block::{AuthoritativeOriginBackend, CacheReleaseOutcome, FileOrigin};

use super::worker_recovery::{self, CacheWorkerObservation};
use super::{
    ControlPlaneCacheAdmission, IsolatedWorkerRuntime, NbdRuntimeStarter, OriginCache,
    OriginCacheState,
};

#[derive(Default)]
pub(super) struct WorkerSet {
    members: Vec<IsolatedWorkerRuntime>,
    next_recovery: usize,
    pressure_release_pending: bool,
    pressure_members_armed: bool,
    pressure_members: Vec<usize>,
    pressure_supervisor: Option<Arc<std::os::fd::OwnedFd>>,
}

impl WorkerSet {
    pub(super) fn with_capacity(count: usize) -> Result<Self, std::collections::TryReserveError> {
        let mut members = Vec::new();
        members.try_reserve_exact(count)?;
        Ok(Self::new(members))
    }

    /// Only construction appends; the complete capacity is reserved first.
    pub(super) fn append_initial(&mut self, mut member: IsolatedWorkerRuntime) {
        member.cache_member = self.members.len();
        self.members.push(member);
    }

    /// The origin builder owns membership; recovery never reorders its slots.
    pub(super) fn new(members: Vec<IsolatedWorkerRuntime>) -> Self {
        Self {
            members,
            next_recovery: 0,
            pressure_release_pending: false,
            pressure_members_armed: false,
            pressure_members: Vec::new(),
            pressure_supervisor: None,
        }
    }

    pub(super) fn record_pressure_release(&mut self, revoked: Vec<(usize, bool)>) {
        let mut added = false;
        for (index, starting) in revoked {
            if let Some(member) = self.members.iter_mut().find(|member| {
                member.cache_member == index && (!starting || member.startup.is_some())
            }) {
                member.unavailable_reason =
                    Some("paused by control-plane memory pressure; recovery pending".into());
                if !self.pressure_members.contains(&index) {
                    self.pressure_members.push(index);
                    added = true;
                }
            }
        }
        if added {
            self.pressure_release_pending = true;
            self.pressure_members_armed = false;
            self.pressure_supervisor = None;
        }
    }

    #[cfg(test)]
    pub(super) fn pressure_release_pending(&self) -> bool {
        self.pressure_release_pending
    }

    pub(super) fn pressure_recovery_allowed(&mut self, daemon: &str, now: u64) -> bool {
        // Every automatic restart needs fresh supervisor custody, including
        // failures that were not preceded by a recorded pressure release.
        let path = std::path::Path::new("/run/ramshared/supervisor-state.json");
        if self.pressure_supervisor.as_ref().is_some_and(|fd| {
            !super::pressure_recovery::pinned_still_healthy(fd, path, daemon, now)
        }) {
            self.pressure_supervisor = None;
            return false;
        }
        if self.pressure_supervisor.is_none() {
            self.pressure_supervisor =
                super::pressure_recovery::pin_healthy_supervisor(path, daemon, now).map(Arc::new);
        }
        let Some(fd) = self.pressure_supervisor.as_ref() else {
            return false;
        };
        if !super::pressure_recovery::pinned_still_healthy(fd, path, daemon, now) {
            self.pressure_supervisor = None;
            return false;
        }
        if self.pressure_release_pending {
            self.arm_pressure_members();
        }
        true
    }

    pub(super) fn arm_pressure_members(&mut self) {
        if !self.pressure_members_armed {
            for member in &mut self.members {
                if self.pressure_members.contains(&member.cache_member) {
                    member.lease_lost_release = true;
                }
            }
            self.pressure_members.clear();
            self.pressure_members_armed = true;
        }
    }

    pub(super) fn finish_pressure_recovery_if_active(
        &mut self,
        cache: &AuthoritativeOriginBackend<FileOrigin, OriginCache>,
    ) {
        if self.pressure_release_pending
            && self.pressure_members_armed
            && self.pressure_members.is_empty()
            && !self.members.is_empty()
            && self
                .members
                .iter()
                .all(|member| !member.lease_lost_release && member.startup.is_none())
            && matches!(
                cache.cache_state(),
                OriginCacheState::Active | OriginCacheState::Restricted
            )
        {
            self.pressure_release_pending = false;
            self.pressure_members_armed = false;
            self.pressure_supervisor = None;
        }
    }

    pub(super) fn iter_mut(&mut self) -> std::slice::IterMut<'_, IsolatedWorkerRuntime> {
        self.members.iter_mut()
    }

    #[cfg(test)]
    pub(super) fn recover<S: NbdRuntimeStarter + ?Sized>(
        &mut self,
        starter: &mut S,
        cache: &AuthoritativeOriginBackend<FileOrigin, OriginCache>,
        lease_live: bool,
    ) {
        self.recover_with_authority(starter, cache, lease_live, Arc::new(|| true));
    }

    #[cfg(test)]
    pub(super) fn set_supervisor_fixture(&mut self, fd: std::os::fd::OwnedFd) {
        self.pressure_supervisor = Some(Arc::new(fd));
    }

    pub(super) fn startup_authority(
        &self,
        daemon: &str,
        admission: Option<ControlPlaneCacheAdmission>,
    ) -> super::worker_startup::StartupAuthority {
        let pinned = self.pressure_supervisor.clone();
        let daemon = daemon.to_string();
        Arc::new(move || {
            let Some(fd) = pinned.as_ref() else {
                return false;
            };
            let Some(now) = crate::unix_time_ms() else {
                return false;
            };
            admission
                .as_ref()
                .is_none_or(ControlPlaneCacheAdmission::allows_worker_recovery)
                && !super::nbd_runtime::starter::critical_cache_reclaim_pending(&daemon)
                && super::pressure_recovery::pinned_still_healthy(
                    fd,
                    std::path::Path::new("/run/ramshared/supervisor-state.json"),
                    &daemon,
                    now,
                )
        })
    }

    pub(super) fn recover_with_authority<S: NbdRuntimeStarter + ?Sized>(
        &mut self,
        starter: &mut S,
        cache: &AuthoritativeOriginBackend<FileOrigin, OriginCache>,
        lease_live: bool,
        authority: super::worker_startup::StartupAuthority,
    ) {
        if self.members.is_empty() {
            return;
        }
        let start = self.next_recovery;
        self.next_recovery = (start + 1) % self.members.len();
        for step in 0..self.members.len() {
            let index = (start + step) % self.members.len();
            worker_recovery::recover_isolated_gpu_worker_guarded(
                starter,
                cache,
                &mut self.members[index],
                lease_live,
                Arc::clone(&authority),
            );
        }
    }

    pub(super) fn observation<'a>(
        &'a self,
        critical_reclaim: bool,
        admission_reason: Option<&'a str>,
    ) -> CacheWorkerObservation<'a> {
        CacheWorkerObservation {
            critical_reclaim,
            unavailable_reason: admission_reason.or_else(|| {
                self.members
                    .iter()
                    .find_map(|member| member.unavailable_reason.as_deref())
            }),
            startup_pending: self.members.iter().any(|member| member.startup.is_some()),
        }
    }

    pub(super) fn shutdown(&mut self, timeout: Duration, poll_interval: Duration) -> bool {
        worker_recovery::shutdown_worker_runtimes(&mut self.members, timeout, poll_interval)
    }

    /// Expiry bounds observation, never the lifetime of owned resources.
    pub(super) fn wait_for_shutdown(&mut self) {
        while !self.shutdown(Duration::from_millis(5500), Duration::from_millis(10)) {
            eprintln!(
                "[ramsharedd] shutdown pending: GPU worker or startup exit is unconfirmed; retaining worker custody and origin resources"
            );
        }
    }
}

pub(super) fn release_cache_after_lease_loss<'a>(
    cache: &AuthoritativeOriginBackend<FileOrigin, OriginCache>,
    admission: Option<&ControlPlaneCacheAdmission>,
    runtimes: impl IntoIterator<Item = &'a mut IsolatedWorkerRuntime>,
) {
    if !admission.is_some_and(ControlPlaneCacheAdmission::lease_lost) {
        return;
    }
    for runtime in runtimes {
        runtime.lease_lost_release = true;
        worker_recovery::cancel_pending_startup(
            runtime,
            "control-plane lease lost during worker startup",
        );
    }
    if !matches!(
        cache.cache_state(),
        OriginCacheState::Active | OriginCacheState::Restricted
    ) {
        return;
    }
    match cache.release_cache() {
        Ok(CacheReleaseOutcome::Revoked) => {
            eprintln!("[ramsharedd] control-plane lease loss revoked the clean origin cache")
        }
        Ok(CacheReleaseOutcome::AlreadyReleased) => eprintln!(
            "[ramsharedd] control-plane lease loss found the clean origin cache already released"
        ),
        Err(error) => eprintln!(
            "[ramsharedd] control-plane cache release was not acknowledged: {}",
            error.0
        ),
    }
}
